use std::fmt::Write as _;
use std::fs;
use std::future::Future;
use std::path::{Component, Path, PathBuf};
use std::pin::Pin;
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use regex::RegexBuilder;
use serde_json::{json, Value};
use tokio::process::Command;

use crate::diff;
use crate::error::{Error, Result};
use crate::shellguard::{self, CommandReviewer};
use crate::wintrack::{self, WindowHub};

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

const MAX_FILE_BYTES: usize = 256 * 1024;
const MAX_PATTERN_CHARS: usize = 512;
const MAX_READ_MATCHES: usize = 80;
const MAX_READ_MATCHES_CAP: usize = 200;
const MAX_CONTEXT_LINES: usize = 20;
const REGEX_SIZE_LIMIT: usize = 1_048_576;

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
        let text = read_utf8_capped(&resolved)?;
        let file = parse_text_file(&text);
        let n = file.lines.len();
        let start = optional_usize(args, "start_line")?.unwrap_or(1);
        let end = optional_usize(args, "end_line")?.unwrap_or(n.max(1));
        if start < 1 {
            return Err(Error::Tool("start_line is 1-based".into()));
        }
        if end < start {
            return Err(Error::Tool("end_line must be >= start_line".into()));
        }
        if n == 0 {
            if start > 1 {
                return Err(Error::Tool("file is empty".into()));
            }
        } else if start > n {
            return Err(Error::Tool(format!(
                "start_line {start} is past end of file ({n} lines)"
            )));
        }
        let slice_end = if n == 0 { 0 } else { end.min(n) };
        let rel = path.replace('\\', "/");
        let pattern = args.get("pattern").and_then(Value::as_str).unwrap_or("");
        if pattern.is_empty() {
            return Ok(numbered_block(&rel, &file, start, slice_end));
        }
        let re = compile_regex(pattern)?;
        let context = optional_usize(args, "context")?.unwrap_or(0).min(MAX_CONTEXT_LINES);
        let max_matches = optional_usize(args, "max_matches")?
            .unwrap_or(MAX_READ_MATCHES)
            .clamp(1, MAX_READ_MATCHES_CAP);
        let from = start.saturating_sub(1);
        let to = slice_end;
        let mut matches = Vec::new();
        let mut numbered = String::new();
        let mut total = 0usize;
        let width = n.max(1).to_string().len();
        for i in from..to {
            let line = &file.lines[i];
            if !re.is_match(line) {
                continue;
            }
            total += 1;
            if matches.len() >= max_matches {
                continue;
            }
            let mut item = json!({
                "line": i + 1,
                "text": line,
            });
            if context > 0 {
                let ctx_from = i.saturating_sub(context);
                let ctx_to = (i + 1).saturating_add(context).min(file.lines.len());
                item["before"] = json!(file.lines[ctx_from..i]);
                item["after"] = json!(file.lines[i + 1..ctx_to]);
            }
            matches.push(item);
            let _ = writeln!(numbered, "{:>width$}|{line}", i + 1);
        }
        Ok(json!({
            "path": rel,
            "pattern": pattern,
            "matches": matches,
            "count": total,
            "truncated": total > matches.len(),
            "numbered": numbered
        })
        .to_string())
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

fn read_utf8_capped(path: &Path) -> Result<String> {
    let bytes = fs::read(path)?;
    if bytes.len() > MAX_FILE_BYTES {
        return Err(Error::Tool("file larger than 256KiB".into()));
    }
    String::from_utf8(bytes).map_err(|_| Error::Tool("file is not valid UTF-8".into()))
}

fn compile_regex(pattern: &str) -> Result<regex::Regex> {
    if pattern.is_empty() {
        return Err(Error::Tool("pattern is empty".into()));
    }
    if pattern.chars().count() > MAX_PATTERN_CHARS {
        return Err(Error::Tool(format!(
            "pattern longer than {MAX_PATTERN_CHARS} characters"
        )));
    }
    RegexBuilder::new(pattern)
        .size_limit(REGEX_SIZE_LIMIT)
        .dfa_size_limit(REGEX_SIZE_LIMIT)
        .nest_limit(32)
        .build()
        .map_err(|e| Error::Tool(format!("invalid regex: {e}")))
}

fn optional_usize(args: &Value, key: &str) -> Result<Option<usize>> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(v) => parse_usize(v, key).map(Some),
    }
}

fn parse_usize(v: &Value, key: &str) -> Result<usize> {
    let n = match v {
        Value::Number(n) => n
            .as_u64()
            .or_else(|| {
                n.as_f64()
                    .filter(|f| *f >= 0.0 && f.fract() == 0.0 && *f <= usize::MAX as f64)
                    .map(|f| f as u64)
            })
            .ok_or_else(|| Error::Tool(format!("{key} must be a non-negative integer")))?,
        Value::String(s) => s
            .trim()
            .parse::<u64>()
            .map_err(|_| Error::Tool(format!("{key} must be a non-negative integer")))?,
        _ => return Err(Error::Tool(format!("{key} must be a non-negative integer"))),
    };
    usize::try_from(n).map_err(|_| Error::Tool(format!("{key} is too large")))
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct TextFile {
    lines: Vec<String>,
    newline: &'static str,
    trailing_nl: bool,
}

fn parse_text_file(s: &str) -> TextFile {
    let newline = if s.contains("\r\n") { "\r\n" } else { "\n" };
    let trailing_nl = s.ends_with('\n');
    let mut body = if trailing_nl {
        s.strip_suffix('\n').unwrap_or(s)
    } else {
        s
    };
    if newline == "\r\n" {
        body = body.strip_suffix('\r').unwrap_or(body);
    }
    let lines = if s.is_empty() {
        Vec::new()
    } else {
        body.split(newline).map(|l| l.to_string()).collect()
    };
    TextFile {
        lines,
        newline,
        trailing_nl,
    }
}

fn join_text_file(file: &TextFile) -> String {
    if file.lines.is_empty() {
        return if file.trailing_nl {
            file.newline.to_string()
        } else {
            String::new()
        };
    }
    let mut out = file.lines.join(file.newline);
    if file.trailing_nl {
        out.push_str(file.newline);
    }
    out
}

fn content_lines(s: &str) -> Vec<String> {
    if s.is_empty() {
        return Vec::new();
    }
    let newline = if s.contains("\r\n") { "\r\n" } else { "\n" };
    let mut body = if s.ends_with('\n') {
        s.strip_suffix('\n').unwrap_or(s)
    } else {
        s
    };
    if newline == "\r\n" {
        body = body.strip_suffix('\r').unwrap_or(body);
    }
    body.split(newline).map(|l| l.to_string()).collect()
}

fn replace_line_range(file: &mut TextFile, start: usize, end: usize, contents: &str) -> Result<()> {
    if start < 1 {
        return Err(Error::Tool("line is 1-based".into()));
    }
    if end < start {
        return Err(Error::Tool("end_line must be >= line".into()));
    }
    let n = file.lines.len();
    if start > n + 1 {
        return Err(Error::Tool(format!(
            "line {start} is past end of file ({n} lines)"
        )));
    }
    let new_lines = content_lines(contents);
    if start == n + 1 {
        if end != start {
            return Err(Error::Tool("cannot replace a range past end of file".into()));
        }
        file.lines.extend(new_lines);
        if contents.ends_with('\n') {
            file.trailing_nl = true;
        }
        return Ok(());
    }
    if end > n {
        return Err(Error::Tool(format!(
            "end_line {end} is past end of file ({n} lines)"
        )));
    }
    file.lines.splice(start - 1..end, new_lines);
    Ok(())
}

fn numbered_block(path: &str, file: &TextFile, start: usize, end: usize) -> String {
    let n = file.lines.len();
    let mut out = String::new();
    if n == 0 {
        let _ = writeln!(out, "{path} (0 lines)");
        out.push_str("(empty file)\n");
        return out;
    }
    let start = start.max(1);
    let end = end.min(n);
    if start == 1 && end == n {
        let _ = writeln!(out, "{path} ({n} lines)");
    } else {
        let _ = writeln!(out, "{path} lines {start}-{end} of {n}");
    }
    let width = n.to_string().len();
    for i in start - 1..end {
        let _ = writeln!(out, "{:>width$}|{}", i + 1, file.lines[i]);
    }
    out
}

impl ClientTool for ReadFileTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "read_file".into(),
            description: "Read a UTF-8 text file. Every returned line is prefixed with its 1-based line number (N|text) so later write_file line=N edits match. Optional pattern is a regex/keyword (only matching lines). Optional start_line/end_line slice the file.".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string", "description": "Relative path inside the workspace"},
                    "pattern": {"type": "string", "description": "Regex (or keyword) to keep. Omit to return the numbered file (or slice)."},
                    "start_line": {"type": "integer", "description": "1-based first line to return. Default 1."},
                    "end_line": {"type": "integer", "description": "1-based last line to return. Default is the last line."},
                    "context": {"type": "integer", "description": "With pattern: extra lines before/after each match. Default 0."},
                    "max_matches": {"type": "integer", "description": "With pattern: cap returned matches. Default 80."}
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
        if contents.len() > MAX_FILE_BYTES {
            return Err(Error::Tool("contents larger than 256KiB".into()));
        }
        let line = optional_usize(args, "line")?;
        let end_line = optional_usize(args, "end_line")?;
        let pattern = args
            .get("pattern")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty());
        if line.is_some() && pattern.is_some() {
            return Err(Error::Tool("use either line or pattern, not both".into()));
        }
        if end_line.is_some() && line.is_none() {
            return Err(Error::Tool("end_line requires line".into()));
        }
        let target = resolve_target_in_workspace(&self.workspace, path)?;
        let rel = path.replace('\\', "/");
        if let Some(start) = line {
            let before = read_utf8_capped(&target).map_err(|e| {
                if target.exists() {
                    e
                } else {
                    Error::Tool(format!("file not found: {path}"))
                }
            })?;
            let mut file = parse_text_file(&before);
            replace_line_range(
                &mut file,
                start,
                end_line.unwrap_or(start),
                contents,
            )?;
            let after = join_text_file(&file);
            fs::write(&target, &after)?;
            return Ok(diff::file_change_json(&rel, Some(&before), Some(&after)).to_string());
        }
        if let Some(pat) = pattern {
            let before = read_utf8_capped(&target).map_err(|e| {
                if target.exists() {
                    e
                } else {
                    Error::Tool(format!("file not found: {path}"))
                }
            })?;
            let re = compile_regex(pat)?;
            let replacements = re.find_iter(&before).count();
            let after = re.replace_all(&before, contents).into_owned();
            fs::write(&target, &after)?;
            let mut out = diff::file_change_json(&rel, Some(&before), Some(&after));
            out["replacements"] = json!(replacements);
            return Ok(out.to_string());
        }
        let before = fs::read_to_string(&target).ok();
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(&target, contents)?;
        Ok(diff::file_change_json(&rel, before.as_deref(), Some(contents)).to_string())
    }
}

impl ClientTool for WriteFileTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "write_file".into(),
            description: "Create, overwrite, or edit a UTF-8 file. Set line/end_line (1-based, from read_file) to replace that range with contents (may be multiple lines), or pattern for regex replace-all ($1 captures). Omit both to overwrite the whole file. Result JSON includes a git unified diff of before vs after.".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string", "description": "Relative path inside the workspace"},
                    "contents": {"type": "string", "description": "Full file, replacement lines, or regex replacement. Newlines write multiple lines."},
                    "line": {"type": "integer", "description": "1-based start line to replace. Omit with pattern for regex; omit both to overwrite the whole file."},
                    "end_line": {"type": "integer", "description": "1-based inclusive end line. Default is line."},
                    "pattern": {"type": "string", "description": "Regex to replace everywhere in the file. Do not combine with line."}
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
    windows: Option<Arc<WindowHub>>,
}

impl RunCommandTool {
    pub fn new(workspace: PathBuf) -> Self {
        Self {
            workspace,
            guard: None,
            windows: None,
        }
    }

    pub fn with_guard(workspace: PathBuf, guard: Arc<dyn CommandReviewer>) -> Self {
        Self {
            workspace,
            guard: Some(guard),
            windows: None,
        }
    }

    pub fn with_windows(mut self, windows: Arc<WindowHub>) -> Self {
        self.windows = Some(windows);
        self
    }

    async fn run(&self, args: &Value) -> Result<String> {
        let command = args
            .get("command")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| Error::Tool("command is required".into()))?;
        let cwd = args.get("cwd").and_then(Value::as_str);
        let window = args.get("window").and_then(Value::as_str);
        run_workspace_command_ex(
            &self.workspace,
            command,
            cwd,
            self.guard.as_ref(),
            window,
            self.windows.clone(),
        )
        .await
    }
}

/// Workspace shell used by `run_command` and by `timer` when a command fires.
pub(crate) async fn run_workspace_command(
    workspace: &Path,
    command: &str,
    cwd: Option<&str>,
    guard: Option<&Arc<dyn CommandReviewer>>,
) -> Result<String> {
    run_workspace_command_ex(workspace, command, cwd, guard, None, None).await
}

pub(crate) async fn run_workspace_command_ex(
    workspace: &Path,
    command: &str,
    cwd: Option<&str>,
    guard: Option<&Arc<dyn CommandReviewer>>,
    window: Option<&str>,
    windows: Option<Arc<WindowHub>>,
) -> Result<String> {
    let command = command.trim();
    if command.is_empty() {
        return Err(Error::Tool("command is required".into()));
    }
    let cwd = match cwd.map(str::trim).filter(|s| !s.is_empty() && *s != ".") {
        Some(p) => resolve_in_workspace(workspace, p)?,
        None => workspace
            .canonicalize()
            .map_err(|e| Error::Tool(format!("workspace not found: {e}")))?,
    };
    if !cwd.is_dir() {
        return Err(Error::Tool("cwd is not a directory".into()));
    }
    shellguard::enforce(guard, command, &cwd.to_string_lossy()).await?;
    let window = wintrack::optional_name(window)?;

    let mut cmd = shell_command(command);
    cmd.current_dir(&cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    hide_window(&mut cmd);

    let before = if window.is_some() {
        wintrack::snapshot().await
    } else {
        Default::default()
    };
    let child = cmd
        .spawn()
        .map_err(|e| Error::Tool(format!("spawn failed: {e}")))?;
    let watch = async {
        let Some(label) = window.as_deref() else {
            return Vec::new();
        };
        let found = wintrack::watch(before).await;
        match &windows {
            Some(hub) => wintrack::bind_appeared(hub, label, &found),
            None => wintrack::label_appeared(label, &found),
        }
    };
    let timed = tokio::time::timeout(Duration::from_secs(60), child.wait_with_output());
    let (timed, bound) = tokio::join!(timed, watch);
    let (timed_out, output) = match timed {
        Ok(Ok(o)) => (false, o),
        Ok(Err(e)) => return Err(Error::Tool(format!("command failed: {e}"))),
        Err(_) => {
            let mut body = json!({
                "timed_out": true,
                "exit_code": null,
                "stdout": "",
                "stderr": "timed out after 60s",
                "files": []
            });
            if window.is_some() {
                wintrack::attach_to(&mut body, &bound);
            }
            return Ok(body.to_string());
        }
    };
    let stdout = clip_text(&String::from_utf8_lossy(&output.stdout), 32 * 1024);
    let stderr = clip_text(&String::from_utf8_lossy(&output.stderr), 16 * 1024);
    let git_cwd = cwd.clone();
    let files = tokio::task::spawn_blocking(move || diff::git_file_changes(&git_cwd))
        .await
        .unwrap_or_else(|_| Vec::new());
    let mut body = json!({
        "timed_out": timed_out,
        "exit_code": output.status.code(),
        "stdout": stdout,
        "stderr": stderr,
        "files": files
    });
    if window.is_some() {
        wintrack::attach_to(&mut body, &bound);
    }
    Ok(body.to_string())
}

impl ClientTool for RunCommandTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "run_command".into(),
            description: "Run a shell command with cwd in the workspace (Windows cmd / Unix sh). Compound or nested commands are reviewed against this OS shell's quoting rules before they run. Returns stdout, stderr, exit_code, and git-style file diffs when the workspace is a git repo. If the command will open a GUI, set window to a short label; the result includes that window's pid so screenshot can target it.".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "command": {"type": "string", "description": "Shell command to run"},
                    "cwd": {"type": "string", "description": "Optional subdirectory inside the workspace"},
                    "window": {"type": "string", "description": "If this command opens a GUI, a short name (ascii, dash, underscore). Result includes windows[].pid; pass the same name to screenshot."}
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
    windows: Option<Arc<WindowHub>>,
}

impl ScreenshotTool {
    pub fn new(workspace: PathBuf) -> Self {
        Self {
            workspace,
            spawned: Arc::new(Vec::new),
            windows: None,
        }
    }

    pub fn with_spawned(
        workspace: PathBuf,
        spawned: Arc<dyn Fn() -> Vec<u32> + Send + Sync>,
    ) -> Self {
        Self {
            workspace,
            spawned,
            windows: None,
        }
    }

    pub fn with_windows(mut self, windows: Arc<WindowHub>) -> Self {
        self.windows = Some(windows);
        self
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
        let (want_pid, bound_name) = resolve_shot_pid(args, self.windows.as_deref())?;
        let names = self.windows.as_ref().map(|h| h.pid_names());
        let spawned = (self.spawned)();
        let hint = self.workspace_hint();
        let self_pid = std::process::id();

        if list_only {
            return tokio::task::spawn_blocking(move || {
                list_windows_json(
                    &spawned,
                    self_pid,
                    hint.as_deref(),
                    title.as_deref(),
                    app.as_deref(),
                    want_pid,
                    names.as_ref(),
                )
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
                    want_pid,
                    bound_name.as_deref(),
                    &spawned,
                    self_pid,
                    hint.as_deref(),
                    names.as_ref(),
                )
            }
        })
        .await
        .map_err(|e| Error::Tool(format!("screenshot join: {e}")))?
    }
}

fn resolve_shot_pid(
    args: &Value,
    windows: Option<&WindowHub>,
) -> Result<(Option<u32>, Option<String>)> {
    if let Some(name) = args
        .get("name")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        let rec = windows
            .and_then(|h| h.get(name))
            .ok_or_else(|| {
                Error::Tool(format!(
                    "no window named {name}; pass window={name} when launching the GUI, or screenshot list=true"
                ))
            })?;
        return Ok((Some(rec.pid), Some(name.to_string())));
    }
    let pid = args
        .get("pid")
        .and_then(Value::as_u64)
        .map(|n| n as u32)
        .filter(|p| *p > 0);
    Ok((pid, None))
}

fn list_windows_json(
    spawned: &[u32],
    self_pid: u32,
    hint: Option<&str>,
    title: Option<&str>,
    app: Option<&str>,
    pid: Option<u32>,
    names: Option<&std::collections::HashMap<u32, String>>,
) -> Result<String> {
    let listed = crate::shot::collect_windows()?;
    let metas: Vec<_> = listed.iter().map(|(_, m)| m.clone()).collect();
    let filter = crate::shot::ShotFilter {
        title,
        app,
        pid,
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
            let mut row = json!({
                "title": m.title,
                "app": m.app,
                "pid": m.pid,
                "width": m.width,
                "height": m.height
            });
            if let Some(name) = names.and_then(|n| n.get(&m.pid)) {
                row["name"] = json!(name);
            }
            row
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
    pid: Option<u32>,
    bound_name: Option<&str>,
    spawned: &[u32],
    self_pid: u32,
    hint: Option<&str>,
    names: Option<&std::collections::HashMap<u32, String>>,
) -> Result<String> {
    let listed = crate::shot::collect_windows()?;
    let metas: Vec<_> = listed.iter().map(|(_, m)| m.clone()).collect();
    let filter = crate::shot::ShotFilter {
        title,
        app,
        pid,
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
        let mut extra = json!({
            "source": "window",
            "title": meta.title,
            "app": meta.app,
            "pid": meta.pid
        });
        if let Some(name) = bound_name.or_else(|| names.and_then(|n| n.get(&meta.pid)).map(|s| s.as_str()))
        {
            extra["name"] = json!(name);
        }
        return shot_json(target, rel, &img, extra);
    }
    let mut extra = json!({
        "source": "monitor",
        "note": "no matching window; captured primary display. Pass name (from window=), pid, title/app, or list=true."
    });
    if let Some(name) = bound_name {
        extra["name"] = json!(name);
    }
    if let Some(pid) = pid {
        extra["wanted_pid"] = json!(pid);
    }
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
            description: "Capture the GUI window you opened to a JPEG and attach the pixels on the next turn. Prefer name from the window= label you set when launching, or pid from that result. Does not capture the IDE or the grokaagent terminal unless you ask for it. Optional title/app substring; target=monitor for the whole primary display; list=true returns windows (with bound names) without capturing. Default path is .groka/shots/<timestamp>.jpg.".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string", "description": "Relative JPEG path inside the workspace"},
                    "name": {"type": "string", "description": "Window label you set with window= on run_command / run_background"},
                    "pid": {"type": "integer", "description": "Exact process id of the window to capture"},
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
        let (width, height) = (img.width(), img.height());
        if crate::vision::below_vision_min(width, height) {
            return Ok(json!({
                "path": path.replace('\\', "/"),
                "attach_image": false,
                "width": width,
                "height": height,
                "pixels": width.saturating_mul(height),
                "error": crate::vision::enlarge_yourself_note(width, height)
            })
            .to_string());
        }
        Ok(json!({
            "path": path.replace('\\', "/"),
            "attach_image": true,
            "mime": "image/jpeg",
            "width": width,
            "height": height
        })
        .to_string())
    }
}

impl ClientTool for ReadImageTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "read_image".into(),
            description: "Load a PNG or JPEG from the workspace and attach the pixels so you can see the image on the next turn. xAI rejects images under 512 total pixels (for example 16x16); those are not attached — enlarge or regenerate the file, then call again.".into(),
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
        assert!(out.contains("note.txt (1 lines)"), "{out}");
        assert!(out.contains("1|hello-agent"), "{out}");
        assert!(!out.starts_with("hello-agent"), "{out}");
    }

    #[test]
    fn read_file_slice_keeps_original_line_numbers() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("a.txt"), "a\nb\nc\nd\n").unwrap();
        let tool = ReadFileTool::new(dir.path().to_path_buf());
        let out = tool
            .call_sync(&json!({"path": "a.txt", "start_line": 2, "end_line": 3}))
            .unwrap();
        assert!(out.contains("lines 2-3 of 4"), "{out}");
        assert!(out.contains("2|b"), "{out}");
        assert!(out.contains("3|c"), "{out}");
        assert!(!out.contains("1|a"), "{out}");
        assert!(!out.contains("4|d"), "{out}");
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
        let diff = out["diff"].as_str().unwrap();
        assert!(diff.contains("--- a/src/a.txt"), "{out}");
        assert!(diff.contains("+++ b/src/a.txt"), "{out}");
        assert!(diff.contains("+hello"), "{out}");
        assert_eq!(out["plus"], 1);
        assert_eq!(out["unchanged"], false);
        assert_eq!(fs::read_to_string(dir.path().join("src/a.txt")).unwrap(), "hello\n");
    }

    #[test]
    fn read_file_pattern_returns_matching_lines_with_numbers() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("src.rs"),
            "fn a() {}\nTODO: one\nfn b() {}\n// TODO two\n",
        )
        .unwrap();
        let tool = ReadFileTool::new(dir.path().to_path_buf());
        let out: Value = serde_json::from_str(
            &tool
                .call_sync(&json!({"path": "src.rs", "pattern": "TODO"}))
                .unwrap(),
        )
        .unwrap();
        assert_eq!(out["count"], 2);
        assert_eq!(out["truncated"], false);
        let matches = out["matches"].as_array().unwrap();
        assert_eq!(matches[0]["line"], 2);
        assert_eq!(matches[0]["text"], "TODO: one");
        assert_eq!(matches[1]["line"], 4);
        assert_eq!(matches[1]["text"], "// TODO two");
        let numbered = out["numbered"].as_str().unwrap();
        assert!(numbered.contains("2|TODO: one"), "{numbered}");
        assert!(numbered.contains("4|// TODO two"), "{numbered}");
    }

    #[test]
    fn read_file_pattern_is_regex_and_can_add_context() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("a.txt"), "alpha\nkeep-me-12\nomega\n").unwrap();
        let tool = ReadFileTool::new(dir.path().to_path_buf());
        let out: Value = serde_json::from_str(
            &tool
                .call_sync(&json!({
                    "path": "a.txt",
                    "pattern": r"keep-me-\d+",
                    "context": 1
                }))
                .unwrap(),
        )
        .unwrap();
        let m = &out["matches"][0];
        assert_eq!(m["line"], 2);
        assert_eq!(m["before"], json!(["alpha"]));
        assert_eq!(m["after"], json!(["omega"]));
    }

    #[test]
    fn read_file_rejects_invalid_regex() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("a.txt"), "x").unwrap();
        let tool = ReadFileTool::new(dir.path().to_path_buf());
        let err = tool
            .call_sync(&json!({"path": "a.txt", "pattern": "["}))
            .unwrap_err();
        assert!(err.to_string().contains("invalid regex"), "{err}");
    }

    #[test]
    fn write_file_replaces_one_line_with_several() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("a.txt"), "one\ntwo\nthree\n").unwrap();
        let tool = WriteFileTool::new(dir.path().to_path_buf());
        let out: Value = serde_json::from_str(
            &tool
                .call_sync(&json!({
                    "path": "a.txt",
                    "line": 2,
                    "contents": "TWO-A\nTWO-B\n"
                }))
                .unwrap(),
        )
        .unwrap();
        assert_eq!(out["kind"], "modify");
        let diff = out["diff"].as_str().unwrap();
        assert!(diff.contains("@@"), "{diff}");
        assert!(diff.contains("-two"), "{diff}");
        assert!(diff.contains("+TWO-A"), "{diff}");
        assert!(diff.contains("+TWO-B"), "{diff}");
        assert_eq!(
            fs::read_to_string(dir.path().join("a.txt")).unwrap(),
            "one\nTWO-A\nTWO-B\nthree\n"
        );
    }

    #[test]
    fn write_file_replaces_a_line_range() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("a.txt"), "a\nb\nc\nd\n").unwrap();
        let tool = WriteFileTool::new(dir.path().to_path_buf());
        tool.call_sync(&json!({
            "path": "a.txt",
            "line": 2,
            "end_line": 3,
            "contents": "X\nY\nZ"
        }))
        .unwrap();
        assert_eq!(
            fs::read_to_string(dir.path().join("a.txt")).unwrap(),
            "a\nX\nY\nZ\nd\n"
        );
    }

    #[test]
    fn write_file_appends_when_line_is_len_plus_one() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("a.txt"), "a\nb\n").unwrap();
        let tool = WriteFileTool::new(dir.path().to_path_buf());
        tool.call_sync(&json!({
            "path": "a.txt",
            "line": 3,
            "contents": "c\nd"
        }))
        .unwrap();
        assert_eq!(
            fs::read_to_string(dir.path().join("a.txt")).unwrap(),
            "a\nb\nc\nd\n"
        );
    }

    #[test]
    fn write_file_regex_replaces_every_match() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("a.txt"), "foo 1\nbar\nfoo 2\n").unwrap();
        let tool = WriteFileTool::new(dir.path().to_path_buf());
        let out: Value = serde_json::from_str(
            &tool
                .call_sync(&json!({
                    "path": "a.txt",
                    "pattern": r"foo (\d+)",
                    "contents": "FOO-$1"
                }))
                .unwrap(),
        )
        .unwrap();
        assert_eq!(out["replacements"], 2);
        assert_eq!(
            fs::read_to_string(dir.path().join("a.txt")).unwrap(),
            "FOO-1\nbar\nFOO-2\n"
        );
    }

    #[test]
    fn write_file_rejects_line_and_pattern_together() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("a.txt"), "x\n").unwrap();
        let tool = WriteFileTool::new(dir.path().to_path_buf());
        let err = tool
            .call_sync(&json!({
                "path": "a.txt",
                "line": 1,
                "pattern": "x",
                "contents": "y"
            }))
            .unwrap_err();
        assert!(err.to_string().contains("either line or pattern"), "{err}");
    }

    #[test]
    fn write_file_line_out_of_range_errors() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("a.txt"), "only\n").unwrap();
        let tool = WriteFileTool::new(dir.path().to_path_buf());
        let err = tool
            .call_sync(&json!({"path": "a.txt", "line": 5, "contents": "no"}))
            .unwrap_err();
        assert!(err.to_string().contains("past end"), "{err}");
    }

    #[test]
    fn parse_join_roundtrip_keeps_crlf() {
        let raw = "a\r\nb\r\n";
        assert_eq!(join_text_file(&parse_text_file(raw)), raw);
        assert_eq!(join_text_file(&parse_text_file("a\nb")), "a\nb");
        assert_eq!(join_text_file(&parse_text_file("")), "");
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
    fn read_image_tiny_file_asks_the_model_to_enlarge() {
        let dir = tempfile::tempdir().unwrap();
        let img = crate::vision::from_rgba(4, 4, vec![0, 255, 0, 255].repeat(16)).unwrap();
        crate::vision::save_jpeg(&dir.path().join("pic.jpg"), &img).unwrap();
        let tool = ReadImageTool::new(dir.path().to_path_buf());
        let out: Value = serde_json::from_str(&tool.call_sync(&json!({"path": "pic.jpg"})).unwrap())
            .unwrap();
        assert_eq!(out["attach_image"], false);
        assert_eq!(out["width"], 4);
        assert_eq!(out["height"], 4);
        let err = out["error"].as_str().unwrap();
        assert!(err.contains("512"), "{err}");
        assert!(err.contains("4x4"), "{err}");
    }

    #[test]
    fn read_image_flags_attach() {
        let dir = tempfile::tempdir().unwrap();
        let img = crate::vision::from_rgba(32, 32, vec![0, 255, 0, 255].repeat(32 * 32)).unwrap();
        crate::vision::save_jpeg(&dir.path().join("pic.jpg"), &img).unwrap();
        let tool = ReadImageTool::new(dir.path().to_path_buf());
        let out: Value = serde_json::from_str(&tool.call_sync(&json!({"path": "pic.jpg"})).unwrap())
            .unwrap();
        assert_eq!(out["attach_image"], true);
        assert_eq!(out["path"], "pic.jpg");
        assert_eq!(out["width"], 32);
        assert_eq!(out["height"], 32);
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

    #[tokio::test]
    async fn run_command_rejects_bad_window_name() {
        let dir = tempfile::tempdir().unwrap();
        let tool = RunCommandTool::new(dir.path().to_path_buf());
        let err = tool
            .run(&json!({"command": "echo x", "window": "has space"}))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("window name"), "{err}");
    }

    #[tokio::test]
    async fn screenshot_unknown_name_errors_without_capturing() {
        let dir = tempfile::tempdir().unwrap();
        let tool = ScreenshotTool::new(dir.path().to_path_buf());
        let err = tool
            .run(&json!({"name": "preview", "path": "s.jpg"}))
            .await
            .unwrap_err();
        assert!(
            err.to_string().contains("no window named preview"),
            "{err}"
        );
        assert!(!dir.path().join("s.jpg").exists());
    }
}
