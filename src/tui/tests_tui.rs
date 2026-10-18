//! Image previews at the app level: Kitty pictures and overlays, thumbnails
//! in a short chat, files rewritten in place, and the viewer's mouse and
//! Ctrl+C handling.

use std::sync::{Arc, Mutex};

use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use ratatui::backend::TestBackend;
use ratatui::layout::Position;
use ratatui::style::Color;
use ratatui::Terminal;
use ratatui_image::picker::{Picker, ProtocolType};

use super::app::{App, Rt};
use super::model::rows::{Row, UserMsg};
use super::model::session::{intro_rows, ChatSel, Session};
use super::settings::Settings;
use super::{input, ui, TuiOptions};
use crate::config::ProviderConfig;
use crate::provider::ReasoningEffort;
use crate::session::SessionMeta;
use crate::skills::SkillStore;
use crate::task::TaskHub;

struct Fixture {
    app: App,
    _dir: tempfile::TempDir,
}

fn fixture(proto: Option<ProtocolType>) -> Fixture {
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
    let picker = proto.map(|p| {
        let mut picker = Picker::from_fontsize((8, 16));
        picker.set_protocol_type(p);
        picker
    });
    let mut app = App::new(opts, None, boot, settings, skills, Rt::dummy(), picker);
    app.ui.side_open = false;
    Fixture { app, _dir: dir }
}

fn save(app: &App, rel: &str, w: u32, h: u32, rgb: [u8; 3]) {
    image::RgbImage::from_pixel(w, h, image::Rgb(rgb)).save(app.workspace().join(rel)).unwrap();
}

fn show(app: &mut App, rel: &str) {
    app.cur_mut().chat.push(Row::User(UserMsg { text: "see".into(), images: vec![rel.into()] }));
}

fn draw(term: &mut Terminal<TestBackend>, app: &mut App) {
    term.draw(|f| {
        let _ = ui::draw(f, app, false);
    })
    .unwrap();
}

/// What the terminal holds at a cell, as far as ratatui wrote it.
fn shown(term: &Terminal<TestBackend>, x: u16, y: u16) -> String {
    term.backend().buffer()[(x, y)].symbol().to_string()
}

fn click(app: &mut App, col: u16, row: u16) {
    for kind in [MouseEventKind::Down(MouseButton::Left), MouseEventKind::Up(MouseButton::Left)] {
        input::handle(app, Event::Mouse(MouseEvent { kind, column: col, row, modifiers: KeyModifiers::NONE }));
    }
}

fn ctrl_c(app: &mut App) -> bool {
    input::handle(app, Event::Key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)))
}

#[test]
fn a_kitty_picture_first_drawn_under_the_viewer_still_gets_its_pixels() {
    let mut fx = fixture(Some(ProtocolType::Kitty));
    let app = &mut fx.app;
    save(app, "a.png", 80, 40, [200, 0, 0]);
    save(app, "b.png", 80, 40, [0, 0, 200]);
    show(app, "a.png");
    let mut term = Terminal::new(TestBackend::new(120, 40)).unwrap();
    draw(&mut term, app);
    app.open_image("a.png".into());
    draw(&mut term, app);
    // The agent reads another image while the viewer is open.
    show(app, "b.png");
    draw(&mut term, app);
    app.ui.image_view = None;
    draw(&mut term, app);
    assert_eq!(app.images.placements.len(), 2);
    let b = app.images.placements[1].area;
    assert!(shown(&term, b.x, b.y).contains("\x1b_G"), "b's pixels are sent once it is visible");
}

#[test]
fn a_kitty_picture_is_placed_again_after_an_overlay_closes() {
    let mut fx = fixture(Some(ProtocolType::Kitty));
    let app = &mut fx.app;
    save(app, "a.png", 320, 64, [200, 0, 0]);
    for i in 0..40 {
        app.cur_mut().chat.push(Row::meta(format!("line {i}")));
    }
    show(app, "a.png");
    let mut term = Terminal::new(TestBackend::new(120, 40)).unwrap();
    draw(&mut term, app);
    let a = app.images.placements[0].area;
    assert!(shown(&term, a.x, a.y).contains('\u{10EEEE}'));
    // The inspector covers the middle of the picture's rows, not their first cell.
    app.ui.inspector = Some("hook".into());
    draw(&mut term, app);
    let panel = app.ui.hits.iter().find(|(_, h)| *h == super::app::Hit::Overlay).unwrap().0;
    assert!(panel.x > a.x && panel.intersects(a), "the test needs a partly covered picture");
    assert_eq!(shown(&term, a.x, a.y), " ", "a covered picture is erased, not left under the overlay");
    app.ui.inspector = None;
    draw(&mut term, app);
    for y in a.y..a.bottom() {
        assert!(shown(&term, a.x, y).contains('\u{10EEEE}'), "row {y} is placed again");
    }
}

#[test]
fn a_thumbnail_fits_a_short_chat() {
    let mut fx = fixture(Some(ProtocolType::Sixel));
    let app = &mut fx.app;
    save(app, "shot.png", 1280, 720, [0, 120, 0]);
    show(app, "shot.png");
    let mut term = Terminal::new(TestBackend::new(80, 20)).unwrap();
    draw(&mut term, app);
    let inner = app.ui.chat_inner;
    assert!(inner.height < crate::preview::MAX_ROWS, "the test needs a chat shorter than a thumbnail");
    assert_eq!(app.images.blits.len(), 1, "the thumbnail shrinks to fit instead of never being painted");
    assert!(app.images.blits[0].height < inner.height);
}

#[test]
fn a_picture_followed_by_a_reply_scrolls_into_view() {
    let mut fx = fixture(Some(ProtocolType::Sixel));
    let app = &mut fx.app;
    save(app, "shot.png", 1280, 720, [0, 120, 0]);
    app.cur_mut().chat.push(Row::Picture { path: "shot.png".into(), label: "screenshot".into(), source: String::new() });
    for i in 0..6 {
        app.cur_mut().chat.push(Row::meta(format!("reply {i}")));
    }
    let mut term = Terminal::new(TestBackend::new(80, 20)).unwrap();
    draw(&mut term, app);
    assert!(app.ui.chat_inner.height < crate::preview::MAX_ROWS, "the test needs a chat shorter than a thumbnail");
    assert!(app.images.blits.is_empty(), "the reply pushed it up");
    // Scrolled up, "▼ 最新" takes the chat's last row.
    let mut shown_at = Vec::new();
    for off in 1..=app.ui.chat_max_off {
        let view = app.cur_mut().view_mut();
        view.stick_bottom = false;
        view.scroll = off;
        draw(&mut term, app);
        if !app.images.blits.is_empty() {
            shown_at.push(off);
        }
    }
    assert!(!shown_at.is_empty(), "some scroll position shows the whole picture clear of the label");
}

#[test]
fn a_rewritten_file_shows_its_new_pixels() {
    let mut fx = fixture(None);
    let app = &mut fx.app;
    // Picture rows have no CJK caption, whose hidden second cells keep stale
    // symbols in the test backend.
    let read = |app: &mut App| {
        app.cur_mut().chat.push(Row::Picture { path: "plot.png".into(), label: "read_image plot.png".into(), source: String::new() });
    };
    save(app, "plot.png", 40, 20, [255, 0, 0]);
    read(app);
    let mut term = Terminal::new(TestBackend::new(90, 32)).unwrap();
    draw(&mut term, app);
    let colors = |term: &Terminal<TestBackend>| -> Vec<Color> {
        term.backend().buffer().content.iter().filter(|c| c.symbol() == "▀").map(|c| c.fg).collect()
    };
    assert!(colors(&term).contains(&Color::Rgb(255, 0, 0)));
    // The script is fixed and the chart regenerated under the same name.
    save(app, "plot.png", 60, 20, [0, 0, 255]);
    read(app);
    draw(&mut term, app);
    let now = colors(&term);
    assert!(now.contains(&Color::Rgb(0, 0, 255)));
    assert!(!now.contains(&Color::Rgb(255, 0, 0)), "no row keeps the first decode");
}

#[test]
fn a_click_outside_the_viewer_closes_it() {
    let mut fx = fixture(None);
    let app = &mut fx.app;
    save(app, "a.png", 40, 20, [255, 0, 0]);
    show(app, "a.png");
    let mut term = Terminal::new(TestBackend::new(120, 40)).unwrap();
    app.open_image("a.png".into());
    draw(&mut term, app);
    click(app, 60, 20);
    assert!(app.ui.image_view.is_some(), "a click on the picture keeps it open");
    // The activity bar and the status bar show around the 90% panel.
    for (x, y) in [(1, 10), (118, 39)] {
        app.open_image("a.png".into());
        draw(&mut term, app);
        assert!(!app.ui.hits.iter().any(|(r, h)| *h == super::app::Hit::Overlay && r.contains(Position::new(x, y))));
        click(app, x, y);
        assert!(app.ui.image_view.is_none(), "click at {x},{y} closes the viewer");
    }
}

#[test]
fn ctrl_c_in_the_viewer_copies_the_image_and_never_quits() {
    let mut fx = fixture(None);
    let app = &mut fx.app;
    save(app, "a.png", 40, 20, [255, 0, 0]);
    show(app, "a.png");
    app.cur_mut().draft = crate::tui::edit::Edit { text: "draft".into(), caret: 5, anchor: Some(0) };
    app.open_image("a.png".into());
    assert!(!ctrl_c(app));
    // Copying the draft would say 已複製 or 無法複製到剪貼簿.
    let status = app.cur().chat.status.clone();
    assert!(matches!(status.as_str(), "已複製圖片" | "已複製路徑" | "無法複製圖片"), "the viewer copies its image: {status}");
    // A transcript trim resets the chat selection while the viewer is open.
    app.cur_mut().draft.anchor = None;
    app.cur_mut().view_mut().sel = ChatSel::None;
    assert!(!ctrl_c(app), "Ctrl+C in the viewer must not quit");
    assert!(app.ui.image_view.is_some());
}

#[test]
fn a_graphic_under_the_jump_label_says_to_scroll() {
    let mut fx = fixture(Some(ProtocolType::Sixel));
    let app = &mut fx.app;
    save(app, "a.png", 640, 160, [0, 120, 0]);
    for i in 0..40 {
        app.cur_mut().chat.push(Row::meta(format!("line {i}")));
    }
    show(app, "a.png");
    let mut term = Terminal::new(TestBackend::new(100, 40)).unwrap();
    draw(&mut term, app);
    assert_eq!(app.images.blits.len(), 1);
    // Unstuck from the bottom: "▼ 最新" now sits on the picture's last row.
    app.cur_mut().view_mut().stick_bottom = false;
    draw(&mut term, app);
    assert!(app.images.blits.is_empty(), "a covered graphic is dropped");
    let text: String = term.backend().buffer().content.iter().map(|c| c.symbol()).collect::<String>().split_whitespace().collect();
    assert!(text.contains("捲動以顯示"), "and the gap says why");
}
