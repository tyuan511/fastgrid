//! Database driver abstraction. `mysql` and `postgres` backends will implement [`Driver`].

use async_trait::async_trait;
use fg_core::{ConnectionConfig, Result};

#[async_trait]
pub trait Driver: Send + Sync {
    /// Open a connection and verify it works (used by "Test connection").
    async fn ping(&self, config: &ConnectionConfig) -> Result<()>;
}
