//! Windows consoles hand a paste or a dropped file path to the app as plain
//! key presses: crossterm emits no `Event::Paste` there. A run of text keys
//! that arrives in one read, faster than anyone types, is turned back into a
//! paste, so its line breaks do not send and dropped paths can attach.

use std::time::{Duration, Instant};

use crossterm::event::{Event, KeyCode, KeyEventKind, KeyModifiers};

/// Text keys in one read that make a paste. Typing delivers a key or two per
/// read even during a slow frame; an IME commit of a phrase pastes the same
/// text anyway.
const MIN_KEYS: usize = 6;
/// A console may deliver a long paste over several reads: a text run that
/// opens a read this soon after a paste continues it.
const CONTINUE: Duration = Duration::from_millis(50);

#[derive(Default)]
pub(crate) struct PasteBurst {
    /// When the last read ended inside a paste, and the line breaks it held
    /// back: trailing ones are dropped (an IME's confirming Enter), unless
    /// the paste goes on in the next read.
    open: Option<(Instant, usize)>,
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
        let mut cont = self
            .open
            .take()
            .filter(|(at, _)| now.saturating_duration_since(*at) <= CONTINUE)
            .map(|(_, held)| held);
        let mut out = Vec::with_capacity(batch.len());
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
                self.finish(std::mem::take(&mut run), &mut cont, &mut out, None);
                cont = None;
                lead = false;
                out.push(ev);
                continue;
            };
            if run.raw.is_empty() {
                // A run starts with a character, unless it goes on with a paste.
                if !(cont.is_some() && lead) && !matches!(key, TextKey::Char(_)) {
                    cont = None;
                    lead = false;
                    out.push(ev);
                    continue;
                }
                run.first = lead;
            }
            run.push(ev, key);
        }
        self.finish(run, &mut cont, &mut out, Some(now));
        out
    }

    /// Emit a run as a paste or as its own keys. `end` = the run ends the read.
    fn finish(&mut self, run: Run, cont: &mut Option<usize>, out: &mut Vec<Event>, end: Option<Instant>) {
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
        text.truncate(body);
        if let Some(at) = end {
            self.open = Some((at, trailing));
        }
        if !text.is_empty() {
            out.push(Event::Paste(text));
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
    }

    #[test]
    fn typing_and_shortcuts_stay_keys() {
        let mut keys = console_keys("ok\r");
        let out = PasteBurst::default().coalesce(keys.clone(), Instant::now());
        assert_eq!(out, keys, "a short run sends with its Enter");
        keys = vec![press(KeyCode::Enter, KeyModifiers::NONE)];
        keys.extend(console_keys("next message"));
        let out = PasteBurst::default().coalesce(keys, Instant::now());
        assert!(matches!(&out[0], Event::Key(k) if k.code == KeyCode::Enter), "a leading Enter still sends");
        assert_eq!(pastes(&out), ["next message"]);
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
}
