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
    /// The finished calls of an earlier run (a missing or unreadable
    /// journal is an empty cache). Failed calls are not cached: they run
    /// again.
    pub fn load(path: &Path) -> Self {
        let mut hits = HashMap::new();
        let Ok(text) = std::fs::read_to_string(path) else {
            return Self::default();
        };
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
        Self { hits }
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
}
