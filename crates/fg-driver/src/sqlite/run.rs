use std::sync::atomic::{AtomicBool, Ordering};

use fallible_iterator::FallibleIterator;
use fg_core::{ColumnMeta, Error, QueryError, Value};
use rusqlite::{Batch, Connection, ErrorCode};
use tokio::sync::mpsc::Sender;

use super::types::{data_type, to_sql, to_value};
use crate::{Batcher, Event, ExecOptions, Push};

pub enum RunError {
    Db(Error),
    /// The consumer dropped the stream; stop quietly.
    Closed,
}

impl From<rusqlite::Error> for RunError {
    fn from(e: rusqlite::Error) -> Self {
        RunError::Db(map_err(e))
    }
}

pub fn map_err(e: rusqlite::Error) -> Error {
    match e {
        rusqlite::Error::SqliteFailure(f, msg) => failure(f, msg, None),
        rusqlite::Error::SqlInputError {
            error, msg, offset, ..
        } => failure(error, Some(msg), usize::try_from(offset).ok()),
        other => Error::Query(QueryError::new(other.to_string())),
    }
}

fn failure(f: rusqlite::ffi::Error, msg: Option<String>, position: Option<usize>) -> Error {
    if f.code == ErrorCode::OperationInterrupted {
        return Error::Cancelled;
    }
    Error::Query(QueryError {
        message: msg.unwrap_or_else(|| f.to_string()),
        code: Some(f.extended_code.to_string()),
        position,
    })
}

fn send(tx: &Sender<fg_core::Result<Event>>, event: Event) -> Result<(), RunError> {
    tx.blocking_send(Ok(event)).map_err(|_| RunError::Closed)
}

fn check_cancelled(cancelled: &AtomicBool) -> Result<(), RunError> {
    if cancelled.load(Ordering::SeqCst) {
        Err(RunError::Db(Error::Cancelled))
    } else {
        Ok(())
    }
}

pub fn run(
    conn: &Connection,
    sql: &str,
    params: &[Value],
    options: ExecOptions,
    tx: &Sender<fg_core::Result<Event>>,
    cancelled: &AtomicBool,
) -> Result<(), RunError> {
    let mut batch = Batch::new(conn, sql);
    let mut index = 0usize;
    while let Some(mut stmt) = batch.next()? {
        check_cancelled(cancelled)?;
        if index > 0 && !params.is_empty() {
            return Err(RunError::Db(Error::Query(QueryError::new(
                "parameters are only supported for single statements",
            ))));
        }
        index += 1;
        for (i, p) in params.iter().enumerate() {
            stmt.raw_bind_parameter(i + 1, to_sql(p))?;
        }

        let columns: Vec<ColumnMeta> = stmt
            .columns()
            .iter()
            .map(|c| ColumnMeta {
                name: c.name().to_owned(),
                data_type: data_type(c.decl_type()),
            })
            .collect();
        let col_count = columns.len();
        send(tx, Event::ResultStart { columns })?;

        if col_count == 0 {
            let changed = stmt.raw_execute()?;
            send(
                tx,
                Event::ResultEnd {
                    affected_rows: Some(changed as u64),
                    truncated: false,
                },
            )?;
            continue;
        }

        let mut batcher = Batcher::new(options);
        let mut rows = stmt.raw_query();
        while let Some(row) = rows.next()? {
            let values = (0..col_count)
                .map(|i| row.get_ref(i).map(to_value))
                .collect::<rusqlite::Result<Vec<_>>>()?;
            match batcher.push(values) {
                Push::Continue => {}
                Push::Flush(batch_rows) => {
                    send(tx, Event::Rows(batch_rows))?;
                    check_cancelled(cancelled)?;
                }
                Push::Stop(batch_rows) => {
                    if !batch_rows.is_empty() {
                        send(tx, Event::Rows(batch_rows))?;
                    }
                    break;
                }
            }
        }
        let (rest, truncated) = batcher.finish();
        if !rest.is_empty() {
            send(tx, Event::Rows(rest))?;
        }
        send(
            tx,
            Event::ResultEnd {
                affected_rows: None,
                truncated,
            },
        )?;
    }
    Ok(())
}
