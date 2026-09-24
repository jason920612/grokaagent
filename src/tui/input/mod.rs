//! Keyboard input. Keys go to the innermost thing that is open: a modal
//! overlay, the rename field, the settings tab, an agent tab, or the chat.

mod mouse;

use crossterm::event::{Event, KeyCode, KeyEventKind, KeyModifiers};

use crate::task;
use crate::tui::app::{App, BottomTab, Focus, SideView, Tab, TaskUi};
use crate::tui::edit::{clipboard_set, is_paste_key};
use crate::tui::settings::{DropKind, Field};
use crate::config::ProviderKind;

/// Handle one terminal event. Returns `true` to quit.
pub(crate) fn handle(app: &mut App, ev: Event) -> bool {
    match ev {
        Event::Key(k) if k.kind == KeyEventKind::Press => key(app, k.code, k.modifiers),
        Event::Mouse(m) => {
            mouse::handle(app, m.kind, m.column, m.row, m.modifiers);
            false
        }
        Event::Paste(s) => {
            paste(app, &s);
            false
        }
        _ => false,
    }
}

fn is_enter(ev: &Event) -> bool {
    matches!(ev, Event::Key(k) if k.kind == KeyEventKind::Press
        && k.code == KeyCode::Enter
        && !k.modifiers.contains(KeyModifiers::CONTROL))
}

fn is_ime_commit(ev: &Event) -> bool {
    matches!(ev, Event::Key(k) if k.kind == KeyEventKind::Press
        && matches!(k.code, KeyCode::Char(c) if !c.is_ascii())
        && !k.modifiers.contains(KeyModifiers::CONTROL))
}

/// The IME's first Enter commits CJK text in the same burst as the
/// characters. That Enter confirms the composition; it must not send.
pub(crate) fn coalesce_ime_enter(events: Vec<Event>) -> Vec<Event> {
    if events.iter().any(is_ime_commit) && events.iter().any(is_enter) {
        events.into_iter().filter(|e| !is_enter(e)).collect()
    } else {
        events
    }
}

fn paste(app: &mut App, s: &str) {
    let one_line = || s.replace(['\n', '\r'], "");
    if let Some(p) = app.ui.workspace_pick.as_mut() {
        p.edit.insert_str(&one_line());
        p.path_focus = true;
        p.sync();
    } else if let Some(a) = app.cur_mut().ask.as_mut().filter(|a| a.filling) {
        let remain = crate::ask::MAX_INPUT.saturating_sub(a.fill.len());
        a.fill.insert_str(&s.chars().take(remain).collect::<String>());
    } else if let Some(TaskUi::Form(edit)) = app.ui.task_ui.as_mut() {
        let remain = task::MAX_GOAL.saturating_sub(edit.len());
        edit.insert_str(&s.chars().take(remain).collect::<String>());
    } else if let Some((_, edit)) = app.ui.rename.as_mut() {
        edit.insert_str(&one_line());
    } else if app.ui.skill_view.is_some() {
    } else if app.active_tab() == Tab::Settings {
        if let Some(e) = app.settings.edit_mut() {
            e.insert_str(&one_line());
        }
    } else {
        app.show_chat(None);
        app.paste_from_terminal(s);
    }
}

fn key(app: &mut App, code: KeyCode, mods: KeyModifiers) -> bool {
    let ctrl = mods.contains(KeyModifiers::CONTROL);
    if ctrl && matches!(code, KeyCode::Char('q' | 'Q')) {
        app.cur_mut().cancel_ask();
        return true;
    }
    if ctrl && matches!(code, KeyCode::Char('c' | 'C')) {
        if app.copy_selection() {
            return false;
        }
        app.cur_mut().cancel_ask();
        return true;
    }
    if app.ui.workspace_pick.is_some() {
        workspace_key(app, code, mods);
        return false;
    }
    if ctrl && matches!(code, KeyCode::Char('n' | 'N')) {
        app.begin_new_chat();
        return false;
    }
    if app.cur().ask.is_some() {
        ask_key(app, code, mods);
        return false;
    }
    if app.ui.task_ui.is_some() {
        task_key(app, code, mods);
        return false;
    }
    if app.ui.focus == Focus::Rename {
        rename_key(app, code, mods);
        return false;
    }
    if app.ui.image_view.is_some() {
        if code == KeyCode::Esc {
            app.ui.image_view = None;
        }
        return false;
    }
    if app.ui.skill_view.is_some() {
        skill_key(app, code, mods);
        return false;
    }
    if app.ui.inspector.is_some() {
        if code == KeyCode::Esc {
            app.ui.inspector = None;
        }
        return false;
    }
    if code == KeyCode::F(2) || (ctrl && matches!(code, KeyCode::Char('g' | 'G'))) {
        if app.active_tab() == Tab::Settings {
            app.close_settings();
        } else {
            app.open_settings();
        }
        return false;
    }
    if workbench_key(app, code, mods) {
        return false;
    }
    match app.active_tab() {
        Tab::Settings => settings_key(app, code, mods),
        Tab::Agent(_) => {
            if agent_tab_key(app, code, mods) {
                return false;
            }
            chat_key(app, code, mods);
        }
        Tab::Chat => chat_key(app, code, mods),
    }
    false
}

fn toggle_side(app: &mut App, view: SideView) {
    if app.ui.side_open && app.ui.side_view == view {
        app.ui.side_open = false;
    } else {
        app.ui.side_open = true;
        app.ui.side_view = view;
        app.ui.side_scroll = 0;
    }
}

fn toggle_bottom(app: &mut App) {
    app.ui.bottom = match app.ui.bottom {
        Some(_) => None,
        None => Some(BottomTab::Tools),
    };
    app.ui.bottom_scroll = 0;
}

fn narrow(app: &App) -> bool {
    app.ui.area.width > 0 && app.ui.area.width < crate::tui::theme::SIDEBAR_MIN_TERM
}

/// Workbench shortcuts. `true` = consumed.
fn workbench_key(app: &mut App, code: KeyCode, mods: KeyModifiers) -> bool {
    let ctrl = mods.contains(KeyModifiers::CONTROL);
    let alt = mods.contains(KeyModifiers::ALT);
    match code {
        KeyCode::F(3) => app.ui.side_open = !app.ui.side_open,
        KeyCode::Char('b' | 'B') if ctrl => app.ui.side_open = !app.ui.side_open,
        KeyCode::F(4) => toggle_bottom(app),
        KeyCode::Char('j' | 'J') if ctrl => toggle_bottom(app),
        KeyCode::Char('w' | 'W') if ctrl => {
            let tab = app.active_tab();
            app.close_tab(&tab);
        }
        KeyCode::Char('0') if alt => app.show_chat(None),
        KeyCode::Char(c @ '1'..='5') if alt => toggle_side(app, SideView::ALL[c as usize - '1' as usize]),
        KeyCode::Left if alt => app.cycle_tab(-1),
        KeyCode::Right if alt => app.cycle_tab(1),
        KeyCode::PageUp if ctrl => app.cycle_tab(-1),
        KeyCode::PageDown if ctrl => app.cycle_tab(1),
        KeyCode::Esc if app.ui.side_open && narrow(app) => app.ui.side_open = false,
        _ => return false,
    }
    true
}

/// Agent tabs are read-only: navigation scrolls, Esc and typing go back to
/// the main chat. `true` = consumed.
fn agent_tab_key(app: &mut App, code: KeyCode, mods: KeyModifiers) -> bool {
    let ctrl = mods.contains(KeyModifiers::CONTROL);
    let s = app.cur_mut();
    match code {
        KeyCode::Esc => {
            let v = s.view_mut();
            if v.open_tool.take().is_none() && std::mem::take(&mut v.sel) == Default::default() {
                app.show_chat(None);
            }
        }
        KeyCode::Up => s.view_mut().scroll_by(1),
        KeyCode::Down => s.view_mut().scroll_by(-1),
        KeyCode::PageUp => {
            let page = page_step(app);
            app.cur_mut().view_mut().scroll_by(page);
        }
        KeyCode::PageDown => {
            let page = page_step(app);
            app.cur_mut().view_mut().scroll_by(-page);
        }
        KeyCode::Home => s.view_mut().scroll_by(i32::from(u16::MAX)),
        KeyCode::End => s.view_mut().jump_bottom(),
        KeyCode::Char(_) | KeyCode::Enter | KeyCode::Backspace | KeyCode::Delete | KeyCode::Tab if !ctrl => {
            app.show_chat(None);
            return false;
        }
        _ if is_paste_key(code, mods) => {
            app.show_chat(None);
            return false;
        }
        _ => {}
    }
    true
}

fn page_step(app: &App) -> i32 {
    i32::from(app.ui.chat_inner.height.saturating_sub(1).max(8))
}

fn chat_key(app: &mut App, code: KeyCode, mods: KeyModifiers) {
    let shift = mods.contains(KeyModifiers::SHIFT);
    let ctrl = mods.contains(KeyModifiers::CONTROL);
    app.ui.focus = Focus::Chat;
    if is_paste_key(code, mods) {
        app.paste_clipboard();
        return;
    }
    let width = app.ui.composer_inner.width.max(1);
    match code {
        KeyCode::Esc => esc_chain(app),
        KeyCode::Char('a' | 'A') if ctrl => app.cur_mut().draft.select_all(),
        KeyCode::Up if app.cur().draft.is_empty() && !shift => app.cur_mut().view_mut().scroll_by(1),
        KeyCode::Down if app.cur().draft.is_empty() && !shift => app.cur_mut().view_mut().scroll_by(-1),
        KeyCode::Up => app.cur_mut().draft.move_visual(width, -1, shift),
        KeyCode::Down => app.cur_mut().draft.move_visual(width, 1, shift),
        KeyCode::PageUp => {
            let page = page_step(app);
            app.cur_mut().view_mut().scroll_by(page);
        }
        KeyCode::PageDown => {
            let page = page_step(app);
            app.cur_mut().view_mut().scroll_by(-page);
        }
        KeyCode::Enter if ctrl => {
            if app.cur().queue_edit.is_some() {
                app.cur_mut().commit_queue_edit();
            } else {
                app.submit_current(true);
            }
        }
        KeyCode::Enter if shift => app.cur_mut().draft.insert_char('\n'),
        KeyCode::Enter => {
            // The first Enter while an IME composes confirms it; it is no send.
            if crate::hostio::ime_composing() {
            } else if app.cur().queue_edit.is_some() {
                app.cur_mut().commit_queue_edit();
            } else {
                app.submit_current(false);
            }
        }
        KeyCode::Backspace if app.cur().draft.is_empty() && !app.cur().pending.is_empty() => {
            app.cur_mut().pending.pop();
        }
        _ => {
            app.cur_mut().draft.apply_key(code, mods, 100_000);
        }
    }
}

/// Esc in the chat closes the innermost thing, then stops work.
fn esc_chain(app: &mut App) {
    let s = app.cur_mut();
    if s.view_mut().open_tool.take().is_some() {
        return;
    }
    if s.queue_edit.is_some() {
        s.cancel_queue_edit();
        return;
    }
    if s.chat.running {
        s.interrupt();
        return;
    }
    if s.view_transcript_mut().collapse_all() {
        return;
    }
    if s.draft.has_sel() {
        s.draft.clear_sel();
        return;
    }
    let v = s.view_mut();
    if v.sel != Default::default() {
        v.sel = Default::default();
        return;
    }
    s.interrupt();
}

fn workspace_key(app: &mut App, code: KeyCode, mods: KeyModifiers) {
    let Some(p) = app.ui.workspace_pick.as_mut() else { return };
    match code {
        KeyCode::Esc => app.ui.workspace_pick = None,
        KeyCode::Tab => p.path_focus = !p.path_focus,
        KeyCode::Up => p.move_cursor(-1),
        KeyCode::Down => p.move_cursor(1),
        KeyCode::PageUp => p.move_cursor(-8),
        KeyCode::PageDown => p.move_cursor(8),
        KeyCode::Enter => workspace_enter(app),
        _ => {
            let before = p.edit.text.clone();
            if p.edit.apply_key(code, mods, 1000) {
                p.path_focus = true;
                if p.edit.text != before {
                    p.sync();
                }
            }
        }
    }
}

fn workspace_enter(app: &mut App) {
    let Some(p) = app.ui.workspace_pick.as_mut() else { return };
    if p.path_focus {
        let typed = std::path::PathBuf::from(p.edit.text.trim());
        if typed.is_dir() {
            if crate::folderpick::normalize(&typed) == p.view.cwd {
                confirm_workspace(app);
            } else {
                p.enter_dir(typed);
            }
            return;
        }
        let dirs: Vec<_> = p.view.entries.iter().filter(|e| e.is_dir && !e.is_parent).cloned().collect();
        if dirs.len() == 1 {
            p.enter_dir(dirs[0].path.clone());
            return;
        }
        confirm_workspace(app);
        return;
    }
    match p.view.entries.get(p.cursor).cloned() {
        Some(e) if e.is_dir => p.enter_dir(e.path),
        _ => confirm_workspace(app),
    }
}

pub(crate) fn confirm_workspace(app: &mut App) {
    let Some(p) = app.ui.workspace_pick.as_mut() else { return };
    let dir = p.target();
    if !dir.is_dir() {
        p.notice = Some("請選擇存在的資料夾".into());
        return;
    }
    app.ui.workspace_pick = None;
    app.create_chat(dir);
}

pub(crate) fn create_workspace_dir(app: &mut App) {
    let Some(p) = app.ui.workspace_pick.as_mut() else { return };
    let target = crate::folderpick::create_target(&p.edit.text, &p.view.cwd);
    match crate::folderpick::mkdir(&target) {
        Ok(dir) => {
            p.enter_dir(dir);
            p.notice = Some("已建立資料夾".into());
        }
        Err(e) => p.notice = Some(format!("無法建立: {e}")),
    }
}

pub(crate) fn activate_ws_entry(app: &mut App, idx: usize) {
    let Some(p) = app.ui.workspace_pick.as_mut() else { return };
    let Some(e) = p.view.entries.get(idx).cloned() else { return };
    p.cursor = idx;
    if e.is_dir {
        p.enter_dir(e.path);
    } else {
        p.edit = crate::tui::edit::Edit::at_end(crate::folderpick::display_path(&e.path));
        p.notice = Some("檔案：確定時會使用上層資料夾".into());
        p.path_focus = false;
    }
}

fn ask_key(app: &mut App, code: KeyCode, mods: KeyModifiers) {
    let s = app.cur_mut();
    let Some(ask) = s.ask.as_mut() else { return };
    if ask.filling {
        match code {
            KeyCode::Esc => ask.save_fill(),
            KeyCode::Enter => submit_ask(app),
            KeyCode::Up => ask.move_cursor(-1),
            KeyCode::Down => ask.move_cursor(1),
            _ => {
                ask.fill.apply_key(code, mods, crate::ask::MAX_INPUT);
            }
        }
        return;
    }
    match code {
        KeyCode::Esc => s.cancel_ask(),
        KeyCode::Up => ask.move_cursor(-1),
        KeyCode::Down => ask.move_cursor(1),
        KeyCode::Char(' ') => {
            if ask.question.options.get(ask.cursor).is_some_and(|o| o.input) {
                ask.enter_fill();
            } else {
                ask.toggle_cursor();
            }
        }
        KeyCode::Enter => {
            let i = ask.cursor;
            if ask.question.options.get(i).is_some_and(|o| o.input) {
                activate_ask_option(app, i, false);
            } else if ask.question.allow_multiple {
                submit_ask(app);
            } else {
                activate_ask_option(app, i, true);
            }
        }
        _ => {}
    }
}

/// Pick option `i`: an input option opens its field, others toggle (and
/// submit a single-choice questionnaire when `submit`).
pub(crate) fn activate_ask_option(app: &mut App, i: usize, submit: bool) {
    let Some(ask) = app.cur_mut().ask.as_mut() else { return };
    if i >= ask.n() {
        return;
    }
    ask.save_fill();
    ask.cursor = i;
    if ask.question.options[i].input {
        ask.enter_fill();
        return;
    }
    if ask.question.allow_multiple {
        ask.toggle_cursor();
    } else {
        ask.toggle_cursor();
        if submit {
            submit_ask(app);
        }
    }
}

pub(crate) fn submit_ask(app: &mut App) {
    if let Err(msg) = app.cur_mut().submit_ask() {
        app.flash(msg);
    }
}

fn task_key(app: &mut App, code: KeyCode, mods: KeyModifiers) {
    let shift = mods.contains(KeyModifiers::SHIFT);
    let ctrl = mods.contains(KeyModifiers::CONTROL);
    match app.ui.task_ui.as_mut() {
        Some(TaskUi::Form(edit)) => match code {
            KeyCode::Esc => app.close_task(),
            KeyCode::Enter if !shift => {
                let goal = edit.text.clone();
                app.submit_task_goal(&goal);
            }
            KeyCode::Char('s') if ctrl => {
                let goal = edit.text.clone();
                app.submit_task_goal(&goal);
            }
            KeyCode::Enter => {
                if edit.len() < task::MAX_GOAL {
                    edit.insert_char('\n');
                }
            }
            _ => {
                edit.apply_key(code, mods, task::MAX_GOAL);
            }
        },
        Some(TaskUi::Status) => match code {
            KeyCode::Esc | KeyCode::Enter => app.close_task(),
            _ => {}
        },
        None => {}
    }
}

fn rename_key(app: &mut App, code: KeyCode, mods: KeyModifiers) {
    match code {
        KeyCode::Esc => app.cancel_rename(),
        KeyCode::Enter => app.commit_rename(),
        _ => {
            if let Some((_, edit)) = app.ui.rename.as_mut() {
                edit.apply_key(code, mods, 200);
            }
        }
    }
}

fn skill_key(app: &mut App, code: KeyCode, mods: KeyModifiers) {
    let shift = mods.contains(KeyModifiers::SHIFT);
    let width = app.ui.skill_inner.width.max(1);
    let page = app.ui.skill_inner.height.max(1);
    let Some(v) = app.ui.skill_view.as_mut() else { return };
    match code {
        KeyCode::Esc => app.ui.skill_view = None,
        KeyCode::Up => v.edit.move_visual(width, -1, shift),
        KeyCode::Down => v.edit.move_visual(width, 1, shift),
        KeyCode::PageUp => v.scroll = v.scroll.saturating_sub(page),
        KeyCode::PageDown => v.scroll = v.scroll.saturating_add(page),
        KeyCode::Left => v.edit.move_left(shift),
        KeyCode::Right => v.edit.move_right(shift),
        KeyCode::Home => v.edit.home(shift),
        KeyCode::End => v.edit.end(shift),
        KeyCode::Char('a' | 'A') if mods.contains(KeyModifiers::CONTROL) => v.edit.select_all(),
        _ => {}
    }
}

fn settings_key(app: &mut App, code: KeyCode, mods: KeyModifiers) {
    app.ui.focus = Focus::Settings;
    if app.settings.drop.is_some() {
        match code {
            KeyCode::Esc => app.settings.drop = None,
            KeyCode::Up => app.move_drop(-1),
            KeyCode::Down => app.move_drop(1),
            KeyCode::Enter | KeyCode::Char(' ') => app.pick_drop(app.settings.drop_cursor),
            KeyCode::Tab => {
                app.settings.drop = None;
                app.cycle_field(false);
            }
            _ => {}
        }
        return;
    }
    match code {
        KeyCode::Esc => {
            if app.settings.login.in_flight() {
                app.cancel_login();
            } else {
                app.close_settings();
            }
            return;
        }
        KeyCode::Tab => {
            app.cycle_field(false);
            return;
        }
        KeyCode::BackTab => {
            app.cycle_field(true);
            return;
        }
        _ => {}
    }
    if let Some(edit) = app.settings.edit_mut() {
        if code == KeyCode::Enter {
            app.flush_conn();
            app.cycle_field(false);
        } else {
            edit.apply_key(code, mods, 2000);
        }
        return;
    }
    let activate = matches!(code, KeyCode::Enter | KeyCode::Char(' '));
    match app.settings.field {
        Field::Kind => match code {
            KeyCode::Left => app.set_provider_kind(ProviderKind::Xai),
            KeyCode::Right => app.set_provider_kind(ProviderKind::Openai),
            _ if activate => {
                let next = if app.settings.conn.kind.is_openai() { ProviderKind::Xai } else { ProviderKind::Openai };
                app.set_provider_kind(next);
            }
            _ => {}
        },
        Field::Account if activate => app.activate_account(),
        Field::Search if activate => app.toggle_search(),
        Field::Dispatcher if activate => app.toggle_dispatcher(),
        Field::ImportClaude if activate => app.toggle_import(true),
        Field::ImportCodex if activate => app.toggle_import(false),
        Field::Skills => match code {
            KeyCode::Up => app.settings.skill_cursor = app.settings.skill_cursor.saturating_sub(1),
            KeyCode::Down => {
                let n = app.settings.skill_list.len();
                app.settings.skill_cursor = (app.settings.skill_cursor + 1).min(n.saturating_sub(1));
            }
            KeyCode::Char(' ') => app.toggle_skill(app.settings.skill_cursor),
            KeyCode::Enter => app.open_skill(app.settings.skill_cursor),
            _ => {}
        },
        Field::Model if app.settings.model_picker() && (activate || code == KeyCode::Down) => app.open_drop(DropKind::Model),
        Field::ChildModel if app.settings.model_picker() && (activate || code == KeyCode::Down) => {
            app.open_drop(DropKind::ChildModel)
        }
        Field::Effort => match code {
            KeyCode::Left => app.cycle_effort(true),
            KeyCode::Right => app.cycle_effort(false),
            _ if activate || code == KeyCode::Down => app.open_drop(DropKind::Effort),
            _ => {}
        },
        _ => {}
    }
}

/// Copy the pending Grok login code.
pub(crate) fn copy_login_code(app: &mut App) {
    if let crate::tui::settings::LoginUi::Waiting { user_code, .. } = &app.settings.login {
        let msg = if clipboard_set(user_code) { "已複製登入代碼" } else { "無法複製登入代碼" };
        app.flash(msg);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::KeyEvent;

    fn press(code: KeyCode) -> Event {
        Event::Key(KeyEvent::new(code, KeyModifiers::NONE))
    }

    #[test]
    fn ime_commit_swallows_the_enter_in_the_same_burst() {
        let out = coalesce_ime_enter(vec![press(KeyCode::Char('你')), press(KeyCode::Enter)]);
        assert_eq!(out.len(), 1);
        let keep = coalesce_ime_enter(vec![press(KeyCode::Char('a')), press(KeyCode::Enter)]);
        assert_eq!(keep.len(), 2, "ASCII typing then Enter still sends");
    }
}
