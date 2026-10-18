//! Windows consoles hand a paste or a dropped file path to the app as plain
//! key presses: crossterm emits no `Event::Paste` there. A run of text keys
//! that arrives in one read, faster than anyone types, is turned back into a
//! paste, so its line breaks do not send and dropped paths can attach.

use std::time::{Duration, Instant};

use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

/// Text keys in one read that make a paste. Typing delivers a key or two per
/// read even during a slow frame; an IME commit of a phrase pastes the same
/// text anyway.
const MIN_KEYS: usize = 6;
/// A console may deliver a long paste over several reads: a text run that
/// opens a read this soon after the loop waits again continues the paste.
const CONTINUE: Duration = Duration::from_millis(50);
/// A read this soon after the loop waits again was already queued: it came
/// while the loop was busy, not as a paste into an idle one.
const QUEUED: Duration = Duration::from_millis(5);
/// Busy this long, the loop may have queued a typed line: MIN_KEYS keys take
/// a fast typist longer.
const STALL: Duration = Duration::from_millis(250);

#[derive(Default)]
pub(crate) struct PasteBurst {
    /// The last read ended inside a paste.
    open: Option<Open>,
    /// `open` was set by the last read and `waiting` has not restarted it yet.
    fresh: bool,
    /// The loop stopped listening for input: a read, or another wakeup.
    woke: Option<Instant>,
    /// The loop last waited for input then, after being busy this long.
    waited: Option<(Instant, Duration)>,
    /// The last read with a key press was queued behind a busy loop.
    last_queued: bool,
    /// `woke` was set by a read, not by another wakeup.
    by_read: bool,
}

/// Line breaks a paste held back at the end of a read: trailing ones are
/// dropped (an IME's confirming Enter), unless the paste goes on in the
/// next read.
struct Open {
    at: Instant,
    held: usize,
    /// One line of plain text and its Enter that queued up while the loop
    /// was busy may be typing: that Enter is pressed after all if the paste
    /// does not go on.
    enter: Vec<Event>,
}

#[derive(Default)]
struct Run {
    raw: Vec<Event>,
    text: String,
    keys: usize,
    /// The run opens the read (nothing came before it).
    first: bool,
    /// The last key was a plain Enter (CR): a Ctrl+Enter (LF) right after it
    /// is the same line break.
    after_cr: bool,
}

enum TextKey {
    Char(char),
    Enter { ctrl: bool },
    Tab,
}

fn text_key(ev: &Event) -> Option<TextKey> {
    let Event::Key(k) = ev else { return None };
    if k.kind != KeyEventKind::Press {
        return None;
    }
    let m = k.modifiers;
    match k.code {
        // Consoles type pasted capitals with Shift and some symbols with AltGr (Ctrl+Alt).
        KeyCode::Char(c)
            if !c.is_control()
                && (m.difference(KeyModifiers::SHIFT).is_empty()
                    || m.contains(KeyModifiers::CONTROL | KeyModifiers::ALT)) =>
        {
            Some(TextKey::Char(c))
        }
        KeyCode::Enter if !m.contains(KeyModifiers::ALT) => Some(TextKey::Enter {
            ctrl: m.contains(KeyModifiers::CONTROL),
        }),
        KeyCode::Tab if m.is_empty() => Some(TextKey::Tab),
        _ => None,
    }
}

impl PasteBurst {
    /// Replace paste-like runs of key presses in one read with `Event::Paste`.
    pub fn coalesce(&mut self, batch: Vec<Event>, now: Instant) -> Vec<Event> {
        if self.woke.is_none() {
            self.woke = Some(now);
            self.by_read = true;
        }
        let queued = self
            .waited
            .is_some_and(|(at, busy)| busy >= STALL && now.saturating_duration_since(at) <= QUEUED);
        let mut out = self.lapse(now);
        if !batch.is_empty() && batch.iter().all(|e| matches!(e, Event::Key(k) if k.kind != KeyEventKind::Press)) {
            // Releases alone (a modifier's, an Alt code's) neither go on
            // with a paste nor end it: the next read still may.
            self.fresh = self.open.is_some();
            out.extend(batch);
            return out;
        }
        self.fresh = false;
        self.last_queued = queued;
        let open = self.open.take();
        let mut cont = open.as_ref().map(|o| o.held);
        // A typed line's Enter, pressed unless this read goes on with the paste.
        let mut enter = open.map(|o| o.enter).unwrap_or_default();
        out.reserve(batch.len());
        let mut run = Run::default();
        // Nothing but key releases came yet.
        let mut lead = true;
        for ev in batch {
            let key = text_key(&ev);
            if key.is_none() && matches!(&ev, Event::Key(k) if k.kind != KeyEventKind::Press) {
                // Windows reports key releases too; they never split a run.
                if run.raw.is_empty() {
                    out.push(ev);
                } else {
                    run.raw.push(ev);
                }
                continue;
            }
            let Some(key) = key else {
                self.finish(std::mem::take(&mut run), &mut cont, &mut out, None, queued);
                if cont.take().is_some() {
                    out.append(&mut enter);
                }
                lead = false;
                out.push(ev);
                continue;
            };
            if run.raw.is_empty() {
                // Line breaks may open a run too (pasted code often starts
                // with one); a lone Enter is too short to count as a paste.
                run.first = lead;
            }
            run.push(ev, key);
        }
        self.finish(run, &mut cont, &mut out, Some(now), queued);
        if cont.is_some() {
            out.append(&mut enter);
        }
        out
    }

    /// One read on Windows: pastes come back together and an IME's
    /// confirming Enter goes, but a typed line's Enter that came due
    /// meanwhile goes first, out of the IME's reach.
    pub fn read(&mut self, batch: Vec<Event>, now: Instant) -> Vec<Event> {
        let mut late = self.expired(now);
        late.extend(super::coalesce_ime_enter(self.coalesce(batch, now)));
        late
    }

    /// The loop woke up (a timer, a message) and stops listening for input
    /// until it waits again. A read counts as waking too.
    pub fn woke(&mut self, now: Instant) {
        self.woke.get_or_insert(now);
    }

    /// The loop is about to wait for input again. A paste the last read left
    /// open gets its window from now: handling that read and painting can
    /// take longer than the window while the rest of the paste is already
    /// queued. Only once per read, so other wakeups cannot keep it open.
    pub fn waiting(&mut self, now: Instant) {
        let mut busy = self.woke.take().map_or(Duration::ZERO, |w| now.saturating_duration_since(w));
        if std::mem::take(&mut self.by_read) && self.last_queued {
            // The rest of a queued backlog is still queued: a console ends a
            // read at every Shift, Ctrl or Alt record, so a typed capital
            // splits the line over reads.
            busy = busy.max(STALL);
        }
        self.waited = Some((now, busy));
        if std::mem::take(&mut self.fresh) {
            if let Some(o) = self.open.as_mut() {
                o.at = now;
            }
        }
    }

    /// Keys due now that a paste the last read left open did not go on: a
    /// typed line's Enter. Nothing while the window has not been restarted.
    pub fn expired(&mut self, now: Instant) -> Vec<Event> {
        if self.fresh {
            return Vec::new();
        }
        self.lapse(now)
    }

    fn lapse(&mut self, now: Instant) -> Vec<Event> {
        match &self.open {
            Some(o) if now.saturating_duration_since(o.at) > CONTINUE => {
                self.open.take().map(|o| o.enter).unwrap_or_default()
            }
            _ => Vec::new(),
        }
    }

    /// Emit a run as a paste or as its own keys. `end` = the run ends the read.
    /// `queued`: the read came at once after the loop was busy long enough
    /// for typing to pile up; a paste into an idle loop keeps its Enter.
    fn finish(&mut self, run: Run, cont: &mut Option<usize>, out: &mut Vec<Event>, end: Option<Instant>, queued: bool) {
        if run.raw.is_empty() {
            return;
        }
        let held = if run.first { cont.take() } else { None };
        let paste = held.is_some() || (run.keys >= MIN_KEYS && !run.text.trim().is_empty());
        if !paste {
            out.extend(run.raw);
            return;
        }
        let mut text = "\n".repeat(held.unwrap_or(0));
        text.push_str(&run.text);
        let body = text.trim_end_matches('\n').len();
        let trailing = text.len() - body;
        let typed = queued && held.is_none() && trailing == 1 && text.is_ascii() && !text[..body].contains('\n');
        let enter = if typed { vec![Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))] } else { Vec::new() };
        text.truncate(body);
        if !text.is_empty() {
            out.push(Event::Paste(text));
        }
        match end {
            Some(at) => {
                self.open = Some(Open { at, held: trailing, enter });
                self.fresh = true;
            }
            // Something else follows in this read: the paste cannot go on.
            None => out.extend(enter),
        }
    }
}

impl Run {
    fn push(&mut self, ev: Event, key: TextKey) {
        self.raw.push(ev);
        self.keys += 1;
        match key {
            TextKey::Char(c) => self.text.push(c),
            TextKey::Enter { ctrl: true } if self.after_cr => {}
            TextKey::Enter { .. } => self.text.push('\n'),
            TextKey::Tab => self.text.push('\t'),
        }
        self.after_cr = matches!(key, TextKey::Enter { ctrl: false });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyEvent, MouseEvent, MouseEventKind};

    fn press(code: KeyCode, mods: KeyModifiers) -> Event {
        Event::Key(KeyEvent::new(code, mods))
    }

    fn release(code: KeyCode) -> Event {
        Event::Key(KeyEvent::new_with_kind(code, KeyModifiers::NONE, KeyEventKind::Release))
    }

    /// What a Windows console delivers for pasted text: a press and a
    /// release per key, capitals with Shift, CR as Enter.
    fn console_keys(s: &str) -> Vec<Event> {
        let mut out = Vec::new();
        for c in s.chars() {
            let (code, mods) = match c {
                '\r' | '\n' => (KeyCode::Enter, KeyModifiers::NONE),
                '\t' => (KeyCode::Tab, KeyModifiers::NONE),
                c if c.is_ascii_uppercase() || c == ':' => (KeyCode::Char(c), KeyModifiers::SHIFT),
                c => (KeyCode::Char(c), KeyModifiers::NONE),
            };
            out.push(press(code, mods));
            out.push(release(code));
        }
        out
    }

    fn pastes(events: &[Event]) -> Vec<&str> {
        events
            .iter()
            .filter_map(|e| match e {
                Event::Paste(s) => Some(s.as_str()),
                _ => None,
            })
            .collect()
    }

    fn presses(events: &[Event]) -> usize {
        events.iter().filter(|e| matches!(e, Event::Key(k) if k.kind == KeyEventKind::Press)).count()
    }

    #[test]
    fn a_dropped_path_typed_by_the_console_becomes_one_paste() {
        let path = r"C:\Users\me\Pictures\Screenshot 2026-10-17 101010.png";
        let out = PasteBurst::default().coalesce(console_keys(path), Instant::now());
        assert_eq!(out.len(), 1);
        assert_eq!(pastes(&out), [path]);
    }

    #[test]
    fn line_breaks_in_a_paste_are_text_not_sends() {
        let out = PasteBurst::default().coalesce(console_keys("first line\rsecond\r"), Instant::now());
        assert_eq!(pastes(&out), ["first line\nsecond"]);
        assert_eq!(presses(&out), 0, "no Enter reaches the composer");
        let mut keys = console_keys("one\r");
        keys.push(press(KeyCode::Enter, KeyModifiers::CONTROL));
        keys.extend(console_keys("two"));
        let out = PasteBurst::default().coalesce(keys, Instant::now());
        assert_eq!(pastes(&out), ["one\ntwo"], "CR LF is one line break");
        let out = PasteBurst::default().coalesce(console_keys("\r\tfn main() {}"), Instant::now());
        assert_eq!(pastes(&out), ["\n\tfn main() {}"], "a leading line break is text too");
        assert_eq!(presses(&out), 0);
    }

    #[test]
    fn typing_and_shortcuts_stay_keys() {
        for typed in ["ok\r", "\r", "\rok", "\tab"] {
            let keys = console_keys(typed);
            assert_eq!(PasteBurst::default().coalesce(keys.clone(), Instant::now()), keys, "{typed:?}");
        }
        let spaces = console_keys("        ");
        assert_eq!(PasteBurst::default().coalesce(spaces.clone(), Instant::now()), spaces);
    }

    #[test]
    fn other_events_split_runs() {
        let mut keys = console_keys("abcdefgh");
        keys.push(press(KeyCode::Char('v'), KeyModifiers::CONTROL));
        keys.push(Event::Mouse(MouseEvent {
            kind: MouseEventKind::ScrollUp,
            column: 0,
            row: 0,
            modifiers: KeyModifiers::NONE,
        }));
        keys.extend(console_keys("xy"));
        let out = PasteBurst::default().coalesce(keys, Instant::now());
        assert_eq!(pastes(&out), ["abcdefgh"]);
        assert_eq!(presses(&out), 3, "Ctrl+V, then x and y as typed");
    }

    #[test]
    fn altgr_symbols_belong_to_the_paste() {
        let mut keys = console_keys("C:");
        keys.push(press(KeyCode::Char('\\'), KeyModifiers::CONTROL | KeyModifiers::ALT));
        keys.extend(console_keys("shot.png"));
        let out = PasteBurst::default().coalesce(keys, Instant::now());
        assert_eq!(pastes(&out), [r"C:\shot.png"]);
    }

    #[test]
    fn an_ime_commit_drops_its_confirming_enter() {
        let out = PasteBurst::default().coalesce(console_keys("日本語を入力します\r"), Instant::now());
        assert_eq!(pastes(&out), ["日本語を入力します"]);
        assert_eq!(presses(&out), 0);
    }

    /// The loop was busy from `from` (a slow frame) and waits again at `to`.
    fn busy(burst: &mut PasteBurst, from: Instant, to: Instant) {
        burst.woke(from);
        burst.waiting(to);
    }

    #[test]
    fn a_typed_line_queued_in_a_slow_frame_still_sends() {
        let t0 = Instant::now();
        let ms = Duration::from_millis;
        let mut burst = PasteBurst::default();
        busy(&mut burst, t0, t0 + ms(600));
        let t1 = t0 + ms(601);
        let out = burst.coalesce(console_keys("ok thanks\r"), t1);
        assert_eq!(pastes(&out), ["ok thanks"]);
        assert_eq!(presses(&out), 0, "held while the paste may go on");
        assert!(burst.expired(t1 + ms(80)).is_empty(), "the window starts when the loop waits");
        burst.waiting(t1 + ms(80));
        assert!(burst.expired(t1 + ms(100)).is_empty());
        let late = burst.expired(t1 + ms(140));
        assert_eq!(presses(&late), 1, "the Enter sends after all");
        assert!(burst.expired(t1 + ms(300)).is_empty(), "once");

        // The next read finds it due first.
        busy(&mut burst, t0, t0 + ms(600));
        burst.coalesce(console_keys("ok thanks\r"), t1);
        let next = burst.coalesce(console_keys("x"), t1 + ms(200));
        assert!(matches!(&next[0], Event::Key(k) if k.code == KeyCode::Enter && k.kind == KeyEventKind::Press), "{next:?}");
        assert_eq!(presses(&next), 2);

        // Followed by another event in the same read: no paste can go on.
        let mut keys = console_keys("ok thanks\r");
        keys.push(press(KeyCode::Up, KeyModifiers::NONE));
        let mut burst = PasteBurst::default();
        busy(&mut burst, t0, t0 + ms(600));
        let out = burst.coalesce(keys, t1);
        assert_eq!(pastes(&out), ["ok thanks"]);
        assert_eq!(presses(&out), 2, "Enter, then Up");

        // A multi-line paste keeps its trailing break as a held line break.
        let mut burst = PasteBurst::default();
        busy(&mut burst, t0, t0 + ms(600));
        burst.coalesce(console_keys("first line\rsecond\r"), t1);
        burst.waiting(t1);
        assert!(burst.expired(t1 + ms(500)).is_empty());
    }

    #[test]
    fn a_typed_line_split_at_its_capital_still_sends() {
        let t0 = Instant::now();
        let ms = Duration::from_millis;
        let mut burst = PasteBurst::default();
        busy(&mut burst, t0, t0 + ms(600));
        let out = burst.read(console_keys("O"), t0 + ms(601));
        assert_eq!(presses(&out), 1);
        burst.woke(t0 + ms(605));
        burst.waiting(t0 + ms(608));
        let out = burst.read(console_keys("k thanks\r"), t0 + ms(608));
        assert_eq!(pastes(&out), ["k thanks"]);
        burst.woke(t0 + ms(610));
        burst.waiting(t0 + ms(615));
        let late = burst.expired(t0 + ms(700));
        assert_eq!(presses(&late), 1, "the Enter sends");

        // Pasted into an idle loop, the same split keeps the draft.
        let mut burst = PasteBurst::default();
        busy(&mut burst, t0, t0 + ms(3));
        burst.read(console_keys("H"), t0 + ms(30));
        burst.woke(t0 + ms(32));
        burst.waiting(t0 + ms(35));
        burst.read(console_keys("ello world\r"), t0 + ms(35));
        burst.woke(t0 + ms(37));
        burst.waiting(t0 + ms(40));
        assert!(burst.expired(t0 + ms(200)).is_empty());

        // A wakeup in between ends the backlog.
        let mut burst = PasteBurst::default();
        busy(&mut burst, t0, t0 + ms(600));
        burst.read(console_keys("O"), t0 + ms(601));
        burst.waiting(t0 + ms(608));
        busy(&mut burst, t0 + ms(700), t0 + ms(705));
        burst.read(console_keys("k thanks\r"), t0 + ms(705));
        burst.waiting(t0 + ms(710));
        assert!(burst.expired(t0 + ms(900)).is_empty());
    }

    #[test]
    fn a_line_pasted_into_an_idle_loop_does_not_send() {
        let t0 = Instant::now();
        let ms = Duration::from_millis;
        for line in ["git status\r", "src/tui/input/burst.rs\r", "first line\r"] {
            let mut burst = PasteBurst::default();
            busy(&mut burst, t0, t0 + ms(3));
            let out = burst.coalesce(console_keys(line), t0 + ms(30));
            assert_eq!(pastes(&out), [line.trim_end()]);
            assert_eq!(presses(&out), 0, "{line:?}");
            burst.waiting(t0 + ms(40));
            assert!(burst.expired(t0 + ms(200)).is_empty(), "{line:?} must stay a draft");
        }
        // After a slow frame too, unless it was already there when the loop waited.
        let mut burst = PasteBurst::default();
        busy(&mut burst, t0, t0 + ms(600));
        burst.coalesce(console_keys("git status\r"), t0 + ms(630));
        burst.waiting(t0 + ms(640));
        assert!(burst.expired(t0 + ms(800)).is_empty());
    }

    #[test]
    fn a_typed_lines_enter_survives_the_next_read() {
        let t0 = Instant::now();
        let ms = Duration::from_millis;
        let moved = Event::Mouse(MouseEvent { kind: MouseEventKind::Moved, column: 3, row: 4, modifiers: KeyModifiers::NONE });
        for other in [moved, Event::FocusLost, Event::Resize(80, 24), press(KeyCode::Left, KeyModifiers::NONE)] {
            let mut burst = PasteBurst::default();
            busy(&mut burst, t0, t0 + ms(600));
            burst.coalesce(console_keys("ok thanks\r"), t0 + ms(601));
            burst.waiting(t0 + ms(620));
            let out = burst.coalesce(vec![other.clone()], t0 + ms(640));
            assert_eq!(presses(&out[..1]), 1, "the Enter first: {out:?}");
            assert!(matches!(&out[0], Event::Key(k) if k.code == KeyCode::Enter));
            assert_eq!(out[1], other);
            burst.waiting(t0 + ms(650));
            assert!(burst.expired(t0 + ms(900)).is_empty(), "once");
        }
        // Due by the time an IME commit arrives: the commit's Enter rule spares it.
        let mut burst = PasteBurst::default();
        busy(&mut burst, t0, t0 + ms(600));
        burst.coalesce(console_keys("ok thanks\r"), t0 + ms(601));
        burst.waiting(t0 + ms(620));
        let out = burst.read(vec![press(KeyCode::Char('好'), KeyModifiers::NONE)], t0 + ms(690));
        assert!(matches!(&out[0], Event::Key(k) if k.code == KeyCode::Enter), "{out:?}");
        assert_eq!(presses(&out), 2);
    }

    #[test]
    fn a_read_of_releases_alone_does_not_end_the_paste() {
        let t0 = Instant::now();
        let ms = Duration::from_millis;
        let mut burst = PasteBurst::default();
        let mut first = console_keys("line one\r");
        let enter_up = first.pop().unwrap();
        burst.coalesce(first, t0);
        burst.waiting(t0 + ms(2));
        assert_eq!(burst.coalesce(vec![enter_up.clone()], t0 + ms(4)), [enter_up]);
        burst.waiting(t0 + ms(6));
        let rest = burst.coalesce(console_keys("ok\r"), t0 + ms(8));
        assert_eq!(pastes(&rest), ["\nok"]);
        assert_eq!(presses(&rest), 0, "the short tail's Enter does not send");
    }

    #[test]
    fn a_paste_split_over_reads_continues_soon_after() {
        let t0 = Instant::now();
        let mut burst = PasteBurst::default();
        let first = burst.coalesce(console_keys("line one\r"), t0);
        assert_eq!(pastes(&first), ["line one"]);
        let rest = burst.coalesce(console_keys("ab"), t0 + Duration::from_millis(5));
        assert_eq!(pastes(&rest), ["\nab"], "the held line break and a short tail join the paste");

        let mut split = console_keys("line one\r");
        let enter_up = split.pop().unwrap();
        burst.coalesce(split, t0);
        let mut tail = vec![enter_up];
        tail.extend(console_keys("ab"));
        let rest = burst.coalesce(tail, t0 + Duration::from_millis(5));
        assert_eq!(pastes(&rest), ["\nab"], "a release opening the read does not end the paste");

        burst.coalesce(console_keys("line one\r"), t0);
        let later = burst.coalesce(console_keys("ab"), t0 + Duration::from_millis(500));
        assert_eq!(presses(&later), 2, "typing later is typing");
    }

    #[test]
    fn the_window_opens_when_the_loop_waits_again() {
        let t0 = Instant::now();
        let ms = Duration::from_millis;
        let mut burst = PasteBurst::default();
        burst.coalesce(console_keys("line one\r"), t0);
        // Handling the read and painting a big console took 80 ms.
        burst.waiting(t0 + ms(80));
        let rest = burst.coalesce(console_keys("ok\r"), t0 + ms(90));
        assert_eq!(pastes(&rest), ["\nok"], "the line break survives a slow frame");
        assert_eq!(presses(&rest), 0, "the short tail's Enter does not send");

        burst.coalesce(console_keys("line one\r"), t0);
        burst.waiting(t0 + ms(10));
        // Later wakeups (streaming output, the frame timer) do not extend it.
        burst.waiting(t0 + ms(55));
        let later = burst.coalesce(console_keys("ab"), t0 + ms(100));
        assert_eq!(presses(&later), 2, "typing later is typing");
    }
}
