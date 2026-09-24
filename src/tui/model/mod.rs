//! UI-independent state: transcript rows, transcripts, the agent tree and
//! sessions. Nothing here draws or reads input, so it is tested directly.

pub(crate) mod agents;
pub(crate) mod rows;
pub(crate) mod session;
pub(crate) mod tool_text;
pub(crate) mod transcript;
