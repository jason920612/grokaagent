//! Pick which window a screenshot should capture: the GUI the agent opened,
//! not the IDE or the terminal hosting grokaagent.

use std::collections::HashMap;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WinMeta {
    /// The OS window handle (0 when unknown); stable while the window lives.
    pub id: u32,
    pub title: String,
    pub app: String,
    pub pid: u32,
    pub width: u32,
    pub height: u32,
    pub minimized: bool,
}

#[derive(Default)]
pub struct ShotFilter<'a> {
    pub title: Option<&'a str>,
    pub app: Option<&'a str>,
    /// That process, or one it started (run_background reports the shell's pid).
    pub pid: Option<u32>,
    /// The exact window a name was bound to.
    pub window_id: Option<u32>,
    /// Only `window_id` may match: the name was bound to a window of a process
    /// that already had other windows (a browser), so its siblings are the user's.
    pub strict_window: bool,
    /// Title at bind time; breaks ties among windows of the bound process.
    pub bound_title: Option<&'a str>,
    /// Processes the agent started; their descendants count too.
    pub spawned: &'a [u32],
    /// Windows bound to a name by `window=`.
    pub bound_ids: &'a [u32],
    pub self_pids: &'a [u32],
    /// Processes hosting grokaagent (its terminal or IDE): skipped unless asked for.
    pub host_pids: &'a [u32],
    /// pid -> parent pid, for the descendant checks.
    pub parents: Option<&'a HashMap<u32, u32>>,
    pub workspace_hint: Option<&'a str>,
}

impl ShotFilter<'_> {
    /// The model named a window (name, pid, title or app) instead of leaving
    /// the choice to us.
    pub fn explicit(&self) -> bool {
        let given = |s: Option<&str>| s.is_some_and(|s| !s.trim().is_empty());
        given(self.title) || given(self.app) || self.pid.is_some() || self.window_id.is_some()
    }
}

pub const MIN_EDGE: u32 = 160;

fn contains_ci(hay: &str, needle: &str) -> bool {
    let n = needle.trim();
    if n.is_empty() {
        return true;
    }
    hay.to_lowercase().contains(&n.to_lowercase())
}

fn norm_app(s: &str) -> String {
    s.to_lowercase()
        .replace([' ', '_', '-'], "")
        .trim_end_matches(".exe")
        .to_string()
}

/// Editors, IDEs, terminals and OS shell surfaces, by app name as xcap reports
/// it (Windows FileDescription, macOS owner name, X11 WM_CLASS), normalized.
const HOST_APPS: &[&str] = &[
    "grokaagent",
    "cursor",
    "code",
    "codeinsiders",
    "codium",
    "vscodium",
    "visualstudiocode",
    "windsurf",
    "zed",
    "fleet",
    "windowsterminal",
    "windowsterminalpreview",
    "openconsole",
    "conhost",
    "consolewindowhost",
    "powershell",
    "pwsh",
    "cmd",
    "windowscommandprocessor",
    "windowspowershell",
    "mintty",
    "terminal",
    "iterm2",
    "gnometerminal",
    "gnometerminalserver",
    "console",
    "kgx",
    "ptyxis",
    "konsole",
    "yakuake",
    "kitty",
    "alacritty",
    "wezterm",
    "weztermgui",
    "foot",
    "ghostty",
    "warp",
    "hyper",
    "tabby",
    "xterm",
    "urxvt",
    "rxvt",
    "tilix",
    "terminator",
    "guake",
    "xfce4terminal",
    "mateterminal",
    "lxterminal",
    "qterminal",
    "terminology",
    "deepinterminal",
    "textinputhost",
    "searchhost",
    "shellexperiencehost",
    "dwm",
    "applicationframehost",
];

/// App-name prefixes for IDE families with many products.
const HOST_APP_PREFIXES: &[&str] = &[
    "jetbrains",
    "intellijidea",
    "pycharm",
    "webstorm",
    "phpstorm",
    "rubymine",
    "clion",
    "goland",
    "rustrover",
    "rider",
    "datagrip",
    "androidstudio",
    "visualstudio",
    "microsoftvisualstudio",
    "sublimetext",
];

/// Host UI the model is looking *from*, not the app it is building.
pub fn is_host_ui(app: &str, title: &str) -> bool {
    let a = norm_app(app);
    // X11 classes are often reverse-DNS: org.wezfurlong.wezterm, org.gnome.Console.
    let short = a.rsplit('.').next().unwrap_or(&a);
    let t = title.to_lowercase();
    HOST_APPS.contains(&a.as_str())
        || HOST_APPS.contains(&short)
        || HOST_APP_PREFIXES.iter().any(|p| a.starts_with(p))
        || t.ends_with("- cursor")
        || t.contains(" - cursor")
        || is_workbench_title(&t)
}

/// The web workbench grokaagent opens in the user's browser (page title
/// "grokaagent"): its address bar carries the hub token.
fn is_workbench_title(lower: &str) -> bool {
    let t = lower.trim();
    if t == "grokaagent" {
        return true;
    }
    let Some(rest) = t.strip_prefix("grokaagent") else {
        return false;
    };
    let Some(rest) = rest
        .strip_prefix(" - ")
        .or_else(|| rest.strip_prefix(" — "))
        .or_else(|| rest.strip_prefix(" – "))
    else {
        return false;
    };
    ["chrome", "chromium", "firefox", "edge", "brave", "opera", "vivaldi", "safari", "browser"]
        .iter()
        .any(|b| rest.contains(b))
}

fn looks_browser(app: &str) -> bool {
    matches!(
        norm_app(app).as_str(),
        "chrome" | "msedge" | "firefox" | "brave" | "opera" | "vivaldi" | "chromium" | "iexplore"
    )
}

fn title_looks_local(title: &str) -> bool {
    let t = title.to_lowercase();
    t.contains("localhost") || t.contains("127.0.0.1") || t.contains("[::1]")
}

fn spawned_by_agent(w: &WinMeta, f: &ShotFilter<'_>) -> bool {
    descends_from(w.pid, f.spawned, f.parents)
}

fn bound_window(w: &WinMeta, f: &ShotFilter<'_>) -> bool {
    w.id != 0 && f.bound_ids.contains(&w.id)
}

/// Evidence the window belongs to something the agent started, enough to
/// capture it unasked: its process (or an ancestor) was started by the agent,
/// it is bound by `window=`, or it shows a local dev URL.
pub fn is_agent_window(w: &WinMeta, f: &ShotFilter<'_>) -> bool {
    bound_window(w, f) || spawned_by_agent(w, f) || title_looks_local(&w.title)
}

pub fn score_window(w: &WinMeta, f: &ShotFilter<'_>) -> Option<i64> {
    if w.minimized || w.width < MIN_EDGE || w.height < MIN_EDGE {
        return None;
    }
    if f.self_pids.contains(&w.pid) {
        return None;
    }
    let exact_window = f.window_id.is_some_and(|id| id != 0 && id == w.id);
    if f.strict_window && f.window_id.is_some() && !exact_window {
        return None;
    }
    let pid_rank = match f.pid {
        Some(pid) if w.pid == pid => 2,
        Some(pid) if descends_from(w.pid, &[pid], f.parents) => 1,
        Some(_) => return None,
        None => 0,
    };
    if f.pid.is_none() && w.title.trim().is_empty() {
        return None;
    }
    let title_f = f.title.map(str::trim).filter(|s| !s.is_empty());
    let app_f = f.app.map(str::trim).filter(|s| !s.is_empty());
    let asked_for_this = exact_window
        || f.pid.is_some_and(|p| p == w.pid)
        || title_f.is_some_and(|t| contains_ci(&w.title, t))
        || app_f.is_some_and(|a| contains_ci(&w.app, a) || contains_ci(&norm_app(&w.app), &norm_app(a)));
    let host = is_host_ui(&w.app, &w.title) || f.host_pids.contains(&w.pid);
    if host && !asked_for_this {
        return None;
    }
    if let Some(t) = title_f {
        if !contains_ci(&w.title, t) {
            return None;
        }
    }
    if let Some(a) = app_f {
        if !contains_ci(&w.app, a) && !contains_ci(&norm_app(&w.app), &norm_app(a)) {
            return None;
        }
    }
    let area = (w.width as i64) * (w.height as i64);
    let mut s = area / 1000;
    if exact_window {
        s += 3_000_000;
    }
    match pid_rank {
        2 => s += 2_000_000,
        1 => s += 1_500_000,
        _ => {}
    }
    if f.bound_title.is_some_and(|t| t.trim().eq_ignore_ascii_case(w.title.trim())) {
        s += 500_000;
    }
    if bound_window(w, f) {
        s += 1_000_000;
    }
    if spawned_by_agent(w, f) {
        s += 1_000_000;
    }
    if looks_browser(&w.app) {
        s += 50_000;
    }
    if title_looks_local(&w.title) {
        s += 80_000;
    }
    if let Some(hint) = f.workspace_hint.map(str::trim).filter(|h| h.len() >= 2) {
        // An IDE or terminal titled after the workspace is not the app.
        if !host && contains_ci(&w.title, hint) {
            s += 40_000;
        }
    }
    Some(s)
}

pub fn pick_index(windows: &[WinMeta], f: &ShotFilter<'_>) -> Option<usize> {
    windows
        .iter()
        .enumerate()
        .filter_map(|(i, w)| score_window(w, f).map(|s| (s, i)))
        .max_by_key(|(s, i)| (*s, usize::MAX - *i))
        .map(|(_, i)| i)
}

pub fn ranked_indices(windows: &[WinMeta], f: &ShotFilter<'_>) -> Vec<usize> {
    let mut pairs: Vec<(i64, usize)> = windows
        .iter()
        .enumerate()
        .filter_map(|(i, w)| score_window(w, f).map(|s| (s, i)))
        .collect();
    pairs.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
    pairs.into_iter().map(|(_, i)| i).collect()
}

/// Windows a capture may take, best first. Without an explicit selector only
/// windows tied to the agent qualify: anything else may be the user's mail or
/// chat, so the caller lists them instead of sending one to the model.
pub fn capture_candidates(windows: &[WinMeta], f: &ShotFilter<'_>) -> Vec<usize> {
    let ranked = ranked_indices(windows, f);
    if f.explicit() {
        return ranked;
    }
    ranked
        .into_iter()
        .filter(|&i| is_agent_window(&windows[i], f))
        .collect()
}

pub fn collect_windows() -> crate::error::Result<Vec<(xcap::Window, WinMeta)>> {
    let raw = xcap::Window::all().map_err(|e| crate::error::Error::Tool(format!("list windows: {e}")))?;
    let mut out = Vec::new();
    for w in raw {
        out.push((
            w.clone(),
            WinMeta {
                id: w.id().unwrap_or(0),
                title: w.title().unwrap_or_default(),
                app: w.app_name().unwrap_or_default(),
                pid: w.pid().unwrap_or(0),
                width: w.width().unwrap_or(0),
                height: w.height().unwrap_or(0),
                minimized: w.is_minimized().unwrap_or(false),
            },
        ));
    }
    Ok(out)
}

pub fn capture_monitor() -> crate::error::Result<image::RgbaImage> {
    let monitors =
        xcap::Monitor::all().map_err(|e| crate::error::Error::Tool(format!("list monitors: {e}")))?;
    let monitor = monitors
        .iter()
        .find(|m| m.is_primary().unwrap_or(false))
        .or_else(|| monitors.first())
        .ok_or_else(|| crate::error::Error::Tool("no monitors to screenshot".into()))?;
    monitor
        .capture_image()
        .map_err(|e| crate::error::Error::Tool(format!("screenshot: {e}")))
}

pub const SCREEN_RECORDING_NOTE: &str = "macOS has not granted Screen Recording to the terminal running grokaagent, so a capture would show only the wallpaper and other windows cannot be listed. Nothing was captured. Ask the user to allow their terminal app in System Settings > Privacy & Security > Screen Recording, then restart the terminal and grokaagent.";

/// macOS hides other apps' windows and pixels until the terminal is granted
/// Screen Recording; captures then "succeed" with only the wallpaper.
#[cfg(target_os = "macos")]
pub fn capture_blocked() -> bool {
    type Check = extern "C" fn() -> bool;
    // CoreGraphics (linked by xcap) has these since 10.15; looked up at run
    // time so older systems, which have no such permission, report allowed.
    let lookup = |name: &[u8]| -> Option<Check> {
        let sym = unsafe { libc::dlsym(libc::RTLD_DEFAULT, name.as_ptr().cast()) };
        (!sym.is_null()).then(|| unsafe { std::mem::transmute::<*mut libc::c_void, Check>(sym) })
    };
    let Some(preflight) = lookup(b"CGPreflightScreenCaptureAccess\0") else {
        return false;
    };
    if preflight() {
        return false;
    }
    // Once per run: shows the system prompt and lists the terminal in Settings.
    static ASKED: std::sync::Once = std::sync::Once::new();
    ASKED.call_once(|| {
        if let Some(request) = lookup(b"CGRequestScreenCaptureAccess\0") {
            request();
        }
    });
    true
}

#[cfg(not(target_os = "macos"))]
pub fn capture_blocked() -> bool {
    false
}

/// pid -> parent pid for `seeds` and their ancestors (Windows: every process).
pub fn parent_map(seeds: &[u32]) -> HashMap<u32, u32> {
    #[cfg(windows)]
    {
        let _ = seeds;
        win_parents()
    }
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    {
        walk_parents(seeds, parent_of)
    }
    #[cfg(not(any(windows, target_os = "linux", target_os = "macos")))]
    {
        let _ = seeds;
        HashMap::new()
    }
}

#[cfg(any(target_os = "linux", target_os = "macos", test))]
fn walk_parents(seeds: &[u32], parent_of: impl Fn(u32) -> Option<u32>) -> HashMap<u32, u32> {
    let mut map = HashMap::new();
    for &seed in seeds {
        let mut cur = seed;
        for _ in 0..MAX_DEPTH {
            if cur == 0 || map.contains_key(&cur) {
                break;
            }
            let Some(parent) = parent_of(cur) else {
                break;
            };
            map.insert(cur, parent);
            cur = parent;
        }
    }
    map
}

const MAX_DEPTH: usize = 64;

/// Ancestors of `pid`, nearest first. Bounded: Windows parent ids can be
/// stale and form loops.
pub fn ancestors(pid: u32, parents: &HashMap<u32, u32>) -> Vec<u32> {
    let mut out = Vec::new();
    let mut cur = pid;
    while let Some(&parent) = parents.get(&cur) {
        if parent == 0 || parent == pid || out.contains(&parent) || out.len() >= MAX_DEPTH {
            break;
        }
        out.push(parent);
        cur = parent;
    }
    out
}

/// `pid` is one of `roots` or was started (transitively) by one of them.
pub fn descends_from(pid: u32, roots: &[u32], parents: Option<&HashMap<u32, u32>>) -> bool {
    if pid == 0 || roots.is_empty() {
        return false;
    }
    roots.contains(&pid)
        || parents.is_some_and(|p| ancestors(pid, p).iter().any(|a| roots.contains(a)))
}

#[cfg(target_os = "linux")]
fn parent_of(pid: u32) -> Option<u32> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    parse_stat_ppid(&stat)
}

/// `/proc/<pid>/stat` is "pid (comm) state ppid ..."; comm may hold spaces
/// and parentheses, so read past the last ')'.
#[cfg(any(target_os = "linux", test))]
fn parse_stat_ppid(stat: &str) -> Option<u32> {
    let rest = &stat[stat.rfind(')')? + 1..];
    rest.split_whitespace().nth(1)?.parse().ok()
}

#[cfg(target_os = "macos")]
fn parent_of(pid: u32) -> Option<u32> {
    let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
    let size = std::mem::size_of::<libc::proc_bsdinfo>() as libc::c_int;
    let got = unsafe {
        libc::proc_pidinfo(
            pid as libc::c_int,
            libc::PROC_PIDTBSDINFO,
            0,
            (&mut info as *mut libc::proc_bsdinfo).cast(),
            size,
        )
    };
    (got == size).then_some(info.pbi_ppid)
}

#[cfg(windows)]
fn win_parents() -> HashMap<u32, u32> {
    use windows_sys::Win32::Foundation::{CloseHandle, BOOL, HANDLE, INVALID_HANDLE_VALUE};

    // Mirrors windows-sys Win32::System::Diagnostics::ToolHelp (feature not enabled).
    const TH32CS_SNAPPROCESS: u32 = 2;
    #[repr(C)]
    struct ProcessEntry32W {
        dw_size: u32,
        cnt_usage: u32,
        th32_process_id: u32,
        th32_default_heap_id: usize,
        th32_module_id: u32,
        cnt_threads: u32,
        th32_parent_process_id: u32,
        pc_pri_class_base: i32,
        dw_flags: u32,
        sz_exe_file: [u16; 260],
    }
    #[link(name = "kernel32")]
    extern "system" {
        fn CreateToolhelp32Snapshot(dwflags: u32, th32processid: u32) -> HANDLE;
        fn Process32FirstW(hsnapshot: HANDLE, lppe: *mut ProcessEntry32W) -> BOOL;
        fn Process32NextW(hsnapshot: HANDLE, lppe: *mut ProcessEntry32W) -> BOOL;
    }

    let mut map = HashMap::new();
    unsafe {
        let snap = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0);
        if snap == INVALID_HANDLE_VALUE {
            return map;
        }
        let mut entry: ProcessEntry32W = std::mem::zeroed();
        entry.dw_size = std::mem::size_of::<ProcessEntry32W>() as u32;
        let mut ok = Process32FirstW(snap, &mut entry);
        while ok != 0 {
            if entry.th32_process_id != 0 {
                map.insert(entry.th32_process_id, entry.th32_parent_process_id);
            }
            ok = Process32NextW(snap, &mut entry);
        }
        CloseHandle(snap);
    }
    map
}

#[cfg(test)]
mod shot_tests {
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

    fn titles(windows: &[WinMeta], order: &[usize]) -> Vec<String> {
        order.iter().map(|&i| windows[i].title.clone()).collect()
    }

    /// The verifier's desktop: the user's mail, an IDE and terminals titled
    /// after the workspace, and the agent's app that was never bound.
    fn desktop() -> Vec<WinMeta> {
        vec![
            win(1, "Gmail - Google Chrome", "Google-chrome", 10, 1920, 1080),
            win(2, "main.rs - proj - Visual Studio Code", "Code", 11, 1920, 1080),
            win(3, "user@host: ~/proj", "Gnome-terminal", 12, 1280, 800),
            win(4, "proj — grokaagent — 120×40", "Terminal", 13, 1280, 800),
            win(5, "My Electron App", "electron", 300, 800, 600),
        ]
    }

    #[test]
    fn host_ui_covers_ides_terminals_and_the_workbench() {
        for (app, title) in [
            ("Code", "main.rs - proj - Visual Studio Code"),
            ("Visual Studio Code", "proj"),
            ("jetbrains-idea", "proj – Main.kt"),
            ("IntelliJ IDEA", "proj"),
            ("Terminal", "zsh"),
            ("iTerm2", "zsh"),
            ("Gnome-terminal", "user@host: ~"),
            ("org.wezfurlong.wezterm", "zsh"),
            ("kitty", "zsh"),
            ("Alacritty", "zsh"),
            ("XTerm", "zsh"),
            ("Windows PowerShell", "PS C:\\proj"),
            ("Google-chrome", "grokaagent - Google Chrome"),
            ("firefox", "grokaagent — Mozilla Firefox"),
            ("Safari", "grokaagent"),
        ] {
            assert!(is_host_ui(app, title), "{app} / {title}");
        }
        for (app, title) in [
            ("electron", "My Electron App"),
            ("Google-chrome", "Gmail - Google Chrome"),
            ("chrome", "grokaagent — localhost:3000"),
            ("python3", "Tk demo"),
        ] {
            assert!(!is_host_ui(app, title), "{app} / {title}");
        }
    }

    #[test]
    fn default_capture_needs_a_window_tied_to_the_agent() {
        let windows = desktop();
        let f = ShotFilter {
            self_pids: &[99],
            workspace_hint: Some("proj"),
            ..Default::default()
        };
        assert!(capture_candidates(&windows, &f).is_empty());
        // Ranking (what list=true shows) still has the non-host windows.
        let ranked = titles(&windows, &ranked_indices(&windows, &f));
        assert_eq!(ranked, ["Gmail - Google Chrome", "My Electron App"]);
    }

    #[test]
    fn default_capture_takes_a_descendant_of_a_background_shell() {
        let windows = desktop();
        let parents = HashMap::from([(300, 200), (200, 100), (100, 1)]);
        let f = ShotFilter {
            spawned: &[100],
            self_pids: &[99],
            parents: Some(&parents),
            workspace_hint: Some("proj"),
            ..Default::default()
        };
        let order = capture_candidates(&windows, &f);
        assert_eq!(titles(&windows, &order), ["My Electron App"]);
    }

    #[test]
    fn localhost_title_and_bound_window_count_as_the_agents() {
        let windows = vec![
            win(1, "Inbox - Google Chrome", "Google-chrome", 10, 1920, 1080),
            win(2, "Vite App - Google Chrome", "Google-chrome", 10, 1280, 800),
            win(3, "localhost:5173/ - Google Chrome", "Google-chrome", 10, 1280, 800),
        ];
        let f = ShotFilter {
            bound_ids: &[2],
            self_pids: &[99],
            ..Default::default()
        };
        let order = capture_candidates(&windows, &f);
        assert_eq!(
            titles(&windows, &order),
            ["Vite App - Google Chrome", "localhost:5173/ - Google Chrome"]
        );
    }

    #[test]
    fn host_processes_are_skipped_unless_asked_for() {
        let windows = vec![
            win(1, "some exotic terminal", "rareterm", 40, 1920, 1080),
            win(2, "Tk demo", "python3", 300, 800, 600),
        ];
        let mut f = ShotFilter {
            spawned: &[300],
            self_pids: &[99],
            host_pids: &[40],
            ..Default::default()
        };
        assert_eq!(titles(&windows, &ranked_indices(&windows, &f)), ["Tk demo"]);
        f.title = Some("exotic");
        assert_eq!(titles(&windows, &capture_candidates(&windows, &f)), ["some exotic terminal"]);
    }

    #[test]
    fn strict_binding_matches_only_the_bound_window() {
        let windows = vec![
            win(1, "grokaagent - Google Chrome", "Google-chrome", 50, 1920, 1080),
            win(2, "Inbox - Google Chrome", "Google-chrome", 50, 1920, 1080),
            win(3, "Vite App - Google Chrome", "Google-chrome", 50, 1000, 700),
        ];
        let f = ShotFilter {
            pid: Some(50),
            window_id: Some(3),
            strict_window: true,
            bound_title: Some("Vite App - Google Chrome"),
            self_pids: &[99],
            ..Default::default()
        };
        assert_eq!(titles(&windows, &capture_candidates(&windows, &f)), ["Vite App - Google Chrome"]);
        // The bound window closed: nothing else of that browser qualifies.
        assert!(capture_candidates(&windows[..2], &f).is_empty());
    }

    #[test]
    fn own_process_binding_prefers_the_bound_title_among_siblings() {
        let windows = vec![
            win(7, "Settings", "electron", 70, 1600, 1000),
            win(8, "My App", "electron", 70, 800, 600),
        ];
        let f = ShotFilter {
            pid: Some(70),
            window_id: Some(9),
            bound_title: Some("My App"),
            self_pids: &[99],
            ..Default::default()
        };
        assert_eq!(titles(&windows, &capture_candidates(&windows, &f)), ["My App", "Settings"]);
        let exact = ShotFilter { window_id: Some(7), ..f };
        assert_eq!(capture_candidates(&windows, &exact)[0], 0);
    }

    #[test]
    fn pid_filter_reaches_windows_of_child_processes() {
        let windows = vec![
            win(1, "Docs", "WINWORD", 50, 1920, 1080),
            win(2, "child app", "electron", 300, 800, 600),
            win(3, "shell's own", "conapp", 100, 400, 300),
        ];
        let parents = HashMap::from([(300, 100)]);
        let f = ShotFilter {
            pid: Some(100),
            self_pids: &[99],
            parents: Some(&parents),
            ..Default::default()
        };
        assert_eq!(titles(&windows, &capture_candidates(&windows, &f)), ["shell's own", "child app"]);
    }

    #[test]
    fn explicit_selector_without_a_match_yields_nothing() {
        let windows = desktop();
        for f in [
            ShotFilter { title: Some("localhost:5173"), self_pids: &[99], ..Default::default() },
            ShotFilter { pid: Some(9999), self_pids: &[99], ..Default::default() },
            ShotFilter { app: Some("tauri"), self_pids: &[99], ..Default::default() },
        ] {
            assert!(f.explicit());
            assert!(capture_candidates(&windows, &f).is_empty());
        }
    }

    #[test]
    fn workspace_hint_skips_host_apps() {
        let windows = vec![
            win(1, "main.rs - proj - Visual Studio Code", "Code", 11, 1920, 1080),
            win(2, "proj", "proj", 300, 800, 600),
        ];
        let f = ShotFilter {
            title: Some("proj"),
            self_pids: &[99],
            workspace_hint: Some("proj"),
            ..Default::default()
        };
        assert_eq!(pick_index(&windows, &f), Some(1));
    }

    #[test]
    fn ancestors_walk_up_and_stop_on_loops() {
        let parents = HashMap::from([(5, 4), (4, 3), (3, 1), (1, 0), (7, 8), (8, 7)]);
        assert_eq!(ancestors(5, &parents), [4, 3, 1]);
        assert_eq!(ancestors(7, &parents), [8]);
        assert!(ancestors(42, &parents).is_empty());
        assert!(descends_from(5, &[3], Some(&parents)));
        assert!(descends_from(5, &[5], None));
        assert!(!descends_from(5, &[3], None));
        assert!(!descends_from(5, &[9], Some(&parents)));
        assert!(!descends_from(0, &[0], Some(&parents)));
    }

    #[test]
    fn walk_parents_shares_chains() {
        let table = HashMap::from([(30, 20), (31, 20), (20, 10), (10, 1)]);
        let calls = std::cell::Cell::new(0);
        let map = walk_parents(&[30, 31], |pid| {
            calls.set(calls.get() + 1);
            table.get(&pid).copied()
        });
        assert_eq!(map, HashMap::from([(30, 20), (31, 20), (20, 10), (10, 1)]));
        // 30, 20, 10, 1 (no parent), then only 31.
        assert_eq!(calls.get(), 5);
    }

    #[test]
    fn stat_ppid_reads_past_the_command_name() {
        assert_eq!(parse_stat_ppid("1234 (bash) S 42 1234 1234 0 -1"), Some(42));
        assert_eq!(parse_stat_ppid("77 (my (odd) app) R 9 77 77"), Some(9));
        assert_eq!(parse_stat_ppid("garbage"), None);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn parent_map_knows_this_process() {
        let me = std::process::id();
        let map = parent_map(&[me]);
        assert_eq!(map.get(&me).copied(), Some(std::os::unix::process::parent_id()));
        assert!(!ancestors(me, &map).is_empty());
    }

    #[cfg(not(target_os = "macos"))]
    #[test]
    fn capture_is_not_blocked_off_macos() {
        assert!(!capture_blocked());
        assert!(SCREEN_RECORDING_NOTE.contains("Screen Recording"));
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

    fn filter<'a>(spawned: &'a [u32], self_pids: &'a [u32]) -> ShotFilter<'a> {
        ShotFilter {
            title: None,
            app: None,
            pid: None,
            spawned,
            self_pids,
            workspace_hint: None,
            ..Default::default()
        }
    }

    #[test]
    fn default_skips_cursor_and_picks_localhost_browser() {
        let windows = vec![
            win("grokaagent — workspace", "WindowsTerminal", 10, 1920, 1080),
            win("src/tui.rs — grokaagent - Cursor", "Cursor", 11, 1920, 1080),
            win("My App — localhost:5173", "chrome", 22, 1280, 800),
        ];
        let f = filter(&[], &[99]);
        let i = pick_index(&windows, &f).unwrap();
        assert_eq!(windows[i].app, "chrome", "{:?}", windows[i]);
        assert!(windows[i].title.contains("localhost"));
    }

    #[test]
    fn spawned_pid_beats_a_larger_unrelated_window() {
        let windows = vec![
            win("Unrelated docs", "WINWORD", 50, 1920, 1080),
            win("demo — Electron", "electron", 77, 800, 600),
        ];
        let spawned = [77u32];
        let f = filter(&spawned, &[1]);
        let i = pick_index(&windows, &f).unwrap();
        assert_eq!(windows[i].pid, 77);
    }

    #[test]
    fn title_filter_can_select_cursor_when_asked() {
        let windows = vec![
            win("My App — localhost:5173", "chrome", 22, 1280, 800),
            win("src/tui.rs — grokaagent - Cursor", "Cursor", 11, 1920, 1080),
        ];
        let f = ShotFilter {
            title: Some("Cursor"),
            app: None,
            pid: None,
            spawned: &[],
            self_pids: &[99],
            workspace_hint: None,
            ..Default::default()
        };
        let i = pick_index(&windows, &f).unwrap();
        assert_eq!(windows[i].app, "Cursor");
    }

    #[test]
    fn app_filter_matches_msedge() {
        let windows = vec![
            win("Docs", "chrome", 1, 1000, 800),
            win("Preview", "msedge", 2, 400, 400),
        ];
        let f = ShotFilter {
            title: None,
            app: Some("edge"),
            pid: None,
            spawned: &[],
            self_pids: &[9],
            workspace_hint: None,
            ..Default::default()
        };
        let i = pick_index(&windows, &f).unwrap();
        assert_eq!(windows[i].app, "msedge");
    }

    #[test]
    fn skips_self_pid_minimized_and_tiny() {
        let windows = vec![
            win("keep", "app", 1, 800, 600),
            win("self", "app", 2, 800, 600),
            win("tiny", "app", 3, 80, 80),
            {
                let mut mini = win("mini", "app", 4, 800, 600);
                mini.minimized = true;
                mini
            },
            win("", "app", 5, 800, 600),
        ];
        let f = filter(&[], &[2]);
        let i = pick_index(&windows, &f).unwrap();
        assert_eq!(windows[i].title, "keep");
    }

    #[test]
    fn workspace_hint_prefers_matching_title() {
        let windows = vec![
            win("other site", "chrome", 1, 1400, 900),
            win("grokaagent — localhost:3000", "chrome", 2, 1000, 700),
        ];
        let f = ShotFilter {
            title: None,
            app: None,
            pid: None,
            spawned: &[],
            self_pids: &[9],
            workspace_hint: Some("grokaagent"),
            ..Default::default()
        };
        let i = pick_index(&windows, &f).unwrap();
        assert!(windows[i].title.contains("grokaagent"));
    }

    #[test]
    fn ranked_puts_spawned_first() {
        let windows = vec![
            win("A", "x", 1, 1920, 1080),
            win("B", "y", 2, 800, 600),
        ];
        let spawned = [2u32];
        let order = ranked_indices(&windows, &filter(&spawned, &[9]));
        assert_eq!(order[0], 1);
    }

    #[test]
    fn pid_filter_picks_that_window_not_a_larger_one() {
        let windows = vec![
            win("Unrelated docs", "WINWORD", 50, 1920, 1080),
            win("demo — Electron", "electron", 77, 800, 600),
        ];
        let f = ShotFilter {
            title: None,
            app: None,
            pid: Some(77),
            spawned: &[],
            self_pids: &[1],
            workspace_hint: None,
            ..Default::default()
        };
        let i = pick_index(&windows, &f).unwrap();
        assert_eq!(windows[i].pid, 77);
    }
}
