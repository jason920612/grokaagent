//! Task mode in the TUI and the web bridge: the goal form, the status dialog
//! and side panel, proxy rows, the task-model setting, and the queue while
//! the user proxy drives the worker.

use std::sync::{Arc, Mutex};

use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use ratatui::backend::TestBackend;
use ratatui::layout::Rect;
use ratatui::Terminal;
use serde_json::json;
use tokio::sync::mpsc;

use super::app::{task_actions, App, Hit, Rt, SendMode, SideItem, SideView, TaskAction, TaskUi};
use super::bridge;
use super::edit::Edit;
use super::input;
use super::model::rows::Row;
use super::model::session::{intro_rows, Session};
use super::settings::{DropKind, Field, Settings};
use super::{ui, TuiOptions};
use crate::agent::UserTurn;
use crate::ask::{AskUserHub, Choice};
use crate::config::{ProviderConfig, ProviderKind};
use crate::events::{AgentEvent, EventMeta};
use crate::hub::UiCommand;
use crate::provider::ReasoningEffort;
use crate::session::{SessionMeta, SessionStore};
use crate::skills::SkillStore;
use crate::task::{self, TaskHub, TaskPhase, TaskState};

struct Fixture {
    app: App,
    _dir: tempfile::TempDir,
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
    let opts = TuiOptions {
        model: "grok-4.6".into(),
        events: dir.path().join("events.jsonl"),
        workspace: dir.path().to_path_buf(),
        max_turns: 0,
        web_search: false,
        dispatcher: false,
        child_model: String::new(),
        reasoning_effort: ReasoningEffort::High,
    };
    // An empty session id: the hub never writes task.json from tests.
    let task = TaskHub::new("");
    let boot = Session::new(meta, intro_rows(), task);
    let settings = Settings::new(ProviderConfig::default(), dir.path().join("no-auth.json"));
    let skills = Arc::new(Mutex::new(SkillStore::open_at(dir.path().join("skills"), dir.path().to_path_buf())));
    let app = App::new(opts, store, boot, settings, skills, Rt::dummy(), None);
    Fixture { app, _dir: dir }
}

fn fixture() -> Fixture {
    fixture_with(false)
}

/// A live run: sends go to its inbox, and starting a task only kicks it.
fn live_run(app: &mut App) -> mpsc::UnboundedReceiver<UserTurn> {
    let (tx, rx) = mpsc::unbounded_channel();
    app.cur_mut().inbox = Some(tx);
    rx
}

/// A ready custom endpoint, so sends pass the login check.
fn connect(app: &mut App) {
    app.settings.conn.kind = ProviderKind::Openai;
    app.settings.endpoint = Edit::at_end("http://127.0.0.1:1/v1");
    app.settings.model_edit = Edit::at_end("local-model");
    app.opts.model = "local-model".into();
    app.snapshot_conn();
    assert!(app.settings.logged_in);
}

fn key(app: &mut App, code: KeyCode, mods: KeyModifiers) {
    input::handle(app, Event::Key(KeyEvent::new(code, mods)));
}

fn press(app: &mut App, code: KeyCode) {
    key(app, code, KeyModifiers::NONE);
}

fn type_text(app: &mut App, s: &str) {
    for c in s.chars() {
        press(app, KeyCode::Char(c));
    }
}

fn click(app: &mut App, r: Rect) {
    for kind in [MouseEventKind::Down(MouseButton::Left), MouseEventKind::Up(MouseButton::Left)] {
        input::handle(app, Event::Mouse(MouseEvent { kind, column: r.x, row: r.y, modifiers: KeyModifiers::NONE }));
    }
}

/// Render and return the screen text without whitespace (CJK cells span two).
fn render(app: &mut App, w: u16, h: u16) -> String {
    let mut term = Terminal::new(TestBackend::new(w, h)).unwrap();
    term.draw(|f| {
        let _ = ui::draw(f, app, false);
    })
    .unwrap();
    term.backend().buffer().content.iter().map(|c| c.symbol()).collect::<String>().chars().filter(|c| !c.is_whitespace()).collect()
}

fn hit_rect(app: &App, want: Hit) -> Rect {
    app.ui.hits.iter().rev().find(|(_, h)| *h == want).map(|(r, _)| *r).unwrap_or_else(|| panic!("no {want:?} drawn"))
}

fn meta(app: &App, agent: &str) -> EventMeta {
    EventMeta {
        ts: chrono::Utc::now(),
        agent_name: agent.into(),
        run_id: app.current.clone(),
        parent_run_id: None,
        path: String::new(),
    }
}

fn task(app: &App) -> TaskState {
    app.cur().task.snapshot()
}

#[test]
fn the_form_takes_a_goal_and_criteria_and_starts_the_task() {
    let mut fx = fixture();
    let app = &mut fx.app;
    let _rx = live_run(app);
    key(app, KeyCode::Char('t'), KeyModifiers::CONTROL);
    assert!(matches!(&app.ui.task_ui, Some(TaskUi::Form(f)) if !f.editing), "Ctrl+T opens the form");
    let screen = render(app, 110, 34);
    assert!(screen.contains("目標（必填）"), "{screen}");
    assert!(screen.contains("驗收條件／偏好（選填）"), "{screen}");

    // An empty goal is refused.
    press(app, KeyCode::Enter);
    assert_eq!(task(app).phase, TaskPhase::Inactive);
    assert_eq!(app.cur().chat.status, "請填寫任務目標");

    type_text(app, "ship the site");
    key(app, KeyCode::Enter, KeyModifiers::SHIFT);
    type_text(app, "with docs");
    press(app, KeyCode::Tab);
    type_text(app, "tests pass");
    key(app, KeyCode::Enter, KeyModifiers::SHIFT);
    type_text(app, "no new deps");
    press(app, KeyCode::BackTab);
    type_text(app, "!");
    match &app.ui.task_ui {
        Some(TaskUi::Form(f)) => {
            assert_eq!(f.goal.text, "ship the site\nwith docs!");
            assert_eq!(f.criteria.text, "tests pass\nno new deps");
        }
        _ => panic!("form closed early"),
    }
    press(app, KeyCode::Enter);
    let t = task(app);
    assert_eq!(t.goal, "ship the site\nwith docs!");
    assert_eq!(t.criteria, "tests pass\nno new deps");
    assert_eq!(t.phase, TaskPhase::NeedPlan);
    assert!(matches!(app.ui.task_ui, Some(TaskUi::Status(_))), "the dialog shows the task");
    assert!(matches!(app.cur().chat.rows.last(), Some(Row::Meta(m)) if m.contains("驗收條件：tests pass")));
}

#[test]
fn ctrl_s_submits_and_starts_a_run_when_none_is_alive() {
    let mut fx = fixture();
    let app = &mut fx.app;
    connect(app);
    app.open_task();
    input::handle(app, Event::Paste("refactor the parser".into()));
    press(app, KeyCode::Tab);
    input::handle(app, Event::Paste("keep the API".into()));
    key(app, KeyCode::Char('s'), KeyModifiers::CONTROL);
    let t = task(app);
    assert_eq!((t.goal.as_str(), t.criteria.as_str()), ("refactor the parser", "keep the API"));
    assert!(app.cur().inbox.is_some(), "a kick run was started for the proxy");
    assert_eq!(app.cur().meta.name, "任務模式");
}

#[test]
fn edit_prefills_the_form_and_edits_the_task() {
    let mut fx = fixture();
    let app = &mut fx.app;
    let _rx = live_run(app);
    app.cur().task.start_task("old goal", "old criteria");
    app.open_task();
    assert!(matches!(app.ui.task_ui, Some(TaskUi::Status(_))));
    press(app, KeyCode::Char('e'));
    match &app.ui.task_ui {
        Some(TaskUi::Form(f)) => {
            assert!(f.editing);
            assert_eq!((f.goal.text.as_str(), f.criteria.text.as_str()), ("old goal", "old criteria"));
        }
        _ => panic!("修改 opens the form"),
    }
    assert!(render(app, 110, 34).contains("修改任務"));
    // Esc goes back to the dialog, not out of it.
    press(app, KeyCode::Esc);
    assert!(matches!(app.ui.task_ui, Some(TaskUi::Status(_))));
    press(app, KeyCode::Char('e'));
    type_text(app, " v2");
    press(app, KeyCode::Tab);
    type_text(app, ", fast");
    press(app, KeyCode::Enter);
    let t = task(app);
    assert_eq!((t.goal.as_str(), t.criteria.as_str()), ("old goal v2", "old criteria, fast"));
    let note = t.proxy_log.last().expect("the proxy is told");
    assert_eq!(note.who, "user");
    assert!(note.text.contains("old goal v2"), "{}", note.text);
    assert!(app.cur().task.take_review_request(), "the proxy looks again");
    assert!(matches!(app.ui.task_ui, Some(TaskUi::Status(_))));
}

#[test]
fn editing_a_handed_back_task_starts_it_over() {
    let mut fx = fixture();
    let app = &mut fx.app;
    let _rx = live_run(app);
    app.cur().task.replace(TaskState {
        goal: "g".into(),
        phase: TaskPhase::HandedBack,
        rounds: 4,
        ..TaskState::default()
    });
    app.open_task();
    let acts = task_actions(&task(app));
    assert_eq!(acts, [TaskAction::Edit, TaskAction::End, TaskAction::Close], "nothing to pause or accept");
    press(app, KeyCode::Char('e'));
    assert!(render(app, 110, 34).contains("修改並重新開始任務"));
    press(app, KeyCode::Tab);
    type_text(app, "use option B");
    press(app, KeyCode::Enter);
    let t = task(app);
    assert_eq!((t.phase, t.criteria.as_str(), t.rounds), (TaskPhase::NeedPlan, "use option B", 0));
}

#[test]
fn dialog_keys_call_the_hub() {
    let mut fx = fixture();
    let app = &mut fx.app;
    let _rx = live_run(app);
    app.cur().task.start_task("goal", "");
    app.open_task();
    press(app, KeyCode::Char('p'));
    assert!(task(app).paused);
    let screen = render(app, 110, 34);
    assert!(screen.contains("規劃中·已暫停·回合0"), "{screen}");
    assert!(screen.contains("繼續(P)") && !screen.contains("立即審查(R)"), "{screen}");
    press(app, KeyCode::Char('r'));
    assert!(!app.cur().task.take_review_request(), "a paused proxy makes no calls");
    assert!(app.cur().chat.status.contains("已暫停"));
    press(app, KeyCode::Char('P'));
    assert!(!task(app).paused);
    press(app, KeyCode::Char('r'));
    assert!(app.cur().task.take_review_request());

    // Enter starts on 關閉; arrows move to an action.
    press(app, KeyCode::Right);
    press(app, KeyCode::Enter);
    assert!(task(app).paused, "→ wraps to 暫停");
    press(app, KeyCode::Enter);
    assert!(!task(app).paused, "the focus moved to 繼續, the same button");
    press(app, KeyCode::Char('d'));
    let t = task(app);
    assert_eq!((t.phase, t.review_note.as_str()), (TaskPhase::Done, "使用者判定完成"));
    press(app, KeyCode::Char('x'));
    assert_eq!(task(app).phase, TaskPhase::Inactive);
    assert!(app.ui.task_ui.is_none());
}

#[test]
fn dialog_and_side_panel_buttons_work_by_mouse() {
    let mut fx = fixture();
    let app = &mut fx.app;
    let _rx = live_run(app);
    app.cur().task.start_task("goal", "");
    app.open_task();
    render(app, 120, 34);
    click(app, hit_rect(app, Hit::TaskAct(TaskAction::Pause)));
    assert!(task(app).paused);
    render(app, 120, 34);
    click(app, hit_rect(app, Hit::TaskAct(TaskAction::Resume)));
    assert!(!task(app).paused);
    render(app, 120, 34);
    click(app, hit_rect(app, Hit::TaskAct(TaskAction::Close)));
    assert!(app.ui.task_ui.is_none());

    app.ui.side_open = true;
    app.ui.side_view = SideView::Task;
    render(app, 120, 34);
    let i = app.ui.side_items.iter().position(|s| *s == SideItem::TaskAct(TaskAction::Review)).expect("side action");
    click(app, hit_rect(app, Hit::SideItem(i as u16)));
    assert!(app.cur().task.take_review_request());
    render(app, 120, 34);
    let i = app.ui.side_items.iter().position(|s| *s == SideItem::TaskAct(TaskAction::Edit)).expect("side action");
    click(app, hit_rect(app, Hit::SideItem(i as u16)));
    assert!(matches!(&app.ui.task_ui, Some(TaskUi::Form(f)) if f.editing), "修改 opens the prefilled form");
    render(app, 120, 34);
    click(app, hit_rect(app, Hit::TaskCriteria));
    type_text(app, "c");
    click(app, hit_rect(app, Hit::TaskConfirm));
    assert_eq!(task(app).criteria, "c");
}

#[test]
fn status_and_side_panel_show_what_the_proxy_said() {
    let mut fx = fixture();
    let app = &mut fx.app;
    app.cur().task.replace(TaskState {
        goal: "make the build green".into(),
        criteria: "cargo test passes\n用戶補充：輸出格式 → JSON".into(),
        phase: TaskPhase::Active,
        rounds: 3,
        last_proxy: "你說測試過了，但我沒看到 cargo test 的輸出。請貼出最後二十行。".into(),
        checklist: vec![task::CheckItem { text: "fix lint".into(), done: true }],
        review_note: "note here".into(),
        ..TaskState::default()
    });
    app.ui.side_open = true;
    app.ui.side_view = SideView::Task;
    let side = render(app, 120, 40);
    for want in ["用戶補充：輸出格式→JSON", "進行中·回合3·1/1", "用戶代理最新訊息", "你說測試過了", "✓fixlint", "▸暫停", "▸判定完成"] {
        assert!(side.contains(want), "{want} missing in {side}");
    }
    app.open_task();
    let dialog = render(app, 120, 40);
    for want in ["驗收條件／偏好", "用戶補充：輸出格式→JSON", "請貼出最後二十行。", "進度筆記1/1", "notehere", "判定完成(D)"] {
        assert!(dialog.contains(want), "{want} missing in {dialog}");
    }
}

#[test]
fn proxy_rows_render_with_their_labels_and_survive_save_and_load() {
    let mut fx = fixture_with(true);
    let app = &mut fx.app;
    let m = meta(app, "root");
    app.route_event(AgentEvent::ChildSpawned {
        meta: m.clone(),
        name: task::AGENT_NAME.into(),
        agent_card_url: String::new(),
        prompt: "goal".into(),
        model: "grok-4.6".into(),
    });
    let intents = [
        ("plan", "開場"),
        ("push", "要求"),
        ("checkpoint", "中途提醒"),
        ("answer", "代答"),
        ("accept", "驗收"),
        ("hand_back", "交還"),
        ("impossible", "無法完成"),
        ("report", "主代理回報"),
    ];
    for (intent, _) in intents {
        app.route_event(AgentEvent::TaskProxy { meta: m.clone(), intent: intent.into(), text: format!("{intent} text\nsecond line") });
    }
    assert!(!app.cur().chat.rows.iter().any(|r| matches!(r, Row::Meta(s) if s.contains(task::AGENT_NAME))), "no child noise for the proxy");
    assert!(app.cur().agents.get(task::AGENT_NAME).is_none());
    let screen = render(app, 120, 60);
    for (intent, label) in intents {
        assert!(screen.contains(&format!("用戶代理·{label}")), "{label} missing in {screen}");
        assert!(screen.contains(&format!("{intent}text")), "{intent} text missing");
    }
    assert!(screen.contains("secondline"), "the full text, every line");

    let row = Row::Proxy { intent: "push".into(), text: "t".into() };
    assert_eq!(serde_json::to_value(&row).unwrap(), json!({"Proxy": {"intent": "push", "text": "t"}}));
    let web = bridge::ui_row(&row);
    assert_eq!((web.kind.as_str(), web.label.as_deref(), web.intent.as_deref()), ("proxy", Some("要求"), Some("push")));

    let id = app.current.clone();
    app.persist(&id);
    let saved = app.store.as_ref().unwrap().load_transcript(&id).unwrap();
    let rows: Vec<Row> = serde_json::from_value(saved).unwrap();
    let back: Vec<(String, String)> = rows
        .iter()
        .filter_map(|r| match r {
            Row::Proxy { intent, text } => Some((intent.clone(), text.clone())),
            _ => None,
        })
        .collect();
    assert_eq!(back.len(), intents.len());
    assert_eq!(back[1], ("push".to_string(), "push text\nsecond line".to_string()));
}

#[test]
fn a_proxy_message_ends_the_streamed_turn() {
    let mut fx = fixture();
    let app = &mut fx.app;
    let m = meta(app, "root");
    let finished = |m: &EventMeta, text: &str| AgentEvent::ModelFinished {
        meta: m.clone(),
        text: text.into(),
        finish: "stop".into(),
        input_tokens: 0,
        cached_tokens: 0,
    };
    app.route_event(finished(&m, "done"));
    app.route_event(AgentEvent::TaskProxy { meta: m.clone(), intent: "push".into(), text: "prove it".into() });
    app.route_event(finished(&m, "done, with proof"));
    let agents: Vec<&str> = app.cur().chat.rows.iter().filter_map(|r| match r {
        Row::Agent(a) => Some(a.text.as_str()),
        _ => None,
    }).collect();
    assert_eq!(agents, ["done", "done, with proof"], "the earlier reply is kept");
}

#[test]
fn task_model_setting_syncs_to_the_knobs() {
    let mut fx = fixture();
    let app = &mut fx.app;
    assert_eq!(app.knobs.lock().unwrap().task_model, "");
    assert_eq!(app.task_model_label(), "跟主模型相同");
    app.settings.grok_catalog = crate::catalog::parse_catalog_value(&json!({
        "data": [{"id": "alpha", "name": "Alpha"}, {"id": "beta", "name": "Beta"}]
    }))
    .unwrap();
    app.rebuild_catalog();
    let choices = app.task_model_choices();
    assert_eq!(choices[0], (String::new(), "跟主模型相同".to_string()), "the main model comes first");
    assert!(choices.iter().any(|(id, _)| id == "alpha"));

    app.open_settings();
    app.settings.field = Field::TaskModel;
    assert!(render(app, 120, 50).contains("任務代理模型"));
    press(app, KeyCode::Enter);
    assert_eq!(app.settings.drop, Some(DropKind::TaskModel));
    let alpha = choices.iter().position(|(id, _)| id == "alpha").unwrap();
    for _ in 0..alpha {
        press(app, KeyCode::Down);
    }
    press(app, KeyCode::Enter);
    assert_eq!(app.knobs.lock().unwrap().task_model, "alpha");
    assert_eq!(app.task_model_label(), "Alpha");
    assert_eq!(app.knobs.lock().unwrap().child_model, "", "the child model is its own setting");

    bridge::apply_command(app, UiCommand::SetTaskModel { id: " beta ".into() });
    assert_eq!(app.knobs.lock().unwrap().task_model, "beta");
    assert_eq!(bridge::snapshot(app).settings_data.unwrap().task_model, "beta");
    app.select_task_model(String::new());
    assert_eq!(app.knobs.lock().unwrap().task_model, "", "empty follows the main model");

    // A custom endpoint without a model list takes a typed name.
    app.set_provider_kind(ProviderKind::Openai);
    app.settings.custom_catalog = Default::default();
    app.settings.field = Field::TaskModel;
    type_text(app, "local-proxy");
    app.flush_conn();
    assert_eq!(app.knobs.lock().unwrap().task_model, "local-proxy");
}

#[test]
fn bridge_commands_drive_the_task() {
    let mut fx = fixture();
    let app = &mut fx.app;
    let _rx = live_run(app);
    let sid = app.current.clone();
    bridge::apply_command(app, UiCommand::StartTask { session_id: "other".into(), goal: "g".into(), criteria: String::new() });
    assert_eq!(task(app).phase, TaskPhase::Inactive, "another session's form");
    bridge::apply_command(app, UiCommand::StartTask { session_id: sid.clone(), goal: "web goal".into(), criteria: "web criteria".into() });
    let t = task(app);
    assert_eq!((t.goal.as_str(), t.criteria.as_str(), t.phase), ("web goal", "web criteria", TaskPhase::NeedPlan));
    let ui = bridge::snapshot(app).task_summary;
    assert_eq!((ui.criteria.as_str(), ui.paused, ui.rounds, ui.live), ("web criteria", false, 0, true));
    assert_eq!(ui.actions, ["pause", "review", "accept", "edit", "end"]);

    bridge::apply_command(app, UiCommand::SetTaskPaused { paused: true });
    let ui = bridge::snapshot(app).task_summary;
    assert!(ui.paused);
    assert_eq!(ui.actions, ["resume", "accept", "edit", "end"]);
    bridge::apply_command(app, UiCommand::ReviewTask);
    assert!(!app.cur().task.take_review_request(), "paused");
    bridge::apply_command(app, UiCommand::SetTaskPaused { paused: false });
    bridge::apply_command(app, UiCommand::ReviewTask);
    assert!(app.cur().task.take_review_request());

    bridge::apply_command(app, UiCommand::EditTask { session_id: sid, goal: "web goal 2".into(), criteria: String::new() });
    let t = task(app);
    assert_eq!((t.goal.as_str(), t.criteria.as_str()), ("web goal 2", ""));

    let mut s = task(app);
    s.rounds = 3;
    s.last_proxy = "請跑測試".into();
    app.cur().task.replace(s);
    let ui = bridge::snapshot(app).task_summary;
    assert_eq!((ui.rounds, ui.last_proxy.as_str()), (3, "請跑測試"));

    bridge::apply_command(app, UiCommand::AcceptTask);
    assert_eq!(task(app).phase, TaskPhase::Done);
    assert!(!bridge::snapshot(app).task_summary.live);
    bridge::apply_command(app, UiCommand::EndTask);
    assert_eq!(task(app).phase, TaskPhase::Inactive);

    // The TUI's own form, driven from the hub.
    app.open_task();
    bridge::apply_command(app, UiCommand::SetTaskDraft { text: "from draft".into() });
    assert_eq!(bridge::snapshot(app).task.unwrap().draft, "from draft");
    bridge::apply_command(app, UiCommand::SubmitTask);
    assert_eq!(task(app).goal, "from draft");
}

#[test]
fn task_commands_parse_from_the_browser() {
    let parse = |v: serde_json::Value| serde_json::from_value::<UiCommand>(v).unwrap();
    match parse(json!({"type": "start_task", "session_id": "s", "goal": "g"})) {
        UiCommand::StartTask { criteria, .. } => assert_eq!(criteria, "", "older pages send no criteria"),
        _ => panic!("wrong variant"),
    }
    match parse(json!({"type": "edit_task", "session_id": "s", "goal": "g", "criteria": "c"})) {
        UiCommand::EditTask { goal, criteria, .. } => assert_eq!((goal.as_str(), criteria.as_str()), ("g", "c")),
        _ => panic!("wrong variant"),
    }
    assert!(matches!(parse(json!({"type": "set_task_paused", "paused": true})), UiCommand::SetTaskPaused { paused: true }));
    assert!(matches!(parse(json!({"type": "review_task"})), UiCommand::ReviewTask));
    assert!(matches!(parse(json!({"type": "accept_task"})), UiCommand::AcceptTask));
    assert!(matches!(parse(json!({"type": "end_task"})), UiCommand::EndTask));
    assert!(matches!(parse(json!({"type": "set_task_model", "id": "x"})), UiCommand::SetTaskModel { id } if id == "x"));
}

#[test]
fn composer_messages_reach_the_worker_while_the_proxy_drives() {
    let mut fx = fixture();
    let app = &mut fx.app;
    connect(app);
    let mut rx = live_run(app);
    app.cur_mut().chat.running = true;
    app.ui.send_mode = SendMode::Queue;

    // No task: a running agent queues the message as before.
    app.cur_mut().draft = Edit::at_end("later");
    app.submit_current(false);
    assert_eq!(app.cur().queue.len(), 1);
    assert!(rx.try_recv().is_err());

    app.cur().task.start_task("goal", "");
    assert!(app.cur_mut().flush_queue(), "the queued message goes in as an interjection");
    assert_eq!(rx.try_recv().unwrap().text, "later");
    app.cur_mut().draft = Edit::at_end("stop and use tabs");
    app.submit_current(false);
    assert!(app.cur().queue.is_empty());
    assert_eq!(rx.try_recv().unwrap().text, "stop and use tabs", "the user comes first");
}

#[test]
fn resuming_a_restored_task_starts_a_run_for_the_proxy() {
    let mut fx = fixture();
    let app = &mut fx.app;
    connect(app);
    app.cur().task.replace(TaskState { goal: "g".into(), phase: TaskPhase::Active, paused: true, ..TaskState::default() });
    app.task_action(TaskAction::Resume);
    assert!(!task(app).paused);
    assert!(app.cur().inbox.is_some());
}

#[test]
fn a_proxy_questionnaire_says_so_and_skipping_works() {
    let mut fx = fixture();
    let app = &mut fx.app;
    let hub = AskUserHub::new();
    app.cur_mut().ask_hub = Some(hub.clone());
    let options = vec![
        Choice { id: "pdf".into(), label: "一份 PDF".into(), input: false },
        Choice { id: "web".into(), label: "一個網頁".into(), input: false },
        Choice { id: "other".into(), label: "其他（自己填寫）".into(), input: true },
        Choice { id: "skip".into(), label: "略過，直接開始".into(), input: false },
    ];
    let ask = |app: &mut App, agent: &str| {
        let m = meta(app, agent);
        app.route_event(AgentEvent::AskUser { meta: m, question: "最後要交出什麼？".into(), allow_multiple: false, options: options.clone() });
    };

    ask(app, task::AGENT_NAME);
    let screen = render(app, 110, 34);
    assert!(screen.contains("任務代理想確認最終效果") && screen.contains("任務代理提問"), "{screen}");
    assert!(bridge::snapshot(app).ask.unwrap().from_proxy);
    let rx = hub.register();
    for _ in 0..3 {
        press(app, KeyCode::Down);
    }
    press(app, KeyCode::Enter);
    assert!(app.cur().ask.is_none());
    let mut rx = rx;
    assert!(rx.try_recv().unwrap().contains("skip"));

    // The input choice, by keyboard.
    ask(app, task::AGENT_NAME);
    let mut rx = hub.register();
    press(app, KeyCode::Down);
    press(app, KeyCode::Down);
    press(app, KeyCode::Enter);
    type_text(app, "slides");
    press(app, KeyCode::Enter);
    assert!(rx.try_recv().unwrap().contains("slides"));

    // Skipping by mouse.
    ask(app, task::AGENT_NAME);
    let mut rx = hub.register();
    render(app, 110, 34);
    click(app, hit_rect(app, Hit::AskOption(3)));
    assert!(rx.try_recv().unwrap().contains("skip"));

    // The agent's own questions keep the plain title.
    ask(app, "root");
    let screen = render(app, 110, 34);
    assert!(screen.contains("問卷") && !screen.contains("任務代理想確認最終效果"), "{screen}");
    assert!(!bridge::snapshot(app).ask.unwrap().from_proxy);
}

#[test]
fn ctrl_t_and_alt_t_toggle_the_task_dialog() {
    let mut fx = fixture();
    let app = &mut fx.app;
    key(app, KeyCode::Char('t'), KeyModifiers::ALT);
    assert!(matches!(app.ui.task_ui, Some(TaskUi::Form(_))));
    press(app, KeyCode::Esc);
    assert!(app.ui.task_ui.is_none());
    app.cur().task.start_task("g", "");
    key(app, KeyCode::Char('t'), KeyModifiers::CONTROL);
    assert!(matches!(app.ui.task_ui, Some(TaskUi::Status(_))));
    key(app, KeyCode::Char('t'), KeyModifiers::CONTROL);
    assert!(app.ui.task_ui.is_none());
}
