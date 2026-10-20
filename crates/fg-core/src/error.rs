use std::fmt;

use thiserror::Error;

pub type Result<T> = std::result::Result<T, Error>;

/// A database-reported error, kept structured so the editor can underline the
/// offending position and tools can branch on the code.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueryError {
    pub message: String,
    /// Native error code or SQLSTATE, when the engine provides one.
    pub code: Option<String>,
    /// 0-based character offset into the statement, when the engine provides one.
    pub position: Option<usize>,
}

impl QueryError {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            code: None,
            position: None,
        }
    }

    pub fn with_code(mut self, code: impl Into<String>) -> Self {
        self.code = Some(code.into());
        self
    }
}

impl fmt::Display for QueryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.code {
            Some(code) => write!(f, "{} ({code})", self.message),
            None => f.write_str(&self.message),
        }
    }
}

#[derive(Debug, Error)]
pub enum Error {
    #[error("connection error: {0}")]
    Connection(String),
    #[error("query error: {0}")]
    Query(QueryError),
    #[error("query cancelled")]
    Cancelled,
    #[error("operation denied by policy: {0}")]
    PolicyDenied(String),
    /// The driver does not support the requested feature; see `Capabilities`.
    #[error("not supported: {0}")]
    Unsupported(String),
    #[error("storage error: {0}")]
    Storage(String),
    #[error("{0}")]
    Other(String),
}

impl From<QueryError> for Error {
    fn from(e: QueryError) -> Self {
        Error::Query(e)
    }
}
