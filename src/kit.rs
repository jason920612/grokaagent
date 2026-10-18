//! Shared wiring for root/worker runs: tools + optional nursery + kernel.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use crate::agent::{self, RunConfig, RunOutcome};
use crate::ask::{AskUserHub, AskUserTool};
use crate::background::{
    BackgroundHub, KillBackgroundTool, ReadBackgroundTool, RunBackgroundTool,
};
use crate::error::Result;
use crate::events::EventSink;
use crate::instructions::STATIC_INSTRUCTIONS;
use crate::memory::ProjectMemoryTool;
use crate::monitor::{AttachMonitorTool, MonitorHub, MonitorSink};
use crate::nursery::{AgentTool, Nursery, DEFAULT_MAX_DEPTH};
use crate::provider::Provider;
use crate::shellguard::{CommandReviewer, ProviderGuard};
use crate::skills::{SkillStore, SkillTool};
use crate::timer::{TimerHub, TimerTool};
use crate::tools::{
    DeleteFileTool, EditFileTool, FileViewStore, ListDirTool, NowTool, ReadFileTool, ReadImageTool, RunCommandTool,
    ScreenshotTool, ToolRegistry, WriteFileTool,
};

/// Search for the live model: Grok's own server tools on the xAI route; on
/// any other route, `grok_search` (Grok answers through the user's login)
/// when a login exists.
pub fn search_tools(enabled: bool, openai_route: bool, grok_login: bool) -> Vec<String> {
    match (enabled, openai_route) {
        (false, _) => Vec::new(),
        (true, false) => vec!["web_search".into(), "x_search".into()],
        (true, true) if grok_login => vec![crate::grok_search::GATE.into()],
        (true, true) => Vec::new(),
    }
}

pub struct KernelSpec {
    pub agent_name: String,
    pub prompt: String,
    pub model: String,
    pub max_turns: u32,
    pub workspace: PathBuf,
    pub events_file: PathBuf,
    pub events_dir: PathBuf,
    pub server_tools: Vec<String>,
    /// The search toggle, inherited by child agents.
    pub search: bool,
    pub depth: u32,
    pub parent_run_id: Option<String>,
    pub run_id: String,
    pub child_mode: String,
    pub reasoning_effort: crate::provider::ReasoningEffort,
    pub knobs: Option<std::sync::Arc<std::sync::Mutex<crate::agent::SessionKnobs>>>,
    pub inbox: Option<tokio::sync::mpsc::UnboundedReceiver<crate::agent::UserTurn>>,
    pub images: Vec<PathBuf>,
    /// When set, the model may call `ask_user` and wait for a TUI pick.
    pub ask: Option<AskUserHub>,
    /// 0 = look up from the model name.
    pub context_window: u32,
    /// TUI Esc trips this; headless runs leave it `None`.
    pub cancel: Option<crate::agent::CancelFlag>,
    /// Shared with the TUI so Settings toggles apply on the next turn.
    pub skills: Option<std::sync::Arc<std::sync::Mutex<SkillStore>>>,
    pub task: Option<std::sync::Arc<crate::task::TaskHub>>,
}

pub async fn run_with_nursery<P: Provider + Clone + 'static>(
    provider: &P,
    sink: Arc<dyn EventSink>,
    spec: KernelSpec,
) -> Result<RunOutcome> {
    let run_id = if spec.run_id.is_empty() {
        uuid::Uuid::new_v4().to_string()
    } else {
        spec.run_id.clone()
    };
    // Resolve bash (Git Bash, or fetch busybox-w32 on Windows) before any command.
    crate::shellrt::prepare().await;
    let hub = MonitorHub::new(
        spec.workspace.clone(),
        spec.events_file.clone(),
        spec.agent_name.clone(),
        run_id.clone(),
        spec.parent_run_id.clone(),
    );
    let (bg_tx, bg_rx) = tokio::sync::mpsc::unbounded_channel();
    let bg = BackgroundHub::new(
        spec.workspace.clone(),
        spec.agent_name.clone(),
        run_id.clone(),
        spec.parent_run_id.clone(),
        Some(bg_tx.clone()),
    );
    let timers = TimerHub::new(
        spec.workspace.clone(),
        spec.agent_name.clone(),
        run_id.clone(),
        spec.parent_run_id.clone(),
        Some(bg_tx.clone()),
    );
    let tee: Arc<dyn EventSink> = Arc::new(MonitorSink {
        inner: sink,
        hub: hub.clone(),
    });
    let guard: Arc<dyn CommandReviewer> = Arc::new(ProviderGuard::new(
        provider.clone(),
        spec.model.clone(),
        spec.workspace.clone(),
    ));
    let memory_root = crate::memory::default_dir()?;
    let skills = spec.skills.clone().unwrap_or_else(|| {
        Arc::new(Mutex::new(SkillStore::open().unwrap_or_else(|_| {
            SkillStore::open_at(
                spec.workspace.join(".groka").join("skills-store"),
                spec.workspace.clone(),
            )
        })))
    });
    let win = crate::wintrack::WindowHub::new();
    let views = FileViewStore::new();
    let mut tools: Vec<Box<dyn crate::tools::ClientTool>> = vec![
        Box::new(NowTool),
        Box::new(ReadFileTool::with_views(spec.workspace.clone(), views.clone())),
        Box::new(ListDirTool::new(spec.workspace.clone())),
        Box::new(WriteFileTool::with_views(spec.workspace.clone(), views.clone())),
        Box::new(EditFileTool::new(spec.workspace.clone(), views)),
        Box::new(DeleteFileTool::new(spec.workspace.clone())),
        Box::new(RunCommandTool::with_guard(
            spec.workspace.clone(),
            guard.clone(),
        )
        .with_windows(win.clone())),
        Box::new(ScreenshotTool::with_spawned(
            spec.workspace.clone(),
            {
                let bg = bg.clone();
                let win = win.clone();
                Arc::new(move || {
                    let mut pids = bg.alive_pids();
                    pids.extend(win.pids());
                    pids.sort_unstable();
                    pids.dedup();
                    pids
                })
            },
        )
        .with_windows(win.clone())),
        Box::new(ReadImageTool::new(spec.workspace.clone())),
        Box::new(AttachMonitorTool::new(
            hub.clone(),
            tee.clone(),
            Some(guard.clone()),
        )),
        Box::new(RunBackgroundTool::new(
            bg.clone(),
            tee.clone(),
            Some(guard.clone()),
        )
        .with_windows(win.clone())),
        Box::new(ReadBackgroundTool::new(bg.clone())),
        Box::new(KillBackgroundTool::new(bg.clone())),
        Box::new(TimerTool::new(
            timers.clone(),
            tee.clone(),
            Some(guard.clone()),
        )),
        Box::new(ProjectMemoryTool::new(
            memory_root.clone(),
            spec.workspace.clone(),
        )),
        Box::new(SkillTool::new(skills.clone(), spec.workspace.clone())),
    ];
    if let Ok(auth_path) = crate::auth::default_auth_path() {
        tools.push(Box::new(crate::grok_search::GrokSearchTool::new(auth_path)));
    }
    if let Some(ask_hub) = spec.ask.clone() {
        tools.extend(ask_tools(provider, &spec, ask_hub, tee.clone(), &run_id));
    }
    if let Some(task) = spec.task.clone() {
        tools.push(Box::new(
            crate::task::TaskReportTool::new(task).with_events(tee.clone(), run_id.clone()),
        ));
    }
    let nursery = if spec.depth < DEFAULT_MAX_DEPTH {
        let bin = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("grokaagent"));
        let n = Nursery::new(
            bin,
            spec.workspace.clone(),
            spec.events_dir.clone(),
            spec.depth,
            run_id.clone(),
            spec.agent_name.clone(),
            spec.model.clone(),
            spec.child_mode.clone(),
            spec.knobs.clone(),
        )?;
        // Finished children wake this session like a background exit does.
        n.set_notify(bg_tx.clone());
        n.set_search(spec.search);
        if let Some(cancel) = &spec.cancel {
            n.follow_cancel(cancel.clone());
        }
        tools.extend(AgentTool::all(n.clone(), tee.clone()));
        Some(n)
    } else {
        None
    };
    let is_root = spec.parent_run_id.is_none();
    let persist_id = run_id.clone();
    let closed_backgrounds = if is_root {
        crate::session::SessionStore::open()
            .ok()
            .map(|s| s.take_closed_backgrounds::<crate::background::ClosedBackground>(&persist_id))
            .filter(|v| !v.is_empty())
            .map(|items| crate::background::format_closed_notice(&items))
    } else {
        None
    };
    let prior_history = if is_root {
        crate::session::SessionStore::open()
            .ok()
            .map(|s| s.load_context(&persist_id))
            .unwrap_or_default()
    } else {
        Vec::new()
    };
    let persist_history = if is_root {
        let id = persist_id.clone();
        Some(std::sync::Arc::new(move |items: &[serde_json::Value]| {
            if let Ok(store) = crate::session::SessionStore::open() {
                let _ = store.save_context(&id, items);
            }
        }) as std::sync::Arc<dyn Fn(&[serde_json::Value]) + Send + Sync>)
    } else {
        None
    };
    let tools = ToolRegistry::new(tools);
    let out = agent::run(
        provider,
        &tools,
        tee.as_ref(),
        RunConfig {
            agent_name: spec.agent_name,
            instructions: STATIC_INSTRUCTIONS.into(),
            prompt: spec.prompt,
            model: spec.model,
            max_turns: spec.max_turns,
            server_tools: spec.server_tools,
            context_window: spec.context_window,
            compact_keep_recent: 0,
            parent_run_id: spec.parent_run_id,
            run_id,
            reasoning_effort: spec.reasoning_effort,
            knobs: spec.knobs,
            inbox: spec.inbox,
            bg_notify: Some(bg_rx),
            workspace: spec.workspace,
            images: spec.images,
            closed_backgrounds,
            prior_history,
            persist_history,
            cancel: spec.cancel,
            skills: Some(skills),
            task: spec.task,
        },
    )
    .await;
    hub.shutdown().await;
    let _ = bg.begin_shutdown();
    timers.shutdown();
    bg.wait_shutdown().await;
    if let Some(n) = nursery {
        n.shutdown(tee.as_ref()).await;
    }
    out
}

/// The tools that ask the human. Only an agent the user can answer may ask
/// for more folders; children ask their parent instead. In task mode
/// ask_user first lets the user's proxy answer what the user's own words
/// settle; request_workspace_access always goes to the human.
fn ask_tools<P: Provider + Clone + 'static>(
    provider: &P,
    spec: &KernelSpec,
    ask_hub: AskUserHub,
    sink: Arc<dyn EventSink>,
    run_id: &str,
) -> Vec<Box<dyn crate::tools::ClientTool>> {
    let access = crate::grants::RequestAccessTool::new(
        ask_hub.clone(),
        sink.clone(),
        spec.workspace.clone(),
        spec.agent_name.clone(),
        run_id.to_string(),
        spec.parent_run_id.clone(),
    );
    let mut ask = AskUserTool::new(
        ask_hub,
        sink.clone(),
        spec.agent_name.clone(),
        run_id.to_string(),
        spec.parent_run_id.clone(),
    );
    if let Some(task) = &spec.task {
        ask = ask.with_delegate(Arc::new(crate::task::ProxyAsk::new(
            task.clone(),
            provider.clone(),
            spec.knobs.clone(),
            spec.model.clone(),
            spec.reasoning_effort,
            spec.workspace.clone(),
            sink,
            run_id.to_string(),
        )));
    }
    vec![Box::new(access), Box::new(ask)]
}

/// Drives a headless task-mode run: prints what the user's proxy says to
/// stderr, sends `--then` messages each time the agent waits, and closes
/// the inbox once nothing is left to push (the task ended or paused, or the
/// turn cap was hit), which ends the run.
pub struct TaskDriver {
    inner: Arc<dyn EventSink>,
    hub: Arc<crate::task::TaskHub>,
    queue: Mutex<std::collections::VecDeque<String>>,
    tx: Mutex<Option<tokio::sync::mpsc::UnboundedSender<crate::agent::UserTurn>>>,
    capped: std::sync::atomic::AtomicBool,
    /// Review requests sent while the task looked live but the agent waited.
    pokes: std::sync::atomic::AtomicU32,
}

impl TaskDriver {
    pub fn new(
        inner: Arc<dyn EventSink>,
        hub: Arc<crate::task::TaskHub>,
        messages: Vec<String>,
    ) -> (Arc<Self>, tokio::sync::mpsc::UnboundedReceiver<crate::agent::UserTurn>) {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let me = Arc::new(Self {
            inner,
            hub,
            queue: Mutex::new(messages.into()),
            tx: Mutex::new(Some(tx)),
            capped: Default::default(),
            pokes: Default::default(),
        });
        (me, rx)
    }
}

impl EventSink for TaskDriver {
    fn emit(&self, event: &crate::events::AgentEvent) {
        use crate::events::AgentEvent;
        use std::sync::atomic::Ordering;
        self.inner.emit(event);
        let meta = event.meta();
        if meta.parent_run_id.is_some() || !meta.path.is_empty() {
            return;
        }
        match event {
            AgentEvent::TaskProxy { intent, text, .. } => {
                self.pokes.store(0, Ordering::SeqCst);
                eprintln!("[user-proxy {intent}] {text}");
            }
            AgentEvent::Error { message, .. } if message.starts_with("max turns (") => {
                self.capped.store(true, Ordering::SeqCst);
            }
            AgentEvent::AwaitingInput { .. } => {
                let next = self.queue.lock().unwrap_or_else(|e| e.into_inner()).pop_front();
                let mut tx = self.tx.lock().unwrap_or_else(|e| e.into_inner());
                if let (Some(text), Some(sender)) = (next, tx.as_ref()) {
                    let _ = sender.send(crate::agent::UserTurn::from(text));
                    return;
                }
                let s = self.hub.snapshot();
                let live = s.phase.is_live() && !s.paused;
                if live && !self.capped.load(Ordering::SeqCst) && self.pokes.fetch_add(1, Ordering::SeqCst) < 2 {
                    self.hub.request_review();
                    return;
                }
                *tx = None;
            }
            _ => {}
        }
    }
}

/// Feeds scripted follow-up messages to a headless run: each time the root
/// agent waits for input, the next message is sent; after the last one the
/// inbox closes and the run ends.
pub struct Followups {
    inner: Arc<dyn EventSink>,
    queue: Mutex<std::collections::VecDeque<String>>,
    tx: Mutex<Option<tokio::sync::mpsc::UnboundedSender<crate::agent::UserTurn>>>,
}

impl Followups {
    pub fn new(
        inner: Arc<dyn EventSink>,
        messages: Vec<String>,
    ) -> (Arc<Self>, tokio::sync::mpsc::UnboundedReceiver<crate::agent::UserTurn>) {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let me = Arc::new(Self { inner, queue: Mutex::new(messages.into()), tx: Mutex::new(Some(tx)) });
        (me, rx)
    }
}

impl EventSink for Followups {
    fn emit(&self, event: &crate::events::AgentEvent) {
        self.inner.emit(event);
        if let crate::events::AgentEvent::AwaitingInput { meta } = event {
            if meta.parent_run_id.is_some() || !meta.path.is_empty() {
                return;
            }
            let next = self.queue.lock().unwrap_or_else(|e| e.into_inner()).pop_front();
            let mut tx = self.tx.lock().unwrap_or_else(|e| e.into_inner());
            match (next, tx.as_ref()) {
                (Some(text), Some(sender)) => {
                    let _ = sender.send(crate::agent::UserTurn::from(text));
                }
                _ => *tx = None,
            }
        }
    }
}

pub fn events_dir(events_file: &Path) -> PathBuf {
    events_file
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::AgentEvent;
    use crate::provider::{CompactRequest, CompactResponse, CompleteRequest, CompleteResponse};
    use serde_json::json;

    struct Rec(Mutex<Vec<AgentEvent>>);
    impl EventSink for Rec {
        fn emit(&self, e: &AgentEvent) {
            self.0.lock().unwrap().push(e.clone());
        }
    }

    /// A proxy that always leaves the question to the human.
    struct Forwarding(Mutex<Vec<CompleteRequest>>);
    impl Provider for Forwarding {
        async fn complete(&self, req: CompleteRequest) -> crate::error::Result<CompleteResponse> {
            self.0.lock().unwrap().push(req);
            Ok(CompleteResponse { text: json!({"forward": true, "why": "not settled"}).to_string(), ..CompleteResponse::new("p") })
        }
        async fn compact(&self, _req: CompactRequest) -> crate::error::Result<CompactResponse> {
            Err(crate::error::Error::Provider("no compact".into()))
        }
    }

    fn spec(ws: &Path, task: Option<Arc<crate::task::TaskHub>>, ask: AskUserHub) -> KernelSpec {
        KernelSpec {
            agent_name: "root".into(),
            prompt: "go".into(),
            model: "grok-4.6".into(),
            max_turns: 0,
            workspace: ws.to_path_buf(),
            events_file: ws.join("events.jsonl"),
            events_dir: ws.to_path_buf(),
            server_tools: vec![],
            search: false,
            depth: 0,
            parent_run_id: None,
            run_id: "r".into(),
            child_mode: "grok".into(),
            reasoning_effort: crate::provider::ReasoningEffort::High,
            knobs: None,
            inbox: None,
            images: vec![],
            ask: Some(ask),
            context_window: 0,
            cancel: None,
            skills: None,
            task,
        }
    }

    #[tokio::test]
    async fn workspace_access_always_asks_the_human_while_ask_user_consults_the_proxy() {
        let dir = tempfile::tempdir().unwrap();
        let ws = dir.path().join("ws");
        let other = dir.path().join("shared-data");
        std::fs::create_dir_all(&ws).unwrap();
        std::fs::create_dir_all(&other).unwrap();
        let hub = crate::task::TaskHub::new("");
        hub.start_goal("parse the shared fixtures");
        let provider = Arc::new(Forwarding(Mutex::new(Vec::new())));
        let ask_hub = AskUserHub::new();
        let rec = Arc::new(Rec(Mutex::new(Vec::new())));
        let spec = spec(&ws, Some(hub.clone()), ask_hub.clone());
        let tools = ask_tools(&provider, &spec, ask_hub.clone(), rec.clone(), "r");
        let names: Vec<String> = tools.iter().map(|t| t.spec().name).collect();
        assert_eq!(names, ["request_workspace_access", "ask_user"]);

        ask_hub.answer(json!({"cancelled": false, "selected": [{"id": "deny", "label": "拒絕", "value": null}]}).to_string());
        let out = tools[0]
            .call(&json!({"path": other.to_string_lossy(), "reason": "read the shared fixtures for the parser tests"}))
            .await
            .unwrap();
        assert!(out.contains("\"granted\":false"), "{out}");
        assert!(provider.0.lock().unwrap().is_empty(), "request_workspace_access is never delegated");
        assert_eq!(rec.0.lock().unwrap().iter().filter(|e| matches!(e, AgentEvent::AskUser { .. })).count(), 1);

        ask_hub.answer(crate::ask::encode_answer(&[crate::ask::Picked { id: "opt0".into(), label: "a".into(), value: None }]));
        let out = tools[1].call(&json!({"question": "Which one?", "options": ["a", "b"]})).await.unwrap();
        assert!(!out.contains("answered_by"), "{out}");
        assert_eq!(provider.0.lock().unwrap().len(), 1, "ask_user asked the proxy first");
        assert_eq!(rec.0.lock().unwrap().iter().filter(|e| matches!(e, AgentEvent::AskUser { .. })).count(), 2);

        // Without a task, nothing is consulted.
        let plain = ask_tools(&provider, &self::spec(&ws, None, ask_hub.clone()), ask_hub.clone(), rec.clone(), "r");
        ask_hub.answer(crate::ask::encode_answer(&[crate::ask::Picked { id: "opt1".into(), label: "b".into(), value: None }]));
        plain[1].call(&json!({"question": "Which one?", "options": ["a", "b"]})).await.unwrap();
        assert_eq!(provider.0.lock().unwrap().len(), 1);
    }

    fn wait_event() -> AgentEvent {
        AgentEvent::AwaitingInput {
            meta: crate::events::EventMeta {
                ts: chrono::Utc::now(),
                agent_name: "root".into(),
                run_id: "r".into(),
                parent_run_id: None,
                path: String::new(),
            },
        }
    }

    #[test]
    fn the_headless_driver_closes_once_the_task_is_over() {
        let hub = crate::task::TaskHub::new("");
        hub.start_goal("x");
        let rec: Arc<dyn EventSink> = Arc::new(Rec(Mutex::new(Vec::new())));
        let (driver, mut rx) = TaskDriver::new(rec, hub.clone(), vec!["then this".into()]);
        // Follow-ups go first.
        driver.emit(&wait_event());
        assert_eq!(rx.try_recv().unwrap().text, "then this");
        // Live but waiting: ask the proxy to look, a couple of times at most.
        hub.take_review_request();
        driver.emit(&wait_event());
        assert!(hub.take_review_request());
        assert!(rx.try_recv().is_err());
        driver.emit(&wait_event());
        driver.emit(&wait_event());
        assert!(matches!(rx.try_recv(), Err(tokio::sync::mpsc::error::TryRecvError::Disconnected)));

        // A task that ended closes the inbox on the next wait.
        let hub = crate::task::TaskHub::new("");
        hub.start_goal("x");
        hub.accept_done();
        let rec: Arc<dyn EventSink> = Arc::new(Rec(Mutex::new(Vec::new())));
        let (driver, mut rx) = TaskDriver::new(rec, hub, vec![]);
        driver.emit(&wait_event());
        assert!(matches!(rx.try_recv(), Err(tokio::sync::mpsc::error::TryRecvError::Disconnected)));
    }
}
