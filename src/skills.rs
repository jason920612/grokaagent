//! Agent skills: grokaagent-owned SKILL.md playbooks, plus optional import
//! of Claude Code and Codex skill trees.
//!
//! Owned: `~/.grokaagent/skills/<name>/SKILL.md` and `<workspace>/.groka/skills/`.
//! Claude: `~/.claude/skills` and `<workspace>/.claude/skills`.
//! Codex: `~/.agents/skills`, `~/.codex/skills`, and the matching workspace dirs.

use std::collections::BTreeSet;
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::error::{Error, Result};
use crate::tools::{ClientTool, ToolCallFut, ToolSpec};

pub const MAX_SKILL_BYTES: usize = 256 * 1024;
pub const MAX_CATALOG: usize = 64;
const DESC_CATALOG: usize = 160;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SkillOrigin {
    GrokaProject,
    GrokaPersonal,
    ClaudeProject,
    ClaudePersonal,
    CodexProject,
    CodexPersonal,
}

impl SkillOrigin {
    pub fn source(self) -> Source {
        match self {
            Self::GrokaProject | Self::GrokaPersonal => Source::Groka,
            Self::ClaudeProject | Self::ClaudePersonal => Source::Claude,
            Self::CodexProject | Self::CodexPersonal => Source::Codex,
        }
    }

    pub fn owned(self) -> bool {
        matches!(self, Self::GrokaProject | Self::GrokaPersonal)
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::GrokaProject => "groka 專案",
            Self::GrokaPersonal => "groka",
            Self::ClaudeProject => "claude 專案",
            Self::ClaudePersonal => "claude",
            Self::CodexProject => "codex 專案",
            Self::CodexPersonal => "codex",
        }
    }

    fn catalog_tag(self) -> &'static str {
        match self.source() {
            Source::Groka => "groka",
            Source::Claude => "claude",
            Source::Codex => "codex",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    Groka,
    Claude,
    Codex,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Skill {
    pub id: String,
    pub name: String,
    pub origin: SkillOrigin,
    pub description: String,
    pub path: PathBuf,
    pub enabled: bool,
}

impl Skill {
    fn new(name: String, origin: SkillOrigin, description: String, path: PathBuf, enabled: bool) -> Self {
        let id = format!("{}:{name}", origin_key(origin));
        Self {
            id,
            name,
            origin,
            description,
            path,
            enabled,
        }
    }
}

fn origin_key(origin: SkillOrigin) -> &'static str {
    match origin {
        SkillOrigin::GrokaProject => "groka-project",
        SkillOrigin::GrokaPersonal => "groka",
        SkillOrigin::ClaudeProject => "claude-project",
        SkillOrigin::ClaudePersonal => "claude",
        SkillOrigin::CodexProject => "codex-project",
        SkillOrigin::CodexPersonal => "codex",
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SkillPrefs {
    #[serde(default)]
    pub import_claude: bool,
    #[serde(default)]
    pub import_codex: bool,
    #[serde(default)]
    pub disabled: BTreeSet<String>,
}

impl Default for SkillPrefs {
    fn default() -> Self {
        Self {
            import_claude: false,
            import_codex: false,
            disabled: BTreeSet::new(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct SkillStore {
    groka_root: PathBuf,
    prefs_path: PathBuf,
    prefs: SkillPrefs,
    home: PathBuf,
}

impl SkillStore {
    pub fn open() -> Result<Self> {
        let home = dirs::home_dir().ok_or_else(|| Error::Auth("cannot resolve home directory".into()))?;
        let groka_root = if let Ok(p) = std::env::var("GROKA_SKILLS_DIR") {
            PathBuf::from(p)
        } else {
            home.join(".grokaagent").join("skills")
        };
        Ok(Self::open_at(groka_root, home))
    }

    pub fn open_at(groka_root: PathBuf, home: PathBuf) -> Self {
        let prefs_path = groka_root.join("prefs.json");
        let prefs = load_prefs(&prefs_path).unwrap_or_default();
        Self {
            groka_root,
            prefs_path,
            prefs,
            home,
        }
    }

    pub fn prefs(&self) -> &SkillPrefs {
        &self.prefs
    }

    pub fn set_import_claude(&mut self, on: bool) -> Result<()> {
        self.prefs.import_claude = on;
        self.save_prefs()
    }

    pub fn set_import_codex(&mut self, on: bool) -> Result<()> {
        self.prefs.import_codex = on;
        self.save_prefs()
    }

    pub fn set_enabled(&mut self, id: &str, enabled: bool) -> Result<()> {
        if enabled {
            self.prefs.disabled.remove(id);
        } else {
            self.prefs.disabled.insert(id.to_string());
        }
        self.save_prefs()
    }

    fn save_prefs(&self) -> Result<()> {
        fs::create_dir_all(&self.groka_root)?;
        let bytes = serde_json::to_vec_pretty(&self.prefs)?;
        atomic_write(&self.prefs_path, &bytes)?;
        Ok(())
    }

    pub fn scan(&self, workspace: &Path) -> Vec<Skill> {
        let mut out = Vec::new();
        push_dir(
            &mut out,
            &self.groka_root,
            SkillOrigin::GrokaPersonal,
            &self.prefs.disabled,
        );
        push_dir(
            &mut out,
            &workspace.join(".groka").join("skills"),
            SkillOrigin::GrokaProject,
            &self.prefs.disabled,
        );
        if self.prefs.import_claude {
            push_dir(
                &mut out,
                &self.home.join(".claude").join("skills"),
                SkillOrigin::ClaudePersonal,
                &self.prefs.disabled,
            );
            push_dir(
                &mut out,
                &workspace.join(".claude").join("skills"),
                SkillOrigin::ClaudeProject,
                &self.prefs.disabled,
            );
        }
        if self.prefs.import_codex {
            push_dir(
                &mut out,
                &self.home.join(".agents").join("skills"),
                SkillOrigin::CodexPersonal,
                &self.prefs.disabled,
            );
            push_dir(
                &mut out,
                &self.home.join(".codex").join("skills"),
                SkillOrigin::CodexPersonal,
                &self.prefs.disabled,
            );
            push_dir(
                &mut out,
                &workspace.join(".agents").join("skills"),
                SkillOrigin::CodexProject,
                &self.prefs.disabled,
            );
            push_dir(
                &mut out,
                &workspace.join(".codex").join("skills"),
                SkillOrigin::CodexProject,
                &self.prefs.disabled,
            );
        }
        out.sort_by(|a, b| a.origin.cmp(&b.origin).then(a.name.cmp(&b.name)));
        out
    }

    pub fn catalog_suffix(&self, workspace: &Path) -> String {
        catalog_suffix(&self.scan(workspace))
    }

    pub fn read_enabled(&self, workspace: &Path, name: &str) -> Result<(Skill, String)> {
        let name = normalize_name(name)?;
        let mut found = self
            .scan(workspace)
            .into_iter()
            .filter(|s| s.enabled && s.name == name)
            .collect::<Vec<_>>();
        found.sort_by_key(|s| s.origin);
        let skill = found
            .into_iter()
            .next()
            .ok_or_else(|| Error::Tool(format!("skill not found or disabled: {name}")))?;
        let body = read_skill_file(&skill.path)?;
        Ok((skill, body))
    }

    pub fn write(
        &self,
        workspace: &Path,
        name: &str,
        body: &str,
        project: bool,
    ) -> Result<PathBuf> {
        let name = normalize_name(name)?;
        let body = ensure_frontmatter(&name, body);
        if body.len() > MAX_SKILL_BYTES {
            return Err(Error::Tool("skill is too large".into()));
        }
        let dir = if project {
            workspace.join(".groka").join("skills").join(&name)
        } else {
            self.groka_root.join(&name)
        };
        fs::create_dir_all(&dir)?;
        let path = dir.join("SKILL.md");
        atomic_write(&path, body.as_bytes())?;
        Ok(path)
    }

    pub fn delete_owned(&self, workspace: &Path, id: &str) -> Result<()> {
        let skill = self
            .scan(workspace)
            .into_iter()
            .find(|s| s.id == id)
            .ok_or_else(|| Error::Tool(format!("skill not found: {id}")))?;
        if !skill.origin.owned() {
            return Err(Error::Tool(
                "imported Claude/Codex skills cannot be deleted here; disable them instead".into(),
            ));
        }
        let dir = skill
            .path
            .parent()
            .ok_or_else(|| Error::Tool("invalid skill path".into()))?;
        fs::remove_file(&skill.path)?;
        let _ = fs::remove_dir(dir);
        Ok(())
    }
}

pub fn catalog_suffix(skills: &[Skill]) -> String {
    let enabled: Vec<&Skill> = skills.iter().filter(|s| s.enabled).take(MAX_CATALOG).collect();
    if enabled.is_empty() {
        return String::new();
    }
    let mut out = String::from(
        "\n\nAvailable skills:\nWhen a listed skill matches the current task, call skill with action=read and that name, then follow it before acting. Do not skip a matching skill.\n",
    );
    for s in enabled {
        let desc = clip_desc(&s.description, DESC_CATALOG);
        out.push_str(&format!("- `{}` ({}): {desc}\n", s.name, s.origin.catalog_tag()));
    }
    out
}

pub fn read_skill_file(path: &Path) -> Result<String> {
    let bytes = fs::read(path)?;
    if bytes.len() > MAX_SKILL_BYTES {
        return Err(Error::Tool("skill is too large".into()));
    }
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

fn push_dir(out: &mut Vec<Skill>, root: &Path, origin: SkillOrigin, disabled: &BTreeSet<String>) {
    let rd = match fs::read_dir(root) {
        Ok(rd) => rd,
        Err(_) => return,
    };
    for ent in rd.flatten() {
        let dir = ent.path();
        if !dir.is_dir() {
            continue;
        }
        let Some(name) = ent.file_name().to_str().map(str::to_string) else {
            continue;
        };
        if !valid_dir_name(&name) {
            continue;
        }
        let path = dir.join("SKILL.md");
        if !path.is_file() {
            continue;
        }
        let raw = match read_skill_file(&path) {
            Ok(s) => s,
            Err(_) => continue,
        };
        let meta = parse_skill_md(&raw);
        let name = if valid_dir_name(&meta.name) {
            meta.name
        } else {
            name
        };
        let skill = Skill::new(name, origin, meta.description, path, true);
        let mut skill = skill;
        if disabled.contains(&skill.id) {
            skill.enabled = false;
        }
        if out.iter().any(|s| s.id == skill.id) {
            continue;
        }
        out.push(skill);
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillMeta {
    pub name: String,
    pub description: String,
}

pub fn parse_skill_md(raw: &str) -> SkillMeta {
    let Some((map, _body)) = split_frontmatter(raw) else {
        let description = raw
            .lines()
            .map(str::trim)
            .find(|l| !l.is_empty() && !l.starts_with('#'))
            .unwrap_or("")
            .to_string();
        return SkillMeta {
            name: String::new(),
            description,
        };
    };
    SkillMeta {
        name: map.name,
        description: map.description,
    }
}

struct Fm {
    name: String,
    description: String,
}

fn split_frontmatter(raw: &str) -> Option<(Fm, &str)> {
    let raw = raw.trim_start_matches('\u{feff}');
    let rest = raw.strip_prefix("---")?;
    let rest = rest.strip_prefix('\r').unwrap_or(rest).strip_prefix('\n')?;
    let mut search = rest;
    let mut end = None;
    while let Some(i) = search.find('\n') {
        let line = &search[..=i];
        let trimmed = line.trim();
        if trimmed == "---" || trimmed == "..." {
            let abs = rest.len() - search.len() + i + 1;
            end = Some(abs);
            break;
        }
        search = &search[i + 1..];
    }
    let end = end?;
    let yaml = &rest[..end.saturating_sub(1)];
    let body = rest[end..].trim_start_matches('\r').trim_start_matches('\n');
    Some((parse_yaml_map(yaml), body))
}

fn parse_yaml_map(yaml: &str) -> Fm {
    let mut name = String::new();
    let mut description = String::new();
    let lines: Vec<&str> = yaml.lines().collect();
    let mut i = 0;
    while i < lines.len() {
        let line = lines[i];
        if line.trim().is_empty() || line.trim_start().starts_with('#') {
            i += 1;
            continue;
        }
        let Some((key, rest)) = split_yaml_key(line) else {
            i += 1;
            continue;
        };
        let (value, consumed) = yaml_value(rest, &lines, i);
        i += consumed;
        match key {
            "name" => name = value,
            "description" => description = value,
            _ => {}
        }
    }
    Fm { name, description }
}

fn split_yaml_key(line: &str) -> Option<(&str, &str)> {
    if line.starts_with(' ') || line.starts_with('\t') {
        return None;
    }
    let (k, v) = line.split_once(':')?;
    Some((k.trim(), v))
}

fn yaml_value(rest: &str, lines: &[&str], idx: usize) -> (String, usize) {
    let t = rest.trim();
    if t == ">" || t == ">|" || t == "|-" || t == "|" || t == ">-" {
        let mut buf = String::new();
        let mut n = 1;
        for line in lines.iter().skip(idx + 1) {
            if line.starts_with(' ') || line.starts_with('\t') {
                if !buf.is_empty() {
                    buf.push(if t.starts_with('>') { ' ' } else { '\n' });
                }
                buf.push_str(line.trim());
                n += 1;
            } else {
                break;
            }
        }
        return (buf, n);
    }
    (unquote(t), 1)
}

fn unquote(s: &str) -> String {
    let s = s.trim();
    if s.len() >= 2 {
        let b = s.as_bytes();
        if (b[0] == b'"' && b[s.len() - 1] == b'"') || (b[0] == b'\'' && b[s.len() - 1] == b'\'') {
            return s[1..s.len() - 1].replace("\\n", "\n");
        }
    }
    s.to_string()
}

pub fn valid_dir_name(name: &str) -> bool {
    let n = name.chars().count();
    if n == 0 || n > 64 {
        return false;
    }
    let mut chars = name.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    if !first.is_ascii_alphanumeric() {
        return false;
    }
    chars.all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

fn normalize_name(name: &str) -> Result<String> {
    let name = name.trim().to_ascii_lowercase().replace('_', "-");
    if !valid_dir_name(&name) {
        return Err(Error::Tool(
            "skill name must be 1–64 letters, digits, or hyphens".into(),
        ));
    }
    Ok(name)
}

fn ensure_frontmatter(name: &str, body: &str) -> String {
    if split_frontmatter(body).is_some() {
        return body.to_string();
    }
    let desc = body
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty() && !l.starts_with('#'))
        .unwrap_or("User-created grokaagent skill");
    format!("---\nname: {name}\ndescription: {desc}\n---\n\n{body}")
}

fn clip_desc(s: &str, max: usize) -> String {
    let one: String = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if one.chars().count() <= max {
        return one;
    }
    let clipped: String = one.chars().take(max.saturating_sub(1)).collect();
    format!("{clipped}…")
}

fn load_prefs(path: &Path) -> Option<SkillPrefs> {
    let raw = fs::read(path).ok()?;
    serde_json::from_slice(&raw).ok()
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

pub struct SkillTool {
    store: Arc<Mutex<SkillStore>>,
    workspace: PathBuf,
}

impl SkillTool {
    pub fn new(store: Arc<Mutex<SkillStore>>, workspace: PathBuf) -> Self {
        Self { store, workspace }
    }

    fn with_store<T>(&self, f: impl FnOnce(&mut SkillStore) -> Result<T>) -> Result<T> {
        let mut g = self
            .store
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        f(&mut g)
    }

    pub fn call_sync(&self, args: &Value) -> Result<String> {
        let action = args
            .get("action")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim();
        match action {
            "list" => {
                let skills = self.with_store(|s| Ok(s.scan(&self.workspace)))?;
                let rows: Vec<Value> = skills
                    .iter()
                    .map(|s| {
                        json!({
                            "name": s.name,
                            "source": s.origin.catalog_tag(),
                            "origin": s.origin.label(),
                            "enabled": s.enabled,
                            "description": s.description,
                            "owned": s.origin.owned(),
                        })
                    })
                    .collect();
                Ok(json!({ "skills": rows }).to_string())
            }
            "read" => {
                let name = req_name(args)?;
                let (skill, body) = self.with_store(|s| s.read_enabled(&self.workspace, name))?;
                Ok(json!({
                    "name": skill.name,
                    "source": skill.origin.catalog_tag(),
                    "path": skill.path.to_string_lossy(),
                    "body": body,
                })
                .to_string())
            }
            "write" => {
                let name = req_name(args)?;
                let body = args
                    .get("body")
                    .and_then(Value::as_str)
                    .ok_or_else(|| Error::Tool("body is required for write".into()))?;
                let project = args
                    .get("scope")
                    .and_then(Value::as_str)
                    .is_some_and(|s| s == "project");
                let path = self.with_store(|s| s.write(&self.workspace, name, body, project))?;
                Ok(json!({
                    "ok": true,
                    "name": normalize_name(name)?,
                    "path": path.to_string_lossy(),
                    "scope": if project { "project" } else { "personal" },
                })
                .to_string())
            }
            "delete" => {
                let name = req_name(args)?;
                let name = normalize_name(name)?;
                let id = self.with_store(|s| {
                    let hit = s
                        .scan(&self.workspace)
                        .into_iter()
                        .find(|sk| sk.origin.owned() && sk.name == name)
                        .ok_or_else(|| {
                            Error::Tool(format!("no grokaagent-owned skill named {name}"))
                        })?;
                    let id = hit.id.clone();
                    s.delete_owned(&self.workspace, &id)?;
                    Ok(id)
                })?;
                Ok(json!({ "ok": true, "deleted": id }).to_string())
            }
            _ => Err(Error::Tool(
                "action must be list, read, write, or delete".into(),
            )),
        }
    }
}

impl ClientTool for SkillTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "skill".into(),
            description: "SKILL.md playbooks. list/read enabled skills (grokaagent-owned plus imported Claude Code or Codex skills if the user turned import on in Settings). write/delete only grokaagent-owned skills: personal (~/.grokaagent/skills) or scope=project (.groka/skills in the workspace). When a listed skill matches the task, read it first and follow it. Do not edit imported Claude/Codex files.".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "action": {
                        "type": "string",
                        "enum": ["list", "read", "write", "delete"],
                        "description": "list catalog, read one SKILL.md, create/overwrite a grokaagent skill, or delete a grokaagent skill"
                    },
                    "name": {
                        "type": "string",
                        "description": "Skill directory name (required except list)"
                    },
                    "body": {
                        "type": "string",
                        "description": "Full SKILL.md including YAML frontmatter (write only)"
                    },
                    "scope": {
                        "type": "string",
                        "enum": ["personal", "project"],
                        "description": "write only; default personal"
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

fn req_name(args: &Value) -> Result<&str> {
    args.get("name")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| Error::Tool("name is required".into()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> (tempfile::TempDir, tempfile::TempDir, SkillStore) {
        let groka = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        let store = SkillStore::open_at(groka.path().to_path_buf(), home.path().to_path_buf());
        (groka, home, store)
    }

    fn write_skill(root: &Path, name: &str, body: &str) {
        let dir = root.join(name);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("SKILL.md"), body).unwrap();
    }

    #[test]
    fn parse_simple_and_folded_description() {
        let simple = "---\nname: pdf\ndescription: Extract text from PDFs\n---\n\n# PDF\nUse pdfplumber.\n";
        let m = parse_skill_md(simple);
        assert_eq!(m.name, "pdf");
        assert_eq!(m.description, "Extract text from PDFs");

        let folded = "---\nname: arch\ndescription: >-\n  Design systems.\n  Start from data flow.\n---\nbody\n";
        let m = parse_skill_md(folded);
        assert_eq!(m.name, "arch");
        assert!(m.description.contains("Design systems"));
        assert!(m.description.contains("data flow"), "{}", m.description);

        let none = "# Hello\n\nDo the thing.\n";
        let m = parse_skill_md(none);
        assert!(m.name.is_empty());
        assert_eq!(m.description, "Do the thing.");
    }

    #[test]
    fn import_flags_hide_foreign_skills() {
        let (groka, home, mut store) = store();
        let ws = tempfile::tempdir().unwrap();
        write_skill(
            groka.path(),
            "mine",
            "---\nname: mine\ndescription: owned\n---\nA\n",
        );
        write_skill(
            &home.path().join(".claude").join("skills"),
            "foreign",
            "---\nname: foreign\ndescription: claude one\n---\nB\n",
        );
        write_skill(
            &home.path().join(".agents").join("skills"),
            "cx",
            "---\nname: cx\ndescription: codex one\n---\nC\n",
        );
        let names = |s: &SkillStore| {
            s.scan(ws.path())
                .into_iter()
                .map(|k| k.name)
                .collect::<Vec<_>>()
        };
        assert_eq!(names(&store), vec!["mine"]);
        store.set_import_claude(true).unwrap();
        assert_eq!(names(&store), vec!["mine", "foreign"]);
        store.set_import_codex(true).unwrap();
        let mut got = names(&store);
        got.sort();
        assert_eq!(got, vec!["cx", "foreign", "mine"]);
    }

    #[test]
    fn disable_persists_and_drops_from_catalog() {
        let (groka, _home, mut store) = store();
        let ws = tempfile::tempdir().unwrap();
        write_skill(
            groka.path(),
            "alpha",
            "---\nname: alpha\ndescription: First skill for listing\n---\nbody\n",
        );
        let id = store.scan(ws.path())[0].id.clone();
        store.set_enabled(&id, false).unwrap();
        let listed = store.scan(ws.path());
        assert!(!listed[0].enabled);
        assert!(catalog_suffix(&listed).is_empty());
        let reloaded = SkillStore::open_at(groka.path().to_path_buf(), store.home.clone());
        assert!(reloaded.prefs().disabled.contains(&id));
    }

    #[test]
    fn write_personal_then_read_and_delete() {
        let (_groka, _home, store) = store();
        let ws = tempfile::tempdir().unwrap();
        store
            .write(ws.path(), "git-safe", "# Git\nNever force-push main.\n", false)
            .unwrap();
        let (skill, body) = store.read_enabled(ws.path(), "git-safe").unwrap();
        assert!(skill.origin.owned());
        assert!(body.contains("Never force-push main"));
        assert!(body.starts_with("---"), "frontmatter must be added: {body}");
        store.delete_owned(ws.path(), &skill.id).unwrap();
        assert!(store.read_enabled(ws.path(), "git-safe").is_err());
    }

    #[test]
    fn cannot_delete_imported_skill() {
        let (_groka, home, mut store) = store();
        let ws = tempfile::tempdir().unwrap();
        store.set_import_claude(true).unwrap();
        write_skill(
            &home.path().join(".claude").join("skills"),
            "keep",
            "---\nname: keep\ndescription: imported\n---\nx\n",
        );
        let id = store.scan(ws.path())[0].id.clone();
        let err = store.delete_owned(ws.path(), &id).unwrap_err().to_string();
        assert!(err.contains("cannot be deleted"), "{err}");
        assert!(home.path().join(".claude").join("skills").join("keep").join("SKILL.md").is_file());
    }

    #[test]
    fn tool_write_rejects_path_escape() {
        let (_g, _h, store) = store();
        let ws = tempfile::tempdir().unwrap();
        let tool = SkillTool::new(Arc::new(Mutex::new(store)), ws.path().to_path_buf());
        let err = tool
            .call_sync(&json!({"action":"write","name":"../x","body":"no"}))
            .unwrap_err()
            .to_string();
        assert!(err.contains("name"), "{err}");
    }

    #[test]
    fn groka_outranks_claude_on_same_name() {
        let (groka, home, mut store) = store();
        let ws = tempfile::tempdir().unwrap();
        store.set_import_claude(true).unwrap();
        write_skill(
            groka.path(),
            "dup",
            "---\nname: dup\ndescription: groka copy\n---\nGROKA\n",
        );
        write_skill(
            &home.path().join(".claude").join("skills"),
            "dup",
            "---\nname: dup\ndescription: claude copy\n---\nCLAUDE\n",
        );
        let (_s, body) = store.read_enabled(ws.path(), "dup").unwrap();
        assert!(body.contains("GROKA"), "{body}");
    }
}
