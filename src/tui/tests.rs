    use super::*;

    #[test]
    fn caret_uses_display_width_not_bytes() {
        let area = Rect::new(0, 10, 40, 1);
        let cjk = caret_in(area, "你好", 2);
        assert_eq!(cjk, Position::new(4, 10));
        let cjk_mid = caret_in(area, "你好", 1);
        assert_eq!(cjk_mid, Position::new(2, 10));
        let ascii = caret_in(area, "ab", 2);
        assert_eq!(ascii, Position::new(2, 10));
        let empty = caret_in(area, "", 0);
        assert_eq!(empty, Position::new(0, 10));
    }

    #[test]
    fn submit_queues_only_while_running() {
        assert_eq!(submit_kind(false, false, SendMode::Queue), Submit::Start);
        assert_eq!(submit_kind(true, true, SendMode::Queue), Submit::Queue);
        assert_eq!(submit_kind(true, true, SendMode::Insert), Submit::Insert);
        assert_eq!(submit_kind(true, false, SendMode::Queue), Submit::Insert);
    }

    #[test]
    fn editing_queued_message_blocks_flush_until_commit() {
        let mut app = test_app();
        let (tx, mut rx) = mpsc::unbounded_channel();
        app.inbox_tx = Some(tx);
        app.awaiting = true;
        app.queue.push_back("old".into());
        app.edit = Edit::at_end("draft".into());
        app.begin_queue_edit(0);
        assert_eq!(app.edit.text, "old");
        assert_eq!(
            app.composer_stash.as_ref().map(|e| e.text.as_str()),
            Some("draft")
        );
        app.edit = Edit::at_end("new text".into());
        flush_queue(&mut app);
        assert!(rx.try_recv().is_err(), "must not send while editing");
        assert_eq!(app.queue.len(), 1);
        assert!(!app.commit_queue_edit());
        assert_eq!(app.edit.text, "draft");
        assert_eq!(app.queue[0].text, "new text");
        flush_queue(&mut app);
        assert_eq!(rx.try_recv().unwrap().text, "new text");
        assert!(app.queue.is_empty());
    }

    #[test]
    fn cancel_queue_edit_restores_original_and_does_not_send_draft() {
        let mut app = test_app();
        let (tx, mut rx) = mpsc::unbounded_channel();
        app.inbox_tx = Some(tx);
        app.awaiting = true;
        app.edit = Edit::at_end("draft".into());
        app.queue.push_back("keep".into());
        app.begin_queue_edit(0);
        app.edit = Edit::at_end("nope".into());
        flush_queue(&mut app);
        assert!(rx.try_recv().is_err());
        app.cancel_queue_edit();
        assert!(app.queue_edit.is_none());
        assert_eq!(app.queue[0].text, "keep");
        assert_eq!(app.edit.text, "draft");
        flush_queue(&mut app);
        assert_eq!(rx.try_recv().unwrap().text, "keep");
    }

    #[test]
    fn empty_commit_removes_queued_item() {
        let mut app = test_app();
        app.queue.push_back("gone".into());
        app.queue.push_back("stay".into());
        app.begin_queue_edit(0);
        app.edit.clear();
        assert!(app.commit_queue_edit());
        assert_eq!(app.queue.len(), 1);
        assert_eq!(app.queue[0].text, "stay");
    }

    #[test]
    fn kick_idle_queue_waits_until_edit_finishes() {
        let mut app = test_app();
        app.logged_in = false;
        app.queue.push_back("later".into());
        app.begin_queue_edit(0);
        let opts = TuiOptions {
            model: "grok-4.6".into(),
            events: PathBuf::from("events.jsonl"),
            workspace: PathBuf::from("."),
            max_turns: 0,
            web_search: false,
            dispatcher: false,
            child_model: String::new(),
            reasoning_effort: ReasoningEffort::High,
        };
        let sink = Arc::new(FanoutSink { sinks: vec![] });
        let (done_tx, _done_rx) = mpsc::unbounded_channel();
        kick_idle_queue(&mut app, &opts, &sink, &done_tx);
        assert_eq!(app.queue.len(), 1, "held while editing");
        app.commit_queue_edit();
        kick_idle_queue(&mut app, &opts, &sink, &done_tx);
        assert!(app.queue.is_empty());
        assert!(app
            .rows
            .iter()
            .any(|r| matches!(r, Row::User(u) if u.text == "later")));
    }

    #[test]
    fn child_gets_a_read_only_transcript_tab_and_monitor_fills_the_side_view() {
        let mut app = test_app();
        app.current_id = "r".into();
        app.session.id = "r".into();
        let meta = test_meta();
        app.route_event(AgentEvent::ChildSpawned {
            meta: meta.clone(),
            name: "coder".into(),
            agent_card_url: "http://127.0.0.1:9/.well-known/agent-card.json".into(),
            prompt: "fix src/a.rs".into(),
            model: "grok-3-mini".into(),
        });
        app.route_event(AgentEvent::AgentMessage {
            meta: meta.clone(),
            from: "root".into(),
            to: "coder".into(),
            text: "fix src/a.rs".into(),
        });
        app.route_event(AgentEvent::MonitorAttached {
            meta: meta.clone(),
            name: "copy".into(),
            command: "python hook.py".into(),
            pid: 4242,
        });
        assert!(app.has_side());
        assert_eq!(app.bench.agents.len(), 1);
        let tab = &app.bench.agents[0];
        assert_eq!(tab.path, "coder");
        assert_eq!(tab.prompt, "fix src/a.rs");
        assert_eq!(tab.model, "grok-3-mini");
        assert_eq!(tab.state, AgentState::Starting);
        assert!(
            matches!(tab.view.rows.first(), Some(Row::User(u)) if u.text == "fix src/a.rs"),
            "the parent's instruction opens the child transcript"
        );
        assert_eq!(app.monitors[0].command, "python hook.py");
        assert_eq!(app.monitors[0].pid, 4242);

        let child_meta = |path: &str, run: &str| crate::events::EventMeta {
            ts: chrono::Utc::now(),
            agent_name: path.rsplit('/').next().unwrap().into(),
            run_id: run.into(),
            parent_run_id: Some("r".into()),
            path: path.into(),
        };
        let before = app.rows.len();
        app.route_event(AgentEvent::TurnStarted { meta: child_meta("coder", "c1"), turn: 1 });
        app.route_event(AgentEvent::ModelDelta {
            meta: child_meta("coder", "c1"),
            text: "reading the file".into(),
        });
        app.route_event(AgentEvent::ToolStarted {
            meta: child_meta("coder", "c1"),
            call_id: "k1".into(),
            name: "read_file".into(),
            args: serde_json::json!({"path": "src/a.rs"}),
            kind: "client".into(),
        });
        assert_eq!(app.rows.len(), before, "child work must not land in parent chat");
        let tab = app.bench.agent("coder").unwrap();
        assert_eq!(tab.state, AgentState::Working);
        assert!(tab.view.rows.iter().any(|r| matches!(r, Row::Agent(a) if a.text.contains("reading the file"))));
        assert!(tab.view.rows.iter().any(|r| matches!(r, Row::Tools(g) if g.calls[0].call_id == "k1")));
        assert_eq!(app.bench.tool_log.back().map(|t| t.path.as_str()), Some("coder"));

        // A grandchild spawned by coder shows up nested under it.
        app.route_event(AgentEvent::ChildSpawned {
            meta: child_meta("coder", "c1"),
            name: "lint".into(),
            agent_card_url: String::new(),
            prompt: "run clippy".into(),
            model: String::new(),
        });
        app.route_event(AgentEvent::ModelDelta {
            meta: child_meta("coder/lint", "g1"),
            text: "clippy clean".into(),
        });
        let lint = app.bench.agent("coder/lint").expect("grandchild tab");
        assert_eq!(lint.depth(), 1);
        assert!(lint.view.rows.iter().any(|r| matches!(r, Row::Agent(a) if a.text == "clippy clean")));

        // Going idle after a natural stop.
        app.route_event(AgentEvent::ModelFinished {
            meta: child_meta("coder", "c1"),
            text: "done".into(),
            finish: "stop".into(),
            input_tokens: 0,
            cached_tokens: 0,
        });
        app.route_event(AgentEvent::AwaitingInput { meta: child_meta("coder", "c1") });
        assert_eq!(app.bench.agent("coder").unwrap().state, AgentState::Idle);

        app.route_event(AgentEvent::ChildExited {
            meta: meta.clone(),
            name: "coder".into(),
            detail: "killed".into(),
        });
        assert_eq!(app.bench.agent("coder").unwrap().state, AgentState::Exited);
        assert_eq!(app.bench.agent("coder/lint").unwrap().state, AgentState::Exited, "descendants die too");
        assert_eq!(app.child_count, 0);
    }

    #[test]
    fn agent_tab_swaps_in_its_own_transcript_and_back() {
        let mut app = test_app();
        app.current_id = "r".into();
        app.session.id = "r".into();
        app.push(Row::Meta("root row".into()));
        app.agent_spawned("", "coder", "fix", "");
        app.with_agent("coder", |app| app.push(Row::Meta("child row".into())));
        app.open_agent_tab("coder");
        assert!(app.viewing_agent());
        let seen = app.with_active_view(|app| app.rows.iter().filter_map(|r| match r {
            Row::Meta(m) => Some(m.clone()),
            _ => None,
        }).collect::<Vec<_>>());
        assert_eq!(seen, vec!["child row".to_string()]);
        assert!(matches!(app.rows.last(), Some(Row::Meta(m)) if m == "root row"), "root rows stay in place");

        let opts = test_opts("grok-4.6", ReasoningEffort::High);
        let mut terminal = Terminal::new(ratatui::backend::TestBackend::new(140, 30)).unwrap();
        terminal.draw(|f| { let _ = draw_ui(f, &mut app, &opts, false); }).unwrap();
        let text: String = terminal.backend().buffer().content.iter().map(|c| c.symbol()).collect();
        let compact: String = text.chars().filter(|c| !c.is_whitespace()).collect();
        assert!(compact.contains("childrow"), "the child tab shows the child transcript");
        assert!(!compact.contains("rootrow"));
        assert!(compact.contains("唯讀"), "child tabs have no composer");
        assert!(matches!(app.rows.last(), Some(Row::Meta(m)) if m == "root row"), "drawing restores root rows");

        // Esc goes back to the main chat; typing on a child tab switches back too.
        let (mut opts, sink, tx) = dummy_key_env();
        handle_key(&mut app, &mut opts, KeyCode::Esc, KeyModifiers::NONE, &sink, &tx);
        assert!(!app.viewing_agent());
        app.open_agent_tab("coder");
        handle_key(&mut app, &mut opts, KeyCode::Char('x'), KeyModifiers::NONE, &sink, &tx);
        assert!(!app.viewing_agent());
        assert_eq!(app.edit.text, "x");

        app.open_agent_tab("coder");
        app.close_agent_tab("coder");
        assert!(app.bench.open.is_empty());
        assert!(!app.viewing_agent());
    }

    #[test]
    fn tool_rows_finish_by_call_id_not_name() {
        let mut app = test_app();
        app.push_tool_start("a".into(), "run_command".into(), serde_json::json!({"command": "one"}));
        app.push_tool_start("b".into(), "run_command".into(), serde_json::json!({"command": "two"}));
        app.finish_tool("a", "run_command", r#"{"exit_code":1}"#.into());
        let Row::Tools(g) = &app.rows[0] else { panic!("tool row"); };
        assert!(g.calls[0].done, "first call finished by id");
        assert_eq!(g.calls[0].phase, "失敗");
        assert!(!g.calls[1].done, "second call still running");
    }

    #[test]
    fn gear_stays_clickable_on_the_status_bar() {
        let mut app = test_app();
        app.current_id = "r".into();
        app.session.id = "r".into();
        app.route_event(AgentEvent::ChildSpawned {
            meta: test_meta(),
            name: "coder".into(),
            agent_card_url: "http://127.0.0.1:9/.well-known/agent-card.json".into(),
            prompt: "fix".into(),
            model: String::new(),
        });
        assert!(app.has_side());
        app.side_open = true;
        app.side_view = SideView::Agents;
        let opts = test_opts("grok-4.6", ReasoningEffort::High);
        let mut terminal =
            Terminal::new(ratatui::backend::TestBackend::new(120, 24)).unwrap();
        terminal
            .draw(|f| {
                let _ = draw_ui(f, &mut app, &opts, false);
            })
            .unwrap();
        let gear = app
            .hits
            .iter()
            .rev()
            .find(|(_, h)| *h == Hit::Gear)
            .map(|(r, _)| *r)
            .expect("gear hit");
        assert_eq!(gear.y, 23, "gear lives on the bottom status bar");
        assert!(gear.x + gear.width >= 110, "gear must sit at the right: {gear:?}");
        assert_eq!(hit_at(&app.hits, gear.x, 23), Some(Hit::Gear));
        assert_eq!(hit_at(&app.hits, gear.x + 1, 23), Some(Hit::Gear));
        let agent_row = app
            .hits
            .iter()
            .find(|(_, h)| matches!(h, Hit::SideAgent(0)))
            .map(|(r, _)| *r)
            .expect("the agent tree lists coder");
        assert_eq!(hit_at(&app.hits, agent_row.x + 2, agent_row.y), Some(Hit::SideAgent(0)));
    }

    #[test]
    fn background_fills_the_side_rail_and_keeps_output_out_of_chat() {
        let mut app = test_app();
        app.current_id = "r".into();
        app.session.id = "r".into();
        let meta = test_meta();
        app.route_event(AgentEvent::BackgroundStarted {
            meta: meta.clone(),
            name: "dev".into(),
            command: "npm run dev".into(),
            pid: 99,
        });
        assert!(app.has_side());
        assert_eq!(app.backgrounds.len(), 1);
        assert_eq!(app.backgrounds[0].command, "npm run dev");
        assert_eq!(app.backgrounds[0].pid, 99);
        assert!(app.backgrounds[0].alive);
        assert!(app.rows.iter().any(|r| match r {
            Row::Meta(s) => s.contains("後台 dev"),
            _ => false,
        }));

        let before = app.rows.len();
        app.route_event(AgentEvent::BackgroundOutput {
            meta: meta.clone(),
            name: "dev".into(),
            stream: "out".into(),
            text: "listening on :3000".into(),
        });
        assert_eq!(app.rows.len(), before, "background stdout must not land in chat");
        assert!(
            app.backgrounds[0]
                .log
                .iter()
                .any(|l| l.contains("listening on :3000")),
            "{:?}",
            app.backgrounds[0].log
        );

        app.route_event(AgentEvent::BackgroundExited {
            meta,
            name: "dev".into(),
            detail: "killed".into(),
        });
        assert!(!app.backgrounds[0].alive);
        assert_eq!(app.backgrounds[0].status, "結束");
        assert_eq!(app.backgrounds[0].detail, "killed");
    }

    #[test]
    fn timer_fills_the_side_rail() {
        let mut app = test_app();
        app.current_id = "r".into();
        app.session.id = "r".into();
        let meta = test_meta();
        app.route_event(AgentEvent::TimerStarted {
            meta: meta.clone(),
            name: "n1".into(),
            seconds: 30,
            command: "echo hi".into(),
        });
        assert!(app.has_side());
        assert_eq!(app.backgrounds.len(), 1);
        assert_eq!(app.backgrounds[0].name, "n1");
        assert!(app.backgrounds[0].alive);
        assert_eq!(app.backgrounds[0].status, "倒數中");
        assert!(app.backgrounds[0].command.contains("30s"), "{}", app.backgrounds[0].command);
        assert!(app.rows.iter().any(|r| match r {
            Row::Meta(s) => s.contains("計時器 n1"),
            _ => false,
        }));
        app.route_event(AgentEvent::TimerFired {
            meta: meta.clone(),
            name: "n1".into(),
            detail: "notified".into(),
        });
        assert!(!app.backgrounds[0].alive);
        assert_eq!(app.backgrounds[0].status, "已到時");
        app.route_event(AgentEvent::TimerStarted {
            meta: meta.clone(),
            name: "n2".into(),
            seconds: 5,
            command: String::new(),
        });
        app.route_event(AgentEvent::TimerCancelled {
            meta,
            name: "n2".into(),
        });
        let n2 = app.backgrounds.iter().find(|b| b.name == "n2").unwrap();
        assert!(!n2.alive);
        assert_eq!(n2.status, "已取消");
    }

    #[test]
    fn child_spawned_prompt_defaults_when_missing() {
        let raw = serde_json::json!({
            "type": "child_spawned",
            "ts": "2026-08-14T00:00:00Z",
            "agent_name": "root",
            "run_id": "s",
            "name": "x",
            "agent_card_url": "http://127.0.0.1:1/.well-known/agent-card.json"
        });
        let ev: AgentEvent = serde_json::from_value(raw).unwrap();
        match ev {
            AgentEvent::ChildSpawned { prompt, name, .. } => {
                assert!(prompt.is_empty());
                assert_eq!(name, "x");
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn hit_at_uses_topmost() {
        let hits = vec![
            (Rect::new(0, 0, 10, 10), Hit::Chat),
            (Rect::new(2, 2, 4, 4), Hit::Gear),
        ];
        assert_eq!(hit_at(&hits, 3, 3), Some(Hit::Gear));
        assert_eq!(hit_at(&hits, 0, 0), Some(Hit::Chat));
        assert_eq!(hit_at(&hits, 50, 50), None);
    }

    #[test]
    fn row_line_prefixes() {
        let row = Row::User("hi".into());
        let l = &row_lines(&row)[0];
        assert!(format!("{l:?}").contains("hi"));
    }

    #[test]
    fn user_row_legacy_string_still_loads() {
        let row: Row = serde_json::from_value(serde_json::json!({"User": "hello"})).unwrap();
        match row {
            Row::User(u) => {
                assert_eq!(u.text, "hello");
                assert!(u.images.is_empty());
            }
            _ => panic!("expected User row"),
        }
    }

    #[test]
    fn screenshot_tool_finished_shows_picture_row() {
        let mut app = test_app();
        app.apply_event(AgentEvent::ToolStarted {
            meta: test_meta(),
            call_id: "c1".into(),
            name: "screenshot".into(),
            args: serde_json::json!({"path": ".groka/shots/a.jpg"}),
            kind: "client".into(),
        });
        app.apply_event(AgentEvent::ToolFinished {
            meta: test_meta(),
            call_id: "c1".into(),
            name: "screenshot".into(),
            output: serde_json::json!({
                "path": ".groka/shots/a.jpg",
                "attach_image": true,
                "mime": "image/jpeg"
            })
            .to_string(),
        });
        assert!(app.rows.iter().any(|r| matches!(
            r,
            Row::Picture { path, .. } if path == ".groka/shots/a.jpg"
        )));
        let pic = app.rows.iter().find_map(|r| match r {
            Row::Picture { path, label } => Some((path.as_str(), label.as_str())),
            _ => None,
        }).unwrap();
        assert!(pic.1.contains("模型在看"), "{}", pic.1);
    }

    fn write_red_png(dir: &std::path::Path, name: &str) -> String {
        let path = dir.join(name);
        image::RgbImage::from_pixel(80, 40, image::Rgb([255, 0, 0]))
            .save(&path)
            .unwrap();
        name.to_string()
    }

    #[test]
    fn missing_picker_falls_back_to_halfblock_rows() {
        let dir = tempfile::tempdir().unwrap();
        let rel = write_red_png(dir.path(), "red.png");
        let mut app = test_app();
        app.session.workspace = dir.path().to_path_buf();
        app.push(Row::Picture {
            path: rel,
            label: "shot".into(),
        });
        let vis = chat_logical_rows(&mut app, 80);
        assert!(vis.iter().all(|l| l.graphic.is_none()));
        assert!(
            vis.iter().any(|l| format!("{:?}", l.line).contains("▀")),
            "{:?}",
            vis.iter().map(|l| format!("{:?}", l.line)).collect::<Vec<_>>()
        );
        assert!(vis.iter().any(|l| l.hit == Some(Hit::ChatImage(0))));
    }

    #[test]
    fn sixel_picker_reserves_a_graphic_slot_instead_of_halfblocks() {
        let dir = tempfile::tempdir().unwrap();
        let rel = write_red_png(dir.path(), "red.png");
        let mut app = test_app();
        app.session.workspace = dir.path().to_path_buf();
        let mut picker = Picker::from_fontsize((8, 16));
        picker.set_protocol_type(ratatui_image::picker::ProtocolType::Sixel);
        app.picker = Some(picker);
        app.push(Row::User(UserMsg {
            text: "see".into(),
            images: vec![rel.clone()],
        }));
        let vis = chat_logical_rows(&mut app, 80);
        let g = vis
            .iter()
            .find_map(|l| l.graphic.clone())
            .expect("graphic slot");
        assert_eq!(g.0, rel);
        assert!(g.1 >= 1 && g.1 <= crate::preview::MAX_COLS, "{}", g.1);
        assert!(g.2 >= 1 && g.2 <= crate::preview::MAX_ROWS, "{}", g.2);
        assert!(
            vis.iter().all(|l| !format!("{:?}", l.line).contains("▀")),
            "sixel path must not emit halfblock lines: {:?}",
            vis.iter().map(|l| format!("{:?}", l.line)).collect::<Vec<_>>()
        );
        let image_hits: Vec<_> = vis
            .iter()
            .filter(|l| l.hit == Some(Hit::ChatImage(0)))
            .collect();
        assert_eq!(
            image_hits.len(),
            2,
            "caption + graphic should both be clickable, got {}",
            image_hits.len()
        );
        assert!(image_hits.iter().any(|l| l.graphic.is_some()));
        assert!(image_hits.iter().any(|l| format!("{:?}", l.line).contains("圖片")));
    }

    fn draw_sixel_chat(app: &mut App) -> ratatui::Terminal<ratatui::backend::TestBackend> {
        let opts = TuiOptions {
            model: "grok-4.6".into(),
            events: PathBuf::from("events.jsonl"),
            workspace: app.session.workspace.clone(),
            max_turns: 0,
            web_search: false,
            dispatcher: false,
            child_model: String::new(),
            reasoning_effort: ReasoningEffort::High,
        };
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(90, 32)).unwrap();
        terminal
            .draw(|f| {
                let _ = draw(f, app, &opts);
            })
            .unwrap();
        terminal
    }

    #[test]
    fn sixel_image_does_not_leave_wide_symbols_in_the_buffer() {
        let dir = tempfile::tempdir().unwrap();
        let rel = write_red_png(dir.path(), "red.png");
        let mut app = test_app();
        app.session.workspace = dir.path().to_path_buf();
        let mut picker = Picker::from_fontsize((8, 16));
        picker.set_protocol_type(ratatui_image::picker::ProtocolType::Sixel);
        app.picker = Some(picker);
        app.push(Row::User(UserMsg {
            text: "see".into(),
            images: vec![rel],
        }));
        let opts = TuiOptions {
            model: "grok-4.6".into(),
            events: PathBuf::from("events.jsonl"),
            workspace: app.session.workspace.clone(),
            max_turns: 0,
            web_search: false,
            dispatcher: false,
            child_model: String::new(),
            reasoning_effort: ReasoningEffort::High,
        };
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(90, 32)).unwrap();
        let completed = terminal
            .draw(|f| {
                let _ = draw(f, &mut app, &opts);
            })
            .unwrap();
        let buf = completed.buffer;
        let wide: Vec<_> = buf
            .content
            .iter()
            .enumerate()
            .filter(|(_, c)| c.symbol().len() > 80)
            .map(|(i, c)| (i, c.symbol().len()))
            .collect();
        assert!(
            wide.is_empty(),
            "sixel payload in a cell poisons ratatui Buffer::diff: {wide:?}"
        );
        assert!(
            !app.graphic_blits.is_empty(),
            "immediate sixel must be queued for a post-diff blit"
        );
        assert!(
            app.graphic_blits[0].data.starts_with('\x1b'),
            "{:?}",
            &app.graphic_blits[0].data[..app.graphic_blits[0].data.len().min(20)]
        );
        let skipped = buf.content.iter().filter(|c| c.skip).count();
        assert!(skipped >= 2, "graphic area should be skipped, got {skipped}");
        let blit = &app.graphic_blits[0];
        let cell = &buf[(blit.x, blit.y)];
        assert!(cell.skip);
        assert_eq!(cell.symbol(), " ");
    }

    #[test]
    fn typing_does_not_rebuild_sixel_blit() {
        let dir = tempfile::tempdir().unwrap();
        let rel = write_red_png(dir.path(), "red.png");
        let mut app = test_app();
        app.session.workspace = dir.path().to_path_buf();
        let mut picker = Picker::from_fontsize((8, 16));
        picker.set_protocol_type(ratatui_image::picker::ProtocolType::Sixel);
        app.picker = Some(picker);
        app.push(Row::User(UserMsg {
            text: "see".into(),
            images: vec![rel],
        }));
        let mut terminal = draw_sixel_chat(&mut app);
        let first = app.graphic_blits.clone();
        assert!(!first.is_empty());
        app.edit.insert_str("hello");
        let opts = TuiOptions {
            model: "grok-4.6".into(),
            events: PathBuf::from("events.jsonl"),
            workspace: app.session.workspace.clone(),
            max_turns: 0,
            web_search: false,
            dispatcher: false,
            child_model: String::new(),
            reasoning_effort: ReasoningEffort::High,
        };
        terminal
            .draw(|f| {
                let _ = draw(f, &mut app, &opts);
            })
            .unwrap();
        assert_eq!(
            app.graphic_blits, first,
            "composer edits must not move or re-encode the image"
        );
    }

    fn sample_bg(name: &str) -> SideBg {
        SideBg {
            name: name.into(),
            command: "sleep 1".into(),
            pid: 1,
            status: "running".into(),
            alive: true,
            detail: String::new(),
            log: vec!["out".into()],
        }
    }

    #[test]
    fn inspector_over_sixel_drops_blit_and_unskips_so_the_overlay_erases_it() {
        let dir = tempfile::tempdir().unwrap();
        let rel = write_red_png(dir.path(), "red.png");
        let mut app = test_app();
        app.session.workspace = dir.path().to_path_buf();
        let mut picker = Picker::from_fontsize((8, 16));
        picker.set_protocol_type(ratatui_image::picker::ProtocolType::Sixel);
        app.picker = Some(picker);
        app.backgrounds.push(sample_bg("job"));
        app.push(Row::User(UserMsg {
            text: "see".into(),
            images: vec![rel],
        }));
        let opts = TuiOptions {
            model: "grok-4.6".into(),
            events: PathBuf::from("events.jsonl"),
            workspace: app.session.workspace.clone(),
            max_turns: 0,
            web_search: false,
            dispatcher: false,
            child_model: String::new(),
            reasoning_effort: ReasoningEffort::High,
        };
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(90, 32)).unwrap();
        terminal
            .draw(|f| {
                let _ = draw(f, &mut app, &opts);
            })
            .unwrap();
        assert!(
            !app.graphic_blits.is_empty(),
            "sixel must blit while the inspector is closed"
        );
        let graphic = app.graphic_blits[0].area();

        app.inspector = Some(Inspector::Background("job".into()));
        let completed = terminal
            .draw(|f| {
                let _ = draw(f, &mut app, &opts);
            })
            .unwrap();
        assert!(
            app.graphic_blits.is_empty(),
            "inspector must hide sixel instead of painting it over the overlay"
        );
        let panel = inspector_rect(Rect::new(0, 0, 90, 32));
        let overlap = graphic.intersection(panel);
        assert!(
            overlap.width > 0 && overlap.height > 0,
            "setup must overlap graphic={graphic:?} panel={panel:?}"
        );
        let buf = completed.buffer;
        for y in overlap.y..overlap.bottom() {
            for x in overlap.x..overlap.right() {
                assert!(
                    !buf[(x, y)].skip,
                    "overlay cell {x},{y} must be written so sixel is erased"
                );
            }
        }

        app.close_inspector();
        let completed = terminal
            .draw(|f| {
                let _ = draw(f, &mut app, &opts);
            })
            .unwrap();
        assert!(
            !app.graphic_blits.is_empty(),
            "closing the inspector must restore the sixel blit"
        );
        let blit = &app.graphic_blits[0];
        assert!(completed.buffer[(blit.x, blit.y)].skip);
    }

    fn press(app: &mut App, code: KeyCode) {
        let (tx, _rx) = mpsc::unbounded_channel();
        let mut opts = TuiOptions {
            model: "grok-4.6".into(),
            events: PathBuf::from("events.jsonl"),
            workspace: PathBuf::from("."),
            max_turns: 0,
            web_search: false,
            dispatcher: false,
            child_model: String::new(),
            reasoning_effort: ReasoningEffort::High,
        };
        let sink = Arc::new(FanoutSink { sinks: vec![] });
        handle_key(app, &mut opts, code, KeyModifiers::NONE, &sink, &tx);
    }

    fn sample_ask(allow_multiple: bool) -> AgentEvent {
        AgentEvent::AskUser {
            meta: test_meta(),
            question: "挑一個".into(),
            allow_multiple,
            options: vec![
                crate::ask::Choice {
                    id: "retry".into(),
                    label: "重試".into(),
                    input: false,
                },
                crate::ask::Choice {
                    id: "other".into(),
                    label: "自填".into(),
                    input: true,
                },
            ],
        }
    }

    #[test]
    fn ask_user_event_opens_overlay() {
        let mut app = test_app();
        app.apply_event(sample_ask(false));
        assert!(app.ask.is_some());
        assert_eq!(app.focus, Focus::Ask);
        assert!(
            app.rows.iter().any(|r| matches!(r, Row::Meta(s) if s.contains("挑一個"))),
            "transcript should record the question"
        );
    }

    #[test]
    fn ask_arrows_move_cursor_and_enter_submits() {
        let mut app = test_app();
        let mut rx = app.ask_hub.register();
        app.apply_event(sample_ask(false));
        press(&mut app, KeyCode::Down);
        assert_eq!(app.ask.as_ref().unwrap().cursor, 1);
        press(&mut app, KeyCode::Up);
        assert_eq!(app.ask.as_ref().unwrap().cursor, 0);
        press(&mut app, KeyCode::Enter);
        assert!(app.ask.is_none(), "plain option should submit immediately");
        assert_eq!(app.focus, Focus::Chat);
        let body = rx.try_recv().expect("answer");
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(v["selected"][0]["id"], "retry");
        assert!(
            app.rows.iter().any(|r| matches!(r, Row::Meta(s) if s.contains("重試"))),
            "transcript should record the pick"
        );
    }

    #[test]
    fn ask_input_option_requires_typed_value() {
        let mut app = test_app();
        let mut rx = app.ask_hub.register();
        app.apply_event(sample_ask(false));
        press(&mut app, KeyCode::Down);
        press(&mut app, KeyCode::Enter);
        assert!(app.ask.as_ref().unwrap().filling, "input option opens the field");
        press(&mut app, KeyCode::Enter);
        assert!(app.ask.is_some(), "empty fill-in must not submit");
        assert_eq!(app.status, "請填寫「自填」");
        for c in "用SQLite".chars() {
            press(&mut app, KeyCode::Char(c));
        }
        press(&mut app, KeyCode::Enter);
        assert!(app.ask.is_none());
        let body = rx.try_recv().expect("answer");
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(v["selected"][0]["id"], "other");
        assert_eq!(v["selected"][0]["value"], "用SQLite");
    }

    #[test]
    fn ask_mouse_click_selects_plain_option() {
        let mut app = test_app();
        let mut rx = app.ask_hub.register();
        app.apply_event(sample_ask(false));
        app.hits = vec![(Rect::new(0, 2, 20, 1), Hit::AskOption(0))];
        let mut opts = TuiOptions {
            model: "grok-4.6".into(),
            events: PathBuf::from("events.jsonl"),
            workspace: PathBuf::from("."),
            max_turns: 0,
            web_search: false,
            dispatcher: false,
            child_model: String::new(),
            reasoning_effort: ReasoningEffort::High,
        };
        handle_mouse(
            &mut app,
            &mut opts,
            MouseEventKind::Down(MouseButton::Left),
            1,
            2,
            KeyModifiers::NONE,
        );
        assert!(app.ask.is_none());
        let body = rx.try_recv().expect("answer");
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(v["selected"][0]["id"], "retry");
    }

    #[test]
    fn parked_event_does_not_dismiss_current_ask() {
        let mut app = test_app();
        app.current_id = "cur".into();
        app.session.id = "cur".into();
        let mut rx = app.ask_hub.register();
        app.apply_event(sample_ask(false));
        app.parked.insert("parked".into(), parked_stub("parked"));
        app.route_event(AgentEvent::ModelDelta {
            meta: crate::events::EventMeta {
                ts: chrono::Utc::now(),
                agent_name: "root".into(),
                run_id: "parked".into(),
                parent_run_id: None,
                path: String::new(),
            },
            text: "bg".into(),
        });
        assert!(app.ask.is_some(), "overlay must survive parked-session events");
        assert_eq!(app.focus, Focus::Ask);
        assert!(
            rx.try_recv().is_err(),
            "current waiter must not be cancelled by another session's events"
        );
    }

    #[test]
    fn switch_to_cancels_open_ask() {
        let mut app = test_app();
        app.current_id = "a".into();
        app.session.id = "a".into();
        let mut rx = app.ask_hub.register();
        app.apply_event(sample_ask(false));
        app.parked.insert("b".into(), parked_stub("b"));
        app.switch_to("b");
        assert!(app.ask.is_none());
        let body = rx.try_recv().expect("switch should cancel the waiter");
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(v["cancelled"], true);
    }

    #[test]
    fn parked_ask_user_cancels_that_hub_only() {
        let mut app = test_app();
        app.current_id = "cur".into();
        app.session.id = "cur".into();
        let parked_hub = AskUserHub::new();
        let mut parked_rx = parked_hub.register();
        app.ask_hubs.insert("parked".into(), parked_hub);
        let mut cur_rx = app.ask_hub.register();
        app.apply_event(sample_ask(false));
        app.parked.insert("parked".into(), parked_stub("parked"));
        app.route_event(AgentEvent::AskUser {
            meta: crate::events::EventMeta {
                ts: chrono::Utc::now(),
                agent_name: "root".into(),
                run_id: "parked".into(),
                parent_run_id: None,
                path: String::new(),
            },
            question: "那邊".into(),
            allow_multiple: false,
            options: vec![crate::ask::Choice {
                id: "x".into(),
                label: "X".into(),
                input: false,
            }],
        });
        assert!(app.ask.is_some(), "current overlay stays");
        assert!(cur_rx.try_recv().is_err(), "current waiter stays");
        let body = parked_rx.try_recv().expect("parked ask_user must not hang");
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(v["cancelled"], true);
        let parked = app.parked.get("parked").unwrap();
        assert!(
            parked
                .rows
                .iter()
                .any(|r| matches!(r, Row::Meta(s) if s.contains("那邊"))),
            "parked transcript should record the cancelled question"
        );
    }

    #[test]
    fn paste_image_path_attaches_pending_and_sends_with_empty_text() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("drop.jpg");
        let img = crate::vision::from_rgba(4, 4, vec![0, 0, 255, 255].repeat(16)).unwrap();
        crate::vision::save_jpeg(&src, &img).unwrap();
        let mut app = test_app();
        app.session.workspace = dir.path().to_path_buf();
        app.paste_text_or_images(&format!("\"{}\"", src.display()));
        assert_eq!(app.pending.len(), 1, "{:?}", app.pending);
        assert!(app.pending[0].starts_with(".groka/inbox/"));
        assert!(app.edit.is_empty());
        let turn = app.take_turn().unwrap();
        assert!(turn.text.is_empty());
        assert_eq!(turn.images.len(), 1);
        assert!(app.pending.is_empty());
    }

    #[test]
    fn paste_from_terminal_text_goes_to_composer() {
        let mut app = test_app();
        app.paste_from_terminal("hello");
        assert_eq!(app.edit.text, "hello");
        assert!(app.pending.is_empty());
    }

    #[test]
    fn composer_cells_keep_composer_background() {
        let mut app = test_app();
        app.edit.insert_str("hi");
        let opts = test_opts("grok-4.6", ReasoningEffort::High);
        let mut terminal =
            Terminal::new(ratatui::backend::TestBackend::new(80, 24)).unwrap();
        terminal
            .draw(|f| {
                let _ = draw(f, &mut app, &opts);
            })
            .unwrap();
        let inner = app.composer_inner;
        assert!(inner.width > 1 && inner.height > 0, "{inner:?}");
        let buf = terminal.backend().buffer();
        let typed = &buf[(inner.x, inner.y)];
        assert_eq!(typed.symbol(), "h", "{typed:?}");
        assert_eq!(typed.bg, COMPOSER, "{typed:?}");
        let rest = &buf[(inner.x.saturating_add(2), inner.y)];
        assert_eq!(rest.bg, COMPOSER, "{rest:?}");
    }

    #[test]
    fn running_placeholder_does_not_spin_inside_composer() {
        let mut app = test_app();
        app.running = true;
        app.activity = "思考中".into();
        app.tick = 3;
        let opts = test_opts("grok-4.6", ReasoningEffort::High);
        let mut terminal =
            Terminal::new(ratatui::backend::TestBackend::new(80, 24)).unwrap();
        terminal
            .draw(|f| {
                let _ = draw(f, &mut app, &opts);
            })
            .unwrap();
        let inner = app.composer_inner;
        let buf = terminal.backend().buffer();
        let mut text = String::new();
        for x in inner.x..inner.right() {
            text.push_str(buf[(x, inner.y)].symbol());
        }
        let compact: String = text.chars().filter(|c| !c.is_whitespace()).collect();
        assert!(compact.contains("模型工作中"), "{text}");
        assert!(
            !compact.contains("思考中"),
            "live activity belongs in the header, not the composer: {text}"
        );
        assert!(
            !['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏']
                .iter()
                .any(|c| compact.contains(*c)),
            "spinner must stay out of the composer: {text}"
        );
        assert_eq!(buf[(inner.x, inner.y)].bg, COMPOSER);
    }

    #[test]
    fn idle_app_does_not_need_pulse() {
        let app = test_app();
        assert!(!is_pulsing(&app));
    }

    #[test]
    fn running_app_needs_pulse() {
        let mut app = test_app();
        app.running = true;
        assert!(is_pulsing(&app));
    }

    #[test]
    fn pulse_advances_spinner_without_being_a_redraw_signal() {
        let mut app = test_app();
        app.running = true;
        let before = app.tick;
        pulse_spinner(&mut app);
        assert_eq!(app.tick, before.wrapping_add(1));
    }

    #[test]
    fn header_pulse_is_independent_of_composer_text() {
        let mut app = test_app();
        app.running = true;
        app.header_bar = Rect::new(0, 0, 80, 1);
        assert!(header_pulse_ok(&app));
        app.edit.insert_str("queue this while it works");
        assert!(header_pulse_ok(&app), "clock must keep ticking while typing");
        assert!(want_hardware_cursor(&app));
        assert!(
            collect_clock_cells(&app, &test_opts("grok-4.6", ReasoningEffort::High))
                .iter()
                .all(|(x, y, _)| !app.composer_frame.contains(Position { x: *x, y: *y })),
            "pulse cells must not include the composer"
        );
    }

    #[test]
    fn header_line_includes_spinner_and_elapsed_clock() {
        let mut app = test_app();
        app.running = true;
        app.status = "已登出".into();
        app.activity = "思考中".into();
        app.tick = 3;
        app.work_started = Some(Instant::now() - Duration::from_millis(3_400));
        app.edit.insert_str("keep me");
        let opts = test_opts("grok-4.6", ReasoningEffort::High);
        let buf = render_header_buffer(&app, &opts, Rect::new(0, 0, 100, 1));
        let mut text = String::new();
        for x in 0..100 {
            text.push_str(buf[(x, 0)].symbol());
        }
        let compact: String = text.chars().filter(|c| !c.is_whitespace()).collect();
        assert!(compact.contains(spinner(3)), "{text}");
        assert!(compact.contains("思考中"), "{text}");
        assert!(compact.contains("3.4s"), "{text}");
        assert!(!compact.contains("keepme"), "{text}");
    }

    #[test]
    fn running_empty_composer_keeps_hardware_cursor_for_ime() {
        let mut app = test_app();
        app.running = true;
        assert!(app.edit.is_empty());
        assert!(
            want_hardware_cursor(&app),
            "IME is drawn at the hardware cursor; it must stay in the composer"
        );
        app.edit.insert_str("queue this");
        assert!(want_hardware_cursor(&app));
    }

    #[test]
    fn idle_empty_composer_keeps_hardware_cursor() {
        let app = test_app();
        assert!(app.edit.is_empty());
        assert!(!app.running);
        assert!(want_hardware_cursor(&app));
    }

    #[test]
    fn paste_from_terminal_image_path_attaches_like_drop() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("drop.jpg");
        let img = crate::vision::from_rgba(4, 4, vec![0, 0, 255, 255].repeat(16)).unwrap();
        crate::vision::save_jpeg(&src, &img).unwrap();
        let mut app = test_app();
        app.session.workspace = dir.path().to_path_buf();
        app.paste_from_terminal(&format!("\"{}\"", src.display()));
        assert_eq!(app.pending.len(), 1, "{:?}", app.pending);
        assert!(app.edit.is_empty(), "{}", app.edit.text);
    }

    #[test]
    fn paste_key_includes_ctrl_v_and_legacy_syn() {
        assert!(is_paste_key(KeyCode::Char('v'), KeyModifiers::CONTROL));
        assert!(is_paste_key(KeyCode::Char('V'), KeyModifiers::CONTROL));
        assert!(is_paste_key(KeyCode::Char('\u{16}'), KeyModifiers::NONE));
        assert!(is_paste_key(KeyCode::Insert, KeyModifiers::SHIFT));
        assert!(!is_paste_key(KeyCode::Char('v'), KeyModifiers::NONE));
        assert!(!is_paste_key(KeyCode::Char('x'), KeyModifiers::CONTROL));
    }

    #[test]
    fn paste_image_button_hit_wins_over_composer() {
        let hits = vec![
            (Rect::new(0, 0, 40, 8), Hit::Composer),
            (Rect::new(16, 0, 12, 1), Hit::PasteImage),
        ];
        assert_eq!(hit_at(&hits, 18, 0), Some(Hit::PasteImage));
        assert_eq!(hit_at(&hits, 2, 2), Some(Hit::Composer));
    }

    #[test]
    fn paste_image_click_attaches_or_explains() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = test_app();
        app.session.workspace = dir.path().to_path_buf();
        app.hits = vec![(Rect::new(16, 0, 12, 1), Hit::PasteImage)];
        let (mut opts, _sink, _tx) = dummy_key_env();
        handle_mouse(
            &mut app,
            &mut opts,
            MouseEventKind::Down(MouseButton::Left),
            18,
            0,
            KeyModifiers::NONE,
        );
        assert!(
            !app.pending.is_empty()
                || app.status.contains("圖片")
                || app.status.contains("剪貼簿"),
            "button must paste an image or say the clipboard is empty: {}",
            app.status
        );
    }

    #[test]
    fn decoded_dib_attaches_through_paste_pipeline() {
        let dir = tempfile::tempdir().unwrap();
        let mut dib = vec![0u8; 40 + 16];
        dib[0..4].copy_from_slice(&40u32.to_le_bytes());
        dib[4..8].copy_from_slice(&2i32.to_le_bytes());
        dib[8..12].copy_from_slice(&2i32.to_le_bytes());
        dib[12..14].copy_from_slice(&1u16.to_le_bytes());
        dib[14..16].copy_from_slice(&32u16.to_le_bytes());
        dib[20..24].copy_from_slice(&16u32.to_le_bytes());
        for px in dib[40..].chunks_exact_mut(4) {
            px.copy_from_slice(&[0, 0, 255, 255]);
        }
        let img = crate::clipimg::from_dib_bytes(&dib).expect("dib");
        let mut app = test_app();
        app.session.workspace = dir.path().to_path_buf();
        let rel = crate::vision::save_user_image(&app.session.workspace, &img).unwrap();
        assert!(app.attach_pending(rel));
        assert_eq!(app.pending.len(), 1);
        assert!(app.pending[0].starts_with(".groka/inbox/"));
    }

    fn dummy_key_env() -> (
        TuiOptions,
        Arc<FanoutSink>,
        mpsc::UnboundedSender<(String, crate::agent::RunOutcome)>,
    ) {
        let (tx, _rx) = mpsc::unbounded_channel();
        let opts = TuiOptions {
            model: "grok-4.6".into(),
            events: PathBuf::from("events.jsonl"),
            workspace: PathBuf::from("."),
            max_turns: 0,
            web_search: false,
            dispatcher: false,
            child_model: String::new(),
            reasoning_effort: ReasoningEffort::High,
        };
        (opts, Arc::new(FanoutSink { sinks: vec![] }), tx)
    }

    #[test]
    fn web_submission_preserves_terminal_draft_and_deduplicates() {
        let mut app = test_app();
        app.logged_in = true;
        app.running = true;
        let (inbox, _rx) = mpsc::unbounded_channel();
        app.inbox_tx = Some(inbox);
        app.edit = Edit::at_end("terminal draft".into());
        let (mut opts, sink, tx) = dummy_key_env();
        let cmd = UiCommand::SubmitText { session_id: app.current_id.clone(), request_id: "web-1".into(), text: "web message".into(), insert: false };
        apply_ui_command(&mut app, &mut opts, &sink, &tx, cmd.clone());
        apply_ui_command(&mut app, &mut opts, &sink, &tx, cmd);
        assert_eq!(app.edit.text, "terminal draft");
        assert_eq!(app.queue.len(), 1);
        assert_eq!(app.queue[0].text, "web message");
        assert_eq!(app.web_receipts, vec![("web-1".into(), "已接收".into())]);
    }

    #[test]
    fn shift_enter_inserts_newline_in_terminal() {
        let mut app = test_app();
        app.edit = Edit::at_end("line".into());
        let (mut opts, sink, tx) = dummy_key_env();
        handle_key(&mut app, &mut opts, KeyCode::Enter, KeyModifiers::SHIFT, &sink, &tx);
        assert_eq!(app.edit.text, "line\n");
        assert!(app.queue.is_empty());
    }

    #[test]
    fn stopping_holds_the_queue_until_another_user_turn() {
        let mut app = test_app();
        app.running = true;
        app.activity = "中斷中".into();
        let (inbox, mut rx) = mpsc::unbounded_channel();
        app.inbox_tx = Some(inbox);
        app.queue.push_back("later".into());
        app.apply_event(AgentEvent::AwaitingInput { meta: test_meta() });
        flush_queue(&mut app);
        assert_eq!(app.status, "已停止");
        assert_eq!(app.queue.len(), 1);
        assert!(rx.try_recv().is_err());
        app.status = "待命".into();
        flush_queue(&mut app);
        assert_eq!(rx.try_recv().unwrap().text, "later");
    }

    #[test]
    fn web_queue_edit_does_not_overwrite_the_next_item_after_queue_moves() {
        let mut app = test_app();
        app.queue.push_back("next item".into());
        let (mut opts, sink, tx) = dummy_key_env();
        let cmd = UiCommand::UpdateQueue { session_id: app.current_id.clone(), request_id: "edit-1".into(),
            index: 0, expected: "already consumed".into(), text: "edited text".into() };
        apply_ui_command(&mut app, &mut opts, &sink, &tx, cmd);
        assert_eq!(app.queue[0].text, "next item");
        assert!(app.web_receipts[0].1.starts_with("未儲存"));
    }

    #[test]
    fn failed_and_cancelled_tools_are_not_reported_as_success() {
        for (output, phase) in [(r#"{"exit_code":1}"#, "失敗"), (r#"{"error":"interrupted","cancelled":true}"#, "已停止")] {
            let mut app = test_app();
            app.push_tool_start("c1".into(), "run_command".into(), serde_json::json!({}));
            app.finish_tool("c1", "run_command", output.into());
            let Row::Tools(g) = &app.rows[0] else { panic!("tool row"); };
            assert_eq!(g.calls[0].phase, phase);
            assert!(g.calls[0].done);
        }
    }

    #[test]
    fn workbench_renders_at_terminal_sizes_with_every_view() {
        for (width, height) in [(80, 24), (140, 40)] {
            for view in SideView::ALL {
                for bottom in [None, Some(BottomTab::Tools), Some(BottomTab::Output), Some(BottomTab::Events)] {
                    let mut app = test_app();
                    app.side_open = true;
                    app.side_view = view;
                    app.bottom = bottom;
                    app.task = TaskHub::new("");
                    app.task.start_goal("驗證控制台");
                    app.agent_spawned("", "coder", "fix", "grok-3-mini");
                    app.bench.log_event("", "error", "boom".into());
                    let opts = test_opts("grok-4.6", ReasoningEffort::High);
                    let mut terminal = Terminal::new(ratatui::backend::TestBackend::new(width, height)).unwrap();
                    terminal.draw(|f| { draw(f, &mut app, &opts); }).unwrap();
                    let text: String = terminal.backend().buffer().content.iter().map(|c| c.symbol()).collect();
                    let compact: String = text.chars().filter(|c| !c.is_whitespace()).collect();
                    if width >= SIDEBAR_MIN_TERM {
                        assert!(compact.contains("主對話"), "tab strip at {width}x{height}");
                    } else {
                        // Narrow: the side bar floats over the editor.
                        assert!(compact.contains(view.title()), "overlay {view:?} at {width}x{height}");
                    }
                    if width == 140 {
                        assert!(app.chat_inner.width >= 70, "docked side bar leaves room for chat");
                        assert!(compact.contains(view.title()), "side view title {view:?}");
                    }
                    if view == SideView::Task && width == 140 {
                        assert!(compact.contains("驗證控制台"));
                    }
                    if bottom == Some(BottomTab::Events) && width >= SIDEBAR_MIN_TERM {
                        assert!(compact.contains("boom"));
                    }
                }
            }
        }
    }

    #[test]
    fn workbench_keys_toggle_side_bar_panel_and_tabs() {
        let mut app = test_app();
        app.agent_spawned("", "coder", "fix", "");
        app.agent_spawned("", "tester", "test", "");
        let (mut opts, sink, tx) = dummy_key_env();
        app.side_open = true;
        handle_key(&mut app, &mut opts, KeyCode::Char('b'), KeyModifiers::CONTROL, &sink, &tx);
        assert!(!app.side_open);
        handle_key(&mut app, &mut opts, KeyCode::Char('2'), KeyModifiers::ALT, &sink, &tx);
        assert!(app.side_open);
        assert_eq!(app.side_view, SideView::Agents);
        handle_key(&mut app, &mut opts, KeyCode::Char('j'), KeyModifiers::CONTROL, &sink, &tx);
        assert_eq!(app.bottom, Some(BottomTab::Tools));
        handle_key(&mut app, &mut opts, KeyCode::F(4), KeyModifiers::NONE, &sink, &tx);
        assert_eq!(app.bottom, None);

        app.open_agent_tab("coder");
        app.open_agent_tab("tester");
        assert_eq!(app.bench.active.as_deref(), Some("tester"));
        handle_key(&mut app, &mut opts, KeyCode::Left, KeyModifiers::ALT, &sink, &tx);
        assert_eq!(app.bench.active.as_deref(), Some("coder"));
        handle_key(&mut app, &mut opts, KeyCode::Char('0'), KeyModifiers::ALT, &sink, &tx);
        assert!(app.bench.active.is_none());
        handle_key(&mut app, &mut opts, KeyCode::Right, KeyModifiers::ALT, &sink, &tx);
        assert_eq!(app.bench.active.as_deref(), Some("coder"));
        handle_key(&mut app, &mut opts, KeyCode::Char('w'), KeyModifiers::CONTROL, &sink, &tx);
        assert_eq!(app.bench.open, vec!["tester".to_string()]);
    }

    #[test]
    fn workbench_clicks_open_agents_changes_and_tools() {
        let mut app = test_app();
        app.agent_spawned("", "coder", "fix", "");
        app.with_agent("coder", |app| {
            app.push_tool_start("w1".into(), "write_file".into(), serde_json::json!({"path": "a.rs"}));
            app.finish_tool("w1", "write_file", r#"{"path":"a.rs","kind":"create","diff":"+x"}"#.into());
        });
        app.bench.tool_started("coder", "w1", "write_file", &serde_json::json!({"path": "a.rs"}));
        assert!(handle_workbench_click(&mut app, Hit::SideAgent(0)));
        assert_eq!(app.bench.active.as_deref(), Some("coder"));
        assert!(handle_workbench_click(&mut app, Hit::EditorTab(0)));
        assert!(app.bench.active.is_none());

        let changes = app.all_file_changes();
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].0, "coder");
        assert!(handle_workbench_click(&mut app, Hit::SideChange(0)));
        assert_eq!(app.bench.active.as_deref(), Some("coder"));
        assert_eq!(app.bench.agent("coder").unwrap().view.open_tool, Some((0, 0)));

        app.bench.active = None;
        app.bottom = Some(BottomTab::Tools);
        assert!(handle_workbench_click(&mut app, Hit::BottomRow(0)));
        assert_eq!(app.bench.active.as_deref(), Some("coder"), "a tool row opens its agent tab");
        assert!(handle_workbench_click(&mut app, Hit::EditorTabClose(1)));
        assert!(app.bench.open.is_empty());
        assert!(!handle_workbench_click(&mut app, Hit::Gear));
    }

    #[test]
    fn legacy_ctrl_v_syn_does_not_insert_control_char() {
        let mut app = test_app();
        app.edit = Edit::at_end("keep".into());
        let (mut opts, sink, tx) = dummy_key_env();
        handle_key(
            &mut app,
            &mut opts,
            KeyCode::Char('\u{16}'),
            KeyModifiers::NONE,
            &sink,
            &tx,
        );
        assert!(
            !app.edit.text.contains('\u{16}'),
            "ctrl-v SYN must paste clipboard, not insert U+0016: {}",
            app.edit.text
        );
    }

    #[test]
    fn copy_selection_of_row_does_not_quit() {
        let mut app = test_app();
        app.push(Row::User("copy me".into()));
        let n = selectable_text(&app.rows[0]).unwrap().chars().count();
        app.chat_sel = ChatSel::Text {
            anchor: ChatPos { row: 0, idx: 0 },
            caret: ChatPos { row: 0, idx: n },
        };
        let (tx, _rx) = mpsc::unbounded_channel();
        let mut opts = TuiOptions {
            model: "grok-4.6".into(),
            events: PathBuf::from("events.jsonl"),
            workspace: PathBuf::from("."),
            max_turns: 0,
            web_search: false,
            dispatcher: false,
            child_model: String::new(),
            reasoning_effort: ReasoningEffort::High,
        };
        let sink = Arc::new(FanoutSink { sinks: vec![] });
        let quit = handle_key(
            &mut app,
            &mut opts,
            KeyCode::Char('c'),
            KeyModifiers::CONTROL,
            &sink,
            &tx,
        );
        assert!(!quit, "ctrl-c with a selected row must copy, not quit");
    }

    #[test]
    fn ctrl_q_quits_even_when_a_row_is_selected() {
        let mut app = test_app();
        app.push(Row::User("copy me".into()));
        let n = selectable_text(&app.rows[0]).unwrap().chars().count();
        app.chat_sel = ChatSel::Text {
            anchor: ChatPos { row: 0, idx: 0 },
            caret: ChatPos { row: 0, idx: n },
        };
        let (tx, _rx) = mpsc::unbounded_channel();
        let mut opts = TuiOptions {
            model: "grok-4.6".into(),
            events: PathBuf::from("events.jsonl"),
            workspace: PathBuf::from("."),
            max_turns: 0,
            web_search: false,
            dispatcher: false,
            child_model: String::new(),
            reasoning_effort: ReasoningEffort::High,
        };
        let sink = Arc::new(FanoutSink { sinks: vec![] });
        let quit = handle_key(
            &mut app,
            &mut opts,
            KeyCode::Char('q'),
            KeyModifiers::CONTROL,
            &sink,
            &tx,
        );
        assert!(quit, "ctrl-q must quit even with a chat selection");
    }

    #[test]
    fn wheel_scrolls_three_lines_and_returns_to_bottom() {
        let mut app = test_app();
        app.chat_inner = Rect::new(0, 0, 40, 30);
        let (mut opts, _sink, _tx) = dummy_key_env();
        handle_mouse(
            &mut app,
            &mut opts,
            MouseEventKind::ScrollUp,
            20,
            10,
            KeyModifiers::NONE,
        );
        assert_eq!(chat_scroll_step(&app), 3);
        assert_eq!(app.scroll, 3);
        assert!(!app.stick_bottom);
        handle_mouse(
            &mut app,
            &mut opts,
            MouseEventKind::ScrollDown,
            20,
            10,
            KeyModifiers::NONE,
        );
        assert_eq!(app.scroll, 0);
        assert!(app.stick_bottom);
    }

    #[test]
    fn scrollbar_offset_maps_top_and_bottom() {
        assert_eq!(scrollbar_offset(100, 20, 4, 0), 100);
        assert_eq!(scrollbar_offset(100, 20, 4, 16), 0);
        assert_eq!(scrollbar_thumb_rel(100, 20, 4, 0), 16);
        assert_eq!(scrollbar_thumb_rel(100, 20, 4, 100), 0);
    }

    #[test]
    fn esc_interrupts_running_work_and_does_not_quit() {
        let mut app = test_app();
        let flag = CancelFlag::new();
        app.running = true;
        app.cancel = Some(flag.clone());
        app.status = "已登出".into();
        let (mut opts, sink, tx) = dummy_key_env();
        let quit = handle_key(&mut app, &mut opts, KeyCode::Esc, KeyModifiers::NONE, &sink, &tx);
        assert!(!quit);
        assert!(flag.is_set(), "Esc must trip the in-flight cancel flag");
        assert_eq!(app.status, "中斷中");
        assert!(!app.streaming);
    }

    #[test]
    fn esc_when_idle_does_not_trip_cancel() {
        let mut app = test_app();
        let flag = CancelFlag::new();
        app.running = false;
        app.cancel = Some(flag.clone());
        let (mut opts, sink, tx) = dummy_key_env();
        let quit = handle_key(&mut app, &mut opts, KeyCode::Esc, KeyModifiers::NONE, &sink, &tx);
        assert!(!quit);
        assert!(!flag.is_set());
    }

    #[test]
    fn drag_scrollbar_thumb_moves_chat_offset() {
        let mut app = test_app();
        for i in 0..40 {
            app.push(Row::User(format!("msg-{i:02}").into()));
        }
        let (mut opts, _sink, _tx) = dummy_key_env();
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(90, 32)).unwrap();
        terminal
            .draw(|f| {
                let _ = draw(f, &mut app, &opts);
            })
            .unwrap();
        let thumb = app
            .hits
            .iter()
            .rev()
            .find(|(_, h)| *h == Hit::ScrollThumb)
            .map(|(r, _)| *r)
            .expect("overflowing chat must show a drag thumb");
        handle_mouse(
            &mut app,
            &mut opts,
            MouseEventKind::Down(MouseButton::Left),
            thumb.x,
            thumb.y,
            KeyModifiers::NONE,
        );
        assert!(app.scroll_grab.is_some());
        let top = app.chat_bar.y;
        handle_mouse(
            &mut app,
            &mut opts,
            MouseEventKind::Drag(MouseButton::Left),
            thumb.x,
            top,
            KeyModifiers::NONE,
        );
        assert!(!app.stick_bottom, "dragging the thumb to the top must unstick");
        assert!(app.scroll > 0, "scroll offset from bottom should increase");
    }

    fn buf_hay(buf: &ratatui::buffer::Buffer) -> String {
        buf.content.iter().map(|c| c.symbol()).collect()
    }

    #[test]
    fn wheel_up_from_bottom_stays_near_latest() {
        let mut app = test_app();
        for i in 0..40 {
            app.push(Row::User(format!("msg-{i:02}").into()));
        }
        let opts = TuiOptions {
            model: "grok-4.6".into(),
            events: PathBuf::from("events.jsonl"),
            workspace: PathBuf::from("."),
            max_turns: 0,
            web_search: false,
            dispatcher: false,
            child_model: String::new(),
            reasoning_effort: ReasoningEffort::High,
        };
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(90, 32)).unwrap();
        let at_bottom = terminal
            .draw(|f| {
                let _ = draw(f, &mut app, &opts);
            })
            .unwrap();
        let hay = buf_hay(at_bottom.buffer);
        assert!(hay.contains("msg-39"), "{hay}");
        assert!(
            !hay.contains("msg-00"),
            "bottom view must not start at the oldest: {hay}"
        );
        let (mut opts, _sink, _tx) = dummy_key_env();
        handle_mouse(
            &mut app,
            &mut opts,
            MouseEventKind::ScrollUp,
            40,
            10,
            KeyModifiers::NONE,
        );
        let after = terminal
            .draw(|f| {
                let _ = draw(f, &mut app, &opts);
            })
            .unwrap();
        let hay = buf_hay(after.buffer);
        assert!(
            !hay.contains("msg-00"),
            "one wheel tick must not jump to the top: {hay}"
        );
        assert!(
            hay.contains("msg-36") || hay.contains("msg-35") || hay.contains("msg-37"),
            "should still be near the latest messages: {hay}"
        );
    }

    #[test]
    fn clipped_sixel_keeps_blank_space_not_a_placeholder_line() {
        let dir = tempfile::tempdir().unwrap();
        let rel = write_red_png(dir.path(), "red.png");
        let mut app = test_app();
        app.session.workspace = dir.path().to_path_buf();
        let mut picker = Picker::from_fontsize((8, 16));
        picker.set_protocol_type(ratatui_image::picker::ProtocolType::Sixel);
        app.picker = Some(picker);
        for i in 0..30 {
            app.push(Row::User(format!("pad-{i}").into()));
        }
        app.push(Row::User(UserMsg {
            text: "see".into(),
            images: vec![rel],
        }));
        let opts = TuiOptions {
            model: "grok-4.6".into(),
            events: PathBuf::from("events.jsonl"),
            workspace: app.session.workspace.clone(),
            max_turns: 0,
            web_search: false,
            dispatcher: false,
            child_model: String::new(),
            reasoning_effort: ReasoningEffort::High,
        };
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(90, 32)).unwrap();
        terminal
            .draw(|f| {
                let _ = draw(f, &mut app, &opts);
            })
            .unwrap();
        assert!(
            !app.graphic_blits.is_empty(),
            "fully visible sixel at the bottom should blit"
        );
        app.stick_bottom = false;
        app.scroll = 1;
        let clipped = terminal
            .draw(|f| {
                let _ = draw(f, &mut app, &opts);
            })
            .unwrap();
        let hay = buf_hay(clipped.buffer);
        assert!(
            !hay.contains("捲動以完整顯示"),
            "partial image must keep blank height, not collapse: {hay}"
        );
        assert!(
            app.graphic_blits.is_empty(),
            "sixel that is not fully on screen must not blit"
        );
    }

    #[test]
    fn jump_bottom_click_sticks_to_latest() {
        let mut app = test_app();
        app.stick_bottom = false;
        app.scroll = 20;
        app.hits = vec![(Rect::new(20, 18, 3, 1), Hit::JumpBottom)];
        let (mut opts, _sink, _tx) = dummy_key_env();
        handle_mouse(
            &mut app,
            &mut opts,
            MouseEventKind::Down(MouseButton::Left),
            21,
            18,
            KeyModifiers::NONE,
        );
        assert!(app.stick_bottom);
        assert_eq!(app.scroll, 0);
    }

    #[test]
    fn jump_bottom_button_only_when_scrolled_up() {
        let mut app = test_app();
        for i in 0..40 {
            app.push(Row::User(format!("line {i}").into()));
        }
        let opts = TuiOptions {
            model: "grok-4.6".into(),
            events: PathBuf::from("events.jsonl"),
            workspace: PathBuf::from("."),
            max_turns: 0,
            web_search: false,
            dispatcher: false,
            child_model: String::new(),
            reasoning_effort: ReasoningEffort::High,
        };
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(90, 32)).unwrap();
        terminal
            .draw(|f| {
                let _ = draw(f, &mut app, &opts);
            })
            .unwrap();
        assert!(
            !app.hits.iter().any(|(_, h)| *h == Hit::JumpBottom),
            "hidden while already at the bottom"
        );
        app.stick_bottom = false;
        app.scroll = 12;
        terminal
            .draw(|f| {
                let _ = draw(f, &mut app, &opts);
            })
            .unwrap();
        assert!(
            app.hits.iter().any(|(_, h)| *h == Hit::JumpBottom),
            "▼ must appear above the composer after scrolling up"
        );
    }

    #[test]
    fn click_image_opens_viewer_esc_closes() {
        let dir = tempfile::tempdir().unwrap();
        let rel = write_red_png(dir.path(), "shot.png");
        let mut app = test_app();
        app.session.workspace = dir.path().to_path_buf();
        app.image_hits = vec![rel.clone()];
        app.hits = vec![(Rect::new(0, 4, 20, 6), Hit::ChatImage(0))];
        let (mut opts, sink, tx) = dummy_key_env();
        handle_mouse(
            &mut app,
            &mut opts,
            MouseEventKind::Down(MouseButton::Left),
            2,
            5,
            KeyModifiers::NONE,
        );
        assert_eq!(app.image_view.as_deref(), Some(rel.as_str()));
        assert!(matches!(app.chat_sel, ChatSel::Image(ref p) if p == &rel));
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(90, 32)).unwrap();
        terminal
            .draw(|f| {
                let _ = draw(f, &mut app, &opts);
            })
            .unwrap();
        assert!(app.hits.iter().any(|(_, h)| *h == Hit::ImageView));
        let quit = handle_key(&mut app, &mut opts, KeyCode::Esc, KeyModifiers::NONE, &sink, &tx);
        assert!(!quit);
        assert!(app.image_view.is_none());
    }

    #[test]
    fn tool_started_shows_command_and_path() {
        let cmd = tool_started_line("run_command", &serde_json::json!({"command": "git status"}));
        assert!(cmd.contains("$ git status"), "{cmd}");
        let named = tool_started_line(
            "run_background",
            &serde_json::json!({"command": "npm start", "window": "preview"}),
        );
        assert!(named.contains("[preview]"), "{named}");
        assert!(named.contains("npm start"), "{named}");
        let bg = tool_started_line(
            "run_background",
            &serde_json::json!({"command": "npm run dev"}),
        );
        assert!(bg.contains("$ npm run dev"), "{bg}");
        let kill = tool_started_line("kill_background", &serde_json::json!({"name": "dev"}));
        assert!(kill.contains("dev"), "{kill}");
        let path = tool_started_line("write_file", &serde_json::json!({"path": "src/a.rs"}));
        assert!(path.contains("src/a.rs"), "{path}");
        let edit = tool_started_line("edit_file", &serde_json::json!({"path": "src/a.rs"}));
        assert!(edit.contains("src/a.rs"), "{edit}");
        let ask = tool_started_line("ask_user", &serde_json::json!({"question": "挑一個"}));
        assert!(ask.contains("挑一個"), "{ask}");
        let mem = tool_started_line(
            "project_memory",
            &serde_json::json!({"action": "write", "path": "goal.md"}),
        );
        assert!(mem.contains("goal.md"), "{mem}");
        assert!(mem.contains("write"), "{mem}");
        let timer = tool_started_line(
            "timer",
            &serde_json::json!({"action": "start", "seconds": 30, "block": true, "command": "echo hi"}),
        );
        assert!(timer.contains("30"), "{timer}");
        assert!(timer.contains("阻塞"), "{timer}");
        assert!(timer.contains("echo hi"), "{timer}");
        let cancel = tool_started_line(
            "timer",
            &serde_json::json!({"action": "cancel", "name": "n1"}),
        );
        assert!(cancel.contains("cancel"), "{cancel}");
        assert!(cancel.contains("n1"), "{cancel}");
    }

    #[test]
    fn diff_row_colors_plus_and_minus() {
        let rendered: String = diff_lines("--- a/a.txt\n+++ b/a.txt\n@@ -1 +1 @@\n-old\n+new\n")
            .iter()
            .map(|l| format!("{l:?}"))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(rendered.contains("-old"), "{rendered}");
        assert!(rendered.contains("+new"), "{rendered}");
    }

    #[test]
    fn model_delta_appends_then_finished_does_not_duplicate() {
        let mut app = test_app();
        let meta = crate::events::EventMeta {
            ts: chrono::Utc::now(),
            agent_name: "root".into(),
            run_id: "r".into(),
            parent_run_id: None,
            path: String::new(),
        };
        app.apply_event(AgentEvent::ModelDelta {
            meta: meta.clone(),
            text: "Hel".into(),
        });
        app.apply_event(AgentEvent::ModelDelta {
            meta: meta.clone(),
            text: "lo".into(),
        });
        app.apply_event(AgentEvent::ModelFinished {
            meta,
            text: "Hello".into(),
            finish: "stop".into(),
            input_tokens: 10,
            cached_tokens: 0,
        });
        let agents: Vec<&str> = app
            .rows
            .iter()
            .filter_map(|r| match r {
                Row::Agent(s) => Some(s.text.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(agents, vec!["Hello"]);
    }

    #[test]
    fn finished_after_trailing_think_does_not_duplicate_answer() {
        let mut app = test_app();
        let meta = crate::events::EventMeta {
            ts: chrono::Utc::now(),
            agent_name: "root".into(),
            run_id: "r".into(),
            parent_run_id: None,
            path: String::new(),
        };
        app.apply_event(AgentEvent::ModelDelta {
            meta: meta.clone(),
            text: "Hello".into(),
        });
        // A reasoning delta lands after the streamed text, pushing a think row
        // behind the agent row.
        app.apply_event(AgentEvent::ReasoningDelta {
            meta: meta.clone(),
            text: "wrap up".into(),
        });
        app.apply_event(AgentEvent::ModelFinished {
            meta,
            text: "Hello".into(),
            finish: "stop".into(),
            input_tokens: 10,
            cached_tokens: 0,
        });
        let agents: Vec<&str> = app
            .rows
            .iter()
            .filter_map(|r| match r {
                Row::Agent(s) => Some(s.text.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(agents, vec!["Hello"], "final text must not be appended twice");
    }

    #[test]
    fn stop_with_same_text_after_tools_does_not_duplicate_answer() {
        let mut app = test_app();
        let meta = crate::events::EventMeta {
            ts: chrono::Utc::now(),
            agent_name: "root".into(),
            run_id: "r".into(),
            parent_run_id: None,
            path: String::new(),
        };
        app.apply_event(AgentEvent::ModelFinished {
            meta: meta.clone(),
            text: "結論".into(),
            finish: "tool_calls".into(),
            input_tokens: 5,
            cached_tokens: 0,
        });
        app.apply_event(AgentEvent::ToolStarted {
            meta: meta.clone(),
            call_id: "1".into(),
            name: "now".into(),
            args: serde_json::json!({}),
            kind: "function".into(),
        });
        app.apply_event(AgentEvent::ToolFinished {
            meta: meta.clone(),
            call_id: "1".into(),
            name: "now".into(),
            output: "2026".into(),
        });
        app.apply_event(AgentEvent::ModelFinished {
            meta,
            text: "結論".into(),
            finish: "stop".into(),
            input_tokens: 5,
            cached_tokens: 0,
        });
        let agents: Vec<&str> = app
            .rows
            .iter()
            .filter_map(|r| match r {
                Row::Agent(s) => Some(s.text.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(agents, vec!["結論"], "identical stop text must reuse the turn's agent row");
    }

    #[test]
    fn new_answer_after_tools_still_gets_its_own_row() {
        let mut app = test_app();
        let meta = crate::events::EventMeta {
            ts: chrono::Utc::now(),
            agent_name: "root".into(),
            run_id: "r".into(),
            parent_run_id: None,
            path: String::new(),
        };
        app.apply_event(AgentEvent::ModelDelta {
            meta: meta.clone(),
            text: "初步想法".into(),
        });
        app.apply_event(AgentEvent::ToolStarted {
            meta: meta.clone(),
            call_id: "1".into(),
            name: "now".into(),
            args: serde_json::json!({}),
            kind: "function".into(),
        });
        app.apply_event(AgentEvent::ToolFinished {
            meta: meta.clone(),
            call_id: "1".into(),
            name: "now".into(),
            output: "2026".into(),
        });
        app.apply_event(AgentEvent::ModelFinished {
            meta,
            text: "最終答案 2026".into(),
            finish: "stop".into(),
            input_tokens: 5,
            cached_tokens: 0,
        });
        let agents: Vec<&str> = app
            .rows
            .iter()
            .filter_map(|r| match r {
                Row::Agent(s) => Some(s.text.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(agents, vec!["初步想法", "最終答案 2026"]);
    }

    fn test_app() -> App {
        App {
            web_receipts: Vec::new(),
            side_view: SideView::Sessions,
            side_open: false,
            side_scroll: 0,
            bottom: None,
            bottom_scroll: 0,
            side_area: Rect::default(),
            bottom_area: Rect::default(),
            web_sent: HashMap::new(),
            web_cache: HashMap::new(),
            rows: vec![],
            edit: Edit::default(),
            status: String::new(),
            cache: String::new(),
            child_count: 0,
            running: false,
            awaiting: false,
            logged_in: true,
            auth_path: PathBuf::from("missing-xai-auth.json"),
            login_ui: LoginUi::Idle,
            login_gen: 0,
            want_login: false,
            scroll: 0,
            stick_bottom: true,
            send_mode: SendMode::Queue,
            queue: VecDeque::new(),
            inbox_tx: None,
            cancel: None,
            knobs: Arc::new(Mutex::new(SessionKnobs {
                model: "grok-4.6".into(),
                reasoning_effort: ReasoningEffort::High,
                send_reasoning: true,
                server_tools: vec![],
                dispatcher: false,
                child_model: String::new(),
            })),
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
            current_id: "s".into(),
            session: dummy_session(),
            parked: HashMap::new(),
            sessions: vec![],
            store: None,
            launch_workspace: PathBuf::from("."),
            sidebar_ids: Vec::new(),
            rename: None,
            rename_inner: Rect::default(),
            work_started: None,
            queue_edit: None,
            composer_stash: None,
            pending: Vec::new(),
            chat_sel: ChatSel::None,
            preview: HashMap::new(),
            picker: None,
            image_proto: HashMap::new(),
            image_cells: HashMap::new(),
            graphic_blits: Vec::new(),
            last_graphic_blits: Vec::new(),
            image_hits: Vec::new(),
            image_view: None,
            bench: Workbench::default(),
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
            task: TaskHub::new("s".to_string()),
            task_ui: None,
            task_draft_inner: Rect::default(),
            task_action: None,
            skills: Arc::new(Mutex::new(SkillStore::open_at(
                PathBuf::from("missing-groka-skills"),
                PathBuf::from("missing-home"),
            ))),
            skill_list: Vec::new(),
            skill_cursor: 0,
            skill_scroll: 0,
            skill_view: None,
            web_url: None,
            composer_seq: 0,
            web_composer_seq: 0,
        }
    }

    fn parked_stub(id: &str) -> ParkedChat {
        let mut session = dummy_session();
        session.id = id.to_string();
        ParkedChat {
            session,
            rows: vec![],
            status: "工作中".into(),
            cache: String::new(),
            child_count: 0,
            running: true,
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
            bench: Workbench::default(),
            monitors: Vec::new(),
            backgrounds: Vec::new(),
            inspector: None,
            inspector_scroll: 0,
            task: TaskHub::new(id.to_string()),
        }
    }

    fn test_meta() -> crate::events::EventMeta {
        crate::events::EventMeta {
            ts: chrono::Utc::now(),
            agent_name: "root".into(),
            run_id: "r".into(),
            parent_run_id: None,
            path: String::new(),
        }
    }

    #[test]
    fn notice_goes_to_chat_not_composer() {
        let mut app = test_app();
        app.edit = Edit::at_end("keep me".into());
        app.apply_event(AgentEvent::Notice {
            meta: test_meta(),
            message: "快取命中 10%（目標 ≥90%）turn=2 cached=10/1000".into(),
        });
        assert_eq!(app.edit.text, "keep me");
        match app.rows.last() {
            Some(Row::Meta(s)) => assert!(s.contains("快取命中"), "{s}"),
            _ => panic!("expected meta row in chat, not the composer"),
        }
    }

    #[test]
    fn tools_in_one_turn_collapse_to_one_row() {
        let mut app = test_app();
        let meta = test_meta();
        app.apply_event(AgentEvent::ToolStarted {
            meta: meta.clone(),
            call_id: "1".into(),
            name: "list_dir".into(),
            args: serde_json::json!({"path": "."}),
            kind: "function".into(),
        });
        app.apply_event(AgentEvent::ToolStarted {
            meta,
            call_id: "2".into(),
            name: "run_command".into(),
            args: serde_json::json!({"command": "git status"}),
            kind: "function".into(),
        });
        assert_eq!(app.rows.len(), 1);
        let Row::Tools(g) = &app.rows[0] else {
            panic!("expected tool group");
        };
        assert!(!g.expanded, "groups start collapsed");
        assert_eq!(g.calls.len(), 2);
        let vis = chat_logical_rows(&mut app, 80);
        assert_eq!(vis.len(), 1, "collapsed group is one clickable line");
        assert_eq!(vis[0].hit, Some(Hit::ToolGroup(0)));
        assert!(
            format!("{:?}", vis[0].line).contains("2 個工具"),
            "{:?}",
            vis[0].line
        );
    }

    #[test]
    fn reasoning_collapses_like_tools_and_stays_out_of_answer() {
        let mut app = test_app();
        let meta = test_meta();
        app.apply_event(AgentEvent::ReasoningDelta {
            meta: meta.clone(),
            text: "use ".into(),
        });
        app.apply_event(AgentEvent::ReasoningDelta {
            meta: meta.clone(),
            text: "energy".into(),
        });
        app.apply_event(AgentEvent::ModelDelta {
            meta: meta.clone(),
            text: "42".into(),
        });
        assert_eq!(app.rows.len(), 2);
        let Row::Think(t) = &app.rows[0] else {
            panic!("expected think row");
        };
        assert!(!t.expanded, "thinking starts collapsed");
        assert!(t.done, "answer seals the think block");
        assert_eq!(t.text, "use energy");
        let Row::Agent(s) = &app.rows[1] else {
            panic!("expected agent row");
        };
        assert_eq!(s.text, "42");
        let vis = chat_logical_rows(&mut app, 80);
        assert_eq!(vis.len(), 2, "collapsed think is one line plus the answer");
        assert_eq!(vis[0].hit, Some(Hit::Think(0)));
        assert!(format!("{:?}", vis[0].line).contains("思考"), "{:?}", vis[0].line);
        if let Some(Row::Think(t)) = app.rows.get_mut(0) {
            t.expanded = true;
        }
        let vis = chat_logical_rows(&mut app, 80);
        let rendered: String = vis
            .iter()
            .map(|l| format!("{:?}", l.line))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(rendered.contains("use energy"), "{rendered}");
    }

    #[test]
    fn new_turn_starts_a_fresh_think_row() {
        let mut app = test_app();
        let meta = test_meta();
        app.apply_event(AgentEvent::ReasoningDelta {
            meta: meta.clone(),
            text: "first".into(),
        });
        app.apply_event(AgentEvent::TurnStarted {
            meta: meta.clone(),
            turn: 2,
        });
        app.apply_event(AgentEvent::ReasoningDelta {
            meta,
            text: "second".into(),
        });
        let thinks: Vec<&str> = app
            .rows
            .iter()
            .filter_map(|r| match r {
                Row::Think(t) => Some(t.text.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(thinks, ["first", "second"]);
    }

    #[test]
    fn server_search_progress_coalesces_into_one_call() {
        let mut app = test_app();
        let meta = test_meta();
        app.apply_event(AgentEvent::ServerToolObserved {
            meta: meta.clone(),
            kind: "response.web_search_call.in_progress".into(),
            payload: serde_json::json!({"action":{"query":"Falcon 9"}}),
        });
        app.apply_event(AgentEvent::ServerToolObserved {
            meta: meta.clone(),
            kind: "response.web_search_call.searching".into(),
            payload: serde_json::json!({"action":{"query":"Falcon 9"}}),
        });
        app.apply_event(AgentEvent::ServerToolObserved {
            meta,
            kind: "web_search_call".into(),
            payload: serde_json::json!({"status":"in_progress","action":{"query":"Falcon 9"}}),
        });
        assert_eq!(app.rows.len(), 1);
        let Row::Tools(g) = &app.rows[0] else {
            panic!("expected tool group");
        };
        assert_eq!(g.calls.len(), 1, "progress events must not spawn extra rows");
        assert_eq!(g.calls[0].name, "web_search");
        assert_eq!(g.calls[0].phase, "進行中");
        assert!(app.activity.contains("Falcon 9"), "{}", app.activity);
        assert!(app.activity.contains("搜尋中") || app.activity.contains("進行中"), "{}", app.activity);
    }

    #[test]
    fn expanding_a_group_lists_each_call() {
        let mut app = test_app();
        let meta = test_meta();
        app.apply_event(AgentEvent::ToolStarted {
            meta: meta.clone(),
            call_id: "1".into(),
            name: "list_dir".into(),
            args: serde_json::json!({"path": "."}),
            kind: "function".into(),
        });
        app.apply_event(AgentEvent::ToolStarted {
            meta,
            call_id: "2".into(),
            name: "run_command".into(),
            args: serde_json::json!({"command": "echo hi"}),
            kind: "function".into(),
        });
        let Row::Tools(g) = &mut app.rows[0] else {
            panic!("expected tool group");
        };
        g.expanded = true;
        let vis = chat_logical_rows(&mut app, 80);
        assert_eq!(vis.len(), 3);
        assert_eq!(vis[1].hit, Some(Hit::ToolItem(0, 0)));
        assert_eq!(vis[2].hit, Some(Hit::ToolItem(0, 1)));
    }

    #[test]
    fn file_changed_attaches_to_the_open_call() {
        let mut app = test_app();
        let meta = test_meta();
        app.apply_event(AgentEvent::ToolStarted {
            meta: meta.clone(),
            call_id: "1".into(),
            name: "write_file".into(),
            args: serde_json::json!({"path": "a.txt"}),
            kind: "function".into(),
        });
        app.apply_event(AgentEvent::FileChanged {
            meta,
            path: "a.txt".into(),
            kind: "create".into(),
            diff: "--- a/a.txt\n+++ b/a.txt\n+hello\n".into(),
        });
        let Row::Tools(g) = &app.rows[0] else {
            panic!("expected tool group");
        };
        assert_eq!(g.calls[0].files.len(), 1);
        assert_eq!(g.calls[0].files[0].path, "a.txt");
        assert!(g.calls[0].files[0].diff.contains("+hello"));
    }

    #[test]
    fn tool_detail_shows_command_and_diff() {
        let call = ToolCall {
            call_id: String::new(),
            name: "run_command".into(),
            args: serde_json::json!({"command": "git status"}),
            output: serde_json::json!({
                "exit_code": 0,
                "stdout": "ok",
                "stderr": ""
            })
            .to_string(),
            files: vec![FileChange {
                path: "a.txt".into(),
                kind: "modify".into(),
                diff: "--- a/a.txt\n+++ b/a.txt\n@@ -1 +1 @@\n-old\n+new\n".into(),
            }],
            done: true,
            phase: "完成".into(),
        };
        let rendered: String = call_detail_lines(&call)
            .iter()
            .map(|l| format!("{l:?}"))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(rendered.contains("$ git status"), "{rendered}");
        assert!(rendered.contains("exit 0"), "{rendered}");
        assert!(rendered.contains("-old"), "{rendered}");
        assert!(rendered.contains("+new"), "{rendered}");
        assert!(rendered.contains("a.txt"), "{rendered}");
    }

    #[test]
    fn dismiss_closes_panel_then_collapses_groups() {
        let mut app = test_app();
        app.push(Row::Tools(ToolGroup {
            calls: vec![ToolCall {
                call_id: String::new(),
                name: "list_dir".into(),
                args: serde_json::json!({"path": "."}),
                output: String::new(),
                files: Vec::new(),
                done: true,
                phase: "完成".into(),
            }],
            expanded: true,
        }));
        app.open_tool = Some((0, 0));
        assert!(app.dismiss_tool_ui());
        assert!(app.open_tool.is_none());
        let Row::Tools(g) = &app.rows[0] else {
            panic!("expected tool group");
        };
        assert!(g.expanded, "first dismiss only closes the detail panel");
        assert!(app.dismiss_tool_ui());
        let Row::Tools(g) = &app.rows[0] else {
            panic!("expected tool group");
        };
        assert!(!g.expanded);
        assert!(!app.dismiss_tool_ui());
    }

    #[test]
    fn dismiss_collapses_expanded_think() {
        let mut app = test_app();
        app.push(Row::Think(Think {
            text: "use energy".into(),
            expanded: true,
            done: true,
            ..Default::default()
        }));
        assert!(app.dismiss_tool_ui());
        let Row::Think(t) = &app.rows[0] else {
            panic!("expected think row");
        };
        assert!(!t.expanded);
        assert!(!app.dismiss_tool_ui());
    }

    #[test]
    fn new_turn_starts_a_fresh_tool_group() {
        let mut app = test_app();
        let meta = test_meta();
        app.apply_event(AgentEvent::ToolStarted {
            meta: meta.clone(),
            call_id: "1".into(),
            name: "list_dir".into(),
            args: serde_json::json!({"path": "."}),
            kind: "function".into(),
        });
        app.apply_event(AgentEvent::TurnStarted {
            meta: meta.clone(),
            turn: 2,
        });
        app.apply_event(AgentEvent::ToolStarted {
            meta,
            call_id: "2".into(),
            name: "run_command".into(),
            args: serde_json::json!({"command": "ls"}),
            kind: "function".into(),
        });
        assert_eq!(app.rows.len(), 2);
        assert!(matches!(&app.rows[0], Row::Tools(_)));
        assert!(matches!(&app.rows[1], Row::Tools(_)));
    }

    #[test]
    fn dismiss_hit_sits_above_chat() {
        let hits = vec![
            (Rect::new(0, 0, 20, 10), Hit::Chat),
            (Rect::new(0, 0, 20, 10), Hit::DismissTool),
            (Rect::new(0, 4, 20, 1), Hit::ToolItem(0, 1)),
            (Rect::new(4, 2, 12, 6), Hit::ToolPanel),
        ];
        assert_eq!(hit_at(&hits, 1, 1), Some(Hit::DismissTool));
        assert_eq!(hit_at(&hits, 2, 4), Some(Hit::ToolItem(0, 1)));
        assert_eq!(hit_at(&hits, 5, 3), Some(Hit::ToolPanel));
    }

    #[test]
    fn edit_inserts_in_the_middle() {
        let mut e = Edit::at_end("ac".into());
        e.caret = 1;
        e.insert_char('b');
        assert_eq!(e.text, "abc");
        assert_eq!(e.caret, 2);
    }

    #[test]
    fn edit_arrows_move_by_unicode_char() {
        let mut e = Edit::at_end("你好".into());
        e.move_left(false);
        assert_eq!(e.caret, 1);
        e.backspace();
        assert_eq!(e.text, "好");
        assert_eq!(e.caret, 0);
    }

    #[test]
    fn edit_select_all_then_backspace_clears() {
        let mut e = Edit::at_end("hello".into());
        e.select_all();
        assert_eq!(e.selected_text().as_deref(), Some("hello"));
        e.backspace();
        assert!(e.is_empty());
        assert_eq!(e.caret, 0);
    }

    #[test]
    fn edit_shift_right_selects_then_typing_replaces() {
        let mut e = Edit::at_end("abcd".into());
        e.home(false);
        e.move_right(true);
        e.move_right(true);
        assert_eq!(e.selected_text().as_deref(), Some("ab"));
        e.insert_char('z');
        assert_eq!(e.text, "zcd");
    }

    #[test]
    fn click_maps_cjk_display_columns() {
        let inner = Rect::new(10, 5, 40, 1);
        assert_eq!(click_to_index("你好", inner, 0, 10, 5), 0);
        assert_eq!(click_to_index("你好", inner, 0, 11, 5), 1);
        assert_eq!(click_to_index("你好", inner, 0, 12, 5), 1);
        assert_eq!(click_to_index("你好", inner, 0, 14, 5), 2);
    }

    #[test]
    fn selectable_user_text_keeps_prefix_and_body() {
        let row = Row::User("hello".into());
        assert_eq!(selectable_text(&row).as_deref(), Some("you   hello"));
    }

    #[test]
    fn chat_selected_text_is_a_char_range_not_the_whole_row() {
        let rows = vec![Row::User("hello".into())];
        let sel = ChatSel::Text {
            anchor: ChatPos { row: 0, idx: 7 },
            caret: ChatPos { row: 0, idx: 9 },
        };
        assert_eq!(chat_selected_text(&rows, &sel).as_deref(), Some("el"));
        let collapsed = ChatSel::Text {
            anchor: ChatPos { row: 0, idx: 7 },
            caret: ChatPos { row: 0, idx: 7 },
        };
        assert_eq!(chat_selected_text(&rows, &collapsed), None);
    }

    #[test]
    fn empty_after_glyphs_hits_chat_not_the_row() {
        let hits = vec![
            (Rect::new(0, 0, 40, 10), Hit::Chat),
            (Rect::new(2, 3, 11, 1), Hit::ChatRow(0)),
        ];
        assert_eq!(hit_at(&hits, 2, 3), Some(Hit::ChatRow(0)));
        assert_eq!(hit_at(&hits, 12, 3), Some(Hit::ChatRow(0)));
        assert_eq!(hit_at(&hits, 13, 3), Some(Hit::Chat));
        assert_eq!(hit_at(&hits, 20, 3), Some(Hit::Chat));
    }

    fn hello_glyphs() -> Vec<ChatGlyphLine> {
        let chars: Vec<char> = "you   hello".chars().collect();
        let text_w: u16 = chars.iter().map(|c| ch_width(*c).max(1)).sum();
        vec![ChatGlyphLine {
            y: 3,
            x: 2,
            text_w,
            row: 0,
            start: 0,
            chars,
        }]
    }

    #[test]
    fn chat_drag_selects_substring() {
        let mut app = test_app();
        app.push(Row::User("hello".into()));
        app.chat_glyphs = hello_glyphs();
        let w = app.chat_glyphs[0].text_w;
        app.hits = vec![
            (Rect::new(0, 0, 40, 10), Hit::Chat),
            (Rect::new(2, 3, w, 1), Hit::ChatRow(0)),
        ];
        let (mut opts, _sink, _tx) = dummy_key_env();
        handle_mouse(
            &mut app,
            &mut opts,
            MouseEventKind::Down(MouseButton::Left),
            8,
            3,
            KeyModifiers::NONE,
        );
        handle_mouse(
            &mut app,
            &mut opts,
            MouseEventKind::Drag(MouseButton::Left),
            2 + w,
            3,
            KeyModifiers::NONE,
        );
        assert_eq!(
            chat_selected_text(&app.rows, &app.chat_sel).as_deref(),
            Some("hello")
        );
    }

    #[test]
    fn chat_click_empty_clears_selection() {
        let mut app = test_app();
        app.push(Row::User("hello".into()));
        app.chat_sel = ChatSel::Text {
            anchor: ChatPos { row: 0, idx: 6 },
            caret: ChatPos { row: 0, idx: 11 },
        };
        let w = 11;
        app.hits = vec![
            (Rect::new(0, 0, 40, 10), Hit::Chat),
            (Rect::new(2, 3, w, 1), Hit::ChatRow(0)),
        ];
        let (mut opts, _sink, _tx) = dummy_key_env();
        handle_mouse(
            &mut app,
            &mut opts,
            MouseEventKind::Down(MouseButton::Left),
            20,
            3,
            KeyModifiers::NONE,
        );
        assert_eq!(app.chat_sel, ChatSel::None);
    }

    #[test]
    fn chat_click_does_not_select_the_whole_row() {
        let mut app = test_app();
        app.push(Row::User("hello".into()));
        app.chat_glyphs = hello_glyphs();
        let w = app.chat_glyphs[0].text_w;
        app.hits = vec![
            (Rect::new(0, 0, 40, 10), Hit::Chat),
            (Rect::new(2, 3, w, 1), Hit::ChatRow(0)),
        ];
        let (mut opts, _sink, _tx) = dummy_key_env();
        handle_mouse(
            &mut app,
            &mut opts,
            MouseEventKind::Down(MouseButton::Left),
            8,
            3,
            KeyModifiers::NONE,
        );
        assert_eq!(chat_selected_text(&app.rows, &app.chat_sel), None);
        assert!(matches!(
            app.chat_sel,
            ChatSel::Text {
                anchor: ChatPos { row: 0, idx: 6 },
                caret: ChatPos { row: 0, idx: 6 }
            }
        ));
    }

    #[test]
    fn chat_pos_at_maps_cjk_display_columns() {
        let glyphs = vec![ChatGlyphLine {
            y: 0,
            x: 10,
            text_w: 4,
            row: 0,
            start: 0,
            chars: "你好".chars().collect(),
        }];
        assert_eq!(chat_pos_at(&glyphs, 10, 0).unwrap().idx, 0);
        assert_eq!(chat_pos_at(&glyphs, 11, 0).unwrap().idx, 1);
        assert_eq!(chat_pos_at(&glyphs, 12, 0).unwrap().idx, 1);
        assert_eq!(chat_pos_at(&glyphs, 14, 0).unwrap().idx, 2);
    }

    #[test]
    fn wrap_line_indexed_keeps_char_offsets() {
        let line = Line::from("you   abcdefghij");
        let pieces = wrap_line_indexed(line, 10);
        assert!(pieces.len() >= 2, "{}", pieces.len());
        assert_eq!(pieces[0].start, 0);
        assert_eq!(pieces[0].chars.len(), 10);
        assert_eq!(pieces[1].start, 10);
        let joined: String = pieces.iter().flat_map(|p| p.chars.iter().copied()).collect();
        assert_eq!(joined, "you   abcdefghij");
    }

    #[test]
    fn settings_import_claude_lists_foreign_skill() {
        let groka = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        let claude = home.path().join(".claude").join("skills").join("review");
        std::fs::create_dir_all(&claude).unwrap();
        std::fs::write(
            claude.join("SKILL.md"),
            "---\nname: review\ndescription: Review code\n---\nBe strict.\n",
        )
        .unwrap();
        let mut app = test_app();
        app.skills = Arc::new(Mutex::new(SkillStore::open_at(
            groka.path().to_path_buf(),
            home.path().to_path_buf(),
        )));
        app.refresh_skills();
        assert!(app.skill_list.is_empty());
        app.toggle_import_claude();
        assert_eq!(app.skill_list.len(), 1);
        assert_eq!(app.skill_list[0].name, "review");
        assert!(app.skill_list[0].enabled);
    }

    #[test]
    fn skill_view_allows_select_but_not_edit() {
        let groka = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(groka.path().join("demo")).unwrap();
        std::fs::write(
            groka.path().join("demo").join("SKILL.md"),
            "---\nname: demo\ndescription: Demo skill\n---\nHello body\n",
        )
        .unwrap();
        let mut app = test_app();
        app.skills = Arc::new(Mutex::new(SkillStore::open_at(
            groka.path().to_path_buf(),
            groka.path().to_path_buf(),
        )));
        app.refresh_skills();
        app.open_skill_view(0);
        let original = app.skill_view.as_ref().unwrap().edit.text.clone();
        assert!(original.contains("Hello body"));
        let (mut opts, sink, tx) = dummy_key_env();
        handle_key(
            &mut app,
            &mut opts,
            KeyCode::Char('x'),
            KeyModifiers::NONE,
            &sink,
            &tx,
        );
        assert_eq!(app.skill_view.as_ref().unwrap().edit.text, original);
        handle_key(
            &mut app,
            &mut opts,
            KeyCode::Char('a'),
            KeyModifiers::CONTROL,
            &sink,
            &tx,
        );
        assert_eq!(
            app.skill_view.as_ref().unwrap().edit.selected_text().as_deref(),
            Some(original.as_str())
        );
    }

    #[test]
    fn wrap_lines_breaks_on_width() {
        let lines = wrap_lines("abcd", 2);
        assert_eq!(lines, vec![(0, 2), (2, 4)]);
    }

    #[test]
    fn wrap_lines_respects_hard_newlines() {
        let lines = wrap_lines("ab\ncd", 10);
        assert_eq!(lines, vec![(0, 2), (3, 5)]);
    }

    #[test]
    fn settings_dock_sits_on_header_right() {
        let header = Rect::new(0, 0, 80, 1);
        let dock = settings_dock_rect(header);
        assert_eq!(dock.y, header.y);
        assert_eq!(dock.height, 1);
        assert!(dock.x + dock.width == header.x + header.width);
        assert!(dock.x > header.x, "dock must leave room for left header text");
    }

    #[test]
    fn spawn_working_output_is_not_finished_line() {
        let out = r#"{"name":"coder","state":"TASK_STATE_WORKING"}"#;
        assert!(spawn_output_still_running(out));
        let line = tool_finished_line("spawn_agent", out);
        assert!(line.contains("執行中"), "{line}");
        assert!(!line.contains('✓'), "{line}");
    }

    #[test]
    fn spawn_completed_output_shows_check() {
        let out = r#"{"name":"coder","state":"TASK_STATE_COMPLETED"}"#;
        assert!(!spawn_output_still_running(out));
        let line = tool_finished_line("spawn_agent", out);
        assert!(line.starts_with('✓'), "{line}");
    }

    #[test]
    fn finish_spawn_marks_the_tool_started_not_running() {
        let mut app = test_app();
        app.push(Row::Tools(ToolGroup {
            calls: vec![ToolCall {
                call_id: "s1".into(),
                name: "spawn_agent".into(),
                args: serde_json::json!({"name": "coder"}),
                output: String::new(),
                files: Vec::new(),
                done: false,
                phase: "執行中".into(),
            }],
            expanded: true,
        }));
        let out = r#"{"name":"coder","state":"working"}"#.to_string();
        app.finish_tool("s1", "spawn_agent", out);
        let Row::Tools(g) = &app.rows[0] else {
            panic!("expected tool group");
        };
        assert!(g.calls[0].done, "a long-lived child must not keep the spawn call spinning");
        assert_eq!(g.calls[0].phase, "已啟動");
    }

    #[test]
    fn side_pulse_when_child_alive() {
        let mut app = test_app();
        app.running = false;
        app.agent_spawned("", "coder", "fix", "");
        assert!(rail_needs_pulse(&app));
        assert!(side_pulse_ok(&app));
        app.agent_exited("", "coder", "killed");
        assert!(!rail_needs_pulse(&app));
    }

    #[test]
    fn route_event_ignores_other_session() {
        let mut app = test_app();
        app.current_id = "aaa".into();
        app.session.id = "aaa".into();
        let other = crate::events::EventMeta {
            ts: chrono::Utc::now(),
            agent_name: "root".into(),
            run_id: "bbb".into(),
            parent_run_id: None,
            path: String::new(),
        };
        app.route_event(AgentEvent::ModelDelta {
            meta: other,
            text: "should not appear".into(),
        });
        assert!(app.rows.is_empty());
    }

    #[test]
    fn route_event_applies_current_session() {
        let mut app = test_app();
        app.current_id = "aaa".into();
        app.session.id = "aaa".into();
        let meta = crate::events::EventMeta {
            ts: chrono::Utc::now(),
            agent_name: "root".into(),
            run_id: "aaa".into(),
            parent_run_id: None,
            path: String::new(),
        };
        app.route_event(AgentEvent::ModelDelta {
            meta,
            text: "hi".into(),
        });
        assert_eq!(app.rows.len(), 1);
        assert!(matches!(&app.rows[0], Row::Agent(s) if s.text == "hi"));
    }

    #[test]
    fn parked_session_receives_events() {
        let mut app = test_app();
        app.current_id = "cur".into();
        app.session.id = "cur".into();
        let mut other = dummy_session();
        other.id = "parked".into();
        app.parked.insert(
            "parked".into(),
            ParkedChat {
                session: other,
                rows: vec![],
                status: "工作中".into(),
                cache: String::new(),
                child_count: 0,
                running: true,
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
            bench: Workbench::default(),
            monitors: Vec::new(),
            backgrounds: Vec::new(),
            inspector: None,
            inspector_scroll: 0,
            task: TaskHub::new("parked"),
            },
        );
        let meta = crate::events::EventMeta {
            ts: chrono::Utc::now(),
            agent_name: "root".into(),
            run_id: "parked".into(),
            parent_run_id: None,
            path: String::new(),
        };
        app.route_event(AgentEvent::ModelDelta {
            meta,
            text: "bg".into(),
        });
        assert!(app.rows.is_empty(), "current chat stays empty");
        let parked = app.parked.get("parked").unwrap();
        assert!(matches!(&parked.rows[0], Row::Agent(s) if s.text == "bg"));
    }

    #[test]
    fn session_named_updates_sidebar_title() {
        let mut app = test_app();
        app.current_id = "s".into();
        let meta = crate::events::EventMeta {
            ts: chrono::Utc::now(),
            agent_name: "root".into(),
            run_id: "s".into(),
            parent_run_id: None,
            path: String::new(),
        };
        app.route_event(AgentEvent::SessionNamed {
            meta,
            name: "修 login race".into(),
        });
        assert_eq!(app.session.name, "修 login race");
        assert!(app.session.named);
    }

    #[test]
    fn switch_to_parks_current_rows() {
        let mut app = test_app();
        app.current_id = "a".into();
        app.session.id = "a".into();
        app.session.name = "A".into();
        app.push(Row::User("from a".into()));
        let mut b = dummy_session();
        b.id = "b".into();
        b.name = "B".into();
        app.parked.insert(
            "b".into(),
            ParkedChat {
                session: b,
                rows: vec![Row::User("from b".into())],
                status: "待命".into(),
                cache: String::new(),
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
            bench: Workbench::default(),
            monitors: Vec::new(),
            backgrounds: Vec::new(),
            inspector: None,
            inspector_scroll: 0,
            task: TaskHub::new("b"),
            },
        );
        app.switch_to("b");
        assert_eq!(app.current_id, "b");
        assert!(matches!(&app.rows[0], Row::User(u) if u.text == "from b"));
        let parked_a = app.parked.get("a").unwrap();
        assert!(matches!(&parked_a.rows[0], Row::User(u) if u.text == "from a"));
    }

    #[test]
    fn new_chat_opens_workspace_picker_then_creates_on_confirm() {
        let mut app = test_app();
        app.session.named = true;
        app.session.name = "舊的".into();
        app.push(Row::User("hello".into()));
        app.new_chat();
        assert!(app.workspace_pick.is_some());
        assert_eq!(app.focus, Focus::Workspace);
        assert_eq!(app.session.name, "舊的", "must not mint a chat before the folder is chosen");
        assert!(app.parked.is_empty());
        app.confirm_workspace_pick();
        assert!(app.workspace_pick.is_none());
        assert_eq!(app.session.name, "新對話");
        assert!(!app.session.named);
        assert!(app.rows.iter().all(|r| matches!(r, Row::Meta(_))));
        assert_eq!(app.parked.len(), 1);
    }

    #[test]
    fn new_chat_cancel_leaves_current_session() {
        let mut app = test_app();
        app.session.named = true;
        app.session.name = "舊的".into();
        app.push(Row::User("hello".into()));
        app.new_chat();
        app.cancel_workspace_pick();
        assert!(app.workspace_pick.is_none());
        assert_eq!(app.session.name, "舊的");
        assert!(app.parked.is_empty());
        assert_eq!(app.focus, Focus::Chat);
    }

    #[test]
    fn blank_draft_confirm_sets_workspace_without_extra_session() {
        let dir = tempfile::tempdir().unwrap();
        let sub = dir.path().join("proj");
        std::fs::create_dir(&sub).unwrap();
        let mut app = test_app();
        app.session.workspace = dir.path().to_path_buf();
        app.launch_workspace = dir.path().to_path_buf();
        let id = app.session.id.clone();
        app.new_chat();
        app.enter_workspace_dir(sub.clone());
        app.confirm_workspace_pick();
        assert_eq!(app.session.id, id);
        assert!(app.parked.is_empty());
        assert_eq!(app.session.workspace, folderpick::normalize(&sub));
        assert_eq!(app.launch_workspace, folderpick::normalize(&sub));
    }

    #[test]
    fn workspace_picker_create_dir_and_mouse_hits() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = test_app();
        app.session.workspace = dir.path().to_path_buf();
        app.new_chat();
        let typed = folderpick::display_path(&dir.path().join("created"));
        if let Some(p) = app.workspace_pick.as_mut() {
            p.edit = Edit::at_end(typed);
        }
        app.create_workspace_dir();
        let created = dir.path().join("created");
        assert!(created.is_dir(), "{}", created.display());
        let cwd = app.workspace_pick.as_ref().unwrap().view.cwd.clone();
        assert_eq!(cwd, folderpick::normalize(&created));

        let hits = vec![
            (Rect::new(0, 0, 40, 20), Hit::WsPanel),
            (Rect::new(2, 4, 20, 1), Hit::WsEntry(1)),
            (Rect::new(2, 18, 10, 1), Hit::WsConfirm),
            (Rect::new(13, 18, 10, 1), Hit::WsCreate),
            (Rect::new(24, 18, 6, 1), Hit::WsCancel),
        ];
        assert_eq!(hit_at(&hits, 3, 4), Some(Hit::WsEntry(1)));
        assert_eq!(hit_at(&hits, 4, 18), Some(Hit::WsConfirm));
        assert_eq!(hit_at(&hits, 14, 18), Some(Hit::WsCreate));
        assert_eq!(hit_at(&hits, 25, 18), Some(Hit::WsCancel));
    }

    #[test]
    fn workspace_picker_type_filters_listing() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("src")).unwrap();
        std::fs::write(dir.path().join("README.md"), b"x").unwrap();
        let mut app = test_app();
        app.session.workspace = dir.path().to_path_buf();
        app.new_chat();
        let sep = std::path::MAIN_SEPARATOR;
        let typed = format!("{}{sep}s", folderpick::display_path(dir.path()));
        if let Some(p) = app.workspace_pick.as_mut() {
            p.edit = Edit::at_end(typed);
        }
        app.sync_workspace_pick();
        let names: Vec<String> = app
            .workspace_pick
            .as_ref()
            .unwrap()
            .view
            .entries
            .iter()
            .filter(|e| !e.is_parent)
            .map(|e| e.name.clone())
            .collect();
        assert_eq!(names, ["src"], "{names:?}");
    }

    #[test]
    fn ctrl_n_opens_workspace_picker() {
        let mut app = test_app();
        app.session.named = true;
        app.session.name = "舊的".into();
        app.push(Row::User("x".into()));
        let (mut opts, sink, tx) = dummy_key_env();
        handle_key(
            &mut app,
            &mut opts,
            KeyCode::Char('n'),
            KeyModifiers::CONTROL,
            &sink,
            &tx,
        );
        assert!(app.workspace_pick.is_some());
        assert_eq!(app.focus, Focus::Workspace);
        assert_eq!(app.session.name, "舊的");
        assert!(app.parked.is_empty());
    }

    #[test]
    fn sidebar_hits_new_chat_and_session() {
        let hits = vec![
            (Rect::new(0, 0, 28, 1), Hit::NewChat),
            (Rect::new(0, 2, 28, 2), Hit::Session(0)),
        ];
        assert_eq!(hit_at(&hits, 1, 0), Some(Hit::NewChat));
        assert_eq!(hit_at(&hits, 3, 3), Some(Hit::Session(0)));
        assert_eq!(hit_at(&hits, 3, 1), None);
    }

    #[test]
    fn truncate_width_pads_and_clips() {
        let s = truncate_width("ab", 4);
        assert_eq!(s.chars().count(), 4);
        assert!(s.starts_with("ab"));
        let cjk = truncate_width("你好世界", 4);
        assert_eq!(Line::from(cjk.as_str()).width(), 4);
    }

    #[test]
    fn wrap_visual_breaks_on_newline_instead_of_squeezing() {
        // Line::from(&str) strips/splits newlines into adjacent spans (ratatui).
        // Embedded \\n in a Span is what a squeezed command dump looks like.
        let line = Line::from(Span::styled("a\nb\nc", Style::default()));
        let wrapped = wrap_visual(line, 40);
        assert_eq!(wrapped.len(), 3, "{wrapped:?}");
        let texts: Vec<String> = wrapped
            .iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.to_string())
                    .collect::<String>()
            })
            .collect();
        assert_eq!(texts, ["a", "b", "c"]);
    }

    #[test]
    fn diff_and_command_output_are_separate_colored_lines() {
        let lines = colored_output_lines(
            "diff --git a/a.txt b/a.txt\n--- a/a.txt\n+++ b/a.txt\n@@ -1 +1 @@\n-old\n+new\n M src/tui.rs\n",
            80,
        );
        assert!(lines.len() >= 6, "{}", lines.len());
        let rendered: Vec<String> = lines.iter().map(|l| format!("{l:?}")).collect();
        let blob = rendered.join("\n");
        assert!(blob.contains("-old"), "{blob}");
        assert!(blob.contains("+new"), "{blob}");
        let add = diff_lines("+hello\n-world\n");
        assert_eq!(add.len(), 2);
        assert!(format!("{:?}", add[0]).contains("hello"));
        assert!(format!("{:?}", add[1]).contains("world"));
    }

    #[test]
    fn ansi_command_output_keeps_color_codes() {
        let line = ansi_line("\u{1b}[32madded\u{1b}[0m \u{1b}[31mremoved\u{1b}[0m");
        let s = format!("{line:?}");
        assert!(s.contains("added"), "{s}");
        assert!(s.contains("removed"), "{s}");
        assert!(s.contains("63, 185, 80") || s.contains("DIFF_ADD") || s.contains("added"), "{s}");
    }

    #[test]
    fn expanded_tool_shows_diff_as_its_own_rows() {
        let mut app = test_app();
        app.push(Row::Tools(ToolGroup {
            calls: vec![ToolCall {
                call_id: String::new(),
                name: "write_file".into(),
                args: serde_json::json!({"path": "a.txt"}),
                output: String::new(),
                files: vec![FileChange {
                    path: "a.txt".into(),
                    kind: "modify".into(),
                    diff: "--- a/a.txt\n+++ b/a.txt\n-old\n+new\n".into(),
                }],
                done: true,
                phase: "完成".into(),
            }],
            expanded: true,
        }));
        let vis = chat_logical_rows(&mut app, 80);
        let blob: String = vis.iter().map(|l| format!("{:?}", l.line)).collect::<Vec<_>>().join("\n");
        assert!(vis.len() > 2, "diff must not collapse into the header: {blob}");
        assert!(blob.contains("-old"), "{blob}");
        assert!(blob.contains("+new"), "{blob}");
    }

    #[test]
    fn rename_pins_title() {
        let mut app = test_app();
        app.session.name = "舊的".into();
        app.begin_rename("s");
        if let Some((_, edit)) = app.rename.as_mut() {
            edit.clear();
            edit.insert_str("手動標題");
        }
        app.commit_rename();
        assert_eq!(app.session.name, "手動標題");
        assert!(app.session.name_is_manual);
        assert!(app.rename.is_none());
    }

    #[test]
    fn delete_current_opens_a_fresh_chat() {
        let mut app = test_app();
        app.session.named = true;
        app.session.name = "要刪的".into();
        app.push(Row::User("bye".into()));
        let old = app.current_id.clone();
        app.delete_session(&old);
        assert_ne!(app.current_id, old);
        assert!(app.rows.iter().all(|r| matches!(r, Row::Meta(_))));
        assert!(app.parked.is_empty());
    }

    #[test]
    fn sidebar_action_hits_sit_above_session() {
        let hits = vec![
            (Rect::new(0, 2, 28, 2), Hit::Session(0)),
            (Rect::new(22, 2, 3, 1), Hit::RenameSession(0)),
            (Rect::new(25, 2, 3, 1), Hit::DeleteSession(0)),
        ];
        assert_eq!(hit_at(&hits, 23, 2), Some(Hit::RenameSession(0)));
        assert_eq!(hit_at(&hits, 26, 2), Some(Hit::DeleteSession(0)));
        assert_eq!(hit_at(&hits, 4, 3), Some(Hit::Session(0)));
    }

    #[test]
    fn agent_markdown_and_work_clock() {
        let lines = agent_lines(&AgentMsg {
            text: "# Hi\n\nuse **bold** and `code`".into(),
            work_ms: 3_400,
        });
        let blob: String = lines.iter().map(|l| format!("{l:?}")).collect::<Vec<_>>().join("\n");
        assert!(blob.contains("Hi"), "{blob}");
        assert!(blob.contains("bold"), "{blob}");
        assert!(blob.contains("code"), "{blob}");
        assert!(blob.contains("工作 3.4s"), "{blob}");
        assert!(blob.contains("grok"), "{blob}");
    }

    #[test]
    fn think_header_shows_elapsed() {
        let line = think_header_line(&Think {
            text: "plan".into(),
            expanded: false,
            done: true,
            elapsed_ms: 3_400,
            started: None,
        });
        let s = format!("{line:?}");
        assert!(s.contains("思考"), "{s}");
        assert!(s.contains("3.4s"), "{s}");
        assert!(s.contains("思考"), "{s}");
    }

    #[test]
    fn think_clock_width_does_not_jump_as_time_grows() {
        let widths: Vec<usize> = [100, 3_400, 9_900, 18_400, 72_000]
            .into_iter()
            .map(|ms| {
                think_header_line(&Think {
                    text: String::new(),
                    expanded: false,
                    done: false,
                    elapsed_ms: 0,
                    started: Some(Instant::now() - Duration::from_millis(ms)),
                })
                .width()
            })
            .collect();
        assert!(widths.windows(2).all(|w| w[0] == w[1]), "{widths:?}");
    }

    #[test]
    fn freeze_composer_keeps_typed_text_while_clock_moves() {
        let mut app = test_app();
        app.edit.insert_str("hello");
        app.running = true;
        app.work_started = Some(Instant::now() - Duration::from_millis(100));
        let opts = test_opts("grok-4.6", ReasoningEffort::High);
        let mut terminal =
            Terminal::new(ratatui::backend::TestBackend::new(80, 24)).unwrap();
        terminal
            .draw(|f| {
                let _ = draw_ui(f, &mut app, &opts, false);
            })
            .unwrap();
        let inner = app.composer_inner;
        app.work_started = Some(Instant::now() - Duration::from_millis(3_400));
        app.tick = 7;
        terminal
            .draw(|f| {
                let _ = draw_ui(f, &mut app, &opts, true);
            })
            .unwrap();
        let buf = terminal.backend().buffer();
        assert_eq!(buf[(inner.x, inner.y)].symbol(), "h", "composer must not redraw for a clock tick");
        assert_eq!(buf[(inner.x + 1, inner.y)].symbol(), "e");
    }

    #[test]
    fn clock_cells_never_touch_composer() {
        let mut app = test_app();
        app.running = true;
        app.work_started = Some(Instant::now() - Duration::from_millis(100));
        app.push(Row::Think(Think {
            text: String::new(),
            expanded: false,
            done: false,
            elapsed_ms: 0,
            started: Some(Instant::now() - Duration::from_millis(100)),
        }));
        app.edit.insert_str("hello");
        let opts = test_opts("grok-4.6", ReasoningEffort::High);
        let mut terminal =
            Terminal::new(ratatui::backend::TestBackend::new(80, 24)).unwrap();
        terminal
            .draw(|f| {
                let _ = draw_ui(f, &mut app, &opts, false);
            })
            .unwrap();
        assert!(
            !app.think_clocks.is_empty(),
            "live think header must be recorded for clock patches"
        );
        let frame = app.composer_frame;
        assert!(frame.height > 0 && frame.y > 0, "{frame:?}");
        if let Some(Row::Think(t)) = app.rows.get_mut(0) {
            t.started = Some(Instant::now() - Duration::from_millis(3_400));
        }
        app.work_started = Some(Instant::now() - Duration::from_millis(3_400));
        app.tick = 7;
        let cells = collect_clock_cells(&app, &opts);
        assert!(!cells.is_empty(), "header and think clocks must emit cells");
        for (x, y, _) in &cells {
            assert!(
                !frame.contains(Position { x: *x, y: *y }),
                "clock cell ({x},{y}) inside composer {frame:?}"
            );
        }
    }

    #[test]
    fn applying_clock_cells_leaves_composer_text() {
        let mut app = test_app();
        app.running = true;
        app.work_started = Some(Instant::now() - Duration::from_millis(100));
        app.push(Row::Think(Think {
            text: String::new(),
            expanded: false,
            done: false,
            elapsed_ms: 0,
            started: Some(Instant::now() - Duration::from_millis(100)),
        }));
        app.edit.insert_str("hello");
        let opts = test_opts("grok-4.6", ReasoningEffort::High);
        let mut terminal =
            Terminal::new(ratatui::backend::TestBackend::new(80, 24)).unwrap();
        terminal
            .draw(|f| {
                let _ = draw_ui(f, &mut app, &opts, false);
            })
            .unwrap();
        let inner = app.composer_inner;
        let frame = app.composer_frame;
        let src = terminal.backend().buffer();
        let before: Vec<String> = (0..5)
            .map(|i| src[(inner.x + i, inner.y)].symbol().to_string())
            .collect();
        assert_eq!(before[0], "h");
        if let Some(Row::Think(t)) = app.rows.get_mut(0) {
            t.started = Some(Instant::now() - Duration::from_millis(3_400));
        }
        app.work_started = Some(Instant::now() - Duration::from_millis(3_400));
        app.tick = 7;
        let cells = collect_clock_cells(&app, &opts);
        let mut buf = terminal.backend().buffer().clone();
        for (x, y, cell) in cells {
            if let Some(out) = buf.cell_mut(Position { x, y }) {
                *out = cell;
            }
        }
        for (i, ch) in before.iter().enumerate() {
            assert_eq!(
                buf[(inner.x + i as u16, inner.y)].symbol(),
                ch.as_str(),
                "composer col {i} changed by a clock patch"
            );
        }
        let &( _ri, area) = app
            .think_clocks
            .first()
            .expect("think clock row");
        let mut think = String::new();
        for x in area.left()..area.right() {
            think.push_str(buf[(x, area.y)].symbol());
        }
        assert!(think.contains("3.4s"), "think={think:?} frame={frame:?}");
    }

    fn key_press(code: KeyCode) -> Event {
        Event::Key(crossterm::event::KeyEvent::new(code, KeyModifiers::NONE))
    }

    #[test]
    fn ime_first_enter_is_confirm_not_send() {
        let batch = vec![
            key_press(KeyCode::Char('你')),
            key_press(KeyCode::Char('你')),
            key_press(KeyCode::Enter),
        ];
        let out = coalesce_ime_enter(batch);
        assert!(out.iter().any(is_ime_commit_char), "{out:?}");
        assert!(
            !out.iter().any(is_plain_enter),
            "first Enter with IME chars must not send: {out:?}"
        );
    }

    #[test]
    fn ascii_enter_still_sends() {
        let batch = vec![
            key_press(KeyCode::Char('h')),
            key_press(KeyCode::Char('i')),
            key_press(KeyCode::Enter),
        ];
        let out = coalesce_ime_enter(batch);
        assert!(
            out.iter().any(is_plain_enter),
            "ASCII Enter must still send: {out:?}"
        );
    }

    #[test]
    fn clock_cells_changed_only_emits_new_glyphs() {
        let mut a = Cell::default();
        a.set_symbol("1");
        let mut b = Cell::default();
        b.set_symbol("2");
        let prev = vec![(3, 4, a.clone())];
        let next = vec![(3, 4, a.clone()), (5, 4, b.clone())];
        let changed = clock_cells_changed(&prev, &next);
        assert_eq!(changed.len(), 1, "{changed:?}");
        assert_eq!(changed[0].0, 5);
        assert_eq!(changed[0].2.symbol(), "2");
        assert!(clock_cells_changed(&next, &next).is_empty());
    }

    #[test]
    fn web_snapshot_renders_user_and_composer() {
        let mut app = test_app();
        app.edit = Edit::at_end("typed".into());
        app.composer_seq = 4;
        app.push(Row::User("hello <b>".into()));
        let opts = test_opts("grok-4.6", ReasoningEffort::High);
        let snap = ui_snapshot(&app, &opts);
        assert_eq!(snap.composer.text, "typed");
        assert_eq!(snap.composer.seq, 4);
        let (patches, removed) = view_patches(&mut app);
        assert!(removed.is_empty());
        let main = patches.iter().find(|p| p.path.is_empty()).expect("main view");
        let user = main.rows.iter().find(|r| r.kind == "user").unwrap();
        assert!(user.html.contains("&lt;b&gt;"), "{}", user.html);
        assert!(!user.html.contains("<b>"), "{}", user.html);
    }

    #[test]
    fn web_patches_send_only_changed_rows_per_view() {
        let mut app = test_app();
        app.push(Row::Meta("one".into()));
        app.push(Row::Meta("two".into()));
        let (first, _) = view_patches(&mut app);
        assert_eq!(first.len(), 1);
        assert_eq!((first[0].from, first[0].len, first[0].rows.len()), (0, 2, 2));

        let (none, _) = view_patches(&mut app);
        assert!(none.is_empty(), "unchanged rows are not resent");

        app.push(Row::Meta("three".into()));
        app.agent_spawned("", "coder", "fix", "");
        app.with_agent("coder", |app| app.push(Row::Meta("kid".into())));
        let (next, _) = view_patches(&mut app);
        let main = next.iter().find(|p| p.path.is_empty()).unwrap();
        assert_eq!((main.from, main.len, main.rows.len()), (2, 3, 1), "only the appended row");
        let kid = next.iter().find(|p| p.path == "coder").unwrap();
        assert_eq!(kid.rows.len(), 1);

        // Leaving the session drops the agent view.
        app.bench = Workbench::default();
        let (_, removed) = view_patches(&mut app);
        assert_eq!(removed, vec!["coder".to_string()]);
    }

    #[test]
    fn web_set_composer_then_submit_queues_while_running() {
        let mut app = test_app();
        app.running = true;
        app.send_mode = SendMode::Queue;
        let (tx, _rx) = mpsc::unbounded_channel();
        app.inbox_tx = Some(tx);
        let opts = test_opts("grok-4.6", ReasoningEffort::High);
        let sink = Arc::new(FanoutSink { sinks: vec![] });
        let (done_tx, _done_rx) = mpsc::unbounded_channel();
        apply_ui_command(
            &mut app,
            &mut { opts.clone() },
            &sink,
            &done_tx,
            UiCommand::SetComposer {
                text: "from web".into(),
                caret: 8,
                seq: 9,
            },
        );
        assert_eq!(app.edit.text, "from web");
        assert_eq!(app.web_composer_seq, 9);
        apply_ui_command(
            &mut app,
            &mut { opts.clone() },
            &sink,
            &done_tx,
            UiCommand::Submit { insert: false },
        );
        assert_eq!(app.queue.len(), 1);
        assert_eq!(app.queue[0].text, "from web");
        assert!(app.edit.is_empty());
    }

    #[test]
    fn web_commands_cover_sessions_ask_settings_and_rail() {
        let mut app = test_app();
        app.current_id = "s".into();
        app.session.id = "s".into();
        app.parked.insert("other".into(), parked_stub("other"));
        let mut opts = test_opts("grok-4.6", ReasoningEffort::High);
        let sink = Arc::new(FanoutSink { sinks: vec![] });
        let (done_tx, _done_rx) = mpsc::unbounded_channel();
        apply_ui_command(&mut app, &mut opts, &sink, &done_tx, UiCommand::Switch { id: "other".into() });
        assert_eq!(app.current_id, "other");
        apply_ui_command(&mut app, &mut opts, &sink, &done_tx, UiCommand::NewChat);
        assert!(app.workspace_pick.is_some());
        apply_ui_command(&mut app, &mut opts, &sink, &done_tx, UiCommand::WsCancel);
        assert!(app.workspace_pick.is_none());

        app.apply_event(sample_ask(false));
        apply_ui_command(&mut app, &mut opts, &sink, &done_tx, UiCommand::AskToggle { index: 0 });
        apply_ui_command(&mut app, &mut opts, &sink, &done_tx, UiCommand::AskConfirm);
        assert!(app.ask.is_none());

        apply_ui_command(&mut app, &mut opts, &sink, &done_tx, UiCommand::OpenSettings);
        assert!(app.settings.is_some());
        apply_ui_command(&mut app, &mut opts, &sink, &done_tx, UiCommand::ToggleSearch);
        assert!(opts.web_search);
        apply_ui_command(&mut app, &mut opts, &sink, &done_tx, UiCommand::ToggleDispatcher);
        assert!(opts.dispatcher);
        assert!(app.knobs.lock().unwrap().dispatcher, "dispatcher must sync into session knobs");
        apply_ui_command(&mut app, &mut opts, &sink, &done_tx, UiCommand::ToggleDispatcher);
        assert!(!opts.dispatcher);
        assert!(!app.knobs.lock().unwrap().dispatcher);
        apply_ui_command(&mut app, &mut opts, &sink, &done_tx, UiCommand::SetChildModel { id: " grok-3-mini ".into() });
        assert_eq!(opts.child_model, "grok-3-mini");
        assert_eq!(
            app.knobs.lock().unwrap().child_model,
            "grok-3-mini",
            "child model must sync into session knobs"
        );
        apply_ui_command(&mut app, &mut opts, &sink, &done_tx, UiCommand::SetChildModel { id: String::new() });
        assert!(opts.child_model.is_empty());
        assert!(app.knobs.lock().unwrap().child_model.is_empty());
        apply_ui_command(&mut app, &mut opts, &sink, &done_tx, UiCommand::CloseSettings);
        assert!(app.settings.is_none());

        apply_ui_command(&mut app, &mut opts, &sink, &done_tx, UiCommand::OpenTask);
        assert!(matches!(app.task_ui, Some(TaskUi::Form { .. })));
        apply_ui_command(&mut app, &mut opts, &sink, &done_tx, UiCommand::SetTaskDraft { text: "把登入做完".into() });
        let (tx, _rx) = mpsc::unbounded_channel();
        app.inbox_tx = Some(tx);
        apply_ui_command(&mut app, &mut opts, &sink, &done_tx, UiCommand::SubmitTask);
        assert_eq!(app.task.snapshot().goal, "把登入做完");
        assert_eq!(app.task.snapshot().phase, TaskPhase::NeedPlan);
        assert!(matches!(app.task_ui, Some(TaskUi::Status)));
        apply_ui_command(&mut app, &mut opts, &sink, &done_tx, UiCommand::EndTask);
        assert_eq!(app.task.snapshot().phase, TaskPhase::Inactive);
        assert!(app.task_ui.is_none());

        app.agent_spawned("", "coder", "fix", "grok-3-mini");
        let logs = ui_logs(&app);
        assert_eq!(logs.agents.len(), 1);
        assert_eq!(logs.agents[0].path, "coder");
        assert_eq!(logs.agents[0].model, "grok-3-mini");
        assert_eq!(logs.agents[0].state, "starting");

        app.monitors.push(SideMon {
            name: "hook".into(),
            command: "python hook.py".into(),
            pid: 7,
            status: "執行中".into(),
            alive: true,
            detail: String::new(),
        });
        apply_ui_command(&mut app, &mut opts, &sink, &done_tx, UiCommand::OpenMonitor { name: "hook".into() });
        let snap = ui_snapshot(&app, &opts);
        assert_eq!(snap.inspector.as_ref().unwrap().name, "hook");
        apply_ui_command(&mut app, &mut opts, &sink, &done_tx, UiCommand::CloseInspector);
        assert!(app.inspector.is_none());

        app.running = true;
        app.cancel = Some(CancelFlag::new());
        apply_ui_command(&mut app, &mut opts, &sink, &done_tx, UiCommand::Interrupt);
        assert_eq!(app.status, "中斷中");
    }

    #[test]
    fn task_chip_is_clickable_on_the_header() {
        let mut app = test_app();
        let opts = test_opts("grok-4.6", ReasoningEffort::High);
        let mut terminal =
            Terminal::new(ratatui::backend::TestBackend::new(120, 24)).unwrap();
        terminal
            .draw(|f| {
                let _ = draw_ui(f, &mut app, &opts, false);
            })
            .unwrap();
        let chip = app
            .hits
            .iter()
            .rev()
            .find(|(_, h)| *h == Hit::TaskChip)
            .map(|(r, _)| *r)
            .expect("task chip");
        assert_eq!(chip.y, 23, "task chip lives on the status bar");
        assert_eq!(hit_at(&app.hits, chip.x, 23), Some(Hit::TaskChip));
    }

    #[test]
    fn awaiting_input_stamps_work_on_last_agent() {
        let mut app = test_app();
        app.work_started = Some(Instant::now() - Duration::from_millis(50));
        app.push(Row::Agent(AgentMsg::new("done".into())));
        app.apply_event(AgentEvent::AwaitingInput { meta: test_meta() });
        let Row::Agent(a) = &app.rows[0] else {
            panic!("expected agent");
        };
        assert!(a.work_ms >= 50, "work_ms={}", a.work_ms);
        assert!(app.work_started.is_none());
    }

    #[test]
    fn agent_row_serde_keeps_legacy_string() {
        let legacy = serde_json::json!({"Agent": "hello"});
        let row: Row = serde_json::from_value(legacy).unwrap();
        assert!(matches!(row, Row::Agent(a) if a.text == "hello" && a.work_ms == 0));
        let plain = Row::Agent(AgentMsg::new("hello".into()));
        assert_eq!(
            serde_json::to_value(&plain).unwrap(),
            serde_json::json!({"Agent": "hello"})
        );
        let timed = Row::Agent(AgentMsg {
            text: "hello".into(),
            work_ms: 1_200,
        });
        let v = serde_json::to_value(&timed).unwrap();
        assert_eq!(v["Agent"]["text"], "hello");
        assert_eq!(v["Agent"]["work_ms"], 1200);
    }

    fn test_opts(model: &str, effort: ReasoningEffort) -> TuiOptions {
        TuiOptions {
            model: model.into(),
            events: PathBuf::from("events.jsonl"),
            workspace: PathBuf::from("."),
            max_turns: 0,
            web_search: false,
            dispatcher: false,
            child_model: String::new(),
            reasoning_effort: effort,
        }
    }

    fn inject_two_model_catalog(app: &mut App) {
        app.catalog = crate::catalog::parse_catalog_value(&serde_json::json!({
            "data": [
                {
                    "id": "alpha",
                    "name": "Alpha",
                    "supportsReasoningEffort": true,
                    "reasoningEffort": "high",
                    "reasoningEfforts": ["low", "high"]
                },
                {
                    "id": "beta",
                    "name": "Beta",
                    "supportsReasoningEffort": false
                }
            ]
        }))
        .unwrap();
        app.grok_catalog = app.catalog.clone();
        app.catalog_status = CatalogStatus::Ready;
    }

    #[test]
    fn settings_dropdown_hits_come_from_catalog_not_hardcoded_tiers() {
        let mut app = test_app();
        app.area = Rect::new(0, 0, 80, 24);
        inject_two_model_catalog(&mut app);
        let mut opts = test_opts("alpha", ReasoningEffort::High);
        open_settings(&mut app);
        app.setting_field = SettingField::Model;
        let mut terminal =
            Terminal::new(ratatui::backend::TestBackend::new(80, 24)).unwrap();
        terminal
            .draw(|f| {
                let _ = draw_settings(f, &mut app, &opts);
            })
            .unwrap();
        assert!(
            app.hits.iter().any(|(_, h)| matches!(h, Hit::SettingModel)),
            "model combo missing: {:?}",
            app.hits.iter().map(|(_, h)| *h).collect::<Vec<_>>()
        );
        assert!(app
            .hits
            .iter()
            .any(|(_, h)| matches!(h, Hit::SettingEffort)));
        assert!(
            !app.hits
                .iter()
                .any(|(_, h)| matches!(h, Hit::CatalogPick(_))),
            "closed combo must not list options"
        );

        handle_settings_key(&mut app, &mut opts, KeyCode::Enter, KeyModifiers::NONE);
        terminal
            .draw(|f| {
                let _ = draw_settings(f, &mut app, &opts);
            })
            .unwrap();
        let picks: Vec<u16> = app
            .hits
            .iter()
            .filter_map(|(_, h)| match h {
                Hit::CatalogPick(i) => Some(*i),
                _ => None,
            })
            .collect();
        assert_eq!(picks, vec![0, 1], "model list must be catalog ids, not a hardcoded grok set");

        let cell = app
            .hits
            .iter()
            .find(|(_, h)| *h == Hit::CatalogPick(1))
            .unwrap()
            .0;
        handle_mouse(
            &mut app,
            &mut opts,
            MouseEventKind::Down(MouseButton::Left),
            cell.x,
            cell.y,
            KeyModifiers::NONE,
        );
        assert_eq!(opts.model, "beta");
        assert!(
            !app.knobs.lock().unwrap().send_reasoning,
            "beta does not support reasoning effort"
        );
        assert!(effort_choices(&app, &opts).is_empty());
    }

    #[test]
    fn switching_model_clamps_effort_to_that_models_catalog_list() {
        let mut app = test_app();
        inject_two_model_catalog(&mut app);
        let mut opts = test_opts("alpha", ReasoningEffort::Xhigh);
        apply_selected_model(&mut app, &mut opts, "alpha".into());
        assert_eq!(opts.reasoning_effort, ReasoningEffort::High);
        assert_eq!(
            effort_choices(&app, &opts)
                .iter()
                .map(|e| e.value)
                .collect::<Vec<_>>(),
            [ReasoningEffort::Low, ReasoningEffort::High]
        );
        app.setting_field = SettingField::Effort;
        handle_settings_key(&mut app, &mut opts, KeyCode::Right, KeyModifiers::NONE);
        assert_eq!(opts.reasoning_effort, ReasoningEffort::Low);
        handle_settings_key(&mut app, &mut opts, KeyCode::Right, KeyModifiers::NONE);
        assert_eq!(opts.reasoning_effort, ReasoningEffort::High);
        assert_ne!(opts.reasoning_effort, ReasoningEffort::Medium);
        assert_ne!(opts.reasoning_effort, ReasoningEffort::Xhigh);
    }

    #[test]
    fn catalog_load_failure_keeps_current_selection_only() {
        let mut app = test_app();
        let mut opts = test_opts("mine", ReasoningEffort::Medium);
        ingest_catalog(
            &mut app,
            &mut opts,
            Err(Error::Provider("nope".into())),
        );
        assert!(matches!(app.catalog_status, CatalogStatus::Failed(_)));
        assert_eq!(
            model_choices(&app, &opts),
            vec![("mine".into(), "mine".into())]
        );
        assert_eq!(
            effort_choices(&app, &opts)
                .iter()
                .map(|e| e.value)
                .collect::<Vec<_>>(),
            [ReasoningEffort::Medium]
        );
    }

    #[test]
    fn settings_login_event_ignores_stale_gen_then_accepts_current() {
        let mut app = test_app();
        app.logged_in = false;
        app.login_gen = 3;
        apply_login_event(
            &mut app,
            LoginEvent::Waiting {
                gen: 2,
                url: "https://auth.x.ai/device".into(),
                user_code: "OLD".into(),
            },
        );
        assert!(matches!(app.login_ui, LoginUi::Idle));
        apply_login_event(
            &mut app,
            LoginEvent::Waiting {
                gen: 3,
                url: "https://auth.x.ai/device".into(),
                user_code: "ABCD-EFGH".into(),
            },
        );
        match &app.login_ui {
            LoginUi::Waiting { user_code, .. } => assert_eq!(user_code, "ABCD-EFGH"),
            other => panic!("{other:?}"),
        }
        apply_login_event(&mut app, LoginEvent::Success { gen: 3 });
        assert!(app.logged_in);
        assert!(app.want_catalog);
        assert!(matches!(app.login_ui, LoginUi::Idle));
    }

    #[test]
    fn settings_logout_deletes_tokens() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("xai-auth.json");
        auth::save_tokens(
            &path,
            &auth::TokenSet {
                access_token: "acc".into(),
                refresh_token: "ref".into(),
                id_token: None,
            },
        )
        .unwrap();
        let mut app = test_app();
        app.auth_path = path.clone();
        app.logged_in = true;
        logout_account(&mut app);
        assert!(!app.logged_in);
        assert!(auth::load_tokens(&path).is_err());
    }

    #[test]
    fn begin_login_when_logged_out_requests_device_flow() {
        let mut app = test_app();
        app.logged_in = false;
        begin_login(&mut app);
        assert!(app.want_login);
        assert!(matches!(app.login_ui, LoginUi::Starting));
        assert_eq!(app.login_gen, 1);
        begin_login(&mut app);
        assert_eq!(app.login_gen, 1, "in-flight login must not restart");
        cancel_login(&mut app);
        assert!(!app.want_login);
        assert!(matches!(app.login_ui, LoginUi::Idle));
        assert_eq!(app.login_gen, 2);
    }

    #[test]
    fn settings_focuses_account_and_draws_login_when_logged_out() {
        let mut app = test_app();
        app.logged_in = false;
        app.area = Rect::new(0, 0, 80, 28);
        open_settings(&mut app);
        assert_eq!(app.setting_field, SettingField::Kind);
        let opts = test_opts("grok-4.6", ReasoningEffort::High);
        let mut terminal = Terminal::new(ratatui::backend::TestBackend::new(80, 28)).unwrap();
        terminal
            .draw(|f| {
                let _ = draw_settings(f, &mut app, &opts);
            })
            .unwrap();
        assert!(app.hits.iter().any(|(_, h)| matches!(h, Hit::AccountBtn)));
        assert!(app.hits.iter().any(|(_, h)| matches!(h, Hit::ProviderXai)));
        app.setting_field = SettingField::Account;
        handle_settings_key(&mut app, &mut test_opts("grok-4.6", ReasoningEffort::High), KeyCode::Enter, KeyModifiers::NONE);
        assert!(app.want_login);
    }

    #[test]
    fn custom_api_settings_show_endpoint_and_model_fields() {
        let mut app = test_app();
        app.logged_in = true;
        app.conn.kind = ProviderKind::Openai;
        app.conn.base_url = "http://127.0.0.1:40056/v1".into();
        app.conn.model = "Qwen3.8-27B-ABLITERATED-Q8_0".into();
        app.endpoint_edit = Edit::at_end(app.conn.base_url.clone());
        app.model_edit = Edit::at_end(app.conn.model.clone());
        app.context_edit = Edit::at_end("262K".into());
        app.area = Rect::new(0, 0, 80, 36);
        open_settings(&mut app);
        let opts = test_opts("Qwen3.8-27B-ABLITERATED-Q8_0", ReasoningEffort::High);
        let mut terminal = Terminal::new(ratatui::backend::TestBackend::new(80, 36)).unwrap();
        terminal
            .draw(|f| {
                let _ = draw_settings(f, &mut app, &opts);
            })
            .unwrap();
        assert!(
            app.hits
                .iter()
                .any(|(_, h)| matches!(h, Hit::SettingEndpoint)),
            "endpoint field missing: {:?}",
            app.hits.iter().map(|(_, h)| *h).collect::<Vec<_>>()
        );
        assert!(app.hits.iter().any(|(_, h)| matches!(h, Hit::SettingModel)));
        assert!(app.hits.iter().any(|(_, h)| matches!(h, Hit::SettingContext)));
        assert!(
            !app.hits.iter().any(|(_, h)| matches!(h, Hit::AccountBtn)),
            "Grok login must hide on custom API"
        );
    }

    #[test]
    fn logout_button_width_fits_full_cjk_label() {
        assert_eq!(display_cols(" 登出 "), 6);
        assert!(display_cols(" 登出 ") > " 登出 ".chars().count() as u16);
        let mut app = test_app();
        app.logged_in = true;
        app.area = Rect::new(0, 0, 80, 28);
        open_settings(&mut app);
        let opts = test_opts("grok-4.6", ReasoningEffort::High);
        let mut terminal = Terminal::new(ratatui::backend::TestBackend::new(80, 28)).unwrap();
        terminal
            .draw(|f| {
                let _ = draw_settings(f, &mut app, &opts);
            })
            .unwrap();
        let btn = app
            .hits
            .iter()
            .find(|(_, h)| *h == Hit::AccountBtn)
            .expect("logout button")
            .0;
        assert!(
            btn.width >= display_cols(" 登出 "),
            "logout hit width {} cannot fit 登出",
            btn.width
        );
    }

    #[test]
    fn sending_while_logged_out_points_at_settings_not_cli() {
        let mut app = test_app();
        app.logged_in = false;
        let opts = test_opts("grok-4.6", ReasoningEffort::High);
        let sink = Arc::new(FanoutSink { sinks: vec![] });
        let (done_tx, _done_rx) = mpsc::unbounded_channel();
        start_or_send(&mut app, &opts, &sink, &done_tx, "hi".into(), true);
        let err = app
            .rows
            .iter()
            .find_map(|r| match r {
                Row::Err(s) => Some(s.as_str()),
                _ => None,
            })
            .unwrap();
        assert!(err.contains("設定"), "{err}");
        assert!(!err.contains("另開終端"), "{err}");
    }

    #[test]
    fn switching_back_to_grok_resets_custom_model_and_route() {
        let mut app = test_app();
        let mut opts = test_opts("Qwen3.8-27B", ReasoningEffort::High);
        app.conn.kind = ProviderKind::Openai;
        app.conn.base_url = "http://127.0.0.1:40056/v1".into();
        app.conn.model = "Qwen3.8-27B".into();
        app.conn.api_key = "sk-test".into();
        app.endpoint_edit = Edit::at_end(app.conn.base_url.clone());
        app.model_edit = Edit::at_end(app.conn.model.clone());
        rebuild_catalog(&mut app, &opts);
        assert!(app.conn.route().is_openai());

        set_provider_kind(&mut app, &mut opts, ProviderKind::Xai);

        assert_eq!(app.conn.kind, ProviderKind::Xai);
        assert!(
            ProviderConfig::looks_like_grok(&opts.model),
            "opts.model={}",
            opts.model
        );
        assert!(
            ProviderConfig::looks_like_grok(&app.conn.model),
            "conn.model={}",
            app.conn.model
        );
        assert!(
            !app.conn.route_for(&opts.model).is_openai(),
            "route still openai with model={}",
            opts.model
        );
        assert!(app.want_catalog);
        // Custom endpoint is kept for when the user switches back.
        assert_eq!(app.conn.base_url, "http://127.0.0.1:40056/v1");
    }

    #[test]
    fn sending_qwen_on_xai_kind_asks_for_custom_endpoint() {
        let mut app = test_app();
        app.logged_in = true;
        app.conn.kind = ProviderKind::Xai;
        app.conn.model = "Qwen3.8-27B-ABLITERATED-Q8_0".into();
        let opts = test_opts("Qwen3.8-27B-ABLITERATED-Q8_0", ReasoningEffort::High);
        let sink = Arc::new(FanoutSink { sinks: vec![] });
        let (done_tx, _done_rx) = mpsc::unbounded_channel();
        start_or_send(&mut app, &opts, &sink, &done_tx, "hi".into(), true);
        let err = app
            .rows
            .iter()
            .find_map(|r| match r {
                Row::Err(s) => Some(s.as_str()),
                _ => None,
            })
            .unwrap();
        assert!(err.contains("自訂 API"), "{err}");
        assert!(!err.contains("xAI"), "{err}");
        assert!(!app.running);
    }

    #[test]
    fn boot_resumes_existing_chat_and_does_not_create() {
        let dir = tempfile::tempdir().unwrap();
        let store = SessionStore::open_at(dir.path().to_path_buf()).unwrap();
        let mut real = store.create(PathBuf::from(".")).unwrap();
        real.name = "實作登入".into();
        real.named = true;
        real.updated_at = chrono::Utc::now() - chrono::Duration::seconds(30);
        store.save_meta(&real).unwrap();
        store
            .save_transcript(
                &real.id,
                &serde_json::to_value(vec![Row::User("先前的對話".into())]).unwrap(),
            )
            .unwrap();
        let blank = store.create(PathBuf::from(".")).unwrap();
        let listed = store.list().unwrap();
        assert_eq!(listed[0].id, blank.id, "blank is newest");
        assert_eq!(listed.len(), 2);

        let boot = boot_session(Some(&store), &listed, PathBuf::from("."));
        assert!(!boot.created);
        assert_eq!(boot.session.id, real.id);
        assert!(
            matches!(&boot.rows[0], Row::User(u) if u.text == "先前的對話"),
            "expected resumed user row"
        );
        assert_eq!(store.list().unwrap().len(), 2);
    }

    #[test]
    fn boot_empty_store_creates_one_session() {
        let dir = tempfile::tempdir().unwrap();
        let store = SessionStore::open_at(dir.path().to_path_buf()).unwrap();
        let listed = store.list().unwrap();
        assert!(listed.is_empty());
        let boot = boot_session(Some(&store), &listed, PathBuf::from("."));
        assert!(boot.created);
        assert_eq!(store.list().unwrap().len(), 1);
        assert!(boot.rows.iter().all(|r| matches!(r, Row::Meta(_))));
    }

    #[test]
    fn boot_only_blank_draft_resumes_it() {
        let dir = tempfile::tempdir().unwrap();
        let store = SessionStore::open_at(dir.path().to_path_buf()).unwrap();
        let blank = store.create(PathBuf::from(".")).unwrap();
        let listed = store.list().unwrap();
        let boot = boot_session(Some(&store), &listed, PathBuf::from("."));
        assert!(!boot.created);
        assert_eq!(boot.session.id, blank.id);
        assert_eq!(store.list().unwrap().len(), 1);
    }
    #[test]
    fn merged_catalog_lists_grok_and_custom_models() {
        let mut app = test_app();
        inject_two_model_catalog(&mut app);
        let mut opts = test_opts("alpha", ReasoningEffort::High);
        ingest_custom_catalog(
            &mut app,
            &mut opts,
            crate::catalog::parse_catalog_json(
                r#"{"data":[{"id":"qwen-2","name":"qwen-2"},{"id":"alpha","name":"alpha"}]}"#,
            )
            .map_err(Error::Provider),
        );
        let (ids, names): (Vec<String>, Vec<String>) =
            model_choices(&app, &opts).into_iter().unzip();
        assert_eq!(ids, ["alpha", "beta", "qwen-2"], "merged, deduped, grok first");
        assert!(names[2].contains("自訂"), "custom source must be tagged: {names:?}");
        assert!(model_picker_available(&app));

        ingest_custom_catalog(&mut app, &mut opts, Err(Error::Provider("no /models".into())));
        let ids: Vec<String> = model_choices(&app, &opts)
            .into_iter()
            .map(|(id, _)| id)
            .collect();
        assert_eq!(ids, ["alpha", "beta"], "failed fetch keeps grok + current model");
        assert!(app.custom_cat_err.is_some());
    }

    #[test]
    fn custom_panel_uses_dropdown_once_models_are_fetched() {
        let mut app = test_app();
        app.conn.kind = ProviderKind::Openai;
        app.conn.base_url = "http://127.0.0.1:9/v1".into();
        app.conn.model = "qwen-2".into();
        app.model_edit = Edit::at_end("qwen-2".into());
        let mut opts = test_opts("qwen-2", ReasoningEffort::High);
        assert!(
            !model_picker_available(&app),
            "before the fetch the model stays hand-typed"
        );
        assert!(setting_is_edit(&mut app, SettingField::Model));
        ingest_custom_catalog(
            &mut app,
            &mut opts,
            crate::catalog::parse_catalog_json(
                r#"{"data":[{"id":"qwen-2","name":"qwen-2"},{"id":"m2","name":"m2"}]}"#,
            )
            .map_err(Error::Provider),
        );
        assert!(model_picker_available(&app));
        assert!(!setting_is_edit(&mut app, SettingField::Model), "list replaces typing");
        app.setting_field = SettingField::Model;
        handle_settings_key(&mut app, &mut opts, KeyCode::Enter, KeyModifiers::NONE);
        assert_eq!(app.drop, Some(DropKind::Model), "Enter opens the model dropdown");
    }

    fn setting_is_edit(app: &mut App, field: SettingField) -> bool {
        app.setting_field = field;
        setting_edit_mut(app).is_some()
    }

    #[test]
    fn picking_custom_model_on_grok_panel_keeps_panel_and_routes_by_model() {
        let mut app = test_app();
        inject_two_model_catalog(&mut app);
        app.conn.base_url = "http://127.0.0.1:9/v1".into();
        let mut opts = test_opts("grok-4.6", ReasoningEffort::High);
        opts.web_search = true;
        apply_selected_model(&mut app, &mut opts, "qwen-2".into());
        assert_eq!(app.conn.kind, ProviderKind::Xai, "panel must not flip");
        assert!(app.conn.route_for(&opts.model).is_openai(), "model still routes to the endpoint");
        let k = app.knobs.lock().unwrap();
        assert_eq!(k.model, "qwen-2");
        assert!(k.server_tools.is_empty(), "xai search tools must not follow a custom model");
    }

