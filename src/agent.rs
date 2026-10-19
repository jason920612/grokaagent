use std::collections::HashSet;
use std::future::Future;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{json, Value};
use tokio::sync::{mpsc, Notify};
use uuid::Uuid;

use crate::compact;
use crate::error::{Error, Result};
use crate::events::{AgentEvent, EventMeta, EventSink};
use crate::provider::{prompt_cache_key, CacheUsage, CompleteRequest, Provider, ReasoningEffort};
use crate::tools::ToolRegistry;

/// Live model / effort / server tools for a long-lived TUI session. Read each turn.
#[derive(Clone)]
pub struct SessionKnobs {
    pub model: String,
    pub reasoning_effort: ReasoningEffort,
    pub send_reasoning: bool,
    pub server_tools: Vec<String>,
    /// Dispatcher mode: append instructions telling the root model to plan
    /// and delegate to child agents instead of working directly.
    pub dispatcher: bool,
    /// Default model for spawned child agents. Empty = follow [`Self::model`].
    /// A `model` argument on spawn_agent still wins.
    pub child_model: String,
    /// Model for the task-mode user proxy. Empty = follow [`Self::model`].
    pub task_model: String,
    /// The effort for a set [`Self::task_model`], and whether to send it:
    /// worked out for that model, which may not take the main one's.
    pub task_reasoning_effort: ReasoningEffort,
    pub task_send_reasoning: bool,
    /// The user's search toggle. [`Self::server_tools`] is what the live
    /// model gets; this is what new child agents inherit.
    pub search: bool,
    /// Standing opt-in to workflows for every substantive task.
    pub ultracode: bool,
    /// How big workflows should be (told to the model, not enforced).
    pub workflow_size: crate::workflow::SizeGuide,
}

pub struct RunConfig {
    pub agent_name: String,
    pub instructions: String,
    pub prompt: String,
    pub model: String,
    /// 0 = unlimited (interactive sessions). Headless scripts may still set a cap.
    pub max_turns: u32,
    pub server_tools: Vec<String>,
    /// 0 = look up from the model name (grok-4.6 → 500_000).
    pub context_window: u32,
    /// 0 = [`compact::DEFAULT_KEEP_RECENT`].
    pub compact_keep_recent: usize,
    pub parent_run_id: Option<String>,
    /// If empty, a UUID is generated.
    pub run_id: String,
    pub reasoning_effort: ReasoningEffort,
    pub knobs: Option<Arc<Mutex<SessionKnobs>>>,
    /// Extra user turns. `None` = one-shot (CLI). `Some` = session: after stop, wait for more.
    pub inbox: Option<mpsc::UnboundedReceiver<UserTurn>>,
    /// Background-exit notices. Independent of [`Self::inbox`] so a live hub sender cannot pin the session open.
    pub bg_notify: Option<mpsc::UnboundedReceiver<UserTurn>>,
    pub workspace: PathBuf,
    /// Images attached to the first user message (workspace-relative paths).
    pub images: Vec<PathBuf>,
    /// Notice from the previous run: backgrounds that were killed because this conversation closed.
    pub closed_backgrounds: Option<String>,
    /// API history from the last run of this session (compact brief + recent tail).
    pub prior_history: Vec<Value>,
    /// Persist a resume snapshot (images stripped). Root TUI sessions only.
    pub persist_history: Option<Arc<dyn Fn(&[Value]) + Send + Sync>>,
    /// TUI Esc trips this so an in-flight stream or tool is dropped, then the session pauses.
    pub cancel: Option<CancelFlag>,
    pub skills: Option<Arc<Mutex<crate::skills::SkillStore>>>,
    /// Root TUI task mode. `None` for workers and headless one-shots.
    pub task: Option<Arc<crate::task::TaskHub>>,
    /// A tool whose accepted call ends the turn chain: its `result` becomes
    /// the final answer, as if the model had stopped with that text. A
    /// workflow agent's `submit_result`.
    pub terminal_tool: Option<String>,
    /// Background work a run without an inbox (or whose inbox closed)
    /// waits for before it ends: a headless run's pending workflows.
    pub hold: Option<Arc<dyn Fn() -> bool + Send + Sync>>,
    /// Called with each message the user wrote (the first prompt included):
    /// a `+500k` in it sets the turn's token target for workflows.
    pub on_user_turn: Option<Arc<dyn Fn(&str) + Send + Sync>>,
}

/// Shared flag so the TUI can abort the current model turn without ending the session.
#[derive(Clone)]
pub struct CancelFlag {
    inner: Arc<AtomicBool>,
    notify: Arc<Notify>,
    /// Counts trips so observers (the nursery) see every Esc even after the
    /// loop has already `take()`n the flag.
    trips: Arc<std::sync::atomic::AtomicU64>,
}

impl CancelFlag {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(AtomicBool::new(false)),
            notify: Arc::new(Notify::new()),
            trips: Arc::new(std::sync::atomic::AtomicU64::new(0)),
        }
    }

    pub fn trip(&self) {
        self.inner.store(true, Ordering::SeqCst);
        self.trips.fetch_add(1, Ordering::SeqCst);
        self.notify.notify_waiters();
    }

    pub fn trips(&self) -> u64 {
        self.trips.load(Ordering::SeqCst)
    }

    /// Wait until the flag has been tripped more than `seen` times; returns the new count.
    pub async fn wait_trip(&self, seen: u64) -> u64 {
        loop {
            let notified = self.notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            let now = self.trips();
            if now > seen {
                return now;
            }
            notified.await;
        }
    }

    pub fn take(&self) -> bool {
        self.inner.swap(false, Ordering::SeqCst)
    }

    pub fn is_set(&self) -> bool {
        self.inner.load(Ordering::SeqCst)
    }

    async fn wait(&self) {
        loop {
            // Register before checking so a trip between the check and the
            // await is not lost.
            let notified = self.notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if self.is_set() {
                return;
            }
            notified.await;
        }
    }
}

impl Default for CancelFlag {
    fn default() -> Self {
        Self::new()
    }
}

/// One user message, optionally with workspace images for vision.
#[derive(Clone, Debug, Default)]
pub struct UserTurn {
    pub text: String,
    pub images: Vec<PathBuf>,
}

impl UserTurn {
    pub fn is_empty(&self) -> bool {
        self.text.trim().is_empty() && self.images.is_empty()
    }
}

impl From<&str> for UserTurn {
    fn from(s: &str) -> Self {
        Self {
            text: s.to_string(),
            images: Vec::new(),
        }
    }
}

impl From<String> for UserTurn {
    fn from(text: String) -> Self {
        Self {
            text,
            images: Vec::new(),
        }
    }
}

#[derive(Debug)]
pub struct RunOutcome {
    pub run_id: String,
    pub text: String,
    pub turns: u32,
    pub cache_turns: Vec<CacheUsage>,
    pub compacted: u32,
}

fn snapshot_for_resume(items: &[Value]) -> Vec<Value> {
    let mut out = compact::close_fold_head(items);
    crate::vision::strip_attached_images(&mut out);
    out
}

fn checkpoint(cfg: &RunConfig, history: &[Value]) {
    if let Some(cb) = &cfg.persist_history {
        cb(&snapshot_for_resume(history));
    }
}

fn outcome(
    cfg: &RunConfig,
    history: &[Value],
    run_id: String,
    text: String,
    turns: u32,
    cache_turns: Vec<CacheUsage>,
    compacted: u32,
) -> RunOutcome {
    checkpoint(cfg, history);
    RunOutcome {
        run_id,
        text,
        turns,
        cache_turns,
        compacted,
    }
}

fn meta(agent_name: &str, run_id: &str, parent_run_id: Option<&str>) -> EventMeta {
    EventMeta {
        ts: chrono::Utc::now(),
        agent_name: agent_name.to_string(),
        run_id: run_id.to_string(),
        parent_run_id: parent_run_id.map(str::to_string),
        path: String::new(),
    }
}

fn notice(
    sink: &dyn EventSink,
    agent_name: &str,
    run_id: &str,
    parent_run_id: Option<&str>,
    message: String,
) {
    sink.emit(&AgentEvent::Notice {
        meta: meta(agent_name, run_id, parent_run_id),
        message,
    });
}

fn emit_model_finished(
    sink: &dyn EventSink,
    agent_name: &str,
    run_id: &str,
    parent_run_id: Option<&str>,
    text: String,
    finish: &str,
    usage: &CacheUsage,
) {
    sink.emit(&AgentEvent::ModelFinished {
        meta: meta(agent_name, run_id, parent_run_id),
        text,
        finish: finish.into(),
        input_tokens: usage.input_tokens,
        cached_tokens: usage.cached_tokens,
        output_tokens: usage.output_tokens,
    });
}

fn live_instructions(cfg: &RunConfig) -> String {
    let mut out = cfg.instructions.clone();
    let knobs = cfg.knobs.as_ref().map(|k| {
        let g = k.lock().unwrap_or_else(|e| e.into_inner());
        (g.dispatcher, g.ultracode, g.workflow_size)
    });
    if knobs.is_some_and(|k| k.0) {
        out.push_str(crate::instructions::DISPATCHER_INSTRUCTIONS);
    }
    // Only a TUI root has live knobs, and only the root has workflows.
    if let Some((_, ultracode, size)) = knobs {
        out.push_str(&crate::workflow::instructions_suffix(ultracode, size));
    }
    let extra = cfg.skills.as_ref().map(|s| {
        s.lock()
            .unwrap_or_else(|e| e.into_inner())
            .catalog_suffix(&cfg.workspace)
    }).unwrap_or_default();
    out.push_str(&extra);
    out
}

fn live_knobs(cfg: &RunConfig) -> (String, ReasoningEffort, bool, Vec<String>) {
    if let Some(k) = &cfg.knobs {
        let g = k.lock().unwrap_or_else(|e| e.into_inner());
        (
            g.model.clone(),
            g.reasoning_effort,
            g.send_reasoning,
            g.server_tools.clone(),
        )
    } else {
        (
            cfg.model.clone(),
            cfg.reasoning_effort,
            true,
            cfg.server_tools.clone(),
        )
    }
}

fn append_user(
    history: &mut Vec<Value>,
    pending: &mut Vec<Value>,
    turn: &UserTurn,
    workspace: &PathBuf,
) {
    let item = crate::vision::user_message(&turn.text, &turn.images, workspace);
    history.push(item.clone());
    pending.push(item);
}

fn drain_inbox(inbox: &mut Option<mpsc::UnboundedReceiver<UserTurn>>) -> Vec<UserTurn> {
    let Some(rx) = inbox else {
        return Vec::new();
    };
    let mut out = Vec::new();
    while let Ok(msg) = rx.try_recv() {
        if !msg.is_empty() {
            out.push(msg);
        }
    }
    out
}

fn drain_session(cfg: &mut RunConfig) -> Vec<UserTurn> {
    let mut out = drain_inbox(&mut cfg.inbox);
    out.extend(drain_inbox(&mut cfg.bg_notify));
    out
}

async fn recv_pending(rx: &mut Option<mpsc::UnboundedReceiver<UserTurn>>) -> Option<UserTurn> {
    match rx.as_mut() {
        Some(rx) => rx.recv().await,
        None => std::future::pending().await,
    }
}

async fn wait_if_cancel(cancel: &Option<CancelFlag>) {
    match cancel {
        Some(c) => c.wait().await,
        None => std::future::pending().await,
    }
}

fn take_task_kick(task: &Option<Arc<crate::task::TaskHub>>) -> bool {
    task.as_ref().is_some_and(|h| h.take_kick())
}

async fn wait_task_kick(task: &Option<Arc<crate::task::TaskHub>>) {
    match task {
        Some(h) => h.wait_kick().await,
        None => std::future::pending().await,
    }
}

/// A fold of the history as it stood at some turn, made by a task of its
/// own while the work goes on, and spliced in as soon as it is ready.
struct BgFold {
    task: tokio::task::JoinHandle<Option<compact::Folded>>,
    /// History length it folded.
    seen: usize,
    /// `history[seen - 1]` then: the splice is valid only while the history
    /// before `seen` is untouched.
    boundary: Option<Value>,
}

impl Drop for BgFold {
    fn drop(&mut self) {
        // Dropped unused (a compaction got there first, the run ended): stop
        // its model call.
        self.task.abort();
    }
}

impl BgFold {
    fn valid_for(&self, history: &[Value]) -> bool {
        self.seen <= history.len() && (self.seen == 0 || history.get(self.seen - 1) == self.boundary.as_ref())
    }

    /// Wait for it to finish: `None` if Esc came first (it keeps running),
    /// `Some(None)` if it failed. Not to be called again once it returned
    /// `Some`.
    async fn wait(&mut self, cancel: &Option<CancelFlag>) -> Option<Option<compact::Folded>> {
        race_cancel(cancel, &mut self.task).await.ok().map(|r| r.ok().flatten())
    }
}

/// What became of a background fold.
enum Finished {
    Ready(usize, compact::Folded),
    /// The fold itself failed (or its task panicked).
    Failed,
    /// It finished, but the history before it has changed since.
    Stale,
}

/// The background fold's outcome, if it has finished. A running fold stays
/// in `slot`.
fn take_finished(slot: &mut Option<BgFold>, history: &[Value]) -> Option<Finished> {
    let bg = slot.as_mut()?;
    if !bg.task.is_finished() {
        return None;
    }
    // Finished, so its handle is ready, unless this task's cooperative
    // budget is spent: poll it outside the budget, and leave it be if not.
    let joined = futures::FutureExt::now_or_never(tokio::task::unconstrained(&mut bg.task))?;
    let bg = slot.take()?;
    Some(match joined.ok().flatten() {
        None => Finished::Failed,
        Some(f) if bg.valid_for(history) => Finished::Ready(bg.seen, f),
        Some(_) => Finished::Stale,
    })
}

/// Whether a finished background fold, spliced in (`spliced` real tokens),
/// is worth keeping over a fold of everything now, which would land near
/// `floor` (`None`: nothing to fold).
fn splice_is_good_enough(spliced: u32, floor: Option<u32>, budget: &compact::Budget) -> bool {
    let Some(floor) = floor else {
        // Nothing left to fold: the splice is all there is.
        return true;
    };
    // Landed at most halfway from the target to the start point: a long
    // stretch of work before the next fold. Higher, the next fold (and cache
    // miss) would follow within a few turns.
    spliced <= (budget.after + budget.prefold) / 2
        || (spliced <= floor.saturating_add(budget.window / 20)
            // At or over the trigger the next turn folds again: refuse it
            // when a fold now would get under.
            && (spliced < budget.trigger || floor >= budget.trigger))
}

/// A finished background fold spliced in: its items, then what came after
/// `seen`. `how` tells the user how it got there: `dynamic` (ready while the
/// work went on), `background, awaited` (waited for at the trigger) or
/// `stand-in` (a fresh fold failed).
fn splice_bg(f: compact::Folded, seen: usize, history: &[Value], how: &str) -> compact::Folded {
    let mut items = f.items;
    items.extend(history[seen..].iter().cloned());
    compact::Folded { kept: items.len(), items, dropped: f.dropped, method: format!("{} ({how})", f.method) }
}

/// Real tokens of the next request: the server's count for the request it
/// last saw (`last_input`, 0 when there is none) plus what was added since,
/// or else the whole history (`all_local`, fixed costs included). Local
/// estimates are scaled by `calib`.
fn used_tokens(last_input: u32, added_local: u32, all_local: u32, calib: f32) -> u32 {
    let real = |local: u32| (local as f32 * calib) as u32;
    if last_input > 0 {
        last_input.saturating_add(real(added_local))
    } else {
        real(all_local)
    }
}

async fn race_cancel<T>(
    cancel: &Option<CancelFlag>,
    fut: impl Future<Output = T>,
) -> std::result::Result<T, ()> {
    if cancel.as_ref().is_some_and(CancelFlag::is_set) {
        return Err(());
    }
    tokio::select! {
        biased;
        _ = wait_if_cancel(cancel) => Err(()),
        out = fut => Ok(out),
    }
}

/// Follows [`crate::notice::INTERRUPT`] in the note sent after Esc.
const INTERRUPT_NOTE: &str = "Do not continue that turn. Follow their next message.";

fn cancelled_tool_output() -> String {
    json!({"error": "interrupted", "cancelled": true}).to_string()
}

/// A tool result that asks for its image to be attached (read_image,
/// screenshot). Parsed, not matched as text, so any JSON layout counts.
fn asks_for_image(output: &str) -> bool {
    serde_json::from_str::<Value>(output)
        .ok()
        .and_then(|v| v.get("attach_image").and_then(Value::as_bool))
        == Some(true)
}

/// `output` with `attach_image` off and `attach_error` saying why, so
/// neither the model nor the picture row claims pixels that were not sent.
fn without_image(output: &str, why: &str) -> String {
    match serde_json::from_str::<Value>(output) {
        Ok(Value::Object(mut o)) => {
            o.insert("attach_image".into(), json!(false));
            o.insert("attach_error".into(), json!(why));
            Value::Object(o).to_string()
        }
        _ => output.to_string(),
    }
}

/// Drop every image in `history` for `model`, which cannot view them, and
/// re-report the tool results that had claimed attached pixels, so their
/// picture rows stop saying the model looked. Results in `seen` went to a
/// model that sees images and keep their claim and rows.
fn strip_for_blind_model(
    sink: &dyn EventSink,
    cfg: &RunConfig,
    run_id: &str,
    history: &mut [Value],
    model: &str,
    seen: &HashSet<String>,
) {
    for call_id in crate::vision::strip_images_for_blind_model(history, model, seen) {
        let output = history
            .iter()
            .find(|i| i["type"] == "function_call_output" && i["call_id"] == call_id.as_str())
            .and_then(|i| i["output"].as_str())
            .unwrap_or("")
            .to_string();
        let name = history
            .iter()
            .find(|i| i["type"] == "function_call" && i["call_id"] == call_id.as_str())
            .and_then(|i| i["name"].as_str())
            .unwrap_or("")
            .to_string();
        sink.emit(&AgentEvent::ToolFinished {
            meta: meta(&cfg.agent_name, run_id, cfg.parent_run_id.as_deref()),
            call_id,
            name,
            output,
        });
    }
}

/// Tell the user, once per model, that the model cannot see the images
/// it is sent.
fn tell_blind(sink: &dyn EventSink, cfg: &RunConfig, run_id: &str, model: &str, told: &mut Option<String>) {
    if told.as_deref() == Some(model) {
        return;
    }
    *told = Some(model.to_string());
    notice(
        sink,
        &cfg.agent_name,
        run_id,
        cfg.parent_run_id.as_deref(),
        format!("模型 {model} 無法看圖：圖片改以文字說明送出，模型看不到內容"),
    );
}

/// How often a held run without an inbox re-checks its hold.
const HOLD_TICK: Duration = Duration::from_millis(250);

/// Whether background work keeps this run alive past its inbox.
fn held(cfg: &RunConfig) -> bool {
    cfg.hold.as_ref().is_some_and(|h| h())
}

/// The result of an accepted terminal-tool call: `{"accepted": true,
/// "result": …}` → the result as JSON text.
fn accepted_result(output: &str) -> Option<String> {
    let v: Value = serde_json::from_str(output).ok()?;
    if v.get("accepted").and_then(Value::as_bool) != Some(true) {
        return None;
    }
    Some(v.get("result")?.to_string())
}

/// Wait for the next user turn or background-exit notice. Closing the user
/// inbox ends the session, unless held background work is still running.
/// Esc while paused is ignored so it cannot cancel the next turn.
async fn wait_session(cfg: &mut RunConfig) -> Option<Vec<UserTurn>> {
    loop {
        // Without an inbox only held background work can still speak. A
        // notice it queued just before letting go is still news.
        if cfg.inbox.is_none() && !(held(cfg) && cfg.bg_notify.is_some()) {
            let late = drain_session(cfg);
            return (!late.is_empty()).then_some(late);
        }
        if let Some(c) = &cfg.cancel {
            c.take();
        }
        let mut msgs = drain_session(cfg);
        let kicked = take_task_kick(&cfg.task);
        if !msgs.is_empty() {
            return Some(msgs);
        }
        if kicked {
            return Some(Vec::new());
        }
        tokio::select! {
            user = recv_pending(&mut cfg.inbox) => {
                let Some(msg) = user else {
                    cfg.inbox = None;
                    continue;
                };
                if !msg.is_empty() {
                    msgs.push(msg);
                }
                msgs.extend(drain_session(cfg));
                return Some(msgs);
            }
            bg = recv_pending(&mut cfg.bg_notify) => {
                let Some(msg) = bg else {
                    cfg.bg_notify = None;
                    continue;
                };
                if !msg.is_empty() {
                    msgs.push(msg);
                }
                msgs.extend(drain_session(cfg));
                return Some(msgs);
            }
            _ = wait_task_kick(&cfg.task) => {
                msgs.extend(drain_session(cfg));
                return Some(msgs);
            }
            // A hold can end without a message (a workflow stopped on
            // request sends none): look again now and then.
            _ = tokio::time::sleep(HOLD_TICK), if cfg.inbox.is_none() && cfg.hold.is_some() => {}
            _ = wait_if_cancel(&cfg.cancel) => {
                if let Some(c) = &cfg.cancel {
                    c.take();
                }
            }
        }
    }
}

/// Pause after Esc. `false` = the session ended (no inbox).
fn after_interrupt(
    cfg: &mut RunConfig,
    sink: &dyn EventSink,
    run_id: &str,
    last_text: &str,
) -> bool {
    if let Some(c) = &cfg.cancel {
        c.take();
    }
    if let Some(t) = &cfg.task {
        t.set_skip_steer(true);
    }
    notice(
        sink,
        &cfg.agent_name,
        run_id,
        cfg.parent_run_id.as_deref(),
        "使用者中斷（Esc）".into(),
    );
    if cfg.inbox.is_none() {
        sink.emit(&AgentEvent::RunFinished {
            meta: meta(&cfg.agent_name, run_id, cfg.parent_run_id.as_deref()),
            reason: "interrupt".into(),
            text: last_text.to_string(),
        });
        return false;
    }
    sink.emit(&AgentEvent::AwaitingInput {
        meta: meta(&cfg.agent_name, run_id, cfg.parent_run_id.as_deref()),
    });
    true
}

/// `true` = keep the session. `false` = run ended.
async fn take_interrupt<P: Provider>(
    cfg: &mut RunConfig,
    provider: &P,
    sink: &dyn EventSink,
    run_id: &str,
    last_text: &str,
    history: &mut Vec<Value>,
    pending: &mut Vec<Value>,
) -> bool {
    checkpoint(cfg, history);
    if !after_interrupt(cfg, sink, run_id, last_text) {
        return false;
    }
    wait_for_work(cfg, provider, sink, run_id, history, pending, last_text, true).await
}

/// Whether a queued turn is the user's own words, not the kick marker or a
/// harness notice (a background exit, a child's reply).
fn is_user_turn(m: &UserTurn) -> bool {
    !m.is_empty() && !crate::task::is_kick(&m.text) && !crate::notice::is_harness(&m.text)
}

fn user_texts(msgs: &[UserTurn]) -> Vec<String> {
    msgs.iter()
        .filter(|m| is_user_turn(m))
        .map(|m| if m.text.trim().is_empty() { "[image]".to_string() } else { m.text.clone() })
        .collect()
}

/// Real user messages that reach a working run go into the user proxy's
/// memory (task mode); the worker handles them first. One also lifts an
/// Esc stop kept from an earlier run.
fn note_user_turns(cfg: &RunConfig, msgs: &[UserTurn]) {
    if let Some(hook) = &cfg.on_user_turn {
        for t in user_texts(msgs) {
            hook(&t);
        }
    }
    if let Some(hub) = &cfg.task {
        let texts = user_texts(msgs);
        if !texts.is_empty() {
            hub.set_skip_steer(false);
        }
        for t in texts {
            hub.note_user(&t);
        }
    }
}

/// Idle until there is work: a message (user, background, child), or the
/// user proxy speaking up (a new goal, a review request, a resume).
/// `false` = the session ended. After Esc (`interrupted`) the messages
/// follow the interrupt note; a proxy message comes on its own.
#[allow(clippy::too_many_arguments)]
async fn wait_for_work<P: Provider>(
    cfg: &mut RunConfig,
    provider: &P,
    sink: &dyn EventSink,
    run_id: &str,
    history: &mut Vec<Value>,
    pending: &mut Vec<Value>,
    last_text: &str,
    interrupted: bool,
) -> bool {
    wait_for_input(cfg, provider, sink, run_id, history, pending, last_text, interrupted).await.is_some()
}

/// [`wait_for_work`], saying what woke it: `Some(true)` = a message (the
/// user's, a background exit, a child's), `Some(false)` = the user proxy
/// alone, `None` = the session ended.
#[allow(clippy::too_many_arguments)]
async fn wait_for_input<P: Provider>(
    cfg: &mut RunConfig,
    provider: &P,
    sink: &dyn EventSink,
    run_id: &str,
    history: &mut Vec<Value>,
    pending: &mut Vec<Value>,
    last_text: &str,
    interrupted: bool,
) -> Option<bool> {
    loop {
        let Some(msgs) = wait_session(cfg).await else {
            return None;
        };
        let msgs: Vec<UserTurn> = msgs
            .into_iter()
            .filter(|m| !m.is_empty() && !crate::task::is_kick(&m.text))
            .collect();
        let woke = !msgs.is_empty();
        if interrupted && woke {
            append_user(
                history,
                pending,
                &UserTurn::from(format!("{} {INTERRUPT_NOTE}", crate::notice::INTERRUPT)),
                &cfg.workspace,
            );
        }
        for msg in &msgs {
            append_user(history, pending, msg, &cfg.workspace);
        }
        let users = user_texts(&msgs);
        if let Some(hook) = &cfg.on_user_turn {
            for t in &users {
                hook(t);
            }
        }
        if let Some(instr) = task_idle(cfg, provider, sink, run_id, history, last_text, &users).await {
            append_user(history, pending, &UserTurn::from(instr), &cfg.workspace);
            return Some(woke);
        }
        if woke || cfg.cancel.as_ref().is_some_and(CancelFlag::is_set) {
            return Some(woke);
        }
        // Only a kick, and the proxy had nothing to say (it failed and
        // paused, or the task is over): still idle. Say so again, so
        // whoever drives the session (the TUI, a headless driver) sees it.
        sink.emit(&AgentEvent::AwaitingInput {
            meta: meta(&cfg.agent_name, run_id, cfg.parent_run_id.as_deref()),
        });
    }
}

fn emit_cancelled_tool(
    sink: &dyn EventSink,
    cfg: &RunConfig,
    run_id: &str,
    call_id: &str,
    name: &str,
    started: bool,
    args: &Value,
    history: &mut Vec<Value>,
    pending: &mut Vec<Value>,
) {
    if !started {
        sink.emit(&AgentEvent::ToolStarted {
            meta: meta(&cfg.agent_name, run_id, cfg.parent_run_id.as_deref()),
            call_id: call_id.to_string(),
            name: name.to_string(),
            args: args.clone(),
            kind: "client".into(),
        });
    }
    let output = cancelled_tool_output();
    sink.emit(&AgentEvent::ToolFinished {
        meta: meta(&cfg.agent_name, run_id, cfg.parent_run_id.as_deref()),
        call_id: call_id.to_string(),
        name: name.to_string(),
        output: output.clone(),
    });
    let item = crate::vision::function_call_output(call_id, &output, None);
    history.push(item.clone());
    pending.push(item);
}

/// Resends of one request after a transient provider error (about two
/// minutes of backoff in total) before it counts as a real failure.
const MAX_TRANSIENT_RETRIES: u32 = 6;

fn transient_backoff(attempt: u32) -> Duration {
    if cfg!(test) {
        return Duration::from_millis(5);
    }
    // Up to a quarter more, at random: agents that hit a rate limit together
    // (a workflow's parallel agents) should not all retry in the same second.
    let base = Duration::from_secs((2u64 << attempt.min(5)).min(60));
    let jitter = (Uuid::new_v4().as_u128() % 1000) as u32;
    base + base * jitter / 4000
}

/// Stop retrying the same failing request so we do not hammer the API.
const MAX_CONSECUTIVE_PROVIDER_ERRORS: u32 = 3;
/// Turns to wait after a fold failed below the hard limit, so a persistent
/// failure does not cost fold requests on every turn.
const COMPACT_BACKOFF_TURNS: u32 = 3;

/// What a provider-error recovery did with the attached images.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ImagesAfterError {
    /// Left in place: the error is not about them.
    Kept,
    /// Removed in case they caused the error.
    Removed,
    /// Removed because the model refused image input.
    Refused,
}

/// The request a provider error answered.
struct Failed<'a> {
    /// The model it went to; the live knob may have moved since.
    model: &'a str,
    /// Tool results whose pixels a model that sees images was sent before.
    seen: &'a HashSet<String>,
}

/// Tell the model why the request failed (and whether its images were
/// removed) and keep the loop going. Do not pause for the user.
#[allow(clippy::too_many_arguments)]
async fn recover_from_provider_error<P: Provider>(
    cfg: &mut RunConfig,
    provider: &P,
    sink: &dyn EventSink,
    run_id: &str,
    history: &mut Vec<Value>,
    pending: &mut Vec<Value>,
    consecutive: &mut u32,
    last_text: &str,
    failed: Failed<'_>,
    err: Error,
) -> Result<ImagesAfterError> {
    *consecutive += 1;
    sink.emit(&AgentEvent::Error {
        meta: meta(&cfg.agent_name, run_id, cfg.parent_run_id.as_deref()),
        message: err.to_string(),
    });
    // Pixels go when they may be the cause, or when a resend already failed
    // too; never for a repetition loop, which they have nothing to do with.
    let had_images = crate::vision::input_has_image(history) || crate::vision::input_has_image(pending);
    let model = failed.model;
    let images = if !had_images || crate::repeat::is_loop(&err) {
        ImagesAfterError::Kept
    } else if image_error(&err, model) && crate::openai::refuses_images(&err.to_string(), model) {
        // Unverified here (no resend without the images), so the error
        // must name images, not only refuse content arrays.
        ImagesAfterError::Refused
    } else if image_error(&err, model) || *consecutive > 1 {
        ImagesAfterError::Removed
    } else {
        ImagesAfterError::Kept
    };
    if images == ImagesAfterError::Refused {
        strip_for_blind_model(sink, cfg, run_id, history, model, failed.seen);
        crate::vision::strip_images_for_blind_model(pending, model, failed.seen);
    } else if images != ImagesAfterError::Kept {
        crate::vision::strip_attached_images(history);
        crate::vision::strip_attached_images(pending);
    }
    let note = if images == ImagesAfterError::Refused {
        // Inviting read_image here would only repeat the refusal.
        format!(
            "{} ({err}). This model cannot view images, so the attached images were removed and no more will be attached. Do not call read_image or screenshot to look at images; work from file paths and text. Continue the task yourself. Do not wait for the user.",
            crate::notice::REQUEST_FAILED
        )
    } else if images == ImagesAfterError::Removed {
        format!(
            "{} ({err}). The attached images were removed from the conversation in case they caused it; read_image their paths if you still need to see them. Continue the task yourself. Do not wait for the user.",
            crate::notice::REQUEST_FAILED
        )
    } else {
        format!(
            "{} ({err}). Continue the task yourself. Do not wait for the user.",
            crate::notice::REQUEST_FAILED
        )
    };
    append_user(history, pending, &UserTurn::from(note), &cfg.workspace);
    *pending = history.clone();

    if *consecutive < MAX_CONSECUTIVE_PROVIDER_ERRORS {
        return Ok(images);
    }
    if cfg.inbox.is_none() {
        sink.emit(&AgentEvent::RunFinished {
            meta: meta(&cfg.agent_name, run_id, cfg.parent_run_id.as_deref()),
            reason: "error".into(),
            text: last_text.to_string(),
        });
        return Err(err);
    }
    *consecutive = 0;
    checkpoint(cfg, history);
    sink.emit(&AgentEvent::AwaitingInput {
        meta: meta(&cfg.agent_name, run_id, cfg.parent_run_id.as_deref()),
    });
    // A task kick (review now, resume, a new goal) counts here as well.
    if wait_for_work(cfg, provider, sink, run_id, history, pending, last_text, false).await {
        return Ok(images);
    }
    sink.emit(&AgentEvent::RunFinished {
        meta: meta(&cfg.agent_name, run_id, cfg.parent_run_id.as_deref()),
        reason: "error".into(),
        text: last_text.to_string(),
    });
    Err(err)
}

/// A provider error that may be about the attached images themselves, or
/// about the size they add to the request. Only the error's message counts,
/// without `model`'s name ("llama3.2-vision does not support tools").
fn image_error(err: &Error, model: &str) -> bool {
    let text = err.to_string();
    let words = crate::openai::error_words(&text, model);
    ["image", "vision", "multimodal", "mmproj"].iter().any(|k| words.contains(k))
        || text.to_ascii_lowercase().contains("http 413")
        || compact::context_overflow(err)
}

/// A response was cut off for degenerate repetition: drop it and retry the
/// turn with a note. Repeated trips fall back to the provider-error path,
/// which tells the model and eventually stops.
#[allow(clippy::too_many_arguments)]
async fn handle_loop<P: Provider>(
    cfg: &mut RunConfig,
    provider: &P,
    sink: &dyn EventSink,
    run_id: &str,
    history: &mut Vec<Value>,
    pending: &mut Vec<Value>,
    loop_trips: &mut u32,
    consecutive: &mut u32,
    last_text: &str,
    trip: crate::repeat::Trip,
) -> Result<()> {
    *loop_trips += 1;
    if *loop_trips > MAX_LOOP_RETRIES {
        *loop_trips = 0;
        // A repetition loop never touches the images.
        let model = live_knobs(cfg).0;
        let failed = Failed { model: &model, seen: &HashSet::new() };
        return recover_from_provider_error(cfg, provider, sink, run_id, history, pending, consecutive, last_text, failed, trip.to_error())
            .await
            .map(|_| ());
    }
    notice(
        sink,
        &cfg.agent_name,
        run_id,
        cfg.parent_run_id.as_deref(),
        format!(
            "模型輸出陷入重複（{}：「{}」×{}），已中止這次回應並重試（{}/{MAX_LOOP_RETRIES}）",
            trip.source,
            trip.preview(),
            trip.repeats,
            *loop_trips
        ),
    );
    append_user(history, pending, &UserTurn::from(crate::repeat::retry_note(&trip)), &cfg.workspace);
    Ok(())
}

/// Retries of a turn whose response kept falling into a repetition loop.
const MAX_LOOP_RETRIES: u32 = 2;

/// Returns how many file changes it reported.
fn emit_file_changes(
    sink: &dyn EventSink,
    agent_name: &str,
    run_id: &str,
    parent_run_id: Option<&str>,
    output: &str,
) -> usize {
    let Ok(v) = serde_json::from_str::<Value>(output) else {
        return 0;
    };
    let mut reported = 0;
    let mut items: Vec<Value> = Vec::new();
    if v.get("path").and_then(Value::as_str).is_some() && v.get("diff").is_some() {
        items.push(v);
    } else if let Some(diffs) = v.get("_diffs").and_then(Value::as_array) {
        items.extend(diffs.iter().cloned());
    } else if let Some(files) = v.get("files").and_then(Value::as_array) {
        items.extend(files.iter().filter(|f| f.get("diff").is_some()).cloned());
    }
    for item in items {
        let path = item
            .get("path")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        if path.is_empty() {
            continue;
        }
        let kind = item
            .get("kind")
            .and_then(Value::as_str)
            .unwrap_or("modify")
            .to_string();
        let diff = item
            .get("diff")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        sink.emit(&AgentEvent::FileChanged {
            meta: meta(agent_name, run_id, parent_run_id),
            path,
            kind,
            diff,
        });
        reported += 1;
    }
    reported
}

/// Replay every `function_call` into history when `output_items` omitted it.
/// Full-history turns need the call sitting in front of `function_call_output`.
fn append_missing_function_calls(history: &mut Vec<Value>, calls: &[crate::provider::FunctionCall]) {
    for call in calls {
        let already = history.iter().any(|item| {
            item.get("type").and_then(Value::as_str) == Some("function_call")
                && item.get("call_id").and_then(Value::as_str) == Some(call.call_id.as_str())
        });
        if already {
            continue;
        }
        history.push(json!({
            "type": "function_call",
            "call_id": call.call_id,
            "name": call.name,
            "arguments": call.arguments,
        }));
    }
}

pub async fn run<P: Provider + Clone + 'static>(
    provider: &P,
    tools: &ToolRegistry,
    sink: &dyn EventSink,
    mut cfg: RunConfig,
) -> Result<RunOutcome> {
    let run_id = if cfg.run_id.is_empty() {
        Uuid::new_v4().to_string()
    } else {
        cfg.run_id.clone()
    };
    let (start_model, _, _, start_tools) = live_knobs(&cfg);
    sink.emit(&AgentEvent::RunStarted {
        meta: meta(&cfg.agent_name, &run_id, cfg.parent_run_id.as_deref()),
        model: start_model.clone(),
    });

    let window = if cfg.context_window == 0 {
        compact::context_window(&start_model)
    } else {
        cfg.context_window
    };
    let budget = compact::Budget::new(window);
    // 0 = the tail is sized by tokens only.
    let max_tail_items = (cfg.compact_keep_recent > 0).then_some(cfg.compact_keep_recent);

    let mut history = snapshot_for_resume(&cfg.prior_history);
    if !history.is_empty() {
        notice(
            sink,
            &cfg.agent_name,
            &run_id,
            cfg.parent_run_id.as_deref(),
            format!("已載入上次上下文（{} 項，含長期記憶）", history.len()),
        );
    }
    if let Some(closed) = cfg.closed_backgrounds.take() {
        if !closed.trim().is_empty() {
            notice(
                sink,
                &cfg.agent_name,
                &run_id,
                cfg.parent_run_id.as_deref(),
                closed.clone(),
            );
            history.push(crate::vision::user_message(
                &closed,
                &[],
                &cfg.workspace,
            ));
        }
    }
    let skip_prompt = crate::task::is_kick(&cfg.prompt);
    if !skip_prompt {
        history.push(crate::vision::user_message(
            &cfg.prompt,
            &cfg.images,
            &cfg.workspace,
        ));
        note_user_turns(&cfg, &[UserTurn { text: cfg.prompt.clone(), images: cfg.images.clone() }]);
    }
    // A kick-started task run with nothing to start on (the proxy failed
    // and paused the task, or it is not live) waits for the user instead.
    let mut idle_start = false;
    if let Some(hub) = cfg.task.clone() {
        if let Some(instr) = task_first_turn(&cfg, provider, sink, &run_id, &history).await {
            history.push(crate::vision::user_message(
                &instr,
                &[],
                &cfg.workspace,
            ));
        } else if skip_prompt {
            let s = hub.snapshot();
            if s.phase.is_live() && !s.paused && !s.goal.is_empty() {
                history.push(crate::vision::user_message(
                    &format!("{}\n{}\nWork toward it.", crate::notice::TASK_MODE, s.goal),
                    &[],
                    &cfg.workspace,
                ));
            } else {
                idle_start = true;
            }
        }
    } else if skip_prompt {
        history.push(crate::vision::user_message(
            &cfg.prompt,
            &cfg.images,
            &cfg.workspace,
        ));
    }
    let mut pending: Vec<Value> = history.clone();
    let mut last_text = String::new();
    let mut last_usage = CacheUsage::default();
    // History length when the request behind `last_usage` was sent: usage
    // is that reported count plus an estimate of what was added since.
    let mut usage_base_len = 0usize;
    // Real tokens per locally estimated token, from the last counted request.
    let mut calib = 1.0f32;
    // At most one fold running in the background (see [`BgFold`]).
    let mut prefold: Option<BgFold> = None;
    // A background fold failed: no other until the next compaction (the
    // trigger then folds in the open).
    let mut bg_failed = false;
    let mut cache_turns = Vec::new();
    let mut compacted = 0u32;
    let max_turns = cfg.max_turns;
    let mut cache_key = prompt_cache_key(&start_model, &tools.specs_for(&start_tools), &start_tools, &run_id);
    let mut last_model = start_model;
    let mut last_server_tools = start_tools;
    let mut turn: u32 = 0;
    let mut total_turns: u32 = 0;
    let mut seen_model = false;
    let mut consecutive_provider_errors: u32 = 0;
    // Responses cut off in a row for degenerate repetition.
    let mut loop_trips: u32 = 0;
    let mut image_turns: u32 = 0;
    let mut compact_backoff: u32 = 0;
    let mut images_known: usize = 0;
    // History items before this went out in a request the model answered;
    // image caps never strip what lies past it.
    let mut seen_upto: usize = 0;
    // The model the user was last told cannot view images.
    let mut blind_noticed: Option<String> = None;
    // Models that refused image input this run, on routes whose provider
    // does not remember it (`Provider::sees_images`).
    let mut blind: HashSet<String> = HashSet::new();
    // Tool results whose pixels went out in an answered request to a model
    // that sees images: a later blind model does not take them back.
    let mut seen_by_vision: HashSet<String> = HashSet::new();

    // Signs the worker is stuck mid-run, for the user proxy (task mode).
    let mut watch = crate::task::Watch::default();

    if idle_start {
        sink.emit(&AgentEvent::AwaitingInput {
            meta: meta(&cfg.agent_name, &run_id, cfg.parent_run_id.as_deref()),
        });
        if !wait_for_work(&mut cfg, provider, sink, &run_id, &mut history, &mut pending, &last_text, false).await {
            sink.emit(&AgentEvent::RunFinished {
                meta: meta(&cfg.agent_name, &run_id, cfg.parent_run_id.as_deref()),
                reason: "stop".into(),
                text: last_text.clone(),
            });
            return Ok(outcome(&cfg, &history, run_id, last_text, total_turns, cache_turns, compacted));
        }
    }

    'run: loop {
        if seen_model {
            let msgs = drain_session(&mut cfg);
            note_user_turns(&cfg, &msgs);
            for msg in msgs {
                if crate::task::is_kick(&msg.text) {
                    continue;
                }
                append_user(&mut history, &mut pending, &msg, &cfg.workspace);
                turn = 0;
            }
        }
        let n_images = crate::vision::image_item_count(&history);
        if n_images > images_known {
            image_turns = 0;
            crate::vision::retain_seen_images(&mut history, crate::vision::KEEP_RECENT_IMAGES, seen_upto);
            images_known = crate::vision::image_item_count(&history);
        }

        if cfg.cancel.as_ref().is_some_and(CancelFlag::is_set) {
            if !take_interrupt(
                &mut cfg,
                provider,
                sink,
                &run_id,
                &last_text,
                &mut history,
                &mut pending,
            )
            .await
            {
                return Ok(outcome(
                    &cfg,
                    &history,
                    run_id,
                    last_text,
                    total_turns,
                    cache_turns,
                    compacted,
                ));
            }
            turn = 0;
            continue;
        }

        turn += 1;
        total_turns += 1;
        if max_turns > 0 && turn > max_turns {
            let err = Error::MaxTurns(max_turns);
            sink.emit(&AgentEvent::Error {
                meta: meta(&cfg.agent_name, &run_id, cfg.parent_run_id.as_deref()),
                message: err.to_string(),
            });
            if cfg.inbox.is_some() {
                turn = 0;
                checkpoint(&cfg, &history);
                sink.emit(&AgentEvent::AwaitingInput {
                    meta: meta(&cfg.agent_name, &run_id, cfg.parent_run_id.as_deref()),
                });
                // A task kick (review now, resume, a new goal) counts here as well.
                if wait_for_work(&mut cfg, provider, sink, &run_id, &mut history, &mut pending, &last_text, false).await {
                    continue;
                }
                sink.emit(&AgentEvent::RunFinished {
                    meta: meta(&cfg.agent_name, &run_id, cfg.parent_run_id.as_deref()),
                    reason: "max_turns".into(),
                    text: last_text,
                });
                return Err(err);
            }
            sink.emit(&AgentEvent::RunFinished {
                meta: meta(&cfg.agent_name, &run_id, cfg.parent_run_id.as_deref()),
                reason: "max_turns".into(),
                text: last_text,
            });
            return Err(err);
        }

        let (model, effort, send_reasoning, server_tools) = live_knobs(&cfg);
        if model != last_model || server_tools != last_server_tools {
            cache_key = prompt_cache_key(&model, &tools.specs_for(&server_tools), &server_tools, &run_id);
            pending = history.clone();
            last_model = model.clone();
            last_server_tools = server_tools.clone();
        }
        // A model that refused image input gets none again: images pasted
        // since give way to their paths before the request goes out.
        if blind.contains(&model) && crate::vision::input_has_image(&history) {
            crate::vision::strip_images_for_blind_model(&mut history, &model, &seen_by_vision);
            pending = history.clone();
            images_known = 0;
            image_turns = 0;
        }

        sink.emit(&AgentEvent::TurnStarted {
            meta: meta(&cfg.agent_name, &run_id, cfg.parent_run_id.as_deref()),
            turn,
        });

        // The server's count for what it last saw is exact; only what was
        // added since is estimated.
        let fixed = compact::fixed_tokens(&live_instructions(&cfg), &tools.specs_for(&live_knobs(&cfg).3));
        let real = |local: u32| (local as f32 * calib) as u32;
        let mut used = if last_usage.input_tokens > 0 && usage_base_len <= history.len() {
            used_tokens(last_usage.input_tokens, compact::estimate_tokens("", &history[usage_base_len..]), 0, calib)
        } else {
            used_tokens(0, 0, compact::estimate_tokens("", &history).saturating_add(fixed), calib)
        };
        let fold_budget = budget.in_local_units(calib);
        let local = |r: u32| (r as f32 / calib) as u32;
        // Each compaction costs the prompt cache one miss, so it should buy
        // a long stretch of work. In order of preference:
        // 1. a background fold that has finished, if it splices in well;
        // 2. past the start point, slimming old tool outputs, if that alone
        //    gets halfway from the start point to the fold target: no model
        //    call, every turn kept word for word;
        // 3. at the trigger: the background fold still running (waited for),
        //    then slimming below the start point, then a fold of everything
        //    now, falling back on a refused background fold if that fails.
        let mut chosen: Option<compact::Folded> = None;
        let mut fallback: Option<(usize, compact::Folded)> = None;
        let mut fold_failed = false;
        // Spliced in, a fold lands near its target plus what arrived while it
        // ran. At or above the start point the next fold would start right
        // away: a fold of everything now does better, unless it could not get
        // lower anyway (small windows, where fixed costs dominate).
        let good = |(seen, f): &(usize, compact::Folded)| {
            let mut spliced: Vec<Value> = f.items.clone();
            spliced.extend(history[*seen..].iter().cloned());
            let spliced = real(compact::estimate_tokens("", &spliced).saturating_add(fixed));
            let memory = compact::memory_item_tokens(&f.items).unwrap_or(fold_budget.memory);
            let floor = compact::fold_floor(&history, fixed, memory, &fold_budget, max_tail_items).map(real);
            splice_is_good_enough(spliced, floor, &budget)
        };
        match take_finished(&mut prefold, &history) {
            Some(Finished::Ready(seen, f)) => {
                let t = (seen, f);
                if good(&t) {
                    chosen = Some(splice_bg(t.1, t.0, &history, "dynamic"));
                } else {
                    fallback = Some(t);
                }
            }
            Some(Finished::Failed) => {
                bg_failed = true;
                notice(sink, &cfg.agent_name, &run_id, cfg.parent_run_id.as_deref(), "背景整理記憶失敗；到上限時再整理".into());
            }
            Some(Finished::Stale) | None => {}
        }
        if chosen.is_none() && used >= budget.prefold {
            chosen = compact::trim_fold(&history, fixed, local((budget.after + budget.prefold) / 2));
        }
        if chosen.is_none() && used >= budget.trigger {
            if let Some(mut bg) = prefold.take() {
                // It has a head start: finish it rather than start over.
                let (seen, valid) = (bg.seen, bg.valid_for(&history));
                match bg.wait(&cfg.cancel).await {
                    // Esc: it keeps running (the fold step below sees the
                    // cancel and stops for the user); the next turn can still
                    // splice it.
                    None => prefold = Some(bg),
                    Some(None) => bg_failed = true,
                    Some(Some(f)) if valid => {
                        let t = (seen, f);
                        if good(&t) {
                            chosen = Some(splice_bg(t.1, t.0, &history, "background, awaited"));
                        } else {
                            fallback = Some(t);
                        }
                    }
                    Some(Some(_)) => {}
                }
            }
        }
        if chosen.is_none() && used >= budget.trigger {
            chosen = compact::trim_fold(&history, fixed, local(budget.prefold).saturating_sub(1));
        }
        if chosen.is_none() && used >= budget.trigger && compact_backoff > 0 && used < budget.hard {
            compact_backoff -= 1;
        } else if chosen.is_none() && used >= budget.trigger {
            let instructions = live_instructions(&cfg);
            let agent_name = cfg.agent_name.clone();
            let parent = cfg.parent_run_id.clone();
            let say = |m: String| notice(sink, &agent_name, &run_id, parent.as_deref(), m);
            let goal = fold_goal(&cfg);
            let ctx = compact::FoldCtx {
                provider,
                instructions: &instructions,
                goal: &goal,
                model: &model,
                effort,
                send_reasoning,
                cache_key: &cache_key,
                client_tools: tools.specs_for(&server_tools),
                server_tools: server_tools.clone(),
                budget: fold_budget,
                max_tail_items,
                workspace: Some(&cfg.workspace),
                prefold: false,
                notice: &say,
            };
            let result = race_cancel(&cfg.cancel, compact::fold(&ctx, &history)).await;
            chosen = match result {
                Err(()) => {
                    if !take_interrupt(&mut cfg, provider, sink, &run_id, &last_text, &mut history, &mut pending).await {
                        return Ok(outcome(&cfg, &history, run_id, last_text, total_turns, cache_turns, compacted));
                    }
                    turn = 0;
                    continue 'run;
                }
                Ok(Ok(f)) => Some(f),
                Ok(Err(compact::FoldError::Nothing)) => {
                    // The verbatim tail alone is this big: slim huge outputs.
                    if used >= budget.hard {
                        compact::trim_history(&history)
                    } else {
                        None
                    }
                }
                Ok(Err(compact::FoldError::Failed(e))) => {
                    fold_failed = true;
                    // The refused background fold stands in, unless it would
                    // leave the context about full anyway.
                    let stand_in = fallback
                        .take()
                        .map(|(seen, f)| splice_bg(f, seen, &history, "stand-in"))
                        .filter(|c| real(compact::estimate_tokens("", &c.items).saturating_add(fixed)) < budget.hard);
                    if let Some(c) = stand_in {
                        say(format!("壓縮失敗（{e}），改用背景先整理好的記憶"));
                        Some(c)
                    } else if used >= budget.hard {
                        say(format!("壓縮失敗（{e}），上下文將滿：改用不經模型的緊急壓縮"));
                        Some(compact::emergency_fold(&goal, &history, &fold_budget, max_tail_items))
                    } else {
                        say(format!(
                            "壓縮失敗（{e}），先不壓縮送出；{COMPACT_BACKOFF_TURNS} 輪後再試"
                        ));
                        None
                    }
                }
            };
        }
        // A model fold that failed is not retried for a few turns, even when
        // something else stood in for it this time.
        if fold_failed {
            compact_backoff = COMPACT_BACKOFF_TURNS;
        }
        if let Some(c) = chosen {
            if !fold_failed {
                compact_backoff = 0;
            }
            prefold = None;
            bg_failed = false;
            compacted += 1;
            sink.emit(&AgentEvent::ContextCompacted {
                meta: meta(&cfg.agent_name, &run_id, cfg.parent_run_id.as_deref()),
                input_tokens: used,
                window,
                dropped_items: c.dropped,
                kept_items: c.kept,
                memory: compact::previous_memory(&c.items).unwrap_or_default(),
                method: c.method,
            });
            history = c.items;
            pending = history.clone();
            last_usage = CacheUsage::default();
            // Compaction may drop pixels too. Recount, or the next image
            // would not count as new and would expire early.
            images_known = crate::vision::image_item_count(&history);
            if images_known == 0 {
                image_turns = 0;
            }
            seen_upto = 0;
            checkpoint(&cfg, &history);
            used = real(compact::estimate_tokens("", &history).saturating_add(fixed));
        }

        // Always resend the full append-only transcript. Chaining with
        // `previous_response_id` and a pending-only `input` makes every tool
        // turn start with a different prefix, so cached_tokens stays 0.
        let has_image = crate::vision::input_has_image(&history);
        let sent_len = history.len();
        // Past the start point, fold in a task of its own while the work goes
        // on; the result is spliced in as soon as it is ready (step 1 above).
        // Not when a fold would gain little (small windows, where fixed costs
        // dominate): that would cost the cache a miss for little room. Not
        // with images either: stripping them rewrites old items, and the
        // splice needs the folded part untouched.
        if used >= budget.prefold
            && used < budget.trigger
            && prefold.is_none()
            && !bg_failed
            && compact_backoff == 0
            && !has_image
        {
            let memory = compact::memory_item_tokens(&history).unwrap_or(fold_budget.memory);
            let worth = compact::fold_floor(&history, fixed, memory, &fold_budget, max_tail_items)
                .is_some_and(|floor| real(floor).saturating_add(budget.window / 10) <= used);
            if worth {
                let snapshot = history.clone();
                let (seen, boundary) = (snapshot.len(), snapshot.last().cloned());
                let p = provider.clone();
                let (instructions, goal, model, cache_key) =
                    (live_instructions(&cfg), fold_goal(&cfg), model.clone(), cache_key.clone());
                let (client_tools, server_tools, workspace) =
                    (tools.specs_for(&server_tools), server_tools.clone(), cfg.workspace.clone());
                let task = tokio::spawn(async move {
                    let quiet = |_: String| {};
                    let ctx = compact::FoldCtx {
                        provider: &p,
                        instructions: &instructions,
                        goal: &goal,
                        model: &model,
                        effort,
                        send_reasoning,
                        // The conversation's own key: the fold request starts
                        // with the same prefix, so it is served from cache.
                        cache_key: &cache_key,
                        client_tools,
                        server_tools,
                        budget: fold_budget,
                        max_tail_items,
                        workspace: Some(&workspace),
                        prefold: true,
                        notice: &quiet,
                    };
                    compact::fold(&ctx, &snapshot).await.ok()
                });
                prefold = Some(BgFold { task, seen, boundary });
                notice(
                    sink,
                    &cfg.agent_name,
                    &run_id,
                    cfg.parent_run_id.as_deref(),
                    format!("背景整理長期記憶中（上下文 {}%），工作不中斷", used as u64 * 100 / window.max(1) as u64),
                );
            }
        }
        let req = CompleteRequest {
            instructions: live_instructions(&cfg),
            input: history.clone(),
            client_tools: tools.specs_for(&server_tools),
            server_tools: server_tools.clone(),
            cache_key: cache_key.clone(),
            previous_response_id: None,
            store: false,
            reasoning_effort: effort,
            send_reasoning,
            model: model.clone(),
            tool_choice: None,
        };

        let parent = cfg.parent_run_id.clone();
        let agent_name = cfg.agent_name.clone();
        let rid = run_id.clone();
        let streamed_server = std::sync::atomic::AtomicBool::new(false);
        let streamed_reasoning = std::sync::atomic::AtomicBool::new(false);
        // Text and reasoning streamed by the current attempt, for the cost of
        // one the loop guard cuts off.
        let streamed_chars = std::sync::atomic::AtomicUsize::new(0);
        let repeat_ctx = crate::repeat::Context::from_items(&history);
        let repeat_guard = crate::repeat::Guard::with_context(repeat_ctx.clone());
        let on_text = |delta: &str| {
            if delta.is_empty() {
                return;
            }
            repeat_guard.feed_text(delta);
            streamed_chars.fetch_add(delta.len(), std::sync::atomic::Ordering::Relaxed);
            sink.emit(&AgentEvent::ModelDelta {
                meta: meta(&agent_name, &rid, parent.as_deref()),
                text: delta.to_string(),
            });
        };
        let on_server = |kind: &str, payload: &Value| {
            streamed_server.store(true, std::sync::atomic::Ordering::Relaxed);
            sink.emit(&AgentEvent::ServerToolObserved {
                meta: meta(&agent_name, &rid, parent.as_deref()),
                kind: kind.to_string(),
                payload: payload.clone(),
            });
        };
        let on_reasoning = |delta: &str| {
            if delta.is_empty() {
                return;
            }
            streamed_reasoning.store(true, std::sync::atomic::Ordering::Relaxed);
            repeat_guard.feed_reasoning(delta);
            streamed_chars.fetch_add(delta.len(), std::sync::atomic::Ordering::Relaxed);
            sink.emit(&AgentEvent::ReasoningDelta {
                meta: meta(&agent_name, &rid, parent.as_deref()),
                text: delta.to_string(),
            });
        };

        // Transient provider trouble (rate limits, 5xx, a dropped stream)
        // resends the same request after a backoff instead of spending one
        // of the model-visible recoveries below.
        let main = async {
        let mut transient_attempt = 0u32;
        loop {
            repeat_guard.reset();
            streamed_chars.store(0, std::sync::atomic::Ordering::Relaxed);
            // A response that falls into a repetition loop is stopped as soon
            // as the loop shows, not when the model hits its output limit.
            let guarded = async {
                tokio::select! {
                    r = provider.complete_stream(req.clone(), &on_text, &on_server, &on_reasoning) => r,
                    trip = repeat_guard.tripped() => Err(trip.to_error()),
                }
            };
            let r = race_cancel(&cfg.cancel, guarded).await;
            match r {
                Ok(Err(e))
                    if transient_attempt < MAX_TRANSIENT_RETRIES
                        && crate::provider::transient_error(&e) =>
                {
                    let wait = transient_backoff(transient_attempt);
                    transient_attempt += 1;
                    let reason: String = e.to_string().chars().take(120).collect();
                    notice(
                        sink,
                        &cfg.agent_name,
                        &run_id,
                        cfg.parent_run_id.as_deref(),
                        format!(
                            "模型服務暫時出錯，{} 秒後重送（{transient_attempt}/{MAX_TRANSIENT_RETRIES}）：{reason}",
                            wait.as_secs().max(1)
                        ),
                    );
                    if race_cancel(&cfg.cancel, tokio::time::sleep(wait)).await.is_err() {
                        break Err(());
                    }
                }
                other => break other,
            }
        }
        };
        let attempt = main.await;
        let response = match attempt {
            Err(()) => {
                if !take_interrupt(
                    &mut cfg,
                    provider,
                    sink,
                    &run_id,
                    &last_text,
                    &mut history,
                    &mut pending,
                )
                .await
                {
                    return Ok(outcome(
                        &cfg,
                        &history,
                        run_id,
                        last_text,
                        total_turns,
                        cache_turns,
                        compacted,
                    ));
                }
                turn = 0;
                continue;
            }
            Ok(Ok(r)) => match crate::repeat::check_response(&r.text, &r.function_calls, &repeat_ctx) {
                None => r,
                Some(trip) => {
                    // The dropped reply was billed all the same.
                    sink.emit(&AgentEvent::UsageReported {
                        meta: meta(&cfg.agent_name, &run_id, cfg.parent_run_id.as_deref()),
                        kind: "loop".into(),
                        input_tokens: r.usage.input_tokens,
                        cached_tokens: r.usage.cached_tokens,
                        output_tokens: r.usage.output_tokens,
                    });
                    handle_loop(&mut cfg, provider, sink, &run_id, &mut history, &mut pending, &mut loop_trips, &mut consecutive_provider_errors, &last_text, trip).await?;
                    continue;
                }
            },
            Ok(Err(e)) if crate::repeat::is_loop(&e) => {
                // Cut off mid-stream, so no usage came back: estimate it from
                // the last request's input and what streamed (≈4 chars a token).
                sink.emit(&AgentEvent::UsageReported {
                    meta: meta(&cfg.agent_name, &run_id, cfg.parent_run_id.as_deref()),
                    kind: "loop".into(),
                    input_tokens: last_usage.input_tokens,
                    cached_tokens: last_usage.cached_tokens,
                    output_tokens: (streamed_chars.load(std::sync::atomic::Ordering::Relaxed) / 4) as u32,
                });
                let trip = repeat_guard.trip().unwrap_or(crate::repeat::Trip {
                    source: "text".into(),
                    unit: String::new(),
                    repeats: 0,
                });
                handle_loop(&mut cfg, provider, sink, &run_id, &mut history, &mut pending, &mut loop_trips, &mut consecutive_provider_errors, &last_text, trip).await?;
                continue;
            }
            Ok(Err(e)) => {
                let images = recover_from_provider_error(
                    &mut cfg,
                    provider,
                    sink,
                    &run_id,
                    &mut history,
                    &mut pending,
                    &mut consecutive_provider_errors,
                    &last_text,
                    Failed { model: &model, seen: &seen_by_vision },
                    e,
                )
                .await?;
                // Kept images keep their clock. Removed ones are gone: any
                // sent from now on count as new (checked at the loop top).
                if images != ImagesAfterError::Kept {
                    images_known = 0;
                    image_turns = 0;
                }
                if images == ImagesAfterError::Refused {
                    blind.insert(model.clone());
                    tell_blind(sink, &cfg, &run_id, &model, &mut blind_noticed);
                }
                continue;
            }
        };
        consecutive_provider_errors = 0;
        loop_trips = 0;
        if has_image && !provider.sees_images(&model) {
            tell_blind(sink, &cfg, &run_id, &model, &mut blind_noticed);
            // The provider sent none of these pixels: the results that
            // claimed them say so now, for the model and the picture rows.
            strip_for_blind_model(sink, &cfg, &run_id, &mut history, &model, &seen_by_vision);
        } else if has_image {
            seen_by_vision.extend(crate::vision::image_call_ids(&history[..sent_len]));
        }

        last_usage = response.usage.clone();
        usage_base_len = sent_len;
        seen_upto = sent_len;
        if last_usage.input_tokens > 0 && !has_image {
            calib = compact::calibration(last_usage.input_tokens, &req.instructions, &req.client_tools, &history[..sent_len]);
        }
        cache_turns.push(last_usage.clone());
        if last_usage.below_target(turn) {
            notice(
                sink,
                &cfg.agent_name,
                &run_id,
                cfg.parent_run_id.as_deref(),
                format!(
                    "快取命中 {:.0}%（目標 ≥90%）turn={turn} cached={}/{}",
                    last_usage.rate() * 100.0,
                    last_usage.cached_tokens,
                    last_usage.input_tokens
                ),
            );
        }

        if !streamed_server.load(std::sync::atomic::Ordering::Relaxed) {
            for item in &response.server_items {
                let kind = item
                    .get("type")
                    .and_then(Value::as_str)
                    .unwrap_or("server_tool")
                    .to_string();
                sink.emit(&AgentEvent::ServerToolObserved {
                    meta: meta(&cfg.agent_name, &run_id, cfg.parent_run_id.as_deref()),
                    kind,
                    payload: item.clone(),
                });
            }
        }
        if !streamed_reasoning.load(std::sync::atomic::Ordering::Relaxed) {
            let think = crate::provider::extract_reasoning_text(&response.output_items);
            if !think.is_empty() {
                sink.emit(&AgentEvent::ReasoningDelta {
                    meta: meta(&cfg.agent_name, &run_id, cfg.parent_run_id.as_deref()),
                    text: think,
                });
            }
        }

        history.extend(response.output_items.iter().cloned());
        append_missing_function_calls(&mut history, &response.function_calls);
        pending.clear();
        seen_model = true;
        if has_image {
            image_turns = image_turns.saturating_add(1);
            crate::vision::prune_attached_images(&mut history, image_turns);
            if image_turns >= crate::vision::IMAGE_KEEP_TURNS {
                image_turns = 0;
            }
            images_known = crate::vision::image_item_count(&history);
        }

        // The accepted result of the terminal tool, if this batch made one.
        let mut terminal: Option<String> = None;
        if !response.function_calls.is_empty() {
            emit_model_finished(
                sink,
                &cfg.agent_name,
                &run_id,
                cfg.parent_run_id.as_deref(),
                response.text.clone(),
                "tool_calls",
                &last_usage,
            );
            let mut interrupted = false;
            // Images attached by this batch; past the cap the rest are not.
            let mut attached = 0usize;
            let sees = provider.sees_images(&model) && !blind.contains(&model);
            let batch_text = response.text.clone();
            // The user proxy may answer an ask_user from the user's words.
            if let Some(hub) = cfg.task.as_ref().filter(|h| h.steering_open()) {
                if response.function_calls.iter().any(|c| c.name == "ask_user") {
                    hub.set_ask_context(crate::task::ask_context(&history, &batch_text));
                }
            }
            for call in response.function_calls {
                let args: Value = serde_json::from_str(&call.arguments).unwrap_or(json!({}));
                if interrupted || cfg.cancel.as_ref().is_some_and(CancelFlag::is_set) {
                    emit_cancelled_tool(
                        sink,
                        &cfg,
                        &run_id,
                        &call.call_id,
                        &call.name,
                        false,
                        &args,
                        &mut history,
                        &mut pending,
                    );
                    interrupted = true;
                    continue;
                }
                sink.emit(&AgentEvent::ToolStarted {
                    meta: meta(&cfg.agent_name, &run_id, cfg.parent_run_id.as_deref()),
                    call_id: call.call_id.clone(),
                    name: call.name.clone(),
                    args: args.clone(),
                    kind: "client".into(),
                });
                let (mut output, cancelled) = match race_cancel(&cfg.cancel, tools.call(&call.name, &args)).await {
                    Ok(Ok(o)) => (o, false),
                    Ok(Err(e)) => (json!({"error": e.to_string()}).to_string(), false),
                    Err(()) => (cancelled_tool_output(), true),
                };
                interrupted |= cancelled;
                // Attach before reporting the result: when no pixels go out,
                // the result says so, for the model and the picture row alike.
                let mut image_uri = None;
                if !cancelled && asks_for_image(&output) {
                    if !sees {
                        output = without_image(
                            &output,
                            &format!("{model} cannot view images, so no pixels were attached. Do not guess what the image shows."),
                        );
                    } else if attached >= crate::vision::KEEP_RECENT_IMAGES {
                        output = without_image(
                            &output,
                            &format!(
                                "At most {} images are attached per turn, so this one was not. read_image its path in a later turn to view it.",
                                crate::vision::KEEP_RECENT_IMAGES
                            ),
                        );
                    } else {
                        // Decoding a big file takes a while: keep it off the
                        // async threads, and let Esc stop the wait.
                        let (ws, out) = (cfg.workspace.clone(), output.clone());
                        let load = tokio::task::spawn_blocking(move || crate::vision::data_uri_for_tool(&ws, &out));
                        match race_cancel(&cfg.cancel, load).await {
                            Ok(Ok(Ok(uri))) => image_uri = uri,
                            Ok(Ok(Err(e))) => {
                                notice(
                                    sink,
                                    &cfg.agent_name,
                                    &run_id,
                                    cfg.parent_run_id.as_deref(),
                                    format!("圖片附加略過: {e}"),
                                );
                                output = without_image(&output, &e.to_string());
                            }
                            Ok(Err(e)) => output = without_image(&output, &format!("loading the image failed: {e}")),
                            Err(()) => {
                                interrupted = true;
                                output = without_image(&output, "interrupted before the image was attached");
                            }
                        }
                    }
                }
                sink.emit(&AgentEvent::ToolFinished {
                    meta: meta(&cfg.agent_name, &run_id, cfg.parent_run_id.as_deref()),
                    call_id: call.call_id.clone(),
                    name: call.name.clone(),
                    output: output.clone(),
                });
                if cancelled {
                    let item = crate::vision::function_call_output(&call.call_id, &output, None);
                    history.push(item.clone());
                    pending.push(item);
                    continue;
                }
                if terminal.is_none() && cfg.terminal_tool.as_deref() == Some(call.name.as_str()) {
                    terminal = accepted_result(&output);
                }
                watch.call(&call.name, &args, &output);
                if emit_file_changes(
                    sink,
                    &cfg.agent_name,
                    &run_id,
                    cfg.parent_run_id.as_deref(),
                    &output,
                ) > 0
                {
                    watch.progress();
                }
                let shown = crate::tools::model_view(&output);
                let item = crate::vision::function_call_output(&call.call_id, &shown, image_uri.as_deref());
                if image_uri.is_some() {
                    attached += 1;
                }
                history.push(item.clone());
                pending.push(item);
            }
            if attached > 0 {
                image_turns = 0;
                crate::vision::retain_seen_images(&mut history, crate::vision::KEEP_RECENT_IMAGES, seen_upto);
                images_known = crate::vision::image_item_count(&history);
            }
            watch.tool_round();
            if !interrupted {
                if let Some(instr) = task_checkpoint(&cfg, provider, sink, &run_id, &history, &batch_text, &mut watch).await {
                    append_user(&mut history, &mut pending, &UserTurn::from(instr), &cfg.workspace);
                    watch.progress();
                }
            }
            if interrupted {
                if !take_interrupt(
                    &mut cfg,
                    provider,
                    sink,
                    &run_id,
                    &last_text,
                    &mut history,
                    &mut pending,
                )
                .await
                {
                    return Ok(outcome(
                        &cfg,
                        &history,
                        run_id,
                        last_text,
                        total_turns,
                        cache_turns,
                        compacted,
                    ));
                }
                turn = 0;
                continue;
            }
            if terminal.is_none() {
                continue;
            }
        }

        // A handed-in result ends the chain like a stop; its tokens were
        // counted with the tool-call turn.
        let stop_usage = if terminal.is_some() { CacheUsage::default() } else { last_usage.clone() };
        last_text = terminal.unwrap_or(response.text);
        emit_model_finished(
            sink,
            &cfg.agent_name,
            &run_id,
            cfg.parent_run_id.as_deref(),
            last_text.clone(),
            "stop",
            &stop_usage,
        );

        watch.progress();
        let extra: Vec<UserTurn> = drain_session(&mut cfg)
            .into_iter()
            .filter(|m| !crate::task::is_kick(&m.text))
            .collect();
        if !extra.is_empty() {
            note_user_turns(&cfg, &extra);
            for msg in extra {
                append_user(&mut history, &mut pending, &msg, &cfg.workspace);
            }
            turn = 0;
            continue;
        }

        if cfg.inbox.is_none() && cfg.hold.is_some() {
            // Headless: a pending workflow's notice is the next message
            // (wait_session also picks up one that just arrived).
            if let Some(msgs) = wait_session(&mut cfg).await {
                note_user_turns(&cfg, &msgs);
                for msg in msgs {
                    append_user(&mut history, &mut pending, &msg, &cfg.workspace);
                }
                turn = 0;
                continue;
            }
        }
        if cfg.inbox.is_none() {
            sink.emit(&AgentEvent::RunFinished {
                meta: meta(&cfg.agent_name, &run_id, cfg.parent_run_id.as_deref()),
                reason: "stop".into(),
                text: last_text.clone(),
            });
            return Ok(outcome(
                &cfg,
                &history,
                run_id,
                last_text,
                total_turns,
                cache_turns,
                compacted,
            ));
        }

        checkpoint(&cfg, &history);
        // The proxy's pushes share the turn budget: a cap (headless
        // --max-turns) bounds a task the proxy never accepts. A verdict
        // that ends the task still gets its closing turn.
        if let Some(instr) = task_after_stop(&cfg, provider, sink, &run_id, &history, &last_text).await {
            append_user(
                &mut history,
                &mut pending,
                &UserTurn::from(instr),
                &cfg.workspace,
            );
            if cfg
                .task
                .as_ref()
                .is_some_and(|h| h.ending_pending() || h.snapshot().phase == crate::task::TaskPhase::ExplainingFail)
            {
                turn = turn.min(max_turns.saturating_sub(1));
            }
            continue;
        }
        if cfg.cancel.as_ref().is_some_and(CancelFlag::is_set) {
            continue;
        }
        sink.emit(&AgentEvent::AwaitingInput {
            meta: meta(&cfg.agent_name, &run_id, cfg.parent_run_id.as_deref()),
        });
        match wait_for_input(&mut cfg, provider, sink, &run_id, &mut history, &mut pending, &last_text, false).await {
            Some(true) => turn = 0,
            Some(false) => {}
            None => {
                sink.emit(&AgentEvent::RunFinished {
                    meta: meta(&cfg.agent_name, &run_id, cfg.parent_run_id.as_deref()),
                    reason: "stop".into(),
                    text: last_text.clone(),
                });
                return Ok(outcome(
                    &cfg,
                    &history,
                    run_id,
                    last_text,
                    total_turns,
                    cache_turns,
                    compacted,
                ));
            }
        }
    }
}

/// The goal a context fold keeps as the original request: in a task run
/// started without a prompt of its own, the task's goal, not the kick
/// marker.
fn fold_goal(cfg: &RunConfig) -> String {
    if crate::task::is_kick(&cfg.prompt) {
        if let Some(goal) = cfg.task.as_ref().map(|h| h.snapshot().goal).filter(|g| !g.trim().is_empty()) {
            return goal;
        }
    }
    cfg.prompt.clone()
}

fn proxy_call<'a, P: Provider>(
    cfg: &'a RunConfig,
    hub: &'a crate::task::TaskHub,
    provider: &'a P,
    sink: &'a dyn EventSink,
    run_id: &'a str,
) -> crate::task::ProxyCall<'a, P> {
    let (model, effort, send_reasoning) = crate::task::proxy_settings(cfg.knobs.as_ref(), &cfg.model, cfg.reasoning_effort);
    crate::task::ProxyCall {
        hub,
        provider,
        model,
        effort,
        send_reasoning,
        run_id,
        workspace: &cfg.workspace,
        sink,
    }
}

/// Task mode at run start: the proxy's opening brief, or in a run started
/// for a live task the look or closing turn it is owed. Esc aborts it.
async fn task_first_turn<P: Provider>(
    cfg: &RunConfig,
    provider: &P,
    sink: &dyn EventSink,
    run_id: &str,
    history: &[Value],
) -> Option<String> {
    let hub = cfg.task.as_ref()?;
    let call = proxy_call(cfg, hub, provider, sink, run_id);
    let kick = crate::task::is_kick(&cfg.prompt);
    race_cancel(&cfg.cancel, crate::task::prepare_first_turn(&call, history, kick)).await.ok().flatten()
}

/// Task mode after the worker stopped on its own.
async fn task_after_stop<P: Provider>(
    cfg: &RunConfig,
    provider: &P,
    sink: &dyn EventSink,
    run_id: &str,
    history: &[Value],
    last_text: &str,
) -> Option<String> {
    let hub = cfg.task.as_ref()?;
    let call = proxy_call(cfg, hub, provider, sink, run_id);
    race_cancel(&cfg.cancel, crate::task::after_natural_stop(&call, history, last_text)).await.ok().flatten()
}

/// Task mode when an idle wait woke; `users` are the real user messages.
async fn task_idle<P: Provider>(
    cfg: &RunConfig,
    provider: &P,
    sink: &dyn EventSink,
    run_id: &str,
    history: &[Value],
    last_text: &str,
    users: &[String],
) -> Option<String> {
    let hub = cfg.task.as_ref()?;
    let call = proxy_call(cfg, hub, provider, sink, run_id);
    race_cancel(&cfg.cancel, crate::task::after_wait(&call, history, last_text, users)).await.ok().flatten()
}

/// Task mode after a tool batch: the proxy looks in when a new goal or a
/// review request is waiting, or when the worker seems stuck.
async fn task_checkpoint<P: Provider>(
    cfg: &RunConfig,
    provider: &P,
    sink: &dyn EventSink,
    run_id: &str,
    history: &[Value],
    last_text: &str,
    watch: &mut crate::task::Watch,
) -> Option<String> {
    let hub = cfg.task.as_ref()?;
    if !hub.steering_open() {
        return None;
    }
    let why = if hub.snapshot().phase == crate::task::TaskPhase::NeedPlan {
        "the user just started this task while the worker was busy".to_string()
    } else if hub.take_review_request() {
        "the user asked you to look now".to_string()
    } else {
        watch.due()?
    };
    watch.looked();
    let call = proxy_call(cfg, hub, provider, sink, run_id);
    race_cancel(&cfg.cancel, crate::task::checkpoint(&call, history, last_text, &why)).await.ok().flatten()
}

#[cfg(test)]
mod agent_tests {
    use super::*;
    use crate::compact::MEMORY_FOLD_MARK;
    use crate::provider::{CompactRequest, CompactResponse, CompleteResponse, FunctionCall};
    use crate::tools::{ClientTool, NowTool, ReadImageTool, ToolCallFut, ToolSpec};
    use std::collections::VecDeque;

    /// Replies in order and records the working requests. Memory-fold
    /// requests get a memory and, once, put `after_fold` in the inbox.
    /// With `refuse`, a request carrying pixels fails with it instead,
    /// as a route that does not learn missing vision would.
    struct Script {
        calls: Mutex<Vec<CompleteRequest>>,
        replies: Mutex<VecDeque<Result<CompleteResponse>>>,
        blind: std::sync::atomic::AtomicBool,
        /// A request carrying pixels makes it blind, as the OpenAI route
        /// learns when it resends one without them.
        learns: bool,
        refuse: Option<&'static str>,
        after_fold: Mutex<Option<(mpsc::UnboundedSender<UserTurn>, UserTurn)>>,
    }

    impl Script {
        fn new(replies: Vec<Result<CompleteResponse>>) -> Self {
            Self {
                calls: Mutex::new(Vec::new()),
                replies: Mutex::new(replies.into()),
                blind: false.into(),
                learns: false,
                refuse: None,
                after_fold: Mutex::new(None),
            }
        }
    }

    impl Provider for Script {
        async fn complete(&self, req: CompleteRequest) -> Result<CompleteResponse> {
            let fold = req.input.iter().any(|i| i["content"].as_str().is_some_and(|s| s.contains(MEMORY_FOLD_MARK)));
            if fold {
                if let Some((tx, turn)) = self.after_fold.lock().unwrap().take() {
                    tx.send(turn).unwrap();
                }
                return Ok(CompleteResponse {
                    text: "## Direction changes\nnone\n## Load-bearing facts\nnone\n## Where you were\nchecking the layout\n## Open work\ncompare\n".into(),
                    ..CompleteResponse::new("mem")
                });
            }
            let refused = self.refuse.filter(|_| crate::vision::input_has_image(&req.input));
            if self.learns && crate::vision::input_has_image(&req.input) {
                self.blind.store(true, std::sync::atomic::Ordering::SeqCst);
            }
            self.calls.lock().unwrap().push(req);
            if let Some(body) = refused {
                return Err(Error::Provider(format!("xAI HTTP 400: {body}")));
            }
            self.replies
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or_else(|| Err(Error::Provider("no scripted reply".into())))
        }

        async fn compact(&self, _req: CompactRequest) -> Result<CompactResponse> {
            Err(Error::Provider("no compact".into()))
        }

        fn sees_images(&self, _model: &str) -> bool {
            !self.blind.load(std::sync::atomic::Ordering::SeqCst)
        }
    }

    /// Claims a screenshot at a path whose file is not there.
    struct GhostShot;

    impl ClientTool for GhostShot {
        fn spec(&self) -> ToolSpec {
            ToolSpec { name: "screenshot".into(), description: "shot".into(), parameters: json!({"type": "object"}) }
        }

        fn call(&self, _args: &Value) -> ToolCallFut<'_> {
            Box::pin(async { Ok(json!({"path": "gone.jpg", "attach_image": true, "mime": "image/jpeg"}).to_string()) })
        }
    }

    struct Rec(Mutex<Vec<AgentEvent>>);

    impl EventSink for Rec {
        fn emit(&self, event: &AgentEvent) {
            self.0.lock().unwrap().push(event.clone());
        }
    }

    fn calls(names: &[(&str, &str, &str)]) -> Result<CompleteResponse> {
        Ok(CompleteResponse {
            function_calls: names
                .iter()
                .map(|(id, name, args)| FunctionCall { call_id: (*id).into(), name: (*name).into(), arguments: (*args).into() })
                .collect(),
            ..CompleteResponse::new("r")
        })
    }

    fn now(id: &str) -> Result<CompleteResponse> {
        calls(&[(id, "now", "{}")])
    }

    fn text(t: &str) -> Result<CompleteResponse> {
        Ok(CompleteResponse { text: t.into(), ..CompleteResponse::new("t") })
    }

    fn picture(dir: &std::path::Path, name: &str) {
        let img = crate::vision::from_rgba(32, 32, vec![200, 0, 0, 255].repeat(32 * 32)).unwrap();
        crate::vision::save_jpeg(&dir.join(name), &img).unwrap();
    }

    fn config(dir: &std::path::Path, prompt: &str) -> RunConfig {
        RunConfig {
            agent_name: "root".into(),
            instructions: "use tools".into(),
            prompt: prompt.into(),
            model: "grok-4.6".into(),
            max_turns: 40,
            server_tools: vec![],
            context_window: 0,
            compact_keep_recent: 0,
            parent_run_id: None,
            run_id: String::new(),
            reasoning_effort: crate::provider::ReasoningEffort::High,
            knobs: None,
            inbox: None,
            bg_notify: None,
            workspace: dir.to_path_buf(),
            images: Vec::new(),
            closed_backgrounds: None,
            prior_history: Vec::new(),
            persist_history: None,
            cancel: None,
            skills: None,
            task: None,
            terminal_tool: None,
            hold: None,
            on_user_turn: None,
        }
    }

    fn output_of<'a>(req: &'a CompleteRequest, call_id: &str) -> &'a Value {
        req.input
            .iter()
            .find(|i| i["type"] == "function_call_output" && i["call_id"] == call_id)
            .unwrap_or_else(|| panic!("no output for {call_id}"))
    }

    fn finished_output(events: &[AgentEvent], call: &str) -> Value {
        events
            .iter()
            .find_map(|e| match e {
                AgentEvent::ToolFinished { call_id, output, .. } if call_id == call => serde_json::from_str(output).ok(),
                _ => None,
            })
            .unwrap_or_else(|| panic!("no ToolFinished for {call}"))
    }

    fn user_has_image(req: &CompleteRequest) -> bool {
        req.input.iter().any(|i| i["role"] == "user" && crate::vision::input_has_image(std::slice::from_ref(i)))
    }

    #[tokio::test]
    async fn a_failed_attach_tells_the_model_and_the_picture_row() {
        let dir = tempfile::tempdir().unwrap();
        let provider = Arc::new(Script::new(vec![calls(&[("c1", "screenshot", "{}")]), text("no image arrived")]));
        let tools = ToolRegistry::new(vec![Box::new(GhostShot)]);
        let rec = Rec(Mutex::new(Vec::new()));
        run(&provider, &tools, &rec, config(dir.path(), "check the UI")).await.unwrap();
        let reqs = provider.calls.lock().unwrap();
        let sent: Value = serde_json::from_str(output_of(&reqs[1], "c1")["output"].as_str().unwrap()).unwrap();
        assert_eq!(sent["attach_image"], false, "{sent}");
        assert!(sent["attach_error"].as_str().is_some_and(|e| !e.is_empty()), "{sent}");
        assert_eq!(sent["path"], "gone.jpg");
        let shown = finished_output(&rec.0.lock().unwrap(), "c1");
        assert_eq!(shown["attach_image"], false, "the UI must not show it as looked at: {shown}");
    }

    #[tokio::test]
    async fn a_model_that_cannot_see_gets_no_pixels_and_is_told_why() {
        let dir = tempfile::tempdir().unwrap();
        picture(dir.path(), "p.jpg");
        picture(dir.path(), "u.jpg");
        let script = Script::new(vec![calls(&[("c1", "read_image", r#"{"path":"p.jpg"}"#)]), text("I cannot see it")]);
        script.blind.store(true, std::sync::atomic::Ordering::SeqCst);
        let provider = Arc::new(script);
        let tools = ToolRegistry::new(vec![Box::new(ReadImageTool::new(dir.path().to_path_buf()))]);
        let rec = Rec(Mutex::new(Vec::new()));
        let mut c = config(dir.path(), "what is wrong here?");
        c.images = vec![PathBuf::from("u.jpg")];
        run(&provider, &tools, &rec, c).await.unwrap();
        let reqs = provider.calls.lock().unwrap();
        let out = &output_of(&reqs[1], "c1")["output"];
        let sent: Value = serde_json::from_str(out.as_str().expect("no pixels: plain text")).unwrap();
        assert_eq!(sent["attach_image"], false);
        assert!(sent["attach_error"].as_str().unwrap().contains("cannot view images"), "{sent}");
        let events = rec.0.lock().unwrap();
        let told = events
            .iter()
            .filter(|e| matches!(e, AgentEvent::Notice { message, .. } if message.contains("無法看圖")))
            .count();
        assert_eq!(told, 1, "the user hears it once");
    }

    #[tokio::test]
    async fn the_image_that_reveals_a_blind_model_stops_claiming_pixels() {
        let dir = tempfile::tempdir().unwrap();
        picture(dir.path(), "p.jpg");
        let mut script = Script::new(vec![
            calls(&[("c1", "read_image", r#"{"path":"p.jpg"}"#)]),
            now("c2"),
            text("worked from the path"),
        ]);
        script.learns = true;
        let provider = Arc::new(script);
        let tools = ToolRegistry::new(vec![Box::new(ReadImageTool::new(dir.path().to_path_buf())), Box::new(NowTool)]);
        let rec = Rec(Mutex::new(Vec::new()));
        run(&provider, &tools, &rec, config(dir.path(), "what is wrong here?")).await.unwrap();
        let reqs = provider.calls.lock().unwrap();
        assert!(crate::vision::input_has_image(&reqs[1].input), "attached before the provider learned");
        let later = output_of(&reqs[2], "c1")["output"].as_str().expect("no pixels: plain text").to_string();
        let later: Value = serde_json::from_str(&later).unwrap();
        assert_eq!(later["attach_image"], false, "{later}");
        assert!(later["attach_error"].as_str().unwrap().contains("cannot view images"), "{later}");
        let events = rec.0.lock().unwrap();
        let last = events
            .iter()
            .filter_map(|e| match e {
                AgentEvent::ToolFinished { call_id, output, .. } if call_id == "c1" => Some(output.clone()),
                _ => None,
            })
            .last()
            .unwrap();
        let last: Value = serde_json::from_str(&last).unwrap();
        assert_eq!(last["attach_image"], false, "the picture row is taken back: {last}");
    }

    #[tokio::test]
    async fn a_big_batch_attaches_up_to_the_cap_and_says_so_for_the_rest() {
        let dir = tempfile::tempdir().unwrap();
        picture(dir.path(), "p.jpg");
        picture(dir.path(), "u.jpg");
        let n = crate::vision::KEEP_RECENT_IMAGES + 1;
        let ids: Vec<String> = (1..=n).map(|i| format!("c{i}")).collect();
        let batch: Vec<(&str, &str, &str)> = ids.iter().map(|id| (id.as_str(), "read_image", r#"{"path":"p.jpg"}"#)).collect();
        let provider = Arc::new(Script::new(vec![calls(&batch), text("checked")]));
        let tools = ToolRegistry::new(vec![Box::new(ReadImageTool::new(dir.path().to_path_buf()))]);
        let rec = Rec(Mutex::new(Vec::new()));
        let mut c = config(dir.path(), "which icons are blurry?");
        c.images = vec![PathBuf::from("u.jpg")];
        run(&provider, &tools, &rec, c).await.unwrap();
        let reqs = provider.calls.lock().unwrap();
        for id in &ids[..n - 1] {
            assert!(
                crate::vision::input_has_image(std::slice::from_ref(output_of(&reqs[1], id))),
                "{id} was attached and never sent: it must go out"
            );
        }
        let last: Value = serde_json::from_str(output_of(&reqs[1], &ids[n - 1])["output"].as_str().unwrap()).unwrap();
        assert_eq!(last["attach_image"], false);
        assert!(last["attach_error"].as_str().unwrap().contains("read_image"), "{last}");
        // The user's image was already seen once; it gives way, path kept.
        let user = reqs[1].input[0]["content"].as_str().expect("seen image capped to text");
        assert!(user.contains("(image file: u.jpg)") && user.contains("no longer attached"), "{user}");
        assert_eq!(crate::vision::image_item_count(&reqs[1].input), crate::vision::KEEP_RECENT_IMAGES);
    }

    #[tokio::test]
    async fn a_pasted_image_does_not_push_out_tool_images_not_yet_sent() {
        let dir = tempfile::tempdir().unwrap();
        picture(dir.path(), "p.jpg");
        picture(dir.path(), "u.jpg");
        let ids: Vec<String> = (1..=crate::vision::KEEP_RECENT_IMAGES).map(|i| format!("c{i}")).collect();
        let batch: Vec<(&str, &str, &str)> = ids.iter().map(|id| (id.as_str(), "read_image", r#"{"path":"p.jpg"}"#)).collect();
        let provider = Arc::new(Script::new(vec![calls(&batch), text("checked")]));
        let tools = ToolRegistry::new(vec![Box::new(ReadImageTool::new(dir.path().to_path_buf()))]);
        let rec = Rec(Mutex::new(Vec::new()));
        // Pasted while the batch runs: it lands right after the tool results.
        let (tx, rx) = mpsc::unbounded_channel::<UserTurn>();
        tx.send(UserTurn { text: "this one too".into(), images: vec![PathBuf::from("u.jpg")] }).unwrap();
        drop(tx);
        let mut c = config(dir.path(), "which icons are blurry?");
        c.inbox = Some(rx);
        run(&provider, &tools, &rec, c).await.unwrap();
        let reqs = provider.calls.lock().unwrap();
        for id in &ids {
            assert!(crate::vision::input_has_image(std::slice::from_ref(output_of(&reqs[1], id))), "{id} never went out");
        }
        assert!(user_has_image(&reqs[1]));
    }

    #[tokio::test]
    async fn an_image_sent_after_a_fold_gets_its_whole_keep_window() {
        let dir = tempfile::tempdir().unwrap();
        picture(dir.path(), "p.jpg");
        picture(dir.path(), "u.jpg");
        let (tx, rx) = mpsc::unbounded_channel::<UserTurn>();
        let mut replies = vec![calls(&[("c1", "read_image", r#"{"path":"p.jpg"}"#)])];
        replies.extend(["c2", "c3", "c4"].map(now));
        // Usage past the trigger: the next turn folds the screenshot away.
        let mut big = now("c5").unwrap();
        big.usage = CacheUsage { input_tokens: 15_000, cached_tokens: 0, output_tokens: 0 };
        replies.push(Ok(big));
        replies.extend((6..=12).map(|i| now(&format!("c{i}"))));
        replies.push(text("matches now"));
        let script = Script::new(replies);
        *script.after_fold.lock().unwrap() =
            Some((tx, UserTurn { text: "compare with this".into(), images: vec![PathBuf::from("u.jpg")] }));
        let provider = Arc::new(script);
        let tools = ToolRegistry::new(vec![
            Box::new(ReadImageTool::new(dir.path().to_path_buf())),
            Box::new(NowTool),
        ]);
        let rec = Rec(Mutex::new(Vec::new()));
        let mut c = config(dir.path(), "check the layout");
        c.context_window = 20_000;
        c.compact_keep_recent = 2;
        c.inbox = Some(rx);
        let out = run(&provider, &tools, &rec, c).await.unwrap();
        assert_eq!(out.compacted, 1);
        let reqs = provider.calls.lock().unwrap();
        assert!(crate::vision::input_has_image(&reqs[4].input), "the screenshot is still attached before the fold");
        assert!(!crate::vision::input_has_image(&reqs[5].input), "the fold dropped it");
        let with_user_image: Vec<usize> = (0..reqs.len()).filter(|&i| user_has_image(&reqs[i])).collect();
        let want: Vec<usize> = (6..6 + crate::vision::IMAGE_KEEP_TURNS as usize).collect();
        assert_eq!(with_user_image, want);
    }

    fn image_turn(dir: &std::path::Path) -> Value {
        picture(dir, "a.jpg");
        crate::vision::user_message("why is this red?", &[PathBuf::from("a.jpg")], dir)
    }

    async fn recover(dir: &std::path::Path, err: Error, consecutive: u32) -> (Vec<Value>, String) {
        recover_as(dir, "grok-4.6", err, consecutive).await
    }

    async fn recover_as(dir: &std::path::Path, model: &str, err: Error, consecutive: u32) -> (Vec<Value>, String) {
        let cfg = &mut config(dir, "go");
        cfg.model = model.into();
        let rec = Rec(Mutex::new(Vec::new()));
        let mut history = vec![image_turn(dir)];
        let mut pending = history.clone();
        let mut n = consecutive;
        let failed = Failed { model, seen: &HashSet::new() };
        recover_from_provider_error(cfg, &Script::new(vec![]), &rec, "r", &mut history, &mut pending, &mut n, "", failed, err).await.unwrap();
        let note = history.last().unwrap()["content"].as_str().unwrap().to_string();
        (history, note)
    }

    #[tokio::test]
    async fn provider_errors_drop_images_only_when_they_may_be_the_cause() {
        let dir = tempfile::tempdir().unwrap();
        let unrelated = || Error::Provider("HTTP 400: tool parameters schema invalid".into());
        let (history, note) = recover(dir.path(), unrelated(), 0).await;
        assert!(crate::vision::input_has_image(&history), "an unrelated error keeps the pixels");
        assert!(!note.contains("removed"), "{note}");

        let (history, note) = recover(dir.path(), Error::Provider("HTTP 400: invalid_image: bad data".into()), 0).await;
        assert!(!crate::vision::input_has_image(&history));
        assert!(note.contains("images were removed") && note.contains("read_image"), "{note}");
        let user = history[0]["content"].as_str().unwrap();
        assert!(user.contains("(image file: a.jpg)"), "the path survives: {user}");

        let (history, _) = recover(dir.path(), Error::Provider("HTTP 400: maximum context length exceeded".into()), 0).await;
        assert!(!crate::vision::input_has_image(&history), "pixels add to an overflow");

        let (history, note) = recover(dir.path(), unrelated(), 1).await;
        assert!(!crate::vision::input_has_image(&history), "the resend failed too: drop them");
        assert!(note.contains("removed"), "{note}");

        let trip = crate::repeat::Trip { source: "text".into(), unit: "ok ".into(), repeats: 40 };
        let (history, note) = recover(dir.path(), trip.to_error(), 1).await;
        assert!(crate::vision::input_has_image(&history), "a repetition loop is not about images");
        assert!(!note.contains("removed"), "{note}");
    }

    #[tokio::test]
    async fn a_refusal_of_image_input_says_not_to_look_again() {
        let dir = tempfile::tempdir().unwrap();
        let refusal = Error::Provider("xAI HTTP 400: Image inputs are not supported by this model.".into());
        let (history, note) = recover(dir.path(), refusal, 0).await;
        assert!(!crate::vision::input_has_image(&history));
        assert!(note.contains("cannot view images") && note.contains("Do not call read_image"), "{note}");
        // Refusing content arrays without naming images is not taken as
        // missing vision here: nothing confirms it.
        let (history, note) = recover(dir.path(), Error::Provider("HTTP 400: effort must be a string".into()), 0).await;
        assert!(crate::vision::input_has_image(&history), "{note}");
    }

    #[tokio::test]
    async fn a_vision_model_id_in_an_error_is_not_about_images() {
        let dir = tempfile::tempdir().unwrap();
        let err = || Error::Provider(r#"HTTP 400: {"error":{"message":"registry.ollama.ai/library/llama3.2-vision:latest does not support tools","type":"api_error"}}"#.into());
        let (history, note) = recover_as(dir.path(), "llama3.2-vision", err(), 0).await;
        assert!(crate::vision::input_has_image(&history), "{note}");
        assert!(!note.contains("cannot view images"), "{note}");
        let (_, note) = recover_as(dir.path(), "llama3.2-vision", err(), 1).await;
        assert!(!note.contains("cannot view images"), "a second failure is not a refusal either: {note}");
    }

    #[tokio::test]
    async fn an_unrelated_error_leaves_the_image_clock_running() {
        let dir = tempfile::tempdir().unwrap();
        picture(dir.path(), "u.jpg");
        let mut replies = vec![now("c1"), now("c2"), Err(Error::Provider("HTTP 400: tool parameters schema invalid".into()))];
        replies.extend((3..=9).map(|i| now(&format!("c{i}"))));
        replies.push(text("done"));
        let provider = Arc::new(Script::new(replies));
        let tools = ToolRegistry::new(vec![Box::new(NowTool)]);
        let rec = Rec(Mutex::new(Vec::new()));
        let mut c = config(dir.path(), "what is wrong here?");
        c.images = vec![PathBuf::from("u.jpg")];
        run(&provider, &tools, &rec, c).await.unwrap();
        let reqs = provider.calls.lock().unwrap();
        let with_image: Vec<usize> = (0..reqs.len()).filter(|&i| user_has_image(&reqs[i])).collect();
        // Sent with every answered request of its keep window, plus the one
        // that failed: the failure did not restart the window.
        let want: Vec<usize> = (0..=crate::vision::IMAGE_KEEP_TURNS as usize).collect();
        assert_eq!(with_image, want);
    }

    #[tokio::test]
    async fn a_refusal_stops_pixels_for_the_rest_of_the_run() {
        let dir = tempfile::tempdir().unwrap();
        picture(dir.path(), "p.jpg");
        picture(dir.path(), "u.jpg");
        let mut script = Script::new(vec![calls(&[("c1", "read_image", r#"{"path":"p.jpg"}"#)]), text("worked from the paths")]);
        script.refuse = Some("Image inputs are not supported by this model.");
        let provider = Arc::new(script);
        let tools = ToolRegistry::new(vec![Box::new(ReadImageTool::new(dir.path().to_path_buf()))]);
        let rec = Rec(Mutex::new(Vec::new()));
        // Pasted later: it must not bring the refusal back.
        let (tx, rx) = mpsc::unbounded_channel::<UserTurn>();
        tx.send(UserTurn { text: "and this one".into(), images: vec![PathBuf::from("u.jpg")] }).unwrap();
        drop(tx);
        let mut c = config(dir.path(), "what is wrong here?");
        c.images = vec![PathBuf::from("u.jpg")];
        c.inbox = Some(rx);
        run(&provider, &tools, &rec, c).await.unwrap();
        let reqs = provider.calls.lock().unwrap();
        assert_eq!(reqs.len(), 3, "refused once only");
        assert!(user_has_image(&reqs[0]));
        let note = reqs[1].input.last().unwrap()["content"].as_str().unwrap();
        assert!(note.contains("cannot view images") && note.contains("Do not call read_image"), "{note}");
        let sent: Value = serde_json::from_str(output_of(&reqs[2], "c1")["output"].as_str().unwrap()).unwrap();
        assert_eq!(sent["attach_image"], false);
        assert!(sent["attach_error"].as_str().unwrap().contains("cannot view images"), "{sent}");
        assert!(!crate::vision::input_has_image(&reqs[2].input));
        let pasted = reqs[2].input.last().unwrap()["content"].as_str().unwrap();
        assert!(pasted.contains("and this one") && pasted.contains("(image file: u.jpg)"), "{pasted}");
        // Nothing left in the request sends the model back to read_image.
        let first = reqs[1].input[0]["content"].as_str().unwrap();
        assert!(!first.contains("read_image") && !pasted.contains("read_image"), "{first}\n{pasted}");
        let events = rec.0.lock().unwrap();
        let told = events
            .iter()
            .filter(|e| matches!(e, AgentEvent::Notice { message, .. } if message.contains("無法看圖")))
            .count();
        assert_eq!(told, 1, "the user hears it once");
    }

    /// Answers in order; after the second request the user picks
    /// `text-model`, which cannot view images. xAI refuses its pixels; the
    /// OpenAI route drops them and learns.
    struct Switching {
        calls: Mutex<Vec<CompleteRequest>>,
        replies: Mutex<VecDeque<Result<CompleteResponse>>>,
        knobs: Arc<Mutex<SessionKnobs>>,
        refuses: bool,
        learned: AtomicBool,
    }

    impl Provider for Switching {
        async fn complete(&self, req: CompleteRequest) -> Result<CompleteResponse> {
            let blind = req.model == "text-model" && crate::vision::input_has_image(&req.input);
            let n = {
                let mut calls = self.calls.lock().unwrap();
                calls.push(req);
                calls.len()
            };
            if blind && self.refuses {
                return Err(Error::Provider("xAI HTTP 400: Image inputs are not supported by this model.".into()));
            }
            if blind {
                self.learned.store(true, Ordering::SeqCst);
            }
            if n == 2 {
                self.knobs.lock().unwrap().model = "text-model".into();
            }
            self.replies.lock().unwrap().pop_front().unwrap_or_else(|| Err(Error::Provider("no scripted reply".into())))
        }

        async fn compact(&self, _req: CompactRequest) -> Result<CompactResponse> {
            Err(Error::Provider("no compact".into()))
        }

        fn sees_images(&self, model: &str) -> bool {
            model != "text-model" || !self.learned.load(Ordering::SeqCst)
        }
    }

    #[tokio::test]
    async fn switching_to_a_blind_model_keeps_the_pictures_a_seeing_one_looked_at() {
        for refuses in [true, false] {
            let dir = tempfile::tempdir().unwrap();
            picture(dir.path(), "p.jpg");
            let knobs = Arc::new(Mutex::new(SessionKnobs {
                model: "vision-model".into(),
                reasoning_effort: crate::provider::ReasoningEffort::High,
                send_reasoning: true,
                server_tools: vec![],
                dispatcher: false,
                child_model: String::new(),
                task_model: String::new(),
                task_reasoning_effort: crate::provider::ReasoningEffort::High,
                task_send_reasoning: true,
                search: false,
                ultracode: false,
                workflow_size: Default::default(),
            }));
            let provider = Arc::new(Switching {
                calls: Mutex::new(Vec::new()),
                replies: Mutex::new(VecDeque::from([
                    calls(&[("c1", "read_image", r#"{"path":"p.jpg"}"#)]),
                    now("c2"),
                    now("c3"),
                    text("fixed the CSS"),
                ])),
                knobs: knobs.clone(),
                refuses,
                learned: false.into(),
            });
            let tools = ToolRegistry::new(vec![Box::new(ReadImageTool::new(dir.path().to_path_buf())), Box::new(NowTool)]);
            let rec = Rec(Mutex::new(Vec::new()));
            let mut c = config(dir.path(), "what is wrong here?");
            c.knobs = Some(knobs);
            run(&provider, &tools, &rec, c).await.unwrap();
            let reqs = provider.calls.lock().unwrap();
            assert_eq!(reqs[1].model, "vision-model");
            assert!(crate::vision::input_has_image(&reqs[1].input), "the seeing model was sent the pixels");
            let last = reqs.last().unwrap();
            assert_eq!(last.model, "text-model");
            assert!(!crate::vision::input_has_image(&last.input));
            let out = output_of(last, "c1")["output"].as_str().unwrap();
            assert!(out.contains(r#""attach_image":true"#) && !out.contains("no pixels were attached"), "refuses={refuses}: {out}");
            assert!(out.contains("cannot view images") && !out.contains("read_image its path"), "refuses={refuses}: {out}");
            let events = rec.0.lock().unwrap();
            let reports = events.iter().filter(|e| matches!(e, AgentEvent::ToolFinished { call_id, .. } if call_id == "c1")).count();
            assert_eq!(reports, 1, "refuses={refuses}: the picture row stays");
        }
    }

    #[tokio::test]
    async fn a_refusal_names_the_model_the_request_went_to() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = &mut config(dir.path(), "go");
        cfg.model = "picked-since".into();
        let rec = Rec(Mutex::new(Vec::new()));
        let tool = crate::vision::function_call_output("c1", r#"{"path":"p.jpg","attach_image":true}"#, Some("data:image/jpeg;base64,AAAA"));
        let mut history = vec![tool];
        let mut pending = history.clone();
        let mut n = 0;
        let err = Error::Provider("xAI HTTP 400: Image inputs are not supported by this model.".into());
        let failed = Failed { model: "text-model", seen: &HashSet::new() };
        let images = recover_from_provider_error(cfg, &Script::new(vec![]), &rec, "r", &mut history, &mut pending, &mut n, "", failed, err).await.unwrap();
        assert_eq!(images, ImagesAfterError::Refused);
        let out: Value = serde_json::from_str(history[0]["output"].as_str().unwrap()).unwrap();
        assert!(out["attach_error"].as_str().unwrap().starts_with("text-model cannot view images"), "{out}");
    }

    /// A screenshot taken just as the user presses Esc.
    struct EscShot(CancelFlag);

    impl ClientTool for EscShot {
        fn spec(&self) -> ToolSpec {
            ToolSpec { name: "screenshot".into(), description: "shot".into(), parameters: json!({"type": "object"}) }
        }

        fn call(&self, _args: &Value) -> ToolCallFut<'_> {
            let esc = self.0.clone();
            Box::pin(async move {
                esc.trip();
                Ok(json!({"path": "p.jpg", "attach_image": true, "mime": "image/jpeg"}).to_string())
            })
        }
    }

    #[tokio::test]
    async fn esc_while_the_image_loads_says_it_was_not_attached() {
        let dir = tempfile::tempdir().unwrap();
        picture(dir.path(), "p.jpg");
        let provider = Arc::new(Script::new(vec![calls(&[("c1", "screenshot", "{}")]), text("stopped")]));
        let esc = CancelFlag::new();
        let tools = ToolRegistry::new(vec![Box::new(EscShot(esc.clone()))]);
        let rec = Rec(Mutex::new(Vec::new()));
        let (tx, rx) = mpsc::unbounded_channel::<UserTurn>();
        tx.send(UserTurn::from("go on".to_string())).unwrap();
        drop(tx);
        let mut c = config(dir.path(), "check the UI");
        c.cancel = Some(esc);
        c.inbox = Some(rx);
        run(&provider, &tools, &rec, c).await.unwrap();
        let events = rec.0.lock().unwrap();
        assert!(
            events.iter().any(|e| matches!(e, AgentEvent::Notice { message, .. } if message.contains("使用者中斷"))),
            "the run takes the interrupt path"
        );
        let shown = finished_output(&events, "c1");
        assert_eq!(shown["attach_image"], false, "{shown}");
        assert!(shown["attach_error"].as_str().unwrap().contains("interrupted"), "{shown}");
        let reqs = provider.calls.lock().unwrap();
        assert_eq!(reqs.len(), 2);
        let sent: Value = serde_json::from_str(output_of(&reqs[1], "c1")["output"].as_str().unwrap()).unwrap();
        assert_eq!(sent, shown);
        assert!(!crate::vision::input_has_image(&reqs[1].input));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::compact::{COMPACT_MARK, MEMORY_FOLD_MARK};
    use crate::events::AgentEvent;
    use crate::memory::ProjectMemoryTool;
    use crate::provider::{CacheUsage, CompactRequest, CompactResponse, CompleteResponse, FunctionCall};
    use crate::tools::{NowTool, ReadImageTool, ToolRegistry, WriteFileTool};
    use std::sync::Mutex;

    struct Scripted {
        calls: Mutex<Vec<CompleteRequest>>,
        replies: Mutex<Vec<CompleteResponse>>,
        compact_calls: Mutex<Vec<CompactRequest>>,
        compact_ok: bool,
    }

    impl Provider for Scripted {
        async fn complete(&self, req: CompleteRequest) -> Result<CompleteResponse> {
            self.calls.lock().unwrap().push(req);
            self.replies
                .lock()
                .unwrap()
                .pop()
                .ok_or_else(|| Error::Provider("no scripted reply".into()))
        }

        async fn compact(&self, req: CompactRequest) -> Result<CompactResponse> {
            self.compact_calls.lock().unwrap().push(req);
            if self.compact_ok {
                Ok(CompactResponse {
                    item: json!({"type":"compaction","id":"cmp_test","encrypted_content":"blob"}),
                    dropped_message_count: 4,
                })
            } else {
                Err(Error::Provider("no compact endpoint".into()))
            }
        }
    }

    struct Rec(Mutex<Vec<AgentEvent>>);
    impl EventSink for Rec {
        fn emit(&self, event: &AgentEvent) {
            self.0.lock().unwrap().push(event.clone());
        }
    }

    fn pop_front(replies: Vec<CompleteResponse>) -> Vec<CompleteResponse> {
        replies.into_iter().rev().collect()
    }

    fn cfg(prompt: &str, max_turns: u32) -> RunConfig {
        RunConfig {
            agent_name: "root".into(),
            instructions: "use tools".into(),
            prompt: prompt.into(),
            model: "grok-4.6".into(),
            max_turns,
            server_tools: vec![],
            context_window: 0,
            compact_keep_recent: 0,
            parent_run_id: None,
            run_id: String::new(),
            reasoning_effort: crate::provider::ReasoningEffort::High,
            knobs: None,
            inbox: None,
            bg_notify: None,
            workspace: PathBuf::from("."),
            images: Vec::new(),
            closed_backgrounds: None,
            prior_history: Vec::new(),
            persist_history: None,
            cancel: None,
            skills: None,
            task: None,
            terminal_tool: None,
            hold: None,
            on_user_turn: None,
        }
    }

    #[test]
    fn dispatcher_knob_appends_dispatcher_instructions() {
        let mut c = cfg("go", 1);
        assert!(!live_instructions(&c).contains("Dispatcher mode"));
        c.knobs = Some(Arc::new(Mutex::new(SessionKnobs {
            model: "grok-4.6".into(),
            reasoning_effort: crate::provider::ReasoningEffort::High,
            send_reasoning: true,
            server_tools: vec![],
            dispatcher: true,
            child_model: String::new(),
            task_model: String::new(),
            task_reasoning_effort: crate::provider::ReasoningEffort::High,
            task_send_reasoning: true,
            search: false,
            ultracode: false,
            workflow_size: Default::default(),
        })));
        let text = live_instructions(&c);
        assert!(text.starts_with("use tools"));
        assert!(text.contains("Dispatcher mode"));
        c.knobs
            .as_ref()
            .unwrap()
            .lock()
            .unwrap()
            .dispatcher = false;
        assert!(!live_instructions(&c).contains("Dispatcher mode"));
    }

    #[tokio::test]
    async fn executes_client_tool_then_stops() {
        let provider = Scripted {
            calls: Mutex::new(Vec::new()),
            replies: Mutex::new(pop_front(vec![
                CompleteResponse {
                    function_calls: vec![FunctionCall {
                        call_id: "c1".into(),
                        name: "now".into(),
                        arguments: "{}".into(),
                    }],
                    ..CompleteResponse::new("1")
                },
                CompleteResponse {
                    text: "it is late".into(),
                    ..CompleteResponse::new("2")
                },
            ])),
            compact_calls: Mutex::new(Vec::new()),
            compact_ok: false,
        };
        let tools = ToolRegistry::new(vec![Box::new(NowTool)]);
        let rec = Rec(Mutex::new(Vec::new()));
        let provider = Arc::new(provider);
        let out = run(&provider, &tools, &rec, cfg("what time?", 4))
            .await
            .unwrap();
        assert_eq!(out.text, "it is late");
        assert_eq!(out.turns, 2);
        assert_eq!(out.compacted, 0);

        let events = rec.0.lock().unwrap().clone();
        let types: Vec<&str> = events
            .iter()
            .map(|e| match e {
                AgentEvent::RunStarted { .. } => "run_started",
                AgentEvent::TurnStarted { .. } => "turn_started",
                AgentEvent::ModelFinished { finish, .. } => {
                    if finish == "tool_calls" {
                        "model_tools"
                    } else {
                        "model_stop"
                    }
                }
                AgentEvent::ToolStarted { name, .. } => {
                    assert_eq!(name, "now");
                    "tool_started"
                }
                AgentEvent::ToolFinished { output, .. } => {
                    assert!(output.contains("utc"), "{output}");
                    "tool_finished"
                }
                AgentEvent::RunFinished { reason, text, .. } => {
                    assert_eq!(reason, "stop");
                    assert_eq!(text, "it is late");
                    "run_finished"
                }
                _ => "other",
            })
            .collect();
        assert_eq!(
            types,
            [
                "run_started",
                "turn_started",
                "model_tools",
                "tool_started",
                "tool_finished",
                "turn_started",
                "model_stop",
                "run_finished"
            ]
        );

        let calls = provider.calls.lock().unwrap();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].input[0]["content"], "what time?");
        assert_eq!(calls[0].cache_key, calls[1].cache_key);
        assert!(calls[0].cache_key.starts_with("grokaagent:v1:grok-4.6:"));
        assert!(
            calls[0].cache_key.ends_with(&format!(":{}", out.run_id)),
            "{}",
            calls[0].cache_key
        );
        assert_eq!(calls[0].previous_response_id, None);
        assert_eq!(calls[1].previous_response_id, None);
        assert!(
            calls[1].input.len() > calls[0].input.len(),
            "follow-up must resend the prefix plus the tool result, got {}",
            calls[1].input.len()
        );
        assert_eq!(
            calls[0].input[0], calls[1].input[0],
            "input prefix must stay byte-identical so prompt cache can hit"
        );
        assert_eq!(calls[1].input.last().unwrap()["type"], "function_call_output");
        let output = calls[1].input.last().unwrap()["output"].as_str().unwrap();
        assert!(output.contains("utc"), "{output}");
        assert!(!calls[0].store && !calls[1].store);
        assert!(
            calls[1]
                .input
                .iter()
                .any(|i| i.get("type").and_then(Value::as_str) == Some("function_call")),
            "function_call must be replayed in full history: {:?}",
            calls[1].input
        );
    }

    #[tokio::test]
    async fn followup_replays_encrypted_reasoning_in_the_prefix() {
        let provider = Scripted {
            calls: Mutex::new(Vec::new()),
            replies: Mutex::new(pop_front(vec![
                CompleteResponse {
                    function_calls: vec![FunctionCall {
                        call_id: "c1".into(),
                        name: "now".into(),
                        arguments: "{}".into(),
                    }],
                    output_items: vec![
                        json!({
                            "type": "reasoning",
                            "encrypted_content": "blob-turn-1",
                            "summary": [{"type": "summary_text", "text": "check the clock"}]
                        }),
                        json!({
                            "type": "function_call",
                            "call_id": "c1",
                            "name": "now",
                            "arguments": "{}"
                        }),
                    ],
                    ..CompleteResponse::new("1")
                },
                CompleteResponse {
                    text: "late".into(),
                    ..CompleteResponse::new("2")
                },
            ])),
            compact_calls: Mutex::new(Vec::new()),
            compact_ok: false,
        };
        let tools = ToolRegistry::new(vec![Box::new(NowTool)]);
        let rec = Rec(Mutex::new(Vec::new()));
        let provider = Arc::new(provider);
        run(&provider, &tools, &rec, cfg("what time?", 4))
            .await
            .unwrap();
        let calls = provider.calls.lock().unwrap();
        assert_eq!(calls[0].input[0], calls[1].input[0]);
        let reasoning = calls[1]
            .input
            .iter()
            .find(|i| i.get("type").and_then(Value::as_str) == Some("reasoning"))
            .expect("encrypted reasoning must stay in the prefix");
        assert_eq!(reasoning["encrypted_content"], "blob-turn-1");
        assert_eq!(
            calls[1]
                .input
                .iter()
                .filter(|i| i.get("type").and_then(Value::as_str) == Some("function_call"))
                .count(),
            1,
            "must not duplicate the function_call item: {:?}",
            calls[1].input
        );
    }

    #[tokio::test]
    async fn an_accepted_terminal_tool_call_ends_the_run_with_its_result() {
        let submit = |id: &str, args: &str| CompleteResponse {
            function_calls: vec![FunctionCall {
                call_id: id.into(),
                name: "submit_result".into(),
                arguments: args.into(),
            }],
            usage: CacheUsage { input_tokens: 100, cached_tokens: 0, output_tokens: 7 },
            ..CompleteResponse::new(id)
        };
        // A misfit first: the error goes back, the model fixes it, and the
        // accepted call ends the run without another model turn.
        let provider = Arc::new(Scripted {
            calls: Mutex::new(Vec::new()),
            replies: Mutex::new(pop_front(vec![submit("c1", r#"{"n":"x"}"#), submit("c2", r#"{"n":3}"#)])),
            compact_calls: Mutex::new(Vec::new()),
            compact_ok: false,
        });
        let schema = json!({"type": "object", "properties": {"n": {"type": "integer"}}, "required": ["n"]});
        let tools = ToolRegistry::new(vec![Box::new(crate::workflow::submit::SubmitResultTool::new(schema))]);
        let rec = Rec(Mutex::new(Vec::new()));
        let mut c = cfg("count", 8);
        c.terminal_tool = Some("submit_result".into());
        let out = run(&provider, &tools, &rec, c).await.unwrap();
        assert_eq!(serde_json::from_str::<Value>(&out.text).unwrap(), json!({"n": 3}));
        assert_eq!(provider.calls.lock().unwrap().len(), 2);
        let second = &provider.calls.lock().unwrap()[1];
        assert!(second.input.iter().any(|i| i.to_string().contains("does not match the schema")));
        let events = rec.0.lock().unwrap();
        let stops: Vec<(&String, u32)> = events
            .iter()
            .filter_map(|e| match e {
                AgentEvent::ModelFinished { text, finish, output_tokens, .. } if finish == "stop" => Some((text, *output_tokens)),
                _ => None,
            })
            .collect();
        // The stop carries the result and no tokens of its own (they were
        // counted with the tool-call turn).
        assert_eq!(stops.len(), 1);
        assert_eq!(stops[0].1, 0);
        assert!(stops[0].0.contains("\"n\":3"));
    }

    #[tokio::test]
    async fn a_held_one_shot_run_waits_for_background_news_before_it_ends() {
        let provider = Arc::new(Scripted {
            calls: Mutex::new(Vec::new()),
            replies: Mutex::new(pop_front(vec![
                CompleteResponse { text: "started it".into(), ..CompleteResponse::new("1") },
                CompleteResponse { text: "it finished".into(), ..CompleteResponse::new("2") },
            ])),
            compact_calls: Mutex::new(Vec::new()),
            compact_ok: false,
        });
        let tools = ToolRegistry::new(vec![Box::new(NowTool)]);
        let rec = Rec(Mutex::new(Vec::new()));
        let (tx, rx) = mpsc::unbounded_channel();
        let pending = Arc::new(std::sync::atomic::AtomicBool::new(true));
        let mut c = cfg("go", 0);
        c.bg_notify = Some(rx);
        let flag = pending.clone();
        c.hold = Some(Arc::new(move || flag.load(std::sync::atomic::Ordering::SeqCst)));
        let seen = Arc::new(Mutex::new(Vec::<String>::new()));
        let log = seen.clone();
        c.on_user_turn = Some(Arc::new(move |t: &str| log.lock().unwrap().push(t.to_string())));
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(100)).await;
            pending.store(false, std::sync::atomic::Ordering::SeqCst);
            let _ = tx.send(UserTurn::from(format!("{}wf-1 finished] done", crate::workflow::NOTICE_PREFIX)));
        });
        let out = tokio::time::timeout(Duration::from_secs(5), run(&provider, &tools, &rec, c))
            .await
            .expect("the run must end once the held work is done")
            .unwrap();
        assert_eq!(out.text, "it finished");
        assert_eq!(provider.calls.lock().unwrap().len(), 2);
        // The first prompt counts as the user's; the notice does not.
        assert_eq!(seen.lock().unwrap().as_slice(), ["go"]);
    }

    #[tokio::test]
    async fn a_hold_that_lets_go_without_a_message_still_ends_the_run() {
        let provider = Arc::new(Scripted {
            calls: Mutex::new(Vec::new()),
            replies: Mutex::new(pop_front(vec![CompleteResponse { text: "stopped it".into(), ..CompleteResponse::new("1") }])),
            compact_calls: Mutex::new(Vec::new()),
            compact_ok: false,
        });
        let tools = ToolRegistry::new(vec![Box::new(NowTool)]);
        let rec = Rec(Mutex::new(Vec::new()));
        let (_tx, rx) = mpsc::unbounded_channel::<UserTurn>();
        let pending = Arc::new(std::sync::atomic::AtomicBool::new(true));
        let mut c = cfg("go", 0);
        c.bg_notify = Some(rx);
        let flag = pending.clone();
        c.hold = Some(Arc::new(move || flag.load(std::sync::atomic::Ordering::SeqCst)));
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(100)).await;
            pending.store(false, std::sync::atomic::Ordering::SeqCst);
        });
        let out = tokio::time::timeout(Duration::from_secs(3), run(&provider, &tools, &rec, c))
            .await
            .expect("the run must notice the hold let go")
            .unwrap();
        assert_eq!(out.text, "stopped it");
    }

    #[tokio::test]
    async fn every_message_the_user_sends_reaches_the_user_turn_hook() {
        let provider = Arc::new(Scripted {
            calls: Mutex::new(Vec::new()),
            replies: Mutex::new(pop_front(vec![
                CompleteResponse { text: "one".into(), ..CompleteResponse::new("1") },
                CompleteResponse { text: "two".into(), ..CompleteResponse::new("2") },
            ])),
            compact_calls: Mutex::new(Vec::new()),
            compact_ok: false,
        });
        let tools = ToolRegistry::new(vec![Box::new(NowTool)]);
        let rec = Rec(Mutex::new(Vec::new()));
        let (tx, inbox) = mpsc::unbounded_channel::<UserTurn>();
        let mut c = cfg("go +100k", 0);
        c.inbox = Some(inbox);
        let seen = Arc::new(Mutex::new(Vec::<String>::new()));
        let log = seen.clone();
        c.on_user_turn = Some(Arc::new(move |t: &str| log.lock().unwrap().push(t.to_string())));
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(100)).await;
            let _ = tx.send(UserTurn::from("next +2m"));
            tokio::time::sleep(Duration::from_millis(200)).await;
            drop(tx);
        });
        tokio::time::timeout(Duration::from_secs(5), run(&provider, &tools, &rec, c))
            .await
            .expect("the session ends when its inbox closes")
            .unwrap();
        assert_eq!(seen.lock().unwrap().as_slice(), ["go +100k", "next +2m"]);
    }

    #[tokio::test]
    async fn separate_runs_get_separate_cache_keys() {
        async fn one_run(id: &str) -> String {
            let provider = Scripted {
                calls: Mutex::new(Vec::new()),
                replies: Mutex::new(pop_front(vec![CompleteResponse {
                    text: "ok".into(),
                    ..CompleteResponse::new("1")
                }])),
                compact_calls: Mutex::new(Vec::new()),
                compact_ok: false,
            };
            let tools = ToolRegistry::new(vec![Box::new(NowTool)]);
            let rec = Rec(Mutex::new(Vec::new()));
            let mut c = cfg("hi", 1);
            c.run_id = id.into();
            let provider = Arc::new(provider);
            super::run(&provider, &tools, &rec, c).await.unwrap();
            let key = provider.calls.lock().unwrap()[0].cache_key.clone();
            key
        }
        let a = one_run("run-aaa").await;
        let b = one_run("run-bbb").await;
        assert!(a.ends_with(":run-aaa"), "{a}");
        assert!(b.ends_with(":run-bbb"), "{b}");
        assert_ne!(a, b);
    }

    #[tokio::test]
    async fn low_cache_emits_notice() {
        let provider = Scripted {
            calls: Mutex::new(Vec::new()),
            replies: Mutex::new(pop_front(vec![
                CompleteResponse {
                    function_calls: vec![FunctionCall {
                        call_id: "c1".into(),
                        name: "now".into(),
                        arguments: "{}".into(),
                    }],
                    usage: CacheUsage {
                        input_tokens: 1000,
                        cached_tokens: 0, output_tokens: 0,
                    },
                    ..CompleteResponse::new("1")
                },
                CompleteResponse {
                    text: "ok".into(),
                    usage: CacheUsage {
                        input_tokens: 1000,
                        cached_tokens: 10, output_tokens: 0,
                    },
                    ..CompleteResponse::new("2")
                },
            ])),
            compact_calls: Mutex::new(Vec::new()),
            compact_ok: false,
        };
        let tools = ToolRegistry::new(vec![Box::new(NowTool)]);
        let rec = Rec(Mutex::new(Vec::new()));
        let provider = Arc::new(provider);
        run(&provider, &tools, &rec, cfg("what time?", 4))
            .await
            .unwrap();
        let notices: Vec<String> = rec
            .0
            .lock()
            .unwrap()
            .iter()
            .filter_map(|e| match e {
                AgentEvent::Notice { message, .. } => Some(message.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(notices.len(), 1, "{notices:?}");
        assert!(notices[0].contains("快取命中"), "{notices:?}");
        assert!(notices[0].contains("1%"), "{notices:?}");
    }

    #[tokio::test]
    async fn reasoning_from_output_items_is_emitted() {
        let provider = Scripted {
            calls: Mutex::new(Vec::new()),
            replies: Mutex::new(pop_front(vec![CompleteResponse {
                text: "42".into(),
                output_items: vec![
                    json!({
                        "type": "reasoning",
                        "summary": [{"type": "summary_text", "text": "conservation of energy"}]
                    }),
                    json!({
                        "type": "message",
                        "role": "assistant",
                        "content": [{"type": "output_text", "text": "42"}]
                    }),
                ],
                ..CompleteResponse::new("1")
            }])),
            compact_calls: Mutex::new(Vec::new()),
            compact_ok: false,
        };
        let tools = ToolRegistry::new(vec![Box::new(NowTool)]);
        let rec = Rec(Mutex::new(Vec::new()));
        let provider = Arc::new(provider);
        let out = run(&provider, &tools, &rec, cfg("why 42?", 2))
            .await
            .unwrap();
        assert_eq!(out.text, "42");
        let think: Vec<String> = rec
            .0
            .lock()
            .unwrap()
            .iter()
            .filter_map(|e| match e {
                AgentEvent::ReasoningDelta { text, .. } => Some(text.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(think, vec!["conservation of energy".to_string()]);
    }

    #[tokio::test]
    async fn write_file_emits_file_changed_with_diff() {
        let dir = tempfile::tempdir().unwrap();
        let provider = Scripted {
            calls: Mutex::new(Vec::new()),
            replies: Mutex::new(pop_front(vec![
                CompleteResponse {
                    function_calls: vec![FunctionCall {
                        call_id: "c1".into(),
                        name: "write_file".into(),
                        arguments: r#"{"path":"n.txt","contents":"hello\n"}"#.into(),
                    }],
                    ..CompleteResponse::new("1")
                },
                CompleteResponse {
                    text: "wrote it".into(),
                    ..CompleteResponse::new("2")
                },
            ])),
            compact_calls: Mutex::new(Vec::new()),
            compact_ok: false,
        };
        let tools = ToolRegistry::new(vec![Box::new(WriteFileTool::new(dir.path().to_path_buf()))]);
        let rec = Rec(Mutex::new(Vec::new()));
        let provider = Arc::new(provider);
        run(&provider, &tools, &rec, cfg("write n.txt", 4))
            .await
            .unwrap();
        let events = rec.0.lock().unwrap().clone();
        let diff = events.iter().find_map(|e| match e {
            AgentEvent::FileChanged { path, kind, diff, .. } => Some((path.as_str(), kind.as_str(), diff.as_str())),
            _ => None,
        });
        let (path, kind, diff) = diff.expect("FileChanged");
        assert_eq!(path, "n.txt");
        assert_eq!(kind, "create");
        assert!(diff.contains("+hello"), "{diff}");
        assert_eq!(
            std::fs::read_to_string(dir.path().join("n.txt")).unwrap(),
            "hello\n"
        );
    }

    #[tokio::test]
    async fn project_memory_is_not_in_context_until_the_model_reads() {
        let mem = tempfile::tempdir().unwrap();
        let ws = tempfile::tempdir().unwrap();
        crate::memory::MemoryStore::open_at(mem.path().to_path_buf())
            .slot(ws.path())
            .unwrap()
            .write("goal.md", "need A2A workers")
            .unwrap();
        let provider = Scripted {
            calls: Mutex::new(Vec::new()),
            replies: Mutex::new(pop_front(vec![
                CompleteResponse {
                    function_calls: vec![FunctionCall {
                        call_id: "c1".into(),
                        name: "project_memory".into(),
                        arguments: r#"{"action":"read","path":"goal.md"}"#.into(),
                    }],
                    ..CompleteResponse::new("1")
                },
                CompleteResponse {
                    text: "the goal is A2A".into(),
                    ..CompleteResponse::new("2")
                },
            ])),
            compact_calls: Mutex::new(Vec::new()),
            compact_ok: false,
        };
        let mut c = cfg("what next?", 4);
        c.workspace = ws.path().to_path_buf();
        let tools = ToolRegistry::new(vec![Box::new(ProjectMemoryTool::new(
            mem.path().to_path_buf(),
            ws.path().to_path_buf(),
        ))]);
        let rec = Rec(Mutex::new(Vec::new()));
        let provider = Arc::new(provider);
        let out = run(&provider, &tools, &rec, c).await.unwrap();
        assert_eq!(out.text, "the goal is A2A");
        let calls = provider.calls.lock().unwrap();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].input.len(), 1);
        assert_eq!(calls[0].input[0]["content"], "what next?");
        let first = calls[0].input[0].to_string();
        assert!(!first.contains("need A2A workers"), "{first}");
        let output = calls[1]
            .input
            .iter()
            .rev()
            .find_map(|i| i.get("output").and_then(Value::as_str))
            .expect("missing tool output");
        assert!(output.contains("need A2A workers"), "{output}");
        assert!(!ws.path().join("goal.md").exists());
    }

    #[tokio::test]
    async fn project_memory_write_stays_outside_workspace() {
        let mem = tempfile::tempdir().unwrap();
        let ws = tempfile::tempdir().unwrap();
        let provider = Scripted {
            calls: Mutex::new(Vec::new()),
            replies: Mutex::new(pop_front(vec![
                CompleteResponse {
                    function_calls: vec![FunctionCall {
                        call_id: "c1".into(),
                        name: "project_memory".into(),
                        arguments: r#"{"action":"write","path":"goal.md","body":"ship kernel"}"#.into(),
                    }],
                    ..CompleteResponse::new("1")
                },
                CompleteResponse {
                    text: "noted".into(),
                    ..CompleteResponse::new("2")
                },
            ])),
            compact_calls: Mutex::new(Vec::new()),
            compact_ok: false,
        };
        let mut c = cfg("remember the goal", 4);
        c.workspace = ws.path().to_path_buf();
        let tools = ToolRegistry::new(vec![Box::new(ProjectMemoryTool::new(
            mem.path().to_path_buf(),
            ws.path().to_path_buf(),
        ))]);
        let rec = Rec(Mutex::new(Vec::new()));
        let provider = Arc::new(provider);
        let out = run(&provider, &tools, &rec, c).await.unwrap();
        assert_eq!(out.text, "noted");
        assert!(!ws.path().join("goal.md").exists());
        let body = crate::memory::MemoryStore::open_at(mem.path().to_path_buf())
            .slot(ws.path())
            .unwrap()
            .read("goal.md")
            .unwrap();
        assert_eq!(body, "ship kernel");
        let dumped = std::fs::read_dir(ws.path()).unwrap().count();
        assert_eq!(dumped, 0);
    }

    #[tokio::test]
    async fn read_image_is_attached_on_next_turn() {
        let dir = tempfile::tempdir().unwrap();
        let img = crate::vision::from_rgba(32, 32, vec![255, 0, 0, 255].repeat(32 * 32)).unwrap();
        crate::vision::save_jpeg(&dir.path().join("p.jpg"), &img).unwrap();
        let provider = Scripted {
            calls: Mutex::new(Vec::new()),
            replies: Mutex::new(pop_front(vec![
                CompleteResponse {
                    function_calls: vec![FunctionCall {
                        call_id: "c1".into(),
                        name: "read_image".into(),
                        arguments: r#"{"path":"p.jpg"}"#.into(),
                    }],
                    ..CompleteResponse::new("1")
                },
                CompleteResponse {
                    text: "I see red".into(),
                    ..CompleteResponse::new("2")
                },
            ])),
            compact_calls: Mutex::new(Vec::new()),
            compact_ok: false,
        };
        let mut c = cfg("look at p.jpg", 4);
        c.workspace = dir.path().to_path_buf();
        let tools = ToolRegistry::new(vec![Box::new(ReadImageTool::new(c.workspace.clone()))]);
        let rec = Rec(Mutex::new(Vec::new()));
        let provider = Arc::new(provider);
        let out = run(&provider, &tools, &rec, c).await.unwrap();
        assert_eq!(out.text, "I see red");
        let calls = provider.calls.lock().unwrap();
        assert_eq!(calls.len(), 2);
        assert!(!calls[1].store, "xAI may reject stored image turns");
        assert_eq!(calls[1].previous_response_id, None);
        let item = calls[1]
            .input
            .iter()
            .find(|i| i["type"] == "function_call_output")
            .expect("missing function_call_output");
        let parts = item["output"]
            .as_array()
            .expect("image tool output must be content parts");
        assert_eq!(parts[0]["type"], "input_text");
        assert_eq!(parts[1]["type"], "input_image");
        assert_eq!(parts[1]["detail"], "high");
        let uri = parts[1]["image_url"].as_str().unwrap();
        assert!(uri.starts_with("data:image/jpeg;base64,"), "{uri}");
    }

    #[tokio::test]
    async fn attached_image_stays_for_a_few_turns_then_strips() {
        let dir = tempfile::tempdir().unwrap();
        let img = crate::vision::from_rgba(32, 32, vec![255, 0, 0, 255].repeat(32 * 32)).unwrap();
        crate::vision::save_jpeg(&dir.path().join("p.jpg"), &img).unwrap();
        let now = |id: &str| CompleteResponse {
            function_calls: vec![FunctionCall {
                call_id: id.into(),
                name: "now".into(),
                arguments: "{}".into(),
            }],
            ..CompleteResponse::new(id)
        };
        let provider = Scripted {
            calls: Mutex::new(Vec::new()),
            replies: Mutex::new(pop_front(vec![
                CompleteResponse {
                    function_calls: vec![FunctionCall {
                        call_id: "c1".into(),
                        name: "read_image".into(),
                        arguments: r#"{"path":"p.jpg"}"#.into(),
                    }],
                    ..CompleteResponse::new("1")
                },
                now("c2"),
                now("c3"),
                now("c4"),
                now("c5"),
                now("c6"),
                now("c7"),
                CompleteResponse {
                    text: "done".into(),
                    ..CompleteResponse::new("8")
                },
            ])),
            compact_calls: Mutex::new(Vec::new()),
            compact_ok: false,
        };
        let mut c = cfg("look at p.jpg", 10);
        c.workspace = dir.path().to_path_buf();
        let tools = ToolRegistry::new(vec![
            Box::new(ReadImageTool::new(c.workspace.clone())),
            Box::new(NowTool),
        ]);
        let rec = Rec(Mutex::new(Vec::new()));
        let provider = Arc::new(provider);
        let out = run(&provider, &tools, &rec, c).await.unwrap();
        assert_eq!(out.text, "done");
        let calls = provider.calls.lock().unwrap();
        assert_eq!(calls.len(), 8);
        assert!(
            !crate::vision::input_has_image(&calls[0].input),
            "pixels attach after the tool returns"
        );
        for i in 1..=6 {
            assert!(
                crate::vision::input_has_image(&calls[i].input),
                "look {i} must still include pixels"
            );
        }
        assert!(
            !crate::vision::input_has_image(&calls[7].input),
            "pixels expire after {} completes: {:?}",
            crate::vision::IMAGE_KEEP_TURNS,
            calls[7].input
        );
    }

    #[tokio::test]
    async fn image_payload_does_not_retrigger_compact_every_tool_turn() {
        struct FoldAware {
            inner: Scripted,
            folds: Mutex<u32>,
        }
        impl Provider for FoldAware {
            async fn complete(&self, req: CompleteRequest) -> Result<CompleteResponse> {
                let fold = req.input.iter().any(|i| {
                    i.get("content")
                        .and_then(Value::as_str)
                        .is_some_and(|s| s.contains(MEMORY_FOLD_MARK))
                });
                if fold {
                    *self.folds.lock().unwrap() += 1;
                    self.inner.calls.lock().unwrap().push(req);
                    return Ok(CompleteResponse {
                        text: "## Original request\nlook\n## Direction changes\nnone\n## Load-bearing facts\nnone\n## Where you were\nlooking\n".into(),
                        ..CompleteResponse::new("mem")
                    });
                }
                self.inner.complete(req).await
            }
            async fn compact(&self, req: CompactRequest) -> Result<CompactResponse> {
                self.inner.compact(req).await
            }
        }
        let dir = tempfile::tempdir().unwrap();
        let mut px = Vec::with_capacity(320 * 320 * 4);
        for i in 0..(320 * 320) {
            px.extend_from_slice(&[
                (i * 17) as u8,
                (i * 31) as u8,
                (i * 13) as u8,
                255,
            ]);
        }
        let img = crate::vision::from_rgba(320, 320, px).unwrap();
        crate::vision::save_jpeg(&dir.path().join("p.jpg"), &img).unwrap();
        let pad = vec![
            json!({"role":"user","content":"pad-1"}),
            json!({"role":"user","content":"pad-2"}),
            json!({"role":"user","content":"pad-3"}),
            json!({"role":"user","content":"pad-4"}),
            json!({"role":"user","content":"pad-5"}),
        ];
        let provider = FoldAware {
            inner: Scripted {
                calls: Mutex::new(Vec::new()),
                replies: Mutex::new(pop_front(vec![
                    CompleteResponse {
                        function_calls: vec![FunctionCall {
                            call_id: "c1".into(),
                            name: "read_image".into(),
                            arguments: r#"{"path":"p.jpg"}"#.into(),
                        }],
                        usage: CacheUsage {
                            input_tokens: 80,
                            cached_tokens: 0, output_tokens: 0,
                        },
                        output_items: {
                            let mut v = pad.clone();
                            v.push(json!({"type":"function_call","call_id":"c1","name":"read_image","arguments":r#"{"path":"p.jpg"}"#}));
                            v
                        },
                        ..CompleteResponse::new("1")
                    },
                    CompleteResponse {
                        function_calls: vec![FunctionCall {
                            call_id: "c2".into(),
                            name: "now".into(),
                            arguments: "{}".into(),
                        }],
                        usage: CacheUsage {
                            input_tokens: 90,
                            cached_tokens: 0, output_tokens: 0,
                        },
                        ..CompleteResponse::new("2")
                    },
                    CompleteResponse {
                        function_calls: vec![FunctionCall {
                            call_id: "c3".into(),
                            name: "now".into(),
                            arguments: "{}".into(),
                        }],
                        usage: CacheUsage {
                            input_tokens: 90,
                            cached_tokens: 0, output_tokens: 0,
                        },
                        ..CompleteResponse::new("3")
                    },
                    CompleteResponse {
                        text: "done".into(),
                        usage: CacheUsage {
                            input_tokens: 90,
                            cached_tokens: 0, output_tokens: 0,
                        },
                        ..CompleteResponse::new("4")
                    },
                ])),
                compact_calls: Mutex::new(Vec::new()),
                compact_ok: true,
            },
            folds: Mutex::new(0),
        };
        let mut c = cfg("look at p.jpg", 8);
        c.workspace = dir.path().to_path_buf();
        c.context_window = 4_000;
        c.compact_keep_recent = 2;
        let tools = ToolRegistry::new(vec![
            Box::new(ReadImageTool::new(c.workspace.clone())),
            Box::new(NowTool),
        ]);
        let rec = Rec(Mutex::new(Vec::new()));
        let provider = Arc::new(provider);
        let out = run(&provider, &tools, &rec, c).await.unwrap();
        assert_eq!(out.text, "done");
        assert_eq!(
            *provider.folds.lock().unwrap(),
            0,
            "image bytes must not trip the 50% trigger when provider usage stays low"
        );
        assert_eq!(out.compacted, 0);
    }

    #[tokio::test]
    async fn read_image_tiny_file_tells_the_model_instead_of_attaching() {
        let dir = tempfile::tempdir().unwrap();
        let img = crate::vision::from_rgba(16, 16, vec![255, 0, 0, 255].repeat(16 * 16)).unwrap();
        crate::vision::save_jpeg(&dir.path().join("star.jpg"), &img).unwrap();
        let provider = Scripted {
            calls: Mutex::new(Vec::new()),
            replies: Mutex::new(pop_front(vec![
                CompleteResponse {
                    function_calls: vec![FunctionCall {
                        call_id: "c1".into(),
                        name: "read_image".into(),
                        arguments: r#"{"path":"star.jpg"}"#.into(),
                    }],
                    ..CompleteResponse::new("1")
                },
                CompleteResponse {
                    text: "I will enlarge it".into(),
                    ..CompleteResponse::new("2")
                },
            ])),
            compact_calls: Mutex::new(Vec::new()),
            compact_ok: false,
        };
        let mut c = cfg("look at star.jpg", 4);
        c.workspace = dir.path().to_path_buf();
        let tools = ToolRegistry::new(vec![Box::new(ReadImageTool::new(c.workspace.clone()))]);
        let rec = Rec(Mutex::new(Vec::new()));
        let provider = Arc::new(provider);
        let out = run(&provider, &tools, &rec, c).await.unwrap();
        assert_eq!(out.text, "I will enlarge it");
        let calls = provider.calls.lock().unwrap();
        let item = calls[1]
            .input
            .iter()
            .find(|i| i["type"] == "function_call_output")
            .expect("missing function_call_output");
        let text = item["output"].as_str().expect("tiny image must stay JSON text, not parts");
        assert!(text.contains("512"), "{text}");
        assert!(text.contains("attach_image"), "{text}");
        assert!(!crate::vision::input_has_image(&calls[1].input));
    }

    #[tokio::test]
    async fn provider_error_tells_the_model_and_continues() {
        struct FailThenOk {
            n: Mutex<u32>,
            calls: Mutex<Vec<CompleteRequest>>,
            replies: Mutex<Vec<CompleteResponse>>,
        }
        impl Provider for FailThenOk {
            async fn complete(&self, req: CompleteRequest) -> Result<CompleteResponse> {
                self.calls.lock().unwrap().push(req);
                let n = {
                    let mut g = self.n.lock().unwrap();
                    *g += 1;
                    *g
                };
                if n == 1 {
                    return Err(Error::Provider(
                        "xAI HTTP 400: invalid_image below 512 pixels".into(),
                    ));
                }
                self.replies
                    .lock()
                    .unwrap()
                    .pop()
                    .ok_or_else(|| Error::Provider("no reply".into()))
            }
            async fn compact(&self, _req: CompactRequest) -> Result<CompactResponse> {
                Err(Error::Provider("no compact".into()))
            }
        }
        let provider = FailThenOk {
            n: Mutex::new(0),
            calls: Mutex::new(Vec::new()),
            replies: Mutex::new(vec![CompleteResponse {
                text: "I will enlarge the image".into(),
                ..CompleteResponse::new("2")
            }]),
        };
        let tools = ToolRegistry::new(vec![Box::new(NowTool)]);
        let rec = Rec(Mutex::new(Vec::new()));
        let provider = Arc::new(provider);
        let out = run(&provider, &tools, &rec, cfg("look", 4)).await.unwrap();
        assert_eq!(out.text, "I will enlarge the image");
        assert_eq!(out.turns, 2);
        let events = rec.0.lock().unwrap().clone();
        assert!(events.iter().any(|e| matches!(e, AgentEvent::Error { .. })));
        assert!(!events
            .iter()
            .any(|e| matches!(e, AgentEvent::AwaitingInput { .. })));
        assert!(!events.iter().any(|e| matches!(
            e,
            AgentEvent::RunFinished { reason, .. } if reason == "error"
        )));
        let calls = provider.calls.lock().unwrap();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[1].previous_response_id, None);
        let blob = calls[1].input.iter().map(|v| v.to_string()).collect::<Vec<_>>().join("\n");
        assert!(blob.contains("invalid_image"), "{blob}");
        assert!(blob.contains("512"), "{blob}");
        assert!(!crate::vision::input_has_image(&calls[1].input));
    }

    #[tokio::test]
    async fn first_user_turn_can_attach_images() {
        let dir = tempfile::tempdir().unwrap();
        let img = crate::vision::from_rgba(32, 32, vec![0, 255, 0, 255].repeat(32 * 32)).unwrap();
        crate::vision::save_jpeg(&dir.path().join("a.jpg"), &img).unwrap();
        let provider = Scripted {
            calls: Mutex::new(Vec::new()),
            replies: Mutex::new(pop_front(vec![CompleteResponse {
                text: "green square".into(),
                ..CompleteResponse::new("1")
            }])),
            compact_calls: Mutex::new(Vec::new()),
            compact_ok: false,
        };
        let mut c = cfg("what is this", 2);
        c.workspace = dir.path().to_path_buf();
        c.images = vec![PathBuf::from("a.jpg")];
        let tools = ToolRegistry::new(vec![Box::new(NowTool)]);
        let rec = Rec(Mutex::new(Vec::new()));
        let provider = Arc::new(provider);
        let out = run(&provider, &tools, &rec, c).await.unwrap();
        assert_eq!(out.text, "green square");
        let calls = provider.calls.lock().unwrap();
        assert_eq!(calls.len(), 1);
        assert!(!calls[0].store);
        assert_eq!(calls[0].previous_response_id, None);
        let parts = calls[0].input[0]["content"]
            .as_array()
            .expect("user content must be parts when images are attached");
        assert_eq!(parts[0]["type"], "input_text");
        assert_eq!(parts[1]["type"], "input_image");
        assert!(parts[1]["image_url"]
            .as_str()
            .unwrap()
            .starts_with("data:image/jpeg;base64,"));
    }

    #[tokio::test]
    async fn max_turns_emits_error_and_fails() {
        let provider = Scripted {
            calls: Mutex::new(Vec::new()),
            replies: Mutex::new(pop_front(vec![CompleteResponse {
                function_calls: vec![FunctionCall {
                    call_id: "c1".into(),
                    name: "now".into(),
                    arguments: "{}".into(),
                }],
                ..CompleteResponse::new("1")
            }])),
            compact_calls: Mutex::new(Vec::new()),
            compact_ok: false,
        };
        let tools = ToolRegistry::new(vec![Box::new(NowTool)]);
        let rec = Rec(Mutex::new(Vec::new()));
        let provider = Arc::new(provider);
        let err = run(&provider, &tools, &rec, cfg("loop", 1))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("max turns"));
        let events = rec.0.lock().unwrap();
        assert!(events
            .iter()
            .any(|e| matches!(e, AgentEvent::Error { .. })));
        assert!(events.iter().any(|e| matches!(
            e,
            AgentEvent::RunFinished { reason, .. } if reason == "max_turns"
        )));
    }

    #[tokio::test]
    async fn zero_max_turns_keeps_going_past_eight() {
        let mut replies = Vec::new();
        for i in 0..9 {
            replies.push(CompleteResponse {
                function_calls: vec![FunctionCall {
                    call_id: format!("c{i}"),
                    name: "now".into(),
                    arguments: "{}".into(),
                }],
                ..CompleteResponse::new(format!("{i}"))
            });
        }
        replies.push(CompleteResponse {
            text: "done after 10".into(),
            ..CompleteResponse::new("end")
        });
        let provider = Scripted {
            calls: Mutex::new(Vec::new()),
            replies: Mutex::new(pop_front(replies)),
            compact_calls: Mutex::new(Vec::new()),
            compact_ok: false,
        };
        let tools = ToolRegistry::new(vec![Box::new(NowTool)]);
        let rec = Rec(Mutex::new(Vec::new()));
        let provider = Arc::new(provider);
        let out = run(&provider, &tools, &rec, cfg("loop", 0))
            .await
            .unwrap();
        assert_eq!(out.text, "done after 10");
        assert_eq!(out.turns, 10);
        assert!(!rec.0.lock().unwrap().iter().any(|e| matches!(
            e,
            AgentEvent::RunFinished { reason, .. } if reason == "max_turns"
        )));
    }

    #[tokio::test]
    async fn inbox_max_turns_pauses_instead_of_killing() {
        let (tx, rx) = mpsc::unbounded_channel();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(30)).await;
            let _ = tx.send("keep going".into());
        });
        let provider = Scripted {
            calls: Mutex::new(Vec::new()),
            replies: Mutex::new(pop_front(vec![
                CompleteResponse {
                    function_calls: vec![FunctionCall {
                        call_id: "c1".into(),
                        name: "now".into(),
                        arguments: "{}".into(),
                    }],
                    ..CompleteResponse::new("1")
                },
                CompleteResponse {
                    text: "continued".into(),
                    ..CompleteResponse::new("2")
                },
            ])),
            compact_calls: Mutex::new(Vec::new()),
            compact_ok: false,
        };
        let mut c = cfg("loop", 1);
        c.inbox = Some(rx);
        let tools = ToolRegistry::new(vec![Box::new(NowTool)]);
        let rec = Rec(Mutex::new(Vec::new()));
        let provider = Arc::new(provider);
        let out = run(&provider, &tools, &rec, c).await.unwrap();
        assert_eq!(out.text, "continued");
        let events = rec.0.lock().unwrap().clone();
        assert!(events.iter().any(|e| matches!(e, AgentEvent::AwaitingInput { .. })));
        assert!(events.iter().any(|e| matches!(
            e,
            AgentEvent::RunFinished { reason, .. } if reason == "stop"
        )));
        assert!(!events.iter().any(|e| matches!(
            e,
            AgentEvent::RunFinished { reason, .. } if reason == "max_turns"
        )));
    }

    #[tokio::test]
    async fn unknown_tool_returns_error_payload_to_model() {
        let provider = Scripted {
            calls: Mutex::new(Vec::new()),
            replies: Mutex::new(pop_front(vec![
                CompleteResponse {
                    function_calls: vec![FunctionCall {
                        call_id: "c1".into(),
                        name: "missing".into(),
                        arguments: "{}".into(),
                    }],
                    ..CompleteResponse::new("1")
                },
                CompleteResponse {
                    text: "ok".into(),
                    ..CompleteResponse::new("2")
                },
            ])),
            compact_calls: Mutex::new(Vec::new()),
            compact_ok: false,
        };
        let tools = ToolRegistry::new(vec![Box::new(NowTool)]);
        let rec = Rec(Mutex::new(Vec::new()));
        let provider = Arc::new(provider);
        run(&provider, &tools, &rec, cfg("x", 3)).await.unwrap();
        let calls = provider.calls.lock().unwrap();
        let output = calls[1]
            .input
            .iter()
            .rev()
            .find_map(|i| i.get("output").and_then(Value::as_str))
            .expect("missing tool output");
        assert!(output.contains("unknown tool"), "{output}");
    }

    #[tokio::test]
    async fn compact_at_half_window_keeps_recent_raw_and_breaks_chain() {
        let first = CompleteResponse {
            function_calls: vec![FunctionCall {
                call_id: "c1".into(),
                name: "now".into(),
                arguments: "{}".into(),
            }],
            usage: CacheUsage {
                input_tokens: 6_000,
                cached_tokens: 0, output_tokens: 0,
            },
            output_items: vec![
                json!({"role":"user","content":"pad-1"}),
                json!({"role":"user","content":"pad-2"}),
                json!({"role":"user","content":"pad-3"}),
                json!({"role":"user","content":"pad-4"}),
                json!({"role":"user","content":"pad-5"}),
                json!({"type":"function_call","call_id":"c1","name":"now","arguments":"{}"}),
            ],
            ..CompleteResponse::new("1")
        };
        let provider = Scripted {
            calls: Mutex::new(Vec::new()),
            replies: Mutex::new(pop_front(vec![
                first,
                CompleteResponse {
                    text: "## Original request\nship the kernel\n## Direction changes\nnone\n## Load-bearing facts\nkeep the ABI stable\n## Where you were\nporting the scheduler\n".into(),
                    ..CompleteResponse::new("mem")
                },
                CompleteResponse {
                    text: "done".into(),
                    usage: CacheUsage {
                        input_tokens: 20,
                        cached_tokens: 18, output_tokens: 0,
                    },
                    ..CompleteResponse::new("2")
                },
            ])),
            compact_calls: Mutex::new(Vec::new()),
            compact_ok: true,
        };
        let tools = ToolRegistry::new(vec![Box::new(NowTool)]);
        let rec = Rec(Mutex::new(Vec::new()));
        let mut config = cfg("ship the kernel", 4);
        config.context_window = 10_000;
        config.compact_keep_recent = 2;
        let provider = Arc::new(provider);
        let out = run(&provider, &tools, &rec, config).await.unwrap();
        assert_eq!(out.text, "done");
        assert_eq!(out.compacted, 1);

        let events = rec.0.lock().unwrap().clone();
        assert!(events.iter().any(|e| matches!(
            e,
            AgentEvent::ContextCompacted { method, kept_items, .. }
                if method == "lived" && *kept_items >= 2
        )));

        let calls = provider.calls.lock().unwrap();
        assert_eq!(calls.len(), 3);
        assert_eq!(calls[0].cache_key, calls[1].cache_key);
        assert_eq!(calls[1].cache_key, calls[2].cache_key);
        assert!(!calls[1].client_tools.is_empty());
        assert_eq!(calls[1].tool_choice.as_deref(), Some("none"));
        assert!(!calls[1].store);
        assert_eq!(
            calls[1].previous_response_id, None,
            "memory-fold must not chain off the function-call response"
        );
        assert!(
            calls[1].input.len() > 1,
            "memory-fold must include the folded head, got {}",
            calls[1].input.len()
        );
        assert_ne!(
            calls[1].input[calls[1].input.len() - 2]
                .get("type")
                .and_then(Value::as_str),
            Some("function_call"),
            "fold must not follow function_call with the memory-ask user"
        );
        let mem_ask = calls[1].input.last().unwrap()["content"].as_str().unwrap();
        assert!(mem_ask.contains(MEMORY_FOLD_MARK), "{mem_ask}");
        assert!(mem_ask.contains("not a new user task"), "{mem_ask}");
        assert_eq!(calls[2].previous_response_id, None);
        assert_ne!(calls[2].input[0].get("type").and_then(Value::as_str), Some("compaction"));
        let contents: Vec<String> = calls[2]
            .input
            .iter()
            .filter_map(|i| i.get("content").and_then(Value::as_str).map(str::to_string))
            .collect();
        assert!(
            contents.iter().any(|c| c.contains("ship the kernel") && c.contains(COMPACT_MARK)),
            "{contents:?}"
        );
        assert!(
            contents.iter().any(|c| c.contains("porting the scheduler")),
            "{contents:?}"
        );
        assert!(
            contents.iter().any(|c| c.contains("not a new user request")),
            "{contents:?}"
        );
        assert_eq!(calls[2].input.last().unwrap()["type"], "function_call_output");
        assert!(provider.compact_calls.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn lived_fold_closes_open_function_call_then_compacts() {
        struct XaiShape {
            inner: Scripted,
        }
        impl Provider for XaiShape {
            async fn complete(&self, req: CompleteRequest) -> Result<CompleteResponse> {
                let fold = req.input.iter().any(|i| {
                    i.get("content")
                        .and_then(Value::as_str)
                        .is_some_and(|s| s.contains(MEMORY_FOLD_MARK))
                });
                if fold {
                    let illegal = req.input.windows(2).any(|w| {
                        w[0].get("type").and_then(Value::as_str) == Some("function_call")
                            && w[1].get("role").and_then(Value::as_str) == Some("user")
                    });
                    self.inner.calls.lock().unwrap().push(req);
                    if illegal {
                        return Err(Error::Provider(
                            "cannot follow a function_call with a new user message".into(),
                        ));
                    }
                    return Ok(CompleteResponse {
                        text: "## Original request\nship the kernel\n## Direction changes\nnone\n## Load-bearing facts\nkeep the ABI stable\n## Where you were\nporting the scheduler\n".into(),
                        ..CompleteResponse::new("mem")
                    });
                }
                self.inner.complete(req).await
            }
            async fn compact(&self, req: CompactRequest) -> Result<CompactResponse> {
                self.inner.compact(req).await
            }
        }
        let first = CompleteResponse {
            function_calls: vec![FunctionCall {
                call_id: "c1".into(),
                name: "now".into(),
                arguments: "{}".into(),
            }],
            usage: CacheUsage {
                input_tokens: 6_000,
                cached_tokens: 0, output_tokens: 0,
            },
            output_items: vec![
                json!({"role":"user","content":"pad-1"}),
                json!({"role":"user","content":"pad-2"}),
                json!({"role":"user","content":"pad-3"}),
                json!({"role":"user","content":"pad-4"}),
                json!({"type":"function_call","call_id":"orphan","name":"now","arguments":"{}"}),
                json!({"role":"user","content":"keep-in-tail"}),
                json!({"type":"function_call","call_id":"c1","name":"now","arguments":"{}"}),
            ],
            ..CompleteResponse::new("1")
        };
        let provider = XaiShape {
            inner: Scripted {
                calls: Mutex::new(Vec::new()),
                replies: Mutex::new(pop_front(vec![
                    first,
                    CompleteResponse {
                        text: "done".into(),
                        ..CompleteResponse::new("2")
                    },
                ])),
                compact_calls: Mutex::new(Vec::new()),
                compact_ok: false,
            },
        };
        let tools = ToolRegistry::new(vec![Box::new(NowTool)]);
        let rec = Rec(Mutex::new(Vec::new()));
        let mut config = cfg("ship the kernel", 4);
        config.context_window = 10_000;
        config.compact_keep_recent = 2;
        let provider = Arc::new(provider);
        let out = run(&provider, &tools, &rec, config).await.unwrap();
        assert_eq!(out.text, "done");
        assert_eq!(out.compacted, 1);
        let events = rec.0.lock().unwrap().clone();
        assert!(events.iter().any(|e| matches!(
            e,
            AgentEvent::ContextCompacted { method, .. } if method == "lived"
        )));
        assert!(!events.iter().any(|e| matches!(
            e,
            AgentEvent::Notice { message, .. } if message.contains("extractive")
        )));
        let calls = provider.inner.calls.lock().unwrap();
        let fold = calls
            .iter()
            .find(|c| {
                c.input.iter().any(|i| {
                    i.get("content")
                        .and_then(Value::as_str)
                        .is_some_and(|s| s.contains(MEMORY_FOLD_MARK))
                })
            })
            .expect("fold request");
        assert!(!fold.client_tools.is_empty());
        assert_ne!(
            fold.input[fold.input.len() - 2]
                .get("type")
                .and_then(Value::as_str),
            Some("function_call")
        );
        assert!(fold.input.iter().any(|i| {
            i.get("type").and_then(Value::as_str) == Some("function_call_output")
                && i.get("call_id").and_then(Value::as_str) == Some("orphan")
        }));
    }

    #[tokio::test]
    async fn lived_fold_gives_up_after_the_cap_and_still_answers() {
        struct FoldAlwaysFails {
            inner: Scripted,
            fold_attempts: Mutex<u32>,
        }
        impl Provider for FoldAlwaysFails {
            async fn complete(&self, req: CompleteRequest) -> Result<CompleteResponse> {
                let fold = req.input.iter().any(|i| {
                    i.get("content")
                        .and_then(Value::as_str)
                        .is_some_and(|s| s.contains(MEMORY_FOLD_MARK))
                });
                if fold {
                    *self.fold_attempts.lock().unwrap() += 1;
                    return Err(Error::Provider("auth expired".into()));
                }
                self.inner.complete(req).await
            }
            async fn compact(&self, req: CompactRequest) -> Result<CompactResponse> {
                self.inner.compact(req).await
            }
        }
        let first = CompleteResponse {
            function_calls: vec![FunctionCall {
                call_id: "c1".into(),
                name: "now".into(),
                arguments: "{}".into(),
            }],
            usage: CacheUsage {
                input_tokens: 6_000,
                cached_tokens: 0, output_tokens: 0,
            },
            output_items: vec![
                json!({"role":"user","content":"pad-1"}),
                json!({"role":"user","content":"pad-2"}),
                json!({"role":"user","content":"pad-3"}),
                json!({"role":"user","content":"pad-4"}),
                json!({"role":"user","content":"pad-5"}),
                json!({"type":"function_call","call_id":"c1","name":"now","arguments":"{}"}),
            ],
            ..CompleteResponse::new("1")
        };
        let provider = FoldAlwaysFails {
            inner: Scripted {
                calls: Mutex::new(Vec::new()),
                replies: Mutex::new(pop_front(vec![
                    first,
                    CompleteResponse {
                        text: "done".into(),
                        ..CompleteResponse::new("2")
                    },
                ])),
                compact_calls: Mutex::new(Vec::new()),
                compact_ok: false,
            },
            fold_attempts: Mutex::new(0),
        };
        let tools = ToolRegistry::new(vec![Box::new(NowTool)]);
        let rec = Rec(Mutex::new(Vec::new()));
        let mut config = cfg("ship the kernel", 4);
        config.context_window = 10_000;
        config.compact_keep_recent = 2;
        let provider = Arc::new(provider);
        let out = tokio::time::timeout(Duration::from_secs(10), run(&provider, &tools, &rec, config))
            .await
            .expect("a failing fold must not spin forever")
            .unwrap();
        assert_eq!(out.text, "done");
        assert_eq!(out.compacted, 0);
        assert_eq!(*provider.fold_attempts.lock().unwrap(), compact::FOLD_ATTEMPTS);
        assert!(rec.0.lock().unwrap().iter().any(|e| matches!(
            e,
            AgentEvent::Notice { message, .. } if message.contains("壓縮失敗")
        )));
    }

    #[test]
    fn a_splice_near_the_start_point_gives_way_unless_a_fold_cannot_do_better() {
        // 128K: target 28,160, start 51,200 (halfway 39,680), trigger 64,000.
        let b = compact::Budget::new(128_000);
        // Landed at most halfway to the start point: keep it.
        assert!(splice_is_good_enough(35_000, Some(20_000), &b));
        // Past halfway, and a fresh fold gets much lower: the next fold (and
        // cache miss) would follow within a few turns, so refuse.
        assert!(!splice_is_good_enough(45_000, Some(25_000), &b));
        assert!(!splice_is_good_enough(57_600, Some(30_000), &b));
        // A fresh fold could not get much lower (fixed costs): keep it.
        assert!(splice_is_good_enough(45_000, Some(42_000), &b));
        assert!(splice_is_good_enough(57_600, Some(54_000), &b));
        // Nothing left to fold: keep it.
        assert!(splice_is_good_enough(60_000, None, &b));
        // At or over the trigger while a fresh fold gets under it: refuse,
        // or the next turn folds again.
        assert!(!splice_is_good_enough(64_500, Some(60_000), &b));
        // Over the trigger and a fresh fold would be too: keep it.
        assert!(splice_is_good_enough(66_000, Some(64_000), &b));
    }

    #[tokio::test]
    async fn a_background_fold_is_taken_only_when_finished_and_only_while_the_history_before_it_is_untouched() {
        let folded = || compact::Folded { items: vec![json!({"role":"user","content":"m"})], dropped: 1, kept: 0, method: "lived".into() };
        let history = vec![json!({"role":"user","content":"a"}), json!({"role":"assistant","content":"b"})];
        let (go, wait) = tokio::sync::oneshot::channel::<()>();
        let mut slot = Some(BgFold {
            task: tokio::spawn(async move {
                let _ = wait.await;
                Some(folded())
            }),
            seen: 2,
            boundary: history.last().cloned(),
        });
        assert!(take_finished(&mut slot, &history).is_none(), "still running: left in place");
        assert!(slot.is_some());
        go.send(()).unwrap();
        while !slot.as_ref().unwrap().task.is_finished() {
            tokio::task::yield_now().await;
        }
        let mut grown = history.clone();
        grown.push(json!({"role":"user","content":"c"}));
        assert!(
            matches!(take_finished(&mut slot, &grown), Some(Finished::Ready(2, _))),
            "appended after it: still valid"
        );
        assert!(slot.is_none());

        // A fold that failed is told apart from one that went stale.
        let mut slot = Some(BgFold { task: tokio::spawn(async { None }), seen: 2, boundary: history.last().cloned() });
        while !slot.as_ref().unwrap().task.is_finished() {
            tokio::task::yield_now().await;
        }
        assert!(matches!(take_finished(&mut slot, &history), Some(Finished::Failed)));

        // Rewritten before `seen`: not usable.
        let mut slot = Some(BgFold { task: tokio::spawn(async move { Some(folded()) }), seen: 2, boundary: history.last().cloned() });
        while !slot.as_ref().unwrap().task.is_finished() {
            tokio::task::yield_now().await;
        }
        let mut rewritten = history.clone();
        rewritten[1] = json!({"role":"assistant","content":"changed"});
        assert!(matches!(take_finished(&mut slot, &rewritten), Some(Finished::Stale)));

        // Dropped unused: its task stops.
        let (alive, gone) = tokio::sync::oneshot::channel::<()>();
        let bg = BgFold {
            task: tokio::spawn(async move {
                tokio::time::sleep(Duration::from_secs(60)).await;
                let _ = alive.send(());
                None
            }),
            seen: 0,
            boundary: None,
        };
        tokio::task::yield_now().await;
        drop(bg);
        assert!(tokio::time::timeout(Duration::from_secs(5), gone).await.unwrap().is_err(), "aborted before it could send");
    }

    /// A tool whose output is big and slims well.
    struct BigLog;
    impl crate::tools::ClientTool for BigLog {
        fn spec(&self) -> crate::tools::ToolSpec {
            crate::tools::ToolSpec {
                name: "big_log".into(),
                description: "Print a long log.".into(),
                parameters: json!({"type": "object", "properties": {}}),
            }
        }
        fn call(&self, _args: &Value) -> crate::tools::ToolCallFut<'_> {
            // Takes a moment, so a prefold started alongside would get to
            // call the model.
            Box::pin(async {
                tokio::time::sleep(Duration::from_millis(5)).await;
                Ok("2026-10-14 INFO request served in 12ms\n".repeat(500))
            })
        }
    }

    #[tokio::test]
    async fn when_slimming_old_outputs_is_enough_no_model_fold_runs_at_all() {
        let log_call = |id: &str| CompleteResponse {
            function_calls: vec![FunctionCall { call_id: id.into(), name: "big_log".into(), arguments: "{}".into() }],
            output_items: vec![json!({"type":"function_call","call_id": id,"name":"big_log","arguments":"{}"})],
            ..CompleteResponse::new(id)
        };
        // No server usage: `used` is the local estimate (calibration 1).
        let mut replies: Vec<CompleteResponse> = (0..12).map(|n| log_call(&format!("c{n}"))).collect();
        replies.push(CompleteResponse { text: "done".into(), ..CompleteResponse::new("end") });
        let provider = Scripted {
            calls: Mutex::new(Vec::new()),
            replies: Mutex::new(pop_front(replies)),
            compact_calls: Mutex::new(Vec::new()),
            compact_ok: false,
        };
        let tools = ToolRegistry::new(vec![Box::new(BigLog)]);
        let rec = Rec(Mutex::new(Vec::new()));
        let mut config = cfg("look through the logs", 20);
        config.context_window = 100_000;
        let provider = Arc::new(provider);
        let out = run(&provider, &tools, &rec, config).await.unwrap();
        assert_eq!(out.text, "done");
        let calls = provider.calls.lock().unwrap();
        assert_eq!(calls.len(), 13, "only working requests: no prefold, no fold");
        assert!(!calls.iter().any(|c| c.input.iter().any(|i| i["content"].as_str().is_some_and(|t| t.contains(MEMORY_FOLD_MARK)))));
        let events = rec.0.lock().unwrap();
        let methods: Vec<&String> = events
            .iter()
            .filter_map(|e| match e { AgentEvent::ContextCompacted { method, .. } => Some(method), _ => None })
            .collect();
        assert!(!methods.is_empty() && methods.iter().all(|m| *m == "trim-outputs"), "{methods:?}");
        // Every turn is still there, word for word apart from the slimmed outputs.
        let last = calls.last().unwrap();
        assert_eq!(last.input.iter().filter(|i| i["type"] == "function_call").count(), 12);
    }

    // ---- dynamic folds: made by a background task, spliced in when ready

    /// A working reply that calls `now`, after `pads` user-role items of
    /// ~500 tokens each (bulk that is cheap to make).
    fn now_call(id: &str, pads: usize) -> CompleteResponse {
        let mut output_items: Vec<Value> = (0..pads)
            .map(|i| json!({"role":"user","content": format!("pad-{id}-{i} {}", "notes ".repeat(330))}))
            .collect();
        output_items.push(json!({"type":"function_call","call_id": id,"name":"now","arguments":"{}"}));
        CompleteResponse {
            function_calls: vec![FunctionCall { call_id: id.into(), name: "now".into(), arguments: "{}".into() }],
            output_items,
            ..CompleteResponse::new(id)
        }
    }

    /// A reply that calls `log` for `n` tokens of output, after `pads`
    /// user-role items of ~500 tokens (bulk that trimming cannot slim).
    fn log_reply(id: &str, n: u32, pads: usize) -> CompleteResponse {
        let mut output_items: Vec<Value> = (0..pads)
            .map(|i| json!({"role":"user","content": format!("pad-{id}-{i} {}", "notes ".repeat(330))}))
            .collect();
        let args = format!("{{\"n\":{n}}}");
        output_items.push(json!({"type":"function_call","call_id": id,"name":"log","arguments": args}));
        CompleteResponse {
            function_calls: vec![FunctionCall { call_id: id.into(), name: "log".into(), arguments: args }],
            output_items,
            ..CompleteResponse::new(id)
        }
    }

    /// `k` parallel `log` calls of `n` tokens each.
    fn parallel_logs(id: &str, k: usize, n: u32) -> CompleteResponse {
        let ids: Vec<String> = (0..k).map(|i| format!("{id}-{i}")).collect();
        let args = format!("{{\"n\":{n}}}");
        CompleteResponse {
            function_calls: ids.iter().map(|c| FunctionCall { call_id: c.clone(), name: "log".into(), arguments: args.clone() }).collect(),
            output_items: ids.iter().map(|c| json!({"type":"function_call","call_id": c,"name":"log","arguments": args})).collect(),
            ..CompleteResponse::new(id)
        }
    }

    fn done_reply() -> CompleteResponse {
        CompleteResponse { text: "done".into(), ..CompleteResponse::new("end") }
    }

    fn is_fold_request(req: &CompleteRequest) -> bool {
        req.input.iter().any(|i| i["content"].as_str().is_some_and(|t| t.contains(MEMORY_FOLD_MARK)))
    }

    /// How fold requests are answered (by [`Watched`], never from the
    /// scripted queue, so the working replies keep their order whatever
    /// the timing).
    #[derive(Clone)]
    struct FoldScript {
        /// The memory written; `None`: nothing usable.
        text: Option<&'static str>,
        delay: Duration,
        /// Fold requests after this many get nothing usable.
        ok_first: usize,
        /// Answer the first fold request (the background fold) only once this
        /// many more tool calls have ended: a deterministic "slow" fold.
        release_after: usize,
    }

    impl FoldScript {
        fn ok(text: &'static str) -> Self {
            Self { text: Some(text), delay: Duration::ZERO, ok_first: usize::MAX, release_after: 0 }
        }
        /// Answered only after `tools` more tool calls have ended.
        fn after_tools(text: &'static str, tools: usize) -> Self {
            Self { release_after: tools, ..Self::ok(text) }
        }
        fn failing() -> Self {
            Self { text: None, ok_first: 0, ..Self::ok("") }
        }
    }

    const MEMORY: &str = "## Load-bearing facts\n- the kernel ABI is frozen\n## Open work\n- port the scheduler";

    #[derive(Clone)]
    struct Watched {
        inner: Arc<Scripted>,
        /// Fold requests made, and answered.
        folds: Arc<std::sync::atomic::AtomicUsize>,
        folds_done: Arc<std::sync::atomic::AtomicUsize>,
        /// Tool calls ended so far.
        tool_calls: Arc<std::sync::atomic::AtomicUsize>,
        fold_reqs: Arc<Mutex<Vec<CompleteRequest>>>,
        script: FoldScript,
    }

    impl Provider for Watched {
        async fn complete(&self, req: CompleteRequest) -> Result<CompleteResponse> {
            if is_fold_request(&req) {
                self.fold_reqs.lock().unwrap().push(req.clone());
                let n = self.folds.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                // Only the first (the background fold): a blocking fold at
                // the trigger would wait for tool calls that never come.
                let release = self.tool_calls.load(std::sync::atomic::Ordering::SeqCst)
                    + if n == 0 { self.script.release_after } else { 0 };
                while self.tool_calls.load(std::sync::atomic::Ordering::SeqCst) < release {
                    tokio::time::sleep(Duration::from_millis(1)).await;
                }
                if !self.script.delay.is_zero() {
                    tokio::time::sleep(self.script.delay).await;
                }
                self.folds_done.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                let text = self.script.text.filter(|_| n < self.script.ok_first).unwrap_or_default();
                return Ok(CompleteResponse { text: text.to_string(), ..CompleteResponse::new(format!("fold{n}")) });
            }
            self.inner.complete(req).await
        }

        async fn compact(&self, req: CompactRequest) -> Result<CompactResponse> {
            self.inner.compact(req).await
        }
    }

    /// Records, when each tool call ends, how many folds had been answered.
    #[derive(Clone)]
    struct Probe {
        folds_done: Arc<std::sync::atomic::AtomicUsize>,
        tool_calls: Arc<std::sync::atomic::AtomicUsize>,
        seen: Arc<Mutex<Vec<usize>>>,
    }

    impl Probe {
        fn mark(&self) {
            self.seen.lock().unwrap().push(self.folds_done.load(std::sync::atomic::Ordering::SeqCst));
            self.tool_calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        }
    }

    /// `now` that takes a moment, so a background fold gets to run.
    struct SlowNow(Probe);

    impl crate::tools::ClientTool for SlowNow {
        fn spec(&self) -> crate::tools::ToolSpec {
            crate::tools::ClientTool::spec(&NowTool)
        }
        fn call(&self, _args: &Value) -> crate::tools::ToolCallFut<'_> {
            Box::pin(async move {
                tokio::time::sleep(Duration::from_millis(5)).await;
                self.0.mark();
                Ok(json!({"utc": "2026-10-14T00:00:00Z"}).to_string())
            })
        }
    }

    /// A tool that prints `n` tokens of log (local estimate).
    struct SizedLog(Probe);

    impl crate::tools::ClientTool for SizedLog {
        fn spec(&self) -> crate::tools::ToolSpec {
            crate::tools::ToolSpec {
                name: "log".into(),
                description: "Print n tokens of log.".into(),
                parameters: json!({"type": "object", "properties": {"n": {"type": "integer"}}}),
            }
        }
        fn call(&self, args: &Value) -> crate::tools::ToolCallFut<'_> {
            let n = args["n"].as_u64().unwrap_or(10) as usize;
            Box::pin(async move {
                tokio::time::sleep(Duration::from_millis(5)).await;
                self.0.mark();
                Ok("log line ok\n".repeat(n / 3))
            })
        }
    }

    struct DynRun {
        out: RunOutcome,
        /// Working requests only (folds are answered by `Watched`).
        calls: Vec<CompleteRequest>,
        events: Vec<AgentEvent>,
        /// Folds answered when each tool call ended.
        seen: Vec<usize>,
        folds: usize,
        fold_reqs: Vec<CompleteRequest>,
    }

    impl DynRun {
        fn methods(&self) -> Vec<String> {
            self.events
                .iter()
                .filter_map(|e| match e {
                    AgentEvent::ContextCompacted { method, .. } => Some(method.clone()),
                    _ => None,
                })
                .collect()
        }

        fn compactions(&self) -> Vec<(String, u32)> {
            self.events
                .iter()
                .filter_map(|e| match e {
                    AgentEvent::ContextCompacted { method, input_tokens, .. } => Some((method.clone(), *input_tokens)),
                    _ => None,
                })
                .collect()
        }

        fn noticed(&self, needle: &str) -> bool {
            self.notices(needle) > 0
        }

        fn notices(&self, needle: &str) -> usize {
            self.events.iter().filter(|e| matches!(e, AgentEvent::Notice { message, .. } if message.contains(needle))).count()
        }
    }

    /// Run with `now` (SlowNow) or `log` (SizedLog) as the only tool.
    async fn dyn_run(replies: Vec<CompleteResponse>, window: u32, logs: bool, script: FoldScript) -> DynRun {
        let scripted = Arc::new(Scripted {
            calls: Mutex::new(Vec::new()),
            replies: Mutex::new(pop_front(replies)),
            compact_calls: Mutex::new(Vec::new()),
            compact_ok: false,
        });
        let folds = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let folds_done = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let tool_calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let probe = Probe { folds_done: folds_done.clone(), tool_calls: tool_calls.clone(), seen: Arc::new(Mutex::new(Vec::new())) };
        let fold_reqs = Arc::new(Mutex::new(Vec::new()));
        let provider = Watched { inner: scripted.clone(), folds: folds.clone(), folds_done, tool_calls, fold_reqs: fold_reqs.clone(), script };
        let tool: Box<dyn crate::tools::ClientTool> =
            if logs { Box::new(SizedLog(probe.clone())) } else { Box::new(SlowNow(probe.clone())) };
        let tools = ToolRegistry::new(vec![tool]);
        let rec = Rec(Mutex::new(Vec::new()));
        let mut config = cfg("ship the kernel", 20);
        config.context_window = window;
        let out = tokio::time::timeout(Duration::from_secs(20), run(&provider, &tools, &rec, config))
            .await
            .expect("the run must not hang on a fold")
            .unwrap();
        let calls = scripted.calls.lock().unwrap().clone();
        let events = rec.0.lock().unwrap().clone();
        let seen = probe.seen.lock().unwrap().clone();
        let fold_reqs = fold_reqs.lock().unwrap().clone();
        DynRun { out, calls, events, seen, folds: folds.load(std::sync::atomic::Ordering::SeqCst), fold_reqs }
    }

    // Window 10,000: start 4,000, trigger 5,000, target 2,200. Nine pads
    // bring turn 2 to ~4,600 (no server usage: measured locally).

    #[tokio::test]
    async fn a_background_fold_is_spliced_in_as_soon_as_it_is_ready() {
        let r = dyn_run(vec![now_call("c1", 9), now_call("c2", 0), done_reply()], 10_000, false, FoldScript::ok(MEMORY)).await;
        assert_eq!(r.out.text, "done");
        assert!(r.noticed("背景整理長期記憶中"), "the user sees it start");
        assert_eq!(r.folds, 1);
        assert_eq!(r.seen, [0, 1], "it was answered while turn 2's tool ran: the work did not wait");
        let compactions = r.compactions();
        assert_eq!(compactions.len(), 1, "{compactions:?}");
        assert_eq!(compactions[0].0, "lived (dynamic)");
        assert!(compactions[0].1 < 5_000, "spliced in before the trigger: {compactions:?}");
        let last = r.calls.last().unwrap();
        assert_eq!(compact::previous_memory(&last.input).map(|m| m.contains("kernel ABI is frozen")), Some(true));
        let mem_at = last.input.iter().position(|i| i["content"].as_str().is_some_and(|t| t.contains(COMPACT_MARK))).unwrap();
        let pad_at = last.input.iter().position(|i| i["content"].as_str().is_some_and(|t| t.starts_with("pad-c1-7 ")));
        assert!(pad_at.is_some_and(|p| p < mem_at), "the user's newest folded words are kept verbatim, before the memory");
        let memory = compact::previous_memory(&last.input).unwrap();
        assert!(memory.contains("## Original request\nship the kernel"), "{memory}");
        // The fold request is served from the conversation's cache: same key,
        // instructions and tools, and it starts with the conversation as sent
        // in the turn it folded.
        let (f, w) = (&r.fold_reqs[0], &r.calls[1]);
        assert_eq!(f.cache_key, w.cache_key);
        assert_eq!(f.instructions, w.instructions);
        let names = |q: &CompleteRequest| q.client_tools.iter().map(|t| t.name.clone()).collect::<Vec<_>>();
        assert_eq!(names(f), names(w));
        let head = f.input.iter().position(|i| i["content"].as_str().is_some_and(|t| t.contains(MEMORY_FOLD_MARK))).unwrap();
        assert!(head >= 2, "{head}");
        assert_eq!(&f.input[..head], &w.input[..head], "the folded turns as sent: a cached prefix");
    }

    #[tokio::test]
    async fn the_work_goes_on_while_a_slow_fold_runs_and_one_fold_runs_at_a_time() {
        // The fold is answered only after three more tool calls: turns 2-4
        // go on while it runs, each still in the band.
        let r = dyn_run(
            vec![now_call("c1", 9), now_call("c2", 0), now_call("c3", 0), now_call("c4", 0), now_call("c5", 0), now_call("c6", 0), done_reply()],
            10_000,
            false,
            FoldScript::after_tools(MEMORY, 3),
        )
        .await;
        assert_eq!(r.out.text, "done");
        assert_eq!(r.folds, 1, "one background fold, not one per turn");
        assert_eq!(r.methods(), ["lived (dynamic)"]);
        assert!(r.seen.starts_with(&[0, 0, 0, 0]), "turns went on while it ran: {:?}", r.seen);
    }

    #[tokio::test]
    async fn a_background_fold_overtaken_by_a_big_arrival_gives_way_to_a_fresh_fold() {
        // The fold is slow; meanwhile eight pads arrive and cross the
        // trigger. Spliced, it would still be over the trigger.
        let r = dyn_run(
            vec![now_call("c1", 9), now_call("c2", 8), done_reply()],
            10_000,
            false,
            FoldScript::after_tools(MEMORY, 1),
        )
        .await;
        assert_eq!(r.out.text, "done");
        let methods = r.methods();
        assert_eq!(methods.len(), 1, "{methods:?}");
        assert!(!methods[0].ends_with("(dynamic)"), "{methods:?}");
        assert!(r.folds >= 2, "the background fold and a fresh one: {}", r.folds);
    }

    #[tokio::test]
    async fn a_finished_background_fold_that_would_land_past_the_start_point_is_refused() {
        // Answered at once (during turn 2's tool call), so turn 3 finds it
        // finished; the eight pads that arrived meanwhile would leave the
        // splice over the trigger, while a fresh fold gets much lower.
        let r = dyn_run(vec![now_call("c1", 9), now_call("c2", 8), done_reply()], 10_000, false, FoldScript::ok(MEMORY)).await;
        assert_eq!(r.out.text, "done");
        assert_eq!(r.seen, [0, 1], "finished before turn 3 began");
        let methods = r.methods();
        assert_eq!(methods.len(), 1, "{methods:?}");
        assert!(!methods[0].ends_with("(dynamic)"), "{methods:?}");
        assert!(r.folds >= 2, "the background fold and a fresh one: {}", r.folds);
    }

    #[tokio::test]
    async fn no_background_fold_when_it_would_gain_little() {
        // Two small turns, then one 4.6K output brings turn 4 into the band.
        // There is something to fold, but a fold must keep that output (it is
        // the newest turn): it would gain under a tenth of the window, for a
        // cache miss, so none starts. Turn 4's tool call would have given a
        // started fold time to reach the model. (By turn 5 the output is no
        // longer the newest, and a fold is worth it.)
        let replies = vec![
            log_reply("s1", 10, 0),
            log_reply("s2", 10, 0),
            log_reply("big", 4_600, 0),
            log_reply("small", 10, 0),
            done_reply(),
        ];
        let r = dyn_run(replies, 10_000, true, FoldScript::ok(MEMORY)).await;
        assert_eq!(r.out.text, "done");
        let turn5 = r.events.iter().position(|e| matches!(e, AgentEvent::TurnStarted { turn: 5, .. })).unwrap();
        assert!(
            !r.events[..turn5].iter().any(|e| matches!(e, AgentEvent::Notice { message, .. } if message.contains("背景整理長期記憶中"))),
            "no fold started at turn 4"
        );
        assert_eq!(r.folds, 0);
    }

    #[tokio::test]
    async fn at_the_trigger_a_running_background_fold_is_waited_for() {
        // The fold is answered a tool call later; meanwhile one pad crosses
        // the trigger. The fold has a head start, so it is waited for and
        // spliced (it lands low enough), not redone.
        let r = dyn_run(vec![now_call("c1", 9), now_call("c2", 1), done_reply()], 10_000, false, FoldScript::after_tools(MEMORY, 1)).await;
        assert_eq!(r.out.text, "done");
        assert_eq!(r.folds, 1);
        assert_eq!(r.methods(), ["lived (background, awaited)"]);
    }

    #[tokio::test]
    async fn a_failed_background_fold_is_not_restarted_every_turn() {
        // Every fold fails. In the band for several turns (never reaching
        // the trigger), only one background fold is tried.
        let mut replies = vec![now_call("c1", 9)];
        replies.extend((2..8).map(|n| now_call(&format!("c{n}"), 0)));
        replies.push(done_reply());
        let r = dyn_run(replies, 10_000, false, FoldScript::failing()).await;
        assert_eq!(r.out.text, "done");
        assert_eq!(r.notices("背景整理長期記憶中"), 1, "started once");
        assert_eq!(r.folds as u32, compact::FOLD_ATTEMPTS, "one fold's attempts, no more");
        assert_eq!(r.notices("背景整理記憶失敗"), 1);
        assert!(r.methods().is_empty());
    }

    #[tokio::test]
    async fn a_finished_refused_background_fold_stands_in_when_the_fresh_fold_fails() {
        // Finished before turn 3 (step 1 takes it), refused for the eight
        // pads that arrived; the fresh fold fails, so it stands in.
        let script = FoldScript { ok_first: 1, ..FoldScript::ok(MEMORY) };
        let r = dyn_run(vec![now_call("c1", 9), now_call("c2", 8), done_reply()], 10_000, false, script).await;
        assert_eq!(r.out.text, "done");
        assert_eq!(r.seen, [0, 1], "finished before turn 3 began");
        assert_eq!(r.methods(), ["lived (stand-in)"]);
        assert!(r.noticed("改用背景先整理好的記憶"));
    }

    #[tokio::test]
    async fn a_refused_background_fold_stands_in_when_the_fresh_fold_fails() {
        let script = FoldScript { ok_first: 1, ..FoldScript::after_tools(MEMORY, 1) };
        let r = dyn_run(vec![now_call("c1", 9), now_call("c2", 8), done_reply()], 10_000, false, script).await;
        assert_eq!(r.out.text, "done");
        assert_eq!(r.methods(), ["lived (stand-in)"], "the refused splice stands in");
        assert!(r.noticed("改用背景先整理好的記憶"));
    }

    // Window 100,000: start 40,000, trigger 50,000, target 22,000; slimming
    // alone is enough in the band when it gets to 31,000.

    #[tokio::test]
    async fn in_the_band_slimming_old_outputs_replaces_a_fold_when_it_gets_far_enough() {
        // Pads and two 12K outputs, then small ones, reach ~40K; slimming
        // the old big ones lands near 17K.
        let replies = vec![
            log_reply("b1", 12_000, 10),
            log_reply("b2", 12_000, 0),
            log_reply("s3", 2_000, 0),
            log_reply("s4", 2_000, 0),
            log_reply("s5", 2_000, 0),
            log_reply("s6", 2_000, 0),
            log_reply("s7", 3_000, 0),
            done_reply(),
        ];
        let r = dyn_run(replies, 100_000, true, FoldScript::ok(MEMORY)).await;
        assert_eq!(r.out.text, "done");
        assert_eq!(r.folds, 0, "no model fold at all");
        assert_eq!(r.methods(), ["trim-outputs"]);
        let last = r.calls.last().unwrap();
        assert!(compact::previous_memory(&last.input).is_none(), "every turn is still there");
        assert_eq!(last.input.iter().filter(|i| i["type"] == "function_call").count(), 7);
    }

    #[tokio::test]
    async fn in_the_band_slimming_that_stops_short_of_halfway_leaves_it_to_a_background_fold() {
        // 25K of pads and an 8K output (~33K), then four parallel 2K outputs
        // reach ~41K, in the band. Slimming the old 8K one would land at
        // ~34K: below the start point, but short of halfway to the target
        // (31K). Not worth a cache miss on its own: a fold runs instead.
        let replies = vec![log_reply("b1", 8_000, 50), parallel_logs("p2", 4, 2_000), log_reply("s3", 10, 0), done_reply()];
        let r = dyn_run(replies, 100_000, true, FoldScript::ok(MEMORY)).await;
        assert_eq!(r.out.text, "done");
        assert_eq!(r.methods(), ["lived (dynamic)"]);
    }

    #[tokio::test]
    async fn at_the_trigger_slimming_below_the_start_point_beats_a_blocking_fold() {
        // 25K of pads and a 12K output (~37K, under the start point), then
        // five parallel 3K outputs jump past the trigger. Slimming the old
        // ones lands at ~38.7K: not far enough to stand for a fold in the
        // band, but below the start point; with no background fold to wait
        // for, that beats a blocking fold now.
        let replies = vec![log_reply("b1", 12_000, 50), parallel_logs("p2", 5, 3_000), done_reply()];
        let r = dyn_run(replies, 100_000, true, FoldScript::ok(MEMORY)).await;
        assert_eq!(r.out.text, "done");
        assert_eq!(r.folds, 0, "no model fold");
        assert_eq!(r.methods(), ["trim-outputs"]);
    }

    #[tokio::test]
    async fn a_compaction_during_backoff_ends_the_backoff() {
        // 25K of pads and an 8K output (~33K), then a 20K output jumps past
        // the trigger with nothing old enough to slim: a blocking fold runs
        // and fails, which sets the backoff. Four parallel calls then push
        // the big outputs out of the newest four, slimming them lands far
        // enough while the backoff is on, and that ends it: back in the band,
        // a background fold starts.
        let replies = vec![
            log_reply("b1", 8_000, 50),
            log_reply("b2", 20_000, 0),
            parallel_logs("p3", 4, 500),
            log_reply("s4", 13_000, 0),
            log_reply("s5", 500, 0),
            done_reply(),
        ];
        let r = dyn_run(replies, 100_000, true, FoldScript::failing()).await;
        assert_eq!(r.out.text, "done");
        assert!(r.noticed("輪後再試"), "the first fold failed and set the backoff");
        assert_eq!(r.methods(), ["trim-outputs"]);
        assert!(r.folds as u32 > compact::FOLD_ATTEMPTS, "a background fold started after the backoff ended: {}", r.folds);
    }

    #[test]
    fn usage_scales_the_estimated_part_by_the_calibration() {
        // Server count plus what was added since, the added part in real tokens.
        assert_eq!(used_tokens(10_000, 1_000, 0, 2.0), 12_000);
        assert_eq!(used_tokens(10_000, 1_000, 0, 1.0), 11_000);
        // No server count: the whole history, scaled.
        assert_eq!(used_tokens(0, 0, 5_000, 1.5), 7_500);
    }

    #[tokio::test]
    async fn a_looping_stream_is_cut_off_and_the_turn_retried() {
        struct Looper {
            calls: Mutex<Vec<CompleteRequest>>,
        }
        impl Provider for Looper {
            async fn complete(&self, _req: CompleteRequest) -> Result<CompleteResponse> {
                unreachable!("streaming only")
            }
            async fn complete_stream<'a>(
                &'a self,
                req: CompleteRequest,
                on_text: &'a (dyn Fn(&str) + Send + Sync),
                _on_server: &'a (dyn Fn(&str, &Value) + Send + Sync),
                _on_reasoning: &'a (dyn Fn(&str) + Send + Sync),
            ) -> Result<CompleteResponse> {
                let first = {
                    let mut c = self.calls.lock().unwrap();
                    c.push(req);
                    c.len() == 1
                };
                if first {
                    // Degenerate: the same sentence forever, never finishing.
                    loop {
                        on_text("I will check the file again. ");
                        tokio::task::yield_now().await;
                    }
                }
                on_text("done");
                Ok(CompleteResponse { text: "done".into(), ..CompleteResponse::new("2") })
            }
            async fn compact(&self, _req: CompactRequest) -> Result<CompactResponse> {
                Err(Error::Provider("no".into()))
            }
        }
        let provider = Looper { calls: Mutex::new(Vec::new()) };
        let tools = ToolRegistry::new(vec![Box::new(NowTool)]);
        let rec = Rec(Mutex::new(Vec::new()));
        let provider = Arc::new(provider);
        let out = tokio::time::timeout(Duration::from_secs(10), run(&provider, &tools, &rec, cfg("go", 4)))
            .await
            .expect("the loop must be cut off")
            .unwrap();
        assert_eq!(out.text, "done");
        let calls = provider.calls.lock().unwrap();
        assert_eq!(calls.len(), 2);
        let note = calls[1].input.last().unwrap()["content"].as_str().unwrap_or("").to_string();
        assert!(note.contains("[loop guard]") && note.contains("I will check the file again."), "{note}");
        let events = rec.0.lock().unwrap();
        assert!(events.iter().any(|e| matches!(e, AgentEvent::Notice { message, .. } if message.contains("重複"))));
        assert!(!events.iter().any(|e| matches!(e, AgentEvent::Error { .. })), "a first loop is not an error");
    }

    #[tokio::test]
    async fn transient_provider_errors_are_resent_without_touching_history() {
        struct Flaky {
            fails: Mutex<u32>,
            calls: Mutex<Vec<usize>>,
        }
        impl Provider for Flaky {
            async fn complete(&self, req: CompleteRequest) -> Result<CompleteResponse> {
                self.calls.lock().unwrap().push(req.input.len());
                let mut f = self.fails.lock().unwrap();
                if *f > 0 {
                    *f -= 1;
                    return Err(Error::Provider(
                        "HTTP 402: in_flight_budget_exhausted, retry after in-flight requests settle".into(),
                    ));
                }
                Ok(CompleteResponse {
                    text: "done".into(),
                    ..CompleteResponse::new("1")
                })
            }
            async fn compact(&self, _req: CompactRequest) -> Result<CompactResponse> {
                Err(Error::Provider("no".into()))
            }
        }
        let provider = Flaky {
            fails: Mutex::new(4),
            calls: Mutex::new(Vec::new()),
        };
        let tools = ToolRegistry::new(vec![Box::new(NowTool)]);
        let rec = Rec(Mutex::new(Vec::new()));
        let provider = Arc::new(provider);
        let out = run(&provider, &tools, &rec, cfg("go", 4)).await.unwrap();
        assert_eq!(out.text, "done");
        let calls = provider.calls.lock().unwrap().clone();
        assert_eq!(calls.len(), 5, "4 transient failures, then the same request succeeds");
        assert!(calls.iter().all(|&n| n == calls[0]), "resends do not add recovery notes: {calls:?}");
        let events = rec.0.lock().unwrap().clone();
        assert!(!events.iter().any(|e| matches!(e, AgentEvent::Error { .. })));
        assert_eq!(
            events
                .iter()
                .filter(|e| matches!(e, AgentEvent::Notice { message, .. } if message.contains("重送")))
                .count(),
            4
        );
    }

    #[tokio::test]
    async fn lived_fold_retries_until_success() {
        struct LivedWriteRetry {
            inner: Scripted,
            fold_attempts: Mutex<u32>,
        }
        impl Provider for LivedWriteRetry {
            async fn complete(&self, req: CompleteRequest) -> Result<CompleteResponse> {
                let fold = req.input.iter().any(|i| {
                    i.get("content")
                        .and_then(Value::as_str)
                        .is_some_and(|s| s.contains(MEMORY_FOLD_MARK))
                });
                if fold {
                    let n = {
                        let mut g = self.fold_attempts.lock().unwrap();
                        *g += 1;
                        *g
                    };
                    self.inner.calls.lock().unwrap().push(req);
                    if n == 1 {
                        return Err(Error::Provider("lived write refused".into()));
                    }
                    return Ok(CompleteResponse {
                        text: "## Original request\nship the kernel\n## Direction changes\nnone\n## Load-bearing facts\nkeep the ABI stable\n## Where you were\nporting the scheduler\n".into(),
                        ..CompleteResponse::new("mem")
                    });
                }
                self.inner.complete(req).await
            }
            async fn compact(&self, req: CompactRequest) -> Result<CompactResponse> {
                self.inner.compact(req).await
            }
        }
        let first = CompleteResponse {
            function_calls: vec![FunctionCall {
                call_id: "c1".into(),
                name: "now".into(),
                arguments: "{}".into(),
            }],
            usage: CacheUsage {
                input_tokens: 6_000,
                cached_tokens: 0, output_tokens: 0,
            },
            output_items: vec![
                json!({"role":"user","content":"pad-1"}),
                json!({"role":"user","content":"pad-2"}),
                json!({"role":"user","content":"pad-3"}),
                json!({"role":"user","content":"pad-4"}),
                json!({"role":"user","content":"pad-5"}),
                json!({"type":"function_call","call_id":"c1","name":"now","arguments":"{}"}),
            ],
            ..CompleteResponse::new("1")
        };
        let provider = LivedWriteRetry {
            inner: Scripted {
                calls: Mutex::new(Vec::new()),
                replies: Mutex::new(pop_front(vec![
                    first,
                    CompleteResponse {
                        text: "done".into(),
                        ..CompleteResponse::new("2")
                    },
                ])),
                compact_calls: Mutex::new(Vec::new()),
                compact_ok: false,
            },
            fold_attempts: Mutex::new(0),
        };
        let tools = ToolRegistry::new(vec![Box::new(NowTool)]);
        let rec = Rec(Mutex::new(Vec::new()));
        let mut config = cfg("ship the kernel", 4);
        config.context_window = 10_000;
        config.compact_keep_recent = 2;
        let provider = Arc::new(provider);
        let out = run(&provider, &tools, &rec, config).await.unwrap();
        assert_eq!(out.text, "done");
        assert_eq!(out.compacted, 1);
        assert_eq!(*provider.fold_attempts.lock().unwrap(), 2);
        let events = rec.0.lock().unwrap().clone();
        assert!(
            events.iter().any(|e| matches!(
                e,
                AgentEvent::Notice { message, .. }
                    if message.contains("壓縮重試") && message.contains("lived write refused")
            )),
            "{events:?}"
        );
        assert!(events.iter().any(|e| matches!(
            e,
            AgentEvent::ContextCompacted { method, .. } if method == "lived"
        )));
        assert!(!events.iter().any(|e| matches!(
            e,
            AgentEvent::Notice { message, .. }
                if message.contains("略過") || message.contains("extractive")
        )));
        let calls = provider.inner.calls.lock().unwrap();
        let work = calls.last().expect("working turn after compact");
        assert!(
            work.input.iter().any(|i| {
                i.get("content")
                    .and_then(Value::as_str)
                    .is_some_and(|s| s.contains(COMPACT_MARK))
            }),
            "must compact before the next working turn: {:?}",
            work.input
        );
    }

    #[tokio::test]
    async fn forwards_reasoning_effort() {
        let provider = Scripted {
            calls: Mutex::new(Vec::new()),
            replies: Mutex::new(pop_front(vec![CompleteResponse {
                text: "ok".into(),
                ..CompleteResponse::new("1")
            }])),
            compact_calls: Mutex::new(Vec::new()),
            compact_ok: false,
        };
        let mut c = cfg("hi", 1);
        c.reasoning_effort = ReasoningEffort::Xhigh;
        let tools = ToolRegistry::new(vec![Box::new(NowTool)]);
        let rec = Rec(Mutex::new(Vec::new()));
        let provider = Arc::new(provider);
        run(&provider, &tools, &rec, c).await.unwrap();
        let calls = provider.calls.lock().unwrap();
        assert_eq!(calls[0].reasoning_effort, ReasoningEffort::Xhigh);
        assert_eq!(calls[0].model, "grok-4.6");
    }

    #[tokio::test]
    async fn insert_after_stop_continues_same_session() {
        let (tx, rx) = mpsc::unbounded_channel();
        tx.send("follow up".into()).unwrap();
        drop(tx);
        let provider = Scripted {
            calls: Mutex::new(Vec::new()),
            replies: Mutex::new(pop_front(vec![
                CompleteResponse {
                    text: "first".into(),
                    ..CompleteResponse::new("resp_1")
                },
                CompleteResponse {
                    text: "second".into(),
                    ..CompleteResponse::new("resp_2")
                },
            ])),
            compact_calls: Mutex::new(Vec::new()),
            compact_ok: false,
        };
        let mut c = cfg("hello", 8);
        c.inbox = Some(rx);
        let tools = ToolRegistry::new(vec![Box::new(NowTool)]);
        let rec = Rec(Mutex::new(Vec::new()));
        let provider = Arc::new(provider);
        let out = run(&provider, &tools, &rec, c).await.unwrap();
        assert_eq!(out.text, "second");
        assert_eq!(out.turns, 2);
        let calls = provider.calls.lock().unwrap();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[1].previous_response_id, None);
        assert!(calls[1].input.len() > 1);
        assert_eq!(
            calls[0].input[0], calls[1].input[0],
            "follow-up user turn must keep the original prefix"
        );
        assert_eq!(calls[1].input.last().unwrap()["role"], "user");
        assert_eq!(calls[1].input.last().unwrap()["content"], "follow up");
    }

    #[tokio::test]
    async fn background_notice_wakes_waiting_session() {
        let (user_tx, user_rx) = mpsc::unbounded_channel::<UserTurn>();
        let (bg_tx, bg_rx) = mpsc::unbounded_channel();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(30)).await;
            let _ = bg_tx.send(UserTurn::from(format!(
                "{}\nname: echo\ndetail: exit 0",
                crate::background::EXIT_NOTICE_PREFIX
            )));
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            drop(user_tx);
        });
        let provider = Scripted {
            calls: Mutex::new(Vec::new()),
            replies: Mutex::new(pop_front(vec![
                CompleteResponse {
                    text: "started".into(),
                    ..CompleteResponse::new("resp_1")
                },
                CompleteResponse {
                    text: "echo died".into(),
                    ..CompleteResponse::new("resp_2")
                },
            ])),
            compact_calls: Mutex::new(Vec::new()),
            compact_ok: false,
        };
        let mut c = cfg("run echo", 8);
        c.inbox = Some(user_rx);
        c.bg_notify = Some(bg_rx);
        let tools = ToolRegistry::new(vec![Box::new(NowTool)]);
        let rec = Rec(Mutex::new(Vec::new()));
        let provider = Arc::new(provider);
        let out = run(&provider, &tools, &rec, c).await.unwrap();
        assert_eq!(out.text, "echo died");
        let calls = provider.calls.lock().unwrap();
        assert_eq!(calls.len(), 2);
        let blob = calls[1]
            .input
            .iter()
            .map(|v| v.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            blob.contains(crate::background::EXIT_NOTICE_PREFIX),
            "{blob}"
        );
        assert!(blob.contains("name: echo"), "{blob}");
        let events = rec.0.lock().unwrap().clone();
        assert!(events
            .iter()
            .any(|e| matches!(e, AgentEvent::AwaitingInput { .. })));
    }

    #[tokio::test]
    async fn closed_backgrounds_notice_precedes_user_prompt() {
        let provider = Scripted {
            calls: Mutex::new(Vec::new()),
            replies: Mutex::new(pop_front(vec![CompleteResponse {
                text: "ok, they were killed".into(),
                ..CompleteResponse::new("1")
            }])),
            compact_calls: Mutex::new(Vec::new()),
            compact_ok: false,
        };
        let mut c = cfg("what happened?", 4);
        c.closed_backgrounds = Some(crate::background::format_closed_notice(&[
            crate::background::ClosedBackground {
                name: "dev".into(),
                command: "npm run dev".into(),
                log: vec!["out listening".into()],
            },
        ]));
        let tools = ToolRegistry::new(vec![Box::new(NowTool)]);
        let rec = Rec(Mutex::new(Vec::new()));
        let provider = Arc::new(provider);
        let out = run(&provider, &tools, &rec, c).await.unwrap();
        assert_eq!(out.text, "ok, they were killed");
        let calls = provider.calls.lock().unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].input.len(), 2);
        let closed = calls[0].input[0]["content"].as_str().unwrap();
        assert!(
            closed.contains(crate::background::CLOSED_NOTICE_PREFIX),
            "{closed}"
        );
        assert!(
            closed.contains("killed because the conversation was closed"),
            "{closed}"
        );
        assert!(closed.contains("npm run dev"), "{closed}");
        assert_eq!(calls[0].input[1]["content"], "what happened?");
        let events = rec.0.lock().unwrap().clone();
        assert!(events.iter().any(|e| match e {
            AgentEvent::Notice { message, .. } => {
                message.contains(crate::background::CLOSED_NOTICE_PREFIX)
            }
            _ => false,
        }));
    }

    #[tokio::test]
    async fn prior_history_is_injected_before_the_new_prompt() {
        let provider = Scripted {
            calls: Mutex::new(Vec::new()),
            replies: Mutex::new(pop_front(vec![CompleteResponse {
                text: "continuing".into(),
                ..CompleteResponse::new("1")
            }])),
            compact_calls: Mutex::new(Vec::new()),
            compact_ok: false,
        };
        let mut c = cfg("keep going", 2);
        c.prior_history = vec![
            json!({
                "role": "user",
                "content": format!("{COMPACT_MARK}\n## Original request\nship the kernel\n")
            }),
            json!({"role": "assistant", "content": "next I will patch the scheduler"}),
        ];
        let tools = ToolRegistry::new(vec![Box::new(NowTool)]);
        let rec = Rec(Mutex::new(Vec::new()));
        let provider = Arc::new(provider);
        let out = run(&provider, &tools, &rec, c).await.unwrap();
        assert_eq!(out.text, "continuing");
        let calls = provider.calls.lock().unwrap();
        assert_eq!(calls.len(), 1);
        let contents: Vec<&str> = calls[0]
            .input
            .iter()
            .filter_map(|i| i.get("content").and_then(Value::as_str))
            .collect();
        assert!(
            contents
                .iter()
                .any(|c| c.contains(COMPACT_MARK) && c.contains("ship the kernel")),
            "{contents:?}"
        );
        assert_eq!(*contents.last().unwrap(), "keep going");
        let events = rec.0.lock().unwrap().clone();
        assert!(events.iter().any(|e| match e {
            AgentEvent::Notice { message, .. } => message.contains("已載入上次上下文"),
            _ => false,
        }));
    }

    #[tokio::test]
    async fn persist_history_strips_images_and_keeps_the_compact_brief() {
        let snaps: Arc<Mutex<Vec<Vec<Value>>>> = Arc::new(Mutex::new(Vec::new()));
        let snaps_cb = snaps.clone();
        let provider = Scripted {
            calls: Mutex::new(Vec::new()),
            replies: Mutex::new(pop_front(vec![CompleteResponse {
                text: "ok".into(),
                ..CompleteResponse::new("1")
            }])),
            compact_calls: Mutex::new(Vec::new()),
            compact_ok: false,
        };
        let mut c = cfg("next", 2);
        c.prior_history = vec![
            json!({
                "role": "user",
                "content": format!("{COMPACT_MARK}\n## Original request\nlook\n")
            }),
            crate::vision::function_call_output(
                "c1",
                "shot",
                Some("data:image/jpeg;base64,aaa"),
            ),
        ];
        c.persist_history = Some(Arc::new(move |items| {
            snaps_cb.lock().unwrap().push(items.to_vec());
        }));
        let tools = ToolRegistry::new(vec![Box::new(NowTool)]);
        let rec = Rec(Mutex::new(Vec::new()));
        let provider = Arc::new(provider);
        run(&provider, &tools, &rec, c).await.unwrap();
        let saved = snaps.lock().unwrap();
        assert!(!saved.is_empty(), "run end must persist a snapshot");
        let last = saved.last().unwrap();
        assert!(
            last.iter().any(|i| i
                .get("content")
                .and_then(Value::as_str)
                .is_some_and(|s| s.contains(COMPACT_MARK))),
            "{last:?}"
        );
        assert!(
            !crate::vision::input_has_image(last),
            "resume snapshot must not keep pixels: {last:?}"
        );
    }

    #[tokio::test]
    async fn closed_inbox_stops_after_first_reply() {
        let (tx, rx) = mpsc::unbounded_channel::<UserTurn>();
        drop(tx);
        let provider = Scripted {
            calls: Mutex::new(Vec::new()),
            replies: Mutex::new(pop_front(vec![CompleteResponse {
                text: "done".into(),
                ..CompleteResponse::new("1")
            }])),
            compact_calls: Mutex::new(Vec::new()),
            compact_ok: false,
        };
        let mut c = cfg("hello", 8);
        c.inbox = Some(rx);
        let tools = ToolRegistry::new(vec![Box::new(NowTool)]);
        let rec = Rec(Mutex::new(Vec::new()));
        let provider = Arc::new(provider);
        let out = run(&provider, &tools, &rec, c).await.unwrap();
        assert_eq!(out.text, "done");
        assert_eq!(out.turns, 1);
        assert_eq!(provider.calls.lock().unwrap().len(), 1);
        let types: Vec<&str> = rec
            .0
            .lock()
            .unwrap()
            .iter()
            .map(|e| match e {
                AgentEvent::AwaitingInput { .. } => "await",
                AgentEvent::RunFinished { .. } => "fin",
                _ => "other",
            })
            .collect();
        assert!(types.contains(&"await"), "{types:?}");
        assert!(types.contains(&"fin"), "{types:?}");
    }

    struct ChainThenZdr {
        calls: Mutex<Vec<CompleteRequest>>,
    }

    impl Provider for ChainThenZdr {
        async fn complete(&self, req: CompleteRequest) -> Result<CompleteResponse> {
            self.calls.lock().unwrap().push(req.clone());
            if req.previous_response_id.is_some() {
                return Err(Error::Provider(
                    r#"xAI HTTP 404: {"code":"not-found","error":"Previous response cannot be used for this organization due to Zero Data Retention"}"#.into(),
                ));
            }
            let n = self.calls.lock().unwrap().len();
            if n == 1 {
                return Ok(CompleteResponse {
                    function_calls: vec![FunctionCall {
                        call_id: "c1".into(),
                        name: "now".into(),
                        arguments: "{}".into(),
                    }],
                    ..CompleteResponse::new("resp_1")
                });
            }
            Ok(CompleteResponse {
                text: "recovered".into(),
                ..CompleteResponse::new("resp_2")
            })
        }

        async fn compact(&self, _req: CompactRequest) -> Result<CompactResponse> {
            Err(Error::Provider("no compact".into()))
        }
    }

    #[tokio::test]
    async fn working_turns_never_send_previous_response_id() {
        let provider = ChainThenZdr {
            calls: Mutex::new(Vec::new()),
        };
        let tools = ToolRegistry::new(vec![Box::new(NowTool)]);
        let rec = Rec(Mutex::new(Vec::new()));
        let provider = Arc::new(provider);
        let out = run(&provider, &tools, &rec, cfg("what time?", 4))
            .await
            .unwrap();
        assert_eq!(out.text, "recovered");
        let calls = provider.calls.lock().unwrap();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].previous_response_id, None);
        assert!(!calls[0].store);
        assert_eq!(calls[1].previous_response_id, None);
        assert!(
            calls[1].input.len() > 1,
            "working turns must resend full history, got {:?}",
            calls[1].input
        );
        assert_eq!(calls[0].input[0], calls[1].input[0]);
        assert!(
            !rec.0
                .lock()
                .unwrap()
                .iter()
                .any(|e| matches!(e, AgentEvent::Error { .. })),
            "working turns do not send previous_response_id, so ZDR 404 cannot fire"
        );
    }

    struct HangThenOk {
        entered: Mutex<Option<tokio::sync::oneshot::Sender<()>>>,
        calls: Mutex<u32>,
    }

    impl Provider for HangThenOk {
        async fn complete(&self, _req: CompleteRequest) -> Result<CompleteResponse> {
            let n = {
                let mut g = self.calls.lock().unwrap();
                *g += 1;
                *g
            };
            if n == 1 {
                if let Some(tx) = self.entered.lock().unwrap().take() {
                    let _ = tx.send(());
                }
                std::future::pending::<()>().await;
                unreachable!();
            }
            Ok(CompleteResponse {
                text: "after interrupt".into(),
                ..CompleteResponse::new("2")
            })
        }

        async fn compact(&self, _req: CompactRequest) -> Result<CompactResponse> {
            Err(Error::Provider("no compact".into()))
        }
    }

    #[tokio::test]
    async fn cancel_flag_drops_in_flight_complete_and_pauses() {
        let (tx, rx) = mpsc::unbounded_channel::<UserTurn>();
        let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
        let cancel = CancelFlag::new();
        let mut c = cfg("hang please", 4);
        c.inbox = Some(rx);
        c.cancel = Some(cancel.clone());
        let provider = HangThenOk {
            entered: Mutex::new(Some(entered_tx)),
            calls: Mutex::new(0),
        };
        let tools = ToolRegistry::new(vec![Box::new(NowTool)]);
        let rec = Rec(Mutex::new(Vec::new()));
        let provider = Arc::new(provider);
        let run_h = tokio::spawn(async move { run(&provider, &tools, &rec, c).await });
        entered_rx.await.expect("complete started");
        cancel.trip();
        tx.send(UserTurn::from("continue")).unwrap();
        drop(tx);
        let out = tokio::time::timeout(std::time::Duration::from_secs(2), run_h)
            .await
            .expect("run must not hang after cancel")
            .expect("join")
            .expect("run ok");
        assert_eq!(out.text, "after interrupt");
    }

    #[tokio::test]
    async fn task_proxy_review_is_cancellable() {
        let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
        let cancel = CancelFlag::new();
        let mut c = cfg("goal", 0);
        let hub = crate::task::TaskHub::new("");
        hub.start_goal("goal");
        c.task = Some(hub.clone());
        c.cancel = Some(cancel.clone());
        let provider = HangThenOk { entered: Mutex::new(Some(entered_tx)), calls: Mutex::new(0) };
        let job = tokio::spawn(async move {
            let rec = Rec(Mutex::new(Vec::new()));
            task_after_stop(&c, &provider, &rec, "r", &[], "").await
        });
        entered_rx.await.unwrap();
        cancel.trip();
        assert!(tokio::time::timeout(Duration::from_secs(2), job).await.unwrap().unwrap().is_none());
        assert_ne!(hub.snapshot().phase, crate::task::TaskPhase::Done);
    }
}

#[cfg(test)]
mod task_proxy_tests {
    use super::*;
    use crate::provider::{CompactRequest, CompactResponse, CompleteResponse, FunctionCall};
    use crate::task::{TaskHub, TaskPhase};
    use crate::tools::{ClientTool, ToolCallFut, ToolSpec};
    use std::collections::VecDeque;
    use std::sync::atomic::AtomicUsize;

    enum Step {
        Reply(Result<CompleteResponse>),
        /// Never answers; says so first.
        Hang(tokio::sync::oneshot::Sender<()>),
    }

    /// The worker's replies and the user proxy's replies, each in order.
    /// Proxy requests are told apart by their instructions.
    struct Duo {
        proxy: Mutex<VecDeque<Result<CompleteResponse>>>,
        worker: Mutex<VecDeque<Step>>,
        proxy_calls: Mutex<Vec<CompleteRequest>>,
        worker_calls: Mutex<Vec<CompleteRequest>>,
    }

    impl Duo {
        fn new(proxy: Vec<Result<CompleteResponse>>, worker: Vec<Step>) -> Arc<Self> {
            Arc::new(Self {
                proxy: Mutex::new(proxy.into()),
                worker: Mutex::new(worker.into()),
                proxy_calls: Mutex::new(Vec::new()),
                worker_calls: Mutex::new(Vec::new()),
            })
        }

        fn proxy_prompt(&self, i: usize) -> String {
            self.proxy_calls.lock().unwrap()[i].input[0]["content"].as_str().unwrap_or("").to_string()
        }
    }

    impl Provider for Duo {
        async fn complete(&self, req: CompleteRequest) -> Result<CompleteResponse> {
            if req.instructions.starts_with("You are the user's proxy") {
                self.proxy_calls.lock().unwrap().push(req);
                let next = self.proxy.lock().unwrap().pop_front();
                return next.unwrap_or_else(|| Err(Error::Provider("no proxy reply".into())));
            }
            self.worker_calls.lock().unwrap().push(req);
            let step = self.worker.lock().unwrap().pop_front();
            match step {
                Some(Step::Reply(r)) => r,
                Some(Step::Hang(tx)) => {
                    let _ = tx.send(());
                    std::future::pending::<Result<CompleteResponse>>().await
                }
                None => Err(Error::Provider("no worker reply".into())),
            }
        }

        async fn compact(&self, _req: CompactRequest) -> Result<CompactResponse> {
            Err(Error::Provider("no compact".into()))
        }
    }

    /// Records events; on the root's n-th wait for input, `on_wait(n)`
    /// says whether to close the inbox (which ends the run).
    struct Closer {
        events: Mutex<Vec<AgentEvent>>,
        tx: Mutex<Option<mpsc::UnboundedSender<UserTurn>>>,
        waits: AtomicUsize,
        on_wait: Box<dyn Fn(usize) -> bool + Send + Sync>,
    }

    impl EventSink for Closer {
        fn emit(&self, event: &AgentEvent) {
            self.events.lock().unwrap().push(event.clone());
            if let AgentEvent::AwaitingInput { meta } = event {
                if meta.parent_run_id.is_none() {
                    let n = self.waits.fetch_add(1, Ordering::SeqCst) + 1;
                    if (self.on_wait)(n) {
                        self.tx.lock().unwrap().take();
                    }
                }
            }
        }
    }

    impl Closer {
        fn intents(&self) -> Vec<String> {
            self.events
                .lock()
                .unwrap()
                .iter()
                .filter_map(|e| match e {
                    AgentEvent::TaskProxy { intent, .. } => Some(intent.clone()),
                    _ => None,
                })
                .collect()
        }
    }

    /// A kick-started task run with an inbox. The returned sender is the
    /// test's own: drop it, or the run waits for more.
    fn task_run(
        hub: &Arc<TaskHub>,
        on_wait: impl Fn(usize) -> bool + Send + Sync + 'static,
    ) -> (RunConfig, Arc<Closer>, mpsc::UnboundedSender<UserTurn>) {
        let (tx, rx) = mpsc::unbounded_channel::<UserTurn>();
        let closer = Arc::new(Closer {
            events: Mutex::new(Vec::new()),
            tx: Mutex::new(Some(tx.clone())),
            waits: AtomicUsize::new(0),
            on_wait: Box::new(on_wait),
        });
        let c = RunConfig {
            agent_name: "root".into(),
            instructions: "use tools".into(),
            prompt: crate::task::KICK.into(),
            model: "grok-4.6".into(),
            max_turns: 0,
            server_tools: vec![],
            context_window: 0,
            compact_keep_recent: 0,
            parent_run_id: None,
            run_id: "run-1".into(),
            reasoning_effort: crate::provider::ReasoningEffort::High,
            knobs: None,
            inbox: Some(rx),
            bg_notify: None,
            workspace: PathBuf::from("."),
            images: Vec::new(),
            closed_backgrounds: None,
            prior_history: Vec::new(),
            persist_history: None,
            cancel: None,
            skills: None,
            task: Some(hub.clone()),
            terminal_tool: None,
            hold: None,
            on_user_turn: None,
        };
        (c, closer, tx)
    }

    async fn go(provider: &Arc<Duo>, tools: ToolRegistry, closer: &Arc<Closer>, c: RunConfig) -> RunOutcome {
        tokio::time::timeout(Duration::from_secs(5), run(provider, &tools, closer.as_ref(), c))
            .await
            .expect("the task run must finish")
            .expect("run ok")
    }

    fn says(v: Value) -> Result<CompleteResponse> {
        Ok(CompleteResponse { text: v.to_string(), ..CompleteResponse::new("proxy") })
    }

    fn text(t: &str) -> Step {
        Step::Reply(Ok(CompleteResponse { text: t.into(), ..CompleteResponse::new("w") }))
    }

    fn tool(id: &str, name: &str, args: Value) -> Step {
        Step::Reply(Ok(CompleteResponse {
            function_calls: vec![FunctionCall { call_id: id.into(), name: name.into(), arguments: args.to_string() }],
            ..CompleteResponse::new("w")
        }))
    }

    /// A run_command stand-in that answers from a script.
    struct Cmd(Mutex<VecDeque<Value>>);

    impl Cmd {
        fn new(outs: Vec<Value>) -> Box<Self> {
            Box::new(Self(Mutex::new(outs.into())))
        }
    }

    impl ClientTool for Cmd {
        fn spec(&self) -> ToolSpec {
            ToolSpec { name: "run_command".into(), description: "run".into(), parameters: json!({"type": "object"}) }
        }

        fn call(&self, _args: &Value) -> ToolCallFut<'_> {
            let out = self.0.lock().unwrap().pop_front().unwrap_or(json!({"exit_code": 0, "stdout": ""}));
            Box::pin(async move { Ok(out.to_string()) })
        }
    }

    fn last_user_text(req: &CompleteRequest) -> String {
        req.input
            .iter()
            .rev()
            .find(|i| i["role"] == "user")
            .and_then(|i| i["content"].as_str())
            .unwrap_or("")
            .to_string()
    }

    #[tokio::test]
    async fn plan_push_then_satisfied_gets_a_summary_turn_then_done() {
        let hub = TaskHub::new("");
        hub.start_task("ship login", "with a test");
        let provider = Duo::new(
            vec![
                says(json!({"status": "continue", "message": "Write the login test first.", "checklist": [{"text": "login test", "done": false}, {"text": "login works", "done": false}], "note": "開場"})),
                says(json!({"status": "continue", "message": "Run the tests and show me the output."})),
                says(json!({"status": "satisfied", "checklist": [{"text": "login test", "done": true}, {"text": "login works", "done": true}], "note": "驗收", "reason": "the test output shows 2 passed"})),
            ],
            vec![
                text("wrote the test"),
                tool("c1", "run_command", json!({"command": "cargo test"})),
                text("tests pass: 2 passed"),
                text("Summary: login done, verified by cargo test (2 passed)."),
            ],
        );
        let (c, closer, tx) = task_run(&hub, |_| true);
        drop(tx);
        let tools = ToolRegistry::new(vec![Cmd::new(vec![json!({"exit_code": 0, "stdout": "running 2 tests\ntest result: ok. 2 passed; 0 failed"})])]);
        let out = go(&provider, tools, &closer, c).await;
        assert_eq!(out.text, "Summary: login done, verified by cargo test (2 passed).");
        let s = hub.snapshot();
        assert_eq!(s.phase, TaskPhase::Done);
        assert_eq!(s.rounds, 3);
        assert_eq!(closer.intents(), ["plan", "push", "accept"]);
        assert_eq!(provider.proxy_calls.lock().unwrap().len(), 3, "no proxy call after the summary");
        let worker = provider.worker_calls.lock().unwrap();
        assert_eq!(worker.len(), 4);
        let first = last_user_text(&worker[0]);
        assert!(first.starts_with("[user-proxy] Message from the user's proxy") && first.contains("Write the login test first."), "{first}");
        assert!(first.contains("Criteria: with a test") && first.contains("- [ ] login test"), "{first}");
        assert!(last_user_text(&worker[1]).contains("Run the tests and show me the output."));
        assert!(last_user_text(&worker[3]).starts_with("[user-proxy] The user's proxy is satisfied: the test output shows 2 passed. Write the final summary"));
        // The proxy saw the call and the end of its output.
        let prompt = provider.proxy_prompt(2);
        assert!(prompt.contains(r#"tool call: run_command {"command":"cargo test"}"#), "{prompt}");
        assert!(prompt.contains("2 passed; 0 failed"), "{prompt}");
        assert!(prompt.contains("The worker's latest reply:\ntests pass: 2 passed"), "{prompt}");
        let proxy = provider.proxy_calls.lock().unwrap();
        assert!(proxy.iter().all(|r| r.cache_key.starts_with("grokaagent-task:")));
        assert!(worker.iter().all(|r| !r.cache_key.starts_with("grokaagent-task:")));
        assert!(proxy.iter().all(|r| r.model == "grok-4.6"), "defaults to the main model");
    }

    #[tokio::test]
    async fn hand_back_returns_the_task_to_the_user() {
        let hub = TaskHub::new("");
        hub.start_goal("pick a database and migrate");
        let provider = Duo::new(
            vec![says(json!({"status": "hand_back", "reason": "only you can pick between Postgres and SQLite"}))],
            vec![text("You need to choose the database; here is where things stand.")],
        );
        let (c, closer, tx) = task_run(&hub, |_| true);
        drop(tx);
        go(&provider, ToolRegistry::new(vec![]), &closer, c).await;
        assert_eq!(hub.snapshot().phase, TaskPhase::HandedBack);
        assert_eq!(closer.intents(), ["hand_back"]);
        let worker = provider.worker_calls.lock().unwrap();
        assert!(last_user_text(&worker[0]).contains("handing this back to the user: only you can pick between Postgres and SQLite"));
    }

    #[tokio::test]
    async fn a_repeated_failing_command_brings_a_checkpoint() {
        let hub = TaskHub::new("");
        hub.start_goal("fix the build");
        let fail = json!({"exit_code": 101, "stdout": "", "stderr": "error[E0425]: cannot find value `x`"});
        let provider = Duo::new(
            vec![
                says(json!({"status": "continue", "message": "Fix the build, then show me cargo build passing."})),
                says(json!({"status": "continue", "message": "You ran the same failing command three times. Read the first error and fix it before running it again."})),
                says(json!({"status": "hand_back", "reason": "stuck"})),
            ],
            vec![
                tool("c1", "run_command", json!({"command": "cargo build"})),
                tool("c2", "run_command", json!({"command": "cargo build"})),
                tool("c3", "run_command", json!({"command": "cargo build"})),
                text("I'll read the error."),
                text("summary"),
            ],
        );
        let (c, closer, tx) = task_run(&hub, |_| true);
        drop(tx);
        let tools = ToolRegistry::new(vec![Cmd::new(vec![fail.clone(), fail.clone(), fail])]);
        go(&provider, tools, &closer, c).await;
        assert_eq!(closer.intents(), ["plan", "checkpoint", "hand_back"]);
        let prompt = provider.proxy_prompt(1);
        assert!(prompt.contains("Review kind: checkpoint"), "{prompt}");
        assert!(prompt.contains("the same failing tool call, 3 times in a row"), "{prompt}");
        assert!(prompt.contains("cannot find value"), "{prompt}");
        let worker = provider.worker_calls.lock().unwrap();
        // Injected right after the third failure, before the next request.
        let fourth = &worker[3];
        let last = fourth.input.last().unwrap();
        assert_eq!(last["role"], "user");
        assert!(last["content"].as_str().unwrap().contains("three times"), "{last}");
        assert_eq!(fourth.input[fourth.input.len() - 2]["type"], "function_call_output");
        assert!(!last_user_text(&worker[2]).contains("three times"));
    }

    #[tokio::test]
    async fn ask_user_is_answered_by_the_proxy_or_forwarded_to_the_human() {
        let hub = TaskHub::new("");
        hub.start_goal("build the notes app on SQLite");
        let provider = Duo::new(
            vec![
                says(json!({"status": "continue", "message": "Build it."})),
                says(json!({"forward": false, "selected": [{"id": "sqlite"}], "why": "the goal says SQLite"})),
                says(json!({"forward": true, "why": "the user never said"})),
                says(json!({"status": "hand_back", "reason": "done for now"})),
            ],
            vec![
                tool("q1", "ask_user", json!({"question": "Which database?", "options": [{"id": "sqlite", "label": "SQLite"}, {"id": "pg", "label": "Postgres"}]})),
                tool("q2", "ask_user", json!({"question": "Dark mode?", "options": ["yes", "no"]})),
                text("built it"),
                text("summary"),
            ],
        );
        let (c, closer, tx) = task_run(&hub, |_| true);
        drop(tx);
        let ask_hub = crate::ask::AskUserHub::new();
        // The human's pick, for the question the proxy leaves to them.
        let human = crate::ask::encode_answer(&[crate::ask::Picked { id: "opt1".into(), label: "no".into(), value: None }]);
        ask_hub.answer(human.clone());
        let sink: Arc<dyn EventSink> = closer.clone();
        let delegate = crate::task::ProxyAsk::new(
            hub.clone(),
            provider.clone(),
            None,
            "grok-4.6".into(),
            crate::provider::ReasoningEffort::High,
            PathBuf::from("."),
            sink.clone(),
            "run-1".into(),
        );
        let ask = crate::ask::AskUserTool::new(ask_hub, sink, "root".into(), "run-1".into(), None).with_delegate(Arc::new(delegate));
        go(&provider, ToolRegistry::new(vec![Box::new(ask)]), &closer, c).await;
        let worker = provider.worker_calls.lock().unwrap();
        let out = |req: &CompleteRequest, id: &str| -> Value {
            let item = req.input.iter().find(|i| i["type"] == "function_call_output" && i["call_id"] == id).unwrap();
            serde_json::from_str(item["output"].as_str().unwrap()).unwrap()
        };
        let a1 = out(&worker[1], "q1");
        assert_eq!(a1["selected"][0]["id"], "sqlite");
        assert_eq!(a1["answered_by"], "user_proxy");
        let a2 = out(&worker[2], "q2");
        assert_eq!(a2, serde_json::from_str::<Value>(&human).unwrap(), "the human's own answer");
        let events = closer.events.lock().unwrap();
        let asked: Vec<&str> = events
            .iter()
            .filter_map(|e| match e {
                AgentEvent::AskUser { question, .. } => Some(question.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(asked, ["Dark mode?"], "only the forwarded question reaches the human");
        assert!(events.iter().any(|e| matches!(e, AgentEvent::TaskProxy { intent, text, .. } if intent == "answer" && text == "問：Which database?\n答：SQLite")));
        // The proxy saw the user's words when deciding.
        let proxy = provider.proxy_calls.lock().unwrap();
        assert!(proxy[1].instructions.contains("multiple-choice question"));
        assert!(proxy[1].input[0]["content"].as_str().unwrap().contains("id=sqlite: SQLite"));
    }

    #[tokio::test]
    async fn paused_means_no_proxy_calls() {
        let hub = TaskHub::new("");
        hub.start_goal("x");
        hub.set_paused(true);
        let fail = json!({"exit_code": 1, "stderr": "boom"});
        let provider = Duo::new(
            vec![],
            vec![
                tool("c1", "run_command", json!({"command": "make"})),
                tool("c2", "run_command", json!({"command": "make"})),
                tool("c3", "run_command", json!({"command": "make"})),
                text("did it"),
            ],
        );
        let (mut c, closer, tx) = task_run(&hub, |_| true);
        drop(tx);
        c.prompt = "build it".into();
        let tools = ToolRegistry::new(vec![Cmd::new(vec![fail.clone(), fail.clone(), fail])]);
        go(&provider, tools, &closer, c).await;
        assert!(provider.proxy_calls.lock().unwrap().is_empty());
        assert!(closer.intents().is_empty());
        let worker = provider.worker_calls.lock().unwrap();
        assert_eq!(worker.len(), 4);
        assert_eq!(last_user_text(&worker[0]), "build it");
        assert_eq!(hub.snapshot().phase, TaskPhase::NeedPlan);
    }

    #[tokio::test]
    async fn a_run_started_for_a_paused_task_waits_instead_of_working() {
        let hub = TaskHub::new("");
        hub.start_goal("ship it");
        let provider = Duo::new(
            vec![
                // The plan fails twice: the task pauses.
                Err(Error::Provider("HTTP 500".into())),
                Err(Error::Provider("HTTP 500".into())),
                says(json!({"status": "hand_back", "reason": "need a decision"})),
            ],
            vec![text("summary")],
        );
        let (h, p) = (hub.clone(), provider.clone());
        let (c, closer, tx) = task_run(&hub, move |n| {
            if n == 1 {
                assert!(h.snapshot().paused);
                assert!(p.worker_calls.lock().unwrap().is_empty(), "no work on a paused task");
                h.set_paused(false);
                return false;
            }
            true
        });
        drop(tx);
        go(&provider, ToolRegistry::new(vec![]), &closer, c).await;
        assert_eq!(closer.intents(), ["hand_back"]);
        let worker = provider.worker_calls.lock().unwrap();
        assert_eq!(worker.len(), 1);
        let first = last_user_text(&worker[0]);
        assert!(first.contains("handing this back") && !first.contains("Task mode is on"), "{first}");
        assert_eq!(hub.snapshot().phase, TaskPhase::HandedBack);
    }

    #[tokio::test]
    async fn a_run_started_for_a_live_task_opens_with_the_asked_look() {
        let hub = TaskHub::from_state("", crate::task::TaskState {
            goal: "ship it".into(),
            criteria: "with a test".into(),
            phase: TaskPhase::Active,
            rounds: 3,
            ..crate::task::TaskState::default()
        });
        // What the UI does for 立即審查 / 繼續 / 修改 when no run is alive.
        hub.request_review();
        let provider = Duo::new(
            vec![
                says(json!({"status": "continue", "message": "Run the tests first and show me."})),
                says(json!({"status": "hand_back", "reason": "r"})),
            ],
            vec![text("ran them: 2 passed"), text("summary")],
        );
        let (c, closer, tx) = task_run(&hub, |_| true);
        drop(tx);
        go(&provider, ToolRegistry::new(vec![]), &closer, c).await;
        assert_eq!(closer.intents(), ["push", "hand_back"]);
        assert!(provider.proxy_prompt(0).contains("Review kind: push") && provider.proxy_prompt(0).contains("the user asked you to look now"));
        let worker = provider.worker_calls.lock().unwrap();
        let first = last_user_text(&worker[0]);
        assert!(first.contains("Run the tests first") && first.contains("Criteria: with a test"), "{first}");
    }

    #[tokio::test]
    async fn a_kept_esc_stop_lifts_on_the_users_own_message() {
        let hub = TaskHub::from_state("", crate::task::TaskState {
            goal: "ship it".into(),
            phase: TaskPhase::Active,
            skip_steer: true,
            ..crate::task::TaskState::default()
        });
        let provider = Duo::new(vec![says(json!({"status": "hand_back", "reason": "r"}))], vec![text("continuing"), text("summary")]);
        let (mut c, closer, tx) = task_run(&hub, |_| true);
        drop(tx);
        c.prompt = "please continue the task".into();
        go(&provider, ToolRegistry::new(vec![]), &closer, c).await;
        let s = hub.snapshot();
        assert!(!s.skip_steer);
        assert_eq!(s.phase, TaskPhase::HandedBack);
        assert_eq!(provider.proxy_calls.lock().unwrap().len(), 1, "the proxy looked at the worker's stop");
        assert!(provider.proxy_prompt(0).contains("[user] please continue the task"));
    }

    #[tokio::test]
    async fn a_kept_ending_leaves_the_users_new_message_alone() {
        let hub = TaskHub::from_state("", crate::task::TaskState {
            goal: "ship it".into(),
            phase: TaskPhase::Active,
            ending: Some(crate::task::Ending { phase: TaskPhase::Done, reason: "tests pass".into() }),
            ..crate::task::TaskState::default()
        });
        let provider = Duo::new(vec![], vec![text("Added the README.")]);
        let (mut c, closer, tx) = task_run(&hub, |_| true);
        drop(tx);
        c.prompt = "also add a README please".into();
        go(&provider, ToolRegistry::new(vec![]), &closer, c).await;
        let worker = provider.worker_calls.lock().unwrap();
        assert_eq!(last_user_text(&worker[0]), "also add a README please", "the user's message is the latest input");
        assert_eq!(hub.snapshot().phase, TaskPhase::Done);
        assert!(provider.proxy_calls.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn review_now_and_resume_wake_an_idle_run() {
        let hub = TaskHub::new("");
        hub.start_goal("ship it");
        let provider = Duo::new(
            vec![
                says(json!({"status": "continue", "message": "step 1"})),
                Err(Error::Provider("HTTP 500".into())),
                Err(Error::Provider("HTTP 500".into())),
                says(json!({"status": "hand_back", "reason": "need a decision"})),
            ],
            vec![text("did step 1"), text("summary")],
        );
        let h = hub.clone();
        let (c, closer, tx) = task_run(&hub, move |n| {
            if n == 1 {
                // The proxy failed and paused; the user resumes and asks for a look.
                assert!(h.snapshot().paused);
                h.set_paused(false);
                h.request_review();
                return false;
            }
            true
        });
        drop(tx);
        go(&provider, ToolRegistry::new(vec![]), &closer, c).await;
        assert_eq!(provider.proxy_calls.lock().unwrap().len(), 4);
        assert!(provider.proxy_prompt(3).contains("the user asked you to look now"));
        assert_eq!(hub.snapshot().phase, TaskPhase::HandedBack);
        assert!(closer.events.lock().unwrap().iter().any(|e| matches!(e, AgentEvent::Notice { message, .. } if message.contains("任務已暫停"))));
    }

    #[tokio::test]
    async fn the_task_model_is_used_for_proxy_requests() {
        for task_model in ["", "proxy-model"] {
            let hub = TaskHub::new("");
            hub.start_goal("x");
            let provider = Duo::new(vec![says(json!({"status": "hand_back", "reason": "r"}))], vec![text("summary")]);
            let (mut c, closer, tx) = task_run(&hub, |_| true);
            drop(tx);
            c.knobs = Some(Arc::new(Mutex::new(SessionKnobs {
                model: "main-model".into(),
                reasoning_effort: crate::provider::ReasoningEffort::Medium,
                send_reasoning: true,
                server_tools: vec![],
                dispatcher: false,
                child_model: "child-model".into(),
                task_model: task_model.into(),
                task_reasoning_effort: crate::provider::ReasoningEffort::Medium,
                task_send_reasoning: true,
                search: false,
                ultracode: false,
                workflow_size: Default::default(),
            })));
            go(&provider, ToolRegistry::new(vec![]), &closer, c).await;
            let proxy = provider.proxy_calls.lock().unwrap();
            let want = if task_model.is_empty() { "main-model" } else { task_model };
            assert_eq!(proxy[0].model, want);
            assert_eq!(proxy[0].reasoning_effort, crate::provider::ReasoningEffort::Medium);
            assert_eq!(provider.worker_calls.lock().unwrap()[0].model, "main-model");
        }
    }

    #[tokio::test]
    async fn user_messages_mid_task_reach_the_proxy_memory() {
        let hub = TaskHub::new("");
        hub.start_goal("build notes");
        let provider = Duo::new(
            vec![
                says(json!({"status": "continue", "message": "go"})),
                says(json!({"status": "hand_back", "reason": "r"})),
            ],
            vec![text("starting"), text("ok, using SQLite"), text("summary")],
        );
        let (c, closer, tx) = task_run(&hub, |_| true);
        tx.send(UserTurn::from("use SQLite please")).unwrap();
        drop(tx);
        go(&provider, ToolRegistry::new(vec![]), &closer, c).await;
        let prompt = provider.proxy_prompt(1);
        assert!(prompt.contains("[user] use SQLite please"), "{prompt}");
        assert!(prompt.contains("The worker's latest reply:\nok, using SQLite"), "{prompt}");
    }

    #[tokio::test]
    async fn esc_pauses_the_proxy_until_the_user_speaks() {
        let hub = TaskHub::new("");
        hub.start_goal("ship it");
        let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
        let provider = Duo::new(
            vec![
                says(json!({"status": "continue", "message": "go"})),
                says(json!({"status": "hand_back", "reason": "r"})),
            ],
            vec![Step::Hang(entered_tx), text("did the step"), text("summary")],
        );
        let (mut c, closer, tx) = task_run(&hub, |n| n >= 2);
        let cancel = CancelFlag::new();
        c.cancel = Some(cancel.clone());
        let (p, cl) = (provider.clone(), closer.clone());
        let job = tokio::spawn(async move { go(&p, ToolRegistry::new(vec![]), &cl, c).await });
        entered_rx.await.unwrap();
        cancel.trip();
        tx.send(UserTurn::from("keep going")).unwrap();
        drop(tx);
        job.await.unwrap();
        let s = hub.snapshot();
        assert_eq!(s.phase, TaskPhase::HandedBack);
        assert!(!s.skip_steer, "the user's message clears the Esc pause");
        assert!(s.proxy_log.iter().any(|t| t.who == "user" && t.text == "keep going"));
        assert_eq!(provider.proxy_calls.lock().unwrap().len(), 2);
        let worker = provider.worker_calls.lock().unwrap();
        let resumed: Vec<&str> = worker[1].input.iter().filter_map(|i| i["content"].as_str()).collect();
        assert!(resumed.iter().any(|t| t.starts_with(crate::notice::INTERRUPT)));
        assert_eq!(resumed.last().copied(), Some("keep going"));
    }

    #[tokio::test]
    async fn a_task_started_mid_turn_gets_a_plan_not_a_check() {
        let hub = TaskHub::new("");
        let provider = Duo::new(
            vec![
                says(json!({"status": "continue", "message": "Here is what I want: tests first."})),
                says(json!({"status": "hand_back", "reason": "r"})),
            ],
            vec![
                tool("c1", "run_command", json!({"command": "ls"})),
                text("listed"),
                text("summary"),
            ],
        );
        let (mut c, closer, tx) = task_run(&hub, |_| true);
        drop(tx);
        c.prompt = "look around".into();
        // The user starts the task while the first command runs.
        struct Start(Arc<TaskHub>);
        impl ClientTool for Start {
            fn spec(&self) -> ToolSpec {
                ToolSpec { name: "run_command".into(), description: "run".into(), parameters: json!({"type": "object"}) }
            }
            fn call(&self, _args: &Value) -> ToolCallFut<'_> {
                self.0.start_goal("add tests");
                Box::pin(async { Ok(json!({"exit_code": 0, "stdout": "a b"}).to_string()) })
            }
        }
        go(&provider, ToolRegistry::new(vec![Box::new(Start(hub.clone()))]), &closer, c).await;
        assert!(provider.proxy_prompt(0).contains("Review kind: plan"));
        assert_eq!(closer.intents(), ["plan", "hand_back"]);
        let worker = provider.worker_calls.lock().unwrap();
        assert!(last_user_text(&worker[1]).contains("tests first"));
    }

    #[tokio::test]
    async fn intake_asks_the_user_before_the_plan() {
        let hub = TaskHub::new("");
        hub.start_goal("make a landing page");
        let ask = crate::ask::AskUserHub::new();
        hub.set_ask_hub(Some(ask.clone()));
        // The user's pick, waiting for the questionnaire.
        ask.answer(crate::ask::encode_answer(&[crate::ask::Picked { id: "opt0".into(), label: "one HTML file".into(), value: None }]));
        let provider = Duo::new(
            vec![
                says(json!({"ask": true, "questions": [{"question": "What do you want at the end?", "options": ["one HTML file", "a React app"]}]})),
                says(json!({"status": "hand_back", "reason": "r"})),
            ],
            vec![text("summary")],
        );
        let (c, closer, tx) = task_run(&hub, |_| true);
        drop(tx);
        go(&provider, ToolRegistry::new(vec![]), &closer, c).await;
        assert_eq!(hub.snapshot().criteria, "用戶補充：What do you want at the end? → one HTML file");
        assert!(provider.proxy_prompt(1).contains("用戶補充：What do you want at the end? → one HTML file"));
        let events = closer.events.lock().unwrap();
        let asked = events
            .iter()
            .position(|e| matches!(e, AgentEvent::AskUser { meta, .. } if meta.agent_name == crate::task::AGENT_NAME && meta.run_id == "run-1"))
            .expect("the proxy asked");
        let first = events.iter().position(|e| matches!(e, AgentEvent::TaskProxy { .. })).unwrap();
        assert!(asked < first, "intake comes before the opening message");
    }

    #[test]
    fn a_kick_started_fold_keeps_the_task_goal() {
        let hub = TaskHub::new("");
        hub.start_goal("做登入");
        let (c, _closer, _tx) = task_run(&hub, |_| true);
        assert_eq!(fold_goal(&c), "做登入");
        let (mut c, _closer, _tx) = task_run(&hub, |_| true);
        c.prompt = "real prompt".into();
        assert_eq!(fold_goal(&c), "real prompt");
    }
}
