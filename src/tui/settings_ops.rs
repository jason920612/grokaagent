fn sync_knobs(app: &App, opts: &TuiOptions) {
    let model = if opts.model.trim().is_empty() {
        app.catalog
            .models
            .first()
            .map(|m| m.id.clone())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "grok-4.6".into())
    } else {
        opts.model.clone()
    };
    // Search and reasoning passthrough only exist on the xAI side; follow the
    // model's actual route, not the settings panel.
    let openai = app.conn.route_for(&model).is_openai();
    let choice = clamp_effort_for_model(&app.catalog, &model, opts.reasoning_effort);
    if let Ok(mut k) = app.knobs.lock() {
        k.model = model;
        k.reasoning_effort = choice.effort;
        k.send_reasoning = choice.send_reasoning && !openai;
        k.server_tools = if openai {
            Vec::new()
        } else {
            kit::search_tools(opts.web_search)
        };
        k.dispatcher = opts.dispatcher;
        k.child_model = opts.child_model.trim().to_string();
    }
}

fn model_choices(app: &App, opts: &TuiOptions) -> Vec<(String, String)> {
    if app.catalog.models.is_empty() {
        let id = opts.model.trim();
        if id.is_empty() {
            return Vec::new();
        }
        return vec![(id.to_string(), id.to_string())];
    }
    app.catalog
        .models
        .iter()
        .map(|m| (m.id.clone(), m.name.clone()))
        .collect()
}

fn effort_choices(app: &App, opts: &TuiOptions) -> Vec<EffortOpt> {
    let listed = app.catalog.picker_efforts(&opts.model);
    if listed.is_empty() && app.catalog.find(&opts.model).is_none() {
        return vec![EffortOpt {
            id: opts.reasoning_effort.as_str().to_string(),
            value: opts.reasoning_effort,
            label: opts.reasoning_effort.label().to_string(),
            default: true,
        }];
    }
    listed
}

/// Dropdown rows for the child-agent default model: follow-main plus catalog.
fn child_model_choices(app: &App, opts: &TuiOptions) -> Vec<(String, String)> {
    let mut out = vec![(String::new(), "跟隨主模型".to_string())];
    out.extend(model_choices(app, opts));
    out
}

fn child_model_label(app: &App, opts: &TuiOptions) -> String {
    let id = opts.child_model.trim();
    if id.is_empty() {
        return "跟隨主模型".into();
    }
    app.catalog
        .find(id)
        .map(|m| m.name.clone())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| id.to_string())
}

fn apply_selected_child_model(app: &mut App, opts: &mut TuiOptions, model_id: String) {
    opts.child_model = model_id.clone();
    app.child_model_edit = Edit::at_end(model_id);
    sync_knobs(app, opts);
}

fn apply_selected_model(app: &mut App, opts: &mut TuiOptions, model_id: String) {
    opts.model = model_id.clone();
    app.model_edit = Edit::at_end(model_id.clone());
    app.conn.model = model_id;
    let choice = clamp_effort_for_model(&app.catalog, &opts.model, opts.reasoning_effort);
    opts.reasoning_effort = choice.effort;
    // Routing is per model now: picking a custom model from the Grok panel (or
    // a Grok model from the custom panel) must not flip the panel.
    let _ = app.conn.save();
    rebuild_catalog(app, opts);
    refresh_ready(app);
    sync_knobs(app, opts);
}

fn apply_selected_effort(app: &App, opts: &mut TuiOptions, effort: ReasoningEffort) {
    opts.reasoning_effort = effort;
    sync_knobs(app, opts);
}

fn refresh_ready(app: &mut App) {
    let xai = auth::load_tokens(&app.auth_path).is_ok();
    app.xai_ready = xai;
    app.logged_in = app.conn.ready(xai);
}

fn edits_from_conn(app: &mut App, opts: &TuiOptions) {
    app.endpoint_edit = Edit::at_end(app.conn.base_url.clone());
    app.api_key_edit = Edit::at_end(app.conn.api_key.clone());
    let model = if app.conn.kind.is_openai() && !app.conn.model.trim().is_empty() {
        app.conn.model.clone()
    } else {
        opts.model.clone()
    };
    app.model_edit = Edit::at_end(model);
    app.context_edit = Edit::at_end(if app.conn.context_window > 0 {
        crate::compact::format_window(app.conn.context_window)
    } else {
        String::new()
    });
}

fn snapshot_conn(app: &mut App) {
    app.conn.base_url = app.endpoint_edit.text.trim().to_string();
    app.conn.api_key = app.api_key_edit.text.trim().to_string();
    if app.conn.kind.is_openai() {
        let model = app.model_edit.text.trim();
        if !model.is_empty() {
            app.conn.model = model.to_string();
        }
        app.conn.context_window =
            crate::compact::parse_window(&app.context_edit.text).unwrap_or(0);
    }
    let _ = app.conn.save();
    refresh_ready(app);
}

fn flush_conn(app: &mut App, opts: &mut TuiOptions) {
    snapshot_conn(app);
    if app.conn.kind.is_openai() {
        if !app.conn.model.trim().is_empty() {
            opts.model = app.conn.model.clone();
        }
        if app.custom_catalog.models.is_empty() {
            // Dropdown-driven when the endpoint listed its models; the edit
            // box only holds the value while hand-typing is the fallback.
            opts.child_model = app.child_model_edit.text.trim().to_string();
        }
        if app.conn.route_for(&opts.model).is_openai() {
            opts.web_search = false;
        }
        rebuild_catalog(app, opts);
    } else {
        app.conn.model = opts.model.clone();
        let _ = app.conn.save();
    }
    sync_knobs(app, opts);
}

/// The model fields open a dropdown on the Grok panel always, and on the
/// custom panel once the endpoint's model list was fetched. Without a list the
/// custom panel keeps the hand-typed edit boxes.
fn model_picker_available(app: &App) -> bool {
    !app.conn.kind.is_openai() || !app.custom_catalog.models.is_empty()
}

/// Rebuild the merged picker catalog: Grok models first, then the custom
/// endpoint's models (tagged so the source is obvious), then whatever the
/// user typed by hand so the current selection is always listed.
fn rebuild_catalog(app: &mut App, opts: &TuiOptions) {
    let mut cat = app.grok_catalog.clone();
    for m in &app.custom_catalog.models {
        if cat.find(&m.id).is_some() {
            continue;
        }
        let mut m = m.clone();
        let base = if m.name.trim().is_empty() {
            m.id.clone()
        } else {
            m.name.clone()
        };
        m.name = format!("{base}（自訂）");
        cat.models.push(m);
    }
    cat.ensure_current(&opts.model, opts.reasoning_effort);
    let child = opts.child_model.trim();
    if !child.is_empty() {
        cat.ensure_current(child, opts.reasoning_effort);
    }
    app.catalog = cat;
}

fn ingest_custom_catalog(
    app: &mut App,
    opts: &mut TuiOptions,
    result: crate::error::Result<ModelCatalog>,
) {
    app.custom_cat_loading = false;
    match result {
        Ok(cat) => {
            app.custom_cat_err = None;
            app.custom_catalog = cat;
        }
        Err(e) => {
            // Endpoint without /models: fall back to the hand-typed model box.
            app.custom_cat_err = Some(e.to_string().chars().take(80).collect());
            app.custom_catalog = ModelCatalog::default();
        }
    }
    rebuild_catalog(app, opts);
    sync_knobs(app, opts);
}

fn set_provider_kind(app: &mut App, opts: &mut TuiOptions, kind: ProviderKind) {
    flush_conn(app, opts);
    app.conn.kind = kind;
    if kind.is_openai() {
        if app.model_edit.text.trim().is_empty() {
            app.model_edit = Edit::at_end(opts.model.clone());
        }
        if app.conn.model.trim().is_empty() {
            app.conn.model = opts.model.clone();
        }
        if app.conn.route_for(&opts.model).is_openai() {
            opts.web_search = false;
        }
        app.want_catalog = app.xai_ready;
        rebuild_catalog(app, opts);
        app.setting_field = SettingField::Endpoint;
    } else {
        // Back on the Grok panel: default to a Grok model, but the merged
        // dropdown still offers the endpoint's models if the user wants one.
        if !ProviderConfig::looks_like_grok(&opts.model) {
            let fallback = app
                .catalog
                .models
                .iter()
                .map(|m| m.id.as_str())
                .find(|id| ProviderConfig::looks_like_grok(id))
                .unwrap_or("grok-4.6")
                .to_string();
            opts.model = fallback;
        }
        app.conn.model = opts.model.clone();
        app.model_edit = Edit::at_end(opts.model.clone());
        rebuild_catalog(app, opts);
        app.want_catalog = true;
        app.catalog_status = CatalogStatus::Idle;
        app.setting_field = SettingField::Account;
    }
    let _ = app.conn.save();
    refresh_ready(app);
    sync_knobs(app, opts);
}

fn setting_edit_mut(app: &mut App) -> Option<&mut Edit> {
    match app.setting_field {
        SettingField::Endpoint => Some(&mut app.endpoint_edit),
        SettingField::ApiKey => Some(&mut app.api_key_edit),
        SettingField::Model
            if app.conn.kind.is_openai() && app.custom_catalog.models.is_empty() =>
        {
            Some(&mut app.model_edit)
        }
        SettingField::ChildModel
            if app.conn.kind.is_openai() && app.custom_catalog.models.is_empty() =>
        {
            Some(&mut app.child_model_edit)
        }
        SettingField::Context => Some(&mut app.context_edit),
        _ => None,
    }
}

fn cycle_setting_field(app: &mut App, opts: &mut TuiOptions) {
    flush_conn(app, opts);
    let openai = app.conn.kind.is_openai();
    app.setting_field = if openai {
        match app.setting_field {
            SettingField::Kind => SettingField::Endpoint,
            SettingField::Endpoint => SettingField::ApiKey,
            SettingField::ApiKey => SettingField::Model,
            SettingField::Model => SettingField::ChildModel,
            SettingField::ChildModel => SettingField::Context,
            SettingField::Context => SettingField::Dispatcher,
            SettingField::Dispatcher => SettingField::ImportClaude,
            SettingField::ImportClaude => SettingField::ImportCodex,
            SettingField::ImportCodex => SettingField::Skills,
            SettingField::Skills | SettingField::Account | SettingField::Effort | SettingField::Search => {
                SettingField::Kind
            }
        }
    } else {
        match app.setting_field {
            SettingField::Kind => SettingField::Account,
            SettingField::Account => SettingField::Model,
            SettingField::Model => SettingField::ChildModel,
            SettingField::ChildModel => SettingField::Effort,
            SettingField::Effort => SettingField::Search,
            SettingField::Search => SettingField::Dispatcher,
            SettingField::Dispatcher => SettingField::ImportClaude,
            SettingField::ImportClaude => SettingField::ImportCodex,
            SettingField::ImportCodex => SettingField::Skills,
            SettingField::Skills | SettingField::Endpoint | SettingField::ApiKey | SettingField::Context => {
                SettingField::Kind
            }
        }
    };
}

fn open_drop(app: &mut App, opts: &TuiOptions, kind: DropKind) {
    let len = match kind {
        DropKind::Model => model_choices(app, opts).len(),
        DropKind::ChildModel => child_model_choices(app, opts).len(),
        DropKind::Effort => effort_choices(app, opts).len(),
    };
    if len == 0 {
        app.drop = None;
        return;
    }
    app.drop = Some(kind);
    app.drop_cursor = match kind {
        DropKind::Model => model_choices(app, opts)
            .iter()
            .position(|(id, _)| id == &opts.model)
            .unwrap_or(0),
        DropKind::ChildModel => child_model_choices(app, opts)
            .iter()
            .position(|(id, _)| id == opts.child_model.trim())
            .unwrap_or(0),
        DropKind::Effort => effort_choices(app, opts)
            .iter()
            .position(|e| e.value == opts.reasoning_effort)
            .unwrap_or(0),
    };
    app.drop_scroll = 0;
    reveal_drop_cursor(app, len);
}

fn reveal_drop_cursor(app: &mut App, len: usize) {
    let vis = DROP_VISIBLE.min(len.max(1));
    if app.drop_cursor < app.drop_scroll as usize {
        app.drop_scroll = app.drop_cursor as u16;
    } else if app.drop_cursor >= app.drop_scroll as usize + vis {
        app.drop_scroll = (app.drop_cursor + 1 - vis) as u16;
    }
}

fn select_catalog_pick(app: &mut App, opts: &mut TuiOptions, index: usize) {
    match app.drop {
        Some(DropKind::Model) => {
            if let Some((id, _)) = model_choices(app, opts).get(index).cloned() {
                apply_selected_model(app, opts, id);
            }
        }
        Some(DropKind::ChildModel) => {
            if let Some((id, _)) = child_model_choices(app, opts).get(index).cloned() {
                apply_selected_child_model(app, opts, id);
            }
        }
        Some(DropKind::Effort) => {
            if let Some(e) = effort_choices(app, opts).get(index).cloned() {
                apply_selected_effort(app, opts, e.value);
            }
        }
        None => {}
    }
    app.drop = None;
}

fn ingest_catalog(app: &mut App, opts: &mut TuiOptions, result: crate::error::Result<ModelCatalog>) {
    match result {
        Ok(cat) => {
            app.grok_catalog = cat;
            app.catalog_status = CatalogStatus::Ready;
            rebuild_catalog(app, opts);
            let choice = clamp_effort_for_model(&app.catalog, &opts.model, opts.reasoning_effort);
            opts.reasoning_effort = choice.effort;
            sync_knobs(app, opts);
        }
        Err(e) => {
            rebuild_catalog(app, opts);
            let msg = e.to_string();
            app.catalog_status = CatalogStatus::Failed(msg.chars().take(80).collect());
        }
    }
}

fn login_in_flight(ui: &LoginUi) -> bool {
    matches!(ui, LoginUi::Starting | LoginUi::Waiting { .. })
}

fn is_pulsing(app: &App) -> bool {
    app.running
        || app.bench.live_count() > 0
        || app.monitors.iter().any(|m| m.alive)
        || app.backgrounds.iter().any(|b| b.alive)
}

fn pulse_spinner(app: &mut App) {
    if is_pulsing(app) {
        app.tick = app.tick.wrapping_add(1);
    }
}

fn header_pulse_ok(app: &App) -> bool {
    pulse_overlays_clear(app)
        && app.running
        && !rail_needs_pulse(app)
}

/// Side-rail spinners need a full redraw; header clock patch alone is not enough.
fn rail_needs_pulse(app: &App) -> bool {
    app.bench.live_count() > 0
        || app.monitors.iter().any(|m| m.alive)
        || app.backgrounds.iter().any(|b| b.alive)
}

fn pulse_overlays_clear(app: &App) -> bool {
    app.image_view.is_none()
        && app.inspector.is_none()
        && app.workspace_pick.is_none()
        && app.skill_view.is_none()
        && app.ask.is_none()
        && app.task_ui.is_none()
        && app.settings.as_ref().is_none_or(|s| s.minimized)
}

fn side_pulse_ok(app: &App) -> bool {
    pulse_overlays_clear(app) && is_pulsing(app) && rail_needs_pulse(app)
}

/// IME composition is drawn at the hardware cursor, so it must stay inside
/// the composer even while the model is working.
fn want_hardware_cursor(app: &App) -> bool {
    app.workspace_pick.is_some()
        || app.ask.is_some()
        || app.task_ui.is_some()
        || app.skill_view.is_some()
        || app.focus == Focus::Rename
        || (app.focus == Focus::Settings && app.settings.as_ref().is_some_and(|s| !s.minimized))
        || app.queue_edit.is_some()
        || (app.focus == Focus::Chat && !app.viewing_agent())
}

fn begin_login(app: &mut App) {
    if app.logged_in || login_in_flight(&app.login_ui) {
        return;
    }
    if auth::load_tokens(&app.auth_path).is_ok() {
        apply_login_success(app);
        return;
    }
    app.login_gen = app.login_gen.wrapping_add(1);
    app.login_ui = LoginUi::Starting;
    app.want_login = true;
    app.status = "正在開始 Grok 登入…".into();
}

fn cancel_login(app: &mut App) {
    if !login_in_flight(&app.login_ui) && !app.want_login {
        return;
    }
    app.login_gen = app.login_gen.wrapping_add(1);
    app.want_login = false;
    app.login_ui = LoginUi::Idle;
    app.status = "已取消登入".into();
}

fn logout_account(app: &mut App) {
    app.login_gen = app.login_gen.wrapping_add(1);
    app.want_login = false;
    app.login_ui = LoginUi::Idle;
    if let Err(e) = auth::delete_auth_file(&app.auth_path) {
        app.login_ui = LoginUi::Failed(e.to_string());
        return;
    }
    app.logged_in = false;
    refresh_ready(app);
    if !app.xai_ready {
        app.want_catalog = false;
        app.grok_catalog = ModelCatalog::default();
    }
    app.status = "已登出".into();
}

fn apply_login_success(app: &mut App) {
    app.logged_in = true;
    app.xai_ready = true;
    app.login_ui = LoginUi::Idle;
    app.want_login = false;
    app.want_catalog = true;
    app.status = "已登入 Grok".into();
}

fn apply_login_event(app: &mut App, ev: LoginEvent) {
    let gen = match &ev {
        LoginEvent::Waiting { gen, .. }
        | LoginEvent::Success { gen }
        | LoginEvent::Failed { gen, .. } => *gen,
    };
    if gen != app.login_gen {
        return;
    }
    match ev {
        LoginEvent::Waiting { url, user_code, .. } => {
            app.login_ui = LoginUi::Waiting { url, user_code };
            app.status = "請在瀏覽器核准 Grok 登入".into();
        }
        LoginEvent::Success { .. } => {
            apply_login_success(app);
            app.push(Row::Meta("已登入 Grok 帳號".into()));
        }
        LoginEvent::Failed { message, .. } => {
            app.login_ui = LoginUi::Failed(message.chars().take(80).collect());
            app.status = "Grok 登入失敗".into();
        }
    }
}

fn activate_account(app: &mut App) {
    if login_in_flight(&app.login_ui) {
        cancel_login(app);
    } else if app.logged_in {
        logout_account(app);
    } else {
        begin_login(app);
    }
}

async fn run_settings_login(
    path: PathBuf,
    gen: u64,
    tx: mpsc::UnboundedSender<LoginEvent>,
) {
    let mut pending = match auth::request_device_login().await {
        Ok(p) => p,
        Err(e) => {
            let _ = tx.send(LoginEvent::Failed {
                gen,
                message: e.to_string(),
            });
            return;
        }
    };
    auth::open_login_browser(&pending.open_url);
    let _ = tx.send(LoginEvent::Waiting {
        gen,
        url: pending.open_url.clone(),
        user_code: pending.user_code.clone(),
    });
    let deadline = std::time::SystemTime::now() + Duration::from_secs(pending.expires_in);
    loop {
        if std::time::SystemTime::now() >= deadline {
            let _ = tx.send(LoginEvent::Failed {
                gen,
                message: "登入已逾時".into(),
            });
            return;
        }
        tokio::time::sleep(pending.interval()).await;
        match auth::poll_device_login(&pending).await {
            Ok(auth::DevicePoll::Pending) => continue,
            Ok(auth::DevicePoll::SlowDown) => pending.bump_interval(),
            Ok(auth::DevicePoll::Success(tokens)) => {
                if let Err(e) = auth::save_tokens(&path, &tokens) {
                    let _ = tx.send(LoginEvent::Failed {
                        gen,
                        message: e.to_string(),
                    });
                    return;
                }
                let _ = tx.send(LoginEvent::Success { gen });
                return;
            }
            Ok(auth::DevicePoll::Denied) => {
                let _ = tx.send(LoginEvent::Failed {
                    gen,
                    message: "瀏覽器拒絕登入".into(),
                });
                return;
            }
            Ok(auth::DevicePoll::Expired) => {
                let _ = tx.send(LoginEvent::Failed {
                    gen,
                    message: "登入代碼已過期".into(),
                });
                return;
            }
            Ok(auth::DevicePoll::Failed(message)) => {
                let _ = tx.send(LoginEvent::Failed { gen, message });
                return;
            }
            Err(e) => {
                let _ = tx.send(LoginEvent::Failed {
                    gen,
                    message: e.to_string(),
                });
                return;
            }
        }
    }
}

fn open_settings(app: &mut App) {
    app.task_ui = None;
    app.drop = None;
    if app.logged_in
        && !matches!(
            app.catalog_status,
            CatalogStatus::Ready | CatalogStatus::Loading
        )
    {
        app.want_catalog = true;
    }
    if let Some(w) = app.settings.as_mut() {
        w.minimized = false;
        app.focus = Focus::Settings;
        return;
    }
    let area = app.area;
    let w = 64u16.min(area.width.saturating_sub(4)).max(36);
    let h = 34u16.min(area.height.saturating_sub(2)).max(18);
    let x = area.x + area.width.saturating_sub(w) / 2;
    let y = area.y + 1;
    app.settings = Some(Win {
        x,
        y,
        w,
        h,
        maximized: false,
        minimized: false,
    });
    app.focus = Focus::Settings;
    app.setting_field = SettingField::Kind;
}

fn open_task(app: &mut App) {
    app.settings = None;
    app.drop = None;
    let phase = app.task.snapshot().phase;
    app.task_ui = Some(if phase.is_live() || matches!(phase, TaskPhase::Done | TaskPhase::Failed) {
        TaskUi::Status
    } else {
        TaskUi::Form {
            edit: Edit::default(),
        }
    });
    app.focus = Focus::Task;
}

fn close_task(app: &mut App) {
    app.task_ui = None;
    if app.focus == Focus::Task {
        app.focus = Focus::Chat;
    }
}

fn submit_task_goal(
    app: &mut App,
    opts: &TuiOptions,
    sink: &Arc<FanoutSink>,
    done_tx: &mpsc::UnboundedSender<(String, crate::agent::RunOutcome)>,
) {
    let Some(TaskUi::Form { edit }) = &app.task_ui else {
        return;
    };
    let goal = edit.text.trim().to_string();
    if goal.is_empty() {
        app.status = "請填寫任務目標".into();
        return;
    }
    app.task.start_goal(goal.clone());
    app.push(Row::Meta(format!("已啟動任務模式：{goal}")));
    app.bench.log_event("", "message", format!("任務目標：{goal}"));
    app.task_ui = Some(TaskUi::Status);
    app.focus = Focus::Task;
    ensure_task_run(app, opts, sink, done_tx);
}

fn end_task_mode(app: &mut App) {
    if !app.task.snapshot().phase.is_live()
        && app.task.snapshot().phase != TaskPhase::Done
        && app.task.snapshot().phase != TaskPhase::Failed
    {
        close_task(app);
        return;
    }
    app.task.end();
    app.bench.log_event("", "message", "任務模式由使用者結束".into());
    app.push(Row::Meta("已結束任務模式".into()));
    close_task(app);
}

fn ensure_task_run(
    app: &mut App,
    opts: &TuiOptions,
    sink: &Arc<FanoutSink>,
    done_tx: &mpsc::UnboundedSender<(String, crate::agent::RunOutcome)>,
) {
    if app.inbox_tx.is_some() {
        return;
    }
    start_or_send(app, opts, sink, done_tx, UserTurn::from(task::KICK), false);
}

fn flush_task_action(
    app: &mut App,
    opts: &TuiOptions,
    sink: &Arc<FanoutSink>,
    done_tx: &mpsc::UnboundedSender<(String, crate::agent::RunOutcome)>,
) {
    match app.task_action.take() {
        Some(TaskAction::Submit) => submit_task_goal(app, opts, sink, done_tx),
        Some(TaskAction::Close) => close_task(app),
        Some(TaskAction::End) => end_task_mode(app),
        None => {}
    }
}

fn win_rect(w: &Win, area: Rect) -> Rect {
    if w.maximized {
        return Rect::new(
            area.x,
            area.y.saturating_add(1),
            area.width,
            area.height.saturating_sub(2),
        );
    }
    let x = w.x.min(area.width.saturating_sub(4));
    let y = w.y.min(area.height.saturating_sub(3));
    Rect::new(x, y, w.w.min(area.width.saturating_sub(x)), w.h.min(area.height.saturating_sub(y)))
}

#[cfg(test)]
fn draw(f: &mut Frame, app: &mut App, opts: &TuiOptions) -> Position {
    draw_ui(f, app, opts, false)
}

