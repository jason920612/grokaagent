// Keyboard and mouse handling for the workbench frame (activity bar, side
// views, editor tabs, bottom panel). Chat-area clicks run against whichever
// transcript the active tab shows.

fn is_chat_hit(hit: Hit) -> bool {
    matches!(
        hit,
        Hit::ChatRow(_)
            | Hit::ChatImage(_)
            | Hit::ToolGroup(_)
            | Hit::Think(_)
            | Hit::ToolItem(_, _)
            | Hit::ToolPanel
            | Hit::ToolPanelClose
            | Hit::JumpBottom
            | Hit::ScrollThumb
            | Hit::ScrollBar
            | Hit::DismissTool
            | Hit::Chat
    )
}

/// A click inside the chat area. Runs with the active transcript swapped in.
fn handle_chat_click(app: &mut App, hit: Hit, col: u16, row: u16, shift: bool) {
    app.focus = Focus::Chat;
    match hit {
        Hit::ChatRow(_) => {
            let _ = app.dismiss_tool_ui();
            if let Some(pos) = chat_pos_at(&app.chat_glyphs, col, row) {
                app.edit.clear_sel();
                let anchor = if shift {
                    match app.chat_sel {
                        ChatSel::Text { anchor, .. } => anchor,
                        _ => pos,
                    }
                } else {
                    pos
                };
                app.chat_sel = ChatSel::Text { anchor, caret: pos };
                app.chat_dragging = true;
            }
        }
        Hit::ChatImage(i) => {
            if let Some(path) = app.image_hits.get(i as usize).cloned() {
                app.open_image_view(path);
            }
        }
        Hit::ToolGroup(i) => {
            if let Some(Row::Tools(g)) = app.rows.get_mut(i) {
                g.expanded = !g.expanded;
                if !g.expanded && app.open_tool.map(|(r, _)| r) == Some(i) {
                    app.open_tool = None;
                }
            }
        }
        Hit::Think(i) => {
            if let Some(Row::Think(t)) = app.rows.get_mut(i) {
                t.expanded = !t.expanded;
            }
        }
        Hit::ToolItem(r, c) => app.open_tool = Some((r, c)),
        Hit::ToolPanel => {}
        Hit::ToolPanelClose => app.open_tool = None,
        Hit::JumpBottom => jump_chat_bottom(app),
        Hit::ScrollThumb => begin_scroll_drag(app, row, true),
        Hit::ScrollBar => begin_scroll_drag(app, row, false),
        Hit::DismissTool | Hit::Chat => {
            app.chat_sel = ChatSel::None;
            let _ = app.dismiss_tool_ui();
        }
        _ => {}
    }
}

fn toggle_side_view(app: &mut App, view: SideView) {
    if app.side_open && app.side_view == view {
        app.side_open = false;
    } else {
        app.side_open = true;
        app.side_view = view;
        app.side_scroll = 0;
    }
}

fn toggle_bottom(app: &mut App) {
    app.bottom = match app.bottom {
        Some(_) => None,
        None => Some(BottomTab::Tools),
    };
    app.bottom_scroll = 0;
}

/// Narrow terminals float the side bar over the editor; close it after a pick.
fn close_side_overlay(app: &mut App) {
    if app.area.width > 0 && app.area.width < SIDEBAR_MIN_TERM {
        app.side_open = false;
    }
}

/// Clicks on workbench chrome. `true` = handled.
fn handle_workbench_click(app: &mut App, hit: Hit) -> bool {
    match hit {
        Hit::Activity(u8::MAX) => open_settings(app),
        Hit::Activity(i) => {
            if let Some(view) = SideView::ALL.get(i as usize) {
                toggle_side_view(app, *view);
            }
        }
        Hit::SideRoot => {
            app.bench.active = None;
            app.focus = Focus::Chat;
            close_side_overlay(app);
        }
        Hit::SideAgent(i) => {
            if let Some(path) = app.bench.agents.get(i as usize).map(|a| a.path.clone()) {
                app.open_agent_tab(&path);
                close_side_overlay(app);
            }
        }
        Hit::SideChange(i) => {
            let changes = app.all_file_changes();
            if let Some((path, row, call, _, _)) = changes.get(i as usize).cloned() {
                app.reveal_tool(&path, row, call);
                close_side_overlay(app);
            }
        }
        Hit::SideScroll | Hit::ReadOnlyBar => {}
        Hit::EditorTab(i) => {
            app.activate_tab(i as usize);
            app.focus = Focus::Chat;
        }
        Hit::EditorTabClose(i) => {
            if let Some(path) = app.bench.open.get((i as usize).saturating_sub(1)).cloned() {
                app.close_agent_tab(&path);
            }
        }
        Hit::BottomTab(i) => {
            if let Some(tab) = BottomTab::ALL.get(i as usize) {
                app.bottom = Some(*tab);
                app.bottom_scroll = 0;
            }
        }
        Hit::BottomClose => app.bottom = None,
        Hit::BottomRow(idx) => match app.bottom {
            Some(BottomTab::Tools) => {
                let entry = app
                    .bench
                    .tool_log
                    .get(idx as usize)
                    .map(|t| (t.path.clone(), t.call_id.clone()));
                if let Some((path, call_id)) = entry {
                    if let Some((row, call)) = find_call(app, &path, &call_id) {
                        app.reveal_tool(&path, row, call);
                    } else if !path.is_empty() {
                        app.open_agent_tab(&path);
                    }
                }
            }
            Some(BottomTab::Events) => {
                let path = app.bench.event_log.get(idx as usize).map(|e| e.path.clone());
                if let Some(path) = path.filter(|p| !p.is_empty()) {
                    app.open_agent_tab(&path);
                }
            }
            _ => {}
        },
        _ => return false,
    }
    true
}

/// Workbench shortcuts. `true` = consumed.
fn handle_workbench_key(app: &mut App, code: KeyCode, mods: KeyModifiers) -> bool {
    let ctrl = mods.contains(KeyModifiers::CONTROL);
    let alt = mods.contains(KeyModifiers::ALT);
    match code {
        KeyCode::F(3) => {
            app.side_open = !app.side_open;
            return true;
        }
        KeyCode::Char('b' | 'B') if ctrl => {
            app.side_open = !app.side_open;
            return true;
        }
        KeyCode::F(4) => {
            toggle_bottom(app);
            return true;
        }
        KeyCode::Char('j' | 'J') if ctrl => {
            toggle_bottom(app);
            return true;
        }
        KeyCode::Char('w' | 'W') if ctrl => {
            if let Some(path) = app.bench.active.clone() {
                app.close_agent_tab(&path);
            }
            return true;
        }
        KeyCode::Char('0') if alt => {
            app.bench.active = None;
            return true;
        }
        KeyCode::Char(c @ '1'..='5') if alt => {
            let i = c as usize - '1' as usize;
            toggle_side_view(app, SideView::ALL[i]);
            return true;
        }
        KeyCode::Left if alt => {
            app.cycle_tab(-1);
            return true;
        }
        KeyCode::Right if alt => {
            app.cycle_tab(1);
            return true;
        }
        KeyCode::PageUp if ctrl => {
            app.cycle_tab(-1);
            return true;
        }
        KeyCode::PageDown if ctrl => {
            app.cycle_tab(1);
            return true;
        }
        KeyCode::Esc if app.side_open && app.area.width > 0 && app.area.width < SIDEBAR_MIN_TERM => {
            app.side_open = false;
            return true;
        }
        _ => {}
    }
    if !app.viewing_agent() || app.focus != Focus::Chat {
        return false;
    }
    // A child tab is read-only: navigation keys scroll it, Esc returns to the
    // main chat, and typing switches back to the main chat's composer.
    match code {
        KeyCode::Esc => {
            let busy = app.with_active_view(|app| {
                app.open_tool.take().is_some() || std::mem::take(&mut app.chat_sel) != ChatSel::None
            });
            if !busy {
                app.bench.active = None;
            }
            true
        }
        KeyCode::Up => {
            app.with_active_view(|app| scroll_chat(app, 1));
            true
        }
        KeyCode::Down => {
            app.with_active_view(|app| scroll_chat(app, -1));
            true
        }
        KeyCode::Home => {
            app.with_active_view(|app| scroll_chat(app, i32::from(u16::MAX)));
            true
        }
        KeyCode::End => {
            app.with_active_view(jump_chat_bottom);
            true
        }
        KeyCode::Char(_) | KeyCode::Enter | KeyCode::Backspace | KeyCode::Delete | KeyCode::Tab
            if !ctrl =>
        {
            app.bench.active = None;
            false
        }
        _ => false,
    }
}

/// Mouse wheel over the side bar or bottom panel. `true` = handled.
fn wheel_workbench(app: &mut App, col: u16, row: u16, delta: i32) -> bool {
    let modal = app.image_view.is_some()
        || app.inspector.is_some()
        || app.skill_view.is_some()
        || app.workspace_pick.is_some()
        || app.ask.is_some()
        || app.task_ui.is_some();
    if modal {
        return false;
    }
    let p = Position::new(col, row);
    if app.side_area.contains(p) {
        app.side_scroll = if delta < 0 {
            app.side_scroll.saturating_sub(delta.unsigned_abs() as usize)
        } else {
            app.side_scroll.saturating_add(delta as usize)
        };
        return true;
    }
    if app.bottom_area.contains(p) {
        // Up (negative) reveals older lines: grow the offset from the end.
        app.bottom_scroll = if delta < 0 {
            app.bottom_scroll.saturating_add(delta.unsigned_abs() as usize)
        } else {
            app.bottom_scroll.saturating_sub(delta as usize)
        };
        return true;
    }
    false
}
