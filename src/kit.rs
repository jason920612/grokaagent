//! Shared wiring for root/worker runs: tools + optional nursery + kernel.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::agent::{self, RunConfig, RunOutcome};
use crate::ask::{AskUserHub, AskUserTool};
use crate::background::{
    BackgroundHub, KillBackgroundTool, ReadBackgroundTool, RunBackgroundTool,
};
use crate::error::Result;
use crate::events::EventSink;
use crate::instructions::STATIC_INSTRUCTIONS;
use crate::monitor::{AttachMonitorTool, MonitorHub, MonitorSink};
use crate::nursery::{Nursery, SendMessageTool, SpawnAgentTool, DEFAULT_MAX_DEPTH};
use crate::provider::Provider;
use crate::shellguard::{CommandReviewer, ProviderGuard};
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
    let bg = BackgroundHub::new(
        spec.workspace.clone(),
        spec.agent_name.clone(),
        run_id.clone(),
        spec.parent_run_id.clone(),
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
    let mut tools: Vec<Box<dyn crate::tools::ClientTool>> = vec![
        Box::new(NowTool),
        Box::new(ReadFileTool::new(spec.workspace.clone())),
        Box::new(ListDirTool::new(spec.workspace.clone())),
        Box::new(WriteFileTool::new(spec.workspace.clone())),
        Box::new(DeleteFileTool::new(spec.workspace.clone())),
        Box::new(RunCommandTool::with_guard(
            spec.workspace.clone(),
            guard.clone(),
        )),
        Box::new(ScreenshotTool::with_spawned(
            spec.workspace.clone(),
            {
                let bg = bg.clone();
                Arc::new(move || bg.alive_pids())
            },
        )),
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
        )),
        Box::new(ReadBackgroundTool::new(bg.clone())),
        Box::new(KillBackgroundTool::new(bg.clone())),
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
            workspace: spec.workspace,
            images: spec.images,
        },
    )
    .await;
    hub.shutdown().await;
    bg.shutdown().await;
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
