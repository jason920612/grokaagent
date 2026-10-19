//! A transcript: the rows of one agent's conversation plus its live status.
//! The main chat and every child agent each own one; `apply` turns the agent's
//! own events into rows the same way for both.

use std::time::Instant;

use serde_json::Value;

use crate::events::AgentEvent;
use crate::md;

use super::rows::{
    parse_file_changes, picture_from_tool, tool_phase, AgentMsg, FileChange, Row, Think, ToolCall,
    ToolGroup,
};
use super::tool_text::{
    canonical_server_tool, live_tool_activity, phase_is_done, server_phase, server_query,
    server_tool_line,
};

/// Rows kept per transcript; past it the oldest quarter is dropped.
pub(crate) const ROWS_CAP: usize = 2_000;

pub(crate) struct Transcript {
    pub rows: Vec<Row>,
    pub status: String,
    pub activity: String,
    pub cache: String,
    pub running: bool,
    pub awaiting: bool,
    /// The last row is an agent row still receiving deltas.
    pub streaming: bool,
    /// The next tool call starts a new group instead of joining the last one.
    pub seal_tools: bool,
    pub work_started: Option<Instant>,
    /// Bumped when rows are dropped from the front: older row indices are stale.
    pub epoch: u64,
    cap: usize,
}

impl Default for Transcript {
    fn default() -> Self {
        Self::new(Vec::new())
    }
}

impl Transcript {
    pub fn new(rows: Vec<Row>) -> Self {
        Self {
            rows,
            status: "待命".into(),
            activity: String::new(),
            cache: "cache —".into(),
            running: false,
            awaiting: false,
            streaming: false,
            seal_tools: false,
            work_started: None,
            epoch: 0,
            cap: ROWS_CAP,
        }
    }

    pub fn with_cap(mut self, cap: usize) -> Self {
        self.cap = cap.max(10);
        self
    }

    pub fn push(&mut self, row: Row) {
        if matches!(&row, Row::User(_) | Row::Agent(_)) {
            self.seal_tools = true;
            self.finish_open_think();
        }
        self.rows.push(row);
        if self.rows.len() > self.cap {
            let keep = (self.cap * 3 / 4).max(1);
            let cut = self.rows.len() - keep;
            self.rows.drain(0..cut);
            self.epoch += 1;
        }
    }

    pub fn mark_work_start(&mut self) {
        if self.work_started.is_none() {
            self.work_started = Some(Instant::now());
        }
    }

    /// Record how long the finished work took on the last agent row.
    pub fn stamp_work(&mut self) {
        let Some(start) = self.work_started.take() else {
            return;
        };
        let ms = start.elapsed().as_millis() as u64;
        if let Some(Row::Agent(a)) = self.rows.iter_mut().rev().find(|r| matches!(r, Row::Agent(_))) {
            a.work_ms = ms;
            return;
        }
        self.push(Row::meta(format!("工作 {}", md::fmt_duration(ms))));
    }

    pub fn finish_open_think(&mut self) {
        if let Some(Row::Think(t)) = self.rows.iter_mut().rev().find(|r| matches!(r, Row::Think(_))) {
            if !t.done {
                if let Some(start) = t.started.take() {
                    t.elapsed_ms = start.elapsed().as_millis() as u64;
                }
                t.done = true;
            }
        }
    }

    /// The user interrupted: drop the stream, wait for the loop to confirm.
    pub fn interrupt(&mut self) {
        self.streaming = false;
        self.finish_open_think();
        self.activity = "中斷中".into();
        self.status = "中斷中".into();
    }

    fn append_think(&mut self, delta: &str) {
        self.activity = "思考中".into();
        self.streaming = false;
        if let Some(Row::Think(t)) = self.rows.last_mut() {
            if !t.done {
                t.text.push_str(delta);
                t.started.get_or_insert_with(Instant::now);
                return;
            }
        }
        self.push(Row::Think(Think {
            text: delta.to_string(),
            started: Some(Instant::now()),
            ..Think::default()
        }));
    }

    fn push_call(&mut self, call: ToolCall) {
        if !self.seal_tools {
            if let Some(Row::Tools(g)) = self.rows.last_mut() {
                g.calls.push(call);
                return;
            }
        }
        self.push(Row::Tools(ToolGroup {
            calls: vec![call],
            expanded: false,
        }));
        self.seal_tools = false;
    }

    pub fn tool_started(&mut self, call_id: &str, name: &str, args: &Value) {
        self.streaming = false;
        self.finish_open_think();
        self.activity = live_tool_activity(name, args, "執行中");
        self.push_call(ToolCall::started(call_id.to_string(), name.to_string(), args.clone()));
    }

    /// Finish a call: by id anywhere in the transcript, else the newest open
    /// call of that name (rows restored from before call ids existed).
    /// Record a tool's result. A result reported again (no pixels went out
    /// after all) replaces the first and returns it.
    pub fn tool_finished(&mut self, call_id: &str, name: &str, output: &str) -> Option<String> {
        let by_id = (!call_id.is_empty())
            .then(|| {
                self.rows.iter().rposition(|r| {
                    matches!(r, Row::Tools(g) if g.calls.iter().any(|c| c.call_id == call_id))
                })
            })
            .flatten();
        let group = by_id.or_else(|| self.rows.iter().rposition(|r| matches!(r, Row::Tools(_))));
        let Some(Row::Tools(g)) = group.and_then(|i| self.rows.get_mut(i)) else {
            return None;
        };
        let idx = (!call_id.is_empty())
            .then(|| g.calls.iter().rposition(|c| c.call_id == call_id))
            .flatten()
            .or_else(|| g.calls.iter().rposition(|c| c.name == name && !c.done))
            .or_else(|| g.calls.iter().rposition(|c| !c.done))
            .or_else(|| g.calls.len().checked_sub(1));
        let i = idx?;
        let c = &mut g.calls[i];
        let again = c.done && !call_id.is_empty() && c.call_id == call_id;
        c.done = true;
        c.phase = tool_phase(name, output).into();
        if !again {
            c.files.extend(parse_file_changes(output));
        }
        let before = std::mem::replace(&mut c.output, output.to_string());
        again.then_some(before)
    }

    fn attach_file(&mut self, file: FileChange) {
        if let Some(Row::Tools(g)) = self.rows.iter_mut().rev().find(|r| matches!(r, Row::Tools(_))) {
            if let Some(c) = g.calls.last_mut() {
                if !c.files.iter().any(|f| f.path == file.path) {
                    c.files.push(file);
                }
            }
        }
    }

    fn observe_server(&mut self, kind: &str, payload: &Value) {
        let name = canonical_server_tool(kind);
        let phase = server_phase(kind, payload);
        let query = server_query(payload);
        self.activity = if query.is_empty() {
            format!("{name}  {phase}")
        } else {
            format!("{name}  {phase}  {query}")
        };
        let done = phase_is_done(&phase);
        if !self.seal_tools {
            if let Some(Row::Tools(g)) = self.rows.last_mut() {
                if let Some(c) = g.calls.iter_mut().rev().find(|c| c.name == name) {
                    if !query.is_empty() {
                        c.args["query"] = Value::String(query);
                    }
                    c.phase = phase;
                    c.output = server_tool_line(kind, &c.args);
                    c.done = done;
                    return;
                }
            }
        }
        let mut args = payload.clone();
        if args.get("query").is_none() && !query.is_empty() && args.is_object() {
            args["query"] = Value::String(query);
        }
        let mut call = ToolCall::started(String::new(), name, args);
        call.output = server_tool_line(kind, &call.args);
        call.phase = phase;
        call.done = done;
        self.push_call(call);
    }

    fn mark_open_server_done(&mut self) {
        if let Some(Row::Tools(g)) = self.rows.last_mut() {
            for c in g.calls.iter_mut().filter(|c| !c.done) {
                if c.name == "web_search" || c.name == "x_search" {
                    c.done = true;
                    c.phase = "完成".into();
                }
            }
        }
    }

    /// Collapse expanded groups/thinks; `true` if anything changed.
    pub fn collapse_all(&mut self) -> bool {
        let mut any = false;
        for r in &mut self.rows {
            match r {
                Row::Tools(g) if g.expanded => {
                    g.expanded = false;
                    any = true;
                }
                Row::Think(t) if t.expanded => {
                    t.expanded = false;
                    any = true;
                }
                _ => {}
            }
        }
        any
    }

    /// Apply one of this agent's own events. `skip_steer` = the task
    /// supervisor was told to stand down (an idle then reads as stopped).
    /// Returns `false` for events that are not about the transcript.
    pub fn apply(&mut self, ev: &AgentEvent, skip_steer: bool) -> bool {
        match ev {
            AgentEvent::RunStarted { model, .. } => {
                self.running = true;
                self.awaiting = false;
                self.streaming = false;
                self.mark_work_start();
                self.activity = format!("連線 {model}");
                self.status = "工作中".into();
            }
            AgentEvent::TurnStarted { turn, .. } => {
                self.running = true;
                self.awaiting = false;
                self.streaming = false;
                self.mark_work_start();
                self.seal_tools = true;
                self.finish_open_think();
                self.mark_open_server_done();
                self.activity = "思考中".into();
                self.status = format!("第 {turn} 輪");
            }
            AgentEvent::ReasoningDelta { text, .. } => {
                if !text.is_empty() {
                    self.append_think(text);
                }
            }
            AgentEvent::ModelDelta { text, .. } => {
                if text.is_empty() {
                    return true;
                }
                self.activity = "撰寫中".into();
                self.finish_open_think();
                if self.streaming {
                    if let Some(Row::Agent(a)) = self.rows.last_mut() {
                        a.text.push_str(text);
                        return true;
                    }
                }
                self.push(Row::Agent(AgentMsg::new(text.clone())));
                self.streaming = true;
                self.seal_tools = true;
            }
            AgentEvent::ModelFinished {
                text,
                input_tokens,
                cached_tokens,
                ..
            } => {
                self.streaming = false;
                self.finish_open_think();
                if *input_tokens > 0 {
                    let pct = (*cached_tokens as f32 / *input_tokens as f32) * 100.0;
                    self.cache = format!("{cached_tokens}/{input_tokens} ({pct:.0}%)");
                }
                if text.is_empty() {
                    return true;
                }
                // A think or server-tool row can land after the streamed text:
                // replace the streamed partial within this turn, do not repeat it.
                for r in self.rows.iter_mut().rev() {
                    match r {
                        // A proxy message is a user turn to the model.
                        Row::User(_) | Row::Proxy { .. } => break,
                        Row::Agent(a) => {
                            if a.text.is_empty()
                                || text.starts_with(a.text.as_str())
                                || a.text.starts_with(text.as_str())
                            {
                                a.text = text.clone();
                                return true;
                            }
                            break;
                        }
                        _ => {}
                    }
                }
                self.push(Row::Agent(AgentMsg::new(text.clone())));
            }
            AgentEvent::ToolStarted {
                call_id, name, args, ..
            } => self.tool_started(call_id, name, args),
            AgentEvent::ToolFinished {
                call_id,
                name,
                output,
                ..
            } => {
                match self.tool_finished(call_id, name, output) {
                    None => {
                        if let Some((path, label, source)) = picture_from_tool(name, output) {
                            self.push(Row::Picture { path, label, source });
                        }
                        self.activity = "思考中".into();
                    }
                    // Reported again: the pixels it claimed never went out.
                    Some(before) => {
                        if let (Some((p, l, _)), None) = (picture_from_tool(name, &before), picture_from_tool(name, output)) {
                            let shown = self
                                .rows
                                .iter()
                                .rposition(|r| matches!(r, Row::Picture { path, label, .. } if *path == p && *label == l));
                            if let Some(i) = shown {
                                self.rows.remove(i);
                                self.epoch += 1;
                            }
                        }
                    }
                }
            }
            AgentEvent::ServerToolObserved { kind, payload, .. } => {
                self.streaming = false;
                self.finish_open_think();
                if kind == "gateway" {
                    if let Some(msg) = payload.get("error").and_then(Value::as_str) {
                        self.push(Row::Err(msg.to_string()));
                    }
                } else {
                    self.observe_server(kind, payload);
                }
            }
            AgentEvent::FileChanged {
                path, kind, diff, ..
            } => {
                self.streaming = false;
                self.attach_file(FileChange {
                    path: path.clone(),
                    kind: kind.clone(),
                    diff: diff.clone(),
                });
            }
            AgentEvent::ContextCompacted {
                method,
                dropped_items,
                kept_items,
                input_tokens,
                window,
                ..
            } => self.push(Row::meta(super::tool_text::compact_line(
                method,
                *dropped_items,
                *kept_items,
                *input_tokens,
                *window,
            ))),
            AgentEvent::Notice { message, .. } => self.push(Row::meta(message.clone())),
            AgentEvent::Error { message, .. } => {
                self.streaming = false;
                self.push(Row::Err(message.clone()));
            }
            AgentEvent::AwaitingInput { .. } => {
                let stopped = self.activity == "中斷中" || skip_steer;
                self.running = false;
                self.awaiting = true;
                self.streaming = false;
                self.finish_open_think();
                self.stamp_work();
                self.activity.clear();
                self.status = if stopped { "已停止" } else { "待命" }.into();
            }
            AgentEvent::RunFinished { reason, text, .. } => {
                self.running = false;
                self.awaiting = false;
                self.streaming = false;
                self.finish_open_think();
                self.activity.clear();
                self.status = format!("結束 ({reason})");
                if !text.is_empty()
                    && !self
                        .rows
                        .iter()
                        .any(|r| matches!(r, Row::Agent(a) if &a.text == text))
                {
                    self.push(Row::Agent(AgentMsg::new(text.clone())));
                }
                self.stamp_work();
            }
            _ => return false,
        }
        true
    }

    /// The spawn call for `child` (a child agent, or a workflow run) is
    /// over: its process or run ended. Its row says how, instead of the
    /// "started" it said when the call returned.
    /// Returns the call's id and its new phase.
    pub fn mark_spawn_done(&mut self, child: &str, detail: &str) -> Option<(String, &'static str)> {
        let phase = if detail.starts_with("失敗") {
            "失敗"
        } else if detail.starts_with("已停止") {
            "已停止"
        } else {
            "完成"
        };
        for r in self.rows.iter_mut().rev() {
            let Row::Tools(g) = r else { continue };
            for c in g.calls.iter_mut().rev().filter(|c| c.name == "spawn_agent" || c.name == "workflow") {
                let out = serde_json::from_str::<Value>(&c.output).ok();
                let named = if c.name == "workflow" {
                    // A workflow's tree node is its run id.
                    out.as_ref().and_then(|v| v.get("run_id").and_then(Value::as_str)) == Some(child)
                } else {
                    out.as_ref()
                        .and_then(|v| v.get("name").and_then(Value::as_str).map(|n| n == child))
                        .unwrap_or(false)
                        || c.args.get("name").and_then(Value::as_str) == Some(child)
                };
                if named {
                    c.done = true;
                    c.phase = phase.into();
                    return Some((c.call_id.clone(), phase));
                }
            }
        }
        None
    }

    /// Every file change: (row, call, path, kind).
    pub fn file_changes(&self) -> Vec<(usize, usize, String, String)> {
        let mut out = Vec::new();
        for (ri, row) in self.rows.iter().enumerate() {
            if let Row::Tools(g) = row {
                for (ci, call) in g.calls.iter().enumerate() {
                    for f in &call.files {
                        out.push((ri, ci, f.path.clone(), f.kind.clone()));
                    }
                }
            }
        }
        out
    }

    /// Locate a tool call by id.
    pub fn find_call(&self, call_id: &str) -> Option<(usize, usize)> {
        if call_id.is_empty() {
            return None;
        }
        self.rows.iter().enumerate().rev().find_map(|(ri, r)| match r {
            Row::Tools(g) => g.calls.iter().position(|c| c.call_id == call_id).map(|ci| (ri, ci)),
            _ => None,
        })
    }

    pub fn call(&self, row: usize, call: usize) -> Option<&ToolCall> {
        match self.rows.get(row)? {
            Row::Tools(g) => g.calls.get(call),
            _ => None,
        }
    }

    /// Only meta rows so far (a blank draft).
    pub fn is_blank(&self) -> bool {
        self.rows.iter().all(|r| matches!(r, Row::Meta(_)))
    }
}

#[cfg(test)]
mod tests {

    #[test]
    fn a_finished_workflow_flips_its_started_call() {
        let mut t = Transcript::new(Vec::new());
        t.tool_started("c1", "workflow", &serde_json::json!({"script": "x"}));
        t.tool_finished("c1", "workflow", r#"{"run_id":"wf-1","status":"running"}"#);
        t.tool_started("c2", "now", &serde_json::json!({}));
        t.tool_finished("c2", "now", "{}");
        assert_eq!(t.mark_spawn_done("wf-1", "失敗：boom"), Some(("c1".to_string(), "失敗")));
        let phase = t.rows.iter().find_map(|r| match r {
            Row::Tools(g) => g.calls.iter().find(|c| c.call_id == "c1").map(|c| c.phase.clone()),
            _ => None,
        });
        assert_eq!(phase.as_deref(), Some("失敗"));
        assert_eq!(t.mark_spawn_done("wf-2", "完成"), None);
    }

    use super::*;
    use crate::events::EventMeta;
    use serde_json::json;

    fn meta() -> EventMeta {
        EventMeta {
            ts: chrono::Utc::now(),
            agent_name: "root".into(),
            run_id: "r".into(),
            parent_run_id: None,
            path: String::new(),
        }
    }

    fn delta(t: &str) -> AgentEvent {
        AgentEvent::ModelDelta { meta: meta(), text: t.into() }
    }

    fn finished(t: &str) -> AgentEvent {
        AgentEvent::ModelFinished {
            meta: meta(),
            text: t.into(),
            finish: "stop".into(),
            input_tokens: 100,
            cached_tokens: 90,
            output_tokens: 0,
        }
    }

    fn agents(t: &Transcript) -> Vec<&str> {
        t.rows
            .iter()
            .filter_map(|r| match r {
                Row::Agent(a) => Some(a.text.as_str()),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn streamed_text_is_replaced_by_the_final_text_once() {
        let mut t = Transcript::default();
        t.apply(&AgentEvent::TurnStarted { meta: meta(), turn: 1 }, false);
        t.apply(&delta("hel"), false);
        t.apply(&delta("lo"), false);
        t.apply(
            &AgentEvent::ReasoningDelta { meta: meta(), text: "late think".into() },
            false,
        );
        t.apply(&finished("hello world"), false);
        assert_eq!(agents(&t), ["hello world"]);
        assert_eq!(t.cache, "90/100 (90%)");
        assert!(matches!(&t.rows[1], Row::Think(th) if th.done));
    }

    #[test]
    fn tools_group_until_sealed_and_finish_by_id() {
        let mut t = Transcript::default();
        t.apply(&AgentEvent::TurnStarted { meta: meta(), turn: 1 }, false);
        for id in ["a", "b"] {
            t.apply(
                &AgentEvent::ToolStarted {
                    meta: meta(),
                    call_id: id.into(),
                    name: "run_command".into(),
                    args: json!({"command": id}),
                    kind: "client".into(),
                },
                false,
            );
        }
        assert_eq!(t.rows.len(), 1, "same-turn calls share a group");
        t.apply(
            &AgentEvent::ToolFinished {
                meta: meta(),
                call_id: "a".into(),
                name: "run_command".into(),
                output: r#"{"exit_code":1}"#.into(),
            },
            false,
        );
        let Row::Tools(g) = &t.rows[0] else { panic!() };
        assert!(g.calls[0].done && g.calls[0].failed());
        assert!(!g.calls[1].done);
        t.apply(&AgentEvent::TurnStarted { meta: meta(), turn: 2 }, false);
        t.apply(
            &AgentEvent::ToolStarted {
                meta: meta(),
                call_id: "c".into(),
                name: "now".into(),
                args: json!({}),
                kind: "client".into(),
            },
            false,
        );
        assert_eq!(t.rows.len(), 2, "a new turn starts a new group");
        assert_eq!(t.find_call("c"), Some((1, 0)));
    }

    #[test]
    fn file_changes_and_pictures_attach_to_calls() {
        let mut t = Transcript::default();
        t.tool_started("w", "write_file", &json!({"path": "a.rs"}));
        t.apply(
            &AgentEvent::ToolFinished {
                meta: meta(),
                call_id: "w".into(),
                name: "write_file".into(),
                output: r#"{"path":"a.rs","kind":"create","diff":"+x"}"#.into(),
            },
            false,
        );
        t.apply(
            &AgentEvent::ToolFinished {
                meta: meta(),
                call_id: "i".into(),
                name: "read_image".into(),
                output: r#"{"attach_image":true,"path":"p.png"}"#.into(),
            },
            false,
        );
        assert_eq!(t.file_changes(), vec![(0, 0, "a.rs".to_string(), "create".to_string())]);
        assert!(matches!(t.rows.last(), Some(Row::Picture { path, .. }) if path == "p.png"));
    }

    #[test]
    fn a_result_reported_again_without_pixels_takes_back_its_picture() {
        let mut t = Transcript::default();
        t.tool_started("i", "read_image", &json!({"path": "p.png"}));
        let done = |output: &str| AgentEvent::ToolFinished {
            meta: meta(),
            call_id: "i".into(),
            name: "read_image".into(),
            output: output.into(),
        };
        t.apply(&done(r#"{"attach_image":true,"path":"p.png"}"#), false);
        assert!(matches!(t.rows.last(), Some(Row::Picture { .. })));
        t.push(Row::meta("模型 m 無法看圖"));
        t.apply(&done(r#"{"attach_image":false,"attach_error":"m cannot view images","path":"p.png"}"#), false);
        assert!(!t.rows.iter().any(|r| matches!(r, Row::Picture { .. })), "{:?}", t.rows.len());
        let Row::Tools(g) = &t.rows[0] else { panic!() };
        assert!(g.calls[0].output.contains("attach_error"));
        assert_eq!(t.rows.len(), 2);
    }

    #[test]
    fn idle_after_interrupt_reads_stopped_and_stamps_work() {
        let mut t = Transcript::default();
        t.apply(&AgentEvent::RunStarted { meta: meta(), model: "m".into() }, false);
        t.apply(&finished("ok"), false);
        t.interrupt();
        t.apply(&AgentEvent::AwaitingInput { meta: meta() }, false);
        assert_eq!(t.status, "已停止");
        assert!(!t.running && t.awaiting);
        assert!(t.work_started.is_none());
        t.apply(&AgentEvent::AwaitingInput { meta: meta() }, false);
        assert_eq!(t.status, "待命");
    }

    #[test]
    fn run_finished_keeps_one_copy_of_the_answer() {
        let mut t = Transcript::default();
        t.apply(&finished("answer"), false);
        t.apply(
            &AgentEvent::RunFinished { meta: meta(), reason: "stop".into(), text: "answer".into() },
            false,
        );
        assert_eq!(agents(&t), ["answer"]);
        assert_eq!(t.status, "結束 (stop)");
    }

    #[test]
    fn trimming_bumps_the_epoch() {
        let mut t = Transcript::default().with_cap(20);
        for i in 0..25 {
            t.push(Row::meta(i.to_string()));
        }
        assert!(t.rows.len() <= 20);
        assert_eq!(t.epoch, 1);
    }

    #[test]
    fn server_search_updates_one_call() {
        let mut t = Transcript::default();
        let ev = |kind: &str| AgentEvent::ServerToolObserved {
            meta: meta(),
            kind: kind.into(),
            payload: json!({"action": {"query": "rust"}}),
        };
        t.apply(&ev("response.web_search_call.searching"), false);
        t.apply(&ev("response.web_search_call.completed"), false);
        let Row::Tools(g) = &t.rows[0] else { panic!() };
        assert_eq!(g.calls.len(), 1);
        assert!(g.calls[0].done);
        assert_eq!(g.calls[0].args["action"]["query"], "rust");
    }
}
