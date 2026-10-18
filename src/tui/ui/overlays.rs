//! Modal overlays: questionnaire, task mode, workspace picker, image viewer,
//! skill viewer, monitor inspector. At most one is shown.

use ratatui::layout::{Position, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Paragraph, Wrap};
use ratatui::Frame;

use super::chat::{blit, wrap};
use super::{centered, chip, draw_edit, line, panel};
use crate::folderpick;
use crate::task::{TaskPhase, TaskState};
use crate::tui::app::{task_actions, App, Hit, TaskAction, TaskUi};
use crate::tui::edit::{caret_row_col, display_cols, wrap_lines};
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
    let (prompt, n, filling, from_proxy) = {
        let a = app.cur().ask.as_ref()?;
        (a.question.prompt.clone(), a.n(), a.filling, a.from_proxy)
    };
    let inner_w = area.width.saturating_sub(10).clamp(36.min(area.width), 72);
    let q_rows = wrap_lines(&prompt, inner_w.saturating_sub(2)).len().max(1) as u16;
    let fill_h = if filling { 3 } else { 0 };
    let head_h = if from_proxy { 2 } else { 0 };
    let h = (q_rows + head_h + n as u16 + fill_h + 6).min(area.height.saturating_sub(2)).max(10);
    let r = centered(area, inner_w + 4, h);
    let title = if from_proxy {
        "任務代理提問"
    } else if prompt.starts_with("代理要求把這個資料夾加入工作區") {
        "工作區存取請求"
    } else {
        "問卷"
    };
    let body = panel(f, app, r, title);
    if from_proxy {
        // Not the agent asking: the proxy wants the final result pinned down.
        line(f, Rect::new(body.x, body.y, body.width, 1), "任務代理想確認最終效果", Style::default().fg(PROXY).add_modifier(Modifier::BOLD));
    }
    f.render_widget(
        Paragraph::new(prompt).style(Style::default().fg(TEXT).add_modifier(Modifier::BOLD)).wrap(Wrap { trim: true }),
        Rect::new(body.x, body.y + head_h, body.width, q_rows),
    );
    let mut y = body.y + head_h + q_rows + 1;
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
    if matches!(app.ui.task_ui, Some(TaskUi::Form(_))) {
        return task_form(f, app, area);
    }
    task_status(f, app, area);
    None
}

fn task_form(f: &mut Frame, app: &mut App, area: Rect) -> Option<Position> {
    let (editing, on_criteria) = match &app.ui.task_ui {
        Some(TaskUi::Form(t)) => (t.editing, t.on_criteria),
        _ => return None,
    };
    let phase = app.cur().task.snapshot().phase;
    let title = if !editing {
        "任務目標"
    } else if phase.is_live() || phase == TaskPhase::Done {
        "修改任務"
    } else {
        "修改並重新開始任務"
    };
    let w = area.width.saturating_sub(8).clamp(40.min(area.width), 80);
    let r = centered(area, w, area.height.saturating_sub(2).clamp(13.min(area.height), 24));
    let body = panel(f, app, r, title);
    line(f, Rect::new(body.x, body.y, body.width, 1), "Tab 切換欄位 · Enter/Ctrl+S 確定 · Shift+Enter 換行 · Esc 取消", Style::default().fg(DIM));
    // Hint, two labels, a gap and the buttons; the fields share the rest.
    let rem = body.height.saturating_sub(5);
    let crit_h = (rem * 2 / 5).max(3);
    let goal_h = rem.saturating_sub(crit_h).max(3);
    let label = |on: bool| Style::default().fg(if on { ACCENT } else { DIM });
    line(f, Rect::new(body.x, body.y + 1, body.width, 1), "目標（必填）", label(!on_criteria));
    let goal_box = Rect::new(body.x, body.y + 2, body.width, goal_h);
    line(f, Rect::new(body.x, goal_box.bottom(), body.width, 1), "驗收條件／偏好（選填）", label(on_criteria));
    let crit_box = Rect::new(body.x, goal_box.bottom() + 1, body.width, crit_h);
    let mut caret = None;
    for (i, (boxr, hit)) in [(goal_box, Hit::TaskDraft), (crit_box, Hit::TaskCriteria)].into_iter().enumerate() {
        let on = on_criteria == (i == 1);
        f.render_widget(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(if on { ACCENT } else { BORDER }))
                .style(Style::default().bg(PANEL)),
            boxr,
        );
        let inner = Rect::new(boxr.x + 1, boxr.y + 1, boxr.width.saturating_sub(2), boxr.height.saturating_sub(2));
        app.ui.task_fields[i] = inner;
        app.ui.hits.push((boxr, hit));
        if let Some(TaskUi::Form(t)) = app.ui.task_ui.as_mut() {
            let edit = if i == 0 { &t.goal } else { &t.criteria };
            let pos = draw_edit(f, inner, edit, &mut t.scroll[i], PANEL);
            if on {
                caret = Some(pos);
            }
        }
    }
    buttons(f, app, body.x, body.bottom() - 1, body.right(), &[(" 確定 ", true, Hit::TaskConfirm), (" 取消 ", false, Hit::TaskCancel)]);
    caret
}

/// A button's label: the action and the letter that runs it.
fn action_label(a: TaskAction) -> String {
    match a.key() {
        Some(k) => format!(" {}({}) ", a.label(), k.to_ascii_uppercase()),
        None => format!(" {} ", a.label()),
    }
}

/// Rows the buttons take when wrapped to `width`.
fn button_rows(labels: &[String], width: u16) -> u16 {
    let mut rows = 1;
    let mut x = 0u16;
    for l in labels {
        let w = display_cols(l);
        if x > 0 && x + w > width {
            rows += 1;
            x = 0;
        }
        x += w + 1;
    }
    rows
}

fn push_text(out: &mut Vec<Line<'static>>, text: &str, width: u16, style: Style) {
    let chars: Vec<char> = text.trim_end().chars().collect();
    let text: String = chars.iter().collect();
    for (a, b) in wrap_lines(&text, width.saturating_sub(2).max(1)) {
        let piece: String = chars[a.min(chars.len())..b.min(chars.len())].iter().collect();
        out.push(Line::from(Span::styled(format!("  {piece}"), style)));
    }
}

/// Everything the status dialog says about the task, wrapped to `width`.
fn task_status_lines(t: &TaskState, width: u16) -> Vec<Line<'static>> {
    let head = |s: &str, color| Line::from(Span::styled(s.to_string(), Style::default().fg(color).add_modifier(Modifier::BOLD)));
    let text = Style::default().fg(TEXT);
    let mut out = Vec::new();
    if t.phase == TaskPhase::Inactive {
        // Ended elsewhere (the web page) while this dialog was open.
        push_text(&mut out, "沒有進行中的任務。關閉後按 Ctrl+T 設定新目標。", width, Style::default().fg(DIM));
        return out;
    }
    let mut state = t.phase.label().to_string();
    if t.paused {
        state.push_str(" · 已暫停");
    }
    state.push_str(&format!(" · 回合 {}", t.rounds));
    out.push(Line::from(Span::styled(format!("狀態  {state}"), Style::default().fg(ACCENT).add_modifier(Modifier::BOLD))));
    out.push(Line::from(""));
    out.push(head("目標", DIM));
    push_text(&mut out, &t.goal, width, text);
    if !t.criteria.trim().is_empty() {
        out.push(Line::from(""));
        out.push(head("驗收條件／偏好", DIM));
        push_text(&mut out, &t.criteria, width, text);
    }
    if !t.last_proxy.trim().is_empty() {
        out.push(Line::from(""));
        out.push(head("用戶代理最新訊息", PROXY));
        push_text(&mut out, &t.last_proxy, width, text);
    }
    out.push(Line::from(""));
    let done = t.checklist.iter().filter(|i| i.done).count();
    out.push(head(&format!("進度筆記  {done}/{}", t.checklist.len()), DIM));
    if t.checklist.is_empty() {
        out.push(Line::from(Span::styled("  （用戶代理尚未寫下進度）", Style::default().fg(DIM))));
    }
    for item in &t.checklist {
        let style = Style::default().fg(if item.done { AGENT } else { TEXT });
        push_text(&mut out, &format!("{} {}", if item.done { "✓" } else { "□" }, item.text), width, style);
    }
    if !t.review_note.trim().is_empty() {
        out.push(Line::from(""));
        push_text(&mut out, &t.review_note, width, Style::default().fg(DIM));
    }
    out
}

fn task_status(f: &mut Frame, app: &mut App, area: Rect) {
    let snap = app.cur().task.snapshot();
    let acts = task_actions(&snap);
    let (focus, scroll) = match &app.ui.task_ui {
        Some(TaskUi::Status(st)) => (st.focus, st.scroll),
        _ => return,
    };
    let w = area.width.saturating_sub(8).clamp(40.min(area.width), 84);
    let text_w = w.saturating_sub(4);
    let lines = task_status_lines(&snap, text_w);
    let labels: Vec<String> = acts.iter().map(|a| action_label(*a)).collect();
    let btn_rows = button_rows(&labels, text_w);
    // Borders, the hint and the button rows around the text.
    let chrome = 3 + btn_rows;
    let h = (lines.len() as u16 + chrome).min(area.height.saturating_sub(2)).max(10.min(area.height));
    let body = panel(f, app, centered(area, w, h), "任務模式");
    let text_h = body.height.saturating_sub(1 + btn_rows);
    let max_off = (lines.len() as u16).saturating_sub(text_h);
    let scroll = scroll.min(max_off);
    if let Some(TaskUi::Status(st)) = app.ui.task_ui.as_mut() {
        st.scroll = scroll;
    }
    let shown: Vec<Line<'static>> = lines.into_iter().skip(scroll as usize).take(text_h as usize).collect();
    f.render_widget(Paragraph::new(shown), Rect::new(body.x, body.y, body.width, text_h));
    let hint = if max_off > 0 {
        "←→ 選擇 · Enter 執行 · 字母鍵直接執行 · ↑↓ 捲動 · Esc 關閉"
    } else {
        "←→ 選擇 · Enter 執行 · 字母鍵直接執行 · Esc 關閉"
    };
    let hint_y = body.bottom().saturating_sub(btn_rows + 1);
    line(f, Rect::new(body.x, hint_y, body.width, 1), hint, Style::default().fg(DIM));
    let (mut x, mut y) = (body.x, hint_y + 1);
    for (a, label) in acts.iter().zip(&labels) {
        let w = display_cols(label);
        if x > body.x && x + w > body.right() {
            x = body.x;
            y += 1;
        }
        if y >= body.bottom() {
            break;
        }
        if let Some(r) = chip(f, app, x, y, body.right(), label, a.same(focus), Hit::TaskAct(*a)) {
            x = r.right() + 1;
        }
    }
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
    let path = app.workspace().join(&rel);
    let key = app.images.key(path);
    let (w, h) = app.images.cells(&key, inner.width, inner.height, false);
    if app.images.defer(&key, w, h) {
        line(f, inner, "…", Style::default().fg(DIM));
        return;
    }
    if !app.images.uses_graphics() {
        let buf = f.buffer_mut();
        for (i, l) in app.images.halfblock(&key, w, h).iter().take(inner.height as usize).enumerate() {
            buf.set_line(inner.x, inner.y + i as u16, l, inner.width);
        }
        return;
    }
    match app.images.graphic(&key, w, h) {
        Some(g) => {
            let pa = g.area();
            let draw = Rect::new(inner.x, inner.y, pa.width.min(inner.width), pa.height.min(inner.height));
            if draw.area() > 0 {
                blit(f, &mut app.images, &g, draw);
            }
        }
        None => line(f, inner, "[無法預覽]", Style::default().fg(DIM)),
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
