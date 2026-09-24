//! Loopback A2A HTTP worker.
//!
//! A worker hosts one long-lived agent session. The first `message/send`
//! starts it; later messages go into the same session's inbox, so the child
//! keeps its history, its own children, and its backgrounds between messages.
//! Each message is an A2A Task that completes when the session next goes idle.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde_json::{json, Value};
use tokio::net::TcpListener;
use tokio::sync::mpsc;

use crate::a2a::{self, Handshake};
use crate::agent::{CancelFlag, UserTurn};
use crate::error::{Error, Result};
use crate::events::{AgentEvent, EventSink, JsonlSink};
use crate::provider::{
    AnyProvider, CompactRequest, CompactResponse, CompleteRequest, CompleteResponse, Provider,
};

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum WorkerMode {
    /// Test double: a real agent session whose model echoes every user line so far.
    Echo,
    Grok,
}

impl WorkerMode {
    pub fn parse(s: &str) -> Self {
        if s.eq_ignore_ascii_case("echo") {
            Self::Echo
        } else {
            Self::Grok
        }
    }
}

pub struct WorkerConfig {
    pub name: String,
    pub depth: u32,
    pub parent_run_id: Option<String>,
    /// Stable run id for this worker's session. Empty = generate one.
    pub run_id: String,
    pub listen: String,
    pub events: PathBuf,
    pub mode: WorkerMode,
    pub workspace: PathBuf,
    pub model: String,
    /// Per-message turn budget; hitting it pauses the session (0 = unlimited).
    pub max_turns: u32,
    pub reasoning_effort: crate::provider::ReasoningEffort,
}

struct Session {
    inbox: mpsc::UnboundedSender<UserTurn>,
    cancel: CancelFlag,
}

/// Tasks shared between HTTP handlers and the session's event sink.
#[derive(Default)]
struct Book {
    tasks: HashMap<String, Value>,
    /// Task ids that finish when the session next goes idle.
    open: Vec<String>,
    /// Last `stop` text of the current turn chain; cleared on `TurnStarted`.
    last_stop: Option<String>,
    last_error: String,
}

struct WorkerState {
    name: String,
    origin: String,
    run_id: String,
    context_id: String,
    mode: WorkerMode,
    cfg: WorkerConfigView,
    book: Arc<Mutex<Book>>,
    interrupting: Arc<AtomicBool>,
    session: tokio::sync::Mutex<Option<Session>>,
    sink: Arc<dyn EventSink>,
}

#[derive(Clone)]
struct WorkerConfigView {
    parent_run_id: Option<String>,
    events: PathBuf,
    workspace: PathBuf,
    model: String,
    max_turns: u32,
    depth: u32,
    reasoning_effort: crate::provider::ReasoningEffort,
}

pub struct BoundWorker {
    pub listener: TcpListener,
    pub handshake: Handshake,
    pub app: Router,
}

fn lock(book: &Mutex<Book>) -> std::sync::MutexGuard<'_, Book> {
    book.lock().unwrap_or_else(|e| e.into_inner())
}

fn finish_open(book: &mut Book, done: impl Fn(&str) -> Value) {
    for id in std::mem::take(&mut book.open) {
        if let Some(t) = book.tasks.get_mut(&id) {
            let ctx = t
                .get("contextId")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            a2a::finish_task(t, done(&ctx));
        }
    }
}

/// Writes the session's events to JSONL and finishes open tasks when the
/// session's own agent (not a relayed descendant) goes idle.
struct TaskSink {
    inner: Arc<dyn EventSink>,
    run_id: String,
    book: Arc<Mutex<Book>>,
    interrupting: Arc<AtomicBool>,
}

impl EventSink for TaskSink {
    fn emit(&self, event: &AgentEvent) {
        self.inner.emit(event);
        if event.run_id() != self.run_id || !event.path().is_empty() {
            return;
        }
        let mut book = lock(&self.book);
        match event {
            AgentEvent::TurnStarted { .. } => {
                book.last_stop = None;
                book.last_error.clear();
            }
            AgentEvent::ModelFinished { text, finish, .. } if finish == "stop" => {
                book.last_stop = Some(text.clone());
            }
            AgentEvent::Error { message, .. } => book.last_error = message.clone(),
            AgentEvent::AwaitingInput { .. } => {
                if self.interrupting.swap(false, Ordering::SeqCst) {
                    finish_open(&mut book, a2a::canceled_task);
                } else if let Some(text) = book.last_stop.clone() {
                    finish_open(&mut book, |ctx| a2a::completed_task(&text, ctx));
                } else {
                    let msg = if book.last_error.is_empty() {
                        "session paused".to_string()
                    } else {
                        format!("session paused: {}", book.last_error)
                    };
                    finish_open(&mut book, |ctx| a2a::failed_task(&msg, ctx));
                }
            }
            AgentEvent::RunFinished { reason, text, .. } => {
                if reason == "stop" {
                    finish_open(&mut book, |ctx| a2a::completed_task(text, ctx));
                } else {
                    let msg = format!("session ended: {reason}");
                    finish_open(&mut book, |ctx| a2a::failed_task(&msg, ctx));
                }
            }
            _ => {}
        }
    }
}

pub async fn bind_worker(cfg: WorkerConfig) -> Result<BoundWorker> {
    let listener = TcpListener::bind(&cfg.listen)
        .await
        .map_err(|e| Error::A2a(format!("bind {}: {e}", cfg.listen)))?;
    let addr = listener.local_addr()?;
    let origin = format!("http://127.0.0.1:{}", addr.port());
    let handshake = Handshake {
        v: 1,
        agent_card_url: format!("{origin}/.well-known/agent-card.json"),
    };
    let run_id = if cfg.run_id.is_empty() {
        uuid::Uuid::new_v4().to_string()
    } else {
        cfg.run_id.clone()
    };
    let jsonl: Arc<dyn EventSink> = Arc::new(JsonlSink::create(&cfg.events)?);
    let book = Arc::new(Mutex::new(Book::default()));
    let interrupting = Arc::new(AtomicBool::new(false));
    let sink: Arc<dyn EventSink> = Arc::new(TaskSink {
        inner: jsonl,
        run_id: run_id.clone(),
        book: book.clone(),
        interrupting: interrupting.clone(),
    });
    let state = Arc::new(WorkerState {
        name: cfg.name.clone(),
        origin: origin.clone(),
        run_id,
        context_id: uuid::Uuid::new_v4().to_string(),
        mode: cfg.mode,
        cfg: WorkerConfigView {
            parent_run_id: cfg.parent_run_id.clone(),
            events: cfg.events.clone(),
            workspace: cfg.workspace.clone(),
            model: cfg.model.clone(),
            max_turns: cfg.max_turns,
            depth: cfg.depth,
            reasoning_effort: cfg.reasoning_effort,
        },
        book,
        interrupting,
        session: tokio::sync::Mutex::new(None),
        sink,
    });
    let app = Router::new()
        .route("/.well-known/agent-card.json", get(agent_card))
        .route("/message:send", post(message_send))
        .route("/v1/message:send", post(message_send))
        .route("/tasks/{id}", get(get_task))
        .route("/tasks/{id}/cancel", post(cancel_task))
        .with_state(state);
    Ok(BoundWorker {
        listener,
        handshake,
        app,
    })
}

pub async fn serve_worker(bound: BoundWorker) -> Result<()> {
    axum::serve(bound.listener, bound.app)
        .await
        .map_err(|e| Error::A2a(format!("worker serve: {e}")))?;
    Ok(())
}

pub async fn run_worker(cfg: WorkerConfig) -> Result<()> {
    let bound = bind_worker(cfg).await?;
    println!(
        "{}",
        serde_json::to_string(&bound.handshake).expect("handshake json")
    );
    let _ = std::io::Write::flush(&mut std::io::stdout());
    serve_worker(bound).await
}

async fn agent_card(State(st): State<Arc<WorkerState>>) -> Json<a2a::AgentCard> {
    Json(a2a::local_card(&st.name, &st.origin))
}

async fn message_send(
    State(st): State<Arc<WorkerState>>,
    Json(body): Json<Value>,
) -> (StatusCode, Json<Value>) {
    let text = a2a::extract_text(&body);
    let context_id = a2a::context_id_of(&body).unwrap_or_else(|| st.context_id.clone());
    if text.trim().is_empty() {
        let task = a2a::failed_task("empty message", &context_id);
        return (StatusCode::OK, Json(json!({"task": task})));
    }
    let task = a2a::working_task(&context_id);
    let task_id = task
        .get("id")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    {
        let mut book = lock(&st.book);
        book.tasks.insert(task_id.clone(), task.clone());
        book.open.push(task_id);
    }
    let mut session = st.session.lock().await;
    let delivered = session
        .as_ref()
        .is_some_and(|s| s.inbox.send(UserTurn::from(text.clone())).is_ok());
    if !delivered {
        *session = Some(start_session(st.clone(), text));
    }
    (StatusCode::OK, Json(json!({"task": task})))
}

fn start_session(st: Arc<WorkerState>, prompt: String) -> Session {
    let (tx, rx) = mpsc::unbounded_channel();
    let cancel = CancelFlag::new();
    let session = Session {
        inbox: tx,
        cancel: cancel.clone(),
    };
    tokio::spawn(async move {
        let result = match st.mode {
            WorkerMode::Echo => run_session(&st, EchoProvider::from_env(), prompt, rx, cancel).await,
            WorkerMode::Grok => {
                let cfg = crate::config::ProviderConfig::load();
                match AnyProvider::connect(&cfg, Some(st.cfg.model.clone())) {
                    Ok(p) => run_session(&st, p, prompt, rx, cancel).await,
                    Err(e) => Err(e),
                }
            }
        };
        let done: Box<dyn Fn(&str) -> Value + Send> = match result {
            Ok(text) => Box::new(move |ctx| a2a::completed_task(&text, ctx)),
            Err(e) => {
                let msg = e.to_string();
                Box::new(move |ctx| a2a::failed_task(&msg, ctx))
            }
        };
        finish_open(&mut lock(&st.book), done);
        *st.session.lock().await = None;
    });
    session
}

async fn run_session<P: Provider + Clone + 'static>(
    st: &WorkerState,
    provider: P,
    prompt: String,
    inbox: mpsc::UnboundedReceiver<UserTurn>,
    cancel: CancelFlag,
) -> Result<String> {
    let context_window = crate::config::ProviderConfig::load().window_tokens_for(&st.cfg.model);
    let out = crate::kit::run_with_nursery(
        &provider,
        st.sink.clone(),
        crate::kit::KernelSpec {
            agent_name: st.name.clone(),
            prompt,
            model: st.cfg.model.clone(),
            max_turns: st.cfg.max_turns,
            workspace: st.cfg.workspace.clone(),
            events_file: st.cfg.events.clone(),
            events_dir: crate::kit::events_dir(&st.cfg.events),
            server_tools: vec![],
            depth: st.cfg.depth,
            parent_run_id: st.cfg.parent_run_id.clone(),
            run_id: st.run_id.clone(),
            child_mode: std::env::var("GROKA_CHILD_MODE").unwrap_or_else(|_| {
                match st.mode {
                    WorkerMode::Echo => "echo".into(),
                    WorkerMode::Grok => "grok".into(),
                }
            }),
            reasoning_effort: st.cfg.reasoning_effort,
            knobs: None,
            inbox: Some(inbox),
            images: Vec::new(),
            ask: None,
            context_window,
            cancel: Some(cancel),
            skills: None,
            task: None,
        },
    )
    .await?;
    Ok(out.text)
}

async fn get_task(
    State(st): State<Arc<WorkerState>>,
    Path(id): Path<String>,
) -> (StatusCode, Json<Value>) {
    match lock(&st.book).tasks.get(&id).cloned() {
        Some(t) => (StatusCode::OK, Json(t)),
        None => (
            StatusCode::NOT_FOUND,
            Json(json!({"error": "TaskNotFoundError", "id": id})),
        ),
    }
}

/// Cancel interrupts the session's current turn (like Esc). The session stays
/// alive and idle; every task still open is reported canceled.
async fn cancel_task(
    State(st): State<Arc<WorkerState>>,
    Path(id): Path<String>,
) -> (StatusCode, Json<Value>) {
    let known = lock(&st.book).tasks.contains_key(&id);
    if !known {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({"error": "TaskNotFoundError"})),
        );
    }
    let busy = !lock(&st.book).open.is_empty();
    if busy {
        if let Some(s) = st.session.lock().await.as_ref() {
            st.interrupting.store(true, Ordering::SeqCst);
            s.cancel.trip();
        }
    }
    let mut book = lock(&st.book);
    if let Some(pos) = book.open.iter().position(|t| t == &id) {
        book.open.remove(pos);
    }
    let t = book.tasks.get_mut(&id).expect("checked above");
    if !a2a::is_terminal(a2a::task_state(t)) {
        let ctx = t
            .get("contextId")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        a2a::finish_task(t, a2a::canceled_task(&ctx));
    }
    (StatusCode::OK, Json(t.clone()))
}

/// Echo model for tests: replies `echo:` + every user line of the session so
/// far, joined by ` | `, so a test can see that the session kept its history.
/// `GROKA_ECHO_DELAY_MS` delays each reply (cancellable like a real request).
#[derive(Clone)]
pub struct EchoProvider {
    delay: Duration,
}

impl EchoProvider {
    pub fn from_env() -> Self {
        let ms = std::env::var("GROKA_ECHO_DELAY_MS")
            .ok()
            .and_then(|s| s.parse::<u64>().ok())
            .unwrap_or(0);
        Self {
            delay: Duration::from_millis(ms),
        }
    }
}

fn user_lines(input: &[Value]) -> Vec<String> {
    input
        .iter()
        .filter(|v| v.get("role").and_then(Value::as_str) == Some("user"))
        .filter_map(|v| match v.get("content") {
            Some(Value::String(s)) => Some(s.clone()),
            Some(Value::Array(parts)) => Some(
                parts
                    .iter()
                    .filter_map(|p| p.get("text").and_then(Value::as_str))
                    .collect::<Vec<_>>()
                    .join(" "),
            ),
            _ => None,
        })
        .collect()
}

impl Provider for EchoProvider {
    async fn complete(&self, req: CompleteRequest) -> Result<CompleteResponse> {
        if !self.delay.is_zero() {
            tokio::time::sleep(self.delay).await;
        }
        let text = format!("echo:{}", user_lines(&req.input).join(" | "));
        Ok(CompleteResponse {
            output_items: vec![json!({
                "type": "message",
                "role": "assistant",
                "content": [{"type": "output_text", "text": text}],
            })],
            text,
            ..CompleteResponse::new(uuid::Uuid::new_v4().to_string())
        })
    }

    async fn compact(&self, _req: CompactRequest) -> Result<CompactResponse> {
        Err(Error::Provider("echo provider does not compact".into()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::a2a::A2aClient;

    async fn echo_worker(dir: &std::path::Path) -> String {
        let bound = bind_worker(WorkerConfig {
            name: "echo".into(),
            depth: 1,
            parent_run_id: None,
            run_id: String::new(),
            listen: "127.0.0.1:0".into(),
            events: dir.join("e.jsonl"),
            mode: WorkerMode::Echo,
            workspace: dir.to_path_buf(),
            model: "grok-4.6".into(),
            max_turns: 4,
            reasoning_effort: crate::provider::ReasoningEffort::High,
        })
        .await
        .unwrap();
        let origin = bound.handshake.origin().unwrap();
        tokio::spawn(async move {
            let _ = serve_worker(bound).await;
        });
        origin
    }

    #[tokio::test]
    async fn echo_worker_keeps_one_session_across_messages() {
        let dir = tempfile::tempdir().unwrap();
        let origin = echo_worker(dir.path()).await;
        let client = A2aClient::new().unwrap();
        let t1 = client.send_text(&origin, "ping", None).await.unwrap();
        let t1 = client
            .wait_task(&origin, t1, Duration::from_secs(20))
            .await
            .unwrap();
        assert_eq!(a2a::task_state(&t1), a2a::TASK_COMPLETED);
        assert_eq!(a2a::artifact_text(&t1), "echo:ping");
        let t2 = client.send_text(&origin, "pong", None).await.unwrap();
        let t2 = client
            .wait_task(&origin, t2, Duration::from_secs(20))
            .await
            .unwrap();
        assert_eq!(
            a2a::artifact_text(&t2),
            "echo:ping | pong",
            "second message must see the first one"
        );
        let card = client
            .fetch_card(&format!("{origin}/.well-known/agent-card.json"))
            .await
            .unwrap();
        assert_eq!(card.name, "echo");
    }
}
