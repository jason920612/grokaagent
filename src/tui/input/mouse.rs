//! Mouse input: clicks resolve against the targets of the last frame.

use crossterm::event::{KeyModifiers, MouseButton, MouseEventKind};
use ratatui::layout::Position;

use super::{activate_ask_option, activate_ws_entry, confirm_workspace, copy_login_code, create_workspace_dir, submit_ask};
use crate::config::ProviderKind;
use crate::tui::app::{App, BottomTab, Focus, Hit, SendMode, SideItem, SideView, Tab, TaskUi};
use crate::tui::edit::{click_to_index, index_at_width};
use crate::tui::model::session::{ChatPos, ChatSel};
use crate::tui::settings::{DropKind, Field};
use crate::tui::ui::chat::{offset_for_thumb, thumb_h, thumb_rel};

const WHEEL: i32 = 3;

pub(crate) fn hit_at(app: &App, col: u16, row: u16) -> Option<Hit> {
    let p = Position::new(col, row);
    app.ui.hits.iter().rev().find(|(r, _)| r.contains(p)).map(|(_, h)| *h)
}

fn chat_pos(app: &App, col: u16, row: u16) -> Option<ChatPos> {
    let g = &app.ui.chat_glyphs;
    let line = g
        .iter()
        .find(|l| l.y == row)
        .or_else(|| g.iter().min_by_key(|l| l.y.abs_diff(row)))?;
    let rel = col.saturating_sub(line.x);
    let idx = if line.chars.is_empty() || rel >= line.text_w {
        line.start + line.chars.len()
    } else {
        line.start + index_at_width(&line.chars, rel)
    };
    Some(ChatPos { row: line.row, idx })
}

pub(crate) fn handle(app: &mut App, kind: MouseEventKind, col: u16, row: u16, mods: KeyModifiers) {
    let shift = mods.contains(KeyModifiers::SHIFT);
    match kind {
        MouseEventKind::Down(MouseButton::Left) => {
            app.ui.chat_dragging = false;
            app.ui.input_dragging = false;
            app.ui.skill_dragging = false;
            app.ui.scroll_grab = None;
            let hit = hit_at(app, col, row);
            if app.has_modal() {
                modal_click(app, hit, col, row, shift);
            } else {
                click(app, hit, col, row, shift);
            }
        }
        MouseEventKind::Drag(MouseButton::Left) => drag(app, col, row),
        MouseEventKind::Up(_) => {
            app.ui.chat_dragging = false;
            app.ui.input_dragging = false;
            app.ui.skill_dragging = false;
            app.ui.scroll_grab = None;
        }
        MouseEventKind::ScrollUp => wheel(app, col, row, -WHEEL),
        MouseEventKind::ScrollDown => wheel(app, col, row, WHEEL),
        _ => {}
    }
}

fn modal_click(app: &mut App, hit: Option<Hit>, col: u16, row: u16, shift: bool) {
    let inside = hit.is_some_and(|h| h != Hit::Chat && !matches!(h, Hit::SideScroll));
    match hit {
        Some(Hit::OverlayClose) => close_modal(app),
        Some(Hit::AskOption(i)) => activate_ask_option(app, i as usize, true),
        Some(Hit::AskConfirm) => submit_ask(app),
        Some(Hit::AskCancel) => app.cur_mut().cancel_ask(),
        Some(Hit::AskFill) => {
            let inner = app.ui.field_inner;
            if let Some(a) = app.cur_mut().ask.as_mut() {
                if !a.filling {
                    a.enter_fill();
                }
                let idx = click_to_index(&a.fill.text, inner, 0, col, row);
                a.fill.click(idx, shift);
            }
        }
        Some(Hit::TaskConfirm) => {
            if let Some(TaskUi::Form(edit)) = &app.ui.task_ui {
                let goal = edit.text.clone();
                app.submit_task_goal(&goal);
            }
        }
        Some(Hit::TaskCancel) => app.close_task(),
        Some(Hit::TaskEnd) => app.end_task(),
        Some(Hit::TaskDraft) => {
            let inner = app.ui.field_inner;
            if let Some(TaskUi::Form(edit)) = app.ui.task_ui.as_mut() {
                let idx = click_to_index(&edit.text, inner, 0, col, row);
                edit.click(idx, shift);
            }
        }
        Some(Hit::WsEntry(i)) => activate_ws_entry(app, i as usize),
        Some(Hit::WsPath) => {
            let inner = app.ui.field_inner;
            if let Some(p) = app.ui.workspace_pick.as_mut() {
                p.path_focus = true;
                let idx = click_to_index(&p.edit.text, inner, 0, col, row);
                p.edit.click(idx, shift);
            }
        }
        Some(Hit::WsConfirm) => confirm_workspace(app),
        Some(Hit::WsCreate) => create_workspace_dir(app),
        Some(Hit::WsCancel) => app.ui.workspace_pick = None,
        Some(Hit::SkillText) => {
            let inner = app.ui.skill_inner;
            if let Some(v) = app.ui.skill_view.as_mut() {
                let idx = click_to_index(&v.edit.text, inner, v.scroll, col, row);
                v.edit.click(idx, shift);
                app.ui.skill_dragging = true;
            }
        }
        _ if !inside => {
            // Viewers close on an outside click; forms stay open.
            if app.ui.image_view.is_some() || app.ui.skill_view.is_some() || app.ui.inspector.is_some() {
                close_modal(app);
            }
        }
        _ => {}
    }
}

fn close_modal(app: &mut App) {
    if app.ui.workspace_pick.is_some() {
        app.ui.workspace_pick = None;
    } else if app.cur().ask.is_some() {
        app.cur_mut().cancel_ask();
    } else if app.ui.task_ui.is_some() {
        app.close_task();
    } else if app.ui.image_view.is_some() {
        app.ui.image_view = None;
    } else if app.ui.skill_view.is_some() {
        app.ui.skill_view = None;
    } else {
        app.ui.inspector = None;
    }
}

fn click(app: &mut App, hit: Option<Hit>, col: u16, row: u16, shift: bool) {
    let Some(hit) = hit else { return };
    if !matches!(hit, Hit::Session(_) | Hit::RenameSession(_)) {
        app.commit_rename();
    }
    match hit {
        // —— Chat area (the active tab's transcript) ——
        Hit::ChatRow(_) => {
            let pos = chat_pos(app, col, row);
            let s = app.cur_mut();
            s.draft.clear_sel();
            let v = s.view_mut();
            v.open_tool = None;
            if let Some(pos) = pos {
                let anchor = match (&v.sel, shift) {
                    (ChatSel::Text { anchor, .. }, true) => *anchor,
                    _ => pos,
                };
                v.sel = ChatSel::Text { anchor, caret: pos };
                app.ui.chat_dragging = true;
            }
            focus_chat_area(app);
        }
        Hit::ChatImage(i) => {
            if let Some(p) = app.ui.image_hits.get(i as usize).cloned() {
                app.open_image(p);
            }
        }
        Hit::ToolGroup(i) => {
            let s = app.cur_mut();
            if let Some(crate::tui::model::rows::Row::Tools(g)) = s.view_transcript_mut().rows.get_mut(i) {
                g.expanded = !g.expanded;
                if !g.expanded {
                    let v = s.view_mut();
                    if v.open_tool.map(|(r, _)| r) == Some(i) {
                        v.open_tool = None;
                    }
                }
            }
            focus_chat_area(app);
        }
        Hit::Think(i) => {
            if let Some(crate::tui::model::rows::Row::Think(t)) = app.cur_mut().view_transcript_mut().rows.get_mut(i) {
                t.expanded = !t.expanded;
            }
            focus_chat_area(app);
        }
        Hit::ToolItem(r, c) => app.cur_mut().view_mut().open_tool = Some((r, c)),
        Hit::ToolPanel => {}
        Hit::ToolPanelClose => app.cur_mut().view_mut().open_tool = None,
        Hit::JumpBottom => app.cur_mut().view_mut().jump_bottom(),
        Hit::ScrollThumb => begin_scroll_drag(app, row, true),
        Hit::ScrollBar => begin_scroll_drag(app, row, false),
        Hit::Chat => {
            let s = app.cur_mut();
            let v = s.view_mut();
            v.sel = ChatSel::None;
            if v.open_tool.take().is_none() {
                s.view_transcript_mut().collapse_all();
            }
            focus_chat_area(app);
        }
        // —— Composer ——
        Hit::Composer => {
            app.ui.focus = Focus::Chat;
            let inner = app.ui.composer_inner;
            let vs = app.ui.composer_vscroll;
            let s = app.cur_mut();
            s.view_mut().sel = ChatSel::None;
            let idx = click_to_index(&s.draft.text, inner, vs, col, row);
            s.draft.click(idx, shift);
            app.ui.input_dragging = true;
        }
        Hit::QueueChip => app.ui.send_mode = SendMode::Queue,
        Hit::InsertChip => app.ui.send_mode = SendMode::Insert,
        Hit::PasteImage => app.paste_image(),
        Hit::StopChip => {
            app.interrupt();
        }
        Hit::QueueItem(i) => app.cur_mut().begin_queue_edit(i as usize),
        Hit::CancelQueueEdit => app.cur_mut().cancel_queue_edit(),
        Hit::PendingClose(i) => {
            let s = app.cur_mut();
            if (i as usize) < s.pending.len() {
                s.pending.remove(i as usize);
            }
        }
        // —— Workbench ——
        Hit::Activity(i) => {
            if let Some(view) = SideView::ALL.get(i as usize) {
                if app.ui.side_open && app.ui.side_view == *view {
                    app.ui.side_open = false;
                } else {
                    app.ui.side_open = true;
                    app.ui.side_view = *view;
                    app.ui.side_scroll = 0;
                }
            }
        }
        Hit::ActivitySettings | Hit::StatusModel => app.open_settings(),
        Hit::StatusTask => app.open_task(),
        Hit::StatusAgents => {
            app.ui.side_open = true;
            app.ui.side_view = SideView::Agents;
        }
        Hit::SideItem(i) => {
            if let Some(item) = app.ui.side_items.get(i as usize).cloned() {
                side_item(app, item);
            }
        }
        Hit::SideScroll | Hit::ReadOnlyBar => {}
        Hit::EditorTab(i) => {
            if let Some(tab) = app.tabs().get(i as usize).cloned() {
                app.activate(tab);
            }
        }
        Hit::EditorTabClose(i) => {
            if let Some(tab) = app.tabs().get(i as usize).cloned() {
                app.close_tab(&tab);
            }
        }
        Hit::BottomTab(i) => {
            app.ui.bottom = BottomTab::ALL.get(i as usize).copied();
            app.ui.bottom_scroll = 0;
        }
        Hit::BottomClose => app.ui.bottom = None,
        Hit::BottomRow(i) => bottom_row(app, i as usize),
        Hit::OutputPick(i) => {
            let name = app.cur().backgrounds.get(i as usize).map(|b| b.name.clone());
            app.cur_mut().output_pick = name;
        }
        // —— Sessions ——
        Hit::NewChat => app.begin_new_chat(),
        Hit::Session(i) => {
            if let Some(id) = app.listed.get(i as usize).map(|m| m.id.clone()) {
                if !app.ui.rename.as_ref().is_some_and(|(rid, _)| *rid == id) {
                    app.commit_rename();
                    app.switch_to(&id);
                    app.show_chat(None);
                    close_side_if_narrow(app);
                }
            }
        }
        Hit::RenameSession(i) => {
            if let Some(id) = app.listed.get(i as usize).map(|m| m.id.clone()) {
                app.begin_rename(&id);
            }
        }
        Hit::DeleteSession(i) => {
            if let Some(id) = app.listed.get(i as usize).map(|m| m.id.clone()) {
                app.delete_session(&id);
            }
        }
        // —— Settings tab ——
        Hit::SetKind(k) => {
            app.ui.focus = Focus::Settings;
            app.settings.field = Field::Kind;
            app.set_provider_kind(if k == 0 { ProviderKind::Xai } else { ProviderKind::Openai });
        }
        Hit::SetAccount => {
            app.ui.focus = Focus::Settings;
            app.settings.field = Field::Account;
            app.activate_account();
        }
        Hit::SetLoginCode => copy_login_code(app),
        Hit::SetField(i) => set_field(app, i as usize, col, row, shift),
        Hit::SetToggle(t) => {
            app.ui.focus = Focus::Settings;
            app.settings.drop = None;
            match t {
                0 => {
                    app.settings.field = Field::Search;
                    app.toggle_search();
                }
                1 => {
                    app.settings.field = Field::Dispatcher;
                    app.toggle_dispatcher();
                }
                2 => {
                    app.settings.field = Field::ImportClaude;
                    app.toggle_import(true);
                }
                _ => {
                    app.settings.field = Field::ImportCodex;
                    app.toggle_import(false);
                }
            }
        }
        Hit::DropPick(i) => app.pick_drop(i as usize),
        Hit::SkillToggle(i) => {
            app.settings.field = Field::Skills;
            app.settings.skill_cursor = i as usize;
            app.toggle_skill(i as usize);
        }
        Hit::SkillRow(i) => {
            app.settings.field = Field::Skills;
            app.settings.skill_cursor = i as usize;
            app.open_skill(i as usize);
        }
        _ => {}
    }
}

/// Clicking a transcript keeps the composer focused on the main chat.
fn focus_chat_area(app: &mut App) {
    if app.active_tab() != Tab::Settings {
        app.ui.focus = Focus::Chat;
    }
}

fn close_side_if_narrow(app: &mut App) {
    if app.ui.area.width > 0 && app.ui.area.width < crate::tui::theme::SIDEBAR_MIN_TERM {
        app.ui.side_open = false;
    }
}

fn side_item(app: &mut App, item: SideItem) {
    match item {
        SideItem::Root => app.show_chat(None),
        SideItem::Agent(p) => app.show_chat(Some(p)),
        SideItem::Change { view, row, call } => app.reveal_tool(&view, row, call),
        SideItem::Background(name) => {
            app.cur_mut().output_pick = Some(name);
            app.ui.bottom = Some(BottomTab::Output);
            app.ui.bottom_scroll = 0;
        }
        SideItem::Monitor(name) => app.ui.inspector = Some(name),
        SideItem::OpenTask => app.open_task(),
    }
    close_side_if_narrow(app);
}

fn bottom_row(app: &mut App, i: usize) {
    match app.ui.bottom {
        Some(BottomTab::Tools) => {
            let Some(t) = app.cur().agents.tool_log.get(i).cloned() else { return };
            let found = if t.path.is_empty() {
                app.cur().chat.find_call(&t.call_id)
            } else {
                app.cur().agents.get(&t.path).and_then(|a| a.transcript.find_call(&t.call_id))
            };
            match found {
                Some((r, c)) => app.reveal_tool(&t.path, r, c),
                None if !t.path.is_empty() => app.show_chat(Some(t.path)),
                None => {}
            }
        }
        Some(BottomTab::Events) => {
            let path = app.cur().agents.event_log.get(i).map(|e| e.path.clone());
            if let Some(p) = path.filter(|p| !p.is_empty()) {
                app.show_chat(Some(p));
            }
        }
        _ => {}
    }
}

fn set_field(app: &mut App, i: usize, col: u16, row: u16, shift: bool) {
    let Some(field) = crate::tui::ui::settings::FIELDS.get(i).copied() else { return };
    app.ui.focus = Focus::Settings;
    if app.settings.field != field {
        app.flush_conn();
        app.settings.field = field;
    }
    let dropdown = match field {
        Field::Model if app.settings.model_picker() => Some(DropKind::Model),
        Field::ChildModel if app.settings.model_picker() => Some(DropKind::ChildModel),
        Field::Effort => Some(DropKind::Effort),
        _ => None,
    };
    if let Some(kind) = dropdown {
        if app.settings.drop == Some(kind) {
            app.settings.drop = None;
        } else {
            app.open_drop(kind);
        }
        return;
    }
    app.settings.drop = None;
    let inner = app.ui.field_inner;
    if let Some(edit) = app.settings.edit_mut() {
        if inner.contains(Position::new(col, row)) {
            let idx = click_to_index(&edit.text, inner, 0, col, row).min(edit.len());
            edit.click(idx, shift);
        } else {
            edit.end(false);
        }
    }
}

fn begin_scroll_drag(app: &mut App, row: u16, on_thumb: bool) {
    let track = app.ui.chat_bar;
    if track.height == 0 {
        return;
    }
    let th = thumb_h(app.ui.chat_total, app.ui.chat_inner.height, track.height);
    let max_off = app.ui.chat_max_off;
    let v = app.cur_mut().view_mut();
    let scroll = if v.stick_bottom { 0 } else { v.scroll.min(max_off) };
    let thumb_y = track.y + thumb_rel(max_off, track.height, th, scroll);
    app.ui.scroll_grab = Some(if on_thumb { row as i16 - thumb_y as i16 } else { (th / 2) as i16 });
    scroll_to_row(app, row);
}

fn scroll_to_row(app: &mut App, row: u16) {
    let track = app.ui.chat_bar;
    if track.height == 0 {
        return;
    }
    let th = thumb_h(app.ui.chat_total, app.ui.chat_inner.height, track.height).max(1);
    let grab = app.ui.scroll_grab.unwrap_or(0);
    let last = (track.y + track.height).saturating_sub(th) as i16;
    let y = (row as i16 - grab).clamp(track.y as i16, last.max(track.y as i16)) as u16;
    let off = offset_for_thumb(app.ui.chat_max_off, track.height, th, y - track.y);
    let v = app.cur_mut().view_mut();
    v.scroll = off;
    v.stick_bottom = off == 0;
}

fn drag(app: &mut App, col: u16, row: u16) {
    if app.ui.scroll_grab.is_some() {
        scroll_to_row(app, row);
    } else if app.ui.chat_dragging {
        if let Some(pos) = chat_pos(app, col, row) {
            let v = app.cur_mut().view_mut();
            if let ChatSel::Text { anchor, .. } = v.sel {
                v.sel = ChatSel::Text { anchor, caret: pos };
            }
        }
    } else if app.ui.input_dragging {
        let inner = app.ui.composer_inner;
        let vs = app.ui.composer_vscroll;
        let d = &mut app.cur_mut().draft;
        let idx = click_to_index(&d.text, inner, vs, col, row);
        d.click(idx, true);
    } else if app.ui.skill_dragging {
        let inner = app.ui.skill_inner;
        if let Some(v) = app.ui.skill_view.as_mut() {
            let idx = click_to_index(&v.edit.text, inner, v.scroll, col, row);
            v.edit.click(idx, true);
        }
    }
}

fn wheel(app: &mut App, col: u16, row: u16, delta: i32) {
    let p = Position::new(col, row);
    if let Some(v) = app.ui.skill_view.as_mut() {
        v.scroll = if delta < 0 { v.scroll.saturating_sub(3) } else { v.scroll.saturating_add(3) };
        return;
    }
    if let Some(ws) = app.ui.workspace_pick.as_mut() {
        ws.move_cursor(delta.signum());
        return;
    }
    if app.has_modal() {
        return;
    }
    if app.settings.drop.is_some() && app.active_tab() == Tab::Settings {
        app.move_drop(delta.signum());
        return;
    }
    if app.ui.side_area.contains(p) {
        app.ui.side_scroll = if delta < 0 {
            app.ui.side_scroll.saturating_sub(delta.unsigned_abs() as usize)
        } else {
            app.ui.side_scroll + delta as usize
        };
        return;
    }
    if app.ui.bottom_area.contains(p) {
        // Up reveals older lines: grow the offset from the end.
        app.ui.bottom_scroll = if delta < 0 {
            app.ui.bottom_scroll + delta.unsigned_abs() as usize
        } else {
            app.ui.bottom_scroll.saturating_sub(delta as usize)
        };
        return;
    }
    if app.active_tab() == Tab::Settings {
        if app.settings.field == Field::Skills {
            let n = app.settings.skill_list.len();
            let c = app.settings.skill_cursor as i32 + delta.signum();
            app.settings.skill_cursor = c.clamp(0, n.saturating_sub(1) as i32) as usize;
        }
        return;
    }
    app.cur_mut().view_mut().scroll_by(-delta);
}
