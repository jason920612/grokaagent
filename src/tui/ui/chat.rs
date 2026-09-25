//! Drawing a transcript: rows to wrapped lines, text selection, inline
//! images, tool details, and the scrollbar.

use std::path::Path;

use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Clear, Paragraph};
use ratatui::Frame;
use ratatui_image::Image;
use serde_json::Value;

use crate::md;
use crate::tui::app::{App, GlyphLine, Hit, Images};
use crate::tui::edit::{ch_width, display_cols};
use crate::tui::model::rows::{AgentMsg, Row, Think, ToolCall, ToolGroup};
use crate::tui::model::session::{ChatSel, ChatView};
use crate::tui::model::tool_text::{tool_finished_line, tool_started_line};
use crate::tui::theme::*;

/// One logical line before wrapping.
struct Item {
    line: Line<'static>,
    hit: Option<Hit>,
    wrap: bool,
    /// Painted as a graphic of (path, width, height) cells.
    graphic: Option<(String, u16, u16)>,
    /// Selectable: transcript row + char offset of this line.
    select: Option<(usize, usize)>,
    /// Live think header whose clock ticks.
    clock: bool,
}

impl Item {
    fn new(line: Line<'static>, hit: Option<Hit>) -> Self {
        Self {
            line,
            hit,
            wrap: true,
            graphic: None,
            select: None,
            clock: false,
        }
    }

    fn raw(line: Line<'static>, hit: Option<Hit>) -> Self {
        Self {
            wrap: false,
            ..Self::new(line, hit)
        }
    }
}

/// Label for the author of user rows: "you" in the main chat, the parent
/// agent in a child's transcript.
fn user_prefix(child: bool) -> &'static str {
    if child {
        "上層  "
    } else {
        "you   "
    }
}

pub(crate) fn prefixed_text(prefix: &'static str, color: Color, s: &str) -> Vec<Line<'static>> {
    let bold = Style::default().fg(color).add_modifier(Modifier::BOLD);
    let mut out: Vec<Line<'static>> = s
        .split('\n')
        .enumerate()
        .map(|(i, part)| {
            let head = if i == 0 { Span::styled(prefix, bold) } else { Span::raw("      ") };
            Line::from(vec![head, Span::styled(part.to_string(), Style::default().fg(TEXT))])
        })
        .collect();
    if out.is_empty() {
        out.push(Line::from(Span::styled(prefix, bold)));
    }
    out
}

/// Author label of the main agent's replies (the model can be any provider).
const MAIN_LABEL: &str = "agent";

fn agent_lines(a: &AgentMsg, label: &str) -> Vec<Line<'static>> {
    // Long child names still need a gap before the text.
    let head = if label.chars().count() >= 6 { format!("{label} ") } else { format!("{label:<6}") };
    let bold = Style::default().fg(AGENT).add_modifier(Modifier::BOLD);
    let mut out: Vec<Line<'static>> = md::markdown_lines(&a.text)
        .into_iter()
        .enumerate()
        .map(|(i, line)| {
            let mut spans = vec![if i == 0 { Span::styled(head.clone(), bold) } else { Span::raw("      ") }];
            spans.extend(line.spans);
            Line::from(spans)
        })
        .collect();
    if out.is_empty() {
        out.push(Line::from(Span::styled(head, bold)));
    }
    if a.work_ms > 0 {
        out.push(Line::from(Span::styled(
            format!("      工作 {}", md::fmt_duration(a.work_ms)),
            Style::default().fg(DIM),
        )));
    }
    out
}

fn group_header(g: &ToolGroup) -> Line<'static> {
    let arrow = if g.expanded { "▾" } else { "▸" };
    let mut names: Vec<&str> = Vec::new();
    for c in &g.calls {
        if !names.contains(&c.name.as_str()) {
            names.push(&c.name);
        }
    }
    let status = if let Some(c) = g.calls.iter().rev().find(|c| !c.done) {
        if c.phase.is_empty() { "執行中" } else { c.phase.as_str() }
    } else if g.calls.iter().any(ToolCall::failed) {
        "有操作失敗"
    } else if g.calls.iter().any(ToolCall::stopped) {
        "已停止"
    } else {
        "完成"
    };
    let names = names.join(" · ");
    let label = if g.calls.len() <= 1 {
        format!("{arrow} {names}    {status}")
    } else {
        format!("{arrow} {} 個工具 · {names}    {status}", g.calls.len())
    };
    Line::from(Span::styled(label, Style::default().fg(TOOL)))
}

pub(crate) fn think_header(t: &Think) -> Line<'static> {
    let arrow = if t.expanded { "▾" } else { "▸" };
    let clock = md::fmt_duration_field(t.elapsed());
    let status = if t.done { "完成" } else { "進行中" };
    Line::from(Span::styled(
        format!("{arrow} 思考  {clock}    {status}"),
        Style::default().fg(THINK),
    ))
}

fn think_body(t: &Think) -> Vec<Line<'static>> {
    const MAX: usize = 120;
    let dim = Style::default().fg(DIM);
    let mut lines: Vec<Line<'static>> = t
        .text
        .split('\n')
        .take(MAX)
        .map(|p| Line::from(Span::styled(format!("  {p}"), dim)))
        .collect();
    let total = t.text.lines().count();
    if total > MAX {
        lines.push(Line::from(Span::styled(format!("  … ({} 行省略)", total - MAX), dim)));
    }
    lines
}

fn call_line(c: &ToolCall, selected: bool) -> Line<'static> {
    let inner = if c.failed() || c.stopped() {
        format!("! {}  {}", c.name, c.phase)
    } else if c.done {
        tool_finished_line(&c.name, &c.output)
    } else if !c.phase.is_empty() {
        format!("{}  {}", tool_started_line(&c.name, &c.args), c.phase)
    } else {
        tool_started_line(&c.name, &c.args)
    };
    let style = if selected {
        Style::default().fg(Color::Black).bg(ACCENT)
    } else if c.failed() {
        Style::default().fg(WARN)
    } else {
        Style::default().fg(TOOL)
    };
    Line::from(Span::styled(format!("  {inner}"), style))
}

fn call_body(c: &ToolCall) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    if c.name == "run_command" {
        if let Ok(v) = serde_json::from_str::<Value>(&c.output) {
            if let Some(stdout) = v.get("stdout").and_then(Value::as_str) {
                lines.extend(colored_lines(stdout, 60));
            }
            if let Some(stderr) = v.get("stderr").and_then(Value::as_str).filter(|s| !s.trim().is_empty()) {
                lines.extend(colored_lines(stderr, 20));
            }
        }
    }
    for f in &c.files {
        lines.push(Line::from(Span::styled(
            format!("    ● {}  {}", f.kind, f.path),
            Style::default().fg(ACCENT),
        )));
        lines.extend(colored_lines(&f.diff, 120));
    }
    lines
}

/// Everything about one call, for the detail popup.
pub(crate) fn call_detail(c: &ToolCall) -> Vec<Line<'static>> {
    let dim = Style::default().fg(DIM);
    let text = Style::default().fg(TEXT);
    let mut lines = vec![Line::from(Span::styled(
        format!(" {}  · {}", c.name, if c.phase.is_empty() { "執行中" } else { &c.phase }),
        Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
    ))];
    let parsed = serde_json::from_str::<Value>(&c.output).ok();
    match c.name.as_str() {
        "run_command" => {
            let cmd = c.args.get("command").and_then(Value::as_str).unwrap_or("");
            lines.push(Line::from(Span::styled(format!(" $ {cmd}"), text)));
            match &parsed {
                Some(v) => {
                    if let Some(code) = v.get("exit_code") {
                        lines.push(Line::from(Span::styled(format!(" exit {code}"), dim)));
                    }
                    for (key, color, max) in [("stdout", DIM, 80), ("stderr", WARN, 40)] {
                        if let Some(s) = v.get(key).and_then(Value::as_str).filter(|s| !s.trim().is_empty()) {
                            lines.push(Line::from(Span::styled(format!(" {key}"), Style::default().fg(color))));
                            lines.extend(colored_lines(s, max));
                        }
                    }
                }
                None if !c.output.is_empty() => lines.extend(colored_lines(&c.output, 80)),
                None => {}
            }
        }
        "list_dir" => {
            let path = c.args.get("path").and_then(Value::as_str).unwrap_or(".");
            lines.push(Line::from(Span::styled(format!(" {path}"), dim)));
            let entries = parsed
                .as_ref()
                .and_then(|v| v.get("entries").and_then(Value::as_array).cloned())
                .unwrap_or_default();
            for e in entries.iter().take(80) {
                let name = e.get("name").and_then(Value::as_str).unwrap_or("");
                let mark = if e.get("dir").and_then(Value::as_bool).unwrap_or(false) { "/" } else { "" };
                lines.push(Line::from(Span::styled(format!("  {name}{mark}"), text)));
            }
        }
        _ => {
            let args = serde_json::to_string_pretty(&c.args).unwrap_or_default();
            for l in args.lines().take(12) {
                lines.push(Line::from(Span::styled(format!(" {l}"), dim)));
            }
            for l in c.output.lines().take(40) {
                lines.push(Line::from(Span::styled(format!(" {l}"), text)));
            }
        }
    }
    for f in &c.files {
        lines.push(Line::from(Span::styled(
            format!(" ● {}  {}", f.kind, f.path),
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
        )));
        lines.extend(colored_lines(&f.diff, 200));
    }
    lines
}

fn line_style(l: &str) -> Style {
    let t = l.trim_start();
    let fg = if t.starts_with("+++") || t.starts_with("---") || t.starts_with("diff ") || t.starts_with("index ") {
        DIM
    } else if t.starts_with("@@") {
        DIFF_HUNK
    } else if t.starts_with('+') || t.contains("new file:") || t.starts_with("??") {
        DIFF_ADD
    } else if t.starts_with('-') || t.contains("deleted:") || t.starts_with("D ") {
        DIFF_DEL
    } else if t.contains("modified:") || t.starts_with("M ") || t.starts_with("MM") {
        DIFF_HUNK
    } else if t.starts_with('$') {
        ACCENT
    } else {
        TEXT
    };
    Style::default().fg(fg)
}

/// Command output / diffs: ANSI colors kept, diff lines colored, capped.
pub(crate) fn colored_lines(text: &str, max: usize) -> Vec<Line<'static>> {
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
                let mut spans = vec![Span::raw("  ")];
                spans.extend(ansi_line(l).spans);
                Line::from(spans)
            } else {
                Line::from(Span::styled(format!("  {l}"), line_style(l)))
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

fn ansi_line(s: &str) -> Line<'static> {
    let mut spans = Vec::new();
    let mut buf = String::new();
    let mut style = Style::default().fg(TEXT);
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' && chars.peek() == Some(&'[') {
            chars.next();
            let mut code = String::new();
            while let Some(d) = chars.next() {
                if d.is_ascii_alphabetic() {
                    if d == 'm' {
                        if !buf.is_empty() {
                            spans.push(Span::styled(std::mem::take(&mut buf), style));
                        }
                        style = sgr(&code, style);
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
    Line::from(spans)
}

fn sgr(code: &str, mut style: Style) -> Style {
    for part in code.split(';') {
        style = match part {
            "" | "0" => Style::default().fg(TEXT),
            "1" => style.add_modifier(Modifier::BOLD),
            "2" | "90" => style.fg(DIM),
            "31" | "91" => style.fg(DIFF_DEL),
            "32" | "92" => style.fg(DIFF_ADD),
            "33" | "93" => style.fg(TOOL),
            "34" | "36" | "94" | "96" => style.fg(DIFF_HUNK),
            "35" | "95" => style.fg(THINK),
            "39" => style.fg(TEXT),
            _ => style,
        };
    }
    style
}

/// Plain lines of a selectable row (what copy returns).
fn row_lines(row: &Row, child: bool, label: &str) -> Option<Vec<Line<'static>>> {
    let dim = |s: &str| vec![Line::from(Span::styled(s.to_string(), Style::default().fg(DIM)))];
    Some(match row {
        Row::User(u) => prefixed_text(user_prefix(child), USER, &u.text),
        Row::Agent(a) => agent_lines(a, label),
        Row::Meta(s) => dim(s),
        Row::Err(s) => vec![Line::from(Span::styled(s.clone(), Style::default().fg(WARN)))],
        Row::Picture { label, .. } => dim(label),
        Row::Tools(_) | Row::Think(_) => return None,
    })
}

fn line_text(line: &Line<'_>) -> String {
    line.spans
        .iter()
        .flat_map(|s| s.content.chars())
        .flat_map(|c| if c == '\t' { vec![' '; 4] } else { vec![c] })
        .collect()
}

fn row_text(row: &Row) -> Option<String> {
    let lines = row_lines(row, false, MAIN_LABEL)?;
    Some(lines.iter().map(line_text).collect::<Vec<_>>().join("\n"))
}

/// Text of a chat selection.
pub(crate) fn selected_text(rows: &[Row], sel: &ChatSel) -> Option<String> {
    let (lo, hi) = sel.text_range()?;
    let mut parts = Vec::new();
    for ri in lo.row..=hi.row.min(rows.len().saturating_sub(1)) {
        let Some(s) = rows.get(ri).and_then(row_text) else {
            continue;
        };
        let n = s.chars().count();
        let a = if ri == lo.row { lo.idx.min(n) } else { 0 };
        let b = if ri == hi.row { hi.idx.min(n) } else { n };
        if a < b {
            parts.push(s.chars().skip(a).take(b - a).collect::<String>());
        }
    }
    (!parts.is_empty()).then(|| parts.join("\n"))
}

fn tint(line: Line<'static>, lo: usize, hi: usize) -> Line<'static> {
    let mut spans = Vec::new();
    let mut i = 0usize;
    for span in line.spans {
        let mut buf = String::new();
        let mut buf_style = span.style;
        for c in span.content.chars() {
            let st = if i >= lo && i < hi { span.style.bg(SELECT_BG) } else { span.style };
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

struct Piece {
    line: Line<'static>,
    start: usize,
    chars: Vec<char>,
}

/// Wrap by display width, remembering each piece's char offset.
fn wrap_indexed(line: Line<'static>, width: u16) -> Vec<Piece> {
    let width = width.max(1);
    let cells: Vec<(char, Style)> = line
        .spans
        .iter()
        .flat_map(|s| {
            s.content
                .chars()
                .flat_map(move |c| if c == '\t' { vec![(' ', s.style); 4] } else { vec![(c, s.style)] })
        })
        .collect();
    let piece = |a: usize, b: usize| -> Piece {
        let slice = &cells[a..b];
        let mut spans = Vec::new();
        let mut buf = String::new();
        let mut style = slice.first().map(|c| c.1).unwrap_or_default();
        for &(c, st) in slice {
            if st != style && !buf.is_empty() {
                spans.push(Span::styled(std::mem::take(&mut buf), style));
            }
            buf.push(c);
            style = st;
        }
        if !buf.is_empty() {
            spans.push(Span::styled(buf, style));
        }
        Piece {
            line: Line::from(spans),
            start: a,
            chars: slice.iter().map(|c| c.0).collect(),
        }
    };
    if cells.is_empty() {
        return vec![piece(0, 0)];
    }
    let mut out = Vec::new();
    let (mut start, mut col, mut i) = (0usize, 0u16, 0usize);
    while i < cells.len() {
        let ch = cells[i].0;
        if ch == '\n' {
            out.push(piece(start, i));
            start = i + 1;
            col = 0;
            i += 1;
            continue;
        }
        let w = ch_width(ch).max(1);
        if col > 0 && col.saturating_add(w) > width {
            out.push(piece(start, i));
            start = i;
            col = 0;
            continue;
        }
        col = col.saturating_add(w);
        i += 1;
    }
    if start < cells.len() || out.is_empty() {
        out.push(piece(start, cells.len()));
    }
    out
}

pub(crate) fn wrap(line: Line<'static>, width: u16) -> Vec<Line<'static>> {
    wrap_indexed(line, width).into_iter().map(|p| p.line).collect()
}

fn image_block(
    images: &mut Images,
    workspace: &Path,
    image_hits: &mut Vec<String>,
    out: &mut Vec<Item>,
    rel: &str,
    caption: String,
    cols: u16,
    selected: bool,
) {
    let hit = Some(Hit::ChatImage(image_hits.len() as u16));
    image_hits.push(rel.to_string());
    if !caption.is_empty() {
        let style = if selected {
            Style::default().fg(Color::Black).bg(ACCENT)
        } else {
            Style::default().fg(DIM)
        };
        out.push(Item::new(Line::from(Span::styled(format!("      {caption}"), style)), hit));
    }
    let max_cols = cols.saturating_sub(crate::preview::INDENT);
    let max_rows = crate::preview::MAX_ROWS;
    if let Some(picker) = images.picker.as_ref().filter(|p| crate::preview::uses_graphics(p)) {
        let key = (rel.to_string(), max_cols, max_rows);
        let (w, h) = match images.cells.get(&key) {
            Some(&sz) => sz,
            None => {
                let sz = crate::preview::cell_size_for(picker, &workspace.join(rel), max_cols, max_rows);
                images.cells.insert(key, sz);
                sz
            }
        };
        out.push(Item {
            graphic: Some((rel.to_string(), w.max(1), h.max(1))),
            ..Item::raw(Line::from(""), hit)
        });
        return;
    }
    let cols = max_cols.min(crate::preview::MAX_COLS).max(4);
    let key = (rel.to_string(), cols);
    let lines = images
        .halfblocks
        .entry(key)
        .or_insert_with(|| crate::preview::from_path(&workspace.join(rel), cols, crate::preview::MAX_ROWS))
        .clone();
    for line in lines {
        let mut padded = vec![Span::raw("      ")];
        padded.extend(line.spans);
        out.push(Item::raw(Line::from(padded), hit));
    }
}

fn items(
    rows: &[Row],
    view: &ChatView,
    child: bool,
    label: &str,
    images: &mut Images,
    workspace: &Path,
    image_hits: &mut Vec<String>,
    cols: u16,
) -> Vec<Item> {
    let mut out = Vec::new();
    for (ri, row) in rows.iter().enumerate() {
        match row {
            Row::Tools(g) => {
                out.push(Item::new(group_header(g), Some(Hit::ToolGroup(ri))));
                if g.expanded {
                    for (ci, call) in g.calls.iter().enumerate() {
                        let hit = Some(Hit::ToolItem(ri, ci));
                        out.push(Item::new(call_line(call, view.open_tool == Some((ri, ci))), hit));
                        out.extend(call_body(call).into_iter().map(|l| Item::new(l, hit)));
                    }
                }
            }
            Row::Think(t) => {
                out.push(Item {
                    clock: !t.done,
                    ..Item::raw(think_header(t), Some(Hit::Think(ri)))
                });
                if t.expanded {
                    out.extend(think_body(t).into_iter().map(|l| Item::new(l, Some(Hit::Think(ri)))));
                }
            }
            other => {
                let hit = Some(Hit::ChatRow(ri as u16));
                let mut off = 0usize;
                for line in row_lines(other, child, label).unwrap_or_default() {
                    let n = line_text(&line).chars().count();
                    out.push(Item {
                        select: Some((ri, off)),
                        ..Item::new(line, hit)
                    });
                    off += n + 1;
                }
                let pics: Vec<(String, String)> = match other {
                    Row::User(u) => u.images.iter().map(|p| (p.clone(), format!("圖片  {p}"))).collect(),
                    Row::Picture { path, .. } => vec![(path.clone(), String::new())],
                    _ => Vec::new(),
                };
                for (p, caption) in pics {
                    let sel = matches!(&view.sel, ChatSel::Image(x) if *x == p);
                    image_block(images, workspace, image_hits, &mut out, &p, caption, cols, sel);
                }
            }
        }
    }
    out
}

fn piece_tint(sel: &ChatSel, row: usize, start: usize, n: usize) -> Option<(usize, usize)> {
    let (lo, hi) = sel.text_range()?;
    if row < lo.row || row > hi.row {
        return None;
    }
    let a = if row == lo.row { lo.idx } else { 0 }.max(start);
    let b = if row == hi.row { hi.idx } else { usize::MAX }.min(start + n);
    (a < b).then(|| (a - start, b - start))
}

enum Paint {
    Line {
        line: Line<'static>,
        hit: Option<Hit>,
        glyph: Option<(usize, usize, Vec<char>)>,
        clock: bool,
    },
    Graphic {
        rel: String,
        width: u16,
        height: u16,
        hit: Option<Hit>,
    },
}

/// Draw the active tab's transcript into `area`.
pub(crate) fn draw(f: &mut Frame, app: &mut App, area: Rect) {
    let App {
        sessions,
        current,
        ui,
        images,
        ..
    } = app;
    let s = sessions.get_mut(current.as_str()).expect("current session");
    let key = s.view_key();
    let child = !key.is_empty();
    let label = if child {
        key.rsplit('/').next().unwrap_or("agent").to_string()
    } else {
        MAIN_LABEL.to_string()
    };
    let workspace = s.meta.workspace.clone();
    let epoch = s.view_transcript().epoch;
    let mut view = s.views.get(&key).cloned().unwrap_or_default();
    view.sync(epoch);

    f.render_widget(
        Block::default()
            .borders(Borders::TOP)
            .border_style(Style::default().fg(BORDER))
            .style(Style::default().bg(PANEL).fg(TEXT)),
        area,
    );
    ui.hits.push((area, Hit::Chat));
    ui.think_clocks.clear();
    ui.image_hits.clear();
    let bar_w = u16::from(area.width >= 8);
    let inner = Rect::new(
        area.x + 1,
        area.y + 1,
        area.width.saturating_sub(2 + bar_w),
        area.height.saturating_sub(1),
    );
    ui.chat_inner = inner;
    ui.chat_bar = if bar_w == 0 {
        Rect::default()
    } else {
        Rect::new(area.right() - 1, inner.y, 1, inner.height)
    };
    if inner.width == 0 || inner.height == 0 {
        s.views.insert(key, view);
        return;
    }

    let rows = &s.view_transcript().rows;
    let mut paints = Vec::new();
    for item in items(rows, &view, child, &label, images, &workspace, &mut ui.image_hits, inner.width) {
        if let Some((rel, width, height)) = item.graphic {
            paints.push(Paint::Graphic { rel, width, height, hit: item.hit });
            continue;
        }
        match item.select {
            Some((row, start)) => {
                let pieces = if item.wrap {
                    wrap_indexed(item.line, inner.width)
                } else {
                    let chars = line_text(&item.line).chars().collect();
                    vec![Piece { line: item.line, start: 0, chars }]
                };
                for p in pieces {
                    paints.push(Paint::Line {
                        line: p.line,
                        hit: item.hit,
                        glyph: Some((row, start + p.start, p.chars)),
                        clock: false,
                    });
                }
            }
            None => {
                let lines = if item.wrap { wrap(item.line, inner.width) } else { vec![item.line] };
                for (i, line) in lines.into_iter().enumerate() {
                    paints.push(Paint::Line { line, hit: item.hit, glyph: None, clock: item.clock && i == 0 });
                }
            }
        }
    }
    let height_of = |p: &Paint| match p {
        Paint::Line { .. } => 1,
        Paint::Graphic { height, .. } => *height,
    };
    let total: u16 = paints.iter().map(height_of).sum();
    let max_off = total.saturating_sub(inner.height);
    ui.chat_total = total;
    ui.chat_max_off = max_off;
    view.scroll = view.scroll.min(max_off);
    let top = if view.stick_bottom { max_off } else { max_off - view.scroll };
    let vis_end = top + inner.height;
    let mut y = 0u16;
    for paint in paints {
        let h = height_of(&paint);
        let (start, end) = (y, y + h);
        y = end;
        if end <= top || start >= vis_end {
            continue;
        }
        let sy = inner.y + start.saturating_sub(top);
        match paint {
            Paint::Line { mut line, hit, glyph, clock } => {
                if let Some((row, gstart, chars)) = &glyph {
                    if let Some((lo, hi)) = piece_tint(&view.sel, *row, *gstart, chars.len()) {
                        line = tint(line, lo, hi);
                    }
                }
                let r = Rect::new(inner.x, sy, inner.width, 1);
                f.render_widget(Paragraph::new(line), r);
                if clock {
                    if let Some(Hit::Think(i)) = hit {
                        ui.think_clocks.push((i, r));
                    }
                }
                match glyph {
                    Some((row, gstart, chars)) => {
                        let text_w: u16 = chars.iter().map(|c| ch_width(*c).max(1)).sum::<u16>().min(inner.width);
                        if let (Some(kind), true) = (hit, text_w > 0) {
                            ui.hits.push((Rect::new(inner.x, sy, text_w, 1), kind));
                        }
                        ui.chat_glyphs.push(GlyphLine { y: sy, x: inner.x, text_w, row, start: gstart, chars });
                    }
                    None => {
                        if let Some(kind) = hit {
                            ui.hits.push((r, kind));
                        }
                    }
                }
            }
            Paint::Graphic { rel, width, height, hit } => {
                let vis_h = end.min(vis_end) - start.max(top);
                if start >= top && end <= vis_end {
                    paint_graphic(f, images, &workspace, &rel, width, height, Rect::new(inner.x, sy, inner.width, height));
                }
                if let Some(kind) = hit {
                    ui.hits.push((Rect::new(inner.x, sy, inner.width, vis_h.max(1)), kind));
                }
            }
        }
    }
    draw_scrollbar(f, ui, &view, max_off, inner.height);
    if !view.stick_bottom && ui.image_view.is_none() {
        draw_jump_bottom(f, ui, area);
    }
    let tool = view.open_tool.and_then(|(r, c)| s.view_transcript().call(r, c).cloned());
    match tool {
        Some(call) => draw_tool_panel(f, ui, area, &call),
        None => view.open_tool = None,
    }
    s.views.insert(key, view);
}

fn paint_graphic(f: &mut Frame, images: &mut Images, workspace: &Path, rel: &str, w: u16, h: u16, slot: Rect) {
    let Some(proto) = cached_proto(images, workspace, rel, w, h) else {
        f.render_widget(
            Paragraph::new(Span::styled("      [無法預覽]", Style::default().fg(DIM))),
            Rect::new(slot.x, slot.y, slot.width, 1),
        );
        return;
    };
    let pa = proto.area();
    let draw = Rect::new(
        slot.x + crate::preview::INDENT,
        slot.y,
        pa.width.min(slot.width.saturating_sub(crate::preview::INDENT)),
        pa.height.min(h),
    );
    if draw.width > 0 && draw.height > 0 {
        blit(f, images, &proto, draw);
    }
}

pub(crate) fn cached_proto(
    images: &mut Images,
    workspace: &Path,
    rel: &str,
    w: u16,
    h: u16,
) -> Option<ratatui_image::protocol::Protocol> {
    let key = (rel.to_string(), w, h);
    if let Some(p) = images.protos.get(&key) {
        return Some(p.clone());
    }
    let picker = images.picker.clone()?;
    let proto = crate::preview::protocol_for(&picker, &workspace.join(rel), w, h)?;
    images.protos.insert(key, proto.clone());
    Some(proto)
}

/// Sixel / iTerm2 payloads go out after the frame; others render in place.
pub(crate) fn blit(f: &mut Frame, images: &mut Images, proto: &ratatui_image::protocol::Protocol, draw: Rect) {
    if let Some(data) = crate::preview::immediate_payload(proto) {
        crate::preview::reserve_graphic_cells(f.buffer_mut(), draw);
        images.blits.push(crate::preview::GraphicBlit {
            x: draw.x,
            y: draw.y,
            width: draw.width,
            height: draw.height,
            data: data.to_string(),
        });
        return;
    }
    f.render_widget(Image::new(proto), draw);
}

pub(crate) fn thumb_h(total: u16, view_h: u16, track_h: u16) -> u16 {
    if track_h == 0 {
        return 0;
    }
    if total <= view_h {
        return track_h;
    }
    let h = (view_h as u32 * track_h as u32) / total.max(1) as u32;
    (h as u16).clamp(2.min(track_h).max(1), track_h)
}

/// Thumb top (relative to the track) for a scroll offset from the bottom.
pub(crate) fn thumb_rel(max_off: u16, track_h: u16, thumb: u16, scroll: u16) -> u16 {
    let travel = track_h.saturating_sub(thumb);
    if travel == 0 || max_off == 0 {
        return 0;
    }
    let visual = max_off - scroll.min(max_off);
    ((visual as u32 * travel as u32) / max_off as u32) as u16
}

/// Scroll offset from the bottom for a thumb top.
pub(crate) fn offset_for_thumb(max_off: u16, track_h: u16, thumb: u16, rel_y: u16) -> u16 {
    let travel = track_h.saturating_sub(thumb);
    if travel == 0 || max_off == 0 {
        return 0;
    }
    let rel = rel_y.min(travel);
    let visual = (rel as u32 * max_off as u32 + travel as u32 / 2) / travel as u32;
    max_off.saturating_sub(visual as u16)
}

fn draw_scrollbar(f: &mut Frame, ui: &mut crate::tui::app::Ui, view: &ChatView, max_off: u16, view_h: u16) {
    let track = ui.chat_bar;
    if track.width == 0 || track.height == 0 {
        return;
    }
    f.render_widget(Block::default().style(Style::default().bg(BORDER)), track);
    ui.hits.push((track, Hit::ScrollBar));
    if max_off == 0 {
        return;
    }
    let th = thumb_h(ui.chat_total, view_h, track.height);
    let scroll = if view.stick_bottom { 0 } else { view.scroll.min(max_off) };
    let rel = thumb_rel(max_off, track.height, th, scroll);
    let thumb = Rect::new(track.x, track.y + rel, 1, th.max(1));
    f.render_widget(Block::default().style(Style::default().bg(ACCENT)), thumb);
    ui.hits.push((thumb, Hit::ScrollThumb));
}

fn draw_jump_bottom(f: &mut Frame, ui: &mut crate::tui::app::Ui, chat: Rect) {
    let label = " ▼ 最新 ";
    let w = display_cols(label);
    if chat.width < w + 2 || chat.height < 2 {
        return;
    }
    let r = Rect::new(chat.x + (chat.width - w) / 2, chat.bottom() - 1, w, 1);
    f.render_widget(Clear, r);
    f.render_widget(
        Paragraph::new(Span::styled(
            label,
            Style::default().fg(Color::Black).bg(ACCENT).add_modifier(Modifier::BOLD),
        )),
        r,
    );
    ui.hits.push((r, Hit::JumpBottom));
}

fn draw_tool_panel(f: &mut Frame, ui: &mut crate::tui::app::Ui, chat: Rect, call: &ToolCall) {
    let w = chat.width.saturating_sub(4).clamp(28.min(chat.width), 96);
    let h = chat.height.saturating_sub(2).clamp(8.min(chat.height), 30);
    let panel = Rect::new(chat.x + chat.width.saturating_sub(w) / 2, chat.y + 1, w, h);
    f.render_widget(Clear, panel);
    f.render_widget(
        Block::default()
            .title(format!(" {} · Esc 關閉 ", call.name))
            .title_style(Style::default().fg(TEXT).add_modifier(Modifier::BOLD))
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(ACCENT))
            .style(Style::default().bg(COMPOSER).fg(TEXT)),
        panel,
    );
    let close = Rect::new(panel.right().saturating_sub(4), panel.y, 3, 1);
    f.render_widget(Paragraph::new(Span::styled(" × ", Style::default().fg(DIM).bg(COMPOSER))), close);
    let inner = Rect::new(panel.x + 1, panel.y + 1, panel.width.saturating_sub(2), panel.height.saturating_sub(2));
    let mut y = inner.y;
    'lines: for line in call_detail(call) {
        for piece in wrap(line, inner.width.max(1)) {
            if y >= inner.bottom() {
                break 'lines;
            }
            f.render_widget(Paragraph::new(piece), Rect::new(inner.x, y, inner.width, 1));
            y += 1;
        }
    }
    ui.hits.push((panel, Hit::ToolPanel));
    ui.hits.push((close, Hit::ToolPanelClose));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::model::rows::UserMsg;
    use crate::tui::model::session::ChatPos;

    #[test]
    fn selection_spans_rows_and_skips_tools() {
        let rows = vec![
            Row::User(UserMsg::from("hello")),
            Row::Tools(ToolGroup { calls: vec![], expanded: false }),
            Row::Meta("world".into()),
        ];
        let sel = ChatSel::Text {
            anchor: ChatPos { row: 0, idx: 6 },
            caret: ChatPos { row: 2, idx: 3 },
        };
        assert_eq!(selected_text(&rows, &sel).as_deref(), Some("hello\nwor"));
    }

    #[test]
    fn wrapping_keeps_char_offsets() {
        let pieces = wrap_indexed(Line::from("你好世界ab"), 5);
        let starts: Vec<usize> = pieces.iter().map(|p| p.start).collect();
        assert_eq!(starts, [0, 2, 5], "世界a fits in five cells");
        assert_eq!(pieces[2].chars, ['b']);
    }

    #[test]
    fn ansi_colors_become_styles() {
        let line = ansi_line("\u{1b}[31mred\u{1b}[0m plain");
        assert_eq!(line.spans[0].content, "red");
        assert_eq!(line.spans[0].style.fg, Some(DIFF_DEL));
        assert_eq!(line.spans[1].content, " plain");
    }

    #[test]
    fn scrollbar_maps_both_ways() {
        let th = thumb_h(100, 20, 20);
        assert_eq!(th, 4);
        let rel = thumb_rel(80, 20, th, 0);
        assert_eq!(rel, 16, "at the bottom the thumb sits at the end");
        assert_eq!(offset_for_thumb(80, 20, th, 16), 0);
        assert_eq!(offset_for_thumb(80, 20, th, 0), 80);
    }
}
