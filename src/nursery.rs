//! Parent-side child process manager: spawn A2A workers and talk Message/Task.
//!
//! Each child is a long-lived agent session in its own process. The parent
//! tails the child's event file, relays every event up (path-prefixed), and
//! tracks the child's own state from its events. When a child goes idle its
//! reply is pushed into the parent's session as a notification, unless a
//! `wait_agents` call is already blocked on that child and takes it instead.

use std::collections::{HashMap, VecDeque};
use std::io::SeekFrom;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use std::time::Duration;

use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncSeekExt, BufReader};
use tokio::process::Command;
use tokio::sync::{mpsc, oneshot, Notify};

use crate::a2a::{A2aClient, Handshake};
use crate::agent::{CancelFlag, UserTurn};
use crate::error::{Error, Result};
use crate::events::{AgentEvent, EventMeta, EventSink};
use crate::procgroup::ProcessGuard;
use crate::tools::{ClientTool, ToolCallFut, ToolSpec};

pub const DEFAULT_MAX_DEPTH: u32 = 2;
pub const DEFAULT_MAX_CHILDREN: usize = 4;
/// Default and ceiling for `wait_agents`.
pub const DEFAULT_WAIT: Duration = Duration::from_secs(300);
pub const MAX_WAIT: Duration = Duration::from_secs(3600);
/// Replies longer than this are cut when handed to the parent model.
const REPLY_CAP: usize = 20_000;
const STDERR_TAIL: usize = 30;
const TAIL_TICK: Duration = Duration::from_millis(60);
/// Check the child process for exit every this many tail ticks.
const EXIT_CHECK_EVERY: u32 = 8;
const CHILD_DIR: &str = "groka-children";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChildState {
    Starting,
    Working,
    Idle,
    /// Stopped without a final reply (error streak or turn budget).
    Paused,
    Exited,
}

impl ChildState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Starting => "starting",
            Self::Working => "working",
            Self::Idle => "idle",
            Self::Paused => "paused",
            Self::Exited => "exited",
        }
    }

    fn busy(self) -> bool {
        matches!(self, Self::Starting | Self::Working)
    }
}

struct Entry {
    run_id: String,
    model: String,
    origin: String,
    last_task: String,
    state: ChildState,
    /// Last reply from an idle child.
    reply: String,
    /// Pause reason, last error, or exit detail.
    note: String,
    /// Reply/pause not yet handed to the parent model.
    unread: bool,
    /// We asked the child to stop its current turn; its next idle is not a reply.
    interrupting: bool,
    last_stop: Option<String>,
    stderr: VecDeque<String>,
    proc: Arc<tokio::sync::Mutex<ProcessGuard>>,
    stop_watch: Option<oneshot::Sender<()>>,
}

#[derive(Default)]
struct Shared {
    children: Mutex<HashMap<String, Entry>>,
    changed: Notify,
    /// Children a running `wait_agents` call is blocked on (name → callers).
    waiting_for: Mutex<HashMap<String, usize>>,
}

impl Shared {
    fn kids(&self) -> MutexGuard<'_, HashMap<String, Entry>> {
        self.children.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn waited(&self, name: &str) -> bool {
        self.waiting_for
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(name)
            .is_some_and(|n| *n > 0)
    }
}

/// Where a finished child's news goes when no `wait_agents` is blocked on it.
type Notifier = mpsc::UnboundedSender<UserTurn>;

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
    /// Live session knobs; `child_model` there overrides [`Self::model`] as
    /// the default for new children. `None` for workers and one-shot runs.
    knobs: Option<Arc<std::sync::Mutex<crate::agent::SessionKnobs>>>,
    client: A2aClient,
    shared: Arc<Shared>,
    notify: OnceLock<Notifier>,
    /// The sink the tools emit to, for events raised outside a tool call.
    sink: OnceLock<Arc<dyn EventSink>>,
    stop_follow: Mutex<Option<oneshot::Sender<()>>>,
}

/// The per-child model override from spawn args, or the parent's model.
fn child_model(args: &Value, parent_model: &str) -> String {
    args.get("model")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| parent_model.to_string())
}

fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.chars().count() <= 40
        && name
            .chars()
            .all(|c| c.is_alphanumeric() || c == '-' || c == '_')
}

fn cap(text: &str) -> String {
    if text.len() <= REPLY_CAP {
        return text.to_string();
    }
    let mut end = REPLY_CAP;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}\n…(cut {} bytes)", &text[..end], text.len() - end)
}

fn str_arg<'a>(args: &'a Value, key: &str) -> Option<&'a str> {
    args.get(key).and_then(Value::as_str).map(str::trim)
}

fn names_arg(args: &Value) -> Option<Vec<String>> {
    let v = args.get("names")?;
    let list: Vec<String> = match v {
        Value::Array(a) => a
            .iter()
            .filter_map(Value::as_str)
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect(),
        Value::String(s) if !s.trim().is_empty() => vec![s.trim().to_string()],
        _ => Vec::new(),
    };
    (!list.is_empty()).then_some(list)
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
        knobs: Option<Arc<std::sync::Mutex<crate::agent::SessionKnobs>>>,
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
            knobs,
            client: A2aClient::new()?,
            shared: Arc::new(Shared::default()),
            notify: OnceLock::new(),
            sink: OnceLock::new(),
            stop_follow: Mutex::new(None),
        }))
    }

    /// Route finished children's replies into the parent session (its
    /// background-notice channel), so an idle parent wakes up for them.
    pub fn set_notify(&self, tx: Notifier) {
        let _ = self.notify.set(tx);
    }

    /// Interrupt every working child whenever the parent's Esc flag trips.
    pub fn follow_cancel(self: &Arc<Self>, cancel: CancelFlag) {
        let (tx, mut rx) = oneshot::channel();
        *self.stop_follow.lock().unwrap_or_else(|e| e.into_inner()) = Some(tx);
        let weak = Arc::downgrade(self);
        tokio::spawn(async move {
            let mut seen = cancel.trips();
            loop {
                tokio::select! {
                    _ = &mut rx => break,
                    n = cancel.wait_trip(seen) => seen = n,
                }
                let Some(n) = weak.upgrade() else { break };
                n.interrupt_busy().await;
            }
        });
    }

    /// The session's configured child model, or this nursery's parent model.
    fn default_child_model(&self) -> String {
        self.knobs
            .as_ref()
            .map(|k| {
                k.lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .child_model
                    .trim()
                    .to_string()
            })
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| self.model.clone())
    }

    pub fn child_names(&self) -> Vec<String> {
        let mut v: Vec<String> = self.shared.kids().keys().cloned().collect();
        v.sort();
        v
    }

    fn meta(&self) -> EventMeta {
        EventMeta {
            ts: chrono::Utc::now(),
            agent_name: self.agent_name.clone(),
            run_id: self.parent_run_id.clone(),
            parent_run_id: None,
            path: String::new(),
        }
    }

    fn child_dir(&self) -> PathBuf {
        if self.events_dir.file_name().is_some_and(|n| n == CHILD_DIR) {
            self.events_dir.clone()
        } else {
            self.events_dir.join(CHILD_DIR)
        }
    }

    pub async fn spawn_agent(&self, args: &Value, sink: Arc<dyn EventSink>) -> Result<String> {
        if self.depth >= self.max_depth {
            return Err(Error::Tool(format!(
                "spawn_agent blocked: depth {} >= max_depth {}",
                self.depth, self.max_depth
            )));
        }
        let name = str_arg(args, "name")
            .ok_or_else(|| Error::Tool("spawn_agent requires name".into()))?
            .to_string();
        if !valid_name(&name) {
            return Err(Error::Tool(
                "spawn_agent name must be 1-40 letters, digits, - or _".into(),
            ));
        }
        let prompt = args
            .get("prompt")
            .and_then(Value::as_str)
            .ok_or_else(|| Error::Tool("spawn_agent requires prompt".into()))?;
        let model = child_model(args, &self.default_child_model());
        self.remember_sink(&sink);
        {
            let kids = self.shared.kids();
            if kids.contains_key(&name) {
                return Err(Error::Tool(format!(
                    "child {name} already exists; send_message to it, or stop_agent kill=true first"
                )));
            }
            if kids.len() >= self.max_children {
                return Err(Error::Tool(format!(
                    "spawn_agent blocked: max_children {} alive; stop_agent kill=true a finished child first",
                    self.max_children
                )));
            }
        }

        let run_id = uuid::Uuid::new_v4().to_string();
        let dir = self.child_dir();
        std::fs::create_dir_all(&dir)?;
        let events = dir.join(format!("{name}-{}.jsonl", &run_id[..8]));
        let mut cmd = Command::new(&self.worker_bin);
        cmd.arg("worker")
            .arg("--name")
            .arg(&name)
            .arg("--depth")
            .arg((self.depth + 1).to_string())
            .arg("--parent-run")
            .arg(&self.parent_run_id)
            .arg("--run-id")
            .arg(&run_id)
            .arg("--listen")
            .arg("127.0.0.1:0")
            .arg("--events")
            .arg(&events)
            .arg("--mode")
            .arg(&self.mode)
            .arg("--workspace")
            .arg(&self.workspace)
            .arg("--model")
            .arg(&model)
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
        let stderr = guard.child_mut().stderr.take();
        let mut lines = BufReader::new(stdout).lines();
        let line = tokio::time::timeout(Duration::from_secs(8), lines.next_line())
            .await
            .map_err(|_| Error::A2a("worker handshake timed out".into()))?
            .map_err(|e| Error::A2a(format!("read handshake: {e}")))?
            .ok_or_else(|| Error::A2a("worker closed stdout before handshake".into()))?;
        let handshake = Handshake::parse_line(&line)?;
        let origin = handshake.origin()?;

        // Drain leftover stdout so the pipe cannot fill.
        tokio::spawn(async move { while let Ok(Some(_)) = lines.next_line().await {} });

        let (stop_tx, stop_rx) = oneshot::channel();
        self.shared.kids().insert(
            name.clone(),
            Entry {
                run_id: run_id.clone(),
                model: model.clone(),
                origin: origin.clone(),
                last_task: String::new(),
                state: ChildState::Starting,
                reply: String::new(),
                note: String::new(),
                unread: false,
                interrupting: false,
                last_stop: None,
                stderr: VecDeque::new(),
                proc: Arc::new(tokio::sync::Mutex::new(guard)),
                stop_watch: Some(stop_tx),
            },
        );
        if let Some(err) = stderr {
            tokio::spawn(keep_stderr(self.shared.clone(), name.clone(), err));
        }

        sink.emit(&AgentEvent::ChildSpawned {
            meta: self.meta(),
            name: name.clone(),
            agent_card_url: handshake.agent_card_url.clone(),
            prompt: prompt.to_string(),
            model: model.clone(),
        });
        self.emit_message(sink.as_ref(), &self.agent_name, &name, prompt);

        tokio::spawn(watch_child(
            Watch {
                shared: self.shared.clone(),
                name: name.clone(),
                run_id,
                parent_run_id: self.parent_run_id.clone(),
                parent_name: self.agent_name.clone(),
                meta: self.meta(),
                notify: self.notify.get().cloned(),
                sink,
            },
            events,
            stop_rx,
        ));

        match self.client.send_text(&origin, prompt, None).await {
            Ok(task) => {
                if let Some(e) = self.shared.kids().get_mut(&name) {
                    e.last_task = task
                        .get("id")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string();
                }
            }
            Err(e) => {
                self.remove(&name, "failed to send the first message").await;
                return Err(e);
            }
        }

        Ok(json!({
            "name": name,
            "state": "working",
            "model": model,
            "note": "Child started and is working in its own session. Its reply comes back to you automatically as a message when it goes idle. Meanwhile keep working, or call wait_agents to block for it. send_message follows up (it keeps its memory); stop_agent interrupts or kills it.",
        })
        .to_string())
    }

    pub async fn send_message(&self, args: &Value, sink: &dyn EventSink) -> Result<String> {
        let name = str_arg(args, "name")
            .ok_or_else(|| Error::Tool("send_message requires name".into()))?
            .to_string();
        let text = args
            .get("text")
            .and_then(Value::as_str)
            .ok_or_else(|| Error::Tool("send_message requires text".into()))?;
        let found = {
            let mut kids = self.shared.kids();
            kids.get_mut(&name).map(|e| {
                let was = e.state;
                e.state = ChildState::Working;
                e.unread = false;
                e.last_stop = None;
                (e.origin.clone(), was)
            })
        };
        // `unknown` locks the map itself, so it runs after the guard is gone.
        let (origin, was) = found.ok_or_else(|| self.unknown(&name))?;
        self.emit_message(sink, &self.agent_name, &name, text);
        let task = self.client.send_text(&origin, text, None).await?;
        if let Some(e) = self.shared.kids().get_mut(&name) {
            e.last_task = task
                .get("id")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
        }
        self.shared.changed.notify_waiters();
        let note = if was.busy() {
            "Delivered while the child was working; it reads it before its next step. Its reply comes back as a message when it goes idle."
        } else {
            "Delivered; the child is working on it. Its reply comes back as a message when it goes idle, or call wait_agents."
        };
        Ok(json!({"name": name, "state": "working", "note": note}).to_string())
    }

    /// Block until the named children (default: all) are not working, or the timeout.
    pub async fn wait_agents(&self, args: &Value) -> Result<String> {
        let all = self.child_names();
        if all.is_empty() {
            return Ok(json!({"agents": [], "note": "no child agents"}).to_string());
        }
        let targets = match names_arg(args) {
            Some(list) => {
                if let Some(bad) = list.iter().find(|n| !all.contains(n)) {
                    return Err(self.unknown(bad));
                }
                list
            }
            None => all,
        };
        let secs = args
            .get("timeout_seconds")
            .and_then(Value::as_u64)
            .map(Duration::from_secs)
            .unwrap_or(DEFAULT_WAIT)
            .min(MAX_WAIT);
        let deadline = tokio::time::Instant::now() + secs;
        let _hold = WaitHold::new(self.shared.clone(), self.notify.get().cloned(), &targets);
        loop {
            let notified = self.shared.changed.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            let busy = {
                let kids = self.shared.kids();
                targets
                    .iter()
                    .any(|n| kids.get(n).is_some_and(|e| e.state.busy()))
            };
            if !busy {
                return Ok(self.report(&targets, false));
            }
            tokio::select! {
                _ = notified => {}
                _ = tokio::time::sleep_until(deadline) => return Ok(self.report(&targets, true)),
            }
        }
    }

    fn report(&self, targets: &[String], timed_out: bool) -> String {
        let mut kids = self.shared.kids();
        let mut out = Vec::new();
        for n in targets {
            match kids.get_mut(n) {
                Some(e) => {
                    let mut item = json!({"name": n, "state": e.state.as_str()});
                    if !e.state.busy() {
                        if !e.reply.is_empty() {
                            item["reply"] = json!(cap(&e.reply));
                        }
                        e.unread = false;
                    }
                    if !e.note.is_empty() {
                        item["note"] = json!(e.note);
                    }
                    out.push(item);
                }
                None => out.push(json!({"name": n, "state": "exited"})),
            }
        }
        let mut v = json!({"agents": out});
        if timed_out {
            v["timed_out"] = json!(true);
            v["note"] = json!("Some children are still working. They keep running; their replies arrive as messages.");
        }
        v.to_string()
    }

    pub fn list_agents(&self) -> String {
        let kids = self.shared.kids();
        let mut names: Vec<&String> = kids.keys().collect();
        names.sort();
        let items: Vec<Value> = names
            .into_iter()
            .map(|n| {
                let e = &kids[n];
                let preview: String = e.reply.chars().take(300).collect();
                json!({
                    "name": n,
                    "state": e.state.as_str(),
                    "model": e.model,
                    "run_id": e.run_id,
                    "reply_preview": preview,
                    "unread": e.unread,
                    "note": e.note,
                })
            })
            .collect();
        json!({
            "agents": items,
            "max_children": self.max_children,
            "depth": self.depth,
            "max_depth": self.max_depth,
        })
        .to_string()
    }

    pub async fn stop_agent(&self, args: &Value) -> Result<String> {
        let name = str_arg(args, "name")
            .ok_or_else(|| Error::Tool("stop_agent requires name".into()))?
            .to_string();
        if !self.shared.kids().contains_key(&name) {
            return Err(self.unknown(&name));
        }
        let kill = args.get("kill").and_then(Value::as_bool).unwrap_or(false);
        if kill {
            self.remove(&name, "killed by parent").await;
            return Ok(json!({"name": name, "state": "exited"}).to_string());
        }
        let interrupted = self.interrupt(&name).await;
        Ok(json!({
            "name": name,
            "state": if interrupted { "interrupting" } else { "idle" },
            "note": if interrupted {
                "The child stops its current turn and stays alive with its memory. send_message resumes it."
            } else {
                "The child was not working."
            },
        })
        .to_string())
    }

    /// Ask a working child to drop its current turn. `false` if it was not working.
    async fn interrupt(&self, name: &str) -> bool {
        let (origin, task) = {
            let mut kids = self.shared.kids();
            let Some(e) = kids.get_mut(name) else {
                return false;
            };
            if !e.state.busy() || e.last_task.is_empty() {
                return false;
            }
            e.interrupting = true;
            (e.origin.clone(), e.last_task.clone())
        };
        let _ = self.client.cancel_task(&origin, &task).await;
        true
    }

    async fn interrupt_busy(&self) {
        for name in self.child_names() {
            self.interrupt(&name).await;
        }
    }

    /// Kill and forget one child.
    async fn remove(&self, name: &str, detail: &str) {
        let entry = self.shared.kids().remove(name);
        let Some(mut e) = entry else { return };
        if let Some(tx) = e.stop_watch.take() {
            let _ = tx.send(());
        }
        let _ = e.proc.lock().await.kill().await;
        self.shared.changed.notify_waiters();
        self.exited_event(name, detail);
    }

    fn exited_event(&self, name: &str, detail: &str) {
        if let Some(sink) = self.last_sink() {
            sink.emit(&AgentEvent::ChildExited {
                meta: self.meta(),
                name: name.to_string(),
                detail: detail.to_string(),
            });
        }
    }

    fn unknown(&self, name: &str) -> Error {
        let names = self.child_names();
        if names.is_empty() {
            Error::Tool(format!("no child named {name}; there are no children"))
        } else {
            Error::Tool(format!(
                "no child named {name}; children: {}",
                names.join(", ")
            ))
        }
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
        if let Some(tx) = self
            .stop_follow
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
        {
            let _ = tx.send(());
        }
        let drained: Vec<(String, Entry)> = self.shared.kids().drain().collect();
        for (name, mut e) in drained {
            if let Some(tx) = e.stop_watch.take() {
                let _ = tx.send(());
            }
            let _ = e.proc.lock().await.kill().await;
            sink.emit(&AgentEvent::ChildExited {
                meta: self.meta(),
                name,
                detail: "killed".into(),
            });
        }
        self.shared.changed.notify_waiters();
    }

    fn last_sink(&self) -> Option<Arc<dyn EventSink>> {
        self.sink.get().cloned()
    }

    fn remember_sink(&self, sink: &Arc<dyn EventSink>) {
        let _ = self.sink.set(sink.clone());
    }
}

/// Registers the targets of a `wait_agents` call so finished replies are
/// returned by the call instead of pushed as notifications. On drop (return
/// or cancellation) anything still unread is pushed after all.
struct WaitHold {
    shared: Arc<Shared>,
    notify: Option<Notifier>,
    names: Vec<String>,
}

impl WaitHold {
    fn new(shared: Arc<Shared>, notify: Option<Notifier>, names: &[String]) -> Self {
        {
            let mut w = shared.waiting_for.lock().unwrap_or_else(|e| e.into_inner());
            for n in names {
                *w.entry(n.clone()).or_default() += 1;
            }
        }
        Self {
            shared,
            notify,
            names: names.to_vec(),
        }
    }
}

impl Drop for WaitHold {
    fn drop(&mut self) {
        let mut released = Vec::new();
        {
            let mut w = self
                .shared
                .waiting_for
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            for n in &self.names {
                if let Some(c) = w.get_mut(n) {
                    *c = c.saturating_sub(1);
                    if *c == 0 {
                        w.remove(n);
                        released.push(n.clone());
                    }
                }
            }
        }
        let Some(tx) = &self.notify else { return };
        let mut kids = self.shared.kids();
        for n in released {
            if let Some(e) = kids.get_mut(&n) {
                if e.unread {
                    e.unread = false;
                    let _ = tx.send(UserTurn::from(news_text(&n, e)));
                }
            }
        }
    }
}

fn news_text(name: &str, e: &Entry) -> String {
    match e.state {
        ChildState::Paused => format!(
            "[child agent `{name}` paused without a final reply: {}. send_message to continue it, or stop_agent kill=true to end it.]",
            if e.note.is_empty() { "no detail" } else { &e.note }
        ),
        ChildState::Exited => format!("[child agent `{name}` exited: {}]", e.note),
        _ => format!(
            "[child agent `{name}` replied and is now idle]\n{}",
            cap(&e.reply)
        ),
    }
}

async fn keep_stderr(
    shared: Arc<Shared>,
    name: String,
    err: tokio::process::ChildStderr,
) {
    let mut lines = BufReader::new(err).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        let mut kids = shared.kids();
        let Some(e) = kids.get_mut(&name) else { continue };
        if e.stderr.len() >= STDERR_TAIL {
            e.stderr.pop_front();
        }
        e.stderr.push_back(line);
    }
}

struct Watch {
    shared: Arc<Shared>,
    name: String,
    run_id: String,
    parent_run_id: String,
    parent_name: String,
    meta: EventMeta,
    notify: Option<Notifier>,
    sink: Arc<dyn EventSink>,
}

impl Watch {
    /// Update the child's state from one of its own events. Returns news for the parent.
    fn apply_own(&self, ev: &AgentEvent) -> Option<String> {
        let mut kids = self.shared.kids();
        let e = kids.get_mut(&self.name)?;
        match ev {
            AgentEvent::TurnStarted { .. } => {
                e.state = ChildState::Working;
                e.last_stop = None;
                e.note.clear();
                None
            }
            AgentEvent::ModelFinished { text, finish, .. } if finish == "stop" => {
                e.last_stop = Some(text.clone());
                None
            }
            AgentEvent::Error { message, .. } => {
                e.note = message.clone();
                None
            }
            AgentEvent::AwaitingInput { .. } => {
                let interrupted = std::mem::take(&mut e.interrupting);
                match e.last_stop.take() {
                    Some(text) => {
                        e.state = ChildState::Idle;
                        e.reply = text;
                        e.note.clear();
                    }
                    None if interrupted => {
                        e.state = ChildState::Idle;
                        e.note = "interrupted".into();
                        return None;
                    }
                    None => {
                        e.state = ChildState::Paused;
                        if e.note.is_empty() {
                            e.note = "turn budget reached or request kept failing".into();
                        }
                    }
                }
                // An interrupted child's reply is kept but does not wake the parent.
                e.unread = true;
                (!interrupted).then(|| news_text(&self.name, e))
            }
            AgentEvent::RunFinished { reason, text, .. } => {
                e.state = if reason == "stop" {
                    ChildState::Idle
                } else {
                    ChildState::Paused
                };
                if !text.is_empty() {
                    e.reply = text.clone();
                }
                if reason != "stop" {
                    e.note = format!("session ended: {reason}");
                }
                e.unread = true;
                Some(news_text(&self.name, e))
            }
            _ => None,
        }
    }

    fn deliver(&self, news: String) {
        let reply = {
            let kids = self.shared.kids();
            kids.get(&self.name)
                .filter(|e| e.state == ChildState::Idle)
                .map(|e| e.reply.clone())
        };
        if let Some(text) = reply.filter(|t| !t.is_empty()) {
            self.sink.emit(&AgentEvent::AgentMessage {
                meta: EventMeta {
                    ts: chrono::Utc::now(),
                    ..self.meta.clone()
                },
                from: self.name.clone(),
                to: self.parent_name.clone(),
                text,
            });
        }
        if !self.shared.waited(&self.name) {
            if let Some(tx) = &self.notify {
                if tx.send(UserTurn::from(news)).is_ok() {
                    if let Some(e) = self.shared.kids().get_mut(&self.name) {
                        e.unread = false;
                    }
                }
            }
        }
        self.shared.changed.notify_waiters();
    }

    fn handle_line(&self, line: &str) {
        let Ok(ev) = serde_json::from_str::<AgentEvent>(line) else {
            return;
        };
        let own = ev.run_id() == self.run_id && ev.path().is_empty();
        let news = if own { self.apply_own(&ev) } else { None };
        if own {
            self.shared.changed.notify_waiters();
        }
        self.sink
            .emit(&ev.relayed(&self.name, &self.parent_run_id));
        if let Some(n) = news {
            self.deliver(n);
        }
    }

    /// The child process is gone: record why and tell the parent.
    fn exited(&self, detail: String) {
        let Some(mut e) = self.shared.kids().remove(&self.name) else {
            return;
        };
        e.state = ChildState::Exited;
        e.note = detail.clone();
        self.sink.emit(&AgentEvent::ChildExited {
            meta: EventMeta {
                ts: chrono::Utc::now(),
                ..self.meta.clone()
            },
            name: self.name.clone(),
            detail: detail.clone(),
        });
        if let Some(tx) = &self.notify {
            let _ = tx.send(UserTurn::from(news_text(&self.name, &e)));
        }
        self.shared.changed.notify_waiters();
    }
}

async fn watch_child(w: Watch, path: PathBuf, mut stop: oneshot::Receiver<()>) {
    let mut offset = 0u64;
    let mut leftover = String::new();
    let mut tick = 0u32;
    loop {
        tokio::select! {
            _ = &mut stop => break,
            _ = tokio::time::sleep(TAIL_TICK) => {}
        }
        read_new(&path, &mut offset, &mut leftover, |l| w.handle_line(l)).await;
        tick += 1;
        if tick % EXIT_CHECK_EVERY != 0 {
            continue;
        }
        let proc = match w.shared.kids().get(&w.name) {
            Some(e) => e.proc.clone(),
            None => break,
        };
        let status = match proc.try_lock() {
            Ok(mut g) => g.child_mut().try_wait().ok().flatten(),
            Err(_) => None,
        };
        if let Some(status) = status {
            read_new(&path, &mut offset, &mut leftover, |l| w.handle_line(l)).await;
            let tail: Vec<String> = w
                .shared
                .kids()
                .get(&w.name)
                .map(|e| e.stderr.iter().rev().take(5).rev().cloned().collect())
                .unwrap_or_default();
            let mut detail = format!("process exited ({status})");
            if !tail.is_empty() {
                detail.push_str(": ");
                detail.push_str(&tail.join(" / "));
            }
            w.exited(detail);
            break;
        }
    }
}

async fn read_new(path: &Path, offset: &mut u64, leftover: &mut String, mut each: impl FnMut(&str)) {
    let Ok(mut f) = tokio::fs::OpenOptions::new().read(true).open(path).await else {
        return;
    };
    if f.seek(SeekFrom::Start(*offset)).await.is_err() {
        return;
    }
    let mut chunk = Vec::new();
    if f.read_to_end(&mut chunk).await.is_err() || chunk.is_empty() {
        return;
    }
    // Only consume whole lines so a UTF-8 sequence is never split.
    let Some(last_nl) = chunk.iter().rposition(|b| *b == b'\n') else {
        return;
    };
    let whole = &chunk[..=last_nl];
    *offset += whole.len() as u64;
    leftover.push_str(&String::from_utf8_lossy(whole));
    let text = std::mem::take(leftover);
    for line in text.lines() {
        let line = line.trim();
        if !line.is_empty() {
            each(line);
        }
    }
}

#[derive(Clone, Copy)]
enum Kind {
    Spawn,
    Send,
    Wait,
    List,
    Stop,
}

/// One of the child-management tools, all backed by the same [`Nursery`].
pub struct AgentTool {
    kind: Kind,
    nursery: Arc<Nursery>,
    sink: Arc<dyn EventSink>,
}

impl AgentTool {
    /// Every child-management tool for this nursery.
    pub fn all(nursery: Arc<Nursery>, sink: Arc<dyn EventSink>) -> Vec<Box<dyn ClientTool>> {
        nursery.remember_sink(&sink);
        [Kind::Spawn, Kind::Send, Kind::Wait, Kind::List, Kind::Stop]
            .into_iter()
            .map(|kind| {
                Box::new(Self {
                    kind,
                    nursery: nursery.clone(),
                    sink: sink.clone(),
                }) as Box<dyn ClientTool>
            })
            .collect()
    }
}

impl ClientTool for AgentTool {
    fn spec(&self) -> ToolSpec {
        match self.kind {
            Kind::Spawn => ToolSpec {
                name: "spawn_agent".into(),
                description: "Start a child agent in its own process and session. Use for a separable subtask with a verifiable done condition. Returns as soon as the child starts; its reply comes back to you automatically as a message when it goes idle. The child keeps its memory between messages until you kill it.".into(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "name": {"type": "string", "description": "Short unique child name: letters, digits, - or _"},
                        "prompt": {"type": "string", "description": "Full goal, paths, and done-criteria. The child has no parent context."},
                        "model": {"type": "string", "description": "Optional model id for this child (e.g. grok-4.6). Defaults to the parent's model."}
                    },
                    "required": ["name", "prompt"],
                    "additionalProperties": false
                }),
            },
            Kind::Send => ToolSpec {
                name: "send_message".into(),
                description: "Send a follow-up message to a child. Returns at once; the child continues in the same session (it remembers earlier messages) and its reply comes back as a message when it goes idle.".into(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "name": {"type": "string"},
                        "text": {"type": "string"}
                    },
                    "required": ["name", "text"],
                    "additionalProperties": false
                }),
            },
            Kind::Wait => ToolSpec {
                name: "wait_agents".into(),
                description: "Block until the named children (default: all) stop working, then return their replies. Use when you cannot continue without them. On timeout the children keep running.".into(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "names": {"type": "array", "items": {"type": "string"}, "description": "Children to wait for. Omit for all."},
                        "timeout_seconds": {"type": "integer", "description": "Default 300, max 3600."}
                    },
                    "additionalProperties": false
                }),
            },
            Kind::List => ToolSpec {
                name: "list_agents".into(),
                description: "List your child agents with state (starting/working/idle/paused), model, and a preview of each last reply.".into(),
                parameters: json!({"type": "object", "properties": {}, "additionalProperties": false}),
            },
            Kind::Stop => ToolSpec {
                name: "stop_agent".into(),
                description: "Interrupt a child's current turn (it stays alive with its memory), or kill=true to end its process and free its slot.".into(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "name": {"type": "string"},
                        "kill": {"type": "boolean", "description": "End the child process. Default false."}
                    },
                    "required": ["name"],
                    "additionalProperties": false
                }),
            },
        }
    }

    fn call(&self, args: &Value) -> ToolCallFut<'_> {
        let n = self.nursery.clone();
        let sink = self.sink.clone();
        let args = args.clone();
        let kind = self.kind;
        Box::pin(async move {
            match kind {
                Kind::Spawn => n.spawn_agent(&args, sink).await,
                Kind::Send => n.send_message(&args, sink.as_ref()).await,
                Kind::Wait => n.wait_agents(&args).await,
                Kind::List => Ok(n.list_agents()),
                Kind::Stop => n.stop_agent(&args).await,
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::FanoutSink;

    fn nursery(dir: &Path, depth: u32, knobs: Option<Arc<std::sync::Mutex<crate::agent::SessionKnobs>>>) -> Arc<Nursery> {
        Nursery::new(
            PathBuf::from("grokaagent"),
            dir.to_path_buf(),
            dir.to_path_buf(),
            depth,
            "r".into(),
            "root".into(),
            "grok-4.6".into(),
            "echo".into(),
            knobs,
        )
        .unwrap()
    }

    #[test]
    fn child_model_prefers_spawn_arg_and_falls_back_to_parent() {
        let args = json!({"name": "a", "prompt": "b", "model": " grok-3-mini "});
        assert_eq!(child_model(&args, "grok-4.6"), "grok-3-mini");
        let args = json!({"name": "a", "prompt": "b", "model": ""});
        assert_eq!(child_model(&args, "grok-4.6"), "grok-4.6");
        let args = json!({"name": "a", "prompt": "b"});
        assert_eq!(child_model(&args, "grok-4.6"), "grok-4.6");
    }

    #[test]
    fn default_child_model_reads_session_knobs() {
        let dir = std::env::temp_dir();
        let knobs = Arc::new(std::sync::Mutex::new(crate::agent::SessionKnobs {
            model: "grok-4.6".into(),
            reasoning_effort: crate::provider::ReasoningEffort::High,
            send_reasoning: true,
            server_tools: vec![],
            dispatcher: false,
            child_model: "grok-3-mini".into(),
        }));
        let n = nursery(&dir, 0, Some(knobs.clone()));
        assert_eq!(n.default_child_model(), "grok-3-mini");
        knobs.lock().unwrap().child_model = "  ".into();
        assert_eq!(n.default_child_model(), "grok-4.6", "blank must follow the parent model");
    }

    #[test]
    fn tools_cover_the_child_lifecycle() {
        let dir = std::env::temp_dir();
        let sink: Arc<dyn EventSink> = Arc::new(FanoutSink { sinks: vec![] });
        let names: Vec<String> = AgentTool::all(nursery(&dir, 0, None), sink)
            .iter()
            .map(|t| t.spec().name)
            .collect();
        assert_eq!(
            names,
            ["spawn_agent", "send_message", "wait_agents", "list_agents", "stop_agent"]
        );
    }

    #[test]
    fn spawn_spec_offers_optional_model() {
        let dir = std::env::temp_dir();
        let sink: Arc<dyn EventSink> = Arc::new(FanoutSink { sinks: vec![] });
        let spec = AgentTool::all(nursery(&dir, 0, None), sink)[0].spec();
        assert!(spec.parameters["properties"]["model"].is_object());
        assert!(!spec.parameters["required"]
            .as_array()
            .unwrap()
            .iter()
            .any(|v| v == "model"));
    }

    #[test]
    fn names_must_be_path_safe() {
        assert!(valid_name("coder"));
        assert!(valid_name("lint_2"));
        assert!(valid_name("測試"));
        assert!(!valid_name("a/b"));
        assert!(!valid_name("a b"));
        assert!(!valid_name(""));
        assert!(!valid_name("..\\x"));
    }

    #[tokio::test]
    async fn spawn_blocked_at_max_depth() {
        let dir = tempfile::tempdir().unwrap();
        let n = nursery(dir.path(), 2, None);
        let sink: Arc<dyn EventSink> = Arc::new(FanoutSink { sinks: vec![] });
        let err = n
            .spawn_agent(&json!({"name": "x", "prompt": "p"}), sink)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("max_depth"), "{err}");
    }

    #[tokio::test]
    async fn wait_with_no_children_returns_at_once() {
        let dir = tempfile::tempdir().unwrap();
        let n = nursery(dir.path(), 0, None);
        let out = n.wait_agents(&json!({})).await.unwrap();
        assert!(out.contains("no child agents"), "{out}");
        let err = n.send_message(&json!({"name": "ghost", "text": "hi"}), &FanoutSink { sinks: vec![] }).await.unwrap_err();
        assert!(err.to_string().contains("no child named ghost"), "{err}");
    }

    #[test]
    fn cap_cuts_on_a_char_boundary() {
        let long = "字".repeat(REPLY_CAP);
        let out = cap(&long);
        assert!(out.contains("…(cut"));
        assert!(out.len() < long.len());
    }

    struct Rec(std::sync::Mutex<Vec<AgentEvent>>);
    impl EventSink for Rec {
        fn emit(&self, event: &AgentEvent) {
            self.0.lock().unwrap().push(event.clone());
        }
    }

    fn ev_line(ev: &AgentEvent) -> String {
        format!("{}\n", serde_json::to_string(ev).unwrap())
    }

    fn own_meta(run_id: &str, path: &str) -> EventMeta {
        EventMeta {
            ts: chrono::Utc::now(),
            agent_name: "kid".into(),
            run_id: run_id.into(),
            parent_run_id: Some("r".into()),
            path: path.into(),
        }
    }

    /// Drive a watcher over a fake event file without a real process.
    #[tokio::test]
    async fn watcher_relays_with_path_and_delivers_idle_reply() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("kid.jsonl");
        std::fs::write(&path, "").unwrap();
        let shared = Arc::new(Shared::default());
        // A placeholder process: this test binary's sibling does not matter; use a
        // short-lived shell-free command that exists everywhere.
        let mut cmd = Command::new(std::env::current_exe().unwrap());
        cmd.arg("--list").stdout(std::process::Stdio::null());
        let guard = ProcessGuard::spawn(cmd).unwrap();
        shared.kids().insert(
            "kid".into(),
            Entry {
                run_id: "c1".into(),
                model: "m".into(),
                origin: String::new(),
                last_task: String::new(),
                state: ChildState::Starting,
                reply: String::new(),
                note: String::new(),
                unread: false,
                interrupting: false,
                last_stop: None,
                stderr: VecDeque::new(),
                proc: Arc::new(tokio::sync::Mutex::new(guard)),
                stop_watch: None,
            },
        );
        let rec = Arc::new(Rec(std::sync::Mutex::new(Vec::new())));
        let (tx, mut rx) = mpsc::unbounded_channel();
        let w = Watch {
            shared: shared.clone(),
            name: "kid".into(),
            run_id: "c1".into(),
            parent_run_id: "r".into(),
            parent_name: "root".into(),
            meta: own_meta("r", ""),
            notify: Some(tx),
            sink: rec.clone(),
        };
        let lines = [
            ev_line(&AgentEvent::TurnStarted { meta: own_meta("c1", ""), turn: 1 }),
            ev_line(&AgentEvent::ModelDelta { meta: own_meta("g1", "lint"), text: "from-grandchild".into() }),
            ev_line(&AgentEvent::ModelFinished {
                meta: own_meta("c1", ""),
                text: "all done".into(),
                finish: "stop".into(),
                input_tokens: 0,
                cached_tokens: 0,
            }),
            ev_line(&AgentEvent::AwaitingInput { meta: own_meta("c1", "") }),
        ];
        for l in &lines {
            w.handle_line(l.trim());
        }
        let got = rx.try_recv().expect("idle reply must reach the parent");
        assert!(got.text.contains("`kid` replied"), "{}", got.text);
        assert!(got.text.contains("all done"));
        let events = rec.0.lock().unwrap().clone();
        assert!(events.iter().any(|e| matches!(e,
            AgentEvent::ModelDelta { meta, text } if text == "from-grandchild" && meta.path == "kid/lint" && meta.parent_run_id.as_deref() == Some("r"))),
            "grandchild event must be relayed under kid/lint");
        assert!(events.iter().any(|e| matches!(e,
            AgentEvent::TurnStarted { meta, .. } if meta.path == "kid")));
        assert!(events.iter().any(|e| matches!(e,
            AgentEvent::AgentMessage { from, text, .. } if from == "kid" && text == "all done")));
        assert_eq!(shared.kids()["kid"].state, ChildState::Idle);
        assert!(!shared.kids()["kid"].unread, "delivered replies are read");
    }

    #[tokio::test]
    async fn interrupted_child_does_not_wake_the_parent() {
        let shared = Arc::new(Shared::default());
        let mut cmd = Command::new(std::env::current_exe().unwrap());
        cmd.arg("--list").stdout(std::process::Stdio::null());
        let guard = ProcessGuard::spawn(cmd).unwrap();
        shared.kids().insert(
            "kid".into(),
            Entry {
                run_id: "c1".into(),
                model: "m".into(),
                origin: String::new(),
                last_task: "t".into(),
                state: ChildState::Working,
                reply: String::new(),
                note: String::new(),
                unread: false,
                interrupting: true,
                last_stop: None,
                stderr: VecDeque::new(),
                proc: Arc::new(tokio::sync::Mutex::new(guard)),
                stop_watch: None,
            },
        );
        let (tx, mut rx) = mpsc::unbounded_channel();
        let w = Watch {
            shared: shared.clone(),
            name: "kid".into(),
            run_id: "c1".into(),
            parent_run_id: "r".into(),
            parent_name: "root".into(),
            meta: own_meta("r", ""),
            notify: Some(tx),
            sink: Arc::new(FanoutSink { sinks: vec![] }),
        };
        w.handle_line(ev_line(&AgentEvent::AwaitingInput { meta: own_meta("c1", "") }).trim());
        assert!(rx.try_recv().is_err());
        assert_eq!(shared.kids()["kid"].state, ChildState::Idle);
    }
}
