//! Loopback web mirror of the TUI: one App writer, deltas over WebSocket.
//!
//! The TUI publishes a small shell (header, composer, sessions, overlays),
//! the workbench logs, and row patches per transcript view (`""` = main chat,
//! `coder/lint` = a child agent). The hub keeps a full mirror so a new client,
//! or one that missed a delta, gets a complete `hello`.

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Query, State};
use axum::http::{header, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::Router;
use rust_embed::RustEmbed;
use serde::{Deserialize, Serialize};
use tokio::net::TcpListener;
use tokio::sync::{broadcast, mpsc};
use uuid::Uuid;

use crate::error::{Error, Result};
use crate::md;
use crate::tools;

const MEDIA_MAX: u64 = 20 * 1024 * 1024;

#[derive(RustEmbed)]
#[folder = "web/"]
struct Assets;

#[derive(Clone)]
struct HubState {
    token: String,
    cmd_tx: mpsc::UnboundedSender<UiCommand>,
    out: broadcast::Sender<String>,
    mirror: Arc<Mutex<Mirror>>,
    workspace: Arc<Mutex<PathBuf>>,
}

/// Everything a client needs to render from scratch.
#[derive(Default)]
struct Mirror {
    seq: u64,
    snapshot: Option<UiSnapshot>,
    snapshot_json: String,
    logs: Option<UiLogs>,
    logs_json: String,
    views: BTreeMap<String, Vec<UiRow>>,
}

impl Mirror {
    fn hello(&self) -> Option<String> {
        let snapshot = self.snapshot.clone()?;
        let msg = ServerMsg::Hello {
            seq: self.seq,
            snapshot,
            logs: self.logs.clone().unwrap_or_default(),
            views: self
                .views
                .iter()
                .map(|(path, rows)| UiView {
                    path: path.clone(),
                    rows: rows.clone(),
                })
                .collect(),
        };
        serde_json::to_string(&msg).ok()
    }
}

pub struct Hub {
    pub url: String,
    pub cmd_rx: mpsc::UnboundedReceiver<UiCommand>,
    handle: HubHandle,
}

#[derive(Clone)]
struct HubHandle {
    out: broadcast::Sender<String>,
    mirror: Arc<Mutex<Mirror>>,
    workspace: Arc<Mutex<PathBuf>>,
}

impl Hub {
    /// Apply an update to the mirror and broadcast only what changed.
    pub fn publish(
        &self,
        snapshot: UiSnapshot,
        logs: UiLogs,
        patches: Vec<UiViewPatch>,
        removed: Vec<String>,
    ) {
        let Ok(mut m) = self.handle.mirror.lock() else {
            return;
        };
        let snap_json = serde_json::to_string(&snapshot).unwrap_or_default();
        let logs_json = serde_json::to_string(&logs).unwrap_or_default();
        let snap_changed = snap_json != m.snapshot_json;
        let logs_changed = logs_json != m.logs_json;
        if !snap_changed && !logs_changed && patches.is_empty() && removed.is_empty() {
            return;
        }
        m.seq += 1;
        for p in &patches {
            let rows = m.views.entry(p.path.clone()).or_default();
            rows.truncate(p.from.min(rows.len()));
            rows.extend(p.rows.iter().cloned());
            rows.truncate(p.len);
        }
        for path in &removed {
            m.views.remove(path);
        }
        let msg = ServerMsg::Delta {
            seq: m.seq,
            snapshot: snap_changed.then(|| snapshot.clone()),
            logs: logs_changed.then(|| logs.clone()),
            views: patches,
            removed,
        };
        m.snapshot = Some(snapshot);
        m.snapshot_json = snap_json;
        m.logs = Some(logs);
        m.logs_json = logs_json;
        let json = serde_json::to_string(&msg).unwrap_or_else(|_| "{}".into());
        drop(m);
        let _ = self.handle.out.send(json);
    }

    pub fn set_workspace(&self, path: PathBuf) {
        if let Ok(mut g) = self.handle.workspace.lock() {
            *g = path;
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServerMsg {
    /// Full state: sent on connect and on a client's `resync`.
    Hello {
        seq: u64,
        snapshot: UiSnapshot,
        logs: UiLogs,
        views: Vec<UiView>,
    },
    /// Changes since `seq - 1`. A client that sees a gap asks for `resync`.
    Delta {
        seq: u64,
        #[serde(skip_serializing_if = "Option::is_none", default)]
        snapshot: Option<UiSnapshot>,
        #[serde(skip_serializing_if = "Option::is_none", default)]
        logs: Option<UiLogs>,
        #[serde(skip_serializing_if = "Vec::is_empty", default)]
        views: Vec<UiViewPatch>,
        #[serde(skip_serializing_if = "Vec::is_empty", default)]
        removed: Vec<String>,
    },
}

/// A transcript view: `""` = main chat, otherwise a child agent's path.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct UiView {
    pub path: String,
    pub rows: Vec<UiRow>,
}

/// Replace rows `from..` of a view with `rows`; the view then has `len` rows.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct UiViewPatch {
    pub path: String,
    pub from: usize,
    pub len: usize,
    pub rows: Vec<UiRow>,
}

/// Workbench panels: agent tree, tool timeline, events, backgrounds.
#[derive(Clone, Debug, Serialize, Deserialize, Default)]
pub struct UiLogs {
    pub agents: Vec<UiAgent>,
    pub tools: Vec<UiToolEntry>,
    pub events: Vec<UiEventEntry>,
    pub rail: UiRail,
    pub changes: Vec<UiChange>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct UiAgent {
    pub path: String,
    pub name: String,
    pub depth: usize,
    pub model: String,
    pub state: String,
    pub label: String,
    pub activity: String,
    pub alive: bool,
    pub turn: u32,
    pub tools: u32,
    pub prompt: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct UiToolEntry {
    pub path: String,
    pub call_id: String,
    pub name: String,
    pub line: String,
    pub phase: String,
    pub done: bool,
    pub ms: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct UiEventEntry {
    pub at: String,
    pub path: String,
    pub kind: String,
    pub text: String,
}

/// One file change and where its tool call sits (`path` view, row, call).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct UiChange {
    pub view: String,
    pub row: usize,
    pub call: usize,
    pub path: String,
    pub kind: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, Default)]
pub struct UiSnapshot {
    pub session_id: String,
    pub header: UiHeader,
    pub composer: UiComposer,
    pub sessions: Vec<UiSession>,
    pub queue: Vec<UiQueued>,
    pub pending: Vec<String>,
    pub send_mode: String,
    pub settings: Option<UiSettings>,
    #[serde(default)]
    pub settings_data: Option<UiSettings>,
    pub ask: Option<UiAsk>,
    pub picker: Option<UiPicker>,
    pub inspector: Option<UiInspector>,
    pub image_view: Option<String>,
    pub tool_panel: Option<UiToolPanel>,
    pub skill_view: Option<UiSkillView>,
    pub rename: Option<UiRename>,
    pub task: Option<UiTask>,
    #[serde(default)]
    pub task_summary: UiTask,
    #[serde(default)]
    pub receipts: Vec<(String, String)>,
    pub web_url: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, Default)]
pub struct UiHeader {
    pub model: String,
    pub effort: String,
    pub status: String,
    pub activity: String,
    pub cache: String,
    pub running: bool,
    pub awaiting: bool,
    pub logged_in: bool,
    pub elapsed_ms: u64,
    pub tick: u8,
    pub workspace: String,
    #[serde(default)]
    pub task_live: bool,
    /// `xai` or `openai` — which connection mode Settings last chose.
    #[serde(default)]
    pub kind: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, Default)]
pub struct UiComposer {
    pub text: String,
    pub caret: usize,
    pub seq: u64,
    pub echo_seq: u64,
    pub queue_edit: Option<usize>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct UiRow {
    pub kind: String,
    pub html: String,
    pub text: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expanded: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub done: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub elapsed_ms: Option<u64>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub images: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub calls: Vec<UiToolCall>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct UiToolCall {
    #[serde(default)]
    pub call_id: String,
    pub name: String,
    pub phase: String,
    pub done: bool,
    pub args: String,
    pub output: String,
    pub files: Vec<UiFileChange>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct UiFileChange {
    pub path: String,
    pub kind: String,
    pub diff_html: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct UiSession {
    pub status: String,
    pub id: String,
    pub name: String,
    pub short_id: String,
    pub folder: String,
    pub current: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct UiQueued {
    pub text: String,
    pub images: usize,
}

#[derive(Clone, Debug, Serialize, Deserialize, Default)]
pub struct UiRail {
    pub monitors: Vec<UiMon>,
    pub backgrounds: Vec<UiBg>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct UiMon {
    pub name: String,
    pub command: String,
    pub pid: u32,
    pub status: String,
    pub alive: bool,
    pub detail: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct UiBg {
    pub name: String,
    pub command: String,
    pub pid: u32,
    pub status: String,
    pub alive: bool,
    pub detail: String,
    pub log: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct UiSettings {
    pub field: String,
    pub login: String,
    pub login_url: Option<String>,
    pub login_code: Option<String>,
    pub models: Vec<(String, String)>,
    pub efforts: Vec<(String, String)>,
    pub web_search: bool,
    #[serde(default)]
    pub dispatcher: bool,
    /// Default model for child agents; empty follows the main model.
    #[serde(default)]
    pub child_model: String,
    /// The custom endpoint listed its models, so pickers can be dropdowns.
    #[serde(default)]
    pub custom_models: bool,
    pub import_claude: bool,
    pub import_codex: bool,
    #[serde(default)]
    pub kind: String,
    #[serde(default)]
    pub base_url: String,
    #[serde(default)]
    pub api_key: String,
    #[serde(default)]
    pub context: String,
    pub skills: Vec<UiSkill>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct UiSkill {
    pub name: String,
    pub origin: String,
    pub enabled: bool,
    pub description: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct UiAsk {
    pub prompt: String,
    pub allow_multiple: bool,
    pub options: Vec<UiAskOpt>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct UiAskOpt {
    pub id: String,
    pub label: String,
    pub input: bool,
    pub chosen: bool,
    pub value: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct UiPicker {
    pub path: String,
    pub notice: Option<String>,
    pub cursor: usize,
    pub entries: Vec<UiPickEntry>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct UiPickEntry {
    pub name: String,
    pub is_dir: bool,
    pub is_parent: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct UiInspector {
    pub kind: String,
    pub name: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct UiToolPanel {
    pub group: usize,
    pub item: usize,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct UiSkillView {
    pub title: String,
    pub origin: String,
    pub body: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct UiRename {
    pub id: String,
    pub text: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct UiTaskItem {
    pub text: String,
    pub done: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize, Default)]
pub struct UiTask {
    #[serde(default)]
    pub note: String,
    pub mode: String,
    pub goal: String,
    pub draft: String,
    pub phase: String,
    pub checklist: Vec<UiTaskItem>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum UiCommand {
    RefreshSettings,
    StartTask { session_id: String, goal: String },
    Rename { id: String, text: String },
    UpdateQueue { session_id: String, request_id: String, index: usize, expected: String, text: String },
    SubmitText { session_id: String, request_id: String, text: String, insert: bool },
    SetComposer { text: String, caret: usize, seq: u64 },
    Submit { insert: bool },
    Interrupt,
    SetSendMode { mode: String },
    PasteImage,
    PasteText { text: String },
    RemovePending { index: usize },
    ToggleExpand { index: usize },
    OpenTool { group: usize, item: usize },
    CloseTool,
    OpenSettings,
    CloseSettings,
    OpenTask,
    CloseTask,
    SetTaskDraft { text: String },
    SubmitTask,
    EndTask,
    Login,
    Logout,
    SetProviderKind { kind: String },
    SetEndpoint { text: String },
    SetApiKey { text: String },
    SetContext { text: String },
    SetModel { id: String },
    SetChildModel { id: String },
    SetEffort { id: String },
    ToggleSearch,
    ToggleDispatcher,
    ToggleImportClaude,
    ToggleImportCodex,
    ToggleSkill { index: usize },
    OpenSkill { index: usize },
    CloseSkill,
    NewChat,
    Switch { id: String },
    BeginRename { id: String },
    CommitRename { text: String },
    CancelRename,
    DeleteSession { id: String },
    AskToggle { index: usize },
    AskFill { index: usize, text: String },
    AskConfirm,
    AskCancel,
    WsSetPath { text: String },
    WsSelect { index: usize },
    WsConfirm,
    WsCancel,
    WsCreate,
    WsEnter,
    OpenMonitor { name: String },
    OpenBackground { name: String },
    CloseInspector,
    OpenImage { path: String },
    CloseImage,
    EditQueue { index: usize },
    CancelQueueEdit,
    CommitQueueEdit,
}

#[derive(Deserialize)]
struct TokenQ {
    #[serde(default)]
    t: String,
}

pub fn enabled() -> bool {
    !env_flag("GROKA_NO_WEB")
}

pub async fn start(workspace: PathBuf) -> Result<Option<Hub>> {
    if !enabled() {
        return Ok(None);
    }
    let token = Uuid::new_v4().simple().to_string();
    let port = std::env::var("GROKA_WEB_PORT")
        .ok()
        .and_then(|s| s.parse::<u16>().ok())
        .unwrap_or(0);
    let listener = TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], port)))
        .await
        .map_err(|e| Error::Io(e))?;
    let addr = listener.local_addr().map_err(Error::Io)?;
    let url = format!("http://127.0.0.1:{}/?t={token}", addr.port());
    let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
    let (out, _) = broadcast::channel(256);
    let mirror = Arc::new(Mutex::new(Mirror::default()));
    let workspace = Arc::new(Mutex::new(workspace));
    let state = HubState {
        token: token.clone(),
        cmd_tx,
        out: out.clone(),
        mirror: mirror.clone(),
        workspace: workspace.clone(),
    };
    let app = Router::new()
        .route("/ws", get(ws_upgrade))
        .route("/media", get(media))
        .fallback(static_file)
        .with_state(state);
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    if !env_flag("GROKA_NO_WEB_OPEN") {
        open_browser(&url);
    }
    Ok(Some(Hub {
        url,
        cmd_rx,
        handle: HubHandle {
            out,
            mirror,
            workspace,
        },
    }))
}

fn is_resync(text: &str) -> bool {
    serde_json::from_str::<serde_json::Value>(text)
        .ok()
        .and_then(|v| v.get("type").and_then(|t| t.as_str()).map(|t| t == "resync"))
        .unwrap_or(false)
}

fn env_flag(name: &str) -> bool {
    matches!(
        std::env::var(name).as_deref(),
        Ok("1") | Ok("true") | Ok("TRUE") | Ok("yes")
    )
}

fn token_ok(got: &str, want: &str) -> bool {
    !want.is_empty() && got == want
}

async fn static_file(uri: Uri) -> Response {
    let path = uri.path().trim_start_matches('/');
    let path = if path.is_empty() { "index.html" } else { path };
    if path.contains("..") {
        return StatusCode::NOT_FOUND.into_response();
    }
    match Assets::get(path) {
        Some(f) => {
            let mime = mime_of(path);
            (
                [(header::CONTENT_TYPE, mime), (header::CACHE_CONTROL, "no-store")],
                f.data.into_owned(),
            )
                .into_response()
        }
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

fn mime_of(path: &str) -> &'static str {
    if path.ends_with(".js") {
        "application/javascript; charset=utf-8"
    } else if path.ends_with(".css") {
        "text/css; charset=utf-8"
    } else if path.ends_with(".svg") {
        "image/svg+xml"
    } else {
        "text/html; charset=utf-8"
    }
}

async fn ws_upgrade(
    ws: WebSocketUpgrade,
    Query(q): Query<TokenQ>,
    State(st): State<HubState>,
) -> Response {
    if !token_ok(&q.t, &st.token) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    ws.on_upgrade(move |socket| client_loop(socket, st))
}

fn mirror_hello(st: &HubState) -> Option<String> {
    st.mirror.lock().ok().and_then(|m| m.hello())
}

async fn client_loop(mut socket: WebSocket, st: HubState) {
    // Subscribe before reading the mirror so no delta falls in between.
    let mut rx = st.out.subscribe();
    if let Some(hello) = mirror_hello(&st) {
        if socket.send(Message::text(hello)).await.is_err() {
            return;
        }
    }
    loop {
        tokio::select! {
            msg = socket.recv() => {
                match msg {
                    Some(Ok(Message::Text(t))) => {
                        if is_resync(t.as_str()) {
                            if let Some(hello) = mirror_hello(&st) {
                                if socket.send(Message::text(hello)).await.is_err() {
                                    break;
                                }
                            }
                            continue;
                        }
                        if let Ok(cmd) = serde_json::from_str::<UiCommand>(t.as_str()) {
                            if st.cmd_tx.send(cmd).is_err() {
                                break;
                            }
                        }
                    }
                    Some(Ok(Message::Close(_))) | None => break,
                    Some(Err(_)) => break,
                    _ => {}
                }
            }
            out = rx.recv() => {
                match out {
                    Ok(json) => {
                        if socket.send(Message::text(json)).await.is_err() {
                            break;
                        }
                    }
                    // Missed deltas: send the full state instead.
                    Err(broadcast::error::RecvError::Lagged(_)) => {
                        if let Some(hello) = mirror_hello(&st) {
                            if socket.send(Message::text(hello)).await.is_err() {
                                break;
                            }
                        }
                    }
                    Err(_) => break,
                }
            }
        }
    }
}

async fn media(
    Query(q): Query<MediaQ>,
    State(st): State<HubState>,
) -> Response {
    if !token_ok(&q.t, &st.token) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    let ws = st
        .workspace
        .lock()
        .ok()
        .map(|g| g.clone())
        .unwrap_or_default();
    match open_media(&ws, &q.path) {
        Ok((bytes, mime)) => (
            [
                (header::CONTENT_TYPE, mime),
                (header::CACHE_CONTROL, "no-store"),
            ],
            bytes,
        )
            .into_response(),
        Err(_) => StatusCode::NOT_FOUND.into_response(),
    }
}

#[derive(Deserialize)]
struct MediaQ {
    #[serde(default)]
    t: String,
    #[serde(default)]
    path: String,
}

pub fn open_media(workspace: &Path, requested: &str) -> Result<(Vec<u8>, &'static str)> {
    let path = tools::resolve_in_workspace(workspace, requested)?;
    let meta = std::fs::metadata(&path)?;
    if !meta.is_file() || meta.len() > MEDIA_MAX {
        return Err(Error::Tool("media unavailable".into()));
    }
    let bytes = std::fs::read(&path)?;
    Ok((bytes, mime_file(&path)))
}

fn mime_file(path: &Path) -> &'static str {
    match path
        .extension()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_ascii_lowercase()
        .as_str()
    {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "bmp" => "image/bmp",
        "svg" => "image/svg+xml",
        _ => "application/octet-stream",
    }
}

fn open_browser(url: &str) {
    #[cfg(windows)]
    {
        let _ = std::process::Command::new("rundll32")
            .args(["url.dll,FileProtocolHandler", url])
            .spawn();
    }
    #[cfg(target_os = "macos")]
    {
        let _ = std::process::Command::new("open").arg(url).spawn();
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        let _ = std::process::Command::new("xdg-open").arg(url).spawn();
    }
}

pub fn text_html(s: &str) -> String {
    md::html_escape(s).replace('\n', "<br>")
}

pub fn diff_html(diff: &str) -> String {
    let mut out = String::from("<pre class=\"diff\">");
    for line in diff.lines() {
        let class = if line.starts_with("+++") || line.starts_with("---") {
            "diff-file"
        } else if line.starts_with("@@") {
            "diff-hunk"
        } else if line.starts_with('+') {
            "diff-add"
        } else if line.starts_with('-') {
            "diff-del"
        } else {
            ""
        };
        if class.is_empty() {
            out.push_str(&md::html_escape(line));
        } else {
            out.push_str("<span class=\"");
            out.push_str(class);
            out.push_str("\">");
            out.push_str(&md::html_escape(line));
            out.push_str("</span>");
        }
        out.push('\n');
    }
    out.push_str("</pre>");
    out
}

pub fn pre_html(s: &str) -> String {
    format!("<pre>{}</pre>", md::html_escape(s))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn command_roundtrip() {
        let raw = json!({"type":"set_composer","text":"hi","caret":2,"seq":3});
        let cmd: UiCommand = serde_json::from_value(raw).unwrap();
        match cmd {
            UiCommand::SetComposer { text, caret, seq } => {
                assert_eq!(text, "hi");
                assert_eq!(caret, 2);
                assert_eq!(seq, 3);
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn task_command_roundtrip() {
        let raw = json!({"type":"open_task"});
        let cmd: UiCommand = serde_json::from_value(raw).unwrap();
        assert!(matches!(cmd, UiCommand::OpenTask));
        let raw = json!({"type":"set_task_draft","text":"goal"});
        let cmd: UiCommand = serde_json::from_value(raw).unwrap();
        match cmd {
            UiCommand::SetTaskDraft { text } => assert_eq!(text, "goal"),
            _ => panic!("wrong variant"),
        }
    }

    fn ui_row(text: &str) -> UiRow {
        UiRow {
            kind: "meta".into(),
            html: text.into(),
            text: text.into(),
            expanded: None,
            done: None,
            elapsed_ms: None,
            images: vec![],
            calls: vec![],
            path: None,
            label: None,
        }
    }

    fn patch(path: &str, from: usize, len: usize, rows: &[&str]) -> UiViewPatch {
        UiViewPatch {
            path: path.into(),
            from,
            len,
            rows: rows.iter().map(|r| ui_row(r)).collect(),
        }
    }

    fn next_delta(rx: &mut broadcast::Receiver<String>) -> serde_json::Value {
        serde_json::from_str(&rx.try_recv().expect("a delta")).unwrap()
    }

    #[tokio::test]
    async fn publish_sends_only_changes_and_keeps_a_full_mirror() {
        std::env::set_var("GROKA_NO_WEB_OPEN", "1");
        let dir = tempfile::tempdir().unwrap();
        let hub = start(dir.path().to_path_buf()).await.unwrap().expect("hub");
        let mut rx = hub.handle.out.subscribe();
        let snap = UiSnapshot {
            session_id: "s".into(),
            ..UiSnapshot::default()
        };

        hub.publish(snap.clone(), UiLogs::default(), vec![patch("", 0, 2, &["a", "b"])], vec![]);
        let d = next_delta(&mut rx);
        assert_eq!(d["type"], "delta");
        assert_eq!(d["seq"], 1);
        assert!(d.get("snapshot").is_some(), "first publish carries the shell");

        hub.publish(snap.clone(), UiLogs::default(), vec![], vec![]);
        assert!(rx.try_recv().is_err(), "nothing changed, nothing sent");

        hub.publish(
            snap.clone(),
            UiLogs::default(),
            vec![patch("", 1, 3, &["c", "d"]), patch("coder", 0, 1, &["kid"])],
            vec![],
        );
        let d = next_delta(&mut rx);
        assert_eq!(d["seq"], 2);
        assert!(d.get("snapshot").is_none(), "unchanged shell is not resent");
        assert_eq!(d["views"].as_array().unwrap().len(), 2);

        hub.publish(snap, UiLogs::default(), vec![], vec!["coder".into()]);
        let d = next_delta(&mut rx);
        assert_eq!(d["seq"], 3);
        assert_eq!(d["removed"][0], "coder");

        let hello: serde_json::Value =
            serde_json::from_str(&hub.handle.mirror.lock().unwrap().hello().unwrap()).unwrap();
        assert_eq!(hello["type"], "hello");
        assert_eq!(hello["seq"], 3);
        let views = hello["views"].as_array().unwrap();
        assert_eq!(views.len(), 1, "removed views leave the mirror");
        let texts: Vec<&str> = views[0]["rows"]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| r["text"].as_str().unwrap())
            .collect();
        assert_eq!(texts, ["a", "c", "d"], "patches applied from their offset");
    }

    #[test]
    fn resync_is_recognised() {
        assert!(is_resync(r#"{"type":"resync"}"#));
        assert!(!is_resync(r#"{"type":"submit","insert":false}"#));
        assert!(!is_resync("not json"));
    }

    #[test]
    fn media_rejects_escape() {
        let dir = tempfile::tempdir().unwrap();
        let err = open_media(dir.path(), "../secret.png").unwrap_err();
        let s = err.to_string();
        assert!(s.contains("escapes") || s.contains("not found") || s.contains("required"), "{s}");
    }

    #[test]
    fn media_reads_workspace_file() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("pic.txt");
        std::fs::write(&p, b"abc").unwrap();
        let (bytes, _) = open_media(dir.path(), "pic.txt").unwrap();
        assert_eq!(bytes, b"abc");
    }

    #[test]
    fn diff_html_marks_plus_minus() {
        let html = diff_html("--- a\n+++ b\n@@\n-old\n+new\n");
        assert!(html.contains("diff-add"), "{html}");
        assert!(html.contains("diff-del"), "{html}");
        assert!(!html.contains("<script>"), "{html}");
    }
}
