//! A workflow run's journal: one JSON line per event (`start`, each
//! finished `agent()` call, `end`), appended as it happens so a killed run
//! still leaves everything it finished. Resuming reads an earlier run's
//! journal: an `agent()` call whose prompt and options are unchanged
//! returns its recorded result instead of running again.
//!
//! Calls are matched by content, not position: the key hashes the prompt
//! and the options that change what the agent does, and the n-th call with
//! a key takes the n-th recorded result with that key. `pipeline()` starts
//! calls in whatever order earlier stages finish, so a position would not
//! be stable between runs; content is. A changed upstream result changes
//! the downstream prompts built from it, so those calls run again.

use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde_json::{json, Value};
use sha2::{Digest, Sha256};

pub const FILE: &str = "journal.jsonl";
/// How far back a resume follows `resumed_from`.
const MAX_CHAIN: usize = 32;

/// The run a journal's run resumed from, if it is a well-formed run id.
fn resumed_from(text: &str) -> Option<String> {
    let start: Value = serde_json::from_str(text.lines().next()?).ok()?;
    if start.get("type").and_then(Value::as_str) != Some("start") {
        return None;
    }
    let id = start.get("resumed_from")?.as_str()?;
    let ok = id.starts_with("wf-") && id.len() <= 40 && id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-');
    ok.then(|| id.to_string())
}

/// The cache key of an `agent()` request: what changes the agent's work.
/// Labels, phases and the nested-workflow group are display only.
pub fn call_key(req: &Value) -> String {
    let pick = |k: &str| req.get(k).cloned().unwrap_or(Value::Null);
    // serde_json maps are sorted, so this text is canonical.
    let canon = json!({
        "prompt": pick("prompt"),
        "schema": pick("schema"),
        "model": pick("model"),
        "effort": pick("effort"),
        "isolation": pick("isolation"),
        "agentType": pick("agentType"),
    })
    .to_string();
    let digest = Sha256::digest(canon.as_bytes());
    digest.iter().take(16).map(|b| format!("{b:02x}")).collect()
}

/// Recorded results by key and occurrence.
#[derive(Default)]
pub struct Cache {
    hits: HashMap<(String, usize), Value>,
}

impl Cache {
    /// The finished calls of an earlier run, and of the runs it resumed
    /// from in turn (a run that failed before re-making its calls still
    /// hands on what its predecessor finished). A missing or unreadable
    /// journal is an empty cache. Failed calls are not cached: they run
    /// again.
    pub fn load(path: &Path) -> Self {
        // Newest first: the run asked for, then each one it resumed from.
        let mut chain = Vec::new();
        let mut at = path.to_path_buf();
        let mut seen = std::collections::HashSet::new();
        while chain.len() < MAX_CHAIN {
            let Ok(text) = std::fs::read_to_string(&at) else {
                break;
            };
            let prev = resumed_from(&text);
            chain.push(text);
            let Some(prev) = prev.filter(|id| seen.insert(id.clone())) else {
                break;
            };
            let Some(runs) = at.parent().and_then(Path::parent) else {
                break;
            };
            at = runs.join(prev).join(FILE);
        }
        let mut hits = HashMap::new();
        // Oldest first, so a newer run's result for the same call wins.
        for text in chain.iter().rev() {
            Self::read_into(text, &mut hits);
        }
        Self { hits }
    }

    fn read_into(text: &str, hits: &mut HashMap<(String, usize), Value>) {
        for line in text.lines() {
            let Ok(v) = serde_json::from_str::<Value>(line) else {
                continue;
            };
            if v.get("type").and_then(Value::as_str) != Some("agent")
                || !matches!(v.get("status").and_then(Value::as_str), Some("ok" | "cached"))
            {
                continue;
            }
            let (Some(key), Some(value)) = (v.get("key").and_then(Value::as_str), v.get("value")) else {
                continue;
            };
            let n = v.get("n").and_then(Value::as_u64).unwrap_or(0) as usize;
            hits.insert((key.to_string(), n), value.clone());
        }
    }

    pub fn len(&self) -> usize {
        self.hits.len()
    }

    pub fn is_empty(&self) -> bool {
        self.hits.is_empty()
    }

    /// The result of the `n`-th call (from 0) with `key`.
    pub fn get(&self, key: &str, n: usize) -> Option<&Value> {
        self.hits.get(&(key.to_string(), n))
    }
}

/// Appends journal lines; write errors are ignored (the run goes on, it
/// just cannot be resumed as far).
pub struct Journal {
    path: PathBuf,
    file: Mutex<Option<File>>,
}

impl Journal {
    pub fn create(dir: &Path) -> Self {
        let path = dir.join(FILE);
        let file = OpenOptions::new().create(true).append(true).open(&path).ok();
        Self {
            path,
            file: Mutex::new(file),
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn append(&self, entry: &Value) {
        let mut f = self.file.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(file) = f.as_mut() {
            let _ = writeln!(file, "{entry}");
            let _ = file.flush();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_ignore_display_options_and_key_order() {
        let a = json!({"prompt": "p", "label": "x", "phase": "A", "schema": {"type": "object", "properties": {"a": {}, "b": {}}}});
        let b = json!({"schema": {"properties": {"b": {}, "a": {}}, "type": "object"}, "prompt": "p", "label": "y"});
        assert_eq!(call_key(&a), call_key(&b));
        assert_ne!(call_key(&a), call_key(&json!({"prompt": "q"})));
        assert_ne!(call_key(&json!({"prompt": "p"})), call_key(&json!({"prompt": "p", "model": "m"})));
    }

    #[test]
    fn the_cache_hands_back_recorded_results_by_key_and_occurrence() {
        let dir = tempfile::tempdir().unwrap();
        let j = Journal::create(dir.path());
        j.append(&json!({"type": "start"}));
        j.append(&json!({"type": "agent", "key": "k", "n": 1, "status": "ok", "value": "second"}));
        j.append(&json!({"type": "agent", "key": "k", "n": 0, "status": "cached", "value": "first"}));
        j.append(&json!({"type": "agent", "key": "f", "n": 0, "status": "null", "value": null}));
        j.append(&json!({"type": "agent", "key": "f", "n": 1, "status": "ok", "value": 7}));
        j.append(&json!({"type": "end"}));
        let c = Cache::load(j.path());
        assert_eq!(c.len(), 3);
        assert_eq!(c.get("k", 0), Some(&json!("first")));
        assert_eq!(c.get("k", 1), Some(&json!("second")));
        assert_eq!(c.get("k", 2), None);
        // A failed call is not cached: it runs again.
        assert_eq!(c.get("f", 0), None);
        assert_eq!(c.get("f", 1), Some(&json!(7)));
        assert!(Cache::load(&dir.path().join("missing")).is_empty());
    }

    #[test]
    fn a_resume_reaches_back_through_runs_that_finished_nothing_new() {
        let runs = tempfile::tempdir().unwrap();
        let mk = |id: &str, from: Option<&str>, entries: &[Value]| {
            let dir = runs.path().join(id);
            std::fs::create_dir_all(&dir).unwrap();
            let j = Journal::create(&dir);
            j.append(&json!({"type": "start", "run": id, "resumed_from": from}));
            for e in entries {
                j.append(e);
            }
            dir.join(FILE)
        };
        mk("wf-a", None, &[json!({"type": "agent", "key": "k", "n": 0, "status": "ok", "value": "old"}), json!({"type": "agent", "key": "j", "n": 0, "status": "ok", "value": 1})]);
        // b failed before making any call; c redid k with a new result.
        mk("wf-b", Some("wf-a"), &[]);
        let c = mk("wf-c", Some("wf-b"), &[json!({"type": "agent", "key": "k", "n": 0, "status": "ok", "value": "new"})]);
        let cache = Cache::load(&c);
        assert_eq!(cache.get("k", 0), Some(&json!("new")));
        assert_eq!(cache.get("j", 0), Some(&json!(1)));
        // A loop in the chain stops instead of spinning.
        mk("wf-x", Some("wf-y"), &[]);
        let y = mk("wf-y", Some("wf-x"), &[json!({"type": "agent", "key": "k", "n": 0, "status": "ok", "value": 2})]);
        assert_eq!(Cache::load(&y).len(), 1);
    }
}
