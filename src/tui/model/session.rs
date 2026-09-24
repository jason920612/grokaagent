//! One chat session: its main transcript, child agents, backgrounds, composer
//! draft and queue, and the handles of its running agent. Every open session
//! is a value in `App::sessions`; events are routed by session id, so a chat
//! in the background updates exactly like the visible one.

use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::Arc;

use tokio::sync::mpsc;

use crate::agent::{CancelFlag, UserTurn};
use crate::ask::{self, AskUserHub, Question};
use crate::events::AgentEvent;
use crate::session::SessionMeta;
use crate::task::{self, TaskHub};

use super::agents::{AgentTree, EventKind};
use super::rows::{Row, UserMsg};
use super::transcript::Transcript;
use crate::tui::edit::Edit;

#[derive(Clone, Debug)]
pub(crate) struct SideMon {
    pub name: String,
    pub command: String,
    pub pid: u32,
    pub status: String,
    pub alive: bool,
    pub detail: String,
}

#[derive(Clone, Debug)]
pub(crate) struct SideBg {
    pub name: String,
    pub command: String,
    pub pid: u32,
    pub status: String,
    pub alive: bool,
    pub detail: String,
    pub log: Vec<String>,
}

impl SideBg {
    fn push_log(&mut self, line: String) {
        if self.log.len() > 400 {
            self.log.drain(0..150);
        }
        self.log.push(line);
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct Queued {
    pub text: String,
    pub images: Vec<String>,
}

impl Queued {
    pub fn label(&self) -> String {
        let t = self.text.replace('\n', " ");
        match (self.images.len(), t.is_empty()) {
            (0, _) => t,
            (n, true) => format!("[{n} 張圖片]"),
            (n, false) => format!("{t}  [{n}圖]"),
        }
    }

    pub fn into_turn(self) -> UserTurn {
        UserTurn {
            text: self.text,
            images: self.images.into_iter().map(PathBuf::from).collect(),
        }
    }
}

impl From<&str> for Queued {
    fn from(s: &str) -> Self {
        Self {
            text: s.to_string(),
            images: Vec::new(),
        }
    }
}

/// Transcript row for a turn the user sent.
pub(crate) fn user_row(turn: &UserTurn) -> UserMsg {
    UserMsg {
        text: turn.text.clone(),
        images: turn
            .images
            .iter()
            .map(|p| p.to_string_lossy().replace('\\', "/"))
            .collect(),
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct ChatPos {
    pub row: usize,
    pub idx: usize,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) enum ChatSel {
    #[default]
    None,
    Text { anchor: ChatPos, caret: ChatPos },
    Image(String),
}

impl ChatSel {
    pub fn text_range(&self) -> Option<(ChatPos, ChatPos)> {
        match *self {
            ChatSel::Text { anchor, caret } if anchor != caret => {
                Some((anchor.min(caret), anchor.max(caret)))
            }
            _ => None,
        }
    }
}

/// Per-transcript view state: scroll, selection, open tool detail.
#[derive(Clone, Debug)]
pub(crate) struct ChatView {
    /// Lines scrolled up from the bottom.
    pub scroll: u16,
    pub stick_bottom: bool,
    pub open_tool: Option<(usize, usize)>,
    pub sel: ChatSel,
    /// Transcript epoch the row indices above refer to.
    pub epoch: u64,
}

impl Default for ChatView {
    fn default() -> Self {
        Self {
            scroll: 0,
            stick_bottom: true,
            open_tool: None,
            sel: ChatSel::None,
            epoch: 0,
        }
    }
}

impl ChatView {
    /// Drop row-index state that a trimmed transcript made stale.
    pub fn sync(&mut self, epoch: u64) {
        if self.epoch != epoch {
            self.epoch = epoch;
            self.open_tool = None;
            self.sel = ChatSel::None;
        }
    }

    pub fn scroll_by(&mut self, delta: i32) {
        if delta > 0 {
            self.stick_bottom = false;
            self.scroll = self.scroll.saturating_add(delta as u16);
        } else {
            self.scroll = self.scroll.saturating_sub(delta.unsigned_abs() as u16);
            if self.scroll == 0 {
                self.stick_bottom = true;
            }
        }
    }

    pub fn jump_bottom(&mut self) {
        self.stick_bottom = true;
        self.scroll = 0;
    }
}

pub(crate) struct AskState {
    pub question: Question,
    pub cursor: usize,
    pub chosen: Vec<bool>,
    pub values: Vec<String>,
    pub filling: bool,
    pub fill: Edit,
}

impl AskState {
    pub fn new(question: Question) -> Self {
        let n = question.options.len();
        Self {
            question,
            cursor: 0,
            chosen: vec![false; n],
            values: vec![String::new(); n],
            filling: false,
            fill: Edit::default(),
        }
    }

    pub fn n(&self) -> usize {
        self.question.options.len()
    }

    pub fn move_cursor(&mut self, delta: i32) {
        self.save_fill();
        let n = self.n() as i32;
        if n > 0 {
            self.cursor = (self.cursor as i32 + delta).rem_euclid(n) as usize;
        }
    }

    pub fn save_fill(&mut self) {
        if self.filling {
            let t: String = self.fill.text.chars().take(ask::MAX_INPUT).collect();
            if let Some(slot) = self.values.get_mut(self.cursor) {
                *slot = t;
            }
            self.filling = false;
        }
    }

    fn choose(&mut self, i: usize) {
        if !self.question.allow_multiple {
            self.chosen.fill(false);
        }
        if let Some(c) = self.chosen.get_mut(i) {
            *c = true;
        }
    }

    pub fn enter_fill(&mut self) {
        if !self.question.options.get(self.cursor).is_some_and(|o| o.input) {
            return;
        }
        self.choose(self.cursor);
        self.filling = true;
        self.fill = Edit::at_end(self.values.get(self.cursor).cloned().unwrap_or_default());
    }

    pub fn toggle_cursor(&mut self) {
        if self.question.allow_multiple {
            if let Some(c) = self.chosen.get_mut(self.cursor) {
                *c = !*c;
            }
        } else {
            self.choose(self.cursor);
        }
    }

    /// Set option `i`'s typed value (and choose it).
    pub fn set_value(&mut self, i: usize, text: &str) {
        if i >= self.values.len() {
            return;
        }
        self.cursor = i;
        let clipped: String = text.chars().take(ask::MAX_INPUT).collect();
        self.values[i] = clipped.clone();
        if self.question.options.get(i).is_some_and(|o| o.input) {
            self.choose(i);
            self.fill = Edit::at_end(clipped);
        }
    }

    /// Values with the open fill box committed.
    pub fn final_values(&self) -> Vec<String> {
        let mut values = self.values.clone();
        if self.filling {
            if let Some(slot) = values.get_mut(self.cursor) {
                *slot = self.fill.text.chars().take(ask::MAX_INPUT).collect();
            }
        }
        values
    }

    pub fn summary(&self) -> String {
        let values = self.final_values();
        let picks: Vec<String> = self
            .question
            .options
            .iter()
            .enumerate()
            .filter(|(i, _)| self.chosen.get(*i).copied().unwrap_or(false))
            .map(|(i, o)| {
                if o.input {
                    format!("{}: {}", o.label, values.get(i).cloned().unwrap_or_default())
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

pub(crate) fn intro_rows() -> Vec<Row> {
    vec![
        Row::meta("VS Code 式工作台：左側活動列切換對話／代理／變更／背景／任務，Ctrl+B 側欄、Ctrl+J 面板。"),
        Row::meta("工作中可「接著做」排隊或「調整工作」插入下一輪 · 選取文字後 Ctrl+C 複製 · Ctrl+N 新對話"),
    ]
}

/// What routing an event asks the app to do next.
#[derive(Debug, Default, PartialEq)]
pub(crate) struct Routed {
    /// A questionnaire opened in this (visible) session.
    pub ask_opened: bool,
    /// The run ended: its inbox is gone.
    pub run_finished: bool,
}

pub(crate) struct Session {
    pub meta: SessionMeta,
    pub chat: Transcript,
    pub agents: AgentTree,
    pub monitors: Vec<SideMon>,
    pub backgrounds: Vec<SideBg>,
    /// View state per transcript: `""` main chat, else agent path.
    pub views: HashMap<String, ChatView>,
    /// Open agent tabs, by path.
    pub open_tabs: Vec<String>,
    /// Active agent tab; `None` = main chat.
    pub active: Option<String>,
    /// Background shown in the Output panel.
    pub output_pick: Option<String>,
    pub draft: Edit,
    pub pending: Vec<String>,
    pub queue: VecDeque<Queued>,
    /// The composer is editing `queue[i]`; auto-send waits for commit/cancel.
    pub queue_edit: Option<usize>,
    pub stash: Option<Edit>,
    pub inbox: Option<mpsc::UnboundedSender<UserTurn>>,
    pub cancel: Option<CancelFlag>,
    pub ask_hub: Option<AskUserHub>,
    pub ask: Option<AskState>,
    pub task: Arc<TaskHub>,
    /// Transcript changed since the last save.
    pub dirty: bool,
}

impl Session {
    pub fn new(meta: SessionMeta, rows: Vec<Row>, task: Arc<TaskHub>) -> Self {
        Self {
            meta,
            chat: Transcript::new(rows),
            agents: AgentTree::default(),
            monitors: Vec::new(),
            backgrounds: Vec::new(),
            views: HashMap::new(),
            open_tabs: Vec::new(),
            active: None,
            output_pick: None,
            draft: Edit::default(),
            pending: Vec::new(),
            queue: VecDeque::new(),
            queue_edit: None,
            stash: None,
            inbox: None,
            cancel: None,
            ask_hub: None,
            ask: None,
            task,
            dirty: false,
        }
    }

    pub fn id(&self) -> &str {
        &self.meta.id
    }

    /// Blank draft: never named, nothing running, only meta rows.
    pub fn is_blank(&self) -> bool {
        !self.meta.named && self.inbox.is_none() && !self.chat.running && self.chat.is_blank()
    }

    /// Transcript shown by the active tab.
    pub fn view_transcript(&self) -> &Transcript {
        self.active
            .as_deref()
            .and_then(|p| self.agents.get(p))
            .map(|a| &a.transcript)
            .unwrap_or(&self.chat)
    }

    pub fn view_transcript_mut(&mut self) -> &mut Transcript {
        match self.active.clone() {
            Some(p) if self.agents.get(&p).is_some() => &mut self.agents.get_mut(&p).unwrap().transcript,
            _ => &mut self.chat,
        }
    }

    pub fn view_key(&self) -> String {
        self.active
            .clone()
            .filter(|p| self.agents.get(p).is_some())
            .unwrap_or_default()
    }

    /// View state for the active tab (synced to its transcript).
    pub fn view_mut(&mut self) -> &mut ChatView {
        let key = self.view_key();
        let epoch = self.view_transcript().epoch;
        let v = self.views.entry(key).or_default();
        v.sync(epoch);
        v
    }

    pub fn view(&self) -> ChatView {
        self.views.get(&self.view_key()).cloned().unwrap_or_default()
    }

    #[cfg(test)]
    pub fn viewing_agent(&self) -> bool {
        !self.view_key().is_empty()
    }

    pub fn open_agent(&mut self, path: &str) {
        if self.agents.get(path).is_none() {
            return;
        }
        if !self.open_tabs.iter().any(|p| p == path) {
            self.open_tabs.push(path.to_string());
        }
        self.active = Some(path.to_string());
    }

    pub fn close_agent(&mut self, path: &str) {
        let Some(i) = self.open_tabs.iter().position(|p| p == path) else {
            return;
        };
        self.open_tabs.remove(i);
        if self.active.as_deref() == Some(path) {
            self.active = if i > 0 {
                self.open_tabs.get(i - 1).cloned()
            } else {
                self.open_tabs.first().cloned()
            };
        }
    }

    /// Every file change: (view path, row, call, file path, kind).
    pub fn file_changes(&self) -> Vec<(String, usize, usize, String, String)> {
        let mut out: Vec<_> = self
            .chat
            .file_changes()
            .into_iter()
            .map(|(r, c, p, k)| (String::new(), r, c, p, k))
            .collect();
        for a in &self.agents.nodes {
            out.extend(
                a.transcript
                    .file_changes()
                    .into_iter()
                    .map(|(r, c, p, k)| (a.path.clone(), r, c, p, k)),
            );
        }
        out
    }

    /// Interrupt the running turn (or, while idle, the working children).
    /// Returns `true` if something was interrupted.
    pub fn interrupt(&mut self) -> bool {
        let Some(cancel) = &self.cancel else {
            return false;
        };
        if self.chat.running {
            cancel.trip();
            self.cancel_ask();
            self.chat.interrupt();
            return true;
        }
        if self.agents.live_count() > 0 {
            // The session ignores Esc while paused; only the nursery reacts.
            cancel.trip();
            self.agents.log("", EventKind::Agent, "中斷工作中的子代理");
            self.chat.status = "已中斷子代理".into();
            return true;
        }
        false
    }

    pub fn cancel_ask(&mut self) {
        if let Some(h) = &self.ask_hub {
            h.cancel();
        }
        if self.ask.take().is_some() {
            self.chat.status = "已取消問卷".into();
        }
    }

    /// Answer the open questionnaire. `Err` keeps it open with a message.
    pub fn submit_ask(&mut self) -> Result<(), String> {
        let Some(ask) = self.ask.take() else {
            return Ok(());
        };
        match ask::answer_from_picks(&ask.question, &ask.chosen, &ask.final_values()) {
            Ok(body) => {
                if let Some(h) = &self.ask_hub {
                    h.answer(body);
                }
                self.chat.push(Row::meta(ask.summary()));
                self.chat.status = "已回答".into();
                Ok(())
            }
            Err(msg) => {
                self.ask = Some(ask);
                Err(msg)
            }
        }
    }

    fn upsert_monitor(&mut self, name: &str, command: &str, pid: u32) {
        let m = SideMon {
            name: name.into(),
            command: command.into(),
            pid,
            status: "執行中".into(),
            alive: true,
            detail: String::new(),
        };
        match self.monitors.iter_mut().find(|x| x.name == name) {
            Some(x) => *x = m,
            None => self.monitors.push(m),
        }
    }

    fn upsert_background(&mut self, name: &str, command: String, pid: u32, status: &str) {
        match self.backgrounds.iter_mut().find(|b| b.name == name) {
            Some(b) => {
                b.command = command;
                b.pid = pid;
                b.alive = true;
                b.status = status.into();
                b.detail.clear();
            }
            None => self.backgrounds.push(SideBg {
                name: name.into(),
                command,
                pid,
                status: status.into(),
                alive: true,
                detail: String::new(),
                log: Vec::new(),
            }),
        }
    }

    fn end_background(&mut self, name: &str, status: &str, detail: &str) {
        if let Some(b) = self.backgrounds.iter_mut().find(|b| b.name == name) {
            b.alive = false;
            b.status = status.into();
            if !detail.is_empty() {
                b.detail = detail.into();
            }
        }
    }

    /// Workbench log entries for the main agent's own events.
    fn log_root(&mut self, ev: &AgentEvent) {
        let t = &mut self.agents;
        match ev {
            AgentEvent::ToolStarted { call_id, name, args, .. } => t.tool_started("", call_id, name, args),
            AgentEvent::ToolFinished { call_id, name, output, .. } => {
                t.tool_finished("", call_id, name, output)
            }
            AgentEvent::Error { message, .. } => t.log("", EventKind::Error, message.clone()),
            AgentEvent::Notice { message, .. } => t.log("", EventKind::Notice, message.clone()),
            AgentEvent::ContextCompacted { method, dropped_items, kept_items, .. } => t.log(
                "",
                EventKind::Notice,
                format!("壓縮 ({method}) 丟 {dropped_items} 留 {kept_items}"),
            ),
            AgentEvent::BackgroundStarted { name, command, .. } => {
                t.log("", EventKind::Background, format!("背景 {name} 開始  $ {command}"))
            }
            AgentEvent::BackgroundExited { name, detail, .. } => {
                t.log("", EventKind::Background, format!("背景 {name} 結束  {detail}"))
            }
            AgentEvent::MonitorAttached { name, command, .. } => {
                t.log("", EventKind::Background, format!("監控 {name}  $ {command}"))
            }
            AgentEvent::TimerFired { name, detail, .. } => {
                t.log("", EventKind::Background, format!("計時器 {name} 到時  {detail}"))
            }
            _ => {}
        }
    }

    /// Route one event of this session. `visible` = this is the chat on
    /// screen (a questionnaire may open); others cancel questionnaires.
    pub fn route(&mut self, ev: AgentEvent, visible: bool) -> Routed {
        let mut out = Routed::default();
        if ev.is_child_work() {
            self.agents.apply_relayed(&ev);
            return out;
        }
        self.log_root(&ev);
        if !matches!(ev, AgentEvent::BackgroundOutput { .. }) {
            self.dirty = true;
        }
        let skip_steer = self.task.snapshot().skip_steer;
        if self.chat.apply(&ev, skip_steer) {
            if let AgentEvent::RunFinished { .. } = ev {
                self.cancel_ask();
                self.inbox = None;
                out.run_finished = true;
            }
            return out;
        }
        match ev {
            AgentEvent::ChildSpawned { name, prompt, model, .. } => {
                // The task supervisor reports itself like a child but is not a process.
                if name != task::AGENT_NAME {
                    self.agents.spawned("", &name, &prompt, &model);
                }
                self.chat.push(Row::meta(format!("子代理 {name} 已啟動")));
            }
            AgentEvent::ChildExited { name, detail, .. } => {
                if name != task::AGENT_NAME {
                    self.agents.exited("", &name, &detail);
                }
                self.chat.mark_spawn_done(&name);
                self.chat.push(Row::meta(format!("子代理 {name} 結束")));
            }
            AgentEvent::AgentMessage { from, to, text, .. } => {
                if from == task::AGENT_NAME || to == task::AGENT_NAME {
                    let preview: String = text.chars().take(160).collect();
                    self.agents.log(
                        "",
                        EventKind::Message,
                        format!("{from} → {to}  {}", preview.replace('\n', " ")),
                    );
                } else {
                    self.agents.message("", &from, &to, &text);
                }
            }
            AgentEvent::MonitorAttached { name, command, pid, .. } => {
                self.upsert_monitor(&name, &command, pid);
                self.chat.push(Row::meta(format!("監控 {name} 已掛上  $ {command}")));
            }
            AgentEvent::MonitorExited { name, detail, .. } => {
                if let Some(m) = self.monitors.iter_mut().find(|m| m.name == name) {
                    m.alive = false;
                    m.status = "結束".into();
                    m.detail = detail.clone();
                }
                self.chat.push(Row::meta(format!("監控 {name} 結束  {detail}")));
            }
            AgentEvent::BackgroundStarted { name, command, pid, .. } => {
                self.upsert_background(&name, command.clone(), pid, "執行中");
                self.chat.push(Row::meta(format!("後台 {name} 已掛上  $ {command}")));
            }
            AgentEvent::BackgroundOutput { name, stream, text, .. } => {
                if let Some(b) = self.backgrounds.iter_mut().find(|b| b.name == name) {
                    b.push_log(format!("{stream} {text}"));
                }
            }
            AgentEvent::BackgroundExited { name, detail, .. } => {
                self.end_background(&name, "結束", &detail);
                self.chat.push(Row::meta(format!("後台 {name} 結束  {detail}")));
            }
            AgentEvent::TimerStarted { name, seconds, command, .. } => {
                let label = if command.is_empty() {
                    format!("timer {seconds}s")
                } else {
                    format!("timer {seconds}s  $ {command}")
                };
                self.upsert_background(&name, label, 0, "倒數中");
                self.chat.push(Row::meta(format!("計時器 {name} 開始  {seconds}s")));
            }
            AgentEvent::TimerFired { name, detail, .. } => {
                self.end_background(&name, "已到時", &detail);
                self.chat.push(Row::meta(format!("計時器 {name} 到時")));
            }
            AgentEvent::TimerCancelled { name, .. } => {
                self.end_background(&name, "已取消", "");
                self.chat.push(Row::meta(format!("計時器 {name} 已取消")));
            }
            AgentEvent::AskUser { question, allow_multiple, options, .. } => {
                if options.is_empty() {
                    return out;
                }
                self.chat.push(Row::meta(format!("問卷  {question}")));
                if !visible {
                    // Nobody can answer a hidden chat's questionnaire.
                    if let Some(h) = &self.ask_hub {
                        h.cancel();
                    }
                    return out;
                }
                self.ask = Some(AskState::new(Question {
                    prompt: question,
                    allow_multiple,
                    options,
                }));
                self.chat.status = "請選擇".into();
                out.ask_opened = true;
            }
            _ => {}
        }
        out
    }

    // —— Composer & queue ——

    pub fn begin_queue_edit(&mut self, index: usize) {
        if index >= self.queue.len() || self.queue_edit == Some(index) {
            return;
        }
        let mut idx = index;
        if let Some(cur) = self.queue_edit {
            if self.commit_queue_edit() && cur < idx {
                idx -= 1;
            }
            if idx >= self.queue.len() {
                return;
            }
        }
        if self.stash.is_none() {
            self.stash = Some(std::mem::take(&mut self.draft));
        }
        self.draft = Edit::at_end(self.queue[idx].text.clone());
        self.queue_edit = Some(idx);
    }

    /// Restore the queued text and the previous draft.
    pub fn cancel_queue_edit(&mut self) {
        if self.queue_edit.take().is_some() {
            self.draft = self.stash.take().unwrap_or_default();
        }
    }

    /// Save the composer into the queued item; empty text drops it.
    /// Returns `true` if the item was removed.
    pub fn commit_queue_edit(&mut self) -> bool {
        let Some(i) = self.queue_edit.take() else {
            return false;
        };
        let text = self.draft.text.trim().to_string();
        let mut removed = false;
        if i < self.queue.len() {
            if text.is_empty() && self.queue[i].images.is_empty() {
                self.queue.remove(i);
                removed = true;
            } else {
                self.queue[i].text = text;
            }
        }
        self.draft = self.stash.take().unwrap_or_default();
        removed
    }

    /// Take the composer contents (text + attachments) as a turn.
    pub fn take_turn(&mut self) -> Option<UserTurn> {
        let text = self.draft.text.trim().to_string();
        let images: Vec<PathBuf> = self.pending.drain(..).map(PathBuf::from).collect();
        if text.is_empty() && images.is_empty() {
            return None;
        }
        self.draft.clear();
        Some(UserTurn { text, images })
    }

    /// Send the next queued message once the agent is waiting.
    pub fn flush_queue(&mut self) -> bool {
        if !self.chat.awaiting || self.queue_edit.is_some() || self.chat.status == "已停止" {
            return false;
        }
        let Some(tx) = self.inbox.clone() else {
            return false;
        };
        let Some(msg) = self.queue.pop_front() else {
            return false;
        };
        let turn = msg.into_turn();
        self.chat.push(Row::User(user_row(&turn)));
        if tx.send(turn).is_ok() {
            self.chat.running = true;
            self.chat.awaiting = false;
            self.chat.status = "工作中".into();
            self.chat.mark_work_start();
        }
        true
    }

    /// Drop the run handles after the run task ended.
    pub fn finish_run(&mut self, turns: u32) {
        self.cancel_ask();
        self.ask_hub = None;
        self.chat.running = false;
        self.chat.awaiting = false;
        self.inbox = None;
        self.cancel = None;
        let s = &self.chat.status;
        if s.starts_with("工作") || s.starts_with("第") || s.starts_with("中斷") {
            self.chat.status = format!("結束 ({turns} 輪)");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ask::Choice;
    use crate::events::EventMeta;

    fn session() -> Session {
        let mut meta = SessionMeta::new(PathBuf::from("."));
        meta.id = "s".into();
        let task = TaskHub::new("s");
        Session::new(meta, Vec::new(), task)
    }

    fn meta(path: &str, parent: Option<&str>) -> EventMeta {
        EventMeta {
            ts: chrono::Utc::now(),
            agent_name: if path.is_empty() { "root".into() } else { path.into() },
            run_id: if path.is_empty() { "s".into() } else { format!("run-{path}") },
            parent_run_id: parent.map(str::to_string),
            path: path.into(),
        }
    }

    #[test]
    fn child_work_stays_out_of_the_main_chat() {
        let mut s = session();
        s.route(
            AgentEvent::ChildSpawned {
                meta: meta("", None),
                name: "coder".into(),
                agent_card_url: String::new(),
                prompt: "fix".into(),
                model: String::new(),
            },
            true,
        );
        let before = s.chat.rows.len();
        s.route(AgentEvent::ModelDelta { meta: meta("coder", Some("s")), text: "hi".into() }, true);
        assert_eq!(s.chat.rows.len(), before);
        assert_eq!(s.agents.get("coder").unwrap().transcript.rows.len(), 1);
    }

    #[test]
    fn backgrounds_track_output_without_chat_rows() {
        let mut s = session();
        s.route(
            AgentEvent::BackgroundStarted { meta: meta("", None), name: "dev".into(), command: "npm run dev".into(), pid: 9 },
            true,
        );
        let rows = s.chat.rows.len();
        s.route(
            AgentEvent::BackgroundOutput { meta: meta("", None), name: "dev".into(), stream: "out".into(), text: "ready".into() },
            true,
        );
        assert_eq!(s.chat.rows.len(), rows);
        assert_eq!(s.backgrounds[0].log, ["out ready"]);
        s.route(
            AgentEvent::BackgroundExited { meta: meta("", None), name: "dev".into(), detail: "killed".into() },
            true,
        );
        assert!(!s.backgrounds[0].alive);
        assert_eq!(s.backgrounds[0].detail, "killed");
    }

    fn ask_event() -> AgentEvent {
        AgentEvent::AskUser {
            meta: meta("", None),
            question: "pick".into(),
            allow_multiple: false,
            options: vec![
                Choice { id: "a".into(), label: "A".into(), input: false },
                Choice { id: "o".into(), label: "其他".into(), input: true },
            ],
        }
    }

    #[test]
    fn questionnaires_open_only_in_the_visible_chat() {
        let mut s = session();
        let hub = AskUserHub::new();
        s.ask_hub = Some(hub);
        assert!(!s.route(ask_event(), false).ask_opened);
        assert!(s.ask.is_none());
        assert!(s.route(ask_event(), true).ask_opened);
        let ask = s.ask.as_mut().unwrap();
        ask.cursor = 1;
        ask.enter_fill();
        ask.fill = Edit::at_end("自訂");
        assert_eq!(ask.summary(), "你選了  其他: 自訂");
        assert!(s.submit_ask().is_ok());
        assert!(s.ask.is_none());
        assert_eq!(s.chat.status, "已回答");
    }

    #[test]
    fn queue_edit_blocks_flush_until_commit() {
        let mut s = session();
        let (tx, mut rx) = mpsc::unbounded_channel();
        s.inbox = Some(tx);
        s.chat.awaiting = true;
        s.queue.push_back("old".into());
        s.draft = Edit::at_end("draft");
        s.begin_queue_edit(0);
        assert_eq!(s.draft.text, "old");
        s.draft = Edit::at_end("new");
        assert!(!s.flush_queue(), "must not send while editing");
        assert!(!s.commit_queue_edit());
        assert_eq!(s.draft.text, "draft");
        assert!(s.flush_queue());
        assert_eq!(rx.try_recv().unwrap().text, "new");
        assert!(s.chat.running);

        s.queue.push_back("gone".into());
        s.begin_queue_edit(0);
        s.draft.clear();
        assert!(s.commit_queue_edit(), "empty commit drops the item");
        assert!(s.queue.is_empty());
    }

    #[test]
    fn interrupt_stops_the_turn_or_idle_children() {
        let mut s = session();
        let cancel = CancelFlag::new();
        s.cancel = Some(cancel.clone());
        assert!(!s.interrupt(), "nothing running");
        s.agents.spawned("", "kid", "", "");
        assert!(s.interrupt(), "idle parent, live child");
        assert_eq!(cancel.trips(), 1);
        s.chat.running = true;
        assert!(s.interrupt());
        assert_eq!(s.chat.status, "中斷中");
    }

    #[test]
    fn tabs_open_close_and_pick_the_transcript() {
        let mut s = session();
        s.chat.push(Row::meta("root"));
        s.agents.spawned("", "a", "", "");
        s.agents.get_mut("a").unwrap().transcript.push(Row::meta("kid"));
        s.open_agent("a");
        assert!(s.viewing_agent());
        assert!(matches!(&s.view_transcript().rows[0], Row::Meta(m) if m == "kid"));
        s.view_mut().scroll_by(3);
        assert_eq!(s.view().scroll, 3);
        s.active = None;
        assert_eq!(s.view().scroll, 0, "each transcript keeps its own scroll");
        s.open_agent("missing");
        assert_eq!(s.open_tabs, ["a"]);
        s.close_agent("a");
        assert!(s.open_tabs.is_empty());
    }
}
