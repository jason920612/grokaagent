//! Parent-side child process manager: spawn A2A workers and talk Message/Task.

use std::collections::HashMap;
use std::io::SeekFrom;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncSeekExt, BufReader};
use tokio::process::Command;
use tokio::sync::{oneshot, Mutex};

use crate::a2a::{self, A2aClient, Handshake};
use crate::error::{Error, Result};
use crate::events::{AgentEvent, EventMeta, EventSink};
use crate::procgroup::ProcessGuard;
use crate::tools::{ClientTool, ToolCallFut, ToolSpec};

pub const DEFAULT_MAX_DEPTH: u32 = 2;
pub const DEFAULT_MAX_CHILDREN: usize = 4;
pub const DEFAULT_TASK_TIMEOUT: Duration = Duration::from_secs(120);

pub struct Nursery {
    worker_bin: PathBuf,
    workspace: PathBuf,
    events_dir: PathBuf,
    depth: u32,
    max_depth: u32,
    max_children: usize,
    parent_run_id: String,
    agent_name: String,
    model: String,
    mode: String,
    timeout: Duration,
    client: A2aClient,
    children: Mutex<HashMap<String, Child>>,
}

struct Child {
    origin: String,
    context_id: String,
    guard: ProcessGuard,
    stop_tail: Option<oneshot::Sender<()>>,
}

impl Nursery {
    pub fn new(
        worker_bin: PathBuf,
        workspace: PathBuf,
        events_dir: PathBuf,
        depth: u32,
        parent_run_id: String,
        agent_name: String,
        model: String,
        mode: String,
    ) -> Result<Arc<Self>> {
        Ok(Arc::new(Self {
            worker_bin,
            workspace,
            events_dir,
            depth,
            max_depth: DEFAULT_MAX_DEPTH,
            max_children: DEFAULT_MAX_CHILDREN,
            parent_run_id,
            agent_name,
            model,
            mode,
            timeout: DEFAULT_TASK_TIMEOUT,
            client: A2aClient::new()?,
            children: Mutex::new(HashMap::new()),
        }))
    }

    pub async fn child_names(&self) -> Vec<String> {
        self.children.lock().await.keys().cloned().collect()
    }

    fn meta(&self) -> EventMeta {
        EventMeta {
            ts: chrono::Utc::now(),
            agent_name: self.agent_name.clone(),
            run_id: self.parent_run_id.clone(),
            parent_run_id: None,
        }
    }

    pub async fn spawn_agent(&self, args: &Value, sink: Arc<dyn EventSink>) -> Result<String> {
        if self.depth >= self.max_depth {
            return Err(Error::Tool(format!(
                "spawn_agent blocked: depth {} >= max_depth {}",
                self.depth, self.max_depth
            )));
        }
        let name = args
            .get("name")
            .and_then(Value::as_str)
            .ok_or_else(|| Error::Tool("spawn_agent requires name".into()))?
            .trim()
            .to_string();
        if name.is_empty() {
            return Err(Error::Tool("spawn_agent name is empty".into()));
        }
        let prompt = args
            .get("prompt")
            .and_then(Value::as_str)
            .ok_or_else(|| Error::Tool("spawn_agent requires prompt".into()))?;
        {
            let kids = self.children.lock().await;
            if kids.contains_key(&name) {
                return Err(Error::Tool(format!("child {name} already exists")));
            }
            if kids.len() >= self.max_children {
                return Err(Error::Tool(format!(
                    "spawn_agent blocked: max_children {}",
                    self.max_children
                )));
            }
        }

        let events = self.events_dir.join(format!("{name}.jsonl"));
        let mut cmd = Command::new(&self.worker_bin);
        cmd.arg("worker")
            .arg("--name")
            .arg(&name)
            .arg("--depth")
            .arg((self.depth + 1).to_string())
            .arg("--parent-run")
            .arg(&self.parent_run_id)
            .arg("--listen")
            .arg("127.0.0.1:0")
            .arg("--events")
            .arg(&events)
            .arg("--mode")
            .arg(&self.mode)
            .arg("--workspace")
            .arg(&self.workspace)
            .arg("--model")
            .arg(&self.model)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true);

        let mut guard = ProcessGuard::spawn(cmd).map_err(|e| Error::A2a(format!("spawn: {e}")))?;
        let stdout = guard
            .child_mut()
            .stdout
            .take()
            .ok_or_else(|| Error::A2a("worker stdout missing".into()))?;
        let mut lines = BufReader::new(stdout).lines();
        let line = tokio::time::timeout(Duration::from_secs(8), lines.next_line())
            .await
            .map_err(|_| Error::A2a("worker handshake timed out".into()))?
            .map_err(|e| Error::A2a(format!("read handshake: {e}")))?
            .ok_or_else(|| Error::A2a("worker closed stdout before handshake".into()))?;
        let handshake = Handshake::parse_line(&line)?;
        let origin = handshake.origin()?;

        // Drain leftover stdout so the pipe cannot fill.
        tokio::spawn(async move {
            while let Ok(Some(_)) = lines.next_line().await {}
        });

        sink.emit(&AgentEvent::ChildSpawned {
            meta: self.meta(),
            name: name.clone(),
            agent_card_url: handshake.agent_card_url.clone(),
            prompt: prompt.to_string(),
        });
        self.emit_message(sink.as_ref(), &self.agent_name, &name, prompt);

        let events_path = events.clone();
        let (stop_tx, stop_rx) = oneshot::channel();
        let tail_sink = sink.clone();
        tokio::spawn(async move {
            tail_jsonl(events_path, tail_sink, stop_rx).await;
        });

        let task = self
            .client
            .send_text(&origin, prompt, None)
            .await?;
        let task = self.client.wait_task(&origin, task, self.timeout).await?;
        let context_id = task
            .get("contextId")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let artifact = a2a::artifact_text(&task);
        let state = a2a::task_state(&task);
        if !artifact.is_empty() {
            self.emit_message(sink.as_ref(), &name, &self.agent_name, &artifact);
        }

        self.children.lock().await.insert(
            name.clone(),
            Child {
                origin,
                context_id: context_id.clone(),
                guard,
                stop_tail: Some(stop_tx),
            },
        );

        Ok(json!({
            "name": name,
            "state": state,
            "context_id": context_id,
            "artifact": artifact,
        })
        .to_string())
    }

    pub async fn send_message(&self, args: &Value, sink: &dyn EventSink) -> Result<String> {
        let name = args
            .get("name")
            .and_then(Value::as_str)
            .ok_or_else(|| Error::Tool("send_message requires name".into()))?;
        let text = args
            .get("text")
            .and_then(Value::as_str)
            .ok_or_else(|| Error::Tool("send_message requires text".into()))?;
        let kids = self.children.lock().await;
        let child = kids
            .get(name)
            .ok_or_else(|| Error::Tool(format!("no child named {name}")))?;
        let origin = child.origin.clone();
        let context_id = args
            .get("context_id")
            .and_then(Value::as_str)
            .unwrap_or(&child.context_id)
            .to_string();
        drop(kids);
        self.emit_message(sink, &self.agent_name, name, text);
        let task = self
            .client
            .send_text(&origin, text, Some(&context_id))
            .await?;
        let task = self.client.wait_task(&origin, task, self.timeout).await?;
        let artifact = a2a::artifact_text(&task);
        if !artifact.is_empty() {
            self.emit_message(sink, name, &self.agent_name, &artifact);
        }
        Ok(json!({
            "name": name,
            "state": a2a::task_state(&task),
            "artifact": artifact,
        })
        .to_string())
    }

    fn emit_message(&self, sink: &dyn EventSink, from: &str, to: &str, text: &str) {
        sink.emit(&AgentEvent::AgentMessage {
            meta: self.meta(),
            from: from.to_string(),
            to: to.to_string(),
            text: text.to_string(),
        });
    }

    pub async fn shutdown(&self, sink: &dyn EventSink) {
        let mut kids = self.children.lock().await;
        for (name, mut child) in kids.drain() {
            if let Some(tx) = child.stop_tail.take() {
                let _ = tx.send(());
            }
            let _ = child.guard.kill().await;
            sink.emit(&AgentEvent::ChildExited {
                meta: self.meta(),
                name,
                detail: "killed".into(),
            });
        }
    }
}

pub struct SpawnAgentTool {
    nursery: Arc<Nursery>,
    sink: Arc<dyn EventSink>,
}

impl SpawnAgentTool {
    pub fn new(nursery: Arc<Nursery>, sink: Arc<dyn EventSink>) -> Self {
        Self { nursery, sink }
    }
}

impl ClientTool for SpawnAgentTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "spawn_agent".into(),
            description: "Start a child agent subprocess over A2A. Use for a separable subtask with a verifiable done condition. Returns the child's artifact.".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "name": {"type": "string", "description": "Short unique child name"},
                    "prompt": {"type": "string", "description": "Full goal, paths, and done-criteria. The child has no parent context."}
                },
                "required": ["name", "prompt"],
                "additionalProperties": false
            }),
        }
    }

    fn call(&self, args: &Value) -> ToolCallFut<'_> {
        let n = self.nursery.clone();
        let sink = self.sink.clone();
        let args = args.clone();
        Box::pin(async move { n.spawn_agent(&args, sink).await })
    }
}

pub struct SendMessageTool {
    nursery: Arc<Nursery>,
    sink: Arc<dyn EventSink>,
}

impl SendMessageTool {
    pub fn new(nursery: Arc<Nursery>, sink: Arc<dyn EventSink>) -> Self {
        Self { nursery, sink }
    }
}

impl ClientTool for SendMessageTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "send_message".into(),
            description: "Send an A2A Message to an already spawned child, reusing its contextId for a session.".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "name": {"type": "string"},
                    "text": {"type": "string"},
                    "context_id": {"type": "string", "description": "Optional. Defaults to the child's session contextId."}
                },
                "required": ["name", "text"],
                "additionalProperties": false
            }),
        }
    }

    fn call(&self, args: &Value) -> ToolCallFut<'_> {
        let n = self.nursery.clone();
        let sink = self.sink.clone();
        let args = args.clone();
        Box::pin(async move { n.send_message(&args, sink.as_ref()).await })
    }
}

async fn tail_jsonl(path: PathBuf, sink: Arc<dyn EventSink>, mut stop: oneshot::Receiver<()>) {
    let mut offset = 0u64;
    let mut leftover = String::new();
    loop {
        tokio::select! {
            _ = &mut stop => break,
            _ = tokio::time::sleep(Duration::from_millis(60)) => {}
        }
        let Ok(mut f) = tokio::fs::OpenOptions::new().read(true).open(&path).await else {
            continue;
        };
        if f.seek(SeekFrom::Start(offset)).await.is_err() {
            continue;
        }
        let mut chunk = Vec::new();
        if f.read_to_end(&mut chunk).await.is_err() {
            continue;
        }
        if chunk.is_empty() {
            continue;
        }
        offset += chunk.len() as u64;
        leftover.push_str(&String::from_utf8_lossy(&chunk));
        while let Some(i) = leftover.find('\n') {
            let line = leftover[..i].trim().to_string();
            leftover = leftover[i + 1..].to_string();
            if line.is_empty() {
                continue;
            }
            if let Ok(ev) = serde_json::from_str::<AgentEvent>(&line) {
                sink.emit(&ev);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::FanoutSink;

    #[tokio::test]
    async fn spawn_blocked_at_max_depth() {
        let dir = tempfile::tempdir().unwrap();
        let n = Nursery::new(
            PathBuf::from("grokaagent"),
            dir.path().to_path_buf(),
            dir.path().to_path_buf(),
            2,
            "r".into(),
            "root".into(),
            "grok-4.6".into(),
            "echo".into(),
        )
        .unwrap();
        let sink: Arc<dyn EventSink> = Arc::new(FanoutSink { sinks: vec![] });
        let err = n
            .spawn_agent(&json!({"name": "x", "prompt": "p"}), sink)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("max_depth"), "{err}");
    }

    struct Rec(std::sync::Mutex<Vec<AgentEvent>>);
    impl EventSink for Rec {
        fn emit(&self, event: &AgentEvent) {
            self.0.lock().unwrap().push(event.clone());
        }
    }

    #[tokio::test]
    async fn tail_jsonl_forwards_child_events() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("kid.jsonl");
        std::fs::write(&path, "").unwrap();
        let rec = Arc::new(Rec(std::sync::Mutex::new(Vec::new())));
        let sink: Arc<dyn EventSink> = rec.clone();
        let (tx, rx) = oneshot::channel();
        let p = path.clone();
        let h = tokio::spawn(async move { tail_jsonl(p, sink, rx).await });
        let meta = EventMeta {
            ts: chrono::Utc::now(),
            agent_name: "kid".into(),
            run_id: "c1".into(),
            parent_run_id: Some("r".into()),
        };
        let line = serde_json::to_string(&AgentEvent::ModelDelta {
            meta,
            text: "hello-from-child".into(),
        })
        .unwrap();
        std::fs::write(&path, format!("{line}\n")).unwrap();
        let mut got = false;
        for _ in 0..40 {
            tokio::time::sleep(Duration::from_millis(50)).await;
            if rec
                .0
                .lock()
                .unwrap()
                .iter()
                .any(|e| matches!(e, AgentEvent::ModelDelta { text, .. } if text == "hello-from-child"))
            {
                got = true;
                break;
            }
        }
        let _ = tx.send(());
        let _ = h.await;
        assert!(got, "tailed event never arrived");
    }
}
