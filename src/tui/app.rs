//! Application state: every open session, the settings panel, and the UI
//! state of the workbench (which views are open, what the last frame laid
//! out). Actions that change sessions live here; drawing lives in `ui`,
//! input decoding in `input`.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

use ratatui::buffer::{Buffer, Cell};
use ratatui::layout::{Position, Rect};
use ratatui_image::picker::Picker;
use ratatui_image::FontSize;
use tokio::sync::mpsc;

use crate::agent::{CancelFlag, RunOutcome, SessionKnobs, UserTurn};
use crate::ask::AskUserHub;
use crate::events::{AgentEvent, EventMeta, EventSink, FanoutSink};
use crate::folderpick::{self, FolderView};
use crate::preview::{Graphic, GraphicBlit, Placement};
use crate::session::{self, SessionMeta, SessionStore};
use crate::skills::SkillStore;
use crate::task::{self, TaskHub, TaskPhase, TaskState};

use super::edit::{clipboard_get, clipboard_set, clipboard_set_image, Edit};
use super::model::agents::{AgentTree, EventKind, SavedAgent};
use super::model::rows::{self, Row};
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

    /// Two-character label under the activity-bar icon.
    pub fn short(self) -> &'static str {
        match self {
            Self::Sessions => "對話",
            Self::Agents => "代理",
            Self::Changes => "變更",
            Self::Background => "背景",
            Self::Task => "任務",
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
    TaskCriteria,
    TaskConfirm,
    TaskCancel,
    TaskAct(TaskAction),
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
    Form(TaskForm),
    Status(TaskStatus),
}

/// The task form: a new goal, or the live task's goal and criteria to edit.
#[derive(Default)]
pub(crate) struct TaskForm {
    pub goal: Edit,
    pub criteria: Edit,
    /// Typing goes to the criteria field (Tab switches).
    pub on_criteria: bool,
    /// Prefilled from the current task: submitting edits it.
    pub editing: bool,
    /// Vertical scroll of each field, so a click lands where it was drawn.
    pub scroll: [u16; 2],
}

impl TaskForm {
    /// Prefilled from the task for 修改.
    pub fn editing(t: &TaskState) -> Self {
        Self {
            goal: Edit::at_end(t.goal.clone()),
            criteria: Edit::at_end(t.criteria.clone()),
            editing: true,
            ..Self::default()
        }
    }

    pub fn field_mut(&mut self) -> &mut Edit {
        if self.on_criteria {
            &mut self.criteria
        } else {
            &mut self.goal
        }
    }

    pub fn limit(&self) -> usize {
        if self.on_criteria {
            task::MAX_CRITERIA
        } else {
            task::MAX_GOAL
        }
    }
}

/// The task status dialog: which action the keyboard is on, and how far the
/// text is scrolled.
pub(crate) struct TaskStatus {
    pub focus: TaskAction,
    pub scroll: u16,
}

impl Default for TaskStatus {
    fn default() -> Self {
        // Enter keeps closing the dialog until the user moves.
        Self { focus: TaskAction::Close, scroll: 0 }
    }
}

/// What the user can do to a task from the dialog, the side panel or the web.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TaskAction {
    Pause,
    Resume,
    Review,
    Accept,
    Edit,
    End,
    Close,
}

impl TaskAction {
    pub fn label(self) -> &'static str {
        match self {
            Self::Pause => "暫停",
            Self::Resume => "繼續",
            Self::Review => "立即審查",
            Self::Accept => "判定完成",
            Self::Edit => "修改",
            Self::End => "結束任務",
            Self::Close => "關閉",
        }
    }

    /// Its name in the web snapshot; the page closes its dialog itself.
    pub fn web_id(self) -> Option<&'static str> {
        match self {
            Self::Pause => Some("pause"),
            Self::Resume => Some("resume"),
            Self::Review => Some("review"),
            Self::Accept => Some("accept"),
            Self::Edit => Some("edit"),
            Self::End => Some("end"),
            Self::Close => None,
        }
    }

    /// Pause and resume are one button that changes its label.
    pub fn same(self, other: TaskAction) -> bool {
        self == other || matches!((self, other), (Self::Pause, Self::Resume) | (Self::Resume, Self::Pause))
    }

    /// The letter that runs it in the status dialog.
    pub fn key(self) -> Option<char> {
        match self {
            Self::Pause | Self::Resume => Some('p'),
            Self::Review => Some('r'),
            Self::Accept => Some('d'),
            Self::Edit => Some('e'),
            Self::End => Some('x'),
            Self::Close => None,
        }
    }
}

/// How long a first press of 結束任務 waits for the second.
pub(crate) const TASK_END_CONFIRM: Duration = Duration::from_secs(4);

/// Why a live proxy is not pushing: paused by the user, or stopped by Esc
/// (it stays quiet until 繼續, 立即審查 or a message of the user's own).
pub(crate) fn task_hold(t: &TaskState) -> Option<&'static str> {
    if !t.phase.is_live() {
        None
    } else if t.paused {
        Some("已暫停")
    } else if t.skip_steer {
        Some("已停止")
    } else {
        None
    }
}

/// The actions that make sense for the task as it stands. A paused proxy
/// makes no calls at all, so it cannot review until resumed.
pub(crate) fn task_actions(t: &TaskState) -> Vec<TaskAction> {
    let mut out = Vec::new();
    if t.phase.is_live() {
        out.push(if task_hold(t).is_some() { TaskAction::Resume } else { TaskAction::Pause });
        if !t.paused {
            out.push(TaskAction::Review);
        }
        out.push(TaskAction::Accept);
    }
    if t.phase != TaskPhase::Inactive {
        out.push(TaskAction::Edit);
        out.push(TaskAction::End);
    }
    out.push(TaskAction::Close);
    out
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
    /// Inner rects of the task form's goal and criteria fields.
    pub task_fields: [Rect; 2],
    /// When and in which session 結束任務 was pressed once; a second press
    /// there soon after ends the task.
    pub task_end_armed: Option<(Instant, String)>,
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
    TaskAct(TaskAction),
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
            task_fields: [Rect::default(); 2],
            task_end_armed: None,
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

/// One version of an image file: its path plus length and mtime, so a file
/// rewritten in place (a re-read chart, a reused screenshot path) gets fresh
/// previews instead of its first decode.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) struct ImageKey {
    pub path: PathBuf,
    len: u64,
    modified: Option<SystemTime>,
}

impl ImageKey {
    pub fn of(path: PathBuf) -> Self {
        let meta = std::fs::metadata(&path).ok();
        Self {
            len: meta.as_ref().map_or(0, |m| m.len()),
            modified: meta.and_then(|m| m.modified().ok()),
            path,
        }
    }
}

/// Rendered previews; once over budget the least recently drawn go first.
struct Lru<K, V> {
    /// Value, its size in bytes, and the frame that last drew it.
    map: HashMap<K, (V, usize, u64)>,
    bytes: usize,
    budget: usize,
}

impl<K: std::hash::Hash + Eq + Clone, V> Lru<K, V> {
    fn new(budget: usize) -> Self {
        Self { map: HashMap::new(), bytes: 0, budget }
    }

    fn contains(&self, key: &K) -> bool {
        self.map.contains_key(key)
    }

    fn get(&mut self, key: &K, frame: u64) -> Option<&V> {
        let entry = self.map.get_mut(key)?;
        entry.2 = frame;
        Some(&entry.0)
    }

    fn insert(&mut self, key: K, value: V, bytes: usize, frame: u64) {
        if let Some(old) = self.map.insert(key, (value, bytes, frame)) {
            self.bytes -= old.1;
        }
        self.bytes += bytes;
    }

    /// Keep what `frame` drew; drop the oldest of the rest down to budget.
    fn trim(&mut self, frame: u64) {
        if self.bytes <= self.budget {
            return;
        }
        let mut old: Vec<(u64, K)> = self
            .map
            .iter()
            .filter(|(_, e)| e.2 < frame)
            .map(|(k, e)| (e.2, k.clone()))
            .collect();
        old.sort_unstable_by_key(|(used, _)| *used);
        for (_, key) in old {
            if self.bytes <= self.budget {
                break;
            }
            if let Some(e) = self.map.remove(&key) {
                self.bytes -= e.1;
            }
        }
    }

    fn clear(&mut self) {
        self.map.clear();
        self.bytes = 0;
    }
}

/// A Kitty viewer image is ~3.6 MB of base64; thumbnails a fraction of that.
const GRAPHICS_BUDGET: usize = 64 << 20;
const HALFBLOCK_BUDGET: usize = 16 << 20;
/// No new viewer decode this soon after a resize: dragging a window edge
/// would otherwise encode the picture once per intermediate size.
const RESIZE_SETTLE: Duration = Duration::from_millis(200);
/// How long one stat stands for a file: every image row needs its key on
/// every frame, and frames come every 80 ms while the agent works.
const STAT_TTL: Duration = Duration::from_secs(1);

type PreviewKey = (ImageKey, u16, u16);

/// Image previews: terminal graphics protocol and cached renderings, keyed by
/// file version and size in cells.
pub(crate) struct Images {
    pub picker: Option<Picker>,
    /// Recent stats by path, and the transcript length they were taken at.
    keys: HashMap<PathBuf, (Instant, ImageKey)>,
    keys_rows: usize,
    /// Header-only sizes; `None` = not a readable image.
    dims: HashMap<ImageKey, Option<(u32, u32)>>,
    halfblocks: Lru<PreviewKey, Vec<ratatui::text::Line<'static>>>,
    graphics: Lru<PreviewKey, Option<Graphic>>,
    pub blits: Vec<GraphicBlit>,
    pub last_blits: Vec<GraphicBlit>,
    pub placements: Vec<Placement>,
    /// Cell size the tty last reported, to notice a font zoom on resize.
    pub cell_px: Option<FontSize>,
    resized_at: Option<Instant>,
    /// A preview was put off during a resize; repaint once it settles.
    redraw_pending: bool,
    frame: u64,
}

impl Images {
    pub fn new(picker: Option<Picker>) -> Self {
        Self {
            picker,
            keys: HashMap::new(),
            keys_rows: 0,
            dims: HashMap::new(),
            halfblocks: Lru::new(HALFBLOCK_BUDGET),
            graphics: Lru::new(GRAPHICS_BUDGET),
            blits: Vec::new(),
            last_blits: Vec::new(),
            placements: Vec::new(),
            cell_px: None,
            resized_at: None,
            redraw_pending: false,
            frame: 0,
        }
    }

    pub fn uses_graphics(&self) -> bool {
        self.picker.as_ref().is_some_and(crate::preview::uses_graphics)
    }

    /// The file at `path` as it is now, stat()ed at most once per `STAT_TTL`.
    pub fn key(&mut self, path: PathBuf) -> ImageKey {
        if let Some((at, key)) = self.keys.get(&path) {
            if at.elapsed() < STAT_TTL {
                return key.clone();
            }
        }
        if self.keys.len() >= 4096 {
            self.keys.clear();
        }
        let key = ImageKey::of(path.clone());
        self.keys.insert(path, (Instant::now(), key.clone()));
        key
    }

    /// Stat again once the transcript changes length: a new row may show a
    /// file rewritten under the same name since the last stat.
    pub fn sync_rows(&mut self, rows: usize) {
        if rows != self.keys_rows {
            self.keys_rows = rows;
            self.keys.clear();
        }
    }

    fn dims(&mut self, key: &ImageKey) -> Option<(u32, u32)> {
        if let Some(dims) = self.dims.get(key) {
            return *dims;
        }
        if self.dims.len() >= 4096 {
            self.dims.clear();
        }
        let dims = crate::vision::image_dimensions(&key.path).ok();
        self.dims.insert(key.clone(), dims);
        dims
    }

    /// Cells a picture takes, read from the file header only: a chat
    /// thumbnail (`thumb`, capped) or the viewer (fit to the bounds).
    pub fn cells(&mut self, key: &ImageKey, max_cols: u16, max_rows: u16, thumb: bool) -> (u16, u16) {
        use crate::preview::{cell_size_fit, cell_size_for, halfblock_size, MAX_COLS, MAX_ROWS};
        let dims = self.dims(key);
        match self.picker.as_ref().filter(|p| crate::preview::uses_graphics(p)) {
            Some(p) if thumb => cell_size_for(p, dims, max_cols, max_rows),
            Some(p) => cell_size_fit(p, dims, max_cols, max_rows),
            None if thumb => halfblock_size(dims, max_cols.min(MAX_COLS).max(4), max_rows.min(MAX_ROWS)),
            None => halfblock_size(dims, max_cols, max_rows),
        }
    }

    /// The encoded picture for `w`×`h` cells; `None` if it cannot be decoded.
    pub fn graphic(&mut self, key: &ImageKey, w: u16, h: u16) -> Option<Graphic> {
        let k = (key.clone(), w, h);
        if let Some(g) = self.graphics.get(&k, self.frame) {
            return g.clone();
        }
        let picker = self.picker.as_ref()?;
        let g = crate::preview::graphic_for(picker, &key.path, w, h);
        let bytes = g.as_ref().map_or(0, |g| g.bytes(picker.font_size()));
        self.graphics.insert(k, g.clone(), bytes, self.frame);
        g
    }

    /// Half-block lines filling at most `w`×`h` cells.
    pub fn halfblock(&mut self, key: &ImageKey, w: u16, h: u16) -> &[ratatui::text::Line<'static>] {
        let k = (key.clone(), w, h);
        if !self.halfblocks.contains(&k) {
            let lines = crate::preview::from_path(&key.path, w, h);
            let bytes = lines
                .iter()
                .map(|l| std::mem::size_of_val(l) + l.spans.len() * std::mem::size_of::<ratatui::text::Span>())
                .sum();
            self.halfblocks.insert(k.clone(), lines, bytes, self.frame);
        }
        self.halfblocks.get(&k, self.frame).map_or(&[], Vec::as_slice)
    }

    /// Right after a resize, put off a preview that is not cached yet;
    /// `true` = draw a placeholder this frame.
    pub fn defer(&mut self, key: &ImageKey, w: u16, h: u16) -> bool {
        let k = (key.clone(), w, h);
        let cached = if self.uses_graphics() { self.graphics.contains(&k) } else { self.halfblocks.contains(&k) };
        let wait = self.settling() && !cached;
        self.redraw_pending |= wait;
        wait
    }

    fn settling(&self) -> bool {
        self.resized_at.is_some_and(|t| t.elapsed() < RESIZE_SETTLE)
    }

    /// A put-off preview can be drawn now.
    pub fn redraw_due(&mut self) -> bool {
        let due = self.redraw_pending && !self.settling();
        if due {
            self.redraw_pending = false;
        }
        due
    }

    pub fn begin_frame(&mut self) {
        self.frame += 1;
        self.blits.clear();
        self.placements.clear();
    }

    /// After the overlays: drop covered graphics, place the Kitty ones, and
    /// trim the caches to what recent frames drew.
    pub fn finish_frame(&mut self, buf: &mut Buffer) {
        crate::preview::reveal_obscured_graphics(buf, &mut self.blits);
        crate::preview::place_graphics(buf, &self.placements);
        self.graphics.trim(self.frame);
        self.halfblocks.trim(self.frame);
    }

    /// The terminal was resized and ratatui erased the screen, so graphics
    /// written outside its diff must be sent again. A font zoom resizes too:
    /// if the tty reports a new cell size, encode at that size from now on.
    pub fn resized(&mut self, cell: Option<FontSize>) {
        self.last_blits.clear();
        self.resized_at = Some(Instant::now());
        let Some(cell) = cell.filter(|c| Some(*c) != self.cell_px) else {
            return;
        };
        self.cell_px = Some(cell);
        if let Some(p) = self.picker.as_mut().filter(|p| crate::preview::uses_graphics(p) && p.font_size() != cell) {
            *p = crate::preview::with_cell(p, cell);
            self.graphics.clear();
        }
    }

    /// The tty reports a new cell size on the same grid: a font zoom in a
    /// terminal that resizes its window to keep the columns and rows (WezTerm,
    /// iTerm2). ratatui erased nothing, so pictures drawn in pixels must be
    /// cleared and drawn again at the new size.
    pub fn font_zoomed(&self, cell: Option<FontSize>) -> bool {
        self.uses_graphics() && cell.is_some_and(|c| Some(c) != self.cell_px)
    }

    /// A session switch repaints everything. Decoded previews stay: they are
    /// keyed by absolute path and file version.
    pub fn clear(&mut self) {
        self.keys.clear();
        self.blits.clear();
        self.last_blits.clear();
        self.placements.clear();
    }
}

#[cfg(test)]
mod images_tests {
    use super::*;
    use ratatui_image::picker::ProtocolType;

    fn picker(proto: ProtocolType, cell: FontSize) -> Picker {
        let mut p = Picker::from_fontsize(cell);
        p.set_protocol_type(proto);
        p
    }

    fn save(path: &std::path::Path, w: u32, h: u32, rgb: [u8; 3]) {
        image::RgbImage::from_pixel(w, h, image::Rgb(rgb)).save(path).unwrap();
    }

    #[test]
    fn lru_drops_the_least_recently_drawn_but_not_this_frame() {
        let mut lru: Lru<&str, ()> = Lru::new(100);
        lru.insert("a", (), 60, 1);
        lru.insert("b", (), 60, 2);
        lru.insert("c", (), 60, 3);
        lru.get(&"a", 3);
        lru.trim(3);
        assert!(lru.contains(&"a") && lru.contains(&"c") && !lru.contains(&"b"));
        lru.insert("d", (), 500, 4);
        lru.get(&"c", 4);
        lru.trim(4);
        assert!(lru.contains(&"c") && lru.contains(&"d"), "what is on screen stays, even over budget");
        assert!(!lru.contains(&"a"));
        assert_eq!(lru.bytes, 560);
    }

    #[test]
    fn a_rewritten_file_is_a_new_key() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("plot.png");
        save(&path, 10, 10, [255, 0, 0]);
        let before = ImageKey::of(path.clone());
        save(&path, 20, 10, [0, 0, 255]);
        assert_ne!(ImageKey::of(path), before);
    }

    #[test]
    fn halfblocks_decode_once_and_outlive_a_session_switch() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("shot.png");
        save(&path, 16, 16, [255, 0, 0]);
        let key = ImageKey::of(path.clone());
        let mut images = Images::new(None);
        let first = images.halfblock(&key, 8, 4).to_vec();
        assert!(!first[0].spans[0].content.contains("無法預覽"));
        // Same length and mtime, unreadable content: only a cache hit still draws it.
        let modified = std::fs::metadata(&path).unwrap().modified().unwrap();
        let len = std::fs::metadata(&path).unwrap().len() as usize;
        std::fs::write(&path, vec![0u8; len]).unwrap();
        std::fs::File::options().write(true).open(&path).unwrap().set_modified(modified).unwrap();
        assert_eq!(ImageKey::of(path), key);
        images.clear();
        assert_eq!(images.halfblock(&key, 8, 4), first.as_slice());
    }

    #[test]
    fn a_resize_resends_blits_and_follows_a_font_zoom() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("shot.png");
        save(&path, 64, 64, [0, 200, 0]);
        let key = ImageKey::of(path);
        let mut images = Images::new(Some(picker(ProtocolType::Sixel, (10, 20))));
        images.cell_px = Some((10, 20));
        let blit = GraphicBlit { x: 1, y: 1, width: 4, height: 2, data: "\x1bPq$".into() };
        images.last_blits = vec![blit];
        images.resized(Some((10, 20)));
        assert!(images.last_blits.is_empty(), "ratatui erased the screen; send the pictures again");
        let before = images.cells(&key, 56, 12, true);
        assert!(images.graphic(&key, before.0, before.1).is_some());
        images.resized(Some((8, 16)));
        assert_eq!(images.picker.as_ref().unwrap().font_size(), (8, 16));
        assert_eq!(images.picker.as_ref().unwrap().protocol_type(), ProtocolType::Sixel);
        assert!(!images.graphics.contains(&(key.clone(), before.0, before.1)), "encoded at the old size");
        assert_ne!(images.cells(&key, 56, 12, true), before);

        let mut plain = Images::new(Some(picker(ProtocolType::Halfblocks, (10, 20))));
        plain.resized(Some((8, 16)));
        assert_eq!(plain.picker.as_ref().unwrap().font_size(), (10, 20), "half-blocks do not care");
    }

    #[test]
    fn the_viewer_waits_for_a_resize_to_settle() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("shot.png");
        save(&path, 64, 64, [0, 200, 0]);
        let key = ImageKey::of(path);
        let mut images = Images::new(Some(picker(ProtocolType::Kitty, (10, 20))));
        assert!(!images.defer(&key, 6, 3), "no resize, no wait");
        images.resized(None);
        assert!(images.defer(&key, 6, 3));
        assert!(!images.redraw_due(), "still dragging");
        images.resized_at = Some(Instant::now() - RESIZE_SETTLE * 2);
        assert!(images.redraw_due());
        assert!(!images.redraw_due(), "one repaint");
        assert!(!images.defer(&key, 6, 3));
    }

    #[test]
    fn a_stat_serves_frames_until_a_new_row_or_it_ages() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("plot.png");
        save(&path, 10, 10, [255, 0, 0]);
        let mut images = Images::new(None);
        images.sync_rows(3);
        let first = images.key(path.clone());
        save(&path, 20, 10, [0, 0, 255]);
        images.sync_rows(3);
        assert_eq!(images.key(path.clone()), first, "the frames that follow reuse the stat");
        // The chart was regenerated and read again: a new row.
        images.sync_rows(4);
        let second = images.key(path.clone());
        assert_ne!(second, first);
        assert_eq!(second, ImageKey::of(path.clone()));
        save(&path, 30, 10, [0, 255, 0]);
        images.keys.get_mut(&path).unwrap().0 -= STAT_TTL;
        assert_ne!(images.key(path), second, "an old stat is taken again");
    }

    #[test]
    fn a_font_zoom_on_the_same_grid_is_noticed() {
        let mut images = Images::new(Some(picker(ProtocolType::Iterm2, (10, 20))));
        images.cell_px = Some((10, 20));
        assert!(!images.font_zoomed(Some((10, 20))), "a plain resize event");
        assert!(!images.font_zoomed(None), "no pixel size reported");
        assert!(images.font_zoomed(Some((8, 16))));
        images.resized(Some((8, 16)));
        assert_eq!(images.picker.as_ref().unwrap().font_size(), (8, 16));
        assert!(!images.font_zoomed(Some((8, 16))), "once");
        let mut plain = Images::new(Some(picker(ProtocolType::Halfblocks, (10, 20))));
        plain.cell_px = Some((10, 20));
        assert!(!plain.font_zoomed(Some((8, 16))), "half-blocks need no repaint");
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

/// What one paste or drop did to the attachments, for a single status line.
#[derive(Debug, Default)]
pub(crate) struct Attached {
    pub added: usize,
    /// Left out because the composer already held `MAX_USER_IMAGES`.
    pub over_cap: usize,
    /// File names in formats the decoders cannot read.
    pub unsupported: Vec<String>,
    pub errors: Vec<String>,
}

impl Attached {
    /// `None` when there was nothing to attach.
    pub fn message(&self) -> Option<String> {
        let mut parts = Vec::new();
        if self.added > 0 {
            parts.push(format!("已附上 {} 張圖片", self.added));
        }
        if self.over_cap > 0 {
            parts.push(format!("最多 {} 張圖片，{} 張未附上", crate::vision::MAX_USER_IMAGES, self.over_cap));
        }
        if !self.unsupported.is_empty() {
            parts.push(format!("不支援的圖片格式：{}（可用 PNG、JPEG、GIF、WebP、BMP）", self.unsupported.join("、")));
        }
        if let Some(e) = self.errors.first() {
            parts.push(format!("無法加入圖片: {e}"));
        }
        (!parts.is_empty()).then(|| parts.join(" · "))
    }
}

/// What a paste takes from the clipboard.
#[derive(Debug, PartialEq, Eq)]
enum ClipPick {
    Files,
    Text,
    /// The bitmap, if there is one: it is read only when picked.
    Bitmap,
}

/// Copied image files come first: a file copied in Finder also puts its icon
/// on the pasteboard as a picture. Then non-blank `text`, which only Ctrl+V
/// passes (the 貼上圖片 button is for images): spreadsheets and office apps
/// copy a picture of the cells next to the text.
fn clip_pick(has_files: bool, text: Option<&str>) -> ClipPick {
    if has_files {
        ClipPick::Files
    } else if text.is_some_and(|t| !t.trim().is_empty()) {
        ClipPick::Text
    } else {
        ClipPick::Bitmap
    }
}

#[cfg(test)]
mod clip_tests {
    use super::*;

    #[test]
    fn copied_files_beat_text_and_text_beats_a_bitmap() {
        assert_eq!(clip_pick(true, Some("/Users/me/design.png")), ClipPick::Files, "not the Finder icon");
        assert_eq!(clip_pick(true, None), ClipPick::Files);
        assert_eq!(clip_pick(false, Some("1\t2\n3\t4")), ClipPick::Text, "not a picture of the cells");
        assert_eq!(clip_pick(false, Some(" \n")), ClipPick::Bitmap, "blank text falls through");
        assert_eq!(clip_pick(false, None), ClipPick::Bitmap, "the button reads no text");
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
            server_tools: crate::kit::search_tools(
                opts.web_search,
                settings.conn.route_for(&opts.model).is_openai(),
                settings.xai_ready,
            ),
            dispatcher: opts.dispatcher,
            child_model: opts.child_model.clone(),
            task_model: String::new(),
            search: opts.web_search,
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
            .map(|v| rows::decode_rows(&v))
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
            let old = self.workspace();
            let Self { store, sessions, current, .. } = self;
            let s = sessions.get_mut(current.as_str()).expect("current session");
            s.meta.workspace = workspace.clone();
            s.meta.updated_at = chrono::Utc::now();
            if let Some(st) = store.as_ref() {
                let _ = st.save_meta(&s.meta);
            }
            let lost = self.carry_pending(&old, &workspace);
            let mut msg = format!("工作目錄  {}", folderpick::display_path(&workspace));
            if lost > 0 {
                msg.push_str(&format!(" · {lost} 張附件無法搬移，已移除"));
            }
            self.flash(msg);
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

    /// Pending attachments are inbox files relative to the workspace. When a
    /// blank chat moves to another folder they move along, or they would
    /// point into the new folder and miss. Returns how many could not move.
    fn carry_pending(&mut self, from: &std::path::Path, to: &std::path::Path) -> usize {
        if from == to || self.cur().pending.is_empty() {
            return 0;
        }
        let pending = std::mem::take(&mut self.cur_mut().pending);
        // A .groka that is a symlink would carry them out of the folder:
        // they stay behind in the old inbox.
        if crate::vision::groka_dir(to, "inbox").is_err() {
            return pending.len();
        }
        let total = pending.len();
        let kept: Vec<String> = pending
            .into_iter()
            .filter(|rel| move_file(&from.join(rel), &to.join(rel)).is_ok())
            .collect();
        let lost = total - kept.len();
        self.cur_mut().pending = kept;
        lost
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
        // While the user proxy drives a task the run seldom waits for input:
        // the user's own words go in as an interjection, so they come first.
        let driving = self.cur().task.snapshot().phase.is_live();
        let mode = if force_insert || driving { SendMode::Insert } else { self.ui.send_mode };
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

    /// Free attachment slots in the composer.
    fn room(&self) -> usize {
        crate::vision::MAX_USER_IMAGES.saturating_sub(self.cur().pending.len())
    }

    fn attach(&mut self, rel: String, got: &mut Attached) {
        self.cur_mut().pending.push(rel);
        got.added += 1;
    }

    /// An image the web page already saved into the inbox. `false` = no slot left.
    pub fn attach_pending(&mut self, rel: String) -> bool {
        if self.cur().pending.contains(&rel) {
            return true;
        }
        if self.room() == 0 {
            return false;
        }
        self.cur_mut().pending.push(rel);
        true
    }

    /// Copy image files into the inbox. Slots are checked before decoding,
    /// so nothing past the cap is read or saved.
    fn ingest_paths(&mut self, paths: &[PathBuf], got: &mut Attached) {
        let ws = self.workspace();
        for p in paths {
            if !crate::vision::is_image_ext(p) {
                let name = p.file_name().unwrap_or(p.as_os_str());
                got.unsupported.push(name.to_string_lossy().into_owned());
            } else if self.room() == 0 {
                got.over_cap += 1;
            } else {
                match crate::vision::ingest_image_file(&ws, p) {
                    Ok(rel) => self.attach(rel, got),
                    Err(e) => got.errors.push(e.to_string()),
                }
            }
        }
    }

    fn ingest_clipboard_bitmap(&mut self, got: &mut Attached) {
        let Some(img) = crate::clipimg::read_image() else {
            return;
        };
        if self.room() == 0 {
            got.over_cap += 1;
            return;
        }
        match crate::vision::save_user_image(&self.workspace(), &img) {
            Ok(rel) => self.attach(rel, got),
            Err(e) => got.errors.push(e.to_string()),
        }
    }

    /// Flash what happened. `false` = nothing did.
    fn report(&mut self, got: &Attached) -> bool {
        let Some(msg) = got.message() else {
            return false;
        };
        self.flash(msg);
        true
    }

    /// Paste what `clip_pick` chooses; `text` is the clipboard text when the
    /// paste may take text. `false` = nothing to paste.
    fn paste_clip(&mut self, text: Option<String>) -> bool {
        let files = crate::clipimg::read_image_files();
        let mut got = Attached::default();
        match clip_pick(!files.is_empty(), text.as_deref()) {
            ClipPick::Files => self.ingest_paths(&files, &mut got),
            ClipPick::Text => {
                self.paste_text(text.as_deref().unwrap_or_default());
                return true;
            }
            ClipPick::Bitmap => self.ingest_clipboard_bitmap(&mut got),
        }
        self.report(&got)
    }

    /// "貼上圖片" button: copied image files, else the clipboard bitmap.
    pub fn paste_image(&mut self) {
        if !self.paste_clip(None) {
            self.flash("剪貼簿沒有圖片 — 先複製截圖或圖片檔，再點「貼上圖片」");
        }
    }

    /// Text pasted into the composer. Dropped image files become attachments
    /// and their paths are not inserted, unless every one is in a format the
    /// decoders cannot read: the notice says so, and a path to a favicon or a
    /// TIFF may be meant as text.
    pub fn paste_text(&mut self, s: &str) {
        let dropped = crate::vision::parse_image_drop(s);
        if dropped.is_empty() {
            self.cur_mut().draft.insert_str(s);
            return;
        }
        let mut got = Attached::default();
        self.ingest_paths(&dropped, &mut got);
        self.report(&got);
        if got.added + got.over_cap == 0 && got.errors.is_empty() {
            self.cur_mut().draft.insert_str(s);
        }
    }

    /// Bracketed paste. A blank payload (Ctrl+V of a bitmap) reads the clipboard.
    pub fn paste_from_terminal(&mut self, s: &str) {
        if !s.trim().is_empty() {
            self.paste_text(s);
        } else if !self.paste_clipboard() {
            self.cur_mut().draft.insert_str(s);
        }
    }

    /// Ctrl+V: copied image files, else text, else a bitmap. `false` =
    /// nothing to paste.
    pub fn paste_clipboard(&mut self) -> bool {
        self.paste_clip(clipboard_get())
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

    /// The image the viewer shows, when no other overlay is drawn over it.
    pub fn viewed_image(&self) -> Option<String> {
        let covered = self.ui.workspace_pick.is_some() || self.cur().ask.is_some() || self.ui.task_ui.is_some();
        self.ui.image_view.clone().filter(|_| !covered)
    }

    /// Copy an image to the clipboard, or its path where images do not fit.
    pub fn copy_image(&mut self, rel: &str) {
        let abs = self.workspace().join(rel);
        let msg = if clipboard_set_image(&abs) {
            "已複製圖片"
        } else if clipboard_set(rel) {
            "已複製路徑"
        } else {
            "無法複製圖片"
        };
        self.flash(msg);
    }

    // —— Task mode ——

    /// The form when there is no task, the status dialog otherwise.
    pub fn open_task(&mut self) {
        let phase = self.cur().task.snapshot().phase;
        self.ui.task_ui = Some(if phase == TaskPhase::Inactive {
            TaskUi::Form(TaskForm::default())
        } else {
            TaskUi::Status(TaskStatus::default())
        });
    }

    pub fn close_task(&mut self) {
        self.ui.task_ui = None;
    }

    /// Submit the open form: start a task, or edit the current one.
    pub fn submit_task_form(&mut self) {
        let Some(TaskUi::Form(form)) = &self.ui.task_ui else {
            return;
        };
        let (goal, criteria) = (form.goal.text.clone(), form.criteria.text.clone());
        if form.editing {
            self.edit_task(&goal, &criteria);
        } else {
            self.start_task(&goal, &criteria);
        }
    }

    pub fn start_task(&mut self, goal: &str, criteria: &str) {
        let goal = goal.trim().to_string();
        if goal.is_empty() {
            self.flash("請填寫任務目標");
            return;
        }
        let criteria = criteria.trim().to_string();
        let s = self.cur_mut();
        s.task.start_task(goal.clone(), criteria.clone());
        let mut line = format!("已啟動任務模式：{goal}");
        if !criteria.is_empty() {
            line.push_str(&format!("\n驗收條件：{criteria}"));
        }
        s.chat.push(Row::meta(line));
        s.agents.log("", EventKind::Message, format!("任務目標：{goal}"));
        s.dirty = true;
        self.ui.task_ui = Some(TaskUi::Status(TaskStatus::default()));
        if self.cur().inbox.is_none() {
            self.start_or_send(UserTurn::from(task::KICK), false);
        }
    }

    /// The user changed the goal or criteria. The proxy keeps its memory,
    /// progress notes and rounds, so an edit after a hand-back (often the
    /// decision it asked for) picks up where it stopped.
    pub fn edit_task(&mut self, goal: &str, criteria: &str) {
        let goal = goal.trim().to_string();
        if goal.is_empty() {
            self.flash("請填寫任務目標");
            return;
        }
        let snap = self.cur().task.snapshot();
        if snap.phase == TaskPhase::Inactive {
            // Ended elsewhere while the form was open: nothing left to edit.
            self.start_task(&goal, criteria);
            return;
        }
        if matches!(snap.phase, TaskPhase::HandedBack | TaskPhase::Failed) {
            // TaskHub::edit_task only reopens Done; bring these back the same
            // way, so intake and planning do not run again.
            self.cur().task.replace(TaskState {
                phase: TaskPhase::Active,
                review_note: String::new(),
                fail_reason: String::new(),
                claim: None,
                skip_steer: false,
                ..snap
            });
        }
        let s = self.cur_mut();
        s.task.edit_task(goal.clone(), criteria.trim().to_string());
        s.chat.push(Row::meta(format!("已修改任務：{goal}")));
        s.agents.log("", EventKind::Message, format!("任務修改：{goal}"));
        s.dirty = true;
        self.ui.task_ui = Some(TaskUi::Status(TaskStatus::default()));
        self.ensure_task_run();
    }

    /// Run one task action. Actions that do not apply to the task as it
    /// stands (a stale web button, a hotkey) do nothing.
    pub fn task_action(&mut self, act: TaskAction) {
        let snap = self.cur().task.snapshot();
        if act == TaskAction::Review && snap.paused && snap.phase.is_live() {
            self.flash("用戶代理已暫停，請先按繼續");
            return;
        }
        if !task_actions(&snap).contains(&act) {
            return;
        }
        let armed = self.task_end_armed();
        self.ui.task_end_armed = None;
        match act {
            TaskAction::Pause => {
                let s = self.cur_mut();
                s.task.set_paused(true);
                s.chat.push(Row::meta("已暫停用戶代理：主代理不再被推進，直到你按繼續"));
                s.dirty = true;
            }
            TaskAction::Resume => {
                let s = self.cur_mut();
                // Resume also lifts an Esc stop; the kernel only reviews on a
                // resume once that is clear.
                s.task.set_skip_steer(false);
                s.task.set_paused(false);
                s.chat.push(Row::meta("用戶代理繼續推進任務"));
                s.dirty = true;
                self.ensure_task_run();
            }
            TaskAction::Review => {
                let t = &self.cur().task;
                t.set_skip_steer(false);
                t.request_review();
                self.flash("已請用戶代理立即審查");
                self.ensure_task_run();
            }
            TaskAction::Accept => {
                let s = self.cur_mut();
                s.task.accept_done();
                s.chat.push(Row::meta("你已判定任務完成，用戶代理停止推進"));
                s.agents.log("", EventKind::Message, "任務由使用者判定完成");
                s.dirty = true;
            }
            TaskAction::Edit => self.ui.task_ui = Some(TaskUi::Form(TaskForm::editing(&snap))),
            // Ending wipes the goal, the progress notes and the proxy's
            // memory: one stray key or click must not do that.
            TaskAction::End if armed => self.end_task(),
            TaskAction::End => {
                self.ui.task_end_armed = Some((Instant::now(), self.current.clone()));
                self.flash("再按一次「結束任務」確認：目標、進度筆記和用戶代理的記憶都會清除");
            }
            TaskAction::Close => self.close_task(),
        }
    }

    /// 結束任務 was pressed once and waits for the second press.
    pub fn task_end_armed(&self) -> bool {
        self.ui.task_end_armed.as_ref().is_some_and(|(t, id)| t.elapsed() < TASK_END_CONFIRM && *id == self.current)
    }

    /// A live task needs a run for the proxy to act in: a restored session
    /// or a finished run has none until something starts one.
    fn ensure_task_run(&mut self) {
        let snap = self.cur().task.snapshot();
        if snap.phase.is_live() && !snap.paused && self.cur().inbox.is_none() {
            // A fresh run's first turn only plans a new task. Past planning,
            // ask the proxy to look, so the run starts with its review.
            if snap.phase != TaskPhase::NeedPlan {
                self.cur().task.request_review();
            }
            self.start_or_send(UserTurn::from(task::KICK), false);
        }
    }

    pub fn end_task(&mut self) {
        if self.cur().task.snapshot().phase != TaskPhase::Inactive {
            let s = self.cur_mut();
            s.task.end();
            s.agents.log("", EventKind::Message, "任務模式由使用者結束");
            s.chat.push(Row::meta("已結束任務模式"));
            s.dirty = true;
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

    /// Alt+↑/↓: open the previous / next agent in tree order (the main chat
    /// sits before the first agent).
    pub fn step_agent(&mut self, delta: i32) {
        let paths: Vec<String> = self.cur().agents.nodes.iter().map(|a| a.path.clone()).collect();
        if paths.is_empty() {
            return;
        }
        let cur = match self.active_tab() {
            Tab::Agent(p) => paths.iter().position(|x| *x == p).map(|i| i as i32 + 1).unwrap_or(0),
            _ => 0,
        };
        let n = paths.len() as i32 + 1;
        let next = (cur + delta).rem_euclid(n);
        if next == 0 {
            self.show_chat(None);
        } else {
            self.show_chat(Some(paths[next as usize - 1].clone()));
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
            .map(|v| rows::decode_rows(&v))
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

/// Rename, or copy and delete across file systems.
fn move_file(src: &std::path::Path, dst: &std::path::Path) -> std::io::Result<()> {
    if let Some(dir) = dst.parent() {
        std::fs::create_dir_all(dir)?;
    }
    if std::fs::rename(src, dst).is_err() {
        std::fs::copy(src, dst)?;
        let _ = std::fs::remove_file(src);
    }
    Ok(())
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
