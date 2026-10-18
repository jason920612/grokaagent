//! Image input flows: drops typed in by a Windows console, paste results,
//! attachment chips, and attachments that follow a blank chat's folder.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::backend::TestBackend;
use ratatui::Terminal;

use super::app::{App, Hit, Rt};
use super::model::session::{intro_rows, Session};
use super::settings::Settings;
use super::{bridge, input, ui, TuiOptions};
use crate::config::ProviderConfig;
use crate::hub::UiCommand;
use crate::provider::ReasoningEffort;
use crate::session::{SessionMeta, SessionStore};
use crate::skills::SkillStore;
use crate::task::TaskHub;

struct Fixture {
    app: App,
    dir: tempfile::TempDir,
}

fn fixture() -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let store = SessionStore::open_at(dir.path().join("sessions")).unwrap();
    let ws = dir.path().join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    let meta: SessionMeta = store.create(ws.clone()).unwrap();
    let task = TaskHub::new(meta.id.clone());
    let boot = Session::new(meta, intro_rows(), task);
    let settings = Settings::new(ProviderConfig::default(), dir.path().join("no-auth.json"));
    let skills = Arc::new(Mutex::new(SkillStore::open_at(dir.path().join("skills"), ws.clone())));
    let opts = TuiOptions {
        model: "grok-4.6".into(),
        events: dir.path().join("events.jsonl"),
        workspace: ws,
        max_turns: 0,
        web_search: false,
        dispatcher: false,
        child_model: String::new(),
        reasoning_effort: ReasoningEffort::High,
    };
    let app = App::new(opts, Some(store), boot, settings, skills, Rt::dummy(), None);
    Fixture { app, dir }
}

/// An image outside the workspace, where drops come from.
fn picture(fx: &Fixture, name: &str) -> PathBuf {
    let dir = fx.dir.path().join("Pictures");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join(name);
    image::RgbImage::from_pixel(40, 40, image::Rgb([255, 0, 0])).save(&path).unwrap();
    path
}

fn inbox_files(ws: &Path) -> usize {
    std::fs::read_dir(ws.join(".groka/inbox")).map(|d| d.count()).unwrap_or(0)
}

/// Keys as a Windows console types pasted text: press and release each.
fn console_keys(s: &str) -> Vec<Event> {
    let mut out = Vec::new();
    for c in s.chars() {
        let code = if c == '\r' { KeyCode::Enter } else { KeyCode::Char(c) };
        let mods = if c.is_ascii_uppercase() { KeyModifiers::SHIFT } else { KeyModifiers::NONE };
        out.push(Event::Key(KeyEvent::new(code, mods)));
        out.push(Event::Key(KeyEvent::new_with_kind(code, mods, KeyEventKind::Release)));
    }
    out
}

/// One read of the terminal loop on Windows.
fn console_read(app: &mut App, burst: &mut input::PasteBurst, keys: Vec<Event>) {
    let batch = input::coalesce_ime_enter(burst.coalesce(keys, Instant::now()));
    for ev in batch {
        input::handle(app, ev);
    }
}

#[test]
fn a_drop_typed_in_by_a_windows_console_attaches() {
    let mut fx = fixture();
    let shot = picture(&fx, "Screenshot 2026-10-17 101010.png");
    let app = &mut fx.app;
    let mut burst = input::PasteBurst::default();
    console_read(app, &mut burst, console_keys(&format!("\"{}\"", shot.display())));
    assert_eq!(app.cur().pending.len(), 1, "{}", app.cur().chat.status);
    assert!(app.cur().draft.is_empty());

    console_read(app, &mut burst, console_keys("first line\rsecond line"));
    assert_eq!(app.cur().draft.text, "first line\nsecond line");
    console_read(app, &mut input::PasteBurst::default(), console_keys("\rthird line"));
    assert_eq!(app.cur().draft.text, "first line\nsecond line\nthird line", "a leading line break does not send");
    assert!(app.cur().queue.is_empty() && !app.cur().chat.rows.iter().any(|r| matches!(r, super::model::rows::Row::User(_))));
}

#[test]
fn drops_past_the_cap_are_counted_and_never_saved() {
    let mut fx = fixture();
    let shots: Vec<PathBuf> = (0..3).map(|i| picture(&fx, &format!("s{i}.png"))).collect();
    let app = &mut fx.app;
    app.cur_mut().pending = vec!["a.jpg".into(), "b.jpg".into(), "c.jpg".into()];
    let list = shots.iter().map(|p| p.display().to_string()).collect::<Vec<_>>().join("\n");
    app.paste_text(&list);
    assert_eq!(app.cur().pending.len(), 4);
    assert_eq!(app.cur().chat.status, "已附上 1 張圖片 · 最多 4 張圖片，2 張未附上");
    assert_eq!(inbox_files(&app.workspace()), 1, "nothing past the cap is decoded or saved");
    assert!(app.cur().draft.is_empty());
}

#[test]
fn unreadable_drops_say_why() {
    let mut fx = fixture();
    let shot = picture(&fx, "shot.png");
    let dir = fx.dir.path().join("Pictures");
    let heic = dir.join("IMG_0001.heic");
    let broken = dir.join("broken.png");
    let svg = dir.join("logo.svg");
    std::fs::write(&heic, "x").unwrap();
    std::fs::write(&broken, "not a png").unwrap();
    std::fs::write(&svg, "<svg/>").unwrap();
    let app = &mut fx.app;
    app.paste_text(&broken.display().to_string());
    assert!(app.cur().chat.status.starts_with("無法加入圖片"), "{}", app.cur().chat.status);
    assert!(app.cur().draft.is_empty(), "a failed attachment does not paste its path");

    app.paste_text(&format!("{}\n{}", shot.display(), heic.display()));
    assert_eq!(
        app.cur().chat.status,
        "已附上 1 張圖片 · 不支援的圖片格式：IMG_0001.heic（可用 PNG、JPEG、GIF、WebP、BMP）"
    );
    assert!(app.cur().draft.is_empty());

    app.paste_text(&heic.display().to_string());
    assert!(app.cur().chat.status.starts_with("不支援的圖片格式：IMG_0001.heic"), "{}", app.cur().chat.status);
    assert_eq!(app.cur().draft.text, heic.display().to_string(), "a path it cannot attach may be meant as text");

    app.cur_mut().draft = super::edit::Edit::default();
    app.flash("");
    app.paste_text(&svg.display().to_string());
    assert_eq!(app.cur().draft.text, svg.display().to_string(), "an SVG path is source text");
    assert_eq!(app.cur().chat.status, "");
    assert_eq!(app.cur().pending.len(), 1);
}

#[test]
fn a_blank_chat_moving_to_another_folder_takes_its_attachments() {
    let mut fx = fixture();
    let shot = picture(&fx, "shot.png");
    let target = fx.dir.path().join("proj");
    std::fs::create_dir_all(&target).unwrap();
    let app = &mut fx.app;
    let old = app.workspace();
    app.paste_text(&shot.display().to_string());
    app.cur_mut().draft = super::edit::Edit::at_end("what is this?");
    let rel = app.cur().pending[0].clone();
    let id = app.current.clone();
    app.create_chat(target.clone());
    assert_eq!(app.current, id, "still the same blank chat");
    let ws = app.workspace();
    assert_ne!(ws, old);
    assert_eq!(app.cur().pending, [rel.clone()]);
    assert!(ws.join(&rel).is_file(), "the attachment moved along");
    assert!(!old.join(&rel).exists());
    assert_eq!(app.cur().draft.text, "what is this?");
    assert_eq!(std::fs::read_to_string(ws.join(".groka/.gitignore")).unwrap().trim(), "*");
}

#[cfg(unix)]
#[test]
fn attachments_never_move_through_a_symlinked_groka() {
    let mut fx = fixture();
    let shot = picture(&fx, "shot.png");
    let target = fx.dir.path().join("cloned");
    let outside = fx.dir.path().join("outside");
    std::fs::create_dir_all(&target).unwrap();
    std::fs::create_dir_all(&outside).unwrap();
    std::os::unix::fs::symlink(&outside, target.join(".groka")).unwrap();
    let app = &mut fx.app;
    let old = app.workspace();
    app.paste_text(&shot.display().to_string());
    let rel = app.cur().pending[0].clone();
    app.create_chat(target.clone());
    assert!(app.cur().pending.is_empty());
    assert!(std::fs::read_dir(&outside).unwrap().next().is_none(), "nothing written through the link");
    assert!(old.join(&rel).is_file(), "left in the old inbox");
    assert!(app.cur().chat.status.contains("1 張附件無法搬移"), "{}", app.cur().chat.status);
}

#[test]
fn four_attachments_all_get_a_chip_on_an_80_column_terminal() {
    let mut fx = fixture();
    let app = &mut fx.app;
    app.ui.side_open = false;
    app.cur_mut().pending = (0..4).map(|i| format!(".groka/inbox/20261017T101010-0000000{i}.jpg")).collect();
    let mut term = Terminal::new(TestBackend::new(80, 30)).unwrap();
    term.draw(|f| {
        let _ = ui::draw(f, app, false);
    })
    .unwrap();
    let chips = app.ui.hits.iter().filter(|(_, h)| matches!(h, Hit::PendingClose(_))).count();
    assert_eq!(chips, 4);
    let mut narrow = Terminal::new(TestBackend::new(24, 30)).unwrap();
    narrow
        .draw(|f| {
            let _ = ui::draw(f, app, false);
        })
        .unwrap();
    let text: String = narrow.backend().buffer().content.iter().map(|c| c.symbol()).collect();
    let shown = app.ui.hits.iter().filter(|(_, h)| matches!(h, Hit::PendingClose(_))).count();
    assert!(shown < 4);
    assert!(text.contains(&format!("+{}", 4 - shown)), "the rest is counted");
}

#[test]
fn web_paste_text_is_only_text() {
    let mut fx = fixture();
    let shot = picture(&fx, "private.png");
    let app = &mut fx.app;
    bridge::apply_command(app, UiCommand::PasteText { text: shot.display().to_string() });
    assert!(app.cur().pending.is_empty(), "a browser cannot pull host files into the inbox");
    assert_eq!(app.cur().draft.text, shot.display().to_string());
    assert_eq!(inbox_files(&app.workspace()), 0);
}
