//! A small pool of sessions per open connection.
//!
//! Why a pool at all: a session runs one statement at a time, and the UI wants a grid page, a
//! row count and a schema listing in flight together. Sessions are leased exclusively and
//! returned when the lease is dropped.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use fg_core::{ConnectionConfig, Error, Result, SafeMode, StatementClass};
use fg_driver::{CancelHandle, Driver, ExecOptions, Session};
use fg_sql::analyze;
use tokio::sync::{Mutex as AsyncMutex, OwnedSemaphorePermit, Semaphore};

use crate::Epochs;

pub(crate) struct SessionPool {
    driver: Arc<dyn Driver>,
    config: ConnectionConfig,
    password: Option<String>,
    idle: Mutex<Vec<Box<dyn Session>>>,
    permits: Arc<Semaphore>,
    /// Cancel handles of sessions currently leased. `close` interrupts them so a settings change
    /// does not leave a write running under the old policy.
    active: Mutex<Vec<(u64, CancelHandle)>>,
    next_active: AtomicU64,
    closed: AtomicBool,
    /// Generation of the config this pool was opened with. A save or delete bumps the shared
    /// counter; a lease from a stale generation is refused.
    epoch: u64,
    epochs: Epochs,
    /// Held across the epoch check and `Session::execute`, so `close` cannot cancel and then
    /// lose that cancel to the driver's reset. `close` waits for it on a helper thread.
    exec_gate: Arc<AsyncMutex<()>>,
    /// Test seam. Taken by the next statement start, before the gate, so a save can finish
    /// closing before execute is entered.
    #[cfg(any(test, feature = "test-seams"))]
    execute_pause: Mutex<Option<ExecutePauseInner>>,
}

impl SessionPool {
    pub fn new(
        driver: Arc<dyn Driver>,
        config: ConnectionConfig,
        password: Option<String>,
        max_sessions: usize,
        epoch: u64,
        epochs: Epochs,
    ) -> Arc<Self> {
        Arc::new(Self {
            driver,
            config,
            password,
            idle: Mutex::new(Vec::new()),
            permits: Arc::new(Semaphore::new(max_sessions.max(1))),
            active: Mutex::new(Vec::new()),
            next_active: AtomicU64::new(1),
            closed: AtomicBool::new(false),
            epoch,
            epochs,
            exec_gate: Arc::new(AsyncMutex::new(())),
            #[cfg(any(test, feature = "test-seams"))]
            execute_pause: Mutex::new(None),
        })
    }

    pub fn is_closed(&self) -> bool {
        self.closed.load(Ordering::SeqCst)
    }

    fn current_epoch(&self) -> u64 {
        self.epochs
            .lock()
            .unwrap_or_else(|err| err.into_inner())
            .get(&self.config.id)
            .copied()
            .unwrap_or(0)
    }

    /// Fail closed when the connection was closed or its saved config moved on.
    pub fn ensure_current(&self) -> Result<()> {
        if self.is_closed() || self.current_epoch() != self.epoch {
            Err(stale_connection())
        } else {
            Ok(())
        }
    }

    /// Open one session: connect, establish read-only, run startup SQL, re-assert read-only.
    ///
    /// Startup SQL is an allow-list of session settings that do not persist (see
    /// [`validate_startup_sql`]). The read-only flag is set before and after those statements so
    /// a session setting cannot leave the engine writable, but that re-assert is not what stops
    /// a header write: the allow-list refuses the statement before it is sent.
    pub async fn open_session(&self) -> Result<Box<dyn Session>> {
        self.ensure_current()?;
        let timeout =
            std::time::Duration::from_secs(u64::from(self.config.timeouts.connect_secs.max(1)));
        let session = tokio::time::timeout(
            timeout,
            self.driver.connect(&self.config, self.password.as_deref()),
        )
        .await
        .map_err(|_| Error::Connection(format!("timed out after {}s", timeout.as_secs())))??;

        let read_only = self.config.effective_safe_mode() == SafeMode::ReadOnly;
        if read_only {
            session.set_read_only(true).await?;
        }
        for sql in &self.config.startup_sql {
            validate_startup_sql(sql, self.config.kind)?;
            session
                .execute(sql, &[], ExecOptions::default())
                .await?
                .collect()
                .await?;
        }
        if read_only {
            session.set_read_only(true).await?;
        }
        self.ensure_current()?;
        Ok(session)
    }

    pub async fn acquire(self: &Arc<Self>) -> Result<Lease> {
        self.ensure_current()?;
        let permit = Arc::clone(&self.permits)
            .acquire_owned()
            .await
            .map_err(|_| stale_connection())?;
        if let Err(err) = self.ensure_current() {
            drop(permit);
            return Err(err);
        }
        let idle = self.idle.lock().ok().and_then(|mut v| v.pop());
        let session = match idle {
            Some(s) => s,
            None => match self.open_session().await {
                Ok(session) => session,
                Err(err) => {
                    drop(permit);
                    return Err(err);
                }
            },
        };
        if let Err(err) = self.ensure_current() {
            drop(session);
            drop(permit);
            return Err(err);
        }
        // Re-assert after a reused session too: a previous statement must not have cleared it.
        if self.config.effective_safe_mode() == SafeMode::ReadOnly
            && let Err(err) = session.set_read_only(true).await
        {
            drop(session);
            drop(permit);
            return Err(err);
        }
        let cancel = session.cancel_handle();
        let active_id = self.next_active.fetch_add(1, Ordering::Relaxed);
        if let Ok(mut active) = self.active.lock() {
            active.push((active_id, cancel.clone()));
        }
        if self.is_closed() {
            cancel.cancel();
            drop(session);
            drop(permit);
            return Err(stale_connection());
        }
        Ok(Lease {
            session: Some(session),
            pool: Arc::clone(self),
            permit: Some(permit),
            broken: false,
            active_id,
        })
    }

    pub fn close(&self) {
        self.closed.store(true, Ordering::SeqCst);
        self.permits.close();
        self.cancel_active();
        // A statement start holds `exec_gate` across `Session::execute`. Wait until that critical
        // section ends, then cancel again: the driver's reset may have cleared a cancel that
        // arrived before the statement was published. A helper thread blocks so this caller does
        // not stall the runtime that is finishing the critical section.
        let gate = Arc::clone(&self.exec_gate);
        let (tx, rx) = std::sync::mpsc::sync_channel(1);
        let spawned = std::thread::Builder::new()
            .name("fastgrid-close".into())
            .spawn(move || {
                let _guard = gate.blocking_lock();
                let _ = tx.send(());
            });
        if spawned.is_ok() {
            let _ = rx.recv_timeout(std::time::Duration::from_secs(5));
        }
        self.cancel_active();
        if let Ok(mut idle) = self.idle.lock() {
            idle.clear();
        }
    }

    fn cancel_active(&self) {
        if let Ok(active) = self.active.lock() {
            for (_, cancel) in active.iter() {
                cancel.cancel();
            }
        }
    }
}

fn stale_connection() -> Error {
    Error::Connection("the connection was closed or its settings changed; reopen it".into())
}

/// Startup SQL may only apply an allow-listed session setting.
///
/// Every statement must classify as [`StatementClass::Utility`] or [`StatementClass::Read`],
/// must not be flagged dangerous, and must be recognised as a setting that does not persist to
/// the database file. Unknown forms are refused. `query_only` is refused even if a later edit
/// adds it to a list: it is the engine guard, not a startup knob.
///
/// SQLite allow-list (session-only, documented by SQLite as not written to the database header
/// or schema):
/// * `foreign_keys` — per-connection enforcement, not stored.
/// * `busy_timeout` — lock wait, per connection.
/// * `cache_size` — page cache hint. `default_cache_size` is persistent and is not listed.
/// * `temp_store` — where temporary objects live for this connection.
/// * `mmap_size` — per-connection mapping hint.
/// * `recursive_triggers`, `defer_foreign_keys`, `read_uncommitted`, `automatic_index` —
///   per-connection behaviour that does not rewrite the file.
///
/// Kept out, because they write the header, change durability, or can rewrite the file:
/// `synchronous`, `journal_mode`, `user_version`, `application_id`, `schema_version`,
/// `page_size`, `auto_vacuum`, `encoding`, `secure_delete`, `wal_checkpoint`,
/// `incremental_vacuum`, `optimize`. `query_only` is refused by name on top of that.
///
/// PostgreSQL: `SET [SESSION|LOCAL] <name>` for a short list of non-persistent client settings
/// (`search_path`, `statement_timeout`, `lock_timeout`,
/// `idle_in_transaction_session_timeout`, `timezone`, `client_encoding`, `extra_float_digits`,
/// `datestyle`, `intervalstyle`, `standard_conforming_strings`, `bytea_output`,
/// `client_min_messages`). `SET ROLE` and anything else is refused.
///
/// MySQL/MariaDB: `SET SESSION`, `SET NAMES`, `SET CHARACTER SET`, and `USE`. Bare `SET` and
/// `SET GLOBAL` are refused because they can persist or affect the server.
pub(crate) fn validate_startup_sql(sql: &str, kind: fg_core::DbKind) -> Result<()> {
    let analysis = analyze(sql, kind);
    if analysis.statements.is_empty() {
        return Err(Error::PolicyDenied(
            "startup SQL is empty and is not an allow-listed session setting".into(),
        ));
    }
    for (index, (span, info)) in analysis.statements.iter().enumerate() {
        let text = span.sql(sql);
        let class_ok = matches!(info.class, StatementClass::Utility | StatementClass::Read);
        if !class_ok || info.danger.is_some() || !is_session_setting(text, kind) {
            return Err(Error::PolicyDenied(format!(
                "startup SQL statement {index} is classified {:?} and is not an allow-listed session setting",
                info.class
            )));
        }
    }
    Ok(())
}

/// A utility the read-only execute path may run: a SQLite pragma query that does not assign and
/// is not a command that writes when invoked. Assignment forms and other engines' `SET` stay
/// blocked, because they change session or persistent state.
pub(crate) fn utility_is_pure_read(sql: &str, kind: fg_core::DbKind) -> bool {
    if kind != fg_core::DbKind::Sqlite {
        return false;
    }
    let Some(pragma) = parse_sqlite_pragma(sql) else {
        return false;
    };
    if pragma.equals || SIDE_EFFECT_PRAGMAS.contains(&pragma.name.as_str()) {
        return false;
    }
    if pragma.parenthesized {
        return READ_PRAGMAS_WITH_ARGS.contains(&pragma.name.as_str());
    }
    true
}

fn is_session_setting(sql: &str, kind: fg_core::DbKind) -> bool {
    match kind {
        fg_core::DbKind::Sqlite => sqlite_session_setting(sql),
        fg_core::DbKind::Postgres => postgres_session_setting(sql),
        fg_core::DbKind::MySql => mysql_session_setting(sql),
        fg_core::DbKind::Unknown => false,
        _ => false,
    }
}

fn sqlite_session_setting(sql: &str) -> bool {
    let Some(pragma) = parse_sqlite_pragma(sql) else {
        return false;
    };
    if pragma.name == "query_only" {
        return false;
    }
    SQLITE_SESSION_PRAGMAS.contains(&pragma.name.as_str())
}

/// Session pragmas that do not persist. See [`validate_startup_sql`].
const SQLITE_SESSION_PRAGMAS: &[&str] = &[
    "foreign_keys",
    "busy_timeout",
    "cache_size",
    "temp_store",
    "mmap_size",
    "recursive_triggers",
    "defer_foreign_keys",
    "read_uncommitted",
    "automatic_index",
];

/// Invoking these without `=` still writes or releases resources. Not a pure read.
const SIDE_EFFECT_PRAGMAS: &[&str] = &[
    "optimize",
    "incremental_vacuum",
    "wal_checkpoint",
    "shrink_memory",
];

/// Parentheses are a query argument for these names (`table_info(t)`), not a setter.
const READ_PRAGMAS_WITH_ARGS: &[&str] = &[
    "table_info",
    "table_xinfo",
    "table_list",
    "index_info",
    "index_xinfo",
    "index_list",
    "foreign_key_list",
    "foreign_key_check",
    "database_list",
    "collation_list",
    "function_list",
    "module_list",
    "pragma_list",
    "compile_options",
    "integrity_check",
    "quick_check",
];

const POSTGRES_SESSION_SETTINGS: &[&str] = &[
    "search_path",
    "statement_timeout",
    "lock_timeout",
    "idle_in_transaction_session_timeout",
    "timezone",
    "client_encoding",
    "extra_float_digits",
    "datestyle",
    "intervalstyle",
    "standard_conforming_strings",
    "bytea_output",
    "client_min_messages",
];

const MYSQL_SESSION_SETTINGS: &[&str] = &[
    "transaction_isolation",
    "tx_isolation",
    "time_zone",
    "sql_mode",
    "wait_timeout",
    "net_read_timeout",
    "net_write_timeout",
    "group_concat_max_len",
    "character_set_client",
    "character_set_results",
    "character_set_connection",
    "collation_connection",
];

struct PragmaCall {
    name: String,
    /// `PRAGMA name = value`. A setter, never a pure read.
    equals: bool,
    /// `PRAGMA name(...)`. A query argument for some names, a setter for others.
    parenthesized: bool,
}

fn parse_sqlite_pragma(sql: &str) -> Option<PragmaCall> {
    let cleaned = strip_comments(sql);
    let mut rest = cleaned.trim();
    while rest.starts_with('(') {
        rest = rest[1..].trim_start();
    }
    let (word, rest) = parse_ident(rest)?;
    if !word.eq_ignore_ascii_case("pragma") {
        return None;
    }
    let mut rest = rest.trim_start();
    let (mut name, after_name) = parse_ident(rest)?;
    rest = after_name.trim_start();
    if let Some(stripped) = rest.strip_prefix('.') {
        rest = stripped.trim_start();
        let (qualified, after) = parse_ident(rest)?;
        name = qualified;
        rest = after.trim_start();
    }
    let parenthesized = rest.starts_with('(');
    let equals = rest.starts_with('=');
    Some(PragmaCall {
        name: name.to_ascii_lowercase(),
        equals,
        parenthesized,
    })
}

fn postgres_session_setting(sql: &str) -> bool {
    let cleaned = strip_comments(sql);
    let rest = cleaned.trim();
    let (word, rest) = match parse_ident(rest) {
        Some(pair) => pair,
        None => return false,
    };
    if !word.eq_ignore_ascii_case("set") {
        return false;
    }
    let rest = rest.trim_start();
    let (second, after) = match parse_ident(rest) {
        Some(pair) => pair,
        None => return false,
    };
    let (name, after_name) =
        if second.eq_ignore_ascii_case("session") || second.eq_ignore_ascii_case("local") {
            match parse_ident(after.trim_start()) {
                Some(pair) => pair,
                None => return false,
            }
        } else if second.eq_ignore_ascii_case("role")
            || second.eq_ignore_ascii_case("session_authorization")
        {
            return false;
        } else {
            (second, after)
        };
    let name = name.to_ascii_lowercase();
    if !POSTGRES_SESSION_SETTINGS.contains(&name.as_str()) {
        return false;
    }
    let after_name = after_name.trim_start();
    after_name.starts_with('=') || after_name.to_ascii_uppercase().starts_with("TO")
}

fn mysql_session_setting(sql: &str) -> bool {
    let cleaned = strip_comments(sql);
    let rest = cleaned.trim();
    let (word, rest) = match parse_ident(rest) {
        Some(pair) => pair,
        None => return false,
    };
    if word.eq_ignore_ascii_case("use") {
        let rest = rest.trim_start();
        let Some((db, after)) = parse_ident(rest) else {
            return false;
        };
        return !db.is_empty() && after.trim().is_empty();
    }
    if !word.eq_ignore_ascii_case("set") {
        return false;
    }
    let rest = rest.trim_start();
    let upper = rest.to_ascii_uppercase();
    if upper.starts_with("GLOBAL") || upper.starts_with("@@GLOBAL") {
        return false;
    }
    if upper.starts_with("NAMES")
        || upper.starts_with("CHARSET")
        || upper.starts_with("CHARACTER SET")
    {
        return true;
    }
    if let Some(after) = rest.strip_prefix("@@") {
        let after = after.trim_start();
        let Some((scope, after)) = parse_ident(after) else {
            return false;
        };
        if !scope.eq_ignore_ascii_case("session") {
            return false;
        }
        let after = after.trim_start().trim_start_matches('.').trim_start();
        let Some((name, _)) = parse_ident(after) else {
            return false;
        };
        return MYSQL_SESSION_SETTINGS.contains(&name.to_ascii_lowercase().as_str());
    }
    let Some((scope, after)) = parse_ident(rest) else {
        return false;
    };
    if !scope.eq_ignore_ascii_case("session") {
        return false;
    }
    let Some((name, _)) = parse_ident(after.trim_start()) else {
        return false;
    };
    MYSQL_SESSION_SETTINGS.contains(&name.to_ascii_lowercase().as_str())
}

fn parse_ident(input: &str) -> Option<(&str, &str)> {
    let input = input.trim_start();
    if input.is_empty() {
        return None;
    }
    let bytes = input.as_bytes();
    if bytes[0] == b'"' || bytes[0] == b'`' || bytes[0] == b'[' {
        let end_ch = if bytes[0] == b'[' { b']' } else { bytes[0] };
        let close = input[1..].find(end_ch as char)?;
        let name = &input[1..1 + close];
        if name.is_empty() {
            return None;
        }
        return Some((name, &input[1 + close + 1..]));
    }
    let end = input
        .find(|c: char| !c.is_ascii_alphanumeric() && c != '_')
        .unwrap_or(input.len());
    if end == 0 {
        return None;
    }
    Some((&input[..end], &input[end..]))
}

fn strip_comments(sql: &str) -> String {
    let mut out = String::with_capacity(sql.len());
    let mut chars = sql.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '-' && chars.peek() == Some(&'-') {
            chars.next();
            for next in chars.by_ref() {
                if next == '\n' {
                    out.push('\n');
                    break;
                }
            }
            continue;
        }
        if ch == '/' && chars.peek() == Some(&'*') {
            chars.next();
            while let Some(next) = chars.next() {
                if next == '*' && chars.peek() == Some(&'/') {
                    chars.next();
                    break;
                }
            }
            out.push(' ');
            continue;
        }
        if ch == '\'' || ch == '"' || ch == '`' {
            out.push(ch);
            while let Some(next) = chars.next() {
                out.push(next);
                if next == ch {
                    if chars.peek() == Some(&ch) {
                        if let Some(doubled) = chars.next() {
                            out.push(doubled);
                        }
                        continue;
                    }
                    break;
                }
            }
            continue;
        }
        out.push(ch);
    }
    out
}

/// Exclusive use of one session. Dropping it returns the session to the pool, unless it was
/// marked broken (a connection error), in which case it is discarded.
pub(crate) struct Lease {
    session: Option<Box<dyn Session>>,
    pool: Arc<SessionPool>,
    permit: Option<OwnedSemaphorePermit>,
    broken: bool,
    active_id: u64,
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

    /// Epoch check and `Session::execute` under the same lock `close` waits for.
    ///
    /// A save that already closed the pool is refused here, so the driver never gets to clear a
    /// cancel that was issued for the retired generation. The pause seam, when armed, runs before
    /// the lock so a test can finish `close` first.
    pub async fn run_statement(
        &self,
        sql: &str,
        params: &[fg_core::Value],
        options: ExecOptions,
    ) -> Result<fg_driver::ExecutionStream> {
        #[cfg(any(test, feature = "test-seams"))]
        self.wait_execute_pause().await;
        let _gate = self.pool.exec_gate.lock().await;
        self.pool.ensure_current()?;
        let stream = self.session()?.execute(sql, params, options).await?;
        if let Err(err) = self.pool.ensure_current() {
            if let Ok(session) = self.session() {
                session.cancel_handle().cancel();
            }
            return Err(err);
        }
        Ok(stream)
    }

    #[cfg(any(test, feature = "test-seams"))]
    async fn wait_execute_pause(&self) {
        let pause = self
            .pool
            .execute_pause
            .lock()
            .ok()
            .and_then(|mut slot| slot.take());
        if let Some(mut pause) = pause {
            if let Some(reached) = pause.reached.take() {
                let _ = reached.send(());
            }
            let _ = pause.release.await;
        }
    }
}

#[cfg(any(test, feature = "test-seams"))]
struct ExecutePauseInner {
    reached: Option<tokio::sync::oneshot::Sender<()>>,
    release: tokio::sync::oneshot::Receiver<()>,
}

/// Handle for the execute-start seam. The statement task waits until [`Self::release`].
#[cfg(any(test, feature = "test-seams"))]
pub struct ExecutePause {
    reached: Option<tokio::sync::oneshot::Receiver<()>>,
    release: tokio::sync::oneshot::Sender<()>,
}

#[cfg(any(test, feature = "test-seams"))]
impl ExecutePause {
    pub async fn wait(&mut self) {
        if let Some(reached) = self.reached.take() {
            let _ = reached.await;
        }
    }

    pub fn release(self) {
        let _ = self.release.send(());
    }
}

#[cfg(any(test, feature = "test-seams"))]
impl SessionPool {
    pub fn arm_execute_pause(&self) -> ExecutePause {
        let (reached_tx, reached_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = tokio::sync::oneshot::channel();
        if let Ok(mut slot) = self.execute_pause.lock() {
            *slot = Some(ExecutePauseInner {
                reached: Some(reached_tx),
                release: release_rx,
            });
        }
        ExecutePause {
            reached: Some(reached_rx),
            release: release_tx,
        }
    }
}

impl Drop for Lease {
    fn drop(&mut self) {
        if let Ok(mut active) = self.pool.active.lock() {
            active.retain(|(id, _)| *id != self.active_id);
        }
        if !self.broken
            && !self.pool.is_closed()
            && let (Some(session), Ok(mut idle)) = (self.session.take(), self.pool.idle.lock())
        {
            idle.push(session);
        }
        // The permit is released after the session is back, so the next acquirer finds it.
        drop(self.permit.take());
    }
}
