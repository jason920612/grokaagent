//! Web console mirror: builds the shell snapshot, workbench logs and row
//! patches for the hub, and applies commands sent from the browser.

use std::collections::HashSet;
use std::hash::{Hash, Hasher};

use crate::hub::{self, UiCommand, UiSnapshot};
use crate::md;
use crate::provider::ReasoningEffort;
use crate::session;
use crate::task;

use super::app::{App, SendMode, TaskUi};
use super::edit::Edit;
use super::input;
use super::model::rows::Row;
use super::settings::{CatalogStatus, LoginUi};
use crate::config::ProviderKind;

pub(crate) fn snapshot(app: &App) -> UiSnapshot {
    let s = app.cur();
    let t = &s.chat;
    let st = &app.settings;
    let (login, login_url, login_code) = match &st.login {
        LoginUi::Idle => ("idle".to_string(), None, None),
        LoginUi::Starting => ("starting".into(), None, None),
        LoginUi::Waiting { url, user_code } => ("waiting".into(), Some(url.clone()), Some(user_code.clone())),
        LoginUi::Failed(m) => (format!("failed:{m}"), None, None),
    };
    let (import_claude, import_codex) = {
        let g = app.skills.lock().unwrap_or_else(|e| e.into_inner());
        (g.prefs().import_claude, g.prefs().import_codex)
    };
    let settings = hub::UiSettings {
        field: format!("{:?}", st.field),
        login,
        login_url,
        login_code,
        models: app.model_choices(),
        efforts: app.effort_choices().into_iter().map(|e| (e.id, e.label)).collect(),
        web_search: app.opts.web_search,
        dispatcher: app.opts.dispatcher,
        child_model: app.opts.child_model.clone(),
        custom_models: !st.custom_catalog.models.is_empty(),
        import_claude,
        import_codex,
        kind: st.conn.kind.as_str().to_string(),
        base_url: st.endpoint.text.clone(),
        api_key: st.api_key.text.clone(),
        context: st.context.text.clone(),
        skills: st
            .skill_list
            .iter()
            .map(|k| hub::UiSkill {
                name: k.name.clone(),
                origin: k.origin.label().to_string(),
                enabled: k.enabled,
                description: k.description.clone(),
            })
            .collect(),
    };
    let task_snap = s.task.snapshot();
    let ui_task = |mode: &str, draft: String| hub::UiTask {
        note: task_snap.review_note.clone(),
        mode: mode.into(),
        goal: task_snap.goal.clone(),
        draft,
        phase: if task_snap.skip_steer { "已暫停".into() } else { task_snap.phase.label().into() },
        checklist: task_snap
            .checklist
            .iter()
            .map(|i| hub::UiTaskItem { text: i.text.clone(), done: i.done })
            .collect(),
    };
    UiSnapshot {
        session_id: app.current.clone(),
        header: hub::UiHeader {
            model: app.opts.model.clone(),
            effort: app.opts.reasoning_effort.as_str().to_string(),
            status: t.status.clone(),
            activity: t.activity.clone(),
            cache: t.cache.clone(),
            running: t.running,
            awaiting: t.awaiting,
            logged_in: st.logged_in,
            elapsed_ms: t.work_started.map(|w| w.elapsed().as_millis() as u64).unwrap_or(0),
            tick: app.tick,
            workspace: crate::folderpick::display_path(&s.meta.workspace),
            task_live: task_snap.phase.is_live(),
            kind: st.conn.kind.as_str().to_string(),
        },
        composer: hub::UiComposer {
            text: s.draft.text.clone(),
            caret: s.draft.caret,
            seq: app.web.composer_seq,
            echo_seq: app.web.web_composer_seq,
            queue_edit: s.queue_edit,
        },
        sessions: app
            .listed
            .iter()
            .map(|m| hub::UiSession {
                status: match app.sessions.get(&m.id) {
                    Some(x) if x.ask.is_some() => "等你回覆".into(),
                    Some(x) => x.chat.status.clone(),
                    None => "待命".into(),
                },
                id: m.id.clone(),
                name: m.name.clone(),
                short_id: m.short_id(),
                folder: m.folder_label(),
                current: m.id == app.current,
            })
            .collect(),
        queue: s.queue.iter().map(|q| hub::UiQueued { text: q.text.clone(), images: q.images.len() }).collect(),
        pending: s.pending.clone(),
        send_mode: match app.ui.send_mode {
            SendMode::Queue => "queue".into(),
            SendMode::Insert => "insert".into(),
        },
        settings: None,
        settings_data: Some(settings),
        ask: s.ask.as_ref().map(|a| hub::UiAsk {
            prompt: a.question.prompt.clone(),
            allow_multiple: a.question.allow_multiple,
            options: a
                .question
                .options
                .iter()
                .enumerate()
                .map(|(i, o)| hub::UiAskOpt {
                    id: o.id.clone(),
                    label: o.label.clone(),
                    input: o.input,
                    chosen: a.chosen.get(i).copied().unwrap_or(false),
                    value: a.values.get(i).cloned().unwrap_or_default(),
                })
                .collect(),
        }),
        picker: app.ui.workspace_pick.as_ref().map(|p| hub::UiPicker {
            path: p.edit.text.clone(),
            notice: p.notice.clone(),
            cursor: p.cursor,
            entries: p
                .view
                .entries
                .iter()
                .map(|e| hub::UiPickEntry { name: e.name.clone(), is_dir: e.is_dir, is_parent: e.is_parent })
                .collect(),
        }),
        inspector: None,
        image_view: None,
        tool_panel: None,
        skill_view: app.ui.skill_view.as_ref().map(|v| hub::UiSkillView {
            title: v.title.clone(),
            origin: v.origin.clone(),
            body: v.edit.text.clone(),
        }),
        rename: None,
        task: app.ui.task_ui.as_ref().map(|ui| match ui {
            TaskUi::Form(edit) => ui_task("form", edit.text.clone()),
            TaskUi::Status => ui_task("status", String::new()),
        }),
        task_summary: ui_task("status", String::new()),
        receipts: app.web.receipts.clone(),
        web_url: app.web.url.clone().unwrap_or_default(),
    }
}

pub(crate) fn logs(app: &App) -> hub::UiLogs {
    const WEB_LOG: usize = 200;
    let s = app.cur();
    let skip = |n: usize| n.saturating_sub(WEB_LOG);
    hub::UiLogs {
        agents: s
            .agents
            .nodes
            .iter()
            .map(|a| hub::UiAgent {
                path: a.path.clone(),
                name: a.name.clone(),
                depth: a.depth(),
                model: a.model.clone(),
                state: a.state.key().into(),
                label: a.state.label().into(),
                activity: a.transcript.activity.clone(),
                alive: a.alive(),
                turn: a.turn,
                tools: a.tools,
                prompt: a.prompt.clone(),
            })
            .collect(),
        tools: s
            .agents
            .tool_log
            .iter()
            .skip(skip(s.agents.tool_log.len()))
            .map(|t| hub::UiToolEntry {
                path: t.path.clone(),
                call_id: t.call_id.clone(),
                name: t.name.clone(),
                line: t.line.clone(),
                phase: t.phase.clone(),
                done: t.done,
                ms: t.ms,
            })
            .collect(),
        events: s
            .agents
            .event_log
            .iter()
            .skip(skip(s.agents.event_log.len()))
            .map(|e| hub::UiEventEntry {
                at: e.at.clone(),
                path: e.path.clone(),
                kind: e.kind.key().into(),
                text: e.text.clone(),
            })
            .collect(),
        rail: hub::UiRail {
            monitors: s
                .monitors
                .iter()
                .map(|m| hub::UiMon {
                    name: m.name.clone(),
                    command: m.command.clone(),
                    pid: m.pid,
                    status: m.status.clone(),
                    alive: m.alive,
                    detail: m.detail.clone(),
                })
                .collect(),
            backgrounds: s
                .backgrounds
                .iter()
                .map(|b| hub::UiBg {
                    name: b.name.clone(),
                    command: b.command.clone(),
                    pid: b.pid,
                    status: b.status.clone(),
                    alive: b.alive,
                    detail: b.detail.clone(),
                    log: b.log.iter().skip(skip(b.log.len())).cloned().collect(),
                })
                .collect(),
        },
        changes: s
            .file_changes()
            .into_iter()
            .map(|(view, row, call, path, kind)| hub::UiChange { view, row, call, path, kind })
            .collect(),
    }
}

fn blank(kind: &str) -> hub::UiRow {
    hub::UiRow {
        kind: kind.into(),
        html: String::new(),
        text: String::new(),
        expanded: None,
        done: None,
        elapsed_ms: None,
        images: Vec::new(),
        calls: Vec::new(),
        path: None,
        label: None,
    }
}

pub(crate) fn ui_row(row: &Row) -> hub::UiRow {
    match row {
        Row::User(u) => hub::UiRow {
            html: hub::text_html(&u.text),
            text: u.text.clone(),
            images: u.images.clone(),
            ..blank("user")
        },
        Row::Agent(a) => hub::UiRow {
            html: md::markdown_html(&a.text),
            text: a.text.clone(),
            elapsed_ms: Some(a.work_ms),
            ..blank("agent")
        },
        Row::Think(t) => hub::UiRow {
            html: hub::pre_html(&t.text),
            text: t.text.clone(),
            expanded: Some(t.expanded),
            done: Some(t.done),
            elapsed_ms: Some(t.elapsed()),
            ..blank("think")
        },
        Row::Tools(g) => hub::UiRow {
            text: format!("{} tools", g.calls.len()),
            expanded: Some(g.expanded),
            done: Some(g.calls.iter().all(|c| c.done)),
            calls: g
                .calls
                .iter()
                .map(|c| hub::UiToolCall {
                    call_id: c.call_id.clone(),
                    name: c.name.clone(),
                    phase: c.phase.clone(),
                    done: c.done,
                    args: c.args.to_string(),
                    output: c.output.clone(),
                    files: c
                        .files
                        .iter()
                        .map(|f| hub::UiFileChange {
                            path: f.path.clone(),
                            kind: f.kind.clone(),
                            diff_html: hub::diff_html(&f.diff),
                        })
                        .collect(),
                })
                .collect(),
            ..blank("tools")
        },
        Row::Meta(s) => hub::UiRow { html: hub::text_html(s), text: s.clone(), ..blank("meta") },
        Row::Err(s) => hub::UiRow { html: hub::text_html(s), text: s.clone(), ..blank("err") },
        Row::Picture { path, label } => hub::UiRow {
            html: hub::text_html(label),
            text: label.clone(),
            path: Some(path.clone()),
            label: Some(label.clone()),
            ..blank("picture")
        },
    }
}

/// Identity of a row's visible content. Running think clocks are left out;
/// the browser ticks those itself.
pub(crate) fn row_hash(row: &Row) -> u64 {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    match row {
        Row::User(u) => (0u8, &u.text, &u.images).hash(&mut h),
        Row::Agent(a) => (1u8, &a.text, a.work_ms).hash(&mut h),
        Row::Tools(g) => {
            (2u8, g.expanded).hash(&mut h);
            for c in &g.calls {
                (&c.call_id, &c.name, c.args.to_string(), &c.output, c.done, &c.phase).hash(&mut h);
                for f in &c.files {
                    (&f.path, &f.kind, &f.diff).hash(&mut h);
                }
            }
        }
        Row::Think(t) => (3u8, &t.text, t.expanded, t.done, if t.done { t.elapsed_ms } else { 0 }).hash(&mut h),
        Row::Meta(s) => (4u8, s).hash(&mut h),
        Row::Err(s) => (5u8, s).hash(&mut h),
        Row::Picture { path, label } => (6u8, path, label).hash(&mut h),
    }
    h.finish()
}

/// Patches for every transcript view whose rows changed since the last
/// publish, plus the views that went away.
pub(crate) fn view_patches(app: &mut App) -> (Vec<hub::UiViewPatch>, Vec<String>) {
    let App { sessions, current, web, .. } = app;
    let s = sessions.get(current.as_str()).expect("current session");
    let mut views: Vec<(String, &Vec<Row>)> = vec![(String::new(), &s.chat.rows)];
    views.extend(s.agents.nodes.iter().map(|a| (a.path.clone(), &a.transcript.rows)));
    let mut patches = Vec::new();
    let mut live = HashSet::new();
    let mut seen = HashSet::new();
    for (path, rows) in views {
        let hashes: Vec<u64> = rows.iter().map(row_hash).collect();
        live.extend(hashes.iter().copied());
        seen.insert(path.clone());
        let prev = web.sent.get(&path);
        if prev == Some(&hashes) {
            continue;
        }
        let from = prev.map(|p| p.iter().zip(&hashes).take_while(|(a, b)| a == b).count()).unwrap_or(0);
        let out = rows[from..]
            .iter()
            .zip(&hashes[from..])
            .map(|(r, h)| web.cache.entry(*h).or_insert_with(|| ui_row(r)).clone())
            .collect();
        patches.push(hub::UiViewPatch { path: path.clone(), from, len: rows.len(), rows: out });
        web.sent.insert(path, hashes);
    }
    let removed: Vec<String> = web.sent.keys().filter(|k| !seen.contains(*k)).cloned().collect();
    for k in &removed {
        web.sent.remove(k);
    }
    web.cache.retain(|h, _| live.contains(h));
    (patches, removed)
}

pub(crate) fn publish(hub: Option<&hub::Hub>, app: &mut App) {
    let Some(hub) = hub else { return };
    hub.set_workspace(app.workspace());
    let snap = snapshot(app);
    let logs = logs(app);
    let (patches, removed) = view_patches(app);
    hub.publish(snap, logs, patches, removed);
}

fn receipt(app: &mut App, id: String, result: &str) {
    app.web.receipts.push((id, result.to_string()));
    if app.web.receipts.len() > 50 {
        app.web.receipts.remove(0);
    }
}

/// Apply one command from the browser.
pub(crate) fn apply_command(app: &mut App, cmd: UiCommand) {
    match cmd {
        UiCommand::RefreshSettings => {
            app.refresh_skills();
            if app.settings.logged_in && !matches!(app.settings.catalog_status, CatalogStatus::Ready | CatalogStatus::Loading) {
                app.settings.want_catalog = true;
            }
        }
        UiCommand::Rename { id, text } => {
            let name = session::sanitize_title(&text);
            if !name.is_empty() {
                app.rename_manual(&id, &name);
            }
        }
        UiCommand::StartTask { session_id, goal } => {
            if session_id == app.current && !goal.trim().is_empty() {
                let goal: String = goal.chars().take(task::MAX_GOAL).collect();
                app.submit_task_goal(&goal);
                app.ui.task_ui = None;
            }
        }
        UiCommand::UpdateQueue { session_id, request_id, index, expected, text } => {
            if app.web.receipts.iter().any(|(id, _)| *id == request_id) {
                return;
            }
            let ok = session_id == app.current
                && app.cur().queue_edit.is_none()
                && app.cur().queue.get(index).is_some_and(|q| q.text == expected);
            let result = if !ok {
                "未儲存：佇列已變動，請保留文字並重新開啟編輯"
            } else {
                let s = app.cur_mut();
                if text.trim().is_empty() && s.queue[index].images.is_empty() {
                    s.queue.remove(index);
                } else {
                    s.queue[index].text = text.chars().take(100_000).collect();
                }
                "已接收"
            };
            receipt(app, request_id, result);
        }
        UiCommand::SubmitText { session_id, request_id, text, insert } => {
            if app.web.receipts.iter().any(|(id, _)| *id == request_id) {
                return;
            }
            let result = if session_id != app.current {
                "未送出：工作已切換，請切回原工作再送出"
            } else if text.trim().is_empty() && app.cur().pending.is_empty() {
                "未送出：訊息是空白"
            } else if !app.settings.logged_in {
                "未送出：請先完成模型連線設定"
            } else if app.cur().inbox.as_ref().is_some_and(|tx| tx.is_closed()) {
                "未送出：執行器已停止，請稍後重試"
            } else {
                let saved = std::mem::replace(&mut app.cur_mut().draft, Edit::at_end(text.chars().take(100_000).collect::<String>()));
                let mode = app.ui.send_mode;
                app.ui.send_mode = if insert { SendMode::Insert } else { SendMode::Queue };
                app.submit_current(insert);
                app.cur_mut().draft = saved;
                app.ui.send_mode = mode;
                "已接收"
            };
            receipt(app, request_id, result);
        }
        UiCommand::SetComposer { text, caret, seq } => {
            let d = &mut app.cur_mut().draft;
            d.text = text.chars().take(100_000).collect();
            d.caret = caret;
            d.anchor = None;
            d.clamp();
            app.web.web_composer_seq = seq;
            app.web.composer_seq = app.web.composer_seq.max(seq);
        }
        UiCommand::Submit { insert } => {
            app.web.web_composer_seq = 0;
            app.web.composer_seq += 1;
            app.submit_current(insert);
        }
        UiCommand::Interrupt => {
            app.interrupt();
        }
        UiCommand::SetSendMode { mode } => {
            app.ui.send_mode = if mode == "insert" { SendMode::Insert } else { SendMode::Queue };
        }
        UiCommand::PasteImage => app.paste_image(),
        UiCommand::PasteText { text } => app.paste_text(&text),
        UiCommand::RemovePending { index } => {
            let s = app.cur_mut();
            if index < s.pending.len() {
                s.pending.remove(index);
            }
        }
        UiCommand::ToggleExpand { index } => match app.cur_mut().chat.rows.get_mut(index) {
            Some(Row::Think(t)) => t.expanded = !t.expanded,
            Some(Row::Tools(g)) => g.expanded = !g.expanded,
            _ => {}
        },
        UiCommand::OpenTool { group, item } => app.reveal_tool("", group, item),
        UiCommand::CloseTool => app.cur_mut().view_mut().open_tool = None,
        // The browser shows settings from `settings_data` on its own.
        UiCommand::OpenSettings => app.refresh_skills(),
        UiCommand::CloseSettings => app.flush_conn(),
        UiCommand::OpenTask => app.open_task(),
        UiCommand::CloseTask => app.close_task(),
        UiCommand::SetTaskDraft { text } => {
            if let Some(TaskUi::Form(edit)) = app.ui.task_ui.as_mut() {
                *edit = Edit::at_end(text.chars().take(task::MAX_GOAL).collect::<String>());
            }
        }
        UiCommand::SubmitTask => {
            if let Some(TaskUi::Form(edit)) = &app.ui.task_ui {
                let goal = edit.text.clone();
                app.submit_task_goal(&goal);
            }
        }
        UiCommand::EndTask => app.end_task(),
        UiCommand::Login => app.activate_account(),
        UiCommand::Logout => app.logout(),
        UiCommand::SetProviderKind { kind } => {
            if let Some(k) = ProviderKind::parse(&kind) {
                app.set_provider_kind(k);
            }
        }
        UiCommand::SetEndpoint { text } => {
            app.settings.endpoint = Edit::at_end(text);
            app.flush_conn();
        }
        UiCommand::SetApiKey { text } => {
            app.settings.api_key = Edit::at_end(text);
            app.flush_conn();
        }
        UiCommand::SetContext { text } => {
            app.settings.context = Edit::at_end(text);
            app.flush_conn();
        }
        UiCommand::SetModel { id } => {
            if app.settings.conn.kind.is_openai() && app.settings.custom_catalog.models.is_empty() {
                app.settings.model_edit = Edit::at_end(id.clone());
            }
            app.select_model(id);
        }
        UiCommand::SetChildModel { id } => app.select_child_model(id),
        UiCommand::SetEffort { id } => {
            if let Some(e) = ReasoningEffort::parse(&id) {
                app.select_effort(e);
            }
        }
        UiCommand::ToggleSearch => app.toggle_search(),
        UiCommand::ToggleDispatcher => app.toggle_dispatcher(),
        UiCommand::ToggleImportClaude => app.toggle_import(true),
        UiCommand::ToggleImportCodex => app.toggle_import(false),
        UiCommand::ToggleSkill { index } => app.toggle_skill(index),
        UiCommand::OpenSkill { index } => app.open_skill(index),
        UiCommand::CloseSkill => app.ui.skill_view = None,
        UiCommand::NewChat => app.begin_new_chat(),
        UiCommand::Switch { id } => {
            app.switch_to(&id);
            app.show_chat(None);
        }
        UiCommand::BeginRename { id } => app.begin_rename(&id),
        UiCommand::CommitRename { text } => {
            if let Some((_, edit)) = app.ui.rename.as_mut() {
                *edit = Edit::at_end(text);
            }
            app.commit_rename();
        }
        UiCommand::CancelRename => app.cancel_rename(),
        UiCommand::DeleteSession { id } => app.delete_session(&id),
        UiCommand::AskToggle { index } => input::activate_ask_option(app, index, false),
        UiCommand::AskFill { index, text } => {
            if let Some(a) = app.cur_mut().ask.as_mut() {
                a.set_value(index, &text);
            }
        }
        UiCommand::AskConfirm => input::submit_ask(app),
        UiCommand::AskCancel => app.cur_mut().cancel_ask(),
        UiCommand::WsSetPath { text } => {
            if let Some(p) = app.ui.workspace_pick.as_mut() {
                p.edit = Edit::at_end(text.chars().take(1000).collect::<String>());
                p.sync();
            }
        }
        UiCommand::WsSelect { index } => {
            if let Some(p) = app.ui.workspace_pick.as_mut() {
                if index < p.view.entries.len() {
                    p.cursor = index;
                }
            }
        }
        UiCommand::WsConfirm => input::confirm_workspace(app),
        UiCommand::WsCancel => app.ui.workspace_pick = None,
        UiCommand::WsCreate => input::create_workspace_dir(app),
        UiCommand::WsEnter => {
            let idx = app.ui.workspace_pick.as_ref().map(|p| p.cursor).unwrap_or(0);
            input::activate_ws_entry(app, idx);
        }
        UiCommand::OpenMonitor { name } => app.ui.inspector = Some(name),
        UiCommand::OpenBackground { name } => app.cur_mut().output_pick = Some(name),
        UiCommand::CloseInspector => app.ui.inspector = None,
        UiCommand::OpenImage { path } => app.open_image(path),
        UiCommand::CloseImage => app.ui.image_view = None,
        UiCommand::EditQueue { index } => app.cur_mut().begin_queue_edit(index),
        UiCommand::CancelQueueEdit => app.cur_mut().cancel_queue_edit(),
        UiCommand::CommitQueueEdit => {
            app.cur_mut().commit_queue_edit();
        }
    }
}
