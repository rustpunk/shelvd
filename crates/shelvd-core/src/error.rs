//! Shared error type for shelvd core operations.

/// Errors originating in `shelvd-core` (configuration, parsing).
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("configuration error: {0}")]
    Config(String),
}

pub type Result<T> = std::result::Result<T, Error>;
