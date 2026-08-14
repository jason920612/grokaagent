//! Pick which window a screenshot should capture: the GUI the agent opened,
//! not the IDE or the terminal hosting grokaagent.

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WinMeta {
    pub title: String,
    pub app: String,
    pub pid: u32,
    pub width: u32,
    pub height: u32,
    pub minimized: bool,
}

pub struct ShotFilter<'a> {
    pub title: Option<&'a str>,
    pub app: Option<&'a str>,
    pub spawned: &'a [u32],
    pub self_pids: &'a [u32],
    pub workspace_hint: Option<&'a str>,
}

const MIN_EDGE: u32 = 160;

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

/// Host UI the model is looking *from*, not the app it is building.
pub fn is_host_ui(app: &str, title: &str) -> bool {
    let a = norm_app(app);
    let t = title.to_lowercase();
    matches!(
        a.as_str(),
        "grokaagent"
            | "cursor"
            | "windowsterminal"
            | "windowsterminalpreview"
            | "conhost"
            | "powershell"
            | "pwsh"
            | "cmd"
            | "textinputhost"
            | "searchhost"
            | "shellexperiencehost"
            | "dwm"
            | "applicationframehost"
    ) || t.ends_with("- cursor")
        || t.contains(" - cursor")
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

pub fn score_window(w: &WinMeta, f: &ShotFilter<'_>) -> Option<i64> {
    if w.minimized || w.width < MIN_EDGE || w.height < MIN_EDGE {
        return None;
    }
    if w.title.trim().is_empty() {
        return None;
    }
    if f.self_pids.contains(&w.pid) {
        return None;
    }
    let title_f = f.title.map(str::trim).filter(|s| !s.is_empty());
    let app_f = f.app.map(str::trim).filter(|s| !s.is_empty());
    let asked_for_this = title_f.is_some_and(|t| contains_ci(&w.title, t))
        || app_f.is_some_and(|a| contains_ci(&w.app, a) || contains_ci(&norm_app(&w.app), &norm_app(a)));
    if is_host_ui(&w.app, &w.title) && !asked_for_this {
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
    if f.spawned.contains(&w.pid) {
        s += 1_000_000;
    }
    if looks_browser(&w.app) {
        s += 50_000;
    }
    if title_looks_local(&w.title) {
        s += 80_000;
    }
    if let Some(hint) = f.workspace_hint.map(str::trim).filter(|h| h.len() >= 2) {
        if contains_ci(&w.title, hint) {
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

pub fn collect_windows() -> crate::error::Result<Vec<(xcap::Window, WinMeta)>> {
    let raw = xcap::Window::all().map_err(|e| crate::error::Error::Tool(format!("list windows: {e}")))?;
    let mut out = Vec::new();
    for w in raw {
        out.push((
            w.clone(),
            WinMeta {
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

#[cfg(test)]
mod tests {
    use super::*;

    fn win(title: &str, app: &str, pid: u32, w: u32, h: u32) -> WinMeta {
        WinMeta {
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
            spawned,
            self_pids,
            workspace_hint: None,
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
            spawned: &[],
            self_pids: &[99],
            workspace_hint: None,
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
            spawned: &[],
            self_pids: &[9],
            workspace_hint: None,
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
            spawned: &[],
            self_pids: &[9],
            workspace_hint: Some("grokaagent"),
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
}
