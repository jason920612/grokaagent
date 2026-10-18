//! Workflows: the root agent writes a JavaScript script that runs many
//! subagents deterministically (fan-out, pipelines, adversarial checks),
//! instead of steering each child turn by turn. See
//! docs/adr/0003-workflows.md.
//!
//! A run is started by the `workflow` tool and goes on in the background;
//! the main agent keeps working and gets one notice when it ends. Every
//! `agent()` call is a one-shot worker process ([`pool`]), at most
//! [`max_concurrency`] at a time. Each run keeps its script and a journal
//! under `.groka/workflow-runs/<id>/`, so an edited script can be resumed
//! with the unchanged calls answered from the journal ([`journal`]).

pub mod engine;
pub mod journal;
pub mod pool;
pub mod submit;

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::sync::{mpsc, watch, Semaphore};

use crate::agent::{SessionKnobs, UserTurn};
use crate::error::{Error, Result};
use crate::events::{AgentEvent, EventMeta, EventSink};
use crate::provider::{CacheUsage, ReasoningEffort};
use crate::tools::{ClientTool, ToolCallFut, ToolSpec};

use engine::{Reply, ReplyFut, Script};
use pool::{AgentEnd, AgentJob, PoolEnv};

/// A run's end, sent to the main agent: `[workflow wf-… finished]`.
pub const NOTICE_PREFIX: &str = "[workflow ";
/// Runs that the closing of the previous run of this conversation stopped.
pub const CLOSED_NOTICE_PREFIX: &str = "[workflows closed]";
/// Where runs keep their script, journal and agent logs (under `.groka`).
pub const RUNS_DIR: &str = "workflow-runs";
/// Saved workflows, by name: `<workspace>/.groka/workflows/<name>.js`, then
/// `~/.grokaagent/workflows/<name>.js`.
pub const SAVED_DIR: &str = "workflows";
/// Agents one run may start in all (a runaway-loop backstop).
pub const MAX_AGENTS: usize = 1000;
/// The script's return value in the end notice is cut here; the whole of
/// it is in `result.json`.
const RESULT_CAP: usize = 20_000;

/// The authoring guide `workflow {"reference": true}` returns.
pub const REFERENCE: &str = include_str!("reference.md");

/// How big the session's workflows should be: a guideline the model is
/// told, not a hard limit (the hard caps are [`MAX_AGENTS`] and
/// [`max_concurrency`]).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum SizeGuide {
    Small,
    #[default]
    Medium,
    Large,
    Unlimited,
}

impl SizeGuide {
    pub const ALL: [SizeGuide; 4] = [Self::Small, Self::Medium, Self::Large, Self::Unlimited];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Small => "small",
            Self::Medium => "medium",
            Self::Large => "large",
            Self::Unlimited => "unlimited",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Small => "小（5 個代理以下）",
            Self::Medium => "中（10 個代理以下）",
            Self::Large => "大（30 個代理以下）",
            Self::Unlimited => "不限",
        }
    }

    pub fn short(self) -> &'static str {
        match self {
            Self::Small => "小",
            Self::Medium => "中",
            Self::Large => "大",
            Self::Unlimited => "不限",
        }
    }

    pub fn next(self, forward: bool) -> Self {
        let i = Self::ALL.iter().position(|s| *s == self).unwrap_or(1);
        let n = Self::ALL.len();
        Self::ALL[if forward { (i + 1) % n } else { (i + n - 1) % n }]
    }

    /// The sentence the system prompt carries.
    fn guideline(self) -> String {
        let scale = match self {
            Self::Small => "small — keep workflows under 5 agents",
            Self::Medium => "medium — keep workflows under 10 agents",
            Self::Large => "large — keep workflows under 30 agents",
            Self::Unlimited => "unlimited — scale workflows to what the task needs",
        };
        format!("This session's workflow size guideline: {scale}. This is a guideline, not a hard limit: follow it unless the user's request calls for a different scale.")
    }
}

/// Appended to the root's system prompt in a TUI session (byte-stable while
/// the settings stay the same, so the prompt cache misses once per change).
pub fn instructions_suffix(ultracode: bool, size: SizeGuide) -> String {
    let mut out = format!("\n\nWorkflows:\n- {}", size.guideline());
    if ultracode {
        out.push_str("\n- Ultracode is on: the user opted into multi-agent orchestration for every substantive task. Optimize for the most exhaustive, correct answer, not the fastest or cheapest: use the workflow tool for every substantive task, often several workflows in sequence (understand → design → implement → review), with adversarial verification of what they find. Token cost is not a constraint. Work alone only on conversational turns and trivial edits.");
    }
    out
}

/// Agents of one run that work at the same time: two fewer than the CPUs,
/// between 1 and 16 (each agent is a local process).
pub fn max_concurrency() -> usize {
    let cpus = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4);
    cpus.saturating_sub(2).clamp(1, 16)
}

/// The turn's token target, from a `+500k`-style directive in the user's
/// latest message, and what the main agent and its agents have spent
/// since. Spending counts generated tokens plus input the prompt cache did
/// not cover ([`CacheUsage::billed`]). Workflows refuse to start new
/// agents once the target is reached.
#[derive(Default)]
pub struct TokenBudget {
    total: Mutex<Option<u64>>,
    spent: AtomicU64,
}

impl TokenBudget {
    pub fn total(&self) -> Option<u64> {
        *self.total.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn spent(&self) -> u64 {
        self.spent.load(Ordering::SeqCst)
    }

    pub fn add(&self, tokens: u64) {
        self.spent.fetch_add(tokens, Ordering::SeqCst);
    }

    /// A message the user wrote: a new turn, with the target it names (or
    /// none) and nothing spent yet.
    pub fn user_turn(&self, text: &str) {
        if crate::notice::is_harness(text) {
            return;
        }
        *self.total.lock().unwrap_or_else(|e| e.into_inner()) = parse_directive(text);
        self.spent.store(0, Ordering::SeqCst);
    }
}

/// `+500k`, `+2m`, `+1.5M` (a standalone word) → tokens.
pub fn parse_directive(text: &str) -> Option<u64> {
    let re = regex::Regex::new(r"(?:^|\s)\+(\d+(?:\.\d+)?)\s?([kKmM])(?:$|[\s.,;:!?)])").ok()?;
    let c = re.captures(text)?;
    let n: f64 = c.get(1)?.as_str().parse().ok()?;
    let unit = match c.get(2)?.as_str() {
        "k" | "K" => 1_000.0,
        _ => 1_000_000.0,
    };
    Some((n * unit) as u64)
}

/// Counts every model turn that passes through toward the token budget:
/// the main agent's, its children's and workflow agents' (all of them are
/// emitted or relayed through the root's sink).
pub struct BudgetSink {
    pub inner: Arc<dyn EventSink>,
    pub budget: Arc<TokenBudget>,
}

impl EventSink for BudgetSink {
    fn emit(&self, event: &AgentEvent) {
        if let AgentEvent::ModelFinished {
            input_tokens,
            cached_tokens,
            output_tokens,
            ..
        } = event
        {
            self.budget.add(
                CacheUsage {
                    input_tokens: *input_tokens,
                    cached_tokens: *cached_tokens,
                    output_tokens: *output_tokens,
                }
                .billed(),
            );
        }
        self.inner.emit(event);
    }
}

/// What a hub needs from the kernel that owns it.
pub struct HubConfig {
    pub workspace: PathBuf,
    pub worker_bin: PathBuf,
    pub root_name: String,
    /// The root run (the session id in the TUI).
    pub root_run: String,
    /// Agents' model when neither the call nor the session's child model
    /// names one.
    pub model: String,
    pub effort: ReasoningEffort,
    pub child_mode: String,
    pub search: bool,
    pub knobs: Option<Arc<Mutex<SessionKnobs>>>,
}

#[derive(Debug, Clone, PartialEq)]
enum Status {
    Running,
    Completed,
    Failed(String),
    Stopped,
}

impl Status {
    fn label(&self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Completed => "completed",
            Self::Failed(_) => "failed",
            Self::Stopped => "stopped",
        }
    }
}

struct Run {
    id: String,
    name: String,
    dir: PathBuf,
    /// The script, relative to the workspace.
    script_rel: String,
    stop: watch::Sender<bool>,
    /// Stopped by a `stop_workflow` call: the caller knows, no notice.
    quiet: AtomicBool,
    slots: Arc<Semaphore>,
    started: AtomicUsize,
    done: AtomicUsize,
    failed: AtomicUsize,
    cached: AtomicUsize,
    tokens: AtomicU64,
    /// Calls seen per cache key, to pick the n-th recorded result.
    seen: Mutex<HashMap<String, usize>>,
    segs: Mutex<HashSet<String>>,
    cache: journal::Cache,
    journal: journal::Journal,
    status: Mutex<Status>,
    t0: Instant,
    /// Told agentType is not supported here (once per run).
    warned_agent_type: AtomicBool,
}

impl Run {
    fn stopped(&self) -> bool {
        *self.stop.borrow()
    }

    fn status(&self) -> Status {
        self.status.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// A unique tree segment for an agent: its label made path-safe, plus
    /// its number in the run.
    fn seg_for(&self, label: Option<&str>, n: usize) -> String {
        let mut base: String = label
            .unwrap_or("")
            .chars()
            .map(|c| if c.is_alphanumeric() || c == '-' || c == '_' { c } else { '-' })
            .collect();
        while base.contains("--") {
            base = base.replace("--", "-");
        }
        let base: String = base.trim_matches('-').chars().take(30).collect();
        let base = if base.is_empty() { "agent".to_string() } else { base };
        let mut segs = self.segs.lock().unwrap_or_else(|e| e.into_inner());
        let mut seg = format!("{base}-{n}");
        let mut k = 1;
        while !segs.insert(seg.clone()) {
            seg = format!("{base}-{n}-{k}");
            k += 1;
        }
        seg
    }
}

/// A run stopped because the conversation's run ended (saved for the next).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClosedRun {
    pub id: String,
    pub name: String,
    pub script: String,
    pub agents_done: usize,
}

pub fn format_closed_notice(runs: &[ClosedRun]) -> String {
    let mut out = format!(
        "{CLOSED_NOTICE_PREFIX} These workflows were still running when this conversation last closed, and were stopped:\n"
    );
    for r in runs {
        out.push_str(&format!(
            "- {} ({}): {} agent call(s) had finished. Resume with workflow {{\"script_path\": \"{}\", \"resume_from_run_id\": \"{}\"}}; finished calls replay from its journal.\n",
            r.id, r.name, r.agents_done, r.script, r.id
        ));
    }
    out.push_str("Only resume one if the user still wants it. This is a system notice, not a user message.");
    out
}

#[derive(Clone)]
pub struct WorkflowHub {
    inner: Arc<Inner>,
}

struct Inner {
    cfg: HubConfig,
    env: Arc<PoolEnv>,
    notify: Option<mpsc::UnboundedSender<UserTurn>>,
    budget: Arc<TokenBudget>,
    runs: Mutex<Vec<Arc<Run>>>,
    shutting_down: AtomicBool,
}

impl WorkflowHub {
    pub fn new(
        cfg: HubConfig,
        sink: Arc<dyn EventSink>,
        notify: Option<mpsc::UnboundedSender<UserTurn>>,
        budget: Arc<TokenBudget>,
    ) -> Result<Self> {
        let env = Arc::new(PoolEnv {
            worker_bin: cfg.worker_bin.clone(),
            workspace: cfg.workspace.clone(),
            mode: cfg.child_mode.clone(),
            root_name: cfg.root_name.clone(),
            root_run: cfg.root_run.clone(),
            sink,
            client: crate::a2a::A2aClient::new()?,
        });
        Ok(Self {
            inner: Arc::new(Inner {
                cfg,
                env,
                notify,
                budget,
                runs: Mutex::new(Vec::new()),
                shutting_down: AtomicBool::new(false),
            }),
        })
    }

    pub fn budget(&self) -> Arc<TokenBudget> {
        self.inner.budget.clone()
    }

    /// Runs still going.
    pub fn running(&self) -> usize {
        self.runs().iter().filter(|r| r.status() == Status::Running).count()
    }

    fn runs(&self) -> Vec<Arc<Run>> {
        self.inner.runs.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    fn root_meta(&self, path: &str) -> EventMeta {
        let parent = (!path.is_empty()).then(|| self.inner.cfg.root_run.clone());
        EventMeta {
            ts: chrono::Utc::now(),
            agent_name: self.inner.cfg.root_name.clone(),
            run_id: self.inner.cfg.root_run.clone(),
            parent_run_id: parent,
            path: path.to_string(),
        }
    }

    /// Start a run (the `workflow` tool). Returns at once.
    pub fn start(&self, args: &Value) -> Result<String> {
        if self.inner.shutting_down.load(Ordering::SeqCst) {
            return Err(Error::Tool("this conversation is closing; no new workflows".into()));
        }
        let ws = &self.inner.cfg.workspace;
        let (source, _from) = self.source(args)?;
        let script = engine::parse(&source).map_err(Error::Tool)?;
        let resumed = match args.get("resume_from_run_id").and_then(Value::as_str).map(str::trim) {
            Some(id) if !id.is_empty() => {
                let dir = self.run_dir(id)?;
                let path = dir.join(journal::FILE);
                if !path.is_file() {
                    return Err(Error::Tool(format!("no journal for workflow run {id} (looked in {})", rel(ws, &path))));
                }
                Some((id.to_string(), journal::Cache::load(&path)))
            }
            _ => None,
        };
        let script_args = args.get("args").cloned().unwrap_or(Value::Null);
        let id = format!("wf-{}", &uuid::Uuid::new_v4().simple().to_string()[..8]);
        let dir = crate::vision::groka_dir(ws, RUNS_DIR)?.join(&id);
        std::fs::create_dir_all(&dir)?;
        let script_path = dir.join("script.js");
        std::fs::write(&script_path, &source)?;
        let journal = journal::Journal::create(&dir);
        journal.append(&json!({
            "type": "start",
            "run": id,
            "name": script.meta.name,
            "args": script_args,
            "resumed_from": resumed.as_ref().map(|(id, _)| id.clone()),
            "ts": chrono::Utc::now().to_rfc3339(),
        }));
        let (cache_hits, cache) = match resumed {
            Some((from, cache)) => (Some((from, cache.len())), cache),
            None => (None, journal::Cache::default()),
        };
        let (stop, _) = watch::channel(false);
        let run = Arc::new(Run {
            id: id.clone(),
            name: script.meta.name.clone(),
            dir: dir.clone(),
            script_rel: rel(ws, &script_path),
            stop,
            quiet: AtomicBool::new(false),
            slots: Arc::new(Semaphore::new(max_concurrency())),
            started: AtomicUsize::new(0),
            done: AtomicUsize::new(0),
            failed: AtomicUsize::new(0),
            cached: AtomicUsize::new(0),
            tokens: AtomicU64::new(0),
            seen: Mutex::new(HashMap::new()),
            segs: Mutex::new(HashSet::new()),
            cache,
            journal,
            status: Mutex::new(Status::Running),
            t0: Instant::now(),
            warned_agent_type: AtomicBool::new(false),
        });
        {
            let mut runs = self.inner.runs.lock().unwrap_or_else(|e| e.into_inner());
            runs.push(run.clone());
            // Keep the list short: finished runs past the newest 20 go.
            while runs.len() > 20 {
                match runs.iter().position(|r| r.status() != Status::Running) {
                    Some(i) => {
                        runs.remove(i);
                    }
                    None => break,
                }
            }
        }
        let sink = &self.inner.env.sink;
        sink.emit(&AgentEvent::ChildSpawned {
            meta: self.root_meta(""),
            name: id.clone(),
            agent_card_url: String::new(),
            prompt: format!("{} — {}", script.meta.name, script.meta.description),
            model: "workflow".into(),
        });
        sink.emit(&AgentEvent::RunStarted {
            meta: self.root_meta(&id),
            model: "workflow".into(),
        });
        let mut opening = format!(
            "工作流程 {}：{}（腳本 {}，最多 {} 個代理同時執行）",
            script.meta.name,
            script.meta.description,
            run.script_rel,
            max_concurrency()
        );
        if let Some((from, n)) = &cache_hits {
            opening.push_str(&format!("；從 {from} 續跑，{n} 個已完成的呼叫可重用"));
        }
        pool::note(&self.inner.env, &id, opening);
        let host = Arc::new(RunHost {
            hub: self.clone(),
            run: run.clone(),
            rt: tokio::runtime::Handle::current(),
        });
        let me = self.clone();
        let driver_run = run.clone();
        tokio::spawn(async move {
            let out = tokio::task::spawn_blocking(move || engine::run(&script, script_args, host)).await;
            let out = out.unwrap_or_else(|_| Err("the script engine crashed".into()));
            me.finish(&driver_run, out);
        });
        let mut reply = json!({
            "run_id": id,
            "name": run.name,
            "status": "running",
            "script_path": run.script_rel,
            "journal_path": rel(ws, run.journal.path()),
            "max_concurrent_agents": max_concurrency(),
            "note": "The workflow runs in the background. Keep working or end your turn: a [workflow … finished] notice with its result arrives when it ends. Do not poll. To iterate, edit script_path and call workflow again with script_path and resume_from_run_id = this run_id.",
        });
        if let Some((from, n)) = cache_hits {
            reply["resumed_from"] = json!(from);
            reply["cached_calls"] = json!(n);
        }
        Ok(reply.to_string())
    }

    /// The script text the call names: inline, a file, or a saved name.
    fn source(&self, args: &Value) -> Result<(String, String)> {
        let pick = |k: &str| args.get(k).and_then(Value::as_str).map(str::trim).filter(|s| !s.is_empty());
        let given = [pick("script").is_some(), pick("script_path").is_some(), pick("name").is_some()];
        if given.iter().filter(|g| **g).count() != 1 {
            return Err(Error::Tool(
                "pass exactly one of script, script_path or name (call workflow with no arguments to list saved workflows)".into(),
            ));
        }
        if let Some(s) = args.get("script").and_then(Value::as_str).filter(|s| !s.trim().is_empty()) {
            return Ok((s.to_string(), "inline".into()));
        }
        if let Some(p) = pick("script_path") {
            let path = self.workspace_file(p)?;
            let text = std::fs::read_to_string(&path)
                .map_err(|e| Error::Tool(format!("reading {p}: {e}")))?;
            return Ok((text, p.to_string()));
        }
        let name = pick("name").unwrap_or_default();
        let path = self.saved(name)?;
        let text = std::fs::read_to_string(&path).map_err(|e| Error::Tool(format!("reading {}: {e}", path.display())))?;
        Ok((text, name.to_string()))
    }

    /// A script path the model gave: inside the workspace (granted folders
    /// count), `.groka` included.
    fn workspace_file(&self, p: &str) -> Result<PathBuf> {
        crate::tools::resolve_in_workspace(&self.inner.cfg.workspace, p)
    }

    fn run_dir(&self, id: &str) -> Result<PathBuf> {
        let ok = id.starts_with("wf-") && id.len() <= 40 && id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-');
        if !ok {
            return Err(Error::Tool(format!("{id} is not a workflow run id (wf-…)")));
        }
        Ok(self.inner.cfg.workspace.join(".groka").join(RUNS_DIR).join(id))
    }

    /// The file of a saved workflow: the project's first, then the user's.
    fn saved(&self, name: &str) -> Result<PathBuf> {
        let ok = !name.is_empty()
            && name.len() <= 64
            && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
        if !ok {
            return Err(Error::Tool(format!("{name} is not a saved workflow name (letters, digits, - and _)")));
        }
        for dir in saved_dirs(&self.inner.cfg.workspace) {
            let path = dir.join(format!("{name}.js"));
            if path.is_file() {
                return Ok(path);
            }
        }
        let known: Vec<String> = list_saved(&self.inner.cfg.workspace).into_iter().map(|s| s.0).collect();
        Err(Error::Tool(if known.is_empty() {
            format!("no saved workflow named {name} (save one as .groka/{SAVED_DIR}/{name}.js)")
        } else {
            format!("no saved workflow named {name}; saved: {}", known.join(", "))
        }))
    }

    /// Saved workflows and this conversation's runs (the `workflow` tool
    /// called without a script).
    pub fn overview(&self) -> String {
        let saved: Vec<Value> = list_saved(&self.inner.cfg.workspace)
            .into_iter()
            .map(|(name, description, path)| json!({"name": name, "description": description, "path": path}))
            .collect();
        let runs: Vec<Value> = self
            .runs()
            .iter()
            .map(|r| {
                let mut v = json!({
                    "run_id": r.id,
                    "name": r.name,
                    "status": r.status().label(),
                    "agents_started": r.started.load(Ordering::SeqCst),
                    "agents_failed": r.failed.load(Ordering::SeqCst),
                    "agents_cached": r.cached.load(Ordering::SeqCst),
                    "tokens": r.tokens.load(Ordering::SeqCst),
                    "script_path": r.script_rel,
                });
                if let Status::Failed(e) = r.status() {
                    v["error"] = json!(e);
                }
                v
            })
            .collect();
        json!({
            "saved_workflows": saved,
            "runs": runs,
            "max_concurrent_agents": max_concurrency(),
            "hint": "Run one with workflow {\"name\": …}, or write a script (workflow {\"reference\": true} returns the authoring guide).",
        })
        .to_string()
    }

    /// Stop a run (the `stop_workflow` tool).
    pub fn stop(&self, id: &str) -> Result<String> {
        let run = self
            .runs()
            .into_iter()
            .find(|r| r.id == id)
            .ok_or_else(|| Error::Tool(format!("no workflow run {id} in this conversation")))?;
        if run.status() != Status::Running {
            return Ok(json!({"run_id": id, "status": run.status().label(), "note": "already ended"}).to_string());
        }
        run.quiet.store(true, Ordering::SeqCst);
        let _ = run.stop.send(true);
        Ok(json!({
            "run_id": id,
            "status": "stopping",
            "agents_finished": run.done.load(Ordering::SeqCst) + run.cached.load(Ordering::SeqCst),
            "note": format!("Its agents are being killed. Resume later with script_path {} and resume_from_run_id {id}.", run.script_rel),
        })
        .to_string())
    }

    /// The script ended (or was stopped): record it, close its tree node and
    /// tell the main agent.
    fn finish(&self, run: &Arc<Run>, out: std::result::Result<Value, String>) {
        let status = match &out {
            Ok(_) => Status::Completed,
            Err(e) if e == "stopped" => Status::Stopped,
            Err(e) => Status::Failed(e.clone()),
        };
        *run.status.lock().unwrap_or_else(|e| e.into_inner()) = status.clone();
        // Calls the script never awaited do not outlive it.
        let _ = run.stop.send(true);
        let ws = &self.inner.cfg.workspace;
        let result_path = run.dir.join("result.json");
        if let Ok(v) = &out {
            let _ = std::fs::write(&result_path, serde_json::to_vec_pretty(v).unwrap_or_default());
        }
        let secs = run.t0.elapsed().as_secs();
        let tokens = run.tokens.load(Ordering::SeqCst);
        let (done, failed, cached) = (
            run.done.load(Ordering::SeqCst),
            run.failed.load(Ordering::SeqCst),
            run.cached.load(Ordering::SeqCst),
        );
        run.journal.append(&json!({
            "type": "end",
            "status": status.label(),
            "error": match &status { Status::Failed(e) => json!(e), _ => Value::Null },
            "agents": {"done": done, "failed": failed, "cached": cached},
            "tokens": tokens,
            "seconds": secs,
        }));
        let summary = format!(
            "{} 個代理完成（{} 個失敗、{} 個來自快取），{} tokens，{}",
            done + failed + cached,
            failed,
            cached,
            tokens,
            duration(secs)
        );
        let (head, line) = match &status {
            Status::Completed => ("完成".to_string(), format!("工作流程完成：{summary}")),
            Status::Stopped => ("已停止".to_string(), format!("工作流程已停止：{summary}")),
            Status::Failed(e) => (format!("失敗：{}", e.lines().next().unwrap_or("")), format!("工作流程失敗：{e}\n{summary}")),
            Status::Running => unreachable!(),
        };
        pool::note(&self.inner.env, &run.id, line);
        self.inner.env.sink.emit(&AgentEvent::ChildExited {
            meta: self.root_meta(""),
            name: run.id.clone(),
            detail: head,
        });
        if run.quiet.load(Ordering::SeqCst) || self.inner.shutting_down.load(Ordering::SeqCst) {
            return;
        }
        let counts = format!(
            "{} agent call(s): {} succeeded, {} failed (null), {} answered from the journal; {} tokens; {}",
            done + failed + cached,
            done,
            failed,
            cached,
            tokens,
            duration(secs)
        );
        let text = match (&status, &out) {
            (Status::Completed, Ok(v)) => format!(
                "{NOTICE_PREFIX}{} finished] {} completed. {counts}.\nResult:\n{}\nFiles: result {} · journal {} · script {}\nThis is a system notice, not a user message.",
                run.id,
                run.name,
                cut(&serde_json::to_string_pretty(v).unwrap_or_default(), RESULT_CAP),
                rel(ws, &result_path),
                rel(ws, run.journal.path()),
                run.script_rel
            ),
            (Status::Failed(e), _) => format!(
                "{NOTICE_PREFIX}{} failed] {} failed: {}\n{counts}.\nFix the script ({}) and run workflow again with script_path and resume_from_run_id \"{}\": finished calls replay from the journal.\nThis is a system notice, not a user message.",
                run.id,
                run.name,
                cut(e, 4000),
                run.script_rel,
                run.id
            ),
            _ => format!(
                "{NOTICE_PREFIX}{} stopped] {} was stopped. {counts}.\nThis is a system notice, not a user message.",
                run.id, run.name
            ),
        };
        if let Some(tx) = &self.inner.notify {
            let _ = tx.send(UserTurn::from(text));
        }
    }

    /// The kernel is ending: stop every run and wait (briefly) for their
    /// agents to die. Returns the runs that were stopped, for the next
    /// run of this conversation to hear about.
    pub async fn shutdown(&self) -> Vec<ClosedRun> {
        self.inner.shutting_down.store(true, Ordering::SeqCst);
        let live: Vec<Arc<Run>> = self.runs().into_iter().filter(|r| r.status() == Status::Running).collect();
        for r in &live {
            let _ = r.stop.send(true);
        }
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline && live.iter().any(|r| r.status() == Status::Running) {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        live.iter()
            .map(|r| ClosedRun {
                id: r.id.clone(),
                name: r.name.clone(),
                script: r.script_rel.clone(),
                agents_done: r.done.load(Ordering::SeqCst) + r.cached.load(Ordering::SeqCst),
            })
            .collect()
    }

    /// The live search toggle (the session's, when it has knobs).
    fn search(&self) -> bool {
        match &self.inner.cfg.knobs {
            Some(k) => k.lock().unwrap_or_else(|e| e.into_inner()).search,
            None => self.inner.cfg.search,
        }
    }

    /// The model and effort for an agent: the call's, else the session's
    /// child model, else the hub's.
    fn agent_model(&self, req: &Value) -> std::result::Result<(String, ReasoningEffort), String> {
        let (child, effort) = match &self.inner.cfg.knobs {
            Some(k) => {
                let g = k.lock().unwrap_or_else(|e| e.into_inner());
                (g.child_model.trim().to_string(), g.reasoning_effort)
            }
            None => (String::new(), self.inner.cfg.effort),
        };
        let model = req
            .get("model")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .or_else(|| (!child.is_empty()).then_some(child))
            .unwrap_or_else(|| self.inner.cfg.model.clone());
        let effort = match req.get("effort").and_then(Value::as_str) {
            Some(e) => ReasoningEffort::parse(e).ok_or_else(|| format!("agent(): unknown effort `{e}` (low|medium|high|xhigh)"))?,
            None => effort,
        };
        Ok((model, effort))
    }
}

/// `.groka/workflows` in the workspace, then `~/.grokaagent/workflows`.
fn saved_dirs(workspace: &Path) -> Vec<PathBuf> {
    let mut v = vec![workspace.join(".groka").join(SAVED_DIR)];
    if let Ok(home) = crate::config::groka_dir() {
        v.push(home.join(SAVED_DIR));
    }
    v
}

/// `(name, description, path)` of every saved workflow; a project one
/// hides a user one of the same name.
fn list_saved(workspace: &Path) -> Vec<(String, String, String)> {
    let mut out: Vec<(String, String, String)> = Vec::new();
    for dir in saved_dirs(workspace) {
        let Ok(entries) = std::fs::read_dir(&dir) else { continue };
        let mut files: Vec<PathBuf> = entries.flatten().map(|e| e.path()).filter(|p| p.extension().is_some_and(|x| x == "js")).collect();
        files.sort();
        for path in files {
            let Some(name) = path.file_stem().map(|s| s.to_string_lossy().to_string()) else { continue };
            if out.iter().any(|(n, _, _)| n == &name) {
                continue;
            }
            let description = std::fs::read_to_string(&path)
                .ok()
                .and_then(|s| engine::parse(&s).ok())
                .map(|s| s.meta.description)
                .unwrap_or_else(|| "(does not parse)".into());
            out.push((name, description, path.display().to_string()));
        }
    }
    out
}

fn rel(workspace: &Path, path: &Path) -> String {
    path.strip_prefix(workspace)
        .map(|p| p.to_string_lossy().replace('\\', "/"))
        .unwrap_or_else(|_| path.display().to_string())
}

fn cut(text: &str, cap: usize) -> String {
    if text.len() <= cap {
        return text.to_string();
    }
    let mut end = cap;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}\n…(cut {} bytes; the whole of it is in the file below)", &text[..end], text.len() - end)
}

fn duration(secs: u64) -> String {
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3600 {
        format!("{}m{:02}s", secs / 60, secs % 60)
    } else {
        format!("{}h{:02}m", secs / 3600, (secs % 3600) / 60)
    }
}

/// The host side of one run's script.
struct RunHost {
    hub: WorkflowHub,
    run: Arc<Run>,
    rt: tokio::runtime::Handle,
}

impl RunHost {
    fn group_prefix(group: &str) -> String {
        if group.is_empty() {
            String::new()
        } else {
            format!("▸ {group}  ")
        }
    }
}

impl engine::Host for RunHost {
    fn agent(&self, req: Value) -> ReplyFut {
        let hub = self.hub.clone();
        let run = self.run.clone();
        let rt = self.rt.clone();
        Box::pin(async move {
            if run.stopped() {
                return Reply::Throw("workflow stopped".into());
            }
            let schema = req.get("schema").cloned();
            if let Some(s) = &schema {
                if let Err(e) = crate::jschema::check_result_schema(s) {
                    return Reply::Throw(format!("agent(): {e}"));
                }
            }
            let worktree = match req.get("isolation").and_then(Value::as_str) {
                None => false,
                Some("worktree") => true,
                Some(other) => return Reply::Throw(format!("agent(): isolation `{other}` is not supported (only 'worktree')")),
            };
            let (model, effort) = match hub.agent_model(&req) {
                Ok(m) => m,
                Err(e) => return Reply::Throw(e),
            };
            if req.get("agentType").is_some() && !run.warned_agent_type.swap(true, Ordering::SeqCst) {
                pool::note(&hub.inner.env, &run.id, "agentType 在這裡不支援，已忽略（每個代理都有完整的工具）".into());
            }
            let label = req.get("label").and_then(Value::as_str).map(str::to_string);
            let phase = req.get("phase").and_then(Value::as_str).map(str::to_string);
            let key = journal::call_key(&req);
            let n = {
                let mut seen = run.seen.lock().unwrap_or_else(|e| e.into_inner());
                let c = seen.entry(key.clone()).or_insert(0);
                *c += 1;
                *c - 1
            };
            let shown = label.clone().unwrap_or_else(|| first_words(req.get("prompt").and_then(Value::as_str).unwrap_or("")));
            if let Some(v) = run.cache.get(&key, n) {
                run.cached.fetch_add(1, Ordering::SeqCst);
                run.journal.append(&json!({
                    "type": "agent", "key": key, "n": n, "label": label, "phase": phase,
                    "status": "cached", "value": v,
                }));
                pool::note(&hub.inner.env, &run.id, format!("快取  {shown}"));
                return Reply::Value(v.clone());
            }
            let number = run.started.fetch_add(1, Ordering::SeqCst) + 1;
            if number > MAX_AGENTS {
                return Reply::Throw(format!("this workflow already started its maximum of {MAX_AGENTS} agents"));
            }
            let permit = {
                let mut stop = run.stop.subscribe();
                tokio::select! {
                    p = run.slots.clone().acquire_owned() => p.ok(),
                    _ = stop.wait_for(|s| *s) => None,
                }
            };
            let Some(permit) = permit else {
                return Reply::Throw("workflow stopped".into());
            };
            if let Some(total) = hub.inner.budget.total() {
                let spent = hub.inner.budget.spent();
                if spent >= total {
                    return Reply::Throw(format!("the token budget of {total} is spent ({spent}); no new agents start"));
                }
            }
            let job = AgentJob {
                node: run.id.clone(),
                dir: run.dir.clone(),
                seg: run.seg_for(label.as_deref(), number),
                prompt: req.get("prompt").and_then(Value::as_str).unwrap_or("").to_string(),
                schema,
                model,
                effort,
                worktree,
                search: hub.search(),
            };
            let seg = job.seg.clone();
            let env = hub.inner.env.clone();
            let stop = run.stop.subscribe();
            let report = rt
                .spawn(async move {
                    let _permit = permit;
                    pool::run_agent(&env, job, stop).await
                })
                .await;
            let report = match report {
                Ok(r) => r,
                Err(_) => pool::AgentReport {
                    end: AgentEnd::Failed("the agent task crashed".into()),
                    tokens: 0,
                    worktree: None,
                },
            };
            run.tokens.fetch_add(report.tokens, Ordering::SeqCst);
            let (status, value, reason) = match &report.end {
                AgentEnd::Ok(v) => ("ok", v.clone(), Value::Null),
                AgentEnd::Failed(why) => ("null", Value::Null, json!(why)),
                AgentEnd::Stopped => ("stopped", Value::Null, Value::Null),
            };
            run.journal.append(&json!({
                "type": "agent", "key": key, "n": n, "label": label, "phase": phase, "node": seg,
                "status": status, "value": value, "reason": reason, "tokens": report.tokens,
                "worktree": report.worktree,
            }));
            match report.end {
                AgentEnd::Ok(v) => {
                    run.done.fetch_add(1, Ordering::SeqCst);
                    Reply::Value(v)
                }
                AgentEnd::Failed(why) => {
                    run.failed.fetch_add(1, Ordering::SeqCst);
                    pool::note(&hub.inner.env, &run.id, format!("{seg} 失敗，結果為 null：{}", why.lines().next().unwrap_or("")));
                    Reply::Null(why)
                }
                AgentEnd::Stopped => Reply::Throw("workflow stopped".into()),
            }
        })
    }

    fn phase(&self, title: &str, group: &str) {
        pool::note(&self.hub.inner.env, &self.run.id, format!("{}階段：{title}", Self::group_prefix(group)));
    }

    fn log(&self, message: &str, group: &str) {
        pool::note(&self.hub.inner.env, &self.run.id, format!("{}{message}", Self::group_prefix(group)));
    }

    fn budget(&self) -> (Option<u64>, u64) {
        let b = &self.hub.inner.budget;
        (b.total(), b.spent())
    }

    fn load(&self, reference: &Value) -> std::result::Result<Script, String> {
        let source = match reference {
            Value::String(name) => {
                let path = self.hub.saved(name).map_err(|e| e.to_string())?;
                std::fs::read_to_string(&path).map_err(|e| format!("reading {}: {e}", path.display()))?
            }
            Value::Object(o) => {
                let p = o.get("scriptPath").and_then(Value::as_str).unwrap_or("");
                let path = self.hub.workspace_file(p).map_err(|e| e.to_string())?;
                std::fs::read_to_string(&path).map_err(|e| format!("reading {p}: {e}"))?
            }
            _ => return Err("workflow(): pass a saved workflow's name or {scriptPath}".into()),
        };
        engine::parse(&source)
    }

    fn stopped(&self) -> bool {
        self.run.stopped()
    }
}

fn first_words(prompt: &str) -> String {
    let line = prompt.lines().next().unwrap_or("").trim();
    if line.chars().count() > 50 {
        format!("{}…", line.chars().take(50).collect::<String>())
    } else {
        line.to_string()
    }
}

/// `workflow`: start a run, list saved workflows and runs, or return the
/// authoring guide.
pub struct WorkflowTool {
    hub: WorkflowHub,
}

impl WorkflowTool {
    pub fn new(hub: WorkflowHub) -> Self {
        Self { hub }
    }
}

impl ClientTool for WorkflowTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "workflow".into(),
            description: format!(
                "Run a workflow: a JavaScript script that orchestrates many subagents deterministically (fan-out, pipelines, adversarial verification, loops until nothing new turns up). It runs in the background: this call returns at once with a run_id, and a [workflow … finished] notice with the script's return value arrives when it ends. Do not poll.\n\
ONLY start one when the user has opted into multi-agent orchestration: their message contains the keyword \"ultracode\", a system note says ultracode is on, they ask in their own words for a workflow / multi-agent orchestration / fan-out, or they name a saved workflow. Otherwise do the work yourself (or with spawn_agent), or briefly describe what a workflow would do and roughly what it would cost, and ask.\n\
Before writing your first script, call workflow {{\"reference\": true}} for the authoring guide (API, gotchas, patterns).\n\
Script: begins with `export const meta = {{ name, description, phases: [{{ title, detail? }}] }}` (a pure literal); the body runs as an async function (top-level await and return work); plain JavaScript, no TypeScript or imports. Globals: agent(prompt, {{label, phase, schema, model, effort, isolation: 'worktree'}}) → the agent's final text, or the schema-validated object, or null if it failed; parallel(thunks) (a barrier; a thunk that throws becomes null); pipeline(items, stage1, stage2, …) (each item runs through the stages on its own, no barrier; stage(prev, item, index)); phase(title); log(message); args; budget {{total, spent(), remaining()}}; workflow(name | {{scriptPath}}, args) (one level of nesting). Date.now(), new Date() and Math.random() throw. Agents start with no context but their prompt, share the workspace and have the full tool set: give parallel writers separate files or isolation: 'worktree'. At most {} agents run at once, {MAX_AGENTS} per run, {} items per parallel/pipeline call.\n\
Pass exactly one of script, script_path (a .js file in the workspace) or name (a saved workflow: .groka/workflows/<name>.js or ~/.grokaagent/workflows/<name>.js). Each run saves its script to .groka/workflow-runs/<run_id>/script.js: to iterate, edit that file and call workflow with script_path and resume_from_run_id; agent() calls whose prompt and options did not change return their recorded results. With no arguments, lists saved workflows and this conversation's runs.",
                max_concurrency(),
                engine::MAX_FAN_OUT,
            ),
            parameters: json!({
                "type": "object",
                "properties": {
                    "script": {"type": "string", "description": "The whole script, starting with export const meta = {...}."},
                    "script_path": {"type": "string", "description": "A script file in the workspace, e.g. a previous run's .groka/workflow-runs/<run_id>/script.js after editing it."},
                    "name": {"type": "string", "description": "A saved workflow's name."},
                    "args": {"description": "Any JSON value; the script reads it as `args`."},
                    "resume_from_run_id": {"type": "string", "description": "An earlier run (wf-…) whose finished agent() calls are reused when unchanged."},
                    "reference": {"type": "boolean", "description": "true: return the authoring guide instead of running anything."}
                },
                "additionalProperties": false
            }),
        }
    }

    fn call(&self, args: &Value) -> ToolCallFut<'_> {
        let out = if args.get("reference").and_then(Value::as_bool) == Some(true) {
            Ok(REFERENCE.replace("{MAX_CONCURRENCY}", &max_concurrency().to_string()))
        } else if ["script", "script_path", "name"]
            .iter()
            .all(|k| args.get(*k).and_then(Value::as_str).is_none_or(|s| s.trim().is_empty()))
        {
            Ok(self.hub.overview())
        } else {
            self.hub.start(args)
        };
        Box::pin(async move { out })
    }
}

/// `stop_workflow`: stop a run and kill its agents.
pub struct StopWorkflowTool {
    hub: WorkflowHub,
}

impl StopWorkflowTool {
    pub fn new(hub: WorkflowHub) -> Self {
        Self { hub }
    }
}

impl ClientTool for StopWorkflowTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "stop_workflow".into(),
            description: "Stop a running workflow: its script is interrupted and its agents are killed. Its journal keeps every finished agent() call, so it can be resumed later (workflow with script_path and resume_from_run_id).".into(),
            parameters: json!({
                "type": "object",
                "properties": {"run_id": {"type": "string", "description": "The run (wf-…)."}},
                "required": ["run_id"],
                "additionalProperties": false
            }),
        }
    }

    fn call(&self, args: &Value) -> ToolCallFut<'_> {
        let out = match args.get("run_id").and_then(Value::as_str).map(str::trim) {
            Some(id) if !id.is_empty() => self.hub.stop(id),
            _ => Err(Error::Tool("stop_workflow requires run_id".into())),
        };
        Box::pin(async move { out })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn directives_name_a_token_target() {
        assert_eq!(parse_directive("do it +500k"), Some(500_000));
        assert_eq!(parse_directive("+2m please"), Some(2_000_000));
        assert_eq!(parse_directive("budget +1.5M."), Some(1_500_000));
        assert_eq!(parse_directive("x+500k"), None);
        assert_eq!(parse_directive("c++ 5k"), None);
        assert_eq!(parse_directive("+500kb"), None);
        assert_eq!(parse_directive("no target"), None);
    }

    #[test]
    fn a_user_turn_resets_the_budget_but_a_notice_does_not() {
        let b = TokenBudget::default();
        b.user_turn("go +100k");
        b.add(40);
        assert_eq!((b.total(), b.spent()), (Some(100_000), 40));
        b.user_turn(&format!("{NOTICE_PREFIX}wf-1 finished] x"));
        assert_eq!((b.total(), b.spent()), (Some(100_000), 40));
        b.user_turn("next");
        assert_eq!((b.total(), b.spent()), (None, 0));
    }

    #[test]
    fn concurrency_is_cpus_minus_two_within_1_to_16() {
        let n = max_concurrency();
        assert!((1..=16).contains(&n));
    }
}
