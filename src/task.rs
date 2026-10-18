//! Task mode: a user proxy. A separate model call (the task model, or the
//! main one) plays the user's part toward the worker: it writes the opening
//! brief, pushes at each stop, looks in mid-run when the worker seems stuck,
//! answers ask_user questions the user's own words already settle, and
//! decides when the work is done or should go back to the user.

use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::sync::Notify;

use crate::agent::SessionKnobs;
use crate::ask::{answer_from_picks, AskDelegate, Delegated, Question};
use crate::error::{Error, Result};
use crate::events::{AgentEvent, EventMeta, EventSink};
use crate::notice::USER_PROXY;
use crate::provider::{CompleteRequest, FunctionCall, Provider, ReasoningEffort};
use crate::tools::{ClientTool, ListDirTool, ReadFileTool, ToolCallFut, ToolSpec};

pub const AGENT_NAME: &str = "任務系統";
pub const KICK: &str = "[grokaagent task]";
pub const MAX_GOAL: usize = 2000;
pub const MAX_CRITERIA: usize = 2000;
pub const MAX_ITEMS: usize = 20;
/// Tool calls the proxy may make in one review before it must answer.
pub const MAX_PROXY_TOOL_CALLS: usize = 8;
/// Entries of the proxy's memory that are kept, and the characters of it
/// that go into one request.
const MAX_LOG: usize = 40;
const MEMORY_CHARS: usize = 12_000;
const LOG_ENTRY_CHARS: usize = 1_500;
const EXCERPT_CHARS: usize = 60_000;
const LAST_REPLY_CHARS: usize = 6_000;
const CALL_ARGS_CHARS: usize = 300;
const OUTPUT_HEAD: usize = 400;
const OUTPUT_TAIL: usize = 1_500;
const MESSAGE_CHARS: usize = 4_000;
const REASON_CHARS: usize = 800;
const NOTE_CHARS: usize = 200;
const ITEM_CHARS: usize = 200;
const TOOL_OUTPUT_CHARS: usize = 20_000;
const FOOTER_GOAL: usize = 600;
const FOOTER_CRITERIA: usize = 600;

pub fn is_kick(s: &str) -> bool {
    s.trim() == KICK
}

const PROXY_INSTRUCTIONS: &str = r#"You are the user's proxy in grokaagent task mode. The user set you up to act for them while a separate worker agent does the work. You are not the worker and you are not the user. You speak for the user, from what the user actually said: their goal, their acceptance criteria and preferences, and their own messages.

Get the goal done the way a demanding but fair user would:
- React to what the worker actually did and said. Name what is missing or wrong, ask pointed follow-up questions, and ask for verification (tests run and their output, files read back, the program actually started) before you accept that something works.
- Remember what you asked before (your memory). If the worker ignored a request or only claimed to do it, say so plainly and escalate: narrower, firmer, explicit about what you will accept.
- Answer the worker's questions when the goal, the criteria or the user's own words settle them. Do not invent requirements the user never stated, and do not decide trade-offs or preferences the user has not decided: that is the user's call.
- Keep messages specific and short, in the user's language (the language of the goal). One clear request beats a list of generic advice.
- Never claim you did work or saw evidence you did not see.

You can check things yourself with read-only tools: read_file, list_dir and workspace_changes (git status and diff stat). Use them when the conversation does not already show the evidence. At most 8 tool calls per review. The worker does not see your tool results. You cannot edit files or run commands; ask the worker for that.

Pick one status:
- continue: more work is needed. message says what you want next.
- satisfied: the goal and every criterion are met, and you have concrete evidence: test or command output in the conversation, a file you read, a diff. The worker's word alone is not evidence. reason names the evidence.
- hand_back: pushing further will not help. You need a decision only the user can make, progress has stalled repeatedly even after you changed approach, or the work is blocked on something only the user can do. reason says what the user needs to decide or do.
- impossible: an unchangeable constraint blocks the goal (credentials that cannot exist here, a legal or physical block, a workspace that cannot hold the result). Hard, slow or unfinished work is not impossible. reason names the constraint.

There is no round limit. You alone decide when pushing further stops helping, and you should not give up early: mistakes, retries and slow progress are normal. Before you hand back, change your approach at least once: a smaller step, a concrete check, a different angle.

The checklist is your own progress notes: 3-12 concrete, verifiable items covering the goal and the criteria. Keep the same wording across reviews. Items you leave out stay as they are. Mark an item done only on evidence. To drop an item, send it with "dropped": true and say why in note.

Reply with ONE JSON object and nothing else:
{"status":"continue"|"satisfied"|"hand_back"|"impossible","message":"...","checklist":[{"text":"...","done":false}],"note":"...","reason":"..."}
- message: what you say to the worker, in the user's language. Required on continue, except at a mid-run checkpoint, where an empty message lets the worker carry on.
- note: one short line for the user's screen about what you decided and why (Traditional Chinese is fine).
- reason: required for satisfied, hand_back and impossible.
"#;

const PROXY_ASK_INSTRUCTIONS: &str = r#"You are the user's proxy in grokaagent task mode: a separate model the user set up to act for them while a worker agent does the work. The worker asked the user a multiple-choice question. Decide whether you may answer it for the user.

Answer only when the user's goal, their criteria or their own earlier words already settle it. Forward it to the user when the answer needs anything new: a new requirement, a trade-off or preference the user has not decided, anything you would be guessing. Also forward anything about access, credentials, money, publishing or deleting data.

Reply with ONE JSON object and nothing else, either
{"forward": true, "why": "..."}
or
{"forward": false, "selected": [{"id": "<option id>", "value": "<text, only for an option marked input>"}], "why": "<the user's words that settle it>"}
Pick exactly one option unless the question allows several.
"#;

/// Starts the goal reminder under each proxy message.
const FOOTER_MARK: &str = "\n\n---\nGoal: ";

/// Opens every message the proxy sends the worker, after [`USER_PROXY`].
const PROXY_HEADER: &str = "Message from the user's proxy, a separate model the user set up to act for them in task mode. It is not the user. Treat its requests as the user's goal applied to your work. The user's own messages always take priority.";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CheckItem {
    pub text: String,
    #[serde(default)]
    pub done: bool,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TaskPhase {
    Inactive,
    NeedPlan,
    Active,
    ExplainingFail,
    Done,
    Failed,
    /// The proxy decided pushing further would not help and handed the
    /// task back to the user.
    HandedBack,
}

impl Default for TaskPhase {
    fn default() -> Self {
        Self::Inactive
    }
}

impl TaskPhase {
    pub fn is_live(self) -> bool {
        matches!(
            self,
            Self::NeedPlan | Self::Active | Self::ExplainingFail
        )
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Inactive => "未啟動",
            Self::NeedPlan => "規劃中",
            Self::Active => "進行中",
            Self::ExplainingFail => "無法完成",
            Self::Done => "已完成",
            Self::Failed => "已豁免",
            Self::HandedBack => "已交還",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct TaskState {
    #[serde(default)]
    pub review_note: String,
    #[serde(default)]
    pub goal: String,
    #[serde(default)]
    pub checklist: Vec<CheckItem>,
    #[serde(default)]
    pub phase: TaskPhase,
    #[serde(default)]
    pub claim: Option<String>,
    #[serde(default)]
    pub fail_reason: String,
    #[serde(default)]
    pub skip_steer: bool,
    #[serde(default)]
    pub spawned: bool,
    /// Optional acceptance criteria and preferences the user gave with the goal.
    #[serde(default)]
    pub criteria: String,
    /// The user paused the proxy: the worker is not pushed until resumed.
    #[serde(default)]
    pub paused: bool,
    /// The proxy's memory: what it said, what the worker answered, what the
    /// user changed. Oldest first, kept short.
    #[serde(default)]
    pub proxy_log: Vec<ProxyTurn>,
    /// Times the proxy has spoken to the worker in this task.
    #[serde(default)]
    pub rounds: u32,
    /// The proxy's latest message to the worker, for the UI.
    #[serde(default)]
    pub last_proxy: String,
}

/// One line of the proxy's memory.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ProxyTurn {
    /// `proxy`, `worker`, `user` or `note`.
    pub who: String,
    pub text: String,
}

impl TaskState {
    pub fn checklist_text(&self) -> String {
        if self.checklist.is_empty() {
            return "（尚無檢查表）".into();
        }
        self.checklist
            .iter()
            .map(|i| {
                format!(
                    "{} {}",
                    if i.done { "[x]" } else { "[ ]" },
                    i.text
                )
            })
            .collect::<Vec<_>>()
            .join("\n")
    }
}

/// Append to the proxy's memory, keeping the newest [`MAX_LOG`] entries.
fn push_log(state: &mut TaskState, who: &str, text: impl Into<String>) {
    let text = clip_ellipsis(&text.into(), 2_000);
    if text.is_empty() {
        return;
    }
    state.proxy_log.push(ProxyTurn { who: who.into(), text });
    let over = state.proxy_log.len().saturating_sub(MAX_LOG);
    if over > 0 {
        state.proxy_log.drain(..over);
    }
}

/// How the task ends once the worker has written its closing turn.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Ending {
    phase: TaskPhase,
    reason: String,
}

pub struct TaskHub {
    session_id: String,
    inner: Mutex<TaskState>,
    kick_pending: AtomicBool,
    kick: Notify,
    review_now: AtomicBool,
    /// The user resumed the proxy; the idle run takes a look.
    resumed: AtomicBool,
    /// Bumped when a task starts or ends, so a review that was running for
    /// the old one is dropped.
    epoch: AtomicU64,
    /// Set by satisfied / hand_back: the worker's next stop ends the task.
    /// Lock order: `inner` before `ending`.
    ending: Mutex<Option<Ending>>,
    /// The user's words and the worker's latest text, set by the kernel just
    /// before an ask_user call so the proxy can tell whether they settle it.
    ask_context: Mutex<String>,
}

impl TaskHub {
    pub fn new(session_id: impl Into<String>) -> Arc<Self> {
        Self::from_state(session_id, TaskState::default())
    }

    pub fn from_state(session_id: impl Into<String>, state: TaskState) -> Arc<Self> {
        Arc::new(Self {
            session_id: session_id.into(),
            inner: Mutex::new(state),
            kick_pending: AtomicBool::new(false),
            kick: Notify::new(),
            review_now: AtomicBool::new(false),
            resumed: AtomicBool::new(false),
            epoch: AtomicU64::new(0),
            ending: Mutex::new(None),
            ask_context: Mutex::new(String::new()),
        })
    }

    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    pub fn snapshot(&self) -> TaskState {
        self.lock().clone()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, TaskState> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn replace(&self, state: TaskState) {
        *self.lock() = state;
        self.persist();
    }

    /// Change the state in place; persisted when `f` returns true.
    fn update(&self, f: impl FnOnce(&mut TaskState) -> bool) -> bool {
        let changed = f(&mut self.lock());
        if changed {
            self.persist();
        }
        changed
    }

    fn epoch(&self) -> u64 {
        self.epoch.load(Ordering::SeqCst)
    }

    fn set_ending(&self, ending: Option<Ending>) {
        *self.ending.lock().unwrap_or_else(|e| e.into_inner()) = ending;
    }

    fn take_ending(&self) -> Option<Ending> {
        self.ending.lock().unwrap_or_else(|e| e.into_inner()).take()
    }

    /// The proxy was satisfied or handed back; the worker is writing its
    /// closing summary.
    pub fn ending_pending(&self) -> bool {
        self.ending.lock().unwrap_or_else(|e| e.into_inner()).is_some()
    }

    /// Whether the proxy may speak now: live, not paused, not stopped by
    /// Esc, not winding down.
    pub fn steering_open(&self) -> bool {
        let s = self.snapshot();
        s.phase.is_live()
            && s.phase != TaskPhase::ExplainingFail
            && !s.paused
            && !s.skip_steer
            && !self.ending_pending()
    }

    pub fn set_ask_context(&self, text: String) {
        *self.ask_context.lock().unwrap_or_else(|e| e.into_inner()) = text;
    }

    fn ask_context(&self) -> String {
        self.ask_context.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    pub fn start_goal(&self, goal: impl Into<String>) {
        self.start_task(goal, "");
    }

    /// Start a task with optional acceptance criteria.
    pub fn start_task(&self, goal: impl Into<String>, criteria: impl Into<String>) {
        let goal = clip(&goal.into(), MAX_GOAL);
        let criteria = clip(criteria.into().trim(), MAX_CRITERIA);
        let mut g = self.lock();
        *g = TaskState {
            goal,
            criteria,
            phase: TaskPhase::NeedPlan,
            ..TaskState::default()
        };
        self.epoch.fetch_add(1, Ordering::SeqCst);
        drop(g);
        self.set_ending(None);
        self.review_now.store(false, Ordering::SeqCst);
        self.resumed.store(false, Ordering::SeqCst);
        self.persist();
        self.kick();
    }

    /// The user changed the goal or criteria mid-task. Progress notes stay;
    /// the proxy is told what changed at its next look.
    pub fn edit_task(&self, goal: impl Into<String>, criteria: impl Into<String>) {
        let goal = clip(&goal.into(), MAX_GOAL);
        let criteria = clip(criteria.into().trim(), MAX_CRITERIA);
        let mut g = self.lock();
        if !g.phase.is_live() && g.phase != TaskPhase::Done {
            return;
        }
        let text = format!("The user changed the task.\nGoal: {goal}\nCriteria: {}", if criteria.is_empty() { "(none)" } else { &criteria });
        push_log(&mut g, "user", text);
        g.goal = goal;
        g.criteria = criteria;
        if g.phase == TaskPhase::Done {
            g.phase = TaskPhase::Active;
        }
        drop(g);
        self.set_ending(None);
        self.persist();
        self.request_review();
    }

    /// Pause or resume the proxy. Resuming wakes an idle run.
    pub fn set_paused(&self, paused: bool) {
        self.lock().paused = paused;
        self.persist();
        self.resumed.store(!paused, Ordering::SeqCst);
        if !paused {
            self.kick();
        }
    }

    fn take_resumed(&self) -> bool {
        self.resumed.swap(false, Ordering::SeqCst)
    }

    /// The user asked the proxy to look now.
    pub fn request_review(&self) {
        self.review_now.store(true, Ordering::SeqCst);
        self.kick();
    }

    /// Taken by the kernel when it runs a review; `true` once per request.
    pub fn take_review_request(&self) -> bool {
        self.review_now.swap(false, Ordering::SeqCst)
    }

    /// The user accepts the work as done, whatever the proxy thinks.
    pub fn accept_done(&self) {
        let mut g = self.lock();
        if !g.phase.is_live() {
            return;
        }
        g.phase = TaskPhase::Done;
        g.review_note = "使用者判定完成".into();
        push_log(&mut g, "user", "The user accepted the work as done.");
        drop(g);
        self.set_ending(None);
        self.persist();
        self.kick();
    }

    pub fn end(&self) {
        let mut g = self.lock();
        *g = TaskState::default();
        self.epoch.fetch_add(1, Ordering::SeqCst);
        drop(g);
        self.set_ending(None);
        self.review_now.store(false, Ordering::SeqCst);
        self.persist();
        self.kick();
    }

    pub fn set_skip_steer(&self, skip: bool) {
        self.update(|s| {
            let changed = s.skip_steer != skip;
            s.skip_steer = skip;
            changed
        });
    }

    /// A real user message while the task is live: the worker handles it
    /// first, the proxy reads it at its next look.
    pub fn note_user(&self, text: &str) {
        let text = text.trim();
        if text.is_empty() {
            return;
        }
        self.update(|s| {
            if !s.phase.is_live() {
                return false;
            }
            push_log(s, "user", text);
            true
        });
    }

    /// The worker reported through task_report. `Err` = task mode is off.
    pub fn record_report(&self, kind: ReportKind, summary: &str, evidence: &str) -> std::result::Result<(), ()> {
        let summary = clip(summary, REASON_CHARS);
        let evidence = clip(evidence, 1_500);
        let ok = self.update(|s| {
            if !s.phase.is_live() {
                return false;
            }
            let mut text = format!("task_report {}: {summary}", kind.as_str());
            if !evidence.is_empty() {
                text.push_str(&format!("\nEvidence: {evidence}"));
            }
            push_log(s, "worker", text);
            if kind == ReportKind::Impossible {
                s.claim = Some(summary.clone());
            }
            true
        });
        if ok {
            Ok(())
        } else {
            Err(())
        }
    }

    /// The old impossibility claim, kept for callers of the earlier API.
    pub fn record_claim(&self, reason: String) -> String {
        match self.record_report(ReportKind::Impossible, &reason, "") {
            Ok(()) => json!({"ok": true, "recorded": true, "note": ReportKind::Impossible.reply()}).to_string(),
            Err(()) => json!({"ok": false, "error": "task mode is not active"}).to_string(),
        }
    }

    pub fn kick(&self) {
        self.kick_pending.store(true, Ordering::SeqCst);
        self.kick.notify_waiters();
    }

    pub fn take_kick(&self) -> bool {
        self.kick_pending.swap(false, Ordering::SeqCst)
    }

    pub async fn wait_kick(&self) {
        loop {
            if self.take_kick() {
                return;
            }
            self.kick.notified().await;
        }
    }

    fn persist(&self) {
        if self.session_id.is_empty() {
            return;
        }
        let Ok(store) = crate::session::SessionStore::open() else {
            return;
        };
        let state = self.snapshot();
        if state.phase == TaskPhase::Inactive {
            store.clear_task(&self.session_id);
            return;
        }
        let _ = store.save_task(&self.session_id, &state);
    }

    /// The review that started at `epoch` from `snap` is still about the
    /// task as it stands, and the proxy may still speak.
    fn same_task(&self, epoch: u64, now: &TaskState, snap: &TaskState) -> bool {
        self.epoch() == epoch
            && now.phase.is_live()
            && !now.paused
            && now.goal == snap.goal
            && now.criteria == snap.criteria
    }
}

/// What the worker tells the proxy through task_report.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReportKind {
    Done,
    Question,
    Progress,
    Impossible,
}

impl ReportKind {
    fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "done" | "complete" => Some(Self::Done),
            "question" => Some(Self::Question),
            "progress" => Some(Self::Progress),
            "impossible" => Some(Self::Impossible),
            _ => None,
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Done => "done",
            Self::Question => "question",
            Self::Progress => "progress",
            Self::Impossible => "impossible",
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Done => "宣稱完成",
            Self::Question => "提問",
            Self::Progress => "進度",
            Self::Impossible => "宣稱無法完成",
        }
    }

    fn reply(self) -> &'static str {
        match self {
            Self::Done => "Recorded. Finish this turn with your summary; the user's proxy checks the evidence when you stop.",
            Self::Question => "Recorded. The user's proxy answers at its next review, when you stop. Keep working on anything that does not depend on the answer.",
            Self::Progress => "Recorded.",
            Self::Impossible => "Recorded. Finish this turn. The user's proxy evaluates when you stop: if it agrees, you will be asked to explain to the user; if not, keep working.",
        }
    }
}

pub struct TaskReportTool {
    hub: Arc<TaskHub>,
    events: Option<(Arc<dyn EventSink>, String)>,
}

impl TaskReportTool {
    pub fn new(hub: Arc<TaskHub>) -> Self {
        Self { hub, events: None }
    }

    /// Show each report in the UI as a TaskProxy `report` row.
    pub fn with_events(mut self, sink: Arc<dyn EventSink>, run_id: String) -> Self {
        self.events = Some((sink, run_id));
        self
    }

    fn report(&self, args: &Value) -> Result<String> {
        let kind = args.get("kind").and_then(Value::as_str).unwrap_or("");
        let Some(kind) = ReportKind::parse(kind) else {
            return Err(Error::Tool(
                "task_report kind must be done, question, progress or impossible".into(),
            ));
        };
        let summary = args
            .get("summary")
            .or_else(|| args.get("reason"))
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim();
        if summary.is_empty() {
            return Err(Error::Tool("summary is required".into()));
        }
        let evidence = args.get("evidence").and_then(Value::as_str).unwrap_or("").trim();
        if self.hub.record_report(kind, summary, evidence).is_err() {
            return Ok(json!({"ok": false, "error": "task mode is not active"}).to_string());
        }
        if let Some((sink, run_id)) = &self.events {
            let mut text = format!("{}：{}", kind.label(), clip(summary, REASON_CHARS));
            if !evidence.is_empty() {
                text.push_str(&format!("\n證據：{}", clip(evidence, 1_500)));
            }
            emit_proxy(sink.as_ref(), run_id, "report", text);
        }
        Ok(json!({
            "ok": true,
            "recorded": true,
            "kind": kind.as_str(),
            "note": kind.reply(),
        })
        .to_string())
    }
}

impl ClientTool for TaskReportTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "task_report".into(),
            description: "Task mode only: report to the user's proxy, the separate model acting for the user. kind=done when you believe the goal is met: give a summary and the evidence (commands and their results, files changed); the proxy checks it when you stop. kind=question for something you need the proxy to settle; it answers at its next review. kind=progress for a short status note. kind=impossible only when an unchangeable constraint blocks the goal, never because the work is hard or unfinished.".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "kind": {
                        "type": "string",
                        "enum": ["done", "question", "progress", "impossible"],
                        "description": "done, question, progress or impossible."
                    },
                    "summary": {
                        "type": "string",
                        "description": "What you did, what you ask, or the unchangeable constraint, in concrete terms."
                    },
                    "evidence": {
                        "type": "string",
                        "description": "For done: the commands you ran and their results, files changed, where to look."
                    }
                },
                "required": ["kind", "summary"],
                "additionalProperties": false
            }),
        }
    }

    fn call(&self, args: &Value) -> ToolCallFut<'_> {
        let out = self.report(args);
        Box::pin(async move { out })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Status {
    Continue,
    Satisfied,
    HandBack,
    Impossible,
}

impl Status {
    fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().replace(['-', ' '], "_").as_str() {
            "continue" | "cont" => Some(Self::Continue),
            "satisfied" | "complete" | "done" | "accept" | "accepted" => Some(Self::Satisfied),
            "hand_back" | "handback" | "handed_back" | "blocked" => Some(Self::HandBack),
            "impossible" | "fail" | "failed" => Some(Self::Impossible),
            _ => None,
        }
    }
}

/// One checklist line as the proxy sent it. `done: None` leaves an
/// existing item's state alone.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ItemUpdate {
    text: String,
    done: Option<bool>,
    dropped: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Verdict {
    status: Status,
    message: String,
    checklist: Vec<ItemUpdate>,
    note: String,
    reason: String,
}

fn parse_verdict(raw: &str) -> Result<Verdict> {
    let v = extract_json(raw)?;
    let status = v
        .get("status")
        .and_then(Value::as_str)
        .and_then(Status::parse)
        .ok_or_else(|| Error::Provider("the user's proxy gave no status".into()))?;
    let text = |k: &str| v.get(k).and_then(Value::as_str).unwrap_or("").to_string();
    let message = if text("message").trim().is_empty() { text("next") } else { text("message") };
    Ok(Verdict {
        status,
        message: clip(&message, MESSAGE_CHARS),
        checklist: parse_updates(v.get("checklist")),
        note: clip(&text("note"), NOTE_CHARS),
        reason: clip(&text("reason"), REASON_CHARS),
    })
}

fn parse_updates(v: Option<&Value>) -> Vec<ItemUpdate> {
    let Some(arr) = v.and_then(Value::as_array) else {
        return Vec::new();
    };
    arr.iter()
        .filter_map(|item| {
            if let Some(s) = item.as_str() {
                let text = clip(s, ITEM_CHARS);
                return (!text.is_empty()).then_some(ItemUpdate { text, done: None, dropped: false });
            }
            let text = item
                .get("text")
                .or_else(|| item.get("item"))
                .and_then(Value::as_str)
                .unwrap_or("");
            let text = clip(text, ITEM_CHARS);
            if text.is_empty() {
                return None;
            }
            Some(ItemUpdate {
                text,
                done: item.get("done").and_then(Value::as_bool),
                dropped: item.get("dropped").and_then(Value::as_bool).unwrap_or(false),
            })
        })
        .take(MAX_ITEMS * 2)
        .collect()
}

fn item_key(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ").to_lowercase()
}

/// The proxy's checklist after a review. Items it left out are kept as
/// they were, so the goalposts cannot move silently; an item goes only
/// when sent with `dropped` and a reason (`may_drop`). Returns the list
/// and the dropped items' text.
fn merge_checklist(old: &[CheckItem], updates: &[ItemUpdate], may_drop: bool) -> (Vec<CheckItem>, Vec<String>) {
    let mut out = old.to_vec();
    let mut dropped = Vec::new();
    for u in updates {
        let key = item_key(&u.text);
        match out.iter().position(|i| item_key(&i.text) == key) {
            Some(at) if u.dropped => {
                if may_drop {
                    dropped.push(out.remove(at).text);
                }
            }
            Some(at) => {
                if let Some(done) = u.done {
                    out[at].done = done;
                }
            }
            None if u.dropped => {}
            None => {
                if out.len() < MAX_ITEMS {
                    out.push(CheckItem { text: u.text.clone(), done: u.done.unwrap_or(false) });
                }
            }
        }
    }
    (out, dropped)
}

fn extract_json(raw: &str) -> Result<Value> {
    let t = raw.trim();
    if let Ok(v) = serde_json::from_str::<Value>(t) {
        if v.is_object() {
            return Ok(v);
        }
    }
    let start = t.find('{').ok_or_else(|| {
        Error::Provider("the user's proxy did not return JSON".into())
    })?;
    let end = t.rfind('}').ok_or_else(|| {
        Error::Provider("the user's proxy did not return JSON".into())
    })?;
    if end < start {
        return Err(Error::Provider("the user's proxy did not return JSON".into()));
    }
    serde_json::from_str(&t[start..=end])
        .map_err(|_| Error::Provider("the user's proxy returned invalid JSON".into()))
}

fn clip(s: &str, max: usize) -> String {
    let t = s.trim();
    if t.chars().count() <= max {
        return t.to_string();
    }
    t.chars().take(max).collect()
}

fn clip_ellipsis(s: &str, max: usize) -> String {
    let t = s.trim();
    if t.chars().count() <= max {
        return t.to_string();
    }
    let mut out: String = t.chars().take(max).collect();
    out.push('…');
    out
}

/// The start and the end of `s`, the middle cut out.
fn head_tail(s: &str, head: usize, tail: usize) -> String {
    let n = s.chars().count();
    if n <= head + tail {
        return s.to_string();
    }
    let a: String = s.chars().take(head).collect();
    let b: String = s.chars().skip(n - tail).collect();
    format!("{a}\n[… {} chars cut …]\n{b}", n - head - tail)
}

/// What the proxy sees of the conversation: user and assistant text, each
/// tool call with its arguments, and each tool output's head and tail
/// (pass or fail is printed at the end). The newest part within the budget.
pub fn history_excerpt(items: &[Value]) -> String {
    let chunks: Vec<String> = items.iter().filter_map(excerpt_item).collect();
    let mut kept: Vec<&str> = Vec::new();
    let mut used = 0usize;
    let mut cut = false;
    for c in chunks.iter().rev() {
        let n = c.chars().count() + 1;
        if used + n > EXCERPT_CHARS {
            cut = true;
            break;
        }
        used += n;
        kept.push(c);
    }
    kept.reverse();
    let body = kept.join("\n");
    if cut {
        format!("[… older conversation cut …]\n{body}")
    } else {
        body
    }
}

fn excerpt_item(item: &Value) -> Option<String> {
    match item.get("type").and_then(Value::as_str) {
        Some("function_call") => {
            let name = item.get("name").and_then(Value::as_str).unwrap_or("tool");
            let args = match item.get("arguments") {
                Some(Value::String(s)) => s.clone(),
                Some(v) => v.to_string(),
                None => String::new(),
            };
            Some(format!("tool call: {name} {}", clip_ellipsis(&args, CALL_ARGS_CHARS)))
        }
        Some("function_call_output") => {
            let text = readable_output(&item_plain(item));
            Some(format!("tool result: {}", head_tail(text.trim(), OUTPUT_HEAD, OUTPUT_TAIL)))
        }
        Some("reasoning") | Some("compaction") => None,
        _ => {
            let mut role = item
                .get("role")
                .and_then(Value::as_str)
                .or_else(|| item.get("type").and_then(Value::as_str))
                .unwrap_or("item")
                .to_string();
            let mut text = item_plain(item);
            if text.trim().is_empty() {
                return None;
            }
            // Its own earlier messages: the body only, without the header
            // and the goal reminder.
            if let Some(rest) = text.trim_start().strip_prefix(USER_PROXY) {
                let rest = rest.trim_start();
                let mut body = rest.strip_prefix(PROXY_HEADER).unwrap_or(rest).trim().to_string();
                if let Some(at) = body.find(FOOTER_MARK) {
                    body.truncate(at);
                }
                text = body;
                role = "you (the proxy)".into();
            }
            Some(format!("{role}: {}", head_tail(text.trim(), 1_500, 2_500)))
        }
    }
}

/// A command result as readable text, so the tail shows the log's end
/// rather than escaped JSON.
fn readable_output(raw: &str) -> String {
    let Ok(Value::Object(o)) = serde_json::from_str::<Value>(raw) else {
        return raw.to_string();
    };
    let out = o.get("stdout").and_then(Value::as_str);
    let err = o.get("stderr").and_then(Value::as_str);
    if out.is_none() && err.is_none() {
        return raw.to_string();
    }
    let mut s = String::new();
    if let Some(code) = o.get("exit_code") {
        s.push_str(&format!("exit_code: {code}\n"));
    }
    if o.get("timed_out").and_then(Value::as_bool) == Some(true) {
        s.push_str("timed out\n");
    }
    if let Some(out) = out.filter(|t| !t.trim().is_empty()) {
        s.push_str("stdout:\n");
        s.push_str(out.trim_end());
        s.push('\n');
    }
    if let Some(err) = err.filter(|t| !t.trim().is_empty()) {
        s.push_str("stderr:\n");
        s.push_str(err.trim_end());
    }
    s
}

fn item_plain(item: &Value) -> String {
    if let Some(s) = item.get("content").and_then(Value::as_str) {
        return strip_image(s);
    }
    if let Some(arr) = item.get("content").and_then(Value::as_array) {
        return join_parts(arr);
    }
    if let Some(t) = item.get("text").and_then(Value::as_str) {
        return strip_image(t);
    }
    if let Some(o) = item.get("output").and_then(Value::as_str) {
        return strip_image(o);
    }
    if let Some(arr) = item.get("output").and_then(Value::as_array) {
        return join_parts(arr);
    }
    String::new()
}

fn join_parts(arr: &[Value]) -> String {
    let mut out = String::new();
    for part in arr {
        let text = if part.get("type").and_then(Value::as_str) == Some("input_image") {
            "[image]".to_string()
        } else if let Some(t) = part.get("text").and_then(Value::as_str) {
            strip_image(t)
        } else {
            continue;
        };
        if !out.is_empty() {
            out.push('\n');
        }
        out.push_str(&text);
    }
    out
}

fn strip_image(s: &str) -> String {
    if s.starts_with("data:image/") {
        "[image]".into()
    } else {
        s.to_string()
    }
}

/// The user's own words in this conversation and what the worker said
/// just before asking, for deciding whether an ask_user question is
/// already settled.
pub fn ask_context(history: &[Value], latest: &str) -> String {
    let mut out = String::new();
    if let Some(mem) = crate::compact::previous_memory(history) {
        out.push_str("Memory of the earlier conversation:\n");
        out.push_str(&clip_ellipsis(&mem, 3_000));
        out.push_str("\n\n");
    }
    out.push_str("The user's own messages, oldest first:\n");
    let words = crate::compact::keep_user_words(history, 3_000);
    if words.is_empty() {
        out.push_str("(none)\n");
    }
    for w in &words {
        out.push_str(&format!("- {}\n", clip_ellipsis(&item_plain(w), 1_500)));
    }
    if !latest.trim().is_empty() {
        out.push_str("\nWhat the worker said before asking:\n");
        out.push_str(&clip_ellipsis(latest, 2_000));
    }
    out
}

fn memory_text(log: &[ProxyTurn]) -> String {
    let mut lines: Vec<String> = Vec::new();
    let mut used = 0usize;
    for t in log.iter().rev().take(MAX_LOG) {
        let line = format!("[{}] {}", t.who, clip_ellipsis(&t.text, LOG_ENTRY_CHARS));
        let n = line.chars().count() + 1;
        if used + n > MEMORY_CHARS && !lines.is_empty() {
            break;
        }
        used += n;
        lines.push(line);
    }
    lines.reverse();
    lines.join("\n")
}

fn checklist_for_model(list: &[CheckItem]) -> String {
    if list.is_empty() {
        return "(none yet)".into();
    }
    list.iter()
        .map(|i| format!("- [{}] {}", if i.done { "x" } else { " " }, i.text))
        .collect::<Vec<_>>()
        .join("\n")
}

fn or_none(s: &str, none: &str) -> String {
    if s.trim().is_empty() {
        none.to_string()
    } else {
        s.trim().to_string()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Plan,
    Push,
    Checkpoint,
}

impl Kind {
    fn intent(self) -> &'static str {
        match self {
            Self::Plan => "plan",
            Self::Push => "push",
            Self::Checkpoint => "checkpoint",
        }
    }

    fn guide(self) -> &'static str {
        match self {
            Self::Plan => "The task just started. Write your opening brief to the worker as the message: what you want done, what done looks like, and how you will check it. Set the initial checklist.",
            Self::Push => "The worker stopped and is waiting. Judge its work against the goal and the criteria, then tell it what you want next, or decide.",
            Self::Checkpoint => "The worker is in the middle of a run. Speak only if it is stuck, looping or going the wrong way; an empty message lets it carry on.",
        }
    }

    fn notice(self) -> &'static str {
        match self {
            Self::Plan => "用戶代理正在讀目標、寫開場要求…",
            Self::Push => "用戶代理正在檢查主代理的成果…",
            Self::Checkpoint => "用戶代理在中途查看進度…",
        }
    }
}

fn review_prompt(state: &TaskState, kind: Kind, why: &str, excerpt: &str, last_text: &str) -> String {
    let why = if why.trim().is_empty() {
        String::new()
    } else {
        format!("Why you are looking now: {}\n", why.trim())
    };
    format!(
        "Review kind: {}\n{}\n{why}\nThe user's goal, in their words:\n{}\n\nThe user's acceptance criteria and preferences:\n{}\n\nYour progress notes (checklist):\n{}\n\nMessages you have sent the worker so far: {}\n\nYour memory, oldest first:\n{}\n\nThe worker's latest reply:\n{}\n\nConversation excerpt, oldest part cut (tool calls and the end of their output are kept):\n{}\n",
        kind.intent(),
        kind.guide(),
        state.goal,
        or_none(&state.criteria, "(none given)"),
        checklist_for_model(&state.checklist),
        state.rounds,
        or_none(&memory_text(&state.proxy_log), "(empty)"),
        or_none(&head_tail(last_text.trim(), LAST_REPLY_CHARS / 3, LAST_REPLY_CHARS * 2 / 3), "(none yet)"),
        or_none(excerpt, "(empty)"),
    )
}

fn ask_prompt(state: &TaskState, q: &Question, context: &str) -> String {
    let options = q
        .options
        .iter()
        .map(|o| {
            format!(
                "- id={}: {}{}",
                o.id,
                o.label,
                if o.input { " (input: the user types a value)" } else { "" }
            )
        })
        .collect::<Vec<_>>()
        .join("\n");
    format!(
        "The worker asked the user:\n{}\n{}\nOptions:\n{options}\n\nThe user's goal, in their words:\n{}\n\nThe user's acceptance criteria and preferences:\n{}\n\nYour memory, oldest first:\n{}\n\n{}\n",
        q.prompt,
        if q.allow_multiple { "Several options may be picked." } else { "Pick one option." },
        state.goal,
        or_none(&state.criteria, "(none given)"),
        or_none(&memory_text(&state.proxy_log), "(empty)"),
        or_none(context, "(no conversation context)"),
    )
}

/// Everything a proxy request needs. `model` is already resolved: the task
/// model, or the main one.
pub struct ProxyCall<'a, P> {
    pub hub: &'a TaskHub,
    pub provider: &'a P,
    pub model: String,
    pub effort: ReasoningEffort,
    pub send_reasoning: bool,
    pub run_id: &'a str,
    pub workspace: &'a Path,
    pub sink: &'a dyn EventSink,
}

/// The proxy's model, effort and whether to send the effort: the task
/// model when the user picked one, else the main model; the main effort.
pub fn proxy_settings(
    knobs: Option<&Arc<Mutex<SessionKnobs>>>,
    model: &str,
    effort: ReasoningEffort,
) -> (String, ReasoningEffort, bool) {
    match knobs {
        Some(k) => {
            let g = k.lock().unwrap_or_else(|e| e.into_inner());
            let task = g.task_model.trim();
            let model = if task.is_empty() { g.model.clone() } else { task.to_string() };
            (model, g.reasoning_effort, g.send_reasoning)
        }
        None => (model.to_string(), effort, true),
    }
}

fn retry_delay() -> Duration {
    if cfg!(test) {
        Duration::from_millis(10)
    } else {
        Duration::from_secs(3)
    }
}

fn workspace_changes_spec() -> ToolSpec {
    ToolSpec {
        name: "workspace_changes".into(),
        description: "Read-only: `git status --short` and `git diff --stat` of the workspace. Empty when the workspace is not a git repo.".into(),
        parameters: json!({"type": "object", "properties": {}, "additionalProperties": false}),
    }
}

fn git_text(ws: &Path, args: &[&str]) -> Option<String> {
    let mut cmd = std::process::Command::new("git");
    cmd.current_dir(ws)
        .args(args)
        .env("GIT_OPTIONAL_LOCKS", "0")
        .stdin(std::process::Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    let out = cmd.output().ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).into_owned())
}

fn workspace_changes(ws: &Path) -> String {
    let Some(status) = git_text(ws, &["status", "--short"]) else {
        return json!({"git": false, "status": "", "diff_stat": ""}).to_string();
    };
    let stat = git_text(ws, &["diff", "--stat"]).unwrap_or_default();
    json!({
        "git": true,
        "status": clip_ellipsis(&status, 8_000),
        "diff_stat": clip_ellipsis(&stat, 8_000),
    })
    .to_string()
}

async fn proxy_tool(workspace: &Path, name: &str, args: &Value) -> String {
    let out = match name {
        "read_file" => ReadFileTool::new(workspace.to_path_buf()).call_sync(args),
        "list_dir" => ListDirTool::new(workspace.to_path_buf()).call_sync(args),
        "workspace_changes" => {
            let ws = workspace.to_path_buf();
            tokio::task::spawn_blocking(move || workspace_changes(&ws))
                .await
                .map_err(|e| Error::Tool(e.to_string()))
        }
        _ => Err(Error::Tool(format!("unknown tool {name}; you have read_file, list_dir and workspace_changes"))),
    };
    match out {
        Ok(s) => clip_ellipsis(&s, TOOL_OUTPUT_CHARS),
        Err(e) => json!({"error": e.to_string()}).to_string(),
    }
}

fn append_missing_calls(input: &mut Vec<Value>, calls: &[FunctionCall]) {
    for call in calls {
        let already = input.iter().any(|i| {
            i.get("type").and_then(Value::as_str) == Some("function_call")
                && i.get("call_id").and_then(Value::as_str) == Some(call.call_id.as_str())
        });
        if !already {
            input.push(json!({
                "type": "function_call",
                "call_id": call.call_id,
                "name": call.name,
                "arguments": call.arguments,
            }));
        }
    }
}

/// One proxy request. With `tools`, a bounded loop: at most
/// [`MAX_PROXY_TOOL_CALLS`] read-only calls, then a final answer is forced.
/// Tool results stay in this request; the worker never sees them.
async fn ask_proxy<P: Provider>(
    call: &ProxyCall<'_, P>,
    instructions: &str,
    prompt: &str,
    tools: bool,
    send_reasoning: bool,
) -> Result<String> {
    let specs = if tools {
        vec![
            ReadFileTool::new(call.workspace.to_path_buf()).spec(),
            ListDirTool::new(call.workspace.to_path_buf()).spec(),
            workspace_changes_spec(),
        ]
    } else {
        Vec::new()
    };
    let mut input = vec![json!({"role": "user", "content": prompt})];
    let mut used = 0usize;
    let mut forced = false;
    for _ in 0..MAX_PROXY_TOOL_CALLS + 2 {
        let req = CompleteRequest {
            instructions: instructions.into(),
            input: input.clone(),
            client_tools: specs.clone(),
            server_tools: vec![],
            cache_key: format!("grokaagent-task:{}", call.run_id),
            previous_response_id: None,
            store: false,
            reasoning_effort: call.effort,
            send_reasoning,
            model: call.model.clone(),
            tool_choice: (forced && tools).then(|| "none".to_string()),
        };
        let r = call.provider.complete(req).await?;
        if r.function_calls.is_empty() {
            return Ok(r.text);
        }
        if forced || !tools {
            return Err(Error::Provider("the user's proxy kept calling tools".into()));
        }
        input.extend(r.output_items.iter().cloned());
        append_missing_calls(&mut input, &r.function_calls);
        for fc in &r.function_calls {
            let output = if used < MAX_PROXY_TOOL_CALLS {
                used += 1;
                let args: Value = serde_json::from_str(&fc.arguments).unwrap_or(json!({}));
                proxy_tool(call.workspace, &fc.name, &args).await
            } else {
                json!({"error": "tool budget used up; answer now"}).to_string()
            };
            input.push(json!({"type": "function_call_output", "call_id": fc.call_id, "output": output}));
        }
        if used >= MAX_PROXY_TOOL_CALLS {
            forced = true;
            input.push(json!({
                "role": "user",
                "content": "Your tool budget for this review is used up. Reply now with the JSON object only.",
            }));
        }
    }
    Err(Error::Provider("the user's proxy did not answer".into()))
}

/// A proxy request and its parse, tried twice (about 3s apart). A model
/// that rejects the reasoning effort is retried without it.
async fn ask_twice<P: Provider, T>(
    call: &ProxyCall<'_, P>,
    instructions: &str,
    prompt: &str,
    tools: bool,
    parse: impl Fn(&str) -> Result<T>,
) -> Result<T> {
    let mut send = call.send_reasoning;
    let mut last = None;
    for attempt in 0..2 {
        if attempt > 0 {
            tokio::time::sleep(retry_delay()).await;
        }
        match ask_proxy(call, instructions, prompt, tools, send).await.and_then(|t| parse(&t)) {
            Ok(v) => return Ok(v),
            Err(e) => {
                let text = e.to_string().to_ascii_lowercase();
                if send && (text.contains("reasoning") || text.contains("effort")) {
                    send = false;
                }
                last = Some(e);
            }
        }
    }
    Err(last.unwrap_or_else(|| Error::Provider("the user's proxy did not answer".into())))
}

fn meta(run_id: &str) -> EventMeta {
    EventMeta {
        ts: chrono::Utc::now(),
        agent_name: "root".into(),
        run_id: run_id.to_string(),
        parent_run_id: None,
        path: String::new(),
    }
}

fn emit_notice(sink: &dyn EventSink, run_id: &str, message: String) {
    sink.emit(&AgentEvent::Notice {
        meta: meta(run_id),
        message,
    });
}

fn emit_proxy(sink: &dyn EventSink, run_id: &str, intent: &str, text: String) {
    sink.emit(&AgentEvent::TaskProxy {
        meta: meta(run_id),
        intent: intent.into(),
        text,
    });
}

/// A proxy message as the worker receives it: labeled as not the user,
/// then the message, then a compact reminder of the goal and what is open.
fn worker_message(state: &TaskState, message: &str) -> String {
    let mut out = format!(
        "{USER_PROXY} {PROXY_HEADER}\n\n{}{FOOTER_MARK}{}",
        message.trim(),
        clip_ellipsis(&state.goal, FOOTER_GOAL)
    );
    if !state.criteria.trim().is_empty() {
        out.push_str(&format!("\nCriteria: {}", clip_ellipsis(&state.criteria, FOOTER_CRITERIA)));
    }
    let open: Vec<&CheckItem> = state.checklist.iter().filter(|i| !i.done).collect();
    if !open.is_empty() {
        out.push_str("\nOpen items:");
        for i in open {
            out.push_str(&format!("\n- [ ] {}", i.text));
        }
    }
    out
}

fn bare(reason: &str) -> &str {
    reason.trim().trim_end_matches(['.', '。'])
}

fn satisfied_message(reason: &str) -> String {
    format!(
        "{USER_PROXY} The user's proxy is satisfied: {}. Write the final summary for the user now (what was done, how it was verified, anything left) and stop.",
        bare(reason)
    )
}

fn hand_back_message(reason: &str) -> String {
    format!(
        "{USER_PROXY} The user's proxy is handing this back to the user: {}. Stop working on it and write the user a short summary of where things stand and what they need to decide.",
        bare(reason)
    )
}

fn impossible_message(reason: &str) -> String {
    format!(
        "{USER_PROXY} The user's proxy confirmed this goal cannot be completed: {}. Tell the user, in their language, that the task stopped unfinished and why. Do not keep trying. Do not call more tools except to quote evidence you already have.",
        bare(reason)
    )
}

/// What the proxy says when it gave no message of its own.
fn fallback_message(list: &[CheckItem], claimed_done: bool) -> String {
    let open: Vec<String> = list.iter().filter(|i| !i.done).map(|i| format!("- {}", i.text)).collect();
    match (claimed_done, open.is_empty()) {
        (true, true) => "Before I accept this, point me to the concrete evidence (test output, files, diffs) that shows the goal is met.".into(),
        (true, false) => format!("Not done yet. These are still open:\n{}\nFinish them and show me the evidence.", open.join("\n")),
        (false, true) => "Keep going toward the goal, and show me the evidence when it is done.".into(),
        (false, false) => format!("Keep going. Still open:\n{}", open.join("\n")),
    }
}

/// First turn after task mode starts: the opening brief.
pub async fn prepare_first_turn<P: Provider>(call: &ProxyCall<'_, P>, history: &[Value]) -> Option<String> {
    let s = call.hub.snapshot();
    if s.phase != TaskPhase::NeedPlan || s.paused {
        return None;
    }
    review(call, history, "", Kind::Plan, "").await
}

/// After the worker stopped on its own. `None` = wait for the user.
pub async fn after_natural_stop<P: Provider>(
    call: &ProxyCall<'_, P>,
    history: &[Value],
    last_text: &str,
) -> Option<String> {
    let hub = call.hub;
    if let Some(ending) = hub.take_ending() {
        finish_ending(call, ending);
        return None;
    }
    let s = hub.snapshot();
    if !s.phase.is_live() || s.paused || s.skip_steer {
        return None;
    }
    if s.phase == TaskPhase::ExplainingFail {
        finish_failed(call);
        return None;
    }
    // This look answers any pending request or resume.
    hub.take_review_request();
    hub.take_resumed();
    let kind = if s.phase == TaskPhase::NeedPlan { Kind::Plan } else { Kind::Push };
    review(call, history, last_text, kind, "").await
}

/// After an idle wait woke. `users` are the real user messages that came
/// in: the worker handles them first, the proxy reads them next time.
/// Otherwise the proxy speaks for a new goal, a review request, or a
/// resume.
pub async fn after_wait<P: Provider>(
    call: &ProxyCall<'_, P>,
    history: &[Value],
    last_text: &str,
    users: &[String],
) -> Option<String> {
    let hub = call.hub;
    if !users.is_empty() {
        for u in users {
            hub.note_user(u);
        }
        hub.set_skip_steer(false);
        return None;
    }
    let s = hub.snapshot();
    if !s.phase.is_live() || s.paused || s.phase == TaskPhase::ExplainingFail || hub.ending_pending() {
        return None;
    }
    if s.phase == TaskPhase::NeedPlan {
        hub.take_review_request();
        return review(call, history, "", Kind::Plan, "").await;
    }
    let asked = hub.take_review_request();
    let resumed = hub.take_resumed();
    if asked || (resumed && !s.skip_steer) {
        hub.set_skip_steer(false);
        let why = if asked { "the user asked you to look now" } else { "the user resumed you" };
        return review(call, history, last_text, Kind::Push, why).await;
    }
    None
}

/// A look in the middle of a run, `why` says what triggered it. `None`
/// (or an empty message from the proxy) = carry on.
pub async fn checkpoint<P: Provider>(
    call: &ProxyCall<'_, P>,
    history: &[Value],
    last_text: &str,
    why: &str,
) -> Option<String> {
    if !call.hub.steering_open() {
        return None;
    }
    let kind = if call.hub.snapshot().phase == TaskPhase::NeedPlan { Kind::Plan } else { Kind::Checkpoint };
    review(call, history, last_text, kind, why).await
}

async fn review<P: Provider>(
    call: &ProxyCall<'_, P>,
    history: &[Value],
    last_text: &str,
    kind: Kind,
    why: &str,
) -> Option<String> {
    let hub = call.hub;
    let snap = hub.snapshot();
    let epoch = hub.epoch();
    if snap.goal.trim().is_empty() {
        return None;
    }
    emit_notice(call.sink, call.run_id, kind.notice().into());
    let prompt = review_prompt(&snap, kind, why, &history_excerpt(history), last_text);
    let verdict = match ask_twice(call, PROXY_INSTRUCTIONS, &prompt, true, parse_verdict).await {
        Ok(v) => v,
        Err(e) => {
            proxy_failed(call, epoch, &e);
            return None;
        }
    };
    apply(call, epoch, &snap, verdict, kind, last_text)
}

/// The proxy could not answer even on the retry: pause the task and say
/// why. The user resumes from the UI.
fn proxy_failed<P>(call: &ProxyCall<'_, P>, epoch: u64, e: &Error) {
    let hub = call.hub;
    let paused = hub.update(|s| {
        if hub.epoch() != epoch || !s.phase.is_live() {
            return false;
        }
        s.paused = true;
        s.review_note = format!("用戶代理呼叫失敗：{e}。已暫停，按「繼續」重試。");
        push_log(s, "note", format!("Your request failed ({e}); the task was paused."));
        true
    });
    if paused {
        hub.resumed.store(false, Ordering::SeqCst);
        emit_notice(call.sink, call.run_id, format!("用戶代理沒有回應（{e}），任務已暫停；按「繼續」重試。"));
    }
}

/// Apply a verdict to the task as it stands now. Returns what the worker
/// is told, if anything.
fn apply<P>(
    call: &ProxyCall<'_, P>,
    epoch: u64,
    snap: &TaskState,
    v: Verdict,
    kind: Kind,
    last_text: &str,
) -> Option<String> {
    let hub = call.hub;
    let mut out: Option<(String, String, String)> = None;
    let mut ending = None;
    let applied = hub.update(|s| {
        // A review for a task that was ended, replaced, edited or paused
        // meanwhile does not count; anything else that changed (new notes,
        // reports) is kept.
        if !hub.same_task(epoch, s, snap) {
            return false;
        }
        let may_drop = !v.note.trim().is_empty();
        let (list, dropped) = merge_checklist(&s.checklist, &v.checklist, may_drop);
        s.checklist = list;
        if kind == Kind::Push && !last_text.trim().is_empty() {
            // A look while idle sees the same reply the last stop did.
            let reply = clip_ellipsis(last_text, 2_000);
            if s.proxy_log.iter().rev().find(|t| t.who == "worker").map(|t| &t.text) != Some(&reply) {
                push_log(s, "worker", reply);
            }
        }
        for d in dropped {
            push_log(s, "note", format!("You dropped a checklist item: {d} ({})", v.note));
        }
        s.claim = None;
        s.skip_steer = false;
        if s.phase == TaskPhase::NeedPlan {
            s.phase = TaskPhase::Active;
        }
        let mut status = v.status;
        // Code holds the line the prompt draws: unfinished notes or no
        // cited evidence is not done.
        if status == Status::Satisfied && (s.checklist.iter().any(|i| !i.done) || v.reason.trim().is_empty()) {
            status = Status::Continue;
        }
        match status {
            Status::Continue => {
                s.review_note = v.note.clone();
                let mut message = v.message.trim().to_string();
                if message.is_empty() {
                    if kind == Kind::Checkpoint {
                        push_log(s, "note", format!("Mid-run look: you let the worker carry on. {}", v.note));
                        return true;
                    }
                    message = fallback_message(&s.checklist, v.status == Status::Satisfied);
                }
                s.rounds += 1;
                s.last_proxy = message.clone();
                push_log(s, "proxy", format!("({}) {message}", kind.intent()));
                out = Some((kind.intent().into(), message.clone(), worker_message(s, &message)));
            }
            Status::Satisfied => {
                let reason = v.reason.trim().to_string();
                s.review_note = if v.note.is_empty() { reason.clone() } else { v.note.clone() };
                s.rounds += 1;
                s.last_proxy = format!("已滿意：{reason}");
                push_log(s, "proxy", format!("(accept) Satisfied: {reason}"));
                ending = Some(Ending { phase: TaskPhase::Done, reason: reason.clone() });
                out = Some(("accept".into(), reason.clone(), satisfied_message(&reason)));
            }
            Status::HandBack => {
                let reason = if v.reason.trim().is_empty() {
                    "用戶代理判斷再推也沒有幫助，需要你決定下一步。".to_string()
                } else {
                    v.reason.trim().to_string()
                };
                s.review_note = if v.note.is_empty() { reason.clone() } else { v.note.clone() };
                s.rounds += 1;
                s.last_proxy = format!("交還使用者：{reason}");
                push_log(s, "proxy", format!("(hand_back) Handed back to the user: {reason}"));
                ending = Some(Ending { phase: TaskPhase::HandedBack, reason: reason.clone() });
                out = Some(("hand_back".into(), reason.clone(), hand_back_message(&reason)));
            }
            Status::Impossible => {
                let reason = if v.reason.trim().is_empty() {
                    "用戶代理判定目標因不可改變的限制而無法完成。".to_string()
                } else {
                    v.reason.trim().to_string()
                };
                s.phase = TaskPhase::ExplainingFail;
                s.fail_reason = reason.clone();
                s.review_note = reason.clone();
                s.rounds += 1;
                s.last_proxy = format!("無法完成：{reason}");
                push_log(s, "proxy", format!("(impossible) {reason}"));
                out = Some(("impossible".into(), reason.clone(), impossible_message(&reason)));
            }
        }
        true
    });
    if !applied {
        return None;
    }
    if ending.is_some() {
        hub.set_ending(ending);
    }
    let (intent, text, inject) = out?;
    emit_proxy(call.sink, call.run_id, &intent, text);
    Some(inject)
}

/// The worker wrote its closing turn after satisfied / hand_back.
fn finish_ending<P>(call: &ProxyCall<'_, P>, e: Ending) {
    let done = call.hub.update(|s| {
        if !s.phase.is_live() {
            return false;
        }
        s.phase = e.phase;
        s.review_note = e.reason.clone();
        s.skip_steer = false;
        true
    });
    if !done {
        return;
    }
    let msg = if e.phase == TaskPhase::Done {
        format!("用戶代理已驗收，任務完成：{}", e.reason)
    } else {
        format!("用戶代理把任務交還給你：{}", e.reason)
    };
    emit_notice(call.sink, call.run_id, msg);
}

fn finish_failed<P>(call: &ProxyCall<'_, P>) {
    let mut reason = String::new();
    call.hub.update(|s| {
        reason = if s.fail_reason.trim().is_empty() {
            "無法完成此任務。".into()
        } else {
            s.fail_reason.clone()
        };
        s.phase = TaskPhase::Failed;
        true
    });
    emit_notice(call.sink, call.run_id, format!("任務未完成。原因：{reason}"));
}

/// What the proxy made of an ask_user question.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Decision {
    /// `body` is what a human pick produces, marked as the proxy's.
    Answer { body: String, shown: String, why: String },
    Forward { why: String },
}

fn parse_decision(raw: &str, q: &Question) -> Result<Decision> {
    let v = extract_json(raw)?;
    let why = clip(v.get("why").and_then(Value::as_str).unwrap_or(""), REASON_CHARS);
    if v.get("forward").and_then(Value::as_bool).unwrap_or(false) {
        return Ok(Decision::Forward { why });
    }
    let Some(sel) = v.get("selected").and_then(Value::as_array) else {
        return Ok(Decision::Forward { why: "no answer given".into() });
    };
    let n = q.options.len();
    let mut chosen = vec![false; n];
    let mut values = vec![String::new(); n];
    for s in sel {
        let (id, value) = match s.as_str() {
            Some(id) => (id, ""),
            None => (
                s.get("id").and_then(Value::as_str).unwrap_or(""),
                s.get("value").and_then(Value::as_str).unwrap_or(""),
            ),
        };
        let at = q
            .options
            .iter()
            .position(|o| o.id == id)
            .or_else(|| q.options.iter().position(|o| o.label == id));
        if let Some(i) = at {
            chosen[i] = true;
            values[i] = value.to_string();
        }
    }
    let body = match answer_from_picks(q, &chosen, &values) {
        Ok(b) => b,
        Err(e) => return Ok(Decision::Forward { why: format!("the proxy's pick did not fit the options ({e})") }),
    };
    let shown = q
        .options
        .iter()
        .enumerate()
        .filter(|(i, _)| chosen[*i])
        .map(|(i, o)| {
            if o.input {
                format!("{}：{}", o.label, values[i].trim())
            } else {
                o.label.clone()
            }
        })
        .collect::<Vec<_>>()
        .join("、");
    // The worker should know who answered (the proxy is not the user).
    let body = match serde_json::from_str::<Value>(&body) {
        Ok(Value::Object(mut o)) => {
            o.insert("answered_by".into(), json!("user_proxy"));
            Value::Object(o).to_string()
        }
        _ => body,
    };
    Ok(Decision::Answer { body, shown, why })
}

/// Ask the proxy to answer the worker's ask_user question for the user.
pub async fn answer_question<P: Provider>(call: &ProxyCall<'_, P>, q: &Question) -> Delegated {
    let hub = call.hub;
    if !hub.steering_open() {
        return Delegated::Forward;
    }
    let snap = hub.snapshot();
    let epoch = hub.epoch();
    let prompt = ask_prompt(&snap, q, &hub.ask_context());
    let decision = match ask_twice(call, PROXY_ASK_INSTRUCTIONS, &prompt, false, |raw| parse_decision(raw, q)).await {
        Ok(d) => d,
        Err(e) => {
            proxy_failed(call, epoch, &e);
            return Delegated::Forward;
        }
    };
    match decision {
        Decision::Answer { body, shown, why } => {
            let text = format!("問：{}\n答：{shown}", q.prompt);
            let ok = hub.update(|s| {
                if !hub.same_task(epoch, s, &snap) {
                    return false;
                }
                push_log(s, "note", format!("You answered the worker's ask_user for the user.\nQ: {}\nA: {shown}\nWhy: {why}", q.prompt));
                s.last_proxy = text.clone();
                true
            });
            if !ok {
                return Delegated::Forward;
            }
            emit_proxy(call.sink, call.run_id, "answer", text);
            Delegated::Answer(body)
        }
        Decision::Forward { why } => {
            hub.update(|s| {
                push_log(s, "note", format!("You left the worker's question to the user: {} ({why})", q.prompt));
                s.phase.is_live()
            });
            Delegated::Forward
        }
    }
}

/// The ask_user delegate kit.rs hands the tool in task mode.
pub struct ProxyAsk<P> {
    hub: Arc<TaskHub>,
    provider: P,
    knobs: Option<Arc<Mutex<SessionKnobs>>>,
    model: String,
    effort: ReasoningEffort,
    workspace: PathBuf,
    sink: Arc<dyn EventSink>,
    run_id: String,
}

impl<P> ProxyAsk<P> {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        hub: Arc<TaskHub>,
        provider: P,
        knobs: Option<Arc<Mutex<SessionKnobs>>>,
        model: String,
        effort: ReasoningEffort,
        workspace: PathBuf,
        sink: Arc<dyn EventSink>,
        run_id: String,
    ) -> Self {
        Self { hub, provider, knobs, model, effort, workspace, sink, run_id }
    }
}

impl<P: Provider + 'static> AskDelegate for ProxyAsk<P> {
    fn consult<'a>(&'a self, question: &'a Question) -> Pin<Box<dyn Future<Output = Delegated> + Send + 'a>> {
        Box::pin(async move {
            let (model, effort, send_reasoning) = proxy_settings(self.knobs.as_ref(), &self.model, self.effort);
            let call = ProxyCall {
                hub: &self.hub,
                provider: &self.provider,
                model,
                effort,
                send_reasoning,
                run_id: &self.run_id,
                workspace: &self.workspace,
                sink: self.sink.as_ref(),
            };
            answer_question(&call, question).await
        })
    }
}

/// Watches the worker's tool rounds for signs it is stuck, so the proxy
/// can look in mid-run: a long stretch with no file change or proxy
/// message, the same failing call again and again, or the same call
/// repeated. At most one look per [`Watch::SPACING`] rounds.
#[derive(Debug, Default)]
pub struct Watch {
    round: u32,
    since_progress: u32,
    last_call: Option<String>,
    same_calls: u32,
    last_fail: Option<String>,
    same_fails: u32,
    last_look: Option<u32>,
}

impl Watch {
    pub const SPACING: u32 = 10;
    pub const STALL_ROUNDS: u32 = 30;
    pub const SAME_FAILS: u32 = 3;
    pub const SAME_CALLS: u32 = 4;

    /// One tool call and its output.
    pub fn call(&mut self, name: &str, args: &Value, output: &str) {
        let sig = format!("{name} {}", clip_ellipsis(&args.to_string(), CALL_ARGS_CHARS));
        if self.last_call.as_deref() == Some(sig.as_str()) {
            self.same_calls += 1;
        } else {
            self.last_call = Some(sig.clone());
            self.same_calls = 1;
        }
        if failed_output(output) {
            if self.last_fail.as_deref() == Some(sig.as_str()) {
                self.same_fails += 1;
            } else {
                self.last_fail = Some(sig);
                self.same_fails = 1;
            }
        } else {
            self.last_fail = None;
            self.same_fails = 0;
        }
    }

    /// A model response's tool batch finished.
    pub fn tool_round(&mut self) {
        self.round += 1;
        self.since_progress += 1;
    }

    /// A file changed, or the proxy spoke.
    pub fn progress(&mut self) {
        self.since_progress = 0;
    }

    /// Why the proxy should look now, if it should.
    pub fn due(&self) -> Option<String> {
        if self.last_look.is_some_and(|at| self.round < at + Self::SPACING) {
            return None;
        }
        if self.same_fails >= Self::SAME_FAILS {
            return Some(format!(
                "the same failing tool call, {} times in a row: {}",
                self.same_fails,
                self.last_fail.as_deref().unwrap_or("")
            ));
        }
        if self.same_calls >= Self::SAME_CALLS {
            return Some(format!(
                "the same tool call, {} times in a row: {}",
                self.same_calls,
                self.last_call.as_deref().unwrap_or("")
            ));
        }
        if self.since_progress >= Self::STALL_ROUNDS {
            return Some(format!(
                "{} tool rounds without a file change or a message from you",
                self.since_progress
            ));
        }
        None
    }

    /// The proxy looked (whatever it said).
    pub fn looked(&mut self) {
        self.last_look = Some(self.round);
    }
}

/// An error result, a nonzero exit or a timeout.
fn failed_output(output: &str) -> bool {
    let Ok(Value::Object(o)) = serde_json::from_str::<Value>(output) else {
        return false;
    };
    o.get("error").is_some_and(|e| !e.is_null())
        || o.get("exit_code").and_then(Value::as_i64).is_some_and(|c| c != 0)
        || o.get("timed_out").and_then(Value::as_bool) == Some(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::CompleteResponse;
    use std::collections::VecDeque;

    #[test]
    fn kick_marker() {
        assert!(is_kick(KICK));
        assert!(is_kick("  [grokaagent task] \n"));
        assert!(!is_kick("hello"));
    }

    #[test]
    fn an_old_task_json_still_loads() {
        let s: TaskState = serde_json::from_str(
            r#"{"goal":"x","phase":"active","continues":48,"checklist":[{"text":"a","done":true}],"claim":"no key","skip_steer":true,"spawned":true,"review_note":"n","fail_reason":""}"#,
        )
        .unwrap();
        assert_eq!(s.goal, "x");
        assert_eq!(s.phase, TaskPhase::Active);
        assert_eq!(s.checklist, vec![CheckItem { text: "a".into(), done: true }]);
        assert!(s.criteria.is_empty() && !s.paused && s.proxy_log.is_empty());
        assert_eq!((s.rounds, s.last_proxy.as_str()), (0, ""));
        let handed: TaskState = serde_json::from_str(r#"{"goal":"y","phase":"handed_back"}"#).unwrap();
        assert_eq!(handed.phase, TaskPhase::HandedBack);
        assert!(!handed.phase.is_live());
    }

    #[test]
    fn parses_fenced_json_and_synonyms() {
        let raw = "```json\n{\"status\":\"satisfied\",\"message\":\"\",\"checklist\":[{\"text\":\"寫 README\",\"done\":true}],\"note\":\"好\",\"reason\":\"README 已讀過\"}\n```";
        let v = parse_verdict(raw).unwrap();
        assert_eq!(v.status, Status::Satisfied);
        assert_eq!(v.checklist[0], ItemUpdate { text: "寫 README".into(), done: Some(true), dropped: false });
        assert_eq!(v.reason, "README 已讀過");
        for (s, want) in [("complete", Status::Satisfied), ("hand-back", Status::HandBack), ("handback", Status::HandBack), ("impossible", Status::Impossible), ("continue", Status::Continue)] {
            assert_eq!(parse_verdict(&format!(r#"{{"status":"{s}"}}"#)).unwrap().status, want, "{s}");
        }
        // The old supervisor field still reads as the message.
        assert_eq!(parse_verdict(r#"{"status":"continue","next":"跑測試"}"#).unwrap().message, "跑測試");
        assert!(parse_verdict(r#"{"message":"x"}"#).is_err());
        assert!(parse_verdict("no json here").is_err());
    }

    #[test]
    fn the_checklist_merge_keeps_omitted_items() {
        let old = vec![
            CheckItem { text: "寫測試".into(), done: false },
            CheckItem { text: "改 README".into(), done: true },
            CheckItem { text: "跑 CI".into(), done: false },
        ];
        let updates = parse_updates(Some(&json!([
            {"text": "寫測試", "done": true},
            {"text": "新增 changelog", "done": false},
            "改  readme",
        ])));
        let (list, dropped) = merge_checklist(&old, &updates, false);
        assert!(dropped.is_empty());
        assert_eq!(
            list,
            vec![
                CheckItem { text: "寫測試".into(), done: true },
                // A bare string leaves the item's state alone.
                CheckItem { text: "改 README".into(), done: true },
                // Omitted: kept, not silently dropped.
                CheckItem { text: "跑 CI".into(), done: false },
                CheckItem { text: "新增 changelog".into(), done: false },
            ]
        );
        // Dropping needs the flag and a reason.
        let drop = parse_updates(Some(&json!([{"text": "跑 CI", "dropped": true}])));
        assert_eq!(merge_checklist(&list, &drop, false).0.len(), 4);
        let (after, gone) = merge_checklist(&list, &drop, true);
        assert_eq!(gone, vec!["跑 CI".to_string()]);
        assert!(after.iter().all(|i| i.text != "跑 CI"));
    }

    #[test]
    fn the_excerpt_keeps_calls_output_tails_and_images_as_markers() {
        let long_log = format!("{}\ntest result: FAILED. 3 passed; 1 failed", "compiling crate\n".repeat(400));
        let items = vec![
            json!({"role": "user", "content": "修好登入"}),
            json!({"role": "user", "content": [{"type": "input_text", "text": "see"}, {"type": "input_image", "image_url": "data:image/jpeg;base64,AAAA"}]}),
            json!({"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": "I will run the tests"}]}),
            json!({"type": "reasoning", "encrypted_content": "zzz"}),
            json!({"type": "function_call", "call_id": "c1", "name": "run_command", "arguments": "{\"command\":\"cargo test\"}"}),
            json!({"type": "function_call_output", "call_id": "c1", "output": json!({"exit_code": 101, "stdout": long_log, "stderr": ""}).to_string()}),
        ];
        let s = history_excerpt(&items);
        assert!(s.contains("user: 修好登入"), "{s}");
        assert!(s.contains("[image]") && !s.contains("AAAA"), "{s}");
        assert!(s.contains("assistant: I will run the tests"), "{s}");
        assert!(s.contains(r#"tool call: run_command {"command":"cargo test"}"#), "{s}");
        assert!(s.contains("exit_code: 101"), "the head is kept: {s}");
        assert!(s.contains("1 failed"), "the tail is kept: {s}");
        assert!(s.contains("chars cut"), "{s}");
        assert!(!s.contains("zzz"));
        // The budget keeps the newest part.
        let many: Vec<Value> = (0..200).map(|i| json!({"role": "user", "content": format!("msg{i} {}", "x".repeat(1_000))})).collect();
        let s = history_excerpt(&many);
        assert!(s.chars().count() <= EXCERPT_CHARS + 40);
        assert!(s.contains("msg199") && !s.contains("msg0 "), "newest kept");
        assert!(s.starts_with("[… older conversation cut …]"));
    }

    #[test]
    fn the_worker_message_says_it_is_not_the_user() {
        let s = TaskState {
            goal: "做登入".into(),
            criteria: "要有測試".into(),
            checklist: vec![CheckItem { text: "寫測試".into(), done: false }, CheckItem { text: "做表單".into(), done: true }],
            ..TaskState::default()
        };
        let m = worker_message(&s, "先把測試寫出來。");
        assert!(m.starts_with("[user-proxy] Message from the user's proxy"), "{m}");
        assert!(m.contains("It is not the user.") && m.contains("The user's own messages always take priority."));
        assert!(m.contains("先把測試寫出來。"));
        assert!(m.contains("Goal: 做登入") && m.contains("Criteria: 要有測試"));
        assert!(m.contains("- [ ] 寫測試") && !m.contains("做表單"));
        assert!(crate::notice::is_harness(&m));
        // Its own past message reads back without the header.
        let ex = history_excerpt(&[json!({"role": "user", "content": m})]);
        assert!(ex.starts_with("you (the proxy): 先把測試寫出來。"), "{ex}");
        assert!(!ex.contains("Goal:"), "{ex}");
    }

    #[test]
    fn memory_keeps_the_newest_entries_within_budget() {
        let mut s = TaskState::default();
        for i in 0..60 {
            push_log(&mut s, "worker", format!("reply {i} {}", "y".repeat(800)));
        }
        assert_eq!(s.proxy_log.len(), MAX_LOG);
        assert!(s.proxy_log[0].text.starts_with("reply 20 "));
        let m = memory_text(&s.proxy_log);
        assert!(m.chars().count() <= MEMORY_CHARS + LOG_ENTRY_CHARS);
        assert!(m.contains("reply 59 ") && !m.contains("reply 20 "));
    }

    #[test]
    fn start_goal_kicks_and_end_clears() {
        let hub = TaskHub::new("");
        hub.start_goal("  把登入做完  ");
        assert!(hub.take_kick());
        let s = hub.snapshot();
        assert_eq!(s.phase, TaskPhase::NeedPlan);
        assert_eq!(s.goal, "把登入做完");
        hub.end();
        assert_eq!(hub.snapshot().phase, TaskPhase::Inactive);
        assert!(hub.snapshot().goal.is_empty());
    }

    #[tokio::test]
    async fn task_report_kinds_are_recorded_and_shown() {
        let hub = TaskHub::new("");
        let rec = Arc::new(Rec(Mutex::new(Vec::new())));
        let tool = TaskReportTool::new(hub.clone()).with_events(rec.clone(), "r".into());
        let idle = tool.call(&json!({"kind": "done", "summary": "x"})).await.unwrap();
        assert!(idle.contains("not active"), "{idle}");
        assert!(tool.call(&json!({"kind": "maybe", "summary": "x"})).await.is_err());
        hub.start_goal("x");
        assert!(tool.call(&json!({"kind": "done", "summary": ""})).await.is_err());
        let ok = tool.call(&json!({"kind": "done", "summary": "登入完成", "evidence": "cargo test: 12 passed"})).await.unwrap();
        assert!(ok.contains("\"recorded\":true"), "{ok}");
        tool.call(&json!({"kind": "question", "summary": "要不要支援 OAuth？"})).await.unwrap();
        tool.call(&json!({"kind": "progress", "summary": "表單做好了"})).await.unwrap();
        // The old shape (reason) still works.
        tool.call(&json!({"kind": "impossible", "reason": "no API key exists"})).await.unwrap();
        let s = hub.snapshot();
        assert_eq!(s.claim.as_deref(), Some("no API key exists"));
        let log: Vec<&str> = s.proxy_log.iter().map(|t| t.text.as_str()).collect();
        assert_eq!(log.len(), 4);
        assert!(log[0].contains("done: 登入完成") && log[0].contains("12 passed"), "{log:?}");
        assert!(log[1].contains("question: 要不要支援 OAuth？"));
        assert!(s.proxy_log.iter().all(|t| t.who == "worker"));
        let events = rec.0.lock().unwrap();
        let reports: Vec<&str> = events
            .iter()
            .filter_map(|e| match e {
                AgentEvent::TaskProxy { intent, text, .. } if intent == "report" => Some(text.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(reports.len(), 4);
        assert!(reports[0].starts_with("宣稱完成：登入完成") && reports[0].contains("證據：cargo test"), "{reports:?}");
    }

    #[test]
    fn the_watch_spots_stalls_repeats_and_failing_loops() {
        let fail = r#"{"exit_code":1,"stdout":"","stderr":"boom"}"#;
        let ok = r#"{"exit_code":0,"stdout":"fine"}"#;
        let mut w = Watch::default();
        let args = json!({"command": "cargo test"});
        for _ in 0..2 {
            w.call("run_command", &args, fail);
            w.tool_round();
        }
        assert!(w.due().is_none());
        w.call("run_command", &args, fail);
        w.tool_round();
        let why = w.due().expect("3 failures in a row");
        assert!(why.contains("failing") && why.contains("cargo test"), "{why}");
        w.looked();
        // At most one look per 10 rounds.
        w.call("run_command", &args, fail);
        w.tool_round();
        assert!(w.due().is_none());

        let mut w = Watch::default();
        for _ in 0..3 {
            w.call("read_file", &json!({"path": "a.rs"}), ok);
            w.tool_round();
        }
        assert!(w.due().is_none());
        w.call("read_file", &json!({"path": "a.rs"}), ok);
        assert!(w.due().unwrap().contains("same tool call"));

        let mut w = Watch::default();
        for i in 0..29 {
            w.call("read_file", &json!({"path": format!("{i}.rs")}), ok);
            w.tool_round();
            if i == 10 {
                w.progress();
            }
        }
        assert!(w.due().is_none(), "progress resets the stall count");
        for i in 0..12 {
            w.call("read_file", &json!({"path": format!("b{i}.rs")}), ok);
            w.tool_round();
        }
        assert!(w.due().unwrap().contains("without a file change"));
        assert!(failed_output(r#"{"error":"no such file"}"#));
        assert!(failed_output(r#"{"timed_out":true,"exit_code":null}"#));
        assert!(!failed_output(r#"{"exit_code":0}"#) && !failed_output("plain text"));
    }

    /// Replies in order and records each request.
    struct Proxy {
        calls: Mutex<Vec<CompleteRequest>>,
        replies: Mutex<VecDeque<Result<CompleteResponse>>>,
    }

    impl Proxy {
        fn new(replies: Vec<Result<CompleteResponse>>) -> Self {
            Self { calls: Mutex::new(Vec::new()), replies: Mutex::new(replies.into()) }
        }

        fn n(&self) -> usize {
            self.calls.lock().unwrap().len()
        }
    }

    impl Provider for Proxy {
        async fn complete(&self, req: CompleteRequest) -> Result<CompleteResponse> {
            self.calls.lock().unwrap().push(req);
            self.replies
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or_else(|| Err(Error::Provider("no scripted reply".into())))
        }

        async fn compact(
            &self,
            _req: crate::provider::CompactRequest,
        ) -> Result<crate::provider::CompactResponse> {
            Err(Error::Provider("no compact".into()))
        }
    }

    fn says(v: Value) -> Result<CompleteResponse> {
        Ok(CompleteResponse { text: v.to_string(), ..CompleteResponse::new("p") })
    }

    fn call_tool(name: &str, args: &str) -> Result<CompleteResponse> {
        Ok(CompleteResponse {
            function_calls: vec![FunctionCall { call_id: format!("p{}", args.len()), name: name.into(), arguments: args.into() }],
            ..CompleteResponse::new("p")
        })
    }

    fn proxy_call<'a>(hub: &'a TaskHub, provider: &'a Proxy, ws: &'a Path, sink: &'a Rec) -> ProxyCall<'a, Proxy> {
        ProxyCall {
            hub,
            provider,
            model: "proxy-model".into(),
            effort: ReasoningEffort::Medium,
            send_reasoning: true,
            run_id: "r",
            workspace: ws,
            sink,
        }
    }

    #[tokio::test]
    async fn the_proxy_tool_loop_is_bounded() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "hello\n").unwrap();
        let mut replies: Vec<Result<CompleteResponse>> = (0..12).map(|_| call_tool("read_file", r#"{"path":"a.txt"}"#)).collect();
        replies.truncate(MAX_PROXY_TOOL_CALLS);
        replies.push(says(json!({"status": "continue", "message": "再跑一次測試", "checklist": [{"text": "測試通過", "done": false}]})));
        let provider = Proxy::new(replies);
        let hub = TaskHub::new("");
        hub.start_goal("做登入");
        let rec = Rec(Mutex::new(Vec::new()));
        let call = proxy_call(&hub, &provider, dir.path(), &rec);
        let out = prepare_first_turn(&call, &[]).await.expect("a plan message");
        assert!(out.contains("再跑一次測試"), "{out}");
        let reqs = provider.calls.lock().unwrap();
        assert_eq!(reqs.len(), MAX_PROXY_TOOL_CALLS + 1);
        assert!(reqs[..MAX_PROXY_TOOL_CALLS].iter().all(|r| r.tool_choice.is_none()));
        let last = reqs.last().unwrap();
        assert_eq!(last.tool_choice.as_deref(), Some("none"), "the final answer is forced");
        let outputs: Vec<&Value> = last.input.iter().filter(|i| i["type"] == "function_call_output").collect();
        assert_eq!(outputs.len(), MAX_PROXY_TOOL_CALLS);
        assert!(outputs[0]["output"].as_str().unwrap().contains("hello"), "{}", outputs[0]);
        let names: Vec<&str> = last.client_tools.iter().map(|t| t.name.as_str()).collect();
        assert_eq!(names, ["read_file", "list_dir", "workspace_changes"]);
        assert_eq!(last.model, "proxy-model");
        assert!(last.cache_key.starts_with("grokaagent-task:"));
        assert!(last.instructions.starts_with("You are the user's proxy"));
        // A proxy that ignores the forced answer is an error, not a loop.
        let stubborn = Proxy::new((0..30).map(|_| call_tool("list_dir", "{}")).collect());
        let hub = TaskHub::new("");
        hub.start_goal("x");
        let call = proxy_call(&hub, &stubborn, dir.path(), &rec);
        assert!(prepare_first_turn(&call, &[]).await.is_none());
        assert!(stubborn.n() <= 2 * (MAX_PROXY_TOOL_CALLS + 1), "{}", stubborn.n());
        assert!(hub.snapshot().paused);
    }

    #[test]
    fn workspace_changes_is_empty_outside_git() {
        let dir = tempfile::tempdir().unwrap();
        let v: Value = serde_json::from_str(&workspace_changes(dir.path())).unwrap();
        if v["git"] == false {
            assert_eq!(v["status"], "");
            assert_eq!(v["diff_stat"], "");
        }
    }

    #[tokio::test]
    async fn an_error_is_retried_once_then_pauses() {
        let dir = tempfile::tempdir().unwrap();
        let hub = TaskHub::new("");
        hub.start_goal("做登入");
        let rec = Rec(Mutex::new(Vec::new()));
        // A transient failure: the retry answers.
        let provider = Proxy::new(vec![
            Err(Error::Provider("HTTP 503".into())),
            says(json!({"status": "continue", "message": "開始吧"})),
        ]);
        let out = prepare_first_turn(&proxy_call(&hub, &provider, dir.path(), &rec), &[]).await;
        assert!(out.unwrap().contains("開始吧"));
        assert!(!hub.snapshot().paused);
        // Two failures: paused with a note; no more calls while paused.
        let provider = Proxy::new(vec![Err(Error::Provider("HTTP 500".into())), says(json!({"oops": 1}))]);
        let call = proxy_call(&hub, &provider, dir.path(), &rec);
        assert!(after_natural_stop(&call, &[], "我做完了").await.is_none());
        assert_eq!(provider.n(), 2);
        let s = hub.snapshot();
        assert!(s.paused);
        assert!(s.review_note.contains("已暫停"), "{}", s.review_note);
        assert!(rec.0.lock().unwrap().iter().any(|e| matches!(e, AgentEvent::Notice { message, .. } if message.contains("任務已暫停"))));
        assert!(after_natural_stop(&call, &[], "還在嗎").await.is_none());
        assert!(after_wait(&call, &[], "", &[]).await.is_none());
        assert!(checkpoint(&call, &[], "", "stuck").await.is_none());
        assert_eq!(provider.n(), 2, "paused: no proxy calls at all");
        // Resuming makes the idle run look again.
        let provider = Proxy::new(vec![says(json!({"status": "continue", "message": "繼續做表單"}))]);
        hub.set_paused(false);
        let call = proxy_call(&hub, &provider, dir.path(), &rec);
        let out = after_wait(&call, &[], "", &[]).await.unwrap();
        assert!(out.contains("繼續做表單"));
        assert!(provider.calls.lock().unwrap()[0].input[0]["content"].as_str().unwrap().contains("the user resumed you"));
    }

    #[tokio::test]
    async fn a_model_that_rejects_the_effort_is_retried_without_it() {
        let dir = tempfile::tempdir().unwrap();
        let hub = TaskHub::new("");
        hub.start_goal("x");
        let rec = Rec(Mutex::new(Vec::new()));
        let provider = Proxy::new(vec![
            Err(Error::Provider("HTTP 400: model does not support reasoning effort".into())),
            says(json!({"status": "continue", "message": "go"})),
        ]);
        prepare_first_turn(&proxy_call(&hub, &provider, dir.path(), &rec), &[]).await.unwrap();
        let reqs = provider.calls.lock().unwrap();
        assert!(reqs[0].send_reasoning && !reqs[1].send_reasoning);
    }

    #[tokio::test]
    async fn satisfied_needs_every_item_done_and_a_reason() {
        let dir = tempfile::tempdir().unwrap();
        let hub = TaskHub::new("");
        hub.start_goal("做登入");
        let rec = Rec(Mutex::new(Vec::new()));
        let provider = Proxy::new(vec![
            says(json!({"status": "continue", "message": "寫測試", "checklist": [{"text": "寫測試", "done": false}, {"text": "做表單", "done": false}]})),
            // Claims satisfied but omits an undone item: it is kept, so not done.
            says(json!({"status": "satisfied", "checklist": [{"text": "寫測試", "done": true}], "reason": "tests pass"})),
            says(json!({"status": "satisfied", "checklist": [{"text": "做表單", "done": true}], "reason": ""})),
            says(json!({"status": "satisfied", "checklist": [], "note": "驗收", "reason": "cargo test: 4 passed; form.rs read"})),
        ]);
        let call = proxy_call(&hub, &provider, dir.path(), &rec);
        prepare_first_turn(&call, &[]).await.unwrap();
        let out = after_natural_stop(&call, &[], "done").await.unwrap();
        assert!(out.contains("still open") && out.contains("做表單"), "{out}");
        assert!(!hub.ending_pending());
        let out = after_natural_stop(&call, &[], "done").await.unwrap();
        assert!(out.contains("evidence"), "no reason, no acceptance: {out}");
        let out = after_natural_stop(&call, &[], "done, see tests").await.unwrap();
        assert!(out.starts_with("[user-proxy] The user's proxy is satisfied: cargo test: 4 passed; form.rs read. Write the final summary"), "{out}");
        assert!(hub.ending_pending());
        assert_eq!(hub.snapshot().phase, TaskPhase::Active, "Done only after the summary turn");
        // The summary turn ends the task without another proxy call.
        assert!(after_natural_stop(&call, &[], "總結：…").await.is_none());
        let s = hub.snapshot();
        assert_eq!(s.phase, TaskPhase::Done);
        assert_eq!(provider.n(), 4);
        assert_eq!(s.rounds, 4);
        assert!(s.last_proxy.contains("cargo test"));
        let intents: Vec<String> = rec
            .0
            .lock()
            .unwrap()
            .iter()
            .filter_map(|e| match e {
                AgentEvent::TaskProxy { intent, .. } => Some(intent.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(intents, ["plan", "push", "push", "accept"]);
    }

    #[tokio::test]
    async fn edits_and_user_messages_reach_the_proxy() {
        let dir = tempfile::tempdir().unwrap();
        let hub = TaskHub::new("");
        hub.start_task("做登入", "要有測試");
        let rec = Rec(Mutex::new(Vec::new()));
        let provider = Proxy::new(vec![
            says(json!({"status": "continue", "message": "開始"})),
            says(json!({"status": "continue", "message": "改用 OAuth 吧"})),
        ]);
        let call = proxy_call(&hub, &provider, dir.path(), &rec);
        prepare_first_turn(&call, &[]).await.unwrap();
        assert!(after_wait(&call, &[], "", &["用 SQLite 就好".into()]).await.is_none(), "the worker handles the user first");
        hub.edit_task("做 OAuth 登入", "要有測試，不要密碼");
        let out = after_wait(&call, &[], "", &[]).await.expect("an edit asks for a review");
        assert!(out.contains("改用 OAuth 吧") && out.contains("Goal: 做 OAuth 登入"), "{out}");
        let reqs = provider.calls.lock().unwrap();
        let prompt = reqs[1].input[0]["content"].as_str().unwrap();
        assert!(prompt.contains("[user] 用 SQLite 就好"), "{prompt}");
        assert!(prompt.contains("The user changed the task.") && prompt.contains("不要密碼"), "{prompt}");
        assert!(prompt.contains("the user asked you to look now"), "{prompt}");
        assert!(prompt.contains("Review kind: push"));
        assert!(reqs[0].input[0]["content"].as_str().unwrap().contains("Review kind: plan"));
        assert!(reqs[0].input[0]["content"].as_str().unwrap().contains("要有測試"));
    }

    #[tokio::test]
    async fn a_review_for_a_replaced_task_is_dropped() {
        struct Swap<'a>(&'a TaskHub, Proxy);
        impl Provider for Swap<'_> {
            async fn complete(&self, req: CompleteRequest) -> Result<CompleteResponse> {
                self.0.start_goal("別的目標");
                self.1.complete(req).await
            }
            async fn compact(&self, r: crate::provider::CompactRequest) -> Result<crate::provider::CompactResponse> {
                self.1.compact(r).await
            }
        }
        let dir = tempfile::tempdir().unwrap();
        let hub = TaskHub::new("");
        hub.start_goal("做登入");
        let rec = Rec(Mutex::new(Vec::new()));
        let provider = Swap(&hub, Proxy::new(vec![says(json!({"status": "continue", "message": "x"}))]));
        let call = ProxyCall {
            hub: &hub,
            provider: &provider,
            model: "m".into(),
            effort: ReasoningEffort::Low,
            send_reasoning: false,
            run_id: "r",
            workspace: dir.path(),
            sink: &rec,
        };
        assert!(prepare_first_turn(&call, &[]).await.is_none());
        let s = hub.snapshot();
        assert_eq!((s.goal.as_str(), s.phase, s.rounds), ("別的目標", TaskPhase::NeedPlan, 0));
    }

    #[tokio::test]
    async fn a_checkpoint_with_no_message_lets_the_worker_carry_on() {
        let dir = tempfile::tempdir().unwrap();
        let hub = TaskHub::new("");
        hub.start_goal("做登入");
        let rec = Rec(Mutex::new(Vec::new()));
        let provider = Proxy::new(vec![
            says(json!({"status": "continue", "message": "開始"})),
            says(json!({"status": "continue", "message": "", "note": "還在正軌上"})),
            says(json!({"status": "continue", "message": "同一個指令失敗三次了，先讀錯誤訊息"})),
        ]);
        let call = proxy_call(&hub, &provider, dir.path(), &rec);
        prepare_first_turn(&call, &[]).await.unwrap();
        assert!(checkpoint(&call, &[], "", "30 tool rounds").await.is_none());
        assert_eq!(hub.snapshot().review_note, "還在正軌上");
        assert_eq!(hub.snapshot().rounds, 1);
        let out = checkpoint(&call, &[], "", "the same failing tool call").await.unwrap();
        assert!(out.contains("失敗三次"));
        let prompt = provider.calls.lock().unwrap()[2].input[0]["content"].as_str().unwrap().to_string();
        assert!(prompt.contains("Review kind: checkpoint") && prompt.contains("the same failing tool call"), "{prompt}");
    }

    #[tokio::test]
    async fn hand_back_and_impossible_end_after_the_worker_explains() {
        let dir = tempfile::tempdir().unwrap();
        let rec = Rec(Mutex::new(Vec::new()));
        let hub = TaskHub::new("");
        hub.start_goal("部署到正式環境");
        let provider = Proxy::new(vec![
            says(json!({"status": "hand_back", "reason": "要部署到哪個區域只有你能決定"})),
            says(json!({"status": "impossible", "reason": "沒有任何憑證"})),
        ]);
        let call = proxy_call(&hub, &provider, dir.path(), &rec);
        let out = prepare_first_turn(&call, &[]).await.unwrap();
        assert!(out.starts_with("[user-proxy] The user's proxy is handing this back to the user: 要部署到哪個區域只有你能決定. Stop working"), "{out}");
        assert!(after_natural_stop(&call, &[], "summary").await.is_none());
        assert_eq!(hub.snapshot().phase, TaskPhase::HandedBack);
        hub.start_goal("部署到正式環境");
        let out = prepare_first_turn(&call, &[]).await.unwrap();
        assert!(out.contains("cannot be completed: 沒有任何憑證"), "{out}");
        assert_eq!(hub.snapshot().phase, TaskPhase::ExplainingFail);
        assert!(after_natural_stop(&call, &[], "沒做完").await.is_none());
        assert_eq!(hub.snapshot().phase, TaskPhase::Failed);
        assert_eq!(provider.n(), 2);
    }

    #[tokio::test]
    async fn accept_done_stops_the_pushing() {
        let dir = tempfile::tempdir().unwrap();
        let rec = Rec(Mutex::new(Vec::new()));
        let hub = TaskHub::new("");
        hub.start_goal("x");
        let provider = Proxy::new(vec![says(json!({"status": "continue", "message": "go"}))]);
        let call = proxy_call(&hub, &provider, dir.path(), &rec);
        prepare_first_turn(&call, &[]).await.unwrap();
        hub.accept_done();
        assert!(after_natural_stop(&call, &[], "more").await.is_none());
        assert!(checkpoint(&call, &[], "", "x").await.is_none());
        assert_eq!(provider.n(), 1);
        assert_eq!(hub.snapshot().phase, TaskPhase::Done);
    }

    fn question() -> Question {
        crate::ask::parse_question(&json!({
            "question": "用哪個資料庫？",
            "options": [{"id": "sqlite", "label": "SQLite"}, {"id": "pg", "label": "Postgres"}, {"id": "other", "label": "其他", "input": true}]
        }))
        .unwrap()
    }

    #[tokio::test]
    async fn the_proxy_answers_or_forwards_ask_user() {
        let dir = tempfile::tempdir().unwrap();
        let rec = Rec(Mutex::new(Vec::new()));
        let hub = TaskHub::new("");
        hub.start_task("做登入，資料存 SQLite", "");
        hub.set_ask_context("The user's own messages, oldest first:\n- 用 SQLite".into());
        let provider = Proxy::new(vec![
            says(json!({"forward": false, "selected": [{"id": "sqlite"}], "why": "目標寫了 SQLite"})),
            says(json!({"forward": true, "why": "使用者沒說過"})),
            says(json!({"forward": false, "selected": [{"id": "other", "value": ""}], "why": "?"})),
        ]);
        let call = proxy_call(&hub, &provider, dir.path(), &rec);
        let q = question();
        let Delegated::Answer(body) = answer_question(&call, &q).await else { panic!("answered") };
        let v: Value = serde_json::from_str(&body).unwrap();
        assert_eq!(v["cancelled"], false);
        assert_eq!(v["selected"][0]["id"], "sqlite");
        assert_eq!(v["selected"][0]["label"], "SQLite");
        assert_eq!(v["answered_by"], "user_proxy");
        let req = provider.calls.lock().unwrap()[0].clone();
        assert!(req.client_tools.is_empty());
        let prompt = req.input[0]["content"].as_str().unwrap();
        assert!(prompt.contains("id=sqlite: SQLite") && prompt.contains("- 用 SQLite"), "{prompt}");
        assert_eq!(answer_question(&call, &q).await, Delegated::Forward);
        // A pick that does not fit (an input option left empty) goes to the human.
        assert_eq!(answer_question(&call, &q).await, Delegated::Forward);
        let events = rec.0.lock().unwrap();
        let answers: Vec<&str> = events
            .iter()
            .filter_map(|e| match e {
                AgentEvent::TaskProxy { intent, text, .. } if intent == "answer" => Some(text.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(answers, ["問：用哪個資料庫？\n答：SQLite"]);
        let s = hub.snapshot();
        assert!(s.proxy_log.iter().any(|t| t.text.contains("You answered the worker's ask_user")));
        assert!(s.proxy_log.iter().any(|t| t.text.contains("You left the worker's question to the user")));
        drop(events);
        // Paused: straight to the human, no call.
        hub.set_paused(true);
        assert_eq!(answer_question(&call, &q).await, Delegated::Forward);
        assert_eq!(provider.n(), 3);
    }

    #[test]
    fn proxy_settings_default_to_the_main_model() {
        let knobs = Arc::new(Mutex::new(SessionKnobs {
            model: "grok-main".into(),
            reasoning_effort: ReasoningEffort::Xhigh,
            send_reasoning: true,
            server_tools: vec![],
            dispatcher: false,
            child_model: "grok-child".into(),
            task_model: String::new(),
            search: false,
        }));
        assert_eq!(proxy_settings(Some(&knobs), "x", ReasoningEffort::Low), ("grok-main".into(), ReasoningEffort::Xhigh, true));
        knobs.lock().unwrap().task_model = " other-proxy ".into();
        assert_eq!(proxy_settings(Some(&knobs), "x", ReasoningEffort::Low).0, "other-proxy");
        assert_eq!(proxy_settings(None, "cli-model", ReasoningEffort::Low), ("cli-model".into(), ReasoningEffort::Low, true));
    }

    struct Rec(Mutex<Vec<AgentEvent>>);
    impl EventSink for Rec {
        fn emit(&self, event: &AgentEvent) {
            self.0.lock().unwrap().push(event.clone());
        }
    }
}
