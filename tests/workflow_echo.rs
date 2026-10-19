//! Workflows end to end with echo workers (a real worker process whose
//! model echoes its messages, no network): fan-out, schema results, the
//! agent tree, the end notice, and resuming from a journal.

use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use grokaagent::agent::UserTurn;
use grokaagent::events::{AgentEvent, EventSink};
use grokaagent::provider::ReasoningEffort;
use grokaagent::workflow::{HubConfig, TokenBudget, WorkflowHub};
use serde_json::{json, Value};
use tokio::sync::mpsc;

struct Rec(Mutex<Vec<AgentEvent>>);
impl EventSink for Rec {
    fn emit(&self, event: &AgentEvent) {
        self.0.lock().unwrap().push(event.clone());
    }
}

fn hub(ws: &Path) -> (WorkflowHub, Arc<Rec>, mpsc::UnboundedReceiver<UserTurn>) {
    let rec = Arc::new(Rec(Mutex::new(Vec::new())));
    let (tx, rx) = mpsc::unbounded_channel();
    let hub = WorkflowHub::new(
        HubConfig {
            workspace: ws.to_path_buf(),
            worker_bin: env!("CARGO_BIN_EXE_grokaagent").into(),
            root_name: "root".into(),
            root_run: "run-root".into(),
            model: "grok-4.6".into(),
            effort: ReasoningEffort::Low,
            child_mode: "echo".into(),
            search: false,
            knobs: None,
        },
        rec.clone(),
        Some(tx),
        Arc::new(TokenBudget::default()),
    )
    .unwrap();
    (hub, rec, rx)
}

async fn notice(rx: &mut mpsc::UnboundedReceiver<UserTurn>) -> String {
    tokio::time::timeout(Duration::from_secs(90), rx.recv())
        .await
        .expect("the workflow must end with a notice")
        .expect("notice channel open")
        .text
}

const SCHEMA: &str = "{type: 'object', properties: {n: {type: 'integer'}}, required: ['n']}";

#[tokio::test]
async fn a_workflow_fans_out_hands_back_structured_results_and_reports_once() {
    let dir = tempfile::tempdir().unwrap();
    let ws = dir.path().canonicalize().unwrap();
    let (hub, rec, mut rx) = hub(&ws);
    let script = format!(
        "export const meta = {{ name: 'echo-fanout', description: 'fan out to echo agents', phases: [{{ title: 'Go' }}] }}\n\
phase('Go')\n\
const texts = await parallel([1, 2, 3].map(i => () => agent('task ' + i, {{ label: 'item ' + i }})))\n\
const obj = await agent('give json ECHO_SUBMIT {{\"n\": 5}}', {{ schema: {SCHEMA}, label: 'json' }})\n\
const bad = await agent('plain text only', {{ schema: {SCHEMA}, label: 'no-json' }})\n\
log('all done')\n\
return {{ texts, obj, bad, n: args.n }}"
    );
    let started: Value = serde_json::from_str(&hub.start(&json!({"script": script, "args": {"n": 9}})).unwrap()).unwrap();
    let id = started["run_id"].as_str().unwrap().to_string();
    assert!(id.starts_with("wf-"));
    assert_eq!(hub.running(), 1);

    let text = notice(&mut rx).await;
    assert!(text.starts_with(&format!("[workflow {id} finished]")), "{text}");
    assert!(text.contains("5 agent call(s): 4 succeeded, 1 failed"), "{text}");
    assert!(grokaagent::notice::is_harness(&text));
    let result: Value = serde_json::from_slice(&std::fs::read(ws.join(format!(".groka/workflow-runs/{id}/result.json"))).unwrap()).unwrap();
    assert_eq!(result["texts"], json!(["echo:task 1", "echo:task 2", "echo:task 3"]));
    assert_eq!(result["obj"], json!({"n": 5}));
    assert_eq!(result["bad"], Value::Null);
    assert_eq!(result["n"], 9);
    assert_eq!(hub.running(), 0);

    // The tree: the workflow node at the root, its agents under it, and
    // everything closed at the end.
    let events = rec.0.lock().unwrap();
    let root_spawn = events.iter().any(|e| matches!(e, AgentEvent::ChildSpawned { name, meta, .. } if name == &id && meta.path.is_empty() && meta.parent_run_id.is_none()));
    assert!(root_spawn);
    let agents: Vec<String> = events
        .iter()
        .filter_map(|e| match e {
            AgentEvent::ChildSpawned { name, meta, .. } if meta.path == id => Some(name.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(agents.len(), 5, "{agents:?}");
    assert!(agents.iter().any(|a| a.starts_with("item-1-")), "{agents:?}");
    let relayed = events.iter().any(|e| e.path().starts_with(&format!("{id}/item-")) && matches!(e, AgentEvent::ModelFinished { .. }));
    assert!(relayed, "agent events are relayed under the workflow node");
    let closed = events.iter().any(|e| matches!(e, AgentEvent::ChildExited { name, meta, .. } if name == &id && meta.path.is_empty()));
    assert!(closed);
    let logged = events.iter().any(|e| matches!(e, AgentEvent::Notice { message, meta } if meta.path == id && message == "all done"));
    assert!(logged);
}

#[tokio::test]
async fn a_resumed_run_answers_unchanged_calls_from_the_journal() {
    let dir = tempfile::tempdir().unwrap();
    let ws = dir.path().canonicalize().unwrap();
    let (hub, rec, mut rx) = hub(&ws);
    let script = "export const meta = { name: 'two', description: 'two calls' }\n\
const a = await agent('first')\n\
const b = await agent('second ' + a)\n\
return [a, b]";
    let first: Value = serde_json::from_str(&hub.start(&json!({"script": script})).unwrap()).unwrap();
    let id = first["run_id"].as_str().unwrap().to_string();
    let text = notice(&mut rx).await;
    assert!(text.contains("2 succeeded"), "{text}");

    // Same script plus one more call: the two old calls come from the cache.
    let script_path = first["script_path"].as_str().unwrap().to_string();
    let edited = format!(
        "{}\n",
        script.replace("return [a, b]", "const c = await agent('third')\nreturn [a, b, c]")
    );
    std::fs::write(ws.join(&script_path), edited).unwrap();
    rec.0.lock().unwrap().clear();
    let second: Value = serde_json::from_str(
        &hub.start(&json!({"script_path": script_path, "resume_from_run_id": id})).unwrap(),
    )
    .unwrap();
    assert_eq!(second["cached_calls"], 2);
    let text = notice(&mut rx).await;
    assert!(text.contains("1 succeeded, 0 failed (null), 2 answered from the journal"), "{text}");
    let spawned = rec
        .0
        .lock()
        .unwrap()
        .iter()
        .filter(|e| matches!(e, AgentEvent::ChildSpawned { meta, .. } if !meta.path.is_empty()))
        .count();
    assert_eq!(spawned, 1, "only the new call starts an agent");
}

#[tokio::test]
async fn bad_scripts_and_calls_are_refused_up_front() {
    let dir = tempfile::tempdir().unwrap();
    let ws = dir.path().canonicalize().unwrap();
    let (hub, _rec, _rx) = hub(&ws);
    let e = hub.start(&json!({"script": "return 1"})).unwrap_err().to_string();
    assert!(e.contains("export const meta"), "{e}");
    let e = hub.start(&json!({"name": "nope"})).unwrap_err().to_string();
    assert!(e.contains("no saved workflow named nope"), "{e}");
    let e = hub.start(&json!({"script": "x", "name": "y"})).unwrap_err().to_string();
    assert!(e.contains("exactly one"), "{e}");
    let e = hub
        .start(&json!({"script": "export const meta = {name: 'a', description: 'b'}\nreturn 1", "resume_from_run_id": "wf-00000000"}))
        .unwrap_err()
        .to_string();
    assert!(e.contains("no journal"), "{e}");
    let overview: Value = serde_json::from_str(&hub.overview()).unwrap();
    assert!(overview["runs"].as_array().unwrap().is_empty());
}

#[tokio::test]
async fn a_saved_workflow_runs_by_name_and_nests() {
    let dir = tempfile::tempdir().unwrap();
    let ws = dir.path().canonicalize().unwrap();
    let saved = ws.join(".groka/workflows");
    std::fs::create_dir_all(&saved).unwrap();
    std::fs::write(
        saved.join("inner.js"),
        "export const meta = { name: 'inner', description: 'one agent' }\nreturn await agent('inner ' + args.x)",
    )
    .unwrap();
    std::fs::write(
        saved.join("outer.js"),
        "export const meta = { name: 'outer', description: 'calls inner' }\nreturn await workflow('inner', { x: args.x })",
    )
    .unwrap();
    let (hub, _rec, mut rx) = hub(&ws);
    let overview: Value = serde_json::from_str(&hub.overview()).unwrap();
    let names: Vec<&str> = overview["saved_workflows"].as_array().unwrap().iter().map(|w| w["name"].as_str().unwrap()).collect();
    assert_eq!(names, ["inner", "outer"]);
    hub.start(&json!({"name": "outer", "args": {"x": "y"}})).unwrap();
    let text = notice(&mut rx).await;
    assert!(text.contains("\"echo:inner y\""), "{text}");
}

#[tokio::test]
async fn a_stop_ends_a_script_that_has_no_agent_running() {
    let dir = tempfile::tempdir().unwrap();
    let ws = dir.path().canonicalize().unwrap();
    let (hub, _rec, _rx) = hub(&ws);
    let started: Value = serde_json::from_str(
        &hub.start(&json!({"script": "export const meta = { name: 'spin', description: 'busy loop' }\nwhile (true) {}"}))
            .unwrap(),
    )
    .unwrap();
    let id = started["run_id"].as_str().unwrap();
    tokio::time::sleep(Duration::from_millis(300)).await;
    hub.stop(id).unwrap();
    let t = std::time::Instant::now();
    while hub.running() > 0 {
        assert!(t.elapsed() < Duration::from_secs(10), "a stop must interrupt the script even with no agent to kill");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}
