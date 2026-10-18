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
fn editing_a_handed_back_task_keeps_the_proxy_memory() {
    let mut fx = fixture();
    let app = &mut fx.app;
    let _rx = live_run(app);
    let asked = task::ProxyTurn { who: "proxy".into(), text: "Option A or B? Only the user can decide.".into() };
    app.cur().task.replace(TaskState {
        goal: "g".into(),
        phase: TaskPhase::HandedBack,
        rounds: 4,
        checklist: vec![task::CheckItem { text: "pick an option".into(), done: false }],
        proxy_log: vec![asked.clone()],
        review_note: "需要你決定 A 或 B".into(),
        ..TaskState::default()
    });
    app.open_task();
    let acts = task_actions(&task(app));
    assert_eq!(acts, [TaskAction::Edit, TaskAction::End, TaskAction::Close], "nothing to pause or accept");
    press(app, KeyCode::Char('e'));
    let screen = render(app, 110, 34);
    assert!(screen.contains("修改任務") && !screen.contains("重新開始"), "{screen}");
    press(app, KeyCode::Tab);
    type_text(app, "use option B");
    press(app, KeyCode::Enter);
    let t = task(app);
    assert_eq!((t.phase, t.criteria.as_str(), t.rounds), (TaskPhase::Active, "use option B", 4), "not planned again");
    assert_eq!(t.checklist.len(), 1, "progress notes stay");
    assert_eq!(t.proxy_log.first(), Some(&asked), "the proxy remembers what it asked");
    assert!(t.proxy_log.last().unwrap().text.contains("use option B"), "and hears the answer");
    assert!(t.review_note.is_empty(), "the hand-back note is gone");
    assert!(app.cur().task.take_review_request(), "the proxy looks again");

    // An exempted task comes back the same way.
    let mut s = task(app);
    s.phase = TaskPhase::Failed;
    s.fail_reason = "no network".into();
    app.cur().task.replace(s);
    app.edit_task("g", "network is back");
    let t = task(app);
    assert_eq!((t.phase, t.fail_reason.as_str(), t.rounds), (TaskPhase::Active, "", 4));
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

    // Ending takes a second press.
    press(app, KeyCode::Char('x'));
    assert_eq!(task(app).phase, TaskPhase::Done, "one stray key ends nothing");
    assert!(app.cur().chat.status.contains("再按一次"));
    assert!(render(app, 110, 34).contains("再按一次確認結束(X)"));
    press(app, KeyCode::Char('e'));
    press(app, KeyCode::Esc);
    press(app, KeyCode::Char('x'));
    assert_eq!(task(app).phase, TaskPhase::Done, "another action in between disarms it");
    let here = app.current.clone();
    app.current = "elsewhere".into();
    assert!(!app.task_end_armed(), "the press counts only in its own session");
    app.current = here;
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

    // 結束任務 in the side panel takes a second click, also on a narrow screen.
    app.close_task();
    let side = render(app, 80, 34);
    assert!(side.contains("▸結束任務"), "{side}");
    let i = app.ui.side_items.iter().position(|s| *s == SideItem::TaskAct(TaskAction::End)).expect("side action");
    click(app, hit_rect(app, Hit::SideItem(i as u16)));
    assert_eq!(task(app).phase, TaskPhase::NeedPlan);
    assert!(app.ui.side_open, "the panel stays for the second click");
    assert!(render(app, 80, 34).contains("▸再按一次確認結束"));
    click(app, hit_rect(app, Hit::SideItem(i as u16)));
    assert_eq!(task(app).phase, TaskPhase::Inactive);
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

    bridge::apply_command(app, UiCommand::SetTaskPaused { session_id: sid.clone(), paused: true });
    let ui = bridge::snapshot(app).task_summary;
    assert!(ui.paused);
    assert_eq!(ui.actions, ["resume", "accept", "edit", "end"]);
    bridge::apply_command(app, UiCommand::ReviewTask { session_id: sid.clone() });
    assert!(!app.cur().task.take_review_request(), "paused");
    bridge::apply_command(app, UiCommand::SetTaskPaused { session_id: String::new(), paused: false });
    assert!(!task(app).paused, "an older page sends no session id");
    bridge::apply_command(app, UiCommand::ReviewTask { session_id: sid.clone() });
    assert!(app.cur().task.take_review_request());

    // A page still showing another session acts on nothing here.
    let other = || "other".to_string();
    bridge::apply_command(app, UiCommand::SetTaskPaused { session_id: other(), paused: true });
    bridge::apply_command(app, UiCommand::ReviewTask { session_id: other() });
    bridge::apply_command(app, UiCommand::AcceptTask { session_id: other() });
    bridge::apply_command(app, UiCommand::EndTask { session_id: other() });
    let t = task(app);
    assert_eq!((t.paused, t.phase), (false, TaskPhase::NeedPlan));
    assert!(!app.cur().task.take_review_request());

    bridge::apply_command(app, UiCommand::EditTask { session_id: sid.clone(), goal: "web goal 2".into(), criteria: String::new() });
    let t = task(app);
    assert_eq!((t.goal.as_str(), t.criteria.as_str()), ("web goal 2", ""));

    let mut s = task(app);
    s.rounds = 3;
    s.last_proxy = "請跑測試".into();
    app.cur().task.replace(s);
    let ui = bridge::snapshot(app).task_summary;
    assert_eq!((ui.rounds, ui.last_proxy.as_str()), (3, "請跑測試"));

    bridge::apply_command(app, UiCommand::AcceptTask { session_id: sid.clone() });
    assert_eq!(task(app).phase, TaskPhase::Done);
    assert!(!bridge::snapshot(app).task_summary.live);
    // The page asks for the second press itself.
    bridge::apply_command(app, UiCommand::EndTask { session_id: sid });
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
    assert!(matches!(parse(json!({"type": "set_task_paused", "paused": true})), UiCommand::SetTaskPaused { paused: true, .. }));
    assert!(matches!(
        parse(json!({"type": "set_task_paused", "session_id": "s", "paused": false})),
        UiCommand::SetTaskPaused { session_id, paused: false } if session_id == "s"
    ));
    assert!(matches!(parse(json!({"type": "review_task"})), UiCommand::ReviewTask { session_id } if session_id.is_empty()));
    assert!(matches!(parse(json!({"type": "accept_task", "session_id": "s"})), UiCommand::AcceptTask { session_id } if session_id == "s"));
    assert!(matches!(parse(json!({"type": "end_task"})), UiCommand::EndTask { session_id } if session_id.is_empty()));
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
    assert!(app.cur().task.take_review_request(), "the new run starts with the proxy's look");
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
    // In the form the key keeps what was typed (Esc leaves it).
    key(app, KeyCode::Char('t'), KeyModifiers::CONTROL);
    type_text(app, "draft");
    key(app, KeyCode::Char('t'), KeyModifiers::CONTROL);
    key(app, KeyCode::Char('t'), KeyModifiers::ALT);
    assert!(matches!(&app.ui.task_ui, Some(TaskUi::Form(f)) if f.goal.text == "draft"));
    press(app, KeyCode::Esc);
    app.cur().task.start_task("g", "");
    key(app, KeyCode::Char('t'), KeyModifiers::CONTROL);
    assert!(matches!(app.ui.task_ui, Some(TaskUi::Status(_))));
    key(app, KeyCode::Char('t'), KeyModifiers::CONTROL);
    assert!(app.ui.task_ui.is_none());
    key(app, KeyCode::Char('t'), KeyModifiers::ALT);
    key(app, KeyCode::Char('x'), KeyModifiers::ALT);
    assert!(!app.task_end_armed(), "Alt+letter is no action key");
    key(app, KeyCode::Char('t'), KeyModifiers::ALT);
    assert!(app.ui.task_ui.is_none());
}

#[test]
fn an_esc_stop_shows_and_resume_or_review_lifts_it() {
    let mut fx = fixture();
    let app = &mut fx.app;
    let _rx = live_run(app);
    app.cur().task.replace(TaskState { goal: "g".into(), phase: TaskPhase::Active, skip_steer: true, ..TaskState::default() });
    app.open_task();
    let screen = render(app, 110, 34);
    assert!(screen.contains("進行中·已停止"), "{screen}");
    assert!(screen.contains("繼續(P)") && screen.contains("立即審查(R)") && !screen.contains("暫停(P)"), "{screen}");
    let ui = bridge::snapshot(app).task_summary;
    assert!(ui.stopped && !ui.paused);
    assert_eq!(ui.actions, ["resume", "review", "accept", "edit", "end"]);

    press(app, KeyCode::Char('p'));
    let t = task(app);
    assert!(!t.skip_steer && !t.paused, "繼續 lifts the Esc stop");
    assert!(app.cur().task.take_kick(), "and wakes the run");
    assert!(!bridge::snapshot(app).task_summary.stopped);
    assert!(render(app, 110, 34).contains("暫停(P)"));

    app.cur().task.set_skip_steer(true);
    press(app, KeyCode::Char('r'));
    assert!(!task(app).skip_steer, "立即審查 lifts it too");
    assert!(app.cur().task.take_review_request());

    // A finished task is not 'stopped', whatever the flag says.
    app.cur().task.set_skip_steer(true);
    app.cur().task.accept_done();
    assert!(!bridge::snapshot(app).task_summary.stopped);
}

#[test]
fn the_mouse_wheel_scrolls_the_status_dialog() {
    let mut fx = fixture();
    let app = &mut fx.app;
    let long: Vec<String> = (1..=60).map(|i| format!("line {i}")).collect();
    app.cur().task.replace(TaskState { goal: "g".into(), phase: TaskPhase::Active, last_proxy: long.join("\n"), ..TaskState::default() });
    app.open_task();
    assert!(!render(app, 110, 30).contains("line60"));
    let wheel = |app: &mut App, kind| {
        input::handle(app, Event::Mouse(MouseEvent { kind, column: 50, row: 15, modifiers: KeyModifiers::NONE }));
    };
    for _ in 0..30 {
        wheel(app, MouseEventKind::ScrollDown);
    }
    let screen = render(app, 110, 30);
    assert!(screen.contains("line60"), "{screen}");
    let scrolled = match &app.ui.task_ui {
        Some(TaskUi::Status(st)) => st.scroll,
        _ => panic!("the dialog closed"),
    };
    wheel(app, MouseEventKind::ScrollUp);
    assert!(matches!(&app.ui.task_ui, Some(TaskUi::Status(st)) if st.scroll < scrolled));
}

#[test]
fn a_row_from_a_newer_version_costs_only_itself() {
    let saved = json!([
        {"Meta": "kept before"},
        {"Future": {"text": "said by a newer kind of row", "extra": 1}},
        {"Gadget": 42},
        {"Proxy": {"intent": "push", "text": "kept proxy"}},
        {"Meta": "kept after"},
    ]);
    let rows = super::model::rows::decode_rows(&saved);
    let text: Vec<String> = rows
        .iter()
        .map(|r| match r {
            Row::Meta(s) => s.clone(),
            Row::Proxy { text, .. } => text.clone(),
            _ => panic!("unexpected row"),
        })
        .collect();
    assert_eq!(text[0], "kept before");
    assert_eq!(text[1], "said by a newer kind of row");
    assert!(text[2].contains("較新版本"), "{}", text[2]);
    assert_eq!(&text[3..], ["kept proxy", "kept after"]);
    assert!(super::model::rows::decode_rows(&json!({"not": "a list"})).is_empty());
}

#[test]
fn an_edit_while_the_worker_explains_a_failure_reopens_the_task() {
    let mut fx = fixture();
    let app = &mut fx.app;
    let _rx = live_run(app);
    app.cur().task.replace(TaskState {
        goal: "deploy".into(),
        phase: TaskPhase::ExplainingFail,
        fail_reason: "no credentials exist".into(),
        review_note: "no credentials exist".into(),
        rounds: 2,
        ..TaskState::default()
    });
    app.edit_task("deploy", "creds are now in .env");
    let t = task(app);
    assert_eq!((t.phase, t.fail_reason.as_str(), t.rounds), (TaskPhase::Active, "", 2));
    assert!(t.review_note.is_empty());
    assert!(app.cur().task.take_review_request(), "the proxy reviews the change at the stop");
}

#[test]
fn ending_or_replacing_the_task_closes_its_intake_question() {
    let mut fx = fixture();
    let app = &mut fx.app;
    let _live = live_run(app);
    let hub = AskUserHub::new();
    app.cur_mut().ask_hub = Some(hub.clone());
    let options = vec![
        Choice { id: "opt0".into(), label: "一份 PDF".into(), input: false },
        Choice { id: "skip".into(), label: "略過，直接開始".into(), input: false },
    ];
    let ask = |app: &mut App, agent: &str| {
        let m = meta(app, agent);
        app.route_event(AgentEvent::AskUser { meta: m, question: "最後要交出什麼？".into(), allow_multiple: false, options: options.clone() });
    };
    app.start_task("goal", "");
    ask(app, task::AGENT_NAME);
    let mut rx = hub.register();
    app.end_task();
    assert!(app.cur().ask.is_none(), "no question left about an ended task");
    assert!(rx.try_recv().unwrap().contains("\"cancelled\":true"));
    // Replacing the task (here from the web, once it ended) does the same.
    ask(app, task::AGENT_NAME);
    let _rx = hub.register();
    app.start_task("other goal", "");
    assert!(app.cur().ask.is_none());
    // The agent's own question is not the task's.
    ask(app, "root");
    app.end_task();
    assert!(app.cur().ask.is_some());
}

#[test]
fn a_stale_web_form_does_not_replace_a_live_task() {
    let mut fx = fixture();
    let app = &mut fx.app;
    let _rx = live_run(app);
    let sid = app.current.clone();
    app.start_task("TUI 的任務", "");
    let mut s = task(app);
    s.rounds = 1;
    s.last_proxy = "先寫測試".into();
    app.cur().task.replace(s);
    bridge::apply_command(app, UiCommand::StartTask { session_id: sid.clone(), goal: "網頁草稿的目標".into(), criteria: String::new() });
    let t = task(app);
    assert_eq!((t.goal.as_str(), t.rounds, t.last_proxy.as_str()), ("TUI 的任務", 1, "先寫測試"));
    // With no task set, the web starts one as before.
    app.end_task();
    bridge::apply_command(app, UiCommand::StartTask { session_id: sid, goal: "網頁草稿的目標".into(), criteria: String::new() });
    assert_eq!(task(app).goal, "網頁草稿的目標");
}

#[test]
fn a_task_model_of_its_own_gets_its_own_effort() {
    let mut fx = fixture();
    let app = &mut fx.app;
    app.settings.grok_catalog = crate::catalog::parse_catalog_value(&json!({
        "data": [
            {"id": "grok-4.6", "name": "Grok", "reasoning_efforts": ["low", "high"]},
            {"id": "plain", "name": "Plain"}
        ]
    }))
    .unwrap();
    app.rebuild_catalog();
    app.select_task_model("plain".into());
    let k = app.knobs.lock().unwrap().clone();
    assert_eq!(k.task_model, "plain");
    assert!(!k.task_send_reasoning, "a model with no efforts is sent none");
    let (model, _, send) = task::proxy_settings(Some(&app.knobs), "x", ReasoningEffort::High);
    assert_eq!((model.as_str(), send), ("plain", false));
}

#[test]
fn settings_fit_a_standard_terminal() {
    let mut fx = fixture();
    let app = &mut fx.app;
    app.ui.side_open = false; // it floats over the tab this narrow
    app.open_settings();
    // Grok: the child and task model boxes share a row, so the toggles
    // after them stay in view.
    let screen = render(app, 80, 24);
    assert!(screen.contains("子代理／任務代理") && screen.contains("任務代理"), "{screen}");
    assert!(screen.contains("調度員模式") && screen.contains("引入Codex技能"), "{screen}");
    app.settings.field = Field::TaskModel;
    press(app, KeyCode::Enter);
    assert_eq!(app.settings.drop, Some(DropKind::TaskModel));
    press(app, KeyCode::Esc);
    // A custom endpoint keeps its context field.
    app.set_provider_kind(ProviderKind::Openai);
    app.settings.custom_catalog = Default::default();
    let screen = render(app, 80, 24);
    assert!(screen.contains("上下文") && screen.contains("任務代理（空=同主）"), "{screen}");
    click(app, hit_rect(app, ui::settings::field_hit(Field::TaskModel)));
    assert_eq!(app.settings.field, Field::TaskModel);
    type_text(app, "local-proxy");
    app.flush_conn();
    assert_eq!(app.knobs.lock().unwrap().task_model, "local-proxy");
    // A tall screen keeps a row each, with the label in full.
    let screen = render(app, 120, 50);
    assert!(!screen.contains("子代理／任務代理") && screen.contains("任務代理（空=同主）"), "{screen}");
}
