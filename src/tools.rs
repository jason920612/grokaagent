use std::collections::HashMap;
use std::fmt::Write as _;
use std::fs;
use std::future::Future;
use std::path::{Component, Path, PathBuf};
use std::pin::Pin;
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use md5::{Digest, Md5};
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
    /// A tool with a gate is offered only while `server_tools` carries that
    /// marker (it follows the live model's route and the search toggle).
    fn gate(&self) -> Option<&'static str> {
        None
    }
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

    /// Specs to send this turn: gated tools only when `server_tools` opens them.
    pub fn specs_for(&self, server_tools: &[String]) -> Vec<ToolSpec> {
        self.tools
            .iter()
            .filter(|t| t.gate().map_or(true, |g| server_tools.iter().any(|s| s == g)))
            .map(|t| t.spec())
            .collect()
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

pub(crate) fn hide_window(cmd: &mut Command) {
    #[cfg(windows)]
    {
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    let _ = cmd;
}

/// The process for `command` in `shell` (bash by default; see [`crate::shellrt`]).
pub(crate) fn shell_command_in(shell: crate::shellrt::Shell, command: &str) -> Result<Command> {
    crate::shellrt::command(shell, command)
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

fn md5_hex(bytes: &[u8]) -> String {
    format!("{:x}", Md5::digest(bytes))
}

/// The last content each file had when this agent saw it (read_file, or its
/// own write_file / edit_file). edit_file uses it to decide how strict to be.
pub struct FileViewStore {
    inner: Mutex<HashMap<PathBuf, String>>,
}

impl FileViewStore {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            inner: Mutex::new(HashMap::new()),
        })
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<PathBuf, String>> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn key(path: &Path) -> PathBuf {
        path.canonicalize().unwrap_or_else(|_| path.to_path_buf())
    }

    pub fn remember(&self, path: &Path, bytes: &[u8]) {
        self.lock().insert(Self::key(path), md5_hex(bytes));
    }

    pub fn view(&self, path: &Path, bytes: &[u8]) -> View {
        match self.lock().get(&Self::key(path)) {
            None => View::Never,
            Some(seen) if *seen != md5_hex(bytes) => View::Stale,
            Some(_) => View::Fresh,
        }
    }
}

/// How the file on disk relates to what the agent last saw of it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum View {
    Never,
    /// Changed since (by a command, another agent, or the user).
    Stale,
    Fresh,
}

pub(crate) fn read_text_at(root: &Path, args: &Value) -> Result<String> {
    read_text_at_tracked(root, args, None)
}

fn read_text_at_tracked(
    root: &Path,
    args: &Value,
    views: Option<&FileViewStore>,
) -> Result<String> {
    let path = args
        .get("path")
        .and_then(Value::as_str)
        .ok_or_else(|| Error::Tool("path is required".into()))?;
    let resolved = resolve_in_workspace(root, path)?;
    let text = read_utf8_capped(&resolved)?;
    if let Some(v) = views {
        v.remember(&resolved, text.as_bytes());
    }
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

pub(crate) fn write_text_at(root: &Path, args: &Value) -> Result<String> {
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
    let target = resolve_target_in_workspace(root, path)?;
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

pub struct ReadFileTool {
    workspace: PathBuf,
    views: Arc<FileViewStore>,
}

impl ReadFileTool {
    pub fn new(workspace: PathBuf) -> Self {
        Self::with_views(workspace, FileViewStore::new())
    }

    pub fn with_views(workspace: PathBuf, views: Arc<FileViewStore>) -> Self {
        Self { workspace, views }
    }

    pub fn call_sync(&self, args: &Value) -> Result<String> {
        read_text_at_tracked(&self.workspace, args, Some(&self.views))
    }
}

pub fn resolve_in_workspace(workspace: &Path, requested: &str) -> Result<PathBuf> {
    let target = resolve_target_in_workspace(workspace, requested)?;
    let canonical = target
        .canonicalize()
        .map_err(|_| Error::Tool(format!("file not found: {requested}")))?;
    let workspace_c = workspace
        .canonicalize()
        .map_err(|e| Error::Tool(format!("workspace not found: {e}")))?;
    if !canonical.starts_with(&workspace_c) && !crate::grants::within(&canonical, &crate::grants::roots(workspace)) {
        return Err(Error::Tool(escape_message(requested)));
    }
    Ok(canonical)
}

fn escape_message(requested: &str) -> String {
    format!(
        "path escapes workspace: {requested} is outside it. If the task needs that folder, ask the user with request_workspace_access (give the reason)."
    )
}

/// A path outside the workspace that lands in a folder the user granted.
fn granted_target(workspace: &Path, requested: &str) -> Option<PathBuf> {
    let roots = crate::grants::roots(workspace);
    if roots.is_empty() {
        return None;
    }
    let joined = if Path::new(requested).is_absolute() { PathBuf::from(requested) } else { workspace.join(requested) };
    // Resolve `..` lexically, then canonicalize what exists (symlinks).
    let mut lexical = PathBuf::new();
    for c in joined.components() {
        match c {
            Component::ParentDir => {
                lexical.pop();
            }
            Component::CurDir => {}
            other => lexical.push(other.as_os_str()),
        }
    }
    // A dangling symlink fails to canonicalize: not a granted target.
    let resolved = if entry_exists(&lexical) {
        lexical.canonicalize().ok()?
    } else {
        let parent = lexical.parent()?.canonicalize().ok()?;
        parent.join(lexical.file_name()?)
    };
    crate::grants::within(&resolved, &roots).then_some(resolved)
}

/// Resolve a path that may not exist yet (write/create).
///
/// Existing path components are canonicalized so a symlink/junction that points
/// outside the workspace cannot be used as a write escape hatch.
pub fn resolve_target_in_workspace(workspace: &Path, requested: &str) -> Result<PathBuf> {
    if requested.is_empty() {
        return Err(Error::Tool("path is required".into()));
    }
    if let Some(p) = granted_target(workspace, requested) {
        return Ok(p);
    }
    let workspace = workspace
        .canonicalize()
        .map_err(|e| Error::Tool(format!("workspace not found: {e}")))?;
    if Path::new(requested).is_absolute() {
        let joined = PathBuf::from(requested);
        let (Some(parent), Some(name)) = (joined.parent(), joined.file_name()) else {
            return Err(Error::Tool(escape_message(requested)));
        };
        if !parent.exists() {
            return Err(Error::Tool(escape_message(requested)));
        }
        let parent = parent
            .canonicalize()
            .map_err(|_| Error::Tool(format!("path not found: {requested}")))?;
        if !parent.starts_with(&workspace) {
            return Err(Error::Tool(escape_message(requested)));
        }
        // The last component may itself be a symlink out of the workspace.
        let target = parent.join(name);
        if entry_exists(&target) {
            return canonicalize_in_workspace(&workspace, &target);
        }
        return Ok(target);
    }
    let mut out = workspace.clone();
    for c in Path::new(requested).components() {
        match c {
            Component::CurDir => {}
            Component::ParentDir => {
                if !out.pop() || !out.starts_with(&workspace) {
                    return Err(Error::Tool(escape_message(requested)));
                }
                if out.exists() {
                    out = canonicalize_in_workspace(&workspace, &out)?;
                }
            }
            Component::Normal(s) => {
                out.push(s);
                if entry_exists(&out) {
                    out = canonicalize_in_workspace(&workspace, &out)?;
                }
            }
            Component::Prefix(_) | Component::RootDir => {
                return Err(Error::Tool(escape_message(requested)));
            }
        }
    }
    if entry_exists(&out) {
        return canonicalize_in_workspace(&workspace, &out);
    }
    if let Some(parent) = out.parent() {
        if parent.exists() {
            let parent = canonicalize_in_workspace(&workspace, parent)?;
            return Ok(parent.join(out.file_name().unwrap_or_default()));
        }
    }
    if !out.starts_with(&workspace) {
        return Err(Error::Tool(escape_message(requested)));
    }
    Ok(out)
}

fn canonicalize_in_workspace(workspace: &Path, path: &Path) -> Result<PathBuf> {
    let canonical = path.canonicalize().map_err(|_| {
        if path.is_symlink() {
            // Writing would create the link's target, wherever it points.
            Error::Tool(format!("dangling symlink: {} points to a missing target", path.display()))
        } else {
            Error::Tool("path escapes workspace".into())
        }
    })?;
    if !canonical.starts_with(workspace) {
        return Err(Error::Tool("path escapes workspace".into()));
    }
    Ok(canonical)
}

/// Something is at `path`, possibly a dangling symlink (`exists()` follows links).
fn entry_exists(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok()
}

#[cfg(all(test, unix))]
mod target_tests {
    use super::*;
    use std::os::unix::fs::symlink;

    /// `(tempdir, workspace with docs/, a folder outside it)`.
    fn ws_and_outside() -> (tempfile::TempDir, PathBuf, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let ws = dir.path().join("ws");
        let outside = dir.path().join("outside");
        fs::create_dir_all(ws.join("docs")).unwrap();
        fs::create_dir_all(&outside).unwrap();
        (dir, ws, outside)
    }

    #[test]
    fn a_final_symlink_out_of_the_workspace_is_refused() {
        let (_dir, ws, outside) = ws_and_outside();
        fs::write(outside.join("bashrc"), "keep").unwrap();
        symlink(outside.join("bashrc"), ws.join("docs/shot.jpg")).unwrap();
        let abs = ws.join("docs/shot.jpg");
        for req in ["docs/shot.jpg", abs.to_str().unwrap()] {
            let err = resolve_target_in_workspace(&ws, req).unwrap_err().to_string();
            assert!(err.contains("escapes"), "{req}: {err}");
        }
    }

    #[test]
    fn a_dangling_symlink_is_never_written_through() {
        let (_dir, ws, outside) = ws_and_outside();
        symlink(outside.join("new.txt"), ws.join("docs/x.txt")).unwrap();
        symlink(outside.join("nodir"), ws.join("gone")).unwrap();
        let abs = ws.join("docs/x.txt");
        for req in ["docs/x.txt", abs.to_str().unwrap(), "gone/y.txt"] {
            let err = resolve_target_in_workspace(&ws, req).unwrap_err().to_string();
            assert!(err.contains("dangling"), "{req}: {err}");
        }
        let tool = WriteFileTool::new(ws.clone());
        assert!(tool.call_sync(&json!({"path": "docs/x.txt", "contents": "echo pwned"})).is_err());
        assert!(!outside.join("new.txt").exists());
    }

    #[test]
    fn a_dangling_symlink_in_a_granted_folder_is_refused() {
        let (dir, ws, outside) = ws_and_outside();
        let granted = dir.path().join("granted");
        fs::create_dir_all(&granted).unwrap();
        crate::grants::add_session(&ws, &granted);
        assert!(resolve_target_in_workspace(&ws, granted.join("ok.txt").to_str().unwrap()).is_ok());
        symlink(outside.join("new.txt"), granted.join("x.txt")).unwrap();
        assert!(resolve_target_in_workspace(&ws, granted.join("x.txt").to_str().unwrap()).is_err());
    }

    #[test]
    fn a_symlink_inside_the_workspace_resolves_to_its_target() {
        let (_dir, ws, _outside) = ws_and_outside();
        fs::write(ws.join("docs/real.txt"), "x").unwrap();
        symlink(ws.join("docs/real.txt"), ws.join("alias.txt")).unwrap();
        let real = ws.join("docs/real.txt").canonicalize().unwrap();
        let abs = ws.join("alias.txt");
        assert_eq!(resolve_target_in_workspace(&ws, "alias.txt").unwrap(), real);
        assert_eq!(resolve_target_in_workspace(&ws, abs.to_str().unwrap()).unwrap(), real);
    }
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
            description: "Read a UTF-8 text file. Every returned line is prefixed with its 1-based line number (N|text); the prefix is not part of the text. Read a file before edit_file changes it. Optional pattern is a regex/keyword (only matching lines). Optional start_line/end_line slice the file.".into(),
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
    views: Option<Arc<FileViewStore>>,
}

impl WriteFileTool {
    pub fn new(workspace: PathBuf) -> Self {
        Self { workspace, views: None }
    }

    /// The agent knows what it just wrote: edit_file may follow directly.
    pub fn with_views(workspace: PathBuf, views: Arc<FileViewStore>) -> Self {
        Self { workspace, views: Some(views) }
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
        if args.get("line").is_some()
            || args.get("end_line").is_some()
            || args
                .get("pattern")
                .and_then(Value::as_str)
                .is_some_and(|s| !s.is_empty())
            || args.get("diff").is_some()
        {
            return Err(Error::Tool(
                "write_file only creates a new file (path+contents). To change an existing file use edit_file (old_string/new_string, or a unified diff).".into(),
            ));
        }
        let target = resolve_target_in_workspace(&self.workspace, path)?;
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent)?;
        }
        // create_new fails on anything already at the path, a dangling symlink
        // included, so write_file never overwrites or writes through a link.
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&target)
            .map_err(|e| {
                if e.kind() == std::io::ErrorKind::AlreadyExists {
                    Error::Tool(format!(
                        "file exists: {path}. write_file cannot overwrite; use edit_file (old_string/new_string for a replacement, or a unified diff)."
                    ))
                } else {
                    e.into()
                }
            })?;
        std::io::Write::write_all(&mut file, contents.as_bytes())?;
        if let Some(v) = &self.views {
            v.remember(&target, contents.as_bytes());
        }
        let rel = path.replace('\\', "/");
        Ok(diff::file_change_json(&rel, None, Some(contents)).to_string())
    }
}

impl ClientTool for WriteFileTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "write_file".into(),
            description: "Create a NEW UTF-8 file. Fails if the path already exists. For edits use edit_file. Result JSON includes a git unified diff of the created file.".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string", "description": "Relative path inside the workspace"},
                    "contents": {"type": "string", "description": "Full file contents to create. Newlines write multiple lines."}
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

pub struct EditFileTool {
    workspace: PathBuf,
    views: Arc<FileViewStore>,
}

impl EditFileTool {
    pub fn new(workspace: PathBuf, views: Arc<FileViewStore>) -> Self {
        Self { workspace, views }
    }

    pub fn call_sync(&self, args: &Value) -> Result<String> {
        let path = args
            .get("path")
            .and_then(Value::as_str)
            .ok_or_else(|| Error::Tool("path is required".into()))?;
        let patch = args.get("diff").and_then(Value::as_str).filter(|s| !s.trim().is_empty());
        let old = args.get("old_string").and_then(Value::as_str);
        let new = args.get("new_string").and_then(Value::as_str);
        let replace_all = args.get("replace_all").and_then(Value::as_bool).unwrap_or(false);
        match (patch, old) {
            (Some(_), Some(_)) => {
                return Err(Error::Tool("give either diff or old_string/new_string, not both".into()));
            }
            (None, None) => {
                return Err(Error::Tool(
                    "edit_file needs old_string + new_string (exact replacement) or diff (unified diff with @@ hunks)".into(),
                ));
            }
            (None, Some(_)) if new.is_none() => {
                return Err(Error::Tool("new_string is required with old_string (use \"\" to delete)".into()));
            }
            _ => {}
        }
        if patch.is_some_and(|p| p.len() > MAX_FILE_BYTES) {
            return Err(Error::Tool("diff larger than 256KiB".into()));
        }
        let target = resolve_in_workspace(&self.workspace, path)?;
        let bytes = fs::read(&target).map_err(|_| {
            Error::Tool(format!("file not found: {path}. edit_file only changes existing files; use write_file to create one"))
        })?;
        if bytes.len() > MAX_FILE_BYTES {
            return Err(Error::Tool("file larger than 256KiB".into()));
        }
        let before = String::from_utf8(bytes)
            .map_err(|_| Error::Tool("file is not valid UTF-8".into()))?;
        let view = self.views.view(&target, before.as_bytes());
        if view == View::Never {
            // Never edit blind, but hand over the file so the retry needs no read.
            self.views.remember(&target, before.as_bytes());
            let file = parse_text_file(&before);
            let shown = file.lines.len().min(NEVER_READ_PREVIEW_LINES);
            let mut msg = format!(
                "read {path} before editing it. Here it is; now retry edit_file against this text.\n"
            );
            msg.push_str(&numbered_block(path, &file, 1, shown));
            if shown < file.lines.len() {
                msg.push_str(&format!("… {} more lines; read_file with start_line to see them\n", file.lines.len() - shown));
            }
            return Err(Error::Tool(msg));
        }
        // A stale view (the file changed since the agent saw it) still edits,
        // but only where the old text matches exactly.
        let loose = view == View::Fresh;
        let applied = match (patch, old, new) {
            (Some(p), _, _) => diff::apply_patch(&before, p, loose),
            (None, Some(o), Some(n)) => diff::apply_replace(&before, o, n, replace_all),
            _ => unreachable!(),
        };
        let applied = applied.map_err(|e| match (view, e) {
            (View::Stale, Error::Tool(m)) => Error::Tool(format!(
                "{path} changed since you last saw it (a command or another agent edited it). {m}"
            )),
            (_, e) => e,
        })?;
        let after = applied.text;
        if after.len() > MAX_FILE_BYTES {
            return Err(Error::Tool("edited file would be larger than 256KiB".into()));
        }
        if after == before {
            return Err(Error::Tool("the edit changes nothing".into()));
        }
        fs::write(&target, &after)?;
        self.views.remember(&target, after.as_bytes());
        let rel = path.replace('\\', "/");
        let mut out = diff::file_change_json(&rel, Some(&before), Some(&after));
        let mut notes = applied.notes;
        if view == View::Stale {
            notes.push("the file had changed since you last saw it; the edit applied because the old text still matched".into());
        }
        if !notes.is_empty() {
            out["notes"] = json!(notes);
        }
        Ok(out.to_string())
    }
}

/// How much of a never-read file an edit_file refusal shows.
const NEVER_READ_PREVIEW_LINES: usize = 400;

impl ClientTool for EditFileTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "edit_file".into(),
            description: "Change an existing UTF-8 file. Two forms:\n\
1) old_string + new_string: replace an exact piece of text (copy it from read_file without the N| prefixes, including indentation). It must occur once unless replace_all is true. Simplest for most edits; new_string \"\" deletes.\n\
2) diff: a unified diff with @@ hunks (' ' context, '-' delete, '+' add). Hunks are placed by their context and '-' lines; the @@ line numbers are only a hint and the counts may be approximate.\n\
You must have seen the file first (read_file, or your own write_file/edit_file). After a successful edit you can edit again without re-reading. Result JSON has the applied diff and any notes.".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string", "description": "Relative path inside the workspace (must already exist)"},
                    "old_string": {"type": "string", "description": "Exact existing text to replace (form 1)"},
                    "new_string": {"type": "string", "description": "Replacement text (form 1)"},
                    "replace_all": {"type": "boolean", "description": "Replace every occurrence of old_string (form 1). Default false"},
                    "diff": {"type": "string", "description": "Unified diff for this one file (form 2): @@ -oldStart,oldCount +newStart,newCount @@ then ' ' context, '-' deleted, '+' added lines."}
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
        let shell = crate::shellrt::shell_arg(args)?;
        let timeout = match args.get("timeout") {
            None | Some(Value::Null) => COMMAND_TIMEOUT_SECS,
            Some(v) => v
                .as_u64()
                .filter(|t| (1..=MAX_COMMAND_TIMEOUT_SECS).contains(t))
                .ok_or_else(|| Error::Tool(format!("timeout must be 1..={MAX_COMMAND_TIMEOUT_SECS} seconds")))?,
        };
        run_workspace_command_ex(
            &self.workspace,
            command,
            cwd,
            self.guard.as_ref(),
            window,
            self.windows.clone(),
            shell,
            timeout,
        )
        .await
    }
}

/// Default and longest wait for one `run_command`.
pub(crate) const COMMAND_TIMEOUT_SECS: u64 = 60;
pub(crate) const MAX_COMMAND_TIMEOUT_SECS: u64 = 600;
/// Kept of a long output: some of the start, most of the end (errors and
/// test summaries come last).
const STDOUT_HEAD: usize = 4 * 1024;
const STDOUT_TAIL: usize = 24 * 1024;
const STDERR_HEAD: usize = 2 * 1024;
const STDERR_TAIL: usize = 12 * 1024;
/// Captured per stream before the middle is dropped.
const CAPTURE_LIMIT: usize = 16 * 1024 * 1024;

/// Workspace shell used by `run_command` and by `timer` when a command fires.
pub(crate) async fn run_workspace_command(
    workspace: &Path,
    command: &str,
    cwd: Option<&str>,
    guard: Option<&Arc<dyn CommandReviewer>>,
) -> Result<String> {
    let shell = crate::shellrt::default_shell();
    run_workspace_command_ex(workspace, command, cwd, guard, None, None, shell, COMMAND_TIMEOUT_SECS).await
}

/// Everything a pipe produced, up to [`CAPTURE_LIMIT`] (then the middle).
#[derive(Default)]
struct Capture {
    head: Vec<u8>,
    tail: std::collections::VecDeque<u8>,
    dropped: usize,
}

impl Capture {
    fn push(&mut self, bytes: &[u8]) {
        let half = CAPTURE_LIMIT / 2;
        for &b in bytes {
            if self.head.len() < half {
                self.head.push(b);
            } else {
                if self.tail.len() >= half {
                    self.tail.pop_front();
                    self.dropped += 1;
                }
                self.tail.push_back(b);
            }
        }
    }

    fn text(&self) -> (String, usize) {
        let mut all = self.head.clone();
        all.extend(self.tail.iter());
        (crate::shellrt::decode_output(&all), self.dropped)
    }
}

fn spawn_reader<R>(pipe: Option<R>) -> (Arc<Mutex<Capture>>, tokio::task::JoinHandle<()>)
where
    R: tokio::io::AsyncRead + Unpin + Send + 'static,
{
    use tokio::io::AsyncReadExt;
    let cap = Arc::new(Mutex::new(Capture::default()));
    let sink = cap.clone();
    let task = tokio::spawn(async move {
        let Some(mut pipe) = pipe else { return };
        let mut buf = [0u8; 8192];
        loop {
            match pipe.read(&mut buf).await {
                Ok(0) | Err(_) => break,
                Ok(n) => sink.lock().unwrap_or_else(|e| e.into_inner()).push(&buf[..n]),
            }
        }
    });
    (cap, task)
}

/// Keep `head` chars of the start and `tail` of the end; say what was cut.
fn clip_head_tail(s: &str, head: usize, tail: usize, full_at: Option<&str>) -> String {
    let n = s.chars().count();
    if n <= head + tail {
        return s.to_string();
    }
    let start: String = s.chars().take(head).collect();
    let end: String = s.chars().skip(n - tail).collect();
    let more = match full_at {
        Some(p) => format!("; full output in {p}"),
        None => String::new(),
    };
    format!("{start}\n[… {} characters omitted{more} …]\n{end}", n - head - tail)
}

/// Save the full output under `.groka/output/` (git-ignored) so the model
/// can grep it. Returns the workspace-relative path.
fn save_full_output(workspace: &Path, command: &str, stdout: &str, stderr: &str) -> Option<String> {
    let out = crate::vision::groka_dir(workspace, "output").ok()?;
    let name = format!("cmd-{}.log", chrono::Utc::now().format("%Y%m%d-%H%M%S-%3f"));
    let body = format!("$ {command}\n\n--- stdout ---\n{stdout}\n--- stderr ---\n{stderr}");
    fs::write(out.join(&name), body).ok()?;
    prune_saved_outputs(&out, SAVED_OUTPUT_KEEP, SAVED_OUTPUT_MAX_BYTES);
    Some(format!(".groka/output/{name}"))
}

/// Saved outputs kept: the newest files, up to a count and a total size.
const SAVED_OUTPUT_KEEP: usize = 50;
const SAVED_OUTPUT_MAX_BYTES: u64 = 200 * 1024 * 1024;

/// Drop the oldest `cmd-*.log` files beyond `keep` files or `max_bytes`
/// in all. The newest file always stays.
fn prune_saved_outputs(dir: &Path, keep: usize, max_bytes: u64) {
    let Ok(entries) = fs::read_dir(dir) else { return };
    let mut logs: Vec<(String, u64, PathBuf)> = entries
        .flatten()
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            let meta = e.metadata().ok()?;
            (meta.is_file() && name.starts_with("cmd-") && name.ends_with(".log")).then(|| (name, meta.len(), e.path()))
        })
        .collect();
    // Names carry the timestamp: newest first.
    logs.sort_by(|a, b| b.0.cmp(&a.0));
    let mut total = 0u64;
    for (i, (_, len, path)) in logs.iter().enumerate() {
        total += len;
        if i > 0 && (i >= keep || total > max_bytes) {
            let _ = fs::remove_file(path);
        }
    }
}

#[allow(clippy::too_many_arguments)]
pub(crate) async fn run_workspace_command_ex(
    workspace: &Path,
    command: &str,
    cwd: Option<&str>,
    guard: Option<&Arc<dyn CommandReviewer>>,
    window: Option<&str>,
    windows: Option<Arc<WindowHub>>,
    shell: crate::shellrt::Shell,
    timeout_secs: u64,
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
    shellguard::enforce(guard, command, workspace, &cwd.to_string_lossy(), shellguard::ShellKind::of(shell)).await?;
    let window = wintrack::optional_name(window)?;
    let timeout_secs = timeout_secs.clamp(1, MAX_COMMAND_TIMEOUT_SECS);

    // Only commands that may write need a before/after comparison.
    let looks_only = shell == crate::shellrt::Shell::Bash && shellguard::read_only(command);
    let before_files = if looks_only {
        None
    } else {
        let at = cwd.clone();
        tokio::task::spawn_blocking(move || diff::snapshot(&at)).await.ok().flatten()
    };

    let mut cmd = shell_command_in(shell, command)?;
    cmd.current_dir(&cwd).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
    hide_window(&mut cmd);

    let before = if window.is_some() {
        wintrack::snapshot().await
    } else {
        Default::default()
    };
    // A process group / Job Object: a timeout stops the whole tree.
    let mut proc = crate::procgroup::ProcessGuard::spawn(cmd).map_err(|e| Error::Tool(format!("spawn failed: {e}")))?;
    let (out_cap, out_task) = spawn_reader(proc.child_mut().stdout.take());
    let (err_cap, err_task) = spawn_reader(proc.child_mut().stderr.take());
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
    let waited = tokio::time::timeout(Duration::from_secs(timeout_secs), proc.child_mut().wait());
    let (waited, bound) = tokio::join!(waited, watch);
    let (timed_out, exit_code) = match waited {
        Ok(Ok(status)) => (false, status.code()),
        Ok(Err(e)) => return Err(Error::Tool(format!("command failed: {e}"))),
        Err(_) => (true, None),
    };
    if timed_out {
        let _ = proc.kill().await;
    }
    // Pipes close when the last writer exits; a background `&` job may keep
    // them open, so do not wait for it forever.
    let drain = async {
        let _ = out_task.await;
        let _ = err_task.await;
    };
    let _ = tokio::time::timeout(Duration::from_secs(3), drain).await;
    if !timed_out {
        // Leave what the command started on purpose (a GUI app, `cmd &`).
        proc.detach();
    }

    let (stdout_full, out_dropped) = out_cap.lock().unwrap_or_else(|e| e.into_inner()).text();
    let (stderr_full, err_dropped) = err_cap.lock().unwrap_or_else(|e| e.into_inner()).text();
    let clipped = out_dropped > 0
        || err_dropped > 0
        || stdout_full.chars().count() > STDOUT_HEAD + STDOUT_TAIL
        || stderr_full.chars().count() > STDERR_HEAD + STDERR_TAIL;
    let full_at = if clipped { save_full_output(workspace, command, &stdout_full, &stderr_full) } else { None };
    let stdout = clip_head_tail(&stdout_full, STDOUT_HEAD, STDOUT_TAIL, full_at.as_deref());
    let stderr = clip_head_tail(&stderr_full, STDERR_HEAD, STDERR_TAIL, full_at.as_deref());

    // Only what this command changed. The model gets paths and counts; the
    // diffs ride along in `_diffs` for the UI and are stripped before the
    // output reaches the model.
    let diffs: Vec<Value> = match before_files {
        Some(snap) => tokio::task::spawn_blocking(move || diff::changes_since(&snap)).await.unwrap_or_default(),
        None => Vec::new(),
    };
    let files: Vec<Value> = diffs
        .iter()
        .map(|d| json!({"path": d["path"], "kind": d["kind"], "plus": d["plus"], "minus": d["minus"]}))
        .collect();
    let mut body = json!({
        "timed_out": timed_out,
        "exit_code": exit_code,
        "stdout": stdout,
        "stderr": stderr,
        "files": files,
    });
    if !diffs.is_empty() {
        body["_diffs"] = Value::Array(diffs);
    }
    if let Some(p) = &full_at {
        body["full_output"] = json!(p);
    }
    if timed_out {
        body["hint"] = json!(format!(
            "Stopped after {timeout_secs}s; the process tree was killed and the output so far is above. For a longer run pass a larger timeout (up to {MAX_COMMAND_TIMEOUT_SECS}s) or use run_background."
        ));
    }
    if window.is_some() {
        wintrack::attach_to(&mut body, &bound);
    }
    Ok(body.to_string())
}

/// What the model sees of a tool output: drops UI-only fields (`_diffs`).
pub fn model_view(output: &str) -> std::borrow::Cow<'_, str> {
    if !output.contains("\"_diffs\"") {
        return std::borrow::Cow::Borrowed(output);
    }
    match serde_json::from_str::<Value>(output) {
        Ok(mut v) => {
            if let Some(o) = v.as_object_mut() {
                o.remove("_diffs");
            }
            std::borrow::Cow::Owned(v.to_string())
        }
        Err(_) => std::borrow::Cow::Borrowed(output),
    }
}

impl ClientTool for RunCommandTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "run_command".into(),
            description: format!("Run a shell command starting in the workspace (default timeout 60s, `timeout` up to 600; on timeout the whole process tree is stopped and the output so far is returned). {} Commands are not sandboxed: they can reach anything your user account can. Ones with risky effects (paths outside the workspace, recursive deletes, uploads, inline code that spawns or networks) are checked by an independent reviewer first. Long output keeps the start and most of the end; the full text is saved and its path given in full_output. `files` lists only what this command changed (path, kind, plus/minus) when the workspace is a git repo; read a file to see its diff. If the command will open a GUI, set window to a short label; the result includes that window's pid so screenshot can target it.", crate::shellrt::describe()),
            parameters: json!({
                "type": "object",
                "properties": {
                    "command": {"type": "string", "description": "Shell command to run"},
                    "shell": {"type": "string", "enum": ["bash", "cmd", "powershell"], "description": "Shell to run in. Default bash; use cmd/powershell only for Windows-native commands."},
                    "cwd": {"type": "string", "description": "Optional subdirectory inside the workspace"},
                    "timeout": {"type": "integer", "minimum": 1, "maximum": 600, "description": "Seconds before the command is stopped (default 60). Use more for slow builds; use run_background for servers."},
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
        if crate::shot::capture_blocked() {
            return Err(Error::Tool(crate::shot::SCREEN_RECORDING_NOTE.into()));
        }
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
        let (pid, bound) = resolve_shot_pid(args, self.windows.as_deref())?;
        let query = ShotQuery {
            title,
            app,
            pid,
            bound,
            known: self.windows.as_ref().map(|h| h.all()).unwrap_or_default(),
            spawned: (self.spawned)(),
            hint: self.workspace_hint(),
        };

        if list_only {
            return tokio::task::spawn_blocking(move || list_windows_json(&query))
                .await
                .map_err(|e| Error::Tool(format!("screenshot join: {e}")))?;
        }

        let named = args
            .get("path")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty());
        let reused_path = named.is_some();
        let requested = named
            .map(|s| s.replace('\\', "/"))
            .unwrap_or_else(|| {
                crate::vision::rel_shot_path()
                    .to_string_lossy()
                    .replace('\\', "/")
            });
        let (rel, path_note) = jpeg_rel_path(&requested, |rel| {
            resolve_target_in_workspace(&self.workspace, rel).is_ok_and(|p| p.exists())
        })?;
        let dest = resolve_target_in_workspace(&self.workspace, &rel)?;
        ensure_groka_parent(&self.workspace, &dest)?;
        let mut extra = json!({});
        if let Some(note) = path_note {
            extra["path_note"] = json!(note);
        }
        let want_monitor = target.eq_ignore_ascii_case("monitor")
            || target.eq_ignore_ascii_case("screen")
            || target.eq_ignore_ascii_case("desktop");
        let workspace = self.workspace.clone();
        tokio::task::spawn_blocking(move || {
            let out = if want_monitor {
                capture_monitor_to(&dest, &rel, extra)
            } else {
                capture_window_to(&dest, &rel, &query, extra)
            }?;
            // Default names are unique; a path the model names may be shot
            // again, so its picture row points at an immutable copy.
            Ok(if reused_path { with_seen_view(&workspace, &dest, out) } else { out })
        })
        .await
        .map_err(|e| Error::Tool(format!("screenshot join: {e}")))?
    }
}

/// Add `"view"`: a `.groka/seen` copy of the shot just written. Best effort;
/// without it the row and the attach use `path`.
fn with_seen_view(workspace: &Path, target: &Path, out: String) -> String {
    let Ok(mut v) = serde_json::from_str::<Value>(&out) else {
        return out;
    };
    if v.get("attach_image").and_then(Value::as_bool) != Some(true) {
        return out;
    }
    match fs::read(target).ok().and_then(|b| crate::vision::save_seen(workspace, &b).ok()) {
        Some(view) => {
            v["view"] = json!(view);
            v.to_string()
        }
        None => out,
    }
}

/// Screenshots are always JPEG bytes and every decoder goes by extension, so
/// any other extension (or none) becomes .jpg; the note tells the model. The
/// changed name never replaces a file the model did not name (`taken`).
fn jpeg_rel_path(requested: &str, taken: impl Fn(&str) -> bool) -> Result<(String, Option<String>)> {
    let path = Path::new(requested);
    let is_jpeg = path
        .extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| e.eq_ignore_ascii_case("jpg") || e.eq_ignore_ascii_case("jpeg"));
    if is_jpeg {
        return Ok((requested.to_string(), None));
    }
    let stem = path.with_extension("").to_string_lossy().replace('\\', "/");
    let fixed = (1..100)
        .map(|n| if n == 1 { format!("{stem}.jpg") } else { format!("{stem}-{n}.jpg") })
        .find(|p| !taken(p))
        .ok_or_else(|| Error::Tool(format!("{stem}.jpg already exists; pass a new .jpg path")))?;
    let note = format!("screenshots are saved as JPEG, so the path is {fixed} instead of {requested}");
    Ok((fixed, Some(note)))
}

/// A shot under .groka/ goes through vision::groka_dir so the folder carries
/// its '*' .gitignore and captures never get committed.
fn ensure_groka_parent(workspace: &Path, dest: &Path) -> Result<()> {
    let Ok(root) = workspace.canonicalize() else {
        return Ok(());
    };
    let Ok(inside) = dest.strip_prefix(root.join(".groka")) else {
        return Ok(());
    };
    let sub = inside.parent().unwrap_or(Path::new(""));
    crate::vision::groka_dir(&root, &sub.to_string_lossy())?;
    Ok(())
}

fn resolve_shot_pid(
    args: &Value,
    windows: Option<&WindowHub>,
) -> Result<(Option<u32>, Option<wintrack::BoundWindow>)> {
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
        return Ok((Some(rec.pid), Some(rec)));
    }
    let pid = args
        .get("pid")
        .and_then(Value::as_u64)
        .map(|n| n as u32)
        .filter(|p| *p > 0);
    Ok((pid, None))
}

/// What the model asked to capture, gathered before the blocking capture.
struct ShotQuery {
    title: Option<String>,
    app: Option<String>,
    pid: Option<u32>,
    /// The binding behind `name`.
    bound: Option<wintrack::BoundWindow>,
    /// Every bound window, for labels and the default pick.
    known: Vec<wintrack::BoundWindow>,
    spawned: Vec<u32>,
    hint: Option<String>,
}

/// Process facts the ranking needs, read once per call.
struct ShotContext {
    parents: HashMap<u32, u32>,
    me: u32,
    /// grokaagent's ancestors: the terminal or IDE it runs in.
    host: Vec<u32>,
    /// Windows of own-process bindings that are still open.
    bound_ids: Vec<u32>,
    /// The query's spawned pids, less those of bound apps that have exited.
    spawned: Vec<u32>,
}

impl ShotContext {
    fn of(metas: &[crate::shot::WinMeta], q: &ShotQuery) -> Self {
        let me = std::process::id();
        let mut seeds: Vec<u32> = metas.iter().map(|m| m.pid).collect();
        seeds.push(me);
        let parents = crate::shot::parent_map(&seeds);
        Self::from_parts(metas, q, me, parents)
    }

    fn from_parts(metas: &[crate::shot::WinMeta], q: &ShotQuery, me: u32, parents: HashMap<u32, u32>) -> Self {
        // Only a launch's own process makes its windows the agent's without
        // asking; a browser window or a retitled one may show the user's page.
        let own: Vec<&wintrack::BoundWindow> = q.known.iter().filter(|b| b.own_process).collect();
        let bound_ids = own
            .iter()
            .filter(|b| metas.iter().any(|m| b.is_window(m)))
            .map(|b| b.id)
            .collect();
        // A bound app that exited leaves its pid to whatever the OS starts next.
        let gone: Vec<u32> = own
            .iter()
            .filter(|b| !metas.iter().any(|m| m.pid == b.pid && crate::shot::same_app(&b.app, &m.app)))
            .map(|b| b.pid)
            .collect();
        Self {
            host: crate::shot::ancestors(me, &parents),
            bound_ids,
            spawned: q.spawned.iter().copied().filter(|p| !gone.contains(p)).collect(),
            parents,
            me,
        }
    }
}

impl ShotQuery {
    fn explicit(&self) -> bool {
        self.title.is_some() || self.app.is_some() || self.pid.is_some()
    }

    /// `selectors: false` drops name/pid/title/app, to list what else is open.
    fn filter<'a>(&'a self, ctx: &'a ShotContext, selectors: bool) -> crate::shot::ShotFilter<'a> {
        let bound = self.bound.as_ref().filter(|_| selectors);
        crate::shot::ShotFilter {
            title: self.title.as_deref().filter(|_| selectors),
            app: self.app.as_deref().filter(|_| selectors),
            pid: self.pid.filter(|_| selectors),
            window_id: bound.map(|b| b.id).filter(|id| *id != 0),
            strict_window: bound.is_some_and(|b| !b.own_process),
            bound_title: bound.map(|b| b.title.as_str()),
            bound_app: bound.map(|b| b.app.as_str()),
            spawned: &ctx.spawned,
            bound_ids: &ctx.bound_ids,
            self_pids: std::slice::from_ref(&ctx.me),
            host_pids: &ctx.host,
            parents: Some(&ctx.parents),
            workspace_hint: self.hint.as_deref(),
        }
    }
}

fn window_rows(q: &ShotQuery, metas: &[crate::shot::WinMeta], order: &[usize], limit: usize) -> Vec<Value> {
    order
        .iter()
        .take(limit)
        .map(|&i| {
            let m = &metas[i];
            let mut row = json!({
                "title": m.title,
                "app": m.app,
                "pid": m.pid,
                "width": m.width,
                "height": m.height
            });
            if let Some(name) = wintrack::name_for(&q.known, m) {
                row["name"] = json!(name);
            }
            row
        })
        .collect()
}

fn list_windows_json(q: &ShotQuery) -> Result<String> {
    let metas: Vec<_> = crate::shot::collect_windows()?.into_iter().map(|(_, m)| m).collect();
    let ctx = ShotContext::of(&metas, q);
    let ranked = crate::shot::ranked_indices(&metas, &q.filter(&ctx, true));
    Ok(json!({
        "windows": window_rows(q, &metas, &ranked, 16),
        "attach_image": false
    })
    .to_string())
}

fn capture_monitor_to(target: &Path, rel: &str, mut extra: Value) -> Result<String> {
    let rgba = crate::shot::capture_monitor()?;
    let img = crate::vision::from_rgba(rgba.width(), rgba.height(), rgba.into_raw())?;
    extra["source"] = json!("monitor");
    shot_json(target, rel, &img, extra)
}

/// Captures only a window the request points at; when none can be taken it
/// captures nothing (never the whole display) and lists the open windows.
fn capture_window_to(target: &Path, rel: &str, q: &ShotQuery, mut extra: Value) -> Result<String> {
    let listed = crate::shot::collect_windows()?;
    let metas: Vec<_> = listed.iter().map(|(_, m)| m.clone()).collect();
    let ctx = ShotContext::of(&metas, q);
    let order = crate::shot::capture_candidates(&metas, &q.filter(&ctx, true));
    let mut failed = None;
    for &i in &order {
        let (win, meta) = &listed[i];
        let rgba = match win.capture_image() {
            Ok(rgba) => rgba,
            Err(e) => {
                failed.get_or_insert_with(|| e.to_string());
                continue;
            }
        };
        if rgba.width() < 160 || rgba.height() < 120 {
            continue;
        }
        let Ok(img) = crate::vision::from_rgba(rgba.width(), rgba.height(), rgba.into_raw()) else {
            continue;
        };
        extra["source"] = json!("window");
        extra["title"] = json!(meta.title);
        extra["app"] = json!(meta.app);
        extra["pid"] = json!(meta.pid);
        let name = q.bound.as_ref().map(|b| b.name.as_str()).or_else(|| wintrack::name_for(&q.known, meta));
        if let Some(name) = name {
            extra["name"] = json!(name);
        }
        if let Some(note) = q.bound.as_ref().and_then(|b| bound_drift(b, meta)) {
            extra["bound_note"] = json!(note);
        }
        return shot_json(target, rel, &img, extra);
    }
    Ok(no_capture_json(q, &metas, &ctx, !order.is_empty(), failed.as_deref()))
}

/// How the captured window differs from the one `name` was bound to.
fn bound_drift(b: &wintrack::BoundWindow, meta: &crate::shot::WinMeta) -> Option<String> {
    if b.id != 0 && b.id != meta.id {
        return Some(format!(
            "the window first bound to {} (\"{}\") is gone; captured another window of the same app",
            b.name, b.title
        ));
    }
    // An app retitles itself as it navigates; a browser window bound by one
    // tab that now shows another title may be the user's page.
    if !b.own_process && !b.title.trim().eq_ignore_ascii_case(meta.title.trim()) {
        return Some(format!(
            "title changed since {} was bound (was \"{}\"); check this is still the window you meant",
            b.name, b.title
        ));
    }
    None
}

fn no_capture_reason(
    q: &ShotQuery,
    metas: &[crate::shot::WinMeta],
    matched: bool,
    failed: Option<&str>,
) -> String {
    if matched {
        let why = failed.map(|e| format!(" ({e})")).unwrap_or_default();
        return format!("could not capture the matching window{why}; nothing was captured. Pick another window below by pid/title/app.");
    }
    if let Some(b) = &q.bound {
        if metas.iter().any(|m| b.is_window(m) && crate::shot::is_workbench(&m.title)) {
            return format!(
                "window {} now shows the grokaagent workbench tab, which is never captured (its address bar holds the session token); nothing was captured. Ask the user to switch that window back to your page, or open the page in a window of its own and launch it with window={}.",
                b.name, b.name
            );
        }
        return format!(
            "window {} (pid {}, \"{}\") is no longer open or visible; nothing was captured. Relaunch it with window={}, or pick one below by pid/title/app.",
            b.name, b.pid, b.title, b.name
        );
    }
    if q.explicit() {
        return "no window matches the given pid/title/app; nothing was captured. Pick one below.".into();
    }
    "no window of an app you started was found, so nothing was captured (other windows may be the user's). Pass name (the window= label; bound windows below show it), pid, title or app to pick one below.".into()
}

fn no_capture_json(
    q: &ShotQuery,
    metas: &[crate::shot::WinMeta],
    ctx: &ShotContext,
    matched: bool,
    failed: Option<&str>,
) -> String {
    let ranked = crate::shot::ranked_indices(metas, &q.filter(ctx, false));
    let mut out = json!({
        "attach_image": false,
        "error": no_capture_reason(q, metas, matched, failed),
        "windows": window_rows(q, metas, &ranked, 8)
    });
    if let Some(b) = &q.bound {
        out["name"] = json!(b.name);
    } else if let Some(pid) = q.pid {
        out["wanted_pid"] = json!(pid);
    }
    out.to_string()
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

#[cfg(test)]
mod shot_tool_tests {
    use super::*;
    use crate::shot::WinMeta;
    use crate::wintrack::BoundWindow;

    fn win(id: u32, title: &str, app: &str, pid: u32, w: u32, h: u32) -> WinMeta {
        WinMeta {
            id,
            title: title.into(),
            app: app.into(),
            pid,
            width: w,
            height: h,
            minimized: false,
        }
    }

    fn query() -> ShotQuery {
        ShotQuery {
            title: None,
            app: None,
            pid: None,
            bound: None,
            known: Vec::new(),
            spawned: Vec::new(),
            hint: None,
        }
    }

    fn ctx() -> ShotContext {
        ShotContext {
            parents: HashMap::new(),
            me: 99,
            host: Vec::new(),
            bound_ids: Vec::new(),
            spawned: Vec::new(),
        }
    }

    fn jpeg(requested: &str) -> (String, Option<String>) {
        jpeg_rel_path(requested, |_| false).unwrap()
    }

    #[test]
    fn shot_paths_always_end_in_jpg() {
        assert_eq!(jpeg("a/b.jpg"), ("a/b.jpg".to_string(), None));
        assert_eq!(jpeg("b.JPEG"), ("b.JPEG".to_string(), None));
        let (rel, note) = jpeg("shots/home.png");
        assert_eq!(rel, "shots/home.jpg");
        assert!(note.unwrap().contains("shots/home.png"));
        assert_eq!(jpeg("shot").0, "shot.jpg");
        assert_eq!(jpeg("docs/screenshot.webp").0, "docs/screenshot.jpg");
    }

    #[test]
    fn a_changed_shot_path_never_lands_on_an_existing_file() {
        let existing = ["assets/logo.jpg", "assets/logo-2.jpg"];
        let (rel, note) = jpeg_rel_path("assets/logo.png", |p| existing.contains(&p)).unwrap();
        assert_eq!(rel, "assets/logo-3.jpg");
        assert!(note.unwrap().contains("assets/logo-3.jpg"));
        // A .jpg the model named is its call to overwrite.
        assert_eq!(jpeg_rel_path("assets/logo.jpg", |_| true).unwrap().0, "assets/logo.jpg");
        let err = jpeg_rel_path("x.png", |_| true).unwrap_err();
        assert!(err.to_string().contains("already exists"), "{err}");
    }

    #[test]
    fn default_mode_trusts_only_bindings_of_a_launched_process() {
        // `web` got a new tab in the user's browser window, which now shows mail.
        let web = BoundWindow::from_meta("web".into(), &win(1, "Vite App - Google Chrome", "Google-chrome", 50, 1920, 1080), false);
        let app = BoundWindow::from_meta("app".into(), &win(4, "My App", "electron", 77, 800, 600), true);
        let metas = vec![
            win(1, "Inbox (4) - Gmail - Google Chrome", "Google-chrome", 50, 1920, 1080),
            win(4, "My App", "electron", 77, 800, 600),
        ];
        let q = ShotQuery {
            known: vec![web.clone()],
            ..query()
        };
        let ctx = ShotContext::from_parts(&metas, &q, 99, HashMap::new());
        assert!(ctx.bound_ids.is_empty());
        assert!(crate::shot::capture_candidates(&metas, &q.filter(&ctx, true)).is_empty());
        let rows = window_rows(&q, &metas, &crate::shot::ranked_indices(&metas, &q.filter(&ctx, false)), 8);
        assert!(rows.iter().any(|r| r["name"] == "web"), "the list still offers name=web");

        let q = ShotQuery {
            known: vec![web, app],
            ..query()
        };
        let ctx = ShotContext::from_parts(&metas, &q, 99, HashMap::new());
        assert_eq!(ctx.bound_ids, vec![4]);
        let order = crate::shot::capture_candidates(&metas, &q.filter(&ctx, true));
        assert_eq!(order, vec![1]);
    }

    #[test]
    fn an_exited_bound_app_leaves_its_pid_behind() {
        let app = BoundWindow::from_meta("app".into(), &win(4, "My App", "electron", 77, 800, 600), true);
        // pid 77 now belongs to Notepad; 300 is a live background shell.
        let metas = vec![win(9, "Untitled - Notepad", "Notepad", 77, 1280, 800)];
        let q = ShotQuery {
            known: vec![app.clone()],
            spawned: vec![77, 300],
            ..query()
        };
        let ctx = ShotContext::from_parts(&metas, &q, 99, HashMap::new());
        assert_eq!(ctx.spawned, vec![300]);
        assert!(crate::shot::capture_candidates(&metas, &q.filter(&ctx, true)).is_empty());
        let named = ShotQuery {
            pid: Some(77),
            bound: Some(app.clone()),
            ..q
        };
        assert!(crate::shot::capture_candidates(&metas, &named.filter(&ctx, true)).is_empty());
        let out: Value = serde_json::from_str(&no_capture_json(&named, &metas, &ctx, false, None)).unwrap();
        assert!(out["error"].as_str().unwrap().contains("no longer open"), "{out}");
    }

    #[test]
    fn a_binding_now_showing_the_workbench_is_refused_with_its_reason() {
        let web = BoundWindow::from_meta("web".into(), &win(1, "Vite App - Google Chrome", "Google-chrome", 50, 1920, 1080), false);
        let metas = vec![win(1, "grokaagent - Google Chrome", "Google-chrome", 50, 1920, 1080)];
        let q = ShotQuery {
            pid: Some(50),
            bound: Some(web.clone()),
            known: vec![web],
            ..query()
        };
        let ctx = ctx();
        assert!(crate::shot::capture_candidates(&metas, &q.filter(&ctx, true)).is_empty());
        let out: Value = serde_json::from_str(&no_capture_json(&q, &metas, &ctx, false, None)).unwrap();
        assert!(out["error"].as_str().unwrap().contains("workbench"), "{out}");
        assert!(out["windows"].as_array().unwrap().is_empty(), "{out}");
    }

    #[test]
    fn shots_under_groka_get_its_gitignore() {
        let dir = tempfile::tempdir().unwrap();
        let dest = resolve_target_in_workspace(dir.path(), ".groka/shots/a.jpg").unwrap();
        ensure_groka_parent(dir.path(), &dest).unwrap();
        assert_eq!(fs::read_to_string(dir.path().join(".groka/.gitignore")).unwrap().trim(), "*");
        assert!(dir.path().join(".groka/shots").is_dir());

        let other = tempfile::tempdir().unwrap();
        let dest = resolve_target_in_workspace(other.path(), "shots/a.jpg").unwrap();
        ensure_groka_parent(other.path(), &dest).unwrap();
        assert!(!other.path().join(".groka").exists());
    }

    #[test]
    fn a_closed_bound_window_is_reported_not_swapped_for_another() {
        let rec = BoundWindow::from_meta("app".into(), &win(4, "My App", "electron", 77, 800, 600), true);
        let q = ShotQuery {
            pid: Some(77),
            bound: Some(rec.clone()),
            known: vec![rec],
            ..query()
        };
        let metas = vec![
            win(1, "Inbox - Google Chrome", "Google-chrome", 50, 1920, 1080),
            win(2, "zsh", "Terminal", 13, 800, 600),
        ];
        let ctx = ctx();
        assert!(crate::shot::capture_candidates(&metas, &q.filter(&ctx, true)).is_empty());
        let out: Value = serde_json::from_str(&no_capture_json(&q, &metas, &ctx, false, None)).unwrap();
        assert_eq!(out["attach_image"], false);
        assert!(out["error"].as_str().unwrap().contains("no longer open"), "{out}");
        assert_eq!(out["name"], "app");
        let rows = out["windows"].as_array().unwrap();
        assert_eq!(rows.len(), 1, "{out}");
        assert_eq!(rows[0]["title"], "Inbox - Google Chrome");
    }

    #[test]
    fn no_capture_reasons_say_what_to_pass() {
        let default = no_capture_reason(&query(), &[], false, None);
        assert!(default.contains("app you started"), "{default}");
        let titled = ShotQuery {
            title: Some("Vite".into()),
            ..query()
        };
        assert!(no_capture_reason(&titled, &[], false, None).contains("no window matches"));
        let failed = no_capture_reason(&titled, &[], true, Some("permission denied"));
        assert!(failed.contains("permission denied") && failed.contains("nothing was captured"), "{failed}");
    }

    #[test]
    fn a_browser_binding_is_strict_and_dropped_for_the_list() {
        let rec = BoundWindow::from_meta("web".into(), &win(3, "Vite App", "Google-chrome", 50, 1280, 800), false);
        let q = ShotQuery {
            pid: Some(50),
            bound: Some(rec.clone()),
            known: vec![rec],
            ..query()
        };
        let ctx = ShotContext {
            bound_ids: vec![3],
            ..ctx()
        };
        let f = q.filter(&ctx, true);
        assert_eq!(f.window_id, Some(3));
        assert!(f.strict_window);
        let relaxed = q.filter(&ctx, false);
        assert!(relaxed.pid.is_none() && relaxed.window_id.is_none() && !relaxed.explicit());
    }

    #[test]
    fn bound_drift_explains_a_different_window() {
        let rec = BoundWindow::from_meta("app".into(), &win(4, "My App", "electron", 77, 800, 600), true);
        assert!(bound_drift(&rec, &win(4, "My App", "electron", 77, 800, 600)).is_none());
        assert!(bound_drift(&rec, &win(5, "My App", "electron", 77, 800, 600)).unwrap().contains("is gone"));
        assert!(bound_drift(&rec, &win(4, "My App - Settings", "electron", 77, 800, 600)).is_none());
        let tab = BoundWindow::from_meta("web".into(), &win(3, "Vite App", "Google-chrome", 50, 1280, 800), false);
        assert!(bound_drift(&tab, &win(3, "Inbox", "Google-chrome", 50, 1280, 800)).unwrap().contains("title changed"));
    }

    #[tokio::test]
    async fn screenshot_png_path_is_saved_as_jpg() {
        let dir = tempfile::tempdir().unwrap();
        let tool = ScreenshotTool::new(dir.path().to_path_buf());
        let raw = tool
            .run(&json!({"path": "shots/home.png", "target": "monitor"}))
            .await
            .unwrap();
        let out: Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(out["path"], "shots/home.jpg");
        assert!(out["path_note"].as_str().unwrap().contains("JPEG"), "{out}");
        assert!(dir.path().join("shots/home.jpg").exists());
        assert!(!dir.path().join("shots/home.png").exists());
    }

    #[tokio::test]
    async fn screenshot_png_path_keeps_an_existing_jpg() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join("assets")).unwrap();
        fs::write(dir.path().join("assets/logo.jpg"), b"keep").unwrap();
        let tool = ScreenshotTool::new(dir.path().to_path_buf());
        let raw = tool
            .run(&json!({"path": "assets/logo.png", "target": "monitor"}))
            .await
            .unwrap();
        let out: Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(out["path"], "assets/logo-2.jpg", "{out}");
        assert_eq!(fs::read(dir.path().join("assets/logo.jpg")).unwrap(), b"keep");
        assert!(dir.path().join("assets/logo-2.jpg").exists());
    }

    #[tokio::test]
    async fn screenshot_default_path_is_git_ignored() {
        let dir = tempfile::tempdir().unwrap();
        let tool = ScreenshotTool::new(dir.path().to_path_buf());
        let raw = tool.run(&json!({"target": "monitor"})).await.unwrap();
        let out: Value = serde_json::from_str(&raw).unwrap();
        assert!(out["path"].as_str().unwrap().starts_with(".groka/shots/"), "{out}");
        assert_eq!(fs::read_to_string(dir.path().join(".groka/.gitignore")).unwrap().trim(), "*");
    }
}

impl ClientTool for ScreenshotTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "screenshot".into(),
            description: "Capture the GUI window you opened to a JPEG and attach the pixels on the next turn. Prefer name from the window= label you set when launching, or pid from that result. Does not capture the IDE or the grokaagent terminal unless you ask for them, and its web workbench only with title=grokaagent. Without name/pid/title/app it captures only a window of an app you started (run_background, window=) or a browser page on localhost; otherwise, or when the named window is gone, nothing is captured and the result lists windows to pick from (a browser window you bound with window= needs name=). Optional title/app substring; target=monitor for the whole primary display; list=true returns windows (with bound names) without capturing. Always saved as JPEG (another extension is changed to .jpg); default path is .groka/shots/<timestamp>.jpg.".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string", "description": "Relative .jpg path inside the workspace"},
                    "name": {"type": "string", "description": "Window label you set with window= on run_command / run_background"},
                    "pid": {"type": "integer", "description": "Process id of the window, or of a process that started it (e.g. run_background's pid)"},
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
        crate::vision::check_source_size(&resolved)?;
        // Header only: a tiny image or a bad region fails before any decode.
        let (width, height) = crate::vision::image_dimensions(&resolved)?;
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
        let region = image_region(args, width, height)?;
        let mut img = crate::vision::open_image(&resolved)?;
        if let Some((x, y, w, h)) = region {
            img = img.crop_imm(x, y, w, h);
        }
        let (jpeg, sent_width, sent_height) = crate::vision::model_jpeg(&img)?;
        let scale = (sent_width as f64 / img.width().max(1) as f64 * 1000.0).round() / 1000.0;
        let mut out = json!({
            "path": path.replace('\\', "/"),
            "attach_image": true,
            "mime": "image/jpeg",
            "width": width,
            "height": height,
            "sent_width": sent_width,
            "sent_height": sent_height,
            "scale": scale
        });
        // The exact pixels sent: attached from here and shown in the picture
        // row. Best effort (a read-only workspace): the attach then re-reads
        // `path` and `region`.
        if let Ok(view) = crate::vision::save_seen(&self.workspace, &jpeg) {
            out["view"] = json!(view);
        }
        if let Some((x, y, w, h)) = region {
            out["region"] = json!({"x": x, "y": y, "width": w, "height": h});
        }
        if sent_width < img.width() {
            out["note"] = json!(format!(
                "Downscaled from {}x{}; small text may be unreadable. Pass region (x, y, width, height in original pixels) to see a part at full detail.",
                img.width(),
                img.height()
            ));
        }
        Ok(out.to_string())
    }
}

/// The optional `region` crop, clamped to the image. Coordinates are in
/// original pixels, after EXIF orientation.
fn image_region(args: &Value, width: u32, height: u32) -> Result<Option<(u32, u32, u32, u32)>> {
    let Some(r) = args.get("region").filter(|r| !r.is_null()) else {
        return Ok(None);
    };
    let num = |k: &str| -> Result<u32> {
        r.get(k)
            .and_then(Value::as_f64)
            .filter(|n| n.is_finite() && *n >= 0.0)
            .map(|n| n.round().min(u32::MAX as f64) as u32)
            .ok_or_else(|| Error::Tool(format!("region.{k} must be a number >= 0")))
    };
    let (x, y, w, h) = (num("x")?, num("y")?, num("width")?, num("height")?);
    if x >= width || y >= height || w == 0 || h == 0 {
        return Err(Error::Tool(format!(
            "region x={x} y={y} width={w} height={h} is empty or outside the {width}x{height} image"
        )));
    }
    let (w, h) = (w.min(width - x), h.min(height - y));
    if crate::vision::below_vision_min(w, h) {
        return Err(Error::Tool(format!(
            "region {w}x{h} is under the {} pixels xAI vision accepts; pick a larger region",
            crate::vision::MIN_VISION_PIXELS
        )));
    }
    Ok(Some((x, y, w, h)))
}

impl ClientTool for ReadImageTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "read_image".into(),
            description: "Load an image (PNG, JPEG, GIF, WebP or BMP) from the workspace and attach the pixels so you can see it on the next turn. It is sent with the long edge at most 1280 px; sent_width/sent_height/scale in the result say what you got. Pass region to crop in original pixels before that downscale, e.g. to read part of a tall full-page screenshot. xAI rejects images under 512 total pixels (for example 16x16); those are not attached — enlarge or regenerate the file, then call again.".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string", "description": "Relative image path inside the workspace"},
                    "region": {
                        "type": "object",
                        "description": "Optional crop in original-image pixels, applied before downscaling",
                        "properties": {
                            "x": {"type": "integer", "description": "Left edge"},
                            "y": {"type": "integer", "description": "Top edge"},
                            "width": {"type": "integer"},
                            "height": {"type": "integer"}
                        },
                        "required": ["x", "y", "width", "height"],
                        "additionalProperties": false
                    }
                },
                "required": ["path"],
                "additionalProperties": false
            }),
        }
    }

    fn call(&self, args: &Value) -> ToolCallFut<'_> {
        // Decoding a large image takes a while: keep it off the async workers.
        let tool = Self::new(self.workspace.clone());
        let args = args.clone();
        Box::pin(async move {
            tokio::task::spawn_blocking(move || tool.call_sync(&args))
                .await
                .map_err(|e| Error::Tool(format!("read_image join: {e}")))?
        })
    }
}

#[cfg(test)]
mod read_image_tests {
    use super::*;
    use crate::vision::vision_tests::{decode_uri, rotated_jpeg};

    fn read(ws: &Path, args: Value) -> Value {
        serde_json::from_str(&ReadImageTool::new(ws.to_path_buf()).call_sync(&args).unwrap()).unwrap()
    }

    /// What the model gets for a read_image result.
    fn sent(ws: &Path, out: &Value) -> image::RgbImage {
        let uri = crate::vision::data_uri_for_tool(ws, &out.to_string()).unwrap().unwrap();
        decode_uri(&uri).to_rgb8()
    }

    #[test]
    fn a_tall_image_reports_what_was_sent_and_region_zooms_in() {
        let ws = tempfile::tempdir().unwrap();
        // 640x2560: white top half, black bottom half.
        let page = image::RgbImage::from_fn(640, 2560, |_, y| {
            image::Rgb(if y < 1280 { [255; 3] } else { [0; 3] })
        });
        page.save(ws.path().join("page.png")).unwrap();
        let out = read(ws.path(), json!({"path": "page.png"}));
        assert_eq!((out["width"].as_u64(), out["height"].as_u64()), (Some(640), Some(2560)));
        assert_eq!((out["sent_width"].as_u64(), out["sent_height"].as_u64()), (Some(320), Some(1280)));
        assert_eq!(out["scale"], 0.5);
        assert!(out["note"].as_str().unwrap().contains("region"), "{out}");
        assert_eq!(sent(ws.path(), &out).dimensions(), (320, 1280));

        let region = json!({"x": 0, "y": 1280, "width": 640, "height": 5000});
        let out = read(ws.path(), json!({"path": "page.png", "region": region}));
        assert_eq!(out["region"], json!({"x": 0, "y": 1280, "width": 640, "height": 1280}));
        assert_eq!((out["sent_width"].as_u64(), out["sent_height"].as_u64()), (Some(640), Some(1280)));
        assert_eq!(out["scale"], 1.0);
        assert!(out.get("note").is_none(), "{out}");
        let pixels = sent(ws.path(), &out);
        assert_eq!(pixels.dimensions(), (640, 1280));
        assert!(pixels.pixels().all(|p| p[0] < 40), "only the black half was sent");
    }

    #[test]
    fn a_bad_region_is_refused_before_decoding() {
        let ws = tempfile::tempdir().unwrap();
        image::RgbImage::new(100, 100).save(ws.path().join("a.png")).unwrap();
        let tool = ReadImageTool::new(ws.path().to_path_buf());
        for (region, want) in [
            (json!({"x": 100, "y": 0, "width": 10, "height": 10}), "outside"),
            (json!({"x": 0, "y": 0, "width": 0, "height": 10}), "empty"),
            (json!({"x": 0, "y": 0, "width": 10, "height": 10}), "512"),
            (json!({"x": "a", "y": 0, "width": 10, "height": 10}), "region.x"),
        ] {
            let err = tool.call_sync(&json!({"path": "a.png", "region": region})).unwrap_err().to_string();
            assert!(err.contains(want), "{region}: {err}");
        }
    }

    #[test]
    fn the_view_keeps_what_the_model_saw_after_the_source_changes() {
        let ws = tempfile::tempdir().unwrap();
        let src = ws.path().join("ui.png");
        image::RgbImage::from_pixel(40, 40, image::Rgb([255, 255, 255])).save(&src).unwrap();
        let first = read(ws.path(), json!({"path": "ui.png"}));
        let view = first["view"].as_str().unwrap();
        assert!(view.starts_with(".groka/seen/"), "{first}");
        image::RgbImage::from_pixel(40, 40, image::Rgb([0, 0, 0])).save(&src).unwrap();
        let second = read(ws.path(), json!({"path": "ui.png"}));
        assert_ne!(second["view"], first["view"]);
        assert!(sent(ws.path(), &first).pixels().all(|p| p[0] > 200), "first view is still white");
        assert!(ws.path().join(".groka/.gitignore").is_file());
    }

    #[test]
    fn every_listed_format_and_exif_orientation_is_read_by_content() {
        let ws = tempfile::tempdir().unwrap();
        let img = image::DynamicImage::ImageRgb8(image::RgbImage::from_pixel(40, 30, image::Rgb([9, 99, 199])));
        for name in ["a.gif", "a.webp", "a.bmp", "a.jfif"] {
            let format = image::ImageFormat::from_path(name).unwrap();
            img.write_to(&mut fs::File::create(ws.path().join(name)).unwrap(), format).unwrap();
        }
        img.write_to(&mut fs::File::create(ws.path().join("png.jpg")).unwrap(), image::ImageFormat::Png).unwrap();
        for name in ["a.gif", "a.webp", "a.bmp", "a.jfif", "png.jpg"] {
            assert!(crate::vision::is_image_ext(Path::new(name)), "{name}");
            let out = read(ws.path(), json!({"path": name}));
            assert_eq!(out["attach_image"], true, "{name}: {out}");
            assert_eq!((out["width"].as_u64(), out["height"].as_u64()), (Some(40), Some(30)), "{name}");
            assert_eq!(sent(ws.path(), &out).dimensions(), (40, 30), "{name}");
        }
        rotated_jpeg(&ws.path().join("phone.jpg"));
        let out = read(ws.path(), json!({"path": "phone.jpg"}));
        assert_eq!((out["width"].as_u64(), out["height"].as_u64()), (Some(32), Some(64)), "{out}");
        assert_eq!(sent(ws.path(), &out).dimensions(), (32, 64));
    }

    #[test]
    fn an_oversized_source_file_is_refused() {
        let ws = tempfile::tempdir().unwrap();
        let f = fs::File::create(ws.path().join("huge.png")).unwrap();
        f.set_len(33 * 1024 * 1024).unwrap();
        let err = ReadImageTool::new(ws.path().to_path_buf())
            .call_sync(&json!({"path": "huge.png"}))
            .unwrap_err();
        assert!(err.to_string().contains("32MiB"), "{err}");
    }

    #[test]
    fn the_registry_call_runs_on_the_blocking_pool() {
        let ws = tempfile::tempdir().unwrap();
        image::RgbImage::new(40, 40).save(ws.path().join("a.png")).unwrap();
        let rt = tokio::runtime::Builder::new_current_thread().max_blocking_threads(1).build().unwrap();
        rt.block_on(async {
            // Hold the only blocking thread, so a read sent to the pool cannot
            // finish on its first poll, as one run inline would.
            let (release, held) = std::sync::mpsc::channel::<()>();
            let holder = tokio::task::spawn_blocking(move || held.recv());
            let reg = ToolRegistry::new(vec![Box::new(ReadImageTool::new(ws.path().to_path_buf()))]);
            let args = json!({"path": "a.png"});
            let mut call = std::pin::pin!(reg.call("read_image", &args));
            assert!(futures::poll!(call.as_mut()).is_pending());
            release.send(()).unwrap();
            holder.await.unwrap().unwrap();
            let out: Value = serde_json::from_str(&call.await.unwrap()).unwrap();
            assert_eq!(out["attach_image"], true, "{out}");
        });
    }

    #[test]
    fn without_a_seen_copy_the_attach_still_sends_what_was_read() {
        let ws = tempfile::tempdir().unwrap();
        // `.groka` cannot be created: as in a read-only checkout.
        fs::write(ws.path().join(".groka"), "not a folder").unwrap();
        let page = image::RgbImage::from_fn(640, 2560, |_, y| {
            image::Rgb(if y < 1280 { [255; 3] } else { [0; 3] })
        });
        page.save(ws.path().join("page.png")).unwrap();
        let out = read(ws.path(), json!({"path": "page.png"}));
        assert!(out.get("view").is_none(), "{out}");
        assert_eq!(sent(ws.path(), &out).dimensions(), (320, 1280));
        let region = json!({"x": 0, "y": 1280, "width": 640, "height": 1280});
        let out = read(ws.path(), json!({"path": "page.png", "region": region}));
        assert!(out.get("view").is_none(), "{out}");
        let pixels = sent(ws.path(), &out);
        assert_eq!(pixels.dimensions(), (640, 1280));
        assert!(pixels.pixels().all(|p| p[0] < 40), "only the black half was sent");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[test]
    fn saved_outputs_keep_only_the_newest_within_count_and_size() {
        let dir = tempfile::tempdir().unwrap();
        for i in 0..6 {
            std::fs::write(dir.path().join(format!("cmd-2026010{i}.log")), vec![b'x'; 100]).unwrap();
        }
        std::fs::write(dir.path().join("notes.txt"), "not ours").unwrap();
        prune_saved_outputs(dir.path(), 4, u64::MAX);
        let mut left: Vec<String> =
            std::fs::read_dir(dir.path()).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
        left.sort();
        assert_eq!(left, ["cmd-20260102.log", "cmd-20260103.log", "cmd-20260104.log", "cmd-20260105.log", "notes.txt"]);
        // The size cap: 250 bytes keeps two of 100 bytes.
        prune_saved_outputs(dir.path(), 50, 250);
        assert!(dir.path().join("cmd-20260105.log").exists());
        assert!(dir.path().join("cmd-20260104.log").exists());
        assert!(!dir.path().join("cmd-20260103.log").exists());
        // The newest stays even when it alone is over the cap.
        prune_saved_outputs(dir.path(), 50, 10);
        assert!(dir.path().join("cmd-20260105.log").exists());
        assert!(!dir.path().join("cmd-20260104.log").exists());
    }

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
    fn write_target_rejects_symlink_escape() {
        let dir = tempfile::tempdir().unwrap();
        let outside = dir.path().join("outside");
        let workspace = dir.path().join("ws");
        fs::create_dir_all(&outside).unwrap();
        fs::create_dir_all(&workspace).unwrap();
        let link = workspace.join("leak");
        let linked = {
            #[cfg(unix)]
            {
                std::os::unix::fs::symlink(&outside, &link).is_ok()
            }
            #[cfg(windows)]
            {
                std::os::windows::fs::symlink_dir(&outside, &link).is_ok()
            }
        };
        if !linked {
            // Symlink creation needs privileges on some Windows setups.
            return;
        }
        let err = resolve_target_in_workspace(&workspace, "leak/evil.txt").unwrap_err();
        assert!(err.to_string().contains("escapes"), "{err}");
        let ok = resolve_target_in_workspace(&workspace, "safe.txt").unwrap();
        assert!(ok.starts_with(workspace.canonicalize().unwrap()));
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
    fn write_text_at_replaces_one_line_with_several() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("a.txt"), "one\ntwo\nthree\n").unwrap();
        let out: Value = serde_json::from_str(
            &write_text_at(
                dir.path(),
                &json!({
                    "path": "a.txt",
                    "line": 2,
                    "contents": "TWO-A\nTWO-B\n"
                }),
            )
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
    fn write_text_at_replaces_a_line_range() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("a.txt"), "a\nb\nc\nd\n").unwrap();
        write_text_at(
            dir.path(),
            &json!({
                "path": "a.txt",
                "line": 2,
                "end_line": 3,
                "contents": "X\nY\nZ"
            }),
        )
        .unwrap();
        assert_eq!(
            fs::read_to_string(dir.path().join("a.txt")).unwrap(),
            "a\nX\nY\nZ\nd\n"
        );
    }

    #[test]
    fn write_text_at_appends_when_line_is_len_plus_one() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("a.txt"), "a\nb\n").unwrap();
        write_text_at(
            dir.path(),
            &json!({
                "path": "a.txt",
                "line": 3,
                "contents": "c\nd"
            }),
        )
        .unwrap();
        assert_eq!(
            fs::read_to_string(dir.path().join("a.txt")).unwrap(),
            "a\nb\nc\nd\n"
        );
    }

    #[test]
    fn write_text_at_regex_replaces_every_match() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("a.txt"), "foo 1\nbar\nfoo 2\n").unwrap();
        let out: Value = serde_json::from_str(
            &write_text_at(
                dir.path(),
                &json!({
                    "path": "a.txt",
                    "pattern": r"foo (\d+)",
                    "contents": "FOO-$1"
                }),
            )
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
    fn write_text_at_rejects_line_and_pattern_together() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("a.txt"), "x\n").unwrap();
        let err = write_text_at(
            dir.path(),
            &json!({
                "path": "a.txt",
                "line": 1,
                "pattern": "x",
                "contents": "y"
            }),
        )
        .unwrap_err();
        assert!(err.to_string().contains("either line or pattern"), "{err}");
    }

    #[test]
    fn write_text_at_line_out_of_range_errors() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("a.txt"), "only\n").unwrap();
        let err = write_text_at(
            dir.path(),
            &json!({"path": "a.txt", "line": 5, "contents": "no"}),
        )
        .unwrap_err();
        assert!(err.to_string().contains("past end"), "{err}");
    }

    #[test]
    fn write_file_rejects_existing() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("a.txt"), "keep\n").unwrap();
        let tool = WriteFileTool::new(dir.path().to_path_buf());
        let err = tool
            .call_sync(&json!({"path": "a.txt", "contents": "nope\n"}))
            .unwrap_err();
        assert!(err.to_string().contains("file exists"), "{err}");
        assert!(err.to_string().contains("edit_file"), "{err}");
        assert_eq!(fs::read_to_string(dir.path().join("a.txt")).unwrap(), "keep\n");
    }

    fn file_pair(dir: &tempfile::TempDir) -> (ReadFileTool, EditFileTool) {
        let views = FileViewStore::new();
        let ws = dir.path().to_path_buf();
        (
            ReadFileTool::with_views(ws.clone(), views.clone()),
            EditFileTool::new(ws, views),
        )
    }

    #[test]
    fn edit_file_never_read_refuses_once_and_shows_the_file() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("a.txt"), "one\ntwo\n").unwrap();
        let views = FileViewStore::new();
        let edit = EditFileTool::new(dir.path().to_path_buf(), views);
        let args = json!({"path": "a.txt", "old_string": "two", "new_string": "TWO"});
        let err = edit.call_sync(&args).unwrap_err().to_string();
        assert!(err.contains("read a.txt before editing"), "{err}");
        assert!(err.contains("2|two"), "the refusal carries the numbered file: {err}");
        assert_eq!(fs::read_to_string(dir.path().join("a.txt")).unwrap(), "one\ntwo\n");
        edit.call_sync(&args).unwrap();
        assert_eq!(fs::read_to_string(dir.path().join("a.txt")).unwrap(), "one\nTWO\n");
    }

    #[test]
    fn edit_file_applies_unified_diff_after_read() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("a.txt"), "one\ntwo\nthree\n").unwrap();
        let (read, edit) = file_pair(&dir);
        read.call_sync(&json!({"path": "a.txt"})).unwrap();
        let out: Value = serde_json::from_str(
            &edit
                .call_sync(&json!({
                    "path": "a.txt",
                    "diff": "--- a/a.txt\n+++ b/a.txt\n@@ -1,3 +1,4 @@\n one\n-two\n+TWO\n+TWO-B\n three\n"
                }))
                .unwrap(),
        )
        .unwrap();
        assert_eq!(out["kind"], "modify");
        assert!(out["diff"].as_str().unwrap().contains("-two"), "{out}");
        assert_eq!(
            fs::read_to_string(dir.path().join("a.txt")).unwrap(),
            "one\nTWO\nTWO-B\nthree\n"
        );
    }

    #[test]
    fn edit_file_on_a_changed_file_needs_an_exact_match() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("a.txt"), "one\ntwo\nthree\n").unwrap();
        let (read, edit) = file_pair(&dir);
        read.call_sync(&json!({"path": "a.txt"})).unwrap();
        fs::write(dir.path().join("a.txt"), "zero\none\nchanged\nthree\n").unwrap();
        let err = edit
            .call_sync(&json!({"path": "a.txt", "diff": "@@ -1,2 +1,2 @@\n one\n-two\n+TWO\n"}))
            .unwrap_err()
            .to_string();
        assert!(err.contains("changed since you last saw it"), "{err}");
        assert!(err.contains("3|changed"), "shows the current text: {err}");
        // The untouched part still matches exactly: that edit goes through.
        let out: Value = serde_json::from_str(
            &edit.call_sync(&json!({"path": "a.txt", "old_string": "three", "new_string": "THREE"})).unwrap(),
        )
        .unwrap();
        assert!(out["notes"][0].as_str().unwrap().contains("changed since"), "{out}");
        assert_eq!(fs::read_to_string(dir.path().join("a.txt")).unwrap(), "zero\none\nchanged\nTHREE\n");
    }

    #[test]
    fn edit_file_can_edit_again_without_rereading() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("a.txt"), "one\ntwo\n").unwrap();
        let (read, edit) = file_pair(&dir);
        read.call_sync(&json!({"path": "a.txt"})).unwrap();
        edit.call_sync(&json!({"path": "a.txt", "diff": "@@ -1,2 +1,2 @@\n one\n-two\n+TWO\n"})).unwrap();
        edit.call_sync(&json!({"path": "a.txt", "old_string": "TWO", "new_string": "2"})).unwrap();
        assert_eq!(fs::read_to_string(dir.path().join("a.txt")).unwrap(), "one\n2\n");
    }

    #[test]
    fn edit_file_after_own_write_file_needs_no_read() {
        let dir = tempfile::tempdir().unwrap();
        let views = FileViewStore::new();
        let write = WriteFileTool::with_views(dir.path().to_path_buf(), views.clone());
        let edit = EditFileTool::new(dir.path().to_path_buf(), views);
        write.call_sync(&json!({"path": "n.py", "contents": "x = 1\n"})).unwrap();
        edit.call_sync(&json!({"path": "n.py", "old_string": "x = 1", "new_string": "x = 2"})).unwrap();
        assert_eq!(fs::read_to_string(dir.path().join("n.py")).unwrap(), "x = 2\n");
    }

    #[test]
    fn edit_file_replace_mode_rules() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("a.txt"), "a = 1\nb = 1\n").unwrap();
        let (read, edit) = file_pair(&dir);
        read.call_sync(&json!({"path": "a.txt"})).unwrap();
        let err = edit
            .call_sync(&json!({"path": "a.txt", "old_string": "= 1", "new_string": "= 2"}))
            .unwrap_err()
            .to_string();
        assert!(err.contains("occurs 2 times (lines 1, 2)"), "{err}");
        let err = edit
            .call_sync(&json!({"path": "a.txt", "old_string": "c = 1", "new_string": "c = 2"}))
            .unwrap_err()
            .to_string();
        assert!(err.contains("not found"), "{err}");
        let err = edit
            .call_sync(&json!({"path": "a.txt", "old_string": "a", "new_string": "b", "diff": "@@\n-a\n+b\n"}))
            .unwrap_err()
            .to_string();
        assert!(err.contains("not both"), "{err}");
        edit.call_sync(&json!({"path": "a.txt", "old_string": "= 1", "new_string": "= 2", "replace_all": true}))
            .unwrap();
        assert_eq!(fs::read_to_string(dir.path().join("a.txt")).unwrap(), "a = 2\nb = 2\n");
    }

    #[test]
    fn edit_file_replace_keeps_crlf() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("w.txt"), "a\r\nb\r\nc\r\n").unwrap();
        let (read, edit) = file_pair(&dir);
        read.call_sync(&json!({"path": "w.txt"})).unwrap();
        edit.call_sync(&json!({"path": "w.txt", "old_string": "a\nb", "new_string": "a\nB\nB2"})).unwrap();
        assert_eq!(fs::read_to_string(dir.path().join("w.txt")).unwrap(), "a\r\nB\r\nB2\r\nc\r\n");
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
            _shell: crate::shellguard::ShellKind,
            _flagged: &'a [String],
        ) -> Pin<Box<dyn Future<Output = Result<crate::shellguard::Verdict>> + Send + 'a>> {
            Box::pin(async {
                Ok(crate::shellguard::Verdict::Deny {
                    reasons: vec!["nope".into()],
                })
            })
        }
    }

    #[tokio::test]
    async fn a_timeout_keeps_the_output_so_far_and_stops_the_whole_tree() {
        let dir = tempfile::tempdir().unwrap();
        let tool = RunCommandTool::new(dir.path().to_path_buf());
        let started = std::time::Instant::now();
        let out: Value = serde_json::from_str(
            &tool
                .run(&json!({
                    "command": "echo started; (sleep 4; echo late > late.txt) & sleep 30",
                    "timeout": 2
                }))
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(out["timed_out"], true);
        assert!(out["stdout"].as_str().unwrap().contains("started"), "{out}");
        assert!(out["hint"].as_str().unwrap().contains("process tree"));
        assert!(started.elapsed() < Duration::from_secs(15), "{:?}", started.elapsed());
        tokio::time::sleep(Duration::from_secs(4)).await;
        assert!(!dir.path().join("late.txt").exists(), "the background child was stopped too");
    }

    #[tokio::test]
    async fn long_output_keeps_the_end_and_saves_the_rest() {
        let dir = tempfile::tempdir().unwrap();
        let tool = RunCommandTool::new(dir.path().to_path_buf());
        let out: Value = serde_json::from_str(
            &tool.run(&json!({"command": "seq 1 20000; echo FINAL-SUMMARY 1>&2"})).await.unwrap(),
        )
        .unwrap();
        let stdout = out["stdout"].as_str().unwrap();
        assert!(stdout.starts_with("1
2
"), "keeps the start");
        assert!(stdout.trim_end().ends_with("20000"), "keeps the end");
        assert!(stdout.contains("characters omitted"), "says what was cut");
        assert!(out["stderr"].as_str().unwrap().contains("FINAL-SUMMARY"));
        let saved = out["full_output"].as_str().expect("full output saved");
        let text = fs::read_to_string(dir.path().join(saved)).unwrap();
        assert!(text.contains("
10000
"), "the middle is in the file");
        assert_eq!(fs::read_to_string(dir.path().join(".groka/.gitignore")).unwrap().trim(), "*");
    }

    #[tokio::test]
    async fn timeout_argument_is_validated() {
        let dir = tempfile::tempdir().unwrap();
        let tool = RunCommandTool::new(dir.path().to_path_buf());
        let err = tool.run(&json!({"command": "echo hi", "timeout": 6000})).await.unwrap_err();
        assert!(err.to_string().contains("1..=600"), "{err}");
    }

    #[test]
    fn the_model_does_not_see_ui_diffs() {
        let out = r#"{"exit_code":0,"files":[{"path":"a.rs","plus":1,"minus":0}],"_diffs":[{"path":"a.rs","diff":"+x"}]}"#;
        let shown = model_view(out);
        assert!(!shown.contains("_diffs") && shown.contains("a.rs"), "{shown}");
        assert_eq!(model_view("plain"), "plain");
    }

    #[tokio::test]
    async fn run_command_blocked_does_not_execute() {
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("pwned.txt");
        let tool = RunCommandTool::with_guard(dir.path().to_path_buf(), Arc::new(DenyAll));
        // Flagged under every shell: a home-folder secret (bash) and a
        // compound command (cmd).
        let command = "echo pwned > pwned.txt && cat ~/.ssh/id_rsa";
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
        // With nothing the agent started on screen, it lists instead of capturing.
        if out["attach_image"] == false {
            assert!(out["error"].as_str().is_some(), "{out}");
            assert!(out["windows"].is_array(), "{out}");
            assert!(!dir.path().join("w.jpg").exists());
            return;
        }
        assert_eq!(out["source"], "window", "{out}");
        assert!(out["title"].as_str().unwrap().len() >= 1, "{out}");
        assert!(out["app"].as_str().is_some(), "{out}");
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

    #[test]
    fn a_named_shot_path_keeps_an_immutable_copy_for_its_row() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("ui.jpg");
        let blue = crate::vision::from_rgba(32, 32, vec![0, 0, 255, 255].repeat(32 * 32)).unwrap();
        let first = with_seen_view(dir.path(), &target, shot_json(&target, "ui.jpg", &blue, json!({})).unwrap());
        let first: Value = serde_json::from_str(&first).unwrap();
        let view = first["view"].as_str().unwrap().to_string();
        assert!(view.starts_with(".groka/seen/"), "{first}");
        let seen = fs::read(dir.path().join(&view)).unwrap();
        let red = crate::vision::from_rgba(32, 32, vec![255, 0, 0, 255].repeat(32 * 32)).unwrap();
        shot_json(&target, "ui.jpg", &red, json!({})).unwrap();
        assert_eq!(fs::read(dir.path().join(&view)).unwrap(), seen, "the first row still shows the first shot");
        let none = json!({"attach_image": false}).to_string();
        assert_eq!(with_seen_view(dir.path(), &target, none.clone()), none);
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
