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
use crate::tools::{ToolRegistry, ToolSpec};

/// Live model / effort / server tools for a long-lived TUI session. Read each turn.
#[derive(Clone)]
pub struct SessionKnobs {
    pub model: String,
    pub reasoning_effort: ReasoningEffort,
    pub send_reasoning: bool,
    pub server_tools: Vec<String>,
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
    /// TUI Esc trips this so an in-flight stream or tool is dropped, then the session pauses.
    pub cancel: Option<CancelFlag>,
}

/// Shared flag so the TUI can abort the current model turn without ending the session.
#[derive(Clone)]
pub struct CancelFlag {
    inner: Arc<AtomicBool>,
    notify: Arc<Notify>,
}

impl CancelFlag {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(AtomicBool::new(false)),
            notify: Arc::new(Notify::new()),
        }
    }

    pub fn trip(&self) {
        self.inner.store(true, Ordering::SeqCst);
        self.notify.notify_waiters();
    }

    pub fn take(&self) -> bool {
        self.inner.swap(false, Ordering::SeqCst)
    }

    pub fn is_set(&self) -> bool {
        self.inner.load(Ordering::SeqCst)
    }

    async fn wait(&self) {
        loop {
            if self.is_set() {
                return;
            }
            self.notify.notified().await;
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

fn meta(agent_name: &str, run_id: &str, parent_run_id: Option<&str>) -> EventMeta {
    EventMeta {
        ts: chrono::Utc::now(),
        agent_name: agent_name.to_string(),
        run_id: run_id.to_string(),
        parent_run_id: parent_run_id.map(str::to_string),
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
    });
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

const INTERRUPT_NOTE: &str = "The user pressed Esc and interrupted the previous model turn. Do not continue that turn. Follow their next message.";

fn cancelled_tool_output() -> String {
    json!({"error": "interrupted", "cancelled": true}).to_string()
}

/// Wait for the next user turn or background-exit notice. Closing the user inbox ends the session.
/// Esc while paused is ignored so it cannot cancel the next turn.
async fn wait_session(cfg: &mut RunConfig) -> Option<Vec<UserTurn>> {
    if cfg.inbox.is_none() {
        return None;
    }
    loop {
        if let Some(c) = &cfg.cancel {
            c.take();
        }
        let mut msgs = drain_session(cfg);
        if !msgs.is_empty() {
            return Some(msgs);
        }
        tokio::select! {
            user = recv_pending(&mut cfg.inbox) => {
                let Some(msg) = user else {
                    return None;
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
            _ = wait_if_cancel(&cfg.cancel) => {
                if let Some(c) = &cfg.cancel {
                    c.take();
                }
            }
        }
    }
}

/// Pause after Esc. `None` = session ended.
async fn after_interrupt(
    cfg: &mut RunConfig,
    sink: &dyn EventSink,
    run_id: &str,
    last_text: &str,
) -> Option<Vec<UserTurn>> {
    if let Some(c) = &cfg.cancel {
        c.take();
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
        return None;
    }
    sink.emit(&AgentEvent::AwaitingInput {
        meta: meta(&cfg.agent_name, run_id, cfg.parent_run_id.as_deref()),
    });
    wait_session(cfg).await
}

fn resume_after_interrupt(
    cfg: &RunConfig,
    history: &mut Vec<Value>,
    pending: &mut Vec<Value>,
    msgs: Vec<UserTurn>,
) {
    append_user(
        history,
        pending,
        &UserTurn::from(INTERRUPT_NOTE),
        &cfg.workspace,
    );
    for msg in msgs {
        append_user(history, pending, &msg, &cfg.workspace);
    }
}

/// `true` = keep the session. `false` = run ended.
async fn take_interrupt(
    cfg: &mut RunConfig,
    sink: &dyn EventSink,
    run_id: &str,
    last_text: &str,
    history: &mut Vec<Value>,
    pending: &mut Vec<Value>,
) -> bool {
    match after_interrupt(cfg, sink, run_id, last_text).await {
        None => false,
        Some(msgs) => {
            resume_after_interrupt(cfg, history, pending, msgs);
            true
        }
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

/// Stop retrying the same failing request so we do not hammer the API.
const MAX_CONSECUTIVE_PROVIDER_ERRORS: u32 = 3;
/// Lived fold is retried until it succeeds. Wait so a 400 does not spin.
const COMPACT_RETRY: Duration = Duration::from_secs(2);

/// Tell the model why the request failed and keep the loop going.
/// Do not pause for the user: the model should enlarge/regenerate and continue.
async fn recover_from_provider_error(
    cfg: &mut RunConfig,
    sink: &dyn EventSink,
    run_id: &str,
    history: &mut Vec<Value>,
    pending: &mut Vec<Value>,
    consecutive: &mut u32,
    last_text: &str,
    err: Error,
) -> Result<()> {
    *consecutive += 1;
    sink.emit(&AgentEvent::Error {
        meta: meta(&cfg.agent_name, run_id, cfg.parent_run_id.as_deref()),
        message: err.to_string(),
    });
    crate::vision::strip_attached_images(history);
    crate::vision::strip_attached_images(pending);
    let note = format!(
        "The previous model request failed ({err}). Continue the task yourself. If an image was rejected for being below {} pixels, enlarge or regenerate the file in the workspace then read_image again. Do not wait for the user and do not ask the user to upscale it.",
        crate::vision::MIN_VISION_PIXELS
    );
    append_user(history, pending, &UserTurn::from(note), &cfg.workspace);
    *pending = history.clone();

    if *consecutive < MAX_CONSECUTIVE_PROVIDER_ERRORS {
        return Ok(());
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
    sink.emit(&AgentEvent::AwaitingInput {
        meta: meta(&cfg.agent_name, run_id, cfg.parent_run_id.as_deref()),
    });
    match wait_session(cfg).await {
        Some(msgs) => {
            for msg in msgs {
                append_user(history, pending, &msg, &cfg.workspace);
            }
            Ok(())
        }
        None => {
            sink.emit(&AgentEvent::RunFinished {
                meta: meta(&cfg.agent_name, run_id, cfg.parent_run_id.as_deref()),
                reason: "error".into(),
                text: last_text.to_string(),
            });
            Err(err)
        }
    }
}

fn emit_file_changes(
    sink: &dyn EventSink,
    agent_name: &str,
    run_id: &str,
    parent_run_id: Option<&str>,
    output: &str,
) {
    let Ok(v) = serde_json::from_str::<Value>(output) else {
        return;
    };
    let mut items: Vec<Value> = Vec::new();
    if v.get("path").and_then(Value::as_str).is_some() && v.get("diff").is_some() {
        items.push(v);
    } else if let Some(files) = v.get("files").and_then(Value::as_array) {
        items.extend(files.iter().cloned());
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
    }
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

/// The working model writes long-term memory because it lived the turns.
/// Not streamed to the TUI and not kept as a chat turn. `store: false` and no
/// `previous_response_id`. Uses the working conversation cache key so the
/// fold stays on the same replica; the body is still a different prefix.
async fn ask_lived_memory<P: Provider>(
    provider: &P,
    instructions: &str,
    goal: &str,
    model: &str,
    effort: ReasoningEffort,
    send_reasoning: bool,
    cache_key: &str,
    head: &[Value],
    client_tools: Vec<ToolSpec>,
    server_tools: Vec<String>,
) -> Result<String> {
    if head.is_empty() {
        return Err(Error::Provider("nothing to fold".into()));
    }
    let req = CompleteRequest {
        instructions: instructions.to_string(),
        input: compact::fold_input(head, goal),
        client_tools,
        server_tools,
        cache_key: cache_key.to_string(),
        previous_response_id: None,
        store: false,
        reasoning_effort: effort,
        send_reasoning,
        model: model.to_string(),
        tool_choice: Some("none".into()),
    };
    let r = provider.complete(req).await?;
    if !r.function_calls.is_empty() {
        return Err(Error::Provider("lived fold called tools".into()));
    }
    let text = compact::lived_text_from_complete(&r.text, &r.output_items);
    if compact::lived_brief_usable(&text) {
        Ok(text)
    } else {
        Err(Error::Provider("lived gist unusable".into()))
    }
}

pub async fn run<P: Provider>(
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
    let keep_recent = if cfg.compact_keep_recent == 0 {
        compact::DEFAULT_KEEP_RECENT
    } else {
        cfg.compact_keep_recent
    };

    let mut history = Vec::new();
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
    history.push(crate::vision::user_message(
        &cfg.prompt,
        &cfg.images,
        &cfg.workspace,
    ));
    let mut pending: Vec<Value> = history.clone();
    let mut last_text = String::new();
    let mut last_usage = CacheUsage::default();
    let mut cache_turns = Vec::new();
    let mut compacted = 0u32;
    let max_turns = cfg.max_turns;
    let mut cache_key = prompt_cache_key(&start_model, &tools.specs(), &start_tools, &run_id);
    let mut last_model = start_model;
    let mut last_server_tools = start_tools;
    let mut turn: u32 = 0;
    let mut total_turns: u32 = 0;
    let mut seen_model = false;
    let mut consecutive_provider_errors: u32 = 0;

    'run: loop {
        if seen_model {
            for msg in drain_session(&mut cfg) {
                append_user(&mut history, &mut pending, &msg, &cfg.workspace);
                turn = 0;
            }
        }

        if cfg.cancel.as_ref().is_some_and(CancelFlag::is_set) {
            if !take_interrupt(
                &mut cfg,
                sink,
                &run_id,
                &last_text,
                &mut history,
                &mut pending,
            )
            .await
            {
                return Ok(RunOutcome {
                    run_id,
                    text: last_text,
                    turns: total_turns,
                    cache_turns,
                    compacted,
                });
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
                sink.emit(&AgentEvent::AwaitingInput {
                    meta: meta(&cfg.agent_name, &run_id, cfg.parent_run_id.as_deref()),
                });
                match wait_session(&mut cfg).await {
                    Some(msgs) if !msgs.is_empty() => {
                        for msg in msgs {
                            append_user(&mut history, &mut pending, &msg, &cfg.workspace);
                        }
                        continue;
                    }
                    Some(_) => continue,
                    None => {
                        sink.emit(&AgentEvent::RunFinished {
                            meta: meta(&cfg.agent_name, &run_id, cfg.parent_run_id.as_deref()),
                            reason: "max_turns".into(),
                            text: last_text,
                        });
                        return Err(err);
                    }
                }
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
            cache_key = prompt_cache_key(&model, &tools.specs(), &server_tools, &run_id);
            pending = history.clone();
            last_model = model.clone();
            last_server_tools = server_tools.clone();
        }

        sink.emit(&AgentEvent::TurnStarted {
            meta: meta(&cfg.agent_name, &run_id, cfg.parent_run_id.as_deref()),
            turn,
        });

        let used = last_usage
            .input_tokens
            .max(compact::estimate_tokens(&cfg.instructions, &history));
        if compact::should_compact(used, window, history.len(), keep_recent) {
            let head = compact::split_head_tail(&history, keep_recent).0.to_vec();
            loop {
                match race_cancel(
                    &cfg.cancel,
                    ask_lived_memory(
                        provider,
                        &cfg.instructions,
                        &cfg.prompt,
                        &model,
                        effort,
                        send_reasoning,
                        &cache_key,
                        &head,
                        tools.specs(),
                        server_tools.clone(),
                    ),
                )
                .await
                {
                    Ok(Ok(text)) => {
                        match compact::compact_history(&cfg.prompt, &history, keep_recent, &text) {
                            Ok(c) => {
                                compacted += 1;
                                sink.emit(&AgentEvent::ContextCompacted {
                                    meta: meta(
                                        &cfg.agent_name,
                                        &run_id,
                                        cfg.parent_run_id.as_deref(),
                                    ),
                                    input_tokens: used,
                                    window,
                                    dropped_items: c.dropped,
                                    kept_items: c.kept,
                                    method: c.method,
                                });
                                history = c.items;
                                pending = history.clone();
                                break;
                            }
                            Err(e) => {
                                notice(
                                    sink,
                                    &cfg.agent_name,
                                    &run_id,
                                    cfg.parent_run_id.as_deref(),
                                    format!("壓縮重試: {e}"),
                                );
                            }
                        }
                    }
                    Ok(Err(e)) => {
                        notice(
                            sink,
                            &cfg.agent_name,
                            &run_id,
                            cfg.parent_run_id.as_deref(),
                            format!("壓縮重試: {e}"),
                        );
                    }
                    Err(()) => {
                        if !take_interrupt(
                            &mut cfg,
                            sink,
                            &run_id,
                            &last_text,
                            &mut history,
                            &mut pending,
                        )
                        .await
                        {
                            return Ok(RunOutcome {
                                run_id,
                                text: last_text,
                                turns: total_turns,
                                cache_turns,
                                compacted,
                            });
                        }
                        turn = 0;
                        continue 'run;
                    }
                }
                match race_cancel(&cfg.cancel, tokio::time::sleep(COMPACT_RETRY)).await {
                    Ok(()) => {}
                    Err(()) => {
                        if !take_interrupt(
                            &mut cfg,
                            sink,
                            &run_id,
                            &last_text,
                            &mut history,
                            &mut pending,
                        )
                        .await
                        {
                            return Ok(RunOutcome {
                                run_id,
                                text: last_text,
                                turns: total_turns,
                                cache_turns,
                                compacted,
                            });
                        }
                        turn = 0;
                        continue 'run;
                    }
                }
            }
        }

        // Always resend the full append-only transcript. Chaining with
        // `previous_response_id` and a pending-only `input` makes every tool
        // turn start with a different prefix, so cached_tokens stays 0.
        let has_image = crate::vision::input_has_image(&history);
        let req = CompleteRequest {
            instructions: cfg.instructions.clone(),
            input: history.clone(),
            client_tools: tools.specs(),
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
        let on_text = |delta: &str| {
            if delta.is_empty() {
                return;
            }
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
            sink.emit(&AgentEvent::ReasoningDelta {
                meta: meta(&agent_name, &rid, parent.as_deref()),
                text: delta.to_string(),
            });
        };

        let response = match race_cancel(
            &cfg.cancel,
            provider.complete_stream(req, &on_text, &on_server, &on_reasoning),
        )
        .await
        {
            Err(()) => {
                if !take_interrupt(
                    &mut cfg,
                    sink,
                    &run_id,
                    &last_text,
                    &mut history,
                    &mut pending,
                )
                .await
                {
                    return Ok(RunOutcome {
                        run_id,
                        text: last_text,
                        turns: total_turns,
                        cache_turns,
                        compacted,
                    });
                }
                turn = 0;
                continue;
            }
            Ok(Ok(r)) => r,
            Ok(Err(e)) => {
                recover_from_provider_error(
                    &mut cfg,
                    sink,
                    &run_id,
                    &mut history,
                    &mut pending,
                    &mut consecutive_provider_errors,
                    &last_text,
                    e,
                )
                .await?;
                continue;
            }
        };
        consecutive_provider_errors = 0;

        last_usage = response.usage.clone();
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
                let output = match race_cancel(&cfg.cancel, tools.call(&call.name, &args)).await {
                    Ok(Ok(o)) => o,
                    Ok(Err(e)) => json!({"error": e.to_string()}).to_string(),
                    Err(()) => {
                        interrupted = true;
                        cancelled_tool_output()
                    }
                };
                sink.emit(&AgentEvent::ToolFinished {
                    meta: meta(&cfg.agent_name, &run_id, cfg.parent_run_id.as_deref()),
                    call_id: call.call_id.clone(),
                    name: call.name.clone(),
                    output: output.clone(),
                });
                if interrupted {
                    let item = crate::vision::function_call_output(&call.call_id, &output, None);
                    history.push(item.clone());
                    pending.push(item);
                    continue;
                }
                emit_file_changes(
                    sink,
                    &cfg.agent_name,
                    &run_id,
                    cfg.parent_run_id.as_deref(),
                    &output,
                );
                let image_uri = match crate::vision::data_uri_for_tool(&cfg.workspace, &output) {
                    Ok(u) => u,
                    Err(e) => {
                        notice(
                            sink,
                            &cfg.agent_name,
                            &run_id,
                            cfg.parent_run_id.as_deref(),
                            format!("圖片附加略過: {e}"),
                        );
                        None
                    }
                };
                let item =
                    crate::vision::function_call_output(&call.call_id, &output, image_uri.as_deref());
                history.push(item.clone());
                pending.push(item);
            }
            if interrupted {
                if !take_interrupt(
                    &mut cfg,
                    sink,
                    &run_id,
                    &last_text,
                    &mut history,
                    &mut pending,
                )
                .await
                {
                    return Ok(RunOutcome {
                        run_id,
                        text: last_text,
                        turns: total_turns,
                        cache_turns,
                        compacted,
                    });
                }
                turn = 0;
            }
            continue;
        }

        if has_image {
            crate::vision::strip_attached_images(&mut history);
        }

        last_text = response.text;
        emit_model_finished(
            sink,
            &cfg.agent_name,
            &run_id,
            cfg.parent_run_id.as_deref(),
            last_text.clone(),
            "stop",
            &last_usage,
        );

        let extra = drain_session(&mut cfg);
        if !extra.is_empty() {
            for msg in extra {
                append_user(&mut history, &mut pending, &msg, &cfg.workspace);
            }
            turn = 0;
            continue;
        }

        if cfg.inbox.is_none() {
            sink.emit(&AgentEvent::RunFinished {
                meta: meta(&cfg.agent_name, &run_id, cfg.parent_run_id.as_deref()),
                reason: "stop".into(),
                text: last_text.clone(),
            });
            return Ok(RunOutcome {
                run_id,
                text: last_text,
                turns: total_turns,
                cache_turns,
                compacted,
            });
        }

        sink.emit(&AgentEvent::AwaitingInput {
            meta: meta(&cfg.agent_name, &run_id, cfg.parent_run_id.as_deref()),
        });
        match wait_session(&mut cfg).await {
            Some(msgs) if !msgs.is_empty() => {
                for msg in msgs {
                    append_user(&mut history, &mut pending, &msg, &cfg.workspace);
                }
                turn = 0;
            }
            Some(_) => {
                turn = 0;
            }
            None => {
                sink.emit(&AgentEvent::RunFinished {
                    meta: meta(&cfg.agent_name, &run_id, cfg.parent_run_id.as_deref()),
                    reason: "stop".into(),
                    text: last_text.clone(),
                });
                return Ok(RunOutcome {
                    run_id,
                    text: last_text,
                    turns: total_turns,
                    cache_turns,
                    compacted,
                });
            }
        }
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
            cancel: None,
        }
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
                        cached_tokens: 0,
                    },
                    ..CompleteResponse::new("1")
                },
                CompleteResponse {
                    text: "ok".into(),
                    usage: CacheUsage {
                        input_tokens: 1000,
                        cached_tokens: 10,
                    },
                    ..CompleteResponse::new("2")
                },
            ])),
            compact_calls: Mutex::new(Vec::new()),
            compact_ok: false,
        };
        let tools = ToolRegistry::new(vec![Box::new(NowTool)]);
        let rec = Rec(Mutex::new(Vec::new()));
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
                input_tokens: 60,
                cached_tokens: 0,
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
                        cached_tokens: 18,
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
        config.context_window = 100;
        config.compact_keep_recent = 2;
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
                input_tokens: 60,
                cached_tokens: 0,
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
        config.context_window = 100;
        config.compact_keep_recent = 2;
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
                input_tokens: 60,
                cached_tokens: 0,
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
        config.context_window = 100;
        config.compact_keep_recent = 2;
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
}
