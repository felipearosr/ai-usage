//! Error type shared across the crate.

use std::fmt;

#[derive(Debug)]
pub enum AiuError {
    Db(rusqlite::Error),
    Io(std::io::Error),
    NoDataDir,
}

impl fmt::Display for AiuError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            AiuError::Db(e) => write!(f, "database error: {e}"),
            AiuError::Io(e) => write!(f, "io error: {e}"),
            AiuError::NoDataDir => {
                write!(f, "could not determine data directory (is HOME set?)")
            }
        }
    }
}

impl std::error::Error for AiuError {}

impl From<rusqlite::Error> for AiuError {
    fn from(e: rusqlite::Error) -> Self {
        AiuError::Db(e)
    }
}

impl From<std::io::Error> for AiuError {
    fn from(e: std::io::Error) -> Self {
        AiuError::Io(e)
    }
}

pub type Result<T> = std::result::Result<T, AiuError>;
