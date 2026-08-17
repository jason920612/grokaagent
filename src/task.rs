//! Task mode: a supervisor sub-agent writes a checklist from live context,
//! then reviews each time the main agent would stop.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::sync::Notify;

use crate::error::{Error, Result};
use crate::events::{AgentEvent, EventMeta, EventSink};
use crate::provider::{CompleteRequest, Provider, ReasoningEffort};
use crate::tools::{ClientTool, ToolCallFut, ToolSpec};

pub const AGENT_NAME: &str = "任務系統";
pub const KICK: &str = "[grokaagent task]";
pub const MAX_GOAL: usize = 2000;
pub const MAX_ITEMS: usize = 20;
const EXCERPT_CHARS: usize = 24_000;
const NEXT_CHARS: usize = 1_200;

pub fn is_kick(s: &str) -> bool {
    s.trim() == KICK
}

const SUPERVISOR_INSTRUCTIONS: &str = r#"You are the grokaagent task supervisor, not the worker.
You never edit files or call tools. You only judge progress against the user's goal.

Return ONE JSON object, no markdown, no extra keys:
{"status":"continue"|"complete"|"impossible","checklist":[{"text":"...","done":true|false}],"next":"...","reason":"..."}

Rules:
- status=complete only when EVERY checklist item is done AND the original goal is fully met. Partial work is continue.
- status=impossible only when an unchangeable constraint blocks the goal (missing credentials the user cannot provide here, physical/legal impossibility, workspace that cannot contain the required artifact). Hard, slow, or unfinished work is continue, not impossible.
- checklist: 3–12 concrete, verifiable items. Keep stable wording across reviews; mark done when the conversation shows the item is actually finished.
- next: one short instruction for the worker agent in the user's language. Empty when complete or impossible.
- reason: empty on continue; on complete say what was achieved; on impossible name the unchangeable constraint.
- If the worker claimed impossibility, accept only when the claim is actually unchangeable. Otherwise status=continue and tell them why they must keep going.
"#;

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
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct TaskState {
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

pub struct TaskHub {
    session_id: String,
    inner: Mutex<TaskState>,
    kick_pending: AtomicBool,
    kick: Notify,
}

impl TaskHub {
    pub fn new(session_id: impl Into<String>) -> Arc<Self> {
        Arc::new(Self {
            session_id: session_id.into(),
            inner: Mutex::new(TaskState::default()),
            kick_pending: AtomicBool::new(false),
            kick: Notify::new(),
        })
    }

    pub fn from_state(session_id: impl Into<String>, state: TaskState) -> Arc<Self> {
        Arc::new(Self {
            session_id: session_id.into(),
            inner: Mutex::new(state),
            kick_pending: AtomicBool::new(false),
            kick: Notify::new(),
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

    pub fn start_goal(&self, goal: impl Into<String>) {
        let goal = clip(&goal.into(), MAX_GOAL);
        let mut g = self.lock();
        *g = TaskState {
            goal,
            phase: TaskPhase::NeedPlan,
            ..TaskState::default()
        };
        drop(g);
        self.persist();
        self.kick();
    }

    pub fn end(&self) {
        let mut g = self.lock();
        *g = TaskState::default();
        drop(g);
        self.persist();
        self.kick();
    }

    pub fn set_skip_steer(&self, skip: bool) {
        self.lock().skip_steer = skip;
        self.persist();
    }

    pub fn record_claim(&self, reason: String) -> String {
        let mut g = self.lock();
        if !g.phase.is_live() {
            return json!({
                "ok": false,
                "error": "task mode is not active",
            })
            .to_string();
        }
        let reason = clip(&reason, 800);
        g.claim = Some(reason.clone());
        drop(g);
        self.persist();
        json!({
            "ok": true,
            "recorded": true,
            "reason": reason,
            "note": "Finish this turn. The task supervisor will evaluate when you stop. If confirmed, you will be asked to explain to the user. If rejected, keep working.",
        })
        .to_string()
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
}

pub struct TaskReportTool {
    hub: Arc<TaskHub>,
}

impl TaskReportTool {
    pub fn new(hub: Arc<TaskHub>) -> Self {
        Self { hub }
    }
}

impl ClientTool for TaskReportTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "task_report".into(),
            description: "Tell the task supervisor that the current goal cannot be completed because of an unchangeable constraint. Do not use this because the work is merely hard or unfinished.".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "kind": {
                        "type": "string",
                        "enum": ["impossible"],
                        "description": "Must be impossible."
                    },
                    "reason": {
                        "type": "string",
                        "description": "The unchangeable constraint, in concrete terms."
                    }
                },
                "required": ["kind", "reason"],
                "additionalProperties": false
            }),
        }
    }

    fn call(&self, args: &Value) -> ToolCallFut<'_> {
        let kind = args
            .get("kind")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim()
            .to_ascii_lowercase();
        let reason = args
            .get("reason")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim()
            .to_string();
        let hub = self.hub.clone();
        Box::pin(async move {
            if kind != "impossible" {
                return Err(Error::Tool(
                    "task_report kind must be \"impossible\"".into(),
                ));
            }
            if reason.is_empty() {
                return Err(Error::Tool("reason is required".into()));
            }
            Ok(hub.record_claim(reason))
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Verdict {
    status: Status,
    checklist: Vec<CheckItem>,
    next: String,
    reason: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Status {
    Continue,
    Complete,
    Impossible,
}

impl Status {
    fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "continue" | "cont" => Some(Self::Continue),
            "complete" | "done" => Some(Self::Complete),
            "impossible" | "blocked" | "fail" => Some(Self::Impossible),
            _ => None,
        }
    }
}

fn parse_verdict_inner(raw: &str) -> Result<Verdict> {
    let v = extract_json(raw)?;
    let status = v
        .get("status")
        .and_then(Value::as_str)
        .and_then(Status::parse)
        .ok_or_else(|| Error::Provider("task supervisor missing status".into()))?;
    let checklist = parse_checklist(v.get("checklist"));
    let next = clip(
        v.get("next").and_then(Value::as_str).unwrap_or(""),
        NEXT_CHARS,
    );
    let reason = clip(
        v.get("reason").and_then(Value::as_str).unwrap_or(""),
        800,
    );
    if matches!(status, Status::Continue) && next.trim().is_empty() && checklist.iter().all(|i| i.done)
    {
        return Ok(Verdict {
            status: Status::Complete,
            checklist,
            next: String::new(),
            reason: if reason.is_empty() {
                "checklist complete".into()
            } else {
                reason
            },
        });
    }
    Ok(Verdict {
        status,
        checklist,
        next,
        reason,
    })
}

fn parse_checklist(v: Option<&Value>) -> Vec<CheckItem> {
    let Some(arr) = v.and_then(Value::as_array) else {
        return Vec::new();
    };
    arr.iter()
        .filter_map(|item| {
            if let Some(s) = item.as_str() {
                let text = clip(s, 200);
                if text.is_empty() {
                    return None;
                }
                return Some(CheckItem { text, done: false });
            }
            let text = item
                .get("text")
                .or_else(|| item.get("item"))
                .and_then(Value::as_str)
                .unwrap_or("")
                .trim();
            let text = clip(text, 200);
            if text.is_empty() {
                return None;
            }
            Some(CheckItem {
                text,
                done: item
                    .get("done")
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
            })
        })
        .take(MAX_ITEMS)
        .collect()
}

fn extract_json(raw: &str) -> Result<Value> {
    let t = raw.trim();
    if let Ok(v) = serde_json::from_str::<Value>(t) {
        return Ok(v);
    }
    let start = t.find('{').ok_or_else(|| {
        Error::Provider("task supervisor did not return JSON".into())
    })?;
    let end = t.rfind('}').ok_or_else(|| {
        Error::Provider("task supervisor did not return JSON".into())
    })?;
    serde_json::from_str(&t[start..=end])
        .map_err(|_| Error::Provider("task supervisor JSON was invalid".into()))
}

fn clip(s: &str, max: usize) -> String {
    let t = s.trim();
    if t.chars().count() <= max {
        return t.to_string();
    }
    t.chars().take(max).collect()
}

pub fn history_excerpt(items: &[Value]) -> String {
    let mut chunks = Vec::new();
    for item in items {
        let role = item
            .get("role")
            .and_then(Value::as_str)
            .or_else(|| item.get("type").and_then(Value::as_str))
            .unwrap_or("item");
        let text = item_plain(item);
        if text.trim().is_empty() {
            continue;
        }
        let text = clip(&text, 1_200);
        chunks.push(format!("{role}: {text}"));
    }
    let mut out = chunks.join("\n");
    if out.chars().count() > EXCERPT_CHARS {
        out = out.chars().skip(out.chars().count() - EXCERPT_CHARS).collect();
    }
    out
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
        if part.get("type").and_then(Value::as_str) == Some("input_image") {
            continue;
        }
        if let Some(t) = part.get("text").and_then(Value::as_str) {
            if !out.is_empty() {
                out.push('\n');
            }
            out.push_str(&strip_image(t));
        }
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

fn meta(run_id: &str) -> EventMeta {
    EventMeta {
        ts: chrono::Utc::now(),
        agent_name: "root".into(),
        run_id: run_id.to_string(),
        parent_run_id: None,
    }
}

fn emit_spawn(sink: &dyn EventSink, run_id: &str, goal: &str, hub: &TaskHub) {
    if hub.lock().spawned {
        return;
    }
    sink.emit(&AgentEvent::ChildSpawned {
        meta: meta(run_id),
        name: AGENT_NAME.into(),
        agent_card_url: String::new(),
        prompt: goal.to_string(),
    });
    hub.lock().spawned = true;
}

fn emit_exit(sink: &dyn EventSink, run_id: &str, detail: &str, hub: &TaskHub) {
    if !hub.lock().spawned {
        return;
    }
    sink.emit(&AgentEvent::ChildExited {
        meta: meta(run_id),
        name: AGENT_NAME.into(),
        detail: detail.to_string(),
    });
    hub.lock().spawned = false;
}

fn emit_notice(sink: &dyn EventSink, run_id: &str, message: String) {
    sink.emit(&AgentEvent::Notice {
        meta: meta(run_id),
        message,
    });
}

fn emit_message(sink: &dyn EventSink, run_id: &str, text: &str) {
    sink.emit(&AgentEvent::AgentMessage {
        meta: meta(run_id),
        from: AGENT_NAME.into(),
        to: "root".into(),
        text: text.to_string(),
    });
}

fn worker_instruction(goal: &str, checklist: &[CheckItem], next: &str) -> String {
    let list = if checklist.is_empty() {
        "（尚無檢查表 — 朝目標推進並留下可驗證結果）".into()
    } else {
        checklist
            .iter()
            .map(|i| {
                format!(
                    "- {} {}",
                    if i.done { "[x]" } else { "[ ]" },
                    i.text
                )
            })
            .collect::<Vec<_>>()
            .join("\n")
    };
    format!(
        "[task-system]\nThe task supervisor reviewed your work. Do not wait for the user. Continue until every item is done.\n\nGoal:\n{goal}\n\nChecklist:\n{list}\n\nNext:\n{next}\n\nIf an unchangeable constraint makes the goal impossible, call task_report with kind=\"impossible\" and a concrete reason. Hard or unfinished work is not impossible."
    )
}

fn explain_fail_instruction(goal: &str, reason: &str) -> String {
    format!(
        "[task-system]\nThe task supervisor confirmed this goal cannot be completed.\n\nGoal:\n{goal}\n\nUnchangeable constraint:\n{reason}\n\nTell the user, in their language, that the task stopped unfinished and why. Do not keep trying. Do not call more tools except to quote evidence you already have."
    )
}

async fn ask_supervisor<P: Provider>(
    provider: &P,
    model: &str,
    effort: ReasoningEffort,
    send_reasoning: bool,
    run_id: &str,
    prompt: String,
) -> Result<Verdict> {
    let req = CompleteRequest {
        instructions: SUPERVISOR_INSTRUCTIONS.into(),
        input: vec![json!({"role": "user", "content": prompt})],
        client_tools: vec![],
        server_tools: vec![],
        cache_key: format!("grokaagent-task:{run_id}"),
        previous_response_id: None,
        store: false,
        reasoning_effort: effort,
        send_reasoning,
        model: model.to_string(),
        tool_choice: Some("none".into()),
    };
    let r = provider.complete(req).await?;
    if !r.function_calls.is_empty() {
        return Err(Error::Provider("task supervisor called tools".into()));
    }
    parse_verdict_inner(&r.text)
}

fn review_prompt(state: &TaskState, excerpt: &str, last_text: &str, kind: &str) -> String {
    let list = state.checklist_text();
    let claim = state
        .claim
        .as_deref()
        .map(|c| format!("\nWorker impossibility claim:\n{c}\n"))
        .unwrap_or_default();
    format!(
        "Review kind: {kind}\n\nUser goal:\n{}\n\nCurrent checklist:\n{list}\n{claim}\nLast worker reply:\n{}\n\nConversation excerpt (oldest truncated):\n{excerpt}\n",
        state.goal,
        clip(last_text, 2_000),
    )
}

/// First turn after task mode starts: write the checklist, return the worker prompt.
pub async fn prepare_first_turn<P: Provider>(
    hub: &TaskHub,
    provider: &P,
    model: &str,
    effort: ReasoningEffort,
    send_reasoning: bool,
    run_id: &str,
    history: &[Value],
    sink: &dyn EventSink,
) -> Option<String> {
    if hub.snapshot().phase != TaskPhase::NeedPlan {
        return None;
    }
    review_and_steer(
        hub,
        provider,
        model,
        effort,
        send_reasoning,
        run_id,
        history,
        "",
        "plan",
        sink,
    )
    .await
}

/// After a natural model stop. `None` = pause for the user.
pub async fn after_natural_stop<P: Provider>(
    hub: &TaskHub,
    provider: &P,
    model: &str,
    effort: ReasoningEffort,
    send_reasoning: bool,
    run_id: &str,
    history: &[Value],
    last_text: &str,
    sink: &dyn EventSink,
) -> Option<String> {
    let snap = hub.snapshot();
    if snap.skip_steer || !snap.phase.is_live() {
        return None;
    }
    if snap.phase == TaskPhase::ExplainingFail {
        finish_failed(hub, run_id, sink);
        return None;
    }
    review_and_steer(
        hub,
        provider,
        model,
        effort,
        send_reasoning,
        run_id,
        history,
        last_text,
        "check",
        sink,
    )
    .await
}

/// After wait_session returns. Used when the user started task mode while idle.
pub async fn after_wait<P: Provider>(
    hub: &TaskHub,
    provider: &P,
    model: &str,
    effort: ReasoningEffort,
    send_reasoning: bool,
    run_id: &str,
    history: &[Value],
    had_user: bool,
    sink: &dyn EventSink,
) -> Option<String> {
    if had_user {
        hub.set_skip_steer(false);
        return None;
    }
    if hub.snapshot().phase != TaskPhase::NeedPlan {
        return None;
    }
    review_and_steer(
        hub,
        provider,
        model,
        effort,
        send_reasoning,
        run_id,
        history,
        "",
        "plan",
        sink,
    )
    .await
}

async fn review_and_steer<P: Provider>(
    hub: &TaskHub,
    provider: &P,
    model: &str,
    effort: ReasoningEffort,
    send_reasoning: bool,
    run_id: &str,
    history: &[Value],
    last_text: &str,
    kind: &str,
    sink: &dyn EventSink,
) -> Option<String> {
    let snap = hub.snapshot();
    if snap.goal.trim().is_empty() {
        return None;
    }
    emit_spawn(sink, run_id, &snap.goal, hub);
    emit_notice(
        sink,
        run_id,
        if kind == "plan" {
            "任務系統正在根據目前上下文寫檢查表…".into()
        } else {
            "任務系統正在檢查目標是否已完成…".into()
        },
    );
    let excerpt = history_excerpt(history);
    let prompt = review_prompt(&snap, &excerpt, last_text, kind);
    let verdict = match ask_supervisor(
        provider,
        model,
        effort,
        send_reasoning,
        run_id,
        prompt,
    )
    .await
    {
        Ok(v) => v,
        Err(e) => {
            emit_notice(sink, run_id, format!("任務系統檢查失敗：{e}。先停下等你。"));
            return None;
        }
    };
    apply_verdict(hub, run_id, snap, verdict, sink)
}

fn apply_verdict(
    hub: &TaskHub,
    run_id: &str,
    mut snap: TaskState,
    verdict: Verdict,
    sink: &dyn EventSink,
) -> Option<String> {
    emit_spawn(sink, run_id, &snap.goal, hub);
    snap.spawned = true;
    if !verdict.checklist.is_empty() {
        snap.checklist = verdict.checklist;
    } else if snap.checklist.is_empty() {
        snap.checklist = vec![CheckItem {
            text: clip(&snap.goal, 200),
            done: false,
        }];
    }
    snap.claim = None;
    snap.skip_steer = false;
    match verdict.status {
        Status::Complete => {
            for i in &mut snap.checklist {
                i.done = true;
            }
            snap.phase = TaskPhase::Done;
            snap.fail_reason.clear();
            hub.replace(snap.clone());
            let msg = format!(
                "任務完成。\n目標：{}\n{}",
                snap.goal,
                snap.checklist_text()
            );
            emit_message(sink, run_id, &msg);
            emit_notice(sink, run_id, "任務系統確認目標已完成。".into());
            emit_exit(sink, run_id, "完成", hub);
            None
        }
        Status::Impossible => {
            let reason = if verdict.reason.trim().is_empty() {
                "任務系統判定目標因不可改變的限制而無法完成。".into()
            } else {
                verdict.reason
            };
            snap.phase = TaskPhase::ExplainingFail;
            snap.fail_reason = reason.clone();
            hub.replace(snap.clone());
            let msg = format!("無法完成。\n目標：{}\n原因：{reason}", snap.goal);
            emit_message(sink, run_id, &msg);
            emit_notice(
                sink,
                run_id,
                "任務系統確認無法達成，主代理將說明原因後停下。".into(),
            );
            Some(explain_fail_instruction(&snap.goal, &reason))
        }
        Status::Continue => {
            snap.phase = TaskPhase::Active;
            let next = if verdict.next.trim().is_empty() {
                "繼續完成尚未勾選的檢查項，做完再停下。".into()
            } else {
                verdict.next
            };
            hub.replace(snap.clone());
            let msg = format!(
                "尚未完成，繼續。\n目標：{}\n{}\n下一步：{next}",
                snap.goal,
                snap.checklist_text()
            );
            emit_message(sink, run_id, &msg);
            emit_notice(
                sink,
                run_id,
                format!("任務系統：尚未完成。\n{}", snap.checklist_text()),
            );
            Some(worker_instruction(&snap.goal, &snap.checklist, &next))
        }
    }
}

fn finish_failed(hub: &TaskHub, run_id: &str, sink: &dyn EventSink) {
    let mut snap = hub.snapshot();
    let reason = if snap.fail_reason.trim().is_empty() {
        "無法完成此任務。".into()
    } else {
        snap.fail_reason.clone()
    };
    snap.phase = TaskPhase::Failed;
    hub.replace(snap);
    emit_notice(
        sink,
        run_id,
        format!("任務未完成。原因：{reason}"),
    );
    emit_exit(sink, run_id, "豁免", hub);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::CompleteResponse;

    #[test]
    fn kick_marker() {
        assert!(is_kick(KICK));
        assert!(is_kick("  [grokaagent task] \n"));
        assert!(!is_kick("hello"));
    }

    #[test]
    fn old_task_json_with_continues_still_loads() {
        let s: TaskState = serde_json::from_str(
            r#"{"goal":"x","phase":"active","continues":48,"checklist":[]}"#,
        )
        .unwrap();
        assert_eq!(s.goal, "x");
        assert_eq!(s.phase, TaskPhase::Active);
    }

    #[test]
    fn parses_fenced_json_and_marks_complete() {
        let raw = "```json\n{\"status\":\"complete\",\"checklist\":[{\"text\":\"寫 README\",\"done\":true}],\"next\":\"\",\"reason\":\"done\"}\n```";
        let v = parse_verdict_inner(raw).unwrap();
        assert_eq!(v.status, Status::Complete);
        assert_eq!(v.checklist[0].text, "寫 README");
        assert!(v.checklist[0].done);
    }

    #[test]
    fn continue_with_all_done_promotes_to_complete() {
        let raw = r#"{"status":"continue","checklist":[{"text":"a","done":true}],"next":"","reason":""}"#;
        let v = parse_verdict_inner(raw).unwrap();
        assert_eq!(v.status, Status::Complete);
    }

    #[test]
    fn excerpt_skips_image_data_uris() {
        let items = vec![
            json!({"role":"user","content":"goal"}),
            json!({"role":"user","content":[{"type":"input_text","text":"see"},{"type":"input_image","image_url":"data:image/jpeg;base64,AAAA"}]}),
        ];
        let s = history_excerpt(&items);
        assert!(s.contains("goal"), "{s}");
        assert!(s.contains("see"), "{s}");
        assert!(!s.contains("AAAA"), "{s}");
    }

    #[test]
    fn start_goal_kicks_and_end_clears() {
        let hub = TaskHub::new("sid");
        hub.start_goal("  把登入做完  ");
        assert!(hub.take_kick());
        let s = hub.snapshot();
        assert_eq!(s.phase, TaskPhase::NeedPlan);
        assert_eq!(s.goal, "把登入做完");
        hub.end();
        assert_eq!(hub.snapshot().phase, TaskPhase::Inactive);
        assert!(hub.snapshot().goal.is_empty());
    }

    #[test]
    fn task_report_records_only_when_live() {
        let hub = TaskHub::new("");
        let tool = TaskReportTool::new(hub.clone());
        let idle = tool
            .call_sync_for_test(&json!({"kind":"impossible","reason":"no key"}));
        assert!(idle.contains("not active"), "{idle}");
        hub.start_goal("x");
        let ok = tool.call_sync_for_test(&json!({"kind":"impossible","reason":"no API key exists"}));
        assert!(ok.contains("recorded"), "{ok}");
        assert_eq!(
            hub.snapshot().claim.as_deref(),
            Some("no API key exists")
        );
    }

    #[tokio::test]
    async fn skip_steer_does_not_call_the_supervisor() {
        let hub = TaskHub::new("");
        hub.start_goal("x");
        apply_verdict(
            &hub,
            "r",
            hub.snapshot(),
            Verdict {
                status: Status::Continue,
                checklist: vec![CheckItem {
                    text: "a".into(),
                    done: false,
                }],
                next: "go".into(),
                reason: String::new(),
            },
            &Rec(Mutex::new(Vec::new())),
        );
        hub.set_skip_steer(true);
        let rec = Rec(Mutex::new(Vec::new()));
        let out = after_natural_stop(
            &hub,
            &PanicProvider,
            "grok-4.6",
            ReasoningEffort::Low,
            false,
            "r",
            &[],
            "stopped",
            &rec,
        )
        .await;
        assert!(out.is_none());
        assert_eq!(hub.snapshot().phase, TaskPhase::Active);
    }

    struct PanicProvider;
    impl Provider for PanicProvider {
        async fn complete(&self, _req: CompleteRequest) -> Result<CompleteResponse> {
            panic!("supervisor should not run after interrupt");
        }
        async fn compact(
            &self,
            _req: crate::provider::CompactRequest,
        ) -> Result<crate::provider::CompactResponse> {
            Err(Error::Provider("no compact".into()))
        }
    }

    #[test]
    fn apply_verdict_continue_then_impossible_then_complete() {
        let hub = TaskHub::new("");
        hub.start_goal("做完登入");
        let snap = hub.snapshot();
        let rec = Rec(Mutex::new(Vec::new()));
        let inj = apply_verdict(
            &hub,
            "r",
            snap,
            Verdict {
                status: Status::Continue,
                checklist: vec![CheckItem {
                    text: "寫測試".into(),
                    done: false,
                }],
                next: "先寫測試".into(),
                reason: String::new(),
            },
            &rec,
        )
        .unwrap();
        assert!(inj.contains("先寫測試"), "{inj}");
        assert_eq!(hub.snapshot().phase, TaskPhase::Active);

        let inj = apply_verdict(
            &hub,
            "r",
            hub.snapshot(),
            Verdict {
                status: Status::Impossible,
                checklist: hub.snapshot().checklist,
                next: String::new(),
                reason: "沒有帳號系統可接".into(),
            },
            &rec,
        )
        .unwrap();
        assert!(inj.contains("沒有帳號系統可接"), "{inj}");
        assert_eq!(hub.snapshot().phase, TaskPhase::ExplainingFail);

        hub.start_goal("做完登入");
        apply_verdict(
            &hub,
            "r",
            hub.snapshot(),
            Verdict {
                status: Status::Complete,
                checklist: vec![CheckItem {
                    text: "寫測試".into(),
                    done: true,
                }],
                next: String::new(),
                reason: "ok".into(),
            },
            &rec,
        );
        assert_eq!(hub.snapshot().phase, TaskPhase::Done);
        hub.start_goal("長任務");
        apply_verdict(
            &hub,
            "r",
            hub.snapshot(),
            Verdict {
                status: Status::Continue,
                checklist: vec![CheckItem {
                    text: "做".into(),
                    done: false,
                }],
                next: "繼續".into(),
                reason: String::new(),
            },
            &rec,
        );
        for i in 0..50 {
            let inj = apply_verdict(
                &hub,
                "r",
                hub.snapshot(),
                Verdict {
                    status: Status::Continue,
                    checklist: vec![CheckItem {
                        text: "做".into(),
                        done: false,
                    }],
                    next: format!("第{}步", i + 1),
                    reason: String::new(),
                },
                &rec,
            );
            assert!(
                inj.is_some(),
                "continue #{i} must keep steering; there is no round cap"
            );
        }
        assert_eq!(hub.snapshot().phase, TaskPhase::Active);
        let events = rec.0.lock().unwrap().clone();
        assert!(events.iter().any(|e| matches!(e, AgentEvent::ChildSpawned { name, .. } if name == AGENT_NAME)));
        assert!(events.iter().any(|e| matches!(e, AgentEvent::ChildExited { name, .. } if name == AGENT_NAME)));
    }

    struct Rec(Mutex<Vec<AgentEvent>>);
    impl EventSink for Rec {
        fn emit(&self, event: &AgentEvent) {
            self.0.lock().unwrap().push(event.clone());
        }
    }

    impl TaskReportTool {
        fn call_sync_for_test(&self, args: &Value) -> String {
            let kind = args.get("kind").and_then(Value::as_str).unwrap_or("");
            let reason = args.get("reason").and_then(Value::as_str).unwrap_or("");
            if kind != "impossible" {
                return "err".into();
            }
            self.hub.record_claim(reason.into())
        }
    }
}
