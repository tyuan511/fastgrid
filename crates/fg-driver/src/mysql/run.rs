//! Statement execution for MySQL / MariaDB.
//!
//! Statements are split with `fg-sql` (like the PostgreSQL driver) and run
//! one at a time, so each statement gets its own `ResultStart` / `Rows` /
//! `ResultEnd` triple. A single statement can still produce several result
//! sets (a `CALL` returning two selects): `mysql_async`'s row stream
//! crosses set boundaries transparently, so the loop watches the current
//! result columns and closes the open set / opens a new one whenever they
//! change. `collect()` tolerates several `ResultStart`s per statement.
//!
//! Cancellation is `KILL QUERY` on a side connection (see `mod.rs`), plus a
//! flag for cancels that arrive before a statement starts — mirroring the
//! SQLite driver's flag, because a kill delivered while the connection is
//! idle has nothing to kill and must not poison the next statement: the
//! flag is cleared when each `execute` call begins.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use fg_core::{ColumnMeta, ColumnOrigin, Error, Result, Value};
use mysql_async::prelude::*;
use mysql_async::{Conn, Opts, Params, QueryResult};
use mysql_common::Value as MysqlValue;
use tokio::sync::{Mutex, mpsc};
use tokio::time::sleep;

use super::meta::map_err;
use super::types::{data_type, decode, to_param};
use crate::{Batcher, Event, ExecOptions, Push};

/// Shared cancellation state for one session.
pub struct Canceller {
    /// Set by `cancel()`; cleared when an `execute` call begins.
    pub flag: AtomicBool,
    /// Connection id of the session's connection, for `KILL QUERY`.
    pub conn_id: u32,
    /// Options for the side connection that issues the kill. No database:
    /// killing does not need one, and the session's database may be gone.
    pub kill_opts: Opts,
}

impl Canceller {
    pub fn cancel(&self) {
        self.flag.store(true, Ordering::SeqCst);
        let opts = self.kill_opts.clone();
        let id = self.conn_id;
        // The kill runs on a side connection so it never waits on the
        // session's mutex. When there is no runtime (a bare `cancel()` call
        // outside async work), the flag alone still aborts the next
        // statement; a statement already in flight cannot be killed
        // without a runtime to run the kill on.
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            handle.spawn(async move {
                if let Ok(mut conn) = Conn::new(opts).await {
                    let _ = conn.query_drop(format!("KILL QUERY {id}")).await;
                }
            });
        }
    }
}

/// Execute `sql` on the session connection, streaming events to `tx`.
/// The caller holds no lock; this takes the mutex for the whole stream.
pub async fn execute(
    conn: Arc<Mutex<Conn>>,
    canceller: Arc<Canceller>,
    sql: &str,
    params: &[Value],
    options: ExecOptions,
    tx: mpsc::Sender<Result<Event>>,
) {
    // A previous cancel must not leak into this call; a cancel issued
    // during this call sets the flag again.
    canceller.flag.store(false, Ordering::SeqCst);
    let statements = fg_sql::split_statements(sql, fg_core::DbKind::MySql);
    if !params.is_empty() && statements.len() != 1 {
        let _ = tx
            .send(Err(Error::InvalidInput(
                "parameters bind to a single statement".into(),
            )))
            .await;
        return;
    }
    let encoded: Result<Vec<MysqlValue>> = params.iter().map(to_param).collect();
    let encoded = match encoded {
        Ok(v) => v,
        Err(e) => {
            let _ = tx.send(Err(e)).await;
            return;
        }
    };
    let mut guard = conn.lock().await;
    for (index, stmt) in statements.iter().enumerate() {
        if canceller.flag.load(Ordering::SeqCst) {
            let _ = tx.send(Err(Error::Cancelled)).await;
            return;
        }
        let text = stmt.sql(sql).trim();
        if text.is_empty() {
            continue;
        }
        let started = Instant::now();
        // A timeout is a kill fired from a side task, not a dropped
        // future: the kill makes the running statement fail with 1317
        // (mapped to `Cancelled`), which keeps the connection clean for
        // the next statement. The task is aborted when the statement
        // finishes first, so a late timeout can never kill a later
        // statement on this session.
        let timeout_task = options.statement_timeout_secs.map(|secs| {
            let canceller = Arc::clone(&canceller);
            tokio::spawn(async move {
                sleep(Duration::from_secs(u64::from(secs))).await;
                canceller.cancel();
            })
        });
        let outcome = run_one(&mut guard, index, text, &encoded, options, started, &tx).await;
        if let Some(task) = timeout_task {
            task.abort();
        }
        match outcome {
            Ok(()) => {}
            Err(e) => {
                let _ = tx.send(Err(e)).await;
                return;
            }
        }
    }
}

/// Run one statement and stream its result sets. Returns `Err` on failure;
/// the caller forwards it to the stream and stops.
#[allow(clippy::too_many_arguments)]
async fn run_one(
    conn: &mut Conn,
    index: usize,
    sql: &str,
    params: &[MysqlValue],
    options: ExecOptions,
    started: Instant,
    tx: &mpsc::Sender<Result<Event>>,
) -> Result<()> {
    // Text and binary protocol results are different types, so each branch
    // hands its result to the same generic streaming loop.
    if params.is_empty() {
        let mut result = conn.query_iter(sql).await.map_err(map_err)?;
        stream_sets(&mut result, index, options, started, tx).await
    } else {
        let mut result = conn
            .exec_iter(sql, Params::Positional(params.to_vec()))
            .await
            .map_err(map_err)?;
        stream_sets(&mut result, index, options, started, tx).await
    }
}

/// Stream every result set of one statement's result. Generic over the
/// protocol because text queries and prepared statements differ in type
/// but not in row handling.
async fn stream_sets<P: mysql_async::prelude::Protocol>(
    result: &mut QueryResult<'_, '_, P>,
    index: usize,
    options: ExecOptions,
    started: Instant,
    tx: &mpsc::Sender<Result<Event>>,
) -> Result<()> {
    // `QueryResult::next` returns `None` at every result-set boundary, having
    // already advanced to the next set when one exists. So `None` closes the
    // open set but never ends the stream on its own; only `is_empty` does.
    // That matters for CALL: one statement can return several sets. The
    // first set is opened up front when it has columns, so a zero-row set
    // still gets its ResultStart/ResultEnd pair; later sets are opened on
    // their boundary for the same reason. A statement with no row set at
    // all (DDL, DML) falls through to the single empty set at the end.
    let mut open: Option<Batcher> = None;
    let mut current_cols: Option<Arc<[mysql_common::packets::Column]>> = None;
    let mut any_set = false;
    if let Some(cols) = result.columns()
        && !cols.is_empty()
    {
        open_set(tx, index, &cols).await?;
        current_cols = Some(Arc::clone(&cols));
        open = Some(Batcher::new(options));
        any_set = true;
    }
    loop {
        let Some(row) = result.next().await.map_err(map_err)? else {
            if let Some(batcher) = open.take() {
                close_set(
                    tx,
                    batcher,
                    started,
                    last_insert_id_of(result),
                    warnings_of(result),
                )
                .await?;
            }
            if result.is_empty() {
                break;
            }
            match result.columns() {
                Some(cols) if !cols.is_empty() => {
                    open_set(tx, index, &cols).await?;
                    current_cols = Some(Arc::clone(&cols));
                    open = Some(Batcher::new(options));
                    any_set = true;
                }
                Some(_) => {
                    // An OK-only set (no columns): skipped silently; the
                    // next iteration advances past it.
                    current_cols = None;
                }
                None => {
                    // No pending set while more results are claimed cannot
                    // happen after `next` advanced, so break rather than spin.
                    break;
                }
            }
            continue;
        };
        // A row implies a current set; losing it mid-stream ends the
        // stream rather than emitting a row against no columns.
        let Some(cols) = result.columns() else {
            break;
        };
        if current_cols
            .as_ref()
            .is_none_or(|prev| !Arc::ptr_eq(prev, &cols))
        {
            // A new result set begins (defensive: boundaries normally
            // surface as `None` first): close the previous one, if any.
            if let Some(batcher) = open.take() {
                close_set(
                    tx,
                    batcher,
                    started,
                    last_insert_id_of(result),
                    warnings_of(result),
                )
                .await?;
            }
            open_set(tx, index, &cols).await?;
            current_cols = Some(Arc::clone(&cols));
            open = Some(Batcher::new(options));
            any_set = true;
        }
        let Some(batcher) = open.as_mut() else {
            continue;
        };
        let values: Vec<MysqlValue> = row.unwrap();
        let decoded: Vec<Value> = cols.iter().zip(values).map(|(c, v)| decode(c, v)).collect();
        match batcher.push(decoded) {
            Push::Continue => {}
            Push::Flush(rows) => {
                send(tx, Event::Rows(rows)).await?;
            }
            Push::Stop(rows) => {
                if !rows.is_empty() {
                    send(tx, Event::Rows(rows)).await?;
                }
                // Returning here drops the result in the caller, which
                // abandons the rest of the set; the next query on this
                // connection cleans it up, and remaining rows carry no
                // errors, so the cleanup is silent.
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
                return Ok(());
            }
        }
    }
    // The loop only exits at stream end or on a defensive break; close any
    // set still open (the normal path already closed it on its boundary).
    if let Some(batcher) = open.take() {
        close_set(
            tx,
            batcher,
            started,
            last_insert_id_of(result),
            warnings_of(result),
        )
        .await?;
    }
    if !any_set {
        // No row set at all: one empty set carrying the final counts.
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
                affected_rows: Some(result.affected_rows()),
                last_insert_id: last_insert_id_of(result),
                truncated: false,
                elapsed_ms: started.elapsed().as_millis() as u64,
                warnings: warnings_of(result),
            },
        )
        .await?;
    }
    Ok(())
}

/// Emit `ResultStart` for one result set's columns, with origins.
async fn open_set(
    tx: &mpsc::Sender<Result<Event>>,
    index: usize,
    cols: &[mysql_common::packets::Column],
) -> Result<()> {
    let metas: Vec<ColumnMeta> = cols
        .iter()
        .map(|c| {
            let mut meta = ColumnMeta::new(c.name_str().into_owned(), data_type(c));
            let org_table = c.org_table_str();
            if !org_table.is_empty() {
                meta.origin = Some(ColumnOrigin {
                    namespace: {
                        let s = c.schema_str();
                        if s.is_empty() {
                            None
                        } else {
                            Some(s.into_owned())
                        }
                    },
                    table: org_table.into_owned(),
                    column: c.org_name_str().into_owned(),
                });
            }
            meta
        })
        .collect();
    send(
        tx,
        Event::ResultStart {
            index,
            columns: metas,
        },
    )
    .await
}

/// Flush a batcher's remainder and emit `ResultEnd` for its set.
/// Row-producing sets never report an affected count (the contract: columns
/// present means affected_rows is None); the OK packet's zero after a SELECT
/// is not a row count.
async fn close_set(
    tx: &mpsc::Sender<Result<Event>>,
    batcher: Batcher,
    started: Instant,
    last_insert_id: Option<u64>,
    warnings: Vec<String>,
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
            warnings,
        },
    )
    .await
}

/// The protocol reports a last-insert id of 0 when nothing was inserted;
/// the driver surface uses `None` for that.
fn last_insert_id_of<P: mysql_async::prelude::Protocol>(
    result: &QueryResult<'_, '_, P>,
) -> Option<u64> {
    result.last_insert_id().filter(|id| *id != 0)
}

fn warnings_of<P: mysql_async::prelude::Protocol>(result: &QueryResult<'_, '_, P>) -> Vec<String> {
    let n = result.warnings();
    if n == 0 {
        Vec::new()
    } else {
        vec![format!("{n} warning(s)")]
    }
}

async fn send(tx: &mpsc::Sender<Result<Event>>, event: Event) -> Result<()> {
    tx.send(Ok(event)).await.map_err(|_| Error::Cancelled)
}
