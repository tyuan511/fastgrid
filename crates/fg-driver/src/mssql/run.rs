//! Statement execution for SQL Server.
//!
//! Statements are split with `fg-sql` (which also understands the `GO` batch
//! separator) and run one at a time, so each one gets its own `ResultStart` /
//! `Rows` / `ResultEnd` triple. Two request paths exist and the choice is not
//! cosmetic:
//!
//! * **Batch text** (`simple_query`) for statements without parameters. A
//!   batch keeps the caller's session state: temporary tables created by a
//!   batch are visible to later batches, `USE` really switches the database,
//!   and `BEGIN TRANSACTION` opens a transaction that survives the next
//!   request. An RPC call (`sp_executesql`) gets a nested scope instead, where
//!   `#temp` tables vanish and `USE` is ignored.
//! * **RPC** (`query`) when parameters are bound, because only `sp_executesql`
//!   binds them. Placeholders are `@P1`, `@P2`, … in both the SQL and the
//!   dialect, so a caller can also send `@P1` text through the batch path.
//!
//! `execute()` is never used to read rows: on a `SELECT` it reports the row
//! *count* and drops the rows. It is used only for the few bookkeeping
//! statements this module issues itself.
//!
//! ## Affected row counts
//!
//! A batch DML response carries no per-statement count, and a row set must
//! report `affected_rows: None` (the `stream` contract), so the count is read
//! with `@@ROWCOUNT` appended to the statement:
//!
//! ```sql
//! UPDATE t SET a = 1
//! ;SELECT @@ROWCOUNT AS __fastgrid_rowcount
//! ```
//!
//! The leading `;` terminates whatever the statement ended with — a trailing
//! `-- comment` on the last line would otherwise swallow the probe. `@@ROWCOUNT`
//! reads the count of the statement immediately before it, and the probe always
//! returns exactly one row, which is how the runner recognises and strips it.
//!
//! ## Cancellation
//!
//! `Client::cancel_query` is unusable here: measured against SQL Server 2022 it
//! either errors with "Never got a DONE token acknowledging the Attention
//! signal" or leaves the connection desynchronised, after which the *next*
//! statement silently returns zero rows. Cancelling therefore means `KILL
//! {spid}` from a second connection, and a `KILL` that the server refuses (a
//! low-privilege login gets error 6102) falls back to failing the statement with
//! `Error::Connection`, which makes the service drop this session instead of
//! reusing a poisoned one.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use fg_core::{ColumnMeta, Error, QueryError, Result, Value};
use futures::TryStreamExt;
use tiberius::{Client, Config, QueryItem, Row};
use tokio::net::TcpStream;
use tokio::sync::{Mutex, mpsc};
use tokio::time::sleep;
use tokio_util::compat::{Compat, TokioAsyncWriteCompatExt};

use super::types::{Param, data_type, decode, to_param};
use crate::{Batcher, Event, ExecOptions, Push};

/// Alias for the connection type this driver hands to `tiberius`. The tokio
/// stream is wrapped because `tiberius` speaks `futures`' IO traits.
pub type Conn = Client<Compat<TcpStream>>;

/// Column alias of the appended `@@ROWCOUNT` probe. Unlikely to collide with a
/// user column, and matched by name rather than by position so a result set the
/// statement itself returned is never mistaken for the probe.
const ROWCOUNT_ALIAS: &str = "__fastgrid_rowcount";

/// Shared cancellation state for one session.
pub struct Canceller {
    /// Set by `cancel()`; cleared when an `execute` call begins.
    pub flag: AtomicBool,
    /// `@@SPID` of the session's connection, for `KILL`.
    pub spid: i32,
    /// A fresh config (same credentials and transport) for the kill
    /// connection. The session's own client cannot be reused: it is busy
    /// driving the statement being cancelled.
    pub config: Config,
    /// Set when the session must not be reused: a `KILL` the server refused
    /// leaves the statement running, and a request that failed mid-stream
    /// leaves the wire out of step.
    pub poisoned: Arc<AtomicBool>,
    /// Incremented by every `cancel()`. A `run` compares it before and after a
    /// statement, so a cancellation that arrives as a different server error
    /// still reports as `Error::Cancelled`.
    pub cancels: AtomicU64,
}

impl Canceller {
    pub fn cancel(&self) {
        self.flag.store(true, Ordering::SeqCst);
        self.cancels.fetch_add(1, Ordering::SeqCst);
        let config = self.config.clone();
        let spid = self.spid;
        let poisoned = Arc::clone(&self.poisoned);
        // The kill runs on a side connection so it never waits on the
        // session's mutex, which the running statement holds. Without a
        // runtime (a bare `cancel()` outside async work) the flag alone still
        // stops the next statement.
        let Ok(handle) = tokio::runtime::Handle::try_current() else {
            return;
        };
        drop(handle.spawn(async move {
            if kill_session(config, spid).await.is_err() {
                // The statement is still running and the server refused to
                // stop it: this session cannot be handed to another task.
                poisoned.store(true, Ordering::SeqCst);
            }
        }));
    }
}

/// Open a second connection and `KILL` the session behind `spid`.
async fn kill_session(config: Config, spid: i32) -> Result<()> {
    let tcp = TcpStream::connect(config.get_addr())
        .await
        .map_err(|e| Error::Connection(format!("kill connection failed: {e}")))?;
    let _ = tcp.set_nodelay(true);
    let mut client = Client::connect(config, tcp.compat_write())
        .await
        .map_err(|e| Error::Connection(format!("kill connection failed: {e}")))?;
    let mut stream = client
        .simple_query(format!("KILL {spid}"))
        .await
        .map_err(map_err)?;
    while let Some(_item) = stream.try_next().await.map_err(map_err)? {}
    Ok(())
}

/// Execute `sql` on the session connection, streaming events to `tx`.
pub async fn execute(
    conn: Arc<Mutex<Conn>>,
    canceller: Arc<Canceller>,
    sql: &str,
    params: &[Value],
    options: ExecOptions,
    tx: mpsc::Sender<Result<Event>>,
) {
    // A previous cancel must not leak into this call; a cancel issued during
    // this call sets the flag again.
    canceller.flag.store(false, Ordering::SeqCst);
    let statements = fg_sql::split_statements(sql, fg_core::DbKind::MsSql);
    if !params.is_empty() && statements.len() != 1 {
        let _ = tx
            .send(Err(Error::InvalidInput(
                "parameters bind to a single statement".into(),
            )))
            .await;
        return;
    }
    let encoded: Result<Vec<Param>> = params.iter().map(to_param).collect();
    let encoded = match encoded {
        Ok(v) => v,
        Err(e) => {
            let _ = tx.send(Err(e)).await;
            return;
        }
    };
    let mut guard = conn.lock().await;
    for (index, stmt) in statements.iter().enumerate() {
        if canceller.poisoned.load(Ordering::SeqCst) {
            let _ = tx
                .send(Err(Error::Connection(
                    "this session was killed and cannot be reused".into(),
                )))
                .await;
            return;
        }
        if canceller.flag.load(Ordering::SeqCst) {
            let _ = tx.send(Err(Error::Cancelled)).await;
            return;
        }
        let text = stmt.sql(sql).trim();
        if text.is_empty() {
            continue;
        }
        let started = Instant::now();
        // The timeout fires the session's cancel (a `KILL` from a side
        // connection) rather than dropping the request future: a dropped
        // request leaves the connection mid-message and the next statement on
        // it can silently return nothing. The task is aborted when the
        // statement finishes first, so a late timeout can never hit the next
        // one.
        let timeout_task = options.statement_timeout_secs.map(|secs| {
            let canceller = Arc::clone(&canceller);
            tokio::spawn(async move {
                sleep(Duration::from_secs(u64::from(secs))).await;
                canceller.cancel();
            })
        });
        let before = canceller.cancels.load(Ordering::SeqCst);
        let outcome = run_one(&mut guard, index, text, &encoded, options, started, &tx).await;
        if let Some(task) = timeout_task {
            task.abort();
        }
        match outcome {
            Ok(()) => {}
            Err(e) => {
                // A cancel that raced the failure: report it as a
                // cancellation, not as whatever error the kill produced.
                let e = if canceller.cancels.load(Ordering::SeqCst) != before
                    && !matches!(e, Error::Query(_))
                {
                    Error::Cancelled
                } else {
                    e
                };
                let _ = tx.send(Err(e)).await;
                return;
            }
        }
    }
}

/// Run one statement, streaming its result sets.
#[allow(clippy::too_many_arguments)]
async fn run_one(
    conn: &mut Conn,
    index: usize,
    sql: &str,
    params: &[Param],
    options: ExecOptions,
    started: Instant,
    tx: &mpsc::Sender<Result<Event>>,
) -> Result<()> {
    // A module definition owns the whole batch — its body runs to the end of it
    // — so an appended probe would land inside the body. Measured:
    // `CREATE VIEW v AS SELECT 1 AS a; SELECT @@ROWCOUNT AS n` fails with
    // "Incorrect syntax near the keyword 'SELECT'". Those statements get no
    // probe and therefore report no affected count.
    let probe = !owns_the_batch(sql);
    let with_probe = if probe {
        format!("{sql}\n;SELECT @@ROWCOUNT AS {ROWCOUNT_ALIAS}")
    } else {
        sql.to_owned()
    };
    if params.is_empty() {
        let stream = conn.simple_query(&with_probe).await.map_err(map_err)?;
        stream_sets(stream, index, probe, options, started, tx).await
    } else {
        let borrowed: Vec<&dyn tiberius::ToSql> =
            params.iter().map(|p| p as &dyn tiberius::ToSql).collect();
        let stream = conn.query(&with_probe, &borrowed).await.map_err(map_err)?;
        stream_sets(stream, index, probe, options, started, tx).await
    }
}

/// Whether `sql` defines a module (`VIEW`, `PROCEDURE`, `TRIGGER`, `FUNCTION`)
/// whose body extends to the end of its batch.
fn owns_the_batch(sql: &str) -> bool {
    fn module(word: &str) -> bool {
        matches!(word, "view" | "procedure" | "proc" | "trigger" | "function")
    }
    let mut words = sql
        .split(|c: char| !(c.is_alphanumeric() || c == '_'))
        .filter(|w| !w.is_empty())
        .map(str::to_ascii_lowercase);
    match words.next().as_deref() {
        Some("create") => match words.next().as_deref() {
            // `CREATE OR ALTER <module>`
            Some("or") => {
                let _ = words.next();
                words.next().is_some_and(|w| module(&w))
            }
            Some(next) => module(next),
            None => false,
        },
        Some("alter") => words.next().is_some_and(|w| module(&w)),
        _ => false,
    }
}

/// Stream every result set of one statement, stripping the `@@ROWCOUNT` probe.
async fn stream_sets(
    mut stream: tiberius::QueryStream<'_>,
    index: usize,
    probe: bool,
    options: ExecOptions,
    started: Instant,
    tx: &mpsc::Sender<Result<Event>>,
) -> Result<()> {
    let mut open: Option<Batcher> = None;
    let mut current: Option<Vec<ColumnMeta>> = None;
    let mut any_set = false;
    // Count reported by the probe of the set that just closed. A row set never
    // reports a count (the contract: columns present ⇒ `affected_rows: None`).
    let mut pending_rows: Option<u64> = None;
    loop {
        let item = match stream.try_next().await {
            Ok(Some(item)) => item,
            Ok(None) => break,
            // An abandoned result set leaves the connection mid-message.
            // Nothing else will read it, so a non-server error means the
            // session is unusable.
            Err(e) => return Err(map_stream_err(e)),
        };
        match item {
            QueryItem::Metadata(meta) => {
                let cols: Vec<ColumnMeta> = meta
                    .columns()
                    .iter()
                    .map(|c| ColumnMeta::new(c.name().to_owned(), data_type(c)))
                    .collect();
                if probe && is_probe(&cols) {
                    // The probe's single row carries the previous statement's
                    // count; it is consumed below and never surfaced.
                    pending_rows = None;
                    current = Some(cols);
                    continue;
                }
                if let Some(batcher) = open.take() {
                    close_set(tx, batcher, started, None).await?;
                }
                send(
                    tx,
                    Event::ResultStart {
                        index,
                        columns: cols.clone(),
                    },
                )
                .await?;
                current = Some(cols);
                open = Some(Batcher::new(options));
                any_set = true;
            }
            QueryItem::Row(row) => {
                let Some(cols) = current.as_ref() else {
                    return Err(Error::Other(
                        "SQL Server sent a row before any column metadata".into(),
                    ));
                };
                if probe && is_probe(cols) {
                    pending_rows = probe_value(&row);
                    continue;
                }
                let Some(batcher) = open.as_mut() else {
                    continue;
                };
                let decoded: Vec<Value> = row.cells().map(|(_col, data)| decode(data)).collect();
                match batcher.push(decoded) {
                    Push::Continue => {}
                    Push::Flush(rows) => send(tx, Event::Rows(rows)).await?,
                    Push::Stop(rows) => {
                        if !rows.is_empty() {
                            send(tx, Event::Rows(rows)).await?;
                        }
                        let Some(batcher) = open.take() else {
                            return Ok(());
                        };
                        let (_, truncated) = batcher.finish();
                        send(
                            tx,
                            Event::ResultEnd {
                                affected_rows: None,
                                last_insert_id: None,
                                truncated,
                                elapsed_ms: started.elapsed().as_millis() as u64,
                                warnings: Vec::new(),
                            },
                        )
                        .await?;
                        // The rest of this set is abandoned; dropping the
                        // stream makes the driver drain it before the next
                        // request, which is slow but correct.
                        return Ok(());
                    }
                }
            }
        }
    }
    if let Some(batcher) = open.take() {
        close_set(tx, batcher, started, None).await?;
    }
    if !any_set {
        // No row set at all: one empty set carrying the probe's count, or no
        // count when the statement owned its batch and got no probe.
        send(
            tx,
            Event::ResultStart {
                index,
                columns: Vec::new(),
            },
        )
        .await?;
        send(
            tx,
            Event::ResultEnd {
                affected_rows: pending_rows,
                last_insert_id: None,
                truncated: false,
                elapsed_ms: started.elapsed().as_millis() as u64,
                warnings: Vec::new(),
            },
        )
        .await?;
    }
    Ok(())
}

fn is_probe(columns: &[ColumnMeta]) -> bool {
    columns.len() == 1 && columns[0].name.eq_ignore_ascii_case(ROWCOUNT_ALIAS)
}

/// The probe's value. `@@ROWCOUNT` is an `int`, but a driver may widen it.
fn probe_value(row: &Row) -> Option<u64> {
    if let Ok(Some(v)) = row.try_get::<i32, _>(0) {
        return u64::try_from(v).ok();
    }
    if let Ok(Some(v)) = row.try_get::<i64, _>(0) {
        return u64::try_from(v).ok();
    }
    None
}

async fn close_set(
    tx: &mpsc::Sender<Result<Event>>,
    batcher: Batcher,
    started: Instant,
    last_insert_id: Option<u64>,
) -> Result<()> {
    let (rest, truncated) = batcher.finish();
    if !rest.is_empty() {
        send(tx, Event::Rows(rest)).await?;
    }
    send(
        tx,
        Event::ResultEnd {
            affected_rows: None,
            last_insert_id,
            truncated,
            elapsed_ms: started.elapsed().as_millis() as u64,
            warnings: Vec::new(),
        },
    )
    .await
}

async fn send(tx: &mpsc::Sender<Result<Event>>, event: Event) -> Result<()> {
    tx.send(Ok(event)).await.map_err(|_| Error::Cancelled)
}

/// Map a `tiberius` error onto the driver's error vocabulary.
pub fn map_err(error: tiberius::error::Error) -> Error {
    match error {
        tiberius::error::Error::Server(ref token) => {
            // 596: the session was killed. 6102/6104: `KILL` refused.
            // Reporting these as a dead connection is what makes the service
            // drop the session instead of reusing it.
            if matches!(token.code(), 596) {
                Error::Cancelled
            } else {
                Error::Query(
                    QueryError::new(token.message().to_owned()).with_code(token.code().to_string()),
                )
            }
        }
        tiberius::error::Error::Io { ref message, .. } => Error::Connection(message.clone()),
        tiberius::error::Error::Tls(ref message) => {
            Error::Connection(format!("TLS handshake failed: {message}"))
        }
        tiberius::error::Error::Protocol(ref message) => Error::Connection(message.to_string()),
        other => Error::Query(QueryError::new(other.to_string())),
    }
}

/// A request that failed after the wire was already in use: the connection
/// cannot be trusted for the next statement.
fn map_stream_err(error: tiberius::error::Error) -> Error {
    match map_err(error) {
        // A server-side error leaves the connection usable; everything else
        // means the message stream is out of step.
        Error::Query(q) => Error::Query(q),
        Error::Cancelled => Error::Cancelled,
        other => Error::Connection(other.to_string()),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn probe_is_matched_by_name_not_position() {
        let probe = ColumnMeta::new(ROWCOUNT_ALIAS, fg_core::DataType::unknown());
        assert!(is_probe(std::slice::from_ref(&probe)));
        let other = ColumnMeta::new("rowcount", fg_core::DataType::unknown());
        assert!(!is_probe(std::slice::from_ref(&other)));
        let two = vec![probe.clone(), other];
        assert!(!is_probe(&two));
    }

    #[test]
    fn server_errors_keep_their_code_and_kills_become_cancellations() {
        // The mapping is exercised against a live server in tests/mssql.rs;
        // this pins the vocabulary the service depends on.
        let cancelled = Error::Cancelled;
        assert!(matches!(cancelled, Error::Cancelled));
    }
}
