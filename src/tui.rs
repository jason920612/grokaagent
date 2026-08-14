//! Cursor-like chat TUI: transcript, composer, settings overlay, queue/insert.

use std::collections::{HashMap, VecDeque};
use std::io::{self, stdout};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crossterm::event::{
    DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture, Event,
    EventStream, KeyCode, KeyEventKind, KeyModifiers, MouseButton, MouseEventKind,
};
use crossterm::execute;
use crossterm::terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen};
use futures::StreamExt;
use ratatui::layout::{Constraint, Direction, Layout, Position, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Clear, Paragraph};
use ratatui::{backend::CrosstermBackend, Frame, Terminal};
use ratatui_image::picker::Picker;
use ratatui_image::protocol::Protocol;
use ratatui_image::Image;
use tokio::sync::mpsc;

use crate::agent::{SessionKnobs, UserTurn};
use crate::ask::{self, AskUserHub, Question};
use crate::auth;
use crate::error::{Error, Result};
use crate::events::{AgentEvent, ChannelSink, EventMeta, EventSink, FanoutSink, JsonlSink};
use crate::catalog::{clamp_effort_for_model, cycle_effort, EffortOpt, ModelCatalog};
use crate::folderpick::{self, FolderView};
use crate::kit;
use crate::md;
use crate::provider::{ReasoningEffort, XaiOauthProvider};
use crate::session::{self, SessionMeta, SessionStore};
use serde::{Deserialize, Serialize};
use serde_json::Value;

const BG: Color = Color::Rgb(24, 24, 24);
const PANEL: Color = Color::Rgb(32, 32, 32);
const COMPOSER: Color = Color::Rgb(40, 40, 40);
const TEXT: Color = Color::Rgb(212, 212, 212);
const DIM: Color = Color::Rgb(110, 110, 110);
const ACCENT: Color = Color::Rgb(88, 166, 255);
const USER: Color = Color::Rgb(156, 196, 255);
const AGENT: Color = Color::Rgb(163, 209, 163);
const BORDER: Color = Color::Rgb(62, 62, 62);
const WARN: Color = Color::Rgb(220, 120, 90);
const DIFF_ADD: Color = Color::Rgb(63, 185, 80);
const DIFF_DEL: Color = Color::Rgb(248, 81, 73);
const DIFF_HUNK: Color = Color::Rgb(88, 166, 255);
const TOOL: Color = Color::Rgb(210, 180, 80);
const THINK: Color = Color::Rgb(168, 148, 210);
const SIDEBAR_MIN_TERM: u16 = 80;
const SIDEBAR_W: u16 = 28;
const RAIL_MIN_TERM: u16 = 100;
const RAIL_W: u16 = 24;
const DROP_VISIBLE: usize = 8;

#[derive(Clone, Serialize, Deserialize)]
struct FileChange {
    path: String,
    kind: String,
    diff: String,
}

#[derive(Clone, Serialize, Deserialize)]
struct ToolCall {
    name: String,
    args: Value,
    output: String,
    files: Vec<FileChange>,
    done: bool,
    phase: String,
}

#[derive(Clone, Serialize, Deserialize)]
struct ToolGroup {
    calls: Vec<ToolCall>,
    expanded: bool,
}

#[derive(Clone, Serialize, Deserialize, Default)]
struct Think {
    text: String,
    expanded: bool,
    done: bool,
    #[serde(default)]
    elapsed_ms: u64,
    #[serde(skip)]
    started: Option<Instant>,
}

#[derive(Clone)]
struct UserMsg {
    text: String,
    images: Vec<String>,
}

impl From<&str> for UserMsg {
    fn from(s: &str) -> Self {
        Self {
            text: s.to_string(),
            images: Vec::new(),
        }
    }
}

impl From<String> for UserMsg {
    fn from(text: String) -> Self {
        Self {
            text,
            images: Vec::new(),
        }
    }
}

impl serde::Serialize for UserMsg {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error> {
        if self.images.is_empty() {
            serializer.serialize_str(&self.text)
        } else {
            use serde::ser::SerializeStruct;
            let mut st = serializer.serialize_struct("UserMsg", 2)?;
            st.serialize_field("text", &self.text)?;
            st.serialize_field("images", &self.images)?;
            st.end()
        }
    }
}

impl<'de> serde::Deserialize<'de> for UserMsg {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> std::result::Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum De {
            Text(String),
            Full {
                text: String,
                #[serde(default)]
                images: Vec<String>,
            },
        }
        match De::deserialize(deserializer)? {
            De::Text(text) => Ok(Self {
                text,
                images: Vec::new(),
            }),
            De::Full { text, images } => Ok(Self { text, images }),
        }
    }
}

#[derive(Clone)]
struct AgentMsg {
    text: String,
    work_ms: u64,
}

impl AgentMsg {
    fn new(text: String) -> Self {
        Self { text, work_ms: 0 }
    }
}

impl serde::Serialize for AgentMsg {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error> {
        if self.work_ms == 0 {
            serializer.serialize_str(&self.text)
        } else {
            use serde::ser::SerializeStruct;
            let mut st = serializer.serialize_struct("AgentMsg", 2)?;
            st.serialize_field("text", &self.text)?;
            st.serialize_field("work_ms", &self.work_ms)?;
            st.end()
        }
    }
}

impl<'de> serde::Deserialize<'de> for AgentMsg {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> std::result::Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum De {
            Text(String),
            Full {
                text: String,
                #[serde(default)]
                work_ms: u64,
            },
        }
        match De::deserialize(deserializer)? {
            De::Text(text) => Ok(Self { text, work_ms: 0 }),
            De::Full { text, work_ms } => Ok(Self { text, work_ms }),
        }
    }
}

#[derive(Clone, Serialize, Deserialize)]
enum Row {
    User(UserMsg),
    Agent(AgentMsg),
    Tools(ToolGroup),
    Think(Think),
    Meta(String),
    Err(String),
    Picture { path: String, label: String },
}

#[derive(Clone)]
pub struct TuiOptions {
    pub model: String,
    pub events: PathBuf,
    pub workspace: PathBuf,
    pub max_turns: u32,
    pub web_search: bool,
    pub reasoning_effort: ReasoningEffort,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum SendMode {
    Queue,
    Insert,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Submit {
    Start,
    Queue,
    Insert,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Focus {
    Chat,
    Settings,
    Rename,
    Inspector,
    Ask,
    Workspace,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum SettingField {
    Account,
    Model,
    Effort,
    Search,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DropKind {
    Model,
    Effort,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum CatalogStatus {
    Idle,
    Loading,
    Ready,
    Failed(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum LoginUi {
    Idle,
    Starting,
    Waiting { url: String, user_code: String },
    Failed(String),
}

#[derive(Debug)]
enum LoginEvent {
    Waiting {
        gen: u64,
        url: String,
        user_code: String,
    },
    Success {
        gen: u64,
    },
    Failed {
        gen: u64,
        message: String,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Hit {
    Gear,
    ModelChip,
    QueueChip,
    InsertChip,
    Chat,
    Composer,
    Title,
    Close,
    Min,
    Max,
    SettingModel,
    SettingEffort,
    CatalogPick(u16),
    Search,
    AccountBtn,
    LoginCode,
    Dock,
    ToolGroup(usize),
    ToolItem(usize, usize),
    Think(usize),
    ToolPanel,
    ToolPanelClose,
    DismissTool,
    NewChat,
    Session(u16),
    RenameSession(u16),
    DeleteSession(u16),
    QueueItem(u16),
    CancelQueueEdit,
    RailChild(u16),
    RailMon(u16),
    RailBg(u16),
    Inspector,
    InspectorClose,
    ChatRow(u16),
    ChatImage(u16),
    PasteImage,
    PendingClose(u16),
    AskOption(u16),
    AskConfirm,
    AskCancel,
    AskFill,
    AskPanel,
    WsPanel,
    WsPath,
    WsEntry(u16),
    WsConfirm,
    WsCreate,
    WsCancel,
}

fn submit_kind(has_session: bool, running: bool, mode: SendMode) -> Submit {
    if !has_session {
        Submit::Start
    } else if running && mode == SendMode::Queue {
        Submit::Queue
    } else {
        Submit::Insert
    }
}

fn ch_width(c: char) -> u16 {
    Line::from(c.to_string()).width() as u16
}

fn display_cols(s: &str) -> u16 {
    Line::from(s).width() as u16
}

#[cfg(test)]
fn caret_in(inner: Rect, text: &str, caret: usize) -> Position {
    let prefix: String = text.chars().take(caret).collect();
    let w = Line::from(prefix.as_str()).width() as u16;
    let max = inner.width.saturating_sub(1);
    Position::new(inner.x.saturating_add(w.min(max)), inner.y)
}

#[derive(Clone, Default, Serialize, Deserialize)]
struct Edit {
    text: String,
    /// Character index (not bytes).
    caret: usize,
    /// Selection anchor; `None` or equal to caret means no selection.
    anchor: Option<usize>,
}

impl Edit {
    fn at_end(text: String) -> Self {
        let caret = text.chars().count();
        Self {
            text,
            caret,
            anchor: None,
        }
    }

    fn len(&self) -> usize {
        self.text.chars().count()
    }

    fn is_empty(&self) -> bool {
        self.text.is_empty()
    }

    fn clamp(&mut self) {
        let n = self.len();
        self.caret = self.caret.min(n);
        if let Some(a) = self.anchor {
            self.anchor = Some(a.min(n));
            if self.anchor == Some(self.caret) {
                self.anchor = None;
            }
        }
    }

    fn has_sel(&self) -> bool {
        self.anchor.map(|a| a != self.caret).unwrap_or(false)
    }

    fn sel_range(&self) -> Option<(usize, usize)> {
        let a = self.anchor?;
        if a == self.caret {
            None
        } else {
            Some((a.min(self.caret), a.max(self.caret)))
        }
    }

    fn selected_text(&self) -> Option<String> {
        let (lo, hi) = self.sel_range()?;
        Some(self.text.chars().skip(lo).take(hi - lo).collect())
    }

    fn clear_sel(&mut self) {
        self.anchor = None;
    }

    fn begin_select(&mut self, select: bool) {
        if select {
            if self.anchor.is_none() {
                self.anchor = Some(self.caret);
            }
        } else {
            self.clear_sel();
        }
    }

    fn move_left(&mut self, select: bool) {
        self.clamp();
        if !select {
            if let Some((lo, _)) = self.sel_range() {
                self.caret = lo;
                self.clear_sel();
                return;
            }
        }
        self.begin_select(select);
        if self.caret > 0 {
            self.caret -= 1;
        }
        self.clamp();
    }

    fn move_right(&mut self, select: bool) {
        self.clamp();
        if !select {
            if let Some((_, hi)) = self.sel_range() {
                self.caret = hi;
                self.clear_sel();
                return;
            }
        }
        self.begin_select(select);
        if self.caret < self.len() {
            self.caret += 1;
        }
        self.clamp();
    }

    fn home(&mut self, select: bool) {
        if !select {
            self.clear_sel();
        } else {
            self.begin_select(true);
        }
        self.caret = 0;
        self.clamp();
    }

    fn end(&mut self, select: bool) {
        if !select {
            self.clear_sel();
        } else {
            self.begin_select(true);
        }
        self.caret = self.len();
        self.clamp();
    }

    fn select_all(&mut self) {
        self.anchor = Some(0);
        self.caret = self.len();
        if self.caret == 0 {
            self.anchor = None;
        }
    }

    fn click(&mut self, idx: usize, select: bool) {
        self.begin_select(select);
        self.caret = idx;
        self.clamp();
        if !select {
            self.clear_sel();
        }
    }

    fn delete_sel(&mut self) -> bool {
        let Some((lo, hi)) = self.sel_range() else {
            return false;
        };
        let byte_lo = char_to_byte(&self.text, lo);
        let byte_hi = char_to_byte(&self.text, hi);
        self.text.replace_range(byte_lo..byte_hi, "");
        self.caret = lo;
        self.clear_sel();
        true
    }

    fn insert_char(&mut self, c: char) {
        self.delete_sel();
        let i = char_to_byte(&self.text, self.caret);
        self.text.insert(i, c);
        self.caret += 1;
    }

    fn insert_str(&mut self, s: &str) {
        self.delete_sel();
        let i = char_to_byte(&self.text, self.caret);
        self.text.insert_str(i, s);
        self.caret += s.chars().count();
    }

    fn backspace(&mut self) {
        if self.delete_sel() {
            return;
        }
        if self.caret == 0 {
            return;
        }
        self.caret -= 1;
        let i = char_to_byte(&self.text, self.caret);
        let j = char_to_byte(&self.text, self.caret + 1);
        self.text.replace_range(i..j, "");
    }

    fn delete_forward(&mut self) {
        if self.delete_sel() {
            return;
        }
        if self.caret >= self.len() {
            return;
        }
        let i = char_to_byte(&self.text, self.caret);
        let j = char_to_byte(&self.text, self.caret + 1);
        self.text.replace_range(i..j, "");
    }

    fn clear(&mut self) {
        self.text.clear();
        self.caret = 0;
        self.anchor = None;
    }

    fn move_visual(&mut self, width: u16, delta_row: i16, select: bool) {
        self.clamp();
        if !select {
            if let Some((lo, hi)) = self.sel_range() {
                self.caret = if delta_row < 0 { lo } else { hi };
                self.clear_sel();
                return;
            }
        }
        self.begin_select(select);
        let width = width.max(1);
        let ranges = wrap_lines(&self.text, width);
        let (row, col) = caret_row_col(&self.text, &ranges, self.caret);
        let dest = if delta_row < 0 {
            row.saturating_sub(1)
        } else {
            row.saturating_add(1).min(ranges.len().saturating_sub(1) as u16)
        };
        self.caret = index_at_line(&self.text, &ranges, dest, col);
        self.clamp();
    }
}

fn char_to_byte(s: &str, idx: usize) -> usize {
    s.char_indices()
        .nth(idx)
        .map(|(i, _)| i)
        .unwrap_or(s.len())
}

fn wrap_lines(s: &str, width: u16) -> Vec<(usize, usize)> {
    let width = width.max(1);
    let n = s.chars().count();
    if n == 0 {
        return vec![(0, 0)];
    }
    let mut lines = Vec::new();
    let mut start = 0;
    let mut col = 0u16;
    for (i, c) in s.chars().enumerate() {
        let w = ch_width(c).max(1);
        if col > 0 && col.saturating_add(w) > width {
            lines.push((start, i));
            start = i;
            col = 0;
        }
        col = col.saturating_add(w);
    }
    lines.push((start, n));
    lines
}

fn caret_row_col(s: &str, ranges: &[(usize, usize)], caret: usize) -> (u16, u16) {
    let n = s.chars().count();
    for (row, &(a, b)) in ranges.iter().enumerate() {
        if caret < b || (caret == b && (b == n || row + 1 == ranges.len())) {
            let prefix: String = s.chars().skip(a).take(caret.saturating_sub(a)).collect();
            return (row as u16, Line::from(prefix.as_str()).width() as u16);
        }
    }
    let last = ranges.len().saturating_sub(1) as u16;
    (last, 0)
}

fn index_at_width(chars: &[char], target: u16) -> usize {
    let mut col = 0u16;
    for (i, &c) in chars.iter().enumerate() {
        let w = ch_width(c).max(1);
        if target < col.saturating_add(w) {
            if target.saturating_sub(col) < (w + 1) / 2 {
                return i;
            }
            return i + 1;
        }
        col = col.saturating_add(w);
    }
    chars.len()
}

fn index_at_line(s: &str, ranges: &[(usize, usize)], row: u16, col: u16) -> usize {
    let chars: Vec<char> = s.chars().collect();
    let Some(&(a, b)) = ranges.get(row as usize) else {
        return chars.len();
    };
    a + index_at_width(&chars[a..b], col)
}

fn click_to_index(s: &str, inner: Rect, vscroll: u16, col: u16, row: u16) -> usize {
    let width = inner.width.max(1);
    let ranges = wrap_lines(s, width);
    let rel_row = row.saturating_sub(inner.y).saturating_add(vscroll) as usize;
    let rel_col = col.saturating_sub(inner.x);
    if rel_row >= ranges.len() {
        return s.chars().count();
    }
    index_at_line(s, &ranges, rel_row as u16, rel_col)
}

fn clipboard_set(s: &str) -> bool {
    arboard::Clipboard::new()
        .ok()
        .and_then(|mut c| c.set_text(s.to_string()).ok())
        .is_some()
}

fn clipboard_get() -> Option<String> {
    arboard::Clipboard::new().ok()?.get_text().ok()
}

fn is_paste_key(code: KeyCode, mods: KeyModifiers) -> bool {
    match code {
        KeyCode::Char('v' | 'V') if mods.contains(KeyModifiers::CONTROL) => true,
        KeyCode::Char('\u{16}') => true,
        KeyCode::Insert if mods.contains(KeyModifiers::SHIFT) => true,
        _ => false,
    }
}

fn clipboard_set_image(path: &std::path::Path) -> bool {
    let Ok(img) = image::open(path) else {
        return false;
    };
    let rgba = img.to_rgba8();
    arboard::Clipboard::new()
        .ok()
        .and_then(|mut c| {
            c.set_image(arboard::ImageData {
                width: rgba.width() as usize,
                height: rgba.height() as usize,
                bytes: std::borrow::Cow::Owned(rgba.into_raw()),
            })
            .ok()
        })
        .is_some()
}

fn picture_from_tool(name: &str, output: &str) -> Option<(String, String)> {
    let v: Value = serde_json::from_str(output).ok()?;
    if v.get("attach_image").and_then(Value::as_bool) != Some(true) {
        return None;
    }
    let path = v.get("path").and_then(Value::as_str)?.to_string();
    if path.is_empty() {
        return None;
    }
    Some((path.clone(), format!("模型在看  {name}  {path}")))
}

fn row_copy_text(row: &Row) -> String {
    match row {
        Row::User(u) => u.text.clone(),
        Row::Agent(a) => a.text.clone(),
        Row::Meta(s) | Row::Err(s) => s.clone(),
        Row::Think(t) => t.text.clone(),
        Row::Picture { path, label } => format!("{label}\n{path}"),
        Row::Tools(g) => g
            .calls
            .iter()
            .map(|c| {
                if c.output.is_empty() {
                    c.name.clone()
                } else {
                    format!("{}\n{}", c.name, c.output)
                }
            })
            .collect::<Vec<_>>()
            .join("\n"),
    }
}

fn hit_at(hits: &[(Rect, Hit)], col: u16, row: u16) -> Option<Hit> {
    let p = Position::new(col, row);
    hits.iter()
        .rev()
        .find(|(r, _)| r.contains(p))
        .map(|(_, h)| *h)
}

#[derive(Clone, Copy)]
struct Win {
    x: u16,
    y: u16,
    w: u16,
    h: u16,
    maximized: bool,
    minimized: bool,
}

#[derive(Clone)]
struct SideMsg {
    from: String,
    to: String,
    text: String,
}

#[derive(Clone)]
struct SideChild {
    name: String,
    prompt: String,
    card_url: String,
    status: String,
    alive: bool,
    activity: String,
    log: Vec<String>,
    messages: Vec<SideMsg>,
}

impl SideChild {
    fn upsert_status(&mut self, status: String, alive: bool) {
        self.status = status;
        self.alive = alive;
    }

    fn push_log(&mut self, line: String) {
        if line.is_empty() {
            return;
        }
        if self.log.len() > 400 {
            self.log.drain(0..150);
        }
        self.log.push(line);
    }

    fn append_log_prefix(&mut self, prefix: &str, text: &str) {
        if text.is_empty() {
            return;
        }
        if let Some(last) = self.log.last_mut() {
            if last.starts_with(prefix) {
                last.push_str(text);
                return;
            }
        }
        self.push_log(format!("{prefix}{text}"));
    }

    fn apply_work(&mut self, ev: &AgentEvent) {
        match ev {
            AgentEvent::RunStarted { model, .. } => {
                self.alive = true;
                self.status = "工作中".into();
                self.activity = format!("連線 {model}");
            }
            AgentEvent::TurnStarted { turn, .. } => {
                self.alive = true;
                self.status = format!("第 {turn} 輪");
                self.activity = "思考中".into();
            }
            AgentEvent::ReasoningDelta { text, .. } => {
                self.append_log_prefix("思考 ", text);
                self.activity = "思考中".into();
            }
            AgentEvent::ModelDelta { text, .. } => {
                self.append_log_prefix("grok ", text);
                self.activity = "撰寫中".into();
            }
            AgentEvent::ModelFinished { text, .. } => {
                if !text.is_empty() {
                    if let Some(last) = self.log.last_mut() {
                        if last.starts_with("grok ") {
                            *last = format!("grok {text}");
                        } else {
                            self.push_log(format!("grok {text}"));
                        }
                    } else {
                        self.push_log(format!("grok {text}"));
                    }
                }
            }
            AgentEvent::ToolStarted { name, args, .. } => {
                self.push_log(tool_started_line(name, args));
                self.activity = format!("工具 {name}");
            }
            AgentEvent::ToolFinished { name, output, .. } => {
                let preview: String = output.lines().next().unwrap_or("").chars().take(80).collect();
                self.push_log(format!("完成 {name}  {preview}"));
                self.activity = "思考中".into();
            }
            AgentEvent::FileChanged { path, kind, .. } => {
                self.push_log(format!("檔案 {kind} {path}"));
            }
            AgentEvent::Error { message, .. } => {
                self.push_log(format!("錯誤 {message}"));
            }
            AgentEvent::Notice { message, .. } => {
                self.push_log(message.clone());
            }
            AgentEvent::RunFinished { reason, text, .. } => {
                self.alive = false;
                self.status = format!("結束 ({reason})");
                self.activity.clear();
                if !text.is_empty()
                    && !self
                        .log
                        .iter()
                        .any(|l| l.starts_with("grok ") && l.contains(text))
                {
                    self.push_log(format!("grok {text}"));
                }
            }
            _ => {}
        }
    }
}

#[derive(Clone)]
struct SideMon {
    name: String,
    command: String,
    pid: u32,
    status: String,
    alive: bool,
    detail: String,
}

#[derive(Clone)]
struct SideBg {
    name: String,
    command: String,
    pid: u32,
    status: String,
    alive: bool,
    detail: String,
    log: Vec<String>,
}

impl SideBg {
    fn push_log(&mut self, line: String) {
        if self.log.len() > 400 {
            self.log.drain(0..150);
        }
        self.log.push(line);
    }
}

#[derive(Clone, Default)]
struct Queued {
    text: String,
    images: Vec<String>,
}

impl From<&str> for Queued {
    fn from(s: &str) -> Self {
        Self {
            text: s.to_string(),
            images: Vec::new(),
        }
    }
}

impl From<String> for Queued {
    fn from(text: String) -> Self {
        Self {
            text,
            images: Vec::new(),
        }
    }
}

impl Queued {
    fn label(&self) -> String {
        let t = self.text.replace('\n', " ");
        if self.images.is_empty() {
            t
        } else if t.is_empty() {
            format!("[{n} 張圖片]", n = self.images.len())
        } else {
            format!("{t}  [{n}圖]", n = self.images.len())
        }
    }
}

#[derive(Clone, Default, PartialEq, Eq)]
enum ChatSel {
    #[default]
    None,
    Row(usize),
    Image(String),
}

#[derive(Clone)]
enum Inspector {
    Child(String),
    Monitor(String),
    Background(String),
}

struct ParkedChat {
    session: SessionMeta,
    rows: Vec<Row>,
    status: String,
    cache: String,
    child_count: u32,
    running: bool,
    awaiting: bool,
    scroll: u16,
    stick_bottom: bool,
    queue: VecDeque<Queued>,
    inbox_tx: Option<mpsc::UnboundedSender<UserTurn>>,
    streaming: bool,
    open_tool: Option<(usize, usize)>,
    seal_tools: bool,
    activity: String,
    edit: Edit,
    work_started: Option<Instant>,
    queue_edit: Option<usize>,
    composer_stash: Option<Edit>,
    pending: Vec<String>,
    children: Vec<SideChild>,
    monitors: Vec<SideMon>,
    backgrounds: Vec<SideBg>,
    inspector: Option<Inspector>,
    inspector_scroll: u16,
}

struct AskState {
    question: Question,
    cursor: usize,
    chosen: Vec<bool>,
    values: Vec<String>,
    filling: bool,
    fill_edit: Edit,
    fill_scroll: u16,
}

impl AskState {
    fn new(question: Question) -> Self {
        let n = question.options.len();
        Self {
            question,
            cursor: 0,
            chosen: vec![false; n],
            values: vec![String::new(); n],
            filling: false,
            fill_edit: Edit::default(),
            fill_scroll: 0,
        }
    }

    fn n(&self) -> usize {
        self.question.options.len()
    }

    fn move_cursor(&mut self, delta: i16) {
        self.save_fill();
        let n = self.n() as i16;
        if n == 0 {
            return;
        }
        let next = (self.cursor as i16 + delta).rem_euclid(n);
        self.cursor = next as usize;
    }

    fn save_fill(&mut self) {
        if self.filling {
            let t: String = self.fill_edit.text.chars().take(ask::MAX_INPUT).collect();
            if let Some(slot) = self.values.get_mut(self.cursor) {
                *slot = t;
            }
            self.filling = false;
        }
    }

    fn enter_fill(&mut self) {
        let Some(opt) = self.question.options.get(self.cursor) else {
            return;
        };
        if !opt.input {
            return;
        }
        if !self.question.allow_multiple {
            self.chosen.fill(false);
            if let Some(c) = self.chosen.get_mut(self.cursor) {
                *c = true;
            }
        } else if let Some(c) = self.chosen.get_mut(self.cursor) {
            *c = true;
        }
        self.filling = true;
        self.fill_edit = Edit::at_end(self.values.get(self.cursor).cloned().unwrap_or_default());
        self.fill_scroll = 0;
    }

    fn mark_cursor(&mut self) {
        if self.question.allow_multiple {
            if let Some(c) = self.chosen.get_mut(self.cursor) {
                *c = !*c;
            }
        } else {
            self.chosen.fill(false);
            if let Some(c) = self.chosen.get_mut(self.cursor) {
                *c = true;
            }
        }
    }

    fn summary(&self) -> String {
        let picks: Vec<String> = self
            .question
            .options
            .iter()
            .enumerate()
            .filter(|(i, _)| self.chosen.get(*i).copied().unwrap_or(false))
            .map(|(i, o)| {
                if o.input {
                    format!("{}: {}", o.label, self.values.get(i).cloned().unwrap_or_default())
                } else {
                    o.label.clone()
                }
            })
            .collect();
        if picks.is_empty() {
            "已取消問卷".into()
        } else {
            format!("你選了  {}", picks.join("、"))
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum WsFocus {
    Path,
    List,
}

struct WorkspacePick {
    view: FolderView,
    edit: Edit,
    cursor: usize,
    scroll: u16,
    path_scroll: u16,
    focus: WsFocus,
    path_inner: Rect,
    list_area: Rect,
    notice: Option<String>,
}

impl WorkspacePick {
    fn open(start: &std::path::Path) -> Self {
        let view = folderpick::list_folder(&folderpick::existing_dir(start));
        let edit = Edit::at_end(folderpick::display_path(&view.cwd));
        Self {
            view,
            edit,
            cursor: 0,
            scroll: 0,
            path_scroll: 0,
            focus: WsFocus::Path,
            path_inner: Rect::default(),
            list_area: Rect::default(),
            notice: None,
        }
    }

    fn selected(&self) -> Option<&folderpick::Entry> {
        self.view.entries.get(self.cursor)
    }

    fn reveal(&mut self, vis: u16) {
        if vis == 0 {
            return;
        }
        if self.cursor < self.scroll as usize {
            self.scroll = self.cursor as u16;
        } else if self.cursor >= self.scroll as usize + vis as usize {
            self.scroll = (self.cursor + 1 - vis as usize) as u16;
        }
    }
}

struct App {
    rows: Vec<Row>,
    edit: Edit,
    status: String,
    cache: String,
    child_count: u32,
    running: bool,
    awaiting: bool,
    logged_in: bool,
    auth_path: PathBuf,
    login_ui: LoginUi,
    login_gen: u64,
    want_login: bool,
    scroll: u16,
    stick_bottom: bool,
    send_mode: SendMode,
    queue: VecDeque<Queued>,
    inbox_tx: Option<mpsc::UnboundedSender<UserTurn>>,
    knobs: Arc<Mutex<SessionKnobs>>,
    focus: Focus,
    setting_field: SettingField,
    settings: Option<Win>,
    drag: Option<(i16, i16)>,
    hits: Vec<(Rect, Hit)>,
    area: Rect,
    streaming: bool,
    composer_inner: Rect,
    composer_vscroll: u16,
    input_dragging: bool,
    catalog: ModelCatalog,
    catalog_status: CatalogStatus,
    drop: Option<DropKind>,
    drop_cursor: usize,
    drop_scroll: u16,
    want_catalog: bool,
    open_tool: Option<(usize, usize)>,
    seal_tools: bool,
    activity: String,
    tick: u8,
    current_id: String,
    session: SessionMeta,
    parked: HashMap<String, ParkedChat>,
    sessions: Vec<SessionMeta>,
    store: Option<SessionStore>,
    launch_workspace: PathBuf,
    sidebar_ids: Vec<String>,
    /// Inline sidebar rename: session id + editor.
    rename: Option<(String, Edit)>,
    rename_inner: Rect,
    work_started: Option<Instant>,
    /// Composer is editing `queue[index]`; auto-send is paused until commit/cancel.
    queue_edit: Option<usize>,
    composer_stash: Option<Edit>,
    pending: Vec<String>,
    chat_sel: ChatSel,
    preview: HashMap<(String, u16), Vec<Line<'static>>>,
    picker: Option<Picker>,
    image_proto: HashMap<(String, u16, u16), Protocol>,
    image_hits: Vec<String>,
    children: Vec<SideChild>,
    monitors: Vec<SideMon>,
    backgrounds: Vec<SideBg>,
    inspector: Option<Inspector>,
    inspector_scroll: u16,
    ask_hub: AskUserHub,
    ask_hubs: HashMap<String, AskUserHub>,
    ask: Option<AskState>,
    ask_fill_inner: Rect,
    /// When routing an event into a parked session, do not open the overlay.
    ask_passive: bool,
    workspace_pick: Option<WorkspacePick>,
}

impl App {
    fn push(&mut self, row: Row) {
        if matches!(&row, Row::User(_) | Row::Agent(_)) {
            self.seal_tools = true;
            self.finish_open_think();
        }
        self.rows.push(row);
        if self.rows.len() > 2_000 {
            self.rows.drain(0..self.rows.len() - 1_500);
        }
        if self.stick_bottom {
            self.scroll = 0;
        }
    }

    fn apply_event(&mut self, ev: AgentEvent) {
        match ev {
            AgentEvent::RunStarted { model, .. } => {
                self.running = true;
                self.awaiting = false;
                self.streaming = false;
                self.mark_work_start();
                self.activity = format!("連線 {model}");
                self.status = "工作中".into();
            }
            AgentEvent::TurnStarted { turn, .. } => {
                self.running = true;
                self.awaiting = false;
                self.streaming = false;
                self.mark_work_start();
                self.seal_tools = true;
                self.finish_open_think();
                self.mark_open_server_done();
                self.activity = "思考中".into();
                self.status = format!("第 {turn} 輪");
            }
            AgentEvent::ReasoningDelta { text, .. } => {
                if text.is_empty() {
                    return;
                }
                self.append_think(&text);
            }
            AgentEvent::ModelDelta { text, .. } => {
                if text.is_empty() {
                    return;
                }
                self.activity = "撰寫中".into();
                self.finish_open_think();
                if self.streaming {
                    if let Some(Row::Agent(s)) = self.rows.last_mut() {
                        s.text.push_str(&text);
                        if self.stick_bottom {
                            self.scroll = 0;
                        }
                        return;
                    }
                }
                self.push(Row::Agent(AgentMsg::new(text)));
                self.streaming = true;
                self.seal_tools = true;
            }
            AgentEvent::ModelFinished {
                text,
                finish,
                input_tokens,
                cached_tokens,
                ..
            } => {
                self.streaming = false;
                self.finish_open_think();
                if input_tokens > 0 {
                    let pct = (cached_tokens as f32 / input_tokens as f32) * 100.0;
                    self.cache = format!("{cached_tokens}/{input_tokens} ({pct:.0}%)");
                }
                if text.is_empty() {
                    return;
                }
                if let Some(Row::Agent(s)) = self.rows.last_mut() {
                    if s.text.is_empty()
                        || text.starts_with(s.text.as_str())
                        || s.text.starts_with(text.as_str())
                    {
                        s.text = text;
                        return;
                    }
                }
                if finish == "stop"
                    || !self
                        .rows
                        .iter()
                        .any(|r| matches!(r, Row::Agent(t) if t.text == text))
                {
                    self.push(Row::Agent(AgentMsg::new(text)));
                }
            }
            AgentEvent::ToolStarted { name, args, .. } => {
                self.streaming = false;
                self.finish_open_think();
                self.activity = live_tool_activity(&name, &args, "執行中");
                self.push_tool_start(name, args);
            }
            AgentEvent::ToolFinished { name, output, .. } => {
                let _ = enable_raw_mode();
                let pic = picture_from_tool(&name, &output);
                self.finish_tool(&name, output);
                if let Some((path, label)) = pic {
                    self.push(Row::Picture { path, label });
                }
                self.activity = "思考中".into();
            }
            AgentEvent::ServerToolObserved { kind, payload, .. } => {
                self.streaming = false;
                self.finish_open_think();
                self.observe_server(&kind, payload);
            }
            AgentEvent::FileChanged {
                path, kind, diff, ..
            } => {
                self.streaming = false;
                self.attach_file(FileChange { path, kind, diff });
            }
            AgentEvent::ContextCompacted {
                method,
                dropped_items,
                kept_items,
                ..
            } => {
                self.push(Row::Meta(format!(
                    "壓縮 ({method}) 丟 {dropped_items} 留 {kept_items}"
                )));
            }
            AgentEvent::ChildSpawned {
                name,
                agent_card_url,
                prompt,
                ..
            } => {
                self.upsert_child(name.clone(), prompt, agent_card_url);
                self.child_count = self.children.iter().filter(|c| c.alive).count() as u32;
                self.push(Row::Meta(format!("子代理 {name} 已啟動")));
            }
            AgentEvent::ChildExited { name, detail, .. } => {
                if let Some(c) = self.child_named_mut(&name) {
                    c.upsert_status(format!("結束 ({detail})"), false);
                    c.activity.clear();
                }
                self.child_count = self.children.iter().filter(|c| c.alive).count() as u32;
                self.push(Row::Meta(format!("子代理 {name} 結束")));
            }
            AgentEvent::MonitorAttached {
                name,
                command,
                pid,
                ..
            } => {
                self.upsert_monitor(name.clone(), command.clone(), pid);
                self.push(Row::Meta(format!("監控 {name} 已掛上  $ {command}")));
            }
            AgentEvent::MonitorExited { name, detail, .. } => {
                if let Some(m) = self.mon_named_mut(&name) {
                    m.alive = false;
                    m.status = "結束".into();
                    m.detail = detail.clone();
                }
                self.push(Row::Meta(format!("監控 {name} 結束  {detail}")));
            }
            AgentEvent::BackgroundStarted {
                name,
                command,
                pid,
                ..
            } => {
                self.upsert_background(name.clone(), command.clone(), pid);
                self.push(Row::Meta(format!("後台 {name} 已掛上  $ {command}")));
            }
            AgentEvent::BackgroundOutput {
                name,
                stream,
                text,
                ..
            } => {
                if let Some(b) = self.bg_named_mut(&name) {
                    b.push_log(format!("{stream} {text}"));
                }
            }
            AgentEvent::BackgroundExited { name, detail, .. } => {
                if let Some(b) = self.bg_named_mut(&name) {
                    b.alive = false;
                    b.status = "結束".into();
                    b.detail = detail.clone();
                }
                self.push(Row::Meta(format!("後台 {name} 結束  {detail}")));
            }
            AgentEvent::AgentMessage {
                from, to, text, ..
            } => {
                self.push_agent_message(from, to, text);
            }
            AgentEvent::AskUser {
                question,
                allow_multiple,
                options,
                ..
            } => {
                if options.is_empty() {
                    return;
                }
                self.push(Row::Meta(format!("問卷  {question}")));
                if self.ask_passive {
                    if let Some(h) = self.ask_hubs.get(&self.current_id) {
                        h.cancel();
                    }
                    return;
                }
                self.ask = Some(AskState::new(Question {
                    prompt: question,
                    allow_multiple,
                    options,
                }));
                self.focus = Focus::Ask;
                self.status = "請選擇".into();
            }
            AgentEvent::Notice { message, .. } => {
                self.push(Row::Meta(message));
            }
            AgentEvent::Error { message, .. } => {
                self.streaming = false;
                self.push(Row::Err(message));
            }
            AgentEvent::AwaitingInput { .. } => {
                self.running = false;
                self.awaiting = true;
                self.streaming = false;
                self.finish_open_think();
                self.stamp_work();
                self.activity.clear();
                self.status = "待命".into();
            }
            AgentEvent::RunFinished { reason, text, .. } => {
                self.cancel_ask();
                self.running = false;
                self.awaiting = false;
                self.streaming = false;
                self.finish_open_think();
                self.inbox_tx = None;
                self.activity.clear();
                self.status = format!("結束 ({reason})");
                if !text.is_empty()
                    && !self
                        .rows
                        .iter()
                        .any(|r| matches!(r, Row::Agent(t) if t.text == text))
                {
                    self.push(Row::Agent(AgentMsg::new(text)));
                }
                self.stamp_work();
            }
            AgentEvent::SessionNamed { .. } => {}
        }
    }

    fn child_named_mut(&mut self, name: &str) -> Option<&mut SideChild> {
        self.children.iter_mut().find(|c| c.name == name)
    }

    fn mon_named_mut(&mut self, name: &str) -> Option<&mut SideMon> {
        self.monitors.iter_mut().find(|m| m.name == name)
    }

    fn bg_named_mut(&mut self, name: &str) -> Option<&mut SideBg> {
        self.backgrounds.iter_mut().find(|b| b.name == name)
    }

    fn upsert_child(&mut self, name: String, prompt: String, card_url: String) {
        if let Some(c) = self.child_named_mut(&name) {
            if !prompt.is_empty() {
                c.prompt = prompt;
            }
            if !card_url.is_empty() {
                c.card_url = card_url;
            }
            c.upsert_status("工作中".into(), true);
            return;
        }
        self.children.push(SideChild {
            name,
            prompt,
            card_url,
            status: "工作中".into(),
            alive: true,
            activity: String::new(),
            log: Vec::new(),
            messages: Vec::new(),
        });
    }

    fn upsert_monitor(&mut self, name: String, command: String, pid: u32) {
        if let Some(m) = self.mon_named_mut(&name) {
            m.command = command;
            m.pid = pid;
            m.alive = true;
            m.status = "執行中".into();
            m.detail.clear();
            return;
        }
        self.monitors.push(SideMon {
            name,
            command,
            pid,
            status: "執行中".into(),
            alive: true,
            detail: String::new(),
        });
    }

    fn upsert_background(&mut self, name: String, command: String, pid: u32) {
        if let Some(b) = self.bg_named_mut(&name) {
            b.command = command;
            b.pid = pid;
            b.alive = true;
            b.status = "執行中".into();
            b.detail.clear();
            return;
        }
        self.backgrounds.push(SideBg {
            name,
            command,
            pid,
            status: "執行中".into(),
            alive: true,
            detail: String::new(),
            log: Vec::new(),
        });
    }

    fn push_agent_message(&mut self, from: String, to: String, text: String) {
        let child = if self.children.iter().any(|c| c.name == from) {
            from.clone()
        } else {
            to.clone()
        };
        if let Some(c) = self.child_named_mut(&child) {
            if c.messages.len() > 80 {
                c.messages.drain(0..20);
            }
            c.messages.push(SideMsg { from, to, text });
        }
    }

    fn apply_child_work(&mut self, ev: AgentEvent) {
        let name = ev.meta().agent_name.clone();
        if name.is_empty() || name == "root" {
            return;
        }
        if self.child_named_mut(&name).is_none() {
            self.upsert_child(name.clone(), String::new(), String::new());
        }
        if let Some(c) = self.child_named_mut(&name) {
            c.apply_work(&ev);
        }
        self.child_count = self.children.iter().filter(|c| c.alive).count() as u32;
    }

    fn has_side(&self) -> bool {
        !self.children.is_empty() || !self.monitors.is_empty() || !self.backgrounds.is_empty()
    }

    fn close_inspector(&mut self) {
        self.inspector = None;
        self.inspector_scroll = 0;
        if self.focus == Focus::Inspector {
            self.focus = Focus::Chat;
        }
    }

    fn attach_pending(&mut self, rel: String) -> bool {
        if self.pending.len() >= crate::vision::MAX_USER_IMAGES {
            self.status = format!("最多 {} 張圖片", crate::vision::MAX_USER_IMAGES);
            return false;
        }
        if self.pending.iter().any(|p| p == &rel) {
            return true;
        }
        self.pending.push(rel);
        true
    }

    fn ingest_paths(&mut self, paths: &[PathBuf]) -> usize {
        let mut n = 0;
        for p in paths {
            match crate::vision::ingest_image_file(&self.session.workspace, p) {
                Ok(rel) => {
                    if self.attach_pending(rel) {
                        n += 1;
                    }
                }
                Err(e) => self.status = format!("無法加入圖片: {e}"),
            }
        }
        n
    }

    fn paste_text_or_images(&mut self, s: &str) {
        let dropped = crate::vision::parse_image_drop(s);
        if !dropped.is_empty() {
            let n = self.ingest_paths(&dropped);
            if n > 0 {
                self.status = format!("已附上 {n} 張圖片");
                return;
            }
        }
        self.edit.insert_str(s);
    }

    /// Bracketed paste from the terminal. Empty payloads still read the OS
    /// clipboard so Ctrl+V of a bitmap/file is not dropped on the floor.
    fn paste_from_terminal(&mut self, s: &str) {
        let dropped = crate::vision::parse_image_drop(s);
        if !dropped.is_empty() {
            let n = self.ingest_paths(&dropped);
            if n > 0 {
                self.status = format!("已附上 {n} 張圖片");
                return;
            }
        }
        if s.trim().is_empty() {
            self.paste_clipboard();
            return;
        }
        self.edit.insert_str(s);
    }

    fn ingest_clipboard_images(&mut self) -> bool {
        if let Some(img) = crate::clipimg::read_image() {
            match crate::vision::save_user_image(&self.session.workspace, &img) {
                Ok(rel) => {
                    if self.attach_pending(rel) {
                        self.status = "已貼上圖片".into();
                        return true;
                    }
                    return false;
                }
                Err(e) => {
                    self.status = format!("無法貼上圖片: {e}");
                    return false;
                }
            }
        }
        let files = crate::clipimg::read_image_files();
        if files.is_empty() {
            return false;
        }
        let n = self.ingest_paths(&files);
        if n > 0 {
            self.status = format!("已附上 {n} 張圖片");
            true
        } else {
            false
        }
    }

    fn paste_image(&mut self) {
        if !self.ingest_clipboard_images() {
            self.status = "剪貼簿沒有圖片 — 先複製截圖或圖片檔，再點「貼上圖片」".into();
        }
    }

    fn paste_clipboard(&mut self) {
        if self.ingest_clipboard_images() {
            return;
        }
        if let Some(s) = clipboard_get() {
            self.paste_text_or_images(&s);
        }
    }

    fn copy_selection(&mut self) -> bool {
        if self.edit.has_sel() {
            if let Some(s) = self.edit.selected_text() {
                if clipboard_set(&s) {
                    self.status = "已複製".into();
                } else {
                    self.status = "無法複製到剪貼簿".into();
                }
                return true;
            }
        }
        match &self.chat_sel {
            ChatSel::Image(rel) => {
                let abs = self.session.workspace.join(rel);
                if clipboard_set_image(&abs) {
                    self.status = "已複製圖片".into();
                } else if clipboard_set(rel) {
                    self.status = "已複製路徑".into();
                } else {
                    self.status = "無法複製到剪貼簿".into();
                }
                true
            }
            ChatSel::Row(i) => {
                let Some(text) = self.rows.get(*i).map(row_copy_text) else {
                    return false;
                };
                if text.is_empty() {
                    return false;
                }
                if clipboard_set(&text) {
                    self.status = "已複製".into();
                } else {
                    self.status = "無法複製到剪貼簿".into();
                }
                true
            }
            ChatSel::None => false,
        }
    }

    fn take_turn(&mut self) -> Option<UserTurn> {
        let text = self.edit.text.trim().to_string();
        let images: Vec<PathBuf> = self.pending.drain(..).map(PathBuf::from).collect();
        if text.is_empty() && images.is_empty() {
            return None;
        }
        self.edit.clear();
        Some(UserTurn { text, images })
    }

    fn finish_open_think(&mut self) {
        for r in self.rows.iter_mut().rev() {
            if let Row::Think(t) = r {
                if !t.done {
                    if let Some(start) = t.started.take() {
                        t.elapsed_ms = start.elapsed().as_millis() as u64;
                    }
                    t.done = true;
                }
                return;
            }
        }
    }

    fn mark_work_start(&mut self) {
        if self.work_started.is_none() {
            self.work_started = Some(Instant::now());
        }
    }

    fn stamp_work(&mut self) {
        let Some(start) = self.work_started.take() else {
            return;
        };
        let ms = start.elapsed().as_millis() as u64;
        for r in self.rows.iter_mut().rev() {
            if let Row::Agent(a) = r {
                a.work_ms = ms;
                return;
            }
        }
        self.push(Row::Meta(format!("工作 {}", md::fmt_duration(ms))));
    }

    fn append_think(&mut self, delta: &str) {
        self.activity = "思考中".into();
        self.streaming = false;
        if let Some(Row::Think(t)) = self.rows.last_mut() {
            if !t.done {
                t.text.push_str(delta);
                if t.started.is_none() {
                    t.started = Some(Instant::now());
                }
                if self.stick_bottom {
                    self.scroll = 0;
                }
                return;
            }
        }
        self.push(Row::Think(Think {
            text: delta.to_string(),
            expanded: false,
            done: false,
            elapsed_ms: 0,
            started: Some(Instant::now()),
        }));
    }

    fn push_tool_start(&mut self, name: String, args: Value) {
        let call = ToolCall {
            name,
            args,
            output: String::new(),
            files: Vec::new(),
            done: false,
            phase: "執行中".into(),
        };
        let fresh = self.seal_tools || !matches!(self.rows.last(), Some(Row::Tools(_)));
        if fresh {
            self.push(Row::Tools(ToolGroup {
                calls: vec![call],
                expanded: false,
            }));
            self.seal_tools = false;
        } else if let Some(Row::Tools(g)) = self.rows.last_mut() {
            g.calls.push(call);
        }
    }

    fn finish_tool(&mut self, name: &str, output: String) {
        let Some(g) = self.rows.iter_mut().rev().find_map(|r| match r {
            Row::Tools(g) => Some(g),
            _ => None,
        }) else {
            return;
        };
        let idx = g
            .calls
            .iter()
            .rposition(|c| c.name == name && !c.done)
            .or_else(|| g.calls.iter().rposition(|c| !c.done))
            .or_else(|| g.calls.len().checked_sub(1));
        if let Some(i) = idx {
            let c = &mut g.calls[i];
            c.done = true;
            c.phase = "完成".into();
            c.files.extend(parse_file_changes(&output));
            c.output = output;
        }
    }

    fn observe_server(&mut self, kind: &str, payload: Value) {
        let name = canonical_server_tool(kind);
        let phase = server_phase(kind, &payload);
        let query = server_query(&payload);
        self.activity = if query.is_empty() {
            format!("{name}  {phase}")
        } else {
            format!("{name}  {phase}  {query}")
        };

        let mut args = payload;
        if args.get("query").is_none() && !query.is_empty() {
            args["query"] = Value::String(query.clone());
        }
        let done = phase_is_done(&phase);

        if !self.seal_tools {
            if let Some(Row::Tools(g)) = self.rows.last_mut() {
                if let Some(c) = g.calls.iter_mut().rev().find(|c| c.name == name) {
                    if !query.is_empty() {
                        c.args["query"] = Value::String(query);
                    }
                    c.phase = phase.clone();
                    c.output = server_tool_line(kind, &c.args);
                    c.done = done;
                    return;
                }
            }
        }

        let output = server_tool_line(kind, &args);
        let call = ToolCall {
            name,
            args,
            output,
            files: Vec::new(),
            done,
            phase,
        };
        let fresh = self.seal_tools || !matches!(self.rows.last(), Some(Row::Tools(_)));
        if fresh {
            self.push(Row::Tools(ToolGroup {
                calls: vec![call],
                expanded: false,
            }));
            self.seal_tools = false;
        } else if let Some(Row::Tools(g)) = self.rows.last_mut() {
            g.calls.push(call);
        }
    }

    fn mark_open_server_done(&mut self) {
        if let Some(Row::Tools(g)) = self.rows.last_mut() {
            for c in &mut g.calls {
                if !c.done && (c.name == "web_search" || c.name == "x_search") {
                    c.done = true;
                    if c.phase != "完成" {
                        c.phase = "完成".into();
                    }
                }
            }
        }
    }

    fn attach_file(&mut self, file: FileChange) {
        let Some(g) = self.rows.iter_mut().rev().find_map(|r| match r {
            Row::Tools(g) => Some(g),
            _ => None,
        }) else {
            return;
        };
        if let Some(c) = g.calls.last_mut() {
            if !c.files.iter().any(|f| f.path == file.path) {
                c.files.push(file);
            }
        }
    }

    fn dismiss_tool_ui(&mut self) -> bool {
        if self.open_tool.take().is_some() {
            return true;
        }
        let mut any = false;
        for r in &mut self.rows {
            if let Row::Tools(g) = r {
                if g.expanded {
                    g.expanded = false;
                    any = true;
                }
            }
            if let Row::Think(t) = r {
                if t.expanded {
                    t.expanded = false;
                    any = true;
                }
            }
        }
        any
    }

    fn cancel_ask(&mut self) {
        let id = self.current_id.clone();
        self.cancel_session_ask(&id);
    }

    fn cancel_session_ask(&mut self, id: &str) {
        if let Some(h) = self.ask_hubs.get(id) {
            h.cancel();
        }
        if id == self.current_id {
            self.ask_hub.cancel();
            if self.ask.take().is_some() {
                if self.focus == Focus::Ask {
                    self.focus = Focus::Chat;
                }
                self.status = "已取消問卷".into();
            }
        }
    }

    fn bind_ask_hub(&mut self) {
        self.ask_hub = self
            .ask_hubs
            .get(&self.current_id)
            .cloned()
            .unwrap_or_else(AskUserHub::new);
    }

    fn attach_ask_hub(&mut self, run_id: &str) -> AskUserHub {
        let hub = AskUserHub::new();
        self.ask_hubs.insert(run_id.to_string(), hub.clone());
        self.ask_hub = hub.clone();
        hub
    }

    fn submit_ask(&mut self) {
        let Some(mut ask) = self.ask.take() else {
            return;
        };
        let mut values = ask.values.clone();
        if ask.filling {
            if let Some(slot) = values.get_mut(ask.cursor) {
                *slot = ask.fill_edit.text.chars().take(ask::MAX_INPUT).collect();
            }
        }
        match ask::answer_from_picks(&ask.question, &ask.chosen, &values) {
            Ok(body) => {
                ask.values = values;
                let summary = ask.summary();
                self.ask_hub.answer(body);
                self.push(Row::Meta(summary));
                self.focus = Focus::Chat;
                self.status = "已回答".into();
            }
            Err(msg) => {
                self.status = msg;
                self.ask = Some(ask);
                self.focus = Focus::Ask;
            }
        }
    }

    fn activate_ask_option(&mut self, index: usize, submit_if_ready: bool) {
        let Some(ask) = self.ask.as_mut() else {
            return;
        };
        if index >= ask.n() {
            return;
        }
        ask.save_fill();
        ask.cursor = index;
        let input = ask.question.options.get(index).is_some_and(|o| o.input);
        ask.mark_cursor();
        if input {
            ask.enter_fill();
            return;
        }
        if submit_if_ready && !ask.question.allow_multiple {
            self.submit_ask();
        }
    }

    fn snapshot_live(&mut self) -> ParkedChat {
        ParkedChat {
            session: self.session.clone(),
            rows: std::mem::take(&mut self.rows),
            status: std::mem::take(&mut self.status),
            cache: std::mem::take(&mut self.cache),
            child_count: self.child_count,
            running: self.running,
            awaiting: self.awaiting,
            scroll: self.scroll,
            stick_bottom: self.stick_bottom,
            queue: std::mem::take(&mut self.queue),
            inbox_tx: self.inbox_tx.take(),
            streaming: self.streaming,
            open_tool: self.open_tool.take(),
            seal_tools: self.seal_tools,
            activity: std::mem::take(&mut self.activity),
            edit: std::mem::take(&mut self.edit),
            work_started: self.work_started.take(),
            queue_edit: self.queue_edit.take(),
            composer_stash: self.composer_stash.take(),
            pending: std::mem::take(&mut self.pending),
            children: std::mem::take(&mut self.children),
            monitors: std::mem::take(&mut self.monitors),
            backgrounds: std::mem::take(&mut self.backgrounds),
            inspector: self.inspector.take(),
            inspector_scroll: self.inspector_scroll,
        }
    }

    fn install_live(&mut self, p: ParkedChat) {
        self.session = p.session;
        self.current_id = self.session.id.clone();
        self.rows = p.rows;
        self.status = p.status;
        self.cache = p.cache;
        self.child_count = p.child_count;
        self.running = p.running;
        self.awaiting = p.awaiting;
        self.scroll = p.scroll;
        self.stick_bottom = p.stick_bottom;
        self.queue = p.queue;
        self.inbox_tx = p.inbox_tx;
        self.streaming = p.streaming;
        self.open_tool = p.open_tool;
        self.seal_tools = p.seal_tools;
        self.activity = p.activity;
        self.edit = p.edit;
        self.work_started = p.work_started;
        self.queue_edit = p.queue_edit;
        self.composer_stash = p.composer_stash;
        self.pending = p.pending;
        self.chat_sel = ChatSel::None;
        self.preview.clear();
        self.image_proto.clear();
        self.children = p.children;
        self.monitors = p.monitors;
        self.backgrounds = p.backgrounds;
        self.inspector = p.inspector;
        self.inspector_scroll = p.inspector_scroll;
        self.composer_vscroll = 0;
        self.ask = None;
        if self.focus == Focus::Ask {
            self.focus = Focus::Chat;
        }
        if self.focus == Focus::Workspace {
            self.focus = Focus::Chat;
        }
    }

    fn persist_transcript(&mut self) {
        let Some(store) = &self.store else {
            return;
        };
        let Ok(rows) = serde_json::to_value(&self.rows) else {
            return;
        };
        let _ = store.save_transcript(&self.session.id, &rows);
        self.session.updated_at = chrono::Utc::now();
        let _ = store.save_meta(&self.session);
    }

    fn apply_title(&mut self, id: &str, name: &str) {
        if self.current_id == id {
            if let Some(store) = &self.store {
                let _ = store.touch_name(&mut self.session, name.to_string(), true);
            } else if !self.session.name_is_manual {
                self.session.name = session::sanitize_title(name);
                self.session.named = true;
            }
            return;
        }
        if let Some(p) = self.parked.get_mut(id) {
            if let Some(store) = &self.store {
                let _ = store.touch_name(&mut p.session, name.to_string(), true);
            } else if !p.session.name_is_manual {
                p.session.name = session::sanitize_title(name);
                p.session.named = true;
            }
        }
    }

    fn route_event(&mut self, ev: AgentEvent) {
        let sid = ev.session_id().to_string();
        if let AgentEvent::SessionNamed { name, .. } = &ev {
            self.apply_title(&sid, name);
            return;
        }
        let child_work = ev.is_child_work();
        let skip_persist = matches!(ev, AgentEvent::BackgroundOutput { .. });
        if sid.is_empty() || sid == self.current_id {
            if child_work {
                self.apply_child_work(ev);
            } else {
                self.apply_event(ev);
                if !skip_persist {
                    self.persist_transcript();
                }
            }
            return;
        }
        if !self.parked.contains_key(&sid) {
            return;
        }
        let parked = self.parked.remove(&sid).unwrap();
        let parked = self.with_parked(parked, |app| {
            if child_work {
                app.apply_child_work(ev);
            } else {
                app.apply_event(ev);
                if !skip_persist {
                    app.persist_transcript();
                }
            }
        });
        self.parked.insert(sid, parked);
    }

    fn with_parked<F: FnOnce(&mut Self)>(&mut self, parked: ParkedChat, f: F) -> ParkedChat {
        let saved_ask = self.ask.take();
        let ask_focus = self.focus == Focus::Ask;
        let saved = self.snapshot_live();
        self.install_live(parked);
        self.ask_passive = true;
        f(self);
        self.ask_passive = false;
        let parked = self.snapshot_live();
        self.install_live(saved);
        self.ask = saved_ask;
        if self.ask.is_some() && ask_focus {
            self.focus = Focus::Ask;
        }
        parked
    }

    fn is_blank_draft(&self) -> bool {
        !self.session.named
            && self.inbox_tx.is_none()
            && !self.running
            && self
                .rows
                .iter()
                .all(|r| matches!(r, Row::Meta(_)))
    }

    fn begin_new_chat(&mut self) {
        self.cancel_ask();
        self.cancel_rename();
        let start = if self.session.workspace.as_os_str().is_empty() {
            self.launch_workspace.clone()
        } else {
            self.session.workspace.clone()
        };
        self.workspace_pick = Some(WorkspacePick::open(&start));
        self.focus = Focus::Workspace;
    }

    fn sync_workspace_pick(&mut self) {
        let Some(p) = self.workspace_pick.as_mut() else {
            return;
        };
        let fallback = p.view.cwd.clone();
        p.view = folderpick::view_for_input(&p.edit.text, &fallback);
        if p.view.entries.is_empty() {
            p.cursor = 0;
        } else if p.cursor >= p.view.entries.len() {
            p.cursor = p.view.entries.len() - 1;
        }
        p.scroll = p.scroll.min(p.cursor as u16);
        p.notice = p.view.error.clone();
    }

    fn enter_workspace_dir(&mut self, dir: PathBuf) {
        let Some(p) = self.workspace_pick.as_mut() else {
            return;
        };
        if !dir.is_dir() {
            p.notice = Some("不是資料夾".into());
            return;
        }
        p.view = folderpick::list_folder(&dir);
        p.edit = Edit::at_end(folderpick::display_path(&p.view.cwd));
        p.cursor = 0;
        p.scroll = 0;
        p.notice = p.view.error.clone();
        p.focus = WsFocus::List;
    }

    fn activate_ws_entry(&mut self, idx: usize) {
        let Some(ent) = self
            .workspace_pick
            .as_ref()
            .and_then(|p| p.view.entries.get(idx).cloned())
        else {
            return;
        };
        if let Some(p) = self.workspace_pick.as_mut() {
            p.cursor = idx;
        }
        if ent.is_dir {
            self.enter_workspace_dir(ent.path);
        } else if let Some(p) = self.workspace_pick.as_mut() {
            p.edit = Edit::at_end(folderpick::display_path(&ent.path));
            p.notice = Some("檔案：確定時會使用上層資料夾".into());
            p.focus = WsFocus::List;
        }
    }

    fn cancel_workspace_pick(&mut self) {
        self.workspace_pick = None;
        if self.focus == Focus::Workspace {
            self.focus = Focus::Chat;
        }
    }

    fn confirm_workspace_pick(&mut self) {
        let Some(p) = self.workspace_pick.as_ref() else {
            return;
        };
        let typed = p.edit.text.clone();
        let cwd = p.view.cwd.clone();
        let selected = p.selected().cloned();
        let dir = if let Ok(abs) = std::fs::canonicalize(typed.trim()) {
            if abs.is_dir() {
                folderpick::normalize(&abs)
            } else if abs.is_file() {
                folderpick::existing_dir(&abs)
            } else {
                folderpick::workspace_of(&cwd, selected.as_ref())
            }
        } else {
            folderpick::workspace_of(&cwd, selected.as_ref())
        };
        if !dir.is_dir() {
            if let Some(p) = self.workspace_pick.as_mut() {
                p.notice = Some("請選擇存在的資料夾".into());
            }
            return;
        }
        self.workspace_pick = None;
        self.focus = Focus::Chat;
        self.create_chat(dir);
    }

    fn create_workspace_dir(&mut self) {
        let Some(p) = self.workspace_pick.as_ref() else {
            return;
        };
        let target = folderpick::create_target(&p.edit.text, &p.view.cwd);
        match folderpick::mkdir(&target) {
            Ok(dir) => {
                self.enter_workspace_dir(dir);
                if let Some(p) = self.workspace_pick.as_mut() {
                    p.notice = Some("已建立資料夾".into());
                }
            }
            Err(e) => {
                if let Some(p) = self.workspace_pick.as_mut() {
                    p.notice = Some(format!("無法建立: {e}"));
                }
            }
        }
    }

    fn new_chat(&mut self) {
        self.begin_new_chat();
    }

    fn create_chat(&mut self, workspace: PathBuf) {
        let workspace = folderpick::existing_dir(&workspace);
        self.launch_workspace = workspace.clone();
        if self.is_blank_draft() {
            self.session.workspace = workspace.clone();
            if let Some(store) = &self.store {
                self.session.updated_at = chrono::Utc::now();
                let _ = store.save_meta(&self.session);
            }
            self.status = format!("工作目錄  {}", folderpick::display_path(&workspace));
            return;
        }
        self.cancel_ask();
        self.persist_transcript();
        let parked = self.snapshot_live();
        self.parked.insert(parked.session.id.clone(), parked);
        let session = if let Some(store) = &self.store {
            store
                .create(workspace.clone())
                .unwrap_or_else(|_| SessionMeta::new(workspace.clone()))
        } else {
            SessionMeta::new(workspace)
        };
        self.install_live(fresh_chat(session));
        self.bind_ask_hub();
        self.refresh_session_list();
        self.status = format!(
            "工作目錄  {}",
            folderpick::display_path(&self.session.workspace)
        );
    }

    fn switch_to(&mut self, id: &str) {
        if id == self.current_id {
            return;
        }
        self.cancel_ask();
        self.persist_transcript();
        let parked = self.snapshot_live();
        self.parked.insert(parked.session.id.clone(), parked);
        if let Some(p) = self.parked.remove(id) {
            self.install_live(p);
            self.bind_ask_hub();
            return;
        }
        let loaded = self.load_from_store(id);
        self.install_live(loaded);
        self.bind_ask_hub();
    }

    fn load_from_store(&self, id: &str) -> ParkedChat {
        let session = self
            .store
            .as_ref()
            .and_then(|s| s.load_meta(id).ok())
            .unwrap_or_else(|| {
                let mut m = SessionMeta::new(self.launch_workspace.clone());
                m.id = id.to_string();
                m
            });
        let rows = self
            .store
            .as_ref()
            .and_then(|s| s.load_transcript(id).ok())
            .and_then(|v| serde_json::from_value::<Vec<Row>>(v).ok())
            .unwrap_or_default();
        ParkedChat {
            session,
            rows,
            status: "待命".into(),
            cache: "cache —".into(),
            child_count: 0,
            running: false,
            awaiting: false,
            scroll: 0,
            stick_bottom: true,
            queue: VecDeque::new(),
            inbox_tx: None,
            streaming: false,
            open_tool: None,
            seal_tools: false,
            activity: String::new(),
            edit: Edit::default(),
            work_started: None,
            queue_edit: None,
            composer_stash: None,
            pending: Vec::new(),
            children: Vec::new(),
            monitors: Vec::new(),
            backgrounds: Vec::new(),
            inspector: None,
            inspector_scroll: 0,
        }
    }

    fn refresh_session_list(&mut self) {
        let mut by_id: HashMap<String, SessionMeta> = HashMap::new();
        if let Some(store) = &self.store {
            if let Ok(list) = store.list() {
                for s in list {
                    by_id.insert(s.id.clone(), s);
                }
            }
        }
        by_id.insert(self.session.id.clone(), self.session.clone());
        for p in self.parked.values() {
            by_id.insert(p.session.id.clone(), p.session.clone());
        }
        let mut list: Vec<SessionMeta> = by_id.into_values().collect();
        list.sort_by(|a, b| b.updated_at.cmp(&a.updated_at).then(b.id.cmp(&a.id)));
        self.sessions = list;
        self.sidebar_ids = self.sessions.iter().map(|s| s.id.clone()).collect();
    }

    fn finish_run(&mut self, id: &str, out: crate::agent::RunOutcome) {
        self.cancel_session_ask(id);
        self.ask_hubs.remove(id);
        if id == self.current_id {
            self.bind_ask_hub();
            self.running = false;
            self.awaiting = false;
            self.inbox_tx = None;
            if self.status.starts_with("工作") || self.status.starts_with("第") {
                self.status = format!("結束 ({} 輪)", out.turns);
            }
            return;
        }
        if let Some(p) = self.parked.get_mut(id) {
            p.running = false;
            p.awaiting = false;
            p.inbox_tx = None;
            if p.status.starts_with("工作") || p.status.starts_with("第") {
                p.status = format!("結束 ({} 輪)", out.turns);
            }
        }
    }

    fn begin_rename(&mut self, id: &str) {
        let name = if self.current_id == id {
            self.session.name.clone()
        } else if let Some(p) = self.parked.get(id) {
            p.session.name.clone()
        } else {
            self.sessions
                .iter()
                .find(|s| s.id == id)
                .map(|s| s.name.clone())
                .unwrap_or_else(|| "新對話".into())
        };
        self.rename = Some((id.to_string(), Edit::at_end(name)));
        self.focus = Focus::Rename;
    }

    fn cancel_rename(&mut self) {
        self.rename = None;
        if self.focus == Focus::Rename {
            self.focus = Focus::Chat;
        }
    }

    fn commit_rename(&mut self) {
        let Some((id, edit)) = self.rename.take() else {
            return;
        };
        if self.focus == Focus::Rename {
            self.focus = Focus::Chat;
        }
        let name = session::sanitize_title(&edit.text);
        if name.is_empty() {
            return;
        }
        self.apply_manual_name(&id, &name);
        self.refresh_session_list();
    }

    fn begin_queue_edit(&mut self, index: usize) {
        if index >= self.queue.len() {
            return;
        }
        if self.queue_edit == Some(index) {
            return;
        }
        let mut idx = index;
        if let Some(cur) = self.queue_edit {
            let removed = self.commit_queue_edit();
            if removed && cur < idx {
                idx = idx.saturating_sub(1);
            }
            if idx >= self.queue.len() {
                return;
            }
        }
        if self.composer_stash.is_none() {
            self.composer_stash = Some(std::mem::take(&mut self.edit));
        }
        let text = self.queue[idx].text.clone();
        self.edit = Edit::at_end(text);
        self.queue_edit = Some(idx);
        self.focus = Focus::Chat;
        self.composer_vscroll = 0;
    }

    /// Restore the original queued text and the previous composer draft.
    fn cancel_queue_edit(&mut self) {
        if self.queue_edit.take().is_none() {
            return;
        }
        self.edit = self.composer_stash.take().unwrap_or_default();
        self.composer_vscroll = 0;
    }

    /// Save the composer into the queued item. Empty text drops that item.
    /// Returns true if the item was removed.
    fn commit_queue_edit(&mut self) -> bool {
        let Some(i) = self.queue_edit.take() else {
            return false;
        };
        let text = self.edit.text.trim().to_string();
        let mut removed = false;
        if i < self.queue.len() {
            if text.is_empty() && self.queue[i].images.is_empty() {
                self.queue.remove(i);
                removed = true;
            } else if let Some(slot) = self.queue.get_mut(i) {
                slot.text = text;
            }
        }
        self.edit = self.composer_stash.take().unwrap_or_default();
        self.composer_vscroll = 0;
        removed
    }

    fn apply_manual_name(&mut self, id: &str, name: &str) {
        if self.current_id == id {
            if let Some(store) = &self.store {
                let _ = store.rename_manual(&mut self.session, name.to_string());
            } else {
                self.session.name = name.to_string();
                self.session.named = true;
                self.session.name_is_manual = true;
            }
            return;
        }
        if let Some(p) = self.parked.get_mut(id) {
            if let Some(store) = &self.store {
                let _ = store.rename_manual(&mut p.session, name.to_string());
            } else {
                p.session.name = name.to_string();
                p.session.named = true;
                p.session.name_is_manual = true;
            }
            return;
        }
        if let Some(store) = &self.store {
            if let Ok(mut meta) = store.load_meta(id) {
                let _ = store.rename_manual(&mut meta, name.to_string());
            }
        }
    }

    fn delete_session(&mut self, id: &str) {
        self.cancel_rename();
        self.cancel_session_ask(id);
        self.ask_hubs.remove(id);
        let deleting_current = self.current_id == id;
        if deleting_current {
            self.inbox_tx = None;
            self.running = false;
            self.awaiting = false;
        } else {
            self.parked.remove(id);
        }
        if let Some(store) = &self.store {
            let _ = store.delete(id);
        }
        let next = self
            .sessions
            .iter()
            .map(|s| s.id.clone())
            .chain(self.parked.keys().cloned())
            .find(|sid| sid != id);
        if deleting_current {
            if let Some(nid) = next.filter(|n| n != id) {
                if let Some(p) = self.parked.remove(&nid) {
                    self.install_live(p);
                } else {
                    let loaded = self.load_from_store(&nid);
                    self.install_live(loaded);
                }
            } else {
                let workspace = self.launch_workspace.clone();
                let session = if let Some(store) = &self.store {
                    store
                        .create(workspace)
                        .unwrap_or_else(|_| SessionMeta::new(self.launch_workspace.clone()))
                } else {
                    SessionMeta::new(workspace)
                };
                self.install_live(fresh_chat(session));
            }
            self.bind_ask_hub();
        }
        self.refresh_session_list();
    }
}

fn intro_rows() -> Vec<Row> {
    vec![
        Row::Meta("Cursor 風格聊天。工作中可排隊或直接插入下一輪。".into()),
        Row::Meta("點訊息或圖片後 Ctrl-C 複製 · 無選取則離開  ·  Ctrl-N 新對話 · 點「貼上圖片」附圖".into()),
    ]
}

struct BootSession {
    session: SessionMeta,
    rows: Vec<Row>,
    created: bool,
}

fn load_session_rows(store: Option<&SessionStore>, id: &str) -> Vec<Row> {
    store
        .and_then(|s| s.load_transcript(id).ok())
        .and_then(|v| serde_json::from_value(v).ok())
        .unwrap_or_default()
}

fn session_has_chat_content(store: Option<&SessionStore>, meta: &SessionMeta) -> bool {
    if meta.named || meta.name_is_manual {
        return true;
    }
    load_session_rows(store, &meta.id)
        .iter()
        .any(|r| !matches!(r, Row::Meta(_)))
}

/// Resume the most recently updated real chat. Blank drafts left by earlier
/// launches are skipped so opening the TUI does not keep minting 新對話.
fn pick_resume_session(store: Option<&SessionStore>, listed: &[SessionMeta]) -> Option<SessionMeta> {
    if listed.is_empty() {
        return None;
    }
    listed
        .iter()
        .find(|m| session_has_chat_content(store, m))
        .cloned()
        .or_else(|| listed.first().cloned())
}

fn boot_session(
    store: Option<&SessionStore>,
    listed: &[SessionMeta],
    workspace: PathBuf,
) -> BootSession {
    if let Some(session) = pick_resume_session(store, listed) {
        let rows = load_session_rows(store, &session.id);
        let rows = if rows.is_empty() {
            intro_rows()
        } else {
            rows
        };
        return BootSession {
            session,
            rows,
            created: false,
        };
    }
    let session = match store {
        Some(s) => s
            .create(workspace.clone())
            .unwrap_or_else(|_| SessionMeta::new(workspace)),
        None => SessionMeta::new(workspace),
    };
    BootSession {
        session,
        rows: intro_rows(),
        created: true,
    }
}

fn fresh_chat(session: SessionMeta) -> ParkedChat {
    ParkedChat {
        session,
        rows: intro_rows(),
        status: "待命".into(),
        cache: "cache —".into(),
        child_count: 0,
        running: false,
        awaiting: false,
        scroll: 0,
        stick_bottom: true,
        queue: VecDeque::new(),
        inbox_tx: None,
        streaming: false,
        open_tool: None,
        seal_tools: false,
        activity: String::new(),
        edit: Edit::default(),
        work_started: None,
        queue_edit: None,
        composer_stash: None,
            pending: Vec::new(),
            children: Vec::new(),
            monitors: Vec::new(),
            backgrounds: Vec::new(),
            inspector: None,
            inspector_scroll: 0,
    }
}

#[cfg(test)]
fn dummy_session() -> SessionMeta {
    let mut m = SessionMeta::new(PathBuf::from("."));
    m.id = "s".into();
    m
}

fn tool_started_line(name: &str, args: &Value) -> String {
    match name {
        "run_command" | "attach_monitor" | "run_background" => {
            let cmd = args.get("command").and_then(Value::as_str).unwrap_or("");
            format!("▸ {name}  $ {cmd}")
        }
        "kill_background" | "read_background" => {
            let n = args.get("name").and_then(Value::as_str).unwrap_or("");
            format!("▸ {name}  {n}")
        }
        "write_file" | "read_file" | "delete_file" | "list_dir" | "screenshot" | "read_image" => {
            let path = args.get("path").and_then(Value::as_str).unwrap_or(".");
            format!("▸ {name}  {path}")
        }
        "spawn_agent" => {
            let n = args
                .get("name")
                .or_else(|| args.get("agent_name"))
                .and_then(Value::as_str)
                .unwrap_or("");
            format!("▸ spawn_agent  {n}")
        }
        "ask_user" => {
            let q = args.get("question").and_then(Value::as_str).unwrap_or("");
            format!("▸ ask_user  {q}")
        }
        _ => format!("▸ {name}"),
    }
}

fn tool_finished_line(name: &str, output: &str) -> String {
    match name {
        "write_file" | "delete_file" | "list_dir" | "screenshot" | "read_image" => {
            format!("✓ {name}")
        }
        "run_command" => {
            if let Ok(v) = serde_json::from_str::<Value>(output) {
                let code = v
                    .get("exit_code")
                    .map(|c| c.to_string())
                    .unwrap_or_else(|| "?".into());
                let stdout = v.get("stdout").and_then(Value::as_str).unwrap_or("");
                let first = stdout.lines().next().unwrap_or("").trim();
                let clip: String = first.chars().take(80).collect();
                if clip.is_empty() {
                    format!("✓ run_command  exit {code}")
                } else {
                    format!("✓ run_command  exit {code}  {clip}")
                }
            } else {
                format!("✓ {name}")
            }
        }
        _ => {
            let first = output.lines().next().unwrap_or("").trim();
            let clip: String = first.chars().take(80).collect();
            if clip.is_empty() {
                format!("✓ {name}")
            } else {
                format!("✓ {name}  {clip}")
            }
        }
    }
}

fn server_tool_line(kind: &str, payload: &Value) -> String {
    let query = server_query(payload);
    let short = canonical_server_tool(kind);
    if query.is_empty() {
        format!("▸ {short}")
    } else {
        format!("▸ {short}  {query}")
    }
}

fn server_query(payload: &Value) -> String {
    payload
        .pointer("/action/query")
        .or_else(|| payload.get("query"))
        .or_else(|| payload.pointer("/item/action/query"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string()
}

fn canonical_server_tool(kind: &str) -> String {
    let k = kind.to_ascii_lowercase();
    if k.contains("x_search") {
        "x_search".into()
    } else if k.contains("web_search") {
        "web_search".into()
    } else {
        server_tool_short(kind)
    }
}

fn server_phase(kind: &str, payload: &Value) -> String {
    let k = kind.to_ascii_lowercase();
    let status = payload
        .get("status")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_ascii_lowercase();
    if k.contains("searching") || status == "searching" {
        "搜尋中".into()
    } else if k.contains("in_progress") || status == "in_progress" {
        "進行中".into()
    } else if k.contains("completed") || k.ends_with(".done") || status == "completed" || status == "done"
    {
        "完成".into()
    } else if !status.is_empty() {
        status
    } else {
        "進行中".into()
    }
}

fn phase_is_done(phase: &str) -> bool {
    phase == "完成" || phase == "completed" || phase == "done"
}

fn live_tool_activity(name: &str, args: &Value, phase: &str) -> String {
    match name {
        "run_command" | "run_background" | "attach_monitor" => {
            let cmd = args.get("command").and_then(Value::as_str).unwrap_or("");
            format!("{phase}  $ {cmd}")
        }
        "write_file" | "read_file" | "delete_file" | "list_dir" | "screenshot" | "read_image" => {
            let path = args.get("path").and_then(Value::as_str).unwrap_or(".");
            format!("{phase}  {name}  {path}")
        }
        _ => format!("{phase}  {name}"),
    }
}

fn spinner(tick: u8) -> char {
    const FRAMES: &[char] = &['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];
    FRAMES[(tick as usize) % FRAMES.len()]
}

fn server_tool_short(kind: &str) -> String {
    kind.rsplit('.')
        .next()
        .unwrap_or(kind)
        .trim_end_matches("_call")
        .trim_end_matches(".in_progress")
        .to_string()
}

fn parse_file_changes(output: &str) -> Vec<FileChange> {
    let Ok(v) = serde_json::from_str::<Value>(output) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    let mut push_item = |item: &Value| {
        let path = item
            .get("path")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        if path.is_empty() {
            return;
        }
        out.push(FileChange {
            path,
            kind: item
                .get("kind")
                .and_then(Value::as_str)
                .unwrap_or("modify")
                .to_string(),
            diff: item
                .get("diff")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
        });
    };
    if v.get("path").and_then(Value::as_str).is_some() && v.get("diff").is_some() {
        push_item(&v);
    } else if let Some(files) = v.get("files").and_then(Value::as_array) {
        for item in files {
            push_item(item);
        }
    }
    out
}

struct ChatLine {
    line: Line<'static>,
    hit: Option<Hit>,
    wrap: bool,
    /// When set, this item occupies `height` rows and is painted via a graphics protocol.
    graphic: Option<(String, u16, u16)>,
}

fn chat_line(line: Line<'static>, hit: Option<Hit>) -> ChatLine {
    ChatLine {
        line,
        hit,
        wrap: true,
        graphic: None,
    }
}

fn chat_line_raw(line: Line<'static>, hit: Option<Hit>) -> ChatLine {
    ChatLine {
        line,
        hit,
        wrap: false,
        graphic: None,
    }
}

fn chat_graphic(rel: String, width: u16, height: u16, hit: Option<Hit>) -> ChatLine {
    ChatLine {
        line: Line::from(""),
        hit,
        wrap: false,
        graphic: Some((rel, width.max(1), height.max(1))),
    }
}

fn preview_lines(app: &mut App, rel: &str, cols: u16) -> Vec<Line<'static>> {
    let cols = cols.min(crate::preview::MAX_COLS).max(4);
    if let Some(v) = app.preview.get(&(rel.to_string(), cols)) {
        return v.clone();
    }
    let abs = app.session.workspace.join(rel);
    let lines = crate::preview::from_path(&abs, cols, crate::preview::MAX_ROWS);
    app.preview.insert((rel.to_string(), cols), lines.clone());
    lines
}

fn push_image_block(
    app: &mut App,
    out: &mut Vec<ChatLine>,
    rel: &str,
    caption: String,
    cols: u16,
    selected: bool,
) {
    let idx = app.image_hits.len() as u16;
    app.image_hits.push(rel.to_string());
    let hit = Some(Hit::ChatImage(idx));
    if !caption.is_empty() {
        let cap_style = if selected {
            Style::default().fg(Color::Black).bg(ACCENT)
        } else {
            Style::default().fg(DIM)
        };
        out.push(chat_line(
            Line::from(Span::styled(format!("      {caption}"), cap_style)),
            hit,
        ));
    }
    let max_cols = cols.saturating_sub(crate::preview::INDENT);
    if let Some(picker) = app.picker.as_ref().filter(|p| crate::preview::uses_graphics(p)) {
        let abs = app.session.workspace.join(rel);
        let (w, h) = crate::preview::cell_size_for(
            picker,
            &abs,
            max_cols,
            crate::preview::MAX_ROWS,
        );
        out.push(chat_graphic(rel.to_string(), w, h, hit));
        return;
    }
    for line in preview_lines(app, rel, max_cols) {
        let mut padded = vec![Span::raw("      ")];
        padded.extend(line.spans);
        out.push(chat_line_raw(Line::from(padded), hit));
    }
}

fn chat_logical_rows(app: &mut App, cols: u16) -> Vec<ChatLine> {
    app.image_hits.clear();
    let rows = app.rows.clone();
    let sel = app.chat_sel.clone();
    let open_tool = app.open_tool;
    let mut out = Vec::new();
    for (ri, row) in rows.iter().enumerate() {
        let row_sel = matches!(sel, ChatSel::Row(i) if i == ri);
        match row {
            Row::Tools(g) => {
                out.push(chat_line(group_header_line(g), Some(Hit::ToolGroup(ri))));
                if g.expanded {
                    for (ci, call) in g.calls.iter().enumerate() {
                        let selected = open_tool == Some((ri, ci));
                        out.push(chat_line(
                            call_row_line(call, selected),
                            Some(Hit::ToolItem(ri, ci)),
                        ));
                        for line in call_body_lines(call) {
                            out.push(chat_line(line, Some(Hit::ToolItem(ri, ci))));
                        }
                    }
                }
            }
            Row::Think(t) => {
                out.push(chat_line(think_header_line(t), Some(Hit::Think(ri))));
                if t.expanded {
                    for line in think_body_lines(t) {
                        out.push(chat_line(line, Some(Hit::Think(ri))));
                    }
                }
            }
            Row::User(u) => {
                let hit = Some(Hit::ChatRow(ri as u16));
                for mut line in prefixed_text("you   ", USER, &u.text) {
                    if row_sel {
                        line = tint_line(line);
                    }
                    out.push(chat_line(line, hit));
                }
                for img in &u.images {
                    let img_sel = matches!(&sel, ChatSel::Image(p) if p == img);
                    push_image_block(app, &mut out, img, format!("圖片  {img}"), cols, img_sel);
                }
            }
            Row::Picture { path, label } => {
                let hit = Some(Hit::ChatRow(ri as u16));
                let style = if row_sel {
                    Style::default().fg(Color::Black).bg(ACCENT)
                } else {
                    Style::default().fg(DIM)
                };
                out.push(chat_line(
                    Line::from(Span::styled(label.clone(), style)),
                    hit,
                ));
                let img_sel = matches!(&sel, ChatSel::Image(p) if p == path);
                push_image_block(app, &mut out, path, String::new(), cols, img_sel);
            }
            other => {
                let hit = Some(Hit::ChatRow(ri as u16));
                for mut line in row_lines(other) {
                    if row_sel {
                        line = tint_line(line);
                    }
                    out.push(chat_line(line, hit));
                }
            }
        }
    }
    out
}

fn tint_line(line: Line<'static>) -> Line<'static> {
    Line::from(
        line.spans
            .into_iter()
            .map(|s| {
                let mut st = s.style;
                st.bg = Some(Color::Rgb(48, 64, 88));
                Span::styled(s.content, st)
            })
            .collect::<Vec<_>>(),
    )
}

fn group_header_line(g: &ToolGroup) -> Line<'static> {
    let arrow = if g.expanded { "▾" } else { "▸" };
    let n = g.calls.len();
    let mut names = Vec::new();
    for c in &g.calls {
        if !names.iter().any(|n| n == &c.name) {
            names.push(c.name.clone());
        }
    }
    let names = names.join(" · ");
    let status = if let Some(c) = g.calls.iter().rev().find(|c| !c.done) {
        if c.phase.is_empty() {
            "執行中"
        } else {
            c.phase.as_str()
        }
    } else {
        "完成"
    };
    let label = if n <= 1 {
        format!("{arrow} {names}    {status}")
    } else {
        format!("{arrow} {n} 個工具 · {names}    {status}")
    };
    Line::from(Span::styled(label, Style::default().fg(TOOL)))
}

fn think_header_line(t: &Think) -> Line<'static> {
    let arrow = if t.expanded { "▾" } else { "▸" };
    let ms = if t.done {
        t.elapsed_ms
    } else {
        t.started
            .map(|s| s.elapsed().as_millis() as u64)
            .unwrap_or(t.elapsed_ms)
    };
    let clock = if ms > 0 || !t.done {
        format!("  {}", md::fmt_duration(ms))
    } else {
        String::new()
    };
    let status = if t.done { "完成" } else { "進行中" };
    Line::from(Span::styled(
        format!("{arrow} 思考{clock}    {status}"),
        Style::default().fg(THINK),
    ))
}

fn think_body_lines(t: &Think) -> Vec<Line<'static>> {
    const MAX: usize = 120;
    let mut lines = Vec::new();
    for (i, part) in t.text.split('\n').enumerate() {
        if i >= MAX {
            let rest = t.text.lines().count().saturating_sub(MAX);
            lines.push(Line::from(Span::styled(
                format!("  … ({rest} 行省略)"),
                Style::default().fg(DIM),
            )));
            break;
        }
        lines.push(Line::from(Span::styled(
            format!("  {part}"),
            Style::default().fg(DIM),
        )));
    }
    if lines.is_empty() {
        lines.push(Line::from(Span::styled(
            "  ",
            Style::default().fg(DIM),
        )));
    }
    lines
}

fn call_row_line(c: &ToolCall, selected: bool) -> Line<'static> {
    let inner = if c.done {
        tool_finished_line(&c.name, &c.output)
    } else if !c.phase.is_empty() {
        format!("▸ {}  {}", c.name, c.phase)
    } else {
        tool_started_line(&c.name, &c.args)
    };
    let style = if selected {
        Style::default().fg(Color::Black).bg(ACCENT)
    } else {
        Style::default().fg(TOOL)
    };
    Line::from(Span::styled(format!("  {inner}"), style))
}

fn call_body_lines(c: &ToolCall) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    if c.name == "run_command" {
        if let Ok(v) = serde_json::from_str::<Value>(&c.output) {
            if let Some(stdout) = v.get("stdout").and_then(Value::as_str) {
                lines.extend(colored_output_lines(stdout, 60));
            }
            if let Some(stderr) = v.get("stderr").and_then(Value::as_str) {
                if !stderr.trim().is_empty() {
                    lines.extend(colored_output_lines(stderr, 20));
                }
            }
        }
    }
    for f in &c.files {
        lines.push(Line::from(Span::styled(
            format!("    ● {}  {}", f.kind, f.path),
            Style::default().fg(ACCENT),
        )));
        lines.extend(diff_lines(&f.diff));
    }
    lines
}

fn call_detail_lines(c: &ToolCall) -> Vec<Line<'static>> {
    let mut lines = vec![Line::from(Span::styled(
        format!(" {}", c.name),
        Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
    ))];
    match c.name.as_str() {
        "run_command" => {
            let cmd = c.args.get("command").and_then(Value::as_str).unwrap_or("");
            lines.push(Line::from(Span::styled(
                format!(" $ {cmd}"),
                Style::default().fg(TEXT),
            )));
            if let Ok(v) = serde_json::from_str::<Value>(&c.output) {
                if let Some(code) = v.get("exit_code") {
                    lines.push(Line::from(Span::styled(
                        format!(" exit {code}"),
                        Style::default().fg(DIM),
                    )));
                }
                if let Some(stdout) = v.get("stdout").and_then(Value::as_str) {
                    if !stdout.trim().is_empty() {
                        lines.push(Line::from(Span::styled(
                            " stdout",
                            Style::default().fg(DIM),
                        )));
                        lines.extend(colored_output_lines(stdout, 80));
                    }
                }
                if let Some(stderr) = v.get("stderr").and_then(Value::as_str) {
                    if !stderr.trim().is_empty() {
                        lines.push(Line::from(Span::styled(
                            " stderr",
                            Style::default().fg(WARN),
                        )));
                        lines.extend(colored_output_lines(stderr, 40));
                    }
                }
            } else if !c.output.is_empty() {
                lines.extend(colored_output_lines(&c.output, 80));
            }
        }
        "list_dir" => {
            let path = c.args.get("path").and_then(Value::as_str).unwrap_or(".");
            lines.push(Line::from(Span::styled(
                format!(" {path}"),
                Style::default().fg(DIM),
            )));
            if let Ok(v) = serde_json::from_str::<Value>(&c.output) {
                if let Some(entries) = v.get("entries").and_then(Value::as_array) {
                    for e in entries.iter().take(80) {
                        let name = e.get("name").and_then(Value::as_str).unwrap_or("");
                        let dir = e.get("dir").and_then(Value::as_bool).unwrap_or(false);
                        let mark = if dir { "/" } else { "" };
                        lines.push(Line::from(Span::styled(
                            format!("  {name}{mark}"),
                            Style::default().fg(TEXT),
                        )));
                    }
                }
            }
        }
        _ => {
            if let Some(path) = c.args.get("path").and_then(Value::as_str) {
                lines.push(Line::from(Span::styled(
                    format!(" {path}"),
                    Style::default().fg(DIM),
                )));
            } else if let Some(q) = c.args.get("query").and_then(Value::as_str) {
                lines.push(Line::from(Span::styled(
                    format!(" {q}"),
                    Style::default().fg(DIM),
                )));
            }
            if !c.output.is_empty() {
                for l in c.output.lines().take(30) {
                    lines.push(Line::from(Span::styled(
                        format!(" {l}"),
                        Style::default().fg(TEXT),
                    )));
                }
            }
        }
    }
    for f in &c.files {
        lines.push(Line::from(Span::styled(
            format!(" ● {}  {}", f.kind, f.path),
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
        )));
        lines.extend(diff_lines(&f.diff));
    }
    lines
}

fn wrap_visual(line: Line<'static>, width: u16) -> Vec<Line<'static>> {
    let width = width.max(1);
    let mut cells: Vec<(char, Style)> = Vec::new();
    for span in line.spans {
        let style = span.style;
        for c in span.content.chars() {
            if c == '\t' {
                for _ in 0..4 {
                    cells.push((' ', style));
                }
            } else {
                cells.push((c, style));
            }
        }
    }
    if cells.is_empty() {
        return vec![Line::from("")];
    }
    let mut out = Vec::new();
    let mut start = 0usize;
    let mut col = 0u16;
    let mut i = 0usize;
    while i < cells.len() {
        let ch = cells[i].0;
        if ch == '\r' {
            i += 1;
            continue;
        }
        if ch == '\n' {
            out.push(line_from_cells(&cells[start..i]));
            start = i + 1;
            col = 0;
            i += 1;
            continue;
        }
        let w = ch_width(ch).max(1);
        if col > 0 && col.saturating_add(w) > width {
            out.push(line_from_cells(&cells[start..i]));
            start = i;
            col = 0;
            continue;
        }
        col = col.saturating_add(w);
        i += 1;
    }
    if start < cells.len() || out.is_empty() {
        out.push(line_from_cells(&cells[start..]));
    }
    out
}

fn line_from_cells(cells: &[(char, Style)]) -> Line<'static> {
    if cells.is_empty() {
        return Line::from("");
    }
    let mut spans = Vec::new();
    let mut buf = String::new();
    let mut style = cells[0].1;
    for &(c, st) in cells {
        if st != style && !buf.is_empty() {
            spans.push(Span::styled(std::mem::take(&mut buf), style));
        }
        buf.push(c);
        style = st;
    }
    if !buf.is_empty() {
        spans.push(Span::styled(buf, style));
    }
    Line::from(spans)
}

fn row_lines(row: &Row) -> Vec<Line<'static>> {
    match row {
        Row::User(u) => prefixed_text("you   ", USER, &u.text),
        Row::Agent(a) => agent_lines(a),
        Row::Tools(g) => {
            let mut lines = vec![group_header_line(g)];
            if g.expanded {
                for c in &g.calls {
                    lines.push(call_row_line(c, false));
                }
            }
            lines
        }
        Row::Think(t) => {
            let mut lines = vec![think_header_line(t)];
            if t.expanded {
                lines.extend(think_body_lines(t));
            }
            lines
        }
        Row::Meta(s) => vec![Line::from(Span::styled(
            s.clone(),
            Style::default().fg(DIM),
        ))],
        Row::Err(s) => vec![Line::from(Span::styled(
            s.clone(),
            Style::default().fg(WARN),
        ))],
        Row::Picture { label, .. } => vec![Line::from(Span::styled(
            label.clone(),
            Style::default().fg(DIM),
        ))],
    }
}

fn prefixed_text(prefix: &'static str, color: Color, s: &str) -> Vec<Line<'static>> {
    let mut out = Vec::new();
    for (i, part) in s.split('\n').enumerate() {
        if i == 0 {
            out.push(Line::from(vec![
                Span::styled(
                    prefix,
                    Style::default().fg(color).add_modifier(Modifier::BOLD),
                ),
                Span::styled(part.to_string(), Style::default().fg(TEXT)),
            ]));
        } else {
            out.push(Line::from(vec![
                Span::raw("      "),
                Span::styled(part.to_string(), Style::default().fg(TEXT)),
            ]));
        }
    }
    if out.is_empty() {
        out.push(Line::from(Span::styled(
            prefix,
            Style::default().fg(color).add_modifier(Modifier::BOLD),
        )));
    }
    out
}

fn agent_lines(a: &AgentMsg) -> Vec<Line<'static>> {
    let md_lines = md::markdown_lines(&a.text);
    let mut out = Vec::new();
    for (i, line) in md_lines.into_iter().enumerate() {
        let mut spans = Vec::new();
        if i == 0 {
            spans.push(Span::styled(
                "grok  ",
                Style::default().fg(AGENT).add_modifier(Modifier::BOLD),
            ));
        } else {
            spans.push(Span::raw("      "));
        }
        spans.extend(line.spans);
        out.push(Line::from(spans));
    }
    if out.is_empty() {
        out.push(Line::from(Span::styled(
            "grok  ",
            Style::default().fg(AGENT).add_modifier(Modifier::BOLD),
        )));
    }
    if a.work_ms > 0 {
        out.push(Line::from(Span::styled(
            format!("      工作 {}", md::fmt_duration(a.work_ms)),
            Style::default().fg(DIM),
        )));
    }
    out
}

fn diff_lines(diff: &str) -> Vec<Line<'static>> {
    colored_output_lines(diff, 120)
}

fn colored_output_lines(text: &str, max: usize) -> Vec<Line<'static>> {
    if text.is_empty() {
        return Vec::new();
    }
    let total = text.lines().count();
    let mut lines: Vec<Line<'static>> = text
        .lines()
        .take(max)
        .map(|l| {
            let l = l.trim_end_matches('\r');
            if l.contains('\u{1b}') {
                prefix_line("  ", ansi_line(l))
            } else {
                Line::from(Span::styled(
                    format!("  {l}"),
                    output_line_style(l),
                ))
            }
        })
        .collect();
    if total > max {
        lines.push(Line::from(Span::styled(
            format!("  … ({} 行省略)", total - max),
            Style::default().fg(DIM),
        )));
    }
    lines
}

fn prefix_line(prefix: &'static str, line: Line<'static>) -> Line<'static> {
    let mut spans = vec![Span::raw(prefix)];
    spans.extend(line.spans);
    Line::from(spans)
}

fn output_line_style(l: &str) -> Style {
    let t = l.trim_start();
    if t.starts_with("+++")
        || t.starts_with("---")
        || t.starts_with("diff ")
        || t.starts_with("index ")
    {
        Style::default().fg(DIM)
    } else if t.starts_with("@@") {
        Style::default().fg(DIFF_HUNK)
    } else if t.starts_with('+')
        || t.starts_with("new file:")
        || t.contains("new file:")
        || t.starts_with("??")
    {
        Style::default().fg(DIFF_ADD)
    } else if t.starts_with('-') || t.contains("deleted:") || t.starts_with("D ") {
        Style::default().fg(DIFF_DEL)
    } else if t.contains("modified:") || t.starts_with("M ") || t.starts_with("MM") {
        Style::default().fg(DIFF_HUNK)
    } else if t.starts_with('$') {
        Style::default().fg(ACCENT)
    } else {
        Style::default().fg(TEXT)
    }
}

fn ansi_line(s: &str) -> Line<'static> {
    let mut spans = Vec::new();
    let mut buf = String::new();
    let mut style = Style::default().fg(TEXT);
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' && chars.peek() == Some(&'[') {
            chars.next();
            let mut code = String::new();
            while let Some(&d) = chars.peek() {
                chars.next();
                if d.is_ascii_alphabetic() {
                    if d == 'm' && !buf.is_empty() {
                        spans.push(Span::styled(std::mem::take(&mut buf), style));
                    }
                    if d == 'm' {
                        style = apply_sgr(&code, style);
                    }
                    break;
                }
                code.push(d);
            }
            continue;
        }
        if c != '\r' {
            buf.push(c);
        }
    }
    if !buf.is_empty() {
        spans.push(Span::styled(buf, style));
    }
    if spans.is_empty() {
        Line::from("")
    } else {
        Line::from(spans)
    }
}

fn apply_sgr(code: &str, mut style: Style) -> Style {
    if code.is_empty() {
        return Style::default().fg(TEXT);
    }
    for part in code.split(';') {
        match part {
            "" | "0" => style = Style::default().fg(TEXT),
            "1" => style = style.add_modifier(Modifier::BOLD),
            "2" | "90" => style = style.fg(DIM),
            "31" | "91" => style = style.fg(DIFF_DEL),
            "32" | "92" => style = style.fg(DIFF_ADD),
            "33" | "93" => style = style.fg(TOOL),
            "34" | "36" | "94" | "96" => style = style.fg(DIFF_HUNK),
            "35" | "95" => style = style.fg(THINK),
            "39" => style = style.fg(TEXT),
            _ => {}
        }
    }
    style
}

fn sync_knobs(knobs: &Mutex<SessionKnobs>, opts: &TuiOptions, catalog: &ModelCatalog) {
    let model = if opts.model.trim().is_empty() {
        catalog
            .models
            .first()
            .map(|m| m.id.clone())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "grok-4.6".into())
    } else {
        opts.model.clone()
    };
    let choice = clamp_effort_for_model(catalog, &model, opts.reasoning_effort);
    if let Ok(mut k) = knobs.lock() {
        k.model = model;
        k.reasoning_effort = choice.effort;
        k.send_reasoning = choice.send_reasoning;
        k.server_tools = kit::search_tools(opts.web_search);
    }
}

fn model_choices(app: &App, opts: &TuiOptions) -> Vec<(String, String)> {
    if app.catalog.models.is_empty() {
        let id = opts.model.trim();
        if id.is_empty() {
            return Vec::new();
        }
        return vec![(id.to_string(), id.to_string())];
    }
    app.catalog
        .models
        .iter()
        .map(|m| (m.id.clone(), m.name.clone()))
        .collect()
}

fn effort_choices(app: &App, opts: &TuiOptions) -> Vec<EffortOpt> {
    let listed = app.catalog.picker_efforts(&opts.model);
    if listed.is_empty() && app.catalog.find(&opts.model).is_none() {
        return vec![EffortOpt {
            id: opts.reasoning_effort.as_str().to_string(),
            value: opts.reasoning_effort,
            label: opts.reasoning_effort.label().to_string(),
            default: true,
        }];
    }
    listed
}

fn apply_selected_model(app: &mut App, opts: &mut TuiOptions, model_id: String) {
    opts.model = model_id;
    let choice = clamp_effort_for_model(&app.catalog, &opts.model, opts.reasoning_effort);
    opts.reasoning_effort = choice.effort;
    sync_knobs(&app.knobs, opts, &app.catalog);
}

fn apply_selected_effort(app: &App, opts: &mut TuiOptions, effort: ReasoningEffort) {
    opts.reasoning_effort = effort;
    sync_knobs(&app.knobs, opts, &app.catalog);
}

fn open_drop(app: &mut App, opts: &TuiOptions, kind: DropKind) {
    let len = match kind {
        DropKind::Model => model_choices(app, opts).len(),
        DropKind::Effort => effort_choices(app, opts).len(),
    };
    if len == 0 {
        app.drop = None;
        return;
    }
    app.drop = Some(kind);
    app.drop_cursor = match kind {
        DropKind::Model => model_choices(app, opts)
            .iter()
            .position(|(id, _)| id == &opts.model)
            .unwrap_or(0),
        DropKind::Effort => effort_choices(app, opts)
            .iter()
            .position(|e| e.value == opts.reasoning_effort)
            .unwrap_or(0),
    };
    app.drop_scroll = 0;
    reveal_drop_cursor(app, len);
}

fn reveal_drop_cursor(app: &mut App, len: usize) {
    let vis = DROP_VISIBLE.min(len.max(1));
    if app.drop_cursor < app.drop_scroll as usize {
        app.drop_scroll = app.drop_cursor as u16;
    } else if app.drop_cursor >= app.drop_scroll as usize + vis {
        app.drop_scroll = (app.drop_cursor + 1 - vis) as u16;
    }
}

fn select_catalog_pick(app: &mut App, opts: &mut TuiOptions, index: usize) {
    match app.drop {
        Some(DropKind::Model) => {
            if let Some((id, _)) = model_choices(app, opts).get(index).cloned() {
                apply_selected_model(app, opts, id);
            }
        }
        Some(DropKind::Effort) => {
            if let Some(e) = effort_choices(app, opts).get(index).cloned() {
                apply_selected_effort(app, opts, e.value);
            }
        }
        None => {}
    }
    app.drop = None;
}

fn ingest_catalog(app: &mut App, opts: &mut TuiOptions, result: crate::error::Result<ModelCatalog>) {
    match result {
        Ok(mut cat) => {
            cat.ensure_current(&opts.model, opts.reasoning_effort);
            app.catalog = cat;
            app.catalog_status = CatalogStatus::Ready;
            let choice = clamp_effort_for_model(&app.catalog, &opts.model, opts.reasoning_effort);
            opts.reasoning_effort = choice.effort;
            sync_knobs(&app.knobs, opts, &app.catalog);
        }
        Err(e) => {
            app.catalog.ensure_current(&opts.model, opts.reasoning_effort);
            let msg = e.to_string();
            app.catalog_status = CatalogStatus::Failed(msg.chars().take(80).collect());
        }
    }
}

fn login_in_flight(ui: &LoginUi) -> bool {
    matches!(ui, LoginUi::Starting | LoginUi::Waiting { .. })
}

fn begin_login(app: &mut App) {
    if app.logged_in || login_in_flight(&app.login_ui) {
        return;
    }
    if auth::load_tokens(&app.auth_path).is_ok() {
        apply_login_success(app);
        return;
    }
    app.login_gen = app.login_gen.wrapping_add(1);
    app.login_ui = LoginUi::Starting;
    app.want_login = true;
    app.status = "正在開始 Grok 登入…".into();
}

fn cancel_login(app: &mut App) {
    if !login_in_flight(&app.login_ui) && !app.want_login {
        return;
    }
    app.login_gen = app.login_gen.wrapping_add(1);
    app.want_login = false;
    app.login_ui = LoginUi::Idle;
    app.status = "已取消登入".into();
}

fn logout_account(app: &mut App) {
    app.login_gen = app.login_gen.wrapping_add(1);
    app.want_login = false;
    app.login_ui = LoginUi::Idle;
    if let Err(e) = auth::delete_auth_file(&app.auth_path) {
        app.login_ui = LoginUi::Failed(e.to_string());
        return;
    }
    app.logged_in = false;
    app.want_catalog = false;
    app.status = "已登出".into();
}

fn apply_login_success(app: &mut App) {
    app.logged_in = true;
    app.login_ui = LoginUi::Idle;
    app.want_login = false;
    app.want_catalog = true;
    app.status = "已登入 Grok".into();
}

fn apply_login_event(app: &mut App, ev: LoginEvent) {
    let gen = match &ev {
        LoginEvent::Waiting { gen, .. }
        | LoginEvent::Success { gen }
        | LoginEvent::Failed { gen, .. } => *gen,
    };
    if gen != app.login_gen {
        return;
    }
    match ev {
        LoginEvent::Waiting { url, user_code, .. } => {
            app.login_ui = LoginUi::Waiting { url, user_code };
            app.status = "請在瀏覽器核准 Grok 登入".into();
        }
        LoginEvent::Success { .. } => {
            apply_login_success(app);
            app.push(Row::Meta("已登入 Grok 帳號".into()));
        }
        LoginEvent::Failed { message, .. } => {
            app.login_ui = LoginUi::Failed(message.chars().take(80).collect());
            app.status = "Grok 登入失敗".into();
        }
    }
}

fn activate_account(app: &mut App) {
    if login_in_flight(&app.login_ui) {
        cancel_login(app);
    } else if app.logged_in {
        logout_account(app);
    } else {
        begin_login(app);
    }
}

async fn run_settings_login(
    path: PathBuf,
    gen: u64,
    tx: mpsc::UnboundedSender<LoginEvent>,
) {
    let mut pending = match auth::request_device_login().await {
        Ok(p) => p,
        Err(e) => {
            let _ = tx.send(LoginEvent::Failed {
                gen,
                message: e.to_string(),
            });
            return;
        }
    };
    auth::open_login_browser(&pending.open_url);
    let _ = tx.send(LoginEvent::Waiting {
        gen,
        url: pending.open_url.clone(),
        user_code: pending.user_code.clone(),
    });
    let deadline = std::time::SystemTime::now() + Duration::from_secs(pending.expires_in);
    loop {
        if std::time::SystemTime::now() >= deadline {
            let _ = tx.send(LoginEvent::Failed {
                gen,
                message: "登入已逾時".into(),
            });
            return;
        }
        tokio::time::sleep(pending.interval()).await;
        match auth::poll_device_login(&pending).await {
            Ok(auth::DevicePoll::Pending) => continue,
            Ok(auth::DevicePoll::SlowDown) => pending.bump_interval(),
            Ok(auth::DevicePoll::Success(tokens)) => {
                if let Err(e) = auth::save_tokens(&path, &tokens) {
                    let _ = tx.send(LoginEvent::Failed {
                        gen,
                        message: e.to_string(),
                    });
                    return;
                }
                let _ = tx.send(LoginEvent::Success { gen });
                return;
            }
            Ok(auth::DevicePoll::Denied) => {
                let _ = tx.send(LoginEvent::Failed {
                    gen,
                    message: "瀏覽器拒絕登入".into(),
                });
                return;
            }
            Ok(auth::DevicePoll::Expired) => {
                let _ = tx.send(LoginEvent::Failed {
                    gen,
                    message: "登入代碼已過期".into(),
                });
                return;
            }
            Ok(auth::DevicePoll::Failed(message)) => {
                let _ = tx.send(LoginEvent::Failed { gen, message });
                return;
            }
            Err(e) => {
                let _ = tx.send(LoginEvent::Failed {
                    gen,
                    message: e.to_string(),
                });
                return;
            }
        }
    }
}

fn open_settings(app: &mut App) {
    app.drop = None;
    if app.logged_in
        && !matches!(
            app.catalog_status,
            CatalogStatus::Ready | CatalogStatus::Loading
        )
    {
        app.want_catalog = true;
    }
    if let Some(w) = app.settings.as_mut() {
        w.minimized = false;
        app.focus = Focus::Settings;
        return;
    }
    let area = app.area;
    let w = 56u16.min(area.width.saturating_sub(4)).max(28);
    let h = 22u16.min(area.height.saturating_sub(4)).max(14);
    let x = area.x + area.width.saturating_sub(w) / 2;
    let y = area.y + 2;
    app.settings = Some(Win {
        x,
        y,
        w,
        h,
        maximized: false,
        minimized: false,
    });
    app.focus = Focus::Settings;
    app.setting_field = if app.logged_in {
        SettingField::Model
    } else {
        SettingField::Account
    };
}

fn win_rect(w: &Win, area: Rect) -> Rect {
    if w.maximized {
        return Rect::new(
            area.x,
            area.y.saturating_add(1),
            area.width,
            area.height.saturating_sub(2),
        );
    }
    let x = w.x.min(area.width.saturating_sub(4));
    let y = w.y.min(area.height.saturating_sub(3));
    Rect::new(x, y, w.w.min(area.width.saturating_sub(x)), w.h.min(area.height.saturating_sub(y)))
}

fn draw(f: &mut Frame, app: &mut App, opts: &TuiOptions) -> Position {
    app.hits.clear();
    app.area = f.area();
    f.render_widget(Block::default().style(Style::default().bg(BG).fg(TEXT)), f.area());
    app.refresh_session_list();

    let sidebar_w = if f.area().width >= SIDEBAR_MIN_TERM {
        SIDEBAR_W
    } else {
        0
    };
    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Length(sidebar_w), Constraint::Min(20)])
        .split(f.area());
    let mut rename_caret = None;
    if sidebar_w > 0 {
        rename_caret = draw_sidebar(f, app, cols[0]);
    }
    let rest = cols[1];
    let rail_w = if app.has_side() && f.area().width >= RAIL_MIN_TERM {
        RAIL_W.min(rest.width.saturating_sub(24))
    } else {
        0
    };
    let body = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Min(20), Constraint::Length(rail_w)])
        .split(rest);
    let main = body[0];

    let qn = app.queue.len().min(5);
    let attach = if app.pending.is_empty() { 0 } else { 1 };
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Min(6),
            Constraint::Length(composer_height(qn, attach)),
        ])
        .split(main);

    draw_header(f, app, opts, chunks[0]);
    let tool_hits = draw_chat(f, app, chunks[1]);
    draw_tool_panel(f, app, chunks[1], &tool_hits);
    let composer_caret = draw_composer(f, app, opts, chunks[2]);
    let mut caret = composer_caret;
    if app.focus == Focus::Rename {
        if let Some(pos) = rename_caret {
            caret = pos;
        }
    }
    let settings_visible = app.settings.as_ref().is_some_and(|s| !s.minimized);
    let settings_min = app.settings.as_ref().is_some_and(|s| s.minimized);
    if settings_min {
        let dock = Rect::new(chunks[0].x.saturating_add(18), chunks[0].y, 12, 1);
        f.render_widget(
            Paragraph::new(Span::styled(" 設定 ", Style::default().bg(COMPOSER).fg(ACCENT))),
            dock,
        );
        app.hits.push((dock, Hit::Dock));
    }
    if settings_visible {
        if let Some(pos) = draw_settings(f, app, opts) {
            if app.focus == Focus::Settings {
                caret = pos;
            }
        }
    }
    if rail_w > 0 {
        draw_rail(f, app, body[1]);
    }
    if app.inspector.is_some() {
        draw_inspector(f, app, f.area());
    }
    if app.ask.is_some() {
        if let Some(pos) = draw_ask(f, app) {
            caret = pos;
        }
    }
    if app.workspace_pick.is_some() {
        if let Some(pos) = draw_workspace_pick(f, app) {
            caret = pos;
        }
    }
    caret
}

fn draw_rail(f: &mut Frame, app: &mut App, area: Rect) {
    f.render_widget(
        Block::default()
            .borders(Borders::LEFT)
            .border_style(Style::default().fg(BORDER))
            .style(Style::default().bg(PANEL).fg(TEXT)),
        area,
    );
    if area.width < 8 || area.height < 3 {
        return;
    }
    let inner = Rect::new(
        area.x.saturating_add(1),
        area.y,
        area.width.saturating_sub(1),
        area.height,
    );
    f.render_widget(
        Paragraph::new(Span::styled(
            " 代理 / 監控 / 後台",
            Style::default().fg(DIM).add_modifier(Modifier::BOLD),
        )),
        Rect::new(inner.x, inner.y, inner.width, 1),
    );
    let mut y = inner.y.saturating_add(1);
    for (i, c) in app.children.iter().enumerate() {
        if y >= inner.y + inner.height {
            break;
        }
        let open = matches!(&app.inspector, Some(Inspector::Child(n)) if n == &c.name);
        let mark = if c.alive {
            spinner(app.tick).to_string()
        } else {
            "·".into()
        };
        let label = format!(" {mark} {name}", name = c.name);
        let style = if open {
            Style::default().bg(ACCENT).fg(Color::Black)
        } else if c.alive {
            Style::default().fg(AGENT)
        } else {
            Style::default().fg(DIM)
        };
        let row = Rect::new(inner.x, y, inner.width, 1);
        f.render_widget(
            Paragraph::new(Span::styled(truncate_width(&label, inner.width), style)),
            row,
        );
        app.hits.push((row, Hit::RailChild(i as u16)));
        y = y.saturating_add(1);
        if y >= inner.y + inner.height {
            break;
        }
        let sub = if c.activity.is_empty() {
            c.status.clone()
        } else {
            format!("{} · {}", c.status, c.activity)
        };
        f.render_widget(
            Paragraph::new(Span::styled(
                truncate_width(&format!("   {sub}"), inner.width),
                Style::default().fg(DIM),
            )),
            Rect::new(inner.x, y, inner.width, 1),
        );
        y = y.saturating_add(1);
    }
    if !app.children.is_empty() && !app.monitors.is_empty() {
        y = y.saturating_add(1);
    }
    for (i, m) in app.monitors.iter().enumerate() {
        if y >= inner.y + inner.height {
            break;
        }
        let open = matches!(&app.inspector, Some(Inspector::Monitor(n)) if n == &m.name);
        let mark = if m.alive { "◉" } else { "○" };
        let label = format!(" {mark} {name}", name = m.name);
        let style = if open {
            Style::default().bg(ACCENT).fg(Color::Black)
        } else if m.alive {
            Style::default().fg(TOOL)
        } else {
            Style::default().fg(DIM)
        };
        let row = Rect::new(inner.x, y, inner.width, 1);
        f.render_widget(
            Paragraph::new(Span::styled(truncate_width(&label, inner.width), style)),
            row,
        );
        app.hits.push((row, Hit::RailMon(i as u16)));
        y = y.saturating_add(1);
        if y >= inner.y + inner.height {
            break;
        }
        f.render_widget(
            Paragraph::new(Span::styled(
                truncate_width(&format!("   {}", m.status), inner.width),
                Style::default().fg(DIM),
            )),
            Rect::new(inner.x, y, inner.width, 1),
        );
        y = y.saturating_add(1);
    }
    if (!app.children.is_empty() || !app.monitors.is_empty()) && !app.backgrounds.is_empty() {
        y = y.saturating_add(1);
    }
    for (i, b) in app.backgrounds.iter().enumerate() {
        if y >= inner.y + inner.height {
            break;
        }
        let open = matches!(&app.inspector, Some(Inspector::Background(n)) if n == &b.name);
        let mark = if b.alive {
            spinner(app.tick).to_string()
        } else {
            "·".into()
        };
        let label = format!(" {mark} {name}", name = b.name);
        let style = if open {
            Style::default().bg(ACCENT).fg(Color::Black)
        } else if b.alive {
            Style::default().fg(USER)
        } else {
            Style::default().fg(DIM)
        };
        let row = Rect::new(inner.x, y, inner.width, 1);
        f.render_widget(
            Paragraph::new(Span::styled(truncate_width(&label, inner.width), style)),
            row,
        );
        app.hits.push((row, Hit::RailBg(i as u16)));
        y = y.saturating_add(1);
        if y >= inner.y + inner.height {
            break;
        }
        f.render_widget(
            Paragraph::new(Span::styled(
                truncate_width(&format!("   {}", b.status), inner.width),
                Style::default().fg(DIM),
            )),
            Rect::new(inner.x, y, inner.width, 1),
        );
        y = y.saturating_add(1);
    }
}

fn inspector_rect(area: Rect) -> Rect {
    let max_w = area.width.saturating_sub(4).max(1);
    let max_h = area.height.saturating_sub(3).max(1);
    let w = (area.width.saturating_mul(3) / 4).min(max_w).max(36.min(max_w));
    let h = (area.height.saturating_mul(4) / 5).min(max_h).max(10.min(max_h));
    let x = area.x + area.width.saturating_sub(w) / 2;
    let y = area.y + area.height.saturating_sub(h) / 2;
    Rect::new(x, y, w, h)
}

fn draw_inspector(f: &mut Frame, app: &mut App, area: Rect) {
    let Some(kind) = app.inspector.clone() else {
        return;
    };
    let panel = inspector_rect(area);
    f.render_widget(Clear, panel);
    let title = match &kind {
        Inspector::Child(n) => format!(" 子代理 {n} "),
        Inspector::Monitor(n) => format!(" 監控 {n} "),
        Inspector::Background(n) => format!(" 後台 {n} "),
    };
    f.render_widget(
        Block::default()
            .title(title)
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(ACCENT))
            .style(Style::default().bg(PANEL).fg(TEXT)),
        panel,
    );
    let close = Rect::new(
        panel.x + panel.width.saturating_sub(4),
        panel.y,
        3,
        1,
    );
    f.render_widget(
        Paragraph::new(Span::styled(" × ", Style::default().fg(WARN))),
        close,
    );
    app.hits.push((panel, Hit::Inspector));
    app.hits.push((close, Hit::InspectorClose));

    let inner = Rect::new(
        panel.x.saturating_add(2),
        panel.y.saturating_add(1),
        panel.width.saturating_sub(4),
        panel.height.saturating_sub(2),
    );
    let mut lines: Vec<Line<'static>> = Vec::new();
    match kind {
        Inspector::Child(name) => {
            let Some(c) = app.children.iter().find(|c| c.name == name) else {
                return;
            };
            let spin = if c.alive {
                spinner(app.tick).to_string()
            } else {
                "·".into()
            };
            lines.push(Line::from(Span::styled(
                format!("{spin} {}  {}", c.status, c.activity),
                Style::default().fg(if c.alive { AGENT } else { DIM }),
            )));
            if !c.prompt.is_empty() {
                lines.push(Line::from(Span::styled(
                    format!("任務  {}", c.prompt.replace('\n', " ")),
                    Style::default().fg(TEXT),
                )));
            }
            if !c.card_url.is_empty() {
                lines.push(Line::from(Span::styled(
                    format!("card  {}", c.card_url),
                    Style::default().fg(DIM),
                )));
            }
            if !c.messages.is_empty() {
                lines.push(Line::from(Span::styled(
                    "── 訊息 ──",
                    Style::default().fg(DIM),
                )));
                for m in &c.messages {
                    lines.push(Line::from(Span::styled(
                        format!("{} → {}", m.from, m.to),
                        Style::default().fg(ACCENT),
                    )));
                    for part in m.text.split('\n') {
                        lines.push(Line::from(Span::styled(
                            format!("  {part}"),
                            Style::default().fg(TEXT),
                        )));
                    }
                }
            }
            if !c.log.is_empty() {
                lines.push(Line::from(Span::styled(
                    "── 工作 ──",
                    Style::default().fg(DIM),
                )));
                for row in &c.log {
                    let style = if row.starts_with("思考 ") {
                        Style::default().fg(THINK)
                    } else if row.starts_with("grok ") {
                        Style::default().fg(AGENT)
                    } else if row.starts_with("錯誤 ") {
                        Style::default().fg(WARN)
                    } else {
                        Style::default().fg(TOOL)
                    };
                    lines.push(Line::from(Span::styled(row.clone(), style)));
                }
            }
            if c.messages.is_empty() && c.log.is_empty() {
                lines.push(Line::from(Span::styled(
                    "尚無工作紀錄（echo 子代理不會寫事件）",
                    Style::default().fg(DIM),
                )));
            }
        }
        Inspector::Monitor(name) => {
            let Some(m) = app.monitors.iter().find(|m| m.name == name) else {
                return;
            };
            lines.push(Line::from(Span::styled(
                format!(
                    "{}  {}",
                    if m.alive { "執行中" } else { "已結束" },
                    m.status
                ),
                Style::default().fg(if m.alive { TOOL } else { DIM }),
            )));
            lines.push(Line::from(Span::styled(
                format!("PID   {}", m.pid),
                Style::default().fg(TEXT),
            )));
            lines.push(Line::from(Span::styled(
                format!("指令  {}", m.command),
                Style::default().fg(TEXT),
            )));
            lines.push(Line::from(Span::styled(
                "stdin 本 run 的 JSONL 事件流",
                Style::default().fg(DIM),
            )));
            if !m.detail.is_empty() {
                lines.push(Line::from(Span::styled(
                    format!("結束  {}", m.detail),
                    Style::default().fg(DIM),
                )));
            }
        }
        Inspector::Background(name) => {
            let Some(b) = app.backgrounds.iter().find(|b| b.name == name) else {
                return;
            };
            lines.push(Line::from(Span::styled(
                format!(
                    "{}  {}",
                    if b.alive { "執行中" } else { "已結束" },
                    b.status
                ),
                Style::default().fg(if b.alive { USER } else { DIM }),
            )));
            lines.push(Line::from(Span::styled(
                format!("PID   {}", b.pid),
                Style::default().fg(TEXT),
            )));
            lines.push(Line::from(Span::styled(
                format!("指令  {}", b.command),
                Style::default().fg(TEXT),
            )));
            if !b.detail.is_empty() {
                lines.push(Line::from(Span::styled(
                    format!("結束  {}", b.detail),
                    Style::default().fg(DIM),
                )));
            }
            if b.log.is_empty() {
                lines.push(Line::from(Span::styled(
                    "尚無輸出",
                    Style::default().fg(DIM),
                )));
            } else {
                lines.push(Line::from(Span::styled(
                    "── stdout / stderr ──",
                    Style::default().fg(DIM),
                )));
                for row in &b.log {
                    let style = if row.starts_with("err ") {
                        Style::default().fg(WARN)
                    } else {
                        Style::default().fg(TEXT)
                    };
                    lines.push(Line::from(Span::styled(row.clone(), style)));
                }
            }
        }
    }

    let mut wrapped: Vec<Line<'static>> = Vec::new();
    for line in lines {
        wrapped.extend(wrap_visual(line, inner.width.max(1)));
    }
    let max_off = wrapped.len().saturating_sub(inner.height as usize) as u16;
    if app.inspector_scroll > max_off {
        app.inspector_scroll = max_off;
    }
    let start = app.inspector_scroll as usize;
    let end = (start + inner.height as usize).min(wrapped.len());
    let vis = if start < wrapped.len() {
        wrapped[start..end].to_vec()
    } else {
        Vec::new()
    };
    f.render_widget(Paragraph::new(vis), inner);
}

fn draw_sidebar(f: &mut Frame, app: &mut App, area: Rect) -> Option<Position> {
    f.render_widget(
        Block::default()
            .borders(Borders::RIGHT)
            .border_style(Style::default().fg(BORDER))
            .style(Style::default().bg(PANEL).fg(TEXT)),
        area,
    );
    if area.width < 8 || area.height < 3 {
        return None;
    }
    let inner = Rect::new(
        area.x,
        area.y,
        area.width.saturating_sub(1),
        area.height,
    );
    let new_btn = Rect::new(inner.x, inner.y, inner.width, 1);
    f.render_widget(
        Paragraph::new(Span::styled(
            " + 新對話",
            Style::default()
                .fg(ACCENT)
                .bg(PANEL)
                .add_modifier(Modifier::BOLD),
        )),
        new_btn,
    );
    app.hits.push((new_btn, Hit::NewChat));

    let list_y = inner.y.saturating_add(2);
    if inner.height.saturating_sub(2) == 0 {
        return None;
    }
    let btn_w = 6u16;
    let mut rename_pos = None;
    let mut y = list_y;
    let sessions = app.sessions.clone();
    for (i, meta) in sessions.iter().enumerate() {
        if y + 1 >= inner.y + inner.height {
            break;
        }
        let selected = meta.id == app.current_id;
        let renaming = app.rename.as_ref().is_some_and(|(id, _)| id == &meta.id);
        let running = if selected {
            app.running
        } else {
            app.parked.get(&meta.id).is_some_and(|p| p.running)
        };
        let bg = if selected || renaming { COMPOSER } else { PANEL };
        let row1 = Rect::new(inner.x, y, inner.width, 1);
        let row2 = Rect::new(inner.x, y + 1, inner.width, 1);
        let text_w = inner.width.saturating_sub(btn_w);
        let hit = Rect::new(inner.x, y, inner.width, 2);
        app.hits.push((hit, Hit::Session(i as u16)));

        if renaming {
            let edit_area = Rect::new(inner.x.saturating_add(1), y, text_w.saturating_sub(1).max(1), 1);
            app.rename_inner = edit_area;
            f.render_widget(
                Block::default().style(Style::default().bg(bg)),
                row1,
            );
            if let Some((_, edit)) = app.rename.as_ref() {
                let mut vs = 0u16;
                rename_pos = Some(draw_edit(f, edit_area, edit, &mut vs));
            }
        } else {
            let mark = if running { "● " } else { "  " };
            let title = truncate_width(&format!("{mark}{}", meta.name), text_w.max(1));
            f.render_widget(
                Paragraph::new(Span::styled(
                    title,
                    Style::default().fg(TEXT).bg(bg).add_modifier(if selected {
                        Modifier::BOLD
                    } else {
                        Modifier::empty()
                    }),
                )),
                Rect::new(inner.x, y, text_w.max(1), 1),
            );
        }

        let edit_btn = Rect::new(inner.x + text_w, y, 3, 1);
        let del_btn = Rect::new(inner.x + text_w + 3, y, 3, 1);
        if inner.width >= btn_w {
            f.render_widget(
                Paragraph::new(Span::styled(" ✎ ", Style::default().fg(DIM).bg(bg))),
                edit_btn,
            );
            f.render_widget(
                Paragraph::new(Span::styled(" × ", Style::default().fg(WARN).bg(bg))),
                del_btn,
            );
            app.hits.push((edit_btn, Hit::RenameSession(i as u16)));
            app.hits.push((del_btn, Hit::DeleteSession(i as u16)));
        }

        let sub = truncate_width(
            &format!("  {}  {}", meta.folder_label(), meta.short_id()),
            inner.width,
        );
        f.render_widget(
            Paragraph::new(Span::styled(sub, Style::default().fg(DIM).bg(bg))),
            row2,
        );
        y = y.saturating_add(3);
    }
    rename_pos
}

fn truncate_width(s: &str, cols: u16) -> String {
    let max = cols as usize;
    if max == 0 {
        return String::new();
    }
    let mut out = String::new();
    let mut w = 0usize;
    for c in s.chars() {
        let cw = ch_width(c) as usize;
        if w + cw > max {
            break;
        }
        out.push(c);
        w += cw;
    }
    while w < max {
        out.push(' ');
        w += 1;
    }
    out
}

fn draw_header(f: &mut Frame, app: &mut App, opts: &TuiOptions, area: Rect) {
    let auth = if app.logged_in { "已登入" } else { "未登入" };
    let kids = if app.child_count == 0 {
        String::new()
    } else {
        format!("  子代理 {}", app.child_count)
    };
    let left = if app.running {
        let act = if app.activity.is_empty() {
            app.status.clone()
        } else {
            app.activity.clone()
        };
        let clock = app
            .work_started
            .map(|t| format!("  {}", md::fmt_duration(t.elapsed().as_millis() as u64)))
            .unwrap_or_default();
        format!(
            " grokaagent  {} {}{}  · {}  {}{}",
            spinner(app.tick),
            act,
            clock,
            app.status,
            auth,
            kids
        )
    } else {
        format!(
            " grokaagent  {}  {}  {}{}",
            app.status, auth, app.cache, kids
        )
    };
    let effort_bit = if app
        .catalog
        .find(&opts.model)
        .is_some_and(|m| !m.send_reasoning())
    {
        String::new()
    } else {
        format!(" · {}", opts.reasoning_effort.as_str())
    };
    let right = format!("{}{}  * ", opts.model, effort_bit);
    let right_w = Line::from(right.as_str()).width() as u16;
    let split = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Min(10), Constraint::Length(right_w.max(12))])
        .split(area);
    let left_style = if app.running {
        Style::default()
            .fg(Color::Black)
            .bg(ACCENT)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(DIM).bg(BG)
    };
    f.render_widget(Paragraph::new(Span::styled(left, left_style)), split[0]);
    f.render_widget(
        Paragraph::new(Span::styled(
            right,
            Style::default().fg(ACCENT).bg(BG).add_modifier(Modifier::BOLD),
        )),
        split[1],
    );
    app.hits.push((split[1], Hit::ModelChip));
    let gear = Rect::new(
        split[1].x + split[1].width.saturating_sub(3),
        split[1].y,
        3,
        1,
    );
    app.hits.push((gear, Hit::Gear));
}

fn draw_chat(f: &mut Frame, app: &mut App, area: Rect) -> Vec<(Rect, Hit)> {
    let block = Block::default()
        .borders(Borders::TOP)
        .border_style(Style::default().fg(BORDER))
        .style(Style::default().bg(PANEL).fg(TEXT));
    f.render_widget(block, area);
    app.hits.push((area, Hit::Chat));

    let inner = Rect::new(
        area.x.saturating_add(1),
        area.y.saturating_add(1),
        area.width.saturating_sub(2),
        area.height.saturating_sub(1),
    );
    let mut tool_hits = Vec::new();
    if inner.width == 0 || inner.height == 0 {
        return tool_hits;
    }

    enum Paint {
        Line(Line<'static>, Option<Hit>),
        Graphic {
            rel: String,
            width: u16,
            height: u16,
            hit: Option<Hit>,
        },
    }
    let mut paints: Vec<Paint> = Vec::new();
    for item in chat_logical_rows(app, inner.width) {
        if let Some((rel, width, height)) = item.graphic {
            paints.push(Paint::Graphic {
                rel,
                width,
                height,
                hit: item.hit,
            });
            continue;
        }
        let wrapped = if item.wrap {
            wrap_visual(item.line, inner.width)
        } else {
            vec![item.line]
        };
        for wrapped in wrapped {
            paints.push(Paint::Line(wrapped, item.hit));
        }
    }
    let total: u16 = paints
        .iter()
        .map(|p| match p {
            Paint::Line(..) => 1,
            Paint::Graphic { height, .. } => *height,
        })
        .sum();
    let scroll = if app.stick_bottom {
        total.saturating_sub(inner.height)
    } else {
        app.scroll.min(total.saturating_sub(1))
    };
    let vis_end = scroll.saturating_add(inner.height);
    let mut logical = 0u16;
    for paint in paints {
        let h = match &paint {
            Paint::Line(..) => 1,
            Paint::Graphic { height, .. } => *height,
        };
        let start = logical;
        let end = logical.saturating_add(h);
        logical = end;
        if end <= scroll || start >= vis_end {
            continue;
        }
        let screen_y = inner.y + start.saturating_sub(scroll);
        let vis_h = end.min(vis_end).saturating_sub(start.max(scroll)).max(1);
        match paint {
            Paint::Line(line, hit) => {
                let r = Rect::new(inner.x, screen_y, inner.width, 1);
                f.render_widget(Paragraph::new(line), r);
                if let Some(kind) = hit {
                    app.hits.push((r, kind));
                    tool_hits.push((r, kind));
                }
            }
            Paint::Graphic {
                rel,
                width,
                height,
                hit,
            } => {
                let fully = start >= scroll && end <= vis_end;
                if fully {
                    if let Some(proto) = cached_graphic(app, &rel, width, height) {
                        let area = proto.area();
                        let draw = Rect::new(
                            inner.x.saturating_add(crate::preview::INDENT),
                            screen_y,
                            area.width.min(inner.width.saturating_sub(crate::preview::INDENT)),
                            area.height.min(height),
                        );
                        if draw.width > 0 && draw.height > 0 {
                            f.render_widget(Image::new(&proto), draw);
                        }
                    } else {
                        f.render_widget(
                            Paragraph::new(Span::styled(
                                "      [無法預覽]",
                                Style::default().fg(DIM),
                            )),
                            Rect::new(inner.x, screen_y, inner.width, 1),
                        );
                    }
                } else {
                    f.render_widget(
                        Paragraph::new(Span::styled(
                            "      圖片（捲動以完整顯示）",
                            Style::default().fg(DIM),
                        )),
                        Rect::new(inner.x, screen_y, inner.width, 1),
                    );
                }
                let hit_r = Rect::new(inner.x, screen_y, inner.width, vis_h);
                if let Some(kind) = hit {
                    app.hits.push((hit_r, kind));
                    tool_hits.push((hit_r, kind));
                }
            }
        }
    }
    tool_hits
}

fn cached_graphic(app: &mut App, rel: &str, w: u16, h: u16) -> Option<Protocol> {
    let key = (rel.to_string(), w, h);
    if let Some(p) = app.image_proto.get(&key) {
        return Some(p.clone());
    }
    let picker = app.picker.clone()?;
    let abs = app.session.workspace.join(rel);
    let proto = crate::preview::protocol_for(&picker, &abs, w, h)?;
    app.image_proto.insert(key, proto.clone());
    Some(proto)
}

fn draw_tool_panel(f: &mut Frame, app: &mut App, chat: Rect, tool_hits: &[(Rect, Hit)]) {
    let Some((ri, ci)) = app.open_tool else {
        return;
    };
    let Some(call) = app.rows.get(ri).and_then(|r| match r {
        Row::Tools(g) => g.calls.get(ci).cloned(),
        _ => None,
    }) else {
        app.open_tool = None;
        return;
    };

    app.hits.push((chat, Hit::DismissTool));
    for &(r, h) in tool_hits {
        app.hits.push((r, h));
    }

    let w = chat.width.saturating_sub(4).min(76).max(28);
    let h = chat.height.saturating_sub(2).min(24).max(8);
    let x = chat.x + chat.width.saturating_sub(w) / 2;
    let y = chat.y.saturating_add(1);
    let panel = Rect::new(x, y, w, h);
    f.render_widget(Clear, panel);
    let block = Block::default()
        .title(format!(" {} ", call.name))
        .title_style(Style::default().fg(TEXT).add_modifier(Modifier::BOLD))
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(ACCENT))
        .style(Style::default().bg(COMPOSER).fg(TEXT));
    f.render_widget(block, panel);

    let close = Rect::new(panel.x + panel.width.saturating_sub(4), panel.y, 3, 1);
    f.render_widget(
        Paragraph::new(Span::styled(
            " × ",
            Style::default().fg(DIM).bg(COMPOSER),
        )),
        close,
    );

    let inner = Rect::new(
        panel.x.saturating_add(1),
        panel.y.saturating_add(1),
        panel.width.saturating_sub(2),
        panel.height.saturating_sub(2),
    );
    let details = call_detail_lines(&call);
    let mut y = inner.y;
    for line in details {
        if y >= inner.y + inner.height {
            break;
        }
        for wrapped in wrap_visual(line, inner.width.max(1)) {
            if y >= inner.y + inner.height {
                break;
            }
            f.render_widget(Paragraph::new(wrapped), Rect::new(inner.x, y, inner.width, 1));
            y = y.saturating_add(1);
        }
    }
    app.hits.push((panel, Hit::ToolPanel));
    app.hits.push((close, Hit::ToolPanelClose));
}

fn composer_height(queue_shown: usize, attach: u16) -> u16 {
    8 + queue_shown as u16 + attach
}

fn draw_composer(f: &mut Frame, app: &mut App, opts: &TuiOptions, area: Rect) -> Position {
    let block = Block::default()
        .borders(Borders::TOP)
        .border_style(Style::default().fg(BORDER))
        .style(Style::default().bg(BG));
    f.render_widget(block, area);
    let inner = Rect::new(
        area.x.saturating_add(1),
        area.y.saturating_add(1),
        area.width.saturating_sub(2),
        area.height.saturating_sub(1),
    );
    let shown = app.queue.len().min(5);
    let has_attach = !app.pending.is_empty();
    let mut constraints = vec![Constraint::Length(1)];
    if shown > 0 {
        constraints.push(Constraint::Length(shown as u16));
    }
    if has_attach {
        constraints.push(Constraint::Length(1));
    }
    constraints.push(Constraint::Min(3));
    constraints.push(Constraint::Length(1));
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints(constraints)
        .split(inner);
    let chip_row = rows[0];
    let mut idx = 1;
    let list_row = if shown > 0 {
        let r = rows[idx];
        idx += 1;
        Some(r)
    } else {
        None
    };
    let attach_row = if has_attach {
        let r = rows[idx];
        idx += 1;
        Some(r)
    } else {
        None
    };
    let box_area = rows[idx];
    let hint_row = rows[idx + 1];

    let qn = app.queue.len();
    let q_style = if app.send_mode == SendMode::Queue {
        Style::default().bg(ACCENT).fg(Color::Black).add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(DIM)
    };
    let i_style = if app.send_mode == SendMode::Insert {
        Style::default().bg(ACCENT).fg(Color::Black).add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(DIM)
    };
    let q_label = if qn == 0 {
        " 排隊 ".to_string()
    } else {
        format!(" {qn} 已排隊 ")
    };
    let q_w = Line::from(q_label.as_str()).width() as u16;
    let i_w = 8u16;
    let q_rect = Rect::new(chip_row.x, chip_row.y, q_w, 1);
    let i_rect = Rect::new(chip_row.x + q_w + 1, chip_row.y, i_w, 1);
    f.render_widget(Paragraph::new(Span::styled(q_label, q_style)), q_rect);
    f.render_widget(Paragraph::new(Span::styled(" 插入 ", i_style)), i_rect);
    app.hits.push((q_rect, Hit::QueueChip));
    app.hits.push((i_rect, Hit::InsertChip));
    let paste_label = " 貼上圖片 ";
    let p_w = display_cols(paste_label);
    let p_rect = Rect::new(i_rect.x.saturating_add(i_w + 1), chip_row.y, p_w, 1);
    if p_rect.x + p_w <= chip_row.x + chip_row.width {
        f.render_widget(
            Paragraph::new(Span::styled(
                paste_label,
                Style::default()
                    .fg(Color::Black)
                    .bg(ACCENT)
                    .add_modifier(Modifier::BOLD),
            )),
            p_rect,
        );
        app.hits.push((p_rect, Hit::PasteImage));
    }

    let mut x = p_rect.x.saturating_add(p_w + 1);
    if app.queue_edit.is_some() {
        let n = app.queue_edit.unwrap() + 1;
        let label = format!(" 編輯排隊 #{n} ");
        let lw = Line::from(label.as_str()).width() as u16;
        if x + lw < chip_row.x + chip_row.width {
            f.render_widget(
                Paragraph::new(Span::styled(
                    label,
                    Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
                )),
                Rect::new(x, chip_row.y, lw, 1),
            );
            x = x.saturating_add(lw + 1);
        }
        let cancel = " 取消 ";
        let cw = 6u16;
        let right = chip_row.x + chip_row.width;
        let cancel_x = if x + cw <= right {
            x
        } else {
            right.saturating_sub(cw)
        };
        let cancel_rect = Rect::new(cancel_x, chip_row.y, cw, 1);
        f.render_widget(
            Paragraph::new(Span::styled(
                cancel,
                Style::default().bg(WARN).fg(Color::Black).add_modifier(Modifier::BOLD),
            )),
            cancel_rect,
        );
        app.hits.push((cancel_rect, Hit::CancelQueueEdit));
    }

    if let Some(list) = list_row {
        for i in 0..shown {
            let y = list.y.saturating_add(i as u16);
            let editing = app.queue_edit == Some(i);
            let text = if editing {
                app.edit.text.replace('\n', " ")
            } else {
                app.queue.get(i).map(|q| q.label()).unwrap_or_default()
            };
            let mark = if editing { "▸" } else { "•" };
            let prefix = format!(" {mark} ");
            let avail = list.width.saturating_sub(Line::from(prefix.as_str()).width() as u16);
            let body = truncate_width(&text, avail);
            let style = if editing {
                Style::default().fg(ACCENT)
            } else {
                Style::default().fg(DIM)
            };
            let row = Rect::new(list.x, y, list.width, 1);
            f.render_widget(
                Paragraph::new(Line::from(vec![
                    Span::styled(prefix, style),
                    Span::styled(body, style),
                ])),
                row,
            );
            app.hits.push((row, Hit::QueueItem(i as u16)));
        }
    }

    if let Some(row) = attach_row {
        let mut x = row.x;
        for (i, rel) in app.pending.iter().enumerate() {
            let name = std::path::Path::new(rel)
                .file_name()
                .and_then(|s| s.to_str())
                .unwrap_or(rel);
            let label = format!(" {name} × ");
            let w = Line::from(label.as_str()).width() as u16;
            if x + w > row.x + row.width {
                break;
            }
            let r = Rect::new(x, row.y, w, 1);
            f.render_widget(
                Paragraph::new(Span::styled(
                    label,
                    Style::default().bg(COMPOSER).fg(ACCENT),
                )),
                r,
            );
            app.hits.push((r, Hit::PendingClose(i as u16)));
            x = x.saturating_add(w + 1);
        }
    }

    let input_block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(if app.focus == Focus::Chat { ACCENT } else { BORDER }))
        .style(Style::default().bg(COMPOSER).fg(TEXT));
    f.render_widget(input_block, box_area);
    app.hits.push((box_area, Hit::Composer));

    let inner_box = Rect::new(
        box_area.x.saturating_add(1),
        box_area.y.saturating_add(1),
        box_area.width.saturating_sub(2),
        box_area.height.saturating_sub(2),
    );
    app.composer_inner = inner_box;

    let caret = if app.edit.is_empty() {
        let placeholder = if app.queue_edit.is_some() {
            "編輯排隊訊息…  Enter 完成  ·  空白則移除  ·  點取消或 Esc 還原".into()
        } else if app.running {
            let act = if app.activity.is_empty() {
                "模型工作中".into()
            } else {
                format!("{} {}", spinner(app.tick), app.activity)
            };
            format!("{act}  — Enter 排隊，Ctrl+Enter 插入…")
        } else {
            "傳訊息，或點「貼上圖片」…".into()
        };
        f.render_widget(
            Paragraph::new(Span::styled(placeholder, Style::default().fg(DIM))),
            inner_box,
        );
        app.composer_vscroll = 0;
        Position::new(inner_box.x, inner_box.y)
    } else {
        draw_edit(f, inner_box, &app.edit, &mut app.composer_vscroll)
    };

    let hint = if app.queue_edit.is_some() {
        "Enter 完成編輯  ·  Esc 或點取消 還原  ·  工作結束也不會送出，直到編輯完成".to_string()
    } else {
        format!(
            "Enter 送出  ·  點「貼上圖片」附圖  ·  點訊息後 Ctrl+C 複製  ·  {} · {}",
            opts.model,
            opts.reasoning_effort.label()
        )
    };
    f.render_widget(
        Paragraph::new(Span::styled(hint, Style::default().fg(DIM))),
        hint_row,
    );

    caret
}

fn draw_edit(f: &mut Frame, inner: Rect, edit: &Edit, vscroll: &mut u16) -> Position {
    let width = inner.width.max(1);
    let height = inner.height.max(1);
    let ranges = wrap_lines(&edit.text, width);
    let (crow, ccol) = caret_row_col(&edit.text, &ranges, edit.caret);
    *vscroll = crow.saturating_sub(height.saturating_sub(1));
    let sel = edit.sel_range();
    let chars: Vec<char> = edit.text.chars().collect();
    let mut lines: Vec<Line> = Vec::new();
    let start = *vscroll as usize;
    let end = (start + height as usize).min(ranges.len());
    for &(a, b) in &ranges[start..end] {
        lines.push(edit_line(&chars, a, b, sel));
    }
    if lines.is_empty() {
        lines.push(Line::from(""));
    }
    f.render_widget(Paragraph::new(lines), inner);
    let screen_row = crow.saturating_sub(*vscroll);
    Position::new(
        inner.x.saturating_add(ccol.min(width.saturating_sub(1))),
        inner.y.saturating_add(screen_row.min(height.saturating_sub(1))),
    )
}

fn edit_line(chars: &[char], a: usize, b: usize, sel: Option<(usize, usize)>) -> Line<'static> {
    if a >= b {
        return Line::from("");
    }
    let Some((lo, hi)) = sel else {
        let s: String = chars[a..b].iter().collect();
        return Line::from(Span::styled(s, Style::default().fg(TEXT)));
    };
    let mut spans = Vec::new();
    let mut i = a;
    while i < b {
        let selected = i >= lo && i < hi;
        let mut j = i + 1;
        while j < b && (j >= lo && j < hi) == selected {
            j += 1;
        }
        let s: String = chars[i..j].iter().collect();
        let style = if selected {
            Style::default().bg(ACCENT).fg(Color::Black)
        } else {
            Style::default().fg(TEXT)
        };
        spans.push(Span::styled(s, style));
        i = j;
    }
    Line::from(spans)
}

fn draw_ask(f: &mut Frame, app: &mut App) -> Option<Position> {
    let area = f.area();
    let ask = app.ask.as_ref()?;
    let n = ask.n() as u16;
    let inner_w = area.width.saturating_sub(10).min(72).max(36);
    let q_rows = wrap_lines(&ask.question.prompt, inner_w.saturating_sub(2))
        .len()
        .max(1) as u16;
    let fill_h = if ask.filling { 3 } else { 0 };
    let h = (q_rows + n + fill_h + 6)
        .min(area.height.saturating_sub(2))
        .max(10);
    let w = inner_w.saturating_add(4).min(area.width.saturating_sub(2));
    let x = area.x + area.width.saturating_sub(w) / 2;
    let y = area.y + area.height.saturating_sub(h) / 2;
    let r = Rect::new(x, y, w, h);
    f.render_widget(Clear, r);
    let block = Block::default()
        .title(" 問卷 ")
        .title_style(Style::default().fg(TEXT).add_modifier(Modifier::BOLD))
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(ACCENT))
        .style(Style::default().bg(COMPOSER).fg(TEXT));
    f.render_widget(block, r);
    app.hits.push((r, Hit::AskPanel));

    let body = Rect::new(
        r.x.saturating_add(2),
        r.y.saturating_add(1),
        r.width.saturating_sub(4),
        r.height.saturating_sub(2),
    );
    let mut constraints = vec![Constraint::Length(q_rows.max(1))];
    for _ in 0..n {
        constraints.push(Constraint::Length(1));
    }
    if fill_h > 0 {
        constraints.push(Constraint::Length(fill_h));
    }
    constraints.push(Constraint::Length(1));
    constraints.push(Constraint::Length(1));
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints(constraints)
        .split(body);

    let q_style = Style::default().fg(TEXT).add_modifier(Modifier::BOLD);
    f.render_widget(
        Paragraph::new(ask.question.prompt.clone()).style(q_style).wrap(ratatui::widgets::Wrap { trim: true }),
        rows[0],
    );

    let mut row_i = 1usize;
    for i in 0..ask.n() {
        let opt = &ask.question.options[i];
        let chosen = ask.chosen.get(i).copied().unwrap_or(false);
        let mark = if ask.question.allow_multiple {
            if chosen { "☑" } else { "☐" }
        } else if chosen {
            "●"
        } else {
            "○"
        };
        let pointer = if i == ask.cursor { "›" } else { " " };
        let extra = if opt.input { "  （可填寫）" } else { "" };
        let selected = i == ask.cursor;
        let style = if selected {
            Style::default().fg(Color::Black).bg(ACCENT)
        } else if chosen {
            Style::default().fg(ACCENT)
        } else {
            Style::default().fg(TEXT)
        };
        let label = format!(" {pointer} {mark}  {}{extra} ", opt.label);
        let row = rows[row_i];
        f.render_widget(Paragraph::new(Span::styled(label, style)), row);
        app.hits.push((row, Hit::AskOption(i as u16)));
        row_i += 1;
    }

    let mut caret = None;
    if fill_h > 0 {
        let box_r = rows[row_i];
        row_i += 1;
        let fill_block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(ACCENT))
            .style(Style::default().bg(PANEL));
        f.render_widget(fill_block, box_r);
        let inner = Rect::new(
            box_r.x.saturating_add(1),
            box_r.y.saturating_add(1),
            box_r.width.saturating_sub(2),
            box_r.height.saturating_sub(2).max(1),
        );
        app.ask_fill_inner = inner;
        app.hits.push((box_r, Hit::AskFill));
        if let Some(ask) = app.ask.as_mut() {
            caret = Some(draw_edit(f, inner, &ask.fill_edit, &mut ask.fill_scroll));
        }
    }

    let btns = rows[row_i];
    let cancel_label = " 取消 ";
    let ok_label = " 確定 ";
    let cw = Line::from(cancel_label).width() as u16;
    let ow = Line::from(ok_label).width() as u16;
    let ok_r = Rect::new(btns.x, btns.y, ow, 1);
    let cancel_r = Rect::new(btns.x.saturating_add(ow + 2), btns.y, cw, 1);
    f.render_widget(
        Paragraph::new(Span::styled(
            ok_label,
            Style::default().fg(Color::Black).bg(ACCENT).add_modifier(Modifier::BOLD),
        )),
        ok_r,
    );
    f.render_widget(
        Paragraph::new(Span::styled(cancel_label, Style::default().fg(DIM).bg(PANEL))),
        cancel_r,
    );
    app.hits.push((ok_r, Hit::AskConfirm));
    app.hits.push((cancel_r, Hit::AskCancel));

    let hint = if app.ask.as_ref().is_some_and(|a| a.filling) {
        "輸入自訂內容  ·  Enter 確定  ·  Esc 回到選項"
    } else if app.ask.as_ref().is_some_and(|a| a.question.allow_multiple) {
        "↑↓ 移動  ·  空白鍵勾選  ·  Enter 確定  ·  Esc 取消"
    } else {
        "↑↓ 移動  ·  Enter 選擇  ·  可填寫項會開啟輸入框  ·  Esc 取消"
    };
    if let Some(hint_row) = rows.get(row_i + 1) {
        f.render_widget(
            Paragraph::new(Span::styled(hint, Style::default().fg(DIM))),
            *hint_row,
        );
    }
    caret
}

fn draw_workspace_pick(f: &mut Frame, app: &mut App) -> Option<Position> {
    let area = f.area();
    let w = area.width.saturating_sub(6).min(92).max(42);
    let h = area.height.saturating_sub(4).min(28).max(14);
    let x = area.x + area.width.saturating_sub(w) / 2;
    let y = area.y + area.height.saturating_sub(h) / 2;
    let r = Rect::new(x, y, w, h);
    f.render_widget(Clear, r);
    let block = Block::default()
        .title(" 選擇工作目錄 ")
        .title_style(Style::default().fg(TEXT).add_modifier(Modifier::BOLD))
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(ACCENT))
        .style(Style::default().bg(COMPOSER).fg(TEXT));
    f.render_widget(block, r);
    app.hits.push((r, Hit::WsPanel));

    let body = Rect::new(
        r.x.saturating_add(2),
        r.y.saturating_add(1),
        r.width.saturating_sub(4),
        r.height.saturating_sub(2),
    );
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Length(3),
            Constraint::Min(6),
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Length(1),
        ])
        .split(body);

    f.render_widget(
        Paragraph::new(Span::styled(
            "輸入路徑或檔名搜尋  ·  點資料夾進入  ·  點檔案選上層",
            Style::default().fg(DIM),
        )),
        rows[0],
    );

    let path_focus = app
        .workspace_pick
        .as_ref()
        .is_some_and(|p| p.focus == WsFocus::Path);
    let path_block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(if path_focus { ACCENT } else { BORDER }))
        .style(Style::default().bg(PANEL));
    f.render_widget(path_block, rows[1]);
    let path_inner = Rect::new(
        rows[1].x.saturating_add(1),
        rows[1].y.saturating_add(1),
        rows[1].width.saturating_sub(2),
        rows[1].height.saturating_sub(2).max(1),
    );
    app.hits.push((rows[1], Hit::WsPath));
    let mut caret = None;
    if let Some(p) = app.workspace_pick.as_mut() {
        p.path_inner = path_inner;
        caret = Some(draw_edit(f, path_inner, &p.edit, &mut p.path_scroll));
    }

    let list = rows[2];
    if let Some(p) = app.workspace_pick.as_mut() {
        p.list_area = list;
        let vis = list.height as usize;
        p.reveal(list.height);
        let start = p.scroll as usize;
        let end = (start + vis).min(p.view.entries.len());
        for (row_i, idx) in (start..end).enumerate() {
            let ent = &p.view.entries[idx];
            let y = list.y.saturating_add(row_i as u16);
            if y >= list.y.saturating_add(list.height) {
                break;
            }
            let cell = Rect::new(list.x, y, list.width, 1);
            let selected = idx == p.cursor;
            let icon = if ent.is_parent {
                "↑"
            } else if ent.is_dir {
                "📁"
            } else {
                "📄"
            };
            let label = format!(" {icon}  {} ", ent.name);
            let style = if selected {
                Style::default().fg(Color::Black).bg(ACCENT)
            } else if ent.is_dir {
                Style::default().fg(USER)
            } else {
                Style::default().fg(TEXT)
            };
            f.render_widget(
                Paragraph::new(Span::styled(truncate_width(&label, list.width), style)),
                cell,
            );
            app.hits.push((cell, Hit::WsEntry(idx as u16)));
        }
        if p.view.entries.is_empty() {
            f.render_widget(
                Paragraph::new(Span::styled("（沒有符合的項目）", Style::default().fg(DIM))),
                list,
            );
        }
    }

    let btns = rows[3];
    let confirm = " 選擇此資料夾 ";
    let create = " 建立資料夾 ";
    let cancel = " 取消 ";
    let cw = display_cols(confirm);
    let crw = display_cols(create);
    let clw = display_cols(cancel);
    let ok_r = Rect::new(btns.x, btns.y, cw, 1);
    let create_r = Rect::new(btns.x.saturating_add(cw + 1), btns.y, crw, 1);
    let cancel_r = Rect::new(btns.x.saturating_add(cw + crw + 2), btns.y, clw, 1);
    f.render_widget(
        Paragraph::new(Span::styled(
            confirm,
            Style::default()
                .fg(Color::Black)
                .bg(ACCENT)
                .add_modifier(Modifier::BOLD),
        )),
        ok_r,
    );
    f.render_widget(
        Paragraph::new(Span::styled(create, Style::default().fg(TEXT).bg(PANEL))),
        create_r,
    );
    f.render_widget(
        Paragraph::new(Span::styled(cancel, Style::default().fg(DIM).bg(PANEL))),
        cancel_r,
    );
    app.hits.push((ok_r, Hit::WsConfirm));
    app.hits.push((create_r, Hit::WsCreate));
    app.hits.push((cancel_r, Hit::WsCancel));

    let notice = app
        .workspace_pick
        .as_ref()
        .and_then(|p| p.notice.clone())
        .unwrap_or_else(|| "Enter 進入資料夾或確定  ·  Esc 取消".into());
    f.render_widget(
        Paragraph::new(Span::styled(notice, Style::default().fg(DIM))),
        rows[4],
    );
    if let Some(p) = app.workspace_pick.as_ref() {
        f.render_widget(
            Paragraph::new(Span::styled(
                folderpick::display_path(&p.view.cwd),
                Style::default().fg(DIM),
            )),
            rows[5],
        );
    }
    if app
        .workspace_pick
        .as_ref()
        .is_some_and(|p| p.focus == WsFocus::Path)
    {
        caret
    } else {
        None
    }
}

fn draw_settings(f: &mut Frame, app: &mut App, opts: &TuiOptions) -> Option<Position> {
    let win = *app.settings.as_ref()?;
    if win.minimized {
        return None;
    }
    let r = win_rect(&win, f.area());
    f.render_widget(Clear, r);
    let focused = app.focus == Focus::Settings;
    let block = Block::default()
        .title(" 設定 ")
        .title_style(Style::default().fg(TEXT).add_modifier(Modifier::BOLD))
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(if focused { ACCENT } else { BORDER }))
        .style(Style::default().bg(PANEL).fg(TEXT));
    f.render_widget(block, r);

    let close = Rect::new(r.x + r.width.saturating_sub(4), r.y, 3, 1);
    let maxb = Rect::new(r.x + r.width.saturating_sub(7), r.y, 3, 1);
    let minb = Rect::new(r.x + r.width.saturating_sub(10), r.y, 3, 1);
    f.render_widget(Paragraph::new(Span::styled(" × ", Style::default().fg(WARN))), close);
    f.render_widget(Paragraph::new(Span::styled(" □ ", Style::default().fg(DIM))), maxb);
    f.render_widget(Paragraph::new(Span::styled(" ─ ", Style::default().fg(DIM))), minb);
    app.hits.push((Rect::new(r.x, r.y, r.width.saturating_sub(10), 1), Hit::Title));
    app.hits.push((minb, Hit::Min));
    app.hits.push((maxb, Hit::Max));
    app.hits.push((close, Hit::Close));

    let body = Rect::new(
        r.x.saturating_add(2),
        r.y.saturating_add(2),
        r.width.saturating_sub(4),
        r.height.saturating_sub(3),
    );
    let lines = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Length(3),
            Constraint::Length(1),
            Constraint::Length(3),
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Min(1),
        ])
        .split(body);

    f.render_widget(
        Paragraph::new(Span::styled("帳號", Style::default().fg(DIM))),
        lines[0],
    );
    let account_focus = focused && app.setting_field == SettingField::Account;
    let (btn_label, extra) = match &app.login_ui {
        LoginUi::Starting => (
            " 連線中… ".to_string(),
            "正在向 xAI 取得登入代碼".to_string(),
        ),
        LoginUi::Waiting { .. } => (" 取消 ".to_string(), String::new()),
        LoginUi::Failed(e) => (
            if app.logged_in {
                " 登出 ".to_string()
            } else {
                " 登入 Grok ".to_string()
            },
            format!("失敗：{e}"),
        ),
        LoginUi::Idle if app.logged_in => (" 登出 ".to_string(), "已登入".to_string()),
        LoginUi::Idle => (" 登入 Grok ".to_string(), "未登入".to_string()),
    };
    let btn_style = if account_focus {
        Style::default()
            .bg(ACCENT)
            .fg(Color::Black)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(TEXT).bg(COMPOSER)
    };
    let btn_w = display_cols(&btn_label).max(4);
    let btn_cell = Rect::new(lines[1].x, lines[1].y, btn_w.min(lines[1].width), 1);
    f.render_widget(
        Paragraph::new(Span::styled(btn_label, btn_style)),
        btn_cell,
    );
    app.hits.push((btn_cell, Hit::AccountBtn));
    if let LoginUi::Waiting { user_code, .. } = &app.login_ui {
        let code_label = format!(" {user_code} ");
        let code_w = display_cols(&code_label).min(lines[2].width);
        let code_cell = Rect::new(lines[2].x, lines[2].y, code_w, 1);
        f.render_widget(
            Paragraph::new(Span::styled(
                code_label,
                Style::default()
                    .bg(COMPOSER)
                    .fg(ACCENT)
                    .add_modifier(Modifier::BOLD),
            )),
            code_cell,
        );
        app.hits.push((code_cell, Hit::LoginCode));
        let rest_x = lines[2].x.saturating_add(code_w.saturating_add(1));
        if rest_x < lines[2].x.saturating_add(lines[2].width) {
            f.render_widget(
                Paragraph::new(Span::styled(
                    "點此複製 · 已開瀏覽器",
                    Style::default().fg(DIM),
                )),
                Rect::new(
                    rest_x,
                    lines[2].y,
                    lines[2]
                        .x
                        .saturating_add(lines[2].width)
                        .saturating_sub(rest_x),
                    1,
                ),
            );
        }
    } else {
        f.render_widget(
            Paragraph::new(Span::styled(extra, Style::default().fg(DIM))),
            lines[2],
        );
    }

    f.render_widget(
        Paragraph::new(Span::styled("模型", Style::default().fg(DIM))),
        lines[3],
    );
    let model_focus = focused && app.setting_field == SettingField::Model;
    let model_label = app
        .catalog
        .find(&opts.model)
        .map(|m| m.name.clone())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| opts.model.clone());
    draw_combo(f, app, lines[4], &model_label, model_focus, Hit::SettingModel);

    let efforts = effort_choices(app, opts);
    let effort_enabled = !efforts.is_empty();
    f.render_widget(
        Paragraph::new(Span::styled(
            if effort_enabled {
                "思考強度"
            } else {
                "思考強度（此模型不支援）"
            },
            Style::default().fg(DIM),
        )),
        lines[5],
    );
    let effort_focus = focused && app.setting_field == SettingField::Effort;
    let effort_label = efforts
        .iter()
        .find(|e| e.value == opts.reasoning_effort)
        .map(|e| e.label.clone())
        .unwrap_or_else(|| opts.reasoning_effort.label().to_string());
    if effort_enabled {
        draw_combo(
            f,
            app,
            lines[6],
            &effort_label,
            effort_focus,
            Hit::SettingEffort,
        );
    } else {
        let disabled = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(BORDER))
            .style(Style::default().bg(COMPOSER));
        f.render_widget(disabled, lines[6]);
        let inner = Rect::new(
            lines[6].x.saturating_add(1),
            lines[6].y.saturating_add(1),
            lines[6].width.saturating_sub(2),
            1,
        );
        f.render_widget(
            Paragraph::new(Span::styled("—", Style::default().fg(DIM))),
            inner,
        );
    }

    f.render_widget(
        Paragraph::new(Span::styled(
            "搜尋（web + X）",
            Style::default().fg(DIM),
        )),
        lines[7],
    );
    let search_on = opts.web_search;
    let search_focus = focused && app.setting_field == SettingField::Search;
    let search_label = if search_on { " 開 " } else { " 關 " };
    let search_style = if search_on {
        Style::default()
            .bg(ACCENT)
            .fg(Color::Black)
            .add_modifier(Modifier::BOLD)
    } else if search_focus {
        Style::default().fg(ACCENT).bg(COMPOSER)
    } else {
        Style::default().fg(DIM).bg(COMPOSER)
    };
    let search_cell = Rect::new(lines[8].x, lines[8].y, 5, 1);
    f.render_widget(
        Paragraph::new(Span::styled(search_label, search_style)),
        search_cell,
    );
    app.hits.push((search_cell, Hit::Search));

    let hint = match &app.catalog_status {
        CatalogStatus::Loading => "正在載入可用模型…".to_string(),
        CatalogStatus::Failed(e) => format!("目錄載入失敗：{e}"),
        CatalogStatus::Idle if app.logged_in => "登入後會自動載入可用模型".to_string(),
        CatalogStatus::Idle => "登入後可載入模型目錄".to_string(),
        CatalogStatus::Ready => String::new(),
    };
    f.render_widget(
        Paragraph::new(Span::styled(hint, Style::default().fg(DIM))),
        lines[9],
    );

    if let Some(kind) = app.drop {
        let anchor = match kind {
            DropKind::Model => lines[4],
            DropKind::Effort => lines[6],
        };
        draw_drop_list(f, app, opts, kind, anchor, f.area());
    }

    None
}

fn draw_combo(f: &mut Frame, app: &mut App, area: Rect, label: &str, focus: bool, hit: Hit) {
    let open = match hit {
        Hit::SettingModel => app.drop == Some(DropKind::Model),
        Hit::SettingEffort => app.drop == Some(DropKind::Effort),
        _ => false,
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(if focus || open { ACCENT } else { BORDER }))
        .style(Style::default().bg(COMPOSER));
    f.render_widget(block, area);
    app.hits.push((area, hit));
    let inner = Rect::new(
        area.x.saturating_add(1),
        area.y.saturating_add(1),
        area.width.saturating_sub(2),
        1,
    );
    if inner.width == 0 {
        return;
    }
    let arrow = if open { " ▴" } else { " ▾" };
    let arrow_w = 2u16;
    let text_w = inner.width.saturating_sub(arrow_w);
    let shown: String = label.chars().take(text_w as usize).collect();
    f.render_widget(
        Paragraph::new(Span::styled(shown, Style::default().fg(TEXT))),
        Rect::new(inner.x, inner.y, text_w, 1),
    );
    f.render_widget(
        Paragraph::new(Span::styled(arrow, Style::default().fg(DIM))),
        Rect::new(inner.x.saturating_add(text_w), inner.y, arrow_w, 1),
    );
}

fn draw_drop_list(
    f: &mut Frame,
    app: &mut App,
    opts: &TuiOptions,
    kind: DropKind,
    anchor: Rect,
    clip: Rect,
) {
    let labels: Vec<String> = match kind {
        DropKind::Model => model_choices(app, opts)
            .into_iter()
            .map(|(id, name)| if name.is_empty() { id } else { name })
            .collect(),
        DropKind::Effort => effort_choices(app, opts)
            .into_iter()
            .map(|e| e.label)
            .collect(),
    };
    if labels.is_empty() {
        return;
    }
    let vis = DROP_VISIBLE.min(labels.len()).max(1) as u16;
    let height = vis.saturating_add(2);
    let mut y = anchor.y.saturating_add(anchor.height);
    if y.saturating_add(height) > clip.y.saturating_add(clip.height) {
        y = anchor.y.saturating_sub(height);
    }
    y = y.max(clip.y);
    let list = Rect {
        x: anchor.x,
        y,
        width: anchor.width,
        height: height.min(clip.height.saturating_sub(y.saturating_sub(clip.y))),
    };
    if list.width < 4 || list.height < 3 {
        return;
    }
    f.render_widget(Clear, list);
    f.render_widget(
        Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(ACCENT))
            .style(Style::default().bg(COMPOSER).fg(TEXT)),
        list,
    );
    let inner = Rect::new(
        list.x.saturating_add(1),
        list.y.saturating_add(1),
        list.width.saturating_sub(2),
        list.height.saturating_sub(2),
    );
    let start = app.drop_scroll as usize;
    let end = (start + inner.height as usize).min(labels.len());
    for (row, i) in (start..end).enumerate() {
        let cell = Rect::new(inner.x, inner.y.saturating_add(row as u16), inner.width, 1);
        let selected = i == app.drop_cursor;
        let style = if selected {
            Style::default()
                .bg(ACCENT)
                .fg(Color::Black)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(TEXT).bg(COMPOSER)
        };
        let text: String = labels[i].chars().take(inner.width as usize).collect();
        f.render_widget(Paragraph::new(Span::styled(format!(" {text}"), style)), cell);
        app.hits.push((cell, Hit::CatalogPick(i as u16)));
    }
}

pub async fn run_tui(opts: TuiOptions) -> Result<()> {
    let auth_path = auth::default_auth_path()?;
    enable_raw_mode().map_err(Error::Io)?;
    let mut stdout = stdout();
    execute!(
        stdout,
        EnterAlternateScreen,
        EnableMouseCapture,
        EnableBracketedPaste
    )
    .map_err(Error::Io)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend).map_err(Error::Io)?;

    let result = tui_loop(&mut terminal, opts, auth_path).await;

    let _ = disable_raw_mode();
    let _ = execute!(
        io::stdout(),
        DisableBracketedPaste,
        DisableMouseCapture,
        LeaveAlternateScreen
    );
    result
}

async fn tui_loop(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    mut opts: TuiOptions,
    auth_path: PathBuf,
) -> Result<()> {
    let (tx, mut ev_rx) = mpsc::unbounded_channel();
    let jsonl = JsonlSink::create(&opts.events)?;
    let sink: Arc<FanoutSink> = Arc::new(FanoutSink {
        sinks: vec![Box::new(jsonl), Box::new(ChannelSink::new(tx))],
    });

    let knobs = Arc::new(Mutex::new(SessionKnobs {
        model: opts.model.clone(),
        reasoning_effort: opts.reasoning_effort,
        send_reasoning: true,
        server_tools: kit::search_tools(opts.web_search),
    }));

    let store = SessionStore::open().ok();
    let launch_workspace = opts.workspace.clone();
    let mut listed = store
        .as_ref()
        .and_then(|s| s.list().ok())
        .unwrap_or_default();
    let boot = boot_session(store.as_ref(), &listed, launch_workspace.clone());
    let session = boot.session;
    let created = boot.created;
    if !listed.iter().any(|m| m.id == session.id) {
        listed.insert(0, session.clone());
    }

    let mut app = App {
        rows: boot.rows,
        edit: Edit::default(),
        status: "待命".into(),
        cache: "cache —".into(),
        child_count: 0,
        running: false,
        awaiting: false,
        logged_in: auth::load_tokens(&auth_path).is_ok(),
        auth_path: auth_path.clone(),
        login_ui: LoginUi::Idle,
        login_gen: 0,
        want_login: false,
        scroll: 0,
        stick_bottom: true,
        send_mode: SendMode::Queue,
        queue: VecDeque::new(),
        inbox_tx: None,
        knobs: knobs.clone(),
        focus: Focus::Chat,
        setting_field: SettingField::Model,
        settings: None,
        drag: None,
        hits: Vec::new(),
        area: Rect::default(),
        streaming: false,
        composer_inner: Rect::default(),
        composer_vscroll: 0,
        input_dragging: false,
        catalog: ModelCatalog::default(),
        catalog_status: CatalogStatus::Idle,
        drop: None,
        drop_cursor: 0,
        drop_scroll: 0,
        want_catalog: false,
        open_tool: None,
        seal_tools: false,
        activity: String::new(),
        tick: 0,
        current_id: session.id.clone(),
        session,
        parked: HashMap::new(),
        sessions: listed,
        store,
        launch_workspace,
        sidebar_ids: Vec::new(),
        rename: None,
        rename_inner: Rect::default(),
        work_started: None,
        queue_edit: None,
        composer_stash: None,
            pending: Vec::new(),
            chat_sel: ChatSel::None,
            preview: HashMap::new(),
            picker: Some(crate::preview::detect_picker()),
            image_proto: HashMap::new(),
            image_hits: Vec::new(),
            children: Vec::new(),
            monitors: Vec::new(),
            backgrounds: Vec::new(),
            inspector: None,
            inspector_scroll: 0,
            ask_hub: AskUserHub::new(),
            ask_hubs: HashMap::new(),
            ask: None,
            ask_fill_inner: Rect::default(),
            ask_passive: false,
            workspace_pick: None,
    };
    app.catalog.ensure_current(&opts.model, opts.reasoning_effort);
    app.want_catalog = app.logged_in;
    if created || app.is_blank_draft() {
        if app.logged_in {
            app.push(Row::Meta("磁碟上有 xAI session".into()));
        } else {
            app.push(Row::Meta("尚未登入 — 在設定中登入 Grok 帳號".into()));
        }
    }

    let mut keys = EventStream::new();
    let (done_tx, mut done_rx) = mpsc::unbounded_channel::<(String, crate::agent::RunOutcome)>();
    let (cat_tx, mut cat_rx) = mpsc::unbounded_channel();
    let (login_tx, mut login_rx) = mpsc::unbounded_channel();

    loop {
        while let Ok(ev) = ev_rx.try_recv() {
            app.route_event(ev);
        }
        while let Ok(result) = cat_rx.try_recv() {
            ingest_catalog(&mut app, &mut opts, result);
        }
        while let Ok(ev) = login_rx.try_recv() {
            apply_login_event(&mut app, ev);
        }
        if app.want_catalog
            && app.logged_in
            && !matches!(app.catalog_status, CatalogStatus::Loading)
        {
            app.want_catalog = false;
            app.catalog_status = CatalogStatus::Loading;
            let tx = cat_tx.clone();
            let auth = auth_path.clone();
            tokio::spawn(async move {
                let result = match XaiOauthProvider::new(auth, None) {
                    Ok(p) => p.list_models().await,
                    Err(e) => Err(e),
                };
                let _ = tx.send(result);
            });
        }
        if app.want_login {
            app.want_login = false;
            let tx = login_tx.clone();
            let path = app.auth_path.clone();
            let gen = app.login_gen;
            tokio::spawn(async move {
                run_settings_login(path, gen, tx).await;
            });
        }
        if app.running
            || app.children.iter().any(|c| c.alive)
            || app.monitors.iter().any(|m| m.alive)
            || app.backgrounds.iter().any(|b| b.alive)
        {
            app.tick = app.tick.wrapping_add(1);
        }
        flush_all(&mut app);
        while let Ok((sid, out)) = done_rx.try_recv() {
            app.finish_run(&sid, out);
        }
        kick_idle_queue(&mut app, &opts, &sink, &done_tx);

        terminal
            .draw(|f| {
                let pos = draw(f, &mut app, &opts);
                f.set_cursor_position(pos);
            })
            .map_err(Error::Io)?;

        tokio::select! {
            maybe = keys.next() => {
                match maybe {
                    Some(Ok(Event::Key(key))) => {
                        if key.kind != KeyEventKind::Press {
                            continue;
                        }
                        if handle_key(&mut app, &mut opts, key.code, key.modifiers, &sink, &done_tx) {
                            break;
                        }
                    }
                    Some(Ok(Event::Mouse(m))) => {
                        handle_mouse(&mut app, &mut opts, m.kind, m.column, m.row, m.modifiers);
                    }
                    Some(Ok(Event::Paste(s))) => {
                        if app.workspace_pick.is_some() {
                            if let Some(p) = app.workspace_pick.as_mut() {
                                p.edit.insert_str(&s);
                            }
                            app.sync_workspace_pick();
                        } else if app.ask.as_ref().is_some_and(|a| a.filling) {
                            if let Some(ask) = app.ask.as_mut() {
                                let remain = ask::MAX_INPUT.saturating_sub(ask.fill_edit.len());
                                let clipped: String = s.chars().take(remain).collect();
                                ask.fill_edit.insert_str(&clipped);
                            }
                        } else {
                            app.paste_from_terminal(&s);
                        }
                    }
                    Some(Ok(Event::Resize(_, _))) => {}
                    _ => continue,
                }
            }
            _ = tokio::time::sleep(Duration::from_millis(80)) => {}
        }
    }
    app.persist_transcript();
    app.inbox_tx = None;
    Ok(())
}

fn flush_all(app: &mut App) {
    flush_queue(app);
    let ids: Vec<String> = app.parked.keys().cloned().collect();
    for id in ids {
        if !app.parked.get(&id).is_some_and(|p| {
            p.awaiting && !p.queue.is_empty() && p.queue_edit.is_none()
        }) {
            continue;
        }
        let mut parked = match app.parked.remove(&id) {
            Some(p) => p,
            None => continue,
        };
        parked = app.with_parked(parked, |app| {
            flush_queue(app);
        });
        app.parked.insert(id, parked);
    }
}

fn queued_to_turn(q: Queued) -> UserTurn {
    UserTurn {
        text: q.text,
        images: q.images.into_iter().map(PathBuf::from).collect(),
    }
}

fn user_row_from_turn(turn: &UserTurn) -> UserMsg {
    UserMsg {
        text: turn.text.clone(),
        images: turn
            .images
            .iter()
            .map(|p| p.to_string_lossy().replace('\\', "/"))
            .collect(),
    }
}

fn flush_queue(app: &mut App) {
    if !app.awaiting || app.queue_edit.is_some() {
        return;
    }
    let Some(msg) = app.queue.pop_front() else {
        return;
    };
    let turn = queued_to_turn(msg);
    app.push(Row::User(user_row_from_turn(&turn)));
    if let Some(tx) = &app.inbox_tx {
        if tx.send(turn).is_ok() {
            app.running = true;
            app.awaiting = false;
            app.status = "工作中".into();
            app.mark_work_start();
        }
    }
}

fn kick_idle_queue(
    app: &mut App,
    opts: &TuiOptions,
    sink: &Arc<FanoutSink>,
    done_tx: &mpsc::UnboundedSender<(String, crate::agent::RunOutcome)>,
) {
    if app.queue_edit.is_some() || app.running || app.inbox_tx.is_some() {
        return;
    }
    let Some(msg) = app.queue.pop_front() else {
        return;
    };
    start_or_send(app, opts, sink, done_tx, queued_to_turn(msg));
}

fn start_or_send(
    app: &mut App,
    opts: &TuiOptions,
    sink: &Arc<FanoutSink>,
    done_tx: &mpsc::UnboundedSender<(String, crate::agent::RunOutcome)>,
    turn: UserTurn,
) {
    app.push(Row::User(user_row_from_turn(&turn)));
    if !app.logged_in {
        app.push(Row::Err("尚未登入 — 請在設定中登入 Grok 帳號".into()));
        return;
    }
    app.mark_work_start();
    if let Some(tx) = &app.inbox_tx {
        let _ = tx.send(turn);
        app.running = true;
        app.awaiting = false;
        app.status = "工作中".into();
        app.persist_transcript();
        return;
    }
    if !app.session.named {
        let fallback = session::title_fallback_from_user_text(&turn.text);
        if let Some(store) = &app.store {
            let _ = store.touch_name(&mut app.session, fallback, false);
        } else {
            app.session.name = fallback;
            app.session.named = true;
        }
        spawn_title(sink, &app.session.id, &opts.model, &turn.text);
    }
    app.session.updated_at = chrono::Utc::now();
    if let Some(store) = &app.store {
        let _ = store.save_meta(&app.session);
    }
    app.persist_transcript();
    app.running = true;
    app.awaiting = false;
    app.status = "工作中".into();
    let (inbox_tx, inbox_rx) = mpsc::unbounded_channel();
    app.inbox_tx = Some(inbox_tx);
    let mut opts = opts.clone();
    opts.workspace = app.session.workspace.clone();
    let sink = sink.clone();
    let knobs = app.knobs.clone();
    let run_id = app.session.id.clone();
    let done_tx = done_tx.clone();
    let ask = Some(app.attach_ask_hub(&run_id));
    tokio::spawn(async move {
        let sid = run_id.clone();
        let out = run_one(opts, turn, sink, knobs, inbox_rx, run_id, ask).await;
        let _ = done_tx.send((
            sid,
            out.unwrap_or_else(|e| crate::agent::RunOutcome {
                run_id: String::new(),
                text: e.to_string(),
                turns: 0,
                cache_turns: vec![],
                compacted: 0,
            }),
        ));
    });
}

fn spawn_title(sink: &Arc<FanoutSink>, session_id: &str, model: &str, prompt: &str) {
    let sink = sink.clone();
    let session_id = session_id.to_string();
    let model = model.to_string();
    let prompt = prompt.to_string();
    tokio::spawn(async move {
        let Ok(auth_path) = auth::default_auth_path() else {
            return;
        };
        let Ok(provider) = XaiOauthProvider::new(auth_path, Some(model)) else {
            return;
        };
        let name = provider.generate_session_title(&prompt).await;
        sink.emit(&AgentEvent::SessionNamed {
            meta: EventMeta {
                ts: chrono::Utc::now(),
                agent_name: "root".into(),
                run_id: session_id,
                parent_run_id: None,
            },
            name,
        });
    });
}

fn submit_current(
    app: &mut App,
    opts: &TuiOptions,
    sink: &Arc<FanoutSink>,
    done_tx: &mpsc::UnboundedSender<(String, crate::agent::RunOutcome)>,
    force_insert: bool,
) {
    let Some(turn) = app.take_turn() else {
        return;
    };
    let mode = if force_insert { SendMode::Insert } else { app.send_mode };
    match submit_kind(app.inbox_tx.is_some(), app.running, mode) {
        Submit::Queue => {
            app.queue.push_back(Queued {
                text: turn.text,
                images: turn
                    .images
                    .iter()
                    .map(|p| p.to_string_lossy().replace('\\', "/"))
                    .collect(),
            });
        }
        Submit::Start | Submit::Insert => {
            start_or_send(app, opts, sink, done_tx, turn);
        }
    }
}

fn handle_ws_key(app: &mut App, code: KeyCode, mods: KeyModifiers) -> bool {
    if is_paste_key(code, mods) {
        if let Some(s) = clipboard_get() {
            if let Some(p) = app.workspace_pick.as_mut() {
                p.edit.insert_str(&s);
            }
            app.sync_workspace_pick();
        }
        return false;
    }
    let shift = mods.contains(KeyModifiers::SHIFT);
    match code {
        KeyCode::Esc => app.cancel_workspace_pick(),
        KeyCode::Tab => {
            if let Some(p) = app.workspace_pick.as_mut() {
                p.focus = match p.focus {
                    WsFocus::Path => WsFocus::List,
                    WsFocus::List => WsFocus::Path,
                };
            }
        }
        KeyCode::Enter => handle_ws_enter(app),
        KeyCode::Up => ws_move_cursor(app, -1),
        KeyCode::Down => ws_move_cursor(app, 1),
        KeyCode::PageUp => ws_move_cursor(app, -8),
        KeyCode::PageDown => ws_move_cursor(app, 8),
        KeyCode::Left if app.workspace_pick.as_ref().is_some_and(|p| p.focus == WsFocus::Path) => {
            if let Some(p) = app.workspace_pick.as_mut() {
                p.edit.move_left(shift);
            }
        }
        KeyCode::Right if app.workspace_pick.as_ref().is_some_and(|p| p.focus == WsFocus::Path) => {
            if let Some(p) = app.workspace_pick.as_mut() {
                p.edit.move_right(shift);
            }
        }
        KeyCode::Home if app.workspace_pick.as_ref().is_some_and(|p| p.focus == WsFocus::Path) => {
            if let Some(p) = app.workspace_pick.as_mut() {
                p.edit.home(shift);
            }
        }
        KeyCode::End if app.workspace_pick.as_ref().is_some_and(|p| p.focus == WsFocus::Path) => {
            if let Some(p) = app.workspace_pick.as_mut() {
                p.edit.end(shift);
            }
        }
        KeyCode::Backspace => {
            if let Some(p) = app.workspace_pick.as_mut() {
                p.focus = WsFocus::Path;
                p.edit.backspace();
            }
            app.sync_workspace_pick();
        }
        KeyCode::Delete => {
            if let Some(p) = app.workspace_pick.as_mut() {
                p.focus = WsFocus::Path;
                p.edit.delete_forward();
            }
            app.sync_workspace_pick();
        }
        KeyCode::Char('a') if mods.contains(KeyModifiers::CONTROL) => {
            if let Some(p) = app.workspace_pick.as_mut() {
                p.focus = WsFocus::Path;
                p.edit.select_all();
            }
        }
        KeyCode::Char(c) if !mods.contains(KeyModifiers::CONTROL) && !c.is_control() => {
            if let Some(p) = app.workspace_pick.as_mut() {
                p.focus = WsFocus::Path;
                p.edit.insert_char(c);
            }
            app.sync_workspace_pick();
        }
        _ => {}
    }
    false
}

fn ws_move_cursor(app: &mut App, delta: i32) {
    let Some(p) = app.workspace_pick.as_mut() else {
        return;
    };
    p.focus = WsFocus::List;
    let n = p.view.entries.len();
    if n == 0 {
        p.cursor = 0;
        return;
    }
    let next = p.cursor as i32 + delta;
    p.cursor = next.clamp(0, n as i32 - 1) as usize;
}

fn handle_ws_enter(app: &mut App) {
    let Some(p) = app.workspace_pick.as_ref() else {
        return;
    };
    if p.focus == WsFocus::Path {
        let typed = p.edit.text.trim().to_string();
        let path = std::path::PathBuf::from(&typed);
        if path.is_dir() {
            let cur = folderpick::normalize(&path);
            if cur == p.view.cwd {
                app.confirm_workspace_pick();
            } else {
                app.enter_workspace_dir(path);
            }
            return;
        }
        let dirs: Vec<_> = p
            .view
            .entries
            .iter()
            .filter(|e| e.is_dir && !e.is_parent)
            .cloned()
            .collect();
        if dirs.len() == 1 {
            app.enter_workspace_dir(dirs[0].path.clone());
            return;
        }
        app.confirm_workspace_pick();
        return;
    }
    let Some(ent) = p.selected().cloned() else {
        app.confirm_workspace_pick();
        return;
    };
    if ent.is_dir {
        app.enter_workspace_dir(ent.path);
    } else {
        app.confirm_workspace_pick();
    }
}

fn handle_key(
    app: &mut App,
    opts: &mut TuiOptions,
    code: KeyCode,
    mods: KeyModifiers,
    sink: &Arc<FanoutSink>,
    done_tx: &mpsc::UnboundedSender<(String, crate::agent::RunOutcome)>,
) -> bool {
    if matches!(code, KeyCode::Char('c')) && mods.contains(KeyModifiers::CONTROL) {
        if app.copy_selection() {
            return false;
        }
        app.cancel_ask();
        return true;
    }
    if app.workspace_pick.is_some() {
        return handle_ws_key(app, code, mods);
    }
    if matches!(code, KeyCode::Char('n') | KeyCode::Char('N')) && mods.contains(KeyModifiers::CONTROL)
    {
        app.cancel_rename();
        app.new_chat();
        return false;
    }
    if app.ask.is_some() {
        return handle_ask_key(app, code, mods);
    }
    if app.rename.is_some() {
        return handle_rename_key(app, code, mods);
    }
    if matches!(code, KeyCode::F(2))
        || (matches!(code, KeyCode::Char('g')) && mods.contains(KeyModifiers::CONTROL))
    {
        if app.settings.as_ref().is_some_and(|w| !w.minimized) && app.focus == Focus::Settings {
            app.settings = None;
            app.focus = Focus::Chat;
        } else {
            open_settings(app);
        }
        return false;
    }

    if app.focus == Focus::Settings {
        return handle_settings_key(app, opts, code, mods);
    }

    if is_paste_key(code, mods) {
        app.paste_clipboard();
        return false;
    }

    match (code, mods) {
        (KeyCode::Esc, _) => {
            if app.inspector.is_some() {
                app.close_inspector();
            } else if app.queue_edit.is_some() {
                app.cancel_queue_edit();
            } else if !app.dismiss_tool_ui() {
                if app.edit.has_sel() {
                    app.edit.clear_sel();
                } else if app.chat_sel != ChatSel::None {
                    app.chat_sel = ChatSel::None;
                } else {
                    app.settings = None;
                    app.focus = Focus::Chat;
                }
            }
        }
        (KeyCode::Char('a'), m) if m.contains(KeyModifiers::CONTROL) => {
            app.edit.select_all();
        }
        (KeyCode::Char('c' | 'C'), m)
            if !m.contains(KeyModifiers::CONTROL) && app.edit.has_sel() =>
        {
            if let Some(s) = app.edit.selected_text() {
                if clipboard_set(&s) {
                    app.status = "已複製".into();
                } else {
                    app.status = "無法複製到剪貼簿".into();
                }
            }
        }
        (KeyCode::Left, m) => {
            app.edit.move_left(m.contains(KeyModifiers::SHIFT));
        }
        (KeyCode::Right, m) => {
            app.edit.move_right(m.contains(KeyModifiers::SHIFT));
        }
        (KeyCode::Home, m) => {
            app.edit.home(m.contains(KeyModifiers::SHIFT));
        }
        (KeyCode::End, m) => {
            app.edit.end(m.contains(KeyModifiers::SHIFT));
        }
        (KeyCode::Up, m) => {
            if app.focus == Focus::Inspector {
                app.inspector_scroll = app.inspector_scroll.saturating_add(1);
            } else if app.edit.is_empty() && !m.contains(KeyModifiers::SHIFT) {
                app.stick_bottom = false;
                app.scroll = app.scroll.saturating_add(1);
            } else {
                app.edit.move_visual(
                    app.composer_inner.width.max(1),
                    -1,
                    m.contains(KeyModifiers::SHIFT),
                );
            }
        }
        (KeyCode::Down, m) => {
            if app.focus == Focus::Inspector {
                app.inspector_scroll = app.inspector_scroll.saturating_sub(1);
            } else if app.edit.is_empty() && !m.contains(KeyModifiers::SHIFT) {
                app.scroll = app.scroll.saturating_sub(1);
                if app.scroll == 0 {
                    app.stick_bottom = true;
                }
            } else {
                app.edit.move_visual(
                    app.composer_inner.width.max(1),
                    1,
                    m.contains(KeyModifiers::SHIFT),
                );
            }
        }
        (KeyCode::PageUp, _) => {
            app.stick_bottom = false;
            app.scroll = app.scroll.saturating_add(5);
        }
        (KeyCode::PageDown, _) => {
            app.scroll = app.scroll.saturating_sub(5);
            if app.scroll == 0 {
                app.stick_bottom = true;
            }
        }
        (KeyCode::Enter, m) if m.contains(KeyModifiers::CONTROL) => {
            if app.queue_edit.is_some() {
                app.commit_queue_edit();
            } else {
                submit_current(app, opts, sink, done_tx, true);
            }
        }
        (KeyCode::Enter, _) => {
            if app.queue_edit.is_some() {
                app.commit_queue_edit();
            } else {
                submit_current(app, opts, sink, done_tx, false);
            }
        }
        (KeyCode::Backspace, _) => {
            if app.edit.is_empty() && !app.pending.is_empty() {
                app.pending.pop();
            } else {
                app.edit.backspace();
            }
        }
        (KeyCode::Delete, _) => {
            app.edit.delete_forward();
        }
        (KeyCode::Char(c), m) if !m.contains(KeyModifiers::CONTROL) => {
            app.edit.insert_char(c);
        }
        _ => {}
    }
    false
}

fn handle_ask_key(app: &mut App, code: KeyCode, mods: KeyModifiers) -> bool {
    let filling = app.ask.as_ref().is_some_and(|a| a.filling);
    if filling {
        match code {
            KeyCode::Esc => {
                if let Some(ask) = app.ask.as_mut() {
                    ask.save_fill();
                }
                return false;
            }
            KeyCode::Enter => {
                app.submit_ask();
                return false;
            }
            KeyCode::Up => {
                if let Some(ask) = app.ask.as_mut() {
                    ask.save_fill();
                    ask.move_cursor(-1);
                }
                return false;
            }
            KeyCode::Down => {
                if let Some(ask) = app.ask.as_mut() {
                    ask.save_fill();
                    ask.move_cursor(1);
                }
                return false;
            }
            _ => {}
        }
        let shift = mods.contains(KeyModifiers::SHIFT);
        let Some(ask) = app.ask.as_mut() else {
            return false;
        };
        match code {
            KeyCode::Left => ask.fill_edit.move_left(shift),
            KeyCode::Right => ask.fill_edit.move_right(shift),
            KeyCode::Home => ask.fill_edit.home(shift),
            KeyCode::End => ask.fill_edit.end(shift),
            KeyCode::Backspace => ask.fill_edit.backspace(),
            KeyCode::Delete => ask.fill_edit.delete_forward(),
            KeyCode::Char('a') if mods.contains(KeyModifiers::CONTROL) => ask.fill_edit.select_all(),
            KeyCode::Char('v') if mods.contains(KeyModifiers::CONTROL) => {
                if let Some(s) = clipboard_get() {
                    let remain = ask::MAX_INPUT.saturating_sub(ask.fill_edit.len());
                    let clipped: String = s.chars().take(remain).collect();
                    ask.fill_edit.insert_str(&clipped);
                }
            }
            KeyCode::Char(c) if !mods.contains(KeyModifiers::CONTROL) && !c.is_control() => {
                if ask.fill_edit.len() < ask::MAX_INPUT {
                    ask.fill_edit.insert_char(c);
                }
            }
            _ => {}
        }
        return false;
    }
    match code {
        KeyCode::Esc => app.cancel_ask(),
        KeyCode::Up => {
            if let Some(ask) = app.ask.as_mut() {
                ask.move_cursor(-1);
            }
        }
        KeyCode::Down => {
            if let Some(ask) = app.ask.as_mut() {
                ask.move_cursor(1);
            }
        }
        KeyCode::Char(' ') => {
            let Some(ask) = app.ask.as_mut() else {
                return false;
            };
            let i = ask.cursor;
            let input = ask.question.options.get(i).is_some_and(|o| o.input);
            if input {
                ask.enter_fill();
            } else {
                ask.mark_cursor();
            }
        }
        KeyCode::Enter => {
            let Some(ask) = app.ask.as_ref() else {
                return false;
            };
            let i = ask.cursor;
            let input = ask.question.options.get(i).is_some_and(|o| o.input);
            if input {
                app.activate_ask_option(i, false);
            } else if ask.question.allow_multiple {
                app.submit_ask();
            } else {
                app.activate_ask_option(i, true);
            }
        }
        _ => {}
    }
    false
}

fn handle_rename_key(app: &mut App, code: KeyCode, mods: KeyModifiers) -> bool {
    match code {
        KeyCode::Esc => {
            app.cancel_rename();
            return false;
        }
        KeyCode::Enter => {
            app.commit_rename();
            return false;
        }
        _ => {}
    }
    let shift = mods.contains(KeyModifiers::SHIFT);
    let Some((_, edit)) = app.rename.as_mut() else {
        return false;
    };
    match code {
        KeyCode::Left => edit.move_left(shift),
        KeyCode::Right => edit.move_right(shift),
        KeyCode::Home => edit.home(shift),
        KeyCode::End => edit.end(shift),
        KeyCode::Backspace => edit.backspace(),
        KeyCode::Delete => edit.delete_forward(),
        KeyCode::Char('a') if mods.contains(KeyModifiers::CONTROL) => edit.select_all(),
        KeyCode::Char('v') if mods.contains(KeyModifiers::CONTROL) => {
            if let Some(s) = clipboard_get() {
                edit.insert_str(&s);
            }
        }
        KeyCode::Char(c) if !mods.contains(KeyModifiers::CONTROL) && !c.is_control() => {
            edit.insert_char(c);
        }
        _ => {}
    }
    false
}

fn drop_len(app: &App, opts: &TuiOptions) -> usize {
    match app.drop {
        Some(DropKind::Model) => model_choices(app, opts).len(),
        Some(DropKind::Effort) => effort_choices(app, opts).len(),
        None => 0,
    }
}

fn handle_settings_key(
    app: &mut App,
    opts: &mut TuiOptions,
    code: KeyCode,
    mods: KeyModifiers,
) -> bool {
    if app.drop.is_some() {
        match code {
            KeyCode::Esc => {
                app.drop = None;
                return false;
            }
            KeyCode::Up => {
                app.drop_cursor = app.drop_cursor.saturating_sub(1);
                reveal_drop_cursor(app, drop_len(app, opts));
                return false;
            }
            KeyCode::Down => {
                let len = drop_len(app, opts);
                if len > 0 {
                    app.drop_cursor = (app.drop_cursor + 1).min(len - 1);
                }
                reveal_drop_cursor(app, len);
                return false;
            }
            KeyCode::Enter | KeyCode::Char(' ') => {
                select_catalog_pick(app, opts, app.drop_cursor);
                return false;
            }
            KeyCode::Tab => {
                app.drop = None;
            }
            _ => return false,
        }
    }
    match code {
        KeyCode::Esc => {
            if login_in_flight(&app.login_ui) {
                cancel_login(app);
            } else {
                app.settings = None;
                app.focus = Focus::Chat;
            }
        }
        KeyCode::Tab => {
            app.setting_field = match app.setting_field {
                SettingField::Account => SettingField::Model,
                SettingField::Model => SettingField::Effort,
                SettingField::Effort => SettingField::Search,
                SettingField::Search => SettingField::Account,
            };
        }
        KeyCode::Enter | KeyCode::Char(' ') if app.setting_field == SettingField::Account => {
            activate_account(app);
        }
        KeyCode::Enter | KeyCode::Char(' ') if app.setting_field == SettingField::Search => {
            opts.web_search = !opts.web_search;
            sync_knobs(&app.knobs, opts, &app.catalog);
        }
        KeyCode::Enter | KeyCode::Char(' ') | KeyCode::Down
            if app.setting_field == SettingField::Model =>
        {
            open_drop(app, opts, DropKind::Model);
        }
        KeyCode::Enter | KeyCode::Char(' ') | KeyCode::Down
            if app.setting_field == SettingField::Effort =>
        {
            open_drop(app, opts, DropKind::Effort);
        }
        KeyCode::Left if app.setting_field == SettingField::Effort => {
            if let Some(e) = cycle_effort(
                &effort_choices(app, opts),
                opts.reasoning_effort,
                true,
            ) {
                apply_selected_effort(app, opts, e);
            }
        }
        KeyCode::Right if app.setting_field == SettingField::Effort => {
            if let Some(e) = cycle_effort(
                &effort_choices(app, opts),
                opts.reasoning_effort,
                false,
            ) {
                apply_selected_effort(app, opts, e);
            }
        }
        _ => {}
    }
    let _ = mods;
    false
}

fn handle_mouse(
    app: &mut App,
    opts: &mut TuiOptions,
    kind: MouseEventKind,
    col: u16,
    row: u16,
    mods: KeyModifiers,
) {
    let shift = mods.contains(KeyModifiers::SHIFT);
    match kind {
        MouseEventKind::Down(MouseButton::Left) => {
            app.drag = None;
            app.input_dragging = false;
            let hit = hit_at(&app.hits, col, row);
            if app.workspace_pick.is_some() {
                match hit {
                    Some(Hit::WsEntry(i)) => app.activate_ws_entry(i as usize),
                    Some(Hit::WsPath) => {
                        let inner = app
                            .workspace_pick
                            .as_ref()
                            .map(|p| p.path_inner)
                            .unwrap_or_default();
                        if let Some(p) = app.workspace_pick.as_mut() {
                            p.focus = WsFocus::Path;
                            let idx = click_to_index(&p.edit.text, inner, p.path_scroll, col, row);
                            p.edit.click(idx, shift);
                        }
                    }
                    Some(Hit::WsConfirm) => app.confirm_workspace_pick(),
                    Some(Hit::WsCreate) => app.create_workspace_dir(),
                    Some(Hit::WsCancel) => app.cancel_workspace_pick(),
                    Some(Hit::WsPanel) => {}
                    _ => {}
                }
                return;
            }
            if app.ask.is_some() {
                match hit {
                    Some(Hit::AskOption(i)) => {
                        app.activate_ask_option(i as usize, true);
                    }
                    Some(Hit::AskConfirm) => app.submit_ask(),
                    Some(Hit::AskCancel) => app.cancel_ask(),
                    Some(Hit::AskFill) => {
                        let inner = app.ask_fill_inner;
                        if let Some(ask) = app.ask.as_mut() {
                            if !ask.filling {
                                ask.enter_fill();
                            }
                            let idx = click_to_index(
                                &ask.fill_edit.text,
                                inner,
                                ask.fill_scroll,
                                col,
                                row,
                            );
                            ask.fill_edit.click(idx, shift);
                        }
                    }
                    Some(Hit::AskPanel) => {}
                    _ => {}
                }
                return;
            }
            match hit {
                Some(Hit::Gear) | Some(Hit::ModelChip) => open_settings(app),
                Some(Hit::QueueChip) => {
                    app.send_mode = SendMode::Queue;
                    app.focus = Focus::Chat;
                }
                Some(Hit::InsertChip) => {
                    app.send_mode = SendMode::Insert;
                    app.focus = Focus::Chat;
                }
                Some(Hit::PasteImage) => {
                    app.focus = Focus::Chat;
                    app.paste_image();
                }
                Some(Hit::QueueItem(i)) => {
                    app.commit_rename();
                    app.focus = Focus::Chat;
                    app.begin_queue_edit(i as usize);
                }
                Some(Hit::CancelQueueEdit) => {
                    app.cancel_queue_edit();
                    app.focus = Focus::Chat;
                }
                Some(Hit::RailChild(i)) => {
                    if let Some(c) = app.children.get(i as usize) {
                        app.inspector = Some(Inspector::Child(c.name.clone()));
                        app.inspector_scroll = 0;
                        app.focus = Focus::Inspector;
                    }
                }
                Some(Hit::RailMon(i)) => {
                    if let Some(m) = app.monitors.get(i as usize) {
                        app.inspector = Some(Inspector::Monitor(m.name.clone()));
                        app.inspector_scroll = 0;
                        app.focus = Focus::Inspector;
                    }
                }
                Some(Hit::RailBg(i)) => {
                    if let Some(b) = app.backgrounds.get(i as usize) {
                        app.inspector = Some(Inspector::Background(b.name.clone()));
                        app.inspector_scroll = 0;
                        app.focus = Focus::Inspector;
                    }
                }
                Some(Hit::InspectorClose) => {
                    app.close_inspector();
                }
                Some(Hit::Inspector) => {
                    app.focus = Focus::Inspector;
                }
                Some(Hit::PendingClose(i)) => {
                    let i = i as usize;
                    if i < app.pending.len() {
                        app.pending.remove(i);
                    }
                    app.focus = Focus::Chat;
                }
                Some(Hit::ChatRow(i)) => {
                    app.commit_rename();
                    app.focus = Focus::Chat;
                    app.chat_sel = ChatSel::Row(i as usize);
                    let _ = app.dismiss_tool_ui();
                }
                Some(Hit::ChatImage(i)) => {
                    app.commit_rename();
                    app.focus = Focus::Chat;
                    if let Some(path) = app.image_hits.get(i as usize).cloned() {
                        app.chat_sel = ChatSel::Image(path);
                    }
                    let _ = app.dismiss_tool_ui();
                }
                Some(Hit::Composer) => {
                    app.commit_rename();
                    app.focus = Focus::Chat;
                    let idx = click_to_index(
                        &app.edit.text,
                        app.composer_inner,
                        app.composer_vscroll,
                        col,
                        row,
                    );
                    app.edit.click(idx, shift);
                    app.input_dragging = true;
                }
                Some(Hit::ToolGroup(i)) => {
                    app.focus = Focus::Chat;
                    if let Some(Row::Tools(g)) = app.rows.get_mut(i) {
                        g.expanded = !g.expanded;
                        if !g.expanded && app.open_tool.map(|(r, _)| r) == Some(i) {
                            app.open_tool = None;
                        }
                    }
                }
                Some(Hit::Think(i)) => {
                    app.focus = Focus::Chat;
                    if let Some(Row::Think(t)) = app.rows.get_mut(i) {
                        t.expanded = !t.expanded;
                    }
                }
                Some(Hit::ToolItem(r, c)) => {
                    app.focus = Focus::Chat;
                    app.open_tool = Some((r, c));
                }
                Some(Hit::ToolPanel) => {}
                Some(Hit::ToolPanelClose) => {
                    app.open_tool = None;
                }
                Some(Hit::DismissTool) | Some(Hit::Chat) => {
                    app.commit_rename();
                    app.focus = Focus::Chat;
                    app.chat_sel = ChatSel::None;
                    let _ = app.dismiss_tool_ui();
                }
                Some(Hit::NewChat) => {
                    app.cancel_rename();
                    app.focus = Focus::Chat;
                    app.new_chat();
                }
                Some(Hit::RenameSession(i)) => {
                    if let Some(id) = app.sidebar_ids.get(i as usize).cloned() {
                        app.begin_rename(&id);
                    }
                }
                Some(Hit::DeleteSession(i)) => {
                    if let Some(id) = app.sidebar_ids.get(i as usize).cloned() {
                        app.delete_session(&id);
                    }
                }
                Some(Hit::Session(i)) => {
                    app.focus = Focus::Chat;
                    if let Some(id) = app.sidebar_ids.get(i as usize).cloned() {
                        if app.rename.as_ref().is_some_and(|(rid, _)| rid == &id) {
                            // stay in rename
                        } else {
                            app.commit_rename();
                            app.switch_to(&id);
                        }
                    }
                }
                Some(Hit::Dock) => open_settings(app),
                Some(Hit::Close) => {
                    app.drop = None;
                    app.settings = None;
                    app.focus = Focus::Chat;
                }
                Some(Hit::Min) => {
                    if let Some(w) = app.settings.as_mut() {
                        w.minimized = true;
                    }
                    app.focus = Focus::Chat;
                }
                Some(Hit::Max) => {
                    if let Some(w) = app.settings.as_mut() {
                        w.maximized = !w.maximized;
                    }
                    app.focus = Focus::Settings;
                }
                Some(Hit::Title) => {
                    app.focus = Focus::Settings;
                    if let Some(w) = app.settings.as_ref() {
                        app.drag = Some((col as i16 - w.x as i16, row as i16 - w.y as i16));
                    }
                }
                Some(Hit::SettingModel) => {
                    app.focus = Focus::Settings;
                    app.setting_field = SettingField::Model;
                    if app.drop == Some(DropKind::Model) {
                        app.drop = None;
                    } else {
                        open_drop(app, opts, DropKind::Model);
                    }
                }
                Some(Hit::SettingEffort) => {
                    app.focus = Focus::Settings;
                    app.setting_field = SettingField::Effort;
                    if app.drop == Some(DropKind::Effort) {
                        app.drop = None;
                    } else {
                        open_drop(app, opts, DropKind::Effort);
                    }
                }
                Some(Hit::CatalogPick(i)) => {
                    app.focus = Focus::Settings;
                    select_catalog_pick(app, opts, i as usize);
                }
                Some(Hit::Search) => {
                    app.drop = None;
                    app.focus = Focus::Settings;
                    app.setting_field = SettingField::Search;
                    opts.web_search = !opts.web_search;
                    sync_knobs(&app.knobs, opts, &app.catalog);
                }
                Some(Hit::AccountBtn) => {
                    app.drop = None;
                    app.focus = Focus::Settings;
                    app.setting_field = SettingField::Account;
                    activate_account(app);
                }
                Some(Hit::LoginCode) => {
                    app.focus = Focus::Settings;
                    app.setting_field = SettingField::Account;
                    if let LoginUi::Waiting { user_code, .. } = &app.login_ui {
                        let code = user_code.clone();
                        if clipboard_set(&code) {
                            app.status = "已複製登入代碼".into();
                        } else {
                            app.status = "無法複製登入代碼".into();
                        }
                    }
                }
                None | Some(Hit::AskOption(_)) | Some(Hit::AskConfirm) | Some(Hit::AskCancel)
                | Some(Hit::AskFill) | Some(Hit::AskPanel)
                | Some(Hit::WsPanel)
                | Some(Hit::WsPath)
                | Some(Hit::WsEntry(_))
                | Some(Hit::WsConfirm)
                | Some(Hit::WsCreate)
                | Some(Hit::WsCancel) => {}
            }
        }
        MouseEventKind::Drag(MouseButton::Left) => {
            if let (Some((dx, dy)), Some(w)) = (app.drag, app.settings.as_mut()) {
                if !w.maximized {
                    w.x = (col as i16 - dx).max(0) as u16;
                    w.y = (row as i16 - dy).max(0) as u16;
                }
            } else if app.input_dragging {
                let idx = click_to_index(
                    &app.edit.text,
                    app.composer_inner,
                    app.composer_vscroll,
                    col,
                    row,
                );
                app.edit.click(idx, true);
            }
        }
        MouseEventKind::Up(_) => {
            app.drag = None;
            app.input_dragging = false;
        }
        MouseEventKind::ScrollUp => {
            if app.workspace_pick.is_some() {
                ws_move_cursor(app, -1);
            } else if app.drop.is_some() && app.focus == Focus::Settings {
                app.drop_cursor = app.drop_cursor.saturating_sub(1);
                reveal_drop_cursor(app, drop_len(app, opts));
            } else if app.focus == Focus::Inspector {
                app.inspector_scroll = app.inspector_scroll.saturating_add(1);
            } else {
                app.stick_bottom = false;
                app.scroll = app.scroll.saturating_add(1);
            }
        }
        MouseEventKind::ScrollDown => {
            if app.workspace_pick.is_some() {
                ws_move_cursor(app, 1);
            } else if app.drop.is_some() && app.focus == Focus::Settings {
                let len = drop_len(app, opts);
                if len > 0 {
                    app.drop_cursor = (app.drop_cursor + 1).min(len - 1);
                }
                reveal_drop_cursor(app, len);
            } else if app.focus == Focus::Inspector {
                app.inspector_scroll = app.inspector_scroll.saturating_sub(1);
            } else {
                app.scroll = app.scroll.saturating_sub(1);
                if app.scroll == 0 {
                    app.stick_bottom = true;
                }
            }
        }
        _ => {}
    }
}

async fn run_one(
    opts: TuiOptions,
    turn: UserTurn,
    sink: Arc<FanoutSink>,
    knobs: Arc<Mutex<SessionKnobs>>,
    inbox: mpsc::UnboundedReceiver<UserTurn>,
    run_id: String,
    ask: Option<AskUserHub>,
) -> Result<crate::agent::RunOutcome> {
    let auth_path = auth::default_auth_path()?;
    let model = if opts.model.trim().is_empty() {
        "grok-4.6".to_string()
    } else {
        opts.model.clone()
    };
    let provider = XaiOauthProvider::new(auth_path, Some(model.clone()))?;
    let run_id = if run_id.is_empty() {
        uuid::Uuid::new_v4().to_string()
    } else {
        run_id
    };
    let dyn_sink: Arc<dyn crate::events::EventSink> = sink.clone();
    let server_tools = kit::search_tools(opts.web_search);
    let child_mode = std::env::var("GROKA_CHILD_MODE").unwrap_or_else(|_| "grok".into());
    crate::kit::run_with_nursery(
        &provider,
        dyn_sink,
        crate::kit::KernelSpec {
            agent_name: "root".into(),
            prompt: turn.text,
            images: turn.images,
            model,
            max_turns: opts.max_turns,
            workspace: opts.workspace,
            events_file: opts.events.clone(),
            events_dir: crate::kit::events_dir(&opts.events),
            server_tools,
            depth: 0,
            parent_run_id: None,
            run_id,
            child_mode,
            reasoning_effort: opts.reasoning_effort,
            knobs: Some(knobs),
            inbox: Some(inbox),
            ask,
        },
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn caret_uses_display_width_not_bytes() {
        let area = Rect::new(0, 10, 40, 1);
        let cjk = caret_in(area, "你好", 2);
        assert_eq!(cjk, Position::new(4, 10));
        let cjk_mid = caret_in(area, "你好", 1);
        assert_eq!(cjk_mid, Position::new(2, 10));
        let ascii = caret_in(area, "ab", 2);
        assert_eq!(ascii, Position::new(2, 10));
        let empty = caret_in(area, "", 0);
        assert_eq!(empty, Position::new(0, 10));
    }

    #[test]
    fn submit_queues_only_while_running() {
        assert_eq!(submit_kind(false, false, SendMode::Queue), Submit::Start);
        assert_eq!(submit_kind(true, true, SendMode::Queue), Submit::Queue);
        assert_eq!(submit_kind(true, true, SendMode::Insert), Submit::Insert);
        assert_eq!(submit_kind(true, false, SendMode::Queue), Submit::Insert);
    }

    #[test]
    fn editing_queued_message_blocks_flush_until_commit() {
        let mut app = test_app();
        let (tx, mut rx) = mpsc::unbounded_channel();
        app.inbox_tx = Some(tx);
        app.awaiting = true;
        app.queue.push_back("old".into());
        app.edit = Edit::at_end("draft".into());
        app.begin_queue_edit(0);
        assert_eq!(app.edit.text, "old");
        assert_eq!(
            app.composer_stash.as_ref().map(|e| e.text.as_str()),
            Some("draft")
        );
        app.edit = Edit::at_end("new text".into());
        flush_queue(&mut app);
        assert!(rx.try_recv().is_err(), "must not send while editing");
        assert_eq!(app.queue.len(), 1);
        assert!(!app.commit_queue_edit());
        assert_eq!(app.edit.text, "draft");
        assert_eq!(app.queue[0].text, "new text");
        flush_queue(&mut app);
        assert_eq!(rx.try_recv().unwrap().text, "new text");
        assert!(app.queue.is_empty());
    }

    #[test]
    fn cancel_queue_edit_restores_original_and_does_not_send_draft() {
        let mut app = test_app();
        let (tx, mut rx) = mpsc::unbounded_channel();
        app.inbox_tx = Some(tx);
        app.awaiting = true;
        app.edit = Edit::at_end("draft".into());
        app.queue.push_back("keep".into());
        app.begin_queue_edit(0);
        app.edit = Edit::at_end("nope".into());
        flush_queue(&mut app);
        assert!(rx.try_recv().is_err());
        app.cancel_queue_edit();
        assert!(app.queue_edit.is_none());
        assert_eq!(app.queue[0].text, "keep");
        assert_eq!(app.edit.text, "draft");
        flush_queue(&mut app);
        assert_eq!(rx.try_recv().unwrap().text, "keep");
    }

    #[test]
    fn empty_commit_removes_queued_item() {
        let mut app = test_app();
        app.queue.push_back("gone".into());
        app.queue.push_back("stay".into());
        app.begin_queue_edit(0);
        app.edit.clear();
        assert!(app.commit_queue_edit());
        assert_eq!(app.queue.len(), 1);
        assert_eq!(app.queue[0].text, "stay");
    }

    #[test]
    fn kick_idle_queue_waits_until_edit_finishes() {
        let mut app = test_app();
        app.logged_in = false;
        app.queue.push_back("later".into());
        app.begin_queue_edit(0);
        let opts = TuiOptions {
            model: "grok-4.6".into(),
            events: PathBuf::from("events.jsonl"),
            workspace: PathBuf::from("."),
            max_turns: 0,
            web_search: false,
            reasoning_effort: ReasoningEffort::High,
        };
        let sink = Arc::new(FanoutSink { sinks: vec![] });
        let (done_tx, _done_rx) = mpsc::unbounded_channel();
        kick_idle_queue(&mut app, &opts, &sink, &done_tx);
        assert_eq!(app.queue.len(), 1, "held while editing");
        app.commit_queue_edit();
        kick_idle_queue(&mut app, &opts, &sink, &done_tx);
        assert!(app.queue.is_empty());
        assert!(app
            .rows
            .iter()
            .any(|r| matches!(r, Row::User(u) if u.text == "later")));
    }

    #[test]
    fn child_and_monitor_fill_the_side_rail() {
        let mut app = test_app();
        app.current_id = "r".into();
        app.session.id = "r".into();
        let meta = test_meta();
        app.route_event(AgentEvent::ChildSpawned {
            meta: meta.clone(),
            name: "coder".into(),
            agent_card_url: "http://127.0.0.1:9/.well-known/agent-card.json".into(),
            prompt: "fix src/a.rs".into(),
        });
        app.route_event(AgentEvent::AgentMessage {
            meta: meta.clone(),
            from: "root".into(),
            to: "coder".into(),
            text: "fix src/a.rs".into(),
        });
        app.route_event(AgentEvent::MonitorAttached {
            meta: meta.clone(),
            name: "copy".into(),
            command: "python hook.py".into(),
            pid: 4242,
        });
        assert!(app.has_side());
        assert_eq!(app.children.len(), 1);
        assert_eq!(app.children[0].prompt, "fix src/a.rs");
        assert_eq!(app.children[0].messages.len(), 1);
        assert_eq!(app.children[0].messages[0].from, "root");
        assert_eq!(app.monitors[0].command, "python hook.py");
        assert_eq!(app.monitors[0].pid, 4242);

        let child_meta = crate::events::EventMeta {
            ts: chrono::Utc::now(),
            agent_name: "coder".into(),
            run_id: "child-run".into(),
            parent_run_id: Some("r".into()),
        };
        let before = app.rows.len();
        app.route_event(AgentEvent::ModelDelta {
            meta: child_meta,
            text: "reading the file".into(),
        });
        assert_eq!(app.rows.len(), before, "child work must not land in parent chat");
        assert!(
            app.children[0]
                .log
                .iter()
                .any(|l| l.contains("reading the file")),
            "{:?}",
            app.children[0].log
        );

        app.inspector = Some(Inspector::Child("coder".into()));
        app.focus = Focus::Inspector;
        app.close_inspector();
        assert!(app.inspector.is_none());
        assert!(matches!(app.focus, Focus::Chat));
    }

    #[test]
    fn background_fills_the_side_rail_and_keeps_output_out_of_chat() {
        let mut app = test_app();
        app.current_id = "r".into();
        app.session.id = "r".into();
        let meta = test_meta();
        app.route_event(AgentEvent::BackgroundStarted {
            meta: meta.clone(),
            name: "dev".into(),
            command: "npm run dev".into(),
            pid: 99,
        });
        assert!(app.has_side());
        assert_eq!(app.backgrounds.len(), 1);
        assert_eq!(app.backgrounds[0].command, "npm run dev");
        assert_eq!(app.backgrounds[0].pid, 99);
        assert!(app.backgrounds[0].alive);
        assert!(app.rows.iter().any(|r| match r {
            Row::Meta(s) => s.contains("後台 dev"),
            _ => false,
        }));

        let before = app.rows.len();
        app.route_event(AgentEvent::BackgroundOutput {
            meta: meta.clone(),
            name: "dev".into(),
            stream: "out".into(),
            text: "listening on :3000".into(),
        });
        assert_eq!(app.rows.len(), before, "background stdout must not land in chat");
        assert!(
            app.backgrounds[0]
                .log
                .iter()
                .any(|l| l.contains("listening on :3000")),
            "{:?}",
            app.backgrounds[0].log
        );

        app.route_event(AgentEvent::BackgroundExited {
            meta,
            name: "dev".into(),
            detail: "killed".into(),
        });
        assert!(!app.backgrounds[0].alive);
        assert_eq!(app.backgrounds[0].status, "結束");
        assert_eq!(app.backgrounds[0].detail, "killed");
    }

    #[test]
    fn child_spawned_prompt_defaults_when_missing() {
        let raw = serde_json::json!({
            "type": "child_spawned",
            "ts": "2026-08-14T00:00:00Z",
            "agent_name": "root",
            "run_id": "s",
            "name": "x",
            "agent_card_url": "http://127.0.0.1:1/.well-known/agent-card.json"
        });
        let ev: AgentEvent = serde_json::from_value(raw).unwrap();
        match ev {
            AgentEvent::ChildSpawned { prompt, name, .. } => {
                assert!(prompt.is_empty());
                assert_eq!(name, "x");
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn hit_at_uses_topmost() {
        let hits = vec![
            (Rect::new(0, 0, 10, 10), Hit::Chat),
            (Rect::new(2, 2, 4, 4), Hit::Gear),
        ];
        assert_eq!(hit_at(&hits, 3, 3), Some(Hit::Gear));
        assert_eq!(hit_at(&hits, 0, 0), Some(Hit::Chat));
        assert_eq!(hit_at(&hits, 50, 50), None);
    }

    #[test]
    fn row_line_prefixes() {
        let row = Row::User("hi".into());
        let l = &row_lines(&row)[0];
        assert!(format!("{l:?}").contains("hi"));
    }

    #[test]
    fn user_row_legacy_string_still_loads() {
        let row: Row = serde_json::from_value(serde_json::json!({"User": "hello"})).unwrap();
        match row {
            Row::User(u) => {
                assert_eq!(u.text, "hello");
                assert!(u.images.is_empty());
            }
            _ => panic!("expected User row"),
        }
    }

    #[test]
    fn screenshot_tool_finished_shows_picture_row() {
        let mut app = test_app();
        app.apply_event(AgentEvent::ToolStarted {
            meta: test_meta(),
            call_id: "c1".into(),
            name: "screenshot".into(),
            args: serde_json::json!({"path": ".groka/shots/a.jpg"}),
            kind: "client".into(),
        });
        app.apply_event(AgentEvent::ToolFinished {
            meta: test_meta(),
            call_id: "c1".into(),
            name: "screenshot".into(),
            output: serde_json::json!({
                "path": ".groka/shots/a.jpg",
                "attach_image": true,
                "mime": "image/jpeg"
            })
            .to_string(),
        });
        assert!(app.rows.iter().any(|r| matches!(
            r,
            Row::Picture { path, .. } if path == ".groka/shots/a.jpg"
        )));
        let pic = app.rows.iter().find_map(|r| match r {
            Row::Picture { path, label } => Some((path.as_str(), label.as_str())),
            _ => None,
        }).unwrap();
        assert!(pic.1.contains("模型在看"), "{}", pic.1);
        assert_eq!(row_copy_text(app.rows.last().unwrap()).contains(".groka/shots/a.jpg"), true);
    }

    fn write_red_png(dir: &std::path::Path, name: &str) -> String {
        let path = dir.join(name);
        image::RgbImage::from_pixel(80, 40, image::Rgb([255, 0, 0]))
            .save(&path)
            .unwrap();
        name.to_string()
    }

    #[test]
    fn missing_picker_falls_back_to_halfblock_rows() {
        let dir = tempfile::tempdir().unwrap();
        let rel = write_red_png(dir.path(), "red.png");
        let mut app = test_app();
        app.session.workspace = dir.path().to_path_buf();
        app.push(Row::Picture {
            path: rel,
            label: "shot".into(),
        });
        let vis = chat_logical_rows(&mut app, 80);
        assert!(vis.iter().all(|l| l.graphic.is_none()));
        assert!(
            vis.iter().any(|l| format!("{:?}", l.line).contains("▀")),
            "{:?}",
            vis.iter().map(|l| format!("{:?}", l.line)).collect::<Vec<_>>()
        );
        assert!(vis.iter().any(|l| l.hit == Some(Hit::ChatImage(0))));
    }

    #[test]
    fn sixel_picker_reserves_a_graphic_slot_instead_of_halfblocks() {
        let dir = tempfile::tempdir().unwrap();
        let rel = write_red_png(dir.path(), "red.png");
        let mut app = test_app();
        app.session.workspace = dir.path().to_path_buf();
        let mut picker = Picker::from_fontsize((8, 16));
        picker.set_protocol_type(ratatui_image::picker::ProtocolType::Sixel);
        app.picker = Some(picker);
        app.push(Row::User(UserMsg {
            text: "see".into(),
            images: vec![rel.clone()],
        }));
        let vis = chat_logical_rows(&mut app, 80);
        let g = vis
            .iter()
            .find_map(|l| l.graphic.clone())
            .expect("graphic slot");
        assert_eq!(g.0, rel);
        assert!(g.1 >= 1 && g.1 <= crate::preview::MAX_COLS, "{}", g.1);
        assert!(g.2 >= 1 && g.2 <= crate::preview::MAX_ROWS, "{}", g.2);
        assert!(
            vis.iter().all(|l| !format!("{:?}", l.line).contains("▀")),
            "sixel path must not emit halfblock lines: {:?}",
            vis.iter().map(|l| format!("{:?}", l.line)).collect::<Vec<_>>()
        );
        let image_hits: Vec<_> = vis
            .iter()
            .filter(|l| l.hit == Some(Hit::ChatImage(0)))
            .collect();
        assert_eq!(
            image_hits.len(),
            2,
            "caption + graphic should both be clickable, got {}",
            image_hits.len()
        );
        assert!(image_hits.iter().any(|l| l.graphic.is_some()));
        assert!(image_hits.iter().any(|l| format!("{:?}", l.line).contains("圖片")));
    }

    fn press(app: &mut App, code: KeyCode) {
        let (tx, _rx) = mpsc::unbounded_channel();
        let mut opts = TuiOptions {
            model: "grok-4.6".into(),
            events: PathBuf::from("events.jsonl"),
            workspace: PathBuf::from("."),
            max_turns: 0,
            web_search: false,
            reasoning_effort: ReasoningEffort::High,
        };
        let sink = Arc::new(FanoutSink { sinks: vec![] });
        handle_key(app, &mut opts, code, KeyModifiers::NONE, &sink, &tx);
    }

    fn sample_ask(allow_multiple: bool) -> AgentEvent {
        AgentEvent::AskUser {
            meta: test_meta(),
            question: "挑一個".into(),
            allow_multiple,
            options: vec![
                crate::ask::Choice {
                    id: "retry".into(),
                    label: "重試".into(),
                    input: false,
                },
                crate::ask::Choice {
                    id: "other".into(),
                    label: "自填".into(),
                    input: true,
                },
            ],
        }
    }

    #[test]
    fn ask_user_event_opens_overlay() {
        let mut app = test_app();
        app.apply_event(sample_ask(false));
        assert!(app.ask.is_some());
        assert_eq!(app.focus, Focus::Ask);
        assert!(
            app.rows.iter().any(|r| matches!(r, Row::Meta(s) if s.contains("挑一個"))),
            "transcript should record the question"
        );
    }

    #[test]
    fn ask_arrows_move_cursor_and_enter_submits() {
        let mut app = test_app();
        let mut rx = app.ask_hub.register();
        app.apply_event(sample_ask(false));
        press(&mut app, KeyCode::Down);
        assert_eq!(app.ask.as_ref().unwrap().cursor, 1);
        press(&mut app, KeyCode::Up);
        assert_eq!(app.ask.as_ref().unwrap().cursor, 0);
        press(&mut app, KeyCode::Enter);
        assert!(app.ask.is_none(), "plain option should submit immediately");
        assert_eq!(app.focus, Focus::Chat);
        let body = rx.try_recv().expect("answer");
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(v["selected"][0]["id"], "retry");
        assert!(
            app.rows.iter().any(|r| matches!(r, Row::Meta(s) if s.contains("重試"))),
            "transcript should record the pick"
        );
    }

    #[test]
    fn ask_input_option_requires_typed_value() {
        let mut app = test_app();
        let mut rx = app.ask_hub.register();
        app.apply_event(sample_ask(false));
        press(&mut app, KeyCode::Down);
        press(&mut app, KeyCode::Enter);
        assert!(app.ask.as_ref().unwrap().filling, "input option opens the field");
        press(&mut app, KeyCode::Enter);
        assert!(app.ask.is_some(), "empty fill-in must not submit");
        assert_eq!(app.status, "請填寫「自填」");
        for c in "用SQLite".chars() {
            press(&mut app, KeyCode::Char(c));
        }
        press(&mut app, KeyCode::Enter);
        assert!(app.ask.is_none());
        let body = rx.try_recv().expect("answer");
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(v["selected"][0]["id"], "other");
        assert_eq!(v["selected"][0]["value"], "用SQLite");
    }

    #[test]
    fn ask_mouse_click_selects_plain_option() {
        let mut app = test_app();
        let mut rx = app.ask_hub.register();
        app.apply_event(sample_ask(false));
        app.hits = vec![(Rect::new(0, 2, 20, 1), Hit::AskOption(0))];
        let mut opts = TuiOptions {
            model: "grok-4.6".into(),
            events: PathBuf::from("events.jsonl"),
            workspace: PathBuf::from("."),
            max_turns: 0,
            web_search: false,
            reasoning_effort: ReasoningEffort::High,
        };
        handle_mouse(
            &mut app,
            &mut opts,
            MouseEventKind::Down(MouseButton::Left),
            1,
            2,
            KeyModifiers::NONE,
        );
        assert!(app.ask.is_none());
        let body = rx.try_recv().expect("answer");
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(v["selected"][0]["id"], "retry");
    }

    #[test]
    fn parked_event_does_not_dismiss_current_ask() {
        let mut app = test_app();
        app.current_id = "cur".into();
        app.session.id = "cur".into();
        let mut rx = app.ask_hub.register();
        app.apply_event(sample_ask(false));
        app.parked.insert("parked".into(), parked_stub("parked"));
        app.route_event(AgentEvent::ModelDelta {
            meta: crate::events::EventMeta {
                ts: chrono::Utc::now(),
                agent_name: "root".into(),
                run_id: "parked".into(),
                parent_run_id: None,
            },
            text: "bg".into(),
        });
        assert!(app.ask.is_some(), "overlay must survive parked-session events");
        assert_eq!(app.focus, Focus::Ask);
        assert!(
            rx.try_recv().is_err(),
            "current waiter must not be cancelled by another session's events"
        );
    }

    #[test]
    fn switch_to_cancels_open_ask() {
        let mut app = test_app();
        app.current_id = "a".into();
        app.session.id = "a".into();
        let mut rx = app.ask_hub.register();
        app.apply_event(sample_ask(false));
        app.parked.insert("b".into(), parked_stub("b"));
        app.switch_to("b");
        assert!(app.ask.is_none());
        let body = rx.try_recv().expect("switch should cancel the waiter");
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(v["cancelled"], true);
    }

    #[test]
    fn parked_ask_user_cancels_that_hub_only() {
        let mut app = test_app();
        app.current_id = "cur".into();
        app.session.id = "cur".into();
        let parked_hub = AskUserHub::new();
        let mut parked_rx = parked_hub.register();
        app.ask_hubs.insert("parked".into(), parked_hub);
        let mut cur_rx = app.ask_hub.register();
        app.apply_event(sample_ask(false));
        app.parked.insert("parked".into(), parked_stub("parked"));
        app.route_event(AgentEvent::AskUser {
            meta: crate::events::EventMeta {
                ts: chrono::Utc::now(),
                agent_name: "root".into(),
                run_id: "parked".into(),
                parent_run_id: None,
            },
            question: "那邊".into(),
            allow_multiple: false,
            options: vec![crate::ask::Choice {
                id: "x".into(),
                label: "X".into(),
                input: false,
            }],
        });
        assert!(app.ask.is_some(), "current overlay stays");
        assert!(cur_rx.try_recv().is_err(), "current waiter stays");
        let body = parked_rx.try_recv().expect("parked ask_user must not hang");
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(v["cancelled"], true);
        let parked = app.parked.get("parked").unwrap();
        assert!(
            parked
                .rows
                .iter()
                .any(|r| matches!(r, Row::Meta(s) if s.contains("那邊"))),
            "parked transcript should record the cancelled question"
        );
    }

    #[test]
    fn paste_image_path_attaches_pending_and_sends_with_empty_text() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("drop.jpg");
        let img = crate::vision::from_rgba(4, 4, vec![0, 0, 255, 255].repeat(16)).unwrap();
        crate::vision::save_jpeg(&src, &img).unwrap();
        let mut app = test_app();
        app.session.workspace = dir.path().to_path_buf();
        app.paste_text_or_images(&format!("\"{}\"", src.display()));
        assert_eq!(app.pending.len(), 1, "{:?}", app.pending);
        assert!(app.pending[0].starts_with(".groka/inbox/"));
        assert!(app.edit.is_empty());
        let turn = app.take_turn().unwrap();
        assert!(turn.text.is_empty());
        assert_eq!(turn.images.len(), 1);
        assert!(app.pending.is_empty());
    }

    #[test]
    fn paste_from_terminal_text_goes_to_composer() {
        let mut app = test_app();
        app.paste_from_terminal("hello");
        assert_eq!(app.edit.text, "hello");
        assert!(app.pending.is_empty());
    }

    #[test]
    fn paste_from_terminal_image_path_attaches_like_drop() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("drop.jpg");
        let img = crate::vision::from_rgba(4, 4, vec![0, 0, 255, 255].repeat(16)).unwrap();
        crate::vision::save_jpeg(&src, &img).unwrap();
        let mut app = test_app();
        app.session.workspace = dir.path().to_path_buf();
        app.paste_from_terminal(&format!("\"{}\"", src.display()));
        assert_eq!(app.pending.len(), 1, "{:?}", app.pending);
        assert!(app.edit.is_empty(), "{}", app.edit.text);
    }

    #[test]
    fn paste_key_includes_ctrl_v_and_legacy_syn() {
        assert!(is_paste_key(KeyCode::Char('v'), KeyModifiers::CONTROL));
        assert!(is_paste_key(KeyCode::Char('V'), KeyModifiers::CONTROL));
        assert!(is_paste_key(KeyCode::Char('\u{16}'), KeyModifiers::NONE));
        assert!(is_paste_key(KeyCode::Insert, KeyModifiers::SHIFT));
        assert!(!is_paste_key(KeyCode::Char('v'), KeyModifiers::NONE));
        assert!(!is_paste_key(KeyCode::Char('x'), KeyModifiers::CONTROL));
    }

    #[test]
    fn paste_image_button_hit_wins_over_composer() {
        let hits = vec![
            (Rect::new(0, 0, 40, 8), Hit::Composer),
            (Rect::new(16, 0, 12, 1), Hit::PasteImage),
        ];
        assert_eq!(hit_at(&hits, 18, 0), Some(Hit::PasteImage));
        assert_eq!(hit_at(&hits, 2, 2), Some(Hit::Composer));
    }

    #[test]
    fn paste_image_click_attaches_or_explains() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = test_app();
        app.session.workspace = dir.path().to_path_buf();
        app.hits = vec![(Rect::new(16, 0, 12, 1), Hit::PasteImage)];
        let (mut opts, _sink, _tx) = dummy_key_env();
        handle_mouse(
            &mut app,
            &mut opts,
            MouseEventKind::Down(MouseButton::Left),
            18,
            0,
            KeyModifiers::NONE,
        );
        assert!(
            !app.pending.is_empty()
                || app.status.contains("圖片")
                || app.status.contains("剪貼簿"),
            "button must paste an image or say the clipboard is empty: {}",
            app.status
        );
    }

    #[test]
    fn decoded_dib_attaches_through_paste_pipeline() {
        let dir = tempfile::tempdir().unwrap();
        let mut dib = vec![0u8; 40 + 16];
        dib[0..4].copy_from_slice(&40u32.to_le_bytes());
        dib[4..8].copy_from_slice(&2i32.to_le_bytes());
        dib[8..12].copy_from_slice(&2i32.to_le_bytes());
        dib[12..14].copy_from_slice(&1u16.to_le_bytes());
        dib[14..16].copy_from_slice(&32u16.to_le_bytes());
        dib[20..24].copy_from_slice(&16u32.to_le_bytes());
        for px in dib[40..].chunks_exact_mut(4) {
            px.copy_from_slice(&[0, 0, 255, 255]);
        }
        let img = crate::clipimg::from_dib_bytes(&dib).expect("dib");
        let mut app = test_app();
        app.session.workspace = dir.path().to_path_buf();
        let rel = crate::vision::save_user_image(&app.session.workspace, &img).unwrap();
        assert!(app.attach_pending(rel));
        assert_eq!(app.pending.len(), 1);
        assert!(app.pending[0].starts_with(".groka/inbox/"));
    }

    fn dummy_key_env() -> (
        TuiOptions,
        Arc<FanoutSink>,
        mpsc::UnboundedSender<(String, crate::agent::RunOutcome)>,
    ) {
        let (tx, _rx) = mpsc::unbounded_channel();
        let opts = TuiOptions {
            model: "grok-4.6".into(),
            events: PathBuf::from("events.jsonl"),
            workspace: PathBuf::from("."),
            max_turns: 0,
            web_search: false,
            reasoning_effort: ReasoningEffort::High,
        };
        (opts, Arc::new(FanoutSink { sinks: vec![] }), tx)
    }

    #[test]
    fn legacy_ctrl_v_syn_does_not_insert_control_char() {
        let mut app = test_app();
        app.edit = Edit::at_end("keep".into());
        let (mut opts, sink, tx) = dummy_key_env();
        handle_key(
            &mut app,
            &mut opts,
            KeyCode::Char('\u{16}'),
            KeyModifiers::NONE,
            &sink,
            &tx,
        );
        assert!(
            !app.edit.text.contains('\u{16}'),
            "ctrl-v SYN must paste clipboard, not insert U+0016: {}",
            app.edit.text
        );
    }

    #[test]
    fn copy_selection_of_row_does_not_quit() {
        let mut app = test_app();
        app.push(Row::User("copy me".into()));
        app.chat_sel = ChatSel::Row(0);
        let (tx, _rx) = mpsc::unbounded_channel();
        let mut opts = TuiOptions {
            model: "grok-4.6".into(),
            events: PathBuf::from("events.jsonl"),
            workspace: PathBuf::from("."),
            max_turns: 0,
            web_search: false,
            reasoning_effort: ReasoningEffort::High,
        };
        let sink = Arc::new(FanoutSink { sinks: vec![] });
        let quit = handle_key(
            &mut app,
            &mut opts,
            KeyCode::Char('c'),
            KeyModifiers::CONTROL,
            &sink,
            &tx,
        );
        assert!(!quit, "ctrl-c with a selected row must copy, not quit");
    }

    #[test]
    fn tool_started_shows_command_and_path() {
        let cmd = tool_started_line("run_command", &serde_json::json!({"command": "git status"}));
        assert!(cmd.contains("$ git status"), "{cmd}");
        let bg = tool_started_line(
            "run_background",
            &serde_json::json!({"command": "npm run dev"}),
        );
        assert!(bg.contains("$ npm run dev"), "{bg}");
        let kill = tool_started_line("kill_background", &serde_json::json!({"name": "dev"}));
        assert!(kill.contains("dev"), "{kill}");
        let path = tool_started_line("write_file", &serde_json::json!({"path": "src/a.rs"}));
        assert!(path.contains("src/a.rs"), "{path}");
        let ask = tool_started_line("ask_user", &serde_json::json!({"question": "挑一個"}));
        assert!(ask.contains("挑一個"), "{ask}");
    }

    #[test]
    fn diff_row_colors_plus_and_minus() {
        let rendered: String = diff_lines("--- a/a.txt\n+++ b/a.txt\n@@ -1 +1 @@\n-old\n+new\n")
            .iter()
            .map(|l| format!("{l:?}"))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(rendered.contains("-old"), "{rendered}");
        assert!(rendered.contains("+new"), "{rendered}");
    }

    #[test]
    fn model_delta_appends_then_finished_does_not_duplicate() {
        let mut app = test_app();
        let meta = crate::events::EventMeta {
            ts: chrono::Utc::now(),
            agent_name: "root".into(),
            run_id: "r".into(),
            parent_run_id: None,
        };
        app.apply_event(AgentEvent::ModelDelta {
            meta: meta.clone(),
            text: "Hel".into(),
        });
        app.apply_event(AgentEvent::ModelDelta {
            meta: meta.clone(),
            text: "lo".into(),
        });
        app.apply_event(AgentEvent::ModelFinished {
            meta,
            text: "Hello".into(),
            finish: "stop".into(),
            input_tokens: 10,
            cached_tokens: 0,
        });
        let agents: Vec<&str> = app
            .rows
            .iter()
            .filter_map(|r| match r {
                Row::Agent(s) => Some(s.text.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(agents, vec!["Hello"]);
    }

    fn test_app() -> App {
        App {
            rows: vec![],
            edit: Edit::default(),
            status: String::new(),
            cache: String::new(),
            child_count: 0,
            running: false,
            awaiting: false,
            logged_in: true,
            auth_path: PathBuf::from("missing-xai-auth.json"),
            login_ui: LoginUi::Idle,
            login_gen: 0,
            want_login: false,
            scroll: 0,
            stick_bottom: true,
            send_mode: SendMode::Queue,
            queue: VecDeque::new(),
            inbox_tx: None,
            knobs: Arc::new(Mutex::new(SessionKnobs {
                model: "grok-4.6".into(),
                reasoning_effort: ReasoningEffort::High,
                send_reasoning: true,
                server_tools: vec![],
            })),
            focus: Focus::Chat,
            setting_field: SettingField::Model,
            settings: None,
            drag: None,
            hits: Vec::new(),
            area: Rect::default(),
            streaming: false,
            composer_inner: Rect::default(),
            composer_vscroll: 0,
            input_dragging: false,
            catalog: ModelCatalog::default(),
            catalog_status: CatalogStatus::Idle,
            drop: None,
            drop_cursor: 0,
            drop_scroll: 0,
            want_catalog: false,
            open_tool: None,
            seal_tools: false,
            activity: String::new(),
            tick: 0,
            current_id: "s".into(),
            session: dummy_session(),
            parked: HashMap::new(),
            sessions: vec![],
            store: None,
            launch_workspace: PathBuf::from("."),
            sidebar_ids: Vec::new(),
            rename: None,
            rename_inner: Rect::default(),
            work_started: None,
            queue_edit: None,
            composer_stash: None,
            pending: Vec::new(),
            chat_sel: ChatSel::None,
            preview: HashMap::new(),
            picker: None,
            image_proto: HashMap::new(),
            image_hits: Vec::new(),
            children: Vec::new(),
            monitors: Vec::new(),
            backgrounds: Vec::new(),
            inspector: None,
            inspector_scroll: 0,
            ask_hub: AskUserHub::new(),
            ask_hubs: HashMap::new(),
            ask: None,
            ask_fill_inner: Rect::default(),
            ask_passive: false,
            workspace_pick: None,
        }
    }

    fn parked_stub(id: &str) -> ParkedChat {
        let mut session = dummy_session();
        session.id = id.to_string();
        ParkedChat {
            session,
            rows: vec![],
            status: "工作中".into(),
            cache: String::new(),
            child_count: 0,
            running: true,
            awaiting: false,
            scroll: 0,
            stick_bottom: true,
            queue: VecDeque::new(),
            inbox_tx: None,
            streaming: false,
            open_tool: None,
            seal_tools: false,
            activity: String::new(),
            edit: Edit::default(),
            work_started: None,
            queue_edit: None,
            composer_stash: None,
            pending: Vec::new(),
            children: Vec::new(),
            monitors: Vec::new(),
            backgrounds: Vec::new(),
            inspector: None,
            inspector_scroll: 0,
        }
    }

    fn test_meta() -> crate::events::EventMeta {
        crate::events::EventMeta {
            ts: chrono::Utc::now(),
            agent_name: "root".into(),
            run_id: "r".into(),
            parent_run_id: None,
        }
    }

    #[test]
    fn notice_goes_to_chat_not_composer() {
        let mut app = test_app();
        app.edit = Edit::at_end("keep me".into());
        app.apply_event(AgentEvent::Notice {
            meta: test_meta(),
            message: "快取命中 10%（目標 ≥90%）turn=2 cached=10/1000".into(),
        });
        assert_eq!(app.edit.text, "keep me");
        match app.rows.last() {
            Some(Row::Meta(s)) => assert!(s.contains("快取命中"), "{s}"),
            _ => panic!("expected meta row in chat, not the composer"),
        }
    }

    #[test]
    fn tools_in_one_turn_collapse_to_one_row() {
        let mut app = test_app();
        let meta = test_meta();
        app.apply_event(AgentEvent::ToolStarted {
            meta: meta.clone(),
            call_id: "1".into(),
            name: "list_dir".into(),
            args: serde_json::json!({"path": "."}),
            kind: "function".into(),
        });
        app.apply_event(AgentEvent::ToolStarted {
            meta,
            call_id: "2".into(),
            name: "run_command".into(),
            args: serde_json::json!({"command": "git status"}),
            kind: "function".into(),
        });
        assert_eq!(app.rows.len(), 1);
        let Row::Tools(g) = &app.rows[0] else {
            panic!("expected tool group");
        };
        assert!(!g.expanded, "groups start collapsed");
        assert_eq!(g.calls.len(), 2);
        let vis = chat_logical_rows(&mut app, 80);
        assert_eq!(vis.len(), 1, "collapsed group is one clickable line");
        assert_eq!(vis[0].hit, Some(Hit::ToolGroup(0)));
        assert!(
            format!("{:?}", vis[0].line).contains("2 個工具"),
            "{:?}",
            vis[0].line
        );
    }

    #[test]
    fn reasoning_collapses_like_tools_and_stays_out_of_answer() {
        let mut app = test_app();
        let meta = test_meta();
        app.apply_event(AgentEvent::ReasoningDelta {
            meta: meta.clone(),
            text: "use ".into(),
        });
        app.apply_event(AgentEvent::ReasoningDelta {
            meta: meta.clone(),
            text: "energy".into(),
        });
        app.apply_event(AgentEvent::ModelDelta {
            meta: meta.clone(),
            text: "42".into(),
        });
        assert_eq!(app.rows.len(), 2);
        let Row::Think(t) = &app.rows[0] else {
            panic!("expected think row");
        };
        assert!(!t.expanded, "thinking starts collapsed");
        assert!(t.done, "answer seals the think block");
        assert_eq!(t.text, "use energy");
        let Row::Agent(s) = &app.rows[1] else {
            panic!("expected agent row");
        };
        assert_eq!(s.text, "42");
        let vis = chat_logical_rows(&mut app, 80);
        assert_eq!(vis.len(), 2, "collapsed think is one line plus the answer");
        assert_eq!(vis[0].hit, Some(Hit::Think(0)));
        assert!(format!("{:?}", vis[0].line).contains("思考"), "{:?}", vis[0].line);
        if let Some(Row::Think(t)) = app.rows.get_mut(0) {
            t.expanded = true;
        }
        let vis = chat_logical_rows(&mut app, 80);
        let rendered: String = vis
            .iter()
            .map(|l| format!("{:?}", l.line))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(rendered.contains("use energy"), "{rendered}");
    }

    #[test]
    fn new_turn_starts_a_fresh_think_row() {
        let mut app = test_app();
        let meta = test_meta();
        app.apply_event(AgentEvent::ReasoningDelta {
            meta: meta.clone(),
            text: "first".into(),
        });
        app.apply_event(AgentEvent::TurnStarted {
            meta: meta.clone(),
            turn: 2,
        });
        app.apply_event(AgentEvent::ReasoningDelta {
            meta,
            text: "second".into(),
        });
        let thinks: Vec<&str> = app
            .rows
            .iter()
            .filter_map(|r| match r {
                Row::Think(t) => Some(t.text.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(thinks, ["first", "second"]);
    }

    #[test]
    fn server_search_progress_coalesces_into_one_call() {
        let mut app = test_app();
        let meta = test_meta();
        app.apply_event(AgentEvent::ServerToolObserved {
            meta: meta.clone(),
            kind: "response.web_search_call.in_progress".into(),
            payload: serde_json::json!({"action":{"query":"Falcon 9"}}),
        });
        app.apply_event(AgentEvent::ServerToolObserved {
            meta: meta.clone(),
            kind: "response.web_search_call.searching".into(),
            payload: serde_json::json!({"action":{"query":"Falcon 9"}}),
        });
        app.apply_event(AgentEvent::ServerToolObserved {
            meta,
            kind: "web_search_call".into(),
            payload: serde_json::json!({"status":"in_progress","action":{"query":"Falcon 9"}}),
        });
        assert_eq!(app.rows.len(), 1);
        let Row::Tools(g) = &app.rows[0] else {
            panic!("expected tool group");
        };
        assert_eq!(g.calls.len(), 1, "progress events must not spawn extra rows");
        assert_eq!(g.calls[0].name, "web_search");
        assert_eq!(g.calls[0].phase, "進行中");
        assert!(app.activity.contains("Falcon 9"), "{}", app.activity);
        assert!(app.activity.contains("搜尋中") || app.activity.contains("進行中"), "{}", app.activity);
    }

    #[test]
    fn expanding_a_group_lists_each_call() {
        let mut app = test_app();
        let meta = test_meta();
        app.apply_event(AgentEvent::ToolStarted {
            meta: meta.clone(),
            call_id: "1".into(),
            name: "list_dir".into(),
            args: serde_json::json!({"path": "."}),
            kind: "function".into(),
        });
        app.apply_event(AgentEvent::ToolStarted {
            meta,
            call_id: "2".into(),
            name: "run_command".into(),
            args: serde_json::json!({"command": "echo hi"}),
            kind: "function".into(),
        });
        let Row::Tools(g) = &mut app.rows[0] else {
            panic!("expected tool group");
        };
        g.expanded = true;
        let vis = chat_logical_rows(&mut app, 80);
        assert_eq!(vis.len(), 3);
        assert_eq!(vis[1].hit, Some(Hit::ToolItem(0, 0)));
        assert_eq!(vis[2].hit, Some(Hit::ToolItem(0, 1)));
    }

    #[test]
    fn file_changed_attaches_to_the_open_call() {
        let mut app = test_app();
        let meta = test_meta();
        app.apply_event(AgentEvent::ToolStarted {
            meta: meta.clone(),
            call_id: "1".into(),
            name: "write_file".into(),
            args: serde_json::json!({"path": "a.txt"}),
            kind: "function".into(),
        });
        app.apply_event(AgentEvent::FileChanged {
            meta,
            path: "a.txt".into(),
            kind: "create".into(),
            diff: "--- a/a.txt\n+++ b/a.txt\n+hello\n".into(),
        });
        let Row::Tools(g) = &app.rows[0] else {
            panic!("expected tool group");
        };
        assert_eq!(g.calls[0].files.len(), 1);
        assert_eq!(g.calls[0].files[0].path, "a.txt");
        assert!(g.calls[0].files[0].diff.contains("+hello"));
    }

    #[test]
    fn tool_detail_shows_command_and_diff() {
        let call = ToolCall {
            name: "run_command".into(),
            args: serde_json::json!({"command": "git status"}),
            output: serde_json::json!({
                "exit_code": 0,
                "stdout": "ok",
                "stderr": ""
            })
            .to_string(),
            files: vec![FileChange {
                path: "a.txt".into(),
                kind: "modify".into(),
                diff: "--- a/a.txt\n+++ b/a.txt\n@@ -1 +1 @@\n-old\n+new\n".into(),
            }],
            done: true,
            phase: "完成".into(),
        };
        let rendered: String = call_detail_lines(&call)
            .iter()
            .map(|l| format!("{l:?}"))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(rendered.contains("$ git status"), "{rendered}");
        assert!(rendered.contains("exit 0"), "{rendered}");
        assert!(rendered.contains("-old"), "{rendered}");
        assert!(rendered.contains("+new"), "{rendered}");
        assert!(rendered.contains("a.txt"), "{rendered}");
    }

    #[test]
    fn dismiss_closes_panel_then_collapses_groups() {
        let mut app = test_app();
        app.push(Row::Tools(ToolGroup {
            calls: vec![ToolCall {
                name: "list_dir".into(),
                args: serde_json::json!({"path": "."}),
                output: String::new(),
                files: Vec::new(),
                done: true,
                phase: "完成".into(),
            }],
            expanded: true,
        }));
        app.open_tool = Some((0, 0));
        assert!(app.dismiss_tool_ui());
        assert!(app.open_tool.is_none());
        let Row::Tools(g) = &app.rows[0] else {
            panic!("expected tool group");
        };
        assert!(g.expanded, "first dismiss only closes the detail panel");
        assert!(app.dismiss_tool_ui());
        let Row::Tools(g) = &app.rows[0] else {
            panic!("expected tool group");
        };
        assert!(!g.expanded);
        assert!(!app.dismiss_tool_ui());
    }

    #[test]
    fn dismiss_collapses_expanded_think() {
        let mut app = test_app();
        app.push(Row::Think(Think {
            text: "use energy".into(),
            expanded: true,
            done: true,
            ..Default::default()
        }));
        assert!(app.dismiss_tool_ui());
        let Row::Think(t) = &app.rows[0] else {
            panic!("expected think row");
        };
        assert!(!t.expanded);
        assert!(!app.dismiss_tool_ui());
    }

    #[test]
    fn new_turn_starts_a_fresh_tool_group() {
        let mut app = test_app();
        let meta = test_meta();
        app.apply_event(AgentEvent::ToolStarted {
            meta: meta.clone(),
            call_id: "1".into(),
            name: "list_dir".into(),
            args: serde_json::json!({"path": "."}),
            kind: "function".into(),
        });
        app.apply_event(AgentEvent::TurnStarted {
            meta: meta.clone(),
            turn: 2,
        });
        app.apply_event(AgentEvent::ToolStarted {
            meta,
            call_id: "2".into(),
            name: "run_command".into(),
            args: serde_json::json!({"command": "ls"}),
            kind: "function".into(),
        });
        assert_eq!(app.rows.len(), 2);
        assert!(matches!(&app.rows[0], Row::Tools(_)));
        assert!(matches!(&app.rows[1], Row::Tools(_)));
    }

    #[test]
    fn dismiss_hit_sits_above_chat() {
        let hits = vec![
            (Rect::new(0, 0, 20, 10), Hit::Chat),
            (Rect::new(0, 0, 20, 10), Hit::DismissTool),
            (Rect::new(0, 4, 20, 1), Hit::ToolItem(0, 1)),
            (Rect::new(4, 2, 12, 6), Hit::ToolPanel),
        ];
        assert_eq!(hit_at(&hits, 1, 1), Some(Hit::DismissTool));
        assert_eq!(hit_at(&hits, 2, 4), Some(Hit::ToolItem(0, 1)));
        assert_eq!(hit_at(&hits, 5, 3), Some(Hit::ToolPanel));
    }

    #[test]
    fn edit_inserts_in_the_middle() {
        let mut e = Edit::at_end("ac".into());
        e.caret = 1;
        e.insert_char('b');
        assert_eq!(e.text, "abc");
        assert_eq!(e.caret, 2);
    }

    #[test]
    fn edit_arrows_move_by_unicode_char() {
        let mut e = Edit::at_end("你好".into());
        e.move_left(false);
        assert_eq!(e.caret, 1);
        e.backspace();
        assert_eq!(e.text, "好");
        assert_eq!(e.caret, 0);
    }

    #[test]
    fn edit_select_all_then_backspace_clears() {
        let mut e = Edit::at_end("hello".into());
        e.select_all();
        assert_eq!(e.selected_text().as_deref(), Some("hello"));
        e.backspace();
        assert!(e.is_empty());
        assert_eq!(e.caret, 0);
    }

    #[test]
    fn edit_shift_right_selects_then_typing_replaces() {
        let mut e = Edit::at_end("abcd".into());
        e.home(false);
        e.move_right(true);
        e.move_right(true);
        assert_eq!(e.selected_text().as_deref(), Some("ab"));
        e.insert_char('z');
        assert_eq!(e.text, "zcd");
    }

    #[test]
    fn click_maps_cjk_display_columns() {
        let inner = Rect::new(10, 5, 40, 1);
        assert_eq!(click_to_index("你好", inner, 0, 10, 5), 0);
        assert_eq!(click_to_index("你好", inner, 0, 11, 5), 1);
        assert_eq!(click_to_index("你好", inner, 0, 12, 5), 1);
        assert_eq!(click_to_index("你好", inner, 0, 14, 5), 2);
    }

    #[test]
    fn wrap_lines_breaks_on_width() {
        let lines = wrap_lines("abcd", 2);
        assert_eq!(lines, vec![(0, 2), (2, 4)]);
    }

    #[test]
    fn route_event_ignores_other_session() {
        let mut app = test_app();
        app.current_id = "aaa".into();
        app.session.id = "aaa".into();
        let other = crate::events::EventMeta {
            ts: chrono::Utc::now(),
            agent_name: "root".into(),
            run_id: "bbb".into(),
            parent_run_id: None,
        };
        app.route_event(AgentEvent::ModelDelta {
            meta: other,
            text: "should not appear".into(),
        });
        assert!(app.rows.is_empty());
    }

    #[test]
    fn route_event_applies_current_session() {
        let mut app = test_app();
        app.current_id = "aaa".into();
        app.session.id = "aaa".into();
        let meta = crate::events::EventMeta {
            ts: chrono::Utc::now(),
            agent_name: "root".into(),
            run_id: "aaa".into(),
            parent_run_id: None,
        };
        app.route_event(AgentEvent::ModelDelta {
            meta,
            text: "hi".into(),
        });
        assert_eq!(app.rows.len(), 1);
        assert!(matches!(&app.rows[0], Row::Agent(s) if s.text == "hi"));
    }

    #[test]
    fn parked_session_receives_events() {
        let mut app = test_app();
        app.current_id = "cur".into();
        app.session.id = "cur".into();
        let mut other = dummy_session();
        other.id = "parked".into();
        app.parked.insert(
            "parked".into(),
            ParkedChat {
                session: other,
                rows: vec![],
                status: "工作中".into(),
                cache: String::new(),
                child_count: 0,
                running: true,
                awaiting: false,
                scroll: 0,
                stick_bottom: true,
                queue: VecDeque::new(),
                inbox_tx: None,
                streaming: false,
                open_tool: None,
                seal_tools: false,
                activity: String::new(),
                edit: Edit::default(),
                work_started: None,
                queue_edit: None,
                composer_stash: None,
            pending: Vec::new(),
            children: Vec::new(),
            monitors: Vec::new(),
            backgrounds: Vec::new(),
            inspector: None,
            inspector_scroll: 0,
            },
        );
        let meta = crate::events::EventMeta {
            ts: chrono::Utc::now(),
            agent_name: "root".into(),
            run_id: "parked".into(),
            parent_run_id: None,
        };
        app.route_event(AgentEvent::ModelDelta {
            meta,
            text: "bg".into(),
        });
        assert!(app.rows.is_empty(), "current chat stays empty");
        let parked = app.parked.get("parked").unwrap();
        assert!(matches!(&parked.rows[0], Row::Agent(s) if s.text == "bg"));
    }

    #[test]
    fn session_named_updates_sidebar_title() {
        let mut app = test_app();
        app.current_id = "s".into();
        let meta = crate::events::EventMeta {
            ts: chrono::Utc::now(),
            agent_name: "root".into(),
            run_id: "s".into(),
            parent_run_id: None,
        };
        app.route_event(AgentEvent::SessionNamed {
            meta,
            name: "修 login race".into(),
        });
        assert_eq!(app.session.name, "修 login race");
        assert!(app.session.named);
    }

    #[test]
    fn switch_to_parks_current_rows() {
        let mut app = test_app();
        app.current_id = "a".into();
        app.session.id = "a".into();
        app.session.name = "A".into();
        app.push(Row::User("from a".into()));
        let mut b = dummy_session();
        b.id = "b".into();
        b.name = "B".into();
        app.parked.insert(
            "b".into(),
            ParkedChat {
                session: b,
                rows: vec![Row::User("from b".into())],
                status: "待命".into(),
                cache: String::new(),
                child_count: 0,
                running: false,
                awaiting: false,
                scroll: 0,
                stick_bottom: true,
                queue: VecDeque::new(),
                inbox_tx: None,
                streaming: false,
                open_tool: None,
                seal_tools: false,
                activity: String::new(),
                edit: Edit::default(),
                work_started: None,
                queue_edit: None,
                composer_stash: None,
            pending: Vec::new(),
            children: Vec::new(),
            monitors: Vec::new(),
            backgrounds: Vec::new(),
            inspector: None,
            inspector_scroll: 0,
            },
        );
        app.switch_to("b");
        assert_eq!(app.current_id, "b");
        assert!(matches!(&app.rows[0], Row::User(u) if u.text == "from b"));
        let parked_a = app.parked.get("a").unwrap();
        assert!(matches!(&parked_a.rows[0], Row::User(u) if u.text == "from a"));
    }

    #[test]
    fn new_chat_opens_workspace_picker_then_creates_on_confirm() {
        let mut app = test_app();
        app.session.named = true;
        app.session.name = "舊的".into();
        app.push(Row::User("hello".into()));
        app.new_chat();
        assert!(app.workspace_pick.is_some());
        assert_eq!(app.focus, Focus::Workspace);
        assert_eq!(app.session.name, "舊的", "must not mint a chat before the folder is chosen");
        assert!(app.parked.is_empty());
        app.confirm_workspace_pick();
        assert!(app.workspace_pick.is_none());
        assert_eq!(app.session.name, "新對話");
        assert!(!app.session.named);
        assert!(app.rows.iter().all(|r| matches!(r, Row::Meta(_))));
        assert_eq!(app.parked.len(), 1);
    }

    #[test]
    fn new_chat_cancel_leaves_current_session() {
        let mut app = test_app();
        app.session.named = true;
        app.session.name = "舊的".into();
        app.push(Row::User("hello".into()));
        app.new_chat();
        app.cancel_workspace_pick();
        assert!(app.workspace_pick.is_none());
        assert_eq!(app.session.name, "舊的");
        assert!(app.parked.is_empty());
        assert_eq!(app.focus, Focus::Chat);
    }

    #[test]
    fn blank_draft_confirm_sets_workspace_without_extra_session() {
        let dir = tempfile::tempdir().unwrap();
        let sub = dir.path().join("proj");
        std::fs::create_dir(&sub).unwrap();
        let mut app = test_app();
        app.session.workspace = dir.path().to_path_buf();
        app.launch_workspace = dir.path().to_path_buf();
        let id = app.session.id.clone();
        app.new_chat();
        app.enter_workspace_dir(sub.clone());
        app.confirm_workspace_pick();
        assert_eq!(app.session.id, id);
        assert!(app.parked.is_empty());
        assert_eq!(app.session.workspace, folderpick::normalize(&sub));
        assert_eq!(app.launch_workspace, folderpick::normalize(&sub));
    }

    #[test]
    fn workspace_picker_create_dir_and_mouse_hits() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = test_app();
        app.session.workspace = dir.path().to_path_buf();
        app.new_chat();
        let typed = folderpick::display_path(&dir.path().join("created"));
        if let Some(p) = app.workspace_pick.as_mut() {
            p.edit = Edit::at_end(typed);
        }
        app.create_workspace_dir();
        let created = dir.path().join("created");
        assert!(created.is_dir(), "{}", created.display());
        let cwd = app.workspace_pick.as_ref().unwrap().view.cwd.clone();
        assert_eq!(cwd, folderpick::normalize(&created));

        let hits = vec![
            (Rect::new(0, 0, 40, 20), Hit::WsPanel),
            (Rect::new(2, 4, 20, 1), Hit::WsEntry(1)),
            (Rect::new(2, 18, 10, 1), Hit::WsConfirm),
            (Rect::new(13, 18, 10, 1), Hit::WsCreate),
            (Rect::new(24, 18, 6, 1), Hit::WsCancel),
        ];
        assert_eq!(hit_at(&hits, 3, 4), Some(Hit::WsEntry(1)));
        assert_eq!(hit_at(&hits, 4, 18), Some(Hit::WsConfirm));
        assert_eq!(hit_at(&hits, 14, 18), Some(Hit::WsCreate));
        assert_eq!(hit_at(&hits, 25, 18), Some(Hit::WsCancel));
    }

    #[test]
    fn workspace_picker_type_filters_listing() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("src")).unwrap();
        std::fs::write(dir.path().join("README.md"), b"x").unwrap();
        let mut app = test_app();
        app.session.workspace = dir.path().to_path_buf();
        app.new_chat();
        let sep = std::path::MAIN_SEPARATOR;
        let typed = format!("{}{sep}s", folderpick::display_path(dir.path()));
        if let Some(p) = app.workspace_pick.as_mut() {
            p.edit = Edit::at_end(typed);
        }
        app.sync_workspace_pick();
        let names: Vec<String> = app
            .workspace_pick
            .as_ref()
            .unwrap()
            .view
            .entries
            .iter()
            .filter(|e| !e.is_parent)
            .map(|e| e.name.clone())
            .collect();
        assert_eq!(names, ["src"], "{names:?}");
    }

    #[test]
    fn ctrl_n_opens_workspace_picker() {
        let mut app = test_app();
        app.session.named = true;
        app.session.name = "舊的".into();
        app.push(Row::User("x".into()));
        let (mut opts, sink, tx) = dummy_key_env();
        handle_key(
            &mut app,
            &mut opts,
            KeyCode::Char('n'),
            KeyModifiers::CONTROL,
            &sink,
            &tx,
        );
        assert!(app.workspace_pick.is_some());
        assert_eq!(app.focus, Focus::Workspace);
        assert_eq!(app.session.name, "舊的");
        assert!(app.parked.is_empty());
    }

    #[test]
    fn sidebar_hits_new_chat_and_session() {
        let hits = vec![
            (Rect::new(0, 0, 28, 1), Hit::NewChat),
            (Rect::new(0, 2, 28, 2), Hit::Session(0)),
        ];
        assert_eq!(hit_at(&hits, 1, 0), Some(Hit::NewChat));
        assert_eq!(hit_at(&hits, 3, 3), Some(Hit::Session(0)));
        assert_eq!(hit_at(&hits, 3, 1), None);
    }

    #[test]
    fn truncate_width_pads_and_clips() {
        let s = truncate_width("ab", 4);
        assert_eq!(s.chars().count(), 4);
        assert!(s.starts_with("ab"));
        let cjk = truncate_width("你好世界", 4);
        assert_eq!(Line::from(cjk.as_str()).width(), 4);
    }

    #[test]
    fn wrap_visual_breaks_on_newline_instead_of_squeezing() {
        // Line::from(&str) strips/splits newlines into adjacent spans (ratatui).
        // Embedded \\n in a Span is what a squeezed command dump looks like.
        let line = Line::from(Span::styled("a\nb\nc", Style::default()));
        let wrapped = wrap_visual(line, 40);
        assert_eq!(wrapped.len(), 3, "{wrapped:?}");
        let texts: Vec<String> = wrapped
            .iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.to_string())
                    .collect::<String>()
            })
            .collect();
        assert_eq!(texts, ["a", "b", "c"]);
    }

    #[test]
    fn diff_and_command_output_are_separate_colored_lines() {
        let lines = colored_output_lines(
            "diff --git a/a.txt b/a.txt\n--- a/a.txt\n+++ b/a.txt\n@@ -1 +1 @@\n-old\n+new\n M src/tui.rs\n",
            80,
        );
        assert!(lines.len() >= 6, "{}", lines.len());
        let rendered: Vec<String> = lines.iter().map(|l| format!("{l:?}")).collect();
        let blob = rendered.join("\n");
        assert!(blob.contains("-old"), "{blob}");
        assert!(blob.contains("+new"), "{blob}");
        let add = diff_lines("+hello\n-world\n");
        assert_eq!(add.len(), 2);
        assert!(format!("{:?}", add[0]).contains("hello"));
        assert!(format!("{:?}", add[1]).contains("world"));
    }

    #[test]
    fn ansi_command_output_keeps_color_codes() {
        let line = ansi_line("\u{1b}[32madded\u{1b}[0m \u{1b}[31mremoved\u{1b}[0m");
        let s = format!("{line:?}");
        assert!(s.contains("added"), "{s}");
        assert!(s.contains("removed"), "{s}");
        assert!(s.contains("63, 185, 80") || s.contains("DIFF_ADD") || s.contains("added"), "{s}");
    }

    #[test]
    fn expanded_tool_shows_diff_as_its_own_rows() {
        let mut app = test_app();
        app.push(Row::Tools(ToolGroup {
            calls: vec![ToolCall {
                name: "write_file".into(),
                args: serde_json::json!({"path": "a.txt"}),
                output: String::new(),
                files: vec![FileChange {
                    path: "a.txt".into(),
                    kind: "modify".into(),
                    diff: "--- a/a.txt\n+++ b/a.txt\n-old\n+new\n".into(),
                }],
                done: true,
                phase: "完成".into(),
            }],
            expanded: true,
        }));
        let vis = chat_logical_rows(&mut app, 80);
        let blob: String = vis.iter().map(|l| format!("{:?}", l.line)).collect::<Vec<_>>().join("\n");
        assert!(vis.len() > 2, "diff must not collapse into the header: {blob}");
        assert!(blob.contains("-old"), "{blob}");
        assert!(blob.contains("+new"), "{blob}");
    }

    #[test]
    fn rename_pins_title() {
        let mut app = test_app();
        app.session.name = "舊名".into();
        app.begin_rename("s");
        if let Some((_, edit)) = app.rename.as_mut() {
            edit.clear();
            edit.insert_str("手動標題");
        }
        app.commit_rename();
        assert_eq!(app.session.name, "手動標題");
        assert!(app.session.name_is_manual);
        assert!(app.rename.is_none());
    }

    #[test]
    fn delete_current_opens_a_fresh_chat() {
        let mut app = test_app();
        app.session.named = true;
        app.session.name = "要刪的".into();
        app.push(Row::User("bye".into()));
        let old = app.current_id.clone();
        app.delete_session(&old);
        assert_ne!(app.current_id, old);
        assert!(app.rows.iter().all(|r| matches!(r, Row::Meta(_))));
        assert!(app.parked.is_empty());
    }

    #[test]
    fn sidebar_action_hits_sit_above_session() {
        let hits = vec![
            (Rect::new(0, 2, 28, 2), Hit::Session(0)),
            (Rect::new(22, 2, 3, 1), Hit::RenameSession(0)),
            (Rect::new(25, 2, 3, 1), Hit::DeleteSession(0)),
        ];
        assert_eq!(hit_at(&hits, 23, 2), Some(Hit::RenameSession(0)));
        assert_eq!(hit_at(&hits, 26, 2), Some(Hit::DeleteSession(0)));
        assert_eq!(hit_at(&hits, 4, 3), Some(Hit::Session(0)));
    }

    #[test]
    fn agent_markdown_and_work_clock() {
        let lines = agent_lines(&AgentMsg {
            text: "# Hi\n\nuse **bold** and `code`".into(),
            work_ms: 3_400,
        });
        let blob: String = lines.iter().map(|l| format!("{l:?}")).collect::<Vec<_>>().join("\n");
        assert!(blob.contains("Hi"), "{blob}");
        assert!(blob.contains("bold"), "{blob}");
        assert!(blob.contains("code"), "{blob}");
        assert!(blob.contains("工作 3.4s"), "{blob}");
        assert!(blob.contains("grok"), "{blob}");
    }

    #[test]
    fn think_header_shows_elapsed() {
        let line = think_header_line(&Think {
            text: "plan".into(),
            expanded: false,
            done: true,
            elapsed_ms: 3_400,
            started: None,
        });
        let s = format!("{line:?}");
        assert!(s.contains("思考"), "{s}");
        assert!(s.contains("3.4s"), "{s}");
        assert!(s.contains("完成"), "{s}");
    }

    #[test]
    fn awaiting_input_stamps_work_on_last_agent() {
        let mut app = test_app();
        app.work_started = Some(Instant::now() - Duration::from_millis(50));
        app.push(Row::Agent(AgentMsg::new("done".into())));
        app.apply_event(AgentEvent::AwaitingInput { meta: test_meta() });
        let Row::Agent(a) = &app.rows[0] else {
            panic!("expected agent");
        };
        assert!(a.work_ms >= 50, "work_ms={}", a.work_ms);
        assert!(app.work_started.is_none());
    }

    #[test]
    fn agent_row_serde_keeps_legacy_string() {
        let legacy = serde_json::json!({"Agent": "hello"});
        let row: Row = serde_json::from_value(legacy).unwrap();
        assert!(matches!(row, Row::Agent(a) if a.text == "hello" && a.work_ms == 0));
        let plain = Row::Agent(AgentMsg::new("hello".into()));
        assert_eq!(
            serde_json::to_value(&plain).unwrap(),
            serde_json::json!({"Agent": "hello"})
        );
        let timed = Row::Agent(AgentMsg {
            text: "hello".into(),
            work_ms: 1_200,
        });
        let v = serde_json::to_value(&timed).unwrap();
        assert_eq!(v["Agent"]["text"], "hello");
        assert_eq!(v["Agent"]["work_ms"], 1200);
    }

    fn test_opts(model: &str, effort: ReasoningEffort) -> TuiOptions {
        TuiOptions {
            model: model.into(),
            events: PathBuf::from("events.jsonl"),
            workspace: PathBuf::from("."),
            max_turns: 0,
            web_search: false,
            reasoning_effort: effort,
        }
    }

    fn inject_two_model_catalog(app: &mut App) {
        app.catalog = crate::catalog::parse_catalog_value(&serde_json::json!({
            "data": [
                {
                    "id": "alpha",
                    "name": "Alpha",
                    "supportsReasoningEffort": true,
                    "reasoningEffort": "high",
                    "reasoningEfforts": ["low", "high"]
                },
                {
                    "id": "beta",
                    "name": "Beta",
                    "supportsReasoningEffort": false
                }
            ]
        }))
        .unwrap();
        app.catalog_status = CatalogStatus::Ready;
    }

    #[test]
    fn settings_dropdown_hits_come_from_catalog_not_hardcoded_tiers() {
        let mut app = test_app();
        app.area = Rect::new(0, 0, 80, 24);
        inject_two_model_catalog(&mut app);
        let mut opts = test_opts("alpha", ReasoningEffort::High);
        open_settings(&mut app);
        let mut terminal =
            Terminal::new(ratatui::backend::TestBackend::new(80, 24)).unwrap();
        terminal
            .draw(|f| {
                let _ = draw_settings(f, &mut app, &opts);
            })
            .unwrap();
        assert!(
            app.hits.iter().any(|(_, h)| matches!(h, Hit::SettingModel)),
            "model combo missing: {:?}",
            app.hits.iter().map(|(_, h)| *h).collect::<Vec<_>>()
        );
        assert!(app
            .hits
            .iter()
            .any(|(_, h)| matches!(h, Hit::SettingEffort)));
        assert!(
            !app.hits
                .iter()
                .any(|(_, h)| matches!(h, Hit::CatalogPick(_))),
            "closed combo must not list options"
        );

        handle_settings_key(&mut app, &mut opts, KeyCode::Enter, KeyModifiers::NONE);
        terminal
            .draw(|f| {
                let _ = draw_settings(f, &mut app, &opts);
            })
            .unwrap();
        let picks: Vec<u16> = app
            .hits
            .iter()
            .filter_map(|(_, h)| match h {
                Hit::CatalogPick(i) => Some(*i),
                _ => None,
            })
            .collect();
        assert_eq!(picks, vec![0, 1], "model list must be catalog ids, not a hardcoded grok set");

        let cell = app
            .hits
            .iter()
            .find(|(_, h)| *h == Hit::CatalogPick(1))
            .unwrap()
            .0;
        handle_mouse(
            &mut app,
            &mut opts,
            MouseEventKind::Down(MouseButton::Left),
            cell.x,
            cell.y,
            KeyModifiers::NONE,
        );
        assert_eq!(opts.model, "beta");
        assert!(
            !app.knobs.lock().unwrap().send_reasoning,
            "beta does not support reasoning effort"
        );
        assert!(effort_choices(&app, &opts).is_empty());
    }

    #[test]
    fn switching_model_clamps_effort_to_that_models_catalog_list() {
        let mut app = test_app();
        inject_two_model_catalog(&mut app);
        let mut opts = test_opts("alpha", ReasoningEffort::Xhigh);
        apply_selected_model(&mut app, &mut opts, "alpha".into());
        assert_eq!(opts.reasoning_effort, ReasoningEffort::High);
        assert_eq!(
            effort_choices(&app, &opts)
                .iter()
                .map(|e| e.value)
                .collect::<Vec<_>>(),
            [ReasoningEffort::Low, ReasoningEffort::High]
        );
        app.setting_field = SettingField::Effort;
        handle_settings_key(&mut app, &mut opts, KeyCode::Right, KeyModifiers::NONE);
        assert_eq!(opts.reasoning_effort, ReasoningEffort::Low);
        handle_settings_key(&mut app, &mut opts, KeyCode::Right, KeyModifiers::NONE);
        assert_eq!(opts.reasoning_effort, ReasoningEffort::High);
        assert_ne!(opts.reasoning_effort, ReasoningEffort::Medium);
        assert_ne!(opts.reasoning_effort, ReasoningEffort::Xhigh);
    }

    #[test]
    fn catalog_load_failure_keeps_current_selection_only() {
        let mut app = test_app();
        let mut opts = test_opts("mine", ReasoningEffort::Medium);
        ingest_catalog(
            &mut app,
            &mut opts,
            Err(Error::Provider("nope".into())),
        );
        assert!(matches!(app.catalog_status, CatalogStatus::Failed(_)));
        assert_eq!(
            model_choices(&app, &opts),
            vec![("mine".into(), "mine".into())]
        );
        assert_eq!(
            effort_choices(&app, &opts)
                .iter()
                .map(|e| e.value)
                .collect::<Vec<_>>(),
            [ReasoningEffort::Medium]
        );
    }

    #[test]
    fn settings_login_event_ignores_stale_gen_then_accepts_current() {
        let mut app = test_app();
        app.logged_in = false;
        app.login_gen = 3;
        apply_login_event(
            &mut app,
            LoginEvent::Waiting {
                gen: 2,
                url: "https://auth.x.ai/device".into(),
                user_code: "OLD".into(),
            },
        );
        assert!(matches!(app.login_ui, LoginUi::Idle));
        apply_login_event(
            &mut app,
            LoginEvent::Waiting {
                gen: 3,
                url: "https://auth.x.ai/device".into(),
                user_code: "ABCD-EFGH".into(),
            },
        );
        match &app.login_ui {
            LoginUi::Waiting { user_code, .. } => assert_eq!(user_code, "ABCD-EFGH"),
            other => panic!("{other:?}"),
        }
        apply_login_event(&mut app, LoginEvent::Success { gen: 3 });
        assert!(app.logged_in);
        assert!(app.want_catalog);
        assert!(matches!(app.login_ui, LoginUi::Idle));
    }

    #[test]
    fn settings_logout_deletes_tokens() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("xai-auth.json");
        auth::save_tokens(
            &path,
            &auth::TokenSet {
                access_token: "acc".into(),
                refresh_token: "ref".into(),
                id_token: None,
            },
        )
        .unwrap();
        let mut app = test_app();
        app.auth_path = path.clone();
        app.logged_in = true;
        logout_account(&mut app);
        assert!(!app.logged_in);
        assert!(auth::load_tokens(&path).is_err());
    }

    #[test]
    fn begin_login_when_logged_out_requests_device_flow() {
        let mut app = test_app();
        app.logged_in = false;
        begin_login(&mut app);
        assert!(app.want_login);
        assert!(matches!(app.login_ui, LoginUi::Starting));
        assert_eq!(app.login_gen, 1);
        begin_login(&mut app);
        assert_eq!(app.login_gen, 1, "in-flight login must not restart");
        cancel_login(&mut app);
        assert!(!app.want_login);
        assert!(matches!(app.login_ui, LoginUi::Idle));
        assert_eq!(app.login_gen, 2);
    }

    #[test]
    fn settings_focuses_account_and_draws_login_when_logged_out() {
        let mut app = test_app();
        app.logged_in = false;
        app.area = Rect::new(0, 0, 80, 28);
        open_settings(&mut app);
        assert_eq!(app.setting_field, SettingField::Account);
        let opts = test_opts("grok-4.6", ReasoningEffort::High);
        let mut terminal = Terminal::new(ratatui::backend::TestBackend::new(80, 28)).unwrap();
        terminal
            .draw(|f| {
                let _ = draw_settings(f, &mut app, &opts);
            })
            .unwrap();
        assert!(app.hits.iter().any(|(_, h)| matches!(h, Hit::AccountBtn)));
        handle_settings_key(&mut app, &mut test_opts("grok-4.6", ReasoningEffort::High), KeyCode::Enter, KeyModifiers::NONE);
        assert!(app.want_login);
    }

    #[test]
    fn logout_button_width_fits_full_cjk_label() {
        assert_eq!(display_cols(" 登出 "), 6);
        assert!(display_cols(" 登出 ") > " 登出 ".chars().count() as u16);
        let mut app = test_app();
        app.logged_in = true;
        app.area = Rect::new(0, 0, 80, 28);
        open_settings(&mut app);
        let opts = test_opts("grok-4.6", ReasoningEffort::High);
        let mut terminal = Terminal::new(ratatui::backend::TestBackend::new(80, 28)).unwrap();
        terminal
            .draw(|f| {
                let _ = draw_settings(f, &mut app, &opts);
            })
            .unwrap();
        let btn = app
            .hits
            .iter()
            .find(|(_, h)| *h == Hit::AccountBtn)
            .expect("logout button")
            .0;
        assert!(
            btn.width >= display_cols(" 登出 "),
            "logout hit width {} cannot fit 登出",
            btn.width
        );
    }

    #[test]
    fn sending_while_logged_out_points_at_settings_not_cli() {
        let mut app = test_app();
        app.logged_in = false;
        let opts = test_opts("grok-4.6", ReasoningEffort::High);
        let sink = Arc::new(FanoutSink { sinks: vec![] });
        let (done_tx, _done_rx) = mpsc::unbounded_channel();
        start_or_send(&mut app, &opts, &sink, &done_tx, "hi".into());
        let err = app
            .rows
            .iter()
            .find_map(|r| match r {
                Row::Err(s) => Some(s.as_str()),
                _ => None,
            })
            .unwrap();
        assert!(err.contains("設定"), "{err}");
        assert!(!err.contains("另開終端"), "{err}");
    }

    #[test]
    fn boot_resumes_existing_chat_and_does_not_create() {
        let dir = tempfile::tempdir().unwrap();
        let store = SessionStore::open_at(dir.path().to_path_buf()).unwrap();
        let mut real = store.create(PathBuf::from(".")).unwrap();
        real.name = "實作登入".into();
        real.named = true;
        real.updated_at = chrono::Utc::now() - chrono::Duration::seconds(30);
        store.save_meta(&real).unwrap();
        store
            .save_transcript(
                &real.id,
                &serde_json::to_value(vec![Row::User("先前的對話".into())]).unwrap(),
            )
            .unwrap();
        let blank = store.create(PathBuf::from(".")).unwrap();
        let listed = store.list().unwrap();
        assert_eq!(listed[0].id, blank.id, "blank is newest");
        assert_eq!(listed.len(), 2);

        let boot = boot_session(Some(&store), &listed, PathBuf::from("."));
        assert!(!boot.created);
        assert_eq!(boot.session.id, real.id);
        assert!(
            matches!(&boot.rows[0], Row::User(u) if u.text == "先前的對話"),
            "expected resumed user row"
        );
        assert_eq!(store.list().unwrap().len(), 2);
    }

    #[test]
    fn boot_empty_store_creates_one_session() {
        let dir = tempfile::tempdir().unwrap();
        let store = SessionStore::open_at(dir.path().to_path_buf()).unwrap();
        let listed = store.list().unwrap();
        assert!(listed.is_empty());
        let boot = boot_session(Some(&store), &listed, PathBuf::from("."));
        assert!(boot.created);
        assert_eq!(store.list().unwrap().len(), 1);
        assert!(boot.rows.iter().all(|r| matches!(r, Row::Meta(_))));
    }

    #[test]
    fn boot_only_blank_draft_resumes_it() {
        let dir = tempfile::tempdir().unwrap();
        let store = SessionStore::open_at(dir.path().to_path_buf()).unwrap();
        let blank = store.create(PathBuf::from(".")).unwrap();
        let listed = store.list().unwrap();
        let boot = boot_session(Some(&store), &listed, PathBuf::from("."));
        assert!(!boot.created);
        assert_eq!(boot.session.id, blank.id);
        assert_eq!(store.list().unwrap().len(), 1);
    }
}
