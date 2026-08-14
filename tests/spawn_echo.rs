use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::Duration;

use grokaagent::a2a::{self, A2aClient, Handshake};
use grokaagent::events::{EventSink, FanoutSink};
use grokaagent::nursery::Nursery;
use serde_json::json;

#[tokio::test]
async fn binary_echo_worker_handshake_and_spawn_agent() {
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
        .stderr(Stdio::piped())
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
            Err(_) => std::thread::sleep(Duration::from_millis(50)),
        }
    }
    let task = task.expect("worker never accepted message:send");
    assert_eq!(a2a::artifact_text(&task), "echo:hello");
    assert_eq!(task["contextId"], "ctx-1");
    let _ = child.kill();

    let nursery = Nursery::new(
        PathBuf::from(bin),
        dir.path().to_path_buf(),
        dir.path().to_path_buf(),
        0,
        "run-test".into(),
        "root".into(),
        "grok-4.6".into(),
        "echo".into(),
    )
    .unwrap();
    let sink: std::sync::Arc<dyn EventSink> = std::sync::Arc::new(FanoutSink { sinks: vec![] });
    let out = nursery
        .spawn_agent(&json!({"name": "kid", "prompt": "do the thing"}), sink.clone())
        .await
        .unwrap();
    assert!(out.contains("echo:do the thing"), "{out}");
    let again = nursery
        .send_message(&json!({"name": "kid", "text": "second"}), sink.as_ref())
        .await
        .unwrap();
    assert!(again.contains("echo:second"), "{again}");
    nursery.shutdown(&sink).await;
}
