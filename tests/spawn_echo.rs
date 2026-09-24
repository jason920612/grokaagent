use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use grokaagent::a2a::{self, A2aClient, Handshake};
use grokaagent::events::{AgentEvent, EventSink};
use grokaagent::nursery::Nursery;
use serde_json::{json, Value};

struct Rec(Mutex<Vec<AgentEvent>>);
impl EventSink for Rec {
    fn emit(&self, event: &AgentEvent) {
        self.0.lock().unwrap().push(event.clone());
    }
}

fn nursery(bin: &str, dir: &std::path::Path) -> Arc<Nursery> {
    Nursery::new(
        PathBuf::from(bin),
        dir.to_path_buf(),
        dir.to_path_buf(),
        0,
        "run-test".into(),
        "root".into(),
        "grok-4.6".into(),
        "echo".into(),
        None,
    )
    .unwrap()
}

fn reply_of(out: &str, name: &str) -> String {
    let v: Value = serde_json::from_str(out).unwrap();
    v["agents"]
        .as_array()
        .unwrap()
        .iter()
        .find(|a| a["name"] == name)
        .and_then(|a| a["reply"].as_str())
        .unwrap_or("")
        .to_string()
}

#[tokio::test]
async fn binary_echo_worker_handshake_and_message() {
    let bin = env!("CARGO_BIN_EXE_grokaagent");
    let dir = tempfile::tempdir().unwrap();
    let mut child = Command::new(bin)
        .args([
            "worker",
            "--name",
            "echo1",
            "--mode",
            "echo",
            "--listen",
            "127.0.0.1:0",
            "--events",
            dir.path().join("w.jsonl").to_str().unwrap(),
            "--workspace",
            dir.path().to_str().unwrap(),
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let stdout = child.stdout.take().unwrap();
    let mut first = String::new();
    {
        use std::io::BufRead;
        let mut r = std::io::BufReader::new(stdout);
        r.read_line(&mut first).unwrap();
    }
    let hs = Handshake::parse_line(&first).expect(&first);
    let origin = hs.origin().unwrap();
    let client = A2aClient::new().unwrap();
    let mut task = None;
    for _ in 0..40 {
        match client.send_text(&origin, "hello", Some("ctx-1")).await {
            Ok(t) => {
                task = Some(t);
                break;
            }
            Err(_) => tokio::time::sleep(Duration::from_millis(50)).await,
        }
    }
    let task = task.expect("worker never accepted message:send");
    assert_eq!(task["contextId"], "ctx-1");
    let task = client
        .wait_task(&origin, task, Duration::from_secs(30))
        .await
        .unwrap();
    assert_eq!(a2a::artifact_text(&task), "echo:hello");
    let _ = child.kill();
}

/// A child keeps its session: the second message sees the first. Replies
/// come back through wait_agents when the parent blocks for them.
#[tokio::test]
async fn spawned_child_keeps_memory_and_answers_wait() {
    let bin = env!("CARGO_BIN_EXE_grokaagent");
    let dir = tempfile::tempdir().unwrap();
    let n = nursery(bin, dir.path());
    let rec = Arc::new(Rec(Mutex::new(Vec::new())));
    let sink: Arc<dyn EventSink> = rec.clone();
    let out = n
        .spawn_agent(&json!({"name": "kid", "prompt": "do the thing"}), sink.clone())
        .await
        .unwrap();
    assert!(out.contains("\"state\":\"working\""), "{out}");
    let waited = n
        .wait_agents(&json!({"names": ["kid"], "timeout_seconds": 30}))
        .await
        .unwrap();
    assert_eq!(reply_of(&waited, "kid"), "echo:do the thing", "{waited}");

    n.send_message(&json!({"name": "kid", "text": "second"}), sink.as_ref())
        .await
        .unwrap();
    let waited = n.wait_agents(&json!({"timeout_seconds": 30})).await.unwrap();
    assert_eq!(
        reply_of(&waited, "kid"),
        "echo:do the thing | second",
        "{waited}"
    );

    let events = rec.0.lock().unwrap().clone();
    assert!(
        events
            .iter()
            .any(|e| matches!(e, AgentEvent::RunStarted { meta, .. } if meta.path == "kid")),
        "child events must be relayed with the child's path"
    );
    let listed = n.list_agents();
    assert!(listed.contains("\"state\":\"idle\""), "{listed}");

    n.stop_agent(&json!({"name": "kid", "kill": true}))
        .await
        .unwrap();
    assert!(n.child_names().is_empty(), "killed child frees its slot");
    assert!(rec
        .0
        .lock()
        .unwrap()
        .iter()
        .any(|e| matches!(e, AgentEvent::ChildExited { name, .. } if name == "kid")));
    n.shutdown(sink.as_ref()).await;
}

/// Without a wait, the idle child's reply is pushed to the parent session.
#[tokio::test]
async fn idle_child_reply_is_pushed_to_the_parent() {
    let bin = env!("CARGO_BIN_EXE_grokaagent");
    let dir = tempfile::tempdir().unwrap();
    let n = nursery(bin, dir.path());
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    n.set_notify(tx);
    let sink: Arc<dyn EventSink> = Arc::new(Rec(Mutex::new(Vec::new())));
    n.spawn_agent(&json!({"name": "kid", "prompt": "ping"}), sink.clone())
        .await
        .unwrap();
    let turn = tokio::time::timeout(Duration::from_secs(30), rx.recv())
        .await
        .expect("parent was never notified")
        .unwrap();
    assert!(turn.text.contains("`kid` replied"), "{}", turn.text);
    assert!(turn.text.contains("echo:ping"), "{}", turn.text);
    n.shutdown(sink.as_ref()).await;
}
