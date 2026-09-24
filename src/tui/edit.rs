#[derive(Clone, Default, Serialize, Deserialize)]
struct Edit {
    text: String,
    /// Character index (not bytes).
    caret: usize,
    /// Selection anchor; `None` or equal to caret means no selection.
    anchor: Option<usize>,
}

impl Edit {
    fn at_end(text: String) -> Self {
        let caret = text.chars().count();
        Self {
            text,
            caret,
            anchor: None,
        }
    }

    fn len(&self) -> usize {
        self.text.chars().count()
    }

    fn is_empty(&self) -> bool {
        self.text.is_empty()
    }

    fn clamp(&mut self) {
        let n = self.len();
        self.caret = self.caret.min(n);
        if let Some(a) = self.anchor {
            self.anchor = Some(a.min(n));
            if self.anchor == Some(self.caret) {
                self.anchor = None;
            }
        }
    }

    fn has_sel(&self) -> bool {
        self.anchor.map(|a| a != self.caret).unwrap_or(false)
    }

    fn sel_range(&self) -> Option<(usize, usize)> {
        let a = self.anchor?;
        if a == self.caret {
            None
        } else {
            Some((a.min(self.caret), a.max(self.caret)))
        }
    }

    fn selected_text(&self) -> Option<String> {
        let (lo, hi) = self.sel_range()?;
        Some(self.text.chars().skip(lo).take(hi - lo).collect())
    }

    fn clear_sel(&mut self) {
        self.anchor = None;
    }

    fn begin_select(&mut self, select: bool) {
        if select {
            if self.anchor.is_none() {
                self.anchor = Some(self.caret);
            }
        } else {
            self.clear_sel();
        }
    }

    fn move_left(&mut self, select: bool) {
        self.clamp();
        if !select {
            if let Some((lo, _)) = self.sel_range() {
                self.caret = lo;
                self.clear_sel();
                return;
            }
        }
        self.begin_select(select);
        if self.caret > 0 {
            self.caret -= 1;
        }
        self.clamp();
    }

    fn move_right(&mut self, select: bool) {
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

    fn home(&mut self, select: bool) {
        if !select {
            self.clear_sel();
        } else {
            self.begin_select(true);
        }
        self.caret = 0;
        self.clamp();
    }

    fn end(&mut self, select: bool) {
        if !select {
            self.clear_sel();
        } else {
            self.begin_select(true);
        }
        self.caret = self.len();
        self.clamp();
    }

    fn select_all(&mut self) {
        self.anchor = Some(0);
        self.caret = self.len();
        if self.caret == 0 {
            self.anchor = None;
        }
    }

    fn click(&mut self, idx: usize, select: bool) {
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
        let byte_lo = char_to_byte(&self.text, lo);
        let byte_hi = char_to_byte(&self.text, hi);
        self.text.replace_range(byte_lo..byte_hi, "");
        self.caret = lo;
        self.clear_sel();
        true
    }

    fn insert_char(&mut self, c: char) {
        self.delete_sel();
        let i = char_to_byte(&self.text, self.caret);
        self.text.insert(i, c);
        self.caret += 1;
    }

    fn insert_str(&mut self, s: &str) {
        self.delete_sel();
        let i = char_to_byte(&self.text, self.caret);
        self.text.insert_str(i, s);
        self.caret += s.chars().count();
    }

    fn backspace(&mut self) {
        if self.delete_sel() {
            return;
        }
        if self.caret == 0 {
            return;
        }
        self.caret -= 1;
        let i = char_to_byte(&self.text, self.caret);
        let j = char_to_byte(&self.text, self.caret + 1);
        self.text.replace_range(i..j, "");
    }

    fn delete_forward(&mut self) {
        if self.delete_sel() {
            return;
        }
        if self.caret >= self.len() {
            return;
        }
        let i = char_to_byte(&self.text, self.caret);
        let j = char_to_byte(&self.text, self.caret + 1);
        self.text.replace_range(i..j, "");
    }

    fn clear(&mut self) {
        self.text.clear();
        self.caret = 0;
        self.anchor = None;
    }

    fn move_visual(&mut self, width: u16, delta_row: i16, select: bool) {
        self.clamp();
        if !select {
            if let Some((lo, hi)) = self.sel_range() {
                self.caret = if delta_row < 0 { lo } else { hi };
                self.clear_sel();
                return;
            }
        }
        self.begin_select(select);
        let width = width.max(1);
        let ranges = wrap_lines(&self.text, width);
        let (row, col) = caret_row_col(&self.text, &ranges, self.caret);
        let dest = if delta_row < 0 {
            row.saturating_sub(1)
        } else {
            row.saturating_add(1).min(ranges.len().saturating_sub(1) as u16)
        };
        self.caret = index_at_line(&self.text, &ranges, dest, col);
        self.clamp();
    }
}

fn char_to_byte(s: &str, idx: usize) -> usize {
    s.char_indices()
        .nth(idx)
        .map(|(i, _)| i)
        .unwrap_or(s.len())
}

fn wrap_lines(s: &str, width: u16) -> Vec<(usize, usize)> {
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

fn caret_row_col(s: &str, ranges: &[(usize, usize)], caret: usize) -> (u16, u16) {
    let n = s.chars().count();
    for (row, &(a, b)) in ranges.iter().enumerate() {
        if caret < b || (caret == b && (b == n || row + 1 == ranges.len())) {
            let prefix: String = s.chars().skip(a).take(caret.saturating_sub(a)).collect();
            return (row as u16, Line::from(prefix.as_str()).width() as u16);
        }
    }
    let last = ranges.len().saturating_sub(1) as u16;
    (last, 0)
}

fn index_at_width(chars: &[char], target: u16) -> usize {
    let mut col = 0u16;
    for (i, &c) in chars.iter().enumerate() {
        let w = ch_width(c).max(1);
        if target < col.saturating_add(w) {
            if target.saturating_sub(col) < (w + 1) / 2 {
                return i;
            }
            return i + 1;
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

fn click_to_index(s: &str, inner: Rect, vscroll: u16, col: u16, row: u16) -> usize {
    let width = inner.width.max(1);
    let ranges = wrap_lines(s, width);
    let rel_row = row.saturating_sub(inner.y).saturating_add(vscroll) as usize;
    let rel_col = col.saturating_sub(inner.x);
    if rel_row >= ranges.len() {
        return s.chars().count();
    }
    index_at_line(s, &ranges, rel_row as u16, rel_col)
}

fn clipboard_set(s: &str) -> bool {
    arboard::Clipboard::new()
        .ok()
        .and_then(|mut c| c.set_text(s.to_string()).ok())
        .is_some()
}

fn clipboard_get() -> Option<String> {
    arboard::Clipboard::new().ok()?.get_text().ok()
}

fn is_paste_key(code: KeyCode, mods: KeyModifiers) -> bool {
    match code {
        KeyCode::Char('v' | 'V') if mods.contains(KeyModifiers::CONTROL) => true,
        KeyCode::Char('\u{16}') => true,
        KeyCode::Insert if mods.contains(KeyModifiers::SHIFT) => true,
        _ => false,
    }
}

fn clipboard_set_image(path: &std::path::Path) -> bool {
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

fn picture_from_tool(name: &str, output: &str) -> Option<(String, String)> {
    let v: Value = serde_json::from_str(output).ok()?;
    if v.get("attach_image").and_then(Value::as_bool) != Some(true) {
        return None;
    }
    let path = v.get("path").and_then(Value::as_str)?.to_string();
    if path.is_empty() {
        return None;
    }
    Some((path.clone(), format!("模型在看  {name}  {path}")))
}

fn hit_at(hits: &[(Rect, Hit)], col: u16, row: u16) -> Option<Hit> {
    let p = Position::new(col, row);
    hits.iter()
        .rev()
        .find(|(r, _)| r.contains(p))
        .map(|(_, h)| *h)
}

fn chat_pos_at(glyphs: &[ChatGlyphLine], col: u16, row: u16) -> Option<ChatPos> {
    if glyphs.is_empty() {
        return None;
    }
    let line = glyphs
        .iter()
        .find(|g| g.y == row)
        .or_else(|| glyphs.iter().min_by_key(|g| g.y.abs_diff(row)))?;
    let rel = col.saturating_sub(line.x);
    let idx = if line.chars.is_empty() || rel >= line.text_w {
        line.start + line.chars.len()
    } else {
        line.start + index_at_width(&line.chars, rel)
    };
    Some(ChatPos {
        row: line.row,
        idx,
    })
}
