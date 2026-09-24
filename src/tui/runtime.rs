//! The terminal loop: sets up the screen, starts agent runs, fetches model
//! catalogs, runs the Grok login, mirrors to the web console, and paints.

use std::io::{self, stdout};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crossterm::cursor::MoveTo;
use crossterm::event::{
    DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture, EventStream,
};
use crossterm::execute;
use crossterm::terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen};
use futures::{FutureExt, StreamExt};
use ratatui::backend::{Backend, CrosstermBackend};
use ratatui::layout::Position;
use ratatui::Terminal;
use tokio::sync::mpsc;

use crate::agent::{CancelFlag, RunOutcome, SessionKnobs, UserTurn};
use crate::ask::AskUserHub;
use crate::auth;
use crate::config::{ProviderConfig, ProviderKind};
use crate::error::{Error, Result};
use crate::events::{ChannelSink, FanoutSink, JsonlSink};
use crate::provider::{AnyProvider, XaiOauthProvider};
use crate::session::SessionStore;
use crate::skills::SkillStore;
use crate::task::TaskHub;

use super::app::{boot_session, title_event, App, Rt};
use super::model::rows::Row;
use super::settings::{run_login, Settings};
use super::theme::SIDEBAR_MIN_TERM;
use super::{bridge, input, ui, TuiOptions};

type Term = Terminal<CrosstermBackend<io::Stdout>>;

/// Save dirty transcripts at most this often while events stream in.
const PERSIST_EVERY: Duration = Duration::from_secs(2);
const FRAME: Duration = Duration::from_millis(80);

pub async fn run_tui(opts: TuiOptions) -> Result<()> {
    enable_raw_mode().map_err(Error::Io)?;
    let mut out = stdout();
    execute!(out, EnterAlternateScreen, EnableMouseCapture, EnableBracketedPaste).map_err(Error::Io)?;
    let mut terminal = Terminal::new(CrosstermBackend::new(out)).map_err(Error::Io)?;
    let result = tui_loop(&mut terminal, opts).await;
    let _ = disable_raw_mode();
    let _ = execute!(io::stdout(), DisableBracketedPaste, DisableMouseCapture, LeaveAlternateScreen);
    result
}

fn boot_app(terminal: &Term, mut opts: TuiOptions, rt: Rt) -> Result<App> {
    let auth_path = auth::default_auth_path()?;
    let store = SessionStore::open().ok();
    let (boot, created) = boot_session(store.as_ref(), opts.workspace.clone());

    let mut conn = ProviderConfig::load();
    if conn.route().is_openai() && !conn.kind.is_openai() {
        conn.kind = ProviderKind::Openai;
        let _ = conn.save();
    }
    if conn.kind.is_openai() {
        if !conn.model.trim().is_empty() {
            opts.model = conn.model.clone();
        }
        opts.web_search = false;
    }
    let mut settings = Settings::new(conn, auth_path);
    settings.model_edit = crate::tui::edit::Edit::at_end(if settings.conn.kind.is_openai() && !settings.conn.model.trim().is_empty() {
        settings.conn.model.clone()
    } else {
        opts.model.clone()
    });
    settings.child_model_edit = crate::tui::edit::Edit::at_end(opts.child_model.clone());

    let fallback = opts.workspace.clone();
    let skills = Arc::new(Mutex::new(SkillStore::open().unwrap_or_else(|_| {
        SkillStore::open_at(fallback.join(".groka").join("skills-store"), fallback)
    })));
    let picker = Some(crate::preview::detect_picker());
    let mut app = App::new(opts, store, boot, settings, skills, rt, picker);
    app.ui.side_open = terminal.size().map(|s| s.width >= SIDEBAR_MIN_TERM).unwrap_or(true);
    app.settings.catalog.ensure_current(&app.opts.model, app.opts.reasoning_effort);
    app.rebuild_catalog();
    app.sync_knobs();
    if created || app.cur().is_blank() {
        let line = if app.settings.conn.kind.is_openai() {
            format!("自訂 API  {}  ·  {}", app.settings.conn.base_url, app.opts.model)
        } else if app.settings.logged_in {
            "磁碟上有 xAI session".into()
        } else {
            "尚未登入 — 在設定中登入 Grok，或改連自訂 API".into()
        };
        app.cur_mut().chat.push(Row::meta(line));
    }
    Ok(app)
}

async fn tui_loop(terminal: &mut Term, opts: TuiOptions) -> Result<()> {
    let (ev_tx, mut ev_rx) = mpsc::unbounded_channel();
    let sink = Arc::new(FanoutSink {
        sinks: vec![
            Box::new(JsonlSink::create(&opts.events)?),
            Box::new(ChannelSink::new(ev_tx)),
        ],
    });
    let (done_tx, mut done_rx) = mpsc::unbounded_channel::<(String, RunOutcome)>();
    let mut app = boot_app(terminal, opts, Rt { sink, done_tx })?;

    let mut hub = match crate::hub::start(app.workspace()).await {
        Ok(h) => h,
        Err(e) => {
            app.cur_mut().chat.push(Row::Err(format!("網頁界面無法啟動: {e}")));
            None
        }
    };
    if let Some(h) = &hub {
        app.web.url = Some(h.url.clone());
        app.cur_mut().chat.push(Row::meta(format!("網頁界面  {}", h.url)));
    }

    let (cat_tx, mut cat_rx) = mpsc::unbounded_channel();
    let (custom_tx, mut custom_rx) = mpsc::unbounded_channel();
    let (login_tx, mut login_rx) = mpsc::unbounded_channel();
    let mut keys = EventStream::new();
    let mut paint = Painter::default();
    let mut last_persist = Instant::now();
    let mut last_size = terminal.size().map_err(Error::Io)?;

    loop {
        while let Ok(ev) = ev_rx.try_recv() {
            app.route_event(ev);
            paint.content = true;
        }
        while let Ok(result) = cat_rx.try_recv() {
            app.ingest_catalog(result);
            paint.content = true;
        }
        while let Ok(result) = custom_rx.try_recv() {
            app.ingest_custom_catalog(result);
            paint.content = true;
        }
        while let Ok(ev) = login_rx.try_recv() {
            app.apply_login_event(ev);
            paint.all();
        }
        if let Some(h) = hub.as_mut() {
            while let Ok(cmd) = h.cmd_rx.try_recv() {
                bridge::apply_command(&mut app, cmd);
                paint.all();
            }
        }
        spawn_fetches(&mut app, &cat_tx, &custom_tx, &login_tx);
        app.flush_queues();
        while let Ok((sid, out)) = done_rx.try_recv() {
            app.finish_run(&sid, out);
            paint.all();
        }
        app.kick_idle_queue();
        if app.pulsing() {
            app.tick = app.tick.wrapping_add(1);
        }
        if last_persist.elapsed() >= PERSIST_EVERY {
            last_persist = Instant::now();
            app.persist_all();
        }
        if paint.content || paint.composer || app.pulsing() {
            bridge::publish(hub.as_ref(), &mut app);
        }
        let size = terminal.size().map_err(Error::Io)?;
        if size != last_size {
            last_size = size;
            paint.all();
        }
        paint.frame(terminal, &mut app)?;

        tokio::select! {
            maybe = keys.next() => {
                let Some(Ok(first)) = maybe else { continue };
                let mut batch = vec![first];
                while let Some(Some(Ok(ev))) = keys.next().now_or_never() {
                    batch.push(ev);
                }
                let batch = input::coalesce_ime_enter(batch);
                if batch.is_empty() {
                    continue;
                }
                paint.all();
                app.web.composer_seq = app.web.composer_seq.saturating_add(1);
                let mut quit = false;
                for ev in batch {
                    if input::handle(&mut app, ev) {
                        quit = true;
                        break;
                    }
                }
                if quit {
                    break;
                }
            }
            cmd = recv_hub(&mut hub) => {
                if let Some(cmd) = cmd {
                    bridge::apply_command(&mut app, cmd);
                    paint.all();
                }
            }
            _ = tokio::time::sleep(FRAME) => {}
        }
    }
    app.persist_all();
    for s in app.sessions.values_mut() {
        s.agents.dirty = true;
    }
    app.persist_all();
    Ok(())
}

async fn recv_hub(hub: &mut Option<crate::hub::Hub>) -> Option<crate::hub::UiCommand> {
    match hub {
        Some(h) => h.cmd_rx.recv().await,
        None => std::future::pending().await,
    }
}

/// Start catalog fetches and the login flow the settings asked for.
fn spawn_fetches(
    app: &mut App,
    cat_tx: &mpsc::UnboundedSender<crate::error::Result<crate::catalog::ModelCatalog>>,
    custom_tx: &mpsc::UnboundedSender<crate::error::Result<crate::catalog::ModelCatalog>>,
    login_tx: &mpsc::UnboundedSender<super::settings::LoginEvent>,
) {
    let st = &mut app.settings;
    if st.want_catalog && st.xai_ready && st.catalog_status != super::settings::CatalogStatus::Loading {
        st.want_catalog = false;
        st.catalog_status = super::settings::CatalogStatus::Loading;
        let auth_path = st.auth_path.clone();
        let tx = cat_tx.clone();
        tokio::spawn(async move {
            let result = match XaiOauthProvider::new(auth_path, None) {
                Ok(p) => p.list_models().await,
                Err(e) => Err(e),
            };
            let _ = tx.send(result);
        });
    }
    let key = (
        st.conn.base_url.trim().to_string(),
        st.conn.api_key.trim().to_string(),
    );
    if !key.0.is_empty() && key != st.custom_key && !st.custom_loading {
        st.custom_loading = true;
        st.custom_key = key;
        let cfg = st.conn.clone();
        let tx = custom_tx.clone();
        tokio::spawn(async move {
            let result = match crate::openai::OpenAiCompatProvider::new(&cfg, Some("catalog-probe".into())) {
                Ok(p) => p.list_models().await,
                Err(e) => Err(e),
            };
            let _ = tx.send(result);
        });
    }
    if st.want_login {
        st.want_login = false;
        let tx = login_tx.clone();
        let path = st.auth_path.clone();
        let gen = st.login_gen;
        tokio::spawn(async move { run_login(path, gen, tx).await });
    }
}

/// Decides how much of the screen to repaint each tick.
#[derive(Default)]
struct Painter {
    content: bool,
    composer: bool,
    cursor_shown: bool,
    cursor_at: Option<Position>,
    started: bool,
}

impl Painter {
    fn all(&mut self) {
        self.content = true;
        self.composer = true;
    }

    fn frame(&mut self, terminal: &mut Term, app: &mut App) -> Result<()> {
        if !self.started {
            self.started = true;
            self.all();
        }
        if self.content || self.composer {
            // Keep the composer (and the IME caret inside it) untouched unless
            // the composer itself changed.
            let freeze = !self.composer;
            if freeze {
                self.hide_cursor(terminal)?;
            }
            self.full(terminal, app, freeze)?;
            self.content = false;
            self.composer = false;
        } else if ui::spinners_need_redraw(app) {
            self.hide_cursor(terminal)?;
            self.full(terminal, app, true)?;
        } else if app.cur().chat.running && !app.has_modal() {
            self.clocks(terminal, app)?;
        }
        Ok(())
    }

    fn hide_cursor(&mut self, terminal: &mut Term) -> Result<()> {
        if self.cursor_shown {
            terminal.hide_cursor().map_err(Error::Io)?;
            self.cursor_shown = false;
            self.cursor_at = None;
        }
        Ok(())
    }

    fn full(&mut self, terminal: &mut Term, app: &mut App, freeze: bool) -> Result<()> {
        terminal.autoresize().map_err(Error::Io)?;
        let caret = {
            let mut frame = terminal.get_frame();
            ui::draw(&mut frame, app, freeze)
        };
        terminal.flush().map_err(Error::Io)?;
        std::io::Write::flush(terminal.backend_mut()).map_err(Error::Io)?;
        terminal.swap_buffers();
        app.ui.last_caret = caret;
        app.ui.last_clock_cells = ui::clock_cells(app);
        flush_blits(app, caret)?;
        let want = ui::wants_cursor(app);
        if self.cursor_at != Some(caret) {
            execute!(terminal.backend_mut(), MoveTo(caret.x, caret.y)).map_err(Error::Io)?;
            self.cursor_at = Some(caret);
        }
        if want != self.cursor_shown {
            if want {
                terminal.show_cursor().map_err(Error::Io)?;
            } else {
                terminal.hide_cursor().map_err(Error::Io)?;
            }
            self.cursor_shown = want;
        }
        std::io::Write::flush(terminal.backend_mut()).map_err(Error::Io)?;
        Ok(())
    }

    /// Patch only the ticking clock cells, without moving the cursor.
    fn clocks(&mut self, terminal: &mut Term, app: &mut App) -> Result<()> {
        let next = ui::clock_cells(app);
        let changed: Vec<_> = next
            .iter()
            .filter(|(x, y, cell)| {
                !app.ui
                    .last_clock_cells
                    .iter()
                    .any(|(px, py, old)| px == x && py == y && old.symbol() == cell.symbol())
            })
            .cloned()
            .collect();
        if !changed.is_empty() {
            let chars: Vec<(u16, u16, char)> = changed
                .iter()
                .filter_map(|(x, y, c)| c.symbol().chars().next().filter(|ch| *ch != '\0').map(|ch| (*x, *y, ch)))
                .collect();
            if !crate::hostio::patch_chars_keep_cursor(&chars).map_err(Error::Io)? {
                terminal
                    .backend_mut()
                    .draw(changed.iter().map(|(x, y, c)| (*x, *y, c)))
                    .map_err(Error::Io)?;
                let caret = app.ui.last_caret;
                execute!(terminal.backend_mut(), MoveTo(caret.x, caret.y)).map_err(Error::Io)?;
                std::io::Write::flush(terminal.backend_mut()).map_err(Error::Io)?;
            }
        }
        app.ui.last_clock_cells = next;
        Ok(())
    }
}

/// Sixel / iTerm2 images are written after the frame, outside ratatui's diff.
fn flush_blits(app: &mut App, caret: Position) -> Result<()> {
    if app.images.blits == app.images.last_blits {
        return Ok(());
    }
    crate::preview::write_blits(&mut io::stdout(), &app.images.blits)?;
    if !app.images.blits.is_empty() {
        execute!(io::stdout(), MoveTo(caret.x, caret.y)).map_err(Error::Io)?;
    }
    app.images.last_blits.clone_from(&app.images.blits);
    Ok(())
}

pub(crate) fn spawn_title(sink: &Arc<FanoutSink>, session_id: &str, cfg: ProviderConfig, model: &str, prompt: &str) {
    let sink = sink.clone();
    let session_id = session_id.to_string();
    let model = model.to_string();
    let prompt = prompt.to_string();
    tokio::spawn(async move {
        let Ok(provider) = AnyProvider::connect(&cfg, Some(model)) else {
            return;
        };
        let name = provider.generate_session_title(&prompt).await;
        title_event(sink.as_ref(), &session_id, name);
    });
}

pub(crate) struct RunSpec {
    pub opts: TuiOptions,
    pub turn: UserTurn,
    pub rt: Rt,
    pub knobs: Arc<Mutex<SessionKnobs>>,
    pub skills: Arc<Mutex<SkillStore>>,
    pub inbox: mpsc::UnboundedReceiver<UserTurn>,
    pub run_id: String,
    pub ask: AskUserHub,
    pub cancel: CancelFlag,
    pub task: Arc<TaskHub>,
    pub cfg: ProviderConfig,
}

/// Run a session's root agent until its inbox closes.
pub(crate) fn spawn_run(spec: RunSpec) {
    // Tests build apps without a runtime; there is nothing to run then.
    if tokio::runtime::Handle::try_current().is_err() {
        return;
    }
    tokio::spawn(async move {
        let sid = spec.run_id.clone();
        let done_tx = spec.rt.done_tx.clone();
        let out = run_one(spec).await.unwrap_or_else(|e| RunOutcome {
            run_id: String::new(),
            text: e.to_string(),
            turns: 0,
            cache_turns: vec![],
            compacted: 0,
        });
        let _ = done_tx.send((sid, out));
    });
}

async fn run_one(spec: RunSpec) -> Result<RunOutcome> {
    let RunSpec {
        opts,
        turn,
        rt,
        knobs,
        skills,
        inbox,
        run_id,
        ask,
        cancel,
        task,
        cfg,
    } = spec;
    let model = if opts.model.trim().is_empty() {
        cfg.effective_model().to_string()
    } else {
        opts.model.clone()
    };
    let provider = AnyProvider::connect(&cfg, Some(model.clone()))?;
    let server_tools = if cfg.route_for(&model).is_openai() {
        vec![]
    } else {
        crate::kit::search_tools(opts.web_search)
    };
    let sink: Arc<dyn crate::events::EventSink> = rt.sink.clone();
    crate::kit::run_with_nursery(
        &provider,
        sink,
        crate::kit::KernelSpec {
            agent_name: "root".into(),
            prompt: turn.text,
            images: turn.images,
            context_window: cfg.window_tokens_for(&model),
            model,
            max_turns: opts.max_turns,
            workspace: opts.workspace,
            events_file: opts.events.clone(),
            events_dir: crate::kit::events_dir(&opts.events),
            server_tools,
            depth: 0,
            parent_run_id: None,
            run_id,
            child_mode: std::env::var("GROKA_CHILD_MODE").unwrap_or_else(|_| "grok".into()),
            reasoning_effort: opts.reasoning_effort,
            knobs: Some(knobs),
            inbox: Some(inbox),
            ask: Some(ask),
            cancel: Some(cancel),
            skills: Some(skills),
            task: Some(task),
        },
    )
    .await
}
