//! Project notes stored *outside* the workspace, as multiple files.
//!
//! Root: `~/.grokaagent/memory/<key>/files/` (override `GROKA_MEMORY_DIR`).
//! The model reads and writes these notes; they never go into the project tree.

use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::error::{Error, Result};
use crate::tools::{resolve_in_workspace, resolve_target_in_workspace, ClientTool, ToolCallFut, ToolSpec};

pub const MAX_FILE_BYTES: usize = 32 * 1024;
pub const MAX_FILES: usize = 64;

#[derive(Debug, Clone)]
pub struct MemoryStore {
    root: PathBuf,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct SlotMeta {
    workspace: String,
}

impl MemoryStore {
    pub fn open() -> Result<Self> {
        Ok(Self::open_at(default_dir()?))
    }

    pub fn open_at(root: PathBuf) -> Self {
        Self { root }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn slot(&self, workspace: &Path) -> Result<Slot> {
        let want = normalize_workspace(workspace)?;
        let key = workspace_key(&want);
        let mut dir = self.root.join(&key);
        if let Some(meta) = read_meta(&dir)? {
            if meta.workspace != want {
                dir = unique_slot(&self.root, &key, &want)?;
            }
        }
        Ok(Slot {
            dir,
            workspace: want,
        })
    }
}

pub struct Slot {
    dir: PathBuf,
    workspace: String,
}

impl Slot {
    fn files_dir(&self) -> PathBuf {
        self.dir.join("files")
    }

    fn ensure(&self) -> Result<()> {
        fs::create_dir_all(self.files_dir())?;
        let meta = SlotMeta {
            workspace: self.workspace.clone(),
        };
        atomic_write(
            &self.dir.join("meta.json"),
            &serde_json::to_vec_pretty(&meta)?,
        )?;
        Ok(())
    }

    pub fn list(&self) -> Result<Vec<String>> {
        let files = self.files_dir();
        if !files.is_dir() {
            return Ok(Vec::new());
        }
        let mut out = Vec::new();
        collect_files(&files, &files, &mut out)?;
        out.sort();
        if out.len() > MAX_FILES {
            out.truncate(MAX_FILES);
        }
        Ok(out)
    }

    pub fn read(&self, path: &str) -> Result<String> {
        let rel = normalize_rel(path)?;
        let files = self.files_dir();
        if !files.is_dir() {
            return Err(Error::Tool(format!("memory file not found: {rel}")));
        }
        let resolved = resolve_in_workspace(&files, &rel)?;
        let bytes = fs::read(&resolved)?;
        if bytes.len() > MAX_FILE_BYTES {
            return Err(Error::Tool(format!("memory file larger than {MAX_FILE_BYTES} bytes")));
        }
        String::from_utf8(bytes).map_err(|_| Error::Tool("memory file is not valid UTF-8".into()))
    }

    pub fn write(&self, path: &str, body: &str) -> Result<String> {
        let rel = normalize_rel(path)?;
        if body.len() > MAX_FILE_BYTES {
            return Err(Error::Tool(format!(
                "memory file larger than {MAX_FILE_BYTES} bytes; condense it"
            )));
        }
        self.ensure()?;
        let files = self.files_dir();
        let target = resolve_target_in_workspace(&files, &rel)?;
        if self.list()?.len() >= MAX_FILES && !target.exists() {
            return Err(Error::Tool(format!("at most {MAX_FILES} memory files")));
        }
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent)?;
        }
        atomic_write(&target, body.as_bytes())?;
        Ok(rel)
    }

    pub fn delete(&self, path: &str) -> Result<String> {
        let rel = normalize_rel(path)?;
        let files = self.files_dir();
        if !files.is_dir() {
            return Err(Error::Tool(format!("memory file not found: {rel}")));
        }
        let resolved = resolve_in_workspace(&files, &rel)?;
        fs::remove_file(&resolved)?;
        Ok(rel)
    }
}

pub fn default_dir() -> Result<PathBuf> {
    if let Ok(p) = std::env::var("GROKA_MEMORY_DIR") {
        return Ok(PathBuf::from(p));
    }
    let home = dirs::home_dir().ok_or_else(|| Error::Auth("cannot resolve home directory".into()))?;
    Ok(home.join(".grokaagent").join("memory"))
}

pub struct ProjectMemoryTool {
    store: MemoryStore,
    workspace: PathBuf,
}

impl ProjectMemoryTool {
    pub fn new(memory_root: PathBuf, workspace: PathBuf) -> Self {
        Self {
            store: MemoryStore::open_at(memory_root),
            workspace,
        }
    }

    pub fn call_sync(&self, args: &Value) -> Result<String> {
        let action = args
            .get("action")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim();
        let slot = self.store.slot(&self.workspace)?;
        match action {
            "list" => {
                let files = slot.list()?;
                Ok(json!({ "files": files, "count": files.len() }).to_string())
            }
            "read" => {
                let path = req_path(args)?;
                let rel = normalize_rel(path)?;
                let files = slot.files_dir();
                if !files.is_dir() {
                    return Err(Error::Tool(format!("memory file not found: {rel}")));
                }
                crate::tools::read_text_at(&files, args)
            }
            "write" => {
                let mut args = args.clone();
                if args.get("contents").and_then(Value::as_str).is_none() {
                    if let Some(body) = args.get("body").cloned() {
                        if let Some(obj) = args.as_object_mut() {
                            obj.insert("contents".into(), body);
                        }
                    }
                }
                let path = req_path(&args)?;
                let rel = normalize_rel(path)?;
                let contents = args
                    .get("contents")
                    .and_then(Value::as_str)
                    .ok_or_else(|| Error::Tool("contents is required for write".into()))?;
                if contents.len() > MAX_FILE_BYTES {
                    return Err(Error::Tool(format!(
                        "memory file larger than {MAX_FILE_BYTES} bytes; condense it"
                    )));
                }
                slot.ensure()?;
                let files = slot.files_dir();
                let target = resolve_target_in_workspace(&files, &rel)?;
                if slot.list()?.len() >= MAX_FILES && !target.exists() {
                    return Err(Error::Tool(format!("at most {MAX_FILES} memory files")));
                }
                crate::tools::write_text_at(&files, &args)
            }
            "delete" => {
                let path = req_path(args)?;
                let path = slot.delete(path)?;
                Ok(json!({ "ok": true, "deleted": path }).to_string())
            }
            _ => Err(Error::Tool(
                "action must be list, read, write, or delete".into(),
            )),
        }
    }
}

impl ClientTool for ProjectMemoryTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "project_memory".into(),
            description: "Persistent notes for THIS workspace, stored outside the project (not in git). Multiple files allowed (goal.md, done.md, constraints.md, …). Notes are not in context until you list/read them. Read and write use the same line numbers, slices, regex, and unified diff as read_file/write_file. Record overall requirements, constraints, and what was done. Never put secrets here and never write these notes into the workspace.".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "action": {
                        "type": "string",
                        "enum": ["list", "read", "write", "delete"],
                        "description": "list files, read one file (numbered like read_file), write/edit one file (like write_file), or delete one file"
                    },
                    "path": {
                        "type": "string",
                        "description": "Relative path inside the memory store (required except list)"
                    },
                    "contents": {
                        "type": "string",
                        "description": "Write: full file, replacement lines, or regex replacement. Newlines write multiple lines."
                    },
                    "line": {
                        "type": "integer",
                        "description": "Write: 1-based start line to replace (from read). Omit with pattern for regex; omit both to overwrite the whole file."
                    },
                    "end_line": {
                        "type": "integer",
                        "description": "Write: 1-based inclusive end line. Default is line. Read: last line to return."
                    },
                    "pattern": {
                        "type": "string",
                        "description": "Read: keep matching lines. Write: regex replace-all ($1 captures). Do not combine write pattern with line."
                    },
                    "start_line": {
                        "type": "integer",
                        "description": "Read: 1-based first line to return. Default 1."
                    },
                    "context": {
                        "type": "integer",
                        "description": "Read with pattern: extra lines before/after each match. Default 0."
                    },
                    "max_matches": {
                        "type": "integer",
                        "description": "Read with pattern: cap returned matches. Default 80."
                    }
                },
                "required": ["action"],
                "additionalProperties": false
            }),
        }
    }

    fn call(&self, args: &Value) -> ToolCallFut<'_> {
        let result = self.call_sync(args);
        Box::pin(async move { result })
    }
}

fn req_path(args: &Value) -> Result<&str> {
    args.get("path")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| Error::Tool("path is required".into()))
}

fn normalize_rel(path: &str) -> Result<String> {
    let path = path.trim().replace('\\', "/");
    if path.is_empty() || path == "." || path.starts_with('/') || path.contains(':') {
        return Err(Error::Tool("memory path must be a relative file".into()));
    }
    if path.split('/').any(|p| p.is_empty() || p == "." || p == "..") {
        return Err(Error::Tool("memory path escapes the store".into()));
    }
    if path.chars().count() > 180 {
        return Err(Error::Tool("memory path too long".into()));
    }
    Ok(path)
}

fn normalize_workspace(workspace: &Path) -> Result<String> {
    let abs = if workspace.is_absolute() {
        workspace.to_path_buf()
    } else {
        std::env::current_dir()?.join(workspace)
    };
    let canon = abs.canonicalize().unwrap_or(abs);
    let mut s = crate::folderpick::display_path(&canon).replace('\\', "/");
    if s.len() >= 2 && s.as_bytes()[1] == b':' {
        let mut chars = s.chars();
        if let Some(drive) = chars.next() {
            s = format!("{}{}", drive.to_ascii_lowercase(), chars.as_str());
        }
    }
    Ok(s)
}

fn workspace_key(norm: &str) -> String {
    let mut h0: u64 = 0xcbf2_9ce4_8422_2325;
    let mut h1: u64 = 0x6c62_272e_07bb_0142;
    for (i, b) in norm.as_bytes().iter().enumerate() {
        h0 ^= *b as u64;
        h0 = h0.wrapping_mul(0x0100_0000_01b3);
        h1 ^= (*b as u64).wrapping_add(i as u64);
        h1 = h1.rotate_left(11).wrapping_mul(0x0100_0000_01b3);
    }
    format!("{h0:016x}{h1:016x}")
}

fn unique_slot(root: &Path, key: &str, want: &str) -> Result<PathBuf> {
    for n in 2..32 {
        let dir = root.join(format!("{key}-{n}"));
        match read_meta(&dir)? {
            Some(meta) if meta.workspace == want => return Ok(dir),
            Some(_) => continue,
            None => return Ok(dir),
        }
    }
    Err(Error::Tool("memory slot collision".into()))
}

fn read_meta(dir: &Path) -> Result<Option<SlotMeta>> {
    let path = dir.join("meta.json");
    if !path.exists() {
        return Ok(None);
    }
    let raw = fs::read(path)?;
    Ok(Some(serde_json::from_slice(&raw)?))
}

fn collect_files(root: &Path, dir: &Path, out: &mut Vec<String>) -> io::Result<()> {
    if out.len() >= MAX_FILES {
        return Ok(());
    }
    let rd = match fs::read_dir(dir) {
        Ok(rd) => rd,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e),
    };
    for ent in rd {
        if out.len() >= MAX_FILES {
            break;
        }
        let ent = ent?;
        let path = ent.path();
        let ft = ent.file_type()?;
        if ft.is_dir() {
            collect_files(root, &path, out)?;
        } else if ft.is_file() {
            if let Ok(rel) = path.strip_prefix(root) {
                out.push(rel.to_string_lossy().replace('\\', "/"));
            }
        }
    }
    Ok(())
}

fn atomic_write(path: &Path, bytes: &[u8]) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("tmp");
    {
        let mut f = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&tmp)?;
        f.write_all(bytes)?;
        f.flush()?;
    }
    if path.exists() {
        fs::remove_file(path)?;
    }
    fs::rename(&tmp, path)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> (tempfile::TempDir, MemoryStore) {
        let dir = tempfile::tempdir().unwrap();
        let store = MemoryStore::open_at(dir.path().to_path_buf());
        (dir, store)
    }

    #[test]
    fn write_read_list_delete_multiple_files() {
        let (_root, store) = store();
        let ws = tempfile::tempdir().unwrap();
        let slot = store.slot(ws.path()).unwrap();
        slot.write("goal.md", "ship the kernel").unwrap();
        slot.write("done.md", "login works").unwrap();
        slot.write("notes/ui.md", "TUI chat").unwrap();
        let files = slot.list().unwrap();
        assert_eq!(files, vec!["done.md", "goal.md", "notes/ui.md"]);
        assert_eq!(slot.read("goal.md").unwrap(), "ship the kernel");
        slot.delete("done.md").unwrap();
        assert_eq!(slot.list().unwrap(), vec!["goal.md", "notes/ui.md"]);
        assert!(!ws.path().join("goal.md").exists());
        assert!(!ws.path().join("done.md").exists());
        let dumped = fs::read_dir(ws.path()).unwrap().count();
        assert_eq!(dumped, 0, "memory must not create files in the workspace");
    }

    #[test]
    fn two_workspaces_are_isolated() {
        let (_root, store) = store();
        let a = tempfile::tempdir().unwrap();
        let b = tempfile::tempdir().unwrap();
        store.slot(a.path()).unwrap().write("goal.md", "A").unwrap();
        store.slot(b.path()).unwrap().write("goal.md", "B").unwrap();
        assert_eq!(store.slot(a.path()).unwrap().read("goal.md").unwrap(), "A");
        assert_eq!(store.slot(b.path()).unwrap().read("goal.md").unwrap(), "B");
    }

    #[test]
    fn rejects_escape_and_oversize() {
        let (_root, store) = store();
        let ws = tempfile::tempdir().unwrap();
        let slot = store.slot(ws.path()).unwrap();
        let err = slot.write("../x.md", "no").unwrap_err();
        assert!(err.to_string().contains("escapes") || err.to_string().contains("relative"), "{err}");
        let big = "x".repeat(MAX_FILE_BYTES + 1);
        let err = slot.write("big.md", &big).unwrap_err();
        assert!(err.to_string().contains("larger"), "{err}");
    }

    #[test]
    fn tool_write_stays_out_of_workspace() {
        let mem = tempfile::tempdir().unwrap();
        let ws = tempfile::tempdir().unwrap();
        let tool = ProjectMemoryTool::new(mem.path().to_path_buf(), ws.path().to_path_buf());
        let out: Value = serde_json::from_str(
            &tool
                .call_sync(&json!({
                    "action": "write",
                    "path": "goal.md",
                    "body": "overall: TUI agent"
                }))
                .unwrap(),
        )
        .unwrap();
        assert_eq!(out["path"], "goal.md");
        assert!(
            out["diff"].as_str().is_some_and(|d| d.contains("overall")),
            "{out}"
        );
        let listed: Value =
            serde_json::from_str(&tool.call_sync(&json!({"action": "list"})).unwrap()).unwrap();
        assert_eq!(listed["files"][0], "goal.md");
        assert!(!ws.path().join("goal.md").exists());
        assert!(mem.path().exists());
    }

    #[test]
    fn tool_read_is_numbered_like_read_file() {
        let mem = tempfile::tempdir().unwrap();
        let ws = tempfile::tempdir().unwrap();
        let tool = ProjectMemoryTool::new(mem.path().to_path_buf(), ws.path().to_path_buf());
        tool.call_sync(&json!({
            "action": "write",
            "path": "goal.md",
            "contents": "alpha\nbeta\ngamma\n"
        }))
        .unwrap();
        let out = tool
            .call_sync(&json!({"action": "read", "path": "goal.md"}))
            .unwrap();
        assert!(out.contains("1|alpha"), "{out}");
        assert!(out.contains("2|beta"), "{out}");
        assert!(out.contains("3|gamma"), "{out}");
        assert!(!out.contains("\"body\""), "{out}");
    }

    #[test]
    fn tool_write_replaces_a_line_and_returns_diff() {
        let mem = tempfile::tempdir().unwrap();
        let ws = tempfile::tempdir().unwrap();
        let tool = ProjectMemoryTool::new(mem.path().to_path_buf(), ws.path().to_path_buf());
        tool.call_sync(&json!({
            "action": "write",
            "path": "goal.md",
            "contents": "one\ntwo\nthree\n"
        }))
        .unwrap();
        let out: Value = serde_json::from_str(
            &tool
                .call_sync(&json!({
                    "action": "write",
                    "path": "goal.md",
                    "line": 2,
                    "contents": "TWO\n"
                }))
                .unwrap(),
        )
        .unwrap();
        assert_eq!(out["path"], "goal.md");
        let diff = out["diff"].as_str().unwrap_or("");
        assert!(diff.contains("-two"), "{diff}");
        assert!(diff.contains("+TWO"), "{diff}");
        let slot = MemoryStore::open_at(mem.path().to_path_buf())
            .slot(ws.path())
            .unwrap();
        assert_eq!(slot.read("goal.md").unwrap(), "one\nTWO\nthree\n");
    }
}
