//! Local chat sessions: metadata, transcript, and Grok-CLI-style titles.

use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::error::{Error, Result};
use crate::provider::CompleteResponse;

const TITLE_SOURCE_MAX_BYTES: usize = 8_000;
const TITLE_FALLBACK_WORDS: usize = 10;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SessionMeta {
    pub id: String,
    /// Sidebar title. `"新對話"` until the first title pass (fallback or LLM).
    pub name: String,
    /// True after fallback or LLM title has been applied.
    #[serde(default)]
    pub named: bool,
    #[serde(default)]
    pub name_is_manual: bool,
    pub workspace: PathBuf,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

impl SessionMeta {
    pub fn new(workspace: PathBuf) -> Self {
        let now = Utc::now();
        Self {
            id: Uuid::new_v4().to_string(),
            name: "新對話".into(),
            named: false,
            name_is_manual: false,
            workspace,
            created_at: now,
            updated_at: now,
        }
    }

    /// `#` + first 8 hex chars of the UUID (dashes stripped).
    pub fn short_id(&self) -> String {
        let hex: String = self.id.chars().filter(|c| *c != '-').take(8).collect();
        format!("#{hex}")
    }

    pub fn folder_label(&self) -> String {
        self.workspace
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| self.workspace.display().to_string())
    }
}

pub struct SessionStore {
    root: PathBuf,
}

impl SessionStore {
    pub fn open() -> Result<Self> {
        Self::open_at(default_sessions_dir()?)
    }

    pub fn open_at(root: PathBuf) -> Result<Self> {
        fs::create_dir_all(&root)?;
        Ok(Self { root })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    fn dir(&self, id: &str) -> PathBuf {
        self.root.join(id)
    }

    pub fn create(&self, workspace: PathBuf) -> Result<SessionMeta> {
        let meta = SessionMeta::new(workspace);
        self.save_meta(&meta)?;
        Ok(meta)
    }

    pub fn save_meta(&self, meta: &SessionMeta) -> Result<()> {
        let dir = self.dir(&meta.id);
        fs::create_dir_all(&dir)?;
        atomic_write(&dir.join("meta.json"), &serde_json::to_vec_pretty(meta)?)?;
        Ok(())
    }

    pub fn load_meta(&self, id: &str) -> Result<SessionMeta> {
        let raw = fs::read(self.dir(id).join("meta.json"))?;
        Ok(serde_json::from_slice(&raw)?)
    }

    pub fn save_transcript(&self, id: &str, rows: &serde_json::Value) -> Result<()> {
        let dir = self.dir(id);
        fs::create_dir_all(&dir)?;
        atomic_write(&dir.join("transcript.json"), &serde_json::to_vec(rows)?)?;
        Ok(())
    }

    pub fn load_transcript(&self, id: &str) -> Result<serde_json::Value> {
        let path = self.dir(id).join("transcript.json");
        if !path.exists() {
            return Ok(serde_json::json!([]));
        }
        let raw = fs::read(path)?;
        Ok(serde_json::from_slice(&raw)?)
    }

    pub fn list(&self) -> Result<Vec<SessionMeta>> {
        let mut out = Vec::new();
        let rd = match fs::read_dir(&self.root) {
            Ok(rd) => rd,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(out),
            Err(e) => return Err(e.into()),
        };
        for ent in rd {
            let ent = ent?;
            if !ent.file_type()?.is_dir() {
                continue;
            }
            let meta_path = ent.path().join("meta.json");
            if !meta_path.exists() {
                continue;
            }
            let raw = match fs::read(&meta_path) {
                Ok(b) => b,
                Err(_) => continue,
            };
            if let Ok(meta) = serde_json::from_slice::<SessionMeta>(&raw) {
                out.push(meta);
            }
        }
        out.sort_by(|a, b| b.updated_at.cmp(&a.updated_at).then(b.id.cmp(&a.id)));
        Ok(out)
    }

    pub fn touch_name(&self, meta: &mut SessionMeta, name: String, _from_llm: bool) -> Result<()> {
        if meta.name_is_manual {
            return Ok(());
        }
        let name = sanitize_title(&name);
        if name.is_empty() {
            return Ok(());
        }
        meta.name = name;
        meta.named = true;
        meta.updated_at = Utc::now();
        self.save_meta(meta)
    }

    pub fn rename_manual(&self, meta: &mut SessionMeta, name: String) -> Result<()> {
        let name = sanitize_title(&name);
        if name.is_empty() {
            return Ok(());
        }
        meta.name = name;
        meta.named = true;
        meta.name_is_manual = true;
        meta.updated_at = Utc::now();
        self.save_meta(meta)
    }

    pub fn delete(&self, id: &str) -> Result<()> {
        let dir = self.dir(id);
        if dir.exists() {
            fs::remove_dir_all(&dir)?;
        }
        Ok(())
    }

    /// Persist backgrounds that were still alive when this conversation's run ended.
    /// No-op unless this id is a real session (`meta.json` exists).
    pub fn save_closed_backgrounds<T: Serialize>(&self, id: &str, items: &[T]) -> Result<()> {
        if items.is_empty() {
            return Ok(());
        }
        let dir = self.dir(id);
        if !dir.join("meta.json").exists() {
            return Ok(());
        }
        atomic_write(
            &dir.join("closed-backgrounds.json"),
            &serde_json::to_vec_pretty(items)?,
        )?;
        Ok(())
    }

    /// Read and delete the closed-background snapshot. Empty if none.
    pub fn take_closed_backgrounds<T: DeserializeOwned>(&self, id: &str) -> Vec<T> {
        let path = self.dir(id).join("closed-backgrounds.json");
        let Ok(raw) = fs::read(&path) else {
            return Vec::new();
        };
        let _ = fs::remove_file(&path);
        serde_json::from_slice(&raw).unwrap_or_default()
    }
}

pub fn default_sessions_dir() -> Result<PathBuf> {
    if let Ok(p) = std::env::var("GROKA_SESSIONS_DIR") {
        return Ok(PathBuf::from(p));
    }
    let home = dirs::home_dir().ok_or_else(|| Error::Auth("cannot resolve home directory".into()))?;
    Ok(home.join(".grokaagent").join("sessions"))
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

fn floor_char_boundary(s: &str, max_bytes: usize) -> usize {
    if s.len() <= max_bytes {
        return s.len();
    }
    let mut i = max_bytes;
    while i > 0 && !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

/// Text the session title is derived from: cap to the first few KB.
pub fn title_source_text(user_message: &str) -> String {
    let t = user_message.trim();
    let end = floor_char_boundary(t, TITLE_SOURCE_MAX_BYTES);
    t[..end].to_string()
}

pub fn title_fallback_from_user_text(user_message: &str) -> String {
    let text = title_source_text(user_message);
    let s = text
        .split_whitespace()
        .take(TITLE_FALLBACK_WORDS)
        .collect::<Vec<_>>()
        .join(" ");
    if s.is_empty() {
        "新對話".to_string()
    } else {
        s
    }
}

pub fn sanitize_title(raw: &str) -> String {
    let one_line: String = raw
        .lines()
        .next()
        .unwrap_or("")
        .trim()
        .trim_matches(|c| c == '"' || c == '\'')
        .chars()
        .take(80)
        .collect();
    one_line
}

/// Pull `session_title` from a forced tool call, else a short plaintext reply.
pub fn title_from_response(resp: &CompleteResponse) -> Option<String> {
    for call in &resp.function_calls {
        if call.name != "session_title" {
            continue;
        }
        let v: serde_json::Value = serde_json::from_str(&call.arguments).ok()?;
        let t = v.get("session_title").and_then(|x| x.as_str())?;
        let t = sanitize_title(t);
        if !t.is_empty() {
            return Some(t);
        }
    }
    let t = sanitize_title(&resp.text);
    if !t.is_empty() && t.chars().count() <= 80 {
        Some(t)
    } else {
        None
    }
}

pub fn session_title_system_prompt() -> &'static str {
    r#"You are tasked with generating the session title. The user is asking questions in a coding agent.
We describe the session title below
# Session Title
A short and distinctive 5-10 word descriptive title for the session. Super info dense, no filler.
Use the same language as the user query.

You will be given the user query below encapsulated in <user_query></user_query>.

Just generate the session_title and nothing else"#
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::background::ClosedBackground;
    use crate::provider::{CompleteResponse, FunctionCall};

    #[test]
    fn short_id_strips_dashes_and_takes_8() {
        let mut m = SessionMeta::new(PathBuf::from("/tmp/proj"));
        m.id = "a3f2c1d4-5678-4000-8000-000000000001".into();
        assert_eq!(m.short_id(), "#a3f2c1d4");
    }

    #[test]
    fn folder_label_uses_last_component() {
        let m = SessionMeta::new(PathBuf::from("C:/Users/jason/Desktop/grokaagent"));
        assert_eq!(m.folder_label(), "grokaagent");
        let unix = SessionMeta::new(PathBuf::from("/Users/jason/Desktop/grokaagent"));
        assert_eq!(unix.folder_label(), "grokaagent");
    }

    #[test]
    fn create_list_save_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let store = SessionStore::open_at(dir.path().to_path_buf()).unwrap();
        let a = store.create(PathBuf::from("/tmp/alpha")).unwrap();
        let mut b = store.create(PathBuf::from("/tmp/beta")).unwrap();
        b.name = "修 sidebar".into();
        b.named = true;
        store.save_meta(&b).unwrap();
        store
            .save_transcript(&a.id, &serde_json::json!([{"User":"hi"}]))
            .unwrap();

        let listed = store.list().unwrap();
        assert_eq!(listed.len(), 2);
        assert_eq!(listed[0].id, b.id, "most recently updated first");
        assert_eq!(listed[0].name, "修 sidebar");
        let loaded = store.load_meta(&a.id).unwrap();
        assert_eq!(loaded.workspace, PathBuf::from("/tmp/alpha"));
        let rows = store.load_transcript(&a.id).unwrap();
        assert_eq!(rows[0]["User"], "hi");
    }

    #[test]
    fn title_source_caps_and_is_utf8_safe() {
        let big = "word ".repeat(10_000);
        let out = title_source_text(&big);
        assert!(!out.is_empty() && out.len() <= TITLE_SOURCE_MAX_BYTES);

        let cjk = "あ".repeat(10_000);
        let out = title_source_text(&cjk);
        assert!(!out.is_empty() && out.len() <= TITLE_SOURCE_MAX_BYTES);
        assert!(out.is_char_boundary(out.len()));
    }

    #[test]
    fn fallback_trims_to_ten_words() {
        assert_eq!(
            title_fallback_from_user_text(
                "one two three four five six seven eight nine ten eleven"
            ),
            "one two three four five six seven eight nine ten"
        );
        assert_eq!(title_fallback_from_user_text(" \n\t"), "新對話");
        assert_eq!(
            title_fallback_from_user_text("修 auth bug in login.rs"),
            "修 auth bug in login.rs"
        );
    }

    #[test]
    fn title_from_forced_tool_call() {
        let resp = CompleteResponse {
            id: "r".into(),
            text: String::new(),
            function_calls: vec![FunctionCall {
                call_id: "c1".into(),
                name: "session_title".into(),
                arguments: r#"{"session_title":"Fix login race"}"#.into(),
            }],
            server_items: vec![],
            usage: Default::default(),
            output_items: vec![],
        };
        assert_eq!(title_from_response(&resp).as_deref(), Some("Fix login race"));
    }

    #[test]
    fn title_from_response_ignores_other_tools() {
        let resp = CompleteResponse {
            id: "r".into(),
            text: "hello".into(),
            function_calls: vec![FunctionCall {
                call_id: "c1".into(),
                name: "now".into(),
                arguments: "{}".into(),
            }],
            server_items: vec![],
            usage: Default::default(),
            output_items: vec![],
        };
        assert_eq!(title_from_response(&resp).as_deref(), Some("hello"));
    }

    #[test]
    fn touch_name_skips_manual() {
        let dir = tempfile::tempdir().unwrap();
        let store = SessionStore::open_at(dir.path().to_path_buf()).unwrap();
        let mut m = store.create(PathBuf::from("/tmp/p")).unwrap();
        m.name_is_manual = true;
        m.name = "我取的名字".into();
        store.save_meta(&m).unwrap();
        store.touch_name(&mut m, "LLM 想蓋掉".into(), true).unwrap();
        assert_eq!(m.name, "我取的名字");
    }

    #[test]
    fn rename_manual_pins_title_against_llm() {
        let dir = tempfile::tempdir().unwrap();
        let store = SessionStore::open_at(dir.path().to_path_buf()).unwrap();
        let mut m = store.create(PathBuf::from("/tmp/p")).unwrap();
        store.rename_manual(&mut m, "側欄改名".into()).unwrap();
        assert_eq!(m.name, "側欄改名");
        assert!(m.name_is_manual);
        store.touch_name(&mut m, "LLM".into(), true).unwrap();
        assert_eq!(m.name, "側欄改名");
        let loaded = store.load_meta(&m.id).unwrap();
        assert_eq!(loaded.name, "側欄改名");
        assert!(loaded.name_is_manual);
    }

    #[test]
    fn delete_removes_session_dir() {
        let dir = tempfile::tempdir().unwrap();
        let store = SessionStore::open_at(dir.path().to_path_buf()).unwrap();
        let a = store.create(PathBuf::from("/tmp/a")).unwrap();
        let b = store.create(PathBuf::from("/tmp/b")).unwrap();
        store.delete(&a.id).unwrap();
        let listed = store.list().unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].id, b.id);
        assert!(!store.root().join(&a.id).exists());
    }

    #[test]
    fn closed_backgrounds_save_take_is_destructive() {
        let dir = tempfile::tempdir().unwrap();
        let store = SessionStore::open_at(dir.path().to_path_buf()).unwrap();
        let a = store.create(PathBuf::from("/tmp/a")).unwrap();
        let item = ClosedBackground {
            name: "dev".into(),
            command: "npm run dev".into(),
            log: vec!["out listening".into()],
        };
        store.save_closed_backgrounds(&a.id, &[item.clone()]).unwrap();
        let first: Vec<ClosedBackground> = store.take_closed_backgrounds(&a.id);
        assert_eq!(first, vec![item]);
        assert!(store.take_closed_backgrounds::<ClosedBackground>(&a.id).is_empty());
    }

    #[test]
    fn closed_backgrounds_skip_unknown_session_and_vanish_on_delete() {
        let dir = tempfile::tempdir().unwrap();
        let store = SessionStore::open_at(dir.path().to_path_buf()).unwrap();
        let item = ClosedBackground {
            name: "hang".into(),
            command: "sleep 60".into(),
            log: vec![],
        };
        store
            .save_closed_backgrounds("missing-id", &[item.clone()])
            .unwrap();
        assert!(store.take_closed_backgrounds::<ClosedBackground>("missing-id").is_empty());

        let a = store.create(PathBuf::from("/tmp/a")).unwrap();
        store.save_closed_backgrounds(&a.id, &[item]).unwrap();
        store.delete(&a.id).unwrap();
        assert!(store.take_closed_backgrounds::<ClosedBackground>(&a.id).is_empty());
    }
}
