//! Statement execution for PostgreSQL: split, prepare, stream through [`Batcher`].
//!
//! One `execute` call runs each statement in turn (the editor splits the same way for
//! "run current statement", so multi-statement scripts behave identically in both paths).
//! Parameters bind to a single statement only, mirroring the SQLite driver. Statements
//! with result columns stream rows through `query_raw`; statements without columns
//! (DDL, DML without `RETURNING`) report the affected count from `execute`.
//!
//! Cancellation uses the connection's cancel token on a fresh connection, which is how
//! PostgreSQL cancels: the token carries the backend pid and secret key. A statement
//! timeout wraps the whole statement — including row streaming — and fires the same
//! token on expiry, so the backend stops promptly instead of running on after the
//! caller gave up.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use fg_core::{ColumnMeta, ColumnOrigin, DbKind, Error, QueryError, Result, Value};
use fg_sql::split_statements;
use futures::StreamExt;
use tokio::sync::mpsc::Sender;
use tokio_postgres::error::ErrorPosition;
use tokio_postgres::{CancelToken, Client, Row};

use super::tls::RustlsFactory;
use super::types::{Cell, PgTextParam, data_type, to_text_param};
use crate::{Batcher, Event, ExecOptions, Push};

pub enum RunError {
    Db(Error),
    /// The consumer dropped the stream; stop quietly.
    Closed,
}

type RunResult<T> = std::result::Result<T, RunError>;

impl From<tokio_postgres::Error> for RunError {
    fn from(e: tokio_postgres::Error) -> Self {
        RunError::Db(map_err(e))
    }
}

impl From<Error> for RunError {
    fn from(e: Error) -> Self {
        RunError::Db(e)
    }
}

pub fn map_err(e: tokio_postgres::Error) -> Error {
    if let Some(db) = e.as_db_error() {
        if db.code().code() == "57014" {
            return Error::Cancelled;
        }
        return Error::Query(QueryError {
            message: db.message().to_owned(),
            code: Some(db.code().code().to_owned()),
            position: match db.position() {
                Some(ErrorPosition::Original(pos)) => usize::try_from(*pos).ok(),
                _ => None,
            },
        });
    }
    if e.is_closed() {
        return Error::Connection(e.to_string());
    }
    // Keep the underlying reason: tokio-postgres reports a failed TLS handshake
    // as a bare "error performing TLS handshake", which hides whether the
    // certificate was untrusted, expired, or for the wrong name — exactly what
    // the connection form's "test connection" needs to show.
    let mut message = e.to_string();
    let mut source = <tokio_postgres::Error as std::error::Error>::source(&e);
    while let Some(cause) = source {
        message.push_str(": ");
        message.push_str(&cause.to_string());
        source = cause.source();
    }
    Error::Query(QueryError::new(message))
}

/// How a cancel reaches the server: plaintext, or the same TLS factory the
/// connection used (a cancel opens its own connection, which must also be TLS).
#[derive(Clone)]
pub enum CancelTls {
    Plain,
    Tls(RustlsFactory),
}

/// A cancel token plus the transport to deliver it on. Cloned into the session's
/// `CancelHandle` and into statement timeouts.
#[derive(Clone)]
pub struct PgCancel {
    token: CancelToken,
    tls: CancelTls,
}

impl PgCancel {
    pub fn new(token: CancelToken, tls: CancelTls) -> Self {
        Self { token, tls }
    }

    pub async fn fire(&self) {
        match &self.tls {
            CancelTls::Plain => {
                let _ = self.token.cancel_query(tokio_postgres::NoTls).await;
            }
            CancelTls::Tls(factory) => {
                let _ = self.token.cancel_query(factory.clone()).await;
            }
        }
    }
}

async fn send(tx: &Sender<Result<Event>>, event: Event) -> RunResult<()> {
    tx.send(Ok(event)).await.map_err(|_| RunError::Closed)
}

fn elapsed_ms(started: Instant) -> u64 {
    u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX)
}

/// Resolve `(table_oid, attnum)` pairs to origins, one catalog query per distinct
/// table. A result set rarely spans tables, and only columns with a table oid are
/// looked up; expressions and aggregates stay origin-less.
async fn origins_by_oid(
    client: &Client,
    columns: &[tokio_postgres::Column],
) -> HashMap<(u32, i16), ColumnOrigin> {
    let mut oids: Vec<u32> = Vec::new();
    for column in columns {
        if let Some(oid) = column.table_oid()
            && !oids.contains(&oid)
        {
            oids.push(oid);
        }
    }
    let mut map = HashMap::new();
    for oid in oids {
        let rows = client
            .query(
                "SELECT n.nspname, c.relname, a.attname, a.attnum \
                 FROM pg_attribute a \
                 JOIN pg_class c ON c.oid = a.attrelid \
                 JOIN pg_namespace n ON n.oid = c.relnamespace \
                 WHERE a.attrelid = $1 AND a.attnum > 0",
                &[&oid],
            )
            .await;
        if let Ok(rows) = rows {
            for row in rows {
                let namespace: String = row.get(0);
                let table: String = row.get(1);
                let column: String = row.get(2);
                let attnum: i16 = row.get(3);
                map.insert(
                    (oid, attnum),
                    ColumnOrigin {
                        namespace: Some(namespace),
                        table,
                        column,
                    },
                );
            }
        }
    }
    map
}

pub async fn run(
    client: &Client,
    cancel: &PgCancel,
    sql: &str,
    params: &[Value],
    options: ExecOptions,
    tx: &Sender<Result<Event>>,
) -> RunResult<()> {
    let statements = split_statements(sql, DbKind::Postgres);
    if !params.is_empty() && statements.len() != 1 {
        return Err(RunError::Db(Error::Query(QueryError::new(
            "parameters are only supported for single statements",
        ))));
    }
    let encoded: Vec<PgTextParam> = params
        .iter()
        .map(to_text_param)
        .collect::<std::result::Result<Vec<PgTextParam>, _>>()
        .map_err(RunError::Db)?;
    let refs: Vec<&(dyn tokio_postgres::types::ToSql + Sync)> = encoded
        .iter()
        .map(|p| p as &(dyn tokio_postgres::types::ToSql + Sync))
        .collect();

    let mut index = 0usize;
    for span in &statements {
        let statement_sql = span.sql(sql);
        if statement_sql.trim().is_empty() {
            continue;
        }
        run_one(client, cancel, statement_sql, &refs, options, tx, index).await?;
        index += 1;
    }
    Ok(())
}

async fn run_one(
    client: &Client,
    cancel: &PgCancel,
    sql: &str,
    params: &[&(dyn tokio_postgres::types::ToSql + Sync)],
    options: ExecOptions,
    tx: &Sender<Result<Event>>,
    index: usize,
) -> RunResult<()> {
    let timeout = options
        .statement_timeout_secs
        .map(|s| Duration::from_secs(u64::from(s)));
    let inner = run_one_inner(client, sql, params, options, tx, index);
    let Some(timeout) = timeout else {
        return inner.await;
    };
    tokio::select! {
        result = inner => result,
        () = tokio::time::sleep(timeout) => {
            cancel.fire().await;
            Err(RunError::Db(Error::Query(QueryError::new(format!(
                "statement timed out after {}s",
                timeout.as_secs()
            )))))
        }
    }
}

async fn run_one_inner(
    client: &Client,
    sql: &str,
    params: &[&(dyn tokio_postgres::types::ToSql + Sync)],
    options: ExecOptions,
    tx: &Sender<Result<Event>>,
    index: usize,
) -> RunResult<()> {
    let started = Instant::now();
    let statement = client.prepare(sql).await?;
    let stmt_columns = statement.columns();
    let origin_map = origins_by_oid(client, stmt_columns).await;
    let columns: Vec<ColumnMeta> = stmt_columns
        .iter()
        .map(|c| {
            let mut meta = ColumnMeta::new(c.name(), data_type(c.type_()));
            if let (Some(oid), Some(attnum)) = (c.table_oid(), c.column_id()) {
                meta.origin = origin_map.get(&(oid, attnum)).cloned();
            }
            meta
        })
        .collect();
    send(tx, Event::ResultStart { index, columns }).await?;

    if statement.columns().is_empty() {
        let affected = client.execute(&statement, params).await?;
        send(
            tx,
            Event::ResultEnd {
                affected_rows: Some(affected),
                last_insert_id: None,
                truncated: false,
                elapsed_ms: elapsed_ms(started),
                warnings: Vec::new(),
            },
        )
        .await?;
        return Ok(());
    }

    let mut batcher = Batcher::new(options);
    let mut stream = std::pin::pin!(client.query_raw(&statement, params.iter().copied()).await?);
    while let Some(row) = stream.next().await {
        let row: Row = row?;
        let values = decode_row(&row)?;
        match batcher.push(values) {
            Push::Continue => {}
            Push::Flush(batch_rows) => {
                send(tx, Event::Rows(batch_rows)).await?;
            }
            Push::Stop(batch_rows) => {
                if !batch_rows.is_empty() {
                    send(tx, Event::Rows(batch_rows)).await?;
                }
                // Dropping the stream abandons the portal; tokio-postgres discards
                // the remaining rows up to CommandComplete before the next query.
                break;
            }
        }
    }
    let (rest, truncated) = batcher.finish();
    if !rest.is_empty() {
        send(tx, Event::Rows(rest)).await?;
    }
    send(
        tx,
        Event::ResultEnd {
            affected_rows: None,
            last_insert_id: None,
            truncated,
            elapsed_ms: elapsed_ms(started),
            warnings: Vec::new(),
        },
    )
    .await?;
    Ok(())
}

fn decode_row(row: &Row) -> RunResult<Vec<Value>> {
    let mut values = Vec::with_capacity(row.len());
    for i in 0..row.len() {
        let cell: Cell = row.try_get(i).map_err(map_err)?;
        values.push(cell.0);
    }
    Ok(values)
}
