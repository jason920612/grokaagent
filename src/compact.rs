//! Context compaction: fold old turns when usage hits 50% of the model window.
//!
//! The working model writes long-term memory — it lived the turns, so it
//! judges what was direction-changing, shocking, or load-bearing. The gist is
//! sparse on purpose: gaps are reconstructed with reasoning and common sense.
//! Only the verbatim tail (short-term memory) stays complete.
//!
//! There is no extractive fallback. The lived write is retried until it
//! succeeds, or the user interrupts. The encrypted xAI
//! `/responses/compact` blob is not used.

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
pub const DEFAULT_WINDOW: u32 = 128_000;
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
const MAX_OPEN_TOOLS: usize = 6;

#[derive(Debug, Clone, PartialEq)]
pub struct CompactOutcome {
    pub items: Vec<Value>,
    pub dropped: usize,
    pub kept: usize,
    pub method: String,
}

#[derive(Default, Clone)]
struct Harvest {
    original: String,
    direction: String,
    facts: String,
    where_were: String,
    open_work: String,
}

pub fn context_window(model: &str) -> u32 {
    let m = model.to_ascii_lowercase();
    if m.starts_with("grok-4") {
        GROK4_WINDOW
    } else {
        DEFAULT_WINDOW
    }
}

/// `262K` → 262×1024, `128000` stays as-is.
pub fn parse_window(s: &str) -> Option<u32> {
    let t = s.trim().to_ascii_lowercase();
    if t.is_empty() {
        return None;
    }
    if let Some(n) = t.strip_suffix('k') {
        return n.trim().parse::<u32>().ok()?.checked_mul(1024);
    }
    if let Some(n) = t.strip_suffix('m') {
        return n.trim().parse::<u32>().ok()?.checked_mul(1024 * 1024);
    }
    t.parse().ok()
}

pub fn format_window(n: u32) -> String {
    if n == 0 {
        return String::new();
    }
    if n % (1024 * 1024) == 0 {
        format!("{}M", n / (1024 * 1024))
    } else if n % 1024 == 0 {
        format!("{}K", n / 1024)
    } else {
        n.to_string()
    }
}

pub fn trigger_tokens(window: u32) -> u32 {
    ((window as f32) * TRIGGER_RATIO) as u32
}

/// Vision tiles are ~1k tokens, not the base64 byte length / 4.
const VISION_ESTIMATE_TOKENS: u32 = 1024;

/// Cheap local estimate when the provider has not yet reported `input_tokens`.
/// Image data URIs are counted as vision tiles, not as text.
pub fn estimate_tokens(instructions: &str, items: &[Value]) -> u32 {
    let mut text = instructions.len() as u32;
    let mut images = 0u32;
    for item in items {
        count_estimate(item, &mut text, &mut images);
    }
    (text / 4)
        .saturating_add(images.saturating_mul(VISION_ESTIMATE_TOKENS))
        .max(1)
}

fn count_estimate(v: &Value, text: &mut u32, images: &mut u32) {
    match v {
        Value::String(s) => {
            if s.starts_with("data:image/") {
                *images = images.saturating_add(1);
            } else {
                *text = text.saturating_add(s.len() as u32);
            }
        }
        Value::Array(arr) => {
            for x in arr {
                count_estimate(x, text, images);
            }
        }
        Value::Object(map) => {
            if map.get("type").and_then(Value::as_str) == Some("input_image") {
                *images = images.saturating_add(1);
                return;
            }
            for (k, x) in map {
                *text = text.saturating_add(k.len() as u32);
                count_estimate(x, text, images);
            }
        }
        Value::Number(n) => *text = text.saturating_add(n.to_string().len() as u32),
        Value::Bool(_) | Value::Null => *text = text.saturating_add(4),
    }
}

pub fn should_compact(used_tokens: u32, window: u32, items: &[Value], keep_recent: usize) -> bool {
    if window == 0 {
        return false;
    }
    used_tokens >= trigger_tokens(window)
        && split_head_tail(items, keep_recent).0.len() >= MIN_HEAD_ITEMS
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

/// Close unpaired `function_call` items so a new user message is legal.
/// xAI rejects: "cannot follow a function_call with a new user message".
pub fn close_fold_head(head: &[Value]) -> Vec<Value> {
    let mut out = head.to_vec();
    let mut open: Vec<String> = Vec::new();
    for item in &out {
        match item_type(item) {
            "function_call" => {
                if let Some(id) = call_id(item) {
                    open.push(id.to_string());
                }
            }
            "function_call_output" => {
                if let Some(id) = call_id(item) {
                    if let Some(pos) = open.iter().rposition(|x| x == id) {
                        open.remove(pos);
                    }
                }
            }
            _ => {}
        }
    }
    for id in open {
        out.push(json!({
            "type": "function_call_output",
            "call_id": id,
            "output": "(folded into long-term memory)",
        }));
    }
    while out.last().is_some_and(|i| item_type(i) == "function_call") {
        out.pop();
    }
    out
}

/// Head plus the memory-ask user turn, legal as a Responses `input`.
pub fn fold_input(head: &[Value], goal: &str) -> Vec<Value> {
    let mut input = close_fold_head(head);
    input.push(json!({
        "role": "user",
        "content": memory_ask(goal, previous_compact_text(head).as_deref()),
    }));
    input
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

fn call_args(item: &Value) -> String {
    item.get("arguments")
        .map(|a| match a {
            Value::String(s) => s.clone(),
            other => other.to_string(),
        })
        .unwrap_or_default()
}

fn call_label(item: &Value) -> String {
    let name = item.get("name").and_then(Value::as_str).unwrap_or("tool");
    let mut paths = Vec::new();
    collect_paths(&call_args(item), &mut paths);
    match paths.first() {
        Some(p) => format!("{name} {p}"),
        None => name.to_string(),
    }
}

/// Live thread sitting in `tail` (fallback: `head`). Last plan + last tools.
/// A forgetful lived gist must not replace this with "shipped, add a new layer".
fn open_work_from(head: &[Value], tail: &[Value]) -> String {
    let from_tail = collect_open_work(tail);
    if !section_empty(&from_tail) {
        from_tail
    } else {
        collect_open_work(head)
    }
}

fn collect_open_work(items: &[Value]) -> String {
    let mut last_asst = String::new();
    let mut tools: Vec<String> = Vec::new();
    for item in items {
        if is_assistant(item) {
            let t = item_text(item);
            if !t.is_empty() && !t.contains(COMPACT_MARK) && !t.contains(MEMORY_FOLD_MARK) {
                last_asst = gist(&t, ASSISTANT_CHARS);
            }
        }
        if item_type(item) == "function_call" {
            tools.push(call_label(item));
        }
    }
    let tools = take_last(&tools, MAX_OPEN_TOOLS);
    let mut out = String::new();
    if !section_empty(&last_asst) {
        out = last_asst;
    }
    if !tools.is_empty() {
        let line = format!("Last tools: {}", tools.join("; "));
        if out.is_empty() {
            out = line;
        } else {
            out.push('\n');
            out.push_str(&line);
        }
    }
    out
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
        || t.starts_with(crate::timer::FIRED_NOTICE_PREFIX)
}

fn is_user(item: &Value) -> bool {
    item.get("role").and_then(Value::as_str) == Some("user")
}

fn is_assistant(item: &Value) -> bool {
    item.get("role").and_then(Value::as_str) == Some("assistant")
}

fn take_last(items: &[String], n: usize) -> Vec<String> {
    if items.len() <= n {
        items.to_vec()
    } else {
        items[items.len() - n..].to_vec()
    }
}

fn section_empty(s: &str) -> bool {
    let t = s.trim();
    t.is_empty() || t.eq_ignore_ascii_case("none") || t.eq_ignore_ascii_case("n/a")
}

fn set_if_present(dst: &mut String, body: &str) {
    if !section_empty(body) {
        *dst = body.trim().to_string();
    }
}

fn append_block(dst: &mut String, add: &str) {
    let add = add.trim();
    if section_empty(add) {
        return;
    }
    if dst.is_empty() {
        *dst = add.to_string();
        return;
    }
    if dst.contains(add) {
        return;
    }
    dst.push('\n');
    dst.push_str(add);
}

/// Keep previous key lines the new gist forgot to copy.
fn retain_unmentioned(prev: &str, new: &str) -> String {
    if section_empty(new) {
        return prev.trim().to_string();
    }
    if section_empty(prev) {
        return new.trim().to_string();
    }
    let mut out = new.trim().to_string();
    for chunk in prev.lines() {
        let chunk = chunk.trim().trim_start_matches("- ").trim();
        if chunk.is_empty() || section_empty(chunk) {
            continue;
        }
        if !out.contains(chunk) {
            out.push_str("\n- ");
            out.push_str(chunk);
        }
    }
    out
}

fn harvest_brief(text: &str, into: &mut Harvest) {
    if !text.contains(COMPACT_MARK) {
        return;
    }
    let mut name = String::new();
    let mut body = String::new();
    let flush = |name: &str, body: &str, into: &mut Harvest| {
        let body = body.trim();
        if name.is_empty() || section_empty(body) {
            return;
        }
        match name {
            "Original request" => set_if_present(&mut into.original, body),
            "Direction changes" | "Later user direction" => append_block(&mut into.direction, body),
            "Load-bearing facts" => append_block(&mut into.facts, body),
            "Where you were" | "Where you were before this fold" => {
                set_if_present(&mut into.where_were, body)
            }
            "Open work" => set_if_present(&mut into.open_work, body),
            "Errors to remember" | "Paths" => append_block(&mut into.facts, body),
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

fn harvest_from_head(head: &[Value]) -> Harvest {
    let mut harvested = Harvest::default();
    for item in head {
        let text = item_text(item);
        if is_user(item) && text.contains(COMPACT_MARK) {
            harvest_brief(&text, &mut harvested);
        }
    }
    harvested
}

fn merge_harvest(goal: &str, prev: &Harvest, new: &Harvest) -> Harvest {
    let original = if !section_empty(&new.original) {
        new.original.clone()
    } else if !section_empty(&prev.original) {
        prev.original.clone()
    } else {
        clip_goal(goal)
    };
    Harvest {
        original,
        direction: retain_unmentioned(&prev.direction, &new.direction),
        facts: retain_unmentioned(&prev.facts, &new.facts),
        where_were: if !section_empty(&new.where_were) {
            new.where_were.clone()
        } else {
            prev.where_were.clone()
        },
        open_work: if !section_empty(&new.open_work) {
            new.open_work.clone()
        } else {
            prev.open_work.clone()
        },
    }
}

fn emit_section(title: &str, body: &str) -> String {
    if section_empty(body) {
        String::new()
    } else {
        format!("\n## {title}\n{}\n", body.trim())
    }
}

fn emit_brief(h: &Harvest, lived: bool) -> String {
    let intro = if lived {
        "This is sparse long-term memory written by you after living the older turns — not a transcript and not a new user request.\nOnly key details are stored. Fill gaps with ordinary reasoning and common sense. Do not restart. Do not invent a different mission. Continue the open thread; do not start a new layer while Open work is unfinished.\n"
    } else {
        "This is sparse long-term memory of older turns — not a transcript and not a new user request.\nOnly key details are stored. Fill gaps with ordinary reasoning and common sense. Do not restart. Do not invent a different mission. Continue the open thread; do not start a new layer while Open work is unfinished.\n"
    };
    let footer = "Open work and the verbatim items after this block are the live thread. They outrank a polished status summary. Finish that thread before starting a new layer or system.\n";
    let orig = if section_empty(&h.original) {
        String::new()
    } else {
        h.original.trim().to_string()
    };
    let where_were = h.where_were.trim().to_string();
    let mut open_work = h.open_work.trim().to_string();
    let mut direction = h.direction.clone();
    let mut facts = h.facts.clone();
    for _ in 0..8 {
        let out = format!(
            "{COMPACT_MARK}\n{intro}\n## Original request\n{orig}\n{}{}{}{}\n{footer}",
            emit_section("Direction changes", &direction),
            emit_section("Load-bearing facts", &facts),
            emit_section("Where you were", &where_were),
            emit_section("Open work", &open_work),
        );
        if out.chars().count() <= RENDER_CHARS {
            return out;
        }
        if facts.chars().count() > 120 {
            facts = gist(&facts, facts.chars().count().saturating_mul(2) / 3);
            continue;
        }
        if direction.chars().count() > 120 {
            direction = gist(&direction, direction.chars().count().saturating_mul(2) / 3);
            continue;
        }
        if open_work.chars().count() > 120 {
            open_work = gist(&open_work, open_work.chars().count().saturating_mul(2) / 3);
            continue;
        }
        return out;
    }
    format!(
        "{COMPACT_MARK}\n{intro}\n## Original request\n{orig}\n{}{}\n{footer}",
        emit_section("Where you were", &where_were),
        emit_section("Open work", &open_work),
    )
}

/// Latest compact block in `items`, if any. Used so a later fold cannot lose it.
pub fn previous_compact_text(items: &[Value]) -> Option<String> {
    items.iter().rev().find_map(|item| {
        if !is_user(item) {
            return None;
        }
        let t = item_text(item);
        if t.contains(COMPACT_MARK) {
            Some(t)
        } else {
            None
        }
    })
}

/// Local memory fold of `head`. Also reconsolidates any previous fold sitting in `head`.
pub fn extractive_brief(goal: &str, head: &[Value]) -> String {
    let prev = harvest_from_head(head);
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
        } else if !section_empty(&prev.original) {
            gist(&prev.original, GOAL_CHARS)
        } else {
            users.first().cloned().unwrap_or_else(|| clip_goal(goal))
        }
    };

    let later = take_last(
        &users
            .iter()
            .filter(|u| u.as_str() != original.as_str())
            .cloned()
            .collect::<Vec<_>>(),
        MAX_LATER,
    );
    let mut direction = String::new();
    for u in &later {
        append_block(&mut direction, &format!("- {u}"));
    }

    let where_were = assistants
        .last()
        .cloned()
        .filter(|s| !s.trim().is_empty())
        .map(|s| gist(&s, ASSISTANT_CHARS))
        .unwrap_or_default();

    let recent_errors = take_last(&errors, MAX_ERRORS);
    let recent_paths = take_last(&paths, MAX_PATHS);
    let mut facts = String::new();
    for e in &recent_errors {
        append_block(&mut facts, &format!("- {e}"));
    }
    for p in &recent_paths {
        append_block(&mut facts, &format!("- {p}"));
    }

    let new = Harvest {
        original: original.clone(),
        direction,
        facts,
        where_were,
        open_work: collect_open_work(head),
    };
    emit_brief(&merge_harvest(&original, &prev, &new), false)
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
pub fn memory_ask(goal: &str, previous: Option<&str>) -> String {
    let original = clip_goal(goal);
    let mut ask = format!(
        "{MEMORY_FOLD_MARK}\nThis is not a new user task. Do not call tools. Do not continue the work in this message.\n\nYou lived the older turns. Write sparse long-term memory for them. The most recent turns will stay verbatim as short-term memory — do not try to replace those.\n\nKeep only what YOU judge is:\n1. The original request (must remain): {original}\n2. Direction-changing or shocking events — a user constraint that rewrote the plan, a surprise, a failure that forced a new approach.\n3. Load-bearing facts that construct or support this task — decisions, invariants, paths, APIs you would need in order to reconstruct the work with reasoning. Not a transcript of every tool call.\n4. Open work — the exact unfinished thread: last bug, last failed check, last screenshot finding, the next concrete action already in flight. If you were debugging, write the bug. Do not write that the work is ready so you can start a new layer.\n\nOmit routine reads, successful boilerplate, and anything the next you can guess from (1)–(4) plus common sense.\n\nWrite markdown with these headings only:\n## Original request\n## Direction changes\n## Load-bearing facts\n## Where you were\n## Open work\n\nIf a heading has nothing, write \"none\". Hard cap: 80 lines. No tool-call dump.\n"
    );
    if let Some(prev) = previous.map(str::trim).filter(|p| !p.is_empty()) {
        ask.push_str(
            "\nPrevious long-term memory — reconsolidate it. Keep every direction change and load-bearing fact that is still true. Do not drop them just to be shorter.\n\n",
        );
        ask.push_str(prev);
        ask.push('\n');
    }
    ask
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
/// Previous compact sections are merged in so a forgetful rewrite cannot drop them.
pub fn wrap_lived_brief(goal: &str, model_text: &str) -> String {
    wrap_lived_brief_with(goal, model_text, &Harvest::default(), "")
}

fn wrap_lived_brief_with(goal: &str, model_text: &str, prev: &Harvest, open_work: &str) -> String {
    let body = strip_fences(model_text);
    let body = if let Some(rest) = body.split(COMPACT_MARK).last() {
        rest.trim().to_string()
    } else {
        body
    };
    let mut incoming = Harvest::default();
    let wrapped = if body.contains("## ") {
        format!("{COMPACT_MARK}\n{body}")
    } else {
        format!("{COMPACT_MARK}\n## Load-bearing facts\n{body}\n")
    };
    harvest_brief(&wrapped, &mut incoming);
    if section_empty(&incoming.original)
        && !section_empty(goal)
        && !body.to_ascii_lowercase().contains("original request")
    {
        incoming.original = clip_goal(goal);
    }
    if section_empty(&incoming.where_were) && !body.contains("## ") {
        incoming.where_were.clear();
    }
    let mut merged = merge_harvest(goal, prev, &incoming);
    if !section_empty(open_work) {
        merged.open_work = open_work.trim().to_string();
    }
    emit_brief(&merged, true)
}

/// Prefer `response.text`; if that is empty, scrape assistant/message output items.
pub fn lived_text_from_complete(text: &str, output_items: &[Value]) -> String {
    if lived_brief_usable(text) {
        return text.to_string();
    }
    let mut out = String::new();
    for item in output_items {
        let typ = item_type(item);
        if is_assistant(item) || typ == "message" {
            let t = item_text(item);
            if t.is_empty() {
                continue;
            }
            if !out.is_empty() {
                out.push('\n');
            }
            out.push_str(&t);
        }
    }
    if lived_brief_usable(&out) {
        out
    } else {
        text.to_string()
    }
}

pub fn compact_history(
    goal: &str,
    history: &[Value],
    keep_recent: usize,
    lived: &str,
) -> Result<CompactOutcome> {
    let (head, tail) = split_head_tail(history, keep_recent);
    if head.is_empty() {
        return Err(Error::Provider("nothing to compact".into()));
    }
    if !lived_brief_usable(lived) {
        return Err(Error::Provider("lived gist unusable".into()));
    }
    let prev = harvest_from_head(head);
    let open = open_work_from(head, tail);
    Ok(CompactOutcome {
        items: splice_brief(&wrap_lived_brief_with(goal, lived, &prev, &open), tail),
        dropped: head.len(),
        kept: tail.len(),
        method: "lived".into(),
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

    fn n_users(n: usize) -> Vec<Value> {
        (0..n).map(|i| user(&format!("u{i}"))).collect()
    }

    #[test]
    fn parse_window_k_and_raw() {
        assert_eq!(parse_window("262K"), Some(262 * 1024));
        assert_eq!(parse_window(" 262144 "), Some(262_144));
        assert_eq!(parse_window(""), None);
        assert_eq!(format_window(262 * 1024), "262K");
    }

    #[test]
    fn grok46_window_is_500k_trigger_is_half() {
        assert_eq!(context_window("grok-4.6"), 500_000);
        assert_eq!(trigger_tokens(500_000), 250_000);
        assert!(should_compact(250_000, 500_000, &n_users(20), 6));
        assert!(!should_compact(249_999, 500_000, &n_users(20), 6));
        assert!(!should_compact(400_000, 500_000, &n_users(5), 6));
    }

    #[test]
    fn estimate_does_not_treat_image_payload_as_text() {
        let huge = "A".repeat(1_000_000);
        let items = vec![json!({
            "type": "function_call_output",
            "call_id": "c1",
            "output": [
                {"type": "input_text", "text": "ok"},
                {
                    "type": "input_image",
                    "image_url": format!("data:image/jpeg;base64,{huge}"),
                    "detail": "high"
                }
            ]
        })];
        let n = estimate_tokens("hi", &items);
        assert!(
            n < 5_000,
            "data URI must not count as chars/4, got {n}"
        );
    }

    #[test]
    fn should_not_compact_when_split_head_is_below_min() {
        let mut items = vec![user("[grokaagent compact v1]\nlived gist")];
        for i in 0..20 {
            items.push(tool(&format!("c{i}"), "now", "{}"));
            items.push(tool_out(&format!("c{i}"), "ok"));
        }
        assert!(
            split_head_tail(&items, DEFAULT_KEEP_RECENT).0.len() < MIN_HEAD_ITEMS,
            "post-fold tail walk must leave a tiny head, got {}",
            split_head_tail(&items, DEFAULT_KEEP_RECENT).0.len()
        );
        assert!(!should_compact(
            400_000,
            500_000,
            &items,
            DEFAULT_KEEP_RECENT
        ));
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
        assert!(brief.contains("do not start a new layer"), "{brief}");
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
            user("[timer fired]\nname: n1\nseconds: 5"),
            assistant("still on real goal"),
        ];
        let brief = extractive_brief("real goal", &head);
        assert!(brief.contains("real goal"), "{brief}");
        assert!(!brief.contains("[background exited]"), "{brief}");
        assert!(!brief.contains("[backgrounds closed]"), "{brief}");
        assert!(!brief.contains("[timer fired]"), "{brief}");
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

    fn fold_has_no_function_call_then_user(input: &[Value]) -> bool {
        input.windows(2).all(|w| {
            !(item_type(&w[0]) == "function_call"
                && w[1].get("role").and_then(Value::as_str) == Some("user"))
        })
    }

    #[test]
    fn fold_input_does_not_follow_function_call_with_user() {
        let trailing = vec![user("goal"), tool("c1", "now", "{}")];
        assert!(
            !fold_has_no_function_call_then_user(&[
                trailing[0].clone(),
                trailing[1].clone(),
                json!({"role":"user","content":"ask"}),
            ]),
            "sanity: raw head+ask is the illegal xAI shape"
        );
        let input = fold_input(&trailing, "goal");
        assert!(
            fold_has_no_function_call_then_user(&input),
            "fold must not send function_call then user: {input:?}"
        );
        assert_eq!(input.last().unwrap()["role"], "user");
        assert!(input.last().unwrap()["content"]
            .as_str()
            .unwrap()
            .contains(MEMORY_FOLD_MARK));
        assert!(input.iter().any(|i| {
            item_type(i) == "function_call_output" && call_id(i) == Some("c1")
        }));

        let parallel = vec![
            user("goal"),
            tool("a", "now", "{}"),
            tool("b", "now", "{}"),
            tool_out("b", "ok"),
        ];
        let input = fold_input(&parallel, "goal");
        assert!(
            fold_has_no_function_call_then_user(&input),
            "{input:?}"
        );
        assert!(input.iter().any(|i| {
            item_type(i) == "function_call_output" && call_id(i) == Some("a")
        }));
    }

    #[test]
    fn fold_input_leaves_closed_head_alone() {
        let head = vec![
            user("goal"),
            tool("c1", "now", "{}"),
            tool_out("c1", "ok"),
        ];
        let input = fold_input(&head, "goal");
        assert_eq!(input.len(), head.len() + 1);
        assert_eq!(input[input.len() - 2]["type"], "function_call_output");
    }

    #[test]
    fn extractive_is_readable_brief_plus_raw_tail() {
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
        let (head, tail) = split_head_tail(&history, 2);
        let items = extractive_items("build the kernel", head, tail);
        assert_ne!(items[0].get("type").and_then(Value::as_str), Some("compaction"));
        let brief = items[0]["content"].as_str().unwrap();
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
        assert_eq!(items[items.len() - 1]["content"], "recent-b");
        assert!(
            items
                .iter()
                .any(|i| i.get("content") == Some(&json!("recent-a"))),
            "{:?}",
            items
        );
    }

    #[test]
    fn missing_goal_falls_back_to_first_real_user() {
        let history: Vec<Value> = (0..10).map(|i| user(&format!("m{i}"))).collect();
        let (head, tail) = split_head_tail(&history, 3);
        let items = extractive_items("ship compact", head, tail);
        assert!(items[0]["content"].as_str().unwrap().contains("ship compact"));
        assert_eq!(items.last().unwrap()["content"], "m9");
    }

    #[test]
    fn memory_ask_is_not_a_new_task_and_keeps_original() {
        let ask = memory_ask("ship the kernel", None);
        assert!(ask.contains(MEMORY_FOLD_MARK));
        assert!(ask.contains("not a new user task"), "{ask}");
        assert!(ask.contains("Do not call tools"), "{ask}");
        assert!(ask.contains("ship the kernel"), "{ask}");
        assert!(ask.contains("Direction changes"), "{ask}");
        assert!(ask.contains("Load-bearing facts"), "{ask}");
        assert!(ask.contains("Open work"), "{ask}");
        assert!(ask.contains("unfinished thread"), "{ask}");
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
        assert!(brief.contains("live thread"), "{brief}");
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
        let out = compact_history("build a cli", &history, 2, lived).unwrap();
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
    fn compact_pins_open_work_when_lived_gist_declares_ready() {
        let history = vec![
            user("build a racing game"),
            user("pad-a"),
            user("pad-b"),
            user("pad-c"),
            assistant("Title screen is up, but it's still on LOADING — check the console."),
            tool("c1", "screenshot", "{}"),
            tool_out("c1", "ok"),
        ];
        let lived = "## Original request\nbuild a racing game\n## Direction changes\nnone\n## Load-bearing facts\nHarbor Ridge exists\n## Where you were\nGame booted. Add harbor water next.\n## Open work\nnone\n";
        let out = compact_history("build a racing game", &history, 3, lived).unwrap();
        let brief = out.items[0]["content"].as_str().unwrap();
        assert!(brief.contains("## Open work"), "{brief}");
        assert!(brief.contains("LOADING"), "{brief}");
        assert!(brief.contains("screenshot"), "{brief}");
        assert!(
            brief.contains("do not start a new layer"),
            "orientation must forbid abandoning the thread: {brief}"
        );
        assert!(
            brief.contains("outrank a polished status summary"),
            "{brief}"
        );
        assert!(brief.contains("Game booted"), "{brief}");
    }

    #[test]
    fn unusable_lived_text_does_not_compact() {
        let history: Vec<Value> = (0..8).map(|i| user(&format!("m{i}"))).collect();
        let err = compact_history("ship it", &history, 2, "ok").unwrap_err();
        assert!(err.to_string().contains("unusable"), "{err}");
    }

    #[test]
    fn lived_brief_usable_rejects_echoed_ask() {
        assert!(!lived_brief_usable("ok"));
        assert!(!lived_brief_usable(&memory_ask("goal", None)));
        assert!(lived_brief_usable(
            "## Original request\nbuild a cli\n## Where you were\nwriting tests\n"
        ));
    }

    fn lived_gist() -> String {
        wrap_lived_brief(
            "build a cli",
            "## Original request\nbuild a cli\n## Direction changes\nuse sqlite, not json files\n## Load-bearing facts\nsrc/db.rs is the store\n## Where you were\nwriting sqlite tests\n",
        )
    }

    #[test]
    fn second_fold_keeps_lived_facts_the_new_gist_omitted() {
        let history = vec![
            user(&lived_gist()),
            user("also add tests"),
            assistant("adding tests now"),
            user("recent"),
        ];
        let forgetful = "## Original request\nbuild a cli\n## Direction changes\nnone\n## Load-bearing facts\nnone\n## Where you were\nadding tests now\n";
        let out = compact_history("build a cli", &history, 1, forgetful).unwrap();
        assert_eq!(out.method, "lived");
        let brief = out.items[0]["content"].as_str().unwrap();
        assert!(brief.contains("use sqlite, not json files"), "{brief}");
        assert!(brief.contains("src/db.rs is the store"), "{brief}");
        assert!(brief.contains("adding tests now"), "{brief}");
    }

    #[test]
    fn extractive_reconsolidates_previous_lived_sections() {
        let mut history = vec![user(&lived_gist())];
        for i in 0..6 {
            history.push(user(&format!("pad-{i}")));
        }
        let (head, _) = split_head_tail(&history, 2);
        let brief = extractive_brief("build a cli", head);
        assert!(brief.contains("use sqlite, not json files"), "{brief}");
        assert!(brief.contains("src/db.rs is the store"), "{brief}");
    }

    #[test]
    fn memory_ask_pins_previous_compact() {
        let prev = lived_gist();
        let ask = memory_ask("build a cli", Some(&prev));
        assert!(ask.contains("reconsolidate"), "{ask}");
        assert!(ask.contains("src/db.rs is the store"), "{ask}");
        assert!(ask.contains(COMPACT_MARK), "{ask}");
    }

    #[test]
    fn previous_compact_text_finds_the_block_even_among_later_users() {
        let items = vec![
            user(&lived_gist()),
            user("follow-up"),
            assistant("ok"),
        ];
        let prev = previous_compact_text(&items).expect("compact block");
        assert!(prev.contains("src/db.rs is the store"), "{prev}");
    }

    #[test]
    fn lived_text_from_complete_scrapes_output_items_when_text_empty() {
        let items = vec![json!({
            "type": "message",
            "role": "assistant",
            "content": [{"type": "output_text", "text": "## Original request\nbuild a cli\n## Where you were\nwriting tests\n"}]
        })];
        let got = lived_text_from_complete("", &items);
        assert!(lived_brief_usable(&got), "{got}");
        assert!(got.contains("writing tests"), "{got}");
    }
}
