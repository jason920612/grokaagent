//! Named GUI windows opened by a command the model just ran.
//!
//! The model passes `window` when launching something that will open a GUI.
//! We snapshot existing windows, wait briefly for a new one, and bind the
//! chosen name to that window (and its process, when the launch started it)
//! so later `screenshot name=...` hits it.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{json, Value};
use tokio::time::{sleep, Instant};

use crate::error::{Error, Result};
use crate::shot::{self, WinMeta, MIN_EDGE};

pub const WATCH_BUDGET: Duration = Duration::from_millis(2500);
pub const WATCH_STEP: Duration = Duration::from_millis(200);

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BoundWindow {
    pub name: String,
    pub pid: u32,
    /// The OS window handle at bind time (0 when unknown).
    pub id: u32,
    pub title: String,
    pub app: String,
    pub width: u32,
    pub height: u32,
    /// The process had no window before the launch, so every window it owns
    /// is the launched app's. False for a browser that was handed a URL: only
    /// the bound window is, its siblings are the user's.
    pub own_process: bool,
    /// Bound because an existing window changed its title, not because one
    /// opened: it may be the user's own window.
    pub retitled: bool,
}

impl BoundWindow {
    pub fn from_meta(name: String, w: &WinMeta, own_process: bool) -> Self {
        Self {
            name,
            pid: w.pid,
            id: w.id,
            title: w.title.clone(),
            app: w.app.clone(),
            width: w.width,
            height: w.height,
            own_process,
            retitled: false,
        }
    }

    pub fn to_json(&self) -> Value {
        json!({
            "name": self.name,
            "pid": self.pid,
            "title": self.title,
            "app": self.app,
            "width": self.width,
            "height": self.height
        })
    }

    /// `w` is the very window bound (an id the OS reused for another app is not).
    pub fn is_window(&self, w: &WinMeta) -> bool {
        self.id != 0 && self.id == w.id && shot::same_app(&self.app, &w.app)
    }

    /// This record names `w`: the very window, or any window of a process the
    /// launch started.
    pub fn names(&self, w: &WinMeta) -> bool {
        self.is_window(w) || (self.own_process && self.pid == w.pid && shot::same_app(&self.app, &w.app))
    }
}

/// The name to show for `w`: an exact window binding first, then one of its
/// process; ties go to the shortest name ("app" before "app-2").
pub fn name_for<'a>(bound: &'a [BoundWindow], w: &WinMeta) -> Option<&'a str> {
    let shortest = |exact: bool| {
        bound
            .iter()
            .filter(|b| if exact { b.is_window(w) } else { b.names(w) })
            .min_by(|a, b| a.name.len().cmp(&b.name.len()).then_with(|| a.name.cmp(&b.name)))
            .map(|b| b.name.as_str())
    };
    shortest(true).or_else(|| shortest(false))
}

pub struct WindowHub {
    inner: Mutex<HashMap<String, BoundWindow>>,
}

impl WindowHub {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            inner: Mutex::new(HashMap::new()),
        })
    }

    /// Bind `name` to `w` and the process that owns it.
    pub fn bind(&self, name: &str, w: &WinMeta) -> BoundWindow {
        let rec = BoundWindow::from_meta(name.to_string(), w, true);
        self.insert(rec.clone());
        rec
    }

    fn insert(&self, rec: BoundWindow) {
        if let Ok(mut inner) = self.inner.lock() {
            inner.insert(rec.name.clone(), rec);
        }
    }

    pub fn get(&self, name: &str) -> Option<BoundWindow> {
        self.inner.lock().ok()?.get(name).cloned()
    }

    pub fn all(&self) -> Vec<BoundWindow> {
        let Ok(inner) = self.inner.lock() else {
            return Vec::new();
        };
        inner.values().cloned().collect()
    }

    pub fn name_for_pid(&self, pid: u32) -> Option<String> {
        let inner = self.inner.lock().ok()?;
        inner
            .values()
            .find(|w| w.pid == pid)
            .map(|w| w.name.clone())
    }

    /// Processes started by a launch that bound a name. A browser bound only
    /// through one of its windows is left out: its other windows are the user's.
    pub fn pids(&self) -> Vec<u32> {
        let Ok(inner) = self.inner.lock() else {
            return Vec::new();
        };
        inner
            .values()
            .filter(|w| w.own_process)
            .map(|w| w.pid)
            .filter(|p| *p > 0)
            .collect()
    }
}

pub fn sanitize_window_name(raw: &str) -> Result<String> {
    let raw = raw.trim();
    if raw.is_empty() {
        return Err(Error::Tool("window name is empty".into()));
    }
    if raw.len() > 40 {
        return Err(Error::Tool("window name too long".into()));
    }
    if !raw
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        return Err(Error::Tool(
            "window name must be ascii alphanumeric, dash, or underscore".into(),
        ));
    }
    Ok(raw.to_string())
}

pub fn optional_name(raw: Option<&str>) -> Result<Option<String>> {
    match raw.map(str::trim).filter(|s| !s.is_empty()) {
        Some(s) => Ok(Some(sanitize_window_name(s)?)),
        None => Ok(None),
    }
}

/// Windows on screen before a launch, to tell what the launch opened.
#[derive(Clone, Debug, Default)]
pub struct Before {
    /// False when the windows could not be listed: then nothing can be told
    /// apart from what was already open.
    taken: bool,
    pids: HashSet<u32>,
    /// Titles are kept squashed (see `squash`).
    titles: HashMap<u32, String>,
    seen: HashSet<(u32, String)>,
}

/// A title without digits, spaces and counter punctuation: "(3) Inbox" turning
/// into "(4) Inbox", or a clock ticking, is not something the launch opened.
pub(crate) fn squash(title: &str) -> String {
    title
        .chars()
        .filter(|c| !c.is_ascii_digit() && !c.is_whitespace() && !"()[]*•·●:".contains(*c))
        .flat_map(char::to_lowercase)
        .collect()
}

impl Before {
    pub fn of(windows: &[WinMeta]) -> Self {
        let mut out = Self {
            taken: true,
            ..Self::default()
        };
        for w in windows {
            let title = squash(&w.title);
            out.pids.insert(w.pid);
            if w.id != 0 {
                out.titles.insert(w.id, title.clone());
            }
            out.seen.insert((w.pid, title));
        }
        out
    }

    fn origin(&self, w: &WinMeta) -> Option<Origin> {
        // No process reported (Tk on X11 sets no _NET_WM_PID): only a window
        // id that was not there before tells what the launch opened.
        if w.pid == 0 {
            return (w.id != 0 && !self.titles.contains_key(&w.id)).then_some(Origin::NewWindow);
        }
        let title = squash(&w.title);
        if !self.pids.contains(&w.pid) {
            return Some(Origin::NewProcess);
        }
        if w.id == 0 {
            return (!self.seen.contains(&(w.pid, title))).then_some(Origin::Retitled);
        }
        match self.titles.get(&w.id) {
            None => Some(Origin::NewWindow),
            Some(old) if *old != title => Some(Origin::Retitled),
            Some(_) => None,
        }
    }
}

/// How a window relates to the launch, strongest evidence first.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
pub enum Origin {
    /// Its process had no window before: the launched app.
    NewProcess,
    /// A new window of a process that already had windows (a browser handed a URL).
    NewWindow,
    /// An existing window whose title changed: often the user's own window
    /// (a mail tab's unread count), so it only counts when nothing else appeared.
    #[default]
    Retitled,
}

/// What a launch opened: the windows of the strongest origin seen.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Appeared {
    pub origin: Origin,
    pub windows: Vec<WinMeta>,
}

impl Appeared {
    fn beats(&self, other: &Appeared) -> bool {
        !self.windows.is_empty() && (other.windows.is_empty() || self.origin <= other.origin)
    }
}

pub fn is_new_gui(w: &WinMeta, self_pids: &[u32]) -> bool {
    if w.minimized || w.width < MIN_EDGE || w.height < MIN_EDGE {
        return false;
    }
    if w.title.trim().is_empty() {
        return false;
    }
    if self_pids.contains(&w.pid) {
        return false;
    }
    !shot::is_host_ui(&w.app, &w.title)
}

/// Windows in `after` that look like a newly opened GUI relative to `before`.
/// Only the strongest origin is kept; within it, processes started under
/// grokaagent (`me`) come first, then larger windows. `host` holds the
/// processes hosting grokaagent (its terminal), which are never the app.
pub fn appeared(
    before: &Before,
    after: &[WinMeta],
    me: u32,
    host: &[u32],
    parents: &HashMap<u32, u32>,
) -> Appeared {
    let mut skip = host.to_vec();
    skip.push(me);
    let mut found: Vec<(Origin, bool, &WinMeta)> = after
        .iter()
        .filter(|w| is_new_gui(w, &skip))
        .filter_map(|w| {
            let child = shot::descends_from(w.pid, &[me], Some(parents));
            before.origin(w).map(|o| (o, child, w))
        })
        .collect();
    let Some(best) = found.iter().map(|(o, _, _)| *o).min() else {
        return Appeared::default();
    };
    found.retain(|(o, _, _)| *o == best);
    let area = |w: &WinMeta| (w.width as u64).saturating_mul(w.height as u64);
    found.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| area(b.2).cmp(&area(a.2))));
    Appeared {
        origin: best,
        windows: found.into_iter().map(|(_, _, w)| w.clone()).collect(),
    }
}

pub fn label_appeared(name: &str, found: &Appeared) -> Vec<BoundWindow> {
    found
        .windows
        .iter()
        .take(8)
        .enumerate()
        .map(|(i, w)| {
            let label = if i == 0 {
                name.to_string()
            } else {
                format!("{}-{}", name, i + 1)
            };
            // A browser the launch started (none was running) still becomes
            // the user's browser: only the bound window is the agent's.
            let own_process = found.origin == Origin::NewProcess && !shot::looks_browser(&w.app) && w.pid != 0;
            let mut rec = BoundWindow::from_meta(label, w, own_process);
            rec.retitled = found.origin == Origin::Retitled;
            rec
        })
        .collect()
}

pub fn bind_appeared(hub: &WindowHub, name: &str, found: &Appeared) -> Vec<BoundWindow> {
    label_appeared(name, found)
        .into_iter()
        .map(|rec| {
            hub.insert(rec.clone());
            rec
        })
        .collect()
}

pub fn attach_to(out: &mut Value, bound: &[BoundWindow]) {
    let Some(obj) = out.as_object_mut() else {
        return;
    };
    obj.insert(
        "windows".into(),
        json!(bound.iter().map(BoundWindow::to_json).collect::<Vec<_>>()),
    );
    if bound.is_empty() {
        obj.insert(
            "window_note".into(),
            json!("no new GUI window detected within 2.5s. Call screenshot with list=true, or pass title/app/pid."),
        );
    } else if bound.iter().any(|b| b.retitled) {
        obj.insert(
            "window_note".into(),
            json!("no new window opened; the name is bound to an existing window whose title changed (e.g. a new browser tab), which may be the user's own window. Check it with screenshot list=true before relying on it."),
        );
    }
}

pub async fn snapshot() -> Before {
    tokio::task::spawn_blocking(|| {
        shot::collect_windows()
            .map(|listed| Before::of(&listed.into_iter().map(|(_, m)| m).collect::<Vec<_>>()))
            .unwrap_or_default()
    })
    .await
    .unwrap_or_default()
}

/// One look at the screen: what appeared since `before`.
async fn poll(before: &Before, me: u32, host: &[u32]) -> Option<Appeared> {
    let before = before.clone();
    let host = host.to_vec();
    tokio::task::spawn_blocking(move || {
        let after: Vec<WinMeta> = shot::collect_windows().ok()?.into_iter().map(|(_, m)| m).collect();
        let novel: Vec<u32> = after
            .iter()
            .filter(|w| before.origin(w).is_some())
            .map(|w| w.pid)
            .collect();
        let parents = shot::parent_map(&novel);
        Some(appeared(&before, &after, me, &host, &parents))
    })
    .await
    .ok()
    .flatten()
}

pub async fn watch(before: Before) -> Appeared {
    if !before.taken {
        return Appeared::default();
    }
    let me = std::process::id();
    let host = tokio::task::spawn_blocking(move || shot::ancestors(me, &shot::parent_map(&[me])))
        .await
        .unwrap_or_default();
    let deadline = Instant::now() + WATCH_BUDGET;
    let mut best = Appeared::default();
    while Instant::now() < deadline {
        sleep(WATCH_STEP).await;
        let Some(found) = poll(&before, me, &host).await else {
            continue;
        };
        if found.beats(&best) {
            best = found;
        }
        // A retitled window alone is weak evidence: keep waiting for the
        // real window until the budget runs out.
        if best.windows.is_empty() || best.origin == Origin::Retitled {
            continue;
        }
        for _ in 0..2 {
            if Instant::now() >= deadline {
                break;
            }
            sleep(WATCH_STEP).await;
            if let Some(found) = poll(&before, me, &host).await {
                if found.beats(&best) {
                    best = found;
                }
            }
        }
        return best;
    }
    best
}

#[cfg(test)]
mod wintrack_tests {
    use super::*;

    fn win(id: u32, title: &str, app: &str, pid: u32, w: u32, h: u32) -> WinMeta {
        WinMeta {
            id,
            title: title.into(),
            app: app.into(),
            pid,
            width: w,
            height: h,
            minimized: false,
        }
    }

    fn mail() -> WinMeta {
        win(1, "(3) Inbox - you@gmail.com - Gmail - Google Chrome", "Google-chrome", 50, 1920, 1080)
    }

    fn retitled_mail() -> WinMeta {
        win(1, "(4) Inbox - you@gmail.com - Gmail - Google Chrome", "Google-chrome", 50, 1920, 1080)
    }

    fn none() -> HashMap<u32, u32> {
        HashMap::new()
    }

    #[test]
    fn a_new_process_beats_a_retitled_mail_tab() {
        let before = Before::of(&[mail()]);
        let after = vec![retitled_mail(), win(2, "My Electron App", "electron", 77, 800, 600)];
        let found = appeared(&before, &after, 5, &[], &none());
        assert_eq!(found.origin, Origin::NewProcess);
        assert_eq!(found.windows.len(), 1, "{found:?}");
        assert_eq!(found.windows[0].pid, 77);
        let bound = label_appeared("app", &found);
        assert_eq!(bound[0].name, "app");
        assert!(bound[0].own_process);
        assert_eq!(bound[0].id, 2);
    }

    #[test]
    fn a_new_browser_window_beats_a_retitled_one() {
        let before = Before::of(&[mail()]);
        let after = vec![
            retitled_mail(),
            win(3, "Vite App - Google Chrome", "Google-chrome", 50, 1280, 800),
        ];
        let found = appeared(&before, &after, 5, &[], &none());
        assert_eq!(found.origin, Origin::NewWindow);
        assert_eq!(found.windows.len(), 1);
        assert_eq!(found.windows[0].id, 3);
        let bound = label_appeared("app", &found);
        assert!(!bound[0].own_process, "a browser's other windows are the user's");
    }

    #[test]
    fn a_retitled_window_counts_only_when_nothing_else_appeared() {
        let tab = win(1, "New Tab - Google Chrome", "Google-chrome", 50, 1280, 800);
        let before = Before::of(&[tab.clone()]);
        let after = vec![win(1, "Vite App - Google Chrome", "Google-chrome", 50, 1280, 800)];
        let found = appeared(&before, &after, 5, &[], &none());
        assert_eq!(found.origin, Origin::Retitled);
        assert_eq!(found.windows[0].title, "Vite App - Google Chrome");
        assert!(appeared(&before, &[tab], 5, &[], &none()).windows.is_empty());
    }

    #[test]
    fn processes_started_under_grokaagent_come_first() {
        let before = Before::of(&[]);
        let after = vec![
            win(1, "Unrelated", "other", 80, 1600, 1000),
            win(2, "Mine", "electron", 77, 800, 600),
        ];
        let parents = HashMap::from([(77, 6), (6, 5)]);
        let found = appeared(&before, &after, 5, &[], &parents);
        let titles: Vec<_> = found.windows.iter().map(|w| w.title.as_str()).collect();
        assert_eq!(titles, ["Mine", "Unrelated"]);
    }

    #[test]
    fn the_hosting_terminal_is_never_bound() {
        let before = Before::of(&[win(1, "zsh", "rareterm", 40, 1280, 800)]);
        let after = vec![win(1, "npm start", "rareterm", 40, 1280, 800)];
        assert!(appeared(&before, &after, 5, &[40], &none()).windows.is_empty());
    }

    #[tokio::test]
    async fn watch_binds_nothing_without_a_snapshot() {
        let found = watch(Before::default()).await;
        assert!(found.windows.is_empty());
    }

    #[test]
    fn better_origins_replace_weaker_ones() {
        let weak = Appeared { origin: Origin::Retitled, windows: vec![mail()] };
        let strong = Appeared { origin: Origin::NewProcess, windows: vec![win(2, "App", "x", 7, 800, 600)] };
        assert!(strong.beats(&weak));
        assert!(!weak.beats(&strong));
        assert!(weak.beats(&Appeared::default()));
        assert!(!Appeared::default().beats(&weak));
    }

    #[test]
    fn hub_pids_leave_out_browsers_bound_by_one_window() {
        let hub = WindowHub::new();
        let browser = Appeared {
            origin: Origin::NewWindow,
            windows: vec![win(3, "Vite App", "Google-chrome", 50, 1280, 800)],
        };
        bind_appeared(&hub, "web", &browser);
        assert!(hub.pids().is_empty());
        let app = Appeared {
            origin: Origin::NewProcess,
            windows: vec![win(4, "My App", "electron", 77, 800, 600)],
        };
        bind_appeared(&hub, "app", &app);
        assert_eq!(hub.pids(), vec![77]);
        assert_eq!(hub.all().len(), 2);
    }

    #[test]
    fn names_go_to_the_bound_window_not_its_browser() {
        let bound = vec![
            BoundWindow::from_meta("web".into(), &win(3, "Vite App", "Google-chrome", 50, 1280, 800), false),
            BoundWindow::from_meta("app-2".into(), &win(5, "Tools", "electron", 77, 400, 300), true),
            BoundWindow::from_meta("app".into(), &win(4, "My App", "electron", 77, 800, 600), true),
        ];
        let other_tab = win(9, "Inbox", "Google-chrome", 50, 1920, 1080);
        assert_eq!(name_for(&bound, &other_tab), None);
        assert_eq!(name_for(&bound, &win(3, "Vite App", "Google-chrome", 50, 1280, 800)), Some("web"));
        assert_eq!(name_for(&bound, &win(5, "Tools", "electron", 77, 400, 300)), Some("app-2"));
        // A new window of the app's own process takes the shortest name.
        assert_eq!(name_for(&bound, &win(6, "Prefs", "electron", 77, 400, 300)), Some("app"));
        // The app exited and the OS gave its pid, or a window id, to another app.
        assert_eq!(name_for(&bound, &win(7, "Untitled - Notepad", "Notepad", 77, 800, 600)), None);
        assert_eq!(name_for(&bound, &win(3, "Untitled - Notepad", "Notepad", 60, 800, 600)), None);
    }

    #[test]
    fn a_ticking_counter_or_clock_binds_nothing() {
        let clock = win(2, "Clock 12:01", "gnome-clocks", 60, 400, 300);
        let before = Before::of(&[mail(), clock]);
        let after = vec![retitled_mail(), win(2, "Clock 12:02", "gnome-clocks", 60, 400, 300)];
        assert!(appeared(&before, &after, 5, &[], &none()).windows.is_empty());
        let cleared = win(1, "Inbox - you@gmail.com - Gmail - Google Chrome", "Google-chrome", 50, 1920, 1080);
        assert!(appeared(&before, &[cleared], 5, &[], &none()).windows.is_empty());
        // A real navigation still counts.
        let tab = win(1, "Vite App - Google Chrome", "Google-chrome", 50, 1920, 1080);
        assert_eq!(appeared(&before, &[tab], 5, &[], &none()).origin, Origin::Retitled);
    }

    #[test]
    fn a_retitled_binding_says_it_may_be_the_users_window() {
        let tab = win(1, "New Tab - Google Chrome", "Google-chrome", 50, 1280, 800);
        let before = Before::of(&[tab]);
        let after = vec![win(1, "Vite App - Google Chrome", "Google-chrome", 50, 1280, 800)];
        let bound = label_appeared("web", &appeared(&before, &after, 5, &[], &none()));
        assert!(bound[0].retitled && !bound[0].own_process);
        let mut out = json!({});
        attach_to(&mut out, &bound);
        assert!(out["window_note"].as_str().unwrap().contains("title changed"), "{out}");
        let fresh = label_appeared("app", &Appeared {
            origin: Origin::NewProcess,
            windows: vec![win(4, "My App", "electron", 77, 800, 600)],
        });
        let mut out = json!({});
        attach_to(&mut out, &fresh);
        assert!(out.get("window_note").is_none(), "{out}");
    }

    #[test]
    fn a_browser_the_launch_started_is_still_the_users() {
        let found = Appeared {
            origin: Origin::NewProcess,
            windows: vec![win(3, "Vite App - Google Chrome", "Google Chrome", 50, 1280, 800)],
        };
        let bound = label_appeared("web", &found);
        assert!(!bound[0].own_process);
        let hub = WindowHub::new();
        bind_appeared(&hub, "web", &found);
        assert!(hub.pids().is_empty());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn win(title: &str, app: &str, pid: u32, w: u32, h: u32) -> WinMeta {
        WinMeta {
            id: 0,
            title: title.into(),
            app: app.into(),
            pid,
            width: w,
            height: h,
            minimized: false,
        }
    }

    #[test]
    fn appeared_skips_host_and_existing() {
        let before = Before::of(&[
            win("grokaagent — workspace", "WindowsTerminal", 10, 1920, 1080),
            win("src/tui.rs — grokaagent - Cursor", "Cursor", 11, 1920, 1080),
        ]);
        let after = vec![
            win("grokaagent — workspace", "WindowsTerminal", 10, 1920, 1080),
            win("src/tui.rs — grokaagent - Cursor", "Cursor", 11, 1920, 1080),
            win("My App", "electron", 77, 800, 600),
            win("tiny", "x", 88, 20, 20),
            win("", "electron", 99, 800, 600),
        ];
        let found = appeared(&before, &after, 1, &[], &HashMap::new()).windows;
        assert_eq!(found.len(), 1, "{found:?}");
        assert_eq!(found[0].pid, 77);
    }

    #[test]
    fn appeared_treats_title_fill_as_new() {
        let before = Before::of(&[win("", "electron", 77, 800, 600)]);
        let after = vec![win("Ready", "electron", 77, 800, 600)];
        let found = appeared(&before, &after, 999, &[], &HashMap::new()).windows;
        assert_eq!(found[0].title, "Ready");
    }

    #[test]
    fn appeared_ranks_larger_first() {
        let before = Before::of(&[]);
        let after = vec![
            win("small", "app", 1, 400, 300),
            win("big", "app", 2, 1200, 800),
        ];
        let found = appeared(&before, &after, 999, &[], &HashMap::new()).windows;
        assert_eq!(found[0].title, "big");
        assert_eq!(found[1].title, "small");
    }

    #[test]
    fn hub_bind_overwrites_and_lookup() {
        let hub = WindowHub::new();
        hub.bind("preview", &win("A", "x", 10, 800, 600));
        hub.bind("preview", &win("B", "y", 20, 800, 600));
        let got = hub.get("preview").unwrap();
        assert_eq!(got.pid, 20);
        assert_eq!(got.title, "B");
        assert_eq!(hub.name_for_pid(20).as_deref(), Some("preview"));
        assert_eq!(hub.pids(), vec![20]);
    }

    #[test]
    fn bind_appeared_names_extras() {
        let hub = WindowHub::new();
        let found = Appeared {
            origin: Origin::NewProcess,
            windows: vec![
                win("one", "a", 1, 800, 600),
                win("two", "b", 2, 800, 600),
            ],
        };
        let bound = bind_appeared(&hub, "ui", &found);
        assert_eq!(bound[0].name, "ui");
        assert_eq!(bound[1].name, "ui-2");
        assert_eq!(hub.get("ui-2").unwrap().pid, 2);
    }

    #[test]
    fn sanitize_window_name_rules() {
        assert!(sanitize_window_name("preview").is_ok());
        assert!(sanitize_window_name("a_b-1").is_ok());
        assert!(sanitize_window_name("").is_err());
        assert!(sanitize_window_name("has space").is_err());
        assert!(sanitize_window_name(&"x".repeat(41)).is_err());
    }

    #[test]
    fn attach_to_empty_sets_note() {
        let mut v = json!({"pid": 1});
        attach_to(&mut v, &[]);
        assert_eq!(v["windows"].as_array().unwrap().len(), 0);
        assert!(v["window_note"].as_str().unwrap().contains("no new GUI"));
    }
}
