struct ChatLine {
    line: Line<'static>,
    hit: Option<Hit>,
    wrap: bool,
    /// When set, this item occupies `height` rows and is painted via a graphics protocol.
    graphic: Option<(String, u16, u16)>,
    /// Selectable text: transcript row + char offset of this logical line.
    select: Option<(usize, usize)>,
    /// Live think-header row: clock ticks patch this line only.
    clock: bool,
}

fn chat_line(line: Line<'static>, hit: Option<Hit>) -> ChatLine {
    ChatLine {
        line,
        hit,
        wrap: true,
        graphic: None,
        select: None,
        clock: false,
    }
}

fn chat_line_sel(line: Line<'static>, hit: Option<Hit>, row: usize, start: usize) -> ChatLine {
    ChatLine {
        line,
        hit,
        wrap: true,
        graphic: None,
        select: Some((row, start)),
        clock: false,
    }
}

fn chat_line_raw(line: Line<'static>, hit: Option<Hit>) -> ChatLine {
    ChatLine {
        line,
        hit,
        wrap: false,
        graphic: None,
        select: None,
        clock: false,
    }
}

fn chat_graphic(rel: String, width: u16, height: u16, hit: Option<Hit>) -> ChatLine {
    ChatLine {
        line: Line::from(""),
        hit,
        wrap: false,
        graphic: Some((rel, width.max(1), height.max(1))),
        select: None,
        clock: false,
    }
}

fn preview_lines(app: &mut App, rel: &str, cols: u16) -> Vec<Line<'static>> {
    let cols = cols.min(crate::preview::MAX_COLS).max(4);
    if let Some(v) = app.preview.get(&(rel.to_string(), cols)) {
        return v.clone();
    }
    let abs = app.session.workspace.join(rel);
    let lines = crate::preview::from_path(&abs, cols, crate::preview::MAX_ROWS);
    app.preview.insert((rel.to_string(), cols), lines.clone());
    lines
}

fn push_image_block(
    app: &mut App,
    out: &mut Vec<ChatLine>,
    rel: &str,
    caption: String,
    cols: u16,
    selected: bool,
) {
    let idx = app.image_hits.len() as u16;
    app.image_hits.push(rel.to_string());
    let hit = Some(Hit::ChatImage(idx));
    if !caption.is_empty() {
        let cap_style = if selected {
            Style::default().fg(Color::Black).bg(ACCENT)
        } else {
            Style::default().fg(DIM)
        };
        out.push(chat_line(
            Line::from(Span::styled(format!("      {caption}"), cap_style)),
            hit,
        ));
    }
    let max_cols = cols.saturating_sub(crate::preview::INDENT);
    let max_rows = crate::preview::MAX_ROWS;
    let uses_gfx = app
        .picker
        .as_ref()
        .is_some_and(crate::preview::uses_graphics);
    if uses_gfx {
        let key = (rel.to_string(), max_cols, max_rows);
        let (w, h) = if let Some(&sz) = app.image_cells.get(&key) {
            sz
        } else {
            let abs = app.session.workspace.join(rel);
            let sz = {
                let picker = app.picker.as_ref().expect("graphics picker");
                crate::preview::cell_size_for(picker, &abs, max_cols, max_rows)
            };
            app.image_cells.insert(key, sz);
            sz
        };
        out.push(chat_graphic(rel.to_string(), w, h, hit));
        return;
    }
    for line in preview_lines(app, rel, max_cols) {
        let mut padded = vec![Span::raw("      ")];
        padded.extend(line.spans);
        out.push(chat_line_raw(Line::from(padded), hit));
    }
}

fn chat_logical_rows(app: &mut App, cols: u16) -> Vec<ChatLine> {
    app.image_hits.clear();
    let rows = app.rows.clone();
    let sel = app.chat_sel.clone();
    let open_tool = app.open_tool;
    let mut out = Vec::new();
    for (ri, row) in rows.iter().enumerate() {
        match row {
            Row::Tools(g) => {
                out.push(chat_line(group_header_line(g), Some(Hit::ToolGroup(ri))));
                if g.expanded {
                    for (ci, call) in g.calls.iter().enumerate() {
                        let selected = open_tool == Some((ri, ci));
                        out.push(chat_line(
                            call_row_line(call, selected),
                            Some(Hit::ToolItem(ri, ci)),
                        ));
                        for line in call_body_lines(call) {
                            out.push(chat_line(line, Some(Hit::ToolItem(ri, ci))));
                        }
                    }
                }
            }
            Row::Think(t) => {
                let mut header = chat_line_raw(think_header_line(t), Some(Hit::Think(ri)));
                header.clock = !t.done;
                out.push(header);
                if t.expanded {
                    for line in think_body_lines(t) {
                        out.push(chat_line(line, Some(Hit::Think(ri))));
                    }
                }
            }
            Row::User(u) => {
                push_selectable(
                    &mut out,
                    prefixed_text("you   ", USER, &u.text),
                    ri,
                );
                for img in &u.images {
                    let img_sel = matches!(&sel, ChatSel::Image(p) if p == img);
                    push_image_block(app, &mut out, img, format!("圖片  {img}"), cols, img_sel);
                }
            }
            Row::Picture { path, label } => {
                push_selectable(
                    &mut out,
                    vec![Line::from(Span::styled(
                        label.clone(),
                        Style::default().fg(DIM),
                    ))],
                    ri,
                );
                let img_sel = matches!(&sel, ChatSel::Image(p) if p == path);
                push_image_block(app, &mut out, path, String::new(), cols, img_sel);
            }
            other => {
                push_selectable(&mut out, row_lines(other), ri);
            }
        }
    }
    out
}

fn push_selectable(out: &mut Vec<ChatLine>, lines: Vec<Line<'static>>, ri: usize) {
    let hit = Some(Hit::ChatRow(ri as u16));
    let mut off = 0usize;
    for line in lines {
        let n = selectable_line_text(&line).chars().count();
        out.push(chat_line_sel(line, hit, ri, off));
        off = off.saturating_add(n).saturating_add(1);
    }
}

fn row_sel_lines(row: &Row) -> Option<Vec<Line<'static>>> {
    match row {
        Row::User(u) => Some(prefixed_text("you   ", USER, &u.text)),
        Row::Agent(a) => Some(agent_lines(a)),
        Row::Meta(s) => Some(vec![Line::from(Span::styled(
            s.clone(),
            Style::default().fg(DIM),
        ))]),
        Row::Err(s) => Some(vec![Line::from(Span::styled(
            s.clone(),
            Style::default().fg(WARN),
        ))]),
        Row::Picture { label, .. } => Some(vec![Line::from(Span::styled(
            label.clone(),
            Style::default().fg(DIM),
        ))]),
        Row::Tools(_) | Row::Think(_) => None,
    }
}

fn selectable_text(row: &Row) -> Option<String> {
    let lines = row_sel_lines(row)?;
    Some(
        lines
            .iter()
            .map(selectable_line_text)
            .collect::<Vec<_>>()
            .join("\n"),
    )
}

fn chat_selected_text(rows: &[Row], sel: &ChatSel) -> Option<String> {
    let (lo, hi) = sel.text_range()?;
    if lo.row >= rows.len() {
        return None;
    }
    if lo.row == hi.row {
        let s = selectable_text(&rows[lo.row])?;
        let n = s.chars().count();
        let a = lo.idx.min(n);
        let b = hi.idx.min(n);
        if a >= b {
            return None;
        }
        return Some(s.chars().skip(a).take(b - a).collect());
    }
    let mut parts = Vec::new();
    for ri in lo.row..=hi.row.min(rows.len().saturating_sub(1)) {
        let Some(s) = selectable_text(&rows[ri]) else {
            continue;
        };
        let n = s.chars().count();
        let a = if ri == lo.row { lo.idx.min(n) } else { 0 };
        let b = if ri == hi.row { hi.idx.min(n) } else { n };
        if a < b {
            parts.push(s.chars().skip(a).take(b - a).collect::<String>());
        }
    }
    if parts.is_empty() {
        None
    } else {
        Some(parts.join("\n"))
    }
}

fn tint_char_range(line: Line<'static>, lo: usize, hi: usize) -> Line<'static> {
    if lo >= hi {
        return line;
    }
    let mut spans = Vec::new();
    let mut i = 0usize;
    for span in line.spans {
        let style = span.style;
        let mut buf = String::new();
        let mut buf_style = style;
        for c in span.content.chars() {
            let mut st = style;
            if i >= lo && i < hi {
                st.bg = Some(Color::Rgb(48, 64, 88));
            }
            if !buf.is_empty() && st != buf_style {
                spans.push(Span::styled(std::mem::take(&mut buf), buf_style));
            }
            buf.push(c);
            buf_style = st;
            i += 1;
        }
        if !buf.is_empty() {
            spans.push(Span::styled(buf, buf_style));
        }
    }
    Line::from(spans)
}

fn piece_tint_range(sel: &ChatSel, row: usize, start: usize, n: usize) -> Option<(usize, usize)> {
    let (lo, hi) = sel.text_range()?;
    if row < lo.row || row > hi.row {
        return None;
    }
    let a = if row == lo.row { lo.idx } else { 0 };
    let b = if row == hi.row { hi.idx } else { usize::MAX };
    let piece_hi = start.saturating_add(n);
    let a = a.max(start);
    let b = b.min(piece_hi);
    if a >= b {
        None
    } else {
        Some((a - start, b - start))
    }
}

fn group_header_line(g: &ToolGroup) -> Line<'static> {
    let arrow = if g.expanded { "▾" } else { "▸" };
    let n = g.calls.len();
    let mut names = Vec::new();
    for c in &g.calls {
        if !names.iter().any(|n| n == &c.name) {
            names.push(c.name.clone());
        }
    }
    let names = names.join(" · ");
    let status = if let Some(c) = g.calls.iter().rev().find(|c| !c.done) {
        if c.phase.is_empty() {
            "執行中"
        } else {
            c.phase.as_str()
        }
    } else if g.calls.iter().any(|c| c.phase == "失敗") {
        "有操作失敗"
    } else if g.calls.iter().any(|c| c.phase == "已停止") {
        "已停止"
    } else {
        "完成"
    };
    let label = if n <= 1 {
        format!("{arrow} {names}    {status}")
    } else {
        format!("{arrow} {n} 個工具 · {names}    {status}")
    };
    Line::from(Span::styled(label, Style::default().fg(TOOL)))
}

fn think_header_line(t: &Think) -> Line<'static> {
    let arrow = if t.expanded { "▾" } else { "▸" };
    let ms = if t.done {
        t.elapsed_ms
    } else {
        t.started
            .map(|s| s.elapsed().as_millis() as u64)
            .unwrap_or(t.elapsed_ms)
    };
    let clock = md::fmt_duration_field(ms);
    let status = if t.done { "完成" } else { "進行中" };
    Line::from(Span::styled(
        format!("{arrow} 思考  {clock}    {status}"),
        Style::default().fg(THINK),
    ))
}

fn think_body_lines(t: &Think) -> Vec<Line<'static>> {
    const MAX: usize = 120;
    let mut lines = Vec::new();
    for (i, part) in t.text.split('\n').enumerate() {
        if i >= MAX {
            let rest = t.text.lines().count().saturating_sub(MAX);
            lines.push(Line::from(Span::styled(
                format!("  … ({rest} 行省略)"),
                Style::default().fg(DIM),
            )));
            break;
        }
        lines.push(Line::from(Span::styled(
            format!("  {part}"),
            Style::default().fg(DIM),
        )));
    }
    if lines.is_empty() {
        lines.push(Line::from(Span::styled(
            "  ",
            Style::default().fg(DIM),
        )));
    }
    lines
}

fn call_row_line(c: &ToolCall, selected: bool) -> Line<'static> {
    let inner = if c.phase == "失敗" || c.phase == "已停止" {
        format!("! {}  {}", c.name, c.phase)
    } else if c.done {
        tool_finished_line(&c.name, &c.output)
    } else if !c.phase.is_empty() {
        format!("▸ {}  {}", c.name, c.phase)
    } else {
        tool_started_line(&c.name, &c.args)
    };
    let style = if selected {
        Style::default().fg(Color::Black).bg(ACCENT)
    } else {
        Style::default().fg(TOOL)
    };
    Line::from(Span::styled(format!("  {inner}"), style))
}

fn call_body_lines(c: &ToolCall) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    if c.name == "run_command" {
        if let Ok(v) = serde_json::from_str::<Value>(&c.output) {
            if let Some(stdout) = v.get("stdout").and_then(Value::as_str) {
                lines.extend(colored_output_lines(stdout, 60));
            }
            if let Some(stderr) = v.get("stderr").and_then(Value::as_str) {
                if !stderr.trim().is_empty() {
                    lines.extend(colored_output_lines(stderr, 20));
                }
            }
        }
    }
    for f in &c.files {
        lines.push(Line::from(Span::styled(
            format!("    ● {}  {}", f.kind, f.path),
            Style::default().fg(ACCENT),
        )));
        lines.extend(diff_lines(&f.diff));
    }
    lines
}

fn call_detail_lines(c: &ToolCall) -> Vec<Line<'static>> {
    let mut lines = vec![Line::from(Span::styled(
        format!(" {}", c.name),
        Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
    ))];
    match c.name.as_str() {
        "run_command" => {
            let cmd = c.args.get("command").and_then(Value::as_str).unwrap_or("");
            lines.push(Line::from(Span::styled(
                format!(" $ {cmd}"),
                Style::default().fg(TEXT),
            )));
            if let Ok(v) = serde_json::from_str::<Value>(&c.output) {
                if let Some(code) = v.get("exit_code") {
                    lines.push(Line::from(Span::styled(
                        format!(" exit {code}"),
                        Style::default().fg(DIM),
                    )));
                }
                if let Some(stdout) = v.get("stdout").and_then(Value::as_str) {
                    if !stdout.trim().is_empty() {
                        lines.push(Line::from(Span::styled(
                            " stdout",
                            Style::default().fg(DIM),
                        )));
                        lines.extend(colored_output_lines(stdout, 80));
                    }
                }
                if let Some(stderr) = v.get("stderr").and_then(Value::as_str) {
                    if !stderr.trim().is_empty() {
                        lines.push(Line::from(Span::styled(
                            " stderr",
                            Style::default().fg(WARN),
                        )));
                        lines.extend(colored_output_lines(stderr, 40));
                    }
                }
            } else if !c.output.is_empty() {
                lines.extend(colored_output_lines(&c.output, 80));
            }
        }
        "list_dir" => {
            let path = c.args.get("path").and_then(Value::as_str).unwrap_or(".");
            lines.push(Line::from(Span::styled(
                format!(" {path}"),
                Style::default().fg(DIM),
            )));
            if let Ok(v) = serde_json::from_str::<Value>(&c.output) {
                if let Some(entries) = v.get("entries").and_then(Value::as_array) {
                    for e in entries.iter().take(80) {
                        let name = e.get("name").and_then(Value::as_str).unwrap_or("");
                        let dir = e.get("dir").and_then(Value::as_bool).unwrap_or(false);
                        let mark = if dir { "/" } else { "" };
                        lines.push(Line::from(Span::styled(
                            format!("  {name}{mark}"),
                            Style::default().fg(TEXT),
                        )));
                    }
                }
            }
        }
        _ => {
            if let Some(path) = c.args.get("path").and_then(Value::as_str) {
                lines.push(Line::from(Span::styled(
                    format!(" {path}"),
                    Style::default().fg(DIM),
                )));
            } else if let Some(q) = c.args.get("query").and_then(Value::as_str) {
                lines.push(Line::from(Span::styled(
                    format!(" {q}"),
                    Style::default().fg(DIM),
                )));
            }
            if !c.output.is_empty() {
                for l in c.output.lines().take(30) {
                    lines.push(Line::from(Span::styled(
                        format!(" {l}"),
                        Style::default().fg(TEXT),
                    )));
                }
            }
        }
    }
    for f in &c.files {
        lines.push(Line::from(Span::styled(
            format!(" ● {}  {}", f.kind, f.path),
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
        )));
        lines.extend(diff_lines(&f.diff));
    }
    lines
}

struct WrapPiece {
    line: Line<'static>,
    start: usize,
    chars: Vec<char>,
}

fn line_cells(line: &Line<'_>) -> Vec<(char, Style)> {
    let mut cells: Vec<(char, Style)> = Vec::new();
    for span in &line.spans {
        let style = span.style;
        for c in span.content.chars() {
            if c == '\t' {
                for _ in 0..4 {
                    cells.push((' ', style));
                }
            } else {
                cells.push((c, style));
            }
        }
    }
    cells
}

fn selectable_line_text(line: &Line<'_>) -> String {
    line_cells(line).into_iter().map(|(c, _)| c).collect()
}

fn wrap_line_indexed(line: Line<'static>, width: u16) -> Vec<WrapPiece> {
    let width = width.max(1);
    let cells = line_cells(&line);
    if cells.is_empty() {
        return vec![WrapPiece {
            line: Line::from(""),
            start: 0,
            chars: Vec::new(),
        }];
    }
    let mut out = Vec::new();
    let mut start = 0usize;
    let mut col = 0u16;
    let mut i = 0usize;
    while i < cells.len() {
        let ch = cells[i].0;
        if ch == '\r' {
            i += 1;
            continue;
        }
        if ch == '\n' {
            out.push(piece_from_cells(&cells, start, i));
            start = i + 1;
            col = 0;
            i += 1;
            continue;
        }
        let w = ch_width(ch).max(1);
        if col > 0 && col.saturating_add(w) > width {
            out.push(piece_from_cells(&cells, start, i));
            start = i;
            col = 0;
            continue;
        }
        col = col.saturating_add(w);
        i += 1;
    }
    if start < cells.len() || out.is_empty() {
        out.push(piece_from_cells(&cells, start, cells.len()));
    }
    out
}

fn piece_from_cells(cells: &[(char, Style)], start: usize, end: usize) -> WrapPiece {
    let slice = &cells[start..end];
    WrapPiece {
        line: line_from_cells(slice),
        start,
        chars: slice.iter().map(|(c, _)| *c).collect(),
    }
}

fn wrap_visual(line: Line<'static>, width: u16) -> Vec<Line<'static>> {
    wrap_line_indexed(line, width)
        .into_iter()
        .map(|p| p.line)
        .collect()
}

fn line_from_cells(cells: &[(char, Style)]) -> Line<'static> {
    if cells.is_empty() {
        return Line::from("");
    }
    let mut spans = Vec::new();
    let mut buf = String::new();
    let mut style = cells[0].1;
    for &(c, st) in cells {
        if st != style && !buf.is_empty() {
            spans.push(Span::styled(std::mem::take(&mut buf), style));
        }
        buf.push(c);
        style = st;
    }
    if !buf.is_empty() {
        spans.push(Span::styled(buf, style));
    }
    Line::from(spans)
}

fn row_lines(row: &Row) -> Vec<Line<'static>> {
    match row {
        Row::User(u) => prefixed_text("you   ", USER, &u.text),
        Row::Agent(a) => agent_lines(a),
        Row::Tools(g) => {
            let mut lines = vec![group_header_line(g)];
            if g.expanded {
                for c in &g.calls {
                    lines.push(call_row_line(c, false));
                }
            }
            lines
        }
        Row::Think(t) => {
            let mut lines = vec![think_header_line(t)];
            if t.expanded {
                lines.extend(think_body_lines(t));
            }
            lines
        }
        Row::Meta(s) => vec![Line::from(Span::styled(
            s.clone(),
            Style::default().fg(DIM),
        ))],
        Row::Err(s) => vec![Line::from(Span::styled(
            s.clone(),
            Style::default().fg(WARN),
        ))],
        Row::Picture { label, .. } => vec![Line::from(Span::styled(
            label.clone(),
            Style::default().fg(DIM),
        ))],
    }
}

fn prefixed_text(prefix: &'static str, color: Color, s: &str) -> Vec<Line<'static>> {
    let mut out = Vec::new();
    for (i, part) in s.split('\n').enumerate() {
        if i == 0 {
            out.push(Line::from(vec![
                Span::styled(
                    prefix,
                    Style::default().fg(color).add_modifier(Modifier::BOLD),
                ),
                Span::styled(part.to_string(), Style::default().fg(TEXT)),
            ]));
        } else {
            out.push(Line::from(vec![
                Span::raw("      "),
                Span::styled(part.to_string(), Style::default().fg(TEXT)),
            ]));
        }
    }
    if out.is_empty() {
        out.push(Line::from(Span::styled(
            prefix,
            Style::default().fg(color).add_modifier(Modifier::BOLD),
        )));
    }
    out
}

fn agent_lines(a: &AgentMsg) -> Vec<Line<'static>> {
    let md_lines = md::markdown_lines(&a.text);
    let mut out = Vec::new();
    for (i, line) in md_lines.into_iter().enumerate() {
        let mut spans = Vec::new();
        if i == 0 {
            spans.push(Span::styled(
                "grok  ",
                Style::default().fg(AGENT).add_modifier(Modifier::BOLD),
            ));
        } else {
            spans.push(Span::raw("      "));
        }
        spans.extend(line.spans);
        out.push(Line::from(spans));
    }
    if out.is_empty() {
        out.push(Line::from(Span::styled(
            "grok  ",
            Style::default().fg(AGENT).add_modifier(Modifier::BOLD),
        )));
    }
    if a.work_ms > 0 {
        out.push(Line::from(Span::styled(
            format!("      工作 {}", md::fmt_duration(a.work_ms)),
            Style::default().fg(DIM),
        )));
    }
    out
}

fn diff_lines(diff: &str) -> Vec<Line<'static>> {
    colored_output_lines(diff, 120)
}

fn colored_output_lines(text: &str, max: usize) -> Vec<Line<'static>> {
    if text.is_empty() {
        return Vec::new();
    }
    let total = text.lines().count();
    let mut lines: Vec<Line<'static>> = text
        .lines()
        .take(max)
        .map(|l| {
            let l = l.trim_end_matches('\r');
            if l.contains('\u{1b}') {
                prefix_line("  ", ansi_line(l))
            } else {
                Line::from(Span::styled(
                    format!("  {l}"),
                    output_line_style(l),
                ))
            }
        })
        .collect();
    if total > max {
        lines.push(Line::from(Span::styled(
            format!("  … ({} 行省略)", total - max),
            Style::default().fg(DIM),
        )));
    }
    lines
}

fn prefix_line(prefix: &'static str, line: Line<'static>) -> Line<'static> {
    let mut spans = vec![Span::raw(prefix)];
    spans.extend(line.spans);
    Line::from(spans)
}

fn output_line_style(l: &str) -> Style {
    let t = l.trim_start();
    if t.starts_with("+++")
        || t.starts_with("---")
        || t.starts_with("diff ")
        || t.starts_with("index ")
    {
        Style::default().fg(DIM)
    } else if t.starts_with("@@") {
        Style::default().fg(DIFF_HUNK)
    } else if t.starts_with('+')
        || t.starts_with("new file:")
        || t.contains("new file:")
        || t.starts_with("??")
    {
        Style::default().fg(DIFF_ADD)
    } else if t.starts_with('-') || t.contains("deleted:") || t.starts_with("D ") {
        Style::default().fg(DIFF_DEL)
    } else if t.contains("modified:") || t.starts_with("M ") || t.starts_with("MM") {
        Style::default().fg(DIFF_HUNK)
    } else if t.starts_with('$') {
        Style::default().fg(ACCENT)
    } else {
        Style::default().fg(TEXT)
    }
}

fn ansi_line(s: &str) -> Line<'static> {
    let mut spans = Vec::new();
    let mut buf = String::new();
    let mut style = Style::default().fg(TEXT);
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' && chars.peek() == Some(&'[') {
            chars.next();
            let mut code = String::new();
            while let Some(&d) = chars.peek() {
                chars.next();
                if d.is_ascii_alphabetic() {
                    if d == 'm' && !buf.is_empty() {
                        spans.push(Span::styled(std::mem::take(&mut buf), style));
                    }
                    if d == 'm' {
                        style = apply_sgr(&code, style);
                    }
                    break;
                }
                code.push(d);
            }
            continue;
        }
        if c != '\r' {
            buf.push(c);
        }
    }
    if !buf.is_empty() {
        spans.push(Span::styled(buf, style));
    }
    if spans.is_empty() {
        Line::from("")
    } else {
        Line::from(spans)
    }
}

fn apply_sgr(code: &str, mut style: Style) -> Style {
    if code.is_empty() {
        return Style::default().fg(TEXT);
    }
    for part in code.split(';') {
        match part {
            "" | "0" => style = Style::default().fg(TEXT),
            "1" => style = style.add_modifier(Modifier::BOLD),
            "2" | "90" => style = style.fg(DIM),
            "31" | "91" => style = style.fg(DIFF_DEL),
            "32" | "92" => style = style.fg(DIFF_ADD),
            "33" | "93" => style = style.fg(TOOL),
            "34" | "36" | "94" | "96" => style = style.fg(DIFF_HUNK),
            "35" | "95" => style = style.fg(THINK),
            "39" => style = style.fg(TEXT),
            _ => {}
        }
    }
    style
}

