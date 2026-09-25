//! Context memory: fold old turns into long-term memory written by the
//! working model itself.
//!
//! The model that lived the turns judges what mattered: direction changes,
//! surprises, load-bearing facts, the open thread. The memory is sparse on
//! purpose; gaps are reconstructed with reasoning. The newest turns stay
//! verbatim as short-term memory. The encrypted xAI `/responses/compact` blob
//! is not used.
//!
//! Everything is budgeted in tokens against the model window:
//! - fold when the context reaches [`TRIGGER_RATIO`] of the window;
//! - short-term memory (the verbatim tail) gets at most [`TAIL_RATIO`];
//! - one fold request may use at most [`FOLD_INPUT_RATIO`]: a normal fold
//!   resends the old turns unchanged (cache hit); a head too large for that is
//!   slimmed (huge tool outputs trimmed), then folded in chunks, oldest first;
//! - past [`HARD_RATIO`] with no working fold, an emergency fold drops the
//!   old turns with a deterministic memory so the run can continue.

use std::time::Duration;

use serde_json::{json, Value};

use crate::error::{Error, Result};
use crate::provider::{CompleteRequest, Provider, ReasoningEffort};
use crate::tools::ToolSpec;

pub const TRIGGER_RATIO: f32 = 0.5;
pub const TAIL_RATIO: f32 = 0.2;
pub const FOLD_INPUT_RATIO: f32 = 0.75;
pub const HARD_RATIO: f32 = 0.9;
/// Aim for this much of the window right after a fold (instructions, tools,
/// memory and tail together), so the next fold is not one turn away.
pub const AFTER_RATIO: f32 = 0.35;
/// Room reserved for the rendered memory when sizing the tail.
const MEMORY_RESERVE_TOKENS: u32 = 2_500;
/// A fold needs at least this many old items besides a previous memory.
pub const MIN_HEAD_ITEMS: usize = 2;

pub const COMPACT_MARK: &str = "[grokaagent memory v2]";
/// Memories written by the previous design; still read on resume.
const LEGACY_MARKS: &[&str] = &["[grokaagent compact v1]"];
pub const MEMORY_FOLD_MARK: &str = "[grokaagent memory-fold]";

const GROK4_WINDOW: u32 = 500_000;
pub const DEFAULT_WINDOW: u32 = 128_000;
/// The original request is kept whole up to this size (task specs live there).
const GOAL_CHARS: usize = 6_000;
const MEMORY_CHARS: usize = 8_000;
const MIN_MEMORY_CHARS: usize = 40;
/// Tool outputs longer than this are trimmed when a head must be slimmed.
const SLIM_OUTPUT_CHARS: usize = 4_000;
const SLIM_TEXT_CHARS: usize = 12_000;
const VISION_ESTIMATE_TOKENS: u32 = 1024;
/// Model calls per fold (or per chunk) before the fold gives up.
pub const FOLD_ATTEMPTS: u32 = 3;
const FOLD_RETRY: Duration = if cfg!(test) {
    Duration::from_millis(5)
} else {
    Duration::from_secs(2)
};

// ---------------------------------------------------------------- windows

pub fn context_window(model: &str) -> u32 {
    if model.to_ascii_lowercase().starts_with("grok-4") {
        GROK4_WINDOW
    } else {
        DEFAULT_WINDOW
    }
}

/// `262K` → 262×1024, `1M` → 1024×1024, `128000` stays as-is.
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
        String::new()
    } else if n % (1024 * 1024) == 0 {
        format!("{}M", n / (1024 * 1024))
    } else if n % 1024 == 0 {
        format!("{}K", n / 1024)
    } else {
        n.to_string()
    }
}

/// Token budgets derived from one model window.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Budget {
    pub window: u32,
    pub trigger: u32,
    pub tail: u32,
    pub fold_input: u32,
    pub hard: u32,
    pub after: u32,
}

impl Budget {
    /// The tail budget once fixed costs (instructions, tool schemas) and
    /// the memory are counted, so a fold lands near [`AFTER_RATIO`].
    pub fn with_fixed(mut self, fixed: u32) -> Self {
        let room = self.after.saturating_sub(fixed.saturating_add(MEMORY_RESERVE_TOKENS));
        self.tail = self.tail.min(room).max(self.window / 20).max(1);
        self
    }

    pub fn new(window: u32) -> Self {
        let at = |r: f32| ((window as f32) * r) as u32;
        Self {
            window,
            trigger: at(TRIGGER_RATIO),
            tail: at(TAIL_RATIO).max(1),
            fold_input: at(FOLD_INPUT_RATIO),
            hard: at(HARD_RATIO),
            after: at(AFTER_RATIO),
        }
    }
}

// ---------------------------------------------------------------- estimates

/// Local token estimate: ASCII ≈ 4 chars/token, other scripts (CJK) ≈ 1
/// char/token, images ≈ one vision tile. Used when the provider has not
/// reported usage for the current history.
pub fn estimate_tokens(instructions: &str, items: &[Value]) -> u32 {
    let mut t = text_tokens(instructions);
    for item in items {
        t = t.saturating_add(item_tokens(item));
    }
    t.max(1)
}

pub fn item_tokens(item: &Value) -> u32 {
    let (mut ascii, mut other, mut images) = (0u32, 0u32, 0u32);
    count(item, &mut ascii, &mut other, &mut images);
    (ascii / 4)
        .saturating_add(other)
        .saturating_add(images.saturating_mul(VISION_ESTIMATE_TOKENS))
        .saturating_add(4)
}

fn text_tokens(s: &str) -> u32 {
    let (mut ascii, mut other) = (0u32, 0u32);
    count_str(s, &mut ascii, &mut other);
    ascii / 4 + other
}

fn count_str(s: &str, ascii: &mut u32, other: &mut u32) {
    for c in s.chars() {
        if c.is_ascii() {
            *ascii = ascii.saturating_add(1);
        } else {
            *other = other.saturating_add(1);
        }
    }
}

fn count(v: &Value, ascii: &mut u32, other: &mut u32, images: &mut u32) {
    match v {
        Value::String(s) if s.starts_with("data:image/") => *images = images.saturating_add(1),
        Value::String(s) => count_str(s, ascii, other),
        Value::Array(arr) => arr.iter().for_each(|x| count(x, ascii, other, images)),
        Value::Object(map) => {
            if map.get("type").and_then(Value::as_str) == Some("input_image") {
                *images = images.saturating_add(1);
                return;
            }
            for (k, x) in map {
                *ascii = ascii.saturating_add(k.len() as u32);
                count(x, ascii, other, images);
            }
        }
        Value::Number(n) => *ascii = ascii.saturating_add(n.to_string().len() as u32),
        Value::Bool(_) | Value::Null => *ascii = ascii.saturating_add(4),
    }
}

// ---------------------------------------------------------------- items

fn item_type(item: &Value) -> &str {
    item.get("type").and_then(Value::as_str).unwrap_or("")
}

fn call_id(item: &Value) -> Option<&str> {
    item.get("call_id").and_then(Value::as_str)
}

fn is_user(item: &Value) -> bool {
    item.get("role").and_then(Value::as_str) == Some("user")
}

fn is_assistant(item: &Value) -> bool {
    item.get("role").and_then(Value::as_str) == Some("assistant") || item_type(item) == "message"
}

fn item_text(item: &Value) -> String {
    for key in ["content", "output"] {
        match item.get(key) {
            Some(Value::String(s)) => return s.clone(),
            Some(Value::Array(parts)) => {
                let text: String = parts
                    .iter()
                    .filter(|p| p.get("type").and_then(Value::as_str) != Some("input_image"))
                    .filter_map(|p| p.get("text").and_then(Value::as_str))
                    .collect();
                if !text.is_empty() {
                    return text;
                }
            }
            _ => {}
        }
    }
    item.get("text").and_then(Value::as_str).unwrap_or("").to_string()
}

fn has_mark(text: &str) -> bool {
    text.contains(COMPACT_MARK) || LEGACY_MARKS.iter().any(|m| text.contains(m))
}

/// A long-term memory block (this design's or the previous one's).
fn is_memory(item: &Value) -> bool {
    is_user(item) && has_mark(&item_text(item))
}

/// System notices that look like user turns but are not the user.
fn is_notice_user(text: &str) -> bool {
    let t = text.trim_start();
    has_mark(t)
        || t.starts_with(MEMORY_FOLD_MARK)
        || t.starts_with(crate::background::EXIT_NOTICE_PREFIX)
        || t.starts_with(crate::background::CLOSED_NOTICE_PREFIX)
        || t.starts_with(crate::timer::FIRED_NOTICE_PREFIX)
}

fn is_real_user(item: &Value) -> bool {
    is_user(item) && {
        let t = item_text(item);
        !t.trim().is_empty() && !is_notice_user(&t)
    }
}

/// Never start a slice on a tool output (its call must come with it) or
/// right after a reasoning item (it belongs to what follows).
fn align_cut(items: &[Value], mut cut: usize) -> usize {
    loop {
        let before = cut;
        if cut < items.len() && item_type(&items[cut]) == "function_call_output" {
            if let Some(id) = call_id(&items[cut]) {
                if let Some(i) = items[..cut]
                    .iter()
                    .rposition(|it| item_type(it) == "function_call" && call_id(it) == Some(id))
                {
                    cut = i;
                }
            }
        }
        if cut > 0 && item_type(&items[cut - 1]) == "reasoning" {
            cut -= 1;
        }
        // Parallel calls: keep a run of function_calls together.
        while cut > 0 && cut < items.len() && item_type(&items[cut]) == "function_call"
            && item_type(&items[cut - 1]) == "function_call"
        {
            cut -= 1;
        }
        if cut == before {
            return cut;
        }
    }
}

// ---------------------------------------------------------------- split

/// Where short-term memory starts: newest items up to the tail budget, whole
/// tool steps only, reaching back to the latest real user turn when that
/// costs at most twice the budget. `max_items` caps the tail (tests, tuning).
/// Returns 0 when there is nothing worth folding.
pub fn plan_cut(items: &[Value], budget: &Budget, max_items: Option<usize>) -> usize {
    if items.len() < MIN_HEAD_ITEMS + 1 {
        return 0;
    }
    let mut cut = items.len();
    let mut tokens = 0u32;
    while cut > 1 {
        let t = item_tokens(&items[cut - 1]);
        let full = max_items.is_some_and(|m| items.len() - cut >= m);
        if cut < items.len() && (tokens.saturating_add(t) > budget.tail || full) {
            break;
        }
        tokens = tokens.saturating_add(t);
        cut -= 1;
    }
    cut = align_cut(items, cut);
    if max_items.is_none() && !items[cut..].iter().any(is_real_user) {
        if let Some(u) = items[..cut].iter().rposition(is_real_user) {
            let reach = align_cut(items, u);
            let cost: u32 = items[reach..].iter().map(item_tokens).sum();
            if cost <= budget.tail.saturating_mul(2) {
                cut = reach;
            }
        }
    }
    let foldable = items[..cut].iter().filter(|i| !is_memory(i)).count();
    if foldable < MIN_HEAD_ITEMS {
        0
    } else {
        cut
    }
}

// ---------------------------------------------------------------- memory text

/// Long-term memory, one field per section.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Memory {
    pub original: String,
    pub direction: String,
    pub facts: String,
    pub where_were: String,
    pub open_work: String,
}

#[derive(Clone, Copy, PartialEq)]
enum Section {
    Original,
    Direction,
    Facts,
    Where,
    Open,
}

fn section_for(heading: &str) -> Option<Section> {
    let h = heading.to_lowercase();
    let any = |keys: &[&str]| keys.iter().any(|k| h.contains(k));
    if any(&["open", "todo", "next", "unfinished", "pending", "未完成", "待辦", "下一步", "待處理", "未解決"]) {
        Some(Section::Open)
    } else if any(&["where", "progress", "status", "state", "進度", "目前", "現況", "位置", "狀態"]) {
        Some(Section::Where)
    } else if any(&["direction", "change", "pivot", "方向", "轉折", "變更", "改變", "調整"]) {
        Some(Section::Direction)
    } else if any(&["original", "request", "goal", "原始", "需求", "目標", "最初"]) {
        Some(Section::Original)
    } else if any(&["fact", "load", "decision", "knowledge", "事實", "關鍵", "決定", "知識", "要點"]) {
        Some(Section::Facts)
    } else {
        None
    }
}

/// A heading line in any common shape: `## X`, `### 2. X`, `**X**`, `X:`
/// on its own line. Returns the bare heading text.
fn heading_of(line: &str) -> Option<String> {
    let t = line.trim();
    let bare = if let Some(rest) = t.strip_prefix('#') {
        rest.trim_start_matches('#').trim()
    } else if t.starts_with("**") && t.ends_with("**") && t.len() > 4 {
        t.trim_matches('*').trim()
    } else {
        return None;
    };
    let bare = bare
        .trim_start_matches(|c: char| c.is_ascii_digit() || matches!(c, '.' | ')' | ' '))
        .trim_end_matches([':', '：'])
        .trim();
    (!bare.is_empty()).then(|| bare.to_string())
}

fn is_empty_body(s: &str) -> bool {
    let t = s.trim().trim_matches(['-', '*', ' ']).trim().to_lowercase();
    t.is_empty() || matches!(t.as_str(), "none" | "n/a" | "na" | "無" | "没有" | "沒有" | "(none)")
}

/// Parse a memory the model wrote. Unknown sections are kept under facts
/// with their heading; text with no headings becomes facts.
pub fn parse_memory(text: &str) -> Memory {
    let mut mem = Memory::default();
    let text = strip_fences(text);
    let text = match text.rfind(COMPACT_MARK) {
        Some(i) => &text[i + COMPACT_MARK.len()..],
        None => text.as_str(),
    };
    let mut current: Option<(Option<Section>, String)> = None;
    let mut body = String::new();
    let mut preface = String::new();
    let flush = |cur: &Option<(Option<Section>, String)>, body: &str, mem: &mut Memory| {
        let Some((sec, heading)) = cur else {
            return;
        };
        if is_empty_body(body) {
            return;
        }
        let body = body.trim();
        let slot = match sec {
            Some(Section::Original) => &mut mem.original,
            Some(Section::Direction) => &mut mem.direction,
            Some(Section::Facts) | None => &mut mem.facts,
            Some(Section::Where) => &mut mem.where_were,
            Some(Section::Open) => &mut mem.open_work,
        };
        let piece = if sec.is_none() { format!("{heading}:\n{body}") } else { body.to_string() };
        if !slot.is_empty() {
            slot.push('\n');
        }
        slot.push_str(&piece);
    };
    for line in text.lines() {
        if let Some(h) = heading_of(line) {
            flush(&current, &body, &mut mem);
            body.clear();
            current = Some((section_for(&h), h));
        } else if current.is_some() {
            body.push_str(line);
            body.push('\n');
        } else {
            preface.push_str(line);
            preface.push('\n');
        }
    }
    flush(&current, &body, &mut mem);
    // Our own wrapper prose before the first heading is not memory.
    let preface = preface.trim();
    if !preface.is_empty() && !preface.starts_with("This is your long-term memory") && !is_empty_body(preface) {
        mem.facts = if mem.facts.is_empty() {
            preface.to_string()
        } else {
            format!("{preface}\n{}", mem.facts)
        };
    }
    mem
}

impl Memory {
    fn is_blank(&self) -> bool {
        [&self.direction, &self.facts, &self.where_were, &self.open_work]
            .iter()
            .all(|s| is_empty_body(s))
    }
}

/// The model's new memory replaces the old one section by section (it was
/// asked to reconsolidate the old one); sections it left empty keep the old
/// text. The original request is pinned to the first one known.
pub fn merge_memory(goal: &str, prev: &Memory, new: &Memory) -> Memory {
    let pick = |n: &str, p: &str| if is_empty_body(n) { p.to_string() } else { n.to_string() };
    let original = [prev.original.as_str(), &clip_goal(goal), new.original.as_str()]
        .into_iter()
        .find(|s| !is_empty_body(s))
        .unwrap_or_default()
        .to_string();
    Memory {
        original,
        direction: pick(&new.direction, &prev.direction),
        facts: pick(&new.facts, &prev.facts),
        where_were: pick(&new.where_were, &prev.where_were),
        open_work: pick(&new.open_work, &prev.open_work),
    }
}

const MEMORY_INTRO: &str = "This is your long-term memory of the earlier turns, written by you after living them — not a transcript and not a new user request.\nOnly key details are kept. Fill gaps with reasoning and common sense; re-read files or re-run commands when you need exact contents. Do not restart and do not invent a different mission.";
const MEMORY_FOOTER: &str = "The verbatim items after this block are your short-term memory and the live thread; they outrank this summary. Finish the open work before starting something new.";

/// Render with the size cap. Over the cap, the oldest lines of facts and
/// direction changes go first; lines are never cut in the middle.
pub fn render_memory(mem: &Memory) -> String {
    // The original request does not count against the cap: it is the spec.
    let cap = MEMORY_CHARS + mem.original.chars().count();
    let mut facts: Vec<String> = mem.facts.lines().map(str::to_string).collect();
    let mut direction: Vec<String> = mem.direction.lines().map(str::to_string).collect();
    let mut dropped = 0usize;
    loop {
        let out = render_with(mem, &direction.join("\n"), &facts.join("\n"), dropped);
        if out.chars().count() <= cap {
            return out;
        }
        if facts.len() > 3 {
            facts.remove(0);
        } else if direction.len() > 2 {
            direction.remove(0);
        } else if !facts.is_empty() {
            facts.remove(0);
        } else if !direction.is_empty() {
            direction.remove(0);
        } else {
            // Only single huge sections remain: clip them by characters.
            let clip = |s: &str| s.chars().take(MEMORY_CHARS / 4).collect::<String>() + "…";
            let slim = Memory {
                original: clip(&mem.original),
                direction: String::new(),
                facts: String::new(),
                where_were: clip(&mem.where_were),
                open_work: clip(&mem.open_work),
            };
            return render_with(&slim, "", "", dropped);
        }
        dropped += 1;
    }
}

fn render_with(mem: &Memory, direction: &str, facts: &str, dropped: usize) -> String {
    let mut out = format!("{COMPACT_MARK}\n{MEMORY_INTRO}\n");
    let mut section = |title: &str, body: &str| {
        if !is_empty_body(body) {
            out.push_str(&format!("\n## {title}\n{}\n", body.trim()));
        }
    };
    section("Original request", &mem.original);
    section("Direction changes", direction);
    let facts = if dropped > 0 {
        format!("({dropped} older lines dropped to fit)\n{facts}")
    } else {
        facts.to_string()
    };
    section("Load-bearing facts", &facts);
    section("Where you were", &mem.where_were);
    section("Open work", &mem.open_work);
    out.push('\n');
    out.push_str(MEMORY_FOOTER);
    out.push('\n');
    out
}

pub fn clip_goal(goal: &str) -> String {
    let n = goal.chars().count();
    if n <= GOAL_CHARS {
        return goal.trim().to_string();
    }
    let head: String = goal.chars().take(GOAL_CHARS * 3 / 4).collect();
    let tail: String = goal.chars().skip(n - GOAL_CHARS / 4).collect();
    format!("{head}\n…\n{tail}")
}

fn strip_fences(s: &str) -> String {
    let t = s.trim();
    let Some(rest) = t.strip_prefix("```") else {
        return t.to_string();
    };
    let rest = rest.split_once('\n').map(|(_, r)| r).unwrap_or("");
    rest.trim_end().strip_suffix("```").unwrap_or(rest).trim().to_string()
}

/// Latest memory text in `items`, if any.
pub fn previous_memory(items: &[Value]) -> Option<String> {
    items.iter().rev().find(|i| is_memory(i)).map(item_text)
}

// ---------------------------------------------------------------- fold request

/// Ask the working model to write long-term memory. Not a new user task.
pub fn memory_ask(goal: &str, previous: Option<&str>) -> String {
    let mut ask = format!(
        "{MEMORY_FOLD_MARK}\nThis is not a new user task. Do not call tools. Do not continue the work in this message.\n\n\
You lived the turns above. Write sparse long-term memory for them; the newest turns stay verbatim as short-term memory, so do not repeat those.\n\n\
Keep only what you judge the next you needs:\n\
1. Original request (kept for you): {}\n\
2. Direction changes — user constraints or corrections that rewrote the plan, surprises, failures that forced a new approach. A later user request that started a new task goes here, newest last; the newest one is what you are doing now.\n\
3. Load-bearing facts — decisions, invariants, file paths, APIs, commands, numbers you would need to rebuild the work by reasoning. Not a log of tool calls.\n\
4. Where you were — the state of the work in a few lines.\n\
5. Open work — the exact unfinished thread: the last failing check, the bug being chased, the next concrete step already in flight. Do not declare the work finished to start something new.\n\n\
Skip routine reads and anything the next you can infer from the rest plus common sense.\n\n\
Use exactly these markdown headings (in English), content in any language:\n\
## Direction changes\n## Load-bearing facts\n## Where you were\n## Open work\n\n\
Write \"none\" under a heading with nothing. At most about 80 lines.\n",
        clip_goal(goal)
    );
    if let Some(prev) = previous.map(str::trim).filter(|p| !p.is_empty()) {
        ask.push_str(
            "\nYour previous long-term memory is below. Reconsolidate: carry over every direction change and fact that is still true, drop what is obsolete, and merge it with what happened since.\n\n",
        );
        ask.push_str(prev);
        ask.push('\n');
    }
    ask
}

/// Answer unpaired `function_call` items right where they stand, so no
/// call is ever followed by a user message (xAI rejects "cannot follow a
/// function_call with a new user message"; chat APIs want the results next
/// to their calls).
pub fn close_fold_head(head: &[Value]) -> Vec<Value> {
    let answered: std::collections::HashSet<&str> = head
        .iter()
        .filter(|i| item_type(i) == "function_call_output")
        .filter_map(call_id)
        .collect();
    let placeholder = |id: &str| {
        json!({
            "type": "function_call_output",
            "call_id": id,
            "output": "(folded into long-term memory)",
        })
    };
    let mut out = Vec::with_capacity(head.len() + 2);
    let mut open: Vec<String> = Vec::new();
    for item in head {
        let typ = item_type(item);
        if typ != "function_call" && typ != "function_call_output" && typ != "reasoning" {
            out.extend(open.drain(..).map(|id| placeholder(&id)));
        }
        if typ == "function_call" {
            if let Some(id) = call_id(item) {
                if !answered.contains(id) {
                    open.push(id.to_string());
                }
            }
        }
        out.push(item.clone());
    }
    out.extend(open.drain(..).map(|id| placeholder(&id)));
    out
}

/// The fold request input: the old turns, then the memory ask.
pub fn fold_input(head: &[Value], goal: &str) -> Vec<Value> {
    let mut input = close_fold_head(head);
    input.push(json!({
        "role": "user",
        "content": memory_ask(goal, previous_memory(head).as_deref()),
    }));
    input
}

/// Trim huge tool outputs and pasted texts, drop images: what a fold needs
/// to see of an oversized head.
pub fn slim_items(items: &[Value]) -> Vec<Value> {
    let mut out = items.to_vec();
    crate::vision::strip_attached_images(&mut out);
    for item in &mut out {
        let limit = if item_type(item) == "function_call_output" {
            SLIM_OUTPUT_CHARS
        } else if is_memory(item) {
            continue;
        } else {
            SLIM_TEXT_CHARS
        };
        let key = if item.get("output").is_some() { "output" } else { "content" };
        let text = item_text(item);
        let n = text.chars().count();
        if n <= limit {
            continue;
        }
        let head: String = text.chars().take(limit * 3 / 5).collect();
        let tail: String = text.chars().skip(n - limit / 4).collect();
        item[key] = json!(format!(
            "{head}\n[… {} characters trimmed for the memory fold; re-run or re-read if needed …]\n{tail}",
            n - limit * 3 / 5 - limit / 4
        ));
    }
    out
}

/// Split `items` into consecutive chunks of at most `budget` tokens each,
/// only at boundaries that keep tool steps whole.
fn chunk_ranges(items: &[Value], budget: u32) -> Vec<std::ops::Range<usize>> {
    let mut out = Vec::new();
    let mut start = 0;
    let mut tokens = 0u32;
    let mut i = 0;
    while i < items.len() {
        let t = item_tokens(&items[i]);
        if i > start && tokens.saturating_add(t) > budget {
            let cut = align_cut(items, i);
            if cut > start {
                out.push(start..cut);
                start = cut;
                tokens = items[start..i].iter().map(item_tokens).sum();
            }
        }
        tokens = tokens.saturating_add(t);
        i += 1;
    }
    if start < items.len() {
        out.push(start..items.len());
    }
    out
}

fn tools_tokens(tools: &[ToolSpec]) -> u32 {
    tools
        .iter()
        .map(|t| text_tokens(&t.description) + text_tokens(&t.parameters.to_string()) + 8)
        .sum()
}

/// Errors that mean "this request is too long for the model".
pub fn context_overflow(err: &Error) -> bool {
    let s = err.to_string().to_ascii_lowercase();
    [
        "context length", "context_length", "maximum context", "context window", "too many tokens",
        "prompt is too long", "input is too long", "exceeds the context", "reduce the length",
        "token limit", "too large",
    ]
    .iter()
    .any(|k| s.contains(k))
}

// ---------------------------------------------------------------- fold

/// What one fold needs from the running session.
pub struct FoldCtx<'a, P: Provider> {
    pub provider: &'a P,
    pub instructions: &'a str,
    pub goal: &'a str,
    pub model: &'a str,
    pub effort: ReasoningEffort,
    pub send_reasoning: bool,
    /// The working conversation's key, so a plain fold lands on the replica
    /// that already caches these turns.
    pub cache_key: &'a str,
    pub client_tools: Vec<ToolSpec>,
    pub server_tools: Vec<String>,
    pub budget: Budget,
    pub max_tail_items: Option<usize>,
    /// Progress for the user (retries, fallbacks).
    pub notice: &'a (dyn Fn(String) + Send + Sync),
}

#[derive(Clone, Debug, PartialEq)]
pub struct Folded {
    pub items: Vec<Value>,
    pub dropped: usize,
    pub kept: usize,
    /// `lived`, `lived-slim`, `lived-chunked`, `trim`, `emergency`.
    pub method: String,
}

pub enum FoldError {
    /// Nothing to fold (the head is too small).
    Nothing,
    Failed(Error),
}

/// Fold the old part of `history` into long-term memory written by the
/// working model.
pub async fn fold<P: Provider>(ctx: &FoldCtx<'_, P>, history: &[Value]) -> std::result::Result<Folded, FoldError> {
    let fixed = text_tokens(ctx.instructions) + tools_tokens(&ctx.client_tools);
    let cut = plan_cut(history, &ctx.budget.with_fixed(fixed), ctx.max_tail_items);
    if cut == 0 {
        return Err(FoldError::Nothing);
    }
    let (head, tail) = history.split_at(cut);
    let prev = previous_memory(head).map(|t| parse_memory(&t)).unwrap_or_default();
    // What every fold request carries besides the turns themselves.
    let overhead = fixed + text_tokens(&memory_ask(ctx.goal, previous_memory(head).as_deref()));
    let fits = |items: &[Value]| estimate_tokens("", items).saturating_add(overhead) <= ctx.budget.fold_input;

    // 1) The old turns as they are: same prefix as the conversation, cached.
    let mut method = "lived";
    let mut attempt_head = head.to_vec();
    if !fits(&attempt_head) {
        attempt_head = slim_items(head);
        method = "lived-slim";
    }
    let text = if fits(&attempt_head) {
        match ask_memory(ctx, fold_input(&attempt_head, ctx.goal)).await {
            Ok(t) => Some(t),
            Err(e) if context_overflow(&e) && method == "lived" => {
                (ctx.notice)("壓縮：舊內容超出模型上下文，改用精簡版".into());
                attempt_head = slim_items(head);
                method = "lived-slim";
                if fits(&attempt_head) {
                    match ask_memory(ctx, fold_input(&attempt_head, ctx.goal)).await {
                        Ok(t) => Some(t),
                        Err(e) if context_overflow(&e) => None,
                        Err(e) => return Err(FoldError::Failed(e)),
                    }
                } else {
                    None
                }
            }
            Err(e) if context_overflow(&e) => None,
            Err(e) => return Err(FoldError::Failed(e)),
        }
    } else {
        None
    };
    let (memory, method) = match text {
        Some(t) => (merge_memory(ctx.goal, &prev, &parse_memory(&t)), method),
        // 2) Still too big: fold in chunks, oldest first, carrying memory.
        None => (fold_chunks(ctx, head, prev, overhead).await?, "lived-chunked"),
    };
    Ok(splice(memory, head.len(), tail, method))
}

async fn fold_chunks<P: Provider>(
    ctx: &FoldCtx<'_, P>,
    head: &[Value],
    mut memory: Memory,
    overhead: u32,
) -> std::result::Result<Memory, FoldError> {
    let body: Vec<Value> = slim_items(head).into_iter().filter(|i| !is_memory(i)).collect();
    let room = ctx
        .budget
        .fold_input
        .saturating_sub(overhead)
        .saturating_sub(text_tokens(&render_memory(&memory)))
        .max(1_000);
    let ranges = chunk_ranges(&body, room);
    (ctx.notice)(format!("壓縮：舊內容分 {} 段整理", ranges.len()));
    for range in ranges {
        let mut chunk = Vec::new();
        let rendered = render_memory(&memory);
        let has_prev = !memory.is_blank();
        if has_prev {
            chunk.push(json!({"role": "user", "content": rendered.clone()}));
        }
        chunk.extend(body[range].iter().cloned());
        let mut input = close_fold_head(&chunk);
        input.push(json!({
            "role": "user",
            "content": memory_ask(ctx.goal, has_prev.then_some(rendered.as_str())),
        }));
        let text = ask_memory(ctx, input).await.map_err(FoldError::Failed)?;
        memory = merge_memory(ctx.goal, &memory, &parse_memory(&text));
    }
    Ok(memory)
}

/// One fold request, retried a few times. Transient provider errors are
/// retried inside the provider layer's callers too; this covers bad output.
async fn ask_memory<P: Provider>(ctx: &FoldCtx<'_, P>, input: Vec<Value>) -> Result<String> {
    let mut last = Error::Provider("fold not attempted".into());
    for attempt in 1..=FOLD_ATTEMPTS {
        let req = CompleteRequest {
            instructions: ctx.instructions.to_string(),
            input: input.clone(),
            client_tools: ctx.client_tools.clone(),
            server_tools: ctx.server_tools.clone(),
            cache_key: ctx.cache_key.to_string(),
            previous_response_id: None,
            store: false,
            reasoning_effort: ctx.effort,
            send_reasoning: ctx.send_reasoning,
            model: ctx.model.to_string(),
            tool_choice: Some("none".into()),
        };
        let err = match ctx.provider.complete(req).await {
            Ok(r) if !r.function_calls.is_empty() => Error::Provider("memory fold called tools".into()),
            Ok(r) => {
                let text = memory_text_from(&r.text, &r.output_items);
                if memory_usable(&text) {
                    return Ok(text);
                }
                Error::Provider("memory fold returned no usable memory".into())
            }
            Err(e) if context_overflow(&e) => return Err(e),
            Err(e) => e,
        };
        (ctx.notice)(format!("壓縮重試 {attempt}/{FOLD_ATTEMPTS}: {err}"));
        last = err;
        if attempt < FOLD_ATTEMPTS {
            tokio::time::sleep(FOLD_RETRY).await;
        }
    }
    Err(last)
}

pub fn memory_usable(text: &str) -> bool {
    let t = strip_fences(text);
    t.chars().count() >= MIN_MEMORY_CHARS && !t.trim_start().starts_with(MEMORY_FOLD_MARK)
}

/// `response.text`, or the assistant message items when text is empty.
pub fn memory_text_from(text: &str, output_items: &[Value]) -> String {
    if memory_usable(text) {
        return text.to_string();
    }
    let joined: Vec<String> = output_items
        .iter()
        .filter(|i| is_assistant(i))
        .map(item_text)
        .filter(|t| !t.is_empty())
        .collect();
    let joined = joined.join("\n");
    if memory_usable(&joined) {
        joined
    } else {
        text.to_string()
    }
}

fn splice(memory: Memory, dropped: usize, tail: &[Value], method: &str) -> Folded {
    let mut items = vec![json!({"role": "user", "content": render_memory(&memory)})];
    items.extend(tail.iter().cloned());
    Folded { items, dropped, kept: tail.len(), method: method.to_string() }
}

/// No model available (or it keeps failing) and the context is nearly full:
/// keep the run alive. The old turns are dropped behind a deterministic
/// memory; if that is not enough, the whole history is slimmed.
pub fn emergency_fold(goal: &str, history: &[Value], budget: &Budget, max_tail_items: Option<usize>) -> Folded {
    let cut = plan_cut(history, budget, max_tail_items);
    if cut == 0 {
        let items = slim_items(history);
        return Folded { kept: items.len(), items, dropped: 0, method: "trim".into() };
    }
    let (head, tail) = history.split_at(cut);
    let mut memory = previous_memory(head).map(|t| parse_memory(&t)).unwrap_or_default();
    memory = merge_memory(goal, &memory, &Memory::default());
    if let Some(last) = head.iter().rev().filter(|i| is_assistant(i)).map(item_text).find(|t| !t.trim().is_empty()) {
        let last: String = last.chars().take(600).collect();
        memory.where_were = format!("(last thing you said before the fold) {last}");
    }
    let note = format!(
        "- {} older items were dropped without a written memory (the memory fold failed). Re-read files and re-run checks you need.",
        head.len()
    );
    memory.facts = if memory.facts.is_empty() { note } else { format!("{}\n{note}", memory.facts) };
    let mut folded = splice(memory, head.len(), tail, "emergency");
    if estimate_tokens("", &folded.items) > budget.hard {
        folded.items = slim_items(&folded.items);
    }
    folded
}

/// Slim the whole history when the verbatim tail alone is too big to fold
/// anything (few items, huge outputs).
pub fn trim_history(history: &[Value]) -> Option<Folded> {
    let slim = slim_items(history);
    (estimate_tokens("", &slim) < estimate_tokens("", history)).then(|| Folded {
        kept: slim.len(),
        items: slim,
        dropped: 0,
        method: "trim".into(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::{CompactRequest, CompactResponse, CompleteResponse};
    use std::sync::Mutex;

    fn user(s: &str) -> Value {
        json!({"role": "user", "content": s})
    }
    fn assistant(s: &str) -> Value {
        json!({"role": "assistant", "content": s})
    }
    fn call(id: &str, args: &str) -> Value {
        json!({"type": "function_call", "call_id": id, "name": "read_file", "arguments": args})
    }
    fn out(id: &str, s: &str) -> Value {
        json!({"type": "function_call_output", "call_id": id, "output": s})
    }

    #[test]
    fn windows_parse_and_budget() {
        assert_eq!(parse_window("262K"), Some(262 * 1024));
        assert_eq!(parse_window("1M"), Some(1024 * 1024));
        assert_eq!(format_window(262 * 1024), "262K");
        assert_eq!(context_window("grok-4.7"), 500_000);
        let b = Budget::new(500_000);
        assert_eq!((b.trigger, b.tail, b.fold_input, b.hard), (250_000, 100_000, 375_000, 450_000));
        // 32K window with 7K of instructions + tools: the tail shrinks so a
        // fold lands near 35% instead of just under the 50% trigger.
        let small = Budget::new(32 * 1024).with_fixed(7_000);
        assert!(small.tail <= 32 * 1024 * 35 / 100 - 7_000 - 2_500, "{}", small.tail);
        assert!(small.tail >= 32 * 1024 / 20);
    }

    #[test]
    fn cjk_counts_more_than_ascii_bytes_over_four() {
        let zh = estimate_tokens("", &[user(&"中".repeat(1000))]);
        let en = estimate_tokens("", &[user(&"a".repeat(1000))]);
        assert!(zh >= 1000 && en < 300, "zh {zh} en {en}");
        let img = json!({"role": "user", "content": [{"type": "input_image", "image_url": format!("data:image/png;base64,{}", "A".repeat(100_000))}]});
        assert!(estimate_tokens("", &[img]) < 2_000);
    }

    #[test]
    fn tail_is_token_budgeted_and_keeps_tool_steps_whole() {
        let big = "x".repeat(40_000); // ~10k tokens
        let mut items = vec![user("build it")];
        for i in 0..6 {
            items.push(call(&format!("c{i}"), "{}"));
            items.push(out(&format!("c{i}"), &big));
        }
        let b = Budget::new(100_000); // tail 20k
        let cut = plan_cut(&items, &b, None);
        assert!(cut > 0);
        let tail = &items[cut..];
        assert_eq!(item_type(&tail[0]), "function_call", "tail starts at a call, never at its output");
        let tokens: u32 = tail.iter().map(item_tokens).sum();
        assert!(tokens <= 2 * b.tail, "{tokens}");
        assert!(tail.len() < items.len() - 2);
    }

    #[test]
    fn tail_reaches_back_to_the_last_user_turn_when_cheap() {
        let mut items = vec![user("old task"), assistant("ok"), user("new direction: use sqlite")];
        for i in 0..8 {
            items.push(call(&format!("c{i}"), "{}"));
            items.push(out(&format!("c{i}"), "small"));
        }
        let cut = plan_cut(&items, &Budget::new(2_000), None);
        assert!(items[cut..].iter().any(|i| item_text(i).contains("use sqlite")));
    }

    #[test]
    fn nothing_to_fold_when_the_head_is_only_memory() {
        let items = vec![user(&format!("{COMPACT_MARK}\n## Open work\nx")), user("hi"), assistant("yo")];
        assert_eq!(plan_cut(&items, &Budget::new(100), Some(2)), 0);
    }

    #[test]
    fn parse_accepts_chinese_numbered_and_bold_headings() {
        for text in [
            "## 原始需求\n做 CLI\n## 關鍵事實\n資料庫在 data/app.db\n## 未完成的工作\n修 test_login 401",
            "## 1. Original request\nbuild a cli\n## 3. Load-bearing facts\ndb in data/app.db\n## 4. Open work\nfix 401",
            "**Load-bearing facts**\ndb in data/app.db\n**Open work:**\nfix 401",
        ] {
            let m = parse_memory(text);
            assert!(m.facts.contains("data/app.db"), "{text} → {m:?}");
            assert!(m.open_work.contains("401"), "{text} → {m:?}");
        }
    }

    #[test]
    fn unknown_sections_and_headingless_text_are_kept() {
        let m = parse_memory("## Risks\nthe migration is irreversible\n## Open work\nnone");
        assert!(m.facts.contains("Risks") && m.facts.contains("irreversible"), "{m:?}");
        assert!(m.open_work.is_empty());
        let m = parse_memory("The API key lives in KEY_ENV and tests use port 8080.");
        assert!(m.facts.contains("port 8080"));
    }

    #[test]
    fn new_memory_replaces_old_and_empty_sections_keep_old() {
        let prev = Memory {
            original: "ship v1".into(),
            direction: "- user wants sqlite".into(),
            facts: "- old fact".into(),
            where_were: "was at step 2".into(),
            open_work: "fix a".into(),
        };
        let new = parse_memory("## Direction changes\nnone\n## Load-bearing facts\n- reconsolidated fact\n## Where you were\nstep 5\n## Open work\nfix b");
        let m = merge_memory("ship v1", &prev, &new);
        assert_eq!(m.direction, "- user wants sqlite");
        assert_eq!(m.facts, "- reconsolidated fact", "no duplicate carry-over");
        assert_eq!(m.open_work, "fix b");
        assert_eq!(m.original, "ship v1");
    }

    #[test]
    fn render_drops_oldest_lines_whole_and_says_so() {
        let facts: Vec<String> = (0..600).map(|i| format!("- fact number {i:04} with some detail")).collect();
        let m = Memory { original: "goal".into(), facts: facts.join("\n"), open_work: "keep me".into(), ..Default::default() };
        let out = render_memory(&m);
        assert!(out.chars().count() <= MEMORY_CHARS + 4);
        assert!(out.contains("fact number 0599 with some detail"), "newest kept whole");
        assert!(!out.contains("fact number 0000"), "oldest dropped");
        assert!(out.contains("older lines dropped"));
        assert!(out.contains("keep me"));
        assert!(out.contains("not a new user request"));
        // Round trip: a rendered memory parses back into the same sections.
        let back = parse_memory(&out);
        assert!(back.open_work.contains("keep me"));
        assert!(back.facts.contains("fact number 0599"));
    }

    #[test]
    fn a_long_original_request_is_kept_whole() {
        let spec: String = (0..120).map(|i| format!("- rule {i:03}: key q{i} must be named exactly k{i}
")).collect();
        let m = merge_memory(&spec, &Memory::default(), &Memory::default());
        let out = render_memory(&m);
        assert!(out.contains("rule 060: key q60 must be named exactly k60"), "middle of the spec survives");
    }

    #[test]
    fn legacy_v1_memory_is_still_read() {
        let v1 = user("[grokaagent compact v1]\nsparse memory\n## Original request\nship\n## Load-bearing facts\n- keep ABI\n");
        let prev = previous_memory(&[v1, user("x")]).unwrap();
        assert!(parse_memory(&prev).facts.contains("keep ABI"));
    }

    #[test]
    fn close_fold_head_answers_open_calls() {
        let head = vec![user("go"), call("a", "{}"), out("a", "ok"), call("b", "{}")];
        let closed = close_fold_head(&head);
        assert_eq!(closed.last().unwrap()["call_id"], "b");
        assert_eq!(item_type(closed.last().unwrap()), "function_call_output");
    }

    #[test]
    fn open_call_is_answered_before_the_next_user_turn() {
        let head = vec![user("go"), call("orphan", "{}"), user("later"), call("a", "{}"), out("a", "ok")];
        let closed = close_fold_head(&head);
        let types: Vec<&str> = closed.iter().map(|i| if is_user(i) { "user" } else { item_type(i) }).collect();
        assert_eq!(types, ["user", "function_call", "function_call_output", "user", "function_call", "function_call_output"]);
    }

    #[test]
    fn slim_trims_outputs_but_not_memory() {
        let mem = user(&format!("{COMPACT_MARK}\n{}", "m".repeat(20_000)));
        let items = slim_items(&[mem.clone(), out("a", &"y".repeat(50_000))]);
        assert_eq!(items[0], mem);
        let o = item_text(&items[1]);
        assert!(o.len() < 6_000 && o.contains("trimmed for the memory fold"), "{}", o.len());
    }

    #[test]
    fn chunks_keep_tool_steps_whole() {
        let mut items = Vec::new();
        for i in 0..10 {
            items.push(call(&format!("c{i}"), "{}"));
            items.push(out(&format!("c{i}"), &"z".repeat(4_000)));
        }
        let ranges = chunk_ranges(&items, 2_500);
        assert!(ranges.len() > 1);
        for r in &ranges {
            assert_ne!(item_type(&items[r.start]), "function_call_output");
        }
        assert_eq!(ranges.last().unwrap().end, items.len());
    }

    #[test]
    fn overflow_errors_are_recognized() {
        assert!(context_overflow(&Error::Provider("HTTP 400: This model's maximum context length is 131072 tokens".into())));
        assert!(context_overflow(&Error::Provider("prompt is too long: 250000 tokens > 200000".into())));
        assert!(!context_overflow(&Error::Provider("HTTP 429: rate limit".into())));
    }

    #[test]
    fn emergency_fold_keeps_the_run_alive() {
        let mut items = vec![user("ship the kernel"), assistant("porting the scheduler now")];
        for i in 0..4 {
            items.push(call(&format!("c{i}"), "{}"));
            items.push(out(&format!("c{i}"), "ok"));
        }
        let f = emergency_fold("ship the kernel", &items, &Budget::new(100), Some(2));
        assert_eq!(f.method, "emergency");
        let mem = item_text(&f.items[0]);
        assert!(mem.contains("ship the kernel") && mem.contains("porting the scheduler") && mem.contains("dropped"));
        assert_eq!(item_type(&f.items[1]), "function_call");
    }

    struct Scripted {
        replies: Mutex<Vec<Result<CompleteResponse>>>,
        inputs: Mutex<Vec<Vec<Value>>>,
    }

    impl Provider for Scripted {
        async fn complete(&self, req: CompleteRequest) -> Result<CompleteResponse> {
            self.inputs.lock().unwrap().push(req.input);
            self.replies.lock().unwrap().remove(0)
        }
        async fn compact(&self, _req: CompactRequest) -> Result<CompactResponse> {
            Err(Error::Provider("unused".into()))
        }
    }

    fn reply(text: &str) -> Result<CompleteResponse> {
        Ok(CompleteResponse { text: text.into(), ..CompleteResponse::new("m") })
    }

    fn ctx<'a>(p: &'a Scripted, notes: &'a (dyn Fn(String) + Send + Sync), window: u32) -> FoldCtx<'a, Scripted> {
        FoldCtx {
            provider: p,
            instructions: "work",
            goal: "ship the kernel",
            model: "m",
            effort: ReasoningEffort::Low,
            send_reasoning: false,
            cache_key: "k",
            client_tools: Vec::new(),
            server_tools: Vec::new(),
            budget: Budget::new(window),
            max_tail_items: Some(2),
            notice: notes,
        }
    }

    #[tokio::test]
    async fn plain_fold_resends_the_head_unchanged() {
        let p = Scripted { replies: Mutex::new(vec![reply("## Load-bearing facts\n- keep ABI stable\n## Open work\nport scheduler")]), inputs: Mutex::new(vec![]) };
        let notes = |_: String| {};
        let history = vec![user("ship the kernel"), assistant("a"), user("b"), assistant("c"), call("x", "{}"), out("x", "ok")];
        let f = fold(&ctx(&p, &notes, 1_000_000), &history).await.ok().unwrap();
        assert_eq!(f.method, "lived");
        let sent = p.inputs.lock().unwrap()[0].clone();
        assert_eq!(&sent[..4], &history[..4], "head verbatim keeps the cache prefix");
        let mem = item_text(&f.items[0]);
        assert!(mem.contains("keep ABI stable") && mem.contains("ship the kernel"));
    }

    #[tokio::test]
    async fn overflowing_fold_falls_back_to_chunks() {
        let p = Scripted {
            replies: Mutex::new(vec![
                Err(Error::Provider("HTTP 400: maximum context length exceeded".into())),
                Err(Error::Provider("HTTP 400: maximum context length exceeded".into())),
                reply("## Load-bearing facts\n- fact from chunk one, long enough"),
                reply("## Load-bearing facts\n- fact from chunk two, long enough\n## Open work\nnext"),
                reply("## Load-bearing facts\n- fact from chunk three, long enough\n## Open work\nlast"),
                reply("## Load-bearing facts\n- fact from chunk four, long enough\n## Open work\nlast"),
                reply("## Load-bearing facts\n- fact from chunk five, long enough\n## Open work\nlast"),
            ]),
            inputs: Mutex::new(vec![]),
        };
        let seen = Mutex::new(Vec::new());
        let notes = |s: String| seen.lock().unwrap().push(s);
        let mut history = vec![user("ship the kernel")];
        for i in 0..6 {
            history.push(call(&format!("c{i}"), "{}"));
            history.push(out(&format!("c{i}"), &"q".repeat(3_000)));
        }
        let f = fold(&ctx(&p, &notes, 8_000), &history).await.ok().unwrap();
        assert!(f.method == "lived-chunked" || f.method == "lived-slim", "{}", f.method);
        let mem = item_text(&f.items[0]);
        assert!(mem.contains("fact from chunk"), "{mem}");
    }
}
