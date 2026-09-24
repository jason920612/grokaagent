//! Modal overlays: questionnaire, task mode, workspace picker, image viewer,
//! skill viewer, monitor inspector. At most one is shown.

use ratatui::layout::{Position, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Paragraph, Wrap};
use ratatui::Frame;

use super::chat::{blit, cached_proto, wrap};
use super::{centered, chip, draw_edit, line, panel};
use crate::folderpick;
use crate::tui::app::{App, Hit, TaskUi};
use crate::tui::edit::{caret_row_col, wrap_lines};
use crate::tui::theme::*;

/// Draw the open overlay; returns the caret if it has a text field.
pub(crate) fn draw(f: &mut Frame, app: &mut App) -> Option<Position> {
    let area = f.area();
    if app.ui.workspace_pick.is_some() {
        return workspace(f, app, area);
    }
    if app.cur().ask.is_some() {
        return ask(f, app, area);
    }
    if app.ui.task_ui.is_some() {
        return task(f, app, area);
    }
    if app.ui.image_view.is_some() {
        image(f, app, area);
        return None;
    }
    if app.ui.skill_view.is_some() {
        return skill(f, app, area);
    }
    if app.ui.inspector.is_some() {
        inspector(f, app, area);
    }
    None
}

fn buttons(f: &mut Frame, app: &mut App, x: u16, y: u16, right: u16, items: &[(&str, bool, Hit)]) {
    let mut x = x;
    for (label, primary, hit) in items {
        if let Some(r) = chip(f, app, x, y, right, label, *primary, *hit) {
            x = r.right() + 2;
        }
    }
}

fn ask(f: &mut Frame, app: &mut App, area: Rect) -> Option<Position> {
    let (prompt, n, filling) = {
        let a = app.cur().ask.as_ref()?;
        (a.question.prompt.clone(), a.n(), a.filling)
    };
    let inner_w = area.width.saturating_sub(10).clamp(36.min(area.width), 72);
    let q_rows = wrap_lines(&prompt, inner_w.saturating_sub(2)).len().max(1) as u16;
    let fill_h = if filling { 3 } else { 0 };
    let h = (q_rows + n as u16 + fill_h + 6).min(area.height.saturating_sub(2)).max(10);
    let r = centered(area, inner_w + 4, h);
    let body = panel(f, app, r, "問卷");
    f.render_widget(
        Paragraph::new(prompt).style(Style::default().fg(TEXT).add_modifier(Modifier::BOLD)).wrap(Wrap { trim: true }),
        Rect::new(body.x, body.y, body.width, q_rows),
    );
    let mut y = body.y + q_rows + 1;
    let rows: Vec<(String, Style)> = {
        let a = app.cur().ask.as_ref()?;
        (0..n)
            .map(|i| {
                let opt = &a.question.options[i];
                let chosen = a.chosen.get(i).copied().unwrap_or(false);
                let mark = match (a.question.allow_multiple, chosen) {
                    (true, true) => "☑",
                    (true, false) => "☐",
                    (false, true) => "●",
                    (false, false) => "○",
                };
                let pointer = if i == a.cursor { "›" } else { " " };
                let extra = if opt.input {
                    let v = a.values.get(i).map(String::as_str).unwrap_or("");
                    if v.is_empty() { "  （可填寫）".to_string() } else { format!("  「{v}」") }
                } else {
                    String::new()
                };
                let style = if i == a.cursor {
                    Style::default().fg(Color::Black).bg(ACCENT)
                } else if chosen {
                    Style::default().fg(ACCENT)
                } else {
                    Style::default().fg(TEXT)
                };
                (format!(" {pointer} {mark}  {}{extra} ", opt.label), style)
            })
            .collect()
    };
    for (i, (label, style)) in rows.into_iter().enumerate() {
        let row = Rect::new(body.x, y, body.width, 1);
        line(f, row, label, style);
        app.ui.hits.push((row, Hit::AskOption(i as u16)));
        y += 1;
    }
    let mut caret = None;
    if filling {
        let boxr = Rect::new(body.x, y, body.width, 3);
        f.render_widget(
            Block::default()
                .borders(Borders::ALL)
                .border_type(BorderType::Rounded)
                .border_style(Style::default().fg(ACCENT))
                .style(Style::default().bg(PANEL)),
            boxr,
        );
        let inner = Rect::new(boxr.x + 1, boxr.y + 1, boxr.width.saturating_sub(2), 1);
        app.ui.field_inner = inner;
        app.ui.hits.push((boxr, Hit::AskFill));
        if let Some(a) = app.cur().ask.as_ref() {
            let mut vs = 0;
            caret = Some(draw_edit(f, inner, &a.fill, &mut vs, PANEL));
        }
        y += 3;
    }
    buttons(f, app, body.x, y, body.right(), &[(" 確定 ", true, Hit::AskConfirm), (" 取消 ", false, Hit::AskCancel)]);
    let hint = if filling {
        "輸入自訂內容 · Enter 確定 · Esc 回到選項"
    } else if app.cur().ask.as_ref().is_some_and(|a| a.question.allow_multiple) {
        "↑↓ 移動 · 空白鍵勾選 · Enter 確定 · Esc 取消"
    } else {
        "↑↓ 移動 · Enter 選擇 · 可填寫項會開啟輸入框 · Esc 取消"
    };
    if y + 1 < body.bottom() {
        line(f, Rect::new(body.x, y + 1, body.width, 1), hint, Style::default().fg(DIM));
    }
    caret
}

fn task(f: &mut Frame, app: &mut App, area: Rect) -> Option<Position> {
    let form = matches!(app.ui.task_ui, Some(TaskUi::Form(_)));
    let r = centered(area, area.width.saturating_sub(8).clamp(40.min(area.width), 76), if form { 14 } else { 18 });
    let body = panel(f, app, r, if form { "任務目標" } else { "任務模式" });
    if form {
        line(f, Rect::new(body.x, body.y, body.width, 1), "寫下這則對話要達成的目標。Enter 確定 · Shift+Enter 換行 · Esc 取消", Style::default().fg(DIM));
        let boxr = Rect::new(body.x, body.y + 2, body.width, body.height.saturating_sub(4).max(3));
        f.render_widget(
            Block::default().borders(Borders::ALL).border_style(Style::default().fg(BORDER)).style(Style::default().bg(PANEL)),
            boxr,
        );
        let inner = Rect::new(boxr.x + 1, boxr.y + 1, boxr.width.saturating_sub(2), boxr.height.saturating_sub(2));
        app.ui.field_inner = inner;
        app.ui.hits.push((inner, Hit::TaskDraft));
        let caret = match &app.ui.task_ui {
            Some(TaskUi::Form(edit)) => {
                let mut vs = 0;
                draw_edit(f, inner, edit, &mut vs, PANEL)
            }
            _ => Position::new(inner.x, inner.y),
        };
        buttons(f, app, body.x, body.bottom() - 1, body.right(), &[(" 確定 ", true, Hit::TaskConfirm), (" 取消 ", false, Hit::TaskCancel)]);
        return Some(caret);
    }
    let snap = app.cur().task.snapshot();
    let phase = if snap.skip_steer { "已暫停" } else { snap.phase.label() };
    line(f, Rect::new(body.x, body.y, body.width, 1), format!("狀態  {phase}"), Style::default().fg(ACCENT));
    line(f, Rect::new(body.x, body.y + 1, body.width, 1), format!("目標  {}", snap.goal.replace('\n', " ")), Style::default().fg(TEXT));
    let list_h = body.height.saturating_sub(5);
    f.render_widget(
        Paragraph::new(snap.checklist_text()).style(Style::default().fg(TEXT)).wrap(Wrap { trim: false }),
        Rect::new(body.x, body.y + 3, body.width, list_h),
    );
    if !snap.review_note.is_empty() && list_h > 1 {
        line(f, Rect::new(body.x, body.bottom() - 2, body.width, 1), snap.review_note.clone(), Style::default().fg(DIM));
    }
    buttons(f, app, body.x, body.bottom() - 1, body.right(), &[(" 關閉 ", false, Hit::TaskCancel), (" 結束任務 ", false, Hit::TaskEnd)]);
    None
}

fn workspace(f: &mut Frame, app: &mut App, area: Rect) -> Option<Position> {
    let r = centered(area, area.width.saturating_sub(6).clamp(42.min(area.width), 92), area.height.saturating_sub(4).clamp(14.min(area.height), 28));
    let body = panel(f, app, r, "選擇工作目錄");
    line(f, Rect::new(body.x, body.y, body.width, 1), "輸入路徑或名稱篩選 · 點資料夾進入 · 點檔案選上層", Style::default().fg(DIM));
    let pathr = Rect::new(body.x, body.y + 1, body.width, 3);
    let path_focus = app.ui.workspace_pick.as_ref().is_some_and(|p| p.path_focus);
    f.render_widget(
        Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(if path_focus { ACCENT } else { BORDER }))
            .style(Style::default().bg(PANEL)),
        pathr,
    );
    let path_inner = Rect::new(pathr.x + 1, pathr.y + 1, pathr.width.saturating_sub(2), 1);
    app.ui.hits.push((pathr, Hit::WsPath));
    app.ui.field_inner = path_inner;
    let mut caret = None;
    if let Some(p) = &app.ui.workspace_pick {
        let mut vs = 0;
        caret = Some(draw_edit(f, path_inner, &p.edit, &mut vs, PANEL));
    }
    let list = Rect::new(body.x, pathr.bottom(), body.width, body.height.saturating_sub(7));
    let entries: Vec<(usize, String, bool, bool)> = {
        let Some(p) = app.ui.workspace_pick.as_mut() else { return None };
        let vis = list.height as usize;
        if p.cursor < p.scroll as usize {
            p.scroll = p.cursor as u16;
        } else if vis > 0 && p.cursor >= p.scroll as usize + vis {
            p.scroll = (p.cursor + 1 - vis) as u16;
        }
        p.view
            .entries
            .iter()
            .enumerate()
            .skip(p.scroll as usize)
            .take(vis)
            .map(|(i, e)| {
                let icon = if e.is_parent { "↑" } else if e.is_dir { "▸" } else { "·" };
                (i, format!(" {icon}  {} ", e.name), e.is_dir, i == p.cursor)
            })
            .collect()
    };
    if entries.is_empty() {
        line(f, Rect::new(list.x, list.y, list.width, 1.min(list.height)), "（沒有符合的項目）", Style::default().fg(DIM));
    }
    for (row, (i, label, dir, selected)) in entries.into_iter().enumerate() {
        let cell = Rect::new(list.x, list.y + row as u16, list.width, 1);
        let style = if selected {
            Style::default().fg(Color::Black).bg(ACCENT)
        } else if dir {
            Style::default().fg(USER)
        } else {
            Style::default().fg(TEXT)
        };
        line(f, cell, label, style);
        app.ui.hits.push((cell, Hit::WsEntry(i as u16)));
    }
    let by = list.bottom();
    buttons(
        f,
        app,
        body.x,
        by,
        body.right(),
        &[(" 選擇此資料夾 ", true, Hit::WsConfirm), (" 建立資料夾 ", false, Hit::WsCreate), (" 取消 ", false, Hit::WsCancel)],
    );
    if let Some(p) = &app.ui.workspace_pick {
        let notice = p.notice.clone().unwrap_or_else(|| "Enter 進入資料夾或確定 · Tab 切換輸入/清單 · Esc 取消".into());
        line(f, Rect::new(body.x, by + 1, body.width, 1), notice, Style::default().fg(DIM));
        line(f, Rect::new(body.x, by + 2, body.width, 1), folderpick::display_path(&p.view.cwd), Style::default().fg(DIM));
    }
    if path_focus { caret } else { None }
}

fn image(f: &mut Frame, app: &mut App, area: Rect) {
    let Some(rel) = app.ui.image_view.clone() else { return };
    let name = std::path::Path::new(&rel).file_name().and_then(|s| s.to_str()).unwrap_or(&rel).to_string();
    let r = centered(area, area.width * 9 / 10, area.height * 9 / 10);
    let body = panel(f, app, r, &format!("圖片  {name}  · Esc 關閉 · Ctrl+C 複製"));
    let inner = Rect::new(body.x - 1, body.y, body.width + 2, body.height);
    if inner.area() == 0 {
        return;
    }
    let ws = app.workspace();
    let abs = ws.join(&rel);
    match app.images.picker.clone().filter(crate::preview::uses_graphics) {
        Some(picker) => {
            let (w, h) = crate::preview::cell_size_fit(&picker, &abs, inner.width, inner.height);
            match cached_proto(&mut app.images, &ws, &rel, w, h) {
                Some(proto) => {
                    let pa = proto.area();
                    let draw = Rect::new(inner.x, inner.y, pa.width.min(inner.width), pa.height.min(inner.height));
                    if draw.area() > 0 {
                        blit(f, &mut app.images, &proto, draw);
                    }
                }
                None => line(f, inner, "[無法預覽]", Style::default().fg(DIM)),
            }
        }
        None => f.render_widget(Paragraph::new(crate::preview::from_path(&abs, inner.width, inner.height)), inner),
    }
}

fn skill(f: &mut Frame, app: &mut App, area: Rect) -> Option<Position> {
    let (title, origin) = {
        let v = app.ui.skill_view.as_ref()?;
        (v.title.clone(), v.origin.clone())
    };
    let r = centered(area, area.width * 9 / 10, area.height * 9 / 10);
    let body = panel(f, app, r, &format!("技能  {title} · {origin} · 唯讀 · 可選取複製 · Esc 關閉"));
    app.ui.skill_inner = body;
    app.ui.hits.push((body, Hit::SkillText));
    let v = app.ui.skill_view.as_mut()?;
    if body.area() == 0 {
        return None;
    }
    let mut text = Line::from(v.edit.text.clone());
    if let Some((lo, hi)) = v.edit.sel_range() {
        text = tint_range(text, lo, hi);
    }
    let lines = wrap(text, body.width.max(1));
    let max_off = (lines.len() as u16).saturating_sub(body.height);
    v.scroll = v.scroll.min(max_off);
    let shown: Vec<Line<'static>> = lines.into_iter().skip(v.scroll as usize).take(body.height as usize).collect();
    f.render_widget(Paragraph::new(shown), body);
    let ranges = wrap_lines(&v.edit.text, body.width.max(1));
    let (row, col) = caret_row_col(&v.edit.text, &ranges, v.edit.caret);
    (row >= v.scroll && row - v.scroll < body.height)
        .then(|| Position::new(body.x + col.min(body.width - 1), body.y + row - v.scroll))
}

fn tint_range(line: Line<'static>, lo: usize, hi: usize) -> Line<'static> {
    let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
    let chars: Vec<char> = text.chars().collect();
    let piece = |a: usize, b: usize| chars[a.min(chars.len())..b.min(chars.len())].iter().collect::<String>();
    Line::from(vec![
        Span::raw(piece(0, lo)),
        Span::styled(piece(lo, hi), Style::default().bg(SELECT_BG)),
        Span::raw(piece(hi, chars.len())),
    ])
}

fn inspector(f: &mut Frame, app: &mut App, area: Rect) {
    let Some(name) = app.ui.inspector.clone() else { return };
    let r = centered(area, area.width * 3 / 4, area.height * 3 / 5);
    let body = panel(f, app, r, &format!("監控  {name}"));
    let text = match app.cur().monitors.iter().find(|m| m.name == name) {
        Some(m) => format!(
            "{} {}\npid {}\n$ {}\n{}",
            if m.alive { "●" } else { "○" },
            m.status,
            m.pid,
            m.command,
            m.detail
        ),
        None => format!("找不到監控「{name}」"),
    };
    f.render_widget(Paragraph::new(text).style(Style::default().fg(TEXT)).wrap(Wrap { trim: false }), body);
}
