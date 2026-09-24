//! The session's child-agent tree, plus the cross-agent tool timeline and
//! event log shown in the bottom panel. Each agent owns a [`Transcript`]
//! built from its relayed events.

use std::collections::VecDeque;
use std::time::Instant;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::events::AgentEvent;

use super::rows::{tool_phase, Row, UserMsg};
use super::tool_text::tool_started_line;
use super::transcript::Transcript;

const TOOL_LOG_CAP: usize = 400;
const EVENT_LOG_CAP: usize = 400;
const AGENT_ROWS_CAP: usize = 1_500;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum AgentState {
    Starting,
    Working,
    Idle,
    Paused,
    Interrupted,
    Exited,
}

impl AgentState {
    pub fn label(self) -> &'static str {
        match self {
            Self::Starting => "啟動中",
            Self::Working => "工作中",
            Self::Idle => "閒置",
            Self::Paused => "暫停",
            Self::Interrupted => "已中斷",
            Self::Exited => "已結束",
        }
    }

    pub fn key(self) -> &'static str {
        match self {
            Self::Starting => "starting",
            Self::Working => "working",
            Self::Idle => "idle",
            Self::Paused => "paused",
            Self::Interrupted => "interrupted",
            Self::Exited => "exited",
        }
    }

    pub fn icon(self, tick: u8) -> &'static str {
        match self {
            Self::Starting => "◌",
            Self::Working => ["◐", "◓", "◑", "◒"][(tick as usize / 3) % 4],
            Self::Idle => "✓",
            Self::Paused => "‖",
            Self::Interrupted => "⊘",
            Self::Exited => "○",
        }
    }

    pub fn live(self) -> bool {
        matches!(self, Self::Starting | Self::Working)
    }
}

pub(crate) struct AgentNode {
    /// Tree path: `coder`, `coder/lint`.
    pub path: String,
    pub name: String,
    pub model: String,
    pub prompt: String,
    pub state: AgentState,
    pub turn: u32,
    pub tools: u32,
    /// Saw a model stop since the last turn began (natural idle vs interrupt).
    stopped: bool,
    pub transcript: Transcript,
}

impl AgentNode {
    fn new(path: &str) -> Self {
        Self {
            path: path.to_string(),
            name: path.rsplit('/').next().unwrap_or(path).to_string(),
            model: String::new(),
            prompt: String::new(),
            state: AgentState::Starting,
            turn: 0,
            tools: 0,
            stopped: false,
            transcript: Transcript::new(Vec::new()).with_cap(AGENT_ROWS_CAP),
        }
    }

    pub fn depth(&self) -> usize {
        self.path.matches('/').count()
    }

    pub fn alive(&self) -> bool {
        self.state != AgentState::Exited
    }

    fn set_state(&mut self, state: AgentState) {
        self.state = state;
        self.transcript.status = state.label().into();
        if state == AgentState::Exited {
            self.transcript.running = false;
            self.transcript.activity.clear();
        }
    }
}

#[derive(Clone)]
pub(crate) struct ToolEntry {
    pub path: String,
    pub call_id: String,
    pub name: String,
    pub line: String,
    pub phase: String,
    pub done: bool,
    pub started: Instant,
    pub ms: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum EventKind {
    Notice,
    Error,
    Agent,
    Message,
    Background,
}

impl EventKind {
    pub fn key(self) -> &'static str {
        match self {
            Self::Notice => "notice",
            Self::Error => "error",
            Self::Agent => "agent",
            Self::Message => "message",
            Self::Background => "bg",
        }
    }
}

#[derive(Clone)]
pub(crate) struct EventEntry {
    pub at: String,
    pub path: String,
    pub kind: EventKind,
    pub text: String,
}

#[derive(Serialize, Deserialize)]
pub(crate) struct SavedAgent {
    path: String,
    model: String,
    prompt: String,
    #[serde(default = "exited")]
    state: AgentState,
    rows: Vec<Row>,
}

fn exited() -> AgentState {
    AgentState::Exited
}

/// Path of the child `name` spawned by the agent at `by` (root = "").
pub(crate) fn child_path(by: &str, name: &str) -> String {
    if by.is_empty() {
        name.to_string()
    } else {
        format!("{by}/{name}")
    }
}

fn who(path: &str) -> &str {
    if path.is_empty() {
        "主代理"
    } else {
        path
    }
}

#[derive(Default)]
pub(crate) struct AgentTree {
    /// Tree order: every agent follows its parent's subtree.
    pub nodes: Vec<AgentNode>,
    pub tool_log: VecDeque<ToolEntry>,
    pub event_log: VecDeque<EventEntry>,
    /// Transcripts changed since the last save.
    pub dirty: bool,
}

impl AgentTree {
    pub fn get(&self, path: &str) -> Option<&AgentNode> {
        self.nodes.iter().find(|a| a.path == path)
    }

    pub fn get_mut(&mut self, path: &str) -> Option<&mut AgentNode> {
        self.nodes.iter_mut().find(|a| a.path == path)
    }

    pub fn ensure(&mut self, path: &str) -> &mut AgentNode {
        if let Some(i) = self.nodes.iter().position(|a| a.path == path) {
            return &mut self.nodes[i];
        }
        let at = match path.rsplit_once('/') {
            Some((parent, _)) => {
                let prefix = format!("{parent}/");
                self.nodes
                    .iter()
                    .rposition(|a| a.path == parent || a.path.starts_with(&prefix))
                    .map(|i| i + 1)
                    .unwrap_or(self.nodes.len())
            }
            None => self.nodes.len(),
        };
        self.nodes.insert(at, AgentNode::new(path));
        &mut self.nodes[at]
    }

    pub fn live_count(&self) -> usize {
        self.nodes.iter().filter(|a| a.state.live()).count()
    }

    pub fn alive_count(&self) -> usize {
        self.nodes.iter().filter(|a| a.alive()).count()
    }

    pub fn log(&mut self, path: &str, kind: EventKind, text: impl Into<String>) {
        let text = text.into();
        if text.trim().is_empty() {
            return;
        }
        if self.event_log.len() >= EVENT_LOG_CAP {
            self.event_log.pop_front();
        }
        self.event_log.push_back(EventEntry {
            at: chrono::Local::now().format("%H:%M:%S").to_string(),
            path: path.to_string(),
            kind,
            text,
        });
    }

    pub fn tool_started(&mut self, path: &str, call_id: &str, name: &str, args: &Value) {
        if self.tool_log.len() >= TOOL_LOG_CAP {
            self.tool_log.pop_front();
        }
        self.tool_log.push_back(ToolEntry {
            path: path.to_string(),
            call_id: call_id.to_string(),
            name: name.to_string(),
            line: tool_started_line(name, args)
                .trim_start_matches("▸ ")
                .to_string(),
            phase: "執行中".into(),
            done: false,
            started: Instant::now(),
            ms: 0,
        });
    }

    pub fn tool_finished(&mut self, path: &str, call_id: &str, name: &str, output: &str) {
        let hit = self.tool_log.iter_mut().rev().find(|t| {
            t.path == path && !t.done && (t.call_id == call_id || (call_id.is_empty() && t.name == name))
        });
        if let Some(t) = hit {
            t.done = true;
            t.ms = t.started.elapsed().as_millis() as u64;
            t.phase = tool_phase(name, output).into();
        }
    }

    /// The agent at `by` spawned `name`.
    pub fn spawned(&mut self, by: &str, name: &str, prompt: &str, model: &str) {
        let path = child_path(by, name);
        let node = self.ensure(&path);
        if node.state == AgentState::Exited {
            // A respawn under the same name starts a fresh transcript.
            node.transcript = Transcript::new(Vec::new()).with_cap(AGENT_ROWS_CAP);
            node.turn = 0;
            node.tools = 0;
        }
        if !prompt.is_empty() {
            node.prompt = prompt.to_string();
        }
        if !model.is_empty() {
            node.model = model.to_string();
        }
        node.set_state(AgentState::Starting);
        let label = if model.is_empty() {
            format!("啟動 {path}")
        } else {
            format!("啟動 {path} · {model}")
        };
        self.log(by, EventKind::Agent, label);
        self.dirty = true;
    }

    /// The child `name` of the agent at `by` is gone, with its descendants.
    pub fn exited(&mut self, by: &str, name: &str, detail: &str) {
        let path = child_path(by, name);
        let prefix = format!("{path}/");
        for a in self
            .nodes
            .iter_mut()
            .filter(|a| a.path == path || a.path.starts_with(&prefix))
        {
            if a.alive() {
                a.set_state(AgentState::Exited);
            }
        }
        if let Some(a) = self.get_mut(&path) {
            a.transcript.push(Row::meta(format!("已結束  {detail}")));
        }
        self.log(by, EventKind::Agent, format!("{path} 結束  {detail}"));
        self.dirty = true;
    }

    /// A2A text between the agent at `by` and one of its children. Messages
    /// down to a child become user turns in the child's transcript.
    pub fn message(&mut self, by: &str, from: &str, to: &str, text: &str) {
        let down_path = child_path(by, to);
        let down = from != to && self.get(&down_path).is_some();
        let path = if down { down_path } else { child_path(by, from) };
        if down {
            if let Some(a) = self.get_mut(&path) {
                a.transcript.push(Row::User(UserMsg::from(text)));
            }
            self.dirty = true;
        }
        let preview: String = text.chars().take(160).collect::<String>().replace('\n', " ");
        let arrow = if down {
            format!("{} → {path}", who(by))
        } else {
            format!("{path} → {}", who(by))
        };
        self.log(by, EventKind::Message, format!("{arrow}  {preview}"));
    }

    /// One event relayed from a descendant. Returns `true` when the agent
    /// settled (idle, paused, finished): a good moment to save.
    pub fn apply_relayed(&mut self, ev: &AgentEvent) -> bool {
        let path = if ev.path().is_empty() {
            ev.meta().agent_name.clone()
        } else {
            ev.path().to_string()
        };
        if path.is_empty() || path == "root" {
            return false;
        }
        match ev {
            AgentEvent::ChildSpawned {
                name, prompt, model, ..
            } => {
                self.ensure(&path);
                self.spawned(&path, name, prompt, model);
                if let Some(a) = self.get_mut(&path) {
                    a.transcript.push(Row::meta(format!("子代理 {name} 已啟動")));
                }
                return false;
            }
            AgentEvent::ChildExited { name, detail, .. } => {
                self.exited(&path, name, detail);
                return false;
            }
            AgentEvent::AgentMessage { from, to, text, .. } => {
                self.ensure(&path);
                self.message(&path, from, to, text);
                return false;
            }
            _ => {}
        }
        match ev {
            AgentEvent::ToolStarted {
                call_id, name, args, ..
            } => self.tool_started(&path, call_id, name, args),
            AgentEvent::ToolFinished {
                call_id,
                name,
                output,
                ..
            } => self.tool_finished(&path, call_id, name, output),
            AgentEvent::Error { message, .. } => self.log(&path, EventKind::Error, message.clone()),
            AgentEvent::Notice { message, .. } => self.log(&path, EventKind::Notice, message.clone()),
            AgentEvent::BackgroundStarted { name, command, .. } => {
                self.log(&path, EventKind::Background, format!("背景 {name} 開始  $ {command}"))
            }
            AgentEvent::BackgroundExited { name, detail, .. } => {
                self.log(&path, EventKind::Background, format!("背景 {name} 結束  {detail}"))
            }
            _ => {}
        }
        let node = self.ensure(&path);
        if node.state == AgentState::Starting && !matches!(ev, AgentEvent::RunStarted { .. }) {
            node.set_state(AgentState::Working);
        }
        match ev {
            AgentEvent::RunStarted { model, .. } => {
                if node.model.is_empty() {
                    node.model = model.clone();
                }
                node.set_state(AgentState::Working);
            }
            AgentEvent::TurnStarted { turn, .. } => {
                node.turn = *turn;
                node.stopped = false;
                node.set_state(AgentState::Working);
            }
            AgentEvent::ModelFinished { finish, .. } if finish == "stop" => node.stopped = true,
            AgentEvent::ToolStarted { .. } => node.tools += 1,
            _ => {}
        }
        let settle = matches!(ev, AgentEvent::AwaitingInput { .. } | AgentEvent::RunFinished { .. });
        if !node.transcript.apply(ev, false) {
            let line = match ev {
                AgentEvent::MonitorAttached { name, command, .. } => Some(format!("監控 {name}  $ {command}")),
                AgentEvent::BackgroundStarted { name, command, .. } => Some(format!("背景 {name}  $ {command}")),
                AgentEvent::BackgroundExited { name, detail, .. } => Some(format!("背景 {name} 結束  {detail}")),
                AgentEvent::TimerStarted { name, seconds, .. } => Some(format!("計時器 {name} {seconds}s")),
                AgentEvent::TimerFired { name, .. } => Some(format!("計時器 {name} 到時")),
                _ => None,
            };
            if let Some(line) = line {
                node.transcript.push(Row::meta(line));
            }
        }
        if settle {
            let next = if node.state == AgentState::Exited {
                AgentState::Exited
            } else if node.stopped {
                AgentState::Idle
            } else if node.transcript.status == "已停止" {
                AgentState::Interrupted
            } else {
                AgentState::Paused
            };
            node.set_state(next);
            self.dirty = true;
        }
        settle
    }

    pub fn saved(&self) -> Vec<SavedAgent> {
        self.nodes
            .iter()
            .map(|a| SavedAgent {
                path: a.path.clone(),
                model: a.model.clone(),
                prompt: a.prompt.clone(),
                state: a.state,
                rows: a.transcript.rows.clone(),
            })
            .collect()
    }

    /// Agents from a saved session. Their processes did not survive, so every
    /// one comes back exited.
    pub fn restore(saved: Vec<SavedAgent>) -> Self {
        let mut tree = Self::default();
        for s in saved {
            let mut node = AgentNode::new(&s.path);
            node.model = s.model;
            node.prompt = s.prompt;
            node.transcript.rows = s.rows;
            node.set_state(AgentState::Exited);
            tree.nodes.push(node);
        }
        tree
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::EventMeta;
    use serde_json::json;

    fn m(path: &str, run: &str) -> EventMeta {
        EventMeta {
            ts: chrono::Utc::now(),
            agent_name: path.rsplit('/').next().unwrap_or("").into(),
            run_id: run.into(),
            parent_run_id: Some("r".into()),
            path: path.into(),
        }
    }

    #[test]
    fn tree_keeps_children_under_their_parent() {
        let mut t = AgentTree::default();
        t.spawned("", "a", "", "");
        t.spawned("", "b", "", "");
        t.apply_relayed(&AgentEvent::ChildSpawned {
            meta: m("a", "ra"),
            name: "x".into(),
            agent_card_url: String::new(),
            prompt: "p".into(),
            model: String::new(),
        });
        let order: Vec<&str> = t.nodes.iter().map(|a| a.path.as_str()).collect();
        assert_eq!(order, ["a", "a/x", "b"]);
        assert_eq!(t.get("a/x").unwrap().depth(), 1);
    }

    #[test]
    fn relayed_events_drive_state_transcript_and_logs() {
        let mut t = AgentTree::default();
        t.spawned("", "coder", "fix it", "fast");
        t.message("", "root", "coder", "fix it");
        assert!(matches!(t.get("coder").unwrap().transcript.rows[0], Row::User(_)));
        t.apply_relayed(&AgentEvent::TurnStarted { meta: m("coder", "c"), turn: 1 });
        t.apply_relayed(&AgentEvent::ToolStarted {
            meta: m("coder", "c"),
            call_id: "k".into(),
            name: "read_file".into(),
            args: json!({"path": "a.rs"}),
            kind: "client".into(),
        });
        assert_eq!(t.get("coder").unwrap().state, AgentState::Working);
        assert_eq!(t.tool_log.back().unwrap().line, "read_file  a.rs");
        t.apply_relayed(&AgentEvent::ToolFinished {
            meta: m("coder", "c"),
            call_id: "k".into(),
            name: "read_file".into(),
            output: "1|x".into(),
        });
        assert!(t.tool_log.back().unwrap().done);
        t.apply_relayed(&AgentEvent::ModelFinished {
            meta: m("coder", "c"),
            text: "done".into(),
            finish: "stop".into(),
            input_tokens: 0,
            cached_tokens: 0,
        });
        assert!(t.apply_relayed(&AgentEvent::AwaitingInput { meta: m("coder", "c") }));
        assert_eq!(t.get("coder").unwrap().state, AgentState::Idle);
        assert!(t.dirty);

        t.apply_relayed(&AgentEvent::TurnStarted { meta: m("coder", "c"), turn: 2 });
        t.get_mut("coder").unwrap().transcript.interrupt();
        t.apply_relayed(&AgentEvent::AwaitingInput { meta: m("coder", "c") });
        assert_eq!(t.get("coder").unwrap().state, AgentState::Interrupted);

        t.apply_relayed(&AgentEvent::TurnStarted { meta: m("coder", "c"), turn: 3 });
        t.apply_relayed(&AgentEvent::AwaitingInput { meta: m("coder", "c") });
        assert_eq!(t.get("coder").unwrap().state, AgentState::Paused, "no final reply");
    }

    #[test]
    fn exit_takes_descendants_and_respawn_starts_fresh() {
        let mut t = AgentTree::default();
        t.spawned("", "a", "", "");
        t.spawned("a", "x", "", "");
        t.get_mut("a").unwrap().transcript.push(Row::meta("old"));
        t.exited("", "a", "killed");
        assert_eq!(t.get("a/x").unwrap().state, AgentState::Exited);
        assert_eq!(t.alive_count(), 0);
        t.spawned("", "a", "again", "");
        assert_eq!(t.get("a").unwrap().state, AgentState::Starting);
        assert!(t.get("a").unwrap().transcript.rows.is_empty());
    }

    #[test]
    fn saved_agents_come_back_exited() {
        let mut t = AgentTree::default();
        t.spawned("", "a", "p", "m");
        t.get_mut("a").unwrap().transcript.push(Row::meta("kept"));
        let json = serde_json::to_string(&t.saved()).unwrap();
        let back = AgentTree::restore(serde_json::from_str(&json).unwrap());
        let a = back.get("a").unwrap();
        assert_eq!(a.state, AgentState::Exited);
        assert_eq!((a.prompt.as_str(), a.model.as_str()), ("p", "m"));
        assert_eq!(a.transcript.rows.len(), 1);
    }
}
