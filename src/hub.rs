//! Loopback web mirror of the TUI: one App writer, snapshot over WebSocket.

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
    latest: Arc<Mutex<String>>,
    workspace: Arc<Mutex<PathBuf>>,
}

pub struct Hub {
    pub url: String,
    pub cmd_rx: mpsc::UnboundedReceiver<UiCommand>,
    handle: HubHandle,
}

#[derive(Clone)]
struct HubHandle {
    out: broadcast::Sender<String>,
    latest: Arc<Mutex<String>>,
    workspace: Arc<Mutex<PathBuf>>,
}

impl Hub {
    pub fn publish(&self, msg: &ServerMsg) {
        let json = serde_json::to_string(msg).unwrap_or_else(|_| "{}".into());
        if let Ok(mut g) = self.handle.latest.lock() {
            *g = json.clone();
        }
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
    Hello { snapshot: UiSnapshot },
    Snapshot { snapshot: UiSnapshot },
}

#[derive(Clone, Debug, Serialize, Deserialize, Default)]
pub struct UiSnapshot {
    pub session_id: String,
    pub header: UiHeader,
    pub composer: UiComposer,
    pub rows: Vec<UiRow>,
    pub sessions: Vec<UiSession>,
    pub queue: Vec<UiQueued>,
    pub pending: Vec<String>,
    pub send_mode: String,
    pub rail: UiRail,
    pub settings: Option<UiSettings>,
    pub ask: Option<UiAsk>,
    pub picker: Option<UiPicker>,
    pub inspector: Option<UiInspector>,
    pub image_view: Option<String>,
    pub tool_panel: Option<UiToolPanel>,
    pub skill_view: Option<UiSkillView>,
    pub rename: Option<UiRename>,
    pub task: Option<UiTask>,
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
    pub children: Vec<UiChild>,
    pub monitors: Vec<UiMon>,
    pub backgrounds: Vec<UiBg>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct UiChild {
    pub name: String,
    pub prompt: String,
    pub status: String,
    pub activity: String,
    pub alive: bool,
    pub card_url: String,
    pub log: Vec<String>,
    pub messages: Vec<UiSideMsg>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct UiSideMsg {
    pub from: String,
    pub to: String,
    pub text: String,
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

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct UiTask {
    pub mode: String,
    pub goal: String,
    pub draft: String,
    pub phase: String,
    pub checklist: Vec<UiTaskItem>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum UiCommand {
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
    SetEffort { id: String },
    ToggleSearch,
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
    OpenChild { name: String },
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
    let (out, _) = broadcast::channel(32);
    let latest = Arc::new(Mutex::new(String::from("{}")));
    let workspace = Arc::new(Mutex::new(workspace));
    let state = HubState {
        token: token.clone(),
        cmd_tx,
        out: out.clone(),
        latest: latest.clone(),
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
            latest,
            workspace,
        },
    }))
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

async fn client_loop(mut socket: WebSocket, st: HubState) {
    let hello = st
        .latest
        .lock()
        .ok()
        .map(|g| g.clone())
        .unwrap_or_default();
    if !hello.is_empty() && hello != "{}" {
        if socket.send(Message::text(hello)).await.is_err() {
            return;
        }
    }
    let mut rx = st.out.subscribe();
    loop {
        tokio::select! {
            msg = socket.recv() => {
                match msg {
                    Some(Ok(Message::Text(t))) => {
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
                    Err(broadcast::error::RecvError::Lagged(_)) => continue,
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
