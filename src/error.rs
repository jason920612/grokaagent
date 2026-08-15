use thiserror::Error;

#[derive(Debug, Error)]
pub enum Error {
    #[error("{0}")]
    Auth(String),
    #[error("{0}")]
    Provider(String),
    #[error("{0}")]
    Tool(String),
    #[error("{0}")]
    A2a(String),
    #[error("{0}")]
    Update(String),
    #[error("empty prompt")]
    EmptyPrompt,
    #[error("max turns ({0}) reached")]
    MaxTurns(u32),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
    #[error(transparent)]
    Http(#[from] reqwest::Error),
}

pub type Result<T> = std::result::Result<T, Error>;
