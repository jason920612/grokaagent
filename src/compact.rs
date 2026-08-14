//! Context compaction: fold old turns when usage hits 50% of the model window.
//!
//! Recent items stay verbatim so the model still sees what to do *now*.
//! Older items go through xAI `/responses/compact` when that endpoint exists,
//! otherwise an extractive brief that keeps the goal, errors, and paths.

use serde_json::{json, Value};

use crate::error::{Error, Result};
use crate::provider::{CompactRequest, Provider};

/// Fold when rendered context ≥ this fraction of the model window.
pub const TRIGGER_RATIO: f32 = 0.5;
/// How many trailing items stay verbatim. Tool call/output pairs are not split.
pub const DEFAULT_KEEP_RECENT: usize = 6;
/// Do not bother folding unless at least this many items sit in the head.
pub const MIN_HEAD_ITEMS: usize = 4;

pub const COMPACT_MARK: &str = "[grokaagent compact v1]";

const GROK4_WINDOW: u32 = 500_000;
const DEFAULT_WINDOW: u32 = 128_000;
const GOAL_CHARS: usize = 4_000;
const SNIPPET_CHARS: usize = 1_200;
const RENDER_CHARS: usize = 24_000;

#[derive(Debug, Clone, PartialEq)]
pub struct CompactOutcome {
    pub items: Vec<Value>,
    pub dropped: usize,
    pub kept: usize,
    pub method: String,
}

pub fn context_window(model: &str) -> u32 {
    let m = model.to_ascii_lowercase();
    if m.starts_with("grok-4") {
        GROK4_WINDOW
    } else {
        DEFAULT_WINDOW
    }
}

pub fn trigger_tokens(window: u32) -> u32 {
    ((window as f32) * TRIGGER_RATIO) as u32
}

/// Cheap local estimate when the provider has not yet reported `input_tokens`.
pub fn estimate_tokens(instructions: &str, items: &[Value]) -> u32 {
    let body = serde_json::to_string(items).unwrap_or_default();
    let n = instructions.len().saturating_add(body.len()) as u32;
    (n / 4).max(1)
}

pub fn should_compact(used_tokens: u32, window: u32, item_count: usize, keep_recent: usize) -> bool {
    if window == 0 {
        return false;
    }
    let keep = keep_recent.max(1);
    used_tokens >= trigger_tokens(window) && item_count >= keep.saturating_add(MIN_HEAD_ITEMS)
}

pub fn split_head_tail(items: &[Value], keep_recent: usize) -> (&[Value], &[Value]) {
    if items.is_empty() {
        return (items, items);
    }
    let keep = keep_recent.max(1).min(items.len());
    let mut cut = items.len() - keep;
    cut = align_cut(items, cut);
    items.split_at(cut)
}

fn item_type(item: &Value) -> &str {
    item.get("type").and_then(Value::as_str).unwrap_or("")
}

fn call_id(item: &Value) -> Option<&str> {
    item.get("call_id").and_then(Value::as_str)
}

/// Keep function_call with its outputs, and a reasoning item with the call it precedes.
fn align_cut(items: &[Value], mut cut: usize) -> usize {
    loop {
        let before = cut;
        if cut < items.len() && item_type(&items[cut]) == "function_call_output" {
            if let Some(id) = call_id(&items[cut]) {
                if let Some(idx) = items[..cut].iter().rposition(|it| {
                    item_type(it) == "function_call" && call_id(it) == Some(id)
                }) {
                    cut = idx;
                }
            }
        }
        if cut > 0 && item_type(&items[cut - 1]) == "reasoning" {
            cut -= 1;
        }
        if cut == before {
            break;
        }
    }
    cut
}

pub fn clip_goal(goal: &str) -> String {
    if goal.len() <= GOAL_CHARS {
        return goal.to_string();
    }
    let start: String = goal.chars().take(2_000).collect();
    let end: String = goal.chars().rev().take(1_000).collect::<String>().chars().rev().collect();
    format!("{start}\n…\n{end}")
}

pub fn task_anchor(goal: &str) -> Value {
    json!({
        "role": "user",
        "content": format!(
            "{COMPACT_MARK}\n## Current task\n{}\n\nContinue this task. Do not restart. Items after this are original recent turns.",
            clip_goal(goal)
        )
    })
}

fn item_text(item: &Value) -> String {
    if let Some(s) = item.get("content").and_then(Value::as_str) {
        return s.to_string();
    }
    if let Some(arr) = item.get("content").and_then(Value::as_array) {
        let mut out = String::new();
        for part in arr {
            if let Some(t) = part.get("text").and_then(Value::as_str) {
                out.push_str(t);
            }
        }
        if !out.is_empty() {
            return out;
        }
    }
    if let Some(t) = item.get("text").and_then(Value::as_str) {
        return t.to_string();
    }
    if let Some(o) = item.get("output").and_then(Value::as_str) {
        return o.to_string();
    }
    if let Some(arr) = item.get("output").and_then(Value::as_array) {
        let mut out = String::new();
        for part in arr {
            if part.get("type").and_then(Value::as_str) == Some("input_image") {
                continue;
            }
            if let Some(t) = part.get("text").and_then(Value::as_str) {
                out.push_str(t);
            }
        }
        return out;
    }
    String::new()
}

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else {
        let mut t: String = s.chars().take(max).collect();
        t.push('…');
        t
    }
}

fn looks_error(s: &str) -> bool {
    let l = s.to_ascii_lowercase();
    l.contains("\"error\"") || l.contains("error:") || l.contains("failed")
}

/// Extractive brief used when `/responses/compact` is missing or fails.
pub fn extractive_brief(goal: &str, head: &[Value]) -> String {
    let mut users = Vec::new();
    let mut errors = Vec::new();
    let mut tools = Vec::new();
    let mut paths = Vec::new();

    for item in head {
        let typ = item_type(item);
        if typ == "reasoning" || typ == "compaction" {
            continue;
        }
        let text = item_text(item);
        if typ.is_empty() || typ == "message" {
            if item.get("role").and_then(Value::as_str) == Some("user") && !text.is_empty() {
                users.push(truncate(&text, SNIPPET_CHARS));
            }
        }
        if typ == "function_call" {
            let name = item.get("name").and_then(Value::as_str).unwrap_or("tool");
            let args = item
                .get("arguments")
                .map(|a| match a {
                    Value::String(s) => s.clone(),
                    other => other.to_string(),
                })
                .unwrap_or_default();
            tools.push(format!("{name}({})", truncate(&args, 240)));
            collect_paths(&args, &mut paths);
        }
        if typ == "function_call_output" {
            collect_paths(&text, &mut paths);
            if looks_error(&text) {
                errors.push(truncate(&text, SNIPPET_CHARS));
            }
        }
    }

    paths.sort();
    paths.dedup();

    let mut out = String::new();
    out.push_str(&format!("{COMPACT_MARK}\n## Current task\n{}\n", clip_goal(goal)));
    if !users.is_empty() {
        out.push_str("\n## Earlier user turns\n");
        for u in users.iter().take(6) {
            out.push_str("- ");
            out.push_str(u);
            out.push('\n');
        }
    }
    if !tools.is_empty() {
        out.push_str("\n## Tools already used\n");
        for t in tools.iter().take(12) {
            out.push_str("- ");
            out.push_str(t);
            out.push('\n');
        }
    }
    if !errors.is_empty() {
        out.push_str("\n## Errors to remember\n");
        for e in errors.iter().take(8) {
            out.push_str("- ");
            out.push_str(e);
            out.push('\n');
        }
    }
    if !paths.is_empty() {
        out.push_str("\n## Paths\n");
        for p in paths.iter().take(16) {
            out.push_str("- ");
            out.push_str(p);
            out.push('\n');
        }
    }
    out.push_str("\nContinue this task. Do not restart. Items after this are original recent turns.\n");
    truncate(&out, RENDER_CHARS)
}

fn collect_paths(s: &str, out: &mut Vec<String>) {
    for token in s.split(|c: char| c.is_whitespace() || matches!(c, '"' | '\'' | ',' | '{' | '}' | '[' | ']')) {
        let t = token.trim_matches(|c: char| matches!(c, '\\' | '/' | '.'));
        if t.len() < 3 {
            continue;
        }
        if token.contains('/') || token.contains('\\') || token.ends_with(".rs") || token.ends_with(".md")
        {
            if !token.contains("://") {
                out.push(truncate(token, 200));
            }
        }
    }
}

pub fn extractive_items(goal: &str, head: &[Value], tail: &[Value]) -> Vec<Value> {
    let mut items = Vec::new();
    if let Some(blob) = head.iter().rev().find(|i| item_type(i) == "compaction") {
        items.push(blob.clone());
    }
    items.push(json!({
        "role": "user",
        "content": extractive_brief(goal, head)
    }));
    items.extend(tail.iter().cloned());
    items
}

pub fn splice_xai(compact_item: Value, goal: &str, tail: &[Value]) -> Vec<Value> {
    let mut items = vec![compact_item, task_anchor(goal)];
    items.extend(tail.iter().cloned());
    items
}

pub async fn compact_history<P: Provider>(
    provider: &P,
    cache_key: &str,
    goal: &str,
    history: &[Value],
    keep_recent: usize,
) -> Result<CompactOutcome> {
    let (head, tail) = split_head_tail(history, keep_recent);
    if head.is_empty() {
        return Err(Error::Provider("nothing to compact".into()));
    }
    let dropped = head.len();
    let kept = tail.len();
    match provider
        .compact(CompactRequest {
            input: head.to_vec(),
            cache_key: cache_key.to_string(),
        })
        .await
    {
        Ok(c) => Ok(CompactOutcome {
            items: splice_xai(c.item, goal, tail),
            dropped,
            kept,
            method: "xai".into(),
        }),
        Err(_) => Ok(CompactOutcome {
            items: extractive_items(goal, head, tail),
            dropped,
            kept,
            method: "extractive".into(),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::{CompactRequest, CompactResponse, CompleteRequest, CompleteResponse};

    struct FailCompact;
    impl Provider for FailCompact {
        async fn complete(&self, _req: CompleteRequest) -> Result<CompleteResponse> {
            Err(Error::Provider("unused".into()))
        }
        async fn compact(&self, _req: CompactRequest) -> Result<CompactResponse> {
            Err(Error::Provider("no compact endpoint".into()))
        }
    }

    struct OkCompact;
    impl Provider for OkCompact {
        async fn complete(&self, _req: CompleteRequest) -> Result<CompleteResponse> {
            Err(Error::Provider("unused".into()))
        }
        async fn compact(&self, req: CompactRequest) -> Result<CompactResponse> {
            assert!(!req.input.is_empty());
            Ok(CompactResponse {
                item: json!({"type":"compaction","id":"cmp_1","encrypted_content":"blob"}),
                dropped_message_count: req.input.len() as u32,
            })
        }
    }

    fn user(s: &str) -> Value {
        json!({"role":"user","content": s})
    }

    #[test]
    fn grok46_window_is_500k_trigger_is_half() {
        assert_eq!(context_window("grok-4.6"), 500_000);
        assert_eq!(trigger_tokens(500_000), 250_000);
        assert!(should_compact(250_000, 500_000, 20, 6));
        assert!(!should_compact(249_999, 500_000, 20, 6));
        assert!(!should_compact(400_000, 500_000, 5, 6));
    }

    #[test]
    fn split_keeps_recent_and_does_not_orphan_tool_output() {
        let items = vec![
            user("goal"),
            json!({"type":"function_call","call_id":"c1","name":"now","arguments":"{}"}),
            json!({"type":"function_call_output","call_id":"c1","output":"t0"}),
            user("later-a"),
            user("later-b"),
            json!({"type":"function_call","call_id":"c2","name":"now","arguments":"{}"}),
            json!({"type":"function_call_output","call_id":"c2","output":"t1"}),
        ];
        // keep 2 would cut inside c2 pair; align pulls the function_call into the tail.
        let (head, tail) = split_head_tail(&items, 2);
        assert_eq!(tail[0]["type"], "function_call");
        assert_eq!(tail[0]["call_id"], "c2");
        assert!(head.iter().any(|i| i.get("content") == Some(&json!("goal"))));
        assert!(!tail.iter().any(|i| i.get("content") == Some(&json!("goal"))));
    }

    #[test]
    fn extractive_keeps_goal_errors_and_paths() {
        let head = vec![
            user("fix src/agent.rs cache"),
            json!({"type":"function_call","call_id":"c1","name":"read_file","arguments":"{\"path\":\"src/agent.rs\"}"}),
            json!({"type":"function_call_output","call_id":"c1","output":"{\"error\":\"file not found: src/agent.rs\"}"}),
        ];
        let brief = extractive_brief("fix src/agent.rs cache", &head);
        assert!(brief.contains("fix src/agent.rs cache"), "{brief}");
        assert!(brief.contains("file not found"), "{brief}");
        assert!(brief.contains("src/agent.rs"), "{brief}");
        assert!(brief.contains(COMPACT_MARK));
    }

    #[tokio::test]
    async fn xai_compact_then_raw_tail_and_task_anchor() {
        let history = vec![
            user("build the kernel"),
            user("old-1"),
            user("old-2"),
            user("old-3"),
            user("old-4"),
            user("recent-a"),
            user("recent-b"),
        ];
        let out = compact_history(&OkCompact, "k", "build the kernel", &history, 2)
            .await
            .unwrap();
        assert_eq!(out.method, "xai");
        assert_eq!(out.items[0]["type"], "compaction");
        assert!(out.items[1]["content"].as_str().unwrap().contains("build the kernel"));
        assert_eq!(out.items[out.items.len() - 1]["content"], "recent-b");
        assert_eq!(out.items[out.items.len() - 2]["content"], "recent-a");
    }

    #[tokio::test]
    async fn missing_endpoint_falls_back_to_extractive() {
        let history: Vec<Value> = (0..10).map(|i| user(&format!("m{i}"))).collect();
        let out = compact_history(&FailCompact, "k", "ship compact", &history, 3)
            .await
            .unwrap();
        assert_eq!(out.method, "extractive");
        assert!(out.items[0]["content"].as_str().unwrap().contains("ship compact"));
        assert_eq!(out.items.last().unwrap()["content"], "m9");
    }
}
