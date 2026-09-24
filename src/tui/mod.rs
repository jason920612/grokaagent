//! Cursor-like chat TUI: transcript, composer, settings overlay, queue/insert.

use std::collections::{HashMap, VecDeque};
use std::io::{self, stdout};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crossterm::event::{
    DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture, Event,
    EventStream, KeyCode, KeyEventKind, KeyModifiers, MouseButton, MouseEventKind,
};
use crossterm::cursor::MoveTo;
use crossterm::execute;
use crossterm::terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen};
use futures::{FutureExt, StreamExt};
use ratatui::buffer::{Buffer, Cell};
use ratatui::layout::{Constraint, Direction, Layout, Position, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Clear, Paragraph, Widget};
use ratatui::{backend::{Backend, CrosstermBackend}, Frame, Terminal};
use ratatui_image::picker::Picker;
use ratatui_image::protocol::Protocol;
use ratatui_image::Image;
use tokio::sync::mpsc;

use crate::agent::{CancelFlag, SessionKnobs, UserTurn};
use crate::ask::{self, AskUserHub, Question};
use crate::auth;
use crate::config::{ProviderConfig, ProviderKind};
use crate::error::{Error, Result};
use crate::events::{AgentEvent, ChannelSink, EventMeta, EventSink, FanoutSink, JsonlSink};
use crate::catalog::{clamp_effort_for_model, cycle_effort, EffortOpt, ModelCatalog};
use crate::folderpick::{self, FolderView};
use crate::hub::{self, UiCommand, UiSnapshot};
use crate::kit;
use crate::md;
use crate::provider::{AnyProvider, ReasoningEffort, XaiOauthProvider};
use crate::session::{self, SessionMeta, SessionStore};
use crate::skills::{Skill, SkillStore};
use crate::task::{self, TaskHub, TaskPhase};
use serde::{Deserialize, Serialize};
use serde_json::Value;

const BG: Color = Color::Rgb(24, 24, 24);
const PANEL: Color = Color::Rgb(32, 32, 32);
const COMPOSER: Color = Color::Rgb(40, 40, 40);
const TEXT: Color = Color::Rgb(212, 212, 212);
const DIM: Color = Color::Rgb(110, 110, 110);
const ACCENT: Color = Color::Rgb(88, 166, 255);
const USER: Color = Color::Rgb(156, 196, 255);
const AGENT: Color = Color::Rgb(163, 209, 163);
const BORDER: Color = Color::Rgb(62, 62, 62);
const WARN: Color = Color::Rgb(220, 120, 90);
const DIFF_ADD: Color = Color::Rgb(63, 185, 80);
const DIFF_DEL: Color = Color::Rgb(248, 81, 73);
const DIFF_HUNK: Color = Color::Rgb(88, 166, 255);
const TOOL: Color = Color::Rgb(210, 180, 80);
const THINK: Color = Color::Rgb(168, 148, 210);
const SIDEBAR_MIN_TERM: u16 = 110;
const SIDEBAR_W: u16 = 28;
const RAIL_MIN_TERM: u16 = 130;
const RAIL_W: u16 = 34;
const DROP_VISIBLE: usize = 8;

include!("types.rs");
include!("edit.rs");
include!("state.rs");
include!("app.rs");
include!("session_boot.rs");
include!("tool_text.rs");
include!("chat_view.rs");
include!("settings_ops.rs");
include!("draw.rs");
include!("hub_bridge.rs");
include!("run.rs");

#[cfg(test)]
mod tests;
