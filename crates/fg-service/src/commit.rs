//! Committing staged row changes (roadmap M3): all of them in one transaction, or none.

use std::sync::Arc;

use fg_core::{Error, ObjectPath, Result, SafeMode};
use fg_driver::ExecOptions;
use fg_sql::{RowChange, compile_changes};

use crate::conn::OpenConnection;

/// What a successful commit did.
#[derive(Debug, Clone, PartialEq)]
pub struct CommitOutcome {
    pub statements: usize,
    pub affected_rows: u64,
    /// One row per insert that read itself back, in insert order: the row as
    /// written, with defaults and generated keys filled in. Dialects with
    /// `RETURNING` read back directly; dialects without it re-select by
    /// primary key (and by the session's last insert id for a generated
    /// single-column key). An insert whose row cannot be located simply
    /// contributes no entry.
    pub returned_rows: Vec<Vec<fg_core::Value>>,
}

impl OpenConnection {
    /// Apply `changes` to `path` in a single transaction on one leased session.
    ///
    /// Refused up front, before a session is leased: a read-only connection, a table without a
    /// primary key, and any update or delete whose key is not exactly the primary key (a partial
    /// key can match many rows). Inside the transaction every `UPDATE` / `DELETE` must affect
    /// exactly one row; anything else, or any engine error, rolls the whole batch back and the
    /// error names the change (1-based) that failed.
    ///
    /// Confirming a large batch in alert/strict mode is the caller's job (the UI shows the
    /// preview); this method is the commit itself, not a second confirmation path. It writes no
    /// history row: constructed SQL is not something the user typed.
    pub async fn commit_changes(
        self: &Arc<Self>,
        path: &ObjectPath,
        changes: &[RowChange],
    ) -> Result<CommitOutcome> {
        self.pool.ensure_current()?;
        if self.safe_mode() == SafeMode::ReadOnly {
            return Err(Error::PolicyDenied(
                "this connection is read-only; changes cannot be committed".into(),
            ));
        }
        if changes.is_empty() {
            return Ok(CommitOutcome {
                statements: 0,
                affected_rows: 0,
                returned_rows: Vec::new(),
            });
        }
        let detail = self.describe(path).await?;
        // Everything the commit task needs is owned up front: the task
        // outlives this method, so borrowed column names, paths, and
        // changes cannot travel into it.
        let primary_key: Vec<String> = detail
            .primary_key()
            .into_iter()
            .map(str::to_owned)
            .collect();
        let columns: Vec<String> = detail.columns.iter().map(|c| c.name.clone()).collect();
        let table = self.dialect().qualified_name(path);
        for (i, change) in changes.iter().enumerate() {
            let key = match change {
                RowChange::Update { key, .. } | RowChange::Delete { key } => key,
                RowChange::Insert { .. } => continue,
            };
            if primary_key.is_empty() {
                return Err(Error::InvalidInput(format!(
                    "change {}: `{}` has no primary key, so its rows cannot be changed",
                    i + 1,
                    path.name
                )));
            }
            let mut given: Vec<&str> = key.iter().map(|(c, _)| c.as_str()).collect();
            let mut expected: Vec<&str> = primary_key.iter().map(String::as_str).collect();
            given.sort_unstable();
            expected.sort_unstable();
            if given != expected {
                return Err(Error::InvalidInput(format!(
                    "change {}: the key must be exactly the primary key ({})",
                    i + 1,
                    primary_key.join(", ")
                )));
            }
        }
        let statements = compile_changes(path, changes, self.dialect())?;
        let changes = changes.to_vec();

        let this = Arc::clone(self);
        self.rt
            .run(async move {
                let mut lease = this.pool.acquire().await?;
                let run = |sql: String, params: Vec<fg_core::Value>| {
                    let lease = &lease;
                    async move {
                        let stream = lease
                            .run_statement(&sql, &params, ExecOptions::default())
                            .await?;
                        let sets = stream.collect().await?;
                        let last = sets.last();
                        Ok::<_, Error>((
                            last.and_then(|s| s.affected_rows),
                            last.map(|s| s.rows.clone()).unwrap_or_default(),
                        ))
                    }
                };

                run("BEGIN".into(), Vec::new()).await?;
                let mut affected = 0;
                let mut returned_rows = Vec::new();
                let mut failure = None;
                for (i, statement) in statements.iter().enumerate() {
                    match run(statement.sql.clone(), statement.params.clone()).await {
                        Ok((rows, returned)) => {
                            if statement.returns_row {
                                // A RETURNING insert has no row count; its
                                // result set is the row as written, and there
                                // must be exactly one of it.
                                if returned.len() != 1 {
                                    failure = Some(Error::Other(format!(
                                        "change {}: expected to insert 1 row, got {}",
                                        i + 1,
                                        returned.len()
                                    )));
                                    break;
                                }
                                affected += 1;
                                returned_rows.push(returned.into_iter().next().unwrap_or_default());
                                continue;
                            }
                            let rows = rows.unwrap_or(0);
                            if let Some(want) = statement.expect_rows
                                && rows != want
                            {
                                failure = Some(Error::Other(format!(
                                    "change {}: expected to affect {want} row, affected {rows}; \
                                     the row was changed or removed since it was loaded",
                                    i + 1
                                )));
                                break;
                            }
                            affected += rows;
                            // No `RETURNING`? Re-select the inserted row by
                            // primary key, inside the same transaction, so
                            // the grid still shows defaults and generated
                            // keys. Only inserts read back; updates and
                            // deletes already know their row.
                            if let fg_sql::RowChange::Insert { cells } = &changes[i]
                                && !primary_key.is_empty()
                            {
                                // The key is the change's explicit PK values
                                // when all are present; otherwise, for a
                                // generated single-column key, the session's
                                // last insert id. A composite key with a
                                // missing value cannot be located: no
                                // readback, but the insert still stands.
                                let explicit: Option<Vec<fg_core::Value>> = primary_key
                                    .iter()
                                    .map(|pk| {
                                        cells.iter().find(|(c, _)| c.as_str() == pk.as_str()).and_then(
                                            |(_, change)| match change {
                                                fg_sql::CellChange::Set(v)
                                                    if *v != fg_core::Value::Null =>
                                                {
                                                    Some(v.clone())
                                                }
                                                _ => None,
                                            },
                                        )
                                    })
                                    .collect();
                                let key = match explicit {
                                    Some(key) => Some(key),
                                    None if primary_key.len() == 1 => {
                                        match this.dialect().last_insert_id_sql() {
                                            Some(sql) => {
                                                match run(sql.to_owned(), Vec::new()).await {
                                                    Ok((_, rows)) => rows
                                                        .into_iter()
                                                        .next()
                                                        .and_then(|r| r.into_iter().next())
                                                        .map(|id| vec![id]),
                                                    Err(e) => {
                                                        failure = Some(Error::Other(format!(
                                                            "change {}: {}",
                                                            i + 1,
                                                            e
                                                        )));
                                                        break;
                                                    }
                                                }
                                            }
                                            None => None,
                                        }
                                    }
                                    None => None,
                                };
                                if let Some(key) = key {
                                    let cols = columns
                                        .iter()
                                        .map(|c| this.dialect().quote_ident(c))
                                        .collect::<Vec<_>>()
                                        .join(", ");
                                    let where_clause = primary_key
                                        .iter()
                                        .enumerate()
                                        .map(|(j, pk)| {
                                            format!(
                                                "{} = {}",
                                                this.dialect().quote_ident(pk),
                                                this.dialect().placeholder(j + 1)
                                            )
                                        })
                                        .collect::<Vec<_>>()
                                        .join(" AND ");
                                    let sql = format!(
                                        "SELECT {cols} FROM {table} WHERE {where_clause}"
                                    );
                                    match run(sql, key).await {
                                        Ok((_, mut found)) => {
                                            if found.len() != 1 {
                                                failure = Some(Error::Other(format!(
                                                    "change {}: inserted row could not be read back",
                                                    i + 1
                                                )));
                                                break;
                                            }
                                            returned_rows.push(found.remove(0));
                                        }
                                        Err(e) => {
                                            failure = Some(Error::Other(format!(
                                                "change {}: {}",
                                                i + 1,
                                                e
                                            )));
                                            break;
                                        }
                                    }
                                }
                            }
                        }
                        Err(e) => {
                            failure = Some(match e {
                                Error::Query(q) => {
                                    Error::Other(format!("change {}: {}", i + 1, q.message))
                                }
                                other => other,
                            });
                            break;
                        }
                    }
                }
                let finish = if failure.is_some() {
                    "ROLLBACK"
                } else {
                    "COMMIT"
                };
                if let Err(e) = run(finish.into(), Vec::new()).await {
                    // A session in an unknown transaction state must not go back to the pool.
                    lease.mark_broken();
                    return Err(failure.unwrap_or(e));
                }
                match failure {
                    Some(e) => Err(e),
                    None => Ok(CommitOutcome {
                        statements: statements.len(),
                        affected_rows: affected,
                        returned_rows,
                    }),
                }
            })
            .await
    }
}
