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
use crate::nursery::{Nursery, SendMessageTool, SpawnAgentTool, DEFAULT_MAX_DEPTH};
use crate::provider::Provider;
use crate::shellguard::{CommandReviewer, ProviderGuard};
use crate::skills::{SkillStore, SkillTool};
use crate::timer::{TimerHub, TimerTool};
use crate::tools::{
    DeleteFileTool, ListDirTool, NowTool, ReadFileTool, ReadImageTool, RunCommandTool,
    ScreenshotTool, ToolRegistry, WriteFileTool,
};

pub fn search_tools(enabled: bool) -> Vec<String> {
    if enabled {
        vec!["web_search".into(), "x_search".into()]
    } else {
        Vec::new()
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
    /// TUI Esc trips this; headless runs leave it `None`.
    pub cancel: Option<crate::agent::CancelFlag>,
    /// Shared with the TUI so Settings toggles apply on the next turn.
    pub skills: Option<std::sync::Arc<std::sync::Mutex<SkillStore>>>,
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
        Some(bg_tx),
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
    let mut tools: Vec<Box<dyn crate::tools::ClientTool>> = vec![
        Box::new(NowTool),
        Box::new(ReadFileTool::new(spec.workspace.clone())),
        Box::new(ListDirTool::new(spec.workspace.clone())),
        Box::new(WriteFileTool::new(spec.workspace.clone())),
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
    if let Some(ask_hub) = spec.ask {
        tools.push(Box::new(AskUserTool::new(
            ask_hub,
            tee.clone(),
            spec.agent_name.clone(),
            run_id.clone(),
            spec.parent_run_id.clone(),
        )));
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
        )?;
        tools.push(Box::new(SpawnAgentTool::new(n.clone(), tee.clone())));
        tools.push(Box::new(SendMessageTool::new(n.clone(), tee.clone())));
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
            context_window: 0,
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

pub fn events_dir(events_file: &Path) -> PathBuf {
    events_file
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."))
}
