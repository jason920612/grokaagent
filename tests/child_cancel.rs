//! Separate test binary: it sets GROKA_ECHO_DELAY_MS for the echo children it spawns.
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use grokaagent::agent::CancelFlag;
use grokaagent::events::{EventSink, FanoutSink};
use grokaagent::nursery::Nursery;
use serde_json::{json, Value};

/// Esc on the parent interrupts a working child. The child stays alive,
/// goes idle without waking the parent, and still answers follow-ups.
#[tokio::test]
async fn parent_esc_interrupts_working_child_without_killing_it() {
    std::env::set_var("GROKA_ECHO_DELAY_MS", "3000");
    let bin = env!("CARGO_BIN_EXE_grokaagent");
    let dir = tempfile::tempdir().unwrap();
    let n = Nursery::new(
        PathBuf::from(bin),
        dir.path().to_path_buf(),
        dir.path().to_path_buf(),
        0,
        "run-test".into(),
        "root".into(),
        "grok-4.6".into(),
        "echo".into(),
        None,
    )
    .unwrap();
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    n.set_notify(tx);
    let cancel = CancelFlag::new();
    n.follow_cancel(cancel.clone());
    let sink: Arc<dyn EventSink> = Arc::new(FanoutSink { sinks: vec![] });
    n.spawn_agent(&json!({"name": "slow", "prompt": "take your time"}), sink.clone())
        .await
        .unwrap();
    // Let the child reach its delayed model call, then press Esc on the parent.
    tokio::time::sleep(Duration::from_millis(1200)).await;
    cancel.trip();
    let out = n
        .wait_agents(&json!({"names": ["slow"], "timeout_seconds": 20}))
        .await
        .unwrap();
    let v: Value = serde_json::from_str(&out).unwrap();
    assert_eq!(v["agents"][0]["state"], "idle", "{out}");
    assert!(
        v["agents"][0].get("reply").is_none(),
        "an interrupted turn has no reply: {out}"
    );
    assert!(
        rx.try_recv().is_err(),
        "an interrupted child must not wake the parent"
    );
    assert_eq!(n.child_names(), vec!["slow".to_string()], "interrupt keeps the child alive");

    // It still has its memory and answers a follow-up.
    n.send_message(&json!({"name": "slow", "text": "again"}), sink.as_ref())
        .await
        .unwrap();
    let out = n
        .wait_agents(&json!({"names": ["slow"], "timeout_seconds": 30}))
        .await
        .unwrap();
    let v: Value = serde_json::from_str(&out).unwrap();
    let reply = v["agents"][0]["reply"].as_str().unwrap_or("");
    assert!(reply.starts_with("echo:take your time"), "{out}");
    assert!(reply.ends_with("again"), "{out}");
    n.shutdown(sink.as_ref()).await;
}
