//! Applying staged schema changes (roadmap M6), mirroring `commit.rs`.
//!
//! The UI previews first ([`OpenConnection::preview_schema_changes`], which
//! also computes the warnings the confirm bar must show), then applies
//! ([`OpenConnection::apply_schema_changes`]). Engines with transactional
//! DDL run the whole script in one transaction and roll back on any
//! failure; engines without it (MySQL) apply statement by statement, and a
//! mid-batch failure is reported as a partial outcome — the earlier
//! statements stay applied, so the caller must say exactly which ones did.

use std::sync::Arc;

use fg_core::{Error, ObjectPath, Result, SafeMode};
use fg_driver::ExecOptions;
use fg_sql::{SchemaChange, SchemaStatement, compile_schema_changes};

use crate::conn::{OpenConnection, Refresh};

/// What the preview panel shows, plus what the confirm bar must warn about.
#[derive(Debug, Clone, PartialEq)]
pub struct SchemaPreview {
    pub statements: Vec<SchemaStatement>,
    pub warnings: Vec<String>,
}

/// What applying a schema script did. `failed` is only ever set on engines
/// without transactional DDL: the statement number (1-based) that failed
/// and its error, with `applied` counting the statements that already took
/// effect. Transactional engines roll back and return `Err` instead.
#[derive(Debug, Clone, PartialEq)]
pub struct ApplySchemaOutcome {
    pub statements: usize,
    pub applied: usize,
    pub failed: Option<String>,
}

impl OpenConnection {
    /// Compile `changes` against the table as it stands, and compute the
    /// warnings for the confirm bar. Refused on a read-only connection.
    /// A batch whose first change creates the table needs no existing
    /// table to describe: the created detail is the base.
    pub async fn preview_schema_changes(
        self: &Arc<Self>,
        path: &ObjectPath,
        changes: &[SchemaChange],
    ) -> Result<SchemaPreview> {
        self.pool.ensure_current()?;
        if self.safe_mode() == SafeMode::ReadOnly {
            return Err(Error::PolicyDenied(
                "this connection is read-only; structure cannot be changed".into(),
            ));
        }
        let statements = self.compile_schema(path, changes).await?;
        let mut warnings = Vec::new();
        if changes.iter().any(|c| matches!(c, SchemaChange::DropTable)) {
            warnings.push("Dropping a table cannot be undone.".into());
        }
        if !self.capabilities().transactional_ddl {
            warnings.push(
                "DDL on this engine cannot be rolled back: if a later statement fails, \
                 earlier ones stay applied."
                    .into(),
            );
        }
        if statements
            .iter()
            .any(|s| s.description.contains("via table rebuild"))
        {
            warnings.push(
                "One or more changes rebuild the table: a new table is created, rows are \
                 copied, the old table is dropped, and the new one is renamed back. Indexes \
                 and triggers are recreated from their stored definitions."
                    .into(),
            );
        }
        Ok(SchemaPreview {
            statements,
            warnings,
        })
    }

    /// Apply `changes` to `path`, in order, on one leased session.
    pub async fn apply_schema_changes(
        self: &Arc<Self>,
        path: &ObjectPath,
        changes: &[SchemaChange],
    ) -> Result<ApplySchemaOutcome> {
        self.pool.ensure_current()?;
        if self.safe_mode() == SafeMode::ReadOnly {
            return Err(Error::PolicyDenied(
                "this connection is read-only; structure cannot be changed".into(),
            ));
        }
        if changes.is_empty() {
            return Ok(ApplySchemaOutcome {
                statements: 0,
                applied: 0,
                failed: None,
            });
        }
        let statements = self.compile_schema(path, changes).await?;
        let statements = statements.into_iter().map(|s| s.sql).collect::<Vec<_>>();
        let transactional = self.capabilities().transactional_ddl;
        let path = path.clone();

        let this = Arc::clone(self);
        let outcome = self
            .rt
            .run(async move {
                let mut lease = this.pool.acquire().await?;
                let run = |sql: String| {
                    let lease = &lease;
                    async move {
                        let stream = lease
                            .run_statement(&sql, &[], ExecOptions::default())
                            .await?;
                        stream.collect().await?;
                        Ok::<_, Error>(())
                    }
                };
                if transactional {
                    run("BEGIN".into()).await?;
                    let mut failure = None;
                    for (i, sql) in statements.iter().enumerate() {
                        if let Err(e) = run(sql.clone()).await {
                            failure = Some(match e {
                                Error::Query(q) => {
                                    format!("statement {}: {}", i + 1, q.message)
                                }
                                other => format!("statement {}: {other}", i + 1),
                            });
                            break;
                        }
                    }
                    let finish = if failure.is_some() {
                        "ROLLBACK"
                    } else {
                        "COMMIT"
                    };
                    if let Err(e) = run(finish.into()).await {
                        lease.mark_broken();
                        return Err(failure.map(Error::Other).unwrap_or(e));
                    }
                    return match failure {
                        Some(message) => Err(Error::Other(message)),
                        None => Ok(ApplySchemaOutcome {
                            statements: statements.len(),
                            applied: statements.len(),
                            failed: None,
                        }),
                    };
                }
                // No transactional DDL: apply in order and report where it
                // stopped. The caller refreshes the structure view either
                // way, because the applied prefix is real.
                let mut applied = 0;
                let mut failed = None;
                for (i, sql) in statements.iter().enumerate() {
                    match run(sql.clone()).await {
                        Ok(()) => applied += 1,
                        Err(e) => {
                            failed = Some(match e {
                                Error::Query(q) => {
                                    format!("statement {}: {}", i + 1, q.message)
                                }
                                other => format!("statement {}: {other}", i + 1),
                            });
                            break;
                        }
                    }
                }
                Ok(ApplySchemaOutcome {
                    statements: statements.len(),
                    applied,
                    failed,
                })
            })
            .await?;
        self.refresh(&Refresh::Table(path));
        Ok(outcome)
    }

    async fn compile_schema(
        self: &Arc<Self>,
        path: &ObjectPath,
        changes: &[SchemaChange],
    ) -> Result<Vec<SchemaStatement>> {
        // A batch that starts by creating the table has nothing to describe
        // yet; the created detail is its own base.
        if let Some(SchemaChange::CreateTable { detail }) = changes.first() {
            return compile_schema_changes(path, detail, changes, self.dialect());
        }
        let detail = self.describe(path).await?;
        compile_schema_changes(path, &detail, changes, self.dialect())
    }
}
