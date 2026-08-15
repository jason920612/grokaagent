//! Countdown timers the model can arm: blocking or background, notify or run a command.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{json, Value};
use tokio::sync::mpsc;
use tokio::task::AbortHandle;
use tokio::time::Instant;

use crate::agent::UserTurn;
use crate::error::{Error, Result};
use crate::events::{AgentEvent, EventMeta, EventSink};
use crate::shellguard::CommandReviewer;
use crate::tools::{run_workspace_command, ClientTool, ToolCallFut, ToolSpec};

pub const FIRED_NOTICE_PREFIX: &str = "[timer fired]";
pub const MAX_TIMERS: usize = 8;
pub const MAX_SECONDS: u64 = 86_400;
pub const MIN_SECONDS: u64 = 1;

struct Slot {
    seconds: u64,
    command: Option<String>,
    deadline: Instant,
    abort: Option<AbortHandle>,
    alive: bool,
}

struct Inner {
    slots: HashMap<String, Slot>,
    next_id: u32,
}

pub struct TimerHub {
    workspace: std::path::PathBuf,
    agent_name: String,
    run_id: String,
    parent_run_id: Option<String>,
    inner: Mutex<Inner>,
    model_tx: Option<mpsc::UnboundedSender<UserTurn>>,
    shutting_down: AtomicBool,
}

impl TimerHub {
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

    pub fn list(&self) -> String {
        let now = Instant::now();
        let inner = self.inner.lock().unwrap();
        let timers: Vec<Value> = inner
            .slots
            .iter()
            .map(|(name, slot)| {
                let remaining = if slot.alive {
                    slot.deadline.saturating_duration_since(now).as_secs()
                } else {
                    0
                };
                json!({
                    "name": name,
                    "seconds": slot.seconds,
                    "remaining": remaining,
                    "command": slot.command,
                    "alive": slot.alive,
                })
            })
            .collect();
        json!({ "timers": timers, "count": timers.len() }).to_string()
    }

    pub fn cancel(&self, name: &str, sink: &dyn EventSink) -> Result<String> {
        let name = name.trim();
        if name.is_empty() {
            return Err(Error::Tool("name is required".into()));
        }
        let abort = {
            let mut inner = self.inner.lock().unwrap();
            let slot = inner
                .slots
                .get_mut(name)
                .ok_or_else(|| Error::Tool(format!("no timer named {name}")))?;
            if !slot.alive {
                return Ok(json!({"name": name, "cancelled": false, "alive": false}).to_string());
            }
            slot.alive = false;
            slot.abort.take()
        };
        if let Some(h) = abort {
            h.abort();
        }
        sink.emit(&AgentEvent::TimerCancelled {
            meta: self.meta(),
            name: name.to_string(),
        });
        Ok(json!({"name": name, "cancelled": true}).to_string())
    }

    pub fn shutdown(&self) {
        self.shutting_down.store(true, Ordering::Relaxed);
        let mut aborts = Vec::new();
        if let Ok(mut inner) = self.inner.lock() {
            for slot in inner.slots.values_mut() {
                if let Some(h) = slot.abort.take() {
                    aborts.push(h);
                }
                slot.alive = false;
            }
        }
        for h in aborts {
            h.abort();
        }
    }

    fn take_name(&self, requested: Option<&str>) -> Result<String> {
        let mut inner = self.inner.lock().unwrap();
        let name = match requested.map(str::trim).filter(|s| !s.is_empty()) {
            Some(raw) => {
                let name = sanitize_name(raw)?;
                if inner.slots.get(&name).is_some_and(|s| s.alive) {
                    return Err(Error::Tool(format!("timer {name} already running")));
                }
                name
            }
            None => loop {
                let n = format!("timer-{}", inner.next_id);
                inner.next_id += 1;
                if !inner.slots.contains_key(&n) {
                    break n;
                }
            },
        };
        let replacing_dead = inner.slots.get(&name).is_some_and(|s| !s.alive);
        let alive = inner.slots.values().filter(|s| s.alive).count();
        if alive >= MAX_TIMERS && !replacing_dead {
            return Err(Error::Tool(format!(
                "timer blocked: max {MAX_TIMERS} timers"
            )));
        }
        Ok(name)
    }

    fn mark_dead(&self, name: &str) {
        if let Ok(mut inner) = self.inner.lock() {
            if let Some(slot) = inner.slots.get_mut(name) {
                slot.alive = false;
                slot.abort = None;
            }
        }
    }

    fn notify_fired(&self, name: &str, seconds: u64, command: Option<&str>, result: Option<&Value>) {
        if self.shutting_down.load(Ordering::Relaxed) {
            return;
        }
        let Some(tx) = &self.model_tx else {
            return;
        };
        let mut text = format!(
            "{FIRED_NOTICE_PREFIX}\nname: {name}\nseconds: {seconds}\nThis is a system notice, not a user message."
        );
        if let Some(cmd) = command {
            text.push_str("\ncommand: ");
            text.push_str(cmd);
        }
        if let Some(v) = result {
            if let Some(code) = v.get("exit_code") {
                text.push_str(&format!("\nexit_code: {code}"));
            }
            if v.get("timed_out").and_then(Value::as_bool) == Some(true) {
                text.push_str("\ntimed_out: true");
            }
            if let Some(stdout) = v.get("stdout").and_then(Value::as_str) {
                if !stdout.is_empty() {
                    text.push_str("\nstdout:\n");
                    text.push_str(stdout);
                }
            }
            if let Some(stderr) = v.get("stderr").and_then(Value::as_str) {
                if !stderr.trim().is_empty() {
                    text.push_str("\nstderr:\n");
                    text.push_str(stderr);
                }
            }
        }
        let _ = tx.send(UserTurn::from(text));
    }

    fn emit_file_changes(&self, sink: &dyn EventSink, output: &Value) {
        let Some(files) = output.get("files").and_then(Value::as_array) else {
            return;
        };
        for item in files {
            let path = item
                .get("path")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            if path.is_empty() {
                continue;
            }
            sink.emit(&AgentEvent::FileChanged {
                meta: self.meta(),
                path,
                kind: item
                    .get("kind")
                    .and_then(Value::as_str)
                    .unwrap_or("modify")
                    .to_string(),
                diff: item
                    .get("diff")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string(),
            });
        }
    }
}

fn sanitize_name(raw: &str) -> Result<String> {
    if raw.len() > 40 {
        return Err(Error::Tool("timer name too long".into()));
    }
    if !raw
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        return Err(Error::Tool(
            "timer name must be ascii alphanumeric, dash, or underscore".into(),
        ));
    }
    Ok(raw.to_string())
}

fn parse_seconds(args: &Value) -> Result<u64> {
    let v = args
        .get("seconds")
        .ok_or_else(|| Error::Tool("seconds is required".into()))?;
    let n = if let Some(u) = v.as_u64() {
        u
    } else if let Some(f) = v.as_f64() {
        if !f.is_finite() || f < MIN_SECONDS as f64 {
            return Err(Error::Tool(format!(
                "seconds must be an integer {MIN_SECONDS}–{MAX_SECONDS}"
            )));
        }
        f.floor() as u64
    } else {
        return Err(Error::Tool("seconds must be a number".into()));
    };
    if n < MIN_SECONDS || n > MAX_SECONDS {
        return Err(Error::Tool(format!(
            "seconds must be {MIN_SECONDS}–{MAX_SECONDS}"
        )));
    }
    Ok(n)
}

fn optional_command(args: &Value) -> Option<String> {
    args.get("command")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

fn merge_fire_payload(seconds: u64, name: &str, command: Option<&str>, cmd_json: Option<Value>) -> String {
    let mut v = json!({
        "fired": true,
        "name": name,
        "seconds": seconds,
        "block": true,
    });
    if let Some(cmd) = command {
        v["command"] = json!(cmd);
    }
    if let Some(out) = cmd_json {
        if let Some(obj) = out.as_object() {
            for (k, val) in obj {
                v[k] = val.clone();
            }
        }
    }
    v.to_string()
}

pub struct TimerTool {
    hub: Arc<TimerHub>,
    sink: Arc<dyn EventSink>,
    guard: Option<Arc<dyn CommandReviewer>>,
}

impl TimerTool {
    pub fn new(
        hub: Arc<TimerHub>,
        sink: Arc<dyn EventSink>,
        guard: Option<Arc<dyn CommandReviewer>>,
    ) -> Self {
        Self { hub, sink, guard }
    }

    async fn start(&self, args: &Value) -> Result<String> {
        let seconds = parse_seconds(args)?;
        let block = args.get("block").and_then(Value::as_bool).unwrap_or(false);
        let command = optional_command(args);
        let cwd = args
            .get("cwd")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string);
        if let Some(cmd) = command.as_deref() {
            crate::shellguard::enforce(
                self.guard.as_ref(),
                cmd,
                cwd.as_deref().unwrap_or("."),
            )
            .await?;
        }
        if block {
            return self.run_blocking(seconds, command.as_deref(), cwd.as_deref()).await;
        }
        self.arm_background(seconds, command, cwd, args.get("name").and_then(Value::as_str))
    }

    async fn run_blocking(
        &self,
        seconds: u64,
        command: Option<&str>,
        cwd: Option<&str>,
    ) -> Result<String> {
        tokio::time::sleep(Duration::from_secs(seconds)).await;
        if self.hub.shutting_down.load(Ordering::Relaxed) {
            return Err(Error::Tool("timer cancelled".into()));
        }
        let cmd_json = match command {
                Some(cmd) => {
                let raw = run_workspace_command(
                    &self.hub.workspace,
                    cmd,
                    cwd,
                    None,
                )
                .await?;
                Some(serde_json::from_str(&raw).unwrap_or(json!({"raw": raw})))
            }
            None => None,
        };
        Ok(merge_fire_payload(seconds, "blocking", command, cmd_json))
    }

    fn arm_background(
        &self,
        seconds: u64,
        command: Option<String>,
        cwd: Option<String>,
        requested_name: Option<&str>,
    ) -> Result<String> {
        let name = self.hub.take_name(requested_name)?;
        let deadline = Instant::now() + Duration::from_secs(seconds);
        let hub = self.hub.clone();
        let sink = self.sink.clone();
        let wait_name = name.clone();
        let wait_cmd = command.clone();
        let wait_cwd = cwd.clone();
        let wait_secs = seconds;
        let handle = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(wait_secs)).await;
            if hub.shutting_down.load(Ordering::Relaxed) {
                return;
            }
            let cmd_json = match wait_cmd.as_deref() {
                Some(cmd) => match run_workspace_command(
                    &hub.workspace,
                    cmd,
                    wait_cwd.as_deref(),
                    None,
                )
                .await
                {
                    Ok(raw) => serde_json::from_str(&raw).unwrap_or(json!({"raw": raw})),
                    Err(e) => json!({"error": e.to_string()}),
                },
                None => json!({}),
            };
            if hub.shutting_down.load(Ordering::Relaxed) {
                return;
            }
            hub.mark_dead(&wait_name);
            if wait_cmd.is_some() {
                hub.emit_file_changes(sink.as_ref(), &cmd_json);
            }
            let result = if wait_cmd.is_some() {
                Some(&cmd_json)
            } else {
                None
            };
            hub.notify_fired(&wait_name, wait_secs, wait_cmd.as_deref(), result);
            let detail = if wait_cmd.is_some() {
                cmd_json.to_string()
            } else {
                "notified".into()
            };
            sink.emit(&AgentEvent::TimerFired {
                meta: hub.meta(),
                name: wait_name,
                detail,
            });
        });
        {
            let mut inner = self.hub.inner.lock().unwrap();
            inner.slots.insert(
                name.clone(),
                Slot {
                    seconds,
                    command: command.clone(),
                    deadline,
                    abort: Some(handle.abort_handle()),
                    alive: true,
                },
            );
        }
        let fires_at = chrono::Utc::now() + chrono::Duration::seconds(seconds as i64);
        self.sink.emit(&AgentEvent::TimerStarted {
            meta: self.hub.meta(),
            name: name.clone(),
            seconds,
            command: command.clone().unwrap_or_default(),
        });
        Ok(json!({
            "name": name,
            "seconds": seconds,
            "block": false,
            "command": command,
            "alive": true,
            "fires_at": fires_at.to_rfc3339(),
        })
        .to_string())
    }
}

impl ClientTool for TimerTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "timer".into(),
            description: "Set a countdown timer. action=start with seconds (1–86400). block=true waits in this tool call until it fires; block=false (default) returns immediately and later sends a [timer fired] system notice. Optional command runs in the workspace when the timer fires (same review rules as run_command); omit command to only notify. action=list shows running timers; action=cancel stops a named background timer.".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "action": {
                        "type": "string",
                        "enum": ["start", "list", "cancel"],
                        "description": "start a timer, list timers, or cancel a background timer"
                    },
                    "seconds": {
                        "type": "number",
                        "description": "Countdown length in seconds (1–86400). Required for start."
                    },
                    "block": {
                        "type": "boolean",
                        "description": "If true, wait here until the timer fires. If false (default), run in the background and notify later."
                    },
                    "command": {
                        "type": "string",
                        "description": "Optional workspace shell command to run when the timer fires. Omit to only notify."
                    },
                    "cwd": {
                        "type": "string",
                        "description": "Optional subdirectory inside the workspace for command"
                    },
                    "name": {
                        "type": "string",
                        "description": "Optional timer name (ascii, dash, underscore). Required for cancel."
                    }
                },
                "required": ["action"],
                "additionalProperties": false
            }),
        }
    }

    fn call(&self, args: &Value) -> ToolCallFut<'_> {
        let args = args.clone();
        Box::pin(async move {
            let action = args
                .get("action")
                .and_then(Value::as_str)
                .unwrap_or("start");
            match action {
                "list" => Ok(self.hub.list()),
                "cancel" => {
                    let name = args.get("name").and_then(Value::as_str).unwrap_or("");
                    self.hub.cancel(name, self.sink.as_ref())
                }
                "start" => self.start(&args).await,
                other => Err(Error::Tool(format!("unknown timer action {other}"))),
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::future::Future;
    use std::pin::Pin;

    struct Rec(Mutex<Vec<AgentEvent>>);
    impl EventSink for Rec {
        fn emit(&self, event: &AgentEvent) {
            self.0.lock().unwrap().push(event.clone());
        }
    }

    struct DenyAll;
    impl CommandReviewer for DenyAll {
        fn review<'a>(
            &'a self,
            _command: &'a str,
            _cwd: &'a str,
        ) -> Pin<Box<dyn Future<Output = Result<crate::shellguard::Verdict>> + Send + 'a>> {
            Box::pin(async {
                Ok(crate::shellguard::Verdict::Deny {
                    reasons: vec!["nope".into()],
                })
            })
        }
    }

    fn echo_cmd() -> &'static str {
        #[cfg(windows)]
        {
            "echo hello-timer"
        }
        #[cfg(not(windows))]
        {
            "printf 'hello-timer\\n'"
        }
    }

    fn tool(dir: &std::path::Path, tx: Option<mpsc::UnboundedSender<UserTurn>>) -> (TimerTool, Arc<Rec>) {
        let rec = Arc::new(Rec(Mutex::new(Vec::new())));
        let hub = TimerHub::new(
            dir.to_path_buf(),
            "root".into(),
            "r".into(),
            None,
            tx,
        );
        let sink: Arc<dyn EventSink> = rec.clone();
        (
            TimerTool::new(hub, sink, None),
            rec,
        )
    }

    #[tokio::test]
    async fn rejects_zero_and_huge_seconds() {
        let dir = tempfile::tempdir().unwrap();
        let (tool, _) = tool(dir.path(), None);
        let err = tool
            .call(&json!({"action":"start","seconds":0,"block":true}))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("seconds"), "{err}");
        let err = tool
            .call(&json!({"action":"start","seconds":86_401,"block":true}))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("seconds"), "{err}");
    }

    #[tokio::test]
    async fn blocking_notify_only_waits_then_returns() {
        let dir = tempfile::tempdir().unwrap();
        let (tool, rec) = tool(dir.path(), None);
        let started = std::time::Instant::now();
        let out: Value = serde_json::from_str(
            &tool
                .call(&json!({"action":"start","seconds":1,"block":true}))
                .await
                .unwrap(),
        )
        .unwrap();
        assert!(started.elapsed() >= Duration::from_millis(900), "{:?}", started.elapsed());
        assert_eq!(out["fired"], true);
        assert_eq!(out["seconds"], 1);
        assert!(out.get("stdout").is_none());
        assert!(
            rec.0
                .lock()
                .unwrap()
                .iter()
                .all(|e| !matches!(e, AgentEvent::TimerStarted { .. })),
            "blocking timers stay on the tool call, not the background rail"
        );
    }

    #[tokio::test]
    async fn blocking_runs_command_and_returns_stdout() {
        let dir = tempfile::tempdir().unwrap();
        let (tool, _) = tool(dir.path(), None);
        let out: Value = serde_json::from_str(
            &tool
                .call(&json!({
                    "action":"start",
                    "seconds":1,
                    "block":true,
                    "command": echo_cmd()
                }))
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(out["fired"], true);
        let stdout = out["stdout"].as_str().unwrap();
        assert!(stdout.contains("hello-timer"), "{stdout}");
        assert_eq!(out["exit_code"], 0);
    }

    #[tokio::test]
    async fn background_notify_wakes_model_channel() {
        let dir = tempfile::tempdir().unwrap();
        let (tx, mut rx) = mpsc::unbounded_channel();
        let (tool, rec) = tool(dir.path(), Some(tx));
        let out: Value = serde_json::from_str(
            &tool
                .call(&json!({"action":"start","seconds":1,"block":false,"name":"n1"}))
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(out["name"], "n1");
        assert_eq!(out["alive"], true);
        assert!(
            rec.0
                .lock()
                .unwrap()
                .iter()
                .any(|e| matches!(e, AgentEvent::TimerStarted { name, seconds, .. } if name == "n1" && *seconds == 1))
        );
        let notice = tokio::time::timeout(Duration::from_secs(3), rx.recv())
            .await
            .expect("timer notice timed out")
            .expect("channel closed");
        assert!(notice.text.contains(FIRED_NOTICE_PREFIX), "{}", notice.text);
        assert!(notice.text.contains("n1"), "{}", notice.text);
        assert!(!notice.text.contains("command:"), "{}", notice.text);
        assert!(
            rec.0
                .lock()
                .unwrap()
                .iter()
                .any(|e| matches!(e, AgentEvent::TimerFired { name, .. } if name == "n1"))
        );
        let listed: Value = serde_json::from_str(&tool.call(&json!({"action":"list"})).await.unwrap()).unwrap();
        assert_eq!(listed["timers"][0]["alive"], false);
    }

    #[tokio::test]
    async fn background_runs_command_then_notifies_with_stdout() {
        let dir = tempfile::tempdir().unwrap();
        let (tx, mut rx) = mpsc::unbounded_channel();
        let (tool, rec) = tool(dir.path(), Some(tx));
        tool.call(&json!({
            "action":"start",
            "seconds":1,
            "name":"job",
            "command": echo_cmd()
        }))
        .await
        .unwrap();
        let notice = tokio::time::timeout(Duration::from_secs(5), rx.recv())
            .await
            .expect("timer notice timed out")
            .expect("channel closed");
        assert!(notice.text.contains(FIRED_NOTICE_PREFIX), "{}", notice.text);
        assert!(notice.text.contains("hello-timer"), "{}", notice.text);
        assert!(notice.text.contains("command:"), "{}", notice.text);
        assert!(
            rec.0
                .lock()
                .unwrap()
                .iter()
                .any(|e| matches!(e, AgentEvent::TimerFired { name, .. } if name == "job"))
        );
    }

    #[tokio::test]
    async fn cancel_prevents_fire() {
        let dir = tempfile::tempdir().unwrap();
        let (tx, mut rx) = mpsc::unbounded_channel();
        let (tool, rec) = tool(dir.path(), Some(tx));
        tool.call(&json!({"action":"start","seconds":2,"name":"later"}))
            .await
            .unwrap();
        let out: Value = serde_json::from_str(
            &tool
                .call(&json!({"action":"cancel","name":"later"}))
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(out["cancelled"], true);
        let leftover = tokio::time::timeout(Duration::from_millis(2500), rx.recv()).await;
        assert!(leftover.is_err(), "cancelled timer must not notify");
        assert!(
            rec.0
                .lock()
                .unwrap()
                .iter()
                .any(|e| matches!(e, AgentEvent::TimerCancelled { name, .. } if name == "later"))
        );
        assert!(
            rec.0
                .lock()
                .unwrap()
                .iter()
                .all(|e| !matches!(e, AgentEvent::TimerFired { .. }))
        );
    }

    #[tokio::test]
    async fn blocked_command_fails_before_waiting() {
        let dir = tempfile::tempdir().unwrap();
        let rec = Arc::new(Rec(Mutex::new(Vec::new())));
        let hub = TimerHub::new(dir.path().to_path_buf(), "root".into(), "r".into(), None, None);
        let tool = TimerTool::new(hub, rec, Some(Arc::new(DenyAll)));
        let command = if cfg!(windows) {
            "echo pwned>pwned.txt & echo x"
        } else {
            "echo pwned > pwned.txt && echo x"
        };
        let started = std::time::Instant::now();
        let err = tool
            .call(&json!({
                "action":"start",
                "seconds":3600,
                "block":true,
                "command": command
            }))
            .await
            .unwrap_err();
        assert!(started.elapsed() < Duration::from_secs(2), "must not sleep before deny");
        assert!(err.to_string().contains("blocked"), "{err}");
        assert!(!dir.path().join("pwned.txt").exists());
    }

    #[tokio::test]
    async fn caps_concurrent_background_timers() {
        let dir = tempfile::tempdir().unwrap();
        let (tool, _) = tool(dir.path(), None);
        for i in 0..MAX_TIMERS {
            tool.call(&json!({"action":"start","seconds":60,"name": format!("t{i}")}))
                .await
                .unwrap();
        }
        let err = tool
            .call(&json!({"action":"start","seconds":60,"name":"overflow"}))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("max"), "{err}");
        for i in 0..MAX_TIMERS {
            tool.call(&json!({"action":"cancel","name": format!("t{i}")}))
                .await
                .unwrap();
        }
    }
}
