pub async fn run_tui(opts: TuiOptions) -> Result<()> {
    let auth_path = auth::default_auth_path()?;
    enable_raw_mode().map_err(Error::Io)?;
    let mut stdout = stdout();
    execute!(
        stdout,
        EnterAlternateScreen,
        EnableMouseCapture,
        EnableBracketedPaste
    )
    .map_err(Error::Io)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend).map_err(Error::Io)?;

    let result = tui_loop(&mut terminal, opts, auth_path).await;

    let _ = disable_raw_mode();
    let _ = execute!(
        io::stdout(),
        DisableBracketedPaste,
        DisableMouseCapture,
        LeaveAlternateScreen
    );
    result
}

async fn tui_loop(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    mut opts: TuiOptions,
    auth_path: PathBuf,
) -> Result<()> {
    let (tx, mut ev_rx) = mpsc::unbounded_channel();
    let jsonl = JsonlSink::create(&opts.events)?;
    let sink: Arc<FanoutSink> = Arc::new(FanoutSink {
        sinks: vec![Box::new(jsonl), Box::new(ChannelSink::new(tx))],
    });

    let knobs = Arc::new(Mutex::new(SessionKnobs {
        model: opts.model.clone(),
        reasoning_effort: opts.reasoning_effort,
        send_reasoning: true,
        server_tools: kit::search_tools(opts.web_search),
        dispatcher: opts.dispatcher,
        child_model: opts.child_model.clone(),
    }));

    let store = SessionStore::open().ok();
    let launch_workspace = opts.workspace.clone();
    let mut listed = store
        .as_ref()
        .and_then(|s| s.list().ok())
        .unwrap_or_default();
    let boot = boot_session(store.as_ref(), &listed, launch_workspace.clone());
    let session = boot.session;
    let created = boot.created;
    let session_id = session.id.clone();
    let task_state = store
        .as_ref()
        .and_then(|s| s.load_task::<crate::task::TaskState>(&session_id))
        .unwrap_or_default();
    let boot_bench = store
        .as_ref()
        .and_then(|s| s.load_agents::<Vec<SavedAgent>>(&session_id))
        .map(Workbench::restore)
        .unwrap_or_default();
    if !listed.iter().any(|m| m.id == session.id) {
        listed.insert(0, session.clone());
    }

    let skills_fallback = launch_workspace.clone();
    let mut app = App {
        web_receipts: Vec::new(),
        side_view: SideView::Sessions,
        // Dock the side bar only where it fits; narrow terminals open it on demand.
        side_open: terminal
            .size()
            .map(|s| s.width >= SIDEBAR_MIN_TERM)
            .unwrap_or(true),
        side_scroll: 0,
        bottom: None,
        bottom_scroll: 0,
        side_area: Rect::default(),
        bottom_area: Rect::default(),
        web_sent: HashMap::new(),
        web_cache: HashMap::new(),
        rows: boot.rows,
        edit: Edit::default(),
        status: "待命".into(),
        cache: "cache —".into(),
        child_count: 0,
        running: false,
        awaiting: false,
        logged_in: auth::load_tokens(&auth_path).is_ok(),
        auth_path: auth_path.clone(),
        login_ui: LoginUi::Idle,
        login_gen: 0,
        want_login: false,
        scroll: 0,
        stick_bottom: true,
        send_mode: SendMode::Queue,
        queue: VecDeque::new(),
        inbox_tx: None,
        cancel: None,
        knobs: knobs.clone(),
        focus: Focus::Chat,
        setting_field: SettingField::Model,
        settings: None,
        conn: ProviderConfig::default(),
        endpoint_edit: Edit::default(),
        api_key_edit: Edit::default(),
        model_edit: Edit::default(),
        child_model_edit: Edit::default(),
        context_edit: Edit::default(),
        drag: None,
        scroll_grab: None,
        hits: Vec::new(),
        area: Rect::default(),
        streaming: false,
        composer_inner: Rect::default(),
        composer_frame: Rect::default(),
        composer_snap: None,
        header_bar: Rect::default(),
        think_clocks: Vec::new(),
        last_clock_cells: Vec::new(),
        last_caret: Position::ORIGIN,
        chat_inner: Rect::default(),
        chat_bar: Rect::default(),
        chat_total: 0,
        chat_max_off: 0,
        composer_vscroll: 0,
        input_dragging: false,
        chat_dragging: false,
        chat_glyphs: Vec::new(),
        catalog: ModelCatalog::default(),
        catalog_status: CatalogStatus::Idle,
        grok_catalog: ModelCatalog::default(),
        custom_catalog: ModelCatalog::default(),
        custom_cat_key: (String::new(), String::new()),
        custom_cat_loading: false,
        custom_cat_err: None,
        xai_ready: false,
        drop: None,
        drop_cursor: 0,
        drop_scroll: 0,
        want_catalog: false,
        open_tool: None,
        seal_tools: false,
        activity: String::new(),
        tick: 0,
        current_id: session.id.clone(),
        session,
        parked: HashMap::new(),
        sessions: listed,
        store,
        launch_workspace,
        sidebar_ids: Vec::new(),
        rename: None,
        rename_inner: Rect::default(),
        work_started: None,
        queue_edit: None,
        composer_stash: None,
            pending: Vec::new(),
            chat_sel: ChatSel::None,
            preview: HashMap::new(),
            picker: Some(crate::preview::detect_picker()),
            image_proto: HashMap::new(),
            image_cells: HashMap::new(),
            graphic_blits: Vec::new(),
            last_graphic_blits: Vec::new(),
            image_hits: Vec::new(),
            image_view: None,
            bench: boot_bench,
            monitors: Vec::new(),
            backgrounds: Vec::new(),
            inspector: None,
            inspector_scroll: 0,
            ask_hub: AskUserHub::new(),
            ask_hubs: HashMap::new(),
            ask: None,
            ask_fill_inner: Rect::default(),
            ask_passive: false,
            workspace_pick: None,
            task: TaskHub::from_state(session_id, task_state),
            task_ui: None,
            task_draft_inner: Rect::default(),
            task_action: None,
            skills: Arc::new(Mutex::new(
                SkillStore::open().unwrap_or_else(|_| {
                    SkillStore::open_at(
                        skills_fallback.join(".groka").join("skills-store"),
                        skills_fallback,
                    )
                }),
            )),
            skill_list: Vec::new(),
            skill_cursor: 0,
            skill_scroll: 0,
            skill_view: None,
            web_url: None,
            composer_seq: 0,
            web_composer_seq: 0,
    };
    app.catalog.ensure_current(&opts.model, opts.reasoning_effort);
    let mut conn = ProviderConfig::load();
    if conn.route().is_openai() && !conn.kind.is_openai() {
        conn.kind = ProviderKind::Openai;
        opts.web_search = false;
        let _ = conn.save();
    }
    if conn.kind.is_openai() {
        if !conn.model.trim().is_empty() {
            opts.model = conn.model.clone();
        }
        opts.web_search = false;
    }
    app.conn = conn;
    edits_from_conn(&mut app, &opts);
    refresh_ready(&mut app);
    rebuild_catalog(&mut app, &opts);
    sync_knobs(&app, &opts);
    // Fetch the Grok catalog whenever the login exists, even from the custom
    // panel: both sources feed the same merged model picker.
    app.want_catalog = app.xai_ready;
    if created || app.is_blank_draft() {
        if app.conn.kind.is_openai() {
            app.push(Row::Meta(format!(
                "自訂 API  {}  ·  {}",
                app.conn.base_url,
                opts.model
            )));
        } else if app.logged_in {
            app.push(Row::Meta("磁碟上有 xAI session".into()));
        } else {
            app.push(Row::Meta("尚未登入 — 在設定中登入 Grok，或改連自訂 API".into()));
        }
    }

    let mut hub = match hub::start(app.session.workspace.clone()).await {
        Ok(h) => h,
        Err(e) => {
            app.push(Row::Err(format!("網頁界面無法啟動: {e}")));
            None
        }
    };
    if let Some(h) = &hub {
        app.web_url = Some(h.url.clone());
        app.push(Row::Meta(format!("網頁界面  {}", h.url)));
    }

    let mut keys = EventStream::new();
    let (done_tx, mut done_rx) = mpsc::unbounded_channel::<(String, crate::agent::RunOutcome)>();
    let (cat_tx, mut cat_rx) = mpsc::unbounded_channel();
    let (custom_cat_tx, mut custom_cat_rx) = mpsc::unbounded_channel();
    let (login_tx, mut login_rx) = mpsc::unbounded_channel();
    let mut last_size = terminal.size().map_err(Error::Io)?;
    let mut content_dirty = true;
    let mut composer_dirty = true;
    let mut cursor_shown = true;
    let mut cursor_placed = None;

    loop {
        while let Ok(ev) = ev_rx.try_recv() {
            app.route_event(ev);
            content_dirty = true;
        }
        while let Ok(result) = cat_rx.try_recv() {
            ingest_catalog(&mut app, &mut opts, result);
            content_dirty = true;
        }
        while let Ok(result) = custom_cat_rx.try_recv() {
            ingest_custom_catalog(&mut app, &mut opts, result);
            content_dirty = true;
        }
        while let Ok(ev) = login_rx.try_recv() {
            apply_login_event(&mut app, ev);
            content_dirty = true;
            composer_dirty = true;
        }
        if let Some(h) = hub.as_mut() {
            while let Ok(cmd) = h.cmd_rx.try_recv() {
                apply_ui_command(&mut app, &mut opts, &sink, &done_tx, cmd);
                content_dirty = true;
                composer_dirty = true;
            }
        }
        if app.want_catalog
            && app.xai_ready
            && !matches!(app.catalog_status, CatalogStatus::Loading)
        {
            app.want_catalog = false;
            app.catalog_status = CatalogStatus::Loading;
            content_dirty = true;
            let auth_path = app.auth_path.clone();
            let tx = cat_tx.clone();
            tokio::spawn(async move {
                let result = match XaiOauthProvider::new(auth_path, None) {
                    Ok(p) => p.list_models().await,
                    Err(e) => Err(e),
                };
                let _ = tx.send(result);
            });
        }
        let custom_key = (
            app.conn.base_url.trim().to_string(),
            app.conn.api_key.trim().to_string(),
        );
        if !custom_key.0.is_empty() && custom_key != app.custom_cat_key && !app.custom_cat_loading {
            app.custom_cat_loading = true;
            app.custom_cat_key = custom_key;
            content_dirty = true;
            let cfg = app.conn.clone();
            let tx = custom_cat_tx.clone();
            tokio::spawn(async move {
                let result = match crate::openai::OpenAiCompatProvider::new(
                    &cfg,
                    Some("catalog-probe".into()),
                ) {
                    Ok(p) => p.list_models().await,
                    Err(e) => Err(e),
                };
                let _ = tx.send(result);
            });
        }
        if app.want_login {
            app.want_login = false;
            content_dirty = true;
            composer_dirty = true;
            let tx = login_tx.clone();
            let path = app.auth_path.clone();
            let gen = app.login_gen;
            tokio::spawn(async move {
                run_settings_login(path, gen, tx).await;
            });
        }
        flush_all(&mut app);
        while let Ok((sid, out)) = done_rx.try_recv() {
            app.finish_run(&sid, out);
            content_dirty = true;
            composer_dirty = true;
        }
        kick_idle_queue(&mut app, &opts, &sink, &done_tx);
        pulse_spinner(&mut app);
        if content_dirty || composer_dirty || is_pulsing(&app) {
            publish_ui(hub.as_ref(), &mut app, &opts);
        }
        match terminal.size() {
            Ok(size) if size != last_size => {
                last_size = size;
                content_dirty = true;
                composer_dirty = true;
            }
            Ok(_) => {}
            Err(e) => return Err(Error::Io(e)),
        }

        if content_dirty || composer_dirty {
            let freeze = !composer_dirty;
            if freeze && cursor_shown {
                terminal.hide_cursor().map_err(Error::Io)?;
                cursor_shown = false;
                cursor_placed = None;
            }
            let caret = paint_frame(terminal, &mut app, &opts, freeze)?;
            app.last_caret = caret;
            app.last_clock_cells = collect_clock_cells(&app, &opts);
            flush_image_blits(&mut app, caret)?;
            sync_cursor(
                terminal,
                caret,
                want_hardware_cursor(&app),
                &mut cursor_shown,
                &mut cursor_placed,
            )?;
            content_dirty = false;
            composer_dirty = false;
        } else if side_pulse_ok(&app) {
            // Child/monitor/background still alive: refresh rail spinners.
            if cursor_shown {
                terminal.hide_cursor().map_err(Error::Io)?;
                cursor_shown = false;
                cursor_placed = None;
            }
            let caret = paint_frame(terminal, &mut app, &opts, true)?;
            app.last_caret = caret;
            app.last_clock_cells = collect_clock_cells(&app, &opts);
            flush_image_blits(&mut app, caret)?;
            sync_cursor(
                terminal,
                caret,
                want_hardware_cursor(&app),
                &mut cursor_shown,
                &mut cursor_placed,
            )?;
        } else if header_pulse_ok(&app) {
            paint_clocks(terminal, &mut app, &opts)?;
        }

        tokio::select! {
            maybe = keys.next() => {
                match maybe {
                    Some(Ok(ev)) => {
                        let mut batch = vec![ev];
                        let mut quit = false;
                        while let Some(ready) = keys.next().now_or_never() {
                            match ready {
                                Some(Ok(ev)) => batch.push(ev),
                                _ => break,
                            }
                        }
                        let batch = coalesce_ime_enter(batch);
                        if batch.is_empty() {
                            continue;
                        }
                        content_dirty = true;
                        composer_dirty = true;
                        app.composer_seq = app.composer_seq.saturating_add(1);
                        for ev in batch {
                            if handle_input(&mut app, &mut opts, ev, &sink, &done_tx) {
                                quit = true;
                                break;
                            }
                            flush_task_action(&mut app, &opts, &sink, &done_tx);
                        }
                        if quit {
                            break;
                        }
                    }
                    _ => continue,
                }
            }
            cmd = recv_hub_cmd(&mut hub) => {
                if let Some(cmd) = cmd {
                    apply_ui_command(&mut app, &mut opts, &sink, &done_tx, cmd);
                    content_dirty = true;
                    composer_dirty = true;
                }
            }
            _ = tokio::time::sleep(Duration::from_millis(80)) => {}
        }
    }
    app.persist_transcript();
    app.bench.dirty = true;
    app.persist_agents();
    app.inbox_tx = None;
    Ok(())
}

fn flush_all(app: &mut App) {
    flush_queue(app);
    let ids: Vec<String> = app.parked.keys().cloned().collect();
    for id in ids {
        if !app.parked.get(&id).is_some_and(|p| {
            p.awaiting && !p.queue.is_empty() && p.queue_edit.is_none()
        }) {
            continue;
        }
        let mut parked = match app.parked.remove(&id) {
            Some(p) => p,
            None => continue,
        };
        parked = app.with_parked(parked, |app| {
            flush_queue(app);
        });
        app.parked.insert(id, parked);
    }
}

fn queued_to_turn(q: Queued) -> UserTurn {
    UserTurn {
        text: q.text,
        images: q.images.into_iter().map(PathBuf::from).collect(),
    }
}

fn user_row_from_turn(turn: &UserTurn) -> UserMsg {
    UserMsg {
        text: turn.text.clone(),
        images: turn
            .images
            .iter()
            .map(|p| p.to_string_lossy().replace('\\', "/"))
            .collect(),
    }
}

fn flush_queue(app: &mut App) {
    if !app.awaiting || app.queue_edit.is_some() || app.status == "已停止" {
        return;
    }
    let Some(msg) = app.queue.pop_front() else {
        return;
    };
    let turn = queued_to_turn(msg);
    app.push(Row::User(user_row_from_turn(&turn)));
    if let Some(tx) = &app.inbox_tx {
        if tx.send(turn).is_ok() {
            app.running = true;
            app.awaiting = false;
            app.status = "工作中".into();
            app.mark_work_start();
        }
    }
}

fn kick_idle_queue(
    app: &mut App,
    opts: &TuiOptions,
    sink: &Arc<FanoutSink>,
    done_tx: &mpsc::UnboundedSender<(String, crate::agent::RunOutcome)>,
) {
    if app.queue_edit.is_some() || app.running || app.inbox_tx.is_some() {
        return;
    }
    let Some(msg) = app.queue.pop_front() else {
        return;
    };
    start_or_send(app, opts, sink, done_tx, queued_to_turn(msg), true);
}

fn start_or_send(
    app: &mut App,
    opts: &TuiOptions,
    sink: &Arc<FanoutSink>,
    done_tx: &mpsc::UnboundedSender<(String, crate::agent::RunOutcome)>,
    turn: UserTurn,
    echo: bool,
) {
    if echo {
        app.push(Row::User(user_row_from_turn(&turn)));
    }
    snapshot_conn(app);
    if app.conn.route_for(&opts.model).is_openai() {
        if !opts.model.trim().is_empty() {
            app.conn.model = opts.model.clone();
        }
        if !app.conn.base_url.trim().is_empty() && !app.conn.kind.is_openai() {
            app.conn.kind = ProviderKind::Openai;
            let _ = app.conn.save();
        }
        refresh_ready(app);
    }
    if !app.logged_in {
        app.push(Row::Err(
            if app.conn.route_for(&opts.model).is_openai() {
                if app.conn.base_url.trim().is_empty() {
                    ProviderConfig::missing_endpoint_error(
                        if opts.model.trim().is_empty() {
                            app.conn.effective_model()
                        } else {
                            opts.model.as_str()
                        },
                    )
                } else {
                    "自訂 API 未就緒 — 請在設定填端點和模型名".into()
                }
            } else {
                "尚未登入 — 請在設定中登入 Grok 帳號，或改連自訂 API".into()
            },
        ));
        return;
    }
    app.mark_work_start();
    if let Some(tx) = &app.inbox_tx {
        let _ = tx.send(turn);
        app.running = true;
        app.awaiting = false;
        app.status = "工作中".into();
        app.persist_transcript();
        return;
    }
    if !app.session.named {
        let fallback = if task::is_kick(&turn.text) {
            "任務模式".into()
        } else {
            session::title_fallback_from_user_text(&turn.text)
        };
        if let Some(store) = &app.store {
            let _ = store.touch_name(&mut app.session, fallback, false);
        } else {
            app.session.name = fallback;
            app.session.named = true;
        }
        spawn_title(
            sink,
            &app.session.id,
            app.conn.clone(),
            &opts.model,
            &turn.text,
        );
    }
    app.session.updated_at = chrono::Utc::now();
    if let Some(store) = &app.store {
        let _ = store.save_meta(&app.session);
    }
    app.persist_transcript();
    app.running = true;
    app.awaiting = false;
    app.status = "工作中".into();
    let (inbox_tx, inbox_rx) = mpsc::unbounded_channel();
    let cancel = CancelFlag::new();
    app.inbox_tx = Some(inbox_tx);
    app.cancel = Some(cancel.clone());
    let mut opts = opts.clone();
    opts.workspace = app.session.workspace.clone();
    if app.conn.route_for(&opts.model).is_openai() {
        opts.web_search = false;
    }
    let cfg = app.conn.clone();
    let sink = sink.clone();
    let knobs = app.knobs.clone();
    let skills = app.skills.clone();
    let run_id = app.session.id.clone();
    let done_tx = done_tx.clone();
    let ask = Some(app.attach_ask_hub(&run_id));
    let task = app.task.clone();
    tokio::spawn(async move {
        let sid = run_id.clone();
        let out = run_one(
            opts, turn, sink, knobs, skills, inbox_rx, run_id, ask, cancel, task, cfg,
        )
        .await;
        let _ = done_tx.send((
            sid,
            out.unwrap_or_else(|e| crate::agent::RunOutcome {
                run_id: String::new(),
                text: e.to_string(),
                turns: 0,
                cache_turns: vec![],
                compacted: 0,
            }),
        ));
    });
}

fn spawn_title(
    sink: &Arc<FanoutSink>,
    session_id: &str,
    cfg: ProviderConfig,
    model: &str,
    prompt: &str,
) {
    let sink = sink.clone();
    let session_id = session_id.to_string();
    let model = model.to_string();
    let prompt = prompt.to_string();
    tokio::spawn(async move {
        let Ok(provider) = AnyProvider::connect(&cfg, Some(model)) else {
            return;
        };
        let name = provider.generate_session_title(&prompt).await;
        sink.emit(&AgentEvent::SessionNamed {
            meta: EventMeta {
                ts: chrono::Utc::now(),
                agent_name: "root".into(),
                run_id: session_id,
                parent_run_id: None,
                path: String::new(),
            },
            name,
        });
    });
}

fn submit_current(
    app: &mut App,
    opts: &TuiOptions,
    sink: &Arc<FanoutSink>,
    done_tx: &mpsc::UnboundedSender<(String, crate::agent::RunOutcome)>,
    force_insert: bool,
) {
    let Some(turn) = app.take_turn() else {
        return;
    };
    let mode = if force_insert { SendMode::Insert } else { app.send_mode };
    match submit_kind(app.inbox_tx.is_some(), app.running, mode) {
        Submit::Queue => {
            app.queue.push_back(Queued {
                text: turn.text,
                images: turn
                    .images
                    .iter()
                    .map(|p| p.to_string_lossy().replace('\\', "/"))
                    .collect(),
            });
        }
        Submit::Start | Submit::Insert => {
            start_or_send(app, opts, sink, done_tx, turn, true);
        }
    }
}

fn is_plain_enter(ev: &Event) -> bool {
    matches!(
        ev,
        Event::Key(k)
            if k.kind == KeyEventKind::Press
                && k.code == KeyCode::Enter
                && !k.modifiers.contains(KeyModifiers::CONTROL)
    )
}

fn is_ime_commit_char(ev: &Event) -> bool {
    match ev {
        Event::Key(k) if k.kind == KeyEventKind::Press => {
            matches!(k.code, KeyCode::Char(c) if !c.is_ascii())
                && !k.modifiers.contains(KeyModifiers::CONTROL)
        }
        _ => false,
    }
}

/// IME's first Enter commits CJK into the same event burst. That Enter must
/// not send the message; a later Enter does.
fn coalesce_ime_enter(events: Vec<Event>) -> Vec<Event> {
    let drop_enter = events.iter().any(is_ime_commit_char) && events.iter().any(is_plain_enter);
    if drop_enter {
        events.into_iter().filter(|e| !is_plain_enter(e)).collect()
    } else {
        events
    }
}

fn handle_ws_key(app: &mut App, code: KeyCode, mods: KeyModifiers) -> bool {
    if is_paste_key(code, mods) {
        if let Some(s) = clipboard_get() {
            if let Some(p) = app.workspace_pick.as_mut() {
                p.edit.insert_str(&s);
            }
            app.sync_workspace_pick();
        }
        return false;
    }
    let shift = mods.contains(KeyModifiers::SHIFT);
    match code {
        KeyCode::Esc => app.cancel_workspace_pick(),
        KeyCode::Tab => {
            if let Some(p) = app.workspace_pick.as_mut() {
                p.focus = match p.focus {
                    WsFocus::Path => WsFocus::List,
                    WsFocus::List => WsFocus::Path,
                };
            }
        }
        KeyCode::Enter => handle_ws_enter(app),
        KeyCode::Up => ws_move_cursor(app, -1),
        KeyCode::Down => ws_move_cursor(app, 1),
        KeyCode::PageUp => ws_move_cursor(app, -8),
        KeyCode::PageDown => ws_move_cursor(app, 8),
        KeyCode::Left if app.workspace_pick.as_ref().is_some_and(|p| p.focus == WsFocus::Path) => {
            if let Some(p) = app.workspace_pick.as_mut() {
                p.edit.move_left(shift);
            }
        }
        KeyCode::Right if app.workspace_pick.as_ref().is_some_and(|p| p.focus == WsFocus::Path) => {
            if let Some(p) = app.workspace_pick.as_mut() {
                p.edit.move_right(shift);
            }
        }
        KeyCode::Home if app.workspace_pick.as_ref().is_some_and(|p| p.focus == WsFocus::Path) => {
            if let Some(p) = app.workspace_pick.as_mut() {
                p.edit.home(shift);
            }
        }
        KeyCode::End if app.workspace_pick.as_ref().is_some_and(|p| p.focus == WsFocus::Path) => {
            if let Some(p) = app.workspace_pick.as_mut() {
                p.edit.end(shift);
            }
        }
        KeyCode::Backspace => {
            if let Some(p) = app.workspace_pick.as_mut() {
                p.focus = WsFocus::Path;
                p.edit.backspace();
            }
            app.sync_workspace_pick();
        }
        KeyCode::Delete => {
            if let Some(p) = app.workspace_pick.as_mut() {
                p.focus = WsFocus::Path;
                p.edit.delete_forward();
            }
            app.sync_workspace_pick();
        }
        KeyCode::Char('a') if mods.contains(KeyModifiers::CONTROL) => {
            if let Some(p) = app.workspace_pick.as_mut() {
                p.focus = WsFocus::Path;
                p.edit.select_all();
            }
        }
        KeyCode::Char(c) if !mods.contains(KeyModifiers::CONTROL) && !c.is_control() => {
            if let Some(p) = app.workspace_pick.as_mut() {
                p.focus = WsFocus::Path;
                p.edit.insert_char(c);
            }
            app.sync_workspace_pick();
        }
        _ => {}
    }
    false
}

fn ws_move_cursor(app: &mut App, delta: i32) {
    let Some(p) = app.workspace_pick.as_mut() else {
        return;
    };
    p.focus = WsFocus::List;
    let n = p.view.entries.len();
    if n == 0 {
        p.cursor = 0;
        return;
    }
    let next = p.cursor as i32 + delta;
    p.cursor = next.clamp(0, n as i32 - 1) as usize;
}

fn handle_ws_enter(app: &mut App) {
    let Some(p) = app.workspace_pick.as_ref() else {
        return;
    };
    if p.focus == WsFocus::Path {
        let typed = p.edit.text.trim().to_string();
        let path = std::path::PathBuf::from(&typed);
        if path.is_dir() {
            let cur = folderpick::normalize(&path);
            if cur == p.view.cwd {
                app.confirm_workspace_pick();
            } else {
                app.enter_workspace_dir(path);
            }
            return;
        }
        let dirs: Vec<_> = p
            .view
            .entries
            .iter()
            .filter(|e| e.is_dir && !e.is_parent)
            .cloned()
            .collect();
        if dirs.len() == 1 {
            app.enter_workspace_dir(dirs[0].path.clone());
            return;
        }
        app.confirm_workspace_pick();
        return;
    }
    let Some(ent) = p.selected().cloned() else {
        app.confirm_workspace_pick();
        return;
    };
    if ent.is_dir {
        app.enter_workspace_dir(ent.path);
    } else {
        app.confirm_workspace_pick();
    }
}

fn handle_key(
    app: &mut App,
    opts: &mut TuiOptions,
    code: KeyCode,
    mods: KeyModifiers,
    sink: &Arc<FanoutSink>,
    done_tx: &mpsc::UnboundedSender<(String, crate::agent::RunOutcome)>,
) -> bool {
    if matches!(code, KeyCode::Char('q') | KeyCode::Char('Q'))
        && mods.contains(KeyModifiers::CONTROL)
    {
        app.cancel_ask();
        return true;
    }
    if matches!(code, KeyCode::Char('c')) && mods.contains(KeyModifiers::CONTROL) {
        if app.with_active_view(|app| app.copy_selection()) {
            return false;
        }
        app.cancel_ask();
        return true;
    }
    if app.workspace_pick.is_some() {
        return handle_ws_key(app, code, mods);
    }
    if matches!(code, KeyCode::Char('n') | KeyCode::Char('N')) && mods.contains(KeyModifiers::CONTROL)
    {
        app.cancel_rename();
        app.new_chat();
        return false;
    }
    if app.ask.is_some() {
        return handle_ask_key(app, code, mods);
    }
    if app.task_ui.is_some() {
        return handle_task_key(app, opts, code, mods, sink, done_tx);
    }
    if app.rename.is_some() {
        return handle_rename_key(app, code, mods);
    }
    if matches!(code, KeyCode::F(2))
        || (matches!(code, KeyCode::Char('g')) && mods.contains(KeyModifiers::CONTROL))
    {
        if app.settings.as_ref().is_some_and(|w| !w.minimized) && app.focus == Focus::Settings {
            app.close_skill_view();
            app.settings = None;
            app.focus = Focus::Chat;
        } else {
            open_settings(app);
        }
        return false;
    }

    if app.skill_view.is_some() {
        return handle_skill_view_key(app, code, mods);
    }

    if app.focus == Focus::Settings {
        return handle_settings_key(app, opts, code, mods);
    }

    if is_paste_key(code, mods) {
        app.paste_clipboard();
        return false;
    }

    if handle_workbench_key(app, code, mods) {
        return false;
    }

    match (code, mods) {
        (KeyCode::Esc, _) => {
            if app.image_view.is_some() {
                app.close_image_view();
            } else if app.inspector.is_some() {
                app.close_inspector();
            } else if app.queue_edit.is_some() {
                app.cancel_queue_edit();
            } else if app.running {
                app.interrupt_work();
            } else if !app.dismiss_tool_ui() {
                if app.edit.has_sel() {
                    app.edit.clear_sel();
                } else if app.chat_sel != ChatSel::None {
                    app.chat_sel = ChatSel::None;
                } else if !app.interrupt_children() {
                    app.settings = None;
                    app.focus = Focus::Chat;
                }
            }
        }
        (KeyCode::Char('a'), m) if m.contains(KeyModifiers::CONTROL) => {
            app.edit.select_all();
        }
        (KeyCode::Char('c' | 'C'), m)
            if !m.contains(KeyModifiers::CONTROL) && app.edit.has_sel() =>
        {
            if let Some(s) = app.edit.selected_text() {
                if clipboard_set(&s) {
                    app.status = "已複製".into();
                } else {
                    app.status = "無法複製到剪貼簿".into();
                }
            }
        }
        (KeyCode::Left, m) => {
            app.edit.move_left(m.contains(KeyModifiers::SHIFT));
        }
        (KeyCode::Right, m) => {
            app.edit.move_right(m.contains(KeyModifiers::SHIFT));
        }
        (KeyCode::Home, m) => {
            app.edit.home(m.contains(KeyModifiers::SHIFT));
        }
        (KeyCode::End, m) => {
            app.edit.end(m.contains(KeyModifiers::SHIFT));
        }
        (KeyCode::Up, m) => {
            if app.focus == Focus::Inspector {
                app.inspector_scroll = app.inspector_scroll.saturating_add(1);
            } else if app.edit.is_empty() && !m.contains(KeyModifiers::SHIFT) {
                app.stick_bottom = false;
                app.scroll = app.scroll.saturating_add(1);
            } else {
                app.edit.move_visual(
                    app.composer_inner.width.max(1),
                    -1,
                    m.contains(KeyModifiers::SHIFT),
                );
            }
        }
        (KeyCode::Down, m) => {
            if app.focus == Focus::Inspector {
                app.inspector_scroll = app.inspector_scroll.saturating_sub(1);
            } else if app.edit.is_empty() && !m.contains(KeyModifiers::SHIFT) {
                app.scroll = app.scroll.saturating_sub(1);
                if app.scroll == 0 {
                    app.stick_bottom = true;
                }
            } else {
                app.edit.move_visual(
                    app.composer_inner.width.max(1),
                    1,
                    m.contains(KeyModifiers::SHIFT),
                );
            }
        }
        (KeyCode::PageUp, _) => {
            app.with_active_view(|app| scroll_chat(app, page_scroll_step(app) as i32));
        }
        (KeyCode::PageDown, _) => {
            app.with_active_view(|app| scroll_chat(app, -(page_scroll_step(app) as i32)));
        }
        (KeyCode::Enter, m) if m.contains(KeyModifiers::CONTROL) => {
            if app.queue_edit.is_some() {
                app.commit_queue_edit();
            } else {
                submit_current(app, opts, sink, done_tx, true);
            }
        }
        (KeyCode::Enter, m) if m.contains(KeyModifiers::SHIFT) => {
            app.edit.insert_char('\n');
        }
        (KeyCode::Enter, _) => {
            if crate::hostio::ime_composing() {
                // First Enter confirms IME composition; it is not a send.
            } else if app.queue_edit.is_some() {
                app.commit_queue_edit();
            } else {
                submit_current(app, opts, sink, done_tx, false);
            }
        }
        (KeyCode::Backspace, _) => {
            if app.edit.is_empty() && !app.pending.is_empty() {
                app.pending.pop();
            } else {
                app.edit.backspace();
            }
        }
        (KeyCode::Delete, _) => {
            app.edit.delete_forward();
        }
        (KeyCode::Char(c), m) if !m.contains(KeyModifiers::CONTROL) => {
            app.edit.insert_char(c);
        }
        _ => {}
    }
    false
}

fn handle_task_key(
    app: &mut App,
    opts: &TuiOptions,
    code: KeyCode,
    mods: KeyModifiers,
    sink: &Arc<FanoutSink>,
    done_tx: &mpsc::UnboundedSender<(String, crate::agent::RunOutcome)>,
) -> bool {
    let form = matches!(app.task_ui, Some(TaskUi::Form { .. }));
    match code {
        KeyCode::Esc => close_task(app),
        KeyCode::Enter if form && !mods.contains(KeyModifiers::SHIFT) => {
            submit_task_goal(app, opts, sink, done_tx);
        }
        KeyCode::Char('s') if mods.contains(KeyModifiers::CONTROL) && form => {
            submit_task_goal(app, opts, sink, done_tx);
        }
        KeyCode::Enter if form => {
            if let Some(TaskUi::Form { edit }) = &mut app.task_ui {
                if edit.len() < task::MAX_GOAL {
                    edit.insert_char('\n');
                }
            }
        }
        KeyCode::Backspace if form => {
            if let Some(TaskUi::Form { edit }) = &mut app.task_ui {
                edit.backspace();
            }
        }
        KeyCode::Delete if form => {
            if let Some(TaskUi::Form { edit }) = &mut app.task_ui {
                edit.delete_forward();
            }
        }
        KeyCode::Left if form => {
            if let Some(TaskUi::Form { edit }) = &mut app.task_ui {
                edit.move_left(mods.contains(KeyModifiers::SHIFT));
            }
        }
        KeyCode::Right if form => {
            if let Some(TaskUi::Form { edit }) = &mut app.task_ui {
                edit.move_right(mods.contains(KeyModifiers::SHIFT));
            }
        }
        KeyCode::Char(c) if form && !mods.contains(KeyModifiers::CONTROL) => {
            if let Some(TaskUi::Form { edit }) = &mut app.task_ui {
                if edit.len() < task::MAX_GOAL {
                    edit.insert_char(c);
                }
            }
        }
        _ => {}
    }
    false
}

fn handle_ask_key(app: &mut App, code: KeyCode, mods: KeyModifiers) -> bool {
    let filling = app.ask.as_ref().is_some_and(|a| a.filling);
    if filling {
        match code {
            KeyCode::Esc => {
                if let Some(ask) = app.ask.as_mut() {
                    ask.save_fill();
                }
                return false;
            }
            KeyCode::Enter => {
                app.submit_ask();
                return false;
            }
            KeyCode::Up => {
                if let Some(ask) = app.ask.as_mut() {
                    ask.save_fill();
                    ask.move_cursor(-1);
                }
                return false;
            }
            KeyCode::Down => {
                if let Some(ask) = app.ask.as_mut() {
                    ask.save_fill();
                    ask.move_cursor(1);
                }
                return false;
            }
            _ => {}
        }
        let shift = mods.contains(KeyModifiers::SHIFT);
        let Some(ask) = app.ask.as_mut() else {
            return false;
        };
        match code {
            KeyCode::Left => ask.fill_edit.move_left(shift),
            KeyCode::Right => ask.fill_edit.move_right(shift),
            KeyCode::Home => ask.fill_edit.home(shift),
            KeyCode::End => ask.fill_edit.end(shift),
            KeyCode::Backspace => ask.fill_edit.backspace(),
            KeyCode::Delete => ask.fill_edit.delete_forward(),
            KeyCode::Char('a') if mods.contains(KeyModifiers::CONTROL) => ask.fill_edit.select_all(),
            KeyCode::Char('v') if mods.contains(KeyModifiers::CONTROL) => {
                if let Some(s) = clipboard_get() {
                    let remain = ask::MAX_INPUT.saturating_sub(ask.fill_edit.len());
                    let clipped: String = s.chars().take(remain).collect();
                    ask.fill_edit.insert_str(&clipped);
                }
            }
            KeyCode::Char(c) if !mods.contains(KeyModifiers::CONTROL) && !c.is_control() => {
                if ask.fill_edit.len() < ask::MAX_INPUT {
                    ask.fill_edit.insert_char(c);
                }
            }
            _ => {}
        }
        return false;
    }
    match code {
        KeyCode::Esc => app.cancel_ask(),
        KeyCode::Up => {
            if let Some(ask) = app.ask.as_mut() {
                ask.move_cursor(-1);
            }
        }
        KeyCode::Down => {
            if let Some(ask) = app.ask.as_mut() {
                ask.move_cursor(1);
            }
        }
        KeyCode::Char(' ') => {
            let Some(ask) = app.ask.as_mut() else {
                return false;
            };
            let i = ask.cursor;
            let input = ask.question.options.get(i).is_some_and(|o| o.input);
            if input {
                ask.enter_fill();
            } else {
                ask.mark_cursor();
            }
        }
        KeyCode::Enter => {
            let Some(ask) = app.ask.as_ref() else {
                return false;
            };
            let i = ask.cursor;
            let input = ask.question.options.get(i).is_some_and(|o| o.input);
            if input {
                app.activate_ask_option(i, false);
            } else if ask.question.allow_multiple {
                app.submit_ask();
            } else {
                app.activate_ask_option(i, true);
            }
        }
        _ => {}
    }
    false
}

fn handle_rename_key(app: &mut App, code: KeyCode, mods: KeyModifiers) -> bool {
    match code {
        KeyCode::Esc => {
            app.cancel_rename();
            return false;
        }
        KeyCode::Enter => {
            app.commit_rename();
            return false;
        }
        _ => {}
    }
    let shift = mods.contains(KeyModifiers::SHIFT);
    let Some((_, edit)) = app.rename.as_mut() else {
        return false;
    };
    match code {
        KeyCode::Left => edit.move_left(shift),
        KeyCode::Right => edit.move_right(shift),
        KeyCode::Home => edit.home(shift),
        KeyCode::End => edit.end(shift),
        KeyCode::Backspace => edit.backspace(),
        KeyCode::Delete => edit.delete_forward(),
        KeyCode::Char('a') if mods.contains(KeyModifiers::CONTROL) => edit.select_all(),
        KeyCode::Char('v') if mods.contains(KeyModifiers::CONTROL) => {
            if let Some(s) = clipboard_get() {
                edit.insert_str(&s);
            }
        }
        KeyCode::Char(c) if !mods.contains(KeyModifiers::CONTROL) && !c.is_control() => {
            edit.insert_char(c);
        }
        _ => {}
    }
    false
}

fn drop_len(app: &App, opts: &TuiOptions) -> usize {
    match app.drop {
        Some(DropKind::Model) => model_choices(app, opts).len(),
        Some(DropKind::ChildModel) => child_model_choices(app, opts).len(),
        Some(DropKind::Effort) => effort_choices(app, opts).len(),
        None => 0,
    }
}

fn handle_settings_key(
    app: &mut App,
    opts: &mut TuiOptions,
    code: KeyCode,
    mods: KeyModifiers,
) -> bool {
    if app.drop.is_some() {
        match code {
            KeyCode::Esc => {
                app.drop = None;
                return false;
            }
            KeyCode::Up => {
                app.drop_cursor = app.drop_cursor.saturating_sub(1);
                reveal_drop_cursor(app, drop_len(app, opts));
                return false;
            }
            KeyCode::Down => {
                let len = drop_len(app, opts);
                if len > 0 {
                    app.drop_cursor = (app.drop_cursor + 1).min(len - 1);
                }
                reveal_drop_cursor(app, len);
                return false;
            }
            KeyCode::Enter | KeyCode::Char(' ') => {
                select_catalog_pick(app, opts, app.drop_cursor);
                return false;
            }
            KeyCode::Tab => {
                app.drop = None;
            }
            _ => return false,
        }
    }
    let editing = setting_edit_mut(app).is_some();
    if editing {
        let shift = mods.contains(KeyModifiers::SHIFT);
        match code {
            KeyCode::Esc => {
                flush_conn(app, opts);
                if login_in_flight(&app.login_ui) {
                    cancel_login(app);
                } else {
                    app.settings = None;
                    app.focus = Focus::Chat;
                }
                return false;
            }
            KeyCode::Tab => {
                cycle_setting_field(app, opts);
                return false;
            }
            KeyCode::Left => {
                if let Some(e) = setting_edit_mut(app) {
                    e.move_left(shift);
                }
                return false;
            }
            KeyCode::Right => {
                if let Some(e) = setting_edit_mut(app) {
                    e.move_right(shift);
                }
                return false;
            }
            KeyCode::Home => {
                if let Some(e) = setting_edit_mut(app) {
                    e.home(shift);
                }
                return false;
            }
            KeyCode::End => {
                if let Some(e) = setting_edit_mut(app) {
                    e.end(shift);
                }
                return false;
            }
            KeyCode::Backspace => {
                if let Some(e) = setting_edit_mut(app) {
                    e.backspace();
                }
                return false;
            }
            KeyCode::Delete => {
                if let Some(e) = setting_edit_mut(app) {
                    e.delete_forward();
                }
                return false;
            }
            KeyCode::Char('a') if mods.contains(KeyModifiers::CONTROL) => {
                if let Some(e) = setting_edit_mut(app) {
                    e.select_all();
                }
                return false;
            }
            KeyCode::Char('v') | KeyCode::Char('V') if mods.contains(KeyModifiers::CONTROL) => {
                if let Some(s) = clipboard_get() {
                    if let Some(e) = setting_edit_mut(app) {
                        e.insert_str(&s.replace('\n', "").replace('\r', ""));
                    }
                }
                return false;
            }
            KeyCode::Char(c) if !mods.contains(KeyModifiers::CONTROL) && !c.is_control() => {
                if let Some(e) = setting_edit_mut(app) {
                    e.insert_char(c);
                }
                return false;
            }
            _ => return false,
        }
    }
    match code {
        KeyCode::Esc => {
            if login_in_flight(&app.login_ui) {
                cancel_login(app);
            } else if app.skill_view.is_some() {
                app.close_skill_view();
            } else {
                flush_conn(app, opts);
                app.settings = None;
                app.focus = Focus::Chat;
            }
        }
        KeyCode::Tab => {
            cycle_setting_field(app, opts);
        }
        KeyCode::Enter | KeyCode::Char(' ') if app.setting_field == SettingField::Kind => {
            let next = if app.conn.kind.is_openai() {
                ProviderKind::Xai
            } else {
                ProviderKind::Openai
            };
            set_provider_kind(app, opts, next);
        }
        KeyCode::Left if app.setting_field == SettingField::Kind => {
            set_provider_kind(app, opts, ProviderKind::Xai);
        }
        KeyCode::Right if app.setting_field == SettingField::Kind => {
            set_provider_kind(app, opts, ProviderKind::Openai);
        }
        KeyCode::Enter | KeyCode::Char(' ') if app.setting_field == SettingField::Account => {
            activate_account(app);
        }
        KeyCode::Enter | KeyCode::Char(' ') if app.setting_field == SettingField::Search => {
            opts.web_search = !opts.web_search;
            sync_knobs(app, opts);
        }
        KeyCode::Enter | KeyCode::Char(' ') if app.setting_field == SettingField::Dispatcher => {
            opts.dispatcher = !opts.dispatcher;
            sync_knobs(app, opts);
        }
        KeyCode::Enter | KeyCode::Char(' ') if app.setting_field == SettingField::ImportClaude => {
            app.toggle_import_claude();
        }
        KeyCode::Enter | KeyCode::Char(' ') if app.setting_field == SettingField::ImportCodex => {
            app.toggle_import_codex();
        }
        KeyCode::Enter if app.setting_field == SettingField::Skills => {
            app.open_skill_view(app.skill_cursor);
        }
        KeyCode::Char(' ') if app.setting_field == SettingField::Skills => {
            app.toggle_skill(app.skill_cursor);
        }
        KeyCode::Up if app.setting_field == SettingField::Skills => {
            app.skill_cursor = app.skill_cursor.saturating_sub(1);
        }
        KeyCode::Down if app.setting_field == SettingField::Skills => {
            if !app.skill_list.is_empty() {
                app.skill_cursor = (app.skill_cursor + 1).min(app.skill_list.len() - 1);
            }
        }
        KeyCode::Enter | KeyCode::Char(' ') | KeyCode::Down
            if app.setting_field == SettingField::Model && model_picker_available(app) =>
        {
            open_drop(app, opts, DropKind::Model);
        }
        KeyCode::Enter | KeyCode::Char(' ') | KeyCode::Down
            if app.setting_field == SettingField::ChildModel && model_picker_available(app) =>
        {
            open_drop(app, opts, DropKind::ChildModel);
        }
        KeyCode::Enter | KeyCode::Char(' ') | KeyCode::Down
            if app.setting_field == SettingField::Effort =>
        {
            open_drop(app, opts, DropKind::Effort);
        }
        KeyCode::Left if app.setting_field == SettingField::Effort => {
            if let Some(e) = cycle_effort(
                &effort_choices(app, opts),
                opts.reasoning_effort,
                true,
            ) {
                apply_selected_effort(app, opts, e);
            }
        }
        KeyCode::Right if app.setting_field == SettingField::Effort => {
            if let Some(e) = cycle_effort(
                &effort_choices(app, opts),
                opts.reasoning_effort,
                false,
            ) {
                apply_selected_effort(app, opts, e);
            }
        }
        _ => {}
    }
    false
}
fn handle_skill_view_key(app: &mut App, code: KeyCode, mods: KeyModifiers) -> bool {
    let shift = mods.contains(KeyModifiers::SHIFT);
    match code {
        KeyCode::Esc => app.close_skill_view(),
        KeyCode::Left => {
            if let Some(v) = app.skill_view.as_mut() {
                v.edit.move_left(shift);
            }
        }
        KeyCode::Right => {
            if let Some(v) = app.skill_view.as_mut() {
                v.edit.move_right(shift);
            }
        }
        KeyCode::Home => {
            if let Some(v) = app.skill_view.as_mut() {
                v.edit.home(shift);
            }
        }
        KeyCode::End => {
            if let Some(v) = app.skill_view.as_mut() {
                v.edit.end(shift);
            }
        }
        KeyCode::Up => {
            if let Some(v) = app.skill_view.as_mut() {
                v.edit.move_visual(v.inner.width.max(1), -1, shift);
            }
        }
        KeyCode::Down => {
            if let Some(v) = app.skill_view.as_mut() {
                v.edit.move_visual(v.inner.width.max(1), 1, shift);
            }
        }
        KeyCode::PageUp => {
            if let Some(v) = app.skill_view.as_mut() {
                v.scroll = v.scroll.saturating_sub(v.inner.height.max(1));
            }
        }
        KeyCode::PageDown => {
            if let Some(v) = app.skill_view.as_mut() {
                v.scroll = v.scroll.saturating_add(v.inner.height.max(1));
            }
        }
        KeyCode::Char('a') if mods.contains(KeyModifiers::CONTROL) => {
            if let Some(v) = app.skill_view.as_mut() {
                v.edit.select_all();
            }
        }
        _ => {}
    }
    false
}

fn handle_mouse(
    app: &mut App,
    opts: &mut TuiOptions,
    kind: MouseEventKind,
    col: u16,
    row: u16,
    mods: KeyModifiers,
) {
    let shift = mods.contains(KeyModifiers::SHIFT);
    match kind {
        MouseEventKind::Down(MouseButton::Left) => {
            app.drag = None;
            app.input_dragging = false;
            app.chat_dragging = false;
            app.scroll_grab = None;
            if let Some(v) = app.skill_view.as_mut() {
                v.dragging = false;
            }
            let hit = hit_at(&app.hits, col, row);
            if app.workspace_pick.is_some() {
                match hit {
                    Some(Hit::WsEntry(i)) => app.activate_ws_entry(i as usize),
                    Some(Hit::WsPath) => {
                        let inner = app
                            .workspace_pick
                            .as_ref()
                            .map(|p| p.path_inner)
                            .unwrap_or_default();
                        if let Some(p) = app.workspace_pick.as_mut() {
                            p.focus = WsFocus::Path;
                            let idx = click_to_index(&p.edit.text, inner, p.path_scroll, col, row);
                            p.edit.click(idx, shift);
                        }
                    }
                    Some(Hit::WsConfirm) => app.confirm_workspace_pick(),
                    Some(Hit::WsCreate) => app.create_workspace_dir(),
                    Some(Hit::WsCancel) => app.cancel_workspace_pick(),
                    Some(Hit::WsPanel) => {}
                    _ => {}
                }
                return;
            }
            if app.ask.is_some() {
                match hit {
                    Some(Hit::AskOption(i)) => {
                        app.activate_ask_option(i as usize, true);
                    }
                    Some(Hit::AskConfirm) => app.submit_ask(),
                    Some(Hit::AskCancel) => app.cancel_ask(),
                    Some(Hit::AskFill) => {
                        let inner = app.ask_fill_inner;
                        if let Some(ask) = app.ask.as_mut() {
                            if !ask.filling {
                                ask.enter_fill();
                            }
                            let idx = click_to_index(
                                &ask.fill_edit.text,
                                inner,
                                ask.fill_scroll,
                                col,
                                row,
                            );
                            ask.fill_edit.click(idx, shift);
                        }
                    }
                    Some(Hit::AskPanel) => {}
                    _ => {}
                }
                return;
            }
            if app.task_ui.is_some() {
                match hit {
                    Some(Hit::TaskConfirm) => app.task_action = Some(TaskAction::Submit),
                    Some(Hit::TaskCancel) => app.task_action = Some(TaskAction::Close),
                    Some(Hit::TaskEnd) => app.task_action = Some(TaskAction::End),
                    Some(Hit::TaskDraft) => {
                        let inner = app.task_draft_inner;
                        if let Some(TaskUi::Form { edit }) = &mut app.task_ui {
                            let idx = click_to_index(&edit.text, inner, 0, col, row);
                            edit.click(idx, shift);
                        }
                    }
                    Some(Hit::TaskPanel) | Some(Hit::TaskChip) => {}
                    _ => {}
                }
                return;
            }
            if app.image_view.is_some() {
                match hit {
                    Some(Hit::ImageViewClose) | Some(Hit::ImageViewDismiss) => {
                        app.close_image_view();
                    }
                    Some(Hit::ImageView) => {}
                    _ => app.close_image_view(),
                }
                return;
            }
            if app.skill_view.is_some() {
                match hit {
                    Some(Hit::SkillViewClose) | Some(Hit::SkillViewDismiss) => {
                        app.close_skill_view();
                    }
                    Some(Hit::SkillView) => {
                        if let Some(v) = app.skill_view.as_mut() {
                            let idx = click_to_index(&v.edit.text, v.inner, v.scroll, col, row);
                            v.edit.click(idx, shift);
                            v.dragging = true;
                        }
                    }
                    _ => app.close_skill_view(),
                }
                return;
            }
            if hit.is_some_and(is_chat_hit) {
                let hit = hit.unwrap();
                app.commit_rename();
                app.with_active_view(|app| handle_chat_click(app, hit, col, row, shift));
                return;
            }
            if let Some(hit) = hit {
                if handle_workbench_click(app, hit) {
                    return;
                }
            }
            match hit {
                Some(Hit::TaskChip) => open_task(app),
                Some(Hit::Gear) | Some(Hit::ModelChip) => open_settings(app),
                Some(Hit::QueueChip) => {
                    app.send_mode = SendMode::Queue;
                    app.focus = Focus::Chat;
                }
                Some(Hit::InsertChip) => {
                    app.send_mode = SendMode::Insert;
                    app.focus = Focus::Chat;
                }
                Some(Hit::PasteImage) => {
                    app.focus = Focus::Chat;
                    app.paste_image();
                }
                Some(Hit::QueueItem(i)) => {
                    app.commit_rename();
                    app.focus = Focus::Chat;
                    app.begin_queue_edit(i as usize);
                }
                Some(Hit::CancelQueueEdit) => {
                    app.cancel_queue_edit();
                    app.focus = Focus::Chat;
                }
                Some(Hit::RailMon(i)) => {
                    if let Some(m) = app.monitors.get(i as usize) {
                        app.inspector = Some(Inspector::Monitor(m.name.clone()));
                        app.inspector_scroll = 0;
                        app.focus = Focus::Inspector;
                    }
                }
                Some(Hit::RailBg(i)) => {
                    if let Some(b) = app.backgrounds.get(i as usize) {
                        app.bench.output = Some(b.name.clone());
                        app.bottom = Some(BottomTab::Output);
                        app.bottom_scroll = 0;
                        app.focus = Focus::Chat;
                    }
                }
                Some(Hit::InspectorClose) => {
                    app.close_inspector();
                }
                Some(Hit::Inspector) => {
                    app.focus = Focus::Inspector;
                }
                Some(Hit::PendingClose(i)) => {
                    let i = i as usize;
                    if i < app.pending.len() {
                        app.pending.remove(i);
                    }
                    app.focus = Focus::Chat;
                }
                Some(Hit::Composer) => {
                    app.commit_rename();
                    app.focus = Focus::Chat;
                    app.chat_sel = ChatSel::None;
                    let idx = click_to_index(
                        &app.edit.text,
                        app.composer_inner,
                        app.composer_vscroll,
                        col,
                        row,
                    );
                    app.edit.click(idx, shift);
                    app.input_dragging = true;
                }
                Some(Hit::NewChat) => {
                    app.cancel_rename();
                    app.focus = Focus::Chat;
                    app.new_chat();
                }
                Some(Hit::RenameSession(i)) => {
                    if let Some(id) = app.sidebar_ids.get(i as usize).cloned() {
                        app.begin_rename(&id);
                    }
                }
                Some(Hit::DeleteSession(i)) => {
                    if let Some(id) = app.sidebar_ids.get(i as usize).cloned() {
                        app.delete_session(&id);
                    }
                }
                Some(Hit::Session(i)) => {
                    app.focus = Focus::Chat;
                    if app.area.width < SIDEBAR_MIN_TERM {
                        app.side_open = false;
                    }
                    if let Some(id) = app.sidebar_ids.get(i as usize).cloned() {
                        if app.rename.as_ref().is_some_and(|(rid, _)| rid == &id) {
                            // stay in rename
                        } else {
                            app.commit_rename();
                            app.switch_to(&id);
                        }
                    }
                }
                Some(Hit::Dock) => open_settings(app),
                Some(Hit::Close) => {
                    flush_conn(app, opts);
                    app.drop = None;
                    app.settings = None;
                    app.focus = Focus::Chat;
                }
                Some(Hit::Min) => {
                    if let Some(w) = app.settings.as_mut() {
                        w.minimized = true;
                    }
                    app.focus = Focus::Chat;
                }
                Some(Hit::Max) => {
                    if let Some(w) = app.settings.as_mut() {
                        w.maximized = !w.maximized;
                    }
                    app.focus = Focus::Settings;
                }
                Some(Hit::Title) => {
                    app.focus = Focus::Settings;
                    if let Some(w) = app.settings.as_ref() {
                        app.drag = Some((col as i16 - w.x as i16, row as i16 - w.y as i16));
                    }
                }
                Some(Hit::SettingModel) => {
                    app.focus = Focus::Settings;
                    app.setting_field = SettingField::Model;
                    if model_picker_available(app) {
                        if app.drop == Some(DropKind::Model) {
                            app.drop = None;
                        } else {
                            open_drop(app, opts, DropKind::Model);
                        }
                    }
                }
                Some(Hit::SettingChildModel) => {
                    app.focus = Focus::Settings;
                    app.setting_field = SettingField::ChildModel;
                    if model_picker_available(app) {
                        if app.drop == Some(DropKind::ChildModel) {
                            app.drop = None;
                        } else {
                            open_drop(app, opts, DropKind::ChildModel);
                        }
                    }
                }
                Some(Hit::SettingEndpoint) => {
                    app.focus = Focus::Settings;
                    app.setting_field = SettingField::Endpoint;
                    app.drop = None;
                }
                Some(Hit::SettingApiKey) => {
                    app.focus = Focus::Settings;
                    app.setting_field = SettingField::ApiKey;
                    app.drop = None;
                }
                Some(Hit::SettingContext) => {
                    app.focus = Focus::Settings;
                    app.setting_field = SettingField::Context;
                    app.drop = None;
                }
                Some(Hit::ProviderXai) => {
                    app.focus = Focus::Settings;
                    app.setting_field = SettingField::Kind;
                    set_provider_kind(app, opts, ProviderKind::Xai);
                }
                Some(Hit::ProviderOpenai) => {
                    app.focus = Focus::Settings;
                    app.setting_field = SettingField::Kind;
                    set_provider_kind(app, opts, ProviderKind::Openai);
                }
                Some(Hit::SettingEffort) => {
                    app.focus = Focus::Settings;
                    app.setting_field = SettingField::Effort;
                    if app.drop == Some(DropKind::Effort) {
                        app.drop = None;
                    } else {
                        open_drop(app, opts, DropKind::Effort);
                    }
                }
                Some(Hit::CatalogPick(i)) => {
                    app.focus = Focus::Settings;
                    select_catalog_pick(app, opts, i as usize);
                }
                Some(Hit::Search) => {
                    app.drop = None;
                    app.focus = Focus::Settings;
                    app.setting_field = SettingField::Search;
                    opts.web_search = !opts.web_search;
                    sync_knobs(app, opts);
                }
                Some(Hit::Dispatcher) => {
                    app.drop = None;
                    app.focus = Focus::Settings;
                    app.setting_field = SettingField::Dispatcher;
                    opts.dispatcher = !opts.dispatcher;
                    sync_knobs(app, opts);
                }
                Some(Hit::ImportClaude) => {
                    app.drop = None;
                    app.focus = Focus::Settings;
                    app.setting_field = SettingField::ImportClaude;
                    app.toggle_import_claude();
                }
                Some(Hit::ImportCodex) => {
                    app.drop = None;
                    app.focus = Focus::Settings;
                    app.setting_field = SettingField::ImportCodex;
                    app.toggle_import_codex();
                }
                Some(Hit::SkillToggle(i)) => {
                    app.drop = None;
                    app.focus = Focus::Settings;
                    app.setting_field = SettingField::Skills;
                    app.skill_cursor = i as usize;
                    app.toggle_skill(i as usize);
                }
                Some(Hit::SkillRow(i)) => {
                    app.drop = None;
                    app.focus = Focus::Settings;
                    app.setting_field = SettingField::Skills;
                    app.skill_cursor = i as usize;
                    app.open_skill_view(i as usize);
                }
                Some(Hit::SkillList) => {
                    app.drop = None;
                    app.focus = Focus::Settings;
                    app.setting_field = SettingField::Skills;
                }
                Some(Hit::AccountBtn) => {
                    app.drop = None;
                    app.focus = Focus::Settings;
                    app.setting_field = SettingField::Account;
                    activate_account(app);
                }
                Some(Hit::LoginCode) => {
                    app.focus = Focus::Settings;
                    app.setting_field = SettingField::Account;
                    if let LoginUi::Waiting { user_code, .. } = &app.login_ui {
                        let code = user_code.clone();
                        if clipboard_set(&code) {
                            app.status = "已複製登入代碼".into();
                        } else {
                            app.status = "無法複製登入代碼".into();
                        }
                    }
                }
                _ => {}
            }
        }
        MouseEventKind::Drag(MouseButton::Left) => {
            if app.scroll_grab.is_some() {
                app.with_active_view(|app| apply_scroll_from_row(app, row));
            } else if let (Some((dx, dy)), Some(w)) = (app.drag, app.settings.as_mut()) {
                if !w.maximized {
                    w.x = (col as i16 - dx).max(0) as u16;
                    w.y = (row as i16 - dy).max(0) as u16;
                }
            } else if app.skill_view.as_ref().is_some_and(|v| v.dragging) {
                if let Some(v) = app.skill_view.as_mut() {
                    let idx = click_to_index(&v.edit.text, v.inner, v.scroll, col, row);
                    v.edit.click(idx, true);
                }
            } else if app.chat_dragging {
                if let Some(pos) = chat_pos_at(&app.chat_glyphs, col, row) {
                    app.with_active_view(|app| {
                        if let ChatSel::Text { anchor, .. } = app.chat_sel {
                            app.chat_sel = ChatSel::Text { anchor, caret: pos };
                        }
                    });
                }
            } else if app.input_dragging {
                let idx = click_to_index(
                    &app.edit.text,
                    app.composer_inner,
                    app.composer_vscroll,
                    col,
                    row,
                );
                app.edit.click(idx, true);
            }
        }
        MouseEventKind::Up(_) => {
            app.drag = None;
            app.input_dragging = false;
            app.chat_dragging = false;
            app.scroll_grab = None;
            if let Some(v) = app.skill_view.as_mut() {
                v.dragging = false;
            }
        }
        MouseEventKind::ScrollUp => {
            if wheel_workbench(app, col, row, -3) {
                return;
            }
            if app.image_view.is_some() {
                return;
            }
            if let Some(v) = app.skill_view.as_mut() {
                v.scroll = v.scroll.saturating_sub(3);
                return;
            }
            if matches!(
                hit_at(&app.hits, col, row),
                Some(Hit::SkillList | Hit::SkillRow(_) | Hit::SkillToggle(_))
            ) {
                app.skill_cursor = app.skill_cursor.saturating_sub(1);
                return;
            }
            if app.workspace_pick.is_some() {
                ws_move_cursor(app, -1);
            } else if app.drop.is_some() && app.focus == Focus::Settings {
                app.drop_cursor = app.drop_cursor.saturating_sub(1);
                reveal_drop_cursor(app, drop_len(app, opts));
            } else if app.focus == Focus::Inspector {
                app.inspector_scroll = app.inspector_scroll.saturating_add(1);
            } else {
                app.with_active_view(|app| scroll_chat(app, chat_scroll_step(app) as i32));
            }
        }
        MouseEventKind::ScrollDown => {
            if wheel_workbench(app, col, row, 3) {
                return;
            }
            if app.image_view.is_some() {
                return;
            }
            if let Some(v) = app.skill_view.as_mut() {
                v.scroll = v.scroll.saturating_add(3);
                return;
            }
            if matches!(
                hit_at(&app.hits, col, row),
                Some(Hit::SkillList | Hit::SkillRow(_) | Hit::SkillToggle(_))
            ) {
                if !app.skill_list.is_empty() {
                    app.skill_cursor = (app.skill_cursor + 1).min(app.skill_list.len() - 1);
                }
                return;
            }
            if app.workspace_pick.is_some() {
                ws_move_cursor(app, 1);
            } else if app.drop.is_some() && app.focus == Focus::Settings {
                let len = drop_len(app, opts);
                if len > 0 {
                    app.drop_cursor = (app.drop_cursor + 1).min(len - 1);
                }
                reveal_drop_cursor(app, len);
            } else if app.focus == Focus::Inspector {
                app.inspector_scroll = app.inspector_scroll.saturating_sub(1);
            } else {
                app.with_active_view(|app| scroll_chat(app, -(chat_scroll_step(app) as i32)));
            }
        }
        _ => {}
    }
}

fn handle_input(
    app: &mut App,
    opts: &mut TuiOptions,
    ev: Event,
    sink: &Arc<FanoutSink>,
    done_tx: &mpsc::UnboundedSender<(String, crate::agent::RunOutcome)>,
) -> bool {
    match ev {
        Event::Key(key) => {
            if key.kind != KeyEventKind::Press {
                return false;
            }
            handle_key(app, opts, key.code, key.modifiers, sink, done_tx)
        }
        Event::Mouse(m) => {
            handle_mouse(app, opts, m.kind, m.column, m.row, m.modifiers);
            false
        }
        Event::Paste(s) => {
            if app.skill_view.is_some() {
                return false;
            }
            if app.workspace_pick.is_some() {
                if let Some(p) = app.workspace_pick.as_mut() {
                    p.edit.insert_str(&s);
                }
                app.sync_workspace_pick();
            } else if app.ask.as_ref().is_some_and(|a| a.filling) {
                if let Some(ask) = app.ask.as_mut() {
                    let remain = ask::MAX_INPUT.saturating_sub(ask.fill_edit.len());
                    let clipped: String = s.chars().take(remain).collect();
                    ask.fill_edit.insert_str(&clipped);
                }
            } else if app.focus == Focus::Settings && setting_edit_mut(app).is_some() {
                let clipped: String = s.replace('\n', "").replace('\r', "");
                if let Some(e) = setting_edit_mut(app) {
                    e.insert_str(&clipped);
                }
            } else if let Some(TaskUi::Form { edit }) = &mut app.task_ui {
                let remain = task::MAX_GOAL.saturating_sub(edit.len());
                let clipped: String = s.chars().take(remain).collect();
                edit.insert_str(&clipped);
            } else {
                app.paste_from_terminal(&s);
            }
            false
        }
        Event::Resize(_, _) => false,
        _ => false,
    }
}

async fn run_one(
    opts: TuiOptions,
    turn: UserTurn,
    sink: Arc<FanoutSink>,
    knobs: Arc<Mutex<SessionKnobs>>,
    skills: Arc<Mutex<SkillStore>>,
    inbox: mpsc::UnboundedReceiver<UserTurn>,
    run_id: String,
    ask: Option<AskUserHub>,
    cancel: crate::agent::CancelFlag,
    task: Arc<TaskHub>,
    cfg: ProviderConfig,
) -> Result<crate::agent::RunOutcome> {
    let model = if !opts.model.trim().is_empty() {
        opts.model.clone()
    } else {
        cfg.effective_model().to_string()
    };
    let spec_model = model.clone();
    let provider = AnyProvider::connect(&cfg, Some(model.clone()))?;
    let run_id = if run_id.is_empty() {
        uuid::Uuid::new_v4().to_string()
    } else {
        run_id
    };
    let dyn_sink: Arc<dyn crate::events::EventSink> = sink.clone();
    let server_tools = if cfg.route_for(&model).is_openai() {
        vec![]
    } else {
        kit::search_tools(opts.web_search)
    };
    let child_mode = std::env::var("GROKA_CHILD_MODE").unwrap_or_else(|_| "grok".into());
    crate::kit::run_with_nursery(
        &provider,
        dyn_sink,
        crate::kit::KernelSpec {
            agent_name: "root".into(),
            prompt: turn.text,
            images: turn.images,
            model,
            max_turns: opts.max_turns,
            workspace: opts.workspace,
            events_file: opts.events.clone(),
            events_dir: crate::kit::events_dir(&opts.events),
            server_tools,
            depth: 0,
            parent_run_id: None,
            run_id,
            child_mode,
            reasoning_effort: opts.reasoning_effort,
            knobs: Some(knobs),
            inbox: Some(inbox),
            ask,
            context_window: cfg.window_tokens_for(&spec_model),
            cancel: Some(cancel),
            skills: Some(skills),
            task: Some(task),
        },
    )
    .await
}
