//! Degenerate-repetition guard.
//!
//! Now and then a model falls into a loop and emits the same character,
//! word or sentence until it hits its output limit. The guard watches the
//! streamed text and reasoning, stops the response as soon as the tail has
//! become one short unit repeated many times, and lets the agent retry the
//! turn with a note. Finished responses (and tool-call arguments, which are
//! not streamed) are checked once more.
//!
//! Legitimate repetition is bounded; a loop is not. When the repeated unit
//! already repeats in the recent input (a tool output full of duplicate
//! lines, a pasted log) or the user asked for repetition ("repeat 100
//! times", 「重複 50 次」), the guard switches to [`RELAXED_TEXT`]: the task
//! may repeat as much as it needs, and only a runaway flood is stopped.

use std::sync::Mutex;

use tokio::sync::Notify;

use crate::error::Error;

/// How much repetition counts as a loop.
#[derive(Clone, Copy, Debug)]
pub struct Rules {
    /// Longest repeating unit considered, in characters.
    pub max_period: usize,
    /// The repeated run must cover at least this many characters…
    pub min_span: usize,
    /// …and at least this many copies of the unit.
    pub min_repeats: usize,
    /// Runs of whitespace only must be this long to count.
    pub blank_span: usize,
}

/// Streamed text and reasoning: a word repeated ~60 times, a sentence ~6.
pub const TEXT: Rules = Rules { max_period: 512, min_span: 300, min_repeats: 6, blank_span: 2_000 };
/// Tool-call arguments legitimately carry repetitive data (fixtures,
/// tables), so only a flood counts.
pub const ARGS: Rules = Rules { max_period: 512, min_span: 4_000, min_repeats: 20, blank_span: 8_000 };
/// When the input or the request itself is repetitive.
pub const RELAXED_TEXT: Rules = Rules { max_period: 512, min_span: 20_000, min_repeats: 200, blank_span: 20_000 };
pub const RELAXED_ARGS: Rules = Rules { max_period: 512, min_span: 60_000, min_repeats: 500, blank_span: 60_000 };

/// Enough tail to measure a relaxed run.
const TAIL_CHARS: usize = 24_576;
/// Recent input kept for the "was it already repetitive" check.
const CONTEXT_CHARS: usize = 64_000;
const CHECK_EVERY: usize = 128;
const MARK: &str = "[repetition loop]";

#[derive(Clone, Debug, PartialEq)]
pub struct Trip {
    /// Where it happened: `text`, `reasoning` or `arguments of <tool>`.
    pub source: String,
    /// One copy of the repeating unit.
    pub unit: String,
    pub repeats: usize,
}

impl Trip {
    /// The unit, clipped and escaped for a one-line message.
    pub fn preview(&self) -> String {
        let clipped: String = self.unit.chars().take(80).collect();
        let more = if self.unit.chars().count() > 80 { "…" } else { "" };
        format!("{}{more}", clipped.escape_debug())
    }

    pub fn to_error(&self) -> Error {
        Error::Provider(format!(
            "{MARK} {} kept repeating \"{}\" ({}×)",
            self.source,
            self.preview(),
            self.repeats
        ))
    }
}

pub fn is_loop(err: &Error) -> bool {
    err.to_string().contains(MARK)
}

/// The shortest unit whose copies fill the end of `text`, if that run is
/// long enough under `rules`.
pub fn find_loop(text: &str, rules: &Rules) -> Option<(String, usize)> {
    let chars: Vec<char> = text.chars().collect();
    let n = chars.len();
    // A period that already repeats makes its multiples repeat too; they
    // cannot pass where it failed, and walking them is the costly part.
    let mut repeating: Vec<usize> = Vec::new();
    for p in 1..=rules.max_period.min(n / 2) {
        if repeating.iter().any(|q| p % q == 0) {
            continue;
        }
        // Walk back while each char equals the one a period earlier.
        let mut k = 0;
        while k + p < n && chars[n - 1 - k] == chars[n - 1 - k - p] {
            k += 1;
        }
        let span = k + p;
        let repeats = span / p;
        if repeats >= 2 {
            repeating.push(p);
        }
        let unit: String = chars[n - p..].iter().collect();
        let need = if unit.trim().is_empty() {
            rules.blank_span
        } else {
            rules.min_span.max(p * rules.min_repeats)
        };
        if repeats >= rules.min_repeats && span >= need {
            return Some((unit, repeats));
        }
    }
    None
}

/// What the model was given, for telling a loop from requested repetition.
#[derive(Clone, Debug, Default)]
pub struct Context {
    input: String,
    /// The latest user request asks for repetition.
    intent: bool,
}

impl Context {
    /// The latest real user message plus recent user turns and tool
    /// outputs, newest first, capped.
    pub fn from_items(items: &[serde_json::Value]) -> Self {
        let mut input = String::new();
        let mut latest_user: Option<String> = None;
        for item in items.iter().rev() {
            let text = item_text(item);
            if text.is_empty() {
                continue;
            }
            let role = item.get("role").and_then(serde_json::Value::as_str);
            if role == Some("user") && latest_user.is_none() && !text.starts_with('[') {
                latest_user = Some(text.clone());
            }
            if role == Some("user") || item.get("output").is_some() {
                input.push_str(&text);
                input.push('\n');
            }
            if input.len() >= CONTEXT_CHARS && latest_user.is_some() {
                break;
            }
        }
        let intent = latest_user.as_deref().is_some_and(asks_for_repetition);
        Self { input, intent }
    }

    pub fn from_request(latest_user: &str, input: &str) -> Self {
        Self { input: input.to_string(), intent: asks_for_repetition(latest_user) }
    }

    /// Repetition of `unit` is expected here: the request asks for it, or
    /// the input already carries `unit` back to back.
    fn expects(&self, unit: &str) -> bool {
        if self.intent {
            return true;
        }
        let u = unit.trim();
        if u.is_empty() {
            return false;
        }
        // Short units must already repeat for a while ("aaa" is everywhere).
        let copies = (60 / u.chars().count()).clamp(3, 60);
        self.input.contains(&u.repeat(copies))
            || self.input.contains(&unit.repeat(copies))
    }
}

fn asks_for_repetition(text: &str) -> bool {
    static RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    RE.get_or_init(|| {
        regex::Regex::new(
            r"(?i)(\brepeat|repetition|重複|反覆|反复|重复|\d{2,}\s*(次|遍|行|times\b|lines?\b|copies\b|rows\b)|\bx\s*\d{2,}\b)",
        )
        .expect("repetition intent regex")
    })
    .is_match(text)
}

fn item_text(item: &serde_json::Value) -> String {
    for key in ["content", "output"] {
        match item.get(key) {
            Some(serde_json::Value::String(s)) => return s.clone(),
            Some(serde_json::Value::Array(parts)) => {
                let t: String = parts.iter().filter_map(|p| p.get("text").and_then(serde_json::Value::as_str)).collect();
                if !t.is_empty() {
                    return t;
                }
            }
            _ => {}
        }
    }
    String::new()
}

/// One check under the context: strict rules unless the repetition is
/// expected, then relaxed ones.
fn judge(text: &str, strict: &Rules, relaxed: &Rules, ctx: &Context) -> Option<(String, usize)> {
    let (unit, repeats) = find_loop(text, strict)?;
    if !ctx.expects(&unit) {
        return Some((unit, repeats));
    }
    find_loop(text, relaxed)
}

#[derive(Default)]
struct Tail {
    buf: String,
    fresh: usize,
}

impl Tail {
    fn push(&mut self, delta: &str) -> bool {
        self.buf.push_str(delta);
        let over = self.buf.chars().count().saturating_sub(TAIL_CHARS);
        if over > 0 {
            let cut = self.buf.char_indices().nth(over).map(|(i, _)| i).unwrap_or(0);
            self.buf.drain(..cut);
        }
        self.fresh += delta.chars().count();
        if self.fresh >= CHECK_EVERY {
            self.fresh = 0;
            true
        } else {
            false
        }
    }
}

/// Watches one model response while it streams.
#[derive(Default)]
pub struct Guard {
    state: Mutex<(Tail, Tail, Option<Trip>)>,
    notify: Notify,
    ctx: Context,
}

impl Guard {
    pub fn new() -> Self {
        Self::default()
    }

    /// A guard that knows the request, so requested or echoed repetition
    /// is not mistaken for a loop.
    pub fn with_context(ctx: Context) -> Self {
        Self { ctx, ..Self::default() }
    }

    /// Forget everything: a new request starts.
    pub fn reset(&self) {
        *self.state.lock().unwrap_or_else(|e| e.into_inner()) = Default::default();
    }

    pub fn feed_text(&self, delta: &str) {
        self.feed(delta, false);
    }

    pub fn feed_reasoning(&self, delta: &str) {
        self.feed(delta, true);
    }

    fn feed(&self, delta: &str, reasoning: bool) {
        if delta.is_empty() {
            return;
        }
        let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if st.2.is_some() {
            return;
        }
        let tail = if reasoning { &mut st.1 } else { &mut st.0 };
        if !tail.push(delta) {
            return;
        }
        if let Some((unit, repeats)) = judge(&tail.buf, &TEXT, &RELAXED_TEXT, &self.ctx) {
            st.2 = Some(Trip {
                source: if reasoning { "reasoning" } else { "text" }.into(),
                unit,
                repeats,
            });
            drop(st);
            self.notify.notify_waiters();
            self.notify.notify_one();
        }
    }

    pub fn trip(&self) -> Option<Trip> {
        self.state.lock().unwrap_or_else(|e| e.into_inner()).2.clone()
    }

    /// Resolves once the stream has looped.
    pub async fn tripped(&self) -> Trip {
        loop {
            let wait = self.notify.notified();
            if let Some(t) = self.trip() {
                return t;
            }
            wait.await;
        }
    }
}

/// Check a finished response: its text, and each tool call's arguments.
pub fn check_response(text: &str, calls: &[crate::provider::FunctionCall], ctx: &Context) -> Option<Trip> {
    if let Some((unit, repeats)) = judge(text, &TEXT, &RELAXED_TEXT, ctx) {
        return Some(Trip { source: "text".into(), unit, repeats });
    }
    for c in calls {
        if c.arguments.len() < ARGS.min_span {
            continue;
        }
        // The loop may sit inside a JSON string, before its closing quote.
        let body = c.arguments.trim_end().trim_end_matches(['}', '"', ']', ' ', '\n']);
        if let Some((unit, repeats)) = judge(body, &ARGS, &RELAXED_ARGS, ctx) {
            return Some(Trip { source: format!("arguments of {}", c.name), unit, repeats });
        }
    }
    None
}

/// What the model is told before the retried turn.
pub fn retry_note(trip: &Trip) -> String {
    format!(
        "{} Your previous response was cut off because its {} kept repeating \"{}\". \
That output was discarded. Continue the task from where you were: rethink the next step, \
phrase it differently, and do not repeat that text. If you need repetitive data in a file, \
generate it with a short script instead of writing it out.",
        crate::notice::LOOP_GUARD,
        trip.source,
        trip.preview()
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::FunctionCall;

    #[test]
    fn catches_char_word_and_sentence_loops() {
        let char_loop = format!("Let me think{}", "a".repeat(400));
        assert_eq!(find_loop(&char_loop, &TEXT).unwrap().0, "a");
        let word_loop = format!("Hmm. {}", "wait, ".repeat(80));
        assert_eq!(find_loop(&word_loop, &TEXT).unwrap().0.trim(), "wait,");
        let zh_loop = format!("好的，{}", "我再檢查一次。".repeat(60));
        assert!(find_loop(&zh_loop, &TEXT).is_some());
        let sentence = "The function returns the parsed value and closes the file handle. ";
        let sentence_loop = format!("Summary: {}", sentence.repeat(8));
        let (unit, repeats) = find_loop(&sentence_loop, &TEXT).unwrap();
        assert_eq!(unit, sentence);
        assert!(repeats >= 6);
    }

    #[test]
    fn ordinary_text_code_and_tables_are_not_loops() {
        let prose = "Rust ownership means each value has one owner. When the owner goes out of scope the value is dropped. \
Borrowing lets code use a value without taking ownership, and the borrow checker makes sure references stay valid.";
        assert!(find_loop(prose, &TEXT).is_none());
        let code: String = (0..40).map(|i| format!("    assert_eq!(f({i}), {});\n", i * 2)).collect();
        assert!(find_loop(&code, &TEXT).is_none());
        let table: String = (0..30).map(|i| format!("| row {i} | ok | {} ms |\n", i * 7)).collect();
        assert!(find_loop(&table, &TEXT).is_none());
        // A few repeats, or a short divider, are fine.
        assert!(find_loop(&"ha ".repeat(5), &TEXT).is_none());
        assert!(find_loop(&format!("title\n{}", "=".repeat(80)), &TEXT).is_none());
        assert!(find_loop(&"\n".repeat(500), &TEXT).is_none(), "blank runs need blank_span");
    }

    #[test]
    fn tool_arguments_need_a_flood() {
        let fixture = format!(r#"{{"path":"a.txt","contents":"{}"}}"#, "x,".repeat(500));
        let call = |args: String| FunctionCall { call_id: "c".into(), name: "write_file".into(), arguments: args };
        let none = Context::default();
        assert!(check_response("", &[call(fixture)], &none).is_none(), "1000 chars of fixture data is fine");
        let flood = format!(r#"{{"path":"a.txt","contents":"start {}"}}"#, "the same line again\\n".repeat(400));
        let trip = check_response("", &[call(flood)], &none).unwrap();
        assert_eq!(trip.source, "arguments of write_file");
    }

    #[test]
    fn requested_repetition_is_not_a_loop() {
        let out = "hello world\n".repeat(100);
        for ask in ["請把 hello world 重複輸出 100 次", "print hello world 100 times", "寫 100 行 hello world"] {
            let ctx = Context::from_request(ask, ask);
            assert!(check_response(&out, &[], &ctx).is_none(), "{ask}");
        }
        // Without the request it is a loop.
        assert!(check_response(&out, &[], &Context::from_request("summarize the log", "")).is_some());
    }

    #[test]
    fn echoing_repetitive_input_is_not_a_loop() {
        let log = "WARN disk almost full on /dev/sda1\n".repeat(200);
        let ctx = Context::from_request("what does this log say?", &log);
        let echo = format!("The log repeats one line:\n{}", "WARN disk almost full on /dev/sda1\n".repeat(50));
        assert!(check_response(&echo, &[], &ctx).is_none());
        // A different unit the input never repeated is still a loop.
        let other = format!("Answer: {}", "I am not sure. ".repeat(40));
        assert!(check_response(&other, &[], &ctx).is_some());
    }

    #[test]
    fn a_runaway_flood_trips_even_when_repetition_was_asked_for() {
        let ctx = Context::from_request("repeat ok 100 times", "");
        let flood = "ok ".repeat(10_000);
        let trip = check_response(&flood, &[], &ctx).expect("30K chars of 'ok' is a runaway");
        assert!(trip.repeats >= 200);
    }

    #[test]
    fn context_reads_the_latest_user_request_and_tool_outputs() {
        let items = vec![
            serde_json::json!({"role": "user", "content": "請把這行重複 30 次"}),
            serde_json::json!({"type": "function_call_output", "call_id": "a", "output": "x".repeat(10)}),
        ];
        let ctx = Context::from_items(&items);
        assert!(ctx.intent);
        assert!(ctx.input.contains("xxxxxxxxxx"));
        for ordinary in ["fix the bug", "修改第 3 行的錯字", "列出 5 個檔案", "跑 2 次測試"] {
            let plain = Context::from_items(&[serde_json::json!({"role": "user", "content": ordinary})]);
            assert!(!plain.intent, "{ordinary}");
        }
    }

    #[test]
    fn long_legit_repetition_is_cheap_to_check() {
        let text = "abcdef".repeat(4_000);
        let started = std::time::Instant::now();
        for _ in 0..20 {
            let _ = find_loop(&text, &RELAXED_TEXT);
        }
        assert!(started.elapsed() < std::time::Duration::from_secs(2), "{:?}", started.elapsed());
    }

    #[tokio::test]
    async fn streaming_guard_trips_and_wakes_the_waiter() {
        let guard = Guard::new();
        guard.feed_text("I will now list the steps. ");
        let waiter = async { guard.tripped().await };
        let feeder = async {
            for _ in 0..200 {
                guard.feed_reasoning("again and ");
                tokio::task::yield_now().await;
            }
        };
        let (trip, ()) = tokio::join!(waiter, feeder);
        assert_eq!(trip.source, "reasoning");
        assert_eq!(trip.unit, "again and ");
        assert!(is_loop(&trip.to_error()));
        guard.reset();
        assert!(guard.trip().is_none());
    }
}
