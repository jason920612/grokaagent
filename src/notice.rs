//! Text the harness sends to the model as a user turn that the user did not
//! write. Every such message starts with one of these prefixes, and the
//! injecting code builds it from the constant here, so the context fold
//! (which keeps the user's own words verbatim) can tell the two apart.
//! A new kind of injected message gets its prefix added to [`PREFIXES`].

/// A child agent's news: `[child agent `name` replied …]`, `paused`, `exited`.
pub const CHILD: &str = "[child agent `";
/// The old task supervisor's steering messages (still in saved histories).
pub const TASK_SYSTEM: &str = "[task-system]";
/// The task-mode user proxy's messages: written for the user, not by them.
pub const USER_PROXY: &str = "[user-proxy]";
/// Task mode resumed without a new prompt.
pub const TASK_MODE: &str = "Task mode is on. Goal:";
/// The repetition-loop guard's retry note.
pub const LOOP_GUARD: &str = "[loop guard]";
/// A model request failed and the turn continues without the user.
pub const REQUEST_FAILED: &str = "The previous model request failed";
/// The user pressed Esc (the user's next message follows separately).
pub const INTERRUPT: &str = "The user pressed Esc and interrupted the previous model turn.";

const PREFIXES: &[&str] = &[
    CHILD,
    TASK_SYSTEM,
    USER_PROXY,
    TASK_MODE,
    LOOP_GUARD,
    REQUEST_FAILED,
    INTERRUPT,
    crate::background::EXIT_NOTICE_PREFIX,
    crate::background::CLOSED_NOTICE_PREFIX,
    crate::timer::FIRED_NOTICE_PREFIX,
    crate::task::KICK,
    crate::workflow::NOTICE_PREFIX,
    crate::workflow::CLOSED_NOTICE_PREFIX,
];

/// Whether `text` is a message the harness wrote in the user's place.
pub fn is_harness(text: &str) -> bool {
    let t = text.trim_start();
    PREFIXES.iter().any(|p| t.starts_with(p))
}
