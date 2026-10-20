//! A small pool of sessions per open connection.
//!
//! Why a pool at all: a session runs one statement at a time, and the UI wants a grid page, a
//! row count and a schema listing in flight together. Sessions are leased exclusively and
//! returned when the lease is dropped.

use std::sync::{Arc, Mutex};

use fg_core::{ConnectionConfig, Error, Result, SafeMode};
use fg_driver::{Driver, ExecOptions, Session};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

pub(crate) struct SessionPool {
    driver: Arc<dyn Driver>,
    config: ConnectionConfig,
    password: Option<String>,
    idle: Mutex<Vec<Box<dyn Session>>>,
    permits: Arc<Semaphore>,
}

impl SessionPool {
    pub fn new(
        driver: Arc<dyn Driver>,
        config: ConnectionConfig,
        password: Option<String>,
        max_sessions: usize,
    ) -> Arc<Self> {
        Arc::new(Self {
            driver,
            config,
            password,
            idle: Mutex::new(Vec::new()),
            permits: Arc::new(Semaphore::new(max_sessions.max(1))),
        })
    }

    /// Open one session: connect, run the connection's startup SQL, apply read-only mode.
    pub async fn open_session(&self) -> Result<Box<dyn Session>> {
        let timeout =
            std::time::Duration::from_secs(u64::from(self.config.timeouts.connect_secs.max(1)));
        let session = tokio::time::timeout(
            timeout,
            self.driver.connect(&self.config, self.password.as_deref()),
        )
        .await
        .map_err(|_| Error::Connection(format!("timed out after {}s", timeout.as_secs())))??;

        for sql in &self.config.startup_sql {
            session
                .execute(sql, &[], ExecOptions::default())
                .await?
                .collect()
                .await?;
        }
        if self.config.effective_safe_mode() == SafeMode::ReadOnly {
            session.set_read_only(true).await?;
        }
        Ok(session)
    }

    pub async fn acquire(self: &Arc<Self>) -> Result<Lease> {
        let permit = Arc::clone(&self.permits)
            .acquire_owned()
            .await
            .map_err(|_| Error::Connection("the connection was closed".into()))?;
        let idle = self.idle.lock().ok().and_then(|mut v| v.pop());
        let session = match idle {
            Some(s) => s,
            None => self.open_session().await?,
        };
        Ok(Lease {
            session: Some(session),
            pool: Arc::clone(self),
            permit: Some(permit),
            broken: false,
        })
    }

    pub fn close(&self) {
        self.permits.close();
        if let Ok(mut idle) = self.idle.lock() {
            idle.clear();
        }
    }
}

/// Exclusive use of one session. Dropping it returns the session to the pool, unless it was
/// marked broken (a connection error), in which case it is discarded.
pub(crate) struct Lease {
    session: Option<Box<dyn Session>>,
    pool: Arc<SessionPool>,
    permit: Option<OwnedSemaphorePermit>,
    broken: bool,
}

impl Lease {
    /// The leased session. Fails only if used after release, which the types prevent in practice.
    pub fn session(&self) -> Result<&dyn Session> {
        self.session
            .as_deref()
            .ok_or_else(|| Error::Connection("the session was released".into()))
    }

    pub fn mark_broken(&mut self) {
        self.broken = true;
    }
}

impl Drop for Lease {
    fn drop(&mut self) {
        if !self.broken
            && let (Some(session), Ok(mut idle)) = (self.session.take(), self.pool.idle.lock())
        {
            idle.push(session);
        }
        // The permit is released after the session is back, so the next acquirer finds it.
        drop(self.permit.take());
    }
}
