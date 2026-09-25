//! Degenerate-repetition guard.
//!
//! Now and then a model falls into a loop and emits the same character,
//! word or sentence until it hits its output limit. The guard watches the
//! streamed text and reasoning, stops the response as soon as the tail has
//! become one short unit repeated many times, and lets the agent retry the
//! turn with a note. Finished responses (and tool-call arguments, which are
//! not streamed) are checked once more.

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

const TAIL_CHARS: usize = 8_192;
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
    for p in 1..=rules.max_period.min(n / 2) {
        // Walk back while each char equals the one a period earlier.
        let mut k = 0;
        while k + p < n && chars[n - 1 - k] == chars[n - 1 - k - p] {
            k += 1;
        }
        let span = k + p;
        let repeats = span / p;
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
}

impl Guard {
    pub fn new() -> Self {
        Self::default()
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
        if let Some((unit, repeats)) = find_loop(&tail.buf, &TEXT) {
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
pub fn check_response(text: &str, calls: &[crate::provider::FunctionCall]) -> Option<Trip> {
    if let Some((unit, repeats)) = find_loop(text, &TEXT) {
        return Some(Trip { source: "text".into(), unit, repeats });
    }
    for c in calls {
        if c.arguments.len() < ARGS.min_span {
            continue;
        }
        // The loop may sit inside a JSON string, before its closing quote.
        let body = c.arguments.trim_end().trim_end_matches(['}', '"', ']', ' ', '\n']);
        if let Some((unit, repeats)) = find_loop(body, &ARGS) {
            return Some(Trip { source: format!("arguments of {}", c.name), unit, repeats });
        }
    }
    None
}

/// What the model is told before the retried turn.
pub fn retry_note(trip: &Trip) -> String {
    format!(
        "[loop guard] Your previous response was cut off because its {} kept repeating \"{}\". \
That output was discarded. Continue the task from where you were: rethink the next step, \
phrase it differently, and do not repeat that text. If you need repetitive data in a file, \
generate it with a short script instead of writing it out.",
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
        assert!(check_response("", &[call(fixture)]).is_none(), "1000 chars of fixture data is fine");
        let flood = format!(r#"{{"path":"a.txt","contents":"start {}"}}"#, "the same line again\\n".repeat(400));
        let trip = check_response("", &[call(flood)]).unwrap();
        assert_eq!(trip.source, "arguments of write_file");
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
