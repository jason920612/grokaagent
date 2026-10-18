//! Stopping a workflow kills its agents (slow echo workers here, so they
//! are mid-task) and is reported by the stop call, not by a notice. Its own
//! test binary: the echo delay is process-wide.

use std::sync::Arc;
use std::time::{Duration, Instant};

use grokaagent::events::{AgentEvent, EventSink};
use grokaagent::provider::ReasoningEffort;
use grokaagent::workflow::{HubConfig, TokenBudget, WorkflowHub};
use serde_json::{json, Value};

struct Null;
impl EventSink for Null {
    fn emit(&self, _event: &AgentEvent) {}
}

#[tokio::test]
async fn stopping_a_workflow_kills_its_agents_and_frees_the_hub() {
    std::env::set_var("GROKA_ECHO_DELAY_MS", "30000");
    let dir = tempfile::tempdir().unwrap();
    let ws = dir.path().canonicalize().unwrap();
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let hub = WorkflowHub::new(
        HubConfig {
            workspace: ws.clone(),
            worker_bin: env!("CARGO_BIN_EXE_grokaagent").into(),
            root_name: "root".into(),
            root_run: "run-root".into(),
            model: "grok-4.6".into(),
            effort: ReasoningEffort::Low,
            child_mode: "echo".into(),
            search: false,
            knobs: None,
        },
        Arc::new(Null),
        Some(tx),
        Arc::new(TokenBudget::default()),
    )
    .unwrap();
    let started: Value = serde_json::from_str(
        &hub.start(&json!({"script": "export const meta = { name: 'slow', description: 'slow agents' }\nreturn await parallel([1, 2, 3].map(i => () => agent('wait ' + i)))"}))
            .unwrap(),
    )
    .unwrap();
    let id = started["run_id"].as_str().unwrap().to_string();
    tokio::time::sleep(Duration::from_secs(2)).await;
    let t = Instant::now();
    let stopped: Value = serde_json::from_str(&hub.stop(&id).unwrap()).unwrap();
    assert_eq!(stopped["status"], "stopping");
    while hub.running() > 0 {
        assert!(t.elapsed() < Duration::from_secs(15), "the run must end soon after a stop");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    // A stop the main agent asked for sends no notice.
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(rx.try_recv().is_err());
    let journal = std::fs::read_to_string(ws.join(format!(".groka/workflow-runs/{id}/journal.jsonl"))).unwrap();
    assert!(journal.contains("\"status\":\"stopped\""), "{journal}");
    let again: Value = serde_json::from_str(&hub.stop(&id).unwrap()).unwrap();
    assert_eq!(again["note"], "already ended");
}
