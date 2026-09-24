struct App {
    web_receipts: Vec<(String, String)>,
    side_view: SideView,
    /// Side bar shown (docked when wide, overlaid when narrow).
    side_open: bool,
    side_scroll: usize,
    /// Bottom panel tab; `None` = panel closed.
    bottom: Option<BottomTab>,
    bottom_scroll: usize,
    side_area: Rect,
    bottom_area: Rect,
    /// Row hashes last published to the web, per view path.
    web_sent: HashMap<String, Vec<u64>>,
    /// Rendered rows by hash, so unchanged rows are not re-rendered.
    web_cache: HashMap<u64, hub::UiRow>,
    rows: Vec<Row>,
    edit: Edit,
    status: String,
    cache: String,
    child_count: u32,
    running: bool,
    awaiting: bool,
    logged_in: bool,
    auth_path: PathBuf,
    login_ui: LoginUi,
    login_gen: u64,
    want_login: bool,
    scroll: u16,
    stick_bottom: bool,
    send_mode: SendMode,
    queue: VecDeque<Queued>,
    inbox_tx: Option<mpsc::UnboundedSender<UserTurn>>,
    cancel: Option<CancelFlag>,
    knobs: Arc<Mutex<SessionKnobs>>,
    focus: Focus,
    setting_field: SettingField,
    settings: Option<Win>,
    conn: ProviderConfig,
    endpoint_edit: Edit,
    api_key_edit: Edit,
    model_edit: Edit,
    child_model_edit: Edit,
    context_edit: Edit,
    drag: Option<(i16, i16)>,
    /// Mouse grab offset inside the chat scrollbar thumb.
    scroll_grab: Option<i16>,
    hits: Vec<(Rect, Hit)>,
    area: Rect,
    streaming: bool,
    composer_inner: Rect,
    composer_frame: Rect,
    composer_snap: Option<Buffer>,
    header_bar: Rect,
    /// On-screen think-header rows whose clocks can be patched without redrawing chat or composer.
    think_clocks: Vec<(usize, Rect)>,
    last_clock_cells: Vec<(u16, u16, Cell)>,
    last_caret: Position,
    chat_inner: Rect,
    chat_bar: Rect,
    chat_total: u16,
    chat_max_off: u16,
    composer_vscroll: u16,
    input_dragging: bool,
    chat_dragging: bool,
    chat_glyphs: Vec<ChatGlyphLine>,
    /// Merged view of `grok_catalog` + `custom_catalog`; what every picker reads.
    catalog: ModelCatalog,
    catalog_status: CatalogStatus,
    /// Raw Grok catalog from the xAI login.
    grok_catalog: ModelCatalog,
    /// Raw model list fetched from the custom endpoint's /models.
    custom_catalog: ModelCatalog,
    /// (base_url, api_key) the custom catalog was fetched for.
    custom_cat_key: (String, String),
    custom_cat_loading: bool,
    custom_cat_err: Option<String>,
    /// xAI OAuth tokens exist on disk (independent of the active panel).
    xai_ready: bool,
    drop: Option<DropKind>,
    drop_cursor: usize,
    drop_scroll: u16,
    want_catalog: bool,
    open_tool: Option<(usize, usize)>,
    seal_tools: bool,
    activity: String,
    tick: u8,
    current_id: String,
    session: SessionMeta,
    parked: HashMap<String, ParkedChat>,
    sessions: Vec<SessionMeta>,
    store: Option<SessionStore>,
    launch_workspace: PathBuf,
    sidebar_ids: Vec<String>,
    /// Inline sidebar rename: session id + editor.
    rename: Option<(String, Edit)>,
    rename_inner: Rect,
    work_started: Option<Instant>,
    /// Composer is editing `queue[index]`; auto-send is paused until commit/cancel.
    queue_edit: Option<usize>,
    composer_stash: Option<Edit>,
    pending: Vec<String>,
    chat_sel: ChatSel,
    preview: HashMap<(String, u16), Vec<Line<'static>>>,
    picker: Option<Picker>,
    image_proto: HashMap<(String, u16, u16), Protocol>,
    image_cells: HashMap<(String, u16, u16), (u16, u16)>,
    graphic_blits: Vec<crate::preview::GraphicBlit>,
    last_graphic_blits: Vec<crate::preview::GraphicBlit>,
    image_hits: Vec<String>,
    image_view: Option<String>,
    bench: Workbench,
    monitors: Vec<SideMon>,
    backgrounds: Vec<SideBg>,
    inspector: Option<Inspector>,
    inspector_scroll: u16,
    ask_hub: AskUserHub,
    ask_hubs: HashMap<String, AskUserHub>,
    ask: Option<AskState>,
    ask_fill_inner: Rect,
    /// When routing an event into a parked session, do not open the overlay.
    ask_passive: bool,
    workspace_pick: Option<WorkspacePick>,
    task: Arc<TaskHub>,
    task_ui: Option<TaskUi>,
    task_draft_inner: Rect,
    task_action: Option<TaskAction>,
    skills: Arc<Mutex<SkillStore>>,
    skill_list: Vec<Skill>,
    skill_cursor: usize,
    skill_scroll: u16,
    skill_view: Option<SkillView>,
    web_url: Option<String>,
    composer_seq: u64,
    web_composer_seq: u64,
}

impl App {
    fn push(&mut self, row: Row) {
        if matches!(&row, Row::User(_) | Row::Agent(_)) {
            self.seal_tools = true;
            self.finish_open_think();
        }
        self.rows.push(row);
        if self.rows.len() > 2_000 {
            self.rows.drain(0..self.rows.len() - 1_500);
            self.chat_sel = ChatSel::None;
        }
        if self.stick_bottom {
            self.scroll = 0;
        }
    }

    fn apply_event(&mut self, ev: AgentEvent) {
        match ev {
            AgentEvent::RunStarted { model, .. } => {
                self.running = true;
                self.awaiting = false;
                self.streaming = false;
                self.mark_work_start();
                self.activity = format!("連線 {model}");
                self.status = "工作中".into();
            }
            AgentEvent::TurnStarted { turn, .. } => {
                self.running = true;
                self.awaiting = false;
                self.streaming = false;
                self.mark_work_start();
                self.seal_tools = true;
                self.finish_open_think();
                self.mark_open_server_done();
                self.activity = "思考中".into();
                self.status = format!("第 {turn} 輪");
            }
            AgentEvent::ReasoningDelta { text, .. } => {
                if text.is_empty() {
                    return;
                }
                self.append_think(&text);
            }
            AgentEvent::ModelDelta { text, .. } => {
                if text.is_empty() {
                    return;
                }
                self.activity = "撰寫中".into();
                self.finish_open_think();
                if self.streaming {
                    if let Some(Row::Agent(s)) = self.rows.last_mut() {
                        s.text.push_str(&text);
                        if self.stick_bottom {
                            self.scroll = 0;
                        }
                        return;
                    }
                }
                self.push(Row::Agent(AgentMsg::new(text)));
                self.streaming = true;
                self.seal_tools = true;
            }
            AgentEvent::ModelFinished {
                text,
                input_tokens,
                cached_tokens,
                ..
            } => {
                self.streaming = false;
                self.finish_open_think();
                if input_tokens > 0 {
                    let pct = (cached_tokens as f32 / input_tokens as f32) * 100.0;
                    self.cache = format!("{cached_tokens}/{input_tokens} ({pct:.0}%)");
                }
                if text.is_empty() {
                    return;
                }
                // The streamed agent row may no longer be the last row (a think
                // or server-tool row can land after the text deltas). Search
                // back within this turn so the final text replaces the streamed
                // partial instead of being appended again as a duplicate.
                for r in self.rows.iter_mut().rev() {
                    match r {
                        Row::User(_) => break,
                        Row::Agent(s) => {
                            if s.text.is_empty()
                                || text.starts_with(s.text.as_str())
                                || s.text.starts_with(text.as_str())
                            {
                                s.text = text;
                                return;
                            }
                            break;
                        }
                        _ => {}
                    }
                }
                self.push(Row::Agent(AgentMsg::new(text)));
            }
            AgentEvent::ToolStarted { call_id, name, args, .. } => {
                self.streaming = false;
                self.finish_open_think();
                self.activity = live_tool_activity(&name, &args, "執行中");
                self.push_tool_start(call_id, name, args);
            }
            AgentEvent::ToolFinished { call_id, name, output, .. } => {
                let _ = enable_raw_mode();
                let pic = picture_from_tool(&name, &output);
                self.finish_tool(&call_id, &name, output);
                if let Some((path, label)) = pic {
                    self.push(Row::Picture { path, label });
                }
                self.activity = "思考中".into();
            }
            AgentEvent::ServerToolObserved { kind, payload, .. } => {
                self.streaming = false;
                self.finish_open_think();
                if kind == "gateway" {
                    if let Some(msg) = payload.get("error").and_then(|v| v.as_str()) {
                        self.push(Row::Err(msg.to_string()));
                    }
                } else {
                    self.observe_server(&kind, payload);
                }
            }
            AgentEvent::FileChanged {
                path, kind, diff, ..
            } => {
                self.streaming = false;
                self.attach_file(FileChange { path, kind, diff });
            }
            AgentEvent::ContextCompacted {
                method,
                dropped_items,
                kept_items,
                ..
            } => {
                self.push(Row::Meta(format!(
                    "壓縮 ({method}) 丟 {dropped_items} 留 {kept_items}"
                )));
            }
            AgentEvent::ChildSpawned {
                name,
                prompt,
                model,
                ..
            } => {
                // The task supervisor reports itself like a child but is not a process.
                if name != task::AGENT_NAME {
                    self.agent_spawned("", &name, &prompt, &model);
                }
                self.push(Row::Meta(format!("子代理 {name} 已啟動")));
            }
            AgentEvent::ChildExited { name, detail, .. } => {
                if name != task::AGENT_NAME {
                    self.agent_exited("", &name, &detail);
                }
                self.mark_spawn_tool_done(&name);
                self.push(Row::Meta(format!("子代理 {name} 結束")));
            }
            AgentEvent::MonitorAttached {
                name,
                command,
                pid,
                ..
            } => {
                self.upsert_monitor(name.clone(), command.clone(), pid);
                self.push(Row::Meta(format!("監控 {name} 已掛上  $ {command}")));
            }
            AgentEvent::MonitorExited { name, detail, .. } => {
                if let Some(m) = self.mon_named_mut(&name) {
                    m.alive = false;
                    m.status = "結束".into();
                    m.detail = detail.clone();
                }
                self.push(Row::Meta(format!("監控 {name} 結束  {detail}")));
            }
            AgentEvent::BackgroundStarted {
                name,
                command,
                pid,
                ..
            } => {
                self.upsert_background(name.clone(), command.clone(), pid);
                self.push(Row::Meta(format!("後台 {name} 已掛上  $ {command}")));
            }
            AgentEvent::BackgroundOutput {
                name,
                stream,
                text,
                ..
            } => {
                if let Some(b) = self.bg_named_mut(&name) {
                    b.push_log(format!("{stream} {text}"));
                }
            }
            AgentEvent::BackgroundExited { name, detail, .. } => {
                if let Some(b) = self.bg_named_mut(&name) {
                    b.alive = false;
                    b.status = "結束".into();
                    b.detail = detail.clone();
                }
                self.push(Row::Meta(format!("後台 {name} 結束  {detail}")));
            }
            AgentEvent::TimerStarted {
                name,
                seconds,
                command,
                ..
            } => {
                let label = if command.is_empty() {
                    format!("timer {seconds}s")
                } else {
                    format!("timer {seconds}s  $ {command}")
                };
                self.upsert_background(name.clone(), label, 0);
                if let Some(b) = self.bg_named_mut(&name) {
                    b.status = "倒數中".into();
                }
                self.push(Row::Meta(format!("計時器 {name} 開始  {seconds}s")));
            }
            AgentEvent::TimerFired { name, detail, .. } => {
                if let Some(b) = self.bg_named_mut(&name) {
                    b.alive = false;
                    b.status = "已到時".into();
                    b.detail = detail.clone();
                }
                self.push(Row::Meta(format!("計時器 {name} 到時")));
            }
            AgentEvent::TimerCancelled { name, .. } => {
                if let Some(b) = self.bg_named_mut(&name) {
                    b.alive = false;
                    b.status = "已取消".into();
                }
                self.push(Row::Meta(format!("計時器 {name} 已取消")));
            }
            AgentEvent::AgentMessage {
                from, to, text, ..
            } => {
                if from == task::AGENT_NAME || to == task::AGENT_NAME {
                    let preview: String = text.chars().take(160).collect();
                    let line = format!("{from} → {to}  {}", preview.replace('\n', " "));
                    self.bench.log_event("", "message", line);
                } else {
                    self.agent_message("", &from, &to, &text);
                }
            }
            AgentEvent::AskUser {
                question,
                allow_multiple,
                options,
                ..
            } => {
                if options.is_empty() {
                    return;
                }
                self.push(Row::Meta(format!("問卷  {question}")));
                if self.ask_passive {
                    if let Some(h) = self.ask_hubs.get(&self.current_id) {
                        h.cancel();
                    }
                    return;
                }
                self.ask = Some(AskState::new(Question {
                    prompt: question,
                    allow_multiple,
                    options,
                }));
                self.focus = Focus::Ask;
                self.status = "請選擇".into();
            }
            AgentEvent::Notice { message, .. } => {
                self.push(Row::Meta(message));
            }
            AgentEvent::Error { message, .. } => {
                self.streaming = false;
                self.push(Row::Err(message));
            }
            AgentEvent::AwaitingInput { .. } => {
                let stopped = self.activity == "中斷中" || self.task.snapshot().skip_steer;
                self.running = false;
                self.awaiting = true;
                self.streaming = false;
                self.finish_open_think();
                self.stamp_work();
                self.activity.clear();
                self.status = if stopped { "已停止" } else { "待命" }.into();
            }
            AgentEvent::RunFinished { reason, text, .. } => {
                self.cancel_ask();
                self.running = false;
                self.awaiting = false;
                self.streaming = false;
                self.finish_open_think();
                self.inbox_tx = None;
                self.activity.clear();
                self.status = format!("結束 ({reason})");
                if !text.is_empty()
                    && !self
                        .rows
                        .iter()
                        .any(|r| matches!(r, Row::Agent(t) if t.text == text))
                {
                    self.push(Row::Agent(AgentMsg::new(text)));
                }
                self.stamp_work();
            }
            AgentEvent::SessionNamed { .. } => {}
        }
    }

    fn mon_named_mut(&mut self, name: &str) -> Option<&mut SideMon> {
        self.monitors.iter_mut().find(|m| m.name == name)
    }

    fn bg_named_mut(&mut self, name: &str) -> Option<&mut SideBg> {
        self.backgrounds.iter_mut().find(|b| b.name == name)
    }

    fn upsert_monitor(&mut self, name: String, command: String, pid: u32) {
        if let Some(m) = self.mon_named_mut(&name) {
            m.command = command;
            m.pid = pid;
            m.alive = true;
            m.status = "執行中".into();
            m.detail.clear();
            return;
        }
        self.monitors.push(SideMon {
            name,
            command,
            pid,
            status: "執行中".into(),
            alive: true,
            detail: String::new(),
        });
    }

    fn upsert_background(&mut self, name: String, command: String, pid: u32) {
        if let Some(b) = self.bg_named_mut(&name) {
            b.command = command;
            b.pid = pid;
            b.alive = true;
            b.status = "執行中".into();
            b.detail.clear();
            return;
        }
        self.backgrounds.push(SideBg {
            name,
            command,
            pid,
            status: "執行中".into(),
            alive: true,
            detail: String::new(),
            log: Vec::new(),
        });
    }

    #[cfg(test)]
    fn has_side(&self) -> bool {
        !self.bench.agents.is_empty() || !self.monitors.is_empty() || !self.backgrounds.is_empty()
    }

    fn close_inspector(&mut self) {
        self.inspector = None;
        self.inspector_scroll = 0;
        if self.focus == Focus::Inspector {
            self.focus = Focus::Chat;
        }
    }

    fn open_image_view(&mut self, rel: String) {
        self.chat_sel = ChatSel::Image(rel.clone());
        self.image_view = Some(rel);
        self.close_inspector();
        self.open_tool = None;
        self.focus = Focus::Chat;
    }

    fn close_image_view(&mut self) {
        self.image_view = None;
        self.focus = Focus::Chat;
    }

    fn refresh_skills(&mut self) {
        let ws = self.session.workspace.clone();
        let list = self
            .skills
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .scan(&ws);
        if self.skill_cursor >= list.len() && !list.is_empty() {
            self.skill_cursor = list.len() - 1;
        }
        self.skill_list = list;
    }

    fn toggle_import_claude(&mut self) {
        let mut g = self.skills.lock().unwrap_or_else(|e| e.into_inner());
        let on = !g.prefs().import_claude;
        let _ = g.set_import_claude(on);
        drop(g);
        self.refresh_skills();
        self.status = if on {
            "已引入 Claude Code 技能".into()
        } else {
            "已停止引入 Claude Code 技能".into()
        };
    }

    fn toggle_import_codex(&mut self) {
        let mut g = self.skills.lock().unwrap_or_else(|e| e.into_inner());
        let on = !g.prefs().import_codex;
        let _ = g.set_import_codex(on);
        drop(g);
        self.refresh_skills();
        self.status = if on {
            "已引入 Codex 技能".into()
        } else {
            "已停止引入 Codex 技能".into()
        };
    }

    fn toggle_skill(&mut self, i: usize) {
        let Some(id) = self.skill_list.get(i).map(|s| s.id.clone()) else {
            return;
        };
        let enabled = self.skill_list.get(i).map(|s| s.enabled).unwrap_or(false);
        let mut g = self.skills.lock().unwrap_or_else(|e| e.into_inner());
        let _ = g.set_enabled(&id, !enabled);
        drop(g);
        self.refresh_skills();
    }

    fn open_skill_view(&mut self, i: usize) {
        let Some(skill) = self.skill_list.get(i).cloned() else {
            return;
        };
        let body = crate::skills::read_skill_file(&skill.path).unwrap_or_else(|e| e.to_string());
        let mut edit = Edit::at_end(body);
        edit.home(false);
        self.skill_view = Some(SkillView {
            title: skill.name,
            origin: skill.origin.label().to_string(),
            edit,
            scroll: 0,
            inner: Rect::default(),
            dragging: false,
        });
        self.focus = Focus::Settings;
    }

    fn close_skill_view(&mut self) {
        self.skill_view = None;
        if self.settings.is_some() {
            self.focus = Focus::Settings;
        } else {
            self.focus = Focus::Chat;
        }
    }

    fn attach_pending(&mut self, rel: String) -> bool {
        if self.pending.len() >= crate::vision::MAX_USER_IMAGES {
            self.status = format!("最多 {} 張圖片", crate::vision::MAX_USER_IMAGES);
            return false;
        }
        if self.pending.iter().any(|p| p == &rel) {
            return true;
        }
        self.pending.push(rel);
        true
    }

    fn ingest_paths(&mut self, paths: &[PathBuf]) -> usize {
        let mut n = 0;
        for p in paths {
            match crate::vision::ingest_image_file(&self.session.workspace, p) {
                Ok(rel) => {
                    if self.attach_pending(rel) {
                        n += 1;
                    }
                }
                Err(e) => self.status = format!("無法加入圖片: {e}"),
            }
        }
        n
    }

    fn paste_text_or_images(&mut self, s: &str) {
        let dropped = crate::vision::parse_image_drop(s);
        if !dropped.is_empty() {
            let n = self.ingest_paths(&dropped);
            if n > 0 {
                self.status = format!("已附上 {n} 張圖片");
                return;
            }
        }
        self.edit.insert_str(s);
    }

    /// Bracketed paste from the terminal. Empty payloads still read the OS
    /// clipboard so Ctrl+V of a bitmap/file is not dropped on the floor.
    fn paste_from_terminal(&mut self, s: &str) {
        let dropped = crate::vision::parse_image_drop(s);
        if !dropped.is_empty() {
            let n = self.ingest_paths(&dropped);
            if n > 0 {
                self.status = format!("已附上 {n} 張圖片");
                return;
            }
        }
        if s.trim().is_empty() {
            self.paste_clipboard();
            return;
        }
        self.edit.insert_str(s);
    }

    fn ingest_clipboard_images(&mut self) -> bool {
        if let Some(img) = crate::clipimg::read_image() {
            match crate::vision::save_user_image(&self.session.workspace, &img) {
                Ok(rel) => {
                    if self.attach_pending(rel) {
                        self.status = "已貼上圖片".into();
                        return true;
                    }
                    return false;
                }
                Err(e) => {
                    self.status = format!("無法貼上圖片: {e}");
                    return false;
                }
            }
        }
        let files = crate::clipimg::read_image_files();
        if files.is_empty() {
            return false;
        }
        let n = self.ingest_paths(&files);
        if n > 0 {
            self.status = format!("已附上 {n} 張圖片");
            true
        } else {
            false
        }
    }

    fn paste_image(&mut self) {
        if !self.ingest_clipboard_images() {
            self.status = "剪貼簿沒有圖片 — 先複製截圖或圖片檔，再點「貼上圖片」".into();
        }
    }

    fn paste_clipboard(&mut self) {
        if self.ingest_clipboard_images() {
            return;
        }
        if let Some(s) = clipboard_get() {
            self.paste_text_or_images(&s);
        }
    }

    fn copy_selection(&mut self) -> bool {
        if let Some(view) = self.skill_view.as_ref() {
            if let Some(s) = view.edit.selected_text() {
                if clipboard_set(&s) {
                    self.status = "已複製".into();
                } else {
                    self.status = "無法複製到剪貼簿".into();
                }
                return true;
            }
        }
        if self.edit.has_sel() {
            if let Some(s) = self.edit.selected_text() {
                if clipboard_set(&s) {
                    self.status = "已複製".into();
                } else {
                    self.status = "無法複製到剪貼簿".into();
                }
                return true;
            }
        }
        match &self.chat_sel {
            ChatSel::Image(rel) => {
                let abs = self.session.workspace.join(rel);
                if clipboard_set_image(&abs) {
                    self.status = "已複製圖片".into();
                } else if clipboard_set(rel) {
                    self.status = "已複製路徑".into();
                } else {
                    self.status = "無法複製到剪貼簿".into();
                }
                true
            }
            ChatSel::Text { .. } => {
                let Some(text) = chat_selected_text(&self.rows, &self.chat_sel) else {
                    return false;
                };
                if text.is_empty() {
                    return false;
                }
                if clipboard_set(&text) {
                    self.status = "已複製".into();
                } else {
                    self.status = "無法複製到剪貼簿".into();
                }
                true
            }
            ChatSel::None => false,
        }
    }

    fn take_turn(&mut self) -> Option<UserTurn> {
        let text = self.edit.text.trim().to_string();
        let images: Vec<PathBuf> = self.pending.drain(..).map(PathBuf::from).collect();
        if text.is_empty() && images.is_empty() {
            return None;
        }
        self.edit.clear();
        Some(UserTurn { text, images })
    }

    fn finish_open_think(&mut self) {
        for r in self.rows.iter_mut().rev() {
            if let Row::Think(t) = r {
                if !t.done {
                    if let Some(start) = t.started.take() {
                        t.elapsed_ms = start.elapsed().as_millis() as u64;
                    }
                    t.done = true;
                }
                return;
            }
        }
    }

    fn mark_work_start(&mut self) {
        if self.work_started.is_none() {
            self.work_started = Some(Instant::now());
        }
    }

    fn stamp_work(&mut self) {
        let Some(start) = self.work_started.take() else {
            return;
        };
        let ms = start.elapsed().as_millis() as u64;
        for r in self.rows.iter_mut().rev() {
            if let Row::Agent(a) = r {
                a.work_ms = ms;
                return;
            }
        }
        self.push(Row::Meta(format!("工作 {}", md::fmt_duration(ms))));
    }

    fn append_think(&mut self, delta: &str) {
        self.activity = "思考中".into();
        self.streaming = false;
        if let Some(Row::Think(t)) = self.rows.last_mut() {
            if !t.done {
                t.text.push_str(delta);
                if t.started.is_none() {
                    t.started = Some(Instant::now());
                }
                if self.stick_bottom {
                    self.scroll = 0;
                }
                return;
            }
        }
        self.push(Row::Think(Think {
            text: delta.to_string(),
            expanded: false,
            done: false,
            elapsed_ms: 0,
            started: Some(Instant::now()),
        }));
    }

    fn push_tool_start(&mut self, call_id: String, name: String, args: Value) {
        let call = ToolCall {
            call_id,
            name,
            args,
            output: String::new(),
            files: Vec::new(),
            done: false,
            phase: "執行中".into(),
        };
        let fresh = self.seal_tools || !matches!(self.rows.last(), Some(Row::Tools(_)));
        if fresh {
            self.push(Row::Tools(ToolGroup {
                calls: vec![call],
                expanded: false,
            }));
            self.seal_tools = false;
        } else if let Some(Row::Tools(g)) = self.rows.last_mut() {
            g.calls.push(call);
        }
    }

    fn finish_tool(&mut self, call_id: &str, name: &str, output: String) {
        // Match by call id anywhere in the transcript first; fall back to the
        // newest group by name for rows restored from before call ids existed.
        let by_id = if call_id.is_empty() {
            None
        } else {
            self.rows.iter().rposition(|r| {
                matches!(r, Row::Tools(g) if g.calls.iter().any(|c| c.call_id == call_id))
            })
        };
        let group = by_id.or_else(|| self.rows.iter().rposition(|r| matches!(r, Row::Tools(_))));
        let Some(Row::Tools(g)) = group.and_then(|i| self.rows.get_mut(i)) else {
            return;
        };
        let idx = (!call_id.is_empty())
            .then(|| g.calls.iter().rposition(|c| c.call_id == call_id))
            .flatten()
            .or_else(|| g.calls.iter().rposition(|c| c.name == name && !c.done))
            .or_else(|| g.calls.iter().rposition(|c| !c.done))
            .or_else(|| g.calls.len().checked_sub(1));
        if let Some(i) = idx {
            let c = &mut g.calls[i];
            c.done = true;
            c.phase = tool_phase(name, &output).into();
            c.files.extend(parse_file_changes(&output));
            c.output = output;
        }
    }

    fn mark_spawn_tool_done(&mut self, child_name: &str) {
        let Some(g) = self.rows.iter_mut().rev().find_map(|r| match r {
            Row::Tools(g) => Some(g),
            _ => None,
        }) else {
            return;
        };
        for c in g.calls.iter_mut().rev() {
            if c.name != "spawn_agent" || c.done {
                continue;
            }
            let matches = serde_json::from_str::<Value>(&c.output)
                .ok()
                .and_then(|v| {
                    v.get("name")
                        .and_then(Value::as_str)
                        .map(|n| n == child_name)
                })
                .unwrap_or(false);
            if matches {
                c.done = true;
                c.phase = "完成".into();
                break;
            }
        }
    }

    fn observe_server(&mut self, kind: &str, payload: Value) {
        let name = canonical_server_tool(kind);
        let phase = server_phase(kind, &payload);
        let query = server_query(&payload);
        self.activity = if query.is_empty() {
            format!("{name}  {phase}")
        } else {
            format!("{name}  {phase}  {query}")
        };

        let mut args = payload;
        if args.get("query").is_none() && !query.is_empty() {
            args["query"] = Value::String(query.clone());
        }
        let done = phase_is_done(&phase);

        if !self.seal_tools {
            if let Some(Row::Tools(g)) = self.rows.last_mut() {
                if let Some(c) = g.calls.iter_mut().rev().find(|c| c.name == name) {
                    if !query.is_empty() {
                        c.args["query"] = Value::String(query);
                    }
                    c.phase = phase.clone();
                    c.output = server_tool_line(kind, &c.args);
                    c.done = done;
                    return;
                }
            }
        }

        let output = server_tool_line(kind, &args);
        let call = ToolCall {
            call_id: String::new(),
            name,
            args,
            output,
            files: Vec::new(),
            done,
            phase,
        };
        let fresh = self.seal_tools || !matches!(self.rows.last(), Some(Row::Tools(_)));
        if fresh {
            self.push(Row::Tools(ToolGroup {
                calls: vec![call],
                expanded: false,
            }));
            self.seal_tools = false;
        } else if let Some(Row::Tools(g)) = self.rows.last_mut() {
            g.calls.push(call);
        }
    }

    fn mark_open_server_done(&mut self) {
        if let Some(Row::Tools(g)) = self.rows.last_mut() {
            for c in &mut g.calls {
                if !c.done && (c.name == "web_search" || c.name == "x_search") {
                    c.done = true;
                    if c.phase != "完成" {
                        c.phase = "完成".into();
                    }
                }
            }
        }
    }

    fn attach_file(&mut self, file: FileChange) {
        let Some(g) = self.rows.iter_mut().rev().find_map(|r| match r {
            Row::Tools(g) => Some(g),
            _ => None,
        }) else {
            return;
        };
        if let Some(c) = g.calls.last_mut() {
            if !c.files.iter().any(|f| f.path == file.path) {
                c.files.push(file);
            }
        }
    }

    fn dismiss_tool_ui(&mut self) -> bool {
        if self.open_tool.take().is_some() {
            return true;
        }
        let mut any = false;
        for r in &mut self.rows {
            if let Row::Tools(g) = r {
                if g.expanded {
                    g.expanded = false;
                    any = true;
                }
            }
            if let Row::Think(t) = r {
                if t.expanded {
                    t.expanded = false;
                    any = true;
                }
            }
        }
        any
    }

    fn cancel_ask(&mut self) {
        let id = self.current_id.clone();
        self.cancel_session_ask(&id);
    }

    fn interrupt_work(&mut self) -> bool {
        if !self.running {
            return false;
        }
        if let Some(c) = &self.cancel {
            c.trip();
        }
        self.cancel_ask();
        self.streaming = false;
        self.finish_open_think();
        self.activity = "中斷中".into();
        self.status = "中斷中".into();
        true
    }

    fn cancel_session_ask(&mut self, id: &str) {
        if let Some(h) = self.ask_hubs.get(id) {
            h.cancel();
        }
        if id == self.current_id {
            self.ask_hub.cancel();
            if self.ask.take().is_some() {
                if self.focus == Focus::Ask {
                    self.focus = Focus::Chat;
                }
                self.status = "已取消問卷".into();
            }
        }
    }

    fn bind_ask_hub(&mut self) {
        self.ask_hub = self
            .ask_hubs
            .get(&self.current_id)
            .cloned()
            .unwrap_or_else(AskUserHub::new);
    }

    fn attach_ask_hub(&mut self, run_id: &str) -> AskUserHub {
        let hub = AskUserHub::new();
        self.ask_hubs.insert(run_id.to_string(), hub.clone());
        self.ask_hub = hub.clone();
        hub
    }

    fn submit_ask(&mut self) {
        let Some(mut ask) = self.ask.take() else {
            return;
        };
        let mut values = ask.values.clone();
        if ask.filling {
            if let Some(slot) = values.get_mut(ask.cursor) {
                *slot = ask.fill_edit.text.chars().take(ask::MAX_INPUT).collect();
            }
        }
        match ask::answer_from_picks(&ask.question, &ask.chosen, &values) {
            Ok(body) => {
                ask.values = values;
                let summary = ask.summary();
                self.ask_hub.answer(body);
                self.push(Row::Meta(summary));
                self.focus = Focus::Chat;
                self.status = "已回答".into();
            }
            Err(msg) => {
                self.status = msg;
                self.ask = Some(ask);
                self.focus = Focus::Ask;
            }
        }
    }

    fn activate_ask_option(&mut self, index: usize, submit_if_ready: bool) {
        let Some(ask) = self.ask.as_mut() else {
            return;
        };
        if index >= ask.n() {
            return;
        }
        ask.save_fill();
        ask.cursor = index;
        let input = ask.question.options.get(index).is_some_and(|o| o.input);
        ask.mark_cursor();
        if input {
            ask.enter_fill();
            return;
        }
        if submit_if_ready && !ask.question.allow_multiple {
            self.submit_ask();
        }
    }

    fn snapshot_live(&mut self) -> ParkedChat {
        ParkedChat {
            session: self.session.clone(),
            rows: std::mem::take(&mut self.rows),
            status: std::mem::take(&mut self.status),
            cache: std::mem::take(&mut self.cache),
            child_count: self.child_count,
            running: self.running,
            awaiting: self.awaiting,
            scroll: self.scroll,
            stick_bottom: self.stick_bottom,
            queue: std::mem::take(&mut self.queue),
            inbox_tx: self.inbox_tx.take(),
            cancel: self.cancel.clone(),
            streaming: self.streaming,
            open_tool: self.open_tool.take(),
            seal_tools: self.seal_tools,
            activity: std::mem::take(&mut self.activity),
            edit: std::mem::take(&mut self.edit),
            work_started: self.work_started.take(),
            queue_edit: self.queue_edit.take(),
            composer_stash: self.composer_stash.take(),
            pending: std::mem::take(&mut self.pending),
            bench: std::mem::take(&mut self.bench),
            monitors: std::mem::take(&mut self.monitors),
            backgrounds: std::mem::take(&mut self.backgrounds),
            inspector: self.inspector.take(),
            inspector_scroll: self.inspector_scroll,
            task: self.task.clone(),
        }
    }

    fn install_live(&mut self, p: ParkedChat) {
        self.session = p.session;
        self.current_id = self.session.id.clone();
        self.rows = p.rows;
        self.status = p.status;
        self.cache = p.cache;
        self.child_count = p.child_count;
        self.running = p.running;
        self.awaiting = p.awaiting;
        self.scroll = p.scroll;
        self.stick_bottom = p.stick_bottom;
        self.queue = p.queue;
        self.inbox_tx = p.inbox_tx;
        self.cancel = p.cancel;
        self.streaming = p.streaming;
        self.open_tool = p.open_tool;
        self.seal_tools = p.seal_tools;
        self.activity = p.activity;
        self.edit = p.edit;
        self.work_started = p.work_started;
        self.queue_edit = p.queue_edit;
        self.composer_stash = p.composer_stash;
        self.pending = p.pending;
        self.chat_sel = ChatSel::None;
        self.preview.clear();
        self.image_proto.clear();
        self.image_cells.clear();
        self.graphic_blits.clear();
        self.last_graphic_blits.clear();
        self.bench = p.bench;
        self.monitors = p.monitors;
        self.backgrounds = p.backgrounds;
        self.inspector = p.inspector;
        self.inspector_scroll = p.inspector_scroll;
        self.task = p.task;
        self.task_ui = None;
        self.composer_vscroll = 0;
        self.scroll_grab = None;
        self.image_view = None;
        self.ask = None;
        if self.focus == Focus::Ask {
            self.focus = Focus::Chat;
        }
        if self.focus == Focus::Workspace {
            self.focus = Focus::Chat;
        }
        if self.focus == Focus::Task {
            self.focus = Focus::Chat;
        }
    }

    fn persist_transcript(&mut self) {
        let Some(store) = &self.store else {
            return;
        };
        let Ok(rows) = serde_json::to_value(&self.rows) else {
            return;
        };
        let _ = store.save_transcript(&self.session.id, &rows);
        self.session.updated_at = chrono::Utc::now();
        let _ = store.save_meta(&self.session);
    }

    fn apply_title(&mut self, id: &str, name: &str) {
        if self.current_id == id {
            if let Some(store) = &self.store {
                let _ = store.touch_name(&mut self.session, name.to_string(), true);
            } else if !self.session.name_is_manual {
                self.session.name = session::sanitize_title(name);
                self.session.named = true;
            }
            return;
        }
        if let Some(p) = self.parked.get_mut(id) {
            if let Some(store) = &self.store {
                let _ = store.touch_name(&mut p.session, name.to_string(), true);
            } else if !p.session.name_is_manual {
                p.session.name = session::sanitize_title(name);
                p.session.named = true;
            }
        }
    }

    fn route_event(&mut self, ev: AgentEvent) {
        let sid = ev.session_id().to_string();
        if let AgentEvent::SessionNamed { name, .. } = &ev {
            self.apply_title(&sid, name);
            return;
        }
        let child_work = ev.is_child_work();
        let skip_persist = matches!(ev, AgentEvent::BackgroundOutput { .. });
        if sid.is_empty() || sid == self.current_id {
            if child_work {
                self.apply_child_work(ev);
            } else {
                self.log_root_event(&ev);
                self.apply_event(ev);
                if !skip_persist {
                    self.persist_transcript();
                }
            }
            return;
        }
        if !self.parked.contains_key(&sid) {
            return;
        }
        let parked = self.parked.remove(&sid).unwrap();
        let parked = self.with_parked(parked, |app| {
            if child_work {
                app.apply_child_work(ev);
            } else {
                app.log_root_event(&ev);
                app.apply_event(ev);
                if !skip_persist {
                    app.persist_transcript();
                }
            }
        });
        self.parked.insert(sid, parked);
    }

    fn with_parked<F: FnOnce(&mut Self)>(&mut self, parked: ParkedChat, f: F) -> ParkedChat {
        let saved_ask = self.ask.take();
        let ask_focus = self.focus == Focus::Ask;
        let saved = self.snapshot_live();
        self.install_live(parked);
        self.ask_passive = true;
        f(self);
        self.ask_passive = false;
        let parked = self.snapshot_live();
        self.install_live(saved);
        self.ask = saved_ask;
        if self.ask.is_some() && ask_focus {
            self.focus = Focus::Ask;
        }
        parked
    }

    fn is_blank_draft(&self) -> bool {
        !self.session.named
            && self.inbox_tx.is_none()
            && !self.running
            && self
                .rows
                .iter()
                .all(|r| matches!(r, Row::Meta(_)))
    }

    fn begin_new_chat(&mut self) {
        self.cancel_ask();
        self.cancel_rename();
        let start = if self.session.workspace.as_os_str().is_empty() {
            self.launch_workspace.clone()
        } else {
            self.session.workspace.clone()
        };
        self.workspace_pick = Some(WorkspacePick::open(&start));
        self.focus = Focus::Workspace;
    }

    fn sync_workspace_pick(&mut self) {
        let Some(p) = self.workspace_pick.as_mut() else {
            return;
        };
        let fallback = p.view.cwd.clone();
        p.view = folderpick::view_for_input(&p.edit.text, &fallback);
        if p.view.entries.is_empty() {
            p.cursor = 0;
        } else if p.cursor >= p.view.entries.len() {
            p.cursor = p.view.entries.len() - 1;
        }
        p.scroll = p.scroll.min(p.cursor as u16);
        p.notice = p.view.error.clone();
    }

    fn enter_workspace_dir(&mut self, dir: PathBuf) {
        let Some(p) = self.workspace_pick.as_mut() else {
            return;
        };
        if !dir.is_dir() {
            p.notice = Some("不是資料夾".into());
            return;
        }
        p.view = folderpick::list_folder(&dir);
        p.edit = Edit::at_end(folderpick::display_path(&p.view.cwd));
        p.cursor = 0;
        p.scroll = 0;
        p.notice = p.view.error.clone();
        p.focus = WsFocus::List;
    }

    fn activate_ws_entry(&mut self, idx: usize) {
        let Some(ent) = self
            .workspace_pick
            .as_ref()
            .and_then(|p| p.view.entries.get(idx).cloned())
        else {
            return;
        };
        if let Some(p) = self.workspace_pick.as_mut() {
            p.cursor = idx;
        }
        if ent.is_dir {
            self.enter_workspace_dir(ent.path);
        } else if let Some(p) = self.workspace_pick.as_mut() {
            p.edit = Edit::at_end(folderpick::display_path(&ent.path));
            p.notice = Some("檔案：確定時會使用上層資料夾".into());
            p.focus = WsFocus::List;
        }
    }

    fn cancel_workspace_pick(&mut self) {
        self.workspace_pick = None;
        if self.focus == Focus::Workspace {
            self.focus = Focus::Chat;
        }
    }

    fn confirm_workspace_pick(&mut self) {
        let Some(p) = self.workspace_pick.as_ref() else {
            return;
        };
        let typed = p.edit.text.clone();
        let cwd = p.view.cwd.clone();
        let selected = p.selected().cloned();
        let dir = if let Ok(abs) = std::fs::canonicalize(typed.trim()) {
            if abs.is_dir() {
                folderpick::normalize(&abs)
            } else if abs.is_file() {
                folderpick::existing_dir(&abs)
            } else {
                folderpick::workspace_of(&cwd, selected.as_ref())
            }
        } else {
            folderpick::workspace_of(&cwd, selected.as_ref())
        };
        if !dir.is_dir() {
            if let Some(p) = self.workspace_pick.as_mut() {
                p.notice = Some("請選擇存在的資料夾".into());
            }
            return;
        }
        self.workspace_pick = None;
        self.focus = Focus::Chat;
        self.create_chat(dir);
    }

    fn create_workspace_dir(&mut self) {
        let Some(p) = self.workspace_pick.as_ref() else {
            return;
        };
        let target = folderpick::create_target(&p.edit.text, &p.view.cwd);
        match folderpick::mkdir(&target) {
            Ok(dir) => {
                self.enter_workspace_dir(dir);
                if let Some(p) = self.workspace_pick.as_mut() {
                    p.notice = Some("已建立資料夾".into());
                }
            }
            Err(e) => {
                if let Some(p) = self.workspace_pick.as_mut() {
                    p.notice = Some(format!("無法建立: {e}"));
                }
            }
        }
    }

    fn new_chat(&mut self) {
        self.begin_new_chat();
    }

    fn create_chat(&mut self, workspace: PathBuf) {
        let workspace = folderpick::existing_dir(&workspace);
        self.launch_workspace = workspace.clone();
        if self.is_blank_draft() {
            self.session.workspace = workspace.clone();
            if let Some(store) = &self.store {
                self.session.updated_at = chrono::Utc::now();
                let _ = store.save_meta(&self.session);
            }
            self.status = format!("工作目錄  {}", folderpick::display_path(&workspace));
            return;
        }
        self.cancel_ask();
        self.persist_transcript();
        self.persist_agents();
        let parked = self.snapshot_live();
        self.parked.insert(parked.session.id.clone(), parked);
        let session = if let Some(store) = &self.store {
            store
                .create(workspace.clone())
                .unwrap_or_else(|_| SessionMeta::new(workspace.clone()))
        } else {
            SessionMeta::new(workspace)
        };
        self.install_live(fresh_chat(session));
        self.bind_ask_hub();
        self.refresh_session_list();
        self.status = format!(
            "工作目錄  {}",
            folderpick::display_path(&self.session.workspace)
        );
    }

    fn switch_to(&mut self, id: &str) {
        if id == self.current_id {
            return;
        }
        self.cancel_ask();
        self.persist_transcript();
        self.persist_agents();
        let parked = self.snapshot_live();
        self.parked.insert(parked.session.id.clone(), parked);
        if let Some(p) = self.parked.remove(id) {
            self.install_live(p);
            self.bind_ask_hub();
            return;
        }
        let loaded = self.load_from_store(id);
        self.install_live(loaded);
        self.bind_ask_hub();
    }

    fn load_from_store(&self, id: &str) -> ParkedChat {
        let session = self
            .store
            .as_ref()
            .and_then(|s| s.load_meta(id).ok())
            .unwrap_or_else(|| {
                let mut m = SessionMeta::new(self.launch_workspace.clone());
                m.id = id.to_string();
                m
            });
        let rows = self
            .store
            .as_ref()
            .and_then(|s| s.load_transcript(id).ok())
            .and_then(|v| serde_json::from_value::<Vec<Row>>(v).ok())
            .unwrap_or_default();
        ParkedChat {
            session,
            rows,
            status: "待命".into(),
            cache: "cache —".into(),
            child_count: 0,
            running: false,
            awaiting: false,
            scroll: 0,
            stick_bottom: true,
            queue: VecDeque::new(),
            inbox_tx: None,
            cancel: None,
            streaming: false,
            open_tool: None,
            seal_tools: false,
            activity: String::new(),
            edit: Edit::default(),
            work_started: None,
            queue_edit: None,
            composer_stash: None,
            pending: Vec::new(),
            bench: self
                .store
                .as_ref()
                .and_then(|s| s.load_agents::<Vec<SavedAgent>>(id))
                .map(Workbench::restore)
                .unwrap_or_default(),
            monitors: Vec::new(),
            backgrounds: Vec::new(),
            inspector: None,
            inspector_scroll: 0,
            task: TaskHub::from_state(
                id,
                self.store
                    .as_ref()
                    .and_then(|s| s.load_task::<crate::task::TaskState>(id))
                    .unwrap_or_default(),
            ),
        }
    }

    fn refresh_session_list(&mut self) {
        let mut by_id: HashMap<String, SessionMeta> = HashMap::new();
        if let Some(store) = &self.store {
            if let Ok(list) = store.list() {
                for s in list {
                    by_id.insert(s.id.clone(), s);
                }
            }
        }
        by_id.insert(self.session.id.clone(), self.session.clone());
        for p in self.parked.values() {
            by_id.insert(p.session.id.clone(), p.session.clone());
        }
        let mut list: Vec<SessionMeta> = by_id.into_values().collect();
        list.sort_by(|a, b| b.updated_at.cmp(&a.updated_at).then(b.id.cmp(&a.id)));
        self.sessions = list;
        self.sidebar_ids = self.sessions.iter().map(|s| s.id.clone()).collect();
    }

    fn finish_run(&mut self, id: &str, out: crate::agent::RunOutcome) {
        self.cancel_session_ask(id);
        self.ask_hubs.remove(id);
        if id == self.current_id {
            self.bind_ask_hub();
            self.running = false;
            self.awaiting = false;
            self.inbox_tx = None;
            self.cancel = None;
            if self.status.starts_with("工作") || self.status.starts_with("第") || self.status.starts_with("中斷") {
                self.status = format!("結束 ({} 輪)", out.turns);
            }
            return;
        }
        if let Some(p) = self.parked.get_mut(id) {
            p.running = false;
            p.awaiting = false;
            p.inbox_tx = None;
            p.cancel = None;
            if p.status.starts_with("工作") || p.status.starts_with("第") || p.status.starts_with("中斷") {
                p.status = format!("結束 ({} 輪)", out.turns);
            }
        }
    }

    fn begin_rename(&mut self, id: &str) {
        let name = if self.current_id == id {
            self.session.name.clone()
        } else if let Some(p) = self.parked.get(id) {
            p.session.name.clone()
        } else {
            self.sessions
                .iter()
                .find(|s| s.id == id)
                .map(|s| s.name.clone())
                .unwrap_or_else(|| "新對話".into())
        };
        self.rename = Some((id.to_string(), Edit::at_end(name)));
        self.focus = Focus::Rename;
    }

    fn cancel_rename(&mut self) {
        self.rename = None;
        if self.focus == Focus::Rename {
            self.focus = Focus::Chat;
        }
    }

    fn commit_rename(&mut self) {
        let Some((id, edit)) = self.rename.take() else {
            return;
        };
        if self.focus == Focus::Rename {
            self.focus = Focus::Chat;
        }
        let name = session::sanitize_title(&edit.text);
        if name.is_empty() {
            return;
        }
        self.apply_manual_name(&id, &name);
        self.refresh_session_list();
    }

    fn begin_queue_edit(&mut self, index: usize) {
        if index >= self.queue.len() {
            return;
        }
        if self.queue_edit == Some(index) {
            return;
        }
        let mut idx = index;
        if let Some(cur) = self.queue_edit {
            let removed = self.commit_queue_edit();
            if removed && cur < idx {
                idx = idx.saturating_sub(1);
            }
            if idx >= self.queue.len() {
                return;
            }
        }
        if self.composer_stash.is_none() {
            self.composer_stash = Some(std::mem::take(&mut self.edit));
        }
        let text = self.queue[idx].text.clone();
        self.edit = Edit::at_end(text);
        self.queue_edit = Some(idx);
        self.focus = Focus::Chat;
        self.composer_vscroll = 0;
    }

    /// Restore the original queued text and the previous composer draft.
    fn cancel_queue_edit(&mut self) {
        if self.queue_edit.take().is_none() {
            return;
        }
        self.edit = self.composer_stash.take().unwrap_or_default();
        self.composer_vscroll = 0;
    }

    /// Save the composer into the queued item. Empty text drops that item.
    /// Returns true if the item was removed.
    fn commit_queue_edit(&mut self) -> bool {
        let Some(i) = self.queue_edit.take() else {
            return false;
        };
        let text = self.edit.text.trim().to_string();
        let mut removed = false;
        if i < self.queue.len() {
            if text.is_empty() && self.queue[i].images.is_empty() {
                self.queue.remove(i);
                removed = true;
            } else if let Some(slot) = self.queue.get_mut(i) {
                slot.text = text;
            }
        }
        self.edit = self.composer_stash.take().unwrap_or_default();
        self.composer_vscroll = 0;
        removed
    }

    fn apply_manual_name(&mut self, id: &str, name: &str) {
        if self.current_id == id {
            if let Some(store) = &self.store {
                let _ = store.rename_manual(&mut self.session, name.to_string());
            } else {
                self.session.name = name.to_string();
                self.session.named = true;
                self.session.name_is_manual = true;
            }
            return;
        }
        if let Some(p) = self.parked.get_mut(id) {
            if let Some(store) = &self.store {
                let _ = store.rename_manual(&mut p.session, name.to_string());
            } else {
                p.session.name = name.to_string();
                p.session.named = true;
                p.session.name_is_manual = true;
            }
            return;
        }
        if let Some(store) = &self.store {
            if let Ok(mut meta) = store.load_meta(id) {
                let _ = store.rename_manual(&mut meta, name.to_string());
            }
        }
    }

    fn delete_session(&mut self, id: &str) {
        self.cancel_rename();
        self.cancel_session_ask(id);
        self.ask_hubs.remove(id);
        let deleting_current = self.current_id == id;
        if deleting_current {
            self.inbox_tx = None;
            self.cancel = None;
            self.running = false;
            self.awaiting = false;
        } else {
            self.parked.remove(id);
        }
        if let Some(store) = &self.store {
            let _ = store.delete(id);
        }
        let next = self
            .sessions
            .iter()
            .map(|s| s.id.clone())
            .chain(self.parked.keys().cloned())
            .find(|sid| sid != id);
        if deleting_current {
            if let Some(nid) = next.filter(|n| n != id) {
                if let Some(p) = self.parked.remove(&nid) {
                    self.install_live(p);
                } else {
                    let loaded = self.load_from_store(&nid);
                    self.install_live(loaded);
                }
            } else {
                let workspace = self.launch_workspace.clone();
                let session = if let Some(store) = &self.store {
                    store
                        .create(workspace)
                        .unwrap_or_else(|_| SessionMeta::new(self.launch_workspace.clone()))
                } else {
                    SessionMeta::new(workspace)
                };
                self.install_live(fresh_chat(session));
            }
            self.bind_ask_hub();
        }
        self.refresh_session_list();
    }
}

