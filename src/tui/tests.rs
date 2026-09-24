//! App-level tests: keyboard and mouse flows, layout at terminal sizes, the
//! web bridge, and persistence. Stores live in temp dirs.

use std::sync::{Arc, Mutex};

use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use ratatui::backend::TestBackend;
use ratatui::layout::Position;
use ratatui::Terminal;
use serde_json::json;

use super::app::{submit_kind, App, BottomTab, Focus, Hit, Rt, SendMode, SideItem, SideView, Submit, Tab, TaskUi};
use super::bridge;
use super::edit::Edit;
use super::input;
use super::model::agents::{AgentState, EventKind};
use super::model::rows::Row;
use super::model::session::{intro_rows, ChatSel, Session};
use super::settings::{DropKind, Field, Settings};
use super::{ui, TuiOptions};
use crate::agent::CancelFlag;
use crate::ask::Choice;
use crate::config::{ProviderConfig, ProviderKind};
use crate::events::{AgentEvent, EventMeta};
use crate::hub::UiCommand;
use crate::provider::ReasoningEffort;
use crate::session::{SessionMeta, SessionStore};
use crate::skills::SkillStore;
use crate::task::TaskHub;

struct Fixture {
    app: App,
    _dir: tempfile::TempDir,
}

fn opts(dir: &std::path::Path) -> TuiOptions {
    TuiOptions {
        model: "grok-4.6".into(),
        events: dir.join("events.jsonl"),
        workspace: dir.to_path_buf(),
        max_turns: 0,
        web_search: false,
        dispatcher: false,
        child_model: String::new(),
        reasoning_effort: ReasoningEffort::High,
    }
}

fn fixture_with(store: bool) -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let store = store.then(|| SessionStore::open_at(dir.path().join("sessions")).unwrap());
    let meta = match &store {
        Some(s) => s.create(dir.path().to_path_buf()).unwrap(),
        None => {
            let mut m = SessionMeta::new(dir.path().to_path_buf());
            m.id = "s".into();
            m
        }
    };
    let task = TaskHub::new(meta.id.clone());
    let boot = Session::new(meta, intro_rows(), task);
    let settings = Settings::new(ProviderConfig::default(), dir.path().join("no-auth.json"));
    let skills = Arc::new(Mutex::new(SkillStore::open_at(dir.path().join("skills"), dir.path().to_path_buf())));
    let app = App::new(opts(dir.path()), store, boot, settings, skills, Rt::dummy(), None);
    Fixture { app, _dir: dir }
}

fn fixture() -> Fixture {
    fixture_with(false)
}

/// Point the app at a ready custom endpoint, so sends pass the login check.
fn connect(app: &mut App) {
    app.settings.conn.kind = ProviderKind::Openai;
    app.settings.endpoint = Edit::at_end("http://127.0.0.1:1/v1");
    app.settings.model_edit = Edit::at_end("local-model");
    app.opts.model = "local-model".into();
    app.snapshot_conn();
    assert!(app.settings.logged_in, "a custom endpoint with a model is ready");
}

fn meta(app: &App, path: &str) -> EventMeta {
    EventMeta {
        ts: chrono::Utc::now(),
        agent_name: if path.is_empty() { "root".into() } else { path.rsplit('/').next().unwrap().into() },
        run_id: if path.is_empty() { app.current.clone() } else { format!("run-{path}") },
        parent_run_id: (!path.is_empty()).then(|| app.current.clone()),
        path: path.into(),
    }
}

fn key(app: &mut App, code: KeyCode, mods: KeyModifiers) -> bool {
    input::handle(app, Event::Key(KeyEvent::new(code, mods)))
}

fn press(app: &mut App, code: KeyCode) -> bool {
    key(app, code, KeyModifiers::NONE)
}

fn type_text(app: &mut App, s: &str) {
    for c in s.chars() {
        press(app, KeyCode::Char(c));
    }
}

fn mouse(app: &mut App, kind: MouseEventKind, col: u16, row: u16) {
    input::handle(app, Event::Mouse(MouseEvent { kind, column: col, row, modifiers: KeyModifiers::NONE }));
}

fn click(app: &mut App, col: u16, row: u16) {
    mouse(app, MouseEventKind::Down(MouseButton::Left), col, row);
    mouse(app, MouseEventKind::Up(MouseButton::Left), col, row);
}

/// Render and return the screen text without whitespace (CJK cells span two).
fn render(app: &mut App, w: u16, h: u16) -> String {
    let mut term = Terminal::new(TestBackend::new(w, h)).unwrap();
    term.draw(|f| {
        let _ = ui::draw(f, app, false);
    })
    .unwrap();
    term.backend()
        .buffer()
        .content
        .iter()
        .map(|c| c.symbol())
        .collect::<String>()
        .chars()
        .filter(|c| !c.is_whitespace())
        .collect()
}

fn hit_rect(app: &App, want: Hit) -> ratatui::layout::Rect {
    app.ui
        .hits
        .iter()
        .rev()
        .find(|(_, h)| *h == want)
        .map(|(r, _)| *r)
        .unwrap_or_else(|| panic!("no hit {want:?}"))
}

fn spawn_child(app: &mut App, name: &str) {
    let m = meta(app, "");
    app.route_event(AgentEvent::ChildSpawned {
        meta: m.clone(),
        name: name.into(),
        agent_card_url: String::new(),
        prompt: format!("do {name}"),
        model: "fast".into(),
    });
    app.route_event(AgentEvent::AgentMessage {
        meta: m,
        from: "root".into(),
        to: name.into(),
        text: format!("do {name}"),
    });
}

#[test]
fn submit_queues_only_while_running() {
    assert_eq!(submit_kind(false, false, SendMode::Queue), Submit::Start);
    assert_eq!(submit_kind(true, true, SendMode::Queue), Submit::Queue);
    assert_eq!(submit_kind(true, true, SendMode::Insert), Submit::Insert);
    assert_eq!(submit_kind(true, false, SendMode::Queue), Submit::Insert);
}

#[test]
fn typing_and_enter_queue_while_running_and_ctrl_enter_inserts() {
    let mut fx = fixture();
    let app = &mut fx.app;
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    connect(app);
    let s = app.cur_mut();
    s.inbox = Some(tx);
    s.chat.running = true;
    type_text(app, "later");
    press(app, KeyCode::Enter);
    assert_eq!(app.cur().queue.len(), 1, "Enter queues while running");
    assert!(rx.try_recv().is_err());
    type_text(app, "now");
    key(app, KeyCode::Enter, KeyModifiers::CONTROL);
    assert_eq!(rx.try_recv().unwrap().text, "now", "Ctrl+Enter inserts into the running turn");
    assert!(matches!(app.cur().chat.rows.last(), Some(Row::User(u)) if u.text == "now"));

    // Once the agent waits, the queue drains.
    app.cur_mut().chat.running = false;
    app.cur_mut().chat.awaiting = true;
    app.flush_queues();
    assert_eq!(rx.try_recv().unwrap().text, "later");
    assert!(app.cur().queue.is_empty());
}

#[test]
fn not_logged_in_shows_how_to_connect() {
    let mut fx = fixture();
    let app = &mut fx.app;
    app.settings.logged_in = false;
    type_text(app, "hi");
    press(app, KeyCode::Enter);
    assert!(matches!(app.cur().chat.rows.last(), Some(Row::Err(e)) if e.contains("尚未登入")));
    assert!(app.cur().inbox.is_none());
}

#[test]
fn child_agents_get_read_only_tabs() {
    let mut fx = fixture();
    let app = &mut fx.app;
    spawn_child(app, "coder");
    let m = meta(app, "coder");
    app.route_event(AgentEvent::TurnStarted { meta: m.clone(), turn: 1 });
    app.route_event(AgentEvent::ModelDelta { meta: m, text: "child text".into() });
    assert!(!app.cur().chat.rows.iter().any(|r| matches!(r, Row::Agent(a) if a.text == "child text")));

    app.ui.side_open = true;
    app.ui.side_view = SideView::Agents;
    let out = render(app, 140, 36);
    assert!(out.contains("coder"), "the agent tree lists the child");
    app.show_chat(Some("coder".into()));
    assert_eq!(app.active_tab(), Tab::Agent("coder".into()));
    assert!(app.cur().viewing_agent());
    let out = render(app, 140, 36);
    assert!(out.contains("childtext"));
    assert!(out.contains("唯讀"));
    assert!(!ui::wants_cursor(app), "no composer on a child tab");

    // Scrolling keys stay on the tab; typing returns to the main chat.
    press(app, KeyCode::PageUp);
    assert_eq!(app.active_tab(), Tab::Agent("coder".into()));
    press(app, KeyCode::Char('x'));
    assert_eq!(app.active_tab(), Tab::Chat);
    assert_eq!(app.cur().draft.text, "x");

    app.show_chat(Some("coder".into()));
    press(app, KeyCode::Esc);
    assert_eq!(app.active_tab(), Tab::Chat, "Esc on a child tab goes back");
}

#[test]
fn workbench_keys_toggle_views_panels_and_tabs() {
    let mut fx = fixture();
    let app = &mut fx.app;
    spawn_child(app, "a");
    spawn_child(app, "b");
    app.ui.side_open = true;
    key(app, KeyCode::Char('b'), KeyModifiers::CONTROL);
    assert!(!app.ui.side_open);
    key(app, KeyCode::Char('2'), KeyModifiers::ALT);
    assert!(app.ui.side_open);
    assert_eq!(app.ui.side_view, SideView::Agents);
    key(app, KeyCode::Char('j'), KeyModifiers::CONTROL);
    assert_eq!(app.ui.bottom, Some(BottomTab::Tools));
    press(app, KeyCode::F(4));
    assert_eq!(app.ui.bottom, None);

    app.show_chat(Some("a".into()));
    app.show_chat(Some("b".into()));
    key(app, KeyCode::Left, KeyModifiers::ALT);
    assert_eq!(app.active_tab(), Tab::Agent("a".into()));
    key(app, KeyCode::Char('0'), KeyModifiers::ALT);
    assert_eq!(app.active_tab(), Tab::Chat);
    press(app, KeyCode::F(2));
    assert_eq!(app.active_tab(), Tab::Settings);
    key(app, KeyCode::Right, KeyModifiers::ALT);
    assert_eq!(app.active_tab(), Tab::Chat, "tabs wrap around");
    app.show_chat(Some("b".into()));
    key(app, KeyCode::Char('w'), KeyModifiers::CONTROL);
    assert_eq!(app.cur().open_tabs, ["a"]);

    // Alt+↓/↑ walk the agent tree, opening tabs.
    app.show_chat(None);
    key(app, KeyCode::Down, KeyModifiers::ALT);
    assert_eq!(app.active_tab(), Tab::Agent("a".into()));
    key(app, KeyCode::Down, KeyModifiers::ALT);
    assert_eq!(app.active_tab(), Tab::Agent("b".into()));
    key(app, KeyCode::Down, KeyModifiers::ALT);
    assert_eq!(app.active_tab(), Tab::Chat, "wraps back to the main chat");
    key(app, KeyCode::Up, KeyModifiers::ALT);
    assert_eq!(app.active_tab(), Tab::Agent("b".into()));
}

#[test]
fn every_view_renders_at_small_and_large_sizes() {
    for (w, h) in [(60, 20), (80, 24), (140, 40)] {
        for view in SideView::ALL {
            for bottom in [None, Some(BottomTab::Tools), Some(BottomTab::Output), Some(BottomTab::Events)] {
                let mut fx = fixture();
                let app = &mut fx.app;
                spawn_child(app, "coder");
                app.cur_mut().task.start_goal("驗證控制台");
                app.cur_mut().agents.log("", EventKind::Error, "boom");
                app.ui.side_open = true;
                app.ui.side_view = view;
                app.ui.bottom = bottom;
                let out = render(app, w, h);
                assert!(out.contains(view.title()), "{view:?} at {w}x{h}");
                if w >= crate::tui::theme::SIDEBAR_MIN_TERM {
                    assert!(out.contains("主對話"), "tab strip at {w}x{h}");
                    if view == SideView::Task {
                        assert!(out.contains("驗證控制台"));
                    }
                    if bottom == Some(BottomTab::Events) {
                        assert!(out.contains("boom"));
                    }
                }
            }
        }
        let mut fx = fixture();
        fx.app.ui.side_open = false;
        fx.app.open_settings();
        assert!(render(&mut fx.app, w, h).contains("連線"), "settings at {w}x{h}");
        fx.app.set_provider_kind(ProviderKind::Openai);
        let out = render(&mut fx.app, w, h);
        assert!(out.contains("端點"), "custom panel at {w}x{h}");
    }
}

#[test]
fn clicks_open_agents_changes_tools_and_settings() {
    let mut fx = fixture();
    let app = &mut fx.app;
    spawn_child(app, "coder");
    let m = meta(app, "coder");
    app.route_event(AgentEvent::ToolStarted {
        meta: m.clone(),
        call_id: "w1".into(),
        name: "write_file".into(),
        args: json!({"path": "a.rs"}),
        kind: "client".into(),
    });
    app.route_event(AgentEvent::ToolFinished {
        meta: m,
        call_id: "w1".into(),
        name: "write_file".into(),
        output: r#"{"path":"a.rs","kind":"create","diff":"+x"}"#.into(),
    });
    app.ui.side_open = true;
    app.ui.side_view = SideView::Agents;
    render(app, 140, 36);
    let item = app.ui.side_items.iter().position(|i| *i == SideItem::Agent("coder".into())).unwrap();
    let r = hit_rect(app, Hit::SideItem(item as u16));
    click(app, r.x + 2, r.y);
    assert_eq!(app.active_tab(), Tab::Agent("coder".into()));

    app.show_chat(None);
    app.ui.side_view = SideView::Changes;
    render(app, 140, 36);
    let item = app.ui.side_items.iter().position(|i| matches!(i, SideItem::Change { .. })).unwrap();
    let r = hit_rect(app, Hit::SideItem(item as u16));
    click(app, r.x + 2, r.y);
    assert_eq!(app.active_tab(), Tab::Agent("coder".into()));
    assert_eq!(app.cur().view().open_tool, Some((1, 0)), "row 0 is the instruction, row 1 the tools");

    app.show_chat(None);
    app.ui.bottom = Some(BottomTab::Tools);
    render(app, 140, 36);
    let r = hit_rect(app, Hit::BottomRow(0));
    click(app, r.x + 1, r.y);
    assert_eq!(app.active_tab(), Tab::Agent("coder".into()), "a tool row opens its call");

    render(app, 140, 36);
    let r = hit_rect(app, Hit::StatusModel);
    click(app, r.x + r.width / 2, r.y);
    assert_eq!(app.active_tab(), Tab::Settings);
    render(app, 140, 36);
    let close = app.tabs().iter().position(|t| *t == Tab::Settings).unwrap();
    let r = hit_rect(app, Hit::EditorTabClose(close as u16));
    click(app, r.x, r.y);
    assert!(!app.ui.settings_open);
}

#[test]
fn settings_keys_cycle_fields_toggle_and_pick() {
    let mut fx = fixture();
    let app = &mut fx.app;
    app.open_settings();
    assert_eq!(app.settings.field, Field::Kind);
    press(app, KeyCode::Tab);
    assert_eq!(app.settings.field, Field::Account);
    while app.settings.field != Field::Search {
        press(app, KeyCode::Tab);
    }
    press(app, KeyCode::Enter);
    assert!(app.opts.web_search);
    assert!(!app.knobs.lock().unwrap().server_tools.is_empty(), "search reaches the running session");
    press(app, KeyCode::Tab);
    assert_eq!(app.settings.field, Field::Dispatcher);
    press(app, KeyCode::Char(' '));
    assert!(app.knobs.lock().unwrap().dispatcher);

    app.settings.field = Field::Effort;
    press(app, KeyCode::Enter);
    assert_eq!(app.settings.drop, Some(DropKind::Effort));
    press(app, KeyCode::Esc);
    assert!(app.settings.drop.is_none());

    // Custom API: typed endpoint.
    app.settings.field = Field::Kind;
    press(app, KeyCode::Right);
    assert!(app.settings.conn.kind.is_openai());
    assert_eq!(app.settings.field, Field::Endpoint);
    type_text(app, "http://127.0.0.1:9/v1");
    press(app, KeyCode::Enter);
    assert_eq!(app.settings.conn.base_url, "http://127.0.0.1:9/v1");
    assert_eq!(app.settings.field, Field::ApiKey);
    press(app, KeyCode::Esc);
    assert_eq!(app.active_tab(), Tab::Chat);
}

#[test]
fn questionnaire_by_keys() {
    let mut fx = fixture();
    let app = &mut fx.app;
    app.cur_mut().ask_hub = Some(crate::ask::AskUserHub::new());
    let m = meta(app, "");
    app.route_event(AgentEvent::AskUser {
        meta: m,
        question: "pick".into(),
        allow_multiple: false,
        options: vec![
            Choice { id: "a".into(), label: "A".into(), input: false },
            Choice { id: "b".into(), label: "B".into(), input: false },
        ],
    });
    assert!(app.cur().ask.is_some());
    assert!(render(app, 100, 30).contains("問卷"));
    press(app, KeyCode::Down);
    press(app, KeyCode::Enter);
    assert!(app.cur().ask.is_none());
    assert!(matches!(app.cur().chat.rows.last(), Some(Row::Meta(m)) if m.contains("你選了  B")));
}

#[test]
fn background_sessions_update_without_stealing_focus() {
    let mut fx = fixture_with(true);
    let app = &mut fx.app;
    let first = app.current.clone();
    app.cur_mut().chat.push(Row::User("keep me".into()));
    app.cur_mut().meta.named = true;
    let ws = app.workspace();
    app.create_chat(ws);
    let second = app.current.clone();
    assert_ne!(first, second);
    let m = EventMeta {
        ts: chrono::Utc::now(),
        agent_name: "root".into(),
        run_id: first.clone(),
        parent_run_id: None,
        path: String::new(),
    };
    app.route_event(AgentEvent::ModelDelta { meta: m.clone(), text: "bg work".into() });
    app.route_event(AgentEvent::AskUser {
        meta: m,
        question: "hidden?".into(),
        allow_multiple: false,
        options: vec![Choice { id: "a".into(), label: "A".into(), input: false }],
    });
    assert!(app.sessions[&first].ask.is_none(), "a hidden chat cannot ask");
    assert_eq!(app.current, second);
    app.switch_to(&first);
    assert!(app.cur().chat.rows.iter().any(|r| matches!(r, Row::Agent(a) if a.text == "bg work")));
}

#[test]
fn transcripts_and_agents_survive_a_reload() {
    let mut fx = fixture_with(true);
    let app = &mut fx.app;
    let id = app.current.clone();
    let m = meta(app, "");
    app.route_event(AgentEvent::ModelFinished {
        meta: m,
        text: "saved answer".into(),
        finish: "stop".into(),
        input_tokens: 0,
        cached_tokens: 0,
    });
    spawn_child(app, "coder");
    app.persist_all();
    let store = app.store.take().unwrap();
    let mut fx2 = fixture();
    fx2.app.store = Some(store);
    fx2.app.switch_to(&id);
    let s = fx2.app.cur();
    assert!(s.chat.rows.iter().any(|r| matches!(r, Row::Agent(a) if a.text == "saved answer")));
    let coder = s.agents.get("coder").expect("agent restored");
    assert_eq!(coder.state, AgentState::Exited, "processes do not survive a restart");
    assert!(!coder.transcript.rows.is_empty());
}

#[test]
fn esc_closes_inner_things_then_stops_work_then_children() {
    let mut fx = fixture();
    let app = &mut fx.app;
    let cancel = CancelFlag::new();
    app.cur_mut().cancel = Some(cancel.clone());
    app.cur_mut().chat.running = true;
    app.cur_mut().view_mut().open_tool = Some((0, 0));
    press(app, KeyCode::Esc);
    assert!(app.cur().view().open_tool.is_none());
    assert_eq!(cancel.trips(), 0, "first Esc only closed the popup");
    press(app, KeyCode::Esc);
    assert_eq!(cancel.trips(), 1);
    assert_eq!(app.cur().chat.status, "中斷中");

    app.cur_mut().chat.running = false;
    spawn_child(app, "kid");
    press(app, KeyCode::Esc);
    assert_eq!(cancel.trips(), 2, "idle parent: Esc reaches working children");
}

#[test]
fn rename_and_new_chat_by_keys() {
    let mut fx = fixture_with(true);
    let app = &mut fx.app;
    let id = app.current.clone();
    app.begin_rename(&id);
    key(app, KeyCode::Char('a'), KeyModifiers::CONTROL);
    type_text(app, "新名字");
    press(app, KeyCode::Enter);
    assert_eq!(app.ui.focus, Focus::Chat);
    assert_eq!(app.cur().meta.name, "新名字");

    key(app, KeyCode::Char('n'), KeyModifiers::CONTROL);
    assert!(app.ui.workspace_pick.is_some());
    press(app, KeyCode::Esc);
    assert!(app.ui.workspace_pick.is_none());
}

#[test]
fn mouse_selects_chat_text_and_wheel_scrolls() {
    let mut fx = fixture();
    let app = &mut fx.app;
    for i in 0..60 {
        app.cur_mut().chat.push(Row::meta(format!("line {i}")));
    }
    render(app, 100, 30);
    let (x, y) = app.ui.chat_glyphs.last().map(|g| (g.x, g.y)).unwrap();
    mouse(app, MouseEventKind::Down(MouseButton::Left), x, y);
    mouse(app, MouseEventKind::Drag(MouseButton::Left), x + 4, y);
    let sel = app.cur().view().sel;
    assert!(matches!(sel, ChatSel::Text { .. }));
    let text = super::ui::chat::selected_text(&app.cur().chat.rows, &sel);
    assert_eq!(text.as_deref(), Some("line"));
    let inner = app.ui.chat_inner;
    mouse(app, MouseEventKind::ScrollUp, inner.x + 1, inner.y + 1);
    assert!(!app.cur().view().stick_bottom);
    assert_eq!(app.cur().view().scroll, 3);
}

#[test]
fn clock_cells_stay_out_of_the_composer() {
    let mut fx = fixture();
    let app = &mut fx.app;
    let m = meta(app, "");
    app.route_event(AgentEvent::RunStarted { meta: m.clone(), model: "m".into() });
    app.route_event(AgentEvent::ReasoningDelta { meta: m, text: "hmm".into() });
    render(app, 100, 30);
    assert!(!app.ui.think_clocks.is_empty());
    let frame = app.ui.composer_frame;
    let cells = ui::clock_cells(app);
    assert!(!cells.is_empty());
    for (x, y, _) in cells {
        assert!(!frame.contains(Position::new(x, y)));
    }
}

#[test]
fn web_bridge_snapshot_patches_and_commands() {
    let mut fx = fixture_with(true);
    let app = &mut fx.app;
    app.cur_mut().draft = Edit::at_end("typed");
    app.cur_mut().chat.push(Row::User("hello <b>".into()));
    spawn_child(app, "coder");
    let snap = bridge::snapshot(app);
    assert_eq!(snap.composer.text, "typed");
    let logs = bridge::logs(app);
    assert_eq!(logs.agents[0].path, "coder");
    assert_eq!(logs.agents[0].state, "starting");

    let (patches, removed) = bridge::view_patches(app);
    assert!(removed.is_empty());
    let main = patches.iter().find(|p| p.path.is_empty()).unwrap();
    assert!(main.rows.iter().any(|r| r.kind == "user" && r.html.contains("&lt;b&gt;")));
    assert!(patches.iter().any(|p| p.path == "coder"));
    let (again, _) = bridge::view_patches(app);
    assert!(again.is_empty(), "unchanged views are not resent");
    app.cur_mut().chat.push(Row::meta("new"));
    let (next, _) = bridge::view_patches(app);
    assert_eq!(next.len(), 1);
    assert_eq!(next[0].rows.len(), 1, "only the appended row");

    // Not connected: the browser gets a receipt saying why.
    let sid = app.current.clone();
    bridge::apply_command(app, UiCommand::SubmitText { session_id: sid.clone(), request_id: "r1".into(), text: "hi".into(), insert: false });
    assert!(app.web.receipts[0].1.starts_with("未送出"));
    // A queue edit that raced the terminal is refused.
    app.cur_mut().queue.push_back("next".into());
    bridge::apply_command(
        app,
        UiCommand::UpdateQueue { session_id: sid, request_id: "q1".into(), index: 0, expected: "stale".into(), text: "edited".into() },
    );
    assert_eq!(app.cur().queue[0].text, "next");
    bridge::apply_command(app, UiCommand::ToggleDispatcher);
    assert!(app.knobs.lock().unwrap().dispatcher);
    bridge::apply_command(app, UiCommand::SetChildModel { id: " grok-3-mini ".into() });
    assert_eq!(app.knobs.lock().unwrap().child_model, "grok-3-mini");
    bridge::apply_command(app, UiCommand::OpenTask);
    assert!(app.ui.task_ui.is_some());
    bridge::apply_command(app, UiCommand::CloseTask);
    bridge::apply_command(app, UiCommand::NewChat);
    assert!(app.ui.workspace_pick.is_some());
    bridge::apply_command(app, UiCommand::WsCancel);
    assert!(app.ui.workspace_pick.is_none());
    let cancel = CancelFlag::new();
    app.cur_mut().cancel = Some(cancel.clone());
    app.cur_mut().chat.running = true;
    bridge::apply_command(app, UiCommand::Interrupt);
    assert_eq!(cancel.trips(), 1);
}

#[test]
fn paste_goes_to_the_open_field() {
    let mut fx = fixture();
    let app = &mut fx.app;
    input::handle(app, Event::Paste("hello".into()));
    assert_eq!(app.cur().draft.text, "hello");
    app.open_settings();
    app.set_provider_kind(ProviderKind::Openai);
    input::handle(app, Event::Paste("http://x\n".into()));
    assert_eq!(app.settings.endpoint.text, "http://x", "one line in settings");
    app.close_settings();
    app.open_task();
    input::handle(app, Event::Paste("goal".into()));
    assert!(matches!(&app.ui.task_ui, Some(TaskUi::Form(e)) if e.text == "goal"));
}

#[test]
fn ctrl_q_quits_and_ctrl_c_quits_without_a_selection() {
    let mut fx = fixture();
    assert!(key(&mut fx.app, KeyCode::Char('q'), KeyModifiers::CONTROL));
    let mut fx = fixture();
    assert!(key(&mut fx.app, KeyCode::Char('c'), KeyModifiers::CONTROL));
}

// —— Settings & catalogs ——

fn two_model_catalog(app: &mut App) {
    app.settings.grok_catalog = crate::catalog::parse_catalog_value(&json!({
        "data": [
            {"id": "alpha", "name": "Alpha", "supportsReasoningEffort": true,
             "reasoningEffort": "high", "reasoningEfforts": ["low", "high"]},
            {"id": "beta", "name": "Beta", "supportsReasoningEffort": false}
        ]
    }))
    .unwrap();
    app.settings.catalog_status = super::settings::CatalogStatus::Ready;
    app.rebuild_catalog();
}

fn ids(app: &App) -> Vec<String> {
    app.model_choices().into_iter().map(|(id, _)| id).collect()
}

#[test]
fn merged_catalog_lists_grok_then_tagged_custom_models() {
    let mut fx = fixture();
    let app = &mut fx.app;
    app.opts.model = "alpha".into();
    two_model_catalog(app);
    app.ingest_custom_catalog(
        crate::catalog::parse_catalog_json(r#"{"data":[{"id":"qwen-2","name":"qwen-2"},{"id":"alpha","name":"alpha"}]}"#)
            .map_err(crate::error::Error::Provider),
    );
    assert_eq!(ids(app), ["alpha", "beta", "qwen-2"], "merged, deduped, grok first");
    assert!(app.model_choices()[2].1.contains("自訂"));
    app.ingest_custom_catalog(Err(crate::error::Error::Provider("no /models".into())));
    assert_eq!(ids(app), ["alpha", "beta"], "a failed fetch keeps grok + current model");
    assert!(app.settings.custom_err.is_some());
}

#[test]
fn custom_panel_types_models_until_the_endpoint_lists_them() {
    let mut fx = fixture();
    let app = &mut fx.app;
    app.settings.conn.kind = ProviderKind::Openai;
    app.settings.conn.base_url = "http://127.0.0.1:9/v1".into();
    app.opts.model = "qwen-2".into();
    app.open_settings();
    app.settings.field = Field::Model;
    assert!(!app.settings.model_picker());
    assert!(app.settings.edit_mut().is_some(), "typed before the fetch");
    app.ingest_custom_catalog(
        crate::catalog::parse_catalog_json(r#"{"data":[{"id":"qwen-2","name":"qwen-2"},{"id":"m2","name":"m2"}]}"#)
            .map_err(crate::error::Error::Provider),
    );
    assert!(app.settings.model_picker());
    assert!(app.settings.edit_mut().is_none(), "the list replaces typing");
    press(app, KeyCode::Enter);
    assert_eq!(app.settings.drop, Some(DropKind::Model));
    let before = app.settings.drop_cursor;
    press(app, KeyCode::Down);
    let picked = app.model_choices()[app.settings.drop_cursor].0.clone();
    assert_ne!(app.settings.drop_cursor, before);
    press(app, KeyCode::Enter);
    assert_eq!(app.opts.model, picked);
    assert!(app.settings.drop.is_none());
}

#[test]
fn picking_a_custom_model_on_the_grok_panel_routes_by_model() {
    let mut fx = fixture();
    let app = &mut fx.app;
    two_model_catalog(app);
    app.settings.conn.base_url = "http://127.0.0.1:9/v1".into();
    app.opts.web_search = true;
    app.select_model("qwen-2".into());
    assert_eq!(app.settings.conn.kind, ProviderKind::Xai, "the panel does not flip");
    assert!(app.settings.conn.route_for(&app.opts.model).is_openai());
    let k = app.knobs.lock().unwrap();
    assert_eq!(k.model, "qwen-2");
    assert!(k.server_tools.is_empty(), "no Grok login: nothing to lend search");
    drop(k);
    app.settings.xai_ready = true;
    app.sync_knobs();
    let k = app.knobs.lock().unwrap();
    assert_eq!(k.server_tools, vec![crate::grok_search::GATE.to_string()], "custom models borrow Grok search");
    assert!(k.search);
}

#[test]
fn switching_back_to_grok_resets_model_and_keeps_the_endpoint() {
    let mut fx = fixture();
    let app = &mut fx.app;
    app.opts.model = "Qwen3.8-27B".into();
    app.settings.conn.kind = ProviderKind::Openai;
    app.settings.conn.base_url = "http://127.0.0.1:40056/v1".into();
    app.settings.endpoint = Edit::at_end("http://127.0.0.1:40056/v1");
    app.settings.model_edit = Edit::at_end("Qwen3.8-27B");
    app.set_provider_kind(ProviderKind::Xai);
    assert!(ProviderConfig::looks_like_grok(&app.opts.model), "{}", app.opts.model);
    assert!(!app.settings.conn.route_for(&app.opts.model).is_openai());
    assert!(app.settings.want_catalog);
    assert_eq!(app.settings.conn.base_url, "http://127.0.0.1:40056/v1");
}

#[test]
fn model_switch_clamps_effort_and_arrows_cycle_the_models_list() {
    let mut fx = fixture();
    let app = &mut fx.app;
    two_model_catalog(app);
    app.opts.reasoning_effort = ReasoningEffort::Xhigh;
    app.select_model("alpha".into());
    assert_eq!(app.opts.reasoning_effort, ReasoningEffort::High);
    app.open_settings();
    app.settings.field = Field::Effort;
    press(app, KeyCode::Right);
    assert_eq!(app.opts.reasoning_effort, ReasoningEffort::Low);
    press(app, KeyCode::Right);
    assert_eq!(app.opts.reasoning_effort, ReasoningEffort::High);
}

#[test]
fn catalog_failure_keeps_only_the_current_selection() {
    let mut fx = fixture();
    let app = &mut fx.app;
    app.opts.model = "mine".into();
    app.opts.reasoning_effort = ReasoningEffort::Medium;
    app.ingest_catalog(Err(crate::error::Error::Provider("nope".into())));
    assert!(matches!(app.settings.catalog_status, super::settings::CatalogStatus::Failed(_)));
    assert_eq!(ids(app), ["mine"]);
    let efforts: Vec<_> = app.effort_choices().iter().map(|e| e.value).collect();
    assert_eq!(efforts, [ReasoningEffort::Medium]);
}

#[test]
fn a_rejected_login_offers_a_real_login_again() {
    use super::settings::LoginUi;
    let mut fx = fixture();
    let app = &mut fx.app;
    let dead = crate::auth::TokenSet {
        access_token: "expired".into(),
        refresh_token: "spent".into(),
        id_token: None,
    };
    crate::auth::save_tokens(&app.settings.auth_path, &dead).unwrap();
    app.settings.xai_ready = true;
    app.ingest_catalog(Err(crate::error::Error::Auth(crate::auth::LOGIN_EXPIRED.into())));
    assert!(!app.settings.xai_ready, "a dead token file is not a login");
    // The account button now logs in instead of logging out, and a dead file
    // must not short-circuit the device flow.
    app.activate_account();
    assert_eq!(app.settings.login, LoginUi::Starting);
    assert!(app.settings.want_login);
    assert!(app.settings.auth_path.exists(), "nothing was logged out");
}

#[test]
fn login_events_from_a_stale_attempt_are_ignored() {
    use super::settings::{LoginEvent, LoginUi};
    let mut fx = fixture();
    let app = &mut fx.app;
    app.settings.login_gen = 3;
    app.apply_login_event(LoginEvent::Waiting { gen: 2, url: "u".into(), user_code: "OLD".into() });
    assert_eq!(app.settings.login, LoginUi::Idle);
    app.apply_login_event(LoginEvent::Waiting { gen: 3, url: "u".into(), user_code: "ABCD".into() });
    assert!(matches!(&app.settings.login, LoginUi::Waiting { user_code, .. } if user_code == "ABCD"));
    app.apply_login_event(LoginEvent::Failed { gen: 3, message: "denied".into() });
    assert!(matches!(&app.settings.login, LoginUi::Failed(m) if m == "denied"));
    // Starting a login bumps the generation and asks the runtime to run it.
    app.activate_account();
    assert_eq!(app.settings.login, LoginUi::Starting);
    assert!(app.settings.want_login);
    assert_eq!(app.settings.login_gen, 4);
    app.activate_account();
    assert_eq!(app.settings.login, LoginUi::Idle, "a second press cancels");
}

#[test]
fn a_custom_model_without_endpoint_asks_for_one() {
    let mut fx = fixture();
    let app = &mut fx.app;
    app.opts.model = "Qwen3.8-27B-ABLITERATED-Q8_0".into();
    app.start_or_send("hi".into(), true);
    let err = app
        .cur()
        .chat
        .rows
        .iter()
        .find_map(|r| match r {
            Row::Err(e) => Some(e.clone()),
            _ => None,
        })
        .expect("an error row");
    assert!(err.contains("自訂 API") || err.contains("端點"), "{err}");
    assert!(!app.cur().chat.running);
}

// —— Sessions ——

#[test]
fn boot_resumes_real_chats_and_creates_only_when_empty() {
    let dir = tempfile::tempdir().unwrap();
    let store = SessionStore::open_at(dir.path().to_path_buf()).unwrap();
    let (s, created) = super::app::boot_session(Some(&store), dir.path().to_path_buf());
    assert!(created);
    assert_eq!(store.list().unwrap().len(), 1);
    let (again, created) = super::app::boot_session(Some(&store), dir.path().to_path_buf());
    assert!(!created, "a blank draft is resumed, not duplicated");
    assert_eq!(again.id(), s.id());
    assert_eq!(store.list().unwrap().len(), 1);
}

#[test]
fn new_chat_on_a_blank_draft_only_moves_its_workspace() {
    let mut fx = fixture_with(true);
    let target = fx._dir.path().join("proj");
    std::fs::create_dir_all(&target).unwrap();
    let app = &mut fx.app;
    let id = app.current.clone();
    app.create_chat(target.clone());
    assert_eq!(app.current, id, "no second blank session");
    assert_eq!(app.workspace(), crate::folderpick::existing_dir(&target));
}

#[test]
fn deleting_the_current_chat_opens_another() {
    let mut fx = fixture_with(true);
    let app = &mut fx.app;
    let id = app.current.clone();
    app.delete_session(&id);
    assert_ne!(app.current, id);
    assert!(!app.sessions.contains_key(&id));
    assert!(app.listed.iter().all(|m| m.id != id));
}

#[test]
fn session_titles_follow_the_model_until_renamed() {
    let mut fx = fixture_with(true);
    let app = &mut fx.app;
    let id = app.current.clone();
    let m = meta(app, "");
    app.route_event(AgentEvent::SessionNamed { meta: m.clone(), name: "修測試".into() });
    assert_eq!(app.cur().meta.name, "修測試");
    app.rename_manual(&id, "我的名字");
    app.route_event(AgentEvent::SessionNamed { meta: m, name: "別的".into() });
    assert_eq!(app.cur().meta.name, "我的名字", "a manual name wins");
}

// —— Queue ——

#[test]
fn a_stop_holds_the_queue_until_the_user_speaks() {
    let mut fx = fixture();
    let app = &mut fx.app;
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let s = app.cur_mut();
    s.inbox = Some(tx);
    s.chat.running = true;
    s.queue.push_back("later".into());
    s.chat.interrupt();
    let m = meta(app, "");
    app.route_event(AgentEvent::AwaitingInput { meta: m });
    app.flush_queues();
    assert_eq!(app.cur().chat.status, "已停止");
    assert!(rx.try_recv().is_err(), "a stop is not a cue to send the next item");
    app.cur_mut().chat.status = "待命".into();
    app.flush_queues();
    assert_eq!(rx.try_recv().unwrap().text, "later");
}

#[test]
fn web_submissions_keep_the_terminal_draft_and_are_deduplicated() {
    let mut fx = fixture();
    let app = &mut fx.app;
    connect(app);
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    app.cur_mut().inbox = Some(tx);
    app.cur_mut().chat.running = true;
    app.cur_mut().draft = Edit::at_end("terminal draft");
    let cmd = UiCommand::SubmitText {
        session_id: app.current.clone(),
        request_id: "web-1".into(),
        text: "web message".into(),
        insert: false,
    };
    bridge::apply_command(app, cmd.clone());
    bridge::apply_command(app, cmd);
    assert_eq!(app.cur().draft.text, "terminal draft");
    assert_eq!(app.cur().queue.len(), 1);
    assert_eq!(app.cur().queue[0].text, "web message");
    assert_eq!(app.web.receipts, vec![("web-1".to_string(), "已接收".to_string())]);
}

#[test]
fn pasting_an_image_path_attaches_it() {
    let mut fx = fixture();
    let app = &mut fx.app;
    let ws = app.workspace();
    image::RgbImage::from_pixel(40, 40, image::Rgb([255, 0, 0])).save(ws.join("shot.png")).unwrap();
    input::handle(app, Event::Paste(ws.join("shot.png").to_string_lossy().into_owned()));
    assert_eq!(app.cur().pending.len(), 1);
    assert!(app.cur().draft.is_empty());
}

// —— Images ——

fn with_image(fx: &mut Fixture) -> String {
    let rel = "red.png".to_string();
    image::RgbImage::from_pixel(80, 40, image::Rgb([255, 0, 0]))
        .save(fx.app.workspace().join(&rel))
        .unwrap();
    fx.app.cur_mut().chat.push(Row::User(super::model::rows::UserMsg {
        text: "see".into(),
        images: vec![rel.clone()],
    }));
    fx.app.ui.side_open = false;
    rel
}

#[test]
fn sixel_images_blit_and_overlays_hide_them() {
    let mut fx = fixture();
    with_image(&mut fx);
    let mut picker = ratatui_image::picker::Picker::from_fontsize((8, 16));
    picker.set_protocol_type(ratatui_image::picker::ProtocolType::Sixel);
    fx.app.images.picker = Some(picker);
    let app = &mut fx.app;
    let out = render(app, 90, 32);
    assert!(!out.contains('▀'), "graphics terminals do not get half-blocks");
    assert!(!app.images.blits.is_empty(), "the image is written after the frame");
    let image_hits = app.ui.hits.iter().filter(|(_, h)| *h == Hit::ChatImage(0)).count();
    assert!(image_hits >= 2, "caption and picture are both clickable");
    app.ui.inspector = Some("hook".into());
    render(app, 90, 32);
    assert!(app.images.blits.is_empty(), "an overlay must not be painted over by the image");
}

#[test]
fn halfblock_previews_without_graphics_support() {
    let mut fx = fixture();
    with_image(&mut fx);
    let out = render(&mut fx.app, 90, 32);
    assert!(out.contains('▀'));
    assert!(fx.app.images.blits.is_empty());
}

#[test]
fn activity_bar_shows_labels_when_tall_and_stays_clickable_when_short() {
    let mut fx = fixture();
    let app = &mut fx.app;
    let out = render(app, 120, 30);
    for label in ["對話", "代理", "變更", "背景", "任務", "設定"] {
        assert!(out.contains(label), "missing {label}");
    }
    let agents = hit_rect(app, Hit::Activity(1));
    assert_eq!((agents.width, agents.height), (6, 3), "icon + label + gap is one target");
    render(app, 120, 14);
    let agents = hit_rect(app, Hit::Activity(1));
    assert_eq!(agents.height, 2, "short terminals fall back to the compact bar");
}
