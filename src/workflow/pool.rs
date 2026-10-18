//! Runs one workflow `agent()` call: a worker process of this binary (the
//! same kernel and tools as any child agent), given one task over A2A and
//! killed once that task ends. Its events are relayed under the workflow's
//! node in the agent tree (`wf-…/label`), and its token use is summed.
//!
//! The worker starts at the deepest level, so it cannot spawn agents of its
//! own or start a workflow: a workflow's fan-out is what its script says.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::Value;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;
use tokio::sync::{oneshot, watch};

use crate::a2a::{self, A2aClient, Handshake};
use crate::events::{AgentEvent, EventMeta, EventSink};
use crate::procgroup::ProcessGuard;
use crate::provider::{CacheUsage, ReasoningEffort};

/// Turns one agent may take on its task (or on a follow-up) before it is
/// paused, which ends the call as a failure.
pub const AGENT_MAX_TURNS: u32 = 150;
/// Follow-ups asking an agent that stopped without a valid result to hand
/// one in.
const SCHEMA_NUDGES: u32 = 2;
const POLL: Duration = Duration::from_millis(200);
const TAIL_TICK: Duration = Duration::from_millis(60);

/// What every agent of a hub shares.
pub struct PoolEnv {
    pub worker_bin: PathBuf,
    pub workspace: PathBuf,
    /// `grok` or `echo` (tests).
    pub mode: String,
    /// The root agent's name and run: workflow events are filed under it.
    pub root_name: String,
    pub root_run: String,
    pub sink: Arc<dyn EventSink>,
    pub client: A2aClient,
}

/// One `agent()` call, resolved.
pub struct AgentJob {
    /// The workflow's node in the agent tree (`wf-…`).
    pub node: String,
    /// The run's directory (`.groka/workflow-runs/wf-…`).
    pub dir: PathBuf,
    /// This agent's segment under the node: unique within the run.
    pub seg: String,
    pub prompt: String,
    pub schema: Option<Value>,
    pub model: String,
    pub effort: ReasoningEffort,
    pub worktree: bool,
    /// The session's search toggle when the agent starts.
    pub search: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub enum AgentEnd {
    Ok(Value),
    Failed(String),
    Stopped,
}

pub struct AgentReport {
    pub end: AgentEnd,
    /// Output plus uncached input tokens the agent spent.
    pub tokens: u64,
    /// A kept worktree (the agent changed something in it).
    pub worktree: Option<String>,
}

fn meta(env: &PoolEnv, path: &str) -> EventMeta {
    EventMeta {
        ts: chrono::Utc::now(),
        agent_name: env.root_name.clone(),
        run_id: env.root_run.clone(),
        parent_run_id: Some(env.root_run.clone()),
        path: path.to_string(),
    }
}

/// A line for the workflow node's own log (its tab in the agent tree).
pub fn note(env: &PoolEnv, node: &str, message: String) {
    env.sink.emit(&AgentEvent::Notice {
        meta: meta(env, node),
        message,
    });
}

/// Run `job` to its end. `stop` turning true kills the agent.
pub async fn run_agent(env: &PoolEnv, job: AgentJob, mut stop: watch::Receiver<bool>) -> AgentReport {
    let spent = Arc::new(AtomicU64::new(0));
    let mut kept = None;
    let (workspace, worktree) = if job.worktree {
        match add_worktree(&env.workspace, &job.node, &job.seg).await {
            Ok(wt) => (wt.clone(), Some(wt)),
            Err(e) => {
                return AgentReport {
                    end: AgentEnd::Failed(format!("isolation: worktree: {e}")),
                    tokens: 0,
                    worktree: None,
                }
            }
        }
    } else {
        (env.workspace.clone(), None)
    };
    let end = run_in(env, &job, &workspace, spent.clone(), &mut stop).await;
    let detail = match &end {
        AgentEnd::Ok(_) => "完成".to_string(),
        AgentEnd::Failed(why) => format!("失敗：{}", first_line(why)),
        AgentEnd::Stopped => "已停止".to_string(),
    };
    env.sink.emit(&AgentEvent::ChildExited {
        meta: meta(env, &job.node),
        name: job.seg.clone(),
        detail,
    });
    if let Some(wt) = worktree {
        match finish_worktree(&env.workspace, &wt).await {
            Some(branch) => {
                note(env, &job.node, format!("{} 的 worktree 有變更，保留在 {} （分支 {branch}）", job.seg, wt.display()));
                kept = Some(format!("{} (branch {branch})", wt.display()));
            }
            None => {}
        }
    }
    AgentReport {
        end,
        tokens: spent.load(Ordering::SeqCst),
        worktree: kept,
    }
}

fn first_line(s: &str) -> String {
    let line = s.lines().next().unwrap_or("").trim();
    if line.chars().count() > 120 {
        format!("{}…", line.chars().take(120).collect::<String>())
    } else {
        line.to_string()
    }
}

async fn run_in(
    env: &PoolEnv,
    job: &AgentJob,
    workspace: &Path,
    spent: Arc<AtomicU64>,
    stop: &mut watch::Receiver<bool>,
) -> AgentEnd {
    let agents = job.dir.join("agents");
    if let Err(e) = std::fs::create_dir_all(&agents) {
        return AgentEnd::Failed(format!("agent log folder: {e}"));
    }
    let events = agents.join(format!("{}.jsonl", job.seg));
    let run_id = uuid::Uuid::new_v4().to_string();
    let mut cmd = Command::new(&env.worker_bin);
    cmd.arg("worker")
        .arg("--name")
        .arg(&job.seg)
        .arg("--depth")
        .arg(crate::nursery::DEFAULT_MAX_DEPTH.to_string())
        .arg("--parent-run")
        .arg(&env.root_run)
        .arg("--run-id")
        .arg(&run_id)
        .arg("--listen")
        .arg("127.0.0.1:0")
        .arg("--events")
        .arg(&events)
        .arg("--mode")
        .arg(&env.mode)
        .arg("--workspace")
        .arg(workspace)
        .arg("--model")
        .arg(&job.model)
        .arg("--max-turns")
        .arg(AGENT_MAX_TURNS.to_string())
        .arg("--reasoning")
        .arg(job.effort.as_str())
        .arg("--lifeline");
    if job.search {
        cmd.arg("--search");
    }
    if let Some(schema) = &job.schema {
        let path = agents.join(format!("{}.schema.json", job.seg));
        if let Err(e) = std::fs::write(&path, schema.to_string()) {
            return AgentEnd::Failed(format!("schema file: {e}"));
        }
        cmd.arg("--result-schema").arg(path);
    }
    let extra = crate::grants::env_value(&env.workspace);
    if !extra.is_empty() {
        cmd.env(crate::grants::ENV, extra);
    }
    cmd.stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    let mut guard = match ProcessGuard::spawn(cmd) {
        Ok(g) => g,
        Err(e) => return AgentEnd::Failed(format!("starting the agent: {e}")),
    };
    let origin = match handshake(&mut guard).await {
        Ok(o) => o,
        Err(e) => {
            let _ = guard.kill().await;
            return AgentEnd::Failed(e);
        }
    };
    let stderr = Arc::new(Mutex::new(VecDeque::new()));
    if let Some(err) = guard.child_mut().stderr.take() {
        let tail = stderr.clone();
        tokio::spawn(async move {
            let mut lines = BufReader::new(err).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                let mut t = tail.lock().unwrap_or_else(|e| e.into_inner());
                if t.len() >= 5 {
                    t.pop_front();
                }
                t.push_back(line);
            }
        });
    }
    env.sink.emit(&AgentEvent::ChildSpawned {
        meta: meta(env, &job.node),
        name: job.seg.clone(),
        agent_card_url: String::new(),
        prompt: job.prompt.clone(),
        model: job.model.clone(),
    });
    let (tail_stop, tail_rx) = oneshot::channel();
    let tail = tokio::spawn(relay(
        events,
        job.seg.clone(),
        job.node.clone(),
        env.root_run.clone(),
        env.sink.clone(),
        spent,
        tail_rx,
    ));

    let mut text = job.prompt.clone();
    if job.schema.is_some() {
        text.push_str(&super::submit::task_suffix());
    }
    let mut nudges = 0;
    let end = loop {
        let task = match env.client.send_text(&origin, &text, None).await {
            Ok(t) => t,
            Err(e) => break AgentEnd::Failed(format!("sending the task: {e}")),
        };
        let done = match wait_done(env, &origin, task, &mut guard, stop).await {
            Ok(Some(t)) => t,
            Ok(None) => break AgentEnd::Stopped,
            Err(e) => {
                let tail: Vec<String> = stderr.lock().unwrap_or_else(|e| e.into_inner()).iter().cloned().collect();
                break AgentEnd::Failed(if tail.is_empty() { e } else { format!("{e}: {}", tail.join(" / ")) });
            }
        };
        let state = a2a::task_state(&done).to_string();
        let reply = a2a::artifact_text(&done);
        if !state.ends_with("COMPLETED") {
            let why = done
                .get("status")
                .and_then(|s| s.get("message"))
                .and_then(Value::as_str)
                .map(str::to_string)
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| if reply.is_empty() { state.clone() } else { reply.clone() });
            break AgentEnd::Failed(why);
        }
        let Some(schema) = &job.schema else {
            break AgentEnd::Ok(Value::String(reply));
        };
        match structured(&reply, schema) {
            Ok(v) => break AgentEnd::Ok(v),
            Err(problem) if nudges < SCHEMA_NUDGES => {
                nudges += 1;
                text = format!(
                    "You stopped without handing in a valid result ({problem}). Call `{}` now: its arguments are your final result and must match its schema.",
                    super::submit::NAME
                );
            }
            Err(problem) => break AgentEnd::Failed(format!("no valid structured result: {problem}")),
        }
    };
    let _ = guard.kill().await;
    // Let the tailer read the last lines before it stops.
    tokio::time::sleep(TAIL_TICK * 2).await;
    let _ = tail_stop.send(());
    let _ = tail.await;
    end
}

/// The result for a schema call: the `submit_result` arguments the worker
/// finished with, or a JSON object in a plain reply that fits anyway.
fn structured(reply: &str, schema: &Value) -> Result<Value, String> {
    let parsed = serde_json::from_str::<Value>(reply.trim()).ok().or_else(|| {
        let (a, b) = (reply.find('{')?, reply.rfind('}')?);
        (a < b).then(|| serde_json::from_str::<Value>(&reply[a..=b]).ok()).flatten()
    });
    let Some(v) = parsed else {
        return Err("no JSON result".into());
    };
    let errors = crate::jschema::validate(&v, schema);
    if errors.is_empty() {
        Ok(v)
    } else {
        Err(errors.into_iter().take(3).collect::<Vec<_>>().join("; "))
    }
}

async fn handshake(guard: &mut ProcessGuard) -> Result<String, String> {
    let stdout = guard
        .child_mut()
        .stdout
        .take()
        .ok_or_else(|| "agent stdout missing".to_string())?;
    let mut lines = BufReader::new(stdout).lines();
    let line = tokio::time::timeout(Duration::from_secs(8), lines.next_line())
        .await
        .map_err(|_| "agent handshake timed out".to_string())?
        .map_err(|e| format!("agent handshake: {e}"))?
        .ok_or_else(|| "agent exited before its handshake".to_string())?;
    let origin = Handshake::parse_line(&line)
        .and_then(|h| h.origin())
        .map_err(|e| e.to_string())?;
    tokio::spawn(async move { while let Ok(Some(_)) = lines.next_line().await {} });
    Ok(origin)
}

/// Poll the task to a terminal state. `Ok(None)` = stopped.
async fn wait_done(
    env: &PoolEnv,
    origin: &str,
    task: Value,
    guard: &mut ProcessGuard,
    stop: &mut watch::Receiver<bool>,
) -> Result<Option<Value>, String> {
    let id = task
        .get("id")
        .and_then(Value::as_str)
        .ok_or_else(|| "task without an id".to_string())?
        .to_string();
    let mut task = task;
    loop {
        if a2a::is_terminal(a2a::task_state(&task)) {
            return Ok(Some(task));
        }
        if *stop.borrow() {
            return Ok(None);
        }
        tokio::select! {
            _ = stop.changed() => continue,
            _ = tokio::time::sleep(POLL) => {}
        }
        if let Ok(Some(status)) = guard.child_mut().try_wait() {
            return Err(format!("the agent process exited ({status})"));
        }
        match env.client.get_task(origin, &id).await {
            Ok(t) => task = t,
            Err(e) => return Err(format!("reading the task: {e}")),
        }
    }
}

/// Tail the worker's events: relay each under `node/seg` and sum what its
/// model turns cost.
async fn relay(
    events: PathBuf,
    seg: String,
    node: String,
    root_run: String,
    sink: Arc<dyn EventSink>,
    spent: Arc<AtomicU64>,
    mut stop: oneshot::Receiver<()>,
) {
    let mut offset = 0u64;
    let mut leftover = String::new();
    let mut each = |line: &str| {
        let Ok(ev) = serde_json::from_str::<AgentEvent>(line) else {
            return;
        };
        if let AgentEvent::ModelFinished {
            input_tokens,
            cached_tokens,
            output_tokens,
            ..
        } = &ev
        {
            let usage = CacheUsage {
                input_tokens: *input_tokens,
                cached_tokens: *cached_tokens,
                output_tokens: *output_tokens,
            };
            spent.fetch_add(usage.billed(), Ordering::SeqCst);
        }
        sink.emit(&ev.relayed(&seg, &root_run).relayed(&node, &root_run));
    };
    loop {
        let last = tokio::select! {
            _ = &mut stop => true,
            _ = tokio::time::sleep(TAIL_TICK) => false,
        };
        crate::nursery::read_new(&events, &mut offset, &mut leftover, &mut each).await;
        if last {
            break;
        }
    }
}

/// `git worktree add --detach` at HEAD under `.groka/worktrees/`.
async fn add_worktree(workspace: &Path, node: &str, seg: &str) -> Result<PathBuf, String> {
    let root = crate::vision::groka_dir(workspace, "worktrees").map_err(|e| e.to_string())?;
    let path = root.join(format!("{node}-{seg}"));
    let out = git(workspace, &["worktree", "add", "--detach", &path.to_string_lossy(), "HEAD"]).await?;
    if !out.0 {
        return Err(format!("git worktree add failed: {}", out.1.trim()));
    }
    Ok(path)
}

/// Remove an unchanged worktree. One with changes is kept, on a new branch
/// (with its work committed) so it is easy to find and merge; returns that
/// branch.
async fn finish_worktree(workspace: &Path, wt: &Path) -> Option<String> {
    let status = git(wt, &["status", "--porcelain"]).await.ok()?;
    let head = git(wt, &["rev-parse", "HEAD"]).await.ok()?;
    let base = git(workspace, &["rev-parse", "HEAD"]).await.ok()?;
    let changed = !status.1.trim().is_empty() || head.1.trim() != base.1.trim();
    if !changed {
        let _ = git(workspace, &["worktree", "remove", "--force", &wt.to_string_lossy()]).await;
        return None;
    }
    let name = wt.file_name()?.to_string_lossy().to_string();
    let branch = format!("groka-workflow/{name}");
    let _ = git(wt, &["checkout", "-B", &branch]).await;
    if !status.1.trim().is_empty() {
        let _ = git(wt, &["add", "-A"]).await;
        let _ = git(wt, &["-c", "user.name=grokaagent", "-c", "user.email=grokaagent@localhost", "commit", "-qm", &format!("workflow agent {name}")]).await;
    }
    Some(branch)
}

async fn git(dir: &Path, args: &[&str]) -> Result<(bool, String), String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .await
        .map_err(|e| format!("git: {e}"))?;
    let mut text = String::from_utf8_lossy(&out.stdout).to_string();
    if !out.status.success() {
        text.push_str(&String::from_utf8_lossy(&out.stderr));
    }
    Ok((out.status.success(), text))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn structured_results_come_from_json_text_and_must_fit() {
        let schema = json!({"type": "object", "properties": {"n": {"type": "integer"}}, "required": ["n"]});
        assert_eq!(structured("{\"n\": 1}", &schema), Ok(json!({"n": 1})));
        assert_eq!(structured("Here: {\"n\": 2} done", &schema), Ok(json!({"n": 2})));
        assert!(structured("no json", &schema).unwrap_err().contains("no JSON"));
        assert!(structured("{\"n\": \"x\"}", &schema).unwrap_err().contains("expected integer"));
    }
}
