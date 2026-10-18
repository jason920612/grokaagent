//! Loopback web mirror of the TUI: one App writer, deltas over WebSocket.
//!
//! The TUI publishes a small shell (header, composer, sessions, overlays),
//! the workbench logs, and row patches per transcript view (`""` = main chat,
//! `coder/lint` = a child agent). The hub keeps a full mirror so a new client,
//! or one that missed a delta, gets a complete `hello`.
//!
//! Access: the link the TUI opens and shows carries a one-time code. The
//! page trades it for the hub's key and keeps that in localStorage, which is
//! per origin: a cookie would also go to every other port on this host. The
//! socket and uploads need the key. An `<img>` cannot send it, so `/auth`
//! trades it for a media token that `/media` URLs carry, which opens images
//! only and, unlike a cookie, is never sent to another port.
//! Spending a code mints the next one for the TUI to show; the saved
//! transcript keeps only the bare origin. Every route checks `Host`, and
//! requests that carry the key check `Origin`.
//!
//! The first link sits in the argv of the browser it opens, which other
//! local users can read until the page redeems it; on Linux the hub accepts
//! connections only from processes of the user running it, so they cannot
//! redeem it (macOS and Windows do not show argv across users).

use std::collections::BTreeMap;
use std::io::{Cursor, Read};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::UNIX_EPOCH;

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Query, Request, State};
use axum::http::{header, HeaderMap, HeaderValue, StatusCode, Uri};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::Router;
use image::codecs::jpeg::JpegEncoder;
use image::{DynamicImage, ImageFormat, ImageReader};
use rust_embed::RustEmbed;
use serde::{Deserialize, Serialize};
use tokio::net::TcpListener;
use tokio::sync::{broadcast, mpsc};
use uuid::Uuid;

use crate::error::{Error, Result};
use crate::md;
use crate::tools;
use crate::vision;

/// Largest original `/media` sends as is; a bigger image gets a scaled copy.
const MEDIA_MAX: u64 = 20 * 1024 * 1024;
/// Pixel budget of the copy that stands in for an oversized original.
const PREVIEW_PIXELS: u64 = 16_000_000;
/// Largest image the browser may upload, the same as for a pasted file.
pub(crate) const UPLOAD_MAX: usize = 32 * 1024 * 1024;
/// Header that carries the key on plain requests.
const KEY_HEADER: &str = "x-groka-key";

#[derive(RustEmbed)]
#[folder = "web/"]
struct Assets;

#[derive(Clone)]
struct HubState {
    port: u16,
    /// What the page sends on the socket and uploads; never in a URL or cookie.
    key: String,
    /// The `t` of `/media` URLs: images and nothing else.
    media: String,
    /// One-time code in the link the TUI shows.
    code: Arc<Mutex<String>>,
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
    fn hello(&self, media: &str) -> Option<String> {
        let snapshot = self.snapshot.clone()?;
        let msg = ServerMsg::Hello {
            seq: self.seq,
            media: media.to_string(),
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
    /// Link with the first one-time code; `UiCommand::WebUrl` brings the next.
    pub url: String,
    /// `http://127.0.0.1:<port>`: grants nothing, safe to show and save.
    pub origin: String,
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
        /// The `/media` token: a page whose key came from another tab after
        /// its last `/auth` still loads images.
        #[serde(default)]
        media: String,
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
    /// Limits the page checks before uploading, so it can say why it won't.
    #[serde(default)]
    pub pending_max: usize,
    #[serde(default)]
    pub upload_max: usize,
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
    /// A picture row's original file, when `path` is the copy the model saw.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub source: Option<String>,
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
    /// Workspace-relative paths of those images, for thumbnails.
    #[serde(default)]
    pub paths: Vec<String>,
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
    /// Whether a key is set. The key itself never goes to the browser.
    #[serde(default)]
    pub has_api_key: bool,
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
    /// Drop `queue[index]` if it still holds `expected` text and `images`.
    RemoveQueued { session_id: String, request_id: String, index: usize, expected: String, images: Vec<String> },
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
    /// From the hub, never a browser: the link's code was used; show this one.
    #[serde(skip)]
    WebUrl { url: String },
    /// From the hub, never a browser: an uploaded image, saved under `workspace`.
    #[serde(skip)]
    AttachUpload { session_id: String, request_id: String, workspace: PathBuf, path: String },
}

pub fn enabled() -> bool {
    !env_flag("GROKA_NO_WEB")
}

pub async fn start(workspace: PathBuf) -> Result<Option<Hub>> {
    if !enabled() {
        return Ok(None);
    }
    let port = std::env::var("GROKA_WEB_PORT")
        .ok()
        .and_then(|s| s.parse::<u16>().ok())
        .unwrap_or(0);
    let listener = TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], port)))
        .await
        .map_err(|e| Error::Io(e))?;
    let port = listener.local_addr().map_err(Error::Io)?.port();
    let code = new_code();
    let url = link(port, &code);
    let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
    let (out, _) = broadcast::channel(256);
    let mirror = Arc::new(Mutex::new(Mirror::default()));
    let workspace = Arc::new(Mutex::new(workspace));
    let state = HubState {
        port,
        key: new_secret(),
        media: new_secret(),
        code: Arc::new(Mutex::new(code)),
        cmd_tx,
        out: out.clone(),
        mirror: mirror.clone(),
        workspace: workspace.clone(),
    };
    let app = Router::new()
        .route("/redeem", post(redeem_code))
        .route("/auth", get(auth))
        .route("/ws", get(ws_upgrade))
        .route("/media", get(media))
        .route("/upload", post(upload))
        .fallback(static_file)
        .layer(middleware::from_fn(guard))
        .with_state(state);
    tokio::spawn(async move {
        let _ = axum::serve(OwnUser::new(listener), app).await;
    });
    // The code sits in the browser's argv, which other local users can read
    // until the page spends it; on Linux their connections are refused.
    if !env_flag("GROKA_NO_WEB_OPEN") {
        open_browser(&url);
    }
    Ok(Some(Hub {
        url,
        origin: origin(port),
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

fn new_secret() -> String {
    Uuid::new_v4().simple().to_string()
}

/// 64 random bits (v4 fixes different bits in each half): plenty for a code
/// that works once, and short enough for the status bar to show whole.
fn new_code() -> String {
    let u = Uuid::new_v4().as_u128();
    format!("{:016x}", (u >> 64) as u64 ^ u as u64)
}

fn origin(port: u16) -> String {
    format!("http://127.0.0.1:{port}")
}

fn link(port: u16, code: &str) -> String {
    format!("http://127.0.0.1:{port}/?code={code}")
}

/// Constant time, so response timing says nothing about a guess.
fn secret_eq(got: &str, want: &str) -> bool {
    !want.is_empty()
        && got.len() == want.len()
        && got.bytes().zip(want.bytes()).fold(0u8, |acc, (a, b)| acc | (a ^ b)) == 0
}

/// `127.0.0.1`, `localhost` or `[::1]` on any port: SSH or editor port
/// forwarding may serve this hub on another one.
fn local_authority(s: &str) -> bool {
    let host = match s.rsplit_once(':') {
        Some((h, p)) if !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()) => h,
        _ => s,
    };
    host == "127.0.0.1" || host == "[::1]" || host.eq_ignore_ascii_case("localhost")
}

fn host_of(headers: &HeaderMap) -> Option<&str> {
    headers.get(header::HOST).and_then(|v| v.to_str().ok())
}

/// Any other name means DNS rebinding, not our own page.
fn host_ok(headers: &HeaderMap) -> bool {
    host_of(headers).is_some_and(local_authority)
}

/// Browsers let any page open a socket or POST here; only ours may. Ours is
/// the page served under this very `Host`, port included, so a page on
/// another local port is refused while a forwarded one works.
fn origin_ok(headers: &HeaderMap) -> bool {
    let origin = headers
        .get(header::ORIGIN)
        .and_then(|v| v.to_str().ok())
        .and_then(|o| o.strip_prefix("http://"));
    match (origin, host_of(headers)) {
        (Some(o), Some(h)) => o.eq_ignore_ascii_case(h) && local_authority(o),
        _ => false,
    }
}

/// The key, sent by the page as a header.
fn key_ok(headers: &HeaderMap, st: &HubState) -> bool {
    headers
        .get(KEY_HEADER)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|k| secret_eq(k, &st.key))
}

/// Requested by another site, or another port of this host (same-site).
fn cross_site(headers: &HeaderMap) -> bool {
    matches!(
        headers.get("sec-fetch-site").and_then(|v| v.to_str().ok()),
        Some("cross-site" | "same-site")
    )
}

/// Spend the current one-time code and mint the next one for the TUI to show.
fn redeem(st: &HubState, code: &str) -> bool {
    let Ok(mut cur) = st.code.lock() else {
        return false;
    };
    if !secret_eq(code, &cur) {
        return false;
    }
    *cur = new_code();
    let _ = st.cmd_tx.send(UiCommand::WebUrl { url: link(st.port, &cur) });
    true
}

fn workspace_of(st: &HubState) -> PathBuf {
    st.workspace.lock().ok().map(|g| g.clone()).unwrap_or_default()
}

/// Accepts connections from this user's own processes only, where the OS
/// says who opened them (Linux): another local user who reads the opened
/// link from argv, or the key from a lost race, gets no further.
struct OwnUser {
    listener: TcpListener,
    /// Connections whose owner is being looked up, off the runtime's workers:
    /// the listing grows with every socket on the host.
    checks: tokio::task::JoinSet<Option<(tokio::net::TcpStream, SocketAddr)>>,
}

/// Lookups in flight at once; a connect flood waits in the backlog instead.
const OWNER_CHECKS: usize = 4;

impl OwnUser {
    fn new(listener: TcpListener) -> Self {
        Self { listener, checks: tokio::task::JoinSet::new() }
    }
}

impl axum::serve::Listener for OwnUser {
    type Io = tokio::net::TcpStream;
    type Addr = SocketAddr;

    async fn accept(&mut self) -> (Self::Io, Self::Addr) {
        loop {
            tokio::select! {
                (io, peer) = axum::serve::Listener::accept(&mut self.listener), if self.checks.len() < OWNER_CHECKS => {
                    let Ok(local) = self.listener.local_addr() else { continue };
                    self.checks.spawn_blocking(move || same_user(peer, local).then_some((io, peer)));
                }
                Some(done) = self.checks.join_next() => {
                    if let Ok(Some(conn)) = done {
                        return conn;
                    }
                }
            }
        }
    }

    fn local_addr(&self) -> std::io::Result<SocketAddr> {
        self.listener.local_addr()
    }
}

#[cfg(target_os = "linux")]
fn same_user(peer: SocketAddr, local: SocketAddr) -> bool {
    // A client on an AF_INET6 socket (`::ffff:127.0.0.1`) is listed in tcp6
    // only; that table is read when tcp lacks the client.
    let tables = ["/proc/net/tcp", "/proc/net/tcp6"].into_iter().filter_map(|p| std::fs::read_to_string(p).ok());
    // SAFETY: geteuid has no preconditions.
    peer_allowed(tables, peer, local, unsafe { libc::geteuid() })
}

#[cfg(not(target_os = "linux"))]
fn same_user(_peer: SocketAddr, _local: SocketAddr) -> bool {
    true
}

/// The client end of a loopback connection, `peer` → `local`, is not
/// another user's in the /proc/net/tcp and tcp6 listings. Root's is allowed
/// (a WSL or container port relay; root can read our memory anyway), and so
/// is one the listings lack: it is outside this network namespace (WSL
/// mirrored networking) or they list no sockets at all (WSL 1), and other
/// local users cannot reach this loopback from there.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn peer_allowed(tables: impl IntoIterator<Item = String>, peer: SocketAddr, local: SocketAddr, me: u32) -> bool {
    tables
        .into_iter()
        .find_map(|t| client_owner(&t, peer, local))
        .is_none_or(|uid| uid == me || uid == 0)
}

/// Owner of the socket `from` → `to` in one listing. The hub sees an IPv4
/// peer; tcp6 lists that client as `::ffff:a.b.c.d`.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn client_owner(table: &str, from: SocketAddr, to: SocketAddr) -> Option<u32> {
    let forms = |a: SocketAddr| match a {
        SocketAddr::V4(a) => {
            let ip = u32::from_ne_bytes(a.ip().octets());
            let mapped = u32::from_ne_bytes([0, 0, 0xff, 0xff]);
            Some([format!("{ip:08X}:{:04X}", a.port()), format!("{:016X}{mapped:08X}{ip:08X}:{:04X}", 0, a.port())])
        }
        SocketAddr::V6(_) => None,
    };
    let (from, to) = (forms(from)?, forms(to)?);
    table.lines().skip(1).find_map(|line| {
        let f: Vec<&str> = line.split_whitespace().collect();
        (f.len() > 7 && from.iter().any(|a| a == f[1]) && to.iter().any(|a| a == f[2])).then(|| f[7].parse().ok()).flatten()
    })
}

/// Every route: refuse foreign `Host` names; responses are never sniffed.
async fn guard(req: Request, next: Next) -> Response {
    if !host_ok(req.headers()) {
        return StatusCode::FORBIDDEN.into_response();
    }
    let mut res = next.run(req).await;
    let h = res.headers_mut();
    h.insert(header::X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff"));
    h.insert(header::REFERRER_POLICY, HeaderValue::from_static("no-referrer"));
    res
}

/// The page (`/?code=…`, served like `/`) trades the link's code for the
/// key. Only our page may: a POST with our `Origin`, so a link preview or a
/// prefetch of the link does not spend it.
async fn redeem_code(State(st): State<HubState>, headers: HeaderMap, code: String) -> Response {
    if !origin_ok(&headers) {
        return StatusCode::FORBIDDEN.into_response();
    }
    if !redeem(&st, code.trim()) {
        return StatusCode::GONE.into_response();
    }
    ([(header::CACHE_CONTROL, "no-store")], st.key.clone()).into_response()
}

/// Whether the key still works. The page asks on load and after a socket
/// drops; the answer is the media token its `<img>` loads need.
async fn auth(headers: HeaderMap, State(st): State<HubState>) -> Response {
    if !key_ok(&headers, &st) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    ([(header::CACHE_CONTROL, "no-store")], st.media.clone()).into_response()
}

async fn static_file(uri: Uri, headers: HeaderMap) -> Response {
    let path = uri.path().trim_start_matches('/');
    let path = if path.is_empty() { "index.html" } else { path };
    if path.contains("..") {
        return StatusCode::NOT_FOUND.into_response();
    }
    asset(path, &headers)
}

fn asset(path: &str, headers: &HeaderMap) -> Response {
    let Some(f) = Assets::get(path) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let mime = mime_of(path);
    let mut res = (
        [(header::CONTENT_TYPE, mime), (header::CACHE_CONTROL, "no-store")],
        f.data.into_owned(),
    )
        .into_response();
    if mime.starts_with("text/html") {
        // `guard` already checked Host, so it names this server.
        let host = host_of(headers).unwrap_or("");
        if let Ok(v) = HeaderValue::from_str(&csp(host)) {
            res.headers_mut().insert(header::CONTENT_SECURITY_POLICY, v);
        }
    }
    res
}

/// Scripts, styles and images only from this server, sockets only back to it.
fn csp(host: &str) -> String {
    format!(
        "default-src 'none'; script-src 'self'; style-src 'self'; img-src 'self'; \
         connect-src 'self' ws://{host}; base-uri 'none'; form-action 'none'; frame-ancestors 'none'"
    )
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
    headers: HeaderMap,
    State(st): State<HubState>,
) -> Response {
    if !origin_ok(&headers) {
        return StatusCode::FORBIDDEN.into_response();
    }
    // A browser socket cannot set headers: the key comes as a subprotocol,
    // `k.<key>`, next to the `groka` one we answer with.
    let keyed = ws
        .requested_protocols()
        .filter_map(|p| p.to_str().ok()?.strip_prefix("k."))
        .any(|k| secret_eq(k, &st.key));
    if !keyed {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    ws.protocols(["groka"]).on_upgrade(move |socket| client_loop(socket, st))
}

fn mirror_hello(st: &HubState) -> Option<String> {
    st.mirror.lock().ok().and_then(|m| m.hello(&st.media))
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
    headers: HeaderMap,
    Query(q): Query<MediaQ>,
    State(st): State<HubState>,
) -> Response {
    if !secret_eq(&q.t, &st.media) || cross_site(&headers) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    let ws = workspace_of(&st);
    let seen = headers
        .get(header::IF_NONE_MATCH)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    let loaded = tokio::task::spawn_blocking(move || load_media(&ws, &q.path, q.max, seen.as_deref())).await;
    let (mut res, etag) = match loaded {
        // No Content-Type: a 304's headers update the browser's stored copy.
        Ok(Ok(Media::Unchanged { etag })) => (StatusCode::NOT_MODIFIED.into_response(), etag),
        Ok(Ok(Media::Body { bytes, mime, etag })) => {
            let mut res = bytes.into_response();
            res.headers_mut().insert(header::CONTENT_TYPE, HeaderValue::from_static(mime));
            (res, etag)
        }
        _ => return StatusCode::NOT_FOUND.into_response(),
    };
    let h = res.headers_mut();
    // Revalidated each time: the file may change, but an unchanged one costs a stat.
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static("private, no-cache"));
    if let Ok(v) = HeaderValue::from_str(&etag) {
        h.insert(header::ETAG, v);
    }
    res
}

#[derive(Deserialize)]
struct MediaQ {
    #[serde(default)]
    path: String,
    /// The media token from `/auth`.
    #[serde(default)]
    t: String,
    /// Longest edge wanted (thumbnails). An image that fits comes as is.
    max: Option<u32>,
}

pub enum Media {
    /// The browser's copy (`If-None-Match`) is current.
    Unchanged { etag: String },
    Body { bytes: Vec<u8>, mime: &'static str, etag: String },
}

/// A workspace image for the browser. Only the raster formats the page
/// shows are served, recognised by their bytes, so an SVG with script or a
/// `.env` never is. A thumbnail, or an original over `MEDIA_MAX`, comes as
/// a scaled copy.
pub fn load_media(workspace: &Path, requested: &str, max: Option<u32>, seen: Option<&str>) -> Result<Media> {
    load_media_capped(workspace, requested, max, seen, MEDIA_MAX)
}

fn load_media_capped(
    workspace: &Path,
    requested: &str,
    max: Option<u32>,
    seen: Option<&str>,
    cap: u64,
) -> Result<Media> {
    let path = tools::resolve_in_workspace(workspace, requested)?;
    let meta = std::fs::metadata(&path)?;
    if !meta.is_file() {
        return Err(Error::Tool("media unavailable".into()));
    }
    let mime = sniff_image(&path)?;
    let mtime = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let etag = format!("\"{:x}-{:x}-{}\"", meta.len(), mtime, max.unwrap_or(0));
    if seen.is_some_and(|s| etag_matches(s, &etag)) {
        return Ok(Media::Unchanged { etag });
    }
    let (w, h) = vision::image_dimensions(&path)?;
    let target = match max.and_then(|edge| fit_edge(w, h, edge.max(1))) {
        Some(t) => Some(t),
        None if meta.len() > cap => Some(fit_pixels(w, h, PREVIEW_PIXELS).unwrap_or((w, h))),
        None => None,
    };
    let Some((tw, th)) = target else {
        let bytes = std::fs::read(&path)?;
        return Ok(Media::Body { bytes, mime, etag });
    };
    let img = vision::open_image(&path)?;
    let img = if (tw, th) == (img.width(), img.height()) { img } else { img.thumbnail_exact(tw, th) };
    let (bytes, mime) = encode_preview(&img)?;
    Ok(Media::Body { bytes, mime, etag })
}

/// MIME type of an image the page can show, from the file's first bytes.
fn sniff_image(path: &Path) -> Result<&'static str> {
    let mut head = Vec::with_capacity(32);
    std::fs::File::open(path)?.take(32).read_to_end(&mut head)?;
    match image::guess_format(&head) {
        Ok(ImageFormat::Png) => Ok("image/png"),
        Ok(ImageFormat::Jpeg) => Ok("image/jpeg"),
        Ok(ImageFormat::Gif) => Ok("image/gif"),
        Ok(ImageFormat::WebP) => Ok("image/webp"),
        Ok(ImageFormat::Bmp) => Ok("image/bmp"),
        _ => Err(Error::Tool("not an image the page can show".into())),
    }
}

fn etag_matches(header: &str, etag: &str) -> bool {
    header
        .split(',')
        .map(str::trim)
        .any(|t| t == "*" || t.trim_start_matches("W/") == etag)
}

/// Size that fits `edge` on the longer side; `None` if it already fits.
fn fit_edge(w: u32, h: u32, edge: u32) -> Option<(u32, u32)> {
    if w <= edge && h <= edge {
        return None;
    }
    let scale = edge as f64 / w.max(h) as f64;
    Some((((w as f64 * scale).round() as u32).max(1), ((h as f64 * scale).round() as u32).max(1)))
}

/// Size within `budget` pixels, same aspect; `None` if it already fits.
/// A pixel budget keeps a tall page screenshot readable, unlike an edge cap.
fn fit_pixels(w: u32, h: u32, budget: u64) -> Option<(u32, u32)> {
    let px = w as u64 * h as u64;
    if px <= budget {
        return None;
    }
    let scale = (budget as f64 / px as f64).sqrt();
    Some((((w as f64 * scale) as u32).max(1), ((h as f64 * scale) as u32).max(1)))
}

/// PNG keeps transparency; anything opaque is far smaller as JPEG.
fn encode_preview(img: &DynamicImage) -> Result<(Vec<u8>, &'static str)> {
    let mut buf = Cursor::new(Vec::new());
    let mime = if img.color().has_alpha() {
        DynamicImage::ImageRgba8(img.to_rgba8())
            .write_to(&mut buf, ImageFormat::Png)
            .map(|_| "image/png")
    } else {
        JpegEncoder::new_with_quality(&mut buf, 85)
            .encode_image(&img.to_rgb8())
            .map(|_| "image/jpeg")
    }
    .map_err(|e| Error::Tool(format!("preview: {e}")))?;
    Ok((buf.into_inner(), mime))
}

#[derive(Deserialize)]
struct UploadQ {
    #[serde(default)]
    session_id: String,
    #[serde(default)]
    request_id: String,
}

/// An image dropped or pasted in the browser: saved like a pasted image,
/// then the TUI attaches it to the session's pending images.
async fn upload(State(st): State<HubState>, Query(q): Query<UploadQ>, req: Request) -> Response {
    let headers = req.headers();
    if !origin_ok(headers) {
        return StatusCode::FORBIDDEN.into_response();
    }
    if !key_ok(headers, &st) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    let too_big = || {
        let msg = format!("未附上：圖片超過 {} MiB", UPLOAD_MAX >> 20);
        (StatusCode::PAYLOAD_TOO_LARGE, msg).into_response()
    };
    // The page checks the size first; this is for anything else.
    let declared = headers
        .get(header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<usize>().ok());
    if declared.is_some_and(|n| n > UPLOAD_MAX) {
        return too_big();
    }
    let Ok(body) = axum::body::to_bytes(req.into_body(), UPLOAD_MAX).await else {
        return too_big();
    };
    // Only after the body is read: answered while the browser still sends,
    // the connection is reset and the page could only say it dropped.
    if pending_full(&st, &q.session_id) {
        let msg = format!("未附上：最多 {} 張圖片", vision::MAX_USER_IMAGES);
        return (StatusCode::CONFLICT, msg).into_response();
    }
    let ws = workspace_of(&st);
    let dir = ws.clone();
    match tokio::task::spawn_blocking(move || save_upload(&dir, &body)).await {
        Ok(Ok(path)) => {
            let _ = st.cmd_tx.send(UiCommand::AttachUpload {
                session_id: q.session_id,
                request_id: q.request_id,
                workspace: ws,
                path: path.clone(),
            });
            (StatusCode::OK, path).into_response()
        }
        Ok(Err(e)) => (StatusCode::UNPROCESSABLE_ENTITY, format!("未附上：{e}")).into_response(),
        Err(_) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}

/// The mirror says the session already has all the images it may; the
/// TUI checks again when it attaches.
fn pending_full(st: &HubState, session_id: &str) -> bool {
    st.mirror
        .lock()
        .ok()
        .and_then(|m| {
            m.snapshot
                .as_ref()
                .map(|s| s.session_id == session_id && s.pending.len() >= vision::MAX_USER_IMAGES)
        })
        .unwrap_or(false)
}

/// Decode by content as `vision::open_image` does for files (orientation,
/// allocation cap) and save to the inbox, transparency on white.
fn save_upload(workspace: &Path, bytes: &[u8]) -> Result<String> {
    let reader = ImageReader::new(Cursor::new(bytes))
        .with_guessed_format()
        .map_err(|_| Error::Tool("無法讀取圖片".into()))?;
    let img = vision::decode_capped(reader).map_err(|e| match e {
        image::ImageError::Unsupported(_) => Error::Tool("不是支援的圖片（PNG、JPEG、GIF、WebP、BMP）".into()),
        image::ImageError::Limits(_) => Error::Tool("圖片太大，無法解碼".into()),
        e => Error::Tool(format!("圖片無法解碼: {e}")),
    })?;
    vision::save_user_image(workspace, &img)
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
            source: None,
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
            serde_json::from_str(&hub.handle.mirror.lock().unwrap().hello("t").unwrap()).unwrap();
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
        let Err(err) = load_media(dir.path(), "../secret.png", None, None) else {
            panic!("escaped the workspace");
        };
        let s = err.to_string();
        assert!(s.contains("escapes") || s.contains("not found") || s.contains("required"), "{s}");
    }

    #[test]
    fn media_reads_workspace_images_only() {
        let dir = tempfile::tempdir().unwrap();
        let img = image::RgbImage::from_pixel(4, 3, image::Rgb([9, 9, 9]));
        // The extension says nothing; the bytes decide.
        img.save_with_format(dir.path().join("pic.dat"), ImageFormat::Png).unwrap();
        let png = std::fs::read(dir.path().join("pic.dat")).unwrap();
        match load_media(dir.path(), "pic.dat", None, None).unwrap() {
            Media::Body { bytes, mime, .. } => {
                assert_eq!(bytes, png);
                assert_eq!(mime, "image/png");
            }
            Media::Unchanged { .. } => panic!("no etag was sent"),
        }
        std::fs::write(dir.path().join("pic.txt"), b"abc").unwrap();
        std::fs::write(dir.path().join(".env"), b"KEY=1").unwrap();
        std::fs::write(dir.path().join("logo.svg"), br#"<svg xmlns="http://www.w3.org/2000/svg"><script>1</script></svg>"#).unwrap();
        std::fs::write(dir.path().join("fake.png"), b"BM not really a bitmap").unwrap();
        for p in ["pic.txt", ".env", "logo.svg", "fake.png"] {
            assert!(load_media(dir.path(), p, None, None).is_err(), "{p} must not be served");
        }
    }

    #[test]
    fn diff_html_marks_plus_minus() {
        let html = diff_html("--- a\n+++ b\n@@\n-old\n+new\n");
        assert!(html.contains("diff-add"), "{html}");
        assert!(html.contains("diff-del"), "{html}");
        assert!(!html.contains("<script>"), "{html}");
    }
}

#[cfg(test)]
mod web_tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    struct Reply {
        status: u16,
        head: String,
        body: Vec<u8>,
    }

    impl Reply {
        fn header(&self, name: &str) -> Option<String> {
            self.head.lines().find_map(|l| {
                let (k, v) = l.split_once(':')?;
                k.trim().eq_ignore_ascii_case(name).then(|| v.trim().to_string())
            })
        }
    }

    async fn send(port: u16, head: &str, body: &[u8]) -> Reply {
        let mut s = tokio::net::TcpStream::connect(("127.0.0.1", port)).await.unwrap();
        s.write_all(head.as_bytes()).await.unwrap();
        s.write_all(body).await.unwrap();
        let mut buf = Vec::new();
        let mut chunk = [0u8; 8192];
        // Upgrade requests keep the socket open, so stop at the end of the reply.
        loop {
            let n = tokio::time::timeout(std::time::Duration::from_secs(10), s.read(&mut chunk))
                .await
                .expect("reply in time")
                .unwrap();
            buf.extend_from_slice(&chunk[..n]);
            if n == 0 {
                break;
            }
            let Some(split) = buf.windows(4).position(|w| w == b"\r\n\r\n").map(|i| i + 4) else {
                continue;
            };
            let head = Reply { status: 0, head: String::from_utf8_lossy(&buf[..split]).to_string(), body: vec![] };
            let len = head.header("content-length").and_then(|v| v.parse::<usize>().ok());
            if buf.starts_with(b"HTTP/1.1 101") || len.is_some_and(|len| buf.len() >= split + len) {
                break;
            }
        }
        let split = buf.windows(4).position(|w| w == b"\r\n\r\n").expect("a full head") + 4;
        let head = String::from_utf8_lossy(&buf[..split]).to_string();
        let status = head[9..12].parse().unwrap();
        Reply { status, head, body: buf[split..].to_vec() }
    }

    /// The first text frame the hub sends after upgrading `head`.
    async fn first_frame(port: u16, head: &str) -> String {
        let mut s = tokio::net::TcpStream::connect(("127.0.0.1", port)).await.unwrap();
        s.write_all(head.as_bytes()).await.unwrap();
        let mut buf = Vec::new();
        let mut chunk = [0u8; 8192];
        loop {
            let n = tokio::time::timeout(std::time::Duration::from_secs(10), s.read(&mut chunk))
                .await
                .expect("a frame in time")
                .unwrap();
            assert!(n > 0, "closed before a frame");
            buf.extend_from_slice(&chunk[..n]);
            let Some(at) = buf.windows(4).position(|w| w == b"\r\n\r\n").map(|i| i + 4) else {
                continue;
            };
            let f = &buf[at..];
            // Server frames are unmasked: FIN|text, then a 7, 16 or 64-bit length.
            let (len, from) = match f.get(1).map(|b| b & 0x7f) {
                Some(126) if f.len() >= 4 => (u16::from_be_bytes([f[2], f[3]]) as usize, 4),
                Some(127) if f.len() >= 10 => (u64::from_be_bytes(f[2..10].try_into().unwrap()) as usize, 10),
                Some(n) if n < 126 => (n as usize, 2),
                _ => continue,
            };
            if f.len() >= from + len {
                assert_eq!(f[0], 0x81, "one text frame");
                return String::from_utf8(f[from..from + len].to_vec()).unwrap();
            }
        }
    }

    async fn get(port: u16, path: &str, extra: &str) -> Reply {
        let head = format!("GET {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n{extra}\r\n");
        send(port, &head, b"").await
    }

    async fn hub() -> (Hub, u16, tempfile::TempDir) {
        std::env::set_var("GROKA_NO_WEB_OPEN", "1");
        let dir = tempfile::tempdir().unwrap();
        let hub = start(dir.path().to_path_buf()).await.unwrap().expect("hub");
        let port = hub.origin.rsplit(':').next().unwrap().parse().unwrap();
        (hub, port, dir)
    }

    fn code_of(url: &str) -> String {
        url.split("?code=").nth(1).expect("a code").to_string()
    }

    async fn post(port: u16, path: &str, extra: &str, body: &[u8]) -> Reply {
        let head = format!(
            "POST {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\nContent-Length: {}\r\n{extra}\r\n",
            body.len()
        );
        send(port, &head, body).await
    }

    fn ours(port: u16) -> String {
        format!("Origin: http://127.0.0.1:{port}\r\n")
    }

    /// Redeem the hub's current code as the page does: the key, then the
    /// media token that `/auth` hands out for it.
    async fn access(hub: &mut Hub, port: u16) -> (String, String) {
        let r = post(port, "/redeem", &ours(port), code_of(&hub.url).as_bytes()).await;
        assert_eq!(r.status, 200);
        let key = String::from_utf8(r.body).unwrap();
        if let Ok(UiCommand::WebUrl { url }) = hub.cmd_rx.try_recv() {
            hub.url = url;
        }
        let r = get(port, "/auth", &format!("X-Groka-Key: {key}\r\n")).await;
        assert_eq!(r.status, 200);
        (key, String::from_utf8(r.body).unwrap())
    }

    fn png(w: u32, h: u32) -> Vec<u8> {
        let mut buf = Cursor::new(Vec::new());
        DynamicImage::ImageRgb8(image::RgbImage::from_pixel(w, h, image::Rgb([200, 10, 10])))
            .write_to(&mut buf, ImageFormat::Png)
            .unwrap();
        buf.into_inner()
    }

    #[tokio::test]
    async fn the_page_trades_the_code_once_for_the_key() {
        let (mut hub, port, _dir) = hub().await;
        assert!(hub.url.starts_with(&format!("http://127.0.0.1:{port}/?code=")));
        assert!(!hub.origin.contains("code"), "the origin grants nothing");
        let first = code_of(&hub.url);
        assert_eq!(first.len(), 16, "short enough for the status bar");

        // Opening the link only serves the page, so a prefetch spends nothing.
        let page = get(port, &format!("/?code={first}"), "").await;
        assert_eq!(page.status, 200);
        assert!(page.header("set-cookie").is_none());
        assert!(hub.cmd_rx.try_recv().is_err());

        assert_eq!(post(port, "/redeem", "", first.as_bytes()).await.status, 403, "only our page");
        let other = "Origin: http://127.0.0.1:8000\r\n";
        assert_eq!(post(port, "/redeem", other, first.as_bytes()).await.status, 403);

        let r = post(port, "/redeem", &ours(port), first.as_bytes()).await;
        assert_eq!(r.status, 200);
        assert_eq!(r.header("cache-control").as_deref(), Some("no-store"));
        assert!(r.header("set-cookie").is_none(), "the key is never a cookie");
        let key = String::from_utf8(r.body).unwrap();
        assert_eq!(key.len(), 32);
        let Ok(UiCommand::WebUrl { url }) = hub.cmd_rx.try_recv() else {
            panic!("the TUI is told the next link");
        };
        assert_ne!(code_of(&url), first, "a fresh code replaces the spent one");
        assert_eq!(post(port, "/redeem", &ours(port), first.as_bytes()).await.status, 410, "spent");

        assert_eq!(get(port, "/auth", "").await.status, 401);
        assert_eq!(get(port, "/auth", &format!("X-Groka-Key: {first}\r\n")).await.status, 401);
        let r = get(port, "/auth", &format!("X-Groka-Key: {key}\r\n")).await;
        assert_eq!(r.status, 200);
        assert!(r.header("set-cookie").is_none(), "a cookie would reach every port of this host");
        assert_eq!(r.header("cache-control").as_deref(), Some("no-store"));
        let token = String::from_utf8(r.body).unwrap();
        assert!(token.len() == 32 && token != key, "the media token is not the key");
    }

    #[tokio::test]
    async fn the_page_is_locked_down_and_other_hosts_are_refused() {
        let (_hub, port, _dir) = hub().await;
        let page = get(port, "/", "").await;
        assert_eq!(page.status, 200);
        let csp = page.header("content-security-policy").expect("a CSP");
        assert!(csp.contains("script-src 'self'") && csp.contains("frame-ancestors 'none'"), "{csp}");
        assert!(csp.contains(&format!("connect-src 'self' ws://127.0.0.1:{port}")), "{csp}");
        assert_eq!(page.header("x-content-type-options").as_deref(), Some("nosniff"));
        assert!(!String::from_utf8_lossy(&page.body).contains("<script>"), "no inline script for the CSP to block");

        let theme = get(port, "/theme.js", "").await;
        assert_eq!(theme.status, 200);
        assert_eq!(theme.header("x-content-type-options").as_deref(), Some("nosniff"));

        let head = format!("GET / HTTP/1.1\r\nHost: evil.example:{port}\r\nConnection: close\r\n\r\n");
        assert_eq!(send(port, &head, b"").await.status, 403, "DNS rebinding");
        let head = format!("GET /app.js HTTP/1.1\r\nHost: evil.example:{port}\r\nConnection: close\r\n\r\n");
        assert_eq!(send(port, &head, b"").await.status, 403, "assets too");
        let head = format!("GET / HTTP/1.1\r\nHost: localhost:{port}\r\nConnection: close\r\n\r\n");
        assert_eq!(send(port, &head, b"").await.status, 200);
        // SSH or editor port forwarding: another local port in front of ours.
        let head = "GET / HTTP/1.1\r\nHost: 127.0.0.1:8080\r\nConnection: close\r\n\r\n";
        let page = send(port, head, b"").await;
        assert_eq!(page.status, 200, "forwarded");
        assert!(page.header("content-security-policy").unwrap().contains("ws://127.0.0.1:8080"));
    }

    #[tokio::test]
    async fn the_socket_needs_our_origin_and_the_key() {
        let (mut hub, port, _dir) = hub().await;
        hub.publish(UiSnapshot::default(), UiLogs::default(), vec![], vec![]);
        let (key, token) = access(&mut hub, port).await;
        let upgrade = |host: &str, origin: &str, extra: &str| {
            format!(
                "GET /ws HTTP/1.1\r\nHost: {host}\r\nConnection: Upgrade\r\nUpgrade: websocket\r\n\
                 Sec-WebSocket-Version: 13\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n{origin}{extra}\r\n"
            )
        };
        let host = format!("127.0.0.1:{port}");
        let ours = ours(port);
        let keyed = format!("Sec-WebSocket-Protocol: groka, k.{key}\r\n");
        let evil = "Origin: http://127.0.0.1:8000\r\n";
        assert_eq!(send(port, &upgrade(&host, evil, &keyed), b"").await.status, 403, "another port is another origin");
        assert_eq!(send(port, &upgrade(&host, "", &keyed), b"").await.status, 403);
        assert_eq!(send(port, &upgrade(&host, &ours, ""), b"").await.status, 401);
        let media = format!("Sec-WebSocket-Protocol: groka, k.{token}\r\n");
        assert_eq!(send(port, &upgrade(&host, &ours, &media), b"").await.status, 401, "the media token opens images only");
        let r = send(port, &upgrade(&host, &ours, &keyed), b"").await;
        assert_eq!(r.status, 101);
        assert_eq!(r.header("sec-websocket-protocol").as_deref(), Some("groka"), "the key is not echoed");
        // The key may have come from another tab after this one's `/auth`.
        let hello: serde_json::Value = serde_json::from_str(&first_frame(port, &upgrade(&host, &ours, &keyed)).await).unwrap();
        assert_eq!(hello["type"], "hello");
        assert_eq!(hello["media"], token.as_str(), "a connected page can load images");
        let forwarded = upgrade("localhost:8080", "Origin: http://localhost:8080\r\n", &keyed);
        assert_eq!(send(port, &forwarded, b"").await.status, 101, "behind port forwarding");
    }

    #[tokio::test]
    async fn media_needs_the_token_revalidates_and_refuses_other_sites() {
        let (mut hub, port, dir) = hub().await;
        let (key, token) = access(&mut hub, port).await;
        std::fs::write(dir.path().join("pic.png"), png(6, 4)).unwrap();
        assert_eq!(get(port, "/media?path=pic.png", "").await.status, 401);
        assert_eq!(get(port, "/media?path=pic.png", &format!("Cookie: groka_{port}={token}\r\n")).await.status, 401, "no cookie");
        assert_eq!(get(port, &format!("/media?path=pic.png&t={key}"), "").await.status, 401);
        let path = format!("/media?path=pic.png&t={token}");
        let path = path.as_str();
        let with = String::new();
        let r = get(port, path, &with).await;
        assert_eq!(r.status, 200);
        assert_eq!(r.body, png(6, 4));
        assert_eq!(r.header("content-type").as_deref(), Some("image/png"));
        assert_eq!(r.header("cache-control").as_deref(), Some("private, no-cache"));
        let etag = r.header("etag").expect("an etag");
        let r = get(port, path, &format!("{with}If-None-Match: {etag}\r\n")).await;
        assert_eq!(r.status, 304, "an unchanged image is not sent again");
        assert!(r.body.is_empty());
        assert!(r.header("content-type").is_none(), "it would replace the stored type");
        assert_eq!(r.header("etag").as_deref(), Some(etag.as_str()));
        let r = get(port, path, &format!("{with}Sec-Fetch-Site: same-site\r\n")).await;
        assert_eq!(r.status, 401, "a page on another local port");
        let r = get(port, path, &format!("{with}Sec-Fetch-Site: same-origin\r\n")).await;
        assert_eq!(r.status, 200);
        assert_eq!(get(port, &format!("/media?path=missing.png&t={token}"), &with).await.status, 404);
    }

    #[test]
    fn thumbnails_and_oversized_originals_come_scaled() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("wide.png"), png(64, 32)).unwrap();
        let body = |m: Media| match m {
            Media::Body { bytes, mime, .. } => (image::load_from_memory(&bytes).unwrap(), mime),
            Media::Unchanged { .. } => panic!("no etag was sent"),
        };
        let (img, mime) = body(load_media(dir.path(), "wide.png", Some(16), None).unwrap());
        assert_eq!((img.width(), img.height(), mime), (16, 8, "image/jpeg"));
        let (img, mime) = body(load_media(dir.path(), "wide.png", Some(100), None).unwrap());
        assert_eq!((img.width(), img.height(), mime), (64, 32, "image/png"), "small enough: the original");

        // Over the cap: a re-encoded copy instead of a broken image.
        let (img, mime) = body(load_media_capped(dir.path(), "wide.png", None, None, 10).unwrap());
        assert_eq!((img.width(), img.height(), mime), (64, 32, "image/jpeg"));

        let clear = image::RgbaImage::from_pixel(40, 40, image::Rgba([0, 0, 0, 0]));
        clear.save_with_format(dir.path().join("clear.png"), ImageFormat::Png).unwrap();
        let (img, mime) = body(load_media(dir.path(), "clear.png", Some(10), None).unwrap());
        assert_eq!(mime, "image/png", "transparency survives a thumbnail");
        assert!(img.color().has_alpha());

        let etag = match load_media(dir.path(), "wide.png", Some(16), None).unwrap() {
            Media::Body { etag, .. } => etag,
            Media::Unchanged { .. } => unreachable!(),
        };
        let other = match load_media(dir.path(), "wide.png", None, None).unwrap() {
            Media::Body { etag, .. } => etag,
            Media::Unchanged { .. } => unreachable!(),
        };
        assert_ne!(etag, other, "a thumbnail and the original are different bodies");
        let seen = format!("W/\"x\", {etag}");
        assert!(matches!(load_media(dir.path(), "wide.png", Some(16), Some(&seen)).unwrap(), Media::Unchanged { .. }));
    }

    #[test]
    fn fitting_keeps_the_aspect_and_never_enlarges() {
        assert_eq!(fit_edge(100, 50, 200), None);
        assert_eq!(fit_edge(1000, 500, 100), Some((100, 50)));
        assert_eq!(fit_edge(10, 4000, 400), Some((1, 400)));
        assert_eq!(fit_pixels(1280, 9000, PREVIEW_PIXELS), None, "a tall screenshot stays readable");
        let (w, h) = fit_pixels(8000, 6000, PREVIEW_PIXELS).unwrap();
        assert!(w as u64 * h as u64 <= PREVIEW_PIXELS && w > 4000 && (w as f64 / h as f64 - 4.0 / 3.0).abs() < 0.01);
    }

    #[tokio::test]
    async fn an_upload_is_saved_and_handed_to_the_tui() {
        let (mut hub, port, dir) = hub().await;
        let (key, token) = access(&mut hub, port).await;
        let upload = |extra: &str, body: &[u8]| {
            format!(
                "POST /upload?session_id=s1&request_id=r1 HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n\
                 Content-Length: {}\r\n{extra}\r\n",
                body.len()
            )
        };
        let keyed = format!("X-Groka-Key: {key}\r\n");
        let ours = format!("{}{keyed}", ours(port));
        let img = png(32, 32);

        assert_eq!(send(port, &upload(&keyed, &img), &img).await.status, 403, "no Origin");
        let media = format!("{}X-Groka-Key: {token}\r\n", self::ours(port));
        assert_eq!(send(port, &upload(&media, &img), &img).await.status, 401, "the media token opens images only");

        let r = send(port, &upload(&ours, &img), &img).await;
        assert_eq!(r.status, 200);
        let rel = String::from_utf8(r.body).unwrap();
        assert!(rel.starts_with(".groka/inbox/") && rel.ends_with(".jpg"), "{rel}");
        assert!(dir.path().join(&rel).is_file());
        match hub.cmd_rx.try_recv() {
            Ok(UiCommand::AttachUpload { session_id, request_id, workspace, path }) => {
                assert_eq!((session_id.as_str(), request_id.as_str(), path.as_str()), ("s1", "r1", rel.as_str()));
                assert_eq!(workspace, dir.path());
            }
            other => panic!("expected an attach, got {other:?}"),
        }

        let junk = b"not an image at all";
        let r = send(port, &upload(&ours, junk), junk).await;
        assert_eq!(r.status, 422);
        assert!(String::from_utf8_lossy(&r.body).starts_with("未附上"));

        let big = format!(
            "POST /upload?session_id=s1&request_id=r2 HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n\
             Content-Length: {}\r\n{ours}\r\n",
            UPLOAD_MAX + 1
        );
        assert_eq!(send(port, &big, b"").await.status, 413);

        let full = UiSnapshot {
            session_id: "s1".into(),
            pending: vec!["a".into(); vision::MAX_USER_IMAGES],
            ..UiSnapshot::default()
        };
        hub.publish(full, UiLogs::default(), vec![], vec![]);
        // Answered after the body is read, so the browser gets the reason.
        let r = send(port, &upload(&ours, &img), &img).await;
        assert_eq!(r.status, 409, "the session has all it may");
        assert!(String::from_utf8_lossy(&r.body).starts_with("未附上：最多"));
    }

    #[test]
    fn an_upload_declaring_a_huge_image_is_refused_before_decoding() {
        // A 30000x30000 24-bit BMP header (2.7 GB decoded) and no pixels.
        let mut bmp = b"BM".to_vec();
        for v in [54u32, 0, 54, 40, 30000, 30000] {
            bmp.extend_from_slice(&v.to_le_bytes());
        }
        bmp.extend_from_slice(&1u16.to_le_bytes());
        bmp.extend_from_slice(&24u16.to_le_bytes());
        bmp.extend_from_slice(&[0; 24]);
        let dir = tempfile::tempdir().unwrap();
        let err = save_upload(dir.path(), &bmp).unwrap_err().to_string();
        assert!(err.contains("圖片太大"), "{err}");
        assert!(!dir.path().join(".groka/inbox").exists() || std::fs::read_dir(dir.path().join(".groka/inbox")).unwrap().all(|e| {
            e.unwrap().file_name() == ".gitignore"
        }));
        let ok = save_upload(dir.path(), &png(8, 8)).unwrap();
        assert!(dir.path().join(ok).is_file());
    }

    #[test]
    fn only_this_users_connections_get_in() {
        let hub: SocketAddr = "127.0.0.1:41873".parse().unwrap();
        let peer: SocketAddr = "127.0.0.1:52000".parse().unwrap();
        let ip = format!("{:08X}", u32::from_ne_bytes([127, 0, 0, 1]));
        let row = |n: u32, from: u16, to: u16, uid: u32| {
            format!("   {n}: {ip}:{from:04X} {ip}:{to:04X} 01 00000000:00000000 00:00000000 00000000  {uid}        0 1 1 0 20 4 30 10 -1\n")
        };
        let head = "  sl  local_address rem_address   st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode\n";
        let table = |client: u32| format!("{head}{}{}", row(0, 41873, 52000, 1000), row(1, 52000, 41873, client));
        let allowed = |tables: &[&str]| peer_allowed(tables.iter().map(|t| t.to_string()), peer, hub, 1000);
        assert!(allowed(&[&table(1000)]));
        assert!(!allowed(&[&table(1001)]), "another user's client");
        assert!(allowed(&[&table(0)]), "a port relay run by root");
        // Another user's socket to some other port says nothing about ours.
        let other = format!("{head}{}{}", row(0, 41873, 52000, 1000), row(1, 52001, 41873, 1001));
        assert!(allowed(&[&other]));
        assert!(allowed(&[head]), "a table that lists no sockets cannot tell");
        assert!(allowed(&[]));

        // An AF_INET6 client of the IPv4 hub (`::ffff:127.0.0.1`) is in tcp6 only.
        let mapped = format!("{:016X}{:08X}{ip}", 0, u32::from_ne_bytes([0, 0, 0xff, 0xff]));
        let row6 = |from: u16, to: u16, uid: u32| {
            format!("   0: {mapped}:{from:04X} {mapped}:{to:04X} 01 00000000:00000000 00:00000000 00000000  {uid}        0 1 1 0 20 4 30 10 -1\n")
        };
        let only_hub = format!("{head}{}", row(0, 41873, 52000, 1000));
        let tcp6 = |uid: u32| format!("{head}{}", row6(52000, 41873, uid));
        assert!(!allowed(&[&only_hub, &tcp6(1001)]), "another user's dual-stack client");
        assert!(allowed(&[&only_hub, &tcp6(1000)]));
        if cfg!(target_endian = "little") {
            assert!(tcp6(1001).contains("0000000000000000FFFF00000100007F:CB20"), "the kernel's word order");
        }
    }

    #[test]
    fn hub_only_commands_cannot_come_from_a_browser() {
        let raw = serde_json::json!({"type": "web_url", "url": "http://evil"});
        assert!(serde_json::from_value::<UiCommand>(raw).is_err());
        let raw = serde_json::json!({"type": "attach_upload", "session_id": "s", "request_id": "r",
            "workspace": "/", "path": "/etc/passwd"});
        assert!(serde_json::from_value::<UiCommand>(raw).is_err());
        let raw = serde_json::json!({"type": "remove_queued", "session_id": "s", "request_id": "r",
            "index": 0, "expected": "", "images": ["a.jpg"]});
        assert!(matches!(serde_json::from_value::<UiCommand>(raw), Ok(UiCommand::RemoveQueued { .. })));
    }

    #[test]
    fn authorities_and_secrets_compare_strictly() {
        for ok in ["127.0.0.1:41873", "LOCALHOST:8080", "localhost", "[::1]:41873", "[::1]"] {
            assert!(local_authority(ok), "{ok}");
        }
        for bad in ["127.0.0.2:41873", "evil.example:41873", "127.0.0.1:", "127.0.0.1:80x", "127.0.0.1.nip.io:41873"] {
            assert!(!local_authority(bad), "{bad}");
        }
        let req = |host: &str, origin: Option<&str>| {
            let mut h = HeaderMap::new();
            h.insert(header::HOST, HeaderValue::from_str(host).unwrap());
            if let Some(o) = origin {
                h.insert(header::ORIGIN, HeaderValue::from_str(o).unwrap());
            }
            h
        };
        assert!(origin_ok(&req("127.0.0.1:41873", Some("http://127.0.0.1:41873"))));
        assert!(origin_ok(&req("localhost:8080", Some("http://localhost:8080"))), "forwarded");
        assert!(!origin_ok(&req("127.0.0.1:41873", Some("http://127.0.0.1:8000"))), "a page on another port");
        assert!(!origin_ok(&req("127.0.0.1:41873", Some("http://localhost:41873"))));
        assert!(!origin_ok(&req("evil.example:41873", Some("http://evil.example:41873"))), "rebinding");
        assert!(!origin_ok(&req("127.0.0.1:41873", Some("null"))));
        assert!(!origin_ok(&req("127.0.0.1:41873", None)));
        let code = new_code();
        assert!(code.len() == 16 && code.bytes().all(|b| b.is_ascii_hexdigit()), "{code}");
        assert_ne!(code, new_code());
        assert!(secret_eq("abc", "abc"));
        assert!(!secret_eq("abd", "abc"));
        assert!(!secret_eq("ab", "abc"));
        assert!(!secret_eq("", ""), "an empty secret matches nothing");
    }
}
