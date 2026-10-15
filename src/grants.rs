//! Extra workspace roots the user granted.
//!
//! The agent works inside one workspace. When it needs another folder (a
//! sibling repo, a shared data directory), it asks with
//! `request_workspace_access`, giving a reason; the user allows it for this
//! session, allows it permanently for this project, or refuses. Granted
//! roots count as inside the workspace for the file tools, `run_command`'s
//! cwd and the command review gate.
//!
//! Session grants live in this process (and reach child agents through
//! `GROKA_EXTRA_ROOTS`); permanent ones are stored per project in
//! `~/.grokaagent/workspace-grants.json`, never inside the project.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::ask::{cancelled_json, AskUserHub, Choice};
use crate::error::{Error, Result};
use crate::events::{AgentEvent, EventMeta, EventSink};
use crate::tools::{ClientTool, ToolCallFut, ToolSpec};

pub const ENV: &str = "GROKA_EXTRA_ROOTS";
const MAX_REASON: usize = 400;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Grant {
    pub path: PathBuf,
    pub reason: String,
    #[serde(default)]
    pub granted_at: String,
}

/// Session grants of this process: (workspace, root).
static SESSION: Mutex<Vec<(PathBuf, PathBuf)>> = Mutex::new(Vec::new());

fn canon(p: &Path) -> PathBuf {
    // A path that does not exist yet (a file about to be written) resolves
    // through its nearest existing folder, so a symlinked temp folder
    // (macOS /var -> /private/var) or a Windows short name (RUNNER~1)
    // compares equal to the canonical root it is under.
    let mut rest = Vec::new();
    let mut cur = p;
    let c = loop {
        if let Ok(c) = cur.canonicalize() {
            break rest.iter().rev().fold(c, |acc: PathBuf, name| acc.join(name));
        }
        match (cur.parent(), cur.file_name()) {
            (Some(parent), Some(name)) => {
                rest.push(name.to_os_string());
                cur = parent;
            }
            _ => break p.to_path_buf(),
        }
    };
    // Drop the Windows verbatim prefix so comparisons and messages match
    // what people type.
    let s = c.to_string_lossy();
    match s.strip_prefix(r"\\?\") {
        Some(rest) if !rest.starts_with("UNC") => PathBuf::from(rest),
        _ => c,
    }
}

fn key(workspace: &Path) -> String {
    canon(workspace).to_string_lossy().to_string()
}

/// Permanent grants, per project, in one JSON file.
pub struct Store {
    file: PathBuf,
}

impl Store {
    pub fn at(file: PathBuf) -> Self {
        Self { file }
    }

    /// `~/.grokaagent/workspace-grants.json`. None in unit tests, so they
    /// never touch the developer's real grants.
    pub fn default_store() -> Option<Self> {
        if cfg!(test) {
            return None;
        }
        crate::config::groka_dir().ok().map(|d| Self::at(d.join("workspace-grants.json")))
    }

    fn load(&self) -> BTreeMap<String, Vec<Grant>> {
        std::fs::read_to_string(&self.file)
            .ok()
            .and_then(|t| serde_json::from_str(&t).ok())
            .unwrap_or_default()
    }

    fn save(&self, all: &BTreeMap<String, Vec<Grant>>) -> Result<()> {
        if let Some(dir) = self.file.parent() {
            std::fs::create_dir_all(dir)?;
        }
        std::fs::write(&self.file, serde_json::to_string_pretty(all)?)?;
        Ok(())
    }

    pub fn list(&self, workspace: &Path) -> Vec<Grant> {
        self.load().remove(&key(workspace)).unwrap_or_default()
    }

    pub fn list_all(&self) -> BTreeMap<String, Vec<Grant>> {
        self.load()
    }

    pub fn add(&self, workspace: &Path, root: &Path, reason: &str) -> Result<()> {
        let mut all = self.load();
        let list = all.entry(key(workspace)).or_default();
        let root = canon(root);
        if !list.iter().any(|g| g.path == root) {
            list.push(Grant {
                path: root,
                reason: reason.to_string(),
                granted_at: chrono::Utc::now().to_rfc3339(),
            });
        }
        self.save(&all)
    }

    /// Remove `root` from every project (or only `workspace`). Returns how
    /// many grants were removed.
    pub fn revoke(&self, workspace: Option<&Path>, root: &Path) -> Result<usize> {
        let mut all = self.load();
        let root = canon(root);
        let mut n = 0;
        for (ws, list) in all.iter_mut() {
            if workspace.is_some_and(|w| key(w) != *ws) {
                continue;
            }
            let before = list.len();
            list.retain(|g| g.path != root);
            n += before - list.len();
        }
        all.retain(|_, l| !l.is_empty());
        self.save(&all)?;
        Ok(n)
    }
}

pub fn add_session(workspace: &Path, root: &Path) {
    let mut g = SESSION.lock().unwrap_or_else(|e| e.into_inner());
    let entry = (canon(workspace), canon(root));
    if !g.contains(&entry) {
        g.push(entry);
    }
}

/// Session roots of `workspace`, for handing to child agents.
pub fn session_roots(workspace: &Path) -> Vec<PathBuf> {
    let ws = canon(workspace);
    SESSION
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .iter()
        .filter(|(w, _)| *w == ws)
        .map(|(_, r)| r.clone())
        .collect()
}

/// `GROKA_EXTRA_ROOTS` value for a child: this process's extra roots.
pub fn env_value(workspace: &Path) -> String {
    let mut roots = session_roots(workspace);
    roots.extend(env_roots());
    std::env::join_paths(roots).map(|s| s.to_string_lossy().to_string()).unwrap_or_default()
}

fn env_roots() -> Vec<PathBuf> {
    std::env::var_os(ENV)
        .map(|v| std::env::split_paths(&v).filter(|p| !p.as_os_str().is_empty()).collect())
        .unwrap_or_default()
}

/// Every extra root of `workspace`: session, inherited from a parent, and
/// permanent.
pub fn roots(workspace: &Path) -> Vec<PathBuf> {
    let mut out = session_roots(workspace);
    out.extend(env_roots().into_iter().map(|p| canon(&p)));
    if let Some(store) = Store::default_store() {
        out.extend(store.list(workspace).into_iter().map(|g| g.path));
    }
    out.sort();
    out.dedup();
    out
}

/// `path` (already absolute) inside one of `roots`?
pub fn within(path: &Path, roots: &[PathBuf]) -> bool {
    let p = canon(path);
    roots.iter().any(|r| p.starts_with(r))
}

/// Folders too broad to grant: a filesystem root or the home folder itself.
fn too_broad(path: &Path) -> bool {
    let p = canon(path);
    if p.parent().is_none() {
        return true;
    }
    if let Some(home) = dirs::home_dir() {
        if canon(&home) == p {
            return true;
        }
    }
    false
}

/// The tool the model uses to ask.
pub struct RequestAccessTool {
    hub: AskUserHub,
    sink: Arc<dyn EventSink>,
    workspace: PathBuf,
    agent_name: String,
    run_id: String,
    parent_run_id: Option<String>,
}

impl RequestAccessTool {
    pub fn new(
        hub: AskUserHub,
        sink: Arc<dyn EventSink>,
        workspace: PathBuf,
        agent_name: String,
        run_id: String,
        parent_run_id: Option<String>,
    ) -> Self {
        Self { hub, sink, workspace, agent_name, run_id, parent_run_id }
    }

    fn meta(&self) -> EventMeta {
        EventMeta {
            ts: chrono::Utc::now(),
            agent_name: self.agent_name.clone(),
            run_id: self.run_id.clone(),
            parent_run_id: self.parent_run_id.clone(),
            path: String::new(),
        }
    }

    async fn run(&self, args: Value) -> Result<String> {
        let raw = args.get("path").and_then(Value::as_str).map(str::trim).unwrap_or("");
        let reason = args.get("reason").and_then(Value::as_str).map(str::trim).unwrap_or("");
        if raw.is_empty() {
            return Err(Error::Tool("path is required".into()));
        }
        if reason.chars().count() < 8 {
            return Err(Error::Tool("give the user a real reason (what you need from that folder and why)".into()));
        }
        let reason: String = reason.chars().take(MAX_REASON).collect();
        let suggested = match args.get("scope").and_then(Value::as_str).unwrap_or("session") {
            "session" => "session",
            "permanent" => "permanent",
            other => return Err(Error::Tool(format!("scope must be session or permanent (got {other})"))),
        };
        let requested = PathBuf::from(raw);
        let abs = if requested.is_absolute() { requested } else { self.workspace.join(requested) };
        if !abs.is_dir() {
            return Err(Error::Tool(format!("{raw} is not an existing folder")));
        }
        let root = canon(&abs);
        let ws = canon(&self.workspace);
        if root.starts_with(&ws) || within(&root, &roots(&self.workspace)) {
            return Ok(json!({"granted": true, "path": root, "scope": "already", "note": "already inside the workspace"}).to_string());
        }
        if too_broad(&root) {
            return Err(Error::Tool(format!(
                "{} is too broad to grant; ask for the specific folder you need",
                root.display()
            )));
        }
        let suggestion = if suggested == "permanent" { "永久（此專案）" } else { "只限這次對話" };
        let question = format!(
            "代理要求把這個資料夾加入工作區：\n{}\n理由：{reason}\n代理建議：{suggestion}",
            root.display()
        );
        let options = vec![
            Choice { id: "session".into(), label: "允許（只限這次對話）".into(), input: false },
            Choice { id: "permanent".into(), label: "永久允許（此專案）".into(), input: false },
            Choice { id: "deny".into(), label: "拒絕".into(), input: false },
        ];
        let rx = self.hub.register();
        self.sink.emit(&AgentEvent::AskUser { meta: self.meta(), question, allow_multiple: false, options });
        let answer = rx.await.unwrap_or_else(|_| cancelled_json());
        let picked = serde_json::from_str::<Value>(&answer)
            .ok()
            .and_then(|v| v["selected"][0]["id"].as_str().map(str::to_string))
            .unwrap_or_default();
        match picked.as_str() {
            "session" => {
                add_session(&self.workspace, &root);
                Ok(json!({"granted": true, "scope": "session", "path": root}).to_string())
            }
            "permanent" => {
                add_session(&self.workspace, &root);
                if let Some(store) = Store::default_store() {
                    store.add(&self.workspace, &root, &reason)?;
                }
                Ok(json!({"granted": true, "scope": "permanent", "path": root}).to_string())
            }
            _ => Ok(json!({
                "granted": false,
                "path": root,
                "note": "The user said no. Do not ask again for this folder in this task; work without it or ask the user how to proceed."
            })
            .to_string()),
        }
    }
}

impl ClientTool for RequestAccessTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "request_workspace_access".into(),
            description: "Ask the user to let you work in a folder outside the workspace (a sibling repo, a shared data folder). The user sees the folder and your reason, then allows it for this session, allows it permanently for this project, or refuses. Once granted, file tools, run_command cwd and the command review treat it as part of the workspace. Ask only when the task needs it, name the narrowest folder, and explain concretely what you will read or change there.".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string", "description": "Absolute path of the folder (or relative to the workspace)"},
                    "reason": {"type": "string", "description": "Shown to the user: what you need from this folder and why"},
                    "scope": {"type": "string", "enum": ["session", "permanent"], "description": "What you suggest; the user decides. session = until this conversation ends; permanent = remembered for this project"}
                },
                "required": ["path", "reason"],
                "additionalProperties": false
            }),
        }
    }

    fn call(&self, args: &Value) -> ToolCallFut<'_> {
        let args = args.clone();
        Box::pin(async move { self.run(args).await })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex as StdMutex;

    struct Rec(StdMutex<Vec<AgentEvent>>);
    impl EventSink for Rec {
        fn emit(&self, e: &AgentEvent) {
            self.0.lock().unwrap().push(e.clone());
        }
    }

    fn setup() -> (tempfile::TempDir, PathBuf, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let ws = dir.path().join("ws");
        let other = dir.path().join("shared-data");
        std::fs::create_dir_all(&ws).unwrap();
        std::fs::create_dir_all(&other).unwrap();
        (dir, ws, other)
    }

    async fn ask(ws: &Path, path: &Path, answer: &str) -> (Value, Vec<AgentEvent>) {
        let hub = AskUserHub::new();
        let rec = Arc::new(Rec(StdMutex::new(Vec::new())));
        let tool = RequestAccessTool::new(hub.clone(), rec.clone(), ws.to_path_buf(), "root".into(), "r".into(), None);
        let answer = json!({"cancelled": false, "selected": [{"id": answer, "label": answer, "value": null}]}).to_string();
        hub.answer(answer);
        let out = tool
            .call(&json!({"path": path.to_string_lossy(), "reason": "read the shared fixtures for the parser tests"}))
            .await
            .unwrap();
        let events = rec.0.lock().unwrap().clone();
        (serde_json::from_str(&out).unwrap(), events)
    }

    #[tokio::test]
    async fn the_user_sees_the_folder_and_reason_and_a_session_grant_counts() {
        let (_d, ws, other) = setup();
        assert!(!within(&other, &roots(&ws)));
        let (out, events) = ask(&ws, &other, "session").await;
        assert_eq!(out["granted"], true);
        assert_eq!(out["scope"], "session");
        let shown = events.iter().find_map(|e| match e {
            AgentEvent::AskUser { question, options, .. } => Some((question.clone(), options.len())),
            _ => None,
        });
        let (question, n) = shown.expect("the user was asked");
        assert!(question.contains("shared-data") && question.contains("shared fixtures"), "{question}");
        assert_eq!(n, 3);
        assert!(within(&other.join("x.json"), &roots(&ws)));
        assert!(env_value(&ws).contains("shared-data"), "children inherit it");
    }

    #[test]
    fn a_file_not_written_yet_counts_under_its_granted_folder_however_it_is_spelled() {
        let (_d, ws, other) = setup();
        let root = canon(&other);
        // Not on disk yet, spelled through `..`: resolved via its folder.
        let new_file = ws.join("..").join("shared-data").join("sub").join("x.json");
        assert!(within(&new_file, std::slice::from_ref(&root)), "{}", canon(&new_file).display());
        assert!(!within(&ws.join("y.json"), &[root]));
    }

    #[cfg(unix)]
    #[test]
    fn a_symlinked_spelling_of_a_granted_folder_counts_too() {
        // macOS temp folders live under /var, a symlink to /private/var.
        let (d, _ws, other) = setup();
        let link = d.path().join("link");
        std::os::unix::fs::symlink(&other, &link).unwrap();
        assert!(within(&link.join("new.json"), &[canon(&other)]));
    }

    #[tokio::test]
    async fn a_refusal_grants_nothing() {
        let (_d, ws, other) = setup();
        let (out, _) = ask(&ws, &other, "deny").await;
        assert_eq!(out["granted"], false);
        assert!(!within(&other, &roots(&ws)));
    }

    #[tokio::test]
    async fn requests_need_a_reason_an_existing_folder_and_not_too_much() {
        let (_d, ws, other) = setup();
        let tool = RequestAccessTool::new(AskUserHub::new(), Arc::new(Rec(StdMutex::new(Vec::new()))), ws.clone(), "root".into(), "r".into(), None);
        let err = tool.call(&json!({"path": other.to_string_lossy(), "reason": "need"})).await.unwrap_err();
        assert!(err.to_string().contains("reason"), "{err}");
        let err = tool.call(&json!({"path": other.join("missing").to_string_lossy(), "reason": "read the fixtures please"})).await.unwrap_err();
        assert!(err.to_string().contains("not an existing folder"), "{err}");
        let root = if cfg!(windows) { "C:\\" } else { "/" };
        let err = tool.call(&json!({"path": root, "reason": "read the fixtures please"})).await.unwrap_err();
        assert!(err.to_string().contains("too broad"), "{err}");
    }

    #[tokio::test]
    async fn file_tools_reach_a_granted_folder_and_nothing_else() {
        let (d, ws, other) = setup();
        std::fs::write(other.join("fixture.json"), "{\"ok\": true}").unwrap();
        let secret = d.path().join("not-granted");
        std::fs::create_dir_all(&secret).unwrap();
        std::fs::write(secret.join("key.txt"), "s3cret").unwrap();
        let read = crate::tools::ReadFileTool::new(ws.clone());
        let fixture = other.join("fixture.json").to_string_lossy().to_string();
        let err = read.call_sync(&json!({"path": fixture})).unwrap_err().to_string();
        assert!(err.contains("request_workspace_access"), "the refusal points at the way to ask: {err}");
        add_session(&ws, &other);
        assert!(read.call_sync(&json!({"path": fixture})).unwrap().contains("\"ok\""));
        let key = secret.join("key.txt").to_string_lossy().to_string();
        assert!(read.call_sync(&json!({"path": key})).is_err(), "only the granted folder");
    }

    #[test]
    fn permanent_grants_are_stored_per_project_and_revocable() {
        let (d, ws, other) = setup();
        let store = Store::at(d.path().join("grants.json"));
        store.add(&ws, &other, "fixtures").unwrap();
        store.add(&ws, &other, "again").unwrap();
        let list = store.list(&ws);
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].reason, "fixtures");
        assert!(store.list(&other).is_empty(), "per project");
        assert_eq!(store.revoke(Some(&ws), &other).unwrap(), 1);
        assert!(store.list(&ws).is_empty());
    }
}
