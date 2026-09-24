//! Application state: every open session, the settings panel, and the UI
//! state of the workbench (which views are open, what the last frame laid
//! out). Actions that change sessions live here; drawing lives in `ui`,
//! input decoding in `input`.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use ratatui::buffer::{Buffer, Cell};
use ratatui::layout::{Position, Rect};
use ratatui_image::picker::Picker;
use ratatui_image::protocol::Protocol;
use tokio::sync::mpsc;

use crate::agent::{CancelFlag, RunOutcome, SessionKnobs, UserTurn};
use crate::ask::AskUserHub;
use crate::events::{AgentEvent, EventMeta, EventSink, FanoutSink};
use crate::folderpick::{self, FolderView};
use crate::session::{self, SessionMeta, SessionStore};
use crate::skills::SkillStore;
use crate::task::{self, TaskHub, TaskPhase};

use super::edit::{clipboard_get, clipboard_set, clipboard_set_image, Edit};
use super::model::agents::{AgentTree, EventKind, SavedAgent};
use super::model::rows::Row;
use super::model::session::{intro_rows, user_row, ChatSel, Queued, Session};
use super::settings::Settings;
use super::TuiOptions;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SendMode {
    Queue,
    Insert,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Submit {
    Start,
    Queue,
    Insert,
}

pub(crate) fn submit_kind(has_session: bool, running: bool, mode: SendMode) -> Submit {
    if !has_session {
        Submit::Start
    } else if running && mode == SendMode::Queue {
        Submit::Queue
    } else {
        Submit::Insert
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Focus {
    Chat,
    Settings,
    Rename,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SideView {
    Sessions,
    Agents,
    Changes,
    Background,
    Task,
}

impl SideView {
    pub const ALL: [SideView; 5] = [
        SideView::Sessions,
        SideView::Agents,
        SideView::Changes,
        SideView::Background,
        SideView::Task,
    ];

    pub fn icon(self) -> &'static str {
        match self {
            Self::Sessions => "≡",
            Self::Agents => "◎",
            Self::Changes => "±",
            Self::Background => "▶",
            Self::Task => "✔",
        }
    }

    pub fn title(self) -> &'static str {
        match self {
            Self::Sessions => "對話",
            Self::Agents => "代理",
            Self::Changes => "檔案變更",
            Self::Background => "背景工作",
            Self::Task => "任務",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum BottomTab {
    Tools,
    Output,
    Events,
}

impl BottomTab {
    pub const ALL: [BottomTab; 3] = [BottomTab::Tools, BottomTab::Output, BottomTab::Events];

    pub fn title(self) -> &'static str {
        match self {
            Self::Tools => "工具",
            Self::Output => "輸出",
            Self::Events => "事件",
        }
    }
}

/// A mouse target from the last frame.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Hit {
    // Workbench chrome
    Activity(u8),
    ActivitySettings,
    SideItem(u16),
    SideScroll,
    EditorTab(u16),
    EditorTabClose(u16),
    BottomTab(u8),
    BottomClose,
    BottomRow(u16),
    OutputPick(u16),
    ReadOnlyBar,
    // Status bar
    StatusTask,
    StatusModel,
    StatusAgents,
    // Sessions view
    NewChat,
    Session(u16),
    RenameSession(u16),
    DeleteSession(u16),
    // Chat
    Chat,
    ChatRow(u16),
    ChatImage(u16),
    ToolGroup(usize),
    ToolItem(usize, usize),
    Think(usize),
    ToolPanel,
    ToolPanelClose,
    JumpBottom,
    ScrollBar,
    ScrollThumb,
    // Composer
    Composer,
    QueueChip,
    InsertChip,
    PasteImage,
    StopChip,
    QueueItem(u16),
    CancelQueueEdit,
    PendingClose(u16),
    // Overlays
    Overlay,
    OverlayClose,
    AskOption(u16),
    AskFill,
    AskConfirm,
    AskCancel,
    TaskDraft,
    TaskConfirm,
    TaskCancel,
    TaskEnd,
    WsPath,
    WsEntry(u16),
    WsConfirm,
    WsCreate,
    WsCancel,
    SkillText,
    // Settings tab
    SetKind(u8),
    SetAccount,
    SetLoginCode,
    SetField(u8),
    SetToggle(u8),
    DropPick(u16),
    SkillToggle(u16),
    SkillRow(u16),
}

pub(crate) struct WorkspacePick {
    pub view: FolderView,
    pub edit: Edit,
    pub cursor: usize,
    pub scroll: u16,
    pub path_focus: bool,
    pub notice: Option<String>,
}

impl WorkspacePick {
    pub fn open(start: &std::path::Path) -> Self {
        let view = folderpick::list_folder(&folderpick::existing_dir(start));
        let edit = Edit::at_end(folderpick::display_path(&view.cwd));
        Self {
            view,
            edit,
            cursor: 0,
            scroll: 0,
            path_focus: true,
            notice: None,
        }
    }

    pub fn sync(&mut self) {
        let fallback = self.view.cwd.clone();
        self.view = folderpick::view_for_input(&self.edit.text, &fallback);
        self.cursor = self.cursor.min(self.view.entries.len().saturating_sub(1));
        self.notice = self.view.error.clone();
    }

    pub fn move_cursor(&mut self, delta: i32) {
        self.path_focus = false;
        let n = self.view.entries.len() as i32;
        self.cursor = if n == 0 {
            0
        } else {
            (self.cursor as i32 + delta).clamp(0, n - 1) as usize
        };
    }

    pub fn enter_dir(&mut self, dir: PathBuf) {
        if !dir.is_dir() {
            self.notice = Some("不是資料夾".into());
            return;
        }
        self.view = folderpick::list_folder(&dir);
        self.edit = Edit::at_end(folderpick::display_path(&self.view.cwd));
        self.cursor = 0;
        self.scroll = 0;
        self.notice = self.view.error.clone();
        self.path_focus = false;
    }

    /// The folder a confirm would pick.
    pub fn target(&self) -> PathBuf {
        let selected = self.view.entries.get(self.cursor).cloned();
        match std::fs::canonicalize(self.edit.text.trim()) {
            Ok(abs) if abs.is_dir() => folderpick::normalize(&abs),
            Ok(abs) if abs.is_file() => folderpick::existing_dir(&abs),
            _ => folderpick::workspace_of(&self.view.cwd, selected.as_ref()),
        }
    }
}

pub(crate) enum TaskUi {
    Form(Edit),
    Status,
}

pub(crate) struct SkillView {
    pub title: String,
    pub origin: String,
    pub edit: Edit,
    pub scroll: u16,
}

/// Selectable glyph run of a drawn chat line.
pub(crate) struct GlyphLine {
    pub y: u16,
    pub x: u16,
    pub text_w: u16,
    pub row: usize,
    pub start: usize,
    pub chars: Vec<char>,
}

/// UI state, including where the last frame put things.
pub(crate) struct Ui {
    pub focus: Focus,
    pub send_mode: SendMode,
    pub side_open: bool,
    pub side_view: SideView,
    pub side_scroll: usize,
    pub bottom: Option<BottomTab>,
    pub bottom_scroll: usize,
    /// Settings editor tab open / shown.
    pub settings_open: bool,
    pub settings_active: bool,
    pub rename: Option<(String, Edit)>,
    pub workspace_pick: Option<WorkspacePick>,
    pub task_ui: Option<TaskUi>,
    pub image_view: Option<String>,
    pub skill_view: Option<SkillView>,
    /// Monitor shown in the inspector overlay.
    pub inspector: Option<String>,
    // Layout of the last frame.
    pub area: Rect,
    pub hits: Vec<(Rect, Hit)>,
    pub chat_inner: Rect,
    pub chat_bar: Rect,
    pub chat_total: u16,
    pub chat_max_off: u16,
    pub chat_glyphs: Vec<GlyphLine>,
    pub image_hits: Vec<String>,
    /// Live think headers (row index, rect) for clock patches.
    pub think_clocks: Vec<(usize, Rect)>,
    pub composer_frame: Rect,
    pub composer_inner: Rect,
    pub composer_vscroll: u16,
    pub composer_snap: Option<Buffer>,
    pub status_bar: Rect,
    pub side_area: Rect,
    pub bottom_area: Rect,
    pub field_inner: Rect,
    pub skill_inner: Rect,
    pub last_caret: Position,
    pub last_clock_cells: Vec<(u16, u16, Cell)>,
    // Pointer drags.
    pub chat_dragging: bool,
    pub input_dragging: bool,
    pub skill_dragging: bool,
    pub scroll_grab: Option<i16>,
    /// Side items of the last frame (what `Hit::SideItem(i)` means).
    pub side_items: Vec<SideItem>,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum SideItem {
    Root,
    Agent(String),
    Change { view: String, row: usize, call: usize },
    Background(String),
    Monitor(String),
    OpenTask,
}

impl Default for Ui {
    fn default() -> Self {
        Self {
            focus: Focus::Chat,
            send_mode: SendMode::Queue,
            side_open: true,
            side_view: SideView::Sessions,
            side_scroll: 0,
            bottom: None,
            bottom_scroll: 0,
            settings_open: false,
            settings_active: false,
            rename: None,
            workspace_pick: None,
            task_ui: None,
            image_view: None,
            skill_view: None,
            inspector: None,
            area: Rect::default(),
            hits: Vec::new(),
            chat_inner: Rect::default(),
            chat_bar: Rect::default(),
            chat_total: 0,
            chat_max_off: 0,
            chat_glyphs: Vec::new(),
            image_hits: Vec::new(),
            think_clocks: Vec::new(),
            composer_frame: Rect::default(),
            composer_inner: Rect::default(),
            composer_vscroll: 0,
            composer_snap: None,
            status_bar: Rect::default(),
            side_area: Rect::default(),
            bottom_area: Rect::default(),
            field_inner: Rect::default(),
            skill_inner: Rect::default(),
            last_caret: Position::ORIGIN,
            last_clock_cells: Vec::new(),
            chat_dragging: false,
            input_dragging: false,
            skill_dragging: false,
            scroll_grab: None,
            side_items: Vec::new(),
        }
    }
}

/// Image previews: terminal graphics protocol and cached renderings.
pub(crate) struct Images {
    pub picker: Option<Picker>,
    pub halfblocks: HashMap<(String, u16), Vec<ratatui::text::Line<'static>>>,
    pub protos: HashMap<(String, u16, u16), Protocol>,
    pub cells: HashMap<(String, u16, u16), (u16, u16)>,
    pub blits: Vec<crate::preview::GraphicBlit>,
    pub last_blits: Vec<crate::preview::GraphicBlit>,
}

impl Images {
    pub fn new(picker: Option<Picker>) -> Self {
        Self {
            picker,
            halfblocks: HashMap::new(),
            protos: HashMap::new(),
            cells: HashMap::new(),
            blits: Vec::new(),
            last_blits: Vec::new(),
        }
    }

    pub fn clear(&mut self) {
        self.halfblocks.clear();
        self.protos.clear();
        self.cells.clear();
        self.blits.clear();
        self.last_blits.clear();
    }
}

/// What the web mirror was last sent.
#[derive(Default)]
pub(crate) struct WebSync {
    pub receipts: Vec<(String, String)>,
    pub sent: HashMap<String, Vec<u64>>,
    pub cache: HashMap<u64, crate::hub::UiRow>,
    pub url: Option<String>,
    /// Composer edits from the terminal / from the browser.
    pub composer_seq: u64,
    pub web_composer_seq: u64,
}

/// Runtime handles the app uses to start agent runs.
#[derive(Clone)]
pub(crate) struct Rt {
    pub sink: Arc<FanoutSink>,
    pub done_tx: mpsc::UnboundedSender<(String, RunOutcome)>,
}

impl Rt {
    /// Handles that go nowhere (tests).
    #[cfg(test)]
    pub fn dummy() -> Self {
        let (done_tx, _) = mpsc::unbounded_channel();
        Self {
            sink: Arc::new(FanoutSink { sinks: vec![] }),
            done_tx,
        }
    }
}

pub(crate) struct App {
    pub opts: TuiOptions,
    pub knobs: Arc<Mutex<SessionKnobs>>,
    pub skills: Arc<Mutex<SkillStore>>,
    pub store: Option<SessionStore>,
    pub launch_workspace: PathBuf,
    pub sessions: HashMap<String, Session>,
    pub current: String,
    /// Session list for the side bar, newest first.
    pub listed: Vec<SessionMeta>,
    listed_at: Option<Instant>,
    pub settings: Settings,
    pub ui: Ui,
    pub images: Images,
    pub web: WebSync,
    pub tick: u8,
    pub rt: Rt,
}

const LIST_REFRESH: Duration = Duration::from_secs(3);

impl App {
    pub fn new(
        opts: TuiOptions,
        store: Option<SessionStore>,
        boot: Session,
        settings: Settings,
        skills: Arc<Mutex<SkillStore>>,
        rt: Rt,
        picker: Option<Picker>,
    ) -> Self {
        let knobs = Arc::new(Mutex::new(SessionKnobs {
            model: opts.model.clone(),
            reasoning_effort: opts.reasoning_effort,
            send_reasoning: true,
            server_tools: crate::kit::search_tools(opts.web_search),
            dispatcher: opts.dispatcher,
            child_model: opts.child_model.clone(),
        }));
        let current = boot.id().to_string();
        let launch_workspace = opts.workspace.clone();
        let mut sessions = HashMap::new();
        sessions.insert(current.clone(), boot);
        let mut app = Self {
            opts,
            knobs,
            skills,
            store,
            launch_workspace,
            sessions,
            current,
            listed: Vec::new(),
            listed_at: None,
            settings,
            ui: Ui::default(),
            images: Images::new(picker),
            web: WebSync::default(),
            tick: 0,
            rt,
        };
        app.refresh_list(true);
        app
    }

    pub fn cur(&self) -> &Session {
        self.sessions.get(&self.current).expect("current session is loaded")
    }

    pub fn cur_mut(&mut self) -> &mut Session {
        self.sessions.get_mut(&self.current).expect("current session is loaded")
    }

    /// Show a short status on the visible chat.
    pub fn flash(&mut self, msg: impl Into<String>) {
        self.cur_mut().chat.status = msg.into();
    }

    pub fn workspace(&self) -> PathBuf {
        self.cur().meta.workspace.clone()
    }

    // —— Sessions ——

    /// Rebuild the side-bar list from the store plus loaded sessions.
    pub fn refresh_list(&mut self, force: bool) {
        if !force && self.listed_at.is_some_and(|t| t.elapsed() < LIST_REFRESH) {
            return;
        }
        self.listed_at = Some(Instant::now());
        let mut by_id: HashMap<String, SessionMeta> = HashMap::new();
        if let Some(list) = self.store.as_ref().and_then(|s| s.list().ok()) {
            for s in list {
                by_id.insert(s.id.clone(), s);
            }
        }
        for s in self.sessions.values() {
            by_id.insert(s.meta.id.clone(), s.meta.clone());
        }
        let mut list: Vec<SessionMeta> = by_id.into_values().collect();
        list.sort_by(|a, b| b.updated_at.cmp(&a.updated_at).then(b.id.cmp(&a.id)));
        self.listed = list;
    }

    fn load_session(&self, id: &str) -> Session {
        let store = self.store.as_ref();
        let meta = store.and_then(|s| s.load_meta(id).ok()).unwrap_or_else(|| {
            let mut m = SessionMeta::new(self.launch_workspace.clone());
            m.id = id.to_string();
            m
        });
        let rows: Vec<Row> = store
            .and_then(|s| s.load_transcript(id).ok())
            .and_then(|v| serde_json::from_value(v).ok())
            .unwrap_or_default();
        let task = TaskHub::from_state(
            id,
            store
                .and_then(|s| s.load_task::<crate::task::TaskState>(id))
                .unwrap_or_default(),
        );
        let mut s = Session::new(meta, rows, task);
        s.agents = store
            .and_then(|st| st.load_agents::<Vec<SavedAgent>>(id))
            .map(AgentTree::restore)
            .unwrap_or_default();
        s
    }

    /// Save a session's transcript / agents if they changed.
    pub fn persist(&mut self, id: &str) {
        let Some(store) = &self.store else {
            return;
        };
        let Some(s) = self.sessions.get_mut(id) else {
            return;
        };
        if s.dirty {
            s.dirty = false;
            if let Ok(rows) = serde_json::to_value(&s.chat.rows) {
                let _ = store.save_transcript(&s.meta.id, &rows);
            }
            s.meta.updated_at = chrono::Utc::now();
            let _ = store.save_meta(&s.meta);
        }
        if s.agents.dirty {
            s.agents.dirty = false;
            let _ = store.save_agents(&s.meta.id, &s.agents.saved());
        }
    }

    pub fn persist_all(&mut self) {
        let ids: Vec<String> = self.sessions.keys().cloned().collect();
        for id in ids {
            self.persist(&id);
        }
    }

    fn after_session_change(&mut self) {
        self.ui.image_view = None;
        self.ui.inspector = None;
        self.ui.task_ui = None;
        self.ui.composer_vscroll = 0;
        self.ui.scroll_grab = None;
        self.ui.settings_active = false;
        self.images.clear();
        self.refresh_list(true);
    }

    pub fn switch_to(&mut self, id: &str) {
        if id == self.current {
            return;
        }
        let cur = self.current.clone();
        self.persist(&cur);
        if !self.sessions.contains_key(id) {
            let loaded = self.load_session(id);
            self.sessions.insert(id.to_string(), loaded);
        }
        self.current = id.to_string();
        self.after_session_change();
    }

    pub fn begin_new_chat(&mut self) {
        self.cancel_rename();
        let start = if self.cur().meta.workspace.as_os_str().is_empty() {
            self.launch_workspace.clone()
        } else {
            self.cur().meta.workspace.clone()
        };
        self.ui.workspace_pick = Some(WorkspacePick::open(&start));
    }

    pub fn create_chat(&mut self, workspace: PathBuf) {
        let workspace = folderpick::existing_dir(&workspace);
        self.launch_workspace = workspace.clone();
        if self.cur().is_blank() {
            let Self { store, sessions, current, .. } = self;
            let s = sessions.get_mut(current.as_str()).expect("current session");
            s.meta.workspace = workspace.clone();
            s.meta.updated_at = chrono::Utc::now();
            if let Some(st) = store.as_ref() {
                let _ = st.save_meta(&s.meta);
            }
            self.flash(format!("工作目錄  {}", folderpick::display_path(&workspace)));
            return;
        }
        let cur = self.current.clone();
        self.persist(&cur);
        let meta = match &self.store {
            Some(st) => st
                .create(workspace.clone())
                .unwrap_or_else(|_| SessionMeta::new(workspace.clone())),
            None => SessionMeta::new(workspace.clone()),
        };
        let id = meta.id.clone();
        let task = TaskHub::new(id.clone());
        self.sessions.insert(id.clone(), Session::new(meta, intro_rows(), task));
        self.current = id;
        self.after_session_change();
        self.flash(format!("工作目錄  {}", folderpick::display_path(&workspace)));
    }

    pub fn delete_session(&mut self, id: &str) {
        self.cancel_rename();
        if let Some(mut s) = self.sessions.remove(id) {
            s.cancel_ask();
            // Dropping the inbox ends the run; its nursery kills the children.
            s.inbox = None;
        }
        if let Some(store) = &self.store {
            let _ = store.delete(id);
        }
        self.refresh_list(true);
        if id != self.current {
            return;
        }
        let next = self.listed.iter().map(|m| m.id.clone()).find(|n| n != id);
        match next {
            Some(n) => {
                if !self.sessions.contains_key(&n) {
                    let loaded = self.load_session(&n);
                    self.sessions.insert(n.clone(), loaded);
                }
                self.current = n;
            }
            None => {
                let ws = self.launch_workspace.clone();
                let meta = match &self.store {
                    Some(st) => st.create(ws.clone()).unwrap_or_else(|_| SessionMeta::new(ws)),
                    None => SessionMeta::new(ws),
                };
                let nid = meta.id.clone();
                let task = TaskHub::new(nid.clone());
                self.sessions.insert(nid.clone(), Session::new(meta, intro_rows(), task));
                self.current = nid;
            }
        }
        self.after_session_change();
    }

    /// LLM-suggested title (ignored after a manual rename).
    pub fn apply_title(&mut self, id: &str, name: &str) {
        if let Some(s) = self.sessions.get_mut(id) {
            match &self.store {
                Some(st) => {
                    let _ = st.touch_name(&mut s.meta, name.to_string(), true);
                }
                None if !s.meta.name_is_manual => {
                    s.meta.name = session::sanitize_title(name);
                    s.meta.named = true;
                }
                None => {}
            }
        }
        self.refresh_list(true);
    }

    pub fn begin_rename(&mut self, id: &str) {
        let name = self
            .listed
            .iter()
            .find(|m| m.id == id)
            .map(|m| m.name.clone())
            .unwrap_or_else(|| "新對話".into());
        self.ui.rename = Some((id.to_string(), Edit::at_end(name)));
        self.ui.focus = Focus::Rename;
    }

    pub fn cancel_rename(&mut self) {
        self.ui.rename = None;
        if self.ui.focus == Focus::Rename {
            self.ui.focus = Focus::Chat;
        }
    }

    pub fn commit_rename(&mut self) {
        let Some((id, edit)) = self.ui.rename.take() else {
            return;
        };
        if self.ui.focus == Focus::Rename {
            self.ui.focus = Focus::Chat;
        }
        let name = session::sanitize_title(&edit.text);
        if !name.is_empty() {
            self.rename_manual(&id, &name);
        }
    }

    pub fn rename_manual(&mut self, id: &str, name: &str) {
        match (self.sessions.get_mut(id), &self.store) {
            (Some(s), Some(st)) => {
                let _ = st.rename_manual(&mut s.meta, name.to_string());
            }
            (Some(s), None) => {
                s.meta.name = name.to_string();
                s.meta.named = true;
                s.meta.name_is_manual = true;
            }
            (None, Some(st)) => {
                if let Ok(mut meta) = st.load_meta(id) {
                    let _ = st.rename_manual(&mut meta, name.to_string());
                }
            }
            (None, None) => {}
        }
        self.refresh_list(true);
    }

    // —— Events ——

    pub fn route_event(&mut self, ev: AgentEvent) {
        let sid = ev.session_id().to_string();
        if let AgentEvent::SessionNamed { name, .. } = &ev {
            self.apply_title(&sid, name);
            return;
        }
        let visible = sid == self.current;
        let Some(s) = self.sessions.get_mut(&sid) else {
            return;
        };
        let routed = s.route(ev, visible);
        if routed.ask_opened && visible {
            self.ui.focus = Focus::Chat;
            self.ui.settings_active = false;
        }
        if routed.run_finished {
            self.persist(&sid);
        }
    }

    pub fn finish_run(&mut self, id: &str, out: RunOutcome) {
        if let Some(s) = self.sessions.get_mut(id) {
            s.finish_run(out.turns);
        }
        self.persist(id);
    }

    // —— Sending ——

    /// Composer submit (Enter; Ctrl+Enter forces insert).
    pub fn submit_current(&mut self, force_insert: bool) {
        let Some(turn) = self.cur_mut().take_turn() else {
            return;
        };
        let mode = if force_insert { SendMode::Insert } else { self.ui.send_mode };
        let s = self.cur();
        match submit_kind(s.inbox.is_some(), s.chat.running, mode) {
            Submit::Queue => {
                let q = Queued {
                    text: turn.text.clone(),
                    images: user_row(&turn).images,
                };
                self.cur_mut().queue.push_back(q);
            }
            Submit::Start | Submit::Insert => self.start_or_send(turn, true),
        }
    }

    /// Send into the running session, or start one.
    pub fn start_or_send(&mut self, turn: UserTurn, echo: bool) {
        if echo {
            self.cur_mut().chat.push(Row::User(user_row(&turn)));
        }
        self.snapshot_conn();
        self.adopt_route_for_model();
        if !self.settings.logged_in {
            let msg = self.not_ready_message();
            self.cur_mut().chat.push(Row::Err(msg));
            return;
        }
        self.cur_mut().chat.mark_work_start();
        let id = self.current.clone();
        if let Some(tx) = self.cur().inbox.clone() {
            let _ = tx.send(turn);
            let s = self.cur_mut();
            s.chat.running = true;
            s.chat.awaiting = false;
            s.chat.status = "工作中".into();
            s.dirty = true;
            self.persist(&id);
            return;
        }
        if !self.cur().meta.named {
            let fallback = if task::is_kick(&turn.text) {
                "任務模式".to_string()
            } else {
                session::title_fallback_from_user_text(&turn.text)
            };
            let Self { store, sessions, current, .. } = self;
            let meta = &mut sessions.get_mut(current.as_str()).expect("current session").meta;
            match store.as_ref() {
                Some(st) => {
                    let _ = st.touch_name(meta, fallback, false);
                }
                None => {
                    meta.name = fallback;
                    meta.named = true;
                }
            }
            super::runtime::spawn_title(&self.rt.sink, &id, self.settings.conn.clone(), &self.opts.model, &turn.text);
        }
        let (inbox_tx, inbox_rx) = mpsc::unbounded_channel();
        let cancel = CancelFlag::new();
        let ask = AskUserHub::new();
        let s = self.cur_mut();
        s.dirty = true;
        s.chat.running = true;
        s.chat.awaiting = false;
        s.chat.status = "工作中".into();
        s.inbox = Some(inbox_tx);
        s.cancel = Some(cancel.clone());
        s.ask_hub = Some(ask.clone());
        let task = s.task.clone();
        let workspace = s.meta.workspace.clone();
        self.persist(&id);
        self.refresh_list(true);
        let mut opts = self.opts.clone();
        opts.workspace = workspace;
        if self.settings.conn.route_for(&opts.model).is_openai() {
            opts.web_search = false;
        }
        super::runtime::spawn_run(super::runtime::RunSpec {
            opts,
            turn,
            rt: self.rt.clone(),
            knobs: self.knobs.clone(),
            skills: self.skills.clone(),
            inbox: inbox_rx,
            run_id: id,
            ask,
            cancel,
            task,
            cfg: self.settings.conn.clone(),
        });
    }

    /// Start the queue's head when no run is alive.
    pub fn kick_idle_queue(&mut self) {
        let s = self.cur();
        if s.queue_edit.is_some() || s.chat.running || s.inbox.is_some() || s.queue.is_empty() {
            return;
        }
        let msg = self.cur_mut().queue.pop_front().expect("checked");
        self.start_or_send(msg.into_turn(), true);
    }

    /// Deliver queued messages in every session that is waiting.
    pub fn flush_queues(&mut self) {
        for s in self.sessions.values_mut() {
            if s.flush_queue() {
                s.dirty = true;
            }
        }
    }

    pub fn interrupt(&mut self) -> bool {
        self.cur_mut().interrupt()
    }

    // —— Attachments & clipboard ——

    pub fn attach_pending(&mut self, rel: String) -> bool {
        let s = self.cur_mut();
        if s.pending.len() >= crate::vision::MAX_USER_IMAGES {
            let msg = format!("最多 {} 張圖片", crate::vision::MAX_USER_IMAGES);
            self.flash(msg);
            return false;
        }
        if !s.pending.contains(&rel) {
            s.pending.push(rel);
        }
        true
    }

    fn ingest_paths(&mut self, paths: &[PathBuf]) -> usize {
        let ws = self.workspace();
        let mut n = 0;
        for p in paths {
            match crate::vision::ingest_image_file(&ws, p) {
                Ok(rel) => {
                    if self.attach_pending(rel) {
                        n += 1;
                    }
                }
                Err(e) => self.flash(format!("無法加入圖片: {e}")),
            }
        }
        n
    }

    fn ingest_clipboard_images(&mut self) -> bool {
        if let Some(img) = crate::clipimg::read_image() {
            return match crate::vision::save_user_image(&self.workspace(), &img) {
                Ok(rel) => {
                    let ok = self.attach_pending(rel);
                    if ok {
                        self.flash("已貼上圖片");
                    }
                    ok
                }
                Err(e) => {
                    self.flash(format!("無法貼上圖片: {e}"));
                    false
                }
            };
        }
        let files = crate::clipimg::read_image_files();
        if files.is_empty() {
            return false;
        }
        let n = self.ingest_paths(&files);
        if n > 0 {
            self.flash(format!("已附上 {n} 張圖片"));
        }
        n > 0
    }

    /// "貼上圖片" button: clipboard image or copied image files.
    pub fn paste_image(&mut self) {
        if !self.ingest_clipboard_images() {
            self.flash("剪貼簿沒有圖片 — 先複製截圖或圖片檔，再點「貼上圖片」");
        }
    }

    /// Text pasted into the composer; dropped image paths become attachments.
    pub fn paste_text(&mut self, s: &str) {
        let dropped = crate::vision::parse_image_drop(s);
        if !dropped.is_empty() {
            let n = self.ingest_paths(&dropped);
            if n > 0 {
                self.flash(format!("已附上 {n} 張圖片"));
                return;
            }
        }
        self.cur_mut().draft.insert_str(s);
    }

    /// Bracketed paste. An empty payload (Ctrl+V of a bitmap) reads the clipboard.
    pub fn paste_from_terminal(&mut self, s: &str) {
        if s.trim().is_empty() && crate::vision::parse_image_drop(s).is_empty() {
            self.paste_clipboard();
        } else {
            self.paste_text(s);
        }
    }

    pub fn paste_clipboard(&mut self) {
        if self.ingest_clipboard_images() {
            return;
        }
        if let Some(s) = clipboard_get() {
            self.paste_text(&s);
        }
    }

    /// Ctrl+C: copy the selection. `false` = nothing selected.
    pub fn copy_selection(&mut self) -> bool {
        let text = if let Some(v) = &self.ui.skill_view {
            v.edit.selected_text()
        } else {
            None
        }
        .or_else(|| self.cur().draft.selected_text());
        if let Some(text) = text {
            let msg = if clipboard_set(&text) { "已複製" } else { "無法複製到剪貼簿" };
            self.flash(msg);
            return true;
        }
        let sel = self.cur().view().sel;
        match sel {
            ChatSel::Image(rel) => {
                let abs = self.workspace().join(&rel);
                let msg = if clipboard_set_image(&abs) {
                    "已複製圖片"
                } else if clipboard_set(&rel) {
                    "已複製路徑"
                } else {
                    "無法複製到剪貼簿"
                };
                self.flash(msg);
                true
            }
            ChatSel::Text { .. } => {
                let text = super::ui::chat::selected_text(&self.cur().view_transcript().rows, &sel);
                match text.filter(|t| !t.is_empty()) {
                    Some(t) => {
                        let msg = if clipboard_set(&t) { "已複製" } else { "無法複製到剪貼簿" };
                        self.flash(msg);
                        true
                    }
                    None => false,
                }
            }
            ChatSel::None => false,
        }
    }

    pub fn open_image(&mut self, rel: String) {
        self.cur_mut().view_mut().sel = ChatSel::Image(rel.clone());
        self.ui.image_view = Some(rel);
    }

    // —— Task mode ——

    pub fn open_task(&mut self) {
        let phase = self.cur().task.snapshot().phase;
        self.ui.task_ui = Some(if phase.is_live() || matches!(phase, TaskPhase::Done | TaskPhase::Failed) {
            TaskUi::Status
        } else {
            TaskUi::Form(Edit::default())
        });
    }

    pub fn close_task(&mut self) {
        self.ui.task_ui = None;
    }

    pub fn submit_task_goal(&mut self, goal: &str) {
        let goal = goal.trim().to_string();
        if goal.is_empty() {
            self.flash("請填寫任務目標");
            return;
        }
        let s = self.cur_mut();
        s.task.start_goal(goal.clone());
        s.chat.push(Row::meta(format!("已啟動任務模式：{goal}")));
        s.agents.log("", EventKind::Message, format!("任務目標：{goal}"));
        s.dirty = true;
        self.ui.task_ui = Some(TaskUi::Status);
        if self.cur().inbox.is_none() {
            self.start_or_send(UserTurn::from(task::KICK), false);
        }
    }

    pub fn end_task(&mut self) {
        let phase = self.cur().task.snapshot().phase;
        if phase.is_live() || matches!(phase, TaskPhase::Done | TaskPhase::Failed) {
            let s = self.cur_mut();
            s.task.end();
            s.agents.log("", EventKind::Message, "任務模式由使用者結束");
            s.chat.push(Row::meta("已結束任務模式"));
        }
        self.close_task();
    }

    // —— Skills ——

    pub fn refresh_skills(&mut self) {
        let ws = self.workspace();
        let list = self.skills.lock().unwrap_or_else(|e| e.into_inner()).scan(&ws);
        self.settings.skill_cursor = self.settings.skill_cursor.min(list.len().saturating_sub(1));
        self.settings.skill_list = list;
    }

    pub fn toggle_import(&mut self, claude: bool) {
        let mut g = self.skills.lock().unwrap_or_else(|e| e.into_inner());
        let on = if claude {
            let on = !g.prefs().import_claude;
            let _ = g.set_import_claude(on);
            on
        } else {
            let on = !g.prefs().import_codex;
            let _ = g.set_import_codex(on);
            on
        };
        drop(g);
        self.refresh_skills();
        let who = if claude { "Claude Code" } else { "Codex" };
        self.flash(if on {
            format!("已引入 {who} 技能")
        } else {
            format!("已停止引入 {who} 技能")
        });
    }

    pub fn toggle_skill(&mut self, i: usize) {
        let Some(skill) = self.settings.skill_list.get(i).cloned() else {
            return;
        };
        let _ = self
            .skills
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .set_enabled(&skill.id, !skill.enabled);
        self.refresh_skills();
    }

    pub fn open_skill(&mut self, i: usize) {
        let Some(skill) = self.settings.skill_list.get(i).cloned() else {
            return;
        };
        let body = crate::skills::read_skill_file(&skill.path).unwrap_or_else(|e| e.to_string());
        let mut edit = Edit::at_end(body);
        edit.home(false);
        self.ui.skill_view = Some(super::app::SkillView {
            title: skill.name,
            origin: skill.origin.label().to_string(),
            edit,
            scroll: 0,
        });
    }

    // —— Tabs & views ——

    pub fn open_settings(&mut self) {
        self.ui.settings_open = true;
        self.ui.settings_active = true;
        self.ui.focus = Focus::Settings;
        self.settings.drop = None;
        self.refresh_skills();
        if self.settings.logged_in
            && !matches!(
                self.settings.catalog_status,
                super::settings::CatalogStatus::Ready | super::settings::CatalogStatus::Loading
            )
        {
            self.settings.want_catalog = true;
        }
    }

    pub fn close_settings(&mut self) {
        self.flush_conn();
        self.settings.drop = None;
        self.ui.settings_open = false;
        self.ui.settings_active = false;
        self.ui.skill_view = None;
        if self.ui.focus == Focus::Settings {
            self.ui.focus = Focus::Chat;
        }
    }

    /// Show the main chat (or an agent tab) instead of settings.
    pub fn show_chat(&mut self, agent: Option<String>) {
        self.ui.settings_active = false;
        if self.ui.focus == Focus::Settings {
            self.ui.focus = Focus::Chat;
        }
        let s = self.cur_mut();
        match agent {
            Some(p) => s.open_agent(&p),
            None => s.active = None,
        }
    }

    /// Editor tabs in strip order.
    pub fn tabs(&self) -> Vec<Tab> {
        let mut out = vec![Tab::Chat];
        out.extend(self.cur().open_tabs.iter().map(|p| Tab::Agent(p.clone())));
        if self.ui.settings_open {
            out.push(Tab::Settings);
        }
        out
    }

    pub fn active_tab(&self) -> Tab {
        if self.ui.settings_active && self.ui.settings_open {
            return Tab::Settings;
        }
        match self.cur().view_key() {
            k if k.is_empty() => Tab::Chat,
            k => Tab::Agent(k),
        }
    }

    pub fn activate(&mut self, tab: Tab) {
        match tab {
            Tab::Chat => self.show_chat(None),
            Tab::Agent(p) => self.show_chat(Some(p)),
            Tab::Settings => self.open_settings(),
        }
    }

    pub fn close_tab(&mut self, tab: &Tab) {
        match tab {
            Tab::Chat => {}
            Tab::Agent(p) => {
                let p = p.clone();
                self.cur_mut().close_agent(&p);
            }
            Tab::Settings => self.close_settings(),
        }
    }

    pub fn cycle_tab(&mut self, delta: i32) {
        let tabs = self.tabs();
        let cur = self.active_tab();
        let i = tabs.iter().position(|t| *t == cur).unwrap_or(0) as i32;
        let n = tabs.len() as i32;
        let next = tabs[(i + delta).rem_euclid(n) as usize].clone();
        self.activate(next);
    }

    /// Jump to a tool call in the transcript that owns it.
    pub fn reveal_tool(&mut self, view: &str, row: usize, call: usize) {
        self.show_chat((!view.is_empty()).then(|| view.to_string()));
        self.cur_mut().view_mut().open_tool = Some((row, call));
    }

    /// Anything is spinning: advance the animation tick.
    pub fn pulsing(&self) -> bool {
        self.sessions.values().any(|s| {
            s.chat.running
                || s.agents.live_count() > 0
                || s.backgrounds.iter().any(|b| b.alive)
                || s.monitors.iter().any(|m| m.alive)
        })
    }

    pub fn has_modal(&self) -> bool {
        self.ui.workspace_pick.is_some()
            || self.cur().ask.is_some()
            || self.ui.task_ui.is_some()
            || self.ui.image_view.is_some()
            || self.ui.skill_view.is_some()
            || self.ui.inspector.is_some()
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Tab {
    Chat,
    Agent(String),
    Settings,
}

/// Session to resume at start: the newest one with real content; blank
/// drafts from earlier launches are skipped.
pub(crate) fn boot_session(store: Option<&SessionStore>, workspace: PathBuf) -> (Session, bool) {
    let listed = store.and_then(|s| s.list().ok()).unwrap_or_default();
    let rows_of = |id: &str| -> Vec<Row> {
        store
            .and_then(|s| s.load_transcript(id).ok())
            .and_then(|v| serde_json::from_value(v).ok())
            .unwrap_or_default()
    };
    let pick = listed
        .iter()
        .find(|m| m.named || m.name_is_manual || rows_of(&m.id).iter().any(|r| !matches!(r, Row::Meta(_))))
        .or_else(|| listed.first())
        .cloned();
    if let Some(meta) = pick {
        let id = meta.id.clone();
        let rows = rows_of(&id);
        let rows = if rows.is_empty() { intro_rows() } else { rows };
        let task = TaskHub::from_state(
            id.clone(),
            store
                .and_then(|s| s.load_task::<crate::task::TaskState>(&id))
                .unwrap_or_default(),
        );
        let mut s = Session::new(meta, rows, task);
        s.agents = store
            .and_then(|st| st.load_agents::<Vec<SavedAgent>>(&id))
            .map(AgentTree::restore)
            .unwrap_or_default();
        return (s, false);
    }
    let meta = match store {
        Some(s) => s.create(workspace.clone()).unwrap_or_else(|_| SessionMeta::new(workspace)),
        None => SessionMeta::new(workspace),
    };
    let task = TaskHub::new(meta.id.clone());
    (Session::new(meta, intro_rows(), task), true)
}

/// Emit a session title event (the runtime also uses this for LLM titles).
pub(crate) fn title_event(sink: &dyn EventSink, session_id: &str, name: String) {
    sink.emit(&AgentEvent::SessionNamed {
        meta: EventMeta {
            ts: chrono::Utc::now(),
            agent_name: "root".into(),
            run_id: session_id.to_string(),
            parent_run_id: None,
            path: String::new(),
        },
        name,
    });
}
