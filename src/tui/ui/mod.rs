//! Rendering. `draw` lays out the workbench each frame and records mouse
//! targets in `app.ui.hits`; it reads state and never changes sessions.

pub(crate) mod chat;
mod composer;
mod overlays;
pub(crate) mod settings;
mod workbench;

use ratatui::buffer::{Buffer, Cell};
use ratatui::layout::{Constraint, Direction, Layout, Position, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Clear, Paragraph, Widget};
use ratatui::Frame;

use crate::md;
use crate::tui::app::{App, Focus, Hit, Tab};
use crate::tui::edit::{caret_row_col, ch_width, display_cols, wrap_lines, Edit};
use crate::tui::model::rows::Row;
use crate::tui::model::tool_text::spinner;
use crate::tui::theme::*;

/// Draw the whole frame; returns where the hardware cursor belongs.
pub(crate) fn draw(f: &mut Frame, app: &mut App, freeze_composer: bool) -> Position {
    app.ui.hits.clear();
    app.ui.chat_glyphs.clear();
    app.ui.side_items.clear();
    app.images.blits.clear();
    app.ui.area = f.area();
    app.refresh_list(false);
    f.render_widget(Block::default().style(Style::default().bg(BG).fg(TEXT)), f.area());

    let whole = f.area();
    let [work, status] = split_v(whole, [Constraint::Min(4), Constraint::Length(1)]);
    draw_status(f, app, status);

    let act_w = if whole.width >= ACTIVITY_MIN_TERM { ACTIVITY_W } else { 0 };
    let docked = app.ui.side_open && whole.width >= SIDEBAR_MIN_TERM;
    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Length(act_w),
            Constraint::Length(if docked { SIDEBAR_W } else { 0 }),
            Constraint::Min(20),
        ])
        .split(work);
    if act_w > 0 {
        workbench::activity_bar(f, app, cols[0]);
    }
    app.ui.side_area = Rect::default();
    let mut caret = None;
    if docked {
        caret = workbench::side_bar(f, app, cols[1]);
    }
    let editor = cols[2];
    let tab = app.active_tab();
    let composer_h = match &tab {
        Tab::Chat => composer::height(app),
        Tab::Agent(_) => 2,
        Tab::Settings => 0,
    };
    let panel_h = if app.ui.bottom.is_some() {
        (editor.height * 35 / 100)
            .clamp(6, 16)
            .min(editor.height.saturating_sub(composer_h + 8))
    } else {
        0
    };
    let [tabs, body, footer, panel] = split_v(
        editor,
        [
            Constraint::Length(1),
            Constraint::Min(4),
            Constraint::Length(composer_h),
            Constraint::Length(panel_h),
        ],
    );
    workbench::tab_strip(f, app, tabs);
    app.ui.composer_frame = footer;
    let mut cursor = Position::new(footer.x, footer.y);
    match tab {
        Tab::Settings => {
            app.ui.composer_snap = None;
            if let Some(pos) = settings::draw(f, app, body) {
                cursor = pos;
            }
        }
        Tab::Agent(_) => {
            chat::draw(f, app, body);
            app.ui.composer_snap = None;
            workbench::readonly_bar(f, app, footer);
        }
        Tab::Chat => {
            chat::draw(f, app, body);
            cursor = if freeze_composer && composer_snap_fits(app) {
                if let Some(snap) = &app.ui.composer_snap {
                    copy_rect(snap, f.buffer_mut(), footer);
                }
                app.ui.last_caret
            } else {
                let pos = composer::draw(f, app, footer);
                app.ui.composer_snap = Some(clone_rect(f.buffer_mut(), footer));
                pos
            };
        }
    }
    app.ui.bottom_area = Rect::default();
    if panel_h > 0 {
        workbench::bottom_panel(f, app, panel);
    }
    if app.ui.side_open && !docked {
        // Narrow terminals: the side bar floats over the editor.
        let over = Rect::new(editor.x, editor.y, (SIDEBAR_W + 6).min(editor.width), work.height);
        f.render_widget(Clear, over);
        caret = workbench::side_bar(f, app, over);
    }
    if app.ui.focus == Focus::Rename {
        if let Some(pos) = caret {
            cursor = pos;
        }
    }
    if let Some(pos) = overlays::draw(f, app) {
        cursor = pos;
    }
    crate::preview::reveal_obscured_graphics(f.buffer_mut(), &mut app.images.blits);
    cursor
}

fn split_v<const N: usize>(area: Rect, constraints: [Constraint; N]) -> [Rect; N] {
    let rects = Layout::default()
        .direction(Direction::Vertical)
        .constraints(constraints)
        .split(area);
    std::array::from_fn(|i| rects[i])
}

/// The hardware cursor shows where typing goes.
pub(crate) fn wants_cursor(app: &App) -> bool {
    if app.ui.workspace_pick.as_ref().is_some_and(|p| p.path_focus)
        || app.cur().ask.as_ref().is_some_and(|a| a.filling)
        || matches!(app.ui.task_ui, Some(crate::tui::app::TaskUi::Form(_)))
        || app.ui.skill_view.is_some()
        || app.ui.focus == Focus::Rename
    {
        return true;
    }
    if app.has_modal() {
        return false;
    }
    match app.active_tab() {
        Tab::Settings => app.ui.focus == Focus::Settings && app.settings.drop.is_none(),
        Tab::Chat => app.ui.focus == Focus::Chat,
        Tab::Agent(_) => false,
    }
}

/// Side-bar or tab spinners are live: a full redraw is needed each tick.
pub(crate) fn spinners_need_redraw(app: &App) -> bool {
    if app.has_modal() {
        return false;
    }
    app.sessions.values().any(|s| {
        s.agents.live_count() > 0
            || s.backgrounds.iter().any(|b| b.alive)
            || s.monitors.iter().any(|m| m.alive)
    })
}

fn status_text(app: &App) -> (String, String) {
    let s = app.cur();
    let t = &s.chat;
    let state = if s.ask.is_some() {
        "等你回覆".to_string()
    } else {
        t.status.clone()
    };
    let mut left = if t.running {
        let act = if t.activity.is_empty() { t.status.clone() } else { t.activity.clone() };
        let clock = t
            .work_started
            .map(|w| md::fmt_duration_field(w.elapsed().as_millis() as u64))
            .unwrap_or_else(|| " ".repeat(md::DURATION_FIELD));
        format!(" {} {act}  {clock}  · {state}  Esc 中斷", spinner(app.tick))
    } else {
        format!(" ● {state}")
    };
    let agents = s.agents.alive_count();
    if agents > 0 {
        left.push_str(&format!("  ◎ 代理 {}/{agents}", s.agents.live_count()));
    }
    let running_tools = s.agents.tool_log.iter().filter(|x| !x.done).count();
    if running_tools > 0 {
        left.push_str(&format!("  ⚙ 工具 {running_tools}"));
    }
    left.push_str(&format!("  {}", if app.settings.logged_in { "已登入" } else { "未登入" }));
    if !t.running {
        left.push_str(&format!("  {}", t.cache));
        if let Some(url) = &app.web.url {
            left.push_str(&format!("  {url}"));
        }
    }
    let effort = if app
        .settings
        .catalog
        .find(&app.opts.model)
        .is_some_and(|m| !m.send_reasoning())
    {
        String::new()
    } else {
        format!(" · {}", app.opts.reasoning_effort.as_str())
    };
    let task = if s.task.snapshot().phase.is_live() { "任務●" } else { "任務" };
    let right = format!(" {task}  {}{effort}  ⚙ ", app.opts.model);
    (left, right)
}

fn render_status(buf: &mut Buffer, app: &App, area: Rect) -> (Rect, Rect, Rect) {
    let (left, right) = status_text(app);
    let running = app.cur().chat.running;
    let bg = if running { Color::Rgb(0, 122, 204) } else { Color::Rgb(45, 45, 45) };
    let base = Style::default().bg(bg).fg(Color::White);
    Block::default().style(base).render(area, buf);
    let rw = display_cols(&right).min(area.width);
    let lw = area.width.saturating_sub(rw);
    Paragraph::new(Span::styled(truncate(&left, lw), base.add_modifier(if running { Modifier::BOLD } else { Modifier::empty() })))
        .render(Rect::new(area.x, area.y, lw, 1), buf);
    let right_r = Rect::new(area.x + lw, area.y, rw, 1);
    Paragraph::new(Span::styled(right.clone(), base)).render(right_r, buf);
    let task_w = display_cols(right.split("  ").next().unwrap_or("")) + 1;
    let task = Rect::new(right_r.x, area.y, task_w.min(rw), 1);
    let gear = Rect::new(right_r.right().saturating_sub(3), area.y, 3.min(rw), 1);
    (right_r, task, gear)
}

fn draw_status(f: &mut Frame, app: &mut App, area: Rect) {
    app.ui.status_bar = area;
    let (model, task, gear) = render_status(f.buffer_mut(), app, area);
    app.ui.hits.push((model, Hit::StatusModel));
    app.ui.hits.push((task, Hit::StatusTask));
    app.ui.hits.push((gear, Hit::ActivitySettings));
    if app.cur().agents.alive_count() > 0 {
        app.ui.hits.push((Rect::new(area.x, area.y, area.width / 3, 1), Hit::StatusAgents));
    }
}

/// Cells of every ticking clock (status bar, live think headers).
pub(crate) fn clock_cells(app: &App) -> Vec<(u16, u16, Cell)> {
    let mut out = Vec::new();
    let bar = app.ui.status_bar;
    if bar.width > 0 {
        let mut buf = Buffer::empty(bar);
        render_status(&mut buf, app, bar);
        push_cells(&buf, bar, &mut out);
    }
    let rows = &app.cur().view_transcript().rows;
    for &(ri, area) in &app.ui.think_clocks {
        let Some(Row::Think(t)) = rows.get(ri) else { continue };
        if t.done || area.width == 0 || rects_overlap(area, app.ui.composer_frame) {
            continue;
        }
        let mut buf = Buffer::empty(area);
        Paragraph::new(chat::think_header(t))
            .style(Style::default().bg(PANEL))
            .render(area, &mut buf);
        push_cells(&buf, area, &mut out);
    }
    out.retain(|(x, y, _)| !app.ui.composer_frame.contains(Position::new(*x, *y)));
    out
}

fn push_cells(buf: &Buffer, area: Rect, out: &mut Vec<(u16, u16, Cell)>) {
    for y in area.top()..area.bottom() {
        for x in area.left()..area.right() {
            if let Some(cell) = buf.cell(Position::new(x, y)) {
                out.push((x, y, cell.clone()));
            }
        }
    }
}

fn rects_overlap(a: Rect, b: Rect) -> bool {
    a.width > 0 && b.width > 0 && a.intersects(b)
}

fn copy_rect(src: &Buffer, dest: &mut Buffer, area: Rect) {
    for y in area.top()..area.bottom() {
        for x in area.left()..area.right() {
            let p = Position::new(x, y);
            if let (Some(cell), Some(out)) = (src.cell(p), dest.cell_mut(p)) {
                *out = cell.clone();
            }
        }
    }
}

fn clone_rect(src: &Buffer, area: Rect) -> Buffer {
    let mut dest = Buffer::empty(area);
    copy_rect(src, &mut dest, area);
    dest
}

fn composer_snap_fits(app: &App) -> bool {
    app.ui
        .composer_snap
        .as_ref()
        .is_some_and(|b| *b.area() == app.ui.composer_frame)
        && app.ui.composer_frame.area() > 0
}

/// Clip `s` to `cols` display cells and pad to exactly that width.
pub(crate) fn truncate(s: &str, cols: u16) -> String {
    let max = cols as usize;
    let mut out = String::new();
    let mut w = 0usize;
    for c in s.chars() {
        let cw = ch_width(c) as usize;
        if w + cw > max {
            break;
        }
        out.push(c);
        w += cw;
    }
    out.extend(std::iter::repeat(' ').take(max.saturating_sub(w)));
    out
}

pub(crate) fn line(f: &mut Frame, area: Rect, text: impl Into<String>, style: Style) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    let text = text.into();
    f.render_widget(Paragraph::new(Span::styled(truncate(&text, area.width), style)), area);
}

/// A bordered panel with a title and a × close target.
pub(crate) fn panel(f: &mut Frame, app: &mut App, r: Rect, title: &str) -> Rect {
    f.render_widget(Clear, r);
    f.render_widget(
        Block::default()
            .title(format!(" {title} "))
            .title_style(Style::default().fg(TEXT).add_modifier(Modifier::BOLD))
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(ACCENT))
            .style(Style::default().bg(COMPOSER).fg(TEXT)),
        r,
    );
    let close = Rect::new(r.right().saturating_sub(4), r.y, 3, 1);
    f.render_widget(Paragraph::new(Span::styled(" × ", Style::default().fg(WARN).bg(COMPOSER))), close);
    app.ui.hits.push((r, Hit::Overlay));
    app.ui.hits.push((close, Hit::OverlayClose));
    Rect::new(r.x + 2, r.y + 1, r.width.saturating_sub(4), r.height.saturating_sub(2))
}

pub(crate) fn centered(area: Rect, w: u16, h: u16) -> Rect {
    let w = w.min(area.width);
    let h = h.min(area.height);
    Rect::new(area.x + (area.width - w) / 2, area.y + (area.height - h) / 2, w, h)
}

/// A clickable chip; returns its rect (or `None` if it does not fit).
pub(crate) fn chip(f: &mut Frame, app: &mut App, x: u16, y: u16, right: u16, label: &str, on: bool, hit: Hit) -> Option<Rect> {
    let w = display_cols(label);
    if x + w > right {
        return None;
    }
    let r = Rect::new(x, y, w, 1);
    let style = if on {
        Style::default().bg(ACCENT).fg(Color::Black).add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(TEXT).bg(COMPOSER)
    };
    f.render_widget(Paragraph::new(Span::styled(label.to_string(), style)), r);
    app.ui.hits.push((r, hit));
    Some(r)
}

/// A wrapped multi-line text field; returns the caret position.
pub(crate) fn draw_edit(f: &mut Frame, inner: Rect, edit: &Edit, vscroll: &mut u16, bg: Color) -> Position {
    let width = inner.width.max(1);
    let height = inner.height.max(1);
    let ranges = wrap_lines(&edit.text, width);
    let (crow, ccol) = caret_row_col(&edit.text, &ranges, edit.caret);
    if crow < *vscroll {
        *vscroll = crow;
    } else if crow >= *vscroll + height {
        *vscroll = crow + 1 - height;
    }
    let sel = edit.sel_range();
    let chars: Vec<char> = edit.text.chars().collect();
    let start = *vscroll as usize;
    let end = (start + height as usize).min(ranges.len());
    let lines: Vec<Line<'static>> = ranges[start..end]
        .iter()
        .map(|&(a, b)| edit_line(&chars, a, b, sel, bg))
        .collect();
    f.render_widget(Paragraph::new(lines).style(Style::default().fg(TEXT).bg(bg)), inner);
    Position::new(
        inner.x + ccol.min(width - 1),
        inner.y + crow.saturating_sub(*vscroll).min(height - 1),
    )
}

fn edit_line(chars: &[char], a: usize, b: usize, sel: Option<(usize, usize)>, bg: Color) -> Line<'static> {
    let plain = Style::default().fg(TEXT).bg(bg);
    let Some((lo, hi)) = sel else {
        return Line::from(Span::styled(chars[a..b].iter().collect::<String>(), plain));
    };
    let mut spans = Vec::new();
    let mut i = a;
    while i < b {
        let selected = i >= lo && i < hi;
        let mut j = i + 1;
        while j < b && (j >= lo && j < hi) == selected {
            j += 1;
        }
        let style = if selected { Style::default().bg(ACCENT).fg(Color::Black) } else { plain };
        spans.push(Span::styled(chars[i..j].iter().collect::<String>(), style));
        i = j;
    }
    Line::from(spans)
}

/// A one-line boxed field (settings); `mask` hides the text.
pub(crate) fn boxed_edit(f: &mut Frame, app: &mut App, area: Rect, edit: &Edit, focus: bool, hit: Hit, mask: bool) -> Position {
    f.render_widget(
        Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(if focus { ACCENT } else { BORDER }))
            .style(Style::default().bg(COMPOSER)),
        area,
    );
    app.ui.hits.push((area, hit));
    let inner = Rect::new(area.x + 1, area.y + 1, area.width.saturating_sub(2), 1);
    if inner.width == 0 {
        return Position::new(area.x, area.y);
    }
    // Scroll horizontally so the caret stays visible.
    let chars: Vec<char> = if mask {
        vec!['•'; edit.len()]
    } else {
        edit.text.chars().collect()
    };
    let mut start = 0usize;
    let caret_w = |from: usize| -> u16 { chars[from..edit.caret.min(chars.len())].iter().map(|c| ch_width(*c).max(1)).sum() };
    while start < edit.caret && caret_w(start) >= inner.width {
        start += 1;
    }
    let shown: String = chars[start..].iter().collect();
    f.render_widget(
        Paragraph::new(Span::styled(truncate(&shown, inner.width), Style::default().fg(TEXT).bg(COMPOSER))),
        inner,
    );
    if focus {
        app.ui.field_inner = inner;
    }
    Position::new(inner.x + caret_w(start).min(inner.width - 1), inner.y)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncate_pads_and_clips_by_width() {
        assert_eq!(truncate("ab", 4), "ab  ");
        assert_eq!(truncate("你好世界", 5), "你好 ");
    }
}
