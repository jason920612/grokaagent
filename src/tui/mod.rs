//! Terminal workbench in the shape of VS Code: an activity bar, side views
//! (sessions, agent tree, file changes, backgrounds, task), editor tabs (the
//! main chat, one read-only tab per child agent, settings), a bottom panel
//! (tool timeline, background output, events) and a status bar.
//!
//! Layers:
//! - [`model`]: sessions, transcripts, the agent tree. No drawing, no input.
//! - `app` / `settings`: actions that change the model and settings.
//! - `ui`: draws a frame from the state; `input`: keys and mouse to actions.
//! - `runtime`: the event loop, agent runs, background fetches.
//! - `bridge`: the browser mirror.

mod app;
mod bridge;
mod edit;
mod input;
pub(crate) mod model;
mod runtime;
mod settings;
mod theme;
mod ui;

#[cfg(test)]
mod tests;

use std::path::PathBuf;

use crate::provider::ReasoningEffort;

pub use runtime::run_tui;

#[derive(Clone, Debug)]
pub struct TuiOptions {
    pub model: String,
    pub events: PathBuf,
    pub workspace: PathBuf,
    pub max_turns: u32,
    pub web_search: bool,
    pub dispatcher: bool,
    /// Default model for spawned child agents. Empty = follow the main model.
    pub child_model: String,
    pub reasoning_effort: ReasoningEffort,
}
