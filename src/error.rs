use thiserror::Error;

#[derive(Debug, Error)]
pub enum SyncError {
    #[error("failed to load config: {0}")]
    Config(String),

    #[error("simplefin request failed: {0}")]
    Simplefin(#[from] reqwest::Error),

    #[error("simplefin access url is invalid: {0}")]
    InvalidAccessUrl(String),

    #[error("browser automation error: {0}")]
    Browser(String),
}
