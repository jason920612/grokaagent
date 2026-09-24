//! Single text field: caret and selection by character index, soft wrapping
//! by display width (CJK-aware), plus clipboard helpers.

use crossterm::event::{KeyCode, KeyModifiers};
use ratatui::layout::Rect;
use ratatui::text::Line;
use serde::{Deserialize, Serialize};

/// Display width of one char in terminal cells.
pub(crate) fn ch_width(c: char) -> u16 {
    Line::from(c.to_string()).width() as u16
}

/// Display width of a string in terminal cells.
pub(crate) fn display_cols(s: &str) -> u16 {
    Line::from(s).width() as u16
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub(crate) struct Edit {
    pub text: String,
    /// Character index (not bytes).
    pub caret: usize,
    /// Selection anchor; `None` or equal to caret means no selection.
    pub anchor: Option<usize>,
}

impl Edit {
    pub fn at_end(text: impl Into<String>) -> Self {
        let text = text.into();
        let caret = text.chars().count();
        Self {
            text,
            caret,
            anchor: None,
        }
    }

    pub fn len(&self) -> usize {
        self.text.chars().count()
    }

    pub fn is_empty(&self) -> bool {
        self.text.is_empty()
    }

    pub fn clamp(&mut self) {
        let n = self.len();
        self.caret = self.caret.min(n);
        if let Some(a) = self.anchor {
            self.anchor = Some(a.min(n));
            if self.anchor == Some(self.caret) {
                self.anchor = None;
            }
        }
    }

    pub fn has_sel(&self) -> bool {
        self.sel_range().is_some()
    }

    pub fn sel_range(&self) -> Option<(usize, usize)> {
        let a = self.anchor?;
        (a != self.caret).then(|| (a.min(self.caret), a.max(self.caret)))
    }

    pub fn selected_text(&self) -> Option<String> {
        let (lo, hi) = self.sel_range()?;
        Some(self.text.chars().skip(lo).take(hi - lo).collect())
    }

    pub fn clear_sel(&mut self) {
        self.anchor = None;
    }

    fn begin_select(&mut self, select: bool) {
        if select {
            self.anchor.get_or_insert(self.caret);
        } else {
            self.clear_sel();
        }
    }

    pub fn move_left(&mut self, select: bool) {
        self.clamp();
        if !select {
            if let Some((lo, _)) = self.sel_range() {
                self.caret = lo;
                self.clear_sel();
                return;
            }
        }
        self.begin_select(select);
        self.caret = self.caret.saturating_sub(1);
        self.clamp();
    }

    pub fn move_right(&mut self, select: bool) {
        self.clamp();
        if !select {
            if let Some((_, hi)) = self.sel_range() {
                self.caret = hi;
                self.clear_sel();
                return;
            }
        }
        self.begin_select(select);
        if self.caret < self.len() {
            self.caret += 1;
        }
        self.clamp();
    }

    pub fn home(&mut self, select: bool) {
        self.begin_select(select);
        self.caret = 0;
        self.clamp();
    }

    pub fn end(&mut self, select: bool) {
        self.begin_select(select);
        self.caret = self.len();
        self.clamp();
    }

    pub fn select_all(&mut self) {
        self.caret = self.len();
        self.anchor = (self.caret > 0).then_some(0);
    }

    pub fn click(&mut self, idx: usize, select: bool) {
        self.begin_select(select);
        self.caret = idx;
        self.clamp();
        if !select {
            self.clear_sel();
        }
    }

    fn delete_sel(&mut self) -> bool {
        let Some((lo, hi)) = self.sel_range() else {
            return false;
        };
        let a = char_to_byte(&self.text, lo);
        let b = char_to_byte(&self.text, hi);
        self.text.replace_range(a..b, "");
        self.caret = lo;
        self.clear_sel();
        true
    }

    pub fn insert_char(&mut self, c: char) {
        self.delete_sel();
        let i = char_to_byte(&self.text, self.caret);
        self.text.insert(i, c);
        self.caret += 1;
    }

    pub fn insert_str(&mut self, s: &str) {
        self.delete_sel();
        let i = char_to_byte(&self.text, self.caret);
        self.text.insert_str(i, s);
        self.caret += s.chars().count();
    }

    pub fn backspace(&mut self) {
        if self.delete_sel() || self.caret == 0 {
            return;
        }
        self.caret -= 1;
        let i = char_to_byte(&self.text, self.caret);
        let j = char_to_byte(&self.text, self.caret + 1);
        self.text.replace_range(i..j, "");
    }

    pub fn delete_forward(&mut self) {
        if self.delete_sel() || self.caret >= self.len() {
            return;
        }
        let i = char_to_byte(&self.text, self.caret);
        let j = char_to_byte(&self.text, self.caret + 1);
        self.text.replace_range(i..j, "");
    }

    pub fn clear(&mut self) {
        self.text.clear();
        self.caret = 0;
        self.anchor = None;
    }

    /// Move the caret one wrapped line up (`-1`) or down (`1`).
    pub fn move_visual(&mut self, width: u16, delta_row: i16, select: bool) {
        self.clamp();
        if !select {
            if let Some((lo, hi)) = self.sel_range() {
                self.caret = if delta_row < 0 { lo } else { hi };
                self.clear_sel();
                return;
            }
        }
        self.begin_select(select);
        let ranges = wrap_lines(&self.text, width.max(1));
        let (row, col) = caret_row_col(&self.text, &ranges, self.caret);
        let dest = if delta_row < 0 {
            row.saturating_sub(1)
        } else {
            row.saturating_add(1).min(ranges.len().saturating_sub(1) as u16)
        };
        self.caret = index_at_line(&self.text, &ranges, dest, col);
        self.clamp();
    }

    /// Apply a plain editing key (single-line unless `multiline`). Returns
    /// `true` if the key was an edit key.
    pub fn apply_key(&mut self, code: KeyCode, mods: KeyModifiers, limit: usize) -> bool {
        let shift = mods.contains(KeyModifiers::SHIFT);
        let ctrl = mods.contains(KeyModifiers::CONTROL);
        match code {
            KeyCode::Left => self.move_left(shift),
            KeyCode::Right => self.move_right(shift),
            KeyCode::Home => self.home(shift),
            KeyCode::End => self.end(shift),
            KeyCode::Backspace => self.backspace(),
            KeyCode::Delete => self.delete_forward(),
            KeyCode::Char('a' | 'A') if ctrl => self.select_all(),
            _ if is_paste_key(code, mods) => {
                if let Some(s) = clipboard_get() {
                    let remain = limit.saturating_sub(self.len());
                    let clipped: String = s.chars().take(remain).collect();
                    self.insert_str(&clipped);
                }
            }
            KeyCode::Char(c) if !ctrl && !c.is_control() => {
                if self.len() < limit {
                    self.insert_char(c);
                }
            }
            _ => return false,
        }
        true
    }
}

pub(crate) fn char_to_byte(s: &str, idx: usize) -> usize {
    s.char_indices().nth(idx).map(|(i, _)| i).unwrap_or(s.len())
}

/// Soft-wrap `s` to `width` cells: (start, end) char ranges per visual line.
pub(crate) fn wrap_lines(s: &str, width: u16) -> Vec<(usize, usize)> {
    let width = width.max(1);
    let n = s.chars().count();
    if n == 0 {
        return vec![(0, 0)];
    }
    let mut lines = Vec::new();
    let mut start = 0;
    let mut col = 0u16;
    for (i, c) in s.chars().enumerate() {
        if c == '\n' {
            lines.push((start, i));
            start = i + 1;
            col = 0;
            continue;
        }
        let w = ch_width(c).max(1);
        if col > 0 && col.saturating_add(w) > width {
            lines.push((start, i));
            start = i;
            col = 0;
        }
        col = col.saturating_add(w);
    }
    lines.push((start, n));
    lines
}

pub(crate) fn caret_row_col(s: &str, ranges: &[(usize, usize)], caret: usize) -> (u16, u16) {
    let n = s.chars().count();
    for (row, &(a, b)) in ranges.iter().enumerate() {
        if caret < b || (caret == b && (b == n || row + 1 == ranges.len())) {
            let prefix: String = s.chars().skip(a).take(caret.saturating_sub(a)).collect();
            return (row as u16, display_cols(&prefix));
        }
    }
    (ranges.len().saturating_sub(1) as u16, 0)
}

/// Char index within `chars` at display column `target` (rounding to nearest).
pub(crate) fn index_at_width(chars: &[char], target: u16) -> usize {
    let mut col = 0u16;
    for (i, &c) in chars.iter().enumerate() {
        let w = ch_width(c).max(1);
        if target < col.saturating_add(w) {
            return if target.saturating_sub(col) < (w + 1) / 2 { i } else { i + 1 };
        }
        col = col.saturating_add(w);
    }
    chars.len()
}

fn index_at_line(s: &str, ranges: &[(usize, usize)], row: u16, col: u16) -> usize {
    let chars: Vec<char> = s.chars().collect();
    let Some(&(a, b)) = ranges.get(row as usize) else {
        return chars.len();
    };
    a + index_at_width(&chars[a..b], col)
}

/// Char index under a click at (col, row) in a wrapped field.
pub(crate) fn click_to_index(s: &str, inner: Rect, vscroll: u16, col: u16, row: u16) -> usize {
    let ranges = wrap_lines(s, inner.width.max(1));
    let rel_row = row.saturating_sub(inner.y).saturating_add(vscroll) as usize;
    if rel_row >= ranges.len() {
        return s.chars().count();
    }
    index_at_line(s, &ranges, rel_row as u16, col.saturating_sub(inner.x))
}

pub(crate) fn is_paste_key(code: KeyCode, mods: KeyModifiers) -> bool {
    match code {
        KeyCode::Char('v' | 'V') if mods.contains(KeyModifiers::CONTROL) => true,
        // Some consoles deliver Ctrl+V as SYN.
        KeyCode::Char('\u{16}') => true,
        KeyCode::Insert if mods.contains(KeyModifiers::SHIFT) => true,
        _ => false,
    }
}

pub(crate) fn clipboard_set(s: &str) -> bool {
    arboard::Clipboard::new()
        .ok()
        .and_then(|mut c| c.set_text(s.to_string()).ok())
        .is_some()
}

pub(crate) fn clipboard_get() -> Option<String> {
    arboard::Clipboard::new().ok()?.get_text().ok()
}

pub(crate) fn clipboard_set_image(path: &std::path::Path) -> bool {
    let Ok(img) = image::open(path) else {
        return false;
    };
    let rgba = img.to_rgba8();
    arboard::Clipboard::new()
        .ok()
        .and_then(|mut c| {
            c.set_image(arboard::ImageData {
                width: rgba.width() as usize,
                height: rgba.height() as usize,
                bytes: std::borrow::Cow::Owned(rgba.into_raw()),
            })
            .ok()
        })
        .is_some()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn editing_by_chars_not_bytes() {
        let mut e = Edit::at_end("你好");
        e.move_left(false);
        e.insert_char('x');
        assert_eq!(e.text, "你x好");
        e.backspace();
        e.backspace();
        assert_eq!(e.text, "好");
        e.select_all();
        e.insert_str("ab");
        assert_eq!(e.text, "ab");
        e.home(false);
        e.move_right(true);
        assert_eq!(e.selected_text().as_deref(), Some("a"));
        e.delete_forward();
        assert_eq!(e.text, "b");
    }

    #[test]
    fn wrapping_counts_display_width() {
        assert_eq!(wrap_lines("你好世界", 4), vec![(0, 2), (2, 4)]);
        assert_eq!(wrap_lines("ab\ncd", 10), vec![(0, 2), (3, 5)]);
        let ranges = wrap_lines("你好世界", 4);
        assert_eq!(caret_row_col("你好世界", &ranges, 3), (1, 2));
        let mut e = Edit::at_end("你好世界");
        e.move_visual(4, -1, false);
        assert_eq!(e.caret, 2);
    }

    #[test]
    fn click_maps_to_the_nearest_char() {
        let inner = Rect::new(10, 5, 4, 3);
        assert_eq!(click_to_index("你好世界", inner, 0, 12, 6), 3);
        assert_eq!(click_to_index("你好世界", inner, 0, 10, 9), 4, "below the text = end");
    }

    #[test]
    fn apply_key_respects_the_limit() {
        let mut e = Edit::default();
        for c in "abcdef".chars() {
            e.apply_key(KeyCode::Char(c), KeyModifiers::NONE, 3);
        }
        assert_eq!(e.text, "abc");
        assert!(!e.apply_key(KeyCode::Enter, KeyModifiers::NONE, 3));
    }
}
