use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::Path;
use std::sync::Mutex;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::ask::Choice;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct EventMeta {
    pub ts: DateTime<Utc>,
    pub agent_name: String,
    pub run_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent_run_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AgentEvent {
    RunStarted {
        #[serde(flatten)]
        meta: EventMeta,
        model: String,
    },
    TurnStarted {
        #[serde(flatten)]
        meta: EventMeta,
        turn: u32,
    },
    ModelFinished {
        #[serde(flatten)]
        meta: EventMeta,
        text: String,
        finish: String,
        input_tokens: u32,
        cached_tokens: u32,
    },
    ToolStarted {
        #[serde(flatten)]
        meta: EventMeta,
        call_id: String,
        name: String,
        args: Value,
        kind: String,
    },
    ToolFinished {
        #[serde(flatten)]
        meta: EventMeta,
        call_id: String,
        name: String,
        output: String,
    },
    ServerToolObserved {
        #[serde(flatten)]
        meta: EventMeta,
        kind: String,
        payload: Value,
    },
    ContextCompacted {
        #[serde(flatten)]
        meta: EventMeta,
        input_tokens: u32,
        window: u32,
        dropped_items: usize,
        kept_items: usize,
        method: String,
    },
    Error {
        #[serde(flatten)]
        meta: EventMeta,
        message: String,
    },
    ChildSpawned {
        #[serde(flatten)]
        meta: EventMeta,
        name: String,
        agent_card_url: String,
        #[serde(default)]
        prompt: String,
    },
    ChildExited {
        #[serde(flatten)]
        meta: EventMeta,
        name: String,
        detail: String,
    },
    RunFinished {
        #[serde(flatten)]
        meta: EventMeta,
        reason: String,
        text: String,
    },
    /// Model stopped; the session is waiting for the next user line.
    AwaitingInput {
        #[serde(flatten)]
        meta: EventMeta,
    },
    /// Streaming token(s) from the model.
    ModelDelta {
        #[serde(flatten)]
        meta: EventMeta,
        text: String,
    },
    /// Streaming reasoning / thinking summary (not the visible answer).
    ReasoningDelta {
        #[serde(flatten)]
        meta: EventMeta,
        text: String,
    },
    FileChanged {
        #[serde(flatten)]
        meta: EventMeta,
        path: String,
        kind: String,
        diff: String,
    },
    MonitorAttached {
        #[serde(flatten)]
        meta: EventMeta,
        name: String,
        command: String,
        pid: u32,
    },
    MonitorExited {
        #[serde(flatten)]
        meta: EventMeta,
        name: String,
        detail: String,
    },
    BackgroundStarted {
        #[serde(flatten)]
        meta: EventMeta,
        name: String,
        command: String,
        pid: u32,
    },
    BackgroundOutput {
        #[serde(flatten)]
        meta: EventMeta,
        name: String,
        stream: String,
        text: String,
    },
    BackgroundExited {
        #[serde(flatten)]
        meta: EventMeta,
        name: String,
        detail: String,
    },
    TimerStarted {
        #[serde(flatten)]
        meta: EventMeta,
        name: String,
        seconds: u64,
        command: String,
    },
    TimerFired {
        #[serde(flatten)]
        meta: EventMeta,
        name: String,
        detail: String,
    },
    TimerCancelled {
        #[serde(flatten)]
        meta: EventMeta,
        name: String,
    },
    /// Kernel warning that must not go to stderr (TUI cursor sits in the composer).
    Notice {
        #[serde(flatten)]
        meta: EventMeta,
        message: String,
    },
    /// LLM (or fallback) session title for the sidebar. `run_id` is the session id.
    SessionNamed {
        #[serde(flatten)]
        meta: EventMeta,
        name: String,
    },
    /// A2A text between parent and a named child (spawn prompt, send_message, reply).
    AgentMessage {
        #[serde(flatten)]
        meta: EventMeta,
        from: String,
        to: String,
        text: String,
    },
    /// Model posed a questionnaire; the TUI should collect a pick.
    AskUser {
        #[serde(flatten)]
        meta: EventMeta,
        question: String,
        allow_multiple: bool,
        options: Vec<Choice>,
    },
}

impl AgentEvent {
    pub fn meta(&self) -> &EventMeta {
        match self {
            Self::RunStarted { meta, .. }
            | Self::TurnStarted { meta, .. }
            | Self::ModelFinished { meta, .. }
            | Self::ToolStarted { meta, .. }
            | Self::ToolFinished { meta, .. }
            | Self::ServerToolObserved { meta, .. }
            | Self::ContextCompacted { meta, .. }
            | Self::Error { meta, .. }
            | Self::ChildSpawned { meta, .. }
            | Self::ChildExited { meta, .. }
            | Self::RunFinished { meta, .. }
            | Self::AwaitingInput { meta, .. }
            | Self::ModelDelta { meta, .. }
            | Self::ReasoningDelta { meta, .. }
            | Self::FileChanged { meta, .. }
            | Self::MonitorAttached { meta, .. }
            | Self::MonitorExited { meta, .. }
            | Self::BackgroundStarted { meta, .. }
            | Self::BackgroundOutput { meta, .. }
            | Self::BackgroundExited { meta, .. }
            | Self::TimerStarted { meta, .. }
            | Self::TimerFired { meta, .. }
            | Self::TimerCancelled { meta, .. }
            | Self::Notice { meta, .. }
            | Self::SessionNamed { meta, .. }
            | Self::AgentMessage { meta, .. }
            | Self::AskUser { meta, .. } => meta,
        }
    }

    pub fn run_id(&self) -> &str {
        &self.meta().run_id
    }

    /// Session this event belongs to in the TUI: child's parent run, else own run.
    pub fn session_id(&self) -> &str {
        self.meta()
            .parent_run_id
            .as_deref()
            .filter(|s| !s.is_empty())
            .unwrap_or(self.run_id())
    }

    pub fn is_child_work(&self) -> bool {
        self.meta()
            .parent_run_id
            .as_deref()
            .is_some_and(|s| !s.is_empty())
    }
}

pub trait EventSink: Send + Sync {
    fn emit(&self, event: &AgentEvent);
}

impl<T: EventSink + ?Sized> EventSink for std::sync::Arc<T> {
    fn emit(&self, event: &AgentEvent) {
        (**self).emit(event);
    }
}

pub struct JsonlSink {
    file: Mutex<File>,
}

impl JsonlSink {
    pub fn create(path: &Path) -> std::io::Result<Self> {
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent)?;
            }
        }
        let file = OpenOptions::new().create(true).append(true).open(path)?;
        Ok(Self {
            file: Mutex::new(file),
        })
    }
}

impl EventSink for JsonlSink {
    fn emit(&self, event: &AgentEvent) {
        let Ok(line) = serde_json::to_string(event) else {
            return;
        };
        if let Ok(mut file) = self.file.lock() {
            let _ = writeln!(file, "{line}");
            let _ = file.flush();
        }
    }
}

pub struct FanoutSink {
    pub sinks: Vec<Box<dyn EventSink>>,
}

impl EventSink for FanoutSink {
    fn emit(&self, event: &AgentEvent) {
        for s in &self.sinks {
            s.emit(event);
        }
    }
}

pub struct ChannelSink {
    tx: tokio::sync::mpsc::UnboundedSender<AgentEvent>,
}

impl ChannelSink {
    pub fn new(tx: tokio::sync::mpsc::UnboundedSender<AgentEvent>) -> Self {
        Self { tx }
    }
}

impl EventSink for ChannelSink {
    fn emit(&self, event: &AgentEvent) {
        let _ = self.tx.send(event.clone());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;

    #[test]
    fn jsonl_appends_one_object_per_line() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("events.jsonl");
        let sink = JsonlSink::create(&path).unwrap();
        let meta = EventMeta {
            ts: DateTime::from_timestamp(1_700_000_000, 0).unwrap(),
            agent_name: "root".into(),
            run_id: "run-1".into(),
            parent_run_id: None,
        };
        sink.emit(&AgentEvent::RunStarted {
            meta: meta.clone(),
            model: "grok-4.6".into(),
        });
        sink.emit(&AgentEvent::RunFinished {
            meta,
            reason: "stop".into(),
            text: "hi".into(),
        });
        let mut raw = String::new();
        File::open(&path).unwrap().read_to_string(&mut raw).unwrap();
        let lines: Vec<&str> = raw.trim().lines().collect();
        assert_eq!(lines.len(), 2);
        let first: Value = serde_json::from_str(lines[0]).unwrap();
        assert_eq!(first["type"], "run_started");
        assert_eq!(first["model"], "grok-4.6");
        assert_eq!(first["run_id"], "run-1");
        let second: Value = serde_json::from_str(lines[1]).unwrap();
        assert_eq!(second["type"], "run_finished");
        assert_eq!(second["text"], "hi");
    }

    #[test]
    fn run_id_reads_from_every_variant() {
        let meta = EventMeta {
            ts: DateTime::from_timestamp(1_700_000_000, 0).unwrap(),
            agent_name: "root".into(),
            run_id: "sess-9".into(),
            parent_run_id: None,
        };
        let named = AgentEvent::SessionNamed {
            meta: meta.clone(),
            name: "修 sidebar".into(),
        };
        assert_eq!(named.run_id(), "sess-9");
        assert_eq!(named.meta().agent_name, "root");
        let delta = AgentEvent::ModelDelta {
            meta: meta.clone(),
            text: "x".into(),
        };
        assert_eq!(delta.run_id(), "sess-9");
        let child = AgentEvent::ModelDelta {
            meta: EventMeta {
                ts: meta.ts,
                agent_name: "coder".into(),
                run_id: "child-1".into(),
                parent_run_id: Some("sess-9".into()),
            },
            text: "x".into(),
        };
        assert_eq!(child.session_id(), "sess-9");
        assert!(child.is_child_work());
        assert!(!delta.is_child_work());
        assert_eq!(delta.session_id(), "sess-9");
        let ask = AgentEvent::AskUser {
            meta: meta.clone(),
            question: "挑一個".into(),
            allow_multiple: false,
            options: vec![Choice {
                id: "a".into(),
                label: "A".into(),
                input: false,
            }],
        };
        assert_eq!(ask.run_id(), "sess-9");
        assert_eq!(ask.session_id(), "sess-9");
    }
}
