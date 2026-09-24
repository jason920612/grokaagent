//! Workbench chrome: activity bar, side views, tab strip, read-only footer
//! of agent tabs, and the bottom panel.

use ratatui::layout::{Position, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};
use ratatui::Frame;

use super::{draw_edit, line, truncate};
use crate::md;
use crate::tui::app::{App, BottomTab, Hit, SideItem, SideView, Tab};
use crate::tui::edit::display_cols;
use crate::tui::model::agents::{AgentState, EventKind};
use crate::tui::model::tool_text::spinner;
use crate::tui::theme::*;

fn state_color(s: AgentState) -> Color {
    match s {
        AgentState::Starting | AgentState::Working => ACCENT,
        AgentState::Idle => AGENT,
        AgentState::Paused | AgentState::Interrupted => TOOL,
        AgentState::Exited => DIM,
    }
}

pub(crate) fn activity_bar(f: &mut Frame, app: &mut App, area: Rect) {
    f.render_widget(Block::default().style(Style::default().bg(ACTIVITY_BG)), area);
    let s = app.cur();
    let badges: Vec<String> = SideView::ALL
        .iter()
        .map(|v| match v {
            SideView::Agents if s.agents.live_count() > 0 => s.agents.live_count().to_string(),
            SideView::Changes => {
                let n = s.file_changes().len();
                if n > 0 { n.min(99).to_string() } else { String::new() }
            }
            SideView::Background => {
                let n = s.backgrounds.iter().filter(|b| b.alive).count() + s.monitors.iter().filter(|m| m.alive).count();
                if n > 0 { n.to_string() } else { String::new() }
            }
            SideView::Task if s.task.snapshot().phase.is_live() => "●".into(),
            _ => String::new(),
        })
        .collect();
    let mut y = area.y;
    for (i, view) in SideView::ALL.iter().enumerate() {
        if y + 1 >= area.bottom() {
            break;
        }
        let on = app.ui.side_open && app.ui.side_view == *view;
        f.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled(if on { "▎" } else { " " }, Style::default().fg(ACCENT).bg(ACTIVITY_BG)),
                Span::styled(
                    view.icon(),
                    Style::default()
                        .fg(if on { TEXT } else { DIM })
                        .bg(ACTIVITY_BG)
                        .add_modifier(if on { Modifier::BOLD } else { Modifier::empty() }),
                ),
            ])),
            Rect::new(area.x, y, area.width, 1),
        );
        if !badges[i].is_empty() {
            line(f, Rect::new(area.x, y + 1, area.width, 1), format!(" {}", badges[i]), Style::default().fg(ACCENT).bg(ACTIVITY_BG));
        }
        app.ui.hits.push((Rect::new(area.x, y, area.width, 2), Hit::Activity(i as u8)));
        y += 2;
    }
    if area.height >= 2 {
        let gear = Rect::new(area.x, area.bottom() - 1, area.width, 1);
        line(f, gear, " ⚙", Style::default().fg(DIM).bg(ACTIVITY_BG));
        app.ui.hits.push((gear, Hit::ActivitySettings));
    }
}

/// Returns the rename caret when a session is being renamed.
pub(crate) fn side_bar(f: &mut Frame, app: &mut App, area: Rect) -> Option<Position> {
    app.ui.side_area = area;
    f.render_widget(
        Block::default()
            .borders(Borders::RIGHT)
            .border_style(Style::default().fg(BORDER))
            .style(Style::default().bg(PANEL).fg(TEXT)),
        area,
    );
    if area.width < 8 || area.height < 3 {
        return None;
    }
    let inner_w = area.width - 1;
    line(
        f,
        Rect::new(area.x, area.y, inner_w, 1),
        format!(" {}", app.ui.side_view.title()),
        Style::default().fg(DIM).bg(PANEL).add_modifier(Modifier::BOLD),
    );
    let body = Rect::new(area.x, area.y + 1, inner_w, area.height - 1);
    app.ui.hits.push((body, Hit::SideScroll));
    if app.ui.side_view == SideView::Sessions {
        return sessions(f, app, body);
    }
    let lines = match app.ui.side_view {
        SideView::Agents => agent_lines(app),
        SideView::Changes => change_lines(app),
        SideView::Background => background_lines(app),
        SideView::Task => task_lines(app),
        SideView::Sessions => unreachable!(),
    };
    paint_lines(f, app, body, lines);
    None
}

/// (text, color, item, highlighted)
type SideLine = (String, Color, Option<SideItem>, bool);

fn paint_lines(f: &mut Frame, app: &mut App, area: Rect, lines: Vec<SideLine>) {
    let view_h = area.height as usize;
    app.ui.side_scroll = app.ui.side_scroll.min(lines.len().saturating_sub(view_h));
    for (i, (text, color, item, on)) in lines.into_iter().skip(app.ui.side_scroll).take(view_h).enumerate() {
        let row = Rect::new(area.x, area.y + i as u16, area.width, 1);
        line(f, row, text, Style::default().fg(color).bg(if on { COMPOSER } else { PANEL }));
        if let Some(item) = item {
            let idx = app.ui.side_items.len() as u16;
            app.ui.side_items.push(item);
            app.ui.hits.push((row, Hit::SideItem(idx)));
        }
    }
}

fn sessions(f: &mut Frame, app: &mut App, area: Rect) -> Option<Position> {
    let new_btn = Rect::new(area.x, area.y, area.width, 1);
    line(f, new_btn, " + 新對話", Style::default().fg(ACCENT).bg(PANEL).add_modifier(Modifier::BOLD));
    app.ui.hits.push((new_btn, Hit::NewChat));
    let mut caret = None;
    let mut y = area.y + 2;
    let list = app.listed.clone();
    let btn_w = 6u16;
    for (i, meta) in list.iter().enumerate().skip(app.ui.side_scroll) {
        if y + 1 >= area.bottom() {
            break;
        }
        let current = meta.id == app.current;
        let (running, status) = match app.sessions.get(&meta.id) {
            Some(s) => (s.chat.running, if s.ask.is_some() { "等你回覆".to_string() } else { s.chat.status.clone() }),
            None => (false, "待命".to_string()),
        };
        let bg = if current { COMPOSER } else { PANEL };
        let text_w = area.width.saturating_sub(btn_w);
        app.ui.hits.push((Rect::new(area.x, y, area.width, 2), Hit::Session(i as u16)));
        let renaming = app.ui.rename.as_ref().is_some_and(|(id, _)| *id == meta.id);
        if renaming {
            let edit_area = Rect::new(area.x + 1, y, text_w.saturating_sub(1).max(1), 1);
            f.render_widget(Block::default().style(Style::default().bg(bg)), Rect::new(area.x, y, area.width, 1));
            if let Some((_, edit)) = app.ui.rename.as_ref() {
                let mut vs = 0;
                caret = Some(draw_edit(f, edit_area, edit, &mut vs, bg));
            }
        } else {
            let mark = if running { format!("{} ", spinner(app.tick)) } else { "  ".into() };
            line(
                f,
                Rect::new(area.x, y, text_w.max(1), 1),
                format!("{mark}{}", meta.name),
                Style::default().fg(TEXT).bg(bg).add_modifier(if current { Modifier::BOLD } else { Modifier::empty() }),
            );
        }
        if area.width >= btn_w + 4 {
            let edit_btn = Rect::new(area.x + text_w, y, 3, 1);
            let del_btn = Rect::new(area.x + text_w + 3, y, 3, 1);
            line(f, edit_btn, " ✎ ", Style::default().fg(DIM).bg(bg));
            line(f, del_btn, " × ", Style::default().fg(WARN).bg(bg));
            app.ui.hits.push((edit_btn, Hit::RenameSession(i as u16)));
            app.ui.hits.push((del_btn, Hit::DeleteSession(i as u16)));
        }
        line(
            f,
            Rect::new(area.x, y + 1, area.width, 1),
            format!("  {status} · {}", meta.folder_label()),
            Style::default().fg(DIM).bg(bg),
        );
        y += 3;
    }
    caret
}

fn agent_lines(app: &App) -> Vec<SideLine> {
    let s = app.cur();
    let active = s.active.clone();
    let root_state = if s.ask.is_some() { "等你回覆".to_string() } else { s.chat.status.clone() };
    let root_icon = if s.chat.running { spinner(app.tick).to_string() } else { "●".into() };
    let mut out: Vec<SideLine> = vec![
        (format!(" {root_icon} 主代理  {root_state}"), if s.chat.running { ACCENT } else { TEXT }, Some(SideItem::Root), active.is_none()),
        (format!("   {}", app.opts.model), DIM, Some(SideItem::Root), active.is_none()),
    ];
    if s.agents.nodes.is_empty() {
        for t in ["", " 尚無子代理", " 主代理呼叫 spawn_agent 後", " 會以樹狀出現在這裡，點選開啟"] {
            out.push((t.into(), DIM, None, false));
        }
    }
    for a in &s.agents.nodes {
        let indent = "  ".repeat(a.depth() + 1);
        let on = active.as_deref() == Some(a.path.as_str());
        let turn = if a.turn > 0 { format!(" · 第{}輪", a.turn) } else { String::new() };
        let item = Some(SideItem::Agent(a.path.clone()));
        out.push((
            format!(" {indent}{} {}  {}{turn}", a.state.icon(app.tick), a.name, a.state.label()),
            state_color(a.state),
            item.clone(),
            on,
        ));
        let detail = if a.state.live() && !a.transcript.activity.is_empty() {
            a.transcript.activity.clone()
        } else {
            a.model.clone()
        };
        if !detail.is_empty() {
            out.push((format!(" {indent}  {detail}"), DIM, item, on));
        }
    }
    out
}

fn change_lines(app: &App) -> Vec<SideLine> {
    let changes = app.cur().file_changes();
    let files: std::collections::HashSet<&str> = changes.iter().map(|c| c.3.as_str()).collect();
    let mut out: Vec<SideLine> = vec![(format!(" {} 個檔案 · {} 次變更", files.len(), changes.len()), DIM, None, false)];
    if changes.is_empty() {
        out.push((" 這項工作尚無檔案變更".into(), DIM, None, false));
    }
    for (view, row, call, path, kind) in changes.iter().rev() {
        let (mark, color) = match kind.as_str() {
            "create" | "add" => ("A", DIFF_ADD),
            "delete" => ("D", DIFF_DEL),
            _ => ("M", TOOL),
        };
        let who = if view.is_empty() { String::new() } else { format!("  ·{view}") };
        out.push((
            format!(" {mark} {path}{who}"),
            color,
            Some(SideItem::Change { view: view.clone(), row: *row, call: *call }),
            false,
        ));
    }
    out
}

fn background_lines(app: &App) -> Vec<SideLine> {
    let s = app.cur();
    let mut out: Vec<SideLine> = Vec::new();
    if s.backgrounds.is_empty() && s.monitors.is_empty() {
        out.push((" 尚無背景行程、監控或計時器".into(), DIM, None, false));
    }
    for b in &s.backgrounds {
        let icon = if b.alive { spinner(app.tick).to_string() } else { "○".into() };
        let on = s.output_pick.as_deref() == Some(b.name.as_str());
        let item = Some(SideItem::Background(b.name.clone()));
        out.push((format!(" {icon} {}  {}", b.name, b.status), if b.alive { USER } else { DIM }, item.clone(), on));
        out.push((format!("    {}", b.command), DIM, item, on));
    }
    for m in &s.monitors {
        out.push((
            format!(" {} 監控 {}  {}", if m.alive { "●" } else { "○" }, m.name, m.status),
            if m.alive { TOOL } else { DIM },
            Some(SideItem::Monitor(m.name.clone())),
            false,
        ));
    }
    out
}

fn task_lines(app: &App) -> Vec<SideLine> {
    let task = app.cur().task.snapshot();
    let mut out: Vec<SideLine> = Vec::new();
    if task.goal.is_empty() {
        out.push((" 尚未設定任務目標".into(), DIM, None, false));
        out.push((" 任務模式讓監督者檢查表".into(), DIM, None, false));
        out.push((" 推著主代理做到完成".into(), DIM, None, false));
        out.push((" ▸ 設定任務目標".into(), ACCENT, Some(SideItem::OpenTask), false));
        return out;
    }
    out.push((format!(" 目標  {}", task.goal.replace('\n', " ")), TEXT, Some(SideItem::OpenTask), false));
    let done = task.checklist.iter().filter(|i| i.done).count();
    let phase = if task.skip_steer { "已暫停" } else { task.phase.label() };
    out.push((format!(" {phase} · {done}/{}", task.checklist.len()), ACCENT, Some(SideItem::OpenTask), false));
    for item in &task.checklist {
        out.push((format!(" {} {}", if item.done { "✓" } else { "□" }, item.text), if item.done { AGENT } else { TEXT }, None, false));
    }
    if !task.review_note.is_empty() {
        out.push((format!(" {}", task.review_note), DIM, None, false));
    }
    out.push((" ▸ 開啟任務面板".into(), ACCENT, Some(SideItem::OpenTask), false));
    out
}

pub(crate) fn tab_strip(f: &mut Frame, app: &mut App, area: Rect) {
    f.render_widget(Block::default().style(Style::default().bg(TABS_BG)), area);
    let active = app.active_tab();
    let s = app.cur();
    let labels: Vec<(String, Option<AgentState>)> = app
        .tabs()
        .iter()
        .map(|t| match t {
            Tab::Chat => (format!(" {} 主對話 ", if s.chat.running { spinner(app.tick) } else { '●' }), None),
            Tab::Agent(p) => {
                let a = s.agents.get(p);
                let state = a.map(|a| a.state).unwrap_or(AgentState::Exited);
                (format!(" {} {} ", state.icon(app.tick), a.map(|a| a.name.as_str()).unwrap_or(p)), Some(state))
            }
            Tab::Settings => (" ⚙ 設定 ".into(), None),
        })
        .collect();
    let tabs = app.tabs();
    let mut x = area.x;
    for (i, ((label, state), tab)) in labels.into_iter().zip(tabs.iter()).enumerate() {
        let on = *tab == active;
        let closable = *tab != Tab::Chat;
        let w = display_cols(&label);
        if x + w + if closable { 2 } else { 0 } > area.right() {
            break;
        }
        let bg = if on { BG } else { TABS_BG };
        let fg = match state {
            Some(st) if !on => state_color(st),
            _ if on => TEXT,
            _ => DIM,
        };
        let r = Rect::new(x, area.y, w, 1);
        f.render_widget(
            Paragraph::new(Span::styled(
                label,
                Style::default().fg(fg).bg(bg).add_modifier(if on { Modifier::BOLD } else { Modifier::empty() }),
            )),
            r,
        );
        app.ui.hits.push((r, Hit::EditorTab(i as u16)));
        x += w;
        if closable {
            let c = Rect::new(x, area.y, 2, 1);
            line(f, c, "× ", Style::default().fg(DIM).bg(bg));
            app.ui.hits.push((c, Hit::EditorTabClose(i as u16)));
            x += 2;
        }
        if x < area.right() {
            line(f, Rect::new(x, area.y, 1, 1), "│", Style::default().fg(BORDER).bg(TABS_BG));
            x += 1;
        }
    }
    let hint = " Ctrl+B 側欄 · Ctrl+J 面板 ";
    let hw = display_cols(hint);
    if area.right() >= x + hw {
        line(f, Rect::new(area.right() - hw, area.y, hw, 1), hint, Style::default().fg(DIM).bg(TABS_BG));
    }
}

pub(crate) fn readonly_bar(f: &mut Frame, app: &mut App, area: Rect) {
    f.render_widget(
        Block::default()
            .borders(Borders::TOP)
            .border_style(Style::default().fg(BORDER))
            .style(Style::default().bg(PANEL)),
        area,
    );
    app.ui.hits.push((area, Hit::ReadOnlyBar));
    if area.height < 2 {
        return;
    }
    let s = app.cur();
    let Some(a) = s.active.as_deref().and_then(|p| s.agents.get(p)) else {
        return;
    };
    let mut info = format!(" {} {} · {}", a.state.icon(app.tick), a.path, a.state.label());
    if !a.model.is_empty() {
        info.push_str(&format!(" · {}", a.model));
    }
    if a.turn > 0 {
        info.push_str(&format!(" · 第{}輪", a.turn));
    }
    info.push_str(&format!(" · 工具 {}   唯讀：子代理由主代理指揮 · Esc 回主對話 · 直接輸入會切回主對話", a.tools));
    let color = state_color(a.state);
    line(f, Rect::new(area.x, area.y + 1, area.width, 1), info, Style::default().fg(color).bg(PANEL));
}

/// (text, color, row hit) lines of the bottom panel, plus the Output picker.
struct PanelData {
    counts: [usize; 3],
    lines: Vec<(String, Color, Option<Hit>)>,
    /// Output tab: (label, selected) of each background.
    picks: Vec<(String, bool)>,
}

fn panel_data(app: &App, tab: BottomTab) -> PanelData {
    let s = app.cur();
    let counts = [
        s.agents.tool_log.iter().filter(|t| !t.done).count(),
        s.backgrounds.iter().filter(|b| b.alive).count(),
        0,
    ];
    let mut picks = Vec::new();
    let lines = match tab {
        BottomTab::Tools => s
            .agents
            .tool_log
            .iter()
            .enumerate()
            .map(|(i, t)| {
                let who = if t.path.is_empty() { "主" } else { t.path.as_str() };
                let (mark, color) = if !t.done {
                    (spinner(app.tick).to_string(), ACCENT)
                } else if t.phase == "失敗" {
                    ("!".into(), WARN)
                } else if t.phase == "已停止" {
                    ("⊘".into(), TOOL)
                } else {
                    ("✓".into(), TEXT)
                };
                let ms = if t.done { format!("  {}", md::fmt_duration(t.ms)) } else { String::new() };
                (format!("{mark} [{who}] {}  {}{ms}", t.line.replace('\n', " "), t.phase), color, Some(Hit::BottomRow(i as u16)))
            })
            .collect(),
        BottomTab::Events => s
            .agents
            .event_log
            .iter()
            .enumerate()
            .map(|(i, e)| {
                let who = if e.path.is_empty() { "主" } else { e.path.as_str() };
                let color = match e.kind {
                    EventKind::Error => WARN,
                    EventKind::Agent => ACCENT,
                    EventKind::Message => AGENT,
                    EventKind::Background => USER,
                    EventKind::Notice => DIM,
                };
                (format!("{} [{who}] {}", e.at, e.text.replace('\n', " ")), color, Some(Hit::BottomRow(i as u16)))
            })
            .collect(),
        BottomTab::Output => {
            let pick = s
                .output_pick
                .clone()
                .filter(|n| s.backgrounds.iter().any(|b| &b.name == n))
                .or_else(|| s.backgrounds.iter().rev().find(|b| b.alive).or(s.backgrounds.last()).map(|b| b.name.clone()));
            picks = s
                .backgrounds
                .iter()
                .map(|b| (format!(" {} {} ", if b.alive { "●" } else { "○" }, b.name), pick.as_deref() == Some(b.name.as_str())))
                .collect();
            match pick.and_then(|n| s.backgrounds.iter().find(|b| b.name == n)) {
                Some(b) => {
                    let mut out = vec![(format!("$ {} · {} {}", b.command, b.status, b.detail), DIM, None)];
                    out.extend(b.log.iter().map(|l| (l.clone(), if l.starts_with("stderr") { WARN } else { TEXT }, None)));
                    out
                }
                None => Vec::new(),
            }
        }
    };
    PanelData { counts, lines, picks }
}

pub(crate) fn bottom_panel(f: &mut Frame, app: &mut App, area: Rect) {
    app.ui.bottom_area = area;
    f.render_widget(
        Block::default()
            .borders(Borders::TOP)
            .border_style(Style::default().fg(BORDER))
            .style(Style::default().bg(PANEL).fg(TEXT)),
        area,
    );
    if area.height < 3 {
        return;
    }
    let tab = app.ui.bottom.unwrap_or(BottomTab::Tools);
    let data = panel_data(app, tab);
    let ty = area.y + 1;
    let mut x = area.x + 1;
    for (i, t) in BottomTab::ALL.iter().enumerate() {
        let on = *t == tab;
        let label = if data.counts[i] > 0 {
            format!(" {} {} ", t.title(), data.counts[i])
        } else {
            format!(" {} ", t.title())
        };
        let w = display_cols(&label);
        let r = Rect::new(x, ty, w, 1);
        f.render_widget(
            Paragraph::new(Span::styled(
                label,
                Style::default()
                    .fg(if on { TEXT } else { DIM })
                    .bg(PANEL)
                    .add_modifier(if on { Modifier::BOLD | Modifier::UNDERLINED } else { Modifier::empty() }),
            )),
            r,
        );
        app.ui.hits.push((r, Hit::BottomTab(i as u8)));
        x += w + 1;
    }
    let close = Rect::new(area.right().saturating_sub(3), ty, 3, 1);
    line(f, close, " × ", Style::default().fg(DIM).bg(PANEL));
    app.ui.hits.push((close, Hit::BottomClose));

    let mut body = Rect::new(area.x + 1, ty + 1, area.width.saturating_sub(2), area.height.saturating_sub(2));
    if !data.picks.is_empty() && body.height > 1 {
        let mut px = body.x;
        for (i, (label, on)) in data.picks.iter().enumerate() {
            let w = display_cols(label);
            if px + w > body.right() {
                break;
            }
            let r = Rect::new(px, body.y, w, 1);
            let style = if *on {
                Style::default().fg(Color::Black).bg(ACCENT)
            } else {
                Style::default().fg(TEXT).bg(COMPOSER)
            };
            line(f, r, label.clone(), style);
            app.ui.hits.push((r, Hit::OutputPick(i as u16)));
            px += w + 1;
        }
        body = Rect::new(body.x, body.y + 1, body.width, body.height - 1);
    }
    let lines = data.lines;
    if lines.is_empty() {
        let empty = match tab {
            BottomTab::Tools => "尚無工具呼叫",
            BottomTab::Events => "尚無事件",
            BottomTab::Output => "尚無背景行程輸出",
        };
        line(f, Rect::new(body.x, body.y, body.width, 1.min(body.height)), empty, Style::default().fg(DIM).bg(PANEL));
        return;
    }
    // Newest at the bottom; `bottom_scroll` counts lines up from the end.
    let view_h = body.height as usize;
    app.ui.bottom_scroll = app.ui.bottom_scroll.min(lines.len().saturating_sub(view_h));
    let end = lines.len() - app.ui.bottom_scroll;
    let start = end.saturating_sub(view_h);
    for (row, (text, color, hit)) in lines[start..end].iter().enumerate() {
        let r = Rect::new(body.x, body.y + row as u16, body.width, 1);
        f.render_widget(
            Paragraph::new(Span::styled(truncate(text, body.width), Style::default().fg(*color).bg(PANEL))),
            r,
        );
        if let Some(h) = hit {
            app.ui.hits.push((r, *h));
        }
    }
}
