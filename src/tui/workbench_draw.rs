// VS Code-style frame: activity bar | side bar | editor (tabs, chat, composer,
// bottom panel), with a status bar along the bottom edge.

const ACTIVITY_BG: Color = Color::Rgb(18, 18, 18);
const TABS_BG: Color = Color::Rgb(28, 28, 28);

fn draw_ui(f: &mut Frame, app: &mut App, opts: &TuiOptions, freeze_composer: bool) -> Position {
    app.hits.clear();
    app.chat_glyphs.clear();
    app.graphic_blits.clear();
    app.image_hits.clear();
    app.area = f.area();
    f.render_widget(Block::default().style(Style::default().bg(BG).fg(TEXT)), f.area());
    app.refresh_session_list();

    let whole = f.area();
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(4), Constraint::Length(1)])
        .split(whole);
    let work = rows[0];
    let status = rows[1];
    draw_header(f, app, opts, status);

    let act_w = if whole.width >= ACTIVITY_MIN_TERM { ACTIVITY_W } else { 0 };
    let docked = app.side_open && whole.width >= SIDEBAR_MIN_TERM;
    let side_w = if docked { SIDEBAR_W } else { 0 };
    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Length(act_w),
            Constraint::Length(side_w),
            Constraint::Min(20),
        ])
        .split(work);
    if act_w > 0 {
        draw_activity_bar(f, app, cols[0]);
    }
    let mut rename_caret = None;
    app.side_area = Rect::default();
    if docked {
        rename_caret = draw_side(f, app, cols[1]);
    }
    let editor = cols[2];

    let viewing = app.viewing_agent();
    let qn = app.queue.len().min(5);
    let attach = if app.pending.is_empty() { 0 } else { 1 };
    let composer_h = if viewing {
        2
    } else {
        composer_height(qn, attach)
            .saturating_add(app.edit.text.lines().count().saturating_sub(1).min(3) as u16)
    };
    let panel_h = if app.bottom.is_some() {
        (editor.height.saturating_mul(35) / 100)
            .clamp(6, 16)
            .min(editor.height.saturating_sub(composer_h + 8))
    } else {
        0
    };
    let parts = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Min(4),
            Constraint::Length(composer_h),
            Constraint::Length(panel_h),
        ])
        .split(editor);
    draw_tabs(f, app, parts[0]);
    let chat_area = parts[1];
    app.with_active_view(|app| {
        let tool_hits = draw_chat(f, app, chat_area);
        draw_tool_panel(f, app, chat_area, &tool_hits);
        draw_jump_bottom(f, app, chat_area);
    });

    app.composer_frame = parts[2];
    let mut caret = if viewing {
        app.composer_snap = None;
        draw_readonly_bar(f, app, parts[2]);
        Position::new(parts[2].x, parts[2].y)
    } else if freeze_composer && composer_snap_matches(app) {
        if let Some(snap) = &app.composer_snap {
            copy_rect(snap, f.buffer_mut(), app.composer_frame);
        }
        app.last_caret
    } else {
        let caret = draw_composer(f, app, opts, parts[2]);
        app.composer_snap = Some(clone_rect(f.buffer_mut(), app.composer_frame));
        caret
    };
    app.bottom_area = Rect::default();
    if panel_h > 0 {
        draw_bottom(f, app, parts[3]);
    }

    // Narrow terminals: the side bar floats over the editor.
    if app.side_open && !docked {
        let w = (SIDEBAR_W + 6).min(editor.width);
        let over = Rect::new(editor.x, editor.y, w, work.height);
        f.render_widget(Clear, over);
        rename_caret = draw_side(f, app, over);
    }
    if app.focus == Focus::Rename {
        if let Some(pos) = rename_caret {
            caret = pos;
        }
    }

    let settings_visible = app.settings.as_ref().is_some_and(|s| !s.minimized);
    let settings_min = app.settings.as_ref().is_some_and(|s| s.minimized);
    if settings_min {
        let dock = settings_dock_rect(status);
        f.render_widget(
            Paragraph::new(Span::styled(" 設定 ", Style::default().bg(COMPOSER).fg(ACCENT))),
            dock,
        );
        app.hits.push((dock, Hit::Dock));
    }
    if settings_visible {
        if let Some(pos) = draw_settings(f, app, opts) {
            if app.focus == Focus::Settings {
                caret = pos;
            }
        }
    }
    if app.inspector.is_some() {
        draw_inspector(f, app, f.area());
    }
    if app.image_view.is_some() {
        draw_image_view(f, app, f.area());
    }
    if app.skill_view.is_some() {
        if let Some(pos) = draw_skill_view(f, app, f.area()) {
            caret = pos;
        }
    }
    if app.ask.is_some() {
        if let Some(pos) = draw_ask(f, app) {
            caret = pos;
        }
    }
    if app.task_ui.is_some() {
        if let Some(pos) = draw_task(f, app) {
            caret = pos;
        }
    }
    if app.workspace_pick.is_some() {
        if let Some(pos) = draw_workspace_pick(f, app) {
            caret = pos;
        }
    }
    crate::preview::reveal_obscured_graphics(f.buffer_mut(), &mut app.graphic_blits);
    caret
}

fn draw_activity_bar(f: &mut Frame, app: &mut App, area: Rect) {
    f.render_widget(Block::default().style(Style::default().bg(ACTIVITY_BG)), area);
    let mut y = area.y;
    for (i, view) in SideView::ALL.iter().enumerate() {
        if y + 1 >= area.bottom() {
            break;
        }
        let on = app.side_open && app.side_view == *view;
        let cell = Rect::new(area.x, y, area.width, 2);
        let mark = if on { "▎" } else { " " };
        let icon_style = Style::default()
            .fg(if on { TEXT } else { DIM })
            .bg(ACTIVITY_BG)
            .add_modifier(if on { Modifier::BOLD } else { Modifier::empty() });
        f.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled(mark, Style::default().fg(ACCENT).bg(ACTIVITY_BG)),
                Span::styled(view.icon(), icon_style),
            ])),
            Rect::new(area.x, y, area.width, 1),
        );
        // Badge under the icon: live agents / live backgrounds.
        let badge = match view {
            SideView::Agents if app.bench.live_count() > 0 => app.bench.live_count().to_string(),
            SideView::Background => {
                let n = app.backgrounds.iter().filter(|b| b.alive).count()
                    + app.monitors.iter().filter(|m| m.alive).count();
                if n > 0 { n.to_string() } else { String::new() }
            }
            SideView::Task if app.task.snapshot().phase.is_live() => "●".into(),
            _ => String::new(),
        };
        if !badge.is_empty() {
            f.render_widget(
                Paragraph::new(Span::styled(
                    format!(" {}", truncate_width(&badge, area.width.saturating_sub(1))),
                    Style::default().fg(ACCENT).bg(ACTIVITY_BG),
                )),
                Rect::new(area.x, y + 1, area.width, 1),
            );
        }
        app.hits.push((cell, Hit::Activity(i as u8)));
        y += 2;
    }
    if area.height >= 2 {
        let gear = Rect::new(area.x, area.bottom() - 1, area.width, 1);
        f.render_widget(
            Paragraph::new(Span::styled(" ⚙", Style::default().fg(DIM).bg(ACTIVITY_BG))),
            gear,
        );
        app.hits.push((gear, Hit::Activity(u8::MAX)));
    }
}

fn draw_side(f: &mut Frame, app: &mut App, area: Rect) -> Option<Position> {
    app.side_area = area;
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
    let title = Rect::new(area.x, area.y, area.width.saturating_sub(1), 1);
    f.render_widget(
        Paragraph::new(Span::styled(
            format!(" {}", app.side_view.title()),
            Style::default().fg(DIM).bg(PANEL).add_modifier(Modifier::BOLD),
        )),
        title,
    );
    let body = Rect::new(area.x, area.y + 1, area.width, area.height - 1);
    app.hits.push((body, Hit::SideScroll));
    match app.side_view {
        SideView::Sessions => draw_sidebar(f, app, body),
        SideView::Agents => {
            draw_side_agents(f, app, body);
            None
        }
        SideView::Changes => {
            draw_side_changes(f, app, body);
            None
        }
        SideView::Background => {
            draw_side_background(f, app, body);
            None
        }
        SideView::Task => {
            draw_side_task(f, app, body);
            None
        }
    }
}

/// Paint a scrolled list of (text, color, hit, highlighted) lines.
fn draw_side_lines(
    f: &mut Frame,
    app: &mut App,
    area: Rect,
    lines: Vec<(String, Color, Option<Hit>, bool)>,
) {
    let inner = Rect::new(area.x, area.y, area.width.saturating_sub(1), area.height);
    let view_h = inner.height as usize;
    app.side_scroll = app.side_scroll.min(lines.len().saturating_sub(view_h));
    for (i, (text, color, hit, on)) in lines
        .iter()
        .skip(app.side_scroll)
        .take(view_h)
        .enumerate()
    {
        let row = Rect::new(inner.x, inner.y + i as u16, inner.width, 1);
        let bg = if *on { COMPOSER } else { PANEL };
        f.render_widget(
            Paragraph::new(Span::styled(
                truncate_width(text, inner.width),
                Style::default().fg(*color).bg(bg),
            )),
            row,
        );
        if let Some(hit) = hit {
            app.hits.push((row, *hit));
        }
    }
}

fn draw_side_agents(f: &mut Frame, app: &mut App, area: Rect) {
    let mut lines: Vec<(String, Color, Option<Hit>, bool)> = Vec::new();
    let root_icon = if app.running { spinner(app.tick).to_string() } else { "●".into() };
    let root_state = if app.ask.is_some() { "等你回覆".to_string() } else { app.status.clone() };
    lines.push((
        format!(" {root_icon} 主代理  {root_state}"),
        if app.running { ACCENT } else { TEXT },
        Some(Hit::SideRoot),
        app.bench.active.is_none(),
    ));
    let knobs_model = app.knobs.lock().map(|k| k.model.clone()).unwrap_or_default();
    lines.push((format!("   {knobs_model}"), DIM, Some(Hit::SideRoot), app.bench.active.is_none()));
    if app.bench.agents.is_empty() {
        lines.push((String::new(), DIM, None, false));
        lines.push((" 尚無子代理".into(), DIM, None, false));
        lines.push((" 主代理呼叫 spawn_agent 後".into(), DIM, None, false));
        lines.push((" 會出現在這裡".into(), DIM, None, false));
    }
    let tick = app.tick;
    for (i, a) in app.bench.agents.iter().enumerate() {
        let indent = "  ".repeat(a.depth() + 1);
        let on = app.bench.active.as_deref() == Some(a.path.as_str());
        let turn = if a.turn > 0 { format!(" · 第{}輪", a.turn) } else { String::new() };
        lines.push((
            format!(" {indent}{} {}  {}{turn}", a.state.icon(tick), a.name, a.state.label()),
            a.state.color(),
            Some(Hit::SideAgent(i as u16)),
            on,
        ));
        let detail = if a.state.live() && !a.view.activity.is_empty() {
            a.view.activity.clone()
        } else if !a.model.is_empty() {
            a.model.clone()
        } else {
            String::new()
        };
        if !detail.is_empty() {
            lines.push((format!(" {indent}  {detail}"), DIM, Some(Hit::SideAgent(i as u16)), on));
        }
    }
    draw_side_lines(f, app, area, lines);
}

fn draw_side_changes(f: &mut Frame, app: &mut App, area: Rect) {
    let changes = app.all_file_changes();
    let mut lines: Vec<(String, Color, Option<Hit>, bool)> = Vec::new();
    let distinct: std::collections::HashSet<&str> = changes.iter().map(|c| c.3.as_str()).collect();
    lines.push((format!(" {} 個檔案 · {} 次變更", distinct.len(), changes.len()), DIM, None, false));
    if changes.is_empty() {
        lines.push((" 這項工作尚無檔案變更".into(), DIM, None, false));
    }
    for (i, (owner, _, _, path, kind)) in changes.iter().enumerate().rev() {
        let mark = match kind.as_str() {
            "create" | "add" => "A",
            "delete" => "D",
            _ => "M",
        };
        let color = match mark {
            "A" => DIFF_ADD,
            "D" => DIFF_DEL,
            _ => TOOL,
        };
        let who = if owner.is_empty() { String::new() } else { format!("  ·{owner}") };
        lines.push((format!(" {mark} {path}{who}"), color, Some(Hit::SideChange(i as u16)), false));
    }
    draw_side_lines(f, app, area, lines);
}

fn draw_side_background(f: &mut Frame, app: &mut App, area: Rect) {
    let mut lines: Vec<(String, Color, Option<Hit>, bool)> = Vec::new();
    if app.backgrounds.is_empty() && app.monitors.is_empty() {
        lines.push((" 尚無背景行程、監控或計時器".into(), DIM, None, false));
    }
    let tick = app.tick;
    for (i, b) in app.backgrounds.iter().enumerate() {
        let icon = if b.alive { spinner(tick).to_string() } else { "○".into() };
        let on = app.bench.output.as_deref() == Some(b.name.as_str());
        lines.push((
            format!(" {icon} {}  {}", b.name, b.status),
            if b.alive { USER } else { DIM },
            Some(Hit::RailBg(i as u16)),
            on,
        ));
        lines.push((format!("    {}", b.command), DIM, Some(Hit::RailBg(i as u16)), on));
    }
    for (i, m) in app.monitors.iter().enumerate() {
        let icon = if m.alive { "●" } else { "○" };
        lines.push((
            format!(" {icon} 監控 {}  {}", m.name, m.status),
            if m.alive { TOOL } else { DIM },
            Some(Hit::RailMon(i as u16)),
            false,
        ));
    }
    draw_side_lines(f, app, area, lines);
}

fn draw_side_task(f: &mut Frame, app: &mut App, area: Rect) {
    let task = app.task.snapshot();
    let mut lines: Vec<(String, Color, Option<Hit>, bool)> = Vec::new();
    if task.goal.is_empty() {
        lines.push((" 尚未設定任務目標".into(), DIM, None, false));
        lines.push((" ▸ 設定任務目標".into(), ACCENT, Some(Hit::TaskChip), false));
    } else {
        lines.push((format!(" 目標  {}", task.goal.replace('\n', " ")), TEXT, Some(Hit::TaskChip), false));
        let done = task.checklist.iter().filter(|i| i.done).count();
        let phase = if task.skip_steer { "已暫停" } else { task.phase.label() };
        lines.push((format!(" {phase} · {done}/{}", task.checklist.len()), ACCENT, Some(Hit::TaskChip), false));
        for item in &task.checklist {
            lines.push((
                format!(" {} {}", if item.done { "✓" } else { "□" }, item.text),
                if item.done { AGENT } else { TEXT },
                None,
                false,
            ));
        }
        if !task.review_note.is_empty() {
            lines.push((format!(" {}", task.review_note), DIM, None, false));
        }
        lines.push((" ▸ 開啟任務面板".into(), ACCENT, Some(Hit::TaskChip), false));
    }
    draw_side_lines(f, app, area, lines);
}

fn draw_tabs(f: &mut Frame, app: &mut App, area: Rect) {
    f.render_widget(Block::default().style(Style::default().bg(TABS_BG)), area);
    let mut x = area.x;
    let right = area.right();
    let tick = app.tick;
    let mut tabs: Vec<(String, Option<AgentState>, bool)> = vec![(
        format!(" {} 主對話 ", if app.running { spinner(tick) } else { '●' }),
        None,
        app.bench.active.is_none(),
    )];
    for p in &app.bench.open {
        if let Some(a) = app.bench.agent(p) {
            tabs.push((
                format!(" {} {} ", a.state.icon(tick), a.name),
                Some(a.state),
                app.bench.active.as_deref() == Some(p.as_str()),
            ));
        }
    }
    for (i, (label, state, on)) in tabs.iter().enumerate() {
        let w = display_cols(label);
        let close_w = if i > 0 { 2 } else { 0 };
        if x + w + close_w > right {
            break;
        }
        let bg = if *on { BG } else { TABS_BG };
        let fg = match state {
            Some(s) if !*on => s.color(),
            _ if *on => TEXT,
            _ => DIM,
        };
        let style = Style::default()
            .fg(fg)
            .bg(bg)
            .add_modifier(if *on { Modifier::BOLD } else { Modifier::empty() });
        let rect = Rect::new(x, area.y, w, 1);
        f.render_widget(Paragraph::new(Span::styled(label.clone(), style)), rect);
        app.hits.push((rect, Hit::EditorTab(i as u16)));
        x += w;
        if i > 0 {
            let close = Rect::new(x, area.y, 2, 1);
            f.render_widget(
                Paragraph::new(Span::styled("× ", Style::default().fg(DIM).bg(bg))),
                close,
            );
            app.hits.push((close, Hit::EditorTabClose(i as u16)));
            x += 2;
        }
        if x < right {
            f.render_widget(
                Paragraph::new(Span::styled("│", Style::default().fg(BORDER).bg(TABS_BG))),
                Rect::new(x, area.y, 1, 1),
            );
            x += 1;
        }
    }
    let hint = " Ctrl+B 側欄 · Ctrl+J 面板 ";
    let hw = display_cols(hint);
    if right >= x + hw {
        f.render_widget(
            Paragraph::new(Span::styled(hint, Style::default().fg(DIM).bg(TABS_BG))),
            Rect::new(right - hw, area.y, hw, 1),
        );
    }
}

fn draw_readonly_bar(f: &mut Frame, app: &mut App, area: Rect) {
    f.render_widget(
        Block::default()
            .borders(Borders::TOP)
            .border_style(Style::default().fg(BORDER))
            .style(Style::default().bg(PANEL)),
        area,
    );
    app.hits.push((area, Hit::ReadOnlyBar));
    let Some(a) = app.bench.active.as_deref().and_then(|p| app.bench.agent(p)) else {
        return;
    };
    if area.height < 2 {
        return;
    }
    let mut info = format!(" {} {} · {}", a.state.icon(app.tick), a.path, a.state.label());
    if !a.model.is_empty() {
        info.push_str(&format!(" · {}", a.model));
    }
    if a.turn > 0 {
        info.push_str(&format!(" · 第{}輪", a.turn));
    }
    info.push_str(&format!(" · 工具 {}", a.tools));
    info.push_str("   唯讀：子代理由主代理指揮 · Esc 回主對話 · 直接輸入會切回主對話");
    f.render_widget(
        Paragraph::new(Span::styled(
            truncate_width(&info, area.width),
            Style::default().fg(a.state.color()).bg(PANEL),
        )),
        Rect::new(area.x, area.y + 1, area.width, 1),
    );
}

fn draw_bottom(f: &mut Frame, app: &mut App, area: Rect) {
    app.bottom_area = area;
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
    let tabs_y = area.y + 1;
    let mut x = area.x + 1;
    for (i, tab) in BottomTab::ALL.iter().enumerate() {
        let on = app.bottom == Some(*tab);
        let count = match tab {
            BottomTab::Tools => app.bench.tool_log.iter().filter(|t| !t.done).count(),
            BottomTab::Output => app.backgrounds.iter().filter(|b| b.alive).count(),
            BottomTab::Events => 0,
        };
        let label = if count > 0 {
            format!(" {} {count} ", tab.title())
        } else {
            format!(" {} ", tab.title())
        };
        let w = display_cols(&label);
        let rect = Rect::new(x, tabs_y, w, 1);
        f.render_widget(
            Paragraph::new(Span::styled(
                label,
                Style::default()
                    .fg(if on { TEXT } else { DIM })
                    .bg(PANEL)
                    .add_modifier(if on { Modifier::BOLD | Modifier::UNDERLINED } else { Modifier::empty() }),
            )),
            rect,
        );
        app.hits.push((rect, Hit::BottomTab(i as u8)));
        x += w + 1;
    }
    let close = Rect::new(area.right().saturating_sub(3), tabs_y, 3, 1);
    f.render_widget(Paragraph::new(Span::styled(" × ", Style::default().fg(DIM).bg(PANEL))), close);
    app.hits.push((close, Hit::BottomClose));

    let body = Rect::new(area.x + 1, tabs_y + 1, area.width.saturating_sub(2), area.height.saturating_sub(2));
    let lines: Vec<(String, Color)> = match app.bottom.unwrap_or(BottomTab::Tools) {
        BottomTab::Tools => app
            .bench
            .tool_log
            .iter()
            .map(|t| {
                let who = if t.path.is_empty() { "主" } else { t.path.as_str() };
                let mark = if !t.done {
                    spinner(app.tick).to_string()
                } else if t.phase == "失敗" {
                    "!".into()
                } else if t.phase == "已停止" {
                    "⊘".into()
                } else {
                    "✓".into()
                };
                let ms = if t.done { format!("  {}", md::fmt_duration(t.ms)) } else { String::new() };
                let line = t.line.trim_start_matches("▸ ").replace('\n', " ");
                let color = if !t.done { ACCENT } else if t.phase == "失敗" { WARN } else { TEXT };
                (format!("{mark} [{who}] {line}  {}{ms}", t.phase), color)
            })
            .collect(),
        BottomTab::Output => {
            let pick = app
                .bench
                .output
                .clone()
                .filter(|n| app.backgrounds.iter().any(|b| &b.name == n))
                .or_else(|| {
                    app.backgrounds
                        .iter()
                        .rev()
                        .find(|b| b.alive)
                        .or(app.backgrounds.last())
                        .map(|b| b.name.clone())
                });
            match pick.and_then(|n| app.backgrounds.iter().find(|b| b.name == n)) {
                Some(b) => {
                    let mut v = vec![(format!("── {}  {}  $ {}", b.name, b.status, b.command), ACCENT)];
                    v.extend(b.log.iter().map(|l| {
                        let color = if l.starts_with("stderr") { WARN } else { TEXT };
                        (l.clone(), color)
                    }));
                    v
                }
                None => vec![("尚無背景行程輸出".into(), DIM)],
            }
        }
        BottomTab::Events => app
            .bench
            .event_log
            .iter()
            .map(|e| {
                let who = if e.path.is_empty() { "主" } else { e.path.as_str() };
                let color = match e.kind {
                    "error" => WARN,
                    "agent" => ACCENT,
                    "message" => AGENT,
                    "bg" => USER,
                    _ => DIM,
                };
                (format!("{} [{who}] {}", e.at, e.text.replace('\n', " ")), color)
            })
            .collect(),
    };
    // Newest at the bottom; `bottom_scroll` counts lines up from the end.
    let view_h = body.height as usize;
    app.bottom_scroll = app.bottom_scroll.min(lines.len().saturating_sub(view_h));
    let end = lines.len() - app.bottom_scroll;
    let start = end.saturating_sub(view_h);
    if lines.is_empty() {
        let empty = match app.bottom.unwrap_or(BottomTab::Tools) {
            BottomTab::Tools => "尚無工具呼叫",
            BottomTab::Output => "尚無背景行程輸出",
            BottomTab::Events => "尚無事件",
        };
        f.render_widget(
            Paragraph::new(Span::styled(empty, Style::default().fg(DIM).bg(PANEL))),
            Rect::new(body.x, body.y, body.width, 1.min(body.height)),
        );
    }
    for (row, idx) in (start..end).enumerate() {
        let (text, color) = &lines[idx];
        let rect = Rect::new(body.x, body.y + row as u16, body.width, 1);
        f.render_widget(
            Paragraph::new(Span::styled(truncate_width(text, body.width), Style::default().fg(*color).bg(PANEL))),
            rect,
        );
        app.hits.push((rect, Hit::BottomRow(idx as u16)));
    }
}

/// Locate a tool call by id in a transcript (root = "").
fn find_call(app: &App, path: &str, call_id: &str) -> Option<(usize, usize)> {
    if call_id.is_empty() {
        return None;
    }
    let rows: &[Row] = if path.is_empty() {
        &app.rows
    } else {
        &app.bench.agent(path)?.view.rows
    };
    rows.iter().enumerate().rev().find_map(|(ri, r)| match r {
        Row::Tools(g) => g
            .calls
            .iter()
            .position(|c| c.call_id == call_id)
            .map(|ci| (ri, ci)),
        _ => None,
    })
}
