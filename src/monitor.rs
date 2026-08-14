//! Live event hooks: the model writes a script, the kernel only pipes JSONL to it.

use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::{Arc, Mutex};

use serde_json::{json, Value};
use tokio::io::AsyncWriteExt;
use tokio::sync::{mpsc, oneshot};

use crate::error::{Error, Result};
use crate::events::{AgentEvent, EventMeta, EventSink};
use crate::procgroup::ProcessGuard;
use crate::shellguard::{self, CommandReviewer};
use crate::tools::{hide_window, ClientTool, ToolCallFut, ToolSpec};

pub const MAX_MONITORS: usize = 4;

struct Slot {
    line_tx: mpsc::UnboundedSender<String>,
    kill_tx: Option<oneshot::Sender<()>>,
}

struct Inner {
    slots: HashMap<String, Slot>,
    next_id: u32,
}

pub struct MonitorHub {
    workspace: PathBuf,
    events_file: PathBuf,
    agent_name: String,
    run_id: String,
    parent_run_id: Option<String>,
    inner: Mutex<Inner>,
}

impl MonitorHub {
    pub fn new(
        workspace: PathBuf,
        events_file: PathBuf,
        agent_name: String,
        run_id: String,
        parent_run_id: Option<String>,
    ) -> Arc<Self> {
        Arc::new(Self {
            workspace,
            events_file,
            agent_name,
            run_id,
            parent_run_id,
            inner: Mutex::new(Inner {
                slots: HashMap::new(),
                next_id: 1,
            }),
        })
    }

    fn meta(&self) -> EventMeta {
        EventMeta {
            ts: chrono::Utc::now(),
            agent_name: self.agent_name.clone(),
            run_id: self.run_id.clone(),
            parent_run_id: self.parent_run_id.clone(),
        }
    }

    pub fn forward(&self, event: &AgentEvent) {
        let Ok(line) = serde_json::to_string(event) else {
            return;
        };
        let mut dead = Vec::new();
        if let Ok(inner) = self.inner.lock() {
            for (name, slot) in &inner.slots {
                if slot.line_tx.send(line.clone()).is_err() {
                    dead.push(name.clone());
                }
            }
        }
        for name in dead {
            self.remove(&name);
        }
    }

    fn remove(&self, name: &str) {
        if let Ok(mut inner) = self.inner.lock() {
            inner.slots.remove(name);
        }
    }

    pub fn len(&self) -> usize {
        self.inner.lock().map(|i| i.slots.len()).unwrap_or(0)
    }

    pub fn attach(
        self: &Arc<Self>,
        command: &str,
        name: Option<&str>,
        sink: Arc<dyn EventSink>,
    ) -> Result<String> {
        let command = command.trim();
        if command.is_empty() {
            return Err(Error::Tool("command is required".into()));
        }
        let name = self.take_name(name)?;
        let events_path = abs_events_path(&self.events_file);
        let cwd = self
            .workspace
            .canonicalize()
            .unwrap_or_else(|_| self.workspace.clone());
        if !cwd.is_dir() {
            return Err(Error::Tool("workspace is not a directory".into()));
        }

        let mut cmd = crate::tools::shell_command(command);
        cmd.current_dir(&cwd)
            .env("GROKA_EVENTS_PATH", &events_path)
            .env("GROKA_MONITOR_NAME", &name)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        hide_window(&mut cmd);

        let mut guard = match ProcessGuard::spawn(cmd) {
            Ok(g) => g,
            Err(e) => {
                return Err(Error::Tool(format!("attach_monitor spawn failed: {e}")));
            }
        };
        let pid = guard.child_mut().id().unwrap_or(0);
        let stdin = match guard.child_mut().stdin.take() {
            Some(s) => s,
            None => {
                return Err(Error::Tool("attach_monitor stdin missing".into()));
            }
        };

        let (line_tx, line_rx) = mpsc::unbounded_channel();
        let (kill_tx, kill_rx) = oneshot::channel();
        {
            let mut inner = self.inner.lock().unwrap();
            inner.slots.insert(
                name.clone(),
                Slot {
                    line_tx,
                    kill_tx: Some(kill_tx),
                },
            );
        }

        tokio::spawn(write_lines(stdin, line_rx));
        let hub = Arc::clone(self);
        let wait_name = name.clone();
        let wait_sink = sink.clone();
        tokio::spawn(async move {
            let detail = wait_or_kill(guard, kill_rx).await;
            hub.remove(&wait_name);
            wait_sink.emit(&AgentEvent::MonitorExited {
                meta: hub.meta(),
                name: wait_name,
                detail,
            });
        });

        sink.emit(&AgentEvent::MonitorAttached {
            meta: self.meta(),
            name: name.clone(),
            command: command.to_string(),
            pid,
        });

        Ok(json!({
            "name": name,
            "pid": pid,
            "events_path": events_path.to_string_lossy(),
            "stdin": "jsonl"
        })
        .to_string())
    }

    fn take_name(&self, requested: Option<&str>) -> Result<String> {
        let mut inner = self.inner.lock().unwrap();
        if inner.slots.len() >= MAX_MONITORS {
            return Err(Error::Tool(format!(
                "attach_monitor blocked: max {MAX_MONITORS} hooks"
            )));
        }
        let name = match requested.map(str::trim).filter(|s| !s.is_empty()) {
            Some(raw) => {
                let name = sanitize_name(raw)?;
                if inner.slots.contains_key(&name) {
                    return Err(Error::Tool(format!("monitor {name} already attached")));
                }
                name
            }
            None => loop {
                let n = format!("mon-{}", inner.next_id);
                inner.next_id += 1;
                if !inner.slots.contains_key(&n) {
                    break n;
                }
            },
        };
        Ok(name)
    }

    pub async fn shutdown(&self) {
        let mut kills = Vec::new();
        if let Ok(mut inner) = self.inner.lock() {
            for (_, mut slot) in inner.slots.drain() {
                drop(slot.line_tx);
                if let Some(tx) = slot.kill_tx.take() {
                    kills.push(tx);
                }
            }
        }
        tokio::time::sleep(std::time::Duration::from_millis(400)).await;
        for tx in kills {
            let _ = tx.send(());
        }
    }
}

fn sanitize_name(raw: &str) -> Result<String> {
    if raw.len() > 40 {
        return Err(Error::Tool("monitor name too long".into()));
    }
    if !raw
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        return Err(Error::Tool(
            "monitor name must be ascii alphanumeric, dash, or underscore".into(),
        ));
    }
    Ok(raw.to_string())
}

fn abs_events_path(path: &std::path::Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| {
        if path.is_absolute() {
            path.to_path_buf()
        } else {
            std::env::current_dir()
                .unwrap_or_else(|_| PathBuf::from("."))
                .join(path)
        }
    })
}

async fn write_lines(mut stdin: tokio::process::ChildStdin, mut rx: mpsc::UnboundedReceiver<String>) {
    while let Some(line) = rx.recv().await {
        if stdin.write_all(line.as_bytes()).await.is_err() {
            break;
        }
        if stdin.write_all(b"\n").await.is_err() {
            break;
        }
        let _ = stdin.flush().await;
    }
}

async fn wait_or_kill(mut guard: ProcessGuard, kill_rx: oneshot::Receiver<()>) -> String {
    tokio::select! {
        status = guard.child_mut().wait() => match status {
            Ok(s) => format!("exit {}", s.code().unwrap_or(-1)),
            Err(e) => format!("wait: {e}"),
        },
        _ = kill_rx => {
            let _ = guard.kill().await;
            "killed".into()
        }
    }
}

pub struct MonitorSink {
    pub inner: Arc<dyn EventSink>,
    pub hub: Arc<MonitorHub>,
}

impl EventSink for MonitorSink {
    fn emit(&self, event: &AgentEvent) {
        self.inner.emit(event);
        self.hub.forward(event);
    }
}

pub struct AttachMonitorTool {
    hub: Arc<MonitorHub>,
    sink: Arc<dyn EventSink>,
    guard: Option<Arc<dyn CommandReviewer>>,
}

impl AttachMonitorTool {
    pub fn new(
        hub: Arc<MonitorHub>,
        sink: Arc<dyn EventSink>,
        guard: Option<Arc<dyn CommandReviewer>>,
    ) -> Self {
        Self { hub, sink, guard }
    }
}

impl ClientTool for AttachMonitorTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "attach_monitor".into(),
            description: "Start a workspace shell command as a live event hook. The process receives one JSON event per stdin line (same objects as the events JSONL). GROKA_EVENTS_PATH points at that file. The kernel does not interpret the script. If the hook exits, the agent run continues.".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "command": {"type": "string", "description": "Shell command to run (cwd = workspace). Reads JSONL events from stdin."},
                    "name": {"type": "string", "description": "Optional hook name (ascii, dash, underscore)"}
                },
                "required": ["command"],
                "additionalProperties": false
            }),
        }
    }

    fn call(&self, args: &Value) -> ToolCallFut<'_> {
        let command = args
            .get("command")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let name = args
            .get("name")
            .and_then(Value::as_str)
            .map(str::to_string);
        let hub = self.hub.clone();
        let sink = self.sink.clone();
        let guard = self.guard.clone();
        Box::pin(async move {
            shellguard::enforce(guard.as_ref(), &command, ".").await?;
            hub.attach(&command, name.as_deref(), sink)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    struct Rec(Mutex<Vec<AgentEvent>>);
    impl EventSink for Rec {
        fn emit(&self, event: &AgentEvent) {
            self.0.lock().unwrap().push(event.clone());
        }
    }

    fn hook_copy_cmd(rel: &str) -> String {
        #[cfg(windows)]
        {
            format!("findstr /R \".*\" > {rel}")
        }
        #[cfg(not(windows))]
        {
            format!("cat > {rel}")
        }
    }

    fn hang_cmd() -> &'static str {
        #[cfg(windows)]
        {
            "ping -n 60 127.0.0.1 >nul"
        }
        #[cfg(not(windows))]
        {
            "sleep 60"
        }
    }

    fn meta() -> EventMeta {
        EventMeta {
            ts: chrono::Utc::now(),
            agent_name: "root".into(),
            run_id: "r".into(),
            parent_run_id: None,
        }
    }

    async fn wait_file(path: &std::path::Path, want: usize) -> String {
        let start = std::time::Instant::now();
        loop {
            if let Ok(raw) = std::fs::read_to_string(path) {
                if raw.lines().filter(|l| !l.trim().is_empty()).count() >= want {
                    return raw;
                }
            }
            if start.elapsed() > Duration::from_secs(4) {
                return std::fs::read_to_string(path).unwrap_or_default();
            }
            tokio::time::sleep(Duration::from_millis(40)).await;
        }
    }

    #[tokio::test]
    async fn hook_receives_jsonl_on_stdin() {
        let dir = tempfile::tempdir().unwrap();
        let events = dir.path().join("e.jsonl");
        std::fs::write(&events, "").unwrap();
        let hub = MonitorHub::new(
            dir.path().to_path_buf(),
            events,
            "root".into(),
            "run-1".into(),
            None,
        );
        let rec = Arc::new(Rec(Mutex::new(Vec::new())));
        let out: Value = serde_json::from_str(
            &hub.attach(&hook_copy_cmd("hook.jsonl"), Some("copy"), rec.clone())
                .unwrap(),
        )
        .unwrap();
        assert_eq!(out["name"], "copy");
        assert_eq!(out["stdin"], "jsonl");

        hub.forward(&AgentEvent::TurnStarted { meta: meta(), turn: 1 });
        hub.forward(&AgentEvent::RunFinished {
            meta: meta(),
            reason: "stop".into(),
            text: "done".into(),
        });
        hub.shutdown().await;
        let raw = wait_file(&dir.path().join("hook.jsonl"), 2).await;
        assert!(raw.contains("turn_started"), "{raw}");
        assert!(raw.contains("run_finished"), "{raw}");
        assert!(
            rec.0
                .lock()
                .unwrap()
                .iter()
                .any(|e| matches!(e, AgentEvent::MonitorAttached { name, .. } if name == "copy"))
        );
    }

    #[tokio::test]
    async fn hook_crash_does_not_break_forward() {
        let dir = tempfile::tempdir().unwrap();
        let events = dir.path().join("e.jsonl");
        std::fs::write(&events, "").unwrap();
        let hub = MonitorHub::new(
            dir.path().to_path_buf(),
            events,
            "root".into(),
            "run-1".into(),
            None,
        );
        let rec = Arc::new(Rec(Mutex::new(Vec::new())));
        hub.attach("exit 1", Some("boom"), rec.clone()).unwrap();
        let start = std::time::Instant::now();
        loop {
            let hit = rec.0.lock().unwrap().iter().any(|e| {
                matches!(e, AgentEvent::MonitorExited { name, .. } if name == "boom")
            });
            if hit {
                break;
            }
            if start.elapsed() > Duration::from_secs(4) {
                panic!("monitor should exit without killing the hub");
            }
            tokio::time::sleep(Duration::from_millis(40)).await;
        }
        hub.forward(&AgentEvent::TurnStarted { meta: meta(), turn: 3 });
        assert_eq!(hub.len(), 0);
    }

    #[tokio::test]
    async fn attach_rejects_empty_and_caps() {
        let dir = tempfile::tempdir().unwrap();
        let events = dir.path().join("e.jsonl");
        std::fs::write(&events, "").unwrap();
        let hub = MonitorHub::new(
            dir.path().to_path_buf(),
            events,
            "root".into(),
            "run-1".into(),
            None,
        );
        let rec = Arc::new(Rec(Mutex::new(Vec::new())));
        let err = hub.attach("  ", None, rec.clone()).unwrap_err();
        assert!(err.to_string().contains("command is required"), "{err}");

        for i in 0..MAX_MONITORS {
            hub.attach(hang_cmd(), Some(&format!("h{i}")), rec.clone())
                .unwrap();
        }
        let err = hub.attach(hang_cmd(), Some("h4"), rec.clone()).unwrap_err();
        assert!(err.to_string().contains("max"), "{err}");
        hub.shutdown().await;
    }

    #[tokio::test]
    async fn tool_rejects_empty_command() {
        let dir = tempfile::tempdir().unwrap();
        let hub = MonitorHub::new(
            dir.path().to_path_buf(),
            dir.path().join("e.jsonl"),
            "root".into(),
            "r".into(),
            None,
        );
        let rec: Arc<dyn EventSink> = Arc::new(Rec(Mutex::new(Vec::new())));
        let tool = AttachMonitorTool::new(hub, rec, None);
        assert_eq!(tool.spec().name, "attach_monitor");
        let err = tool.call(&json!({"command": ""})).await.unwrap_err();
        assert!(err.to_string().contains("command is required"), "{err}");
    }
}
