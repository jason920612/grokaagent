use std::fs;
use std::future::Future;
use std::path::{Component, Path, PathBuf};
use std::pin::Pin;
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use serde_json::{json, Value};
use tokio::process::Command;

use crate::diff;
use crate::error::{Error, Result};
use crate::shellguard::{self, CommandReviewer};

#[derive(Debug, Clone)]
pub struct ToolSpec {
    pub name: String,
    pub description: String,
    pub parameters: Value,
}

pub type ToolCallFut<'a> = Pin<Box<dyn Future<Output = Result<String>> + Send + 'a>>;

pub trait ClientTool: Send + Sync {
    fn spec(&self) -> ToolSpec;
    fn call(&self, args: &Value) -> ToolCallFut<'_>;
}

pub struct ToolRegistry {
    tools: Vec<Box<dyn ClientTool>>,
}

impl ToolRegistry {
    pub fn new(tools: Vec<Box<dyn ClientTool>>) -> Self {
        Self { tools }
    }

    pub fn specs(&self) -> Vec<ToolSpec> {
        self.tools.iter().map(|t| t.spec()).collect()
    }

    pub async fn call(&self, name: &str, args: &Value) -> Result<String> {
        let tool = self
            .tools
            .iter()
            .find(|t| t.spec().name == name)
            .ok_or_else(|| Error::Tool(format!("unknown tool {name}")))?;
        tool.call(args).await
    }
}

fn ready(result: Result<String>) -> ToolCallFut<'static> {
    Box::pin(async move { result })
}

fn clip_text(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let clipped: String = s.chars().take(max).collect();
        format!("{clipped}\n[truncated]")
    }
}

pub(crate) fn hide_window(cmd: &mut Command) {
    #[cfg(windows)]
    {
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    let _ = cmd;
}

pub(crate) fn shell_command(command: &str) -> Command {
    #[cfg(windows)]
    {
        let mut c = Command::new("cmd");
        c.arg("/C").arg(command);
        c
    }
    #[cfg(not(windows))]
    {
        let mut c = Command::new("sh");
        c.arg("-c").arg(command);
        c
    }
}

pub struct NowTool;

impl NowTool {
    pub fn call_sync(&self, _args: &Value) -> Result<String> {
        Ok(json!({"utc": chrono::Utc::now().to_rfc3339()}).to_string())
    }
}

impl ClientTool for NowTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "now".into(),
            description: "Return the current UTC time as RFC 3339.".into(),
            parameters: json!({"type": "object", "properties": {}, "additionalProperties": false}),
        }
    }

    fn call(&self, args: &Value) -> ToolCallFut<'_> {
        ready(self.call_sync(args))
    }
}

pub struct ReadFileTool {
    workspace: PathBuf,
}

impl ReadFileTool {
    pub fn new(workspace: PathBuf) -> Self {
        Self { workspace }
    }

    pub fn call_sync(&self, args: &Value) -> Result<String> {
        let path = args
            .get("path")
            .and_then(Value::as_str)
            .ok_or_else(|| Error::Tool("path is required".into()))?;
        let resolved = resolve_in_workspace(&self.workspace, path)?;
        let bytes = fs::read(&resolved)?;
        if bytes.len() > 256 * 1024 {
            return Err(Error::Tool("file larger than 256KiB".into()));
        }
        let text = String::from_utf8(bytes)
            .map_err(|_| Error::Tool("file is not valid UTF-8".into()))?;
        Ok(text)
    }
}

pub fn resolve_in_workspace(workspace: &Path, requested: &str) -> Result<PathBuf> {
    let target = resolve_target_in_workspace(workspace, requested)?;
    let canonical = target
        .canonicalize()
        .map_err(|_| Error::Tool(format!("file not found: {requested}")))?;
    let workspace = workspace
        .canonicalize()
        .map_err(|e| Error::Tool(format!("workspace not found: {e}")))?;
    if !canonical.starts_with(&workspace) {
        return Err(Error::Tool("path escapes workspace".into()));
    }
    Ok(canonical)
}

/// Resolve a path that may not exist yet (write/create).
pub fn resolve_target_in_workspace(workspace: &Path, requested: &str) -> Result<PathBuf> {
    if requested.is_empty() {
        return Err(Error::Tool("path is required".into()));
    }
    let workspace = workspace
        .canonicalize()
        .map_err(|e| Error::Tool(format!("workspace not found: {e}")))?;
    if Path::new(requested).is_absolute() {
        let joined = PathBuf::from(requested);
        let parent = joined.parent().unwrap_or(Path::new("."));
        if parent.exists() {
            let parent = parent
                .canonicalize()
                .map_err(|_| Error::Tool(format!("path not found: {requested}")))?;
            if !parent.starts_with(&workspace) {
                return Err(Error::Tool("path escapes workspace".into()));
            }
            return Ok(parent.join(joined.file_name().unwrap_or_default()));
        }
        return Err(Error::Tool("path escapes workspace".into()));
    }
    let mut out = workspace.clone();
    for c in Path::new(requested).components() {
        match c {
            Component::CurDir => {}
            Component::ParentDir => {
                if !out.pop() || !out.starts_with(&workspace) {
                    return Err(Error::Tool("path escapes workspace".into()));
                }
            }
            Component::Normal(s) => out.push(s),
            Component::Prefix(_) | Component::RootDir => {
                return Err(Error::Tool("path escapes workspace".into()));
            }
        }
    }
    if !out.starts_with(&workspace) {
        return Err(Error::Tool("path escapes workspace".into()));
    }
    Ok(out)
}

impl ClientTool for ReadFileTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "read_file".into(),
            description: "Read a UTF-8 text file relative to the workspace.".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string", "description": "Relative path inside the workspace"}
                },
                "required": ["path"],
                "additionalProperties": false
            }),
        }
    }

    fn call(&self, args: &Value) -> ToolCallFut<'_> {
        ready(self.call_sync(args))
    }
}

pub struct ListDirTool {
    workspace: PathBuf,
}

impl ListDirTool {
    pub fn new(workspace: PathBuf) -> Self {
        Self { workspace }
    }

    pub fn call_sync(&self, args: &Value) -> Result<String> {
        let path = args.get("path").and_then(Value::as_str).unwrap_or(".");
        let resolved = if path.is_empty() || path == "." {
            self.workspace
                .canonicalize()
                .map_err(|e| Error::Tool(format!("workspace not found: {e}")))?
        } else {
            resolve_in_workspace(&self.workspace, path)?
        };
        if !resolved.is_dir() {
            return Err(Error::Tool(format!("not a directory: {path}")));
        }
        const MAX: usize = 200;
        let mut items: Vec<_> = fs::read_dir(&resolved)?
            .filter_map(|e| e.ok())
            .collect();
        items.sort_by_key(|e| e.file_name());
        let total = items.len();
        let truncated = total > MAX;
        let mut entries = Vec::new();
        for e in items.into_iter().take(MAX) {
            let name = e.file_name().to_string_lossy().into_owned();
            let meta = e.metadata().ok();
            let is_dir = meta.as_ref().map(|m| m.is_dir()).unwrap_or(false);
            let size = if is_dir {
                Value::Null
            } else {
                json!(meta.map(|m| m.len()).unwrap_or(0))
            };
            entries.push(json!({"name": name, "dir": is_dir, "size": size}));
        }
        Ok(json!({
            "path": path,
            "entries": entries,
            "count": total,
            "truncated": truncated
        })
        .to_string())
    }
}

impl ClientTool for ListDirTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "list_dir".into(),
            description: "List files and directories in a workspace folder (not recursive). Prefer this over run_command dir/ls.".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string", "description": "Relative directory inside the workspace. Default is ."}
                },
                "additionalProperties": false
            }),
        }
    }

    fn call(&self, args: &Value) -> ToolCallFut<'_> {
        ready(self.call_sync(args))
    }
}

pub struct WriteFileTool {
    workspace: PathBuf,
}

impl WriteFileTool {
    pub fn new(workspace: PathBuf) -> Self {
        Self { workspace }
    }

    pub fn call_sync(&self, args: &Value) -> Result<String> {
        let path = args
            .get("path")
            .and_then(Value::as_str)
            .ok_or_else(|| Error::Tool("path is required".into()))?;
        let contents = args
            .get("contents")
            .and_then(Value::as_str)
            .ok_or_else(|| Error::Tool("contents is required".into()))?;
        if contents.len() > 256 * 1024 {
            return Err(Error::Tool("contents larger than 256KiB".into()));
        }
        let target = resolve_target_in_workspace(&self.workspace, path)?;
        let before = fs::read_to_string(&target).ok();
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(&target, contents)?;
        let rel = path.replace('\\', "/");
        Ok(diff::file_change_json(&rel, before.as_deref(), Some(contents)).to_string())
    }
}

impl ClientTool for WriteFileTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "write_file".into(),
            description: "Create or overwrite a UTF-8 text file in the workspace. Returns a unified diff.".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string", "description": "Relative path inside the workspace"},
                    "contents": {"type": "string", "description": "Full file contents"}
                },
                "required": ["path", "contents"],
                "additionalProperties": false
            }),
        }
    }

    fn call(&self, args: &Value) -> ToolCallFut<'_> {
        ready(self.call_sync(args))
    }
}

pub struct DeleteFileTool {
    workspace: PathBuf,
}

impl DeleteFileTool {
    pub fn new(workspace: PathBuf) -> Self {
        Self { workspace }
    }

    pub fn call_sync(&self, args: &Value) -> Result<String> {
        let path = args
            .get("path")
            .and_then(Value::as_str)
            .ok_or_else(|| Error::Tool("path is required".into()))?;
        let resolved = resolve_in_workspace(&self.workspace, path)?;
        let before = fs::read_to_string(&resolved).ok();
        fs::remove_file(&resolved)?;
        let rel = path.replace('\\', "/");
        Ok(diff::file_change_json(&rel, before.as_deref(), None).to_string())
    }
}

impl ClientTool for DeleteFileTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "delete_file".into(),
            description: "Delete a file in the workspace. Returns a unified diff of the removed contents.".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string", "description": "Relative path inside the workspace"}
                },
                "required": ["path"],
                "additionalProperties": false
            }),
        }
    }

    fn call(&self, args: &Value) -> ToolCallFut<'_> {
        ready(self.call_sync(args))
    }
}

pub struct RunCommandTool {
    workspace: PathBuf,
    guard: Option<Arc<dyn CommandReviewer>>,
}

impl RunCommandTool {
    pub fn new(workspace: PathBuf) -> Self {
        Self {
            workspace,
            guard: None,
        }
    }

    pub fn with_guard(workspace: PathBuf, guard: Arc<dyn CommandReviewer>) -> Self {
        Self {
            workspace,
            guard: Some(guard),
        }
    }

    async fn run(&self, args: &Value) -> Result<String> {
        let command = args
            .get("command")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| Error::Tool("command is required".into()))?;
        let cwd = match args.get("cwd").and_then(Value::as_str) {
            Some(p) if !p.is_empty() && p != "." => resolve_in_workspace(&self.workspace, p)?,
            _ => self
                .workspace
                .canonicalize()
                .map_err(|e| Error::Tool(format!("workspace not found: {e}")))?,
        };
        if !cwd.is_dir() {
            return Err(Error::Tool("cwd is not a directory".into()));
        }
        shellguard::enforce(self.guard.as_ref(), command, &cwd.to_string_lossy()).await?;

        let mut cmd = {
            #[cfg(windows)]
            {
                let mut c = Command::new("cmd");
                c.arg("/C").arg(command);
                c
            }
            #[cfg(not(windows))]
            {
                let mut c = Command::new("sh");
                c.arg("-c").arg(command);
                c
            }
        };
        cmd.current_dir(&cwd)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        hide_window(&mut cmd);

        let child = cmd
            .spawn()
            .map_err(|e| Error::Tool(format!("spawn failed: {e}")))?;
        let timed = tokio::time::timeout(Duration::from_secs(60), child.wait_with_output()).await;
        let (timed_out, output) = match timed {
            Ok(Ok(o)) => (false, o),
            Ok(Err(e)) => return Err(Error::Tool(format!("command failed: {e}"))),
            Err(_) => {
                return Ok(json!({
                    "timed_out": true,
                    "exit_code": null,
                    "stdout": "",
                    "stderr": "timed out after 60s",
                    "files": []
                })
                .to_string());
            }
        };
        let stdout = clip_text(&String::from_utf8_lossy(&output.stdout), 32 * 1024);
        let stderr = clip_text(&String::from_utf8_lossy(&output.stderr), 16 * 1024);
        let git_cwd = cwd.clone();
        let files = tokio::task::spawn_blocking(move || diff::git_file_changes(&git_cwd))
            .await
            .unwrap_or_else(|_| Vec::new());
        Ok(json!({
            "timed_out": timed_out,
            "exit_code": output.status.code(),
            "stdout": stdout,
            "stderr": stderr,
            "files": files
        })
        .to_string())
    }
}

impl ClientTool for RunCommandTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "run_command".into(),
            description: "Run a shell command with cwd in the workspace (Windows cmd / Unix sh). Compound or nested commands are reviewed against this OS shell's quoting rules before they run. Returns stdout, stderr, exit_code, and git-style file diffs when the workspace is a git repo.".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "command": {"type": "string", "description": "Shell command to run"},
                    "cwd": {"type": "string", "description": "Optional subdirectory inside the workspace"}
                },
                "required": ["command"],
                "additionalProperties": false
            }),
        }
    }

    fn call(&self, args: &Value) -> ToolCallFut<'_> {
        let args = args.clone();
        Box::pin(async move { self.run(&args).await })
    }
}

pub struct ScreenshotTool {
    workspace: PathBuf,
    spawned: Arc<dyn Fn() -> Vec<u32> + Send + Sync>,
}

impl ScreenshotTool {
    pub fn new(workspace: PathBuf) -> Self {
        Self {
            workspace,
            spawned: Arc::new(Vec::new),
        }
    }

    pub fn with_spawned(
        workspace: PathBuf,
        spawned: Arc<dyn Fn() -> Vec<u32> + Send + Sync>,
    ) -> Self {
        Self { workspace, spawned }
    }

    fn workspace_hint(&self) -> Option<String> {
        self.workspace
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .filter(|s| !s.is_empty() && s != "." && s != "..")
    }

    async fn run(&self, args: &Value) -> Result<String> {
        let list_only = args.get("list").and_then(Value::as_bool).unwrap_or(false);
        let target = args
            .get("target")
            .and_then(Value::as_str)
            .map(str::trim)
            .unwrap_or("window");
        let title = args
            .get("title")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string());
        let app = args
            .get("app")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string());
        let spawned = (self.spawned)();
        let hint = self.workspace_hint();
        let self_pid = std::process::id();

        if list_only {
            return tokio::task::spawn_blocking(move || {
                list_windows_json(&spawned, self_pid, hint.as_deref(), title.as_deref(), app.as_deref())
            })
            .await
            .map_err(|e| Error::Tool(format!("screenshot join: {e}")))?;
        }

        let rel = args
            .get("path")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(|s| s.replace('\\', "/"))
            .unwrap_or_else(|| {
                crate::vision::rel_shot_path()
                    .to_string_lossy()
                    .replace('\\', "/")
            });
        let dest = resolve_target_in_workspace(&self.workspace, &rel)?;
        let want_monitor = target.eq_ignore_ascii_case("monitor")
            || target.eq_ignore_ascii_case("screen")
            || target.eq_ignore_ascii_case("desktop");
        tokio::task::spawn_blocking(move || {
            if want_monitor {
                capture_monitor_to(&dest, &rel)
            } else {
                capture_window_to(
                    &dest,
                    &rel,
                    title.as_deref(),
                    app.as_deref(),
                    &spawned,
                    self_pid,
                    hint.as_deref(),
                )
            }
        })
        .await
        .map_err(|e| Error::Tool(format!("screenshot join: {e}")))?
    }
}

fn list_windows_json(
    spawned: &[u32],
    self_pid: u32,
    hint: Option<&str>,
    title: Option<&str>,
    app: Option<&str>,
) -> Result<String> {
    let listed = crate::shot::collect_windows()?;
    let metas: Vec<_> = listed.iter().map(|(_, m)| m.clone()).collect();
    let filter = crate::shot::ShotFilter {
        title,
        app,
        spawned,
        self_pids: &[self_pid],
        workspace_hint: hint,
    };
    let ranked = crate::shot::ranked_indices(&metas, &filter);
    let windows: Vec<Value> = ranked
        .into_iter()
        .take(16)
        .map(|i| {
            let m = &metas[i];
            json!({
                "title": m.title,
                "app": m.app,
                "pid": m.pid,
                "width": m.width,
                "height": m.height
            })
        })
        .collect();
    Ok(json!({
        "windows": windows,
        "attach_image": false
    })
    .to_string())
}

fn capture_monitor_to(target: &Path, rel: &str) -> Result<String> {
    let rgba = crate::shot::capture_monitor()?;
    let img = crate::vision::from_rgba(rgba.width(), rgba.height(), rgba.into_raw())?;
    shot_json(target, rel, &img, json!({"source": "monitor"}))
}

fn capture_window_to(
    target: &Path,
    rel: &str,
    title: Option<&str>,
    app: Option<&str>,
    spawned: &[u32],
    self_pid: u32,
    hint: Option<&str>,
) -> Result<String> {
    let listed = crate::shot::collect_windows()?;
    let metas: Vec<_> = listed.iter().map(|(_, m)| m.clone()).collect();
    let filter = crate::shot::ShotFilter {
        title,
        app,
        spawned,
        self_pids: &[self_pid],
        workspace_hint: hint,
    };
    let ranked = crate::shot::ranked_indices(&metas, &filter);
    for i in &ranked {
        let (win, meta) = &listed[*i];
        let Ok(rgba) = win.capture_image() else {
            continue;
        };
        if rgba.width() < 160 || rgba.height() < 120 {
            continue;
        }
        let Ok(img) = crate::vision::from_rgba(rgba.width(), rgba.height(), rgba.into_raw()) else {
            continue;
        };
        return shot_json(
            target,
            rel,
            &img,
            json!({
                "source": "window",
                "title": meta.title,
                "app": meta.app,
                "pid": meta.pid
            }),
        );
    }
    let mut extra = json!({
        "source": "monitor",
        "note": "no matching window; captured primary display. Pass title/app or list=true."
    });
    let preview: Vec<Value> = ranked
        .into_iter()
        .take(8)
        .map(|i| {
            json!({
                "title": metas[i].title,
                "app": metas[i].app,
                "pid": metas[i].pid
            })
        })
        .collect();
    extra["windows"] = Value::Array(preview);
    let rgba = crate::shot::capture_monitor()?;
    let img = crate::vision::from_rgba(rgba.width(), rgba.height(), rgba.into_raw())?;
    shot_json(target, rel, &img, extra)
}

fn shot_json(
    target: &Path,
    rel: &str,
    img: &image::DynamicImage,
    extra: Value,
) -> Result<String> {
    let (width, height, bytes) = crate::vision::save_jpeg(target, img)?;
    let mut out = json!({
        "path": rel.replace('\\', "/"),
        "kind": "create",
        "diff": format!("(image jpeg {width}x{height})"),
        "attach_image": true,
        "mime": "image/jpeg",
        "width": width,
        "height": height,
        "bytes": bytes
    });
    if let (Some(obj), Some(extra_obj)) = (out.as_object_mut(), extra.as_object()) {
        for (k, v) in extra_obj {
            obj.insert(k.clone(), v.clone());
        }
    }
    Ok(out.to_string())
}

impl ClientTool for ScreenshotTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "screenshot".into(),
            description: "Capture the GUI you opened (a window) to a JPEG and attach the pixels on the next turn. Does not capture the IDE or the grokaagent terminal. Optional title/app substring to pick a window; target=monitor captures the whole primary display; list=true returns window titles without capturing. Default path is .groka/shots/<timestamp>.jpg.".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string", "description": "Relative JPEG path inside the workspace"},
                    "title": {"type": "string", "description": "Substring of the window title to capture"},
                    "app": {"type": "string", "description": "Substring of the app name (chrome, msedge, electron, …)"},
                    "target": {"type": "string", "description": "window (default) or monitor"},
                    "list": {"type": "boolean", "description": "If true, list matching windows and do not capture"}
                },
                "additionalProperties": false
            }),
        }
    }

    fn call(&self, args: &Value) -> ToolCallFut<'_> {
        let args = args.clone();
        Box::pin(async move { self.run(&args).await })
    }
}

pub struct ReadImageTool {
    workspace: PathBuf,
}

impl ReadImageTool {
    pub fn new(workspace: PathBuf) -> Self {
        Self { workspace }
    }

    pub fn call_sync(&self, args: &Value) -> Result<String> {
        let path = args
            .get("path")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| Error::Tool("path is required".into()))?;
        let resolved = resolve_in_workspace(&self.workspace, path)?;
        let img = image::open(&resolved).map_err(|e| Error::Tool(format!("open image: {e}")))?;
        Ok(json!({
            "path": path.replace('\\', "/"),
            "attach_image": true,
            "mime": "image/jpeg",
            "width": img.width(),
            "height": img.height()
        })
        .to_string())
    }
}

impl ClientTool for ReadImageTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "read_image".into(),
            description: "Load a PNG or JPEG from the workspace and attach the pixels so you can see the image on the next turn. Use after screenshot, or to inspect an image file the user or a web app wrote.".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string", "description": "Relative PNG or JPEG path inside the workspace"}
                },
                "required": ["path"],
                "additionalProperties": false
            }),
        }
    }

    fn call(&self, args: &Value) -> ToolCallFut<'_> {
        ready(self.call_sync(args))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[test]
    fn now_returns_rfc3339() {
        let out: Value = serde_json::from_str(&NowTool.call_sync(&json!({})).unwrap()).unwrap();
        let utc = out["utc"].as_str().unwrap();
        assert!(utc.contains('T'), "{utc}");
        chrono::DateTime::parse_from_rfc3339(utc).unwrap();
    }

    #[test]
    fn read_file_reads_workspace_file() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("note.txt"), "hello-agent").unwrap();
        let tool = ReadFileTool::new(dir.path().to_path_buf());
        let out = tool.call_sync(&json!({"path": "note.txt"})).unwrap();
        assert_eq!(out, "hello-agent");
    }

    #[test]
    fn read_file_rejects_escape() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("secret.txt"), "nope").unwrap();
        let workspace = dir.path().join("sub");
        fs::create_dir_all(&workspace).unwrap();
        let err = resolve_in_workspace(&workspace, "../secret.txt").unwrap_err();
        assert!(err.to_string().contains("escapes"), "{}", err);
    }

    #[test]
    fn list_dir_lists_files_and_dirs() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("a.txt"), "x").unwrap();
        fs::create_dir(dir.path().join("sub")).unwrap();
        let tool = ListDirTool::new(dir.path().to_path_buf());
        let out: Value = serde_json::from_str(&tool.call_sync(&json!({})).unwrap()).unwrap();
        let names: Vec<&str> = out["entries"]
            .as_array()
            .unwrap()
            .iter()
            .map(|e| e["name"].as_str().unwrap())
            .collect();
        assert!(names.contains(&"a.txt"), "{out}");
        assert!(names.contains(&"sub"), "{out}");
        let sub = out["entries"]
            .as_array()
            .unwrap()
            .iter()
            .find(|e| e["name"] == "sub")
            .unwrap();
        assert_eq!(sub["dir"], true);
        assert_eq!(out["truncated"], false);
    }

    #[test]
    fn list_dir_rejects_escape() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = dir.path().join("sub");
        fs::create_dir_all(&workspace).unwrap();
        let tool = ListDirTool::new(workspace);
        let err = tool.call_sync(&json!({"path": ".."})).unwrap_err();
        assert!(err.to_string().contains("escapes"), "{err}");
    }

    #[test]
    fn write_file_creates_with_diff() {
        let dir = tempfile::tempdir().unwrap();
        let tool = WriteFileTool::new(dir.path().to_path_buf());
        let out: Value = serde_json::from_str(
            &tool
                .call_sync(&json!({"path": "src/a.txt", "contents": "hello\n"}))
                .unwrap(),
        )
        .unwrap();
        assert_eq!(out["kind"], "create");
        assert!(out["diff"].as_str().unwrap().contains("+hello"), "{out}");
        assert_eq!(fs::read_to_string(dir.path().join("src/a.txt")).unwrap(), "hello\n");
    }

    #[test]
    fn write_file_rejects_escape() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = dir.path().join("sub");
        fs::create_dir_all(&workspace).unwrap();
        let tool = WriteFileTool::new(workspace);
        let err = tool
            .call_sync(&json!({"path": "../x.txt", "contents": "no"}))
            .unwrap_err();
        assert!(err.to_string().contains("escapes"), "{err}");
    }

    #[test]
    fn delete_file_returns_delete_diff() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("gone.txt"), "bye\n").unwrap();
        let tool = DeleteFileTool::new(dir.path().to_path_buf());
        let out: Value = serde_json::from_str(
            &tool.call_sync(&json!({"path": "gone.txt"})).unwrap(),
        )
        .unwrap();
        assert_eq!(out["kind"], "delete");
        assert!(out["diff"].as_str().unwrap().contains("-bye"), "{out}");
        assert!(!dir.path().join("gone.txt").exists());
    }

    #[tokio::test]
    async fn run_command_echoes() {
        let dir = tempfile::tempdir().unwrap();
        let tool = RunCommandTool::new(dir.path().to_path_buf());
        let out: Value = serde_json::from_str(
            &tool
                .run(&json!({"command": "echo hello-agent"}))
                .await
                .unwrap(),
        )
        .unwrap();
        let stdout = out["stdout"].as_str().unwrap();
        assert!(stdout.contains("hello-agent"), "{stdout}");
        assert_eq!(out["exit_code"], 0);
        assert_eq!(out["timed_out"], false);
    }

    #[tokio::test]
    async fn run_command_rejects_empty() {
        let dir = tempfile::tempdir().unwrap();
        let tool = RunCommandTool::new(dir.path().to_path_buf());
        let err = tool.run(&json!({"command": "  "})).await.unwrap_err();
        assert!(err.to_string().contains("command is required"), "{err}");
    }

    struct DenyAll;
    impl CommandReviewer for DenyAll {
        fn review<'a>(
            &'a self,
            _command: &'a str,
            _cwd: &'a str,
        ) -> Pin<Box<dyn Future<Output = Result<crate::shellguard::Verdict>> + Send + 'a>> {
            Box::pin(async {
                Ok(crate::shellguard::Verdict::Deny {
                    reasons: vec!["nope".into()],
                })
            })
        }
    }

    #[tokio::test]
    async fn run_command_blocked_does_not_execute() {
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("pwned.txt");
        let tool = RunCommandTool::with_guard(dir.path().to_path_buf(), Arc::new(DenyAll));
        let command = if cfg!(windows) {
            "echo pwned>pwned.txt & echo x"
        } else {
            "echo pwned > pwned.txt && echo x"
        };
        let err = tool.run(&json!({"command": command})).await.unwrap_err();
        let s = err.to_string();
        assert!(s.contains("blocked"), "{s}");
        assert!(!marker.exists(), "blocked command must not run");
    }

    #[tokio::test]
    async fn unknown_tool_errors() {
        let reg = ToolRegistry::new(vec![Box::new(NowTool)]);
        let err = reg.call("nope", &json!({})).await.unwrap_err();
        assert!(err.to_string().contains("unknown tool"));
    }

    #[test]
    fn read_image_flags_attach() {
        let dir = tempfile::tempdir().unwrap();
        let img = crate::vision::from_rgba(4, 4, vec![0, 255, 0, 255].repeat(16)).unwrap();
        crate::vision::save_jpeg(&dir.path().join("pic.jpg"), &img).unwrap();
        let tool = ReadImageTool::new(dir.path().to_path_buf());
        let out: Value = serde_json::from_str(&tool.call_sync(&json!({"path": "pic.jpg"})).unwrap())
            .unwrap();
        assert_eq!(out["attach_image"], true);
        assert_eq!(out["path"], "pic.jpg");
        assert_eq!(out["width"], 4);
        assert_eq!(out["height"], 4);
    }

    #[test]
    fn read_image_rejects_escape() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = dir.path().join("sub");
        fs::create_dir_all(&workspace).unwrap();
        let tool = ReadImageTool::new(workspace);
        let err = tool.call_sync(&json!({"path": "../x.jpg"})).unwrap_err();
        assert!(err.to_string().contains("escapes") || err.to_string().contains("not found"), "{err}");
    }

    #[tokio::test]
    async fn screenshot_captures_primary_monitor() {
        let dir = tempfile::tempdir().unwrap();
        let tool = ScreenshotTool::new(dir.path().to_path_buf());
        let raw = tool
            .run(&json!({"path": "s.jpg", "target": "monitor"}))
            .await
            .unwrap();
        let out: Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(out["attach_image"], true);
        assert_eq!(out["path"], "s.jpg");
        assert_eq!(out["source"], "monitor");
        assert!(dir.path().join("s.jpg").exists());
        assert!(out["width"].as_u64().unwrap() >= 1);
        assert!(out["height"].as_u64().unwrap() >= 1);
    }

    #[tokio::test]
    async fn screenshot_list_does_not_write_a_file() {
        let dir = tempfile::tempdir().unwrap();
        let tool = ScreenshotTool::new(dir.path().to_path_buf());
        let raw = tool.run(&json!({"list": true})).await.unwrap();
        let out: Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(out["attach_image"], false);
        assert!(out["windows"].is_array(), "{out}");
        assert!(
            !dir.path().join("s.jpg").exists(),
            "list=true must not capture"
        );
    }

    #[tokio::test]
    async fn screenshot_default_names_the_source() {
        let dir = tempfile::tempdir().unwrap();
        let tool = ScreenshotTool::new(dir.path().to_path_buf());
        let raw = tool.run(&json!({"path": "w.jpg"})).await.unwrap();
        let out: Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(out["attach_image"], true);
        let source = out["source"].as_str().unwrap();
        assert!(
            source == "window" || source == "monitor",
            "{out}"
        );
        if source == "window" {
            assert!(out["title"].as_str().unwrap().len() >= 1, "{out}");
            assert!(out["app"].as_str().is_some(), "{out}");
        }
        assert!(dir.path().join("w.jpg").exists());
    }

    #[test]
    fn image_tool_json_writes_jpeg() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("s.jpg");
        let img = crate::vision::from_rgba(2, 2, vec![0, 0, 255, 255].repeat(4)).unwrap();
        let out: Value = serde_json::from_str(&shot_json(&target, "s.jpg", &img, json!({})).unwrap()).unwrap();
        assert_eq!(out["attach_image"], true);
        assert!(target.exists());
        assert!(out["diff"].as_str().unwrap().contains("2x2"), "{out}");
    }
}
