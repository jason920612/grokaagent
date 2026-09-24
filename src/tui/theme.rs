//! Colors and fixed sizes of the terminal workbench.

use ratatui::style::Color;

pub(crate) const BG: Color = Color::Rgb(24, 24, 24);
pub(crate) const PANEL: Color = Color::Rgb(32, 32, 32);
pub(crate) const COMPOSER: Color = Color::Rgb(40, 40, 40);
pub(crate) const ACTIVITY_BG: Color = Color::Rgb(18, 18, 18);
pub(crate) const TABS_BG: Color = Color::Rgb(28, 28, 28);
pub(crate) const SELECT_BG: Color = Color::Rgb(48, 64, 88);
pub(crate) const TEXT: Color = Color::Rgb(212, 212, 212);
pub(crate) const DIM: Color = Color::Rgb(110, 110, 110);
pub(crate) const ACCENT: Color = Color::Rgb(88, 166, 255);
pub(crate) const USER: Color = Color::Rgb(156, 196, 255);
pub(crate) const AGENT: Color = Color::Rgb(163, 209, 163);
pub(crate) const BORDER: Color = Color::Rgb(62, 62, 62);
pub(crate) const WARN: Color = Color::Rgb(220, 120, 90);
pub(crate) const DIFF_ADD: Color = Color::Rgb(63, 185, 80);
pub(crate) const DIFF_DEL: Color = Color::Rgb(248, 81, 73);
pub(crate) const DIFF_HUNK: Color = Color::Rgb(88, 166, 255);
pub(crate) const TOOL: Color = Color::Rgb(210, 180, 80);
pub(crate) const THINK: Color = Color::Rgb(168, 148, 210);

/// Below this width the side bar floats over the editor instead of docking.
pub(crate) const SIDEBAR_MIN_TERM: u16 = 100;
pub(crate) const SIDEBAR_W: u16 = 30;
/// Below this width the activity bar is hidden (side views stay on F3 / Ctrl+B).
pub(crate) const ACTIVITY_MIN_TERM: u16 = 60;
pub(crate) const ACTIVITY_W: u16 = 6;
/// Rows a settings dropdown shows before scrolling.
pub(crate) const DROP_VISIBLE: usize = 8;
