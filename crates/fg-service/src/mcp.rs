//! Agent write approvals: the registry behind `execute_write`.
//!
//! An agent may never approve its own write. When `run` meets a write
//! on a connection with `mcp.allow_write`, it files one request here,
//! broadcasts it so the GUI can show it, and waits for a person to
//! resolve it. The request is bound to the exact SQL by a one-shot id:
//! resolving an id answers that script only, and a timed-out request
//! is removed, so a late approval cannot land on a dead run.
//!
//! A service with no event listener has no GUI attached, so a write
//! there is denied at once instead of waiting out the timeout: the
//! headless case must not stall an agent for minutes per statement.

use std::collections::HashMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use fg_core::{ConnectionId, DangerReason, StatementClass};
use tokio::sync::oneshot;

use crate::Service;

/// How long an agent write waits for a person before it is denied.
pub const MCP_WRITE_APPROVAL_TIMEOUT: Duration = Duration::from_secs(300);

/// One statement of a pending agent write, as the GUI shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpWriteStatement {
    pub class: StatementClass,
    pub danger: Option<DangerReason>,
}

/// A write waiting for a person. The SQL is shown to the approver;
/// it never leaves this process except through that UI.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpWriteRequest {
    pub id: u64,
    pub client: String,
    pub connection_id: ConnectionId,
    pub connection_name: String,
    pub sql: String,
    pub statements: Vec<McpWriteStatement>,
}

/// Pending agent writes, shared by every open connection of a service.
pub struct McpWriteRegistry {
    next: AtomicU64,
    pending: Mutex<HashMap<u64, (McpWriteRequest, oneshot::Sender<bool>)>>,
}

impl McpWriteRegistry {
    pub fn new() -> Self {
        Self {
            next: AtomicU64::new(1),
            pending: Mutex::new(HashMap::new()),
        }
    }

    /// File a request and return it with the channel its resolution
    /// arrives on. The caller broadcasts the request itself.
    pub fn request(
        &self,
        client: String,
        connection_id: ConnectionId,
        connection_name: String,
        sql: String,
        statements: Vec<McpWriteStatement>,
    ) -> (McpWriteRequest, oneshot::Receiver<bool>) {
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        let request = McpWriteRequest {
            id,
            client,
            connection_id,
            connection_name,
            sql,
            statements,
        };
        let (tx, rx) = oneshot::channel();
        if let Ok(mut pending) = self.pending.lock() {
            pending.insert(id, (request.clone(), tx));
        }
        (request, rx)
    }

    /// Answer a pending request. `false` when the id is unknown or
    /// already resolved: a late answer changes nothing.
    pub fn resolve(&self, id: u64, approved: bool) -> bool {
        let entry = self
            .pending
            .lock()
            .ok()
            .and_then(|mut pending| pending.remove(&id));
        match entry {
            Some((_, tx)) => tx.send(approved).is_ok(),
            None => false,
        }
    }

    /// Forget a request without answering it, e.g. after its wait
    /// timed out. The run already denied itself; a later resolve for
    /// this id must not revive it.
    pub fn forget(&self, id: u64) {
        if let Ok(mut pending) = self.pending.lock() {
            pending.remove(&id);
        }
    }

    /// A snapshot for the GUI's pending list. Missed broadcasts still
    /// show up here, so the panel never depends on catching an event.
    pub fn pending(&self) -> Vec<McpWriteRequest> {
        self.pending
            .lock()
            .ok()
            .map(|pending| pending.values().map(|(req, _)| req.clone()).collect())
            .unwrap_or_default()
    }
}

impl Default for McpWriteRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl Service {
    /// Writes currently waiting for a person to approve or deny.
    pub fn pending_mcp_writes(&self) -> Vec<McpWriteRequest> {
        self.inner.mcp_writes.pending()
    }

    /// Answer a pending agent write. Returns whether the id was live.
    pub fn resolve_mcp_write(&self, id: u64, approved: bool) -> bool {
        self.inner.mcp_writes.resolve(id, approved)
    }
}
