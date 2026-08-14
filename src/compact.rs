//! Context compaction: fold old turns when usage hits 50% of the model window.
//!
//! The working model writes long-term memory — it lived the turns, so it
//! judges what was direction-changing, shocking, or load-bearing. The gist is
//! sparse on purpose: gaps are reconstructed with reasoning and common sense.
//! Only the verbatim tail (short-term memory) stays complete.
//!
//! If that write fails, a local extractive gist is the fallback. The encrypted
//! xAI `/responses/compact` blob is not used.

use serde_json::{json, Value};

use crate::error::{Error, Result};

/// Fold when rendered context ≥ this fraction of the model window.
pub const TRIGGER_RATIO: f32 = 0.5;
/// How many trailing items stay verbatim. Tool call/output pairs are not split.
/// 6 was about one tool burst, so the model lost the live plan after a fold.
pub const DEFAULT_KEEP_RECENT: usize = 16;
/// Walk further back so the tail includes a real user turn (working memory).
const MAX_TAIL_EXTRA: usize = 24;
/// Do not bother folding unless at least this many items sit in the head.
pub const MIN_HEAD_ITEMS: usize = 4;

pub const COMPACT_MARK: &str = "[grokaagent compact v1]";
pub const MEMORY_FOLD_MARK: &str = "[grokaagent memory-fold]";

const GROK4_WINDOW: u32 = 500_000;
const DEFAULT_WINDOW: u32 = 128_000;
const GOAL_CHARS: usize = 800;
const SNIPPET_CHARS: usize = 280;
const ASSISTANT_CHARS: usize = 400;
const ERROR_CHARS: usize = 200;
const PATH_CHARS: usize = 80;
const RENDER_CHARS: usize = 6_000;
const MIN_LIVED_CHARS: usize = 40;
const MAX_LATER: usize = 3;
const MAX_ERRORS: usize = 2;
const MAX_PATHS: usize = 5;

#[derive(Debug, Clone, PartialEq)]
pub struct CompactOutcome {
    pub items: Vec<Value>,
    pub dropped: usize,
    pub kept: usize,
    pub method: String,
}

#[derive(Default)]
struct Harvest {
    original: String,
    later: Vec<String>,
    where_were: String,
    errors: Vec<String>,
    paths: Vec<String>,
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
    let max_tail = keep.saturating_add(MAX_TAIL_EXTRA).min(items.len());
    let min_cut = items.len().saturating_sub(max_tail).max(1.min(cut));
    while cut > min_cut && !tail_has_real_user(&items[cut..]) {
        cut -= 1;
        cut = align_cut(items, cut);
        if cut == 0 {
            break;
        }
    }
    items.split_at(cut)
}

fn tail_has_real_user(tail: &[Value]) -> bool {
    tail.iter().any(|item| {
        is_user(item) && {
            let text = item_text(item);
            !text.is_empty() && !is_noise_user(&text)
        }
    })
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
    let start: String = goal.chars().take(500).collect();
    let end: String = goal.chars().rev().take(200).collect::<String>().chars().rev().collect();
    format!("{start}\n…\n{end}")
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
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let mut t: String = s.chars().take(max).collect();
        t.push('…');
        t
    }
}

/// Keep the start (gist) and the end (what was happening). Gaps are left to inference.
fn gist(s: &str, max: usize) -> String {
    let n = s.chars().count();
    if n <= max {
        return s.to_string();
    }
    let chars: Vec<char> = s.chars().collect();
    let head_n = (max / 3).max(1);
    let tail_n = max.saturating_sub(head_n + 1).max(1);
    let head: String = chars[..head_n.min(chars.len())].iter().collect();
    let tail: String = chars[chars.len().saturating_sub(tail_n)..].iter().collect();
    format!("{head}…{tail}")
}

fn looks_error(s: &str) -> bool {
    let l = s.to_ascii_lowercase();
    l.contains("\"error\"") || l.contains("error:") || l.contains("failed")
}

fn is_noise_user(text: &str) -> bool {
    let t = text.trim_start();
    t.starts_with(COMPACT_MARK)
        || t.starts_with(crate::background::EXIT_NOTICE_PREFIX)
        || t.starts_with(crate::background::CLOSED_NOTICE_PREFIX)
}

fn is_user(item: &Value) -> bool {
    item.get("role").and_then(Value::as_str) == Some("user")
}

fn is_assistant(item: &Value) -> bool {
    item.get("role").and_then(Value::as_str) == Some("assistant")
}

fn push_unique(out: &mut Vec<String>, s: String) {
    if s.is_empty() {
        return;
    }
    out.retain(|x| x != &s);
    out.push(s);
}

fn take_last(items: &[String], n: usize) -> Vec<String> {
    if items.len() <= n {
        items.to_vec()
    } else {
        items[items.len() - n..].to_vec()
    }
}

fn harvest_brief(text: &str, into: &mut Harvest) {
    if !text.contains(COMPACT_MARK) {
        return;
    }
    let mut name = String::new();
    let mut body = String::new();
    let flush = |name: &str, body: &str, into: &mut Harvest| {
        let body = body.trim();
        if name.is_empty() || body.is_empty() {
            return;
        }
        match name {
            "Original request" => into.original = body.to_string(),
            "Later user direction" => {
                for line in body.lines() {
                    let line = line.trim();
                    if let Some(rest) = line.strip_prefix("- ") {
                        push_unique(&mut into.later, rest.to_string());
                    }
                }
            }
            "Where you were before this fold" => into.where_were = body.to_string(),
            "Errors to remember" => {
                for line in body.lines() {
                    let line = line.trim();
                    if let Some(rest) = line.strip_prefix("- ") {
                        push_unique(&mut into.errors, rest.to_string());
                    }
                }
            }
            "Paths" => {
                for line in body.lines() {
                    let line = line.trim();
                    if let Some(rest) = line.strip_prefix("- ") {
                        push_unique(&mut into.paths, rest.to_string());
                    }
                }
            }
            _ => {}
        }
    };
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("## ") {
            flush(&name, &body, into);
            name = rest.trim().to_string();
            body.clear();
        } else if !name.is_empty() {
            body.push_str(line);
            body.push('\n');
        }
    }
    flush(&name, &body, into);
}

/// Local memory fold of `head`. Also reconsolidates any previous fold sitting in `head`.
pub fn extractive_brief(goal: &str, head: &[Value]) -> String {
    let mut harvested = Harvest::default();
    let mut users = Vec::new();
    let mut assistants = Vec::new();
    let mut errors = Vec::new();
    let mut paths = Vec::new();

    for item in head {
        let typ = item_type(item);
        if typ == "reasoning" || typ == "compaction" {
            continue;
        }
        let text = item_text(item);
        if is_user(item) && text.contains(COMPACT_MARK) {
            harvest_brief(&text, &mut harvested);
            continue;
        }
        if is_user(item) && !text.is_empty() && !is_noise_user(&text) {
            users.push(truncate(&text, SNIPPET_CHARS));
        }
        if is_assistant(item) && !text.is_empty() {
            assistants.push(text.clone());
        }
        if typ == "function_call" {
            let args = item
                .get("arguments")
                .map(|a| match a {
                    Value::String(s) => s.clone(),
                    other => other.to_string(),
                })
                .unwrap_or_default();
            collect_paths(&args, &mut paths);
        }
        if typ == "function_call_output" {
            collect_paths(&text, &mut paths);
            if looks_error(&text) {
                errors.push(truncate(&text, ERROR_CHARS));
            }
        }
    }

    let original = {
        let g = goal.trim();
        if !g.is_empty() && !is_noise_user(g) {
            clip_goal(g)
        } else if !harvested.original.is_empty() {
            gist(&harvested.original, GOAL_CHARS)
        } else {
            users.first().cloned().unwrap_or_else(|| clip_goal(goal))
        }
    };

    let mut later = harvested.later;
    for u in &users {
        if u.as_str() != original.as_str() {
            push_unique(&mut later, truncate(u, SNIPPET_CHARS));
        }
    }
    let later = take_last(&later, MAX_LATER);

    let where_were = assistants
        .last()
        .cloned()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or(harvested.where_were);
    let where_were = gist(&where_were, ASSISTANT_CHARS);

    let mut all_errors = harvested.errors;
    for e in errors {
        push_unique(&mut all_errors, truncate(&e, ERROR_CHARS));
    }
    let recent_errors = take_last(&all_errors, MAX_ERRORS);

    let mut all_paths = harvested.paths;
    for p in paths {
        push_unique(&mut all_paths, truncate(&p, PATH_CHARS));
    }
    let all_paths = take_last(&all_paths, MAX_PATHS);

    let mut out = String::new();
    out.push_str(COMPACT_MARK);
    out.push_str(
        "\nThis is sparse long-term memory of older turns — not a transcript and not a new user request.\nOnly key details are stored. Fill gaps with ordinary reasoning and common sense; pick the most plausible continuation, not a perfect replay. Do not restart. Do not invent a different mission.\n",
    );
    out.push_str(&format!("\n## Original request\n{original}\n"));
    if !later.is_empty() {
        out.push_str("\n## Later user direction\n");
        for u in &later {
            out.push_str("- ");
            out.push_str(u);
            out.push('\n');
        }
    }
    if !where_were.is_empty() {
        out.push_str("\n## Where you were before this fold\n");
        out.push_str(&where_were);
        out.push('\n');
    }
    if !recent_errors.is_empty() {
        out.push_str("\n## Errors to remember\n");
        for e in &recent_errors {
            out.push_str("- ");
            out.push_str(e);
            out.push('\n');
        }
    }
    if !all_paths.is_empty() {
        out.push_str("\n## Paths\n");
        for p in &all_paths {
            out.push_str("- ");
            out.push_str(p);
            out.push('\n');
        }
    }
    out.push_str(
        "\nThe items after this block are short-term memory. They are complete and authoritative for what you were doing just now. Continue from there.\n",
    );
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
                out.push(truncate(token, PATH_CHARS));
            }
        }
    }
}

pub fn extractive_items(goal: &str, head: &[Value], tail: &[Value]) -> Vec<Value> {
    splice_brief(&extractive_brief(goal, head), tail)
}

fn splice_brief(brief: &str, tail: &[Value]) -> Vec<Value> {
    let mut items = vec![json!({
        "role": "user",
        "content": brief
    })];
    items.extend(tail.iter().cloned());
    items
}

/// Ask the working model to write long-term memory. Not a new user task.
pub fn memory_ask(goal: &str) -> String {
    let original = clip_goal(goal);
    format!(
        "{MEMORY_FOLD_MARK}\nThis is not a new user task. Do not call tools. Do not continue the work in this message.\n\nYou lived the older turns. Write sparse long-term memory for them. The most recent turns will stay verbatim as short-term memory — do not try to replace those.\n\nKeep only what YOU judge is:\n1. The original request (must remain): {original}\n2. Direction-changing or shocking events — a user constraint that rewrote the plan, a surprise, a failure that forced a new approach.\n3. Load-bearing facts that construct or support this task — decisions, invariants, paths, APIs you would need in order to reconstruct the work with reasoning. Not a transcript of every tool call.\n\nOmit routine reads, successful boilerplate, and anything the next you can guess from (1)–(3) plus common sense.\n\nWrite markdown with these headings only:\n## Original request\n## Direction changes\n## Load-bearing facts\n## Where you were\n\nIf a heading has nothing, write \"none\". Hard cap: 80 lines. No tool-call dump.\n"
    )
}

fn strip_fences(s: &str) -> String {
    let t = s.trim();
    let Some(rest) = t.strip_prefix("```") else {
        return t.to_string();
    };
    let rest = rest
        .strip_prefix("markdown")
        .or_else(|| rest.strip_prefix("md"))
        .unwrap_or(rest);
    let rest = rest.trim_start_matches('\r').trim_start_matches('\n');
    let rest = rest.strip_suffix("```").unwrap_or(rest);
    rest.trim().to_string()
}

pub fn lived_brief_usable(text: &str) -> bool {
    let t = strip_fences(text);
    t.chars().count() >= MIN_LIVED_CHARS && !t.trim_start().starts_with(MEMORY_FOLD_MARK)
}

/// Wrap the working model's own gist so later turns still see orientation rules.
pub fn wrap_lived_brief(goal: &str, model_text: &str) -> String {
    let body = strip_fences(model_text);
    let original = clip_goal(goal);
    let mut out = String::new();
    out.push_str(COMPACT_MARK);
    out.push_str(
        "\nThis is sparse long-term memory written by you after living the older turns — not a transcript and not a new user request.\nOnly key details are stored. Fill gaps with ordinary reasoning and common sense; pick the most plausible continuation, not a perfect replay. Do not restart. Do not invent a different mission.\n",
    );
    let has_original = body.to_ascii_lowercase().contains("original request")
        || (!original.is_empty() && body.contains(original.trim()));
    if !has_original {
        out.push_str(&format!("\n## Original request\n{original}\n"));
    }
    let body = if let Some(rest) = body.split(COMPACT_MARK).last() {
        rest.trim()
    } else {
        body.trim()
    };
    out.push('\n');
    out.push_str(body);
    out.push('\n');
    if !out.contains("short-term memory") {
        out.push_str(
            "\nThe items after this block are short-term memory. They are complete and authoritative for what you were doing just now. Continue from there.\n",
        );
    }
    truncate(&out, RENDER_CHARS)
}

pub fn compact_history(
    goal: &str,
    history: &[Value],
    keep_recent: usize,
    lived: Option<&str>,
) -> Result<CompactOutcome> {
    let (head, tail) = split_head_tail(history, keep_recent);
    if head.is_empty() {
        return Err(Error::Provider("nothing to compact".into()));
    }
    let (items, method) = match lived {
        Some(text) if lived_brief_usable(text) => (
            splice_brief(&wrap_lived_brief(goal, text), tail),
            "lived",
        ),
        _ => (extractive_items(goal, head, tail), "local"),
    };
    Ok(CompactOutcome {
        items,
        dropped: head.len(),
        kept: tail.len(),
        method: method.into(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn user(s: &str) -> Value {
        json!({"role":"user","content": s})
    }

    fn assistant(s: &str) -> Value {
        json!({"role":"assistant","content": s})
    }

    fn tool(id: &str, name: &str, args: &str) -> Value {
        json!({"type":"function_call","call_id":id,"name":name,"arguments":args})
    }

    fn tool_out(id: &str, output: &str) -> Value {
        json!({"type":"function_call_output","call_id":id,"output":output})
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
            tool("c1", "now", "{}"),
            tool_out("c1", "t0"),
            user("later-a"),
            user("later-b"),
            tool("c2", "now", "{}"),
            tool_out("c2", "t1"),
        ];
        let (head, tail) = split_head_tail(&items, 2);
        assert!(
            tail.iter().any(|i| i.get("call_id") == Some(&json!("c2"))),
            "{tail:?}"
        );
        assert!(
            tail.iter().any(|i| i.get("content") == Some(&json!("later-b"))),
            "working memory must include the last real user, got {tail:?}"
        );
        assert!(head.iter().any(|i| i.get("content") == Some(&json!("goal"))));
        assert!(!tail.iter().any(|i| i.get("content") == Some(&json!("goal"))));
    }

    #[test]
    fn extractive_keeps_goal_errors_and_paths() {
        let head = vec![
            user("fix src/agent.rs cache"),
            tool("c1", "read_file", "{\"path\":\"src/agent.rs\"}"),
            tool_out("c1", "{\"error\":\"file not found: src/agent.rs\"}"),
        ];
        let brief = extractive_brief("fix src/agent.rs cache", &head);
        assert!(brief.contains("fix src/agent.rs cache"), "{brief}");
        assert!(brief.contains("file not found"), "{brief}");
        assert!(brief.contains("src/agent.rs"), "{brief}");
        assert!(brief.contains(COMPACT_MARK));
        assert!(brief.contains("not a new user request"), "{brief}");
        assert!(brief.contains("not a transcript"), "{brief}");
        assert!(brief.contains("most plausible"), "{brief}");
        assert!(!brief.contains("## Tools already used"), "{brief}");
        assert!(!brief.contains("\"type\":\"compaction\""), "{brief}");
    }

    #[test]
    fn extractive_keeps_later_direction_and_last_plan() {
        let head = vec![
            user("build a cli"),
            assistant("I'll scaffold the crate first."),
            user("use sqlite, not json files"),
            assistant("Switching the store to sqlite next. Then wire the query path."),
            tool("c1", "write_file", "{\"path\":\"src/db.rs\"}"),
            tool_out("c1", "ok"),
        ];
        let brief = extractive_brief("build a cli", &head);
        assert!(brief.contains("## Original request"), "{brief}");
        assert!(brief.contains("build a cli"), "{brief}");
        assert!(brief.contains("use sqlite, not json files"), "{brief}");
        assert!(
            brief.contains("Switching the store to sqlite next"),
            "last plan must survive the fold: {brief}"
        );
        assert!(!brief.contains("I'll scaffold the crate first."), "{brief}");
    }

    #[test]
    fn extractive_stores_landmarks_not_a_tool_transcript() {
        let mut head = vec![user("ship it")];
        for i in 0..13 {
            head.push(tool(
                &format!("c{i}"),
                "read_file",
                &format!("{{\"path\":\"old-{i}.rs\"}}"),
            ));
            head.push(tool_out(&format!("c{i}"), "ok"));
        }
        let brief = extractive_brief("ship it", &head);
        assert!(brief.contains("old-12.rs"), "{brief}");
        assert!(
            !brief.contains("read_file("),
            "long-term memory must not dump tool calls: {brief}"
        );
        assert!(
            !brief.contains("old-0.rs"),
            "early routine landmarks should fade: {brief}"
        );
        assert!(brief.len() < 2_000, "gist should stay small, got {}", brief.len());
    }

    #[test]
    fn extractive_skips_system_notices_as_user_direction() {
        let head = vec![
            user("real goal"),
            user("[background exited]\nname: dev"),
            user("[backgrounds closed]\nkilled"),
            assistant("still on real goal"),
        ];
        let brief = extractive_brief("real goal", &head);
        assert!(brief.contains("real goal"), "{brief}");
        assert!(!brief.contains("[background exited]"), "{brief}");
        assert!(!brief.contains("[backgrounds closed]"), "{brief}");
        assert!(brief.contains("still on real goal"), "{brief}");
    }

    #[test]
    fn second_fold_reconsolidates_earlier_user_direction() {
        let first_head = vec![
            user("build a cli"),
            user("use sqlite, not json files"),
            assistant("I'll add sqlite next."),
        ];
        let first = extractive_brief("build a cli", &first_head);
        let second_head = vec![
            user(&first),
            user("also add tests"),
            assistant("Writing the sqlite tests now."),
            tool("c9", "write_file", "{\"path\":\"src/db.rs\"}"),
        ];
        let second = extractive_brief("build a cli", &second_head);
        assert!(second.contains("build a cli"), "{second}");
        assert!(
            second.contains("use sqlite, not json files"),
            "reconsolidation must keep the earlier constraint: {second}"
        );
        assert!(second.contains("also add tests"), "{second}");
        assert!(second.contains("Writing the sqlite tests now."), "{second}");
    }

    #[test]
    fn compact_history_is_local_readable_brief_plus_raw_tail() {
        let history = vec![
            user("build the kernel"),
            user("old-1"),
            user("old-2"),
            user("old-3"),
            user("old-4"),
            assistant("porting the scheduler next"),
            user("recent-a"),
            user("recent-b"),
        ];
        let out = compact_history("build the kernel", &history, 2, None).unwrap();
        assert_eq!(out.method, "local");
        assert_ne!(out.items[0].get("type").and_then(Value::as_str), Some("compaction"));
        let brief = out.items[0]["content"].as_str().unwrap();
        assert!(brief.contains("build the kernel"), "{brief}");
        assert!(brief.contains("porting the scheduler next"), "{brief}");
        assert!(
            brief.contains("old-4"),
            "last later constraints are key details: {brief}"
        );
        assert!(
            !brief.contains("old-1"),
            "older later turns should fade into reconstruction: {brief}"
        );
        assert_eq!(out.items[out.items.len() - 1]["content"], "recent-b");
        assert!(
            out.items
                .iter()
                .any(|i| i.get("content") == Some(&json!("recent-a"))),
            "{:?}",
            out.items
        );
    }

    #[test]
    fn missing_goal_falls_back_to_first_real_user() {
        let history: Vec<Value> = (0..10).map(|i| user(&format!("m{i}"))).collect();
        let out = compact_history("ship compact", &history, 3, None).unwrap();
        assert_eq!(out.method, "local");
        assert!(out.items[0]["content"].as_str().unwrap().contains("ship compact"));
        assert_eq!(out.items.last().unwrap()["content"], "m9");
    }

    #[test]
    fn memory_ask_is_not_a_new_task_and_keeps_original() {
        let ask = memory_ask("ship the kernel");
        assert!(ask.contains(MEMORY_FOLD_MARK));
        assert!(ask.contains("not a new user task"), "{ask}");
        assert!(ask.contains("Do not call tools"), "{ask}");
        assert!(ask.contains("ship the kernel"), "{ask}");
        assert!(ask.contains("Direction changes"), "{ask}");
        assert!(ask.contains("Load-bearing facts"), "{ask}");
    }

    #[test]
    fn wrap_lived_brief_injects_original_when_the_model_omits_it() {
        let brief = wrap_lived_brief(
            "build a cli",
            "## Direction changes\nuse sqlite, not json\n## Where you were\nwriting db.rs\n",
        );
        assert!(brief.contains(COMPACT_MARK), "{brief}");
        assert!(brief.contains("build a cli"), "{brief}");
        assert!(brief.contains("use sqlite, not json"), "{brief}");
        assert!(brief.contains("not a new user request"), "{brief}");
        assert!(brief.contains("short-term memory"), "{brief}");
    }

    #[test]
    fn compact_history_prefers_the_lived_gist_over_extractive() {
        let history = vec![
            user("build a cli"),
            user("old-chatter"),
            assistant("I'll add sqlite next."),
            user("recent-a"),
            user("recent-b"),
        ];
        let lived = "## Original request\nbuild a cli\n## Direction changes\nuse sqlite, not json files\n## Load-bearing facts\nsrc/db.rs is the store\n## Where you were\nwriting the sqlite tests\n";
        let out = compact_history("build a cli", &history, 2, Some(lived)).unwrap();
        assert_eq!(out.method, "lived");
        let brief = out.items[0]["content"].as_str().unwrap();
        assert!(brief.contains("use sqlite, not json files"), "{brief}");
        assert!(brief.contains("writing the sqlite tests"), "{brief}");
        assert!(
            !brief.contains("old-chatter"),
            "the model's judgment replaces extractive chatter: {brief}"
        );
        assert_eq!(out.items.last().unwrap()["content"], "recent-b");
    }

    #[test]
    fn unusable_lived_text_falls_back_to_extractive() {
        let history: Vec<Value> = (0..8).map(|i| user(&format!("m{i}"))).collect();
        let out = compact_history("ship it", &history, 2, Some("ok")).unwrap();
        assert_eq!(out.method, "local");
        assert!(out.items[0]["content"].as_str().unwrap().contains("ship it"));
    }

    #[test]
    fn lived_brief_usable_rejects_echoed_ask() {
        assert!(!lived_brief_usable("ok"));
        assert!(!lived_brief_usable(&memory_ask("goal")));
        assert!(lived_brief_usable(
            "## Original request\nbuild a cli\n## Where you were\nwriting tests\n"
        ));
    }
}
