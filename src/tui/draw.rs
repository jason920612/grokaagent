fn inspector_rect(area: Rect) -> Rect {
    let max_w = area.width.saturating_sub(4).max(1);
    let max_h = area.height.saturating_sub(3).max(1);
    let w = (area.width.saturating_mul(3) / 4).min(max_w).max(36.min(max_w));
    let h = (area.height.saturating_mul(4) / 5).min(max_h).max(10.min(max_h));
    let x = area.x + area.width.saturating_sub(w) / 2;
    let y = area.y + area.height.saturating_sub(h) / 2;
    Rect::new(x, y, w, h)
}

fn draw_inspector(f: &mut Frame, app: &mut App, area: Rect) {
    let Some(kind) = app.inspector.clone() else {
        return;
    };
    let panel = inspector_rect(area);
    f.render_widget(Clear, panel);
    let title = match &kind {
        Inspector::Monitor(n) => format!(" 監控 {n} "),
        Inspector::Background(n) => format!(" 後台 {n} "),
    };
    f.render_widget(
        Block::default()
            .title(title)
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(ACCENT))
            .style(Style::default().bg(PANEL).fg(TEXT)),
        panel,
    );
    let close = Rect::new(
        panel.x + panel.width.saturating_sub(4),
        panel.y,
        3,
        1,
    );
    f.render_widget(
        Paragraph::new(Span::styled(" × ", Style::default().fg(WARN))),
        close,
    );
    app.hits.push((panel, Hit::Inspector));
    app.hits.push((close, Hit::InspectorClose));

    let inner = Rect::new(
        panel.x.saturating_add(2),
        panel.y.saturating_add(1),
        panel.width.saturating_sub(4),
        panel.height.saturating_sub(2),
    );
    let mut lines: Vec<Line<'static>> = Vec::new();
    match kind {
        Inspector::Monitor(name) => {
            let Some(m) = app.monitors.iter().find(|m| m.name == name) else {
                lines.push(Line::from(Span::styled(
                    format!("找不到監控「{name}」"),
                    Style::default().fg(DIM),
                )));
                f.render_widget(
                    Paragraph::new(lines).wrap(ratatui::widgets::Wrap { trim: false }),
                    inner,
                );
                return;
            };
            lines.push(Line::from(Span::styled(
                format!(
                    "{}  {}",
                    if m.alive { "執行中" } else { "已結束" },
                    m.status
                ),
                Style::default().fg(if m.alive { TOOL } else { DIM }),
            )));
            lines.push(Line::from(Span::styled(
                format!("PID   {}", m.pid),
                Style::default().fg(TEXT),
            )));
            lines.push(Line::from(Span::styled(
                format!("指令  {}", m.command),
                Style::default().fg(TEXT),
            )));
            lines.push(Line::from(Span::styled(
                "stdin 本 run 的 JSONL 事件流",
                Style::default().fg(DIM),
            )));
            if !m.detail.is_empty() {
                lines.push(Line::from(Span::styled(
                    format!("結束  {}", m.detail),
                    Style::default().fg(DIM),
                )));
            }
        }
        Inspector::Background(name) => {
            let Some(b) = app.backgrounds.iter().find(|b| b.name == name) else {
                lines.push(Line::from(Span::styled(
                    format!("找不到背景行程「{name}」"),
                    Style::default().fg(DIM),
                )));
                f.render_widget(
                    Paragraph::new(lines).wrap(ratatui::widgets::Wrap { trim: false }),
                    inner,
                );
                return;
            };
            lines.push(Line::from(Span::styled(
                format!(
                    "{}  {}",
                    if b.alive { "執行中" } else { "已結束" },
                    b.status
                ),
                Style::default().fg(if b.alive { USER } else { DIM }),
            )));
            lines.push(Line::from(Span::styled(
                format!("PID   {}", b.pid),
                Style::default().fg(TEXT),
            )));
            lines.push(Line::from(Span::styled(
                format!("指令  {}", b.command),
                Style::default().fg(TEXT),
            )));
            if !b.detail.is_empty() {
                lines.push(Line::from(Span::styled(
                    format!("結束  {}", b.detail),
                    Style::default().fg(DIM),
                )));
            }
            if b.log.is_empty() {
                lines.push(Line::from(Span::styled(
                    "尚無輸出",
                    Style::default().fg(DIM),
                )));
            } else {
                lines.push(Line::from(Span::styled(
                    "── stdout / stderr ──",
                    Style::default().fg(DIM),
                )));
                for row in &b.log {
                    let style = if row.starts_with("err ") {
                        Style::default().fg(WARN)
                    } else {
                        Style::default().fg(TEXT)
                    };
                    lines.push(Line::from(Span::styled(row.clone(), style)));
                }
            }
        }
    }

    let mut wrapped: Vec<Line<'static>> = Vec::new();
    for line in lines {
        wrapped.extend(wrap_visual(line, inner.width.max(1)));
    }
    let max_off = wrapped.len().saturating_sub(inner.height as usize) as u16;
    if app.inspector_scroll > max_off {
        app.inspector_scroll = max_off;
    }
    let start = app.inspector_scroll as usize;
    let end = (start + inner.height as usize).min(wrapped.len());
    let vis = if start < wrapped.len() {
        wrapped[start..end].to_vec()
    } else {
        Vec::new()
    };
    f.render_widget(Paragraph::new(vis), inner);
}

fn draw_sidebar(f: &mut Frame, app: &mut App, area: Rect) -> Option<Position> {
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
    let inner = Rect::new(
        area.x,
        area.y,
        area.width.saturating_sub(1),
        area.height,
    );
    let new_btn = Rect::new(inner.x, inner.y, inner.width, 1);
    f.render_widget(
        Paragraph::new(Span::styled(
            " + 新對話",
            Style::default()
                .fg(ACCENT)
                .bg(PANEL)
                .add_modifier(Modifier::BOLD),
        )),
        new_btn,
    );
    app.hits.push((new_btn, Hit::NewChat));

    let list_y = inner.y.saturating_add(2);
    if inner.height.saturating_sub(2) == 0 {
        return None;
    }
    let btn_w = 6u16;
    let mut rename_pos = None;
    let mut y = list_y;
    let sessions = app.sessions.clone();
    for (i, meta) in sessions.iter().enumerate() {
        if y + 1 >= inner.y + inner.height {
            break;
        }
        let selected = meta.id == app.current_id;
        let renaming = app.rename.as_ref().is_some_and(|(id, _)| id == &meta.id);
        let running = if selected {
            app.running
        } else {
            app.parked.get(&meta.id).is_some_and(|p| p.running)
        };
        let bg = if selected || renaming { COMPOSER } else { PANEL };
        let row1 = Rect::new(inner.x, y, inner.width, 1);
        let row2 = Rect::new(inner.x, y + 1, inner.width, 1);
        let text_w = inner.width.saturating_sub(btn_w);
        let hit = Rect::new(inner.x, y, inner.width, 2);
        app.hits.push((hit, Hit::Session(i as u16)));

        if renaming {
            let edit_area = Rect::new(inner.x.saturating_add(1), y, text_w.saturating_sub(1).max(1), 1);
            app.rename_inner = edit_area;
            f.render_widget(
                Block::default().style(Style::default().bg(bg)),
                row1,
            );
            if let Some((_, edit)) = app.rename.as_ref() {
                let mut vs = 0u16;
                rename_pos = Some(draw_edit(f, edit_area, edit, &mut vs));
            }
        } else {
            let mark = if running { "● " } else { "  " };
            let title = truncate_width(&format!("{mark}{}", meta.name), text_w.max(1));
            f.render_widget(
                Paragraph::new(Span::styled(
                    title,
                    Style::default().fg(TEXT).bg(bg).add_modifier(if selected {
                        Modifier::BOLD
                    } else {
                        Modifier::empty()
                    }),
                )),
                Rect::new(inner.x, y, text_w.max(1), 1),
            );
        }

        let edit_btn = Rect::new(inner.x + text_w, y, 3, 1);
        let del_btn = Rect::new(inner.x + text_w + 3, y, 3, 1);
        if inner.width >= btn_w {
            f.render_widget(
                Paragraph::new(Span::styled(" ✎ ", Style::default().fg(DIM).bg(bg))),
                edit_btn,
            );
            f.render_widget(
                Paragraph::new(Span::styled(" × ", Style::default().fg(WARN).bg(bg))),
                del_btn,
            );
            app.hits.push((edit_btn, Hit::RenameSession(i as u16)));
            app.hits.push((del_btn, Hit::DeleteSession(i as u16)));
        }

        let sub = truncate_width(
            &format!("  {}  {}", meta.folder_label(), meta.short_id()),
            inner.width,
        );
        f.render_widget(
            Paragraph::new(Span::styled(sub, Style::default().fg(DIM).bg(bg))),
            row2,
        );
        y = y.saturating_add(3);
    }
    rename_pos
}

fn truncate_width(s: &str, cols: u16) -> String {
    let max = cols as usize;
    if max == 0 {
        return String::new();
    }
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
    while w < max {
        out.push(' ');
        w += 1;
    }
    out
}

fn header_copy(app: &App, opts: &TuiOptions) -> (String, String) {
    let auth = if app.logged_in { "已登入" } else { "未登入" };
    let kids = if app.child_count == 0 {
        String::new()
    } else {
        format!("  代理 {}/{}", app.bench.live_count(), app.child_count)
    };
    let left = if app.running {
        let act = if app.activity.is_empty() {
            app.status.clone()
        } else {
            app.activity.clone()
        };
        let clock = format!(
            "  {}",
            app.work_started
                .map(|t| md::fmt_duration_field(t.elapsed().as_millis() as u64))
                .unwrap_or_else(|| " ".repeat(md::DURATION_FIELD))
        );
        format!(
            " grokaagent  {} {}{}  · {}  Esc 中斷  {}{}",
            spinner(app.tick),
            act,
            clock,
            app.status,
            auth,
            kids
        )
    } else {
        let web = app
            .web_url
            .as_ref()
            .map(|u| format!("  {u}"))
            .unwrap_or_default();
        format!(
            " grokaagent  {}  {}  {}{}{}",
            app.status, auth, app.cache, kids, web
        )
    };
    let effort_bit = if app
        .catalog
        .find(&opts.model)
        .is_some_and(|m| !m.send_reasoning())
    {
        String::new()
    } else {
        format!(" · {}", opts.reasoning_effort.as_str())
    };
    let right = format!("{}{}{}  * ", task_chip_label(app), opts.model, effort_bit);
    (left, right)
}

fn task_chip_label(app: &App) -> &'static str {
    if app.task.snapshot().phase.is_live() {
        "任務●  "
    } else {
        "任務  "
    }
}

fn header_styles(running: bool) -> (Style, Style) {
    let left = if running {
        Style::default()
            .fg(Color::Black)
            .bg(ACCENT)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(DIM).bg(BG)
    };
    let right = Style::default()
        .fg(ACCENT)
        .bg(BG)
        .add_modifier(Modifier::BOLD);
    (left, right)
}

fn render_header_widgets(buf: &mut Buffer, app: &App, opts: &TuiOptions, area: Rect) {
    let (left, right) = header_copy(app, opts);
    let (left_style, right_style) = header_styles(app.running);
    let right_w = Line::from(right.as_str()).width() as u16;
    let split = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Min(10), Constraint::Length(right_w.max(12))])
        .split(area);
    Paragraph::new(Span::styled(left, left_style)).render(split[0], buf);
    Paragraph::new(Span::styled(right, right_style)).render(split[1], buf);
}

fn render_header_buffer(app: &App, opts: &TuiOptions, area: Rect) -> Buffer {
    let mut buf = Buffer::empty(area);
    render_header_widgets(&mut buf, app, opts, area);
    buf
}

fn copy_rect(src: &Buffer, dest: &mut Buffer, area: Rect) {
    for y in area.top()..area.bottom() {
        for x in area.left()..area.right() {
            let pos = Position { x, y };
            let Some(cell) = src.cell(pos) else {
                continue;
            };
            if let Some(out) = dest.cell_mut(pos) {
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

fn composer_snap_matches(app: &App) -> bool {
    app.composer_snap
        .as_ref()
        .is_some_and(|b| *b.area() == app.composer_frame)
        && app.composer_frame.width > 0
        && app.composer_frame.height > 0
}

fn rects_overlap(a: Rect, b: Rect) -> bool {
    a.width > 0
        && a.height > 0
        && b.width > 0
        && b.height > 0
        && a.left() < b.right()
        && b.left() < a.right()
        && a.top() < b.bottom()
        && b.top() < a.bottom()
}

fn settings_dock_rect(header: Rect) -> Rect {
    let label_w = display_cols(" 設定 ").max(6);
    let w = label_w.min(header.width.max(1));
    Rect::new(
        header.x + header.width.saturating_sub(w),
        header.y,
        w,
        1,
    )
}

fn push_buf_cells(src: &Buffer, area: Rect, out: &mut Vec<(u16, u16, Cell)>) {
    for y in area.top()..area.bottom() {
        for x in area.left()..area.right() {
            let pos = Position { x, y };
            if let Some(cell) = src.cell(pos) {
                out.push((x, y, cell.clone()));
            }
        }
    }
}

fn collect_clock_cells(app: &App, opts: &TuiOptions) -> Vec<(u16, u16, Cell)> {
    let mut out = Vec::new();
    if app.header_bar.width > 0 && app.header_bar.height > 0 {
        let mut buf = render_header_buffer(app, opts, app.header_bar);
        if app.settings.as_ref().is_some_and(|s| s.minimized) {
            let dock = settings_dock_rect(app.header_bar);
            Paragraph::new(Span::styled(
                " 設定 ",
                Style::default().bg(COMPOSER).fg(ACCENT),
            ))
            .render(dock, &mut buf);
        }
        push_buf_cells(&buf, app.header_bar, &mut out);
    }
    for &(ri, area) in &app.think_clocks {
        if area.width == 0 || area.height == 0 {
            continue;
        }
        if rects_overlap(area, app.composer_frame) {
            continue;
        }
        let Some(Row::Think(t)) = app.rows.get(ri) else {
            continue;
        };
        if t.done {
            continue;
        }
        let mut buf = Buffer::empty(area);
        Paragraph::new(think_header_line(t))
            .style(Style::default().bg(PANEL))
            .render(area, &mut buf);
        push_buf_cells(&buf, area, &mut out);
    }
    out.retain(|(x, y, _)| {
        !app.composer_frame.contains(Position { x: *x, y: *y })
    });
    out
}

fn clock_cells_changed(prev: &[(u16, u16, Cell)], next: &[(u16, u16, Cell)]) -> Vec<(u16, u16, Cell)> {
    next.iter()
        .filter(|(x, y, cell)| {
            !prev.iter().any(|(px, py, old)| {
                *px == *x && *py == *y && old.symbol() == cell.symbol()
            })
        })
        .cloned()
        .collect()
}

fn cell_char(cell: &Cell) -> Option<char> {
    cell.symbol().chars().next().filter(|c| *c != '\0')
}

fn paint_clocks(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    app: &mut App,
    opts: &TuiOptions,
) -> Result<()> {
    let next = collect_clock_cells(app, opts);
    let changed = clock_cells_changed(&app.last_clock_cells, &next);
    if changed.is_empty() {
        app.last_clock_cells = next;
        return Ok(());
    }
    let chars: Vec<(u16, u16, char)> = changed
        .iter()
        .filter_map(|(x, y, cell)| cell_char(cell).map(|ch| (*x, *y, ch)))
        .collect();
    let patched = crate::hostio::patch_chars_keep_cursor(&chars).map_err(Error::Io)?;
    if !patched {
        terminal
            .backend_mut()
            .draw(changed.iter().map(|(x, y, c)| (*x, *y, c)))
            .map_err(Error::Io)?;
        execute!(
            terminal.backend_mut(),
            MoveTo(app.last_caret.x, app.last_caret.y)
        )
        .map_err(Error::Io)?;
        std::io::Write::flush(terminal.backend_mut()).map_err(Error::Io)?;
    }
    app.last_clock_cells = next;
    Ok(())
}

fn paint_frame(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    app: &mut App,
    opts: &TuiOptions,
    freeze_composer: bool,
) -> Result<Position> {
    terminal.autoresize().map_err(Error::Io)?;
    let caret = {
        let mut frame = terminal.get_frame();
        draw_ui(&mut frame, app, opts, freeze_composer)
    };
    terminal.flush().map_err(Error::Io)?;
    std::io::Write::flush(terminal.backend_mut()).map_err(Error::Io)?;
    terminal.swap_buffers();
    Ok(caret)
}

fn sync_cursor(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    caret: Position,
    want: bool,
    shown: &mut bool,
    placed: &mut Option<Position>,
) -> Result<()> {
    if *placed != Some(caret) {
        execute!(terminal.backend_mut(), MoveTo(caret.x, caret.y)).map_err(Error::Io)?;
        *placed = Some(caret);
    }
    if want != *shown {
        if want {
            terminal.show_cursor().map_err(Error::Io)?;
        } else {
            terminal.hide_cursor().map_err(Error::Io)?;
        }
        *shown = want;
    }
    std::io::Write::flush(terminal.backend_mut()).map_err(Error::Io)?;
    Ok(())
}

fn draw_header(f: &mut Frame, app: &mut App, opts: &TuiOptions, area: Rect) {
    app.header_bar = area;
    render_header_widgets(f.buffer_mut(), app, opts, area);
    let (_, right) = header_copy(app, opts);
    let right_w = Line::from(right.as_str()).width() as u16;
    let split = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Min(10), Constraint::Length(right_w.max(12))])
        .split(area);
    app.hits.push((split[1], Hit::ModelChip));
    let tw = display_cols(task_chip_label(app)).min(split[1].width);
    app.hits.push((
        Rect::new(split[1].x, split[1].y, tw, 1),
        Hit::TaskChip,
    ));
    let gear = Rect::new(
        split[1].x + split[1].width.saturating_sub(3),
        split[1].y,
        3,
        1,
    );
    app.hits.push((gear, Hit::Gear));
}

fn draw_chat(f: &mut Frame, app: &mut App, area: Rect) -> Vec<(Rect, Hit)> {
    let block = Block::default()
        .borders(Borders::TOP)
        .border_style(Style::default().fg(BORDER))
        .style(Style::default().bg(PANEL).fg(TEXT));
    f.render_widget(block, area);
    app.hits.push((area, Hit::Chat));
    app.think_clocks.clear();

    let bar_w = if area.width >= 8 { 1 } else { 0 };
    let inner = Rect::new(
        area.x.saturating_add(1),
        area.y.saturating_add(1),
        area.width.saturating_sub(2 + bar_w),
        area.height.saturating_sub(1),
    );
    app.chat_inner = inner;
    app.chat_bar = if bar_w == 0 {
        Rect::default()
    } else {
        Rect::new(
            area.x + area.width.saturating_sub(1),
            inner.y,
            1,
            inner.height,
        )
    };
    let mut tool_hits = Vec::new();
    if inner.width == 0 || inner.height == 0 {
        return tool_hits;
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
    let mut paints: Vec<Paint> = Vec::new();
    for item in chat_logical_rows(app, inner.width) {
        if let Some((rel, width, height)) = item.graphic {
            paints.push(Paint::Graphic {
                rel,
                width,
                height,
                hit: item.hit,
            });
            continue;
        }
        if let Some((row, start)) = item.select {
            let pieces = if item.wrap {
                wrap_line_indexed(item.line, inner.width)
            } else {
                let chars: Vec<char> = selectable_line_text(&item.line).chars().collect();
                vec![WrapPiece {
                    line: item.line,
                    start: 0,
                    chars,
                }]
            };
            for p in pieces {
                paints.push(Paint::Line {
                    line: p.line,
                    hit: item.hit,
                    glyph: Some((row, start.saturating_add(p.start), p.chars)),
                    clock: false,
                });
            }
        } else {
            let wrapped = if item.wrap {
                wrap_visual(item.line, inner.width)
            } else {
                vec![item.line]
            };
            let mut first = true;
            for wrapped in wrapped {
                paints.push(Paint::Line {
                    line: wrapped,
                    hit: item.hit,
                    glyph: None,
                    clock: item.clock && first,
                });
                first = false;
            }
        }
    }
    let total: u16 = paints
        .iter()
        .map(|p| match p {
            Paint::Line { .. } => 1,
            Paint::Graphic { height, .. } => *height,
        })
        .sum();
    let max_off = total.saturating_sub(inner.height);
    app.chat_total = total;
    app.chat_max_off = max_off;
    let scroll = if app.stick_bottom {
        max_off
    } else {
        max_off.saturating_sub(app.scroll.min(max_off))
    };
    let vis_end = scroll.saturating_add(inner.height);
    let sel = app.chat_sel.clone();
    let mut logical = 0u16;
    for paint in paints {
        let h = match &paint {
            Paint::Line { .. } => 1,
            Paint::Graphic { height, .. } => *height,
        };
        let start = logical;
        let end = logical.saturating_add(h);
        logical = end;
        if end <= scroll || start >= vis_end {
            continue;
        }
        let screen_y = inner.y + start.saturating_sub(scroll);
        let vis_h = end.min(vis_end).saturating_sub(start.max(scroll)).max(1);
        match paint {
            Paint::Line { mut line, hit, glyph, clock } => {
                if let Some((row, gstart, chars)) = &glyph {
                    if let Some((lo, hi)) = piece_tint_range(&sel, *row, *gstart, chars.len()) {
                        line = tint_char_range(line, lo, hi);
                    }
                }
                let r = Rect::new(inner.x, screen_y, inner.width, 1);
                f.render_widget(Paragraph::new(line), r);
                if clock {
                    if let Some(Hit::Think(i)) = hit {
                        app.think_clocks.push((i, r));
                    }
                }
                if let Some((grow, gstart, chars)) = glyph {
                    let text_w: u16 = chars.iter().map(|c| ch_width(*c).max(1)).sum();
                    let text_w = text_w.min(inner.width);
                    if text_w > 0 {
                        if let Some(kind) = hit {
                            let hr = Rect::new(inner.x, screen_y, text_w, 1);
                            app.hits.push((hr, kind));
                            tool_hits.push((hr, kind));
                        }
                    }
                    app.chat_glyphs.push(ChatGlyphLine {
                        y: screen_y,
                        x: inner.x,
                        text_w,
                        row: grow,
                        start: gstart,
                        chars,
                    });
                } else if let Some(kind) = hit {
                    app.hits.push((r, kind));
                    tool_hits.push((r, kind));
                }
            }
            Paint::Graphic {
                rel,
                width,
                height,
                hit,
            } => {
                let fully = start >= scroll && end <= vis_end;
                if fully {
                    if let Some(proto) = cached_graphic(app, &rel, width, height) {
                        let area = proto.area();
                        let draw = Rect::new(
                            inner.x.saturating_add(crate::preview::INDENT),
                            screen_y,
                            area.width.min(inner.width.saturating_sub(crate::preview::INDENT)),
                            area.height.min(height),
                        );
                        if draw.width > 0 && draw.height > 0 {
                            paint_chat_graphic(f, app, &proto, draw);
                        }
                    } else {
                        f.render_widget(
                            Paragraph::new(Span::styled(
                                "      [無法預覽]",
                                Style::default().fg(DIM),
                            )),
                            Rect::new(inner.x, screen_y, inner.width, 1),
                        );
                    }
                }
                let hit_r = Rect::new(inner.x, screen_y, inner.width, vis_h);
                if let Some(kind) = hit {
                    app.hits.push((hit_r, kind));
                    tool_hits.push((hit_r, kind));
                }
            }
        }
    }
    draw_chat_scrollbar(f, app, max_off, inner.height);
    tool_hits
}

fn cached_graphic(app: &mut App, rel: &str, w: u16, h: u16) -> Option<Protocol> {
    let key = (rel.to_string(), w, h);
    if let Some(p) = app.image_proto.get(&key) {
        return Some(p.clone());
    }
    let picker = app.picker.clone()?;
    let abs = app.session.workspace.join(rel);
    let proto = crate::preview::protocol_for(&picker, &abs, w, h)?;
    app.image_proto.insert(key, proto.clone());
    Some(proto)
}

fn paint_chat_graphic(f: &mut Frame, app: &mut App, proto: &Protocol, draw: Rect) {
    if let Some(data) = crate::preview::immediate_payload(proto) {
        crate::preview::reserve_graphic_cells(f.buffer_mut(), draw);
        app.graphic_blits.push(crate::preview::GraphicBlit {
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

fn flush_image_blits(app: &mut App, caret: Position) -> Result<()> {
    if app.graphic_blits == app.last_graphic_blits {
        return Ok(());
    }
    crate::preview::write_blits(&mut io::stdout(), &app.graphic_blits)?;
    if !app.graphic_blits.is_empty() {
        execute!(io::stdout(), MoveTo(caret.x, caret.y)).map_err(Error::Io)?;
    }
    app.last_graphic_blits.clone_from(&app.graphic_blits);
    Ok(())
}

const WHEEL_LINES: u16 = 3;

fn chat_scroll_step(_app: &App) -> u16 {
    WHEEL_LINES
}

fn page_scroll_step(app: &App) -> u16 {
    app.chat_inner.height.saturating_sub(1).max(8)
}

fn scroll_chat(app: &mut App, delta: i32) {
    if delta > 0 {
        app.stick_bottom = false;
        app.scroll = app.scroll.saturating_add(delta as u16);
        return;
    }
    app.scroll = app.scroll.saturating_sub((-delta) as u16);
    if app.scroll == 0 {
        app.stick_bottom = true;
    }
}

fn jump_chat_bottom(app: &mut App) {
    app.stick_bottom = true;
    app.scroll = 0;
}

fn scrollbar_thumb_h(total: u16, view_h: u16, track_h: u16) -> u16 {
    if track_h == 0 {
        return 0;
    }
    if total <= view_h {
        return track_h;
    }
    let h = (view_h as u32 * track_h as u32) / total.max(1) as u32;
    (h as u16).clamp(2.min(track_h).max(1), track_h)
}

/// Map thumb top (relative to track) to `app.scroll` (offset from bottom).
fn scrollbar_offset(max_off: u16, track_h: u16, thumb_h: u16, rel_y: u16) -> u16 {
    let travel = track_h.saturating_sub(thumb_h);
    if travel == 0 || max_off == 0 {
        return 0;
    }
    let rel = rel_y.min(travel);
    let visual = (rel as u32 * max_off as u32 + (travel as u32 / 2)) / travel as u32;
    max_off.saturating_sub(visual as u16)
}

fn scrollbar_thumb_rel(max_off: u16, track_h: u16, thumb_h: u16, scroll: u16) -> u16 {
    let travel = track_h.saturating_sub(thumb_h);
    if travel == 0 || max_off == 0 {
        return 0;
    }
    let visual = max_off.saturating_sub(scroll.min(max_off));
    ((visual as u32 * travel as u32) / max_off as u32) as u16
}

fn apply_scroll_from_row(app: &mut App, row: u16) {
    let track = app.chat_bar;
    if track.height == 0 {
        return;
    }
    let thumb_h = scrollbar_thumb_h(app.chat_total, app.chat_inner.height, track.height);
    let grab = app.scroll_grab.unwrap_or(0);
    let y = (row as i16 - grab).clamp(track.y as i16, {
        let last = track.y as i16 + track.height as i16 - thumb_h.max(1) as i16;
        last.max(track.y as i16)
    }) as u16;
    let rel = y.saturating_sub(track.y);
    app.scroll = scrollbar_offset(app.chat_max_off, track.height, thumb_h, rel);
    app.stick_bottom = app.scroll == 0;
}

fn begin_scroll_drag(app: &mut App, row: u16, on_thumb: bool) {
    let track = app.chat_bar;
    if track.height == 0 {
        return;
    }
    let thumb_h = scrollbar_thumb_h(app.chat_total, app.chat_inner.height, track.height);
    let scroll = if app.stick_bottom {
        0
    } else {
        app.scroll.min(app.chat_max_off)
    };
    let thumb_rel = scrollbar_thumb_rel(app.chat_max_off, track.height, thumb_h, scroll);
    let thumb_y = track.y.saturating_add(thumb_rel);
    if on_thumb {
        app.scroll_grab = Some(row as i16 - thumb_y as i16);
    } else {
        app.scroll_grab = Some((thumb_h / 2) as i16);
    }
    apply_scroll_from_row(app, row);
}

fn draw_chat_scrollbar(f: &mut Frame, app: &mut App, max_off: u16, view_h: u16) {
    let track = app.chat_bar;
    if track.width == 0 || track.height == 0 {
        return;
    }
    f.render_widget(
        Block::default().style(Style::default().bg(BORDER)),
        track,
    );
    app.hits.push((track, Hit::ScrollBar));
    if max_off == 0 {
        return;
    }
    let thumb_h = scrollbar_thumb_h(app.chat_total, view_h, track.height);
    let scroll = if app.stick_bottom {
        0
    } else {
        app.scroll.min(max_off)
    };
    let rel = scrollbar_thumb_rel(max_off, track.height, thumb_h, scroll);
    let thumb = Rect::new(track.x, track.y.saturating_add(rel), 1, thumb_h.max(1));
    f.render_widget(
        Block::default().style(Style::default().bg(ACCENT)),
        thumb,
    );
    app.hits.push((thumb, Hit::ScrollThumb));
}

fn draw_jump_bottom(f: &mut Frame, app: &mut App, chat: Rect) {
    if app.stick_bottom || app.image_view.is_some() {
        return;
    }
    let label = " ▼ ";
    let w = display_cols(label);
    if chat.width < w + 2 || chat.height < 2 {
        return;
    }
    let x = chat.x + chat.width.saturating_sub(w) / 2;
    let y = chat.y + chat.height.saturating_sub(1);
    let r = Rect::new(x, y, w, 1);
    f.render_widget(Clear, r);
    f.render_widget(
        Paragraph::new(Span::styled(
            label,
            Style::default()
                .fg(Color::Black)
                .bg(ACCENT)
                .add_modifier(Modifier::BOLD),
        )),
        r,
    );
    app.hits.push((r, Hit::JumpBottom));
}

fn image_view_rect(area: Rect) -> Rect {
    let max_w = area.width.saturating_sub(2).max(1);
    let max_h = area.height.saturating_sub(2).max(1);
    let w = (area.width.saturating_mul(9) / 10).min(max_w).max(24.min(max_w));
    let h = (area.height.saturating_mul(9) / 10).min(max_h).max(12.min(max_h));
    let x = area.x + area.width.saturating_sub(w) / 2;
    let y = area.y + area.height.saturating_sub(h) / 2;
    Rect::new(x, y, w, h)
}

fn draw_image_view(f: &mut Frame, app: &mut App, area: Rect) {
    let Some(rel) = app.image_view.clone() else {
        return;
    };
    app.hits.push((area, Hit::ImageViewDismiss));
    let panel = image_view_rect(area);
    f.render_widget(Clear, panel);
    let name = std::path::Path::new(&rel)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or(&rel);
    f.render_widget(
        Block::default()
            .title(format!(" 圖片  {name}  · Esc 關閉  · Ctrl+C 複製 "))
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(ACCENT))
            .style(Style::default().bg(PANEL).fg(TEXT)),
        panel,
    );
    let close = Rect::new(
        panel.x + panel.width.saturating_sub(4),
        panel.y,
        3,
        1,
    );
    f.render_widget(
        Paragraph::new(Span::styled(" × ", Style::default().fg(WARN))),
        close,
    );
    app.hits.push((panel, Hit::ImageView));
    app.hits.push((close, Hit::ImageViewClose));

    let inner = Rect::new(
        panel.x.saturating_add(1),
        panel.y.saturating_add(1),
        panel.width.saturating_sub(2),
        panel.height.saturating_sub(2),
    );
    if inner.width == 0 || inner.height == 0 {
        return;
    }
    let abs = app.session.workspace.join(&rel);
    let picker = app.picker.clone();
    if let Some(picker) = picker.filter(|p| crate::preview::uses_graphics(p)) {
        let (w, h) = crate::preview::cell_size_fit(&picker, &abs, inner.width, inner.height);
        if let Some(proto) = cached_graphic(app, &rel, w, h) {
            let area = proto.area();
            let draw = Rect::new(
                inner.x,
                inner.y,
                area.width.min(inner.width),
                area.height.min(inner.height),
            );
            if draw.width > 0 && draw.height > 0 {
                paint_chat_graphic(f, app, &proto, draw);
            }
        } else {
            f.render_widget(
                Paragraph::new(Span::styled("[無法預覽]", Style::default().fg(DIM))),
                inner,
            );
        }
        return;
    }
    let lines = crate::preview::from_path(&abs, inner.width, inner.height);
    f.render_widget(Paragraph::new(lines), inner);
}

fn draw_skill_view(f: &mut Frame, app: &mut App, area: Rect) -> Option<Position> {
    app.hits.push((area, Hit::SkillViewDismiss));
    let panel = image_view_rect(area);
    f.render_widget(Clear, panel);
    let title = app
        .skill_view
        .as_ref()
        .map(|v| format!(" 技能  {}  · {}  · 只讀  · 可選取複製  · Esc 關閉 ", v.title, v.origin))
        .unwrap_or_else(|| " 技能 ".into());
    f.render_widget(
        Block::default()
            .title(title)
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(ACCENT))
            .style(Style::default().bg(PANEL).fg(TEXT)),
        panel,
    );
    let close = Rect::new(panel.x + panel.width.saturating_sub(4), panel.y, 3, 1);
    f.render_widget(
        Paragraph::new(Span::styled(" × ", Style::default().fg(WARN))),
        close,
    );
    app.hits.push((panel, Hit::SkillView));
    app.hits.push((close, Hit::SkillViewClose));
    let inner = Rect::new(
        panel.x.saturating_add(1),
        panel.y.saturating_add(1),
        panel.width.saturating_sub(2),
        panel.height.saturating_sub(2),
    );
    let Some(view) = app.skill_view.as_mut() else {
        return None;
    };
    view.inner = inner;
    if inner.width == 0 || inner.height == 0 {
        return None;
    }
    let mut line = Line::from(view.edit.text.clone());
    if let Some((lo, hi)) = view.edit.sel_range() {
        line = tint_char_range(line, lo, hi);
    }
    let wrapped = wrap_visual(line, inner.width.max(1));
    let total = wrapped.len() as u16;
    let max_off = total.saturating_sub(inner.height);
    view.scroll = view.scroll.min(max_off);
    let start = view.scroll as usize;
    let vis: Vec<Line<'static>> = wrapped
        .into_iter()
        .skip(start)
        .take(inner.height as usize)
        .collect();
    f.render_widget(Paragraph::new(vis), inner);
    let ranges = wrap_lines(&view.edit.text, inner.width.max(1));
    let (row, col) = caret_row_col(&view.edit.text, &ranges, view.edit.caret);
    if row >= view.scroll && row.saturating_sub(view.scroll) < inner.height {
        Some(Position::new(
            inner.x.saturating_add(col.min(inner.width.saturating_sub(1))),
            inner.y + row.saturating_sub(view.scroll),
        ))
    } else {
        None
    }
}

fn draw_tool_panel(f: &mut Frame, app: &mut App, chat: Rect, tool_hits: &[(Rect, Hit)]) {
    let Some((ri, ci)) = app.open_tool else {
        return;
    };
    let Some(call) = app.rows.get(ri).and_then(|r| match r {
        Row::Tools(g) => g.calls.get(ci).cloned(),
        _ => None,
    }) else {
        app.open_tool = None;
        return;
    };

    app.hits.push((chat, Hit::DismissTool));
    for &(r, h) in tool_hits {
        app.hits.push((r, h));
    }

    let w = chat.width.saturating_sub(4).min(76).max(28);
    let h = chat.height.saturating_sub(2).min(24).max(8);
    let x = chat.x + chat.width.saturating_sub(w) / 2;
    let y = chat.y.saturating_add(1);
    let panel = Rect::new(x, y, w, h);
    f.render_widget(Clear, panel);
    let block = Block::default()
        .title(format!(" {} ", call.name))
        .title_style(Style::default().fg(TEXT).add_modifier(Modifier::BOLD))
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(ACCENT))
        .style(Style::default().bg(COMPOSER).fg(TEXT));
    f.render_widget(block, panel);

    let close = Rect::new(panel.x + panel.width.saturating_sub(4), panel.y, 3, 1);
    f.render_widget(
        Paragraph::new(Span::styled(
            " × ",
            Style::default().fg(DIM).bg(COMPOSER),
        )),
        close,
    );

    let inner = Rect::new(
        panel.x.saturating_add(1),
        panel.y.saturating_add(1),
        panel.width.saturating_sub(2),
        panel.height.saturating_sub(2),
    );
    let details = call_detail_lines(&call);
    let mut y = inner.y;
    for line in details {
        if y >= inner.y + inner.height {
            break;
        }
        for wrapped in wrap_visual(line, inner.width.max(1)) {
            if y >= inner.y + inner.height {
                break;
            }
            f.render_widget(Paragraph::new(wrapped), Rect::new(inner.x, y, inner.width, 1));
            y = y.saturating_add(1);
        }
    }
    app.hits.push((panel, Hit::ToolPanel));
    app.hits.push((close, Hit::ToolPanelClose));
}

fn composer_height(queue_shown: usize, attach: u16) -> u16 {
    6 + queue_shown.min(3) as u16 + attach
}

fn draw_composer(f: &mut Frame, app: &mut App, opts: &TuiOptions, area: Rect) -> Position {
    let block = Block::default()
        .borders(Borders::TOP)
        .border_style(Style::default().fg(BORDER))
        .style(Style::default().bg(BG));
    f.render_widget(block, area);
    let inner = Rect::new(
        area.x.saturating_add(1),
        area.y.saturating_add(1),
        area.width.saturating_sub(2),
        area.height.saturating_sub(1),
    );
    let shown = app.queue.len().min(3);
    let has_attach = !app.pending.is_empty();
    let mut constraints = vec![Constraint::Length(1)];
    if shown > 0 {
        constraints.push(Constraint::Length(shown as u16));
    }
    if has_attach {
        constraints.push(Constraint::Length(1));
    }
    constraints.push(Constraint::Min(2));
    constraints.push(Constraint::Length(1));
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints(constraints)
        .split(inner);
    let chip_row = rows[0];
    let mut idx = 1;
    let list_row = if shown > 0 {
        let r = rows[idx];
        idx += 1;
        Some(r)
    } else {
        None
    };
    let attach_row = if has_attach {
        let r = rows[idx];
        idx += 1;
        Some(r)
    } else {
        None
    };
    let box_area = rows[idx];
    let hint_row = rows[idx + 1];

    let qn = app.queue.len();
    let q_style = if app.send_mode == SendMode::Queue {
        Style::default().bg(ACCENT).fg(Color::Black).add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(DIM)
    };
    let i_style = if app.send_mode == SendMode::Insert {
        Style::default().bg(ACCENT).fg(Color::Black).add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(DIM)
    };
    let q_label = if qn == 0 {
        " 接著做 ".to_string()
    } else {
        format!(" {qn} 已排隊 ")
    };
    let q_w = Line::from(q_label.as_str()).width() as u16;
    let i_w = 10u16;
    let q_rect = Rect::new(chip_row.x, chip_row.y, q_w, 1);
    let i_rect = Rect::new(chip_row.x + q_w + 1, chip_row.y, i_w, 1);
    f.render_widget(Paragraph::new(Span::styled(q_label, q_style)), q_rect);
    f.render_widget(Paragraph::new(Span::styled(" 調整工作 ", i_style)), i_rect);
    app.hits.push((q_rect, Hit::QueueChip));
    app.hits.push((i_rect, Hit::InsertChip));
    let paste_label = " 貼上圖片 ";
    let p_w = display_cols(paste_label);
    let p_rect = Rect::new(i_rect.x.saturating_add(i_w + 1), chip_row.y, p_w, 1);
    if p_rect.x + p_w <= chip_row.x + chip_row.width {
        f.render_widget(
            Paragraph::new(Span::styled(
                paste_label,
                Style::default()
                    .fg(Color::Black)
                    .bg(ACCENT)
                    .add_modifier(Modifier::BOLD),
            )),
            p_rect,
        );
        app.hits.push((p_rect, Hit::PasteImage));
    }

    let mut x = p_rect.x.saturating_add(p_w + 1);
    if app.queue_edit.is_some() {
        let n = app.queue_edit.unwrap() + 1;
        let label = format!(" 編輯排隊 #{n} ");
        let lw = Line::from(label.as_str()).width() as u16;
        if x + lw < chip_row.x + chip_row.width {
            f.render_widget(
                Paragraph::new(Span::styled(
                    label,
                    Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
                )),
                Rect::new(x, chip_row.y, lw, 1),
            );
            x = x.saturating_add(lw + 1);
        }
        let cancel = " 取消 ";
        let cw = 6u16;
        let right = chip_row.x + chip_row.width;
        let cancel_x = if x + cw <= right {
            x
        } else {
            right.saturating_sub(cw)
        };
        let cancel_rect = Rect::new(cancel_x, chip_row.y, cw, 1);
        f.render_widget(
            Paragraph::new(Span::styled(
                cancel,
                Style::default().bg(WARN).fg(Color::Black).add_modifier(Modifier::BOLD),
            )),
            cancel_rect,
        );
        app.hits.push((cancel_rect, Hit::CancelQueueEdit));
    }

    if let Some(list) = list_row {
        for i in 0..shown {
            let y = list.y.saturating_add(i as u16);
            let editing = app.queue_edit == Some(i);
            let text = if editing {
                app.edit.text.replace('\n', " ")
            } else {
                app.queue.get(i).map(|q| q.label()).unwrap_or_default()
            };
            let mark = if editing { "▸" } else { "•" };
            let prefix = format!(" {mark} ");
            let avail = list.width.saturating_sub(Line::from(prefix.as_str()).width() as u16);
            let body = truncate_width(&text, avail);
            let style = if editing {
                Style::default().fg(ACCENT)
            } else {
                Style::default().fg(DIM)
            };
            let row = Rect::new(list.x, y, list.width, 1);
            f.render_widget(
                Paragraph::new(Line::from(vec![
                    Span::styled(prefix, style),
                    Span::styled(body, style),
                ])),
                row,
            );
            app.hits.push((row, Hit::QueueItem(i as u16)));
        }
    }

    if let Some(row) = attach_row {
        let mut x = row.x;
        for (i, rel) in app.pending.iter().enumerate() {
            let name = std::path::Path::new(rel)
                .file_name()
                .and_then(|s| s.to_str())
                .unwrap_or(rel);
            let label = format!(" {name} × ");
            let w = Line::from(label.as_str()).width() as u16;
            if x + w > row.x + row.width {
                break;
            }
            let r = Rect::new(x, row.y, w, 1);
            f.render_widget(
                Paragraph::new(Span::styled(
                    label,
                    Style::default().bg(COMPOSER).fg(ACCENT),
                )),
                r,
            );
            app.hits.push((r, Hit::PendingClose(i as u16)));
            x = x.saturating_add(w + 1);
        }
    }

    let input_block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(if app.focus == Focus::Chat { ACCENT } else { BORDER }))
        .style(Style::default().bg(COMPOSER).fg(TEXT));
    f.render_widget(input_block, box_area);
    app.hits.push((box_area, Hit::Composer));

    let inner_box = Rect::new(
        box_area.x.saturating_add(1),
        box_area.y.saturating_add(1),
        box_area.width.saturating_sub(2),
        box_area.height.saturating_sub(2),
    );
    app.composer_inner = inner_box;

    let caret = if app.edit.is_empty() {
        let placeholder: &str = if app.queue_edit.is_some() {
            "編輯排隊訊息…  Enter 完成  ·  空白則移除  ·  點取消或 Esc 還原"
        } else if app.running {
            "模型工作中 · Esc 停止 · Enter 依所選模式送出 · Ctrl+Enter 調整工作"
        } else {
            "傳訊息，或點「貼上圖片」…"
        };
        f.render_widget(
            Paragraph::new(Span::styled(
                placeholder,
                Style::default().fg(DIM).bg(COMPOSER),
            ))
            .style(Style::default().bg(COMPOSER)),
            inner_box,
        );
        app.composer_vscroll = 0;
        Position::new(inner_box.x, inner_box.y)
    } else {
        draw_edit(f, inner_box, &app.edit, &mut app.composer_vscroll)
    };

    let hint = if app.queue_edit.is_some() {
        "Enter 完成編輯  ·  Esc 或點取消 還原  ·  工作結束也不會送出，直到編輯完成".to_string()
    } else {
        format!(
            "Enter 送出 · Shift+Enter 換行 · Esc 停止 · Ctrl+B 側欄 · Ctrl+J 面板 · Ctrl+Q 離開 · {} · {}",
            opts.model,
            opts.reasoning_effort.label()
        )
    };
    f.render_widget(
        Paragraph::new(Span::styled(hint, Style::default().fg(DIM))),
        hint_row,
    );

    caret
}

fn draw_edit(f: &mut Frame, inner: Rect, edit: &Edit, vscroll: &mut u16) -> Position {
    let width = inner.width.max(1);
    let height = inner.height.max(1);
    let ranges = wrap_lines(&edit.text, width);
    let (crow, ccol) = caret_row_col(&edit.text, &ranges, edit.caret);
    *vscroll = crow.saturating_sub(height.saturating_sub(1));
    let sel = edit.sel_range();
    let chars: Vec<char> = edit.text.chars().collect();
    let mut lines: Vec<Line> = Vec::new();
    let start = *vscroll as usize;
    let end = (start + height as usize).min(ranges.len());
    for &(a, b) in &ranges[start..end] {
        lines.push(edit_line(&chars, a, b, sel));
    }
    if lines.is_empty() {
        lines.push(Line::from(""));
    }
    f.render_widget(
        Paragraph::new(lines).style(Style::default().fg(TEXT).bg(COMPOSER)),
        inner,
    );
    let screen_row = crow.saturating_sub(*vscroll);
    Position::new(
        inner.x.saturating_add(ccol.min(width.saturating_sub(1))),
        inner.y.saturating_add(screen_row.min(height.saturating_sub(1))),
    )
}

fn edit_line(chars: &[char], a: usize, b: usize, sel: Option<(usize, usize)>) -> Line<'static> {
    if a >= b {
        return Line::from("");
    }
    let Some((lo, hi)) = sel else {
        let s: String = chars[a..b].iter().collect();
        return Line::from(Span::styled(s, Style::default().fg(TEXT).bg(COMPOSER)));
    };
    let mut spans = Vec::new();
    let mut i = a;
    while i < b {
        let selected = i >= lo && i < hi;
        let mut j = i + 1;
        while j < b && (j >= lo && j < hi) == selected {
            j += 1;
        }
        let s: String = chars[i..j].iter().collect();
        let style = if selected {
            Style::default().bg(ACCENT).fg(Color::Black)
        } else {
            Style::default().fg(TEXT).bg(COMPOSER)
        };
        spans.push(Span::styled(s, style));
        i = j;
    }
    Line::from(spans)
}

fn draw_ask(f: &mut Frame, app: &mut App) -> Option<Position> {
    let area = f.area();
    let ask = app.ask.as_ref()?;
    let n = ask.n() as u16;
    let inner_w = area.width.saturating_sub(10).min(72).max(36);
    let q_rows = wrap_lines(&ask.question.prompt, inner_w.saturating_sub(2))
        .len()
        .max(1) as u16;
    let fill_h = if ask.filling { 3 } else { 0 };
    let h = (q_rows + n + fill_h + 6)
        .min(area.height.saturating_sub(2))
        .max(10);
    let w = inner_w.saturating_add(4).min(area.width.saturating_sub(2));
    let x = area.x + area.width.saturating_sub(w) / 2;
    let y = area.y + area.height.saturating_sub(h) / 2;
    let r = Rect::new(x, y, w, h);
    f.render_widget(Clear, r);
    let block = Block::default()
        .title(" 問卷 ")
        .title_style(Style::default().fg(TEXT).add_modifier(Modifier::BOLD))
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(ACCENT))
        .style(Style::default().bg(COMPOSER).fg(TEXT));
    f.render_widget(block, r);
    app.hits.push((r, Hit::AskPanel));

    let body = Rect::new(
        r.x.saturating_add(2),
        r.y.saturating_add(1),
        r.width.saturating_sub(4),
        r.height.saturating_sub(2),
    );
    let mut constraints = vec![Constraint::Length(q_rows.max(1))];
    for _ in 0..n {
        constraints.push(Constraint::Length(1));
    }
    if fill_h > 0 {
        constraints.push(Constraint::Length(fill_h));
    }
    constraints.push(Constraint::Length(1));
    constraints.push(Constraint::Length(1));
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints(constraints)
        .split(body);

    let q_style = Style::default().fg(TEXT).add_modifier(Modifier::BOLD);
    f.render_widget(
        Paragraph::new(ask.question.prompt.clone()).style(q_style).wrap(ratatui::widgets::Wrap { trim: true }),
        rows[0],
    );

    let mut row_i = 1usize;
    for i in 0..ask.n() {
        let opt = &ask.question.options[i];
        let chosen = ask.chosen.get(i).copied().unwrap_or(false);
        let mark = if ask.question.allow_multiple {
            if chosen { "☑" } else { "☐" }
        } else if chosen {
            "●"
        } else {
            "○"
        };
        let pointer = if i == ask.cursor { "›" } else { " " };
        let extra = if opt.input { "  （可填寫）" } else { "" };
        let selected = i == ask.cursor;
        let style = if selected {
            Style::default().fg(Color::Black).bg(ACCENT)
        } else if chosen {
            Style::default().fg(ACCENT)
        } else {
            Style::default().fg(TEXT)
        };
        let label = format!(" {pointer} {mark}  {}{extra} ", opt.label);
        let row = rows[row_i];
        f.render_widget(Paragraph::new(Span::styled(label, style)), row);
        app.hits.push((row, Hit::AskOption(i as u16)));
        row_i += 1;
    }

    let mut caret = None;
    if fill_h > 0 {
        let box_r = rows[row_i];
        row_i += 1;
        let fill_block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(ACCENT))
            .style(Style::default().bg(PANEL));
        f.render_widget(fill_block, box_r);
        let inner = Rect::new(
            box_r.x.saturating_add(1),
            box_r.y.saturating_add(1),
            box_r.width.saturating_sub(2),
            box_r.height.saturating_sub(2).max(1),
        );
        app.ask_fill_inner = inner;
        app.hits.push((box_r, Hit::AskFill));
        if let Some(ask) = app.ask.as_mut() {
            caret = Some(draw_edit(f, inner, &ask.fill_edit, &mut ask.fill_scroll));
        }
    }

    let btns = rows[row_i];
    let cancel_label = " 取消 ";
    let ok_label = " 確定 ";
    let cw = Line::from(cancel_label).width() as u16;
    let ow = Line::from(ok_label).width() as u16;
    let ok_r = Rect::new(btns.x, btns.y, ow, 1);
    let cancel_r = Rect::new(btns.x.saturating_add(ow + 2), btns.y, cw, 1);
    f.render_widget(
        Paragraph::new(Span::styled(
            ok_label,
            Style::default().fg(Color::Black).bg(ACCENT).add_modifier(Modifier::BOLD),
        )),
        ok_r,
    );
    f.render_widget(
        Paragraph::new(Span::styled(cancel_label, Style::default().fg(DIM).bg(PANEL))),
        cancel_r,
    );
    app.hits.push((ok_r, Hit::AskConfirm));
    app.hits.push((cancel_r, Hit::AskCancel));

    let hint = if app.ask.as_ref().is_some_and(|a| a.filling) {
        "輸入自訂內容  ·  Enter 確定  ·  Esc 回到選項"
    } else if app.ask.as_ref().is_some_and(|a| a.question.allow_multiple) {
        "↑↓ 移動  ·  空白鍵勾選  ·  Enter 確定  ·  Esc 取消"
    } else {
        "↑↓ 移動  ·  Enter 選擇  ·  可填寫項會開啟輸入框  ·  Esc 取消"
    };
    if let Some(hint_row) = rows.get(row_i + 1) {
        f.render_widget(
            Paragraph::new(Span::styled(hint, Style::default().fg(DIM))),
            *hint_row,
        );
    }
    caret
}

fn draw_task(f: &mut Frame, app: &mut App) -> Option<Position> {
    let area = f.area();
    let w = area.width.saturating_sub(8).min(72).max(40);
    let form = matches!(app.task_ui, Some(TaskUi::Form { .. }));
    let h = if form { 14 } else { 16 }
        .min(area.height.saturating_sub(2))
        .max(10);
    let x = area.x + area.width.saturating_sub(w) / 2;
    let y = area.y + area.height.saturating_sub(h) / 2;
    let r = Rect::new(x, y, w, h);
    f.render_widget(Clear, r);
    let title = if form { " 任務目標 " } else { " 任務模式 " };
    let block = Block::default()
        .title(title)
        .title_style(Style::default().fg(TEXT).add_modifier(Modifier::BOLD))
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(ACCENT))
        .style(Style::default().bg(COMPOSER).fg(TEXT));
    f.render_widget(block, r);
    app.hits.push((r, Hit::TaskPanel));

    let body = Rect::new(
        r.x.saturating_add(2),
        r.y.saturating_add(1),
        r.width.saturating_sub(4),
        r.height.saturating_sub(2),
    );
    if form {
        f.render_widget(
            Paragraph::new(Span::styled(
                "寫下這則對話要達成的目標。Esc 取消。",
                Style::default().fg(DIM),
            )),
            Rect::new(body.x, body.y, body.width, 1),
        );
        let box_h = body.height.saturating_sub(4).max(3);
        let box_r = Rect::new(body.x, body.y.saturating_add(2), body.width, box_h);
        f.render_widget(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(BORDER))
                .style(Style::default().bg(PANEL)),
            box_r,
        );
        let inner = Rect::new(
            box_r.x.saturating_add(1),
            box_r.y.saturating_add(1),
            box_r.width.saturating_sub(2),
            box_r.height.saturating_sub(2),
        );
        app.task_draft_inner = inner;
        app.hits.push((inner, Hit::TaskDraft));
        let caret = if let Some(TaskUi::Form { edit }) = &app.task_ui {
            let mut vscroll = 0u16;
            draw_edit(f, inner, edit, &mut vscroll)
        } else {
            Position::new(inner.x, inner.y)
        };
        let btn_y = body.y + body.height.saturating_sub(1);
        let cancel = Rect::new(body.x, btn_y, 8, 1);
        let ok = Rect::new(body.x.saturating_add(10), btn_y, 8, 1);
        f.render_widget(
            Paragraph::new(Span::styled(" 取消 ", Style::default().fg(DIM).bg(PANEL))),
            cancel,
        );
        f.render_widget(
            Paragraph::new(Span::styled(
                " 確定 ",
                Style::default().fg(Color::Black).bg(ACCENT),
            )),
            ok,
        );
        app.hits.push((cancel, Hit::TaskCancel));
        app.hits.push((ok, Hit::TaskConfirm));
        return Some(caret);
    }

    let snap = app.task.snapshot();
    let mut y = body.y;
    f.render_widget(
        Paragraph::new(Span::styled(
            format!("狀態  {}", snap.phase.label()),
            Style::default().fg(ACCENT),
        )),
        Rect::new(body.x, y, body.width, 1),
    );
    y = y.saturating_add(1);
    f.render_widget(
        Paragraph::new(Span::styled(
            truncate_width(&format!("目標  {}", snap.goal), body.width),
            Style::default().fg(TEXT),
        )),
        Rect::new(body.x, y, body.width, 1),
    );
    y = y.saturating_add(2);
    let list_h = body.height.saturating_sub(5).max(3);
    f.render_widget(
        Paragraph::new(snap.checklist_text()).style(Style::default().fg(TEXT)),
        Rect::new(body.x, y, body.width, list_h),
    );
    let btn_y = body.y + body.height.saturating_sub(1);
    let close = Rect::new(body.x, btn_y, 8, 1);
    let end = Rect::new(body.x.saturating_add(10), btn_y, 12, 1);
    f.render_widget(
        Paragraph::new(Span::styled(" 關閉 ", Style::default().fg(DIM).bg(PANEL))),
        close,
    );
    f.render_widget(
        Paragraph::new(Span::styled(
            " 結束任務 ",
            Style::default().fg(WARN).bg(PANEL),
        )),
        end,
    );
    app.hits.push((close, Hit::TaskCancel));
    app.hits.push((end, Hit::TaskEnd));
    None
}

fn draw_workspace_pick(f: &mut Frame, app: &mut App) -> Option<Position> {
    let area = f.area();
    let w = area.width.saturating_sub(6).min(92).max(42);
    let h = area.height.saturating_sub(4).min(28).max(14);
    let x = area.x + area.width.saturating_sub(w) / 2;
    let y = area.y + area.height.saturating_sub(h) / 2;
    let r = Rect::new(x, y, w, h);
    f.render_widget(Clear, r);
    let block = Block::default()
        .title(" 選擇工作目錄 ")
        .title_style(Style::default().fg(TEXT).add_modifier(Modifier::BOLD))
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(ACCENT))
        .style(Style::default().bg(COMPOSER).fg(TEXT));
    f.render_widget(block, r);
    app.hits.push((r, Hit::WsPanel));

    let body = Rect::new(
        r.x.saturating_add(2),
        r.y.saturating_add(1),
        r.width.saturating_sub(4),
        r.height.saturating_sub(2),
    );
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Length(3),
            Constraint::Min(6),
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Length(1),
        ])
        .split(body);

    f.render_widget(
        Paragraph::new(Span::styled(
            "輸入路徑或檔名搜尋  ·  點資料夾進入  ·  點檔案選上層",
            Style::default().fg(DIM),
        )),
        rows[0],
    );

    let path_focus = app
        .workspace_pick
        .as_ref()
        .is_some_and(|p| p.focus == WsFocus::Path);
    let path_block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(if path_focus { ACCENT } else { BORDER }))
        .style(Style::default().bg(PANEL));
    f.render_widget(path_block, rows[1]);
    let path_inner = Rect::new(
        rows[1].x.saturating_add(1),
        rows[1].y.saturating_add(1),
        rows[1].width.saturating_sub(2),
        rows[1].height.saturating_sub(2).max(1),
    );
    app.hits.push((rows[1], Hit::WsPath));
    let mut caret = None;
    if let Some(p) = app.workspace_pick.as_mut() {
        p.path_inner = path_inner;
        caret = Some(draw_edit(f, path_inner, &p.edit, &mut p.path_scroll));
    }

    let list = rows[2];
    if let Some(p) = app.workspace_pick.as_mut() {
        p.list_area = list;
        let vis = list.height as usize;
        p.reveal(list.height);
        let start = p.scroll as usize;
        let end = (start + vis).min(p.view.entries.len());
        for (row_i, idx) in (start..end).enumerate() {
            let ent = &p.view.entries[idx];
            let y = list.y.saturating_add(row_i as u16);
            if y >= list.y.saturating_add(list.height) {
                break;
            }
            let cell = Rect::new(list.x, y, list.width, 1);
            let selected = idx == p.cursor;
            let icon = if ent.is_parent {
                "↑"
            } else if ent.is_dir {
                "📁"
            } else {
                "📄"
            };
            let label = format!(" {icon}  {} ", ent.name);
            let style = if selected {
                Style::default().fg(Color::Black).bg(ACCENT)
            } else if ent.is_dir {
                Style::default().fg(USER)
            } else {
                Style::default().fg(TEXT)
            };
            f.render_widget(
                Paragraph::new(Span::styled(truncate_width(&label, list.width), style)),
                cell,
            );
            app.hits.push((cell, Hit::WsEntry(idx as u16)));
        }
        if p.view.entries.is_empty() {
            f.render_widget(
                Paragraph::new(Span::styled("（沒有符合的項目）", Style::default().fg(DIM))),
                list,
            );
        }
    }

    let btns = rows[3];
    let confirm = " 選擇此資料夾 ";
    let create = " 建立資料夾 ";
    let cancel = " 取消 ";
    let cw = display_cols(confirm);
    let crw = display_cols(create);
    let clw = display_cols(cancel);
    let ok_r = Rect::new(btns.x, btns.y, cw, 1);
    let create_r = Rect::new(btns.x.saturating_add(cw + 1), btns.y, crw, 1);
    let cancel_r = Rect::new(btns.x.saturating_add(cw + crw + 2), btns.y, clw, 1);
    f.render_widget(
        Paragraph::new(Span::styled(
            confirm,
            Style::default()
                .fg(Color::Black)
                .bg(ACCENT)
                .add_modifier(Modifier::BOLD),
        )),
        ok_r,
    );
    f.render_widget(
        Paragraph::new(Span::styled(create, Style::default().fg(TEXT).bg(PANEL))),
        create_r,
    );
    f.render_widget(
        Paragraph::new(Span::styled(cancel, Style::default().fg(DIM).bg(PANEL))),
        cancel_r,
    );
    app.hits.push((ok_r, Hit::WsConfirm));
    app.hits.push((create_r, Hit::WsCreate));
    app.hits.push((cancel_r, Hit::WsCancel));

    let notice = app
        .workspace_pick
        .as_ref()
        .and_then(|p| p.notice.clone())
        .unwrap_or_else(|| "Enter 進入資料夾或確定  ·  Esc 取消".into());
    f.render_widget(
        Paragraph::new(Span::styled(notice, Style::default().fg(DIM))),
        rows[4],
    );
    if let Some(p) = app.workspace_pick.as_ref() {
        f.render_widget(
            Paragraph::new(Span::styled(
                folderpick::display_path(&p.view.cwd),
                Style::default().fg(DIM),
            )),
            rows[5],
        );
    }
    if app
        .workspace_pick
        .as_ref()
        .is_some_and(|p| p.focus == WsFocus::Path)
    {
        caret
    } else {
        None
    }
}

fn draw_kind_buttons(f: &mut Frame, app: &mut App, area: Rect, focused: bool) {
    let openai = app.conn.kind.is_openai();
    let grok = " Grok ";
    let custom = " 自訂 API ";
    let gw = display_cols(grok).max(4);
    let cw = display_cols(custom).max(4);
    let grok_cell = Rect::new(area.x, area.y, gw.min(area.width), 1);
    let custom_x = area.x.saturating_add(gw.saturating_add(1));
    let custom_cell = Rect::new(
        custom_x,
        area.y,
        cw.min(area.width.saturating_sub(gw.saturating_add(1))),
        1,
    );
    let grok_on = focused && app.setting_field == SettingField::Kind && !openai;
    let custom_on = focused && app.setting_field == SettingField::Kind && openai;
    f.render_widget(
        Paragraph::new(Span::styled(
            grok,
            if !openai {
                Style::default()
                    .bg(ACCENT)
                    .fg(Color::Black)
                    .add_modifier(Modifier::BOLD)
            } else if grok_on {
                Style::default().fg(ACCENT).bg(COMPOSER)
            } else {
                Style::default().fg(TEXT).bg(COMPOSER)
            },
        )),
        grok_cell,
    );
    f.render_widget(
        Paragraph::new(Span::styled(
            custom,
            if openai {
                Style::default()
                    .bg(ACCENT)
                    .fg(Color::Black)
                    .add_modifier(Modifier::BOLD)
            } else if custom_on {
                Style::default().fg(ACCENT).bg(COMPOSER)
            } else {
                Style::default().fg(TEXT).bg(COMPOSER)
            },
        )),
        custom_cell,
    );
    app.hits.push((grok_cell, Hit::ProviderXai));
    app.hits.push((custom_cell, Hit::ProviderOpenai));
}

fn draw_boxed_edit(
    f: &mut Frame,
    app: &mut App,
    area: Rect,
    edit: &Edit,
    focus: bool,
    hit: Hit,
    mask: bool,
) -> Position {
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(if focus { ACCENT } else { BORDER }))
        .style(Style::default().bg(COMPOSER));
    f.render_widget(block, area);
    app.hits.push((area, hit));
    let inner = Rect::new(
        area.x.saturating_add(1),
        area.y.saturating_add(1),
        area.width.saturating_sub(2),
        1,
    );
    if inner.width == 0 {
        return Position::new(area.x, area.y);
    }
    let shown = if mask && !edit.text.is_empty() {
        "•".repeat(edit.len().min(inner.width as usize))
    } else {
        edit.text.clone()
    };
    let shown: String = shown.chars().take(inner.width as usize).collect();
    f.render_widget(
        Paragraph::new(Span::styled(shown, Style::default().fg(TEXT).bg(COMPOSER))),
        inner,
    );
    let prefix: String = if mask {
        "•".repeat(edit.caret.min(inner.width as usize))
    } else {
        edit.text.chars().take(edit.caret).collect()
    };
    let w = Line::from(prefix.as_str()).width() as u16;
    Position::new(
        inner.x.saturating_add(w.min(inner.width.saturating_sub(1))),
        inner.y,
    )
}

fn draw_settings(f: &mut Frame, app: &mut App, opts: &TuiOptions) -> Option<Position> {
    let win = *app.settings.as_ref()?;
    if win.minimized {
        return None;
    }
    let r = win_rect(&win, f.area());
    f.render_widget(Clear, r);
    let focused = app.focus == Focus::Settings;
    let block = Block::default()
        .title(" 設定 ")
        .title_style(Style::default().fg(TEXT).add_modifier(Modifier::BOLD))
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(if focused { ACCENT } else { BORDER }))
        .style(Style::default().bg(PANEL).fg(TEXT));
    f.render_widget(block, r);

    let close = Rect::new(r.x + r.width.saturating_sub(4), r.y, 3, 1);
    let maxb = Rect::new(r.x + r.width.saturating_sub(7), r.y, 3, 1);
    let minb = Rect::new(r.x + r.width.saturating_sub(10), r.y, 3, 1);
    f.render_widget(Paragraph::new(Span::styled(" × ", Style::default().fg(WARN))), close);
    f.render_widget(Paragraph::new(Span::styled(" □ ", Style::default().fg(DIM))), maxb);
    f.render_widget(Paragraph::new(Span::styled(" ─ ", Style::default().fg(DIM))), minb);
    app.hits.push((Rect::new(r.x, r.y, r.width.saturating_sub(10), 1), Hit::Title));
    app.hits.push((minb, Hit::Min));
    app.hits.push((maxb, Hit::Max));
    app.hits.push((close, Hit::Close));

    let body = Rect::new(
        r.x.saturating_add(2),
        r.y.saturating_add(2),
        r.width.saturating_sub(4),
        r.height.saturating_sub(3),
    );
    let lines = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Length(3),
            Constraint::Length(1),
            Constraint::Length(3),
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Min(4),
        ])
        .split(body);

    f.render_widget(
        Paragraph::new(Span::styled("連線", Style::default().fg(DIM))),
        lines[0],
    );
    draw_kind_buttons(f, app, lines[1], focused);
    if app.conn.kind.is_openai() {
        return draw_openai_settings(f, app, body, &lines, focused, opts);
    }

    f.render_widget(
        Paragraph::new(Span::styled("帳號", Style::default().fg(DIM))),
        lines[2],
    );
    let account_focus = focused && app.setting_field == SettingField::Account;
    let (btn_label, extra) = match &app.login_ui {
        LoginUi::Starting => (
            " 連線中… ".to_string(),
            "正在向 xAI 取得登入代碼".to_string(),
        ),
        LoginUi::Waiting { .. } => (" 取消 ".to_string(), String::new()),
        LoginUi::Failed(e) => (
            if app.logged_in {
                " 登出 ".to_string()
            } else {
                " 登入 Grok ".to_string()
            },
            format!("失敗：{e}"),
        ),
        LoginUi::Idle if app.logged_in => (" 登出 ".to_string(), "已登入".to_string()),
        LoginUi::Idle => (" 登入 Grok ".to_string(), "未登入".to_string()),
    };
    let btn_style = if account_focus {
        Style::default()
            .bg(ACCENT)
            .fg(Color::Black)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(TEXT).bg(COMPOSER)
    };
    let btn_w = display_cols(&btn_label).max(4);
    let btn_cell = Rect::new(lines[3].x, lines[3].y, btn_w.min(lines[3].width), 1);
    f.render_widget(
        Paragraph::new(Span::styled(btn_label, btn_style)),
        btn_cell,
    );
    app.hits.push((btn_cell, Hit::AccountBtn));
    if let LoginUi::Waiting { user_code, .. } = &app.login_ui {
        let code_label = format!(" {user_code} ");
        let code_w = display_cols(&code_label).min(lines[4].width);
        let code_cell = Rect::new(lines[4].x, lines[4].y, code_w, 1);
        f.render_widget(
            Paragraph::new(Span::styled(
                code_label,
                Style::default()
                    .bg(COMPOSER)
                    .fg(ACCENT)
                    .add_modifier(Modifier::BOLD),
            )),
            code_cell,
        );
        app.hits.push((code_cell, Hit::LoginCode));
        let rest_x = lines[4].x.saturating_add(code_w.saturating_add(1));
        if rest_x < lines[4].x.saturating_add(lines[4].width) {
            f.render_widget(
                Paragraph::new(Span::styled(
                    "點此複製 · 已開瀏覽器",
                    Style::default().fg(DIM),
                )),
                Rect::new(
                    rest_x,
                    lines[4].y,
                    lines[4]
                        .x
                        .saturating_add(lines[4].width)
                        .saturating_sub(rest_x),
                    1,
                ),
            );
        }
    } else {
        f.render_widget(
            Paragraph::new(Span::styled(extra, Style::default().fg(DIM))),
            lines[4],
        );
    }

    let half = lines[6].width / 2;
    let model_rect = Rect::new(
        lines[6].x,
        lines[6].y,
        half.saturating_sub(1),
        lines[6].height,
    );
    let child_rect = Rect::new(
        lines[6].x.saturating_add(half),
        lines[6].y,
        lines[6].width.saturating_sub(half),
        lines[6].height,
    );
    f.render_widget(
        Paragraph::new(Span::styled("模型", Style::default().fg(DIM))),
        Rect::new(lines[5].x, lines[5].y, half.saturating_sub(1), 1),
    );
    f.render_widget(
        Paragraph::new(Span::styled("子代理模型", Style::default().fg(DIM))),
        Rect::new(
            lines[5].x.saturating_add(half),
            lines[5].y,
            lines[5].width.saturating_sub(half),
            1,
        ),
    );
    let model_focus = focused && app.setting_field == SettingField::Model;
    let model_label = app
        .catalog
        .find(&opts.model)
        .map(|m| m.name.clone())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| opts.model.clone());
    draw_combo(f, app, model_rect, &model_label, model_focus, Hit::SettingModel);
    let child_focus = focused && app.setting_field == SettingField::ChildModel;
    let child_label = child_model_label(app, opts);
    draw_combo(
        f,
        app,
        child_rect,
        &child_label,
        child_focus,
        Hit::SettingChildModel,
    );

    let efforts = effort_choices(app, opts);
    let effort_enabled = !efforts.is_empty();
    f.render_widget(
        Paragraph::new(Span::styled(
            if effort_enabled {
                "思考強度"
            } else {
                "思考強度（此模型不支援）"
            },
            Style::default().fg(DIM),
        )),
        lines[7],
    );
    let effort_focus = focused && app.setting_field == SettingField::Effort;
    let effort_label = efforts
        .iter()
        .find(|e| e.value == opts.reasoning_effort)
        .map(|e| e.label.clone())
        .unwrap_or_else(|| opts.reasoning_effort.label().to_string());
    if effort_enabled {
        draw_combo(
            f,
            app,
            lines[8],
            &effort_label,
            effort_focus,
            Hit::SettingEffort,
        );
    } else {
        let disabled = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(BORDER))
            .style(Style::default().bg(COMPOSER));
        f.render_widget(disabled, lines[8]);
        let inner = Rect::new(
            lines[8].x.saturating_add(1),
            lines[8].y.saturating_add(1),
            lines[8].width.saturating_sub(2),
            1,
        );
        f.render_widget(
            Paragraph::new(Span::styled("—", Style::default().fg(DIM))),
            inner,
        );
    }

    f.render_widget(
        Paragraph::new(Span::styled(
            "搜尋（web + X）",
            Style::default().fg(DIM),
        )),
        lines[9],
    );
    let search_on = opts.web_search;
    let search_focus = focused && app.setting_field == SettingField::Search;
    let search_label = if search_on { " 開 " } else { " 關 " };
    let search_style = if search_on {
        Style::default()
            .bg(ACCENT)
            .fg(Color::Black)
            .add_modifier(Modifier::BOLD)
    } else if search_focus {
        Style::default().fg(ACCENT).bg(COMPOSER)
    } else {
        Style::default().fg(DIM).bg(COMPOSER)
    };
    let search_cell = Rect::new(lines[10].x, lines[10].y, 5, 1);
    f.render_widget(
        Paragraph::new(Span::styled(search_label, search_style)),
        search_cell,
    );
    app.hits.push((search_cell, Hit::Search));

    let disp_x = lines[10].x.saturating_add(8);
    let disp_label = "調度員模式";
    let disp_label_w = display_cols(disp_label);
    if disp_x + disp_label_w + 6 <= lines[10].x + lines[10].width {
        let disp_focus = focused && app.setting_field == SettingField::Dispatcher;
        f.render_widget(
            Paragraph::new(Span::styled(
                disp_label,
                Style::default().fg(if disp_focus { ACCENT } else { DIM }),
            )),
            Rect::new(disp_x, lines[10].y, disp_label_w, 1),
        );
        let disp_cell = Rect::new(disp_x + disp_label_w + 1, lines[10].y, 5, 1);
        let disp_style = if opts.dispatcher {
            Style::default()
                .bg(ACCENT)
                .fg(Color::Black)
                .add_modifier(Modifier::BOLD)
        } else if disp_focus {
            Style::default().fg(ACCENT).bg(COMPOSER)
        } else {
            Style::default().fg(DIM).bg(COMPOSER)
        };
        f.render_widget(
            Paragraph::new(Span::styled(
                if opts.dispatcher { " 開 " } else { " 關 " },
                disp_style,
            )),
            disp_cell,
        );
        app.hits.push((disp_cell, Hit::Dispatcher));
    }

    app.refresh_skills();
    let import_claude = app
        .skills
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .prefs()
        .import_claude;
    let import_codex = app
        .skills
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .prefs()
        .import_codex;
    draw_import_row(
        f,
        app,
        lines[11],
        "引入 Claude Code 技能",
        import_claude,
        focused && app.setting_field == SettingField::ImportClaude,
        Hit::ImportClaude,
    );
    draw_import_row(
        f,
        app,
        lines[12],
        "引入 Codex 技能",
        import_codex,
        focused && app.setting_field == SettingField::ImportCodex,
        Hit::ImportCodex,
    );

    let skills_focus = focused && app.setting_field == SettingField::Skills;
    f.render_widget(
        Paragraph::new(Span::styled(
            "技能（點名稱查看 · 不可編輯）",
            Style::default().fg(if skills_focus { ACCENT } else { DIM }),
        )),
        lines[13],
    );
    draw_skill_list(f, app, lines[14], skills_focus);

    if let Some(kind) = app.drop {
        let anchor = match kind {
            DropKind::Model => model_rect,
            DropKind::ChildModel => child_rect,
            DropKind::Effort => lines[8],
        };
        draw_drop_list(f, app, opts, kind, anchor, f.area());
    }

    None
}

fn draw_openai_settings(
    f: &mut Frame,
    app: &mut App,
    body: Rect,
    lines: &[Rect],
    focused: bool,
    opts: &TuiOptions,
) -> Option<Position> {
    let rest = Rect::new(
        lines[2].x,
        lines[2].y,
        lines[2].width,
        body.y.saturating_add(body.height).saturating_sub(lines[2].y),
    );
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Length(3),
            Constraint::Length(1),
            Constraint::Length(3),
            Constraint::Length(1),
            Constraint::Length(3),
            Constraint::Length(1),
            Constraint::Length(3),
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Min(3),
        ])
        .split(rest);
    let mut caret = None;
    f.render_widget(
        Paragraph::new(Span::styled("端點", Style::default().fg(DIM))),
        rows[0],
    );
    let pos = draw_boxed_edit(
        f,
        app,
        rows[1],
        &app.endpoint_edit.clone(),
        focused && app.setting_field == SettingField::Endpoint,
        Hit::SettingEndpoint,
        false,
    );
    if app.setting_field == SettingField::Endpoint {
        caret = Some(pos);
    }
    let half = rows[3].width / 2;
    let model_rect = Rect::new(rows[3].x, rows[3].y, half.saturating_sub(1), rows[3].height);
    let child_rect = Rect::new(
        rows[3].x.saturating_add(half),
        rows[3].y,
        rows[3].width.saturating_sub(half),
        rows[3].height,
    );
    let has_list = !app.custom_catalog.models.is_empty();
    let model_title = if has_list {
        "模型"
    } else if app.custom_cat_err.is_some() {
        "模型名（端點無清單，手動填寫）"
    } else {
        "模型名"
    };
    f.render_widget(
        Paragraph::new(Span::styled(model_title, Style::default().fg(DIM))),
        Rect::new(rows[2].x, rows[2].y, half.saturating_sub(1), 1),
    );
    f.render_widget(
        Paragraph::new(Span::styled(
            if has_list { "子代理模型" } else { "子代理模型（留空＝同主模型）" },
            Style::default().fg(DIM),
        )),
        Rect::new(
            rows[2].x.saturating_add(half),
            rows[2].y,
            rows[2].width.saturating_sub(half),
            1,
        ),
    );
    if has_list {
        let model_label = app
            .catalog
            .find(&opts.model)
            .map(|m| m.name.clone())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| opts.model.clone());
        draw_combo(
            f,
            app,
            model_rect,
            &model_label,
            focused && app.setting_field == SettingField::Model,
            Hit::SettingModel,
        );
        let child_label = child_model_label(app, opts);
        draw_combo(
            f,
            app,
            child_rect,
            &child_label,
            focused && app.setting_field == SettingField::ChildModel,
            Hit::SettingChildModel,
        );
    } else {
        let pos = draw_boxed_edit(
            f,
            app,
            model_rect,
            &app.model_edit.clone(),
            focused && app.setting_field == SettingField::Model,
            Hit::SettingModel,
            false,
        );
        if app.setting_field == SettingField::Model {
            caret = Some(pos);
        }
        let pos = draw_boxed_edit(
            f,
            app,
            child_rect,
            &app.child_model_edit.clone(),
            focused && app.setting_field == SettingField::ChildModel,
            Hit::SettingChildModel,
            false,
        );
        if app.setting_field == SettingField::ChildModel {
            caret = Some(pos);
        }
    }
    f.render_widget(
        Paragraph::new(Span::styled("API 金鑰（可留空）", Style::default().fg(DIM))),
        rows[4],
    );
    let pos = draw_boxed_edit(
        f,
        app,
        rows[5],
        &app.api_key_edit.clone(),
        focused && app.setting_field == SettingField::ApiKey,
        Hit::SettingApiKey,
        true,
    );
    if app.setting_field == SettingField::ApiKey {
        caret = Some(pos);
    }
    f.render_widget(
        Paragraph::new(Span::styled(
            "上下文（token，例如 262K）",
            Style::default().fg(DIM),
        )),
        rows[6],
    );
    let pos = draw_boxed_edit(
        f,
        app,
        rows[7],
        &app.context_edit.clone(),
        focused && app.setting_field == SettingField::Context,
        Hit::SettingContext,
        false,
    );
    if app.setting_field == SettingField::Context {
        caret = Some(pos);
    }

    app.refresh_skills();
    let import_claude = app
        .skills
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .prefs()
        .import_claude;
    let import_codex = app
        .skills
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .prefs()
        .import_codex;
    draw_import_row(
        f,
        app,
        rows[8],
        "調度員模式（只規劃、指揮子代理）",
        opts.dispatcher,
        focused && app.setting_field == SettingField::Dispatcher,
        Hit::Dispatcher,
    );
    draw_import_row(
        f,
        app,
        rows[9],
        "引入 Claude Code 技能",
        import_claude,
        focused && app.setting_field == SettingField::ImportClaude,
        Hit::ImportClaude,
    );
    draw_import_row(
        f,
        app,
        rows[10],
        "引入 Codex 技能",
        import_codex,
        focused && app.setting_field == SettingField::ImportCodex,
        Hit::ImportCodex,
    );
    let skills_focus = focused && app.setting_field == SettingField::Skills;
    f.render_widget(
        Paragraph::new(Span::styled(
            "技能（點名稱查看 · 不可編輯）",
            Style::default().fg(if skills_focus { ACCENT } else { DIM }),
        )),
        rows[11],
    );
    draw_skill_list(f, app, rows[12], skills_focus);

    if let Some(kind) = app.drop {
        let anchor = match kind {
            DropKind::Model | DropKind::Effort => model_rect,
            DropKind::ChildModel => child_rect,
        };
        draw_drop_list(f, app, opts, kind, anchor, f.area());
    }
    caret
}

fn draw_import_row(
    f: &mut Frame,
    app: &mut App,
    area: Rect,
    label: &str,
    on: bool,
    focus: bool,
    hit: Hit,
) {
    let chip = if on { " 開 " } else { " 關 " };
    let chip_style = if on {
        Style::default()
            .bg(ACCENT)
            .fg(Color::Black)
            .add_modifier(Modifier::BOLD)
    } else if focus {
        Style::default().fg(ACCENT).bg(COMPOSER)
    } else {
        Style::default().fg(DIM).bg(COMPOSER)
    };
    let chip_w = 5u16;
    f.render_widget(
        Paragraph::new(Span::styled(label, Style::default().fg(if focus { TEXT } else { DIM }))),
        Rect::new(
            area.x,
            area.y,
            area.width.saturating_sub(chip_w.saturating_add(1)),
            1,
        ),
    );
    let cell = Rect::new(
        area.x + area.width.saturating_sub(chip_w),
        area.y,
        chip_w.min(area.width),
        1,
    );
    f.render_widget(Paragraph::new(Span::styled(chip, chip_style)), cell);
    app.hits.push((area, hit));
}

fn draw_skill_list(f: &mut Frame, app: &mut App, area: Rect, focus: bool) {
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(if focus { ACCENT } else { BORDER }))
        .style(Style::default().bg(COMPOSER));
    f.render_widget(block, area);
    app.hits.push((area, Hit::SkillList));
    let inner = Rect::new(
        area.x.saturating_add(1),
        area.y.saturating_add(1),
        area.width.saturating_sub(2),
        area.height.saturating_sub(2),
    );
    if inner.width == 0 || inner.height == 0 {
        return;
    }
    if app.skill_list.is_empty() {
        f.render_widget(
            Paragraph::new(Span::styled(
                "尚無技能 — 請模型寫一個，或開啟上方引入",
                Style::default().fg(DIM),
            )),
            inner,
        );
        return;
    }
    let vis = inner.height as usize;
    if app.skill_cursor < app.skill_scroll as usize {
        app.skill_scroll = app.skill_cursor as u16;
    }
    if app.skill_cursor >= app.skill_scroll as usize + vis && vis > 0 {
        app.skill_scroll = (app.skill_cursor + 1 - vis) as u16;
    }
    let start = app.skill_scroll as usize;
    let rows: Vec<(usize, String, String, bool)> = app
        .skill_list
        .iter()
        .enumerate()
        .skip(start)
        .take(vis)
        .map(|(i, s)| (i, s.name.clone(), s.origin.label().to_string(), s.enabled))
        .collect();
    for (row, (i, name, origin, enabled)) in rows.into_iter().enumerate() {
        let y = inner.y + row as u16;
        let selected = i == app.skill_cursor && focus;
        let chip = if enabled { " 開 " } else { " 關 " };
        let chip_style = if enabled {
            Style::default()
                .bg(ACCENT)
                .fg(Color::Black)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(DIM).bg(PANEL)
        };
        let chip_cell = Rect::new(inner.x, y, 5.min(inner.width), 1);
        f.render_widget(Paragraph::new(Span::styled(chip, chip_style)), chip_cell);
        app.hits.push((chip_cell, Hit::SkillToggle(i as u16)));
        let rest_x = inner.x.saturating_add(6);
        if rest_x >= inner.x + inner.width {
            continue;
        }
        let rest_w = inner.x + inner.width - rest_x;
        let label = format!("{name}  · {origin}");
        let style = if selected {
            Style::default()
                .fg(Color::Black)
                .bg(ACCENT)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(TEXT)
        };
        let shown: String = label.chars().take(rest_w as usize).collect();
        let rest = Rect::new(rest_x, y, rest_w, 1);
        f.render_widget(Paragraph::new(Span::styled(shown, style)), rest);
        app.hits.push((rest, Hit::SkillRow(i as u16)));
    }
}

fn draw_combo(f: &mut Frame, app: &mut App, area: Rect, label: &str, focus: bool, hit: Hit) {
    let open = match hit {
        Hit::SettingModel => app.drop == Some(DropKind::Model),
        Hit::SettingChildModel => app.drop == Some(DropKind::ChildModel),
        Hit::SettingEffort => app.drop == Some(DropKind::Effort),
        _ => false,
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(if focus || open { ACCENT } else { BORDER }))
        .style(Style::default().bg(COMPOSER));
    f.render_widget(block, area);
    app.hits.push((area, hit));
    let inner = Rect::new(
        area.x.saturating_add(1),
        area.y.saturating_add(1),
        area.width.saturating_sub(2),
        1,
    );
    if inner.width == 0 {
        return;
    }
    let arrow = if open { " ▴" } else { " ▾" };
    let arrow_w = 2u16;
    let text_w = inner.width.saturating_sub(arrow_w);
    let shown: String = label.chars().take(text_w as usize).collect();
    f.render_widget(
        Paragraph::new(Span::styled(shown, Style::default().fg(TEXT))),
        Rect::new(inner.x, inner.y, text_w, 1),
    );
    f.render_widget(
        Paragraph::new(Span::styled(arrow, Style::default().fg(DIM))),
        Rect::new(inner.x.saturating_add(text_w), inner.y, arrow_w, 1),
    );
}

fn draw_drop_list(
    f: &mut Frame,
    app: &mut App,
    opts: &TuiOptions,
    kind: DropKind,
    anchor: Rect,
    clip: Rect,
) {
    let labels: Vec<String> = match kind {
        DropKind::Model => model_choices(app, opts)
            .into_iter()
            .map(|(id, name)| if name.is_empty() { id } else { name })
            .collect(),
        DropKind::ChildModel => child_model_choices(app, opts)
            .into_iter()
            .map(|(id, name)| if name.is_empty() { id } else { name })
            .collect(),
        DropKind::Effort => effort_choices(app, opts)
            .into_iter()
            .map(|e| e.label)
            .collect(),
    };
    if labels.is_empty() {
        return;
    }
    let vis = DROP_VISIBLE.min(labels.len()).max(1) as u16;
    let height = vis.saturating_add(2);
    let mut y = anchor.y.saturating_add(anchor.height);
    if y.saturating_add(height) > clip.y.saturating_add(clip.height) {
        y = anchor.y.saturating_sub(height);
    }
    y = y.max(clip.y);
    let list = Rect {
        x: anchor.x,
        y,
        width: anchor.width,
        height: height.min(clip.height.saturating_sub(y.saturating_sub(clip.y))),
    };
    if list.width < 4 || list.height < 3 {
        return;
    }
    f.render_widget(Clear, list);
    f.render_widget(
        Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(ACCENT))
            .style(Style::default().bg(COMPOSER).fg(TEXT)),
        list,
    );
    let inner = Rect::new(
        list.x.saturating_add(1),
        list.y.saturating_add(1),
        list.width.saturating_sub(2),
        list.height.saturating_sub(2),
    );
    let start = app.drop_scroll as usize;
    let end = (start + inner.height as usize).min(labels.len());
    for (row, i) in (start..end).enumerate() {
        let cell = Rect::new(inner.x, inner.y.saturating_add(row as u16), inner.width, 1);
        let selected = i == app.drop_cursor;
        let style = if selected {
            Style::default()
                .bg(ACCENT)
                .fg(Color::Black)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(TEXT).bg(COMPOSER)
        };
        let text: String = labels[i].chars().take(inner.width as usize).collect();
        f.render_widget(Paragraph::new(Span::styled(format!(" {text}"), style)), cell);
        app.hits.push((cell, Hit::CatalogPick(i as u16)));
    }
}

