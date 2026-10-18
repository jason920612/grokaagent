//! Web bridge: what the browser may see and the commands only the hub sends.

use std::sync::{Arc, Mutex};

use ratatui::backend::TestBackend;
use ratatui::Terminal;

use super::app::{App, Rt};
use super::bridge;
use super::edit::Edit;
use super::model::session::{intro_rows, Queued, Session};
use super::settings::Settings;
use super::TuiOptions;
use crate::config::ProviderConfig;
use crate::hub::UiCommand;
use crate::provider::ReasoningEffort;
use crate::session::SessionMeta;
use crate::skills::SkillStore;
use crate::task::TaskHub;

fn fixture() -> (App, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
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
    let mut meta = SessionMeta::new(dir.path().to_path_buf());
    meta.id = "s".into();
    let task = TaskHub::new(meta.id.clone());
    let boot = Session::new(meta, intro_rows(), task);
    let settings = Settings::new(ProviderConfig::default(), dir.path().join("no-auth.json"));
    let skills = Arc::new(Mutex::new(SkillStore::open_at(dir.path().join("skills"), dir.path().to_path_buf())));
    let app = App::new(opts, None, boot, settings, skills, Rt::dummy(), None);
    (app, dir)
}

/// Each screen row as text without whitespace (CJK cells span two).
fn screen(app: &mut App, w: u16, h: u16) -> Vec<String> {
    let mut term = Terminal::new(TestBackend::new(w, h)).unwrap();
    term.draw(|f| {
        let _ = super::ui::draw(f, app, false);
    })
    .unwrap();
    let buf = term.backend().buffer();
    (0..h)
        .map(|y| (0..w).map(|x| buf[(x, y)].symbol()).collect::<String>().split_whitespace().collect())
        .collect()
}

fn last_receipt(app: &App) -> &str {
    &app.web.receipts.last().expect("a receipt").1
}

#[test]
fn snapshots_never_carry_the_api_key_or_the_link_code() {
    let (mut app, _dir) = fixture();
    let snap = bridge::snapshot(&app);
    assert!(!snap.settings_data.as_ref().unwrap().has_api_key);

    app.settings.api_key = Edit::at_end("sk-very-secret");
    bridge::apply_command(&mut app, UiCommand::WebUrl { url: "http://127.0.0.1:4000/?code=c0ffee".into() });
    assert_eq!(app.web.url.as_deref(), Some("http://127.0.0.1:4000/?code=c0ffee"), "the status bar shows the new link");
    let snap = bridge::snapshot(&app);
    assert!(snap.settings_data.as_ref().unwrap().has_api_key);
    assert_eq!(snap.web_url, "http://127.0.0.1:4000/");
    let json = serde_json::to_string(&snap).unwrap();
    assert!(!json.contains("sk-very-secret"), "{json}");
    assert!(!json.contains("c0ffee"), "{json}");
}

#[test]
fn an_uploaded_image_attaches_to_the_session_it_was_dropped_on() {
    let (mut app, dir) = fixture();
    let snap = bridge::snapshot(&app);
    assert_eq!((snap.pending_max, snap.upload_max), (crate::vision::MAX_USER_IMAGES, crate::hub::UPLOAD_MAX));
    let attach = |request: &str, session: &str, path: &str| UiCommand::AttachUpload {
        session_id: session.into(),
        request_id: request.into(),
        workspace: dir.path().to_path_buf(),
        path: path.into(),
    };
    bridge::apply_command(&mut app, attach("u1", "s", ".groka/inbox/a.jpg"));
    assert_eq!(app.cur().pending, [".groka/inbox/a.jpg"]);
    assert_eq!(last_receipt(&app), "已附上圖片");

    // A retried request is answered once.
    app.cur_mut().pending.clear();
    bridge::apply_command(&mut app, attach("u1", "s", ".groka/inbox/a.jpg"));
    assert!(app.cur().pending.is_empty());

    bridge::apply_command(&mut app, attach("u2", "other", ".groka/inbox/b.jpg"));
    assert!(app.cur().pending.is_empty());
    assert!(last_receipt(&app).starts_with("未附上：工作已切換"));

    let elsewhere = UiCommand::AttachUpload {
        session_id: "s".into(),
        request_id: "u3".into(),
        workspace: dir.path().join("elsewhere"),
        path: ".groka/inbox/c.jpg".into(),
    };
    bridge::apply_command(&mut app, elsewhere);
    assert!(app.cur().pending.is_empty(), "saved under another workspace");

    for i in 0..crate::vision::MAX_USER_IMAGES {
        app.cur_mut().pending.push(format!("p{i}.jpg"));
    }
    bridge::apply_command(&mut app, attach("u4", "s", ".groka/inbox/d.jpg"));
    assert_eq!(app.cur().pending.len(), crate::vision::MAX_USER_IMAGES);
    assert!(last_receipt(&app).starts_with("未附上：最多"));
}

#[test]
fn a_queued_turn_with_only_images_can_be_removed() {
    let (mut app, _dir) = fixture();
    let shot = || Queued { text: String::new(), images: vec!["a.jpg".into()] };
    app.cur_mut().queue.push_back(shot());
    app.cur_mut().queue.push_back(Queued { text: String::new(), images: vec!["b.jpg".into()] });
    let snap = bridge::snapshot(&app);
    assert_eq!(snap.queue[0].paths, ["a.jpg"], "thumbnails for the queue");

    let remove = |request: &str, index: usize, images: &[&str]| UiCommand::RemoveQueued {
        session_id: "s".into(),
        request_id: request.into(),
        index,
        expected: String::new(),
        images: images.iter().map(|s| s.to_string()).collect(),
    };
    // The queue moved on: index 0 is no longer the turn the browser showed.
    bridge::apply_command(&mut app, remove("r1", 0, &["b.jpg"]));
    assert_eq!(app.cur().queue.len(), 2);
    assert!(last_receipt(&app).starts_with("未刪除"));

    bridge::apply_command(&mut app, remove("r2", 1, &["b.jpg"]));
    assert_eq!(app.cur().queue.len(), 1);
    assert_eq!(app.cur().queue[0].images, ["a.jpg"]);
    assert_eq!(last_receipt(&app), "已接收");
}

#[test]
fn the_web_link_is_shown_whole_or_not_at_all() {
    let (mut app, _dir) = fixture();
    let url = "http://127.0.0.1:41873/?code=0123456789abcdef";
    app.web.url = Some(url.into());
    app.cur_mut().chat.status = "待命".into();

    let wide = screen(&mut app, 160, 30);
    assert!(wide.last().unwrap().contains(url), "{:?}", wide.last());
    app.cur_mut().chat.running = true;
    let busy = screen(&mut app, 160, 30);
    assert!(busy.last().unwrap().contains(url), "also during a run: {:?}", busy.last());
    app.cur_mut().chat.running = false;

    // Too narrow for the link: a cut code would look whole and fail.
    let narrow = screen(&mut app, 64, 30);
    let bar = narrow.last().unwrap();
    assert!(!bar.contains("?code="), "{bar}");
    assert!(bar.contains("網頁網址見設定分頁"), "{bar}");

    app.open_settings();
    app.ui.side_open = false; // it floats over the editor this narrow
    let settings = screen(&mut app, 64, 30);
    assert!(settings.iter().any(|row| row.contains(url)), "{settings:#?}");
}

#[test]
fn a_read_image_row_carries_the_original_for_the_viewer() {
    use super::model::rows::Row;
    let row = Row::Picture { path: ".groka/seen/ab.jpg".into(), label: "模型在看  read_image  tall.png".into(), source: "tall.png".into() };
    let ui = bridge::ui_row(&row);
    assert_eq!((ui.path.as_deref(), ui.source.as_deref()), (Some(".groka/seen/ab.jpg"), Some("tall.png")));
    let shot = Row::Picture { path: "s.jpg".into(), label: "l".into(), source: String::new() };
    assert_eq!(bridge::ui_row(&shot).source, None);
    let plain = Row::Picture { path: ".groka/seen/ab.jpg".into(), label: "模型在看  read_image  tall.png".into(), source: String::new() };
    assert_ne!(bridge::row_hash(&row), bridge::row_hash(&plain));
}

#[test]
fn a_standard_24_row_terminal_still_shows_the_web_link() {
    let (mut app, _dir) = fixture();
    let url = "http://127.0.0.1:41873/?code=0123456789abcdef";
    app.web.url = Some(url.into());
    app.cur_mut().chat.status = "待命".into();
    app.opts.reasoning_effort = ReasoningEffort::Medium;
    app.open_settings();
    for openai in [false, true] {
        app.settings.conn.kind = if openai { crate::config::ProviderKind::Openai } else { crate::config::ProviderKind::Xai };
        for (w, h) in [(80, 24), (100, 24), (120, 24), (120, 26)] {
            // As the TUI starts: the side bar shows where it has room.
            app.ui.side_open = w >= crate::tui::theme::SIDEBAR_MIN_TERM;
            let rows = screen(&mut app, w, h);
            // Not counting the status bar.
            let body = &rows[..rows.len() - 1];
            assert!(body.iter().any(|row| row.contains(url)), "{w}x{h} openai={openai}: {rows:#?}");
        }
    }
}
