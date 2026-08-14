//! Interactive questionnaire: the model poses choices, the TUI user picks.

use std::collections::HashSet;
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::sync::oneshot;

use crate::error::{Error, Result};
use crate::events::{AgentEvent, EventMeta, EventSink};
use crate::tools::{ClientTool, ToolCallFut, ToolSpec};

pub const MAX_OPTIONS: usize = 8;
pub const MAX_QUESTION: usize = 240;
pub const MAX_LABEL: usize = 80;
pub const MAX_ID: usize = 40;
pub const MAX_INPUT: usize = 500;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Choice {
    pub id: String,
    pub label: String,
    #[serde(default)]
    pub input: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Question {
    pub prompt: String,
    pub allow_multiple: bool,
    pub options: Vec<Choice>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Picked {
    pub id: String,
    pub label: String,
    pub value: Option<String>,
}

pub fn cancelled_json() -> String {
    json!({"cancelled": true}).to_string()
}

pub fn encode_answer(selected: &[Picked]) -> String {
    json!({
        "cancelled": false,
        "selected": selected.iter().map(|p| {
            json!({
                "id": p.id,
                "label": p.label,
                "value": p.value,
            })
        }).collect::<Vec<_>>()
    })
    .to_string()
}

pub fn parse_question(args: &Value) -> Result<Question> {
    let raw = args
        .get("question")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim();
    if raw.is_empty() {
        return Err(Error::Tool("question is required".into()));
    }
    let prompt = clip(raw, MAX_QUESTION);
    let allow_multiple = args
        .get("allow_multiple")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let Some(arr) = args.get("options").and_then(Value::as_array) else {
        return Err(Error::Tool("options must be a non-empty array".into()));
    };
    if arr.is_empty() {
        return Err(Error::Tool("options must be a non-empty array".into()));
    }
    if arr.len() > MAX_OPTIONS {
        return Err(Error::Tool(format!("at most {MAX_OPTIONS} options")));
    }
    let mut options = Vec::with_capacity(arr.len());
    let mut ids = HashSet::new();
    for (i, item) in arr.iter().enumerate() {
        let choice = parse_choice(item, i)?;
        if !ids.insert(choice.id.clone()) {
            return Err(Error::Tool(format!("duplicate option id {}", choice.id)));
        }
        options.push(choice);
    }
    Ok(Question {
        prompt,
        allow_multiple,
        options,
    })
}

fn parse_choice(item: &Value, index: usize) -> Result<Choice> {
    if let Some(s) = item.as_str() {
        let label = clip(s.trim(), MAX_LABEL);
        if label.is_empty() {
            return Err(Error::Tool(format!("option {index} is empty")));
        }
        return Ok(Choice {
            id: format!("opt{index}"),
            label,
            input: false,
        });
    }
    let obj = item
        .as_object()
        .ok_or_else(|| Error::Tool(format!("option {index} must be a string or object")))?;
    let label = obj
        .get("label")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim();
    if label.is_empty() {
        return Err(Error::Tool(format!("option {index} needs a label")));
    }
    let id_raw = obj
        .get("id")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| clip(s, MAX_ID))
        .unwrap_or_else(|| format!("opt{index}"));
    Ok(Choice {
        id: id_raw,
        label: clip(label, MAX_LABEL),
        input: obj.get("input").and_then(Value::as_bool).unwrap_or(false),
    })
}

pub fn answer_from_picks(
    question: &Question,
    chosen: &[bool],
    values: &[String],
) -> std::result::Result<String, String> {
    if chosen.len() != question.options.len() || values.len() != question.options.len() {
        return Err("選項狀態不一致".into());
    }
    let n = chosen.iter().filter(|c| **c).count();
    if n == 0 {
        return Err("請選一個選項".into());
    }
    if !question.allow_multiple && n > 1 {
        return Err("只能選一個選項".into());
    }
    let mut picked = Vec::new();
    for (i, opt) in question.options.iter().enumerate() {
        if !chosen[i] {
            continue;
        }
        let value = if opt.input {
            let v = values[i].trim();
            if v.is_empty() {
                return Err(format!("請填寫「{}」", opt.label));
            }
            Some(clip(v, MAX_INPUT))
        } else {
            None
        };
        picked.push(Picked {
            id: opt.id.clone(),
            label: opt.label.clone(),
            value,
        });
    }
    Ok(encode_answer(&picked))
}

fn clip(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        s.chars().take(max).collect()
    }
}

struct HubInner {
    waiter: Option<oneshot::Sender<String>>,
    queued: Option<String>,
}

#[derive(Clone)]
pub struct AskUserHub {
    inner: Arc<Mutex<HubInner>>,
}

impl AskUserHub {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Mutex::new(HubInner {
                waiter: None,
                queued: None,
            })),
        }
    }

    /// Register the next waiter. A previously queued answer is delivered immediately.
    pub fn register(&self) -> oneshot::Receiver<String> {
        let (tx, rx) = oneshot::channel();
        let mut g = self.inner.lock().unwrap();
        if let Some(prev) = g.waiter.take() {
            let _ = prev.send(cancelled_json());
        }
        if let Some(ans) = g.queued.take() {
            let _ = tx.send(ans);
        } else {
            g.waiter = Some(tx);
        }
        rx
    }

    pub fn answer(&self, body: String) {
        let mut g = self.inner.lock().unwrap();
        if let Some(tx) = g.waiter.take() {
            let _ = tx.send(body);
        } else {
            g.queued = Some(body);
        }
    }

    pub fn cancel(&self) {
        let mut g = self.inner.lock().unwrap();
        if let Some(tx) = g.waiter.take() {
            let _ = tx.send(cancelled_json());
        }
    }
}

pub struct AskUserTool {
    hub: AskUserHub,
    sink: Arc<dyn EventSink>,
    agent_name: String,
    run_id: String,
    parent_run_id: Option<String>,
}

impl AskUserTool {
    pub fn new(
        hub: AskUserHub,
        sink: Arc<dyn EventSink>,
        agent_name: String,
        run_id: String,
        parent_run_id: Option<String>,
    ) -> Self {
        Self {
            hub,
            sink,
            agent_name,
            run_id,
            parent_run_id,
        }
    }

    fn meta(&self) -> EventMeta {
        EventMeta {
            ts: chrono::Utc::now(),
            agent_name: self.agent_name.clone(),
            run_id: self.run_id.clone(),
            parent_run_id: self.parent_run_id.clone(),
        }
    }
}

impl ClientTool for AskUserTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "ask_user".into(),
            description: "Ask the user a multiple-choice question in the TUI. They pick with mouse or arrow keys. Set input=true on an option to let them type a custom value. Use this when you need a decision, not a free-form chat reply.".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "question": {
                        "type": "string",
                        "description": "The question shown to the user"
                    },
                    "allow_multiple": {
                        "type": "boolean",
                        "description": "If true, the user may pick more than one option"
                    },
                    "options": {
                        "type": "array",
                        "description": "2–8 choices. Each is a string, or {id, label, input?}. input=true means the user types a value for that choice.",
                        "items": {
                            "anyOf": [
                                {"type": "string"},
                                {
                                    "type": "object",
                                    "properties": {
                                        "id": {"type": "string"},
                                        "label": {"type": "string"},
                                        "input": {"type": "boolean"}
                                    },
                                    "required": ["label"]
                                }
                            ]
                        }
                    }
                },
                "required": ["question", "options"],
                "additionalProperties": false
            }),
        }
    }

    fn call(&self, args: &Value) -> ToolCallFut<'_> {
        let args = args.clone();
        let hub = self.hub.clone();
        let sink = self.sink.clone();
        let meta = self.meta();
        Box::pin(async move {
            let q = parse_question(&args)?;
            let rx = hub.register();
            sink.emit(&AgentEvent::AskUser {
                meta,
                question: q.prompt,
                allow_multiple: q.allow_multiple,
                options: q.options,
            });
            Ok(rx.await.unwrap_or_else(|_| cancelled_json()))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn q(v: Value) -> Question {
        parse_question(&v).unwrap()
    }

    #[test]
    fn parse_rejects_empty_and_too_many() {
        assert!(parse_question(&json!({"question":"x","options":[]})).is_err());
        assert!(parse_question(&json!({"options":["a"]})).is_err());
        let many: Vec<_> = (0..9).map(|i| format!("o{i}")).collect();
        assert!(parse_question(&json!({"question":"x","options": many})).is_err());
    }

    #[test]
    fn parse_accepts_strings_and_objects() {
        let q = q(json!({
            "question": "挑一個",
            "options": ["重試", {"id":"skip","label":"跳過"}, {"id":"other","label":"其他","input":true}]
        }));
        assert_eq!(q.prompt, "挑一個");
        assert!(!q.allow_multiple);
        assert_eq!(q.options[0].id, "opt0");
        assert_eq!(q.options[0].label, "重試");
        assert!(!q.options[0].input);
        assert_eq!(q.options[1].id, "skip");
        assert!(q.options[2].input);
    }

    #[test]
    fn parse_rejects_duplicate_ids() {
        assert!(parse_question(&json!({
            "question": "x",
            "options": [{"id":"a","label":"1"},{"id":"a","label":"2"}]
        }))
        .is_err());
    }

    #[test]
    fn answer_requires_a_choice_and_fill_in_text() {
        let question = q(json!({
            "question": "怎麼辦",
            "options": [
                {"id":"a","label":"A"},
                {"id":"b","label":"自填","input":true}
            ]
        }));
        assert_eq!(
            answer_from_picks(&question, &[false, false], &["", ""].map(String::from)).unwrap_err(),
            "請選一個選項"
        );
        assert_eq!(
            answer_from_picks(&question, &[false, true], &["", ""].map(String::from)).unwrap_err(),
            "請填寫「自填」"
        );
        let ok = answer_from_picks(
            &question,
            &[false, true],
            &["", "用 SQLite"].map(String::from),
        )
        .unwrap();
        let v: Value = serde_json::from_str(&ok).unwrap();
        assert_eq!(v["cancelled"], false);
        assert_eq!(v["selected"][0]["id"], "b");
        assert_eq!(v["selected"][0]["value"], "用 SQLite");
    }

    #[test]
    fn multi_select_encodes_all_picks() {
        let question = q(json!({
            "question": "哪些",
            "allow_multiple": true,
            "options": ["A", "B", "C"]
        }));
        let ok =
            answer_from_picks(&question, &[true, false, true], &["", "", ""].map(String::from))
                .unwrap();
        let v: Value = serde_json::from_str(&ok).unwrap();
        assert_eq!(v["selected"].as_array().unwrap().len(), 2);
        assert_eq!(v["selected"][0]["id"], "opt0");
        assert_eq!(v["selected"][1]["id"], "opt2");
        assert!(v["selected"][0]["value"].is_null());
    }

    struct Rec(Mutex<Vec<AgentEvent>>);
    impl EventSink for Rec {
        fn emit(&self, event: &AgentEvent) {
            self.0.lock().unwrap().push(event.clone());
        }
    }

    #[tokio::test]
    async fn queued_answer_unblocks_the_tool() {
        let hub = AskUserHub::new();
        hub.answer(encode_answer(&[Picked {
            id: "a".into(),
            label: "A".into(),
            value: None,
        }]));
        let rec = Arc::new(Rec(Mutex::new(Vec::new())));
        let sink: Arc<dyn EventSink> = rec.clone();
        let tool = AskUserTool::new(
            hub,
            sink,
            "root".into(),
            "r".into(),
            None,
        );
        let out = tool
            .call(&json!({
                "question": "Q",
                "options": [{"id":"a","label":"A"},{"id":"b","label":"B"}]
            }))
            .await
            .unwrap();
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["selected"][0]["id"], "a");
        let evs = rec.0.lock().unwrap();
        assert!(
            evs.iter()
                .any(|e| matches!(e, AgentEvent::AskUser { question, .. } if question == "Q")),
            "{evs:?}"
        );
    }

    #[tokio::test]
    async fn cancel_unblocks_waiter() {
        let hub = AskUserHub::new();
        let rx = hub.register();
        hub.cancel();
        let body = rx.await.unwrap();
        let v: Value = serde_json::from_str(&body).unwrap();
        assert_eq!(v["cancelled"], true);
    }

    #[test]
    fn cancel_without_waiter_does_not_poison_next_register() {
        let hub = AskUserHub::new();
        hub.cancel();
        let mut rx = hub.register();
        assert!(
            rx.try_recv().is_err(),
            "cancel with no waiter must not queue a fake answer"
        );
    }

    #[test]
    fn answer_before_register_is_not_lost() {
        let hub = AskUserHub::new();
        hub.answer("{\"cancelled\":false}".into());
        let rx = hub.register();
        let mut rx = rx;
        let body = rx.try_recv().expect("queued answer should be ready");
        assert!(body.contains("cancelled"));
    }
}
