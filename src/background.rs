//! Named background processes the model can start, inspect, and kill.

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::sync::{mpsc, oneshot};

use crate::agent::UserTurn;
use crate::error::{Error, Result};
use crate::events::{AgentEvent, EventMeta, EventSink};
use crate::procgroup::ProcessGuard;
use crate::shellguard::{self, CommandReviewer};
use crate::tools::{hide_window, resolve_in_workspace, shell_command, ClientTool, ToolCallFut, ToolSpec};
use crate::wintrack::{self, WindowHub};

pub const MAX_BACKGROUNDS: usize = 4;
pub const EXIT_NOTICE_PREFIX: &str = "[background exited]";
pub const CLOSED_NOTICE_PREFIX: &str = "[backgrounds closed]";
const LOG_CAP: usize = 200;
const LOG_TAIL: usize = 20;

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub struct ClosedBackground {
    pub name: String,
    pub command: String,
    pub log: Vec<String>,
}

pub fn format_closed_notice(items: &[ClosedBackground]) -> String {
    let mut body = format!(
        "{CLOSED_NOTICE_PREFIX}\nThese background processes were still running when the conversation ended. They were killed because the conversation was closed, not because they finished on their own.\n"
    );
    for it in items {
        body.push_str(&format!("\n## {}\ncommand: {}\n", it.name, it.command));
        if !it.log.is_empty() {
            body.push_str("log:\n");
            body.push_str(&it.log.join("\n"));
            body.push('\n');
        }
    }
    body
}

struct Slot {
    command: String,
    pid: u32,
    log: VecDeque<String>,
    kill_tx: Option<oneshot::Sender<()>>,
    alive: bool,
    detail: String,
}

struct Inner {
    slots: HashMap<String, Slot>,
    next_id: u32,
}

pub struct BackgroundHub {
    workspace: std::path::PathBuf,
    agent_name: String,
    run_id: String,
    parent_run_id: Option<String>,
    inner: Mutex<Inner>,
    model_tx: Option<mpsc::UnboundedSender<UserTurn>>,
    shutting_down: AtomicBool,
}

impl BackgroundHub {
    pub fn new(
        workspace: std::path::PathBuf,
        agent_name: String,
        run_id: String,
        parent_run_id: Option<String>,
        model_tx: Option<mpsc::UnboundedSender<UserTurn>>,
    ) -> Arc<Self> {
        Arc::new(Self {
            workspace,
            agent_name,
            run_id,
            parent_run_id,
            inner: Mutex::new(Inner {
                slots: HashMap::new(),
                next_id: 1,
            }),
            model_tx,
            shutting_down: AtomicBool::new(false),
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

    pub fn start(
        self: &Arc<Self>,
        command: &str,
        name: Option<&str>,
        cwd: Option<&str>,
        sink: Arc<dyn EventSink>,
    ) -> Result<String> {
        let command = command.trim();
        if command.is_empty() {
            return Err(Error::Tool("command is required".into()));
        }
        let cwd = match cwd.map(str::trim).filter(|s| !s.is_empty() && *s != ".") {
            Some(p) => resolve_in_workspace(&self.workspace, p)?,
            None => self
                .workspace
                .canonicalize()
                .map_err(|e| Error::Tool(format!("workspace not found: {e}")))?,
        };
        if !cwd.is_dir() {
            return Err(Error::Tool("cwd is not a directory".into()));
        }
        let name = self.take_name(name)?;

        let mut cmd = shell_command(command);
        cmd.current_dir(&cwd)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true);
        hide_window(&mut cmd);

        let mut guard = ProcessGuard::spawn(cmd)
            .map_err(|e| Error::Tool(format!("run_background spawn failed: {e}")))?;
        let pid = guard.child_mut().id().unwrap_or(0);
        let stdout = guard.child_mut().stdout.take();
        let stderr = guard.child_mut().stderr.take();
        let (kill_tx, kill_rx) = oneshot::channel();
        {
            let mut inner = self.inner.lock().unwrap();
            inner.slots.insert(
                name.clone(),
                Slot {
                    command: command.to_string(),
                    pid,
                    log: VecDeque::new(),
                    kill_tx: Some(kill_tx),
                    alive: true,
                    detail: String::new(),
                },
            );
        }

        let hub = Arc::clone(self);
        let wait_name = name.clone();
        let wait_sink = sink.clone();
        tokio::spawn(async move {
            let out = pump_lines(stdout, "out", wait_name.clone(), hub.clone(), wait_sink.clone());
            let err = pump_lines(stderr, "err", wait_name.clone(), hub.clone(), wait_sink.clone());
            let wait = wait_or_kill(guard, kill_rx);
            let (detail, _, _) = tokio::join!(wait, out, err);
            hub.mark_dead(&wait_name, &detail);
            hub.notify_model_exit(&wait_name, &detail);
            wait_sink.emit(&AgentEvent::BackgroundExited {
                meta: hub.meta(),
                name: wait_name,
                detail,
            });
        });

        sink.emit(&AgentEvent::BackgroundStarted {
            meta: self.meta(),
            name: name.clone(),
            command: command.to_string(),
            pid,
        });

        Ok(json!({
            "name": name,
            "pid": pid,
            "command": command,
            "alive": true
        })
        .to_string())
    }

    pub fn snapshot(&self, name: &str) -> Result<String> {
        let inner = self.inner.lock().unwrap();
        let slot = inner
            .slots
            .get(name)
            .ok_or_else(|| Error::Tool(format!("no background named {name}")))?;
        Ok(json!({
            "name": name,
            "pid": slot.pid,
            "command": slot.command,
            "alive": slot.alive,
            "detail": slot.detail,
            "log": slot.log.iter().cloned().collect::<Vec<_>>(),
        })
        .to_string())
    }

    pub fn alive_pids(&self) -> Vec<u32> {
        let Ok(inner) = self.inner.lock() else {
            return Vec::new();
        };
        inner
            .slots
            .values()
            .filter(|s| s.alive && s.pid > 0)
            .map(|s| s.pid)
            .collect()
    }

    pub fn kill(&self, name: &str) -> Result<String> {
        let tx = {
            let mut inner = self.inner.lock().unwrap();
            let slot = inner
                .slots
                .get_mut(name)
                .ok_or_else(|| Error::Tool(format!("no background named {name}")))?;
            if !slot.alive {
                return Ok(json!({"name": name, "killed": false, "detail": slot.detail}).to_string());
            }
            slot.kill_tx.take()
        };
        if let Some(tx) = tx {
            let _ = tx.send(());
        }
        Ok(json!({"name": name, "killed": true}).to_string())
    }

    fn push_line(&self, name: &str, line: String) {
        if let Ok(mut inner) = self.inner.lock() {
            if let Some(slot) = inner.slots.get_mut(name) {
                if slot.log.len() >= LOG_CAP {
                    slot.log.pop_front();
                }
                slot.log.push_back(line);
            }
        }
    }

    fn mark_dead(&self, name: &str, detail: &str) {
        if let Ok(mut inner) = self.inner.lock() {
            if let Some(slot) = inner.slots.get_mut(name) {
                slot.alive = false;
                slot.detail = detail.to_string();
                slot.kill_tx = None;
            }
        }
    }

    fn notify_model_exit(&self, name: &str, detail: &str) {
        if self.shutting_down.load(Ordering::Relaxed) {
            return;
        }
        let Some(tx) = &self.model_tx else {
            return;
        };
        let log = match self.inner.lock() {
            Ok(inner) => inner
                .slots
                .get(name)
                .map(|s| {
                    s.log
                        .iter()
                        .rev()
                        .take(LOG_TAIL)
                        .cloned()
                        .collect::<Vec<_>>()
                        .into_iter()
                        .rev()
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default(),
            Err(_) => Vec::new(),
        };
        let mut text = format!(
            "{EXIT_NOTICE_PREFIX}\nname: {name}\ndetail: {detail}\nThis is a system notice, not a user message. Inspect or restart if the task still needs this process."
        );
        if !log.is_empty() {
            text.push_str("\nlog:\n");
            text.push_str(&log.join("\n"));
        }
        let _ = tx.send(UserTurn::from(text));
    }

    fn take_name(&self, requested: Option<&str>) -> Result<String> {
        let mut inner = self.inner.lock().unwrap();
        let name = match requested.map(str::trim).filter(|s| !s.is_empty()) {
            Some(raw) => {
                let name = sanitize_name(raw)?;
                if inner.slots.get(&name).is_some_and(|s| s.alive) {
                    return Err(Error::Tool(format!("background {name} already running")));
                }
                name
            }
            None => loop {
                let n = format!("bg-{}", inner.next_id);
                inner.next_id += 1;
                if !inner.slots.contains_key(&n) {
                    break n;
                }
            },
        };
        let replacing_dead = inner.slots.get(&name).is_some_and(|s| !s.alive);
        let alive = inner.slots.values().filter(|s| s.alive).count();
        if alive >= MAX_BACKGROUNDS && !replacing_dead {
            return Err(Error::Tool(format!(
                "run_background blocked: max {MAX_BACKGROUNDS} processes"
            )));
        }
        Ok(name)
    }

    pub async fn shutdown(&self) -> Vec<ClosedBackground> {
        let closed = self.begin_shutdown();
        self.wait_shutdown().await;
        closed
    }

    /// Mark shutting down, snapshot still-alive processes, and send kill. Persist the snapshot before awaiting wait.
    pub fn begin_shutdown(&self) -> Vec<ClosedBackground> {
        self.shutting_down.store(true, Ordering::Relaxed);
        let closed = self.snapshot_alive();
        let mut kills = Vec::new();
        if let Ok(mut inner) = self.inner.lock() {
            for (_, slot) in inner.slots.iter_mut() {
                if let Some(tx) = slot.kill_tx.take() {
                    kills.push(tx);
                }
            }
        }
        for tx in kills {
            let _ = tx.send(());
        }
        if self.parent_run_id.is_none() && !closed.is_empty() {
            if let Ok(store) = crate::session::SessionStore::open() {
                let _ = store.save_closed_backgrounds(&self.run_id, &closed);
            }
        }
        closed
    }

    pub async fn wait_shutdown(&self) {
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    }

    fn snapshot_alive(&self) -> Vec<ClosedBackground> {
        let Ok(inner) = self.inner.lock() else {
            return Vec::new();
        };
        inner
            .slots
            .iter()
            .filter(|(_, s)| s.alive)
            .map(|(name, s)| ClosedBackground {
                name: name.clone(),
                command: s.command.clone(),
                log: s
                    .log
                    .iter()
                    .rev()
                    .take(LOG_TAIL)
                    .cloned()
                    .collect::<Vec<_>>()
                    .into_iter()
                    .rev()
                    .collect(),
            })
            .collect()
    }
}

impl Drop for BackgroundHub {
    fn drop(&mut self) {
        if !self.shutting_down.load(Ordering::Relaxed) {
            let _ = self.begin_shutdown();
        }
    }
}

fn sanitize_name(raw: &str) -> Result<String> {
    if raw.len() > 40 {
        return Err(Error::Tool("background name too long".into()));
    }
    if !raw
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        return Err(Error::Tool(
            "background name must be ascii alphanumeric, dash, or underscore".into(),
        ));
    }
    Ok(raw.to_string())
}

async fn pump_lines<R: tokio::io::AsyncRead + Unpin + Send>(
    pipe: Option<R>,
    stream: &'static str,
    name: String,
    hub: Arc<BackgroundHub>,
    sink: Arc<dyn EventSink>,
) {
    let Some(pipe) = pipe else {
        return;
    };
    let mut lines = BufReader::new(pipe).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        hub.push_line(&name, format!("{stream} {line}"));
        sink.emit(&AgentEvent::BackgroundOutput {
            meta: hub.meta(),
            name: name.clone(),
            stream: stream.to_string(),
            text: line,
        });
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

pub struct RunBackgroundTool {
    hub: Arc<BackgroundHub>,
    sink: Arc<dyn EventSink>,
    guard: Option<Arc<dyn CommandReviewer>>,
    windows: Option<Arc<WindowHub>>,
}

impl RunBackgroundTool {
    pub fn new(
        hub: Arc<BackgroundHub>,
        sink: Arc<dyn EventSink>,
        guard: Option<Arc<dyn CommandReviewer>>,
    ) -> Self {
        Self {
            hub,
            sink,
            guard,
            windows: None,
        }
    }

    pub fn with_windows(mut self, windows: Arc<WindowHub>) -> Self {
        self.windows = Some(windows);
        self
    }
}

impl ClientTool for RunBackgroundTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "run_background".into(),
            description: "Start a workspace shell command in the background and return immediately. Use for servers, watchers, and other long-running programs. Inspect with read_background; stop with kill_background. When the process exits, you receive a system notice and are called again. Do not use this for short commands — use run_command. If the command will open a GUI, set window to a short label; the result includes that window's pid so screenshot can target it.".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "command": {"type": "string", "description": "Shell command (cwd = workspace unless cwd is set)"},
                    "name": {"type": "string", "description": "Optional process name (ascii, dash, underscore)"},
                    "cwd": {"type": "string", "description": "Optional subdirectory inside the workspace"},
                    "window": {"type": "string", "description": "If this command opens a GUI, a short name (ascii, dash, underscore). Result includes windows[].pid; pass the same name to screenshot."}
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
        let name = args.get("name").and_then(Value::as_str).map(str::to_string);
        let cwd = args.get("cwd").and_then(Value::as_str).map(str::to_string);
        let window = args.get("window").and_then(Value::as_str).map(str::to_string);
        let hub = self.hub.clone();
        let sink = self.sink.clone();
        let guard = self.guard.clone();
        let windows = self.windows.clone();
        Box::pin(async move {
            shellguard::enforce(guard.as_ref(), &command, cwd.as_deref().unwrap_or(".")).await?;
            let window = wintrack::optional_name(window.as_deref())?;
            let before = if window.is_some() {
                wintrack::snapshot().await
            } else {
                Default::default()
            };
            let raw = hub.start(&command, name.as_deref(), cwd.as_deref(), sink)?;
            let mut body: Value = serde_json::from_str(&raw).unwrap_or_else(|_| json!({}));
            if let Some(label) = window {
                let found = wintrack::watch(before).await;
                let bound = match &windows {
                    Some(h) => wintrack::bind_appeared(h, &label, &found),
                    None => wintrack::label_appeared(&label, &found),
                };
                wintrack::attach_to(&mut body, &bound);
            }
            Ok(body.to_string())
        })
    }
}

pub struct ReadBackgroundTool {
    hub: Arc<BackgroundHub>,
}

impl ReadBackgroundTool {
    pub fn new(hub: Arc<BackgroundHub>) -> Self {
        Self { hub }
    }
}

impl ClientTool for ReadBackgroundTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "read_background".into(),
            description: "Read status and recent stdout/stderr of a background process started with run_background.".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "name": {"type": "string", "description": "Process name returned by run_background"}
                },
                "required": ["name"],
                "additionalProperties": false
            }),
        }
    }

    fn call(&self, args: &Value) -> ToolCallFut<'_> {
        let name = args
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let hub = self.hub.clone();
        Box::pin(async move { hub.snapshot(&name) })
    }
}

pub struct KillBackgroundTool {
    hub: Arc<BackgroundHub>,
}

impl KillBackgroundTool {
    pub fn new(hub: Arc<BackgroundHub>) -> Self {
        Self { hub }
    }
}

impl ClientTool for KillBackgroundTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "kill_background".into(),
            description: "Stop a background process started with run_background.".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "name": {"type": "string"}
                },
                "required": ["name"],
                "additionalProperties": false
            }),
        }
    }

    fn call(&self, args: &Value) -> ToolCallFut<'_> {
        let name = args
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let hub = self.hub.clone();
        Box::pin(async move { hub.kill(&name) })
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

    fn echo_cmd() -> &'static str {
        #[cfg(windows)]
        {
            "echo hello-bg"
        }
        #[cfg(not(windows))]
        {
            "printf 'hello-bg\\n'"
        }
    }

    fn hub(dir: &std::path::Path) -> Arc<BackgroundHub> {
        BackgroundHub::new(dir.to_path_buf(), "root".into(), "r".into(), None, None)
    }

    #[test]
    fn alive_pids_empty_until_a_process_starts() {
        let dir = tempfile::tempdir().unwrap();
        let h = hub(dir.path());
        assert!(h.alive_pids().is_empty());
    }

    #[tokio::test]
    async fn start_captures_output_then_exits() {
        let dir = tempfile::tempdir().unwrap();
        let hub = hub(dir.path());
        let rec = Arc::new(Rec(Mutex::new(Vec::new())));
        let sink: Arc<dyn EventSink> = rec.clone();
        let out = hub.start(echo_cmd(), Some("echo"), None, sink).unwrap();
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["name"], "echo");
        assert_eq!(v["alive"], true);

        let mut got_out = false;
        let mut exited = false;
        for _ in 0..40 {
            tokio::time::sleep(Duration::from_millis(50)).await;
            let evs = rec.0.lock().unwrap().clone();
            got_out = evs.iter().any(|e| {
                matches!(e, AgentEvent::BackgroundOutput { text, .. } if text.contains("hello-bg"))
            });
            exited = evs
                .iter()
                .any(|e| matches!(e, AgentEvent::BackgroundExited { name, .. } if name == "echo"));
            if got_out && exited {
                break;
            }
        }
        assert!(got_out, "stdout never arrived: {:?}", rec.0.lock().unwrap());
        assert!(exited, "BackgroundExited never arrived: {:?}", rec.0.lock().unwrap());
        let snap: Value = serde_json::from_str(&hub.snapshot("echo").unwrap()).unwrap();
        assert_eq!(snap["alive"], false);
        let log = snap["log"].as_array().unwrap();
        assert!(
            log.iter().any(|l| l.as_str().unwrap_or("").contains("hello-bg")),
            "{log:?}"
        );
    }

    #[tokio::test]
    async fn kill_stops_a_hanging_process() {
        let dir = tempfile::tempdir().unwrap();
        let hub = hub(dir.path());
        let rec = Arc::new(Rec(Mutex::new(Vec::new())));
        let sink: Arc<dyn EventSink> = rec.clone();
        hub.start(hang_cmd(), Some("hang"), None, sink).unwrap();
        let pids = hub.alive_pids();
        assert_eq!(pids.len(), 1, "{pids:?}");
        assert!(pids[0] > 0);
        let killed: Value = serde_json::from_str(&hub.kill("hang").unwrap()).unwrap();
        assert_eq!(killed["killed"], true);
        let mut exited = false;
        for _ in 0..40 {
            tokio::time::sleep(Duration::from_millis(50)).await;
            if rec.0.lock().unwrap().iter().any(|e| {
                matches!(e, AgentEvent::BackgroundExited { name, detail, .. } if name == "hang" && detail.contains("killed"))
            }) {
                exited = true;
                break;
            }
        }
        assert!(exited, "kill never produced BackgroundExited");
        let snap: Value = serde_json::from_str(&hub.snapshot("hang").unwrap()).unwrap();
        assert_eq!(snap["alive"], false);
        assert!(hub.alive_pids().is_empty(), "{:?}", hub.alive_pids());
    }

    #[tokio::test]
    async fn empty_command_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let hub = hub(dir.path());
        let rec: Arc<dyn EventSink> = Arc::new(Rec(Mutex::new(Vec::new())));
        let err = hub.start("  ", None, None, rec).unwrap_err();
        assert!(err.to_string().contains("command is required"), "{err}");
    }

    #[tokio::test]
    async fn fifth_alive_blocked_until_one_exits() {
        let dir = tempfile::tempdir().unwrap();
        let hub = hub(dir.path());
        let rec: Arc<dyn EventSink> = Arc::new(Rec(Mutex::new(Vec::new())));
        for i in 1..=4 {
            hub.start(hang_cmd(), Some(&format!("hang{i}")), None, rec.clone())
                .unwrap();
        }
        let err = hub
            .start(hang_cmd(), Some("hang5"), None, rec.clone())
            .unwrap_err();
        assert!(err.to_string().contains("max"), "{err}");
        hub.kill("hang1").unwrap();
        for _ in 0..40 {
            tokio::time::sleep(Duration::from_millis(50)).await;
            let snap: Value = serde_json::from_str(&hub.snapshot("hang1").unwrap()).unwrap();
            if snap["alive"] == false {
                break;
            }
        }
        let out = hub
            .start(hang_cmd(), Some("hang5"), None, rec.clone())
            .unwrap();
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["name"], "hang5");
        hub.shutdown().await;
    }

    #[tokio::test]
    async fn dead_name_can_be_reused() {
        let dir = tempfile::tempdir().unwrap();
        let hub = hub(dir.path());
        let rec: Arc<dyn EventSink> = Arc::new(Rec(Mutex::new(Vec::new())));
        hub.start(echo_cmd(), Some("echo"), None, rec.clone()).unwrap();
        for _ in 0..40 {
            tokio::time::sleep(Duration::from_millis(50)).await;
            let snap: Value = serde_json::from_str(&hub.snapshot("echo").unwrap()).unwrap();
            if snap["alive"] == false {
                break;
            }
        }
        let out = hub.start(echo_cmd(), Some("echo"), None, rec).unwrap();
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["name"], "echo");
        assert_eq!(v["alive"], true);
        hub.shutdown().await;
    }

    #[tokio::test]
    async fn exit_sends_a_notice_to_the_model_channel() {
        let dir = tempfile::tempdir().unwrap();
        let (tx, mut rx) = mpsc::unbounded_channel();
        let hub = BackgroundHub::new(
            dir.path().to_path_buf(),
            "root".into(),
            "r".into(),
            None,
            Some(tx),
        );
        let rec: Arc<dyn EventSink> = Arc::new(Rec(Mutex::new(Vec::new())));
        hub.start(echo_cmd(), Some("echo"), None, rec).unwrap();
        let notice = tokio::time::timeout(Duration::from_secs(3), rx.recv())
            .await
            .expect("timed out waiting for model notice")
            .expect("channel closed");
        assert!(notice.text.contains(EXIT_NOTICE_PREFIX), "{}", notice.text);
        assert!(notice.text.contains("name: echo"), "{}", notice.text);
        assert!(notice.text.contains("detail:"), "{}", notice.text);
        assert!(notice.text.contains("hello-bg"), "{}", notice.text);
    }

    #[tokio::test]
    async fn shutdown_does_not_notify_the_model() {
        let dir = tempfile::tempdir().unwrap();
        let (tx, mut rx) = mpsc::unbounded_channel();
        let hub = BackgroundHub::new(
            dir.path().to_path_buf(),
            "root".into(),
            "r".into(),
            None,
            Some(tx),
        );
        let rec: Arc<dyn EventSink> = Arc::new(Rec(Mutex::new(Vec::new())));
        hub.start(hang_cmd(), Some("hang"), None, rec).unwrap();
        hub.shutdown().await;
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert!(
            rx.try_recv().is_err(),
            "shutdown must not call the model: {:?}",
            rx.try_recv()
        );
    }

    #[tokio::test]
    async fn begin_shutdown_snapshots_alive_processes() {
        let dir = tempfile::tempdir().unwrap();
        let hub = hub(dir.path());
        let rec: Arc<dyn EventSink> = Arc::new(Rec(Mutex::new(Vec::new())));
        hub.start(hang_cmd(), Some("hang"), None, rec).unwrap();
        let closed = hub.begin_shutdown();
        assert_eq!(closed.len(), 1);
        assert_eq!(closed[0].name, "hang");
        assert!(closed[0].command.contains("ping") || closed[0].command.contains("sleep"));
        let notice = format_closed_notice(&closed);
        assert!(notice.contains(CLOSED_NOTICE_PREFIX), "{notice}");
        assert!(
            notice.contains("killed because the conversation was closed"),
            "{notice}"
        );
        hub.wait_shutdown().await;
    }

    #[tokio::test]
    async fn begin_shutdown_skips_already_dead_processes() {
        let dir = tempfile::tempdir().unwrap();
        let hub = hub(dir.path());
        let rec: Arc<dyn EventSink> = Arc::new(Rec(Mutex::new(Vec::new())));
        hub.start(echo_cmd(), Some("echo"), None, rec).unwrap();
        for _ in 0..40 {
            tokio::time::sleep(Duration::from_millis(50)).await;
            let snap: Value = serde_json::from_str(&hub.snapshot("echo").unwrap()).unwrap();
            if snap["alive"] == false {
                break;
            }
        }
        let closed = hub.begin_shutdown();
        assert!(closed.is_empty(), "{closed:?}");
        hub.wait_shutdown().await;
    }
}
