//! Transcript rows. The serde format is what session stores hold on disk, so
//! it must keep reading transcripts written by earlier versions.

use std::time::Instant;

use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub(crate) struct FileChange {
    pub path: String,
    pub kind: String,
    pub diff: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct ToolCall {
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub call_id: String,
    pub name: String,
    pub args: Value,
    pub output: String,
    pub files: Vec<FileChange>,
    pub done: bool,
    pub phase: String,
}

impl ToolCall {
    pub fn started(call_id: String, name: String, args: Value) -> Self {
        Self {
            call_id,
            name,
            args,
            output: String::new(),
            files: Vec::new(),
            done: false,
            phase: "執行中".into(),
        }
    }

    pub fn failed(&self) -> bool {
        self.phase == "失敗"
    }

    pub fn stopped(&self) -> bool {
        self.phase == "已停止"
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct ToolGroup {
    pub calls: Vec<ToolCall>,
    pub expanded: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize, Default)]
pub(crate) struct Think {
    pub text: String,
    pub expanded: bool,
    pub done: bool,
    #[serde(default)]
    pub elapsed_ms: u64,
    #[serde(skip)]
    pub started: Option<Instant>,
}

impl Think {
    /// Elapsed time, live while the block is still open.
    pub fn elapsed(&self) -> u64 {
        if self.done {
            self.elapsed_ms
        } else {
            self.started
                .map(|s| s.elapsed().as_millis() as u64)
                .unwrap_or(self.elapsed_ms)
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct UserMsg {
    pub text: String,
    pub images: Vec<String>,
}

impl From<&str> for UserMsg {
    fn from(s: &str) -> Self {
        Self {
            text: s.to_string(),
            images: Vec::new(),
        }
    }
}

impl From<String> for UserMsg {
    fn from(text: String) -> Self {
        Self {
            text,
            images: Vec::new(),
        }
    }
}

impl Serialize for UserMsg {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        if self.images.is_empty() {
            serializer.serialize_str(&self.text)
        } else {
            use serde::ser::SerializeStruct;
            let mut st = serializer.serialize_struct("UserMsg", 2)?;
            st.serialize_field("text", &self.text)?;
            st.serialize_field("images", &self.images)?;
            st.end()
        }
    }
}

impl<'de> Deserialize<'de> for UserMsg {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum De {
            Text(String),
            Full {
                text: String,
                #[serde(default)]
                images: Vec<String>,
            },
        }
        Ok(match De::deserialize(deserializer)? {
            De::Text(text) => Self {
                text,
                images: Vec::new(),
            },
            De::Full { text, images } => Self { text, images },
        })
    }
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct AgentMsg {
    pub text: String,
    pub work_ms: u64,
}

impl AgentMsg {
    pub fn new(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            work_ms: 0,
        }
    }
}

impl Serialize for AgentMsg {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        if self.work_ms == 0 {
            serializer.serialize_str(&self.text)
        } else {
            use serde::ser::SerializeStruct;
            let mut st = serializer.serialize_struct("AgentMsg", 2)?;
            st.serialize_field("text", &self.text)?;
            st.serialize_field("work_ms", &self.work_ms)?;
            st.end()
        }
    }
}

impl<'de> Deserialize<'de> for AgentMsg {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum De {
            Text(String),
            Full {
                text: String,
                #[serde(default)]
                work_ms: u64,
            },
        }
        Ok(match De::deserialize(deserializer)? {
            De::Text(text) => Self { text, work_ms: 0 },
            De::Full { text, work_ms } => Self { text, work_ms },
        })
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) enum Row {
    User(UserMsg),
    Agent(AgentMsg),
    Tools(ToolGroup),
    Think(Think),
    Meta(String),
    Err(String),
    Picture { path: String, label: String },
}

impl Row {
    pub fn meta(s: impl Into<String>) -> Self {
        Self::Meta(s.into())
    }
}

/// File changes a tool reported in its JSON output (`path`+`diff`, or `files[]`).
pub(crate) fn parse_file_changes(output: &str) -> Vec<FileChange> {
    let Ok(v) = serde_json::from_str::<Value>(output) else {
        return Vec::new();
    };
    let one = |item: &Value| -> Option<FileChange> {
        let path = item.get("path").and_then(Value::as_str)?.to_string();
        if path.is_empty() {
            return None;
        }
        let text = |k: &str, d: &str| {
            item.get(k)
                .and_then(Value::as_str)
                .unwrap_or(d)
                .to_string()
        };
        Some(FileChange {
            path,
            kind: text("kind", "modify"),
            diff: text("diff", ""),
        })
    };
    if v.get("path").and_then(Value::as_str).is_some() && v.get("diff").is_some() {
        one(&v).into_iter().collect()
    } else if let Some(files) = v.get("files").and_then(Value::as_array) {
        files.iter().filter_map(one).collect()
    } else {
        Vec::new()
    }
}

/// Outcome label for a finished tool call.
pub(crate) fn tool_phase(name: &str, output: &str) -> &'static str {
    let result = serde_json::from_str::<Value>(output).unwrap_or(Value::Null);
    let cancelled = result.get("cancelled").and_then(Value::as_bool) == Some(true);
    let failed = result.get("error").is_some_and(|e| !e.is_null() && e != "")
        || result
            .get("exit_code")
            .and_then(Value::as_i64)
            .is_some_and(|code| code != 0);
    if cancelled {
        "已停止"
    } else if failed {
        "失敗"
    } else if name == "spawn_agent" {
        "已啟動"
    } else {
        "完成"
    }
}

/// An image the model asked to look at: `(path, label)` for a picture row.
pub(crate) fn picture_from_tool(name: &str, output: &str) -> Option<(String, String)> {
    let v: Value = serde_json::from_str(output).ok()?;
    if v.get("attach_image").and_then(Value::as_bool) != Some(true) {
        return None;
    }
    let path = v.get("path").and_then(Value::as_str)?.to_string();
    if path.is_empty() {
        return None;
    }
    Some((path.clone(), format!("模型在看  {name}  {path}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn legacy_rows_still_load() {
        let raw = serde_json::json!([
            {"User": "hi"},
            {"User": {"text": "look", "images": ["a.png"]}},
            {"Agent": "hello"},
            {"Agent": {"text": "done", "work_ms": 1200}},
            {"Tools": {"calls": [{"name": "now", "args": {}, "output": "t", "files": [], "done": true, "phase": "完成"}], "expanded": false}},
            {"Think": {"text": "hmm", "expanded": false, "done": true}},
            {"Meta": "m"},
            {"Err": "e"},
            {"Picture": {"path": "p.png", "label": "l"}}
        ]);
        let rows: Vec<Row> = serde_json::from_value(raw).unwrap();
        assert_eq!(rows.len(), 9);
        assert!(matches!(&rows[1], Row::User(u) if u.images == ["a.png"]));
        assert!(matches!(&rows[3], Row::Agent(a) if a.work_ms == 1200));
        assert!(matches!(&rows[4], Row::Tools(g) if g.calls[0].call_id.is_empty()));
    }

    #[test]
    fn plain_rows_serialize_as_strings() {
        assert_eq!(
            serde_json::to_value(Row::Agent(AgentMsg::new("hello"))).unwrap(),
            serde_json::json!({"Agent": "hello"})
        );
        assert_eq!(
            serde_json::to_value(Row::User("hi".into())).unwrap(),
            serde_json::json!({"User": "hi"})
        );
    }

    #[test]
    fn tool_outcomes() {
        assert_eq!(tool_phase("run_command", r#"{"exit_code":1}"#), "失敗");
        assert_eq!(tool_phase("x", r#"{"error":"interrupted","cancelled":true}"#), "已停止");
        assert_eq!(tool_phase("spawn_agent", r#"{"state":"working"}"#), "已啟動");
        assert_eq!(tool_phase("read_file", "1|x"), "完成");
    }

    #[test]
    fn file_changes_single_and_many() {
        let one = parse_file_changes(r#"{"path":"a.rs","diff":"+x"}"#);
        assert_eq!(one.len(), 1);
        assert_eq!(one[0].kind, "modify");
        let many = parse_file_changes(r#"{"files":[{"path":"a","kind":"create","diff":""},{"path":""}]}"#);
        assert_eq!(many.len(), 1);
        assert_eq!(many[0].kind, "create");
        assert!(parse_file_changes("not json").is_empty());
    }
}
