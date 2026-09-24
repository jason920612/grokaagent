// Workbench: the VS Code-style shell around the chat. Each child agent gets a
// read-only transcript built by the same row logic as the main chat (its
// `ChatState` is swapped into `App` while events are applied or the tab is
// drawn), plus a cross-agent tool timeline and event log for the bottom panel.

const TOOL_LOG_CAP: usize = 400;
const EVENT_LOG_CAP: usize = 400;
const AGENT_ROWS_CAP: usize = 1_500;

/// Chat-view state that differs per transcript. `App` holds the visible one;
/// the others live in their `AgentTab` and are swapped in on demand.
struct ChatState {
    rows: Vec<Row>,
    status: String,
    activity: String,
    cache: String,
    running: bool,
    awaiting: bool,
    scroll: u16,
    stick_bottom: bool,
    streaming: bool,
    open_tool: Option<(usize, usize)>,
    seal_tools: bool,
    work_started: Option<Instant>,
    chat_sel: ChatSel,
}

impl Default for ChatState {
    fn default() -> Self {
        Self {
            rows: Vec::new(),
            status: "啟動中".into(),
            activity: String::new(),
            cache: String::new(),
            running: false,
            awaiting: false,
            scroll: 0,
            stick_bottom: true,
            streaming: false,
            open_tool: None,
            seal_tools: false,
            work_started: None,
            chat_sel: ChatSel::None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
enum AgentState {
    Starting,
    Working,
    Idle,
    Paused,
    Interrupted,
    Exited,
}

impl AgentState {
    fn label(self) -> &'static str {
        match self {
            Self::Starting => "啟動中",
            Self::Working => "工作中",
            Self::Idle => "閒置",
            Self::Paused => "暫停",
            Self::Interrupted => "已中斷",
            Self::Exited => "已結束",
        }
    }

    fn icon(self, tick: u8) -> &'static str {
        match self {
            Self::Starting => "◌",
            Self::Working => ["◐", "◓", "◑", "◒"][(tick as usize / 3) % 4],
            Self::Idle => "✓",
            Self::Paused => "‖",
            Self::Interrupted => "⊘",
            Self::Exited => "○",
        }
    }

    fn color(self) -> Color {
        match self {
            Self::Starting | Self::Working => ACCENT,
            Self::Idle => AGENT,
            Self::Paused | Self::Interrupted => TOOL,
            Self::Exited => DIM,
        }
    }

    fn as_key(self) -> &'static str {
        match self {
            Self::Starting => "starting",
            Self::Working => "working",
            Self::Idle => "idle",
            Self::Paused => "paused",
            Self::Interrupted => "interrupted",
            Self::Exited => "exited",
        }
    }

    fn live(self) -> bool {
        matches!(self, Self::Starting | Self::Working)
    }
}

struct AgentTab {
    /// Tree path: `coder`, `coder/lint`.
    path: String,
    name: String,
    model: String,
    prompt: String,
    state: AgentState,
    alive: bool,
    turn: u32,
    tools: u32,
    /// Saw a model stop since the last turn started (natural idle vs interrupt).
    stopped: bool,
    view: ChatState,
}

impl AgentTab {
    fn new(path: &str) -> Self {
        let name = path.rsplit('/').next().unwrap_or(path).to_string();
        Self {
            path: path.to_string(),
            name,
            model: String::new(),
            prompt: String::new(),
            state: AgentState::Starting,
            alive: true,
            turn: 0,
            tools: 0,
            stopped: false,
            view: ChatState::default(),
        }
    }

    fn depth(&self) -> usize {
        self.path.matches('/').count()
    }

    fn parent(&self) -> Option<&str> {
        self.path.rsplit_once('/').map(|(p, _)| p)
    }

    fn set_state(&mut self, state: AgentState) {
        self.state = state;
        self.alive = state != AgentState::Exited;
        self.view.status = state.label().into();
    }
}

#[derive(Serialize, Deserialize)]
struct SavedAgent {
    path: String,
    model: String,
    prompt: String,
    state: AgentState,
    rows: Vec<Row>,
}

#[derive(Clone)]
struct ToolEntry {
    path: String,
    call_id: String,
    name: String,
    line: String,
    phase: String,
    done: bool,
    started: Instant,
    ms: u64,
}

#[derive(Clone)]
struct EventEntry {
    at: String,
    path: String,
    kind: &'static str,
    text: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SideView {
    Sessions,
    Agents,
    Changes,
    Background,
    Task,
}

impl SideView {
    const ALL: [SideView; 5] = [
        SideView::Sessions,
        SideView::Agents,
        SideView::Changes,
        SideView::Background,
        SideView::Task,
    ];

    fn icon(self) -> &'static str {
        match self {
            Self::Sessions => "≡",
            Self::Agents => "◎",
            Self::Changes => "±",
            Self::Background => "▶",
            Self::Task => "✔",
        }
    }

    fn title(self) -> &'static str {
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
enum BottomTab {
    Tools,
    Output,
    Events,
}

impl BottomTab {
    const ALL: [BottomTab; 3] = [BottomTab::Tools, BottomTab::Output, BottomTab::Events];

    fn title(self) -> &'static str {
        match self {
            Self::Tools => "工具",
            Self::Output => "輸出",
            Self::Events => "事件",
        }
    }
}

/// Per-session workbench state (parked with the session).
#[derive(Default)]
struct Workbench {
    agents: Vec<AgentTab>,
    /// Open editor tabs besides the main chat, by agent path.
    open: Vec<String>,
    /// Active editor tab; `None` = main chat.
    active: Option<String>,
    tool_log: VecDeque<ToolEntry>,
    event_log: VecDeque<EventEntry>,
    /// Background shown in the Output panel.
    output: Option<String>,
    /// Agent transcripts changed since the last save.
    dirty: bool,
}

impl Workbench {
    fn agent(&self, path: &str) -> Option<&AgentTab> {
        self.agents.iter().find(|a| a.path == path)
    }

    fn agent_mut(&mut self, path: &str) -> Option<&mut AgentTab> {
        self.agents.iter_mut().find(|a| a.path == path)
    }

    fn ensure(&mut self, path: &str) -> &mut AgentTab {
        if let Some(i) = self.agents.iter().position(|a| a.path == path) {
            return &mut self.agents[i];
        }
        // Keep tree order: insert after the parent's last descendant.
        let tab = AgentTab::new(path);
        let at = match tab.parent() {
            Some(parent) => self
                .agents
                .iter()
                .rposition(|a| a.path == parent || a.path.starts_with(&format!("{parent}/")))
                .map(|i| i + 1)
                .unwrap_or(self.agents.len()),
            None => self.agents.len(),
        };
        self.agents.insert(at, tab);
        &mut self.agents[at]
    }

    fn live_count(&self) -> usize {
        self.agents.iter().filter(|a| a.state.live()).count()
    }

    fn alive_count(&self) -> usize {
        self.agents.iter().filter(|a| a.alive).count()
    }

    fn log_event(&mut self, path: &str, kind: &'static str, text: String) {
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

    fn tool_started(&mut self, path: &str, call_id: &str, name: &str, args: &Value) {
        if self.tool_log.len() >= TOOL_LOG_CAP {
            self.tool_log.pop_front();
        }
        self.tool_log.push_back(ToolEntry {
            path: path.to_string(),
            call_id: call_id.to_string(),
            name: name.to_string(),
            line: tool_started_line(name, args),
            phase: "執行中".into(),
            done: false,
            started: Instant::now(),
            ms: 0,
        });
    }

    fn tool_finished(&mut self, path: &str, call_id: &str, name: &str, output: &str) {
        let hit = self.tool_log.iter_mut().rev().find(|t| {
            t.path == path && !t.done && (t.call_id == call_id || (call_id.is_empty() && t.name == name))
        });
        if let Some(t) = hit {
            t.done = true;
            t.ms = t.started.elapsed().as_millis() as u64;
            t.phase = tool_phase(name, output).into();
        }
    }

    fn saved(&self) -> Vec<SavedAgent> {
        self.agents
            .iter()
            .map(|a| SavedAgent {
                path: a.path.clone(),
                model: a.model.clone(),
                prompt: a.prompt.clone(),
                state: a.state,
                rows: a.view.rows.clone(),
            })
            .collect()
    }

    fn restore(saved: Vec<SavedAgent>) -> Self {
        let mut bench = Self::default();
        for s in saved {
            let mut tab = AgentTab::new(&s.path);
            tab.model = s.model;
            tab.prompt = s.prompt;
            tab.view.rows = s.rows;
            // Processes do not survive a restart.
            tab.set_state(AgentState::Exited);
            bench.agents.push(tab);
        }
        bench
    }
}

/// Outcome label for a finished tool call.
fn tool_phase(name: &str, output: &str) -> &'static str {
    let result = serde_json::from_str::<Value>(output).unwrap_or(Value::Null);
    let cancelled = result.get("cancelled").and_then(Value::as_bool) == Some(true);
    let failed = result.get("error").is_some_and(|e| !e.is_null() && e != "")
        || result
            .get("exit_code")
            .and_then(Value::as_i64)
            .is_some_and(|code| code != 0);
    if cancelled {
        "已停止"
    } else if failed {
        "失敗"
    } else if name == "spawn_agent" {
        "已啟動"
    } else {
        "完成"
    }
}

/// Child path for an event about `name` raised by the agent at `by` ("" = root).
fn child_path(by: &str, name: &str) -> String {
    if by.is_empty() {
        name.to_string()
    } else {
        format!("{by}/{name}")
    }
}

impl App {
    /// Exchange the visible chat state with `c`.
    fn swap_chat(&mut self, c: &mut ChatState) {
        std::mem::swap(&mut self.rows, &mut c.rows);
        std::mem::swap(&mut self.status, &mut c.status);
        std::mem::swap(&mut self.activity, &mut c.activity);
        std::mem::swap(&mut self.cache, &mut c.cache);
        std::mem::swap(&mut self.running, &mut c.running);
        std::mem::swap(&mut self.awaiting, &mut c.awaiting);
        std::mem::swap(&mut self.scroll, &mut c.scroll);
        std::mem::swap(&mut self.stick_bottom, &mut c.stick_bottom);
        std::mem::swap(&mut self.streaming, &mut c.streaming);
        std::mem::swap(&mut self.open_tool, &mut c.open_tool);
        std::mem::swap(&mut self.seal_tools, &mut c.seal_tools);
        std::mem::swap(&mut self.work_started, &mut c.work_started);
        std::mem::swap(&mut self.chat_sel, &mut c.chat_sel);
    }

    /// Run `f` with the agent's transcript installed as the visible chat.
    fn with_agent<R>(&mut self, path: &str, f: impl FnOnce(&mut Self) -> R) -> Option<R> {
        let i = self.bench.agents.iter().position(|a| a.path == path)?;
        let mut view = std::mem::take(&mut self.bench.agents[i].view);
        self.swap_chat(&mut view);
        let out = f(self);
        self.swap_chat(&mut view);
        if let Some(a) = self.bench.agent_mut(path) {
            a.view = view;
        }
        Some(out)
    }

    /// Run `f` against whichever transcript the active editor tab shows.
    fn with_active_view<R>(&mut self, f: impl FnOnce(&mut Self) -> R) -> R {
        match self.bench.active.clone() {
            Some(path) if self.bench.agent(&path).is_some() => {
                let mut f = Some(f);
                self.with_agent(&path, |app| (f.take().unwrap())(app))
                    .expect("agent checked above")
            }
            _ => f(self),
        }
    }

    fn open_agent_tab(&mut self, path: &str) {
        if self.bench.agent(path).is_none() {
            return;
        }
        if !self.bench.open.iter().any(|p| p == path) {
            self.bench.open.push(path.to_string());
        }
        self.bench.active = Some(path.to_string());
        self.open_tool = None;
        self.focus = Focus::Chat;
    }

    fn close_agent_tab(&mut self, path: &str) {
        let Some(i) = self.bench.open.iter().position(|p| p == path) else {
            return;
        };
        self.bench.open.remove(i);
        if self.bench.active.as_deref() == Some(path) {
            self.bench.active = if i > 0 {
                self.bench.open.get(i - 1).cloned()
            } else {
                self.bench.open.first().cloned()
            };
        }
    }

    /// Tab index 0 = main chat, 1.. = open agent tabs.
    fn activate_tab(&mut self, index: usize) {
        if index == 0 {
            self.bench.active = None;
            return;
        }
        if let Some(p) = self.bench.open.get(index - 1).cloned() {
            self.bench.active = Some(p);
        }
    }

    fn cycle_tab(&mut self, delta: i32) {
        let n = self.bench.open.len() as i32 + 1;
        let cur = match &self.bench.active {
            None => 0,
            Some(p) => self.bench.open.iter().position(|o| o == p).map(|i| i as i32 + 1).unwrap_or(0),
        };
        self.activate_tab((cur + delta).rem_euclid(n) as usize);
    }

    fn viewing_agent(&self) -> bool {
        self.bench
            .active
            .as_deref()
            .is_some_and(|p| self.bench.agent(p).is_some())
    }

    /// Record a spawn by the agent at `by` (root = "").
    fn agent_spawned(&mut self, by: &str, name: &str, prompt: &str, model: &str) {
        let path = child_path(by, name);
        let tab = self.bench.ensure(&path);
        if !prompt.is_empty() {
            tab.prompt = prompt.to_string();
        }
        if !model.is_empty() {
            tab.model = model.to_string();
        }
        // A respawn under the same name starts a fresh transcript.
        if tab.state == AgentState::Exited {
            tab.view = ChatState::default();
        }
        tab.set_state(AgentState::Starting);
        let label = if model.is_empty() {
            format!("啟動 {path}")
        } else {
            format!("啟動 {path} · {model}")
        };
        self.bench.log_event(by, "agent", label);
        self.bench.dirty = true;
        self.child_count = self.bench.alive_count() as u32;
    }

    fn agent_exited(&mut self, by: &str, name: &str, detail: &str) {
        let path = child_path(by, name);
        if let Some(tab) = self.bench.agent_mut(&path) {
            tab.set_state(AgentState::Exited);
            tab.view.running = false;
            tab.view.activity.clear();
        }
        // Descendants die with their parent's process.
        let prefix = format!("{path}/");
        for a in self.bench.agents.iter_mut().filter(|a| a.path.starts_with(&prefix)) {
            if a.alive {
                a.set_state(AgentState::Exited);
                a.view.running = false;
            }
        }
        self.with_agent(&path, |app| app.push(Row::Meta(format!("已結束  {detail}"))));
        self.bench.log_event(by, "agent", format!("{path} 結束  {detail}"));
        self.bench.dirty = true;
        self.child_count = self.bench.alive_count() as u32;
    }

    /// A2A text between the agent at `by` and one of its children.
    fn agent_message(&mut self, by: &str, from: &str, to: &str, text: &str) {
        let by_name = by.rsplit('/').next().unwrap_or("");
        let (child, down) = if self.bench.agent(&child_path(by, to)).is_some()
            && (from == by_name || by.is_empty())
        {
            (to, true)
        } else {
            (from, false)
        };
        let path = child_path(by, child);
        if down {
            // Instructions from the parent read as user turns in the child's transcript.
            let text = text.to_string();
            self.with_agent(&path, |app| app.push(Row::User(UserMsg::from(text))));
            self.bench.dirty = true;
        }
        let preview: String = text.chars().take(160).collect();
        let arrow = if down {
            format!("{} → {path}", if by.is_empty() { "主代理" } else { by })
        } else {
            format!("{path} → {}", if by.is_empty() { "主代理" } else { by })
        };
        self.bench.log_event(by, "message", format!("{arrow}  {}", preview.replace('\n', " ")));
    }

    /// One relayed event from a descendant agent.
    fn apply_child_work(&mut self, ev: AgentEvent) {
        let path = if ev.path().is_empty() {
            // Relays from before tree paths existed: fall back to the name.
            ev.meta().agent_name.clone()
        } else {
            ev.path().to_string()
        };
        if path.is_empty() || path == "root" {
            return;
        }
        match &ev {
            AgentEvent::ChildSpawned { name, prompt, model, .. } => {
                self.bench.ensure(&path);
                self.agent_spawned(&path, name, prompt, model);
                self.with_agent(&path, |app| app.push(Row::Meta(format!("子代理 {name} 已啟動"))));
                return;
            }
            AgentEvent::ChildExited { name, detail, .. } => {
                self.agent_exited(&path, name, detail);
                return;
            }
            AgentEvent::AgentMessage { from, to, text, .. } => {
                self.bench.ensure(&path);
                self.agent_message(&path, from, to, text);
                return;
            }
            _ => {}
        }
        if self.bench.agent(&path).is_none() {
            self.bench.ensure(&path).set_state(AgentState::Working);
        }
        // Tab state from the agent's own lifecycle.
        if let Some(tab) = self.bench.agent_mut(&path) {
            match &ev {
                AgentEvent::RunStarted { model, .. } => {
                    if tab.model.is_empty() {
                        tab.model = model.clone();
                    }
                    tab.set_state(AgentState::Working);
                }
                AgentEvent::TurnStarted { turn, .. } => {
                    tab.turn = *turn;
                    tab.stopped = false;
                    tab.set_state(AgentState::Working);
                }
                AgentEvent::ModelFinished { finish, .. } if finish == "stop" => tab.stopped = true,
                AgentEvent::ToolStarted { .. } => tab.tools += 1,
                _ => {}
            }
        }
        match &ev {
            AgentEvent::ToolStarted { call_id, name, args, .. } => {
                self.bench.tool_started(&path, call_id, name, args);
            }
            AgentEvent::ToolFinished { call_id, name, output, .. } => {
                self.bench.tool_finished(&path, call_id, name, output);
            }
            AgentEvent::Error { message, .. } => {
                self.bench.log_event(&path, "error", message.clone());
            }
            AgentEvent::Notice { message, .. } => {
                self.bench.log_event(&path, "notice", message.clone());
            }
            AgentEvent::BackgroundStarted { name, command, .. } => {
                self.bench.log_event(&path, "bg", format!("背景 {name} 開始  $ {command}"));
            }
            AgentEvent::BackgroundExited { name, detail, .. } => {
                self.bench.log_event(&path, "bg", format!("背景 {name} 結束  {detail}"));
            }
            _ => {}
        }
        let settle = matches!(ev, AgentEvent::AwaitingInput { .. } | AgentEvent::RunFinished { .. });
        match ev {
            // Transcript events: reuse the main chat's row logic.
            AgentEvent::RunStarted { .. }
            | AgentEvent::TurnStarted { .. }
            | AgentEvent::ReasoningDelta { .. }
            | AgentEvent::ModelDelta { .. }
            | AgentEvent::ModelFinished { .. }
            | AgentEvent::ToolStarted { .. }
            | AgentEvent::ToolFinished { .. }
            | AgentEvent::ServerToolObserved { .. }
            | AgentEvent::FileChanged { .. }
            | AgentEvent::ContextCompacted { .. }
            | AgentEvent::Notice { .. }
            | AgentEvent::Error { .. }
            | AgentEvent::AwaitingInput { .. } => {
                self.with_agent(&path, |app| {
                    app.apply_event(ev);
                    if app.rows.len() > AGENT_ROWS_CAP {
                        let cut = app.rows.len() - AGENT_ROWS_CAP;
                        app.rows.drain(0..cut);
                        app.open_tool = None;
                    }
                });
            }
            AgentEvent::RunFinished { reason, .. } => {
                self.with_agent(&path, |app| {
                    app.running = false;
                    app.streaming = false;
                    app.finish_open_think();
                    app.activity.clear();
                    app.push(Row::Meta(format!("工作階段結束 ({reason})")));
                });
            }
            AgentEvent::MonitorAttached { name, command, .. } => {
                self.with_agent(&path, |app| app.push(Row::Meta(format!("監控 {name}  $ {command}"))));
            }
            AgentEvent::BackgroundStarted { name, command, .. } => {
                self.with_agent(&path, |app| app.push(Row::Meta(format!("背景 {name}  $ {command}"))));
            }
            AgentEvent::BackgroundExited { name, detail, .. } => {
                self.with_agent(&path, |app| app.push(Row::Meta(format!("背景 {name} 結束  {detail}"))));
            }
            AgentEvent::TimerStarted { name, seconds, .. } => {
                self.with_agent(&path, |app| app.push(Row::Meta(format!("計時器 {name} {seconds}s"))));
            }
            AgentEvent::TimerFired { name, .. } => {
                self.with_agent(&path, |app| app.push(Row::Meta(format!("計時器 {name} 到時"))));
            }
            _ => {}
        }
        if settle {
            if let Some(tab) = self.bench.agent_mut(&path) {
                let next = if tab.state == AgentState::Exited {
                    AgentState::Exited
                } else if tab.stopped {
                    AgentState::Idle
                } else if tab.view.status == "已停止" || tab.view.activity == "中斷中" {
                    AgentState::Interrupted
                } else {
                    AgentState::Paused
                };
                tab.set_state(next);
            }
            self.bench.dirty = true;
            self.persist_agents();
        }
        self.child_count = self.bench.alive_count() as u32;
    }

    fn persist_agents(&mut self) {
        if !self.bench.dirty {
            return;
        }
        self.bench.dirty = false;
        if let Some(store) = &self.store {
            let _ = store.save_agents(&self.session.id, &self.bench.saved());
        }
    }

    /// Log a root-agent event into the workbench panels.
    fn log_root_event(&mut self, ev: &AgentEvent) {
        match ev {
            AgentEvent::ToolStarted { call_id, name, args, .. } => {
                self.bench.tool_started("", call_id, name, args);
            }
            AgentEvent::ToolFinished { call_id, name, output, .. } => {
                self.bench.tool_finished("", call_id, name, output);
            }
            AgentEvent::Error { message, .. } => self.bench.log_event("", "error", message.clone()),
            AgentEvent::Notice { message, .. } => self.bench.log_event("", "notice", message.clone()),
            AgentEvent::ContextCompacted { method, dropped_items, kept_items, .. } => {
                self.bench.log_event("", "notice", format!("壓縮 ({method}) 丟 {dropped_items} 留 {kept_items}"));
            }
            AgentEvent::BackgroundStarted { name, command, .. } => {
                self.bench.log_event("", "bg", format!("背景 {name} 開始  $ {command}"));
            }
            AgentEvent::BackgroundExited { name, detail, .. } => {
                self.bench.log_event("", "bg", format!("背景 {name} 結束  {detail}"));
            }
            AgentEvent::MonitorAttached { name, command, .. } => {
                self.bench.log_event("", "bg", format!("監控 {name}  $ {command}"));
            }
            AgentEvent::TimerFired { name, detail, .. } => {
                self.bench.log_event("", "bg", format!("計時器 {name} 到時  {detail}"));
            }
            _ => {}
        }
    }

    /// Every file change in the session: (agent path, row, call, file path, kind).
    fn all_file_changes(&self) -> Vec<(String, usize, usize, String, String)> {
        let mut out = Vec::new();
        let mut scan = |path: &str, rows: &[Row]| {
            for (ri, row) in rows.iter().enumerate() {
                if let Row::Tools(g) = row {
                    for (ci, call) in g.calls.iter().enumerate() {
                        for f in &call.files {
                            out.push((path.to_string(), ri, ci, f.path.clone(), f.kind.clone()));
                        }
                    }
                }
            }
        };
        // `self.rows` is always the main chat outside a view swap.
        scan("", &self.rows);
        for a in &self.bench.agents {
            scan(&a.path, &a.view.rows);
        }
        out
    }

    /// Open a tool call's detail popup in the transcript that owns it.
    fn reveal_tool(&mut self, path: &str, row: usize, call: usize) {
        if path.is_empty() {
            self.bench.active = None;
            self.open_tool = Some((row, call));
        } else {
            self.open_agent_tab(path);
            if let Some(a) = self.bench.agent_mut(path) {
                a.view.open_tool = Some((row, call));
            }
        }
        self.focus = Focus::Chat;
    }
}
