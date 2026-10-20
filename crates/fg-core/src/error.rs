use thiserror::Error;

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, Error)]
pub enum Error {
    #[error("connection error: {0}")]
    Connection(String),
    #[error("query error: {0}")]
    Query(String),
    #[error("query cancelled")]
    Cancelled,
    #[error("operation denied by policy: {0}")]
    PolicyDenied(String),
    #[error("storage error: {0}")]
    Storage(String),
    #[error("{0}")]
    Other(String),
}
