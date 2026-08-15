//! Loopback A2A HTTP worker.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde_json::{json, Value};
use tokio::net::TcpListener;
use tokio::sync::Mutex;

use crate::a2a::{self, Handshake};
use crate::error::{Error, Result};
use crate::events::JsonlSink;
use crate::provider::XaiOauthProvider;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum WorkerMode {
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
    pub listen: String,
    pub events: PathBuf,
    pub mode: WorkerMode,
    pub workspace: PathBuf,
    pub model: String,
    pub max_turns: u32,
    pub reasoning_effort: crate::provider::ReasoningEffort,
}

struct WorkerState {
    name: String,
    origin: String,
    mode: WorkerMode,
    cfg: WorkerConfigView,
    tasks: Mutex<HashMap<String, Value>>,
    run_lock: Mutex<()>,
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
    let state = Arc::new(WorkerState {
        name: cfg.name.clone(),
        origin: origin.clone(),
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
        tasks: Mutex::new(HashMap::new()),
        run_lock: Mutex::new(()),
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
    let context_id = a2a::context_id_of(&body)
        .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
    if text.trim().is_empty() {
        let task = a2a::failed_task("empty message", &context_id);
        return (StatusCode::OK, Json(json!({"task": task})));
    }
    let task = match st.mode {
        WorkerMode::Echo => a2a::completed_task(&format!("echo:{text}"), &context_id),
        WorkerMode::Grok => {
            let _g = st.run_lock.lock().await;
            match run_grok(&st, &text).await {
                Ok(reply) => a2a::completed_task(&reply, &context_id),
                Err(e) => a2a::failed_task(&e.to_string(), &context_id),
            }
        }
    };
    if let Some(id) = task.get("id").and_then(Value::as_str) {
        st.tasks.lock().await.insert(id.to_string(), task.clone());
    }
    (StatusCode::OK, Json(json!({"task": task})))
}

async fn get_task(
    State(st): State<Arc<WorkerState>>,
    Path(id): Path<String>,
) -> (StatusCode, Json<Value>) {
    match st.tasks.lock().await.get(&id).cloned() {
        Some(t) => (StatusCode::OK, Json(t)),
        None => (
            StatusCode::NOT_FOUND,
            Json(json!({"error": "TaskNotFoundError", "id": id})),
        ),
    }
}

async fn cancel_task(
    State(st): State<Arc<WorkerState>>,
    Path(id): Path<String>,
) -> (StatusCode, Json<Value>) {
    let mut tasks = st.tasks.lock().await;
    if let Some(t) = tasks.get_mut(&id) {
        t["status"]["state"] = json!(a2a::TASK_CANCELED);
        return (StatusCode::OK, Json(t.clone()));
    }
    (
        StatusCode::NOT_FOUND,
        Json(json!({"error": "TaskNotFoundError"})),
    )
}

async fn run_grok(st: &WorkerState, prompt: &str) -> Result<String> {
    let sink: std::sync::Arc<dyn crate::events::EventSink> =
        std::sync::Arc::new(JsonlSink::create(&st.cfg.events)?);
    let auth = crate::auth::default_auth_path()?;
    let provider = XaiOauthProvider::new(auth, Some(st.cfg.model.clone()))?;
    let out = crate::kit::run_with_nursery(
        &provider,
        sink,
        crate::kit::KernelSpec {
            agent_name: st.name.clone(),
            prompt: prompt.to_string(),
            model: st.cfg.model.clone(),
            max_turns: st.cfg.max_turns,
            workspace: st.cfg.workspace.clone(),
            events_file: st.cfg.events.clone(),
            events_dir: crate::kit::events_dir(&st.cfg.events),
            server_tools: vec![],
            depth: st.cfg.depth,
            parent_run_id: st.cfg.parent_run_id.clone(),
            run_id: String::new(),
            child_mode: std::env::var("GROKA_CHILD_MODE").unwrap_or_else(|_| "grok".into()),
            reasoning_effort: st.cfg.reasoning_effort,
            knobs: None,
            inbox: None,
            images: Vec::new(),
            ask: None,
            cancel: None,
        },
    )
    .await?;
    Ok(out.text)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::a2a::A2aClient;

    #[tokio::test]
    async fn echo_worker_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let bound = bind_worker(WorkerConfig {
            name: "echo".into(),
            depth: 1,
            parent_run_id: None,
            listen: "127.0.0.1:0".into(),
            events: dir.path().join("e.jsonl"),
            mode: WorkerMode::Echo,
            workspace: dir.path().to_path_buf(),
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
        // wait for accept
        tokio::time::sleep(std::time::Duration::from_millis(30)).await;
        let client = A2aClient::new().unwrap();
        let task = client.send_text(&origin, "ping", None).await.unwrap();
        assert_eq!(a2a::task_state(&task), a2a::TASK_COMPLETED);
        assert_eq!(a2a::artifact_text(&task), "echo:ping");
        let card = client
            .fetch_card(&format!("{origin}/.well-known/agent-card.json"))
            .await
            .unwrap();
        assert_eq!(card.name, "echo");
    }
}
