//! Error type shared across the crate.

use std::fmt;

#[derive(Debug)]
pub enum AiuError {
    Db(rusqlite::Error),
    Io(std::io::Error),
    NoDataDir,
    /// An adapter received data whose format it does not recognize. This is
    /// loud and source-scoped: that source stops, diagnostics are recorded,
    /// other sources keep working (spec: contained adapter failure).
    UnrecognizedFormat {
        source: &'static str,
        detail: String,
    },
}

impl fmt::Display for AiuError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            AiuError::Db(e) => write!(f, "database error: {e}"),
            AiuError::Io(e) => write!(f, "io error: {e}"),
            AiuError::NoDataDir => {
                write!(f, "could not determine data directory (is HOME set?)")
            }
            AiuError::UnrecognizedFormat { source, detail } => write!(
                f,
                "unrecognized {source} data format — the tool may have changed upstream \
                 (diagnostic recorded locally): {detail}"
            ),
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
