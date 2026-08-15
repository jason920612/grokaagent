//! Named GUI windows opened by a command the model just ran.
//!
//! The model passes `window` when launching something that will open a GUI.
//! We snapshot existing windows, wait briefly for a new one, and bind the
//! chosen name to that window's PID so later `screenshot name=...` hits it.

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
    pub title: String,
    pub app: String,
    pub width: u32,
    pub height: u32,
}

impl BoundWindow {
    pub fn from_meta(name: String, w: &WinMeta) -> Self {
        Self {
            name,
            pid: w.pid,
            title: w.title.clone(),
            app: w.app.clone(),
            width: w.width,
            height: w.height,
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

    pub fn bind(&self, name: &str, w: &WinMeta) -> BoundWindow {
        let rec = BoundWindow::from_meta(name.to_string(), w);
        if let Ok(mut inner) = self.inner.lock() {
            inner.insert(name.to_string(), rec.clone());
        }
        rec
    }

    pub fn get(&self, name: &str) -> Option<BoundWindow> {
        self.inner.lock().ok()?.get(name).cloned()
    }

    pub fn name_for_pid(&self, pid: u32) -> Option<String> {
        let inner = self.inner.lock().ok()?;
        inner
            .values()
            .find(|w| w.pid == pid)
            .map(|w| w.name.clone())
    }

    pub fn pids(&self) -> Vec<u32> {
        let Ok(inner) = self.inner.lock() else {
            return Vec::new();
        };
        inner.values().map(|w| w.pid).filter(|p| *p > 0).collect()
    }

    pub fn pid_names(&self) -> HashMap<u32, String> {
        let Ok(inner) = self.inner.lock() else {
            return HashMap::new();
        };
        let mut map = HashMap::new();
        for (name, w) in inner.iter() {
            map.entry(w.pid).or_insert_with(|| name.clone());
        }
        map
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

pub fn fingerprints(windows: &[WinMeta]) -> HashSet<(u32, String)> {
    windows
        .iter()
        .map(|w| (w.pid, w.title.to_lowercase()))
        .collect()
}

pub fn is_new_gui(w: &WinMeta, self_pids: &[u32]) -> bool {
    if w.minimized || w.width < MIN_EDGE || w.height < MIN_EDGE {
        return false;
    }
    if w.title.trim().is_empty() {
        return false;
    }
    if w.pid == 0 || self_pids.contains(&w.pid) {
        return false;
    }
    !shot::is_host_ui(&w.app, &w.title)
}

/// Windows in `after` that look like a newly opened GUI relative to `before`.
pub fn appeared(
    before: &HashSet<(u32, String)>,
    after: &[WinMeta],
    self_pids: &[u32],
) -> Vec<WinMeta> {
    let mut out: Vec<WinMeta> = after
        .iter()
        .filter(|w| is_new_gui(w, self_pids) && !before.contains(&(w.pid, w.title.to_lowercase())))
        .cloned()
        .collect();
    out.sort_by(|a, b| {
        let aa = (a.width as u64).saturating_mul(a.height as u64);
        let ba = (b.width as u64).saturating_mul(b.height as u64);
        ba.cmp(&aa)
    });
    out
}

pub fn label_appeared(name: &str, found: &[WinMeta]) -> Vec<BoundWindow> {
    found
        .iter()
        .take(8)
        .enumerate()
        .map(|(i, w)| {
            let label = if i == 0 {
                name.to_string()
            } else {
                format!("{}-{}", name, i + 1)
            };
            BoundWindow::from_meta(label, w)
        })
        .collect()
}

pub fn bind_appeared(hub: &WindowHub, name: &str, found: &[WinMeta]) -> Vec<BoundWindow> {
    label_appeared(name, found)
        .into_iter()
        .map(|rec| {
            if let Ok(mut inner) = hub.inner.lock() {
                inner.insert(rec.name.clone(), rec.clone());
            }
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
    }
}

pub async fn snapshot() -> HashSet<(u32, String)> {
    tokio::task::spawn_blocking(|| {
        shot::collect_windows()
            .map(|listed| {
                fingerprints(&listed.into_iter().map(|(_, m)| m).collect::<Vec<_>>())
            })
            .unwrap_or_default()
    })
    .await
    .unwrap_or_default()
}

pub async fn watch(before: HashSet<(u32, String)>) -> Vec<WinMeta> {
    let self_pid = std::process::id();
    let deadline = Instant::now() + WATCH_BUDGET;
    let mut best = Vec::new();
    while Instant::now() < deadline {
        sleep(WATCH_STEP).await;
        let after = match tokio::task::spawn_blocking(shot::collect_windows).await {
            Ok(Ok(listed)) => listed.into_iter().map(|(_, m)| m).collect::<Vec<_>>(),
            _ => continue,
        };
        let found = appeared(&before, &after, &[self_pid]);
        if found.is_empty() {
            continue;
        }
        best = found;
        for _ in 0..2 {
            if Instant::now() >= deadline {
                break;
            }
            sleep(WATCH_STEP).await;
            let after = match tokio::task::spawn_blocking(shot::collect_windows).await {
                Ok(Ok(listed)) => listed.into_iter().map(|(_, m)| m).collect::<Vec<_>>(),
                _ => continue,
            };
            let found = appeared(&before, &after, &[self_pid]);
            if !found.is_empty() {
                best = found;
            }
        }
        return best;
    }
    best
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

    #[test]
    fn appeared_skips_host_and_existing() {
        let before = fingerprints(&[
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
        let found = appeared(&before, &after, &[1]);
        assert_eq!(found.len(), 1, "{found:?}");
        assert_eq!(found[0].pid, 77);
    }

    #[test]
    fn appeared_treats_title_fill_as_new() {
        let before = fingerprints(&[win("", "electron", 77, 800, 600)]);
        let after = vec![win("Ready", "electron", 77, 800, 600)];
        let found = appeared(&before, &after, &[]);
        assert_eq!(found[0].title, "Ready");
    }

    #[test]
    fn appeared_ranks_larger_first() {
        let before = fingerprints(&[]);
        let after = vec![
            win("small", "app", 1, 400, 300),
            win("big", "app", 2, 1200, 800),
        ];
        let found = appeared(&before, &after, &[]);
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
        let found = vec![
            win("one", "a", 1, 800, 600),
            win("two", "b", 2, 800, 600),
        ];
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
