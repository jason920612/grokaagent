fn ui_snapshot(app: &App, opts: &TuiOptions) -> UiSnapshot {
    let elapsed_ms = app
        .work_started
        .map(|t| t.elapsed().as_millis() as u64)
        .unwrap_or(0);
    let (login, login_url, login_code) = match &app.login_ui {
        LoginUi::Idle => ("idle".into(), None, None),
        LoginUi::Starting => ("starting".into(), None, None),
        LoginUi::Waiting { url, user_code } => {
            ("waiting".into(), Some(url.clone()), Some(user_code.clone()))
        }
        LoginUi::Failed(m) => (format!("failed:{m}"), None, None),
    };
    let prefs = app
        .skills
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let import_claude = prefs.prefs().import_claude;
    let import_codex = prefs.prefs().import_codex;
    drop(prefs);
    let settings = Some({
        hub::UiSettings {
            field: format!("{:?}", app.setting_field),
            login,
            login_url,
            login_code,
            models: model_choices(app, opts),
            efforts: effort_choices(app, opts)
                .into_iter()
                .map(|e| (e.id, e.label))
                .collect(),
            web_search: opts.web_search,
            dispatcher: opts.dispatcher,
            child_model: opts.child_model.clone(),
            custom_models: !app.custom_catalog.models.is_empty(),
            import_claude,
            import_codex,
            kind: app.conn.kind.as_str().to_string(),
            base_url: app.endpoint_edit.text.clone(),
            api_key: app.api_key_edit.text.clone(),
            context: app.context_edit.text.clone(),
            skills: app
                .skill_list
                .iter()
                .map(|s| hub::UiSkill {
                    name: s.name.clone(),
                    origin: s.origin.label().to_string(),
                    enabled: s.enabled,
                    description: s.description.clone(),
                })
                .collect(),
        }
    });
    UiSnapshot {
        session_id: app.current_id.clone(),
        header: hub::UiHeader {
            model: opts.model.clone(),
            effort: opts.reasoning_effort.as_str().to_string(),
            status: app.status.clone(),
            activity: app.activity.clone(),
            cache: app.cache.clone(),
            running: app.running,
            awaiting: app.awaiting,
            logged_in: app.logged_in,
            elapsed_ms,
            tick: app.tick,
            workspace: folderpick::display_path(&app.session.workspace),
            task_live: app.task.snapshot().phase.is_live(),
            kind: app.conn.kind.as_str().to_string(),
        },
        composer: hub::UiComposer {
            text: app.edit.text.clone(),
            caret: app.edit.caret,
            seq: app.composer_seq,
            echo_seq: app.web_composer_seq,
            queue_edit: app.queue_edit,
        },
        sessions: app
            .sessions
            .iter()
            .map(|s| hub::UiSession {
                status: if s.id == app.current_id {
                    if app.ask.is_some() { "等你回覆".into() } else { app.status.clone() }
                } else {
                    app.parked.get(&s.id).map(|p| p.status.clone()).unwrap_or_else(|| "待命".into())
                },
                id: s.id.clone(),
                name: s.name.clone(),
                short_id: s.short_id(),
                folder: s.folder_label(),
                current: s.id == app.current_id,
            })
            .collect(),
        queue: app
            .queue
            .iter()
            .map(|q| hub::UiQueued {
                text: q.text.clone(),
                images: q.images.len(),
            })
            .collect(),
        pending: app.pending.clone(),
        send_mode: match app.send_mode {
            SendMode::Queue => "queue".into(),
            SendMode::Insert => "insert".into(),
        },
        settings_data: settings.clone(),
        settings: settings.filter(|_| app.settings.as_ref().is_some_and(|w| !w.minimized)),
        ask: app.ask.as_ref().map(|a| hub::UiAsk {
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
        picker: app.workspace_pick.as_ref().map(|p| hub::UiPicker {
            path: p.edit.text.clone(),
            notice: p.notice.clone(),
            cursor: p.cursor,
            entries: p
                .view
                .entries
                .iter()
                .map(|e| hub::UiPickEntry {
                    name: e.name.clone(),
                    is_dir: e.is_dir,
                    is_parent: e.is_parent,
                })
                .collect(),
        }),
        inspector: app.inspector.as_ref().map(|i| match i {
            Inspector::Monitor(n) => hub::UiInspector {
                kind: "monitor".into(),
                name: n.clone(),
            },
            Inspector::Background(n) => hub::UiInspector {
                kind: "background".into(),
                name: n.clone(),
            },
        }),
        image_view: app.image_view.clone(),
        tool_panel: app.open_tool.map(|(group, item)| hub::UiToolPanel { group, item }),
        skill_view: app.skill_view.as_ref().map(|v| hub::UiSkillView {
            title: v.title.clone(),
            origin: v.origin.clone(),
            body: v.edit.text.clone(),
        }),
        rename: app.rename.as_ref().map(|(id, e)| hub::UiRename {
            id: id.clone(),
            text: e.text.clone(),
        }),
        task: app.task_ui.as_ref().map(|ui| {
            let snap = app.task.snapshot();
            hub::UiTask {
                note: snap.review_note.clone(),
                mode: match ui {
                    TaskUi::Form { .. } => "form".into(),
                    TaskUi::Status => "status".into(),
                },
                goal: snap.goal,
                draft: match ui {
                    TaskUi::Form { edit } => edit.text.clone(),
                    TaskUi::Status => String::new(),
                },
                phase: if snap.skip_steer { "已暫停".into() } else { snap.phase.label().to_string() },
                checklist: snap
                    .checklist
                    .into_iter()
                    .map(|i| hub::UiTaskItem {
                        text: i.text,
                        done: i.done,
                    })
                    .collect(),
            }
        }),
        task_summary: {
            let t = app.task.snapshot();
            hub::UiTask {
                note: t.review_note.clone(),
                mode: "status".into(), goal: t.goal, draft: String::new(),
                phase: if t.skip_steer { "已暫停".into() } else { t.phase.label().into() },
                checklist: t.checklist.into_iter().map(|i| hub::UiTaskItem { text: i.text, done: i.done }).collect(),
            }
        },
        receipts: app.web_receipts.clone(),
        web_url: app.web_url.clone().unwrap_or_default(),
    }
}

fn ui_row(row: &Row) -> hub::UiRow {
    match row {
        Row::User(u) => hub::UiRow {
            kind: "user".into(),
            html: hub::text_html(&u.text),
            text: u.text.clone(),
            expanded: None,
            done: None,
            elapsed_ms: None,
            images: u.images.clone(),
            calls: Vec::new(),
            path: None,
            label: None,
        },
        Row::Agent(a) => hub::UiRow {
            kind: "agent".into(),
            html: md::markdown_html(&a.text),
            text: a.text.clone(),
            expanded: None,
            done: None,
            elapsed_ms: Some(a.work_ms),
            images: Vec::new(),
            calls: Vec::new(),
            path: None,
            label: None,
        },
        Row::Think(t) => {
            let elapsed = t
                .started
                .map(|s| s.elapsed().as_millis() as u64)
                .unwrap_or(t.elapsed_ms);
            hub::UiRow {
                kind: "think".into(),
                html: hub::pre_html(&t.text),
                text: t.text.clone(),
                expanded: Some(t.expanded),
                done: Some(t.done),
                elapsed_ms: Some(elapsed),
                images: Vec::new(),
                calls: Vec::new(),
                path: None,
                label: None,
            }
        }
        Row::Tools(g) => hub::UiRow {
            kind: "tools".into(),
            html: String::new(),
            text: format!("{} tools", g.calls.len()),
            expanded: Some(g.expanded),
            done: Some(g.calls.iter().all(|c| c.done)),
            elapsed_ms: None,
            images: Vec::new(),
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
            path: None,
            label: None,
        },
        Row::Meta(s) => hub::UiRow {
            kind: "meta".into(),
            html: hub::text_html(s),
            text: s.clone(),
            expanded: None,
            done: None,
            elapsed_ms: None,
            images: Vec::new(),
            calls: Vec::new(),
            path: None,
            label: None,
        },
        Row::Err(s) => hub::UiRow {
            kind: "err".into(),
            html: hub::text_html(s),
            text: s.clone(),
            expanded: None,
            done: None,
            elapsed_ms: None,
            images: Vec::new(),
            calls: Vec::new(),
            path: None,
            label: None,
        },
        Row::Picture { path, label } => hub::UiRow {
            kind: "picture".into(),
            html: hub::text_html(label),
            text: label.clone(),
            expanded: None,
            done: None,
            elapsed_ms: None,
            images: Vec::new(),
            calls: Vec::new(),
            path: Some(path.clone()),
            label: Some(label.clone()),
        },
    }
}

fn apply_ui_command(
    app: &mut App,
    opts: &mut TuiOptions,
    sink: &Arc<FanoutSink>,
    done_tx: &mpsc::UnboundedSender<(String, crate::agent::RunOutcome)>,
    cmd: UiCommand,
) {
    match cmd {
        UiCommand::RefreshSettings => {
            app.refresh_skills();
            if app.logged_in && !matches!(app.catalog_status, CatalogStatus::Ready | CatalogStatus::Loading) {
                app.want_catalog = true;
            }
        }
        UiCommand::Rename { id, text } => {
            let name = session::sanitize_title(&text);
            if !name.is_empty() { app.apply_manual_name(&id, &name); app.refresh_session_list(); }
        }
        UiCommand::StartTask { session_id, goal } => {
            if session_id != app.current_id || goal.trim().is_empty() { return; }
            let saved_ui = app.task_ui.take();
            let saved_focus = app.focus;
            app.task_ui = Some(TaskUi::Form { edit: Edit::at_end(goal.chars().take(task::MAX_GOAL).collect()) });
            submit_task_goal(app, opts, sink, done_tx);
            app.task_ui = saved_ui;
            app.focus = saved_focus;
        }
        UiCommand::UpdateQueue { session_id, request_id, index, expected, text } => {
            if app.web_receipts.iter().any(|(id, _)| id == &request_id) { return; }
            let result = if session_id != app.current_id || app.queue_edit.is_some()
                || !app.queue.get(index).is_some_and(|q| q.text == expected) {
                "未儲存：佇列已變動，請保留文字並重新開啟編輯"
            } else if text.trim().is_empty() && app.queue[index].images.is_empty() {
                app.queue.remove(index);
                "已接收"
            } else { app.queue[index].text = text.chars().take(100_000).collect(); "已接收" };
            app.web_receipts.push((request_id, result.into()));
        }
        UiCommand::SubmitText { session_id, request_id, text, insert } => {
            if app.web_receipts.iter().any(|(id, _)| id == &request_id) { return; }
            let result = if session_id != app.current_id {
                "未送出：工作已切換，請切回原工作再送出"
            } else if text.trim().is_empty() && app.pending.is_empty() {
                "未送出：訊息是空白"
            } else if !app.logged_in {
                "未送出：請先完成模型連線設定"
            } else if app.inbox_tx.as_ref().is_some_and(|tx| tx.is_closed()) {
                "未送出：執行器已停止，請稍後重試"
            } else {
                let saved_edit = std::mem::replace(&mut app.edit, Edit::at_end(text.chars().take(100_000).collect()));
                let saved_mode = app.send_mode;
                app.send_mode = if insert { SendMode::Insert } else { SendMode::Queue };
                submit_current(app, opts, sink, done_tx, insert);
                app.edit = saved_edit;
                app.send_mode = saved_mode;
                "已接收"
            };
            app.web_receipts.push((request_id, result.into()));
        }
        UiCommand::SetComposer { text, caret, seq } => {
            app.edit.text = text.chars().take(100_000).collect();
            app.edit.caret = caret;
            app.edit.anchor = None;
            app.edit.clamp();
            app.web_composer_seq = seq;
            app.composer_seq = app.composer_seq.max(seq);
        }
        UiCommand::Submit { insert } => {
            app.web_composer_seq = 0;
            app.composer_seq = app.composer_seq.saturating_add(1);
            submit_current(app, opts, sink, done_tx, insert);
        }
        UiCommand::Interrupt => {
            let _ = app.interrupt_work();
        }
        UiCommand::SetSendMode { mode } => {
            app.send_mode = if mode == "insert" {
                SendMode::Insert
            } else {
                SendMode::Queue
            };
        }
        UiCommand::PasteImage => app.paste_image(),
        UiCommand::PasteText { text } => app.paste_text_or_images(&text),
        UiCommand::RemovePending { index } => {
            if index < app.pending.len() {
                app.pending.remove(index);
            }
        }
        UiCommand::ToggleExpand { index } => {
            if let Some(row) = app.rows.get_mut(index) {
                match row {
                    Row::Think(t) => t.expanded = !t.expanded,
                    Row::Tools(g) => g.expanded = !g.expanded,
                    _ => {}
                }
            }
        }
        UiCommand::OpenTool { group, item } => {
            app.open_tool = Some((group, item));
        }
        UiCommand::CloseTool => app.open_tool = None,
        UiCommand::OpenSettings => {
            app.refresh_skills();
            open_settings(app);
        }
        UiCommand::CloseSettings => {
            flush_conn(app, opts);
            app.settings = None;
            app.drop = None;
            if app.focus == Focus::Settings {
                app.focus = Focus::Chat;
            }
        }
        UiCommand::OpenTask => open_task(app),
        UiCommand::CloseTask => close_task(app),
        UiCommand::SetTaskDraft { text } => {
            if let Some(TaskUi::Form { edit }) = &mut app.task_ui {
                *edit = Edit::at_end(text.chars().take(task::MAX_GOAL).collect());
            }
        }
        UiCommand::SubmitTask => submit_task_goal(app, opts, sink, done_tx),
        UiCommand::EndTask => end_task_mode(app),
        UiCommand::Login => activate_account(app),
        UiCommand::Logout => logout_account(app),
        UiCommand::SetProviderKind { kind } => {
            if let Some(k) = ProviderKind::parse(&kind) {
                set_provider_kind(app, opts, k);
            }
        }
        UiCommand::SetEndpoint { text } => {
            app.endpoint_edit = Edit::at_end(text);
            app.setting_field = SettingField::Endpoint;
            flush_conn(app, opts);
        }
        UiCommand::SetApiKey { text } => {
            app.api_key_edit = Edit::at_end(text);
            app.setting_field = SettingField::ApiKey;
            flush_conn(app, opts);
        }
        UiCommand::SetContext { text } => {
            app.context_edit = Edit::at_end(text);
            app.setting_field = SettingField::Context;
            flush_conn(app, opts);
        }
        UiCommand::SetModel { id } => apply_selected_model(app, opts, id),
        UiCommand::SetChildModel { id } => apply_selected_child_model(app, opts, id.trim().to_string()),
        UiCommand::SetEffort { id } => {
            if let Some(e) = ReasoningEffort::parse(&id) {
                apply_selected_effort(app, opts, e);
            }
        }
        UiCommand::ToggleSearch => {
            opts.web_search = !opts.web_search;
            sync_knobs(app, opts);
        }
        UiCommand::ToggleDispatcher => {
            opts.dispatcher = !opts.dispatcher;
            sync_knobs(app, opts);
        }
        UiCommand::ToggleImportClaude => app.toggle_import_claude(),
        UiCommand::ToggleImportCodex => app.toggle_import_codex(),
        UiCommand::ToggleSkill { index } => app.toggle_skill(index),
        UiCommand::OpenSkill { index } => app.open_skill_view(index),
        UiCommand::CloseSkill => app.close_skill_view(),
        UiCommand::NewChat => app.new_chat(),
        UiCommand::Switch { id } => app.switch_to(&id),
        UiCommand::BeginRename { id } => app.begin_rename(&id),
        UiCommand::CommitRename { text } => {
            if let Some((_, edit)) = app.rename.as_mut() {
                *edit = Edit::at_end(text);
            }
            app.commit_rename();
        }
        UiCommand::CancelRename => app.cancel_rename(),
        UiCommand::DeleteSession { id } => app.delete_session(&id),
        UiCommand::AskToggle { index } => app.activate_ask_option(index, false),
        UiCommand::AskFill { index, text } => {
            if let Some(ask) = app.ask.as_mut() {
                if index < ask.values.len() {
                    ask.cursor = index;
                    let clipped: String = text.chars().take(ask::MAX_INPUT).collect();
                    ask.values[index] = clipped.clone();
                    if ask.question.options.get(index).is_some_and(|o| o.input) {
                        if !ask.question.allow_multiple {
                            ask.chosen.fill(false);
                        }
                        if let Some(c) = ask.chosen.get_mut(index) {
                            *c = true;
                        }
                        ask.fill_edit = Edit::at_end(clipped);
                    }
                }
            }
        }
        UiCommand::AskConfirm => app.submit_ask(),
        UiCommand::AskCancel => app.cancel_ask(),
        UiCommand::WsSetPath { text } => {
            if let Some(p) = app.workspace_pick.as_mut() {
                p.edit = Edit::at_end(text.chars().take(1000).collect());
            }
            app.sync_workspace_pick();
        }
        UiCommand::WsSelect { index } => {
            if let Some(p) = app.workspace_pick.as_mut() {
                if index < p.view.entries.len() {
                    p.cursor = index;
                }
            }
        }
        UiCommand::WsConfirm => app.confirm_workspace_pick(),
        UiCommand::WsCancel => app.cancel_workspace_pick(),
        UiCommand::WsCreate => app.create_workspace_dir(),
        UiCommand::WsEnter => {
            let idx = app.workspace_pick.as_ref().map(|p| p.cursor).unwrap_or(0);
            app.activate_ws_entry(idx);
        }
        UiCommand::OpenMonitor { name } => {
            app.inspector = Some(Inspector::Monitor(name));
            app.focus = Focus::Inspector;
        }
        UiCommand::OpenBackground { name } => {
            app.inspector = Some(Inspector::Background(name));
            app.focus = Focus::Inspector;
        }
        UiCommand::CloseInspector => app.close_inspector(),
        UiCommand::OpenImage { path } => app.open_image_view(path),
        UiCommand::CloseImage => app.close_image_view(),
        UiCommand::EditQueue { index } => app.begin_queue_edit(index),
        UiCommand::CancelQueueEdit => app.cancel_queue_edit(),
        UiCommand::CommitQueueEdit => {
            let _ = app.commit_queue_edit();
        }
    }
}

fn ui_logs(app: &App) -> hub::UiLogs {
    const WEB_LOG: usize = 200;
    let tail = |n: usize| n.saturating_sub(WEB_LOG);
    hub::UiLogs {
        agents: app
            .bench
            .agents
            .iter()
            .map(|a| hub::UiAgent {
                path: a.path.clone(),
                name: a.name.clone(),
                depth: a.depth(),
                model: a.model.clone(),
                state: a.state.as_key().into(),
                label: a.state.label().into(),
                activity: a.view.activity.clone(),
                alive: a.alive,
                turn: a.turn,
                tools: a.tools,
                prompt: a.prompt.clone(),
            })
            .collect(),
        tools: app
            .bench
            .tool_log
            .iter()
            .skip(tail(app.bench.tool_log.len()))
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
        events: app
            .bench
            .event_log
            .iter()
            .skip(tail(app.bench.event_log.len()))
            .map(|e| hub::UiEventEntry {
                at: e.at.clone(),
                path: e.path.clone(),
                kind: e.kind.into(),
                text: e.text.clone(),
            })
            .collect(),
        rail: hub::UiRail {
            monitors: app
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
            backgrounds: app
                .backgrounds
                .iter()
                .map(|b| hub::UiBg {
                    name: b.name.clone(),
                    command: b.command.clone(),
                    pid: b.pid,
                    status: b.status.clone(),
                    alive: b.alive,
                    detail: b.detail.clone(),
                    log: b.log.iter().skip(tail(b.log.len())).cloned().collect(),
                })
                .collect(),
        },
        changes: app
            .all_file_changes()
            .into_iter()
            .map(|(view, row, call, path, kind)| hub::UiChange { view, row, call, path, kind })
            .collect(),
    }
}

/// Stable identity of a row's visible content. Running think clocks are left
/// out: the browser ticks those itself.
fn row_hash(row: &Row) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    match row {
        Row::User(u) => {
            0u8.hash(&mut h);
            u.text.hash(&mut h);
            u.images.hash(&mut h);
        }
        Row::Agent(a) => {
            1u8.hash(&mut h);
            a.text.hash(&mut h);
            a.work_ms.hash(&mut h);
        }
        Row::Tools(g) => {
            2u8.hash(&mut h);
            g.expanded.hash(&mut h);
            for c in &g.calls {
                c.call_id.hash(&mut h);
                c.name.hash(&mut h);
                c.args.to_string().hash(&mut h);
                c.output.hash(&mut h);
                c.done.hash(&mut h);
                c.phase.hash(&mut h);
                for f in &c.files {
                    f.path.hash(&mut h);
                    f.kind.hash(&mut h);
                    f.diff.hash(&mut h);
                }
            }
        }
        Row::Think(t) => {
            3u8.hash(&mut h);
            t.text.hash(&mut h);
            t.expanded.hash(&mut h);
            t.done.hash(&mut h);
            if t.done {
                t.elapsed_ms.hash(&mut h);
            }
        }
        Row::Meta(s) => {
            4u8.hash(&mut h);
            s.hash(&mut h);
        }
        Row::Err(s) => {
            5u8.hash(&mut h);
            s.hash(&mut h);
        }
        Row::Picture { path, label } => {
            6u8.hash(&mut h);
            path.hash(&mut h);
            label.hash(&mut h);
        }
    }
    h.finish()
}

/// Row patches for every transcript view whose rows changed since the last
/// publish, plus the views that disappeared.
fn view_patches(app: &mut App) -> (Vec<hub::UiViewPatch>, Vec<String>) {
    let mut views: Vec<(String, &Vec<Row>)> = vec![(String::new(), &app.rows)];
    for a in &app.bench.agents {
        views.push((a.path.clone(), &a.view.rows));
    }
    let mut patches = Vec::new();
    let mut live: std::collections::HashSet<u64> = std::collections::HashSet::new();
    let mut seen: Vec<String> = Vec::new();
    for (path, rows) in views {
        let hashes: Vec<u64> = rows.iter().map(row_hash).collect();
        live.extend(hashes.iter().copied());
        seen.push(path.clone());
        let prev = app.web_sent.get(&path);
        if prev == Some(&hashes) {
            continue;
        }
        let from = match prev {
            Some(p) => p.iter().zip(&hashes).take_while(|(a, b)| a == b).count(),
            None => 0,
        };
        let out: Vec<hub::UiRow> = rows[from..]
            .iter()
            .zip(&hashes[from..])
            .map(|(r, h)| {
                app.web_cache
                    .entry(*h)
                    .or_insert_with(|| ui_row(r))
                    .clone()
            })
            .collect();
        patches.push(hub::UiViewPatch {
            path: path.clone(),
            from,
            len: rows.len(),
            rows: out,
        });
        app.web_sent.insert(path, hashes);
    }
    let removed: Vec<String> = app
        .web_sent
        .keys()
        .filter(|k| !seen.contains(k))
        .cloned()
        .collect();
    for k in &removed {
        app.web_sent.remove(k);
    }
    app.web_cache.retain(|h, _| live.contains(h));
    (patches, removed)
}

fn publish_ui(hub: Option<&hub::Hub>, app: &mut App, opts: &TuiOptions) {
    let Some(hub) = hub else {
        return;
    };
    app.refresh_session_list();
    hub.set_workspace(app.session.workspace.clone());
    let snapshot = ui_snapshot(app, opts);
    let logs = ui_logs(app);
    let (patches, removed) = view_patches(app);
    hub.publish(snapshot, logs, patches, removed);
}

async fn recv_hub_cmd(hub: &mut Option<hub::Hub>) -> Option<UiCommand> {
    match hub {
        Some(h) => h.cmd_rx.recv().await,
        None => std::future::pending().await,
    }
}

