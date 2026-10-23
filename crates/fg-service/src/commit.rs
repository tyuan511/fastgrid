//! Committing staged row changes (roadmap M3): all of them in one transaction, or none.

use std::sync::Arc;

use fg_core::{Error, ObjectPath, Result, SafeMode};
use fg_driver::ExecOptions;
use fg_sql::{RowChange, compile_changes};

use crate::conn::OpenConnection;

/// What a successful commit did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CommitOutcome {
    pub statements: usize,
    pub affected_rows: u64,
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
            });
        }
        let detail = self.describe(path).await?;
        let primary_key = detail.primary_key();
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
            let mut expected = primary_key.clone();
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
                        Ok::<_, Error>(sets.last().and_then(|s| s.affected_rows))
                    }
                };

                run("BEGIN".into(), Vec::new()).await?;
                let mut affected = 0;
                let mut failure = None;
                for (i, statement) in statements.iter().enumerate() {
                    match run(statement.sql.clone(), statement.params.clone()).await {
                        Ok(rows) => {
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
                    }),
                }
            })
            .await
    }
}
