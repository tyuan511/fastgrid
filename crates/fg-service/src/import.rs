//! CSV import: a file's rows into a table, in batched transactions.
//!
//! The mapping is positional: each [`ColumnMapping`] names the CSV
//! column index and the target column (`None` skips that CSV column).
//! Target columns with no mapping are omitted when the server can fill
//! them (a default, or an auto-increment key) or written as NULL when
//! nullable; a NOT NULL column with neither fails validation before
//! anything is written.
//!
//! Values go through [`fg_sql::coerce_text`], so a field that does not
//! parse as its target type is a row error, not a silent default.
//! [`ImportErrorPolicy::Abort`] rolls back the failing batch and stops;
//! [`ImportErrorPolicy::SkipAndRecord`] retries a failed batch row by
//! row inside the same transaction, so good rows still land and bad
//! rows are reported (capped at 1,000 entries).
//!
//! The file is read fully into memory before parsing (v1 limitation).
//! A 1M-row CSV is tens of megabytes; a streaming reader is the
//! follow-up if imports grow past that.

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use fg_core::{ColumnInfo, Dialect, Error, ObjectPath, Result, SafeMode, TableDetail, Value};
use fg_driver::{Event, ExecOptions};
use fg_sql::coerce_text;

use crate::conn::OpenConnection;
use crate::pool::Lease;
use crate::run::{PolicyRefusal, RunOptions};

/// One CSV column's destination. `column: None` skips the CSV column.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ColumnMapping {
    pub csv_index: usize,
    pub column: Option<String>,
}

/// What happens to a row that fails to convert or insert.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImportErrorPolicy {
    Abort,
    SkipAndRecord,
}

/// Options for one import. `batch_rows` is the rows per transaction.
/// `cancel`, when set, is checked between batches: a set flag stops
/// the import after the current batch commits.
#[derive(Debug, Clone)]
pub struct CsvImportOptions {
    pub has_header: bool,
    pub batch_rows: usize,
    pub error_policy: ImportErrorPolicy,
    pub cancel: Option<Arc<std::sync::atomic::AtomicBool>>,
}

impl Default for CsvImportOptions {
    fn default() -> Self {
        Self {
            has_header: true,
            batch_rows: 1000,
            error_policy: ImportErrorPolicy::Abort,
            cancel: None,
        }
    }
}

impl CsvImportOptions {
    fn cancelled(&self) -> bool {
        self.cancel
            .as_ref()
            .is_some_and(|c| c.load(std::sync::atomic::Ordering::Relaxed))
    }
}

/// What an import did. `skipped` holds the first 1,000 bad rows as
/// (data-row number, reason); `skipped_total` counts them all.
/// `cancelled` means the run stopped early: `inserted` rows are
/// committed, the rest never ran.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CsvImportReport {
    pub inserted: u64,
    pub skipped: Vec<(u64, String)>,
    pub skipped_total: u64,
    pub cancelled: bool,
}

/// A preview for the wizard: column names (the header row, or
/// `column_1…` when there is none) and the first 10 data rows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CsvPreview {
    pub columns: Vec<String>,
    pub rows: Vec<Vec<String>>,
}

/// Parse `text` for a preview: the header (or generated names) plus
/// the first 10 data rows. Only the head of the file is read.
pub fn preview_csv(text: &str, has_header: bool) -> Result<CsvPreview> {
    let mut reader = csv::ReaderBuilder::new()
        .has_headers(has_header)
        .flexible(true)
        .from_reader(text.as_bytes());
    let mut columns = if has_header {
        reader
            .headers()
            .map_err(csv_error)?
            .iter()
            .map(str::to_owned)
            .collect()
    } else {
        Vec::new()
    };
    let mut rows = Vec::new();
    for record in reader.records().take(10) {
        let record = record.map_err(csv_error)?;
        if columns.is_empty() {
            columns = (1..=record.len()).map(|i| format!("column_{i}")).collect();
        }
        rows.push(record.iter().map(str::to_owned).collect());
    }
    Ok(CsvPreview { columns, rows })
}

fn csv_error(e: csv::Error) -> Error {
    Error::InvalidInput(format!("CSV parse failed: {e}"))
}

fn invalid(msg: impl Into<String>) -> Error {
    Error::InvalidInput(msg.into())
}

impl OpenConnection {
    /// Import a CSV file into `path` under `mappings`.
    pub async fn import_csv(
        self: &Arc<Self>,
        path: &ObjectPath,
        mappings: &[ColumnMapping],
        options: &CsvImportOptions,
        file: &Path,
    ) -> Result<CsvImportReport> {
        self.pool.ensure_current()?;
        if self.safe_mode() == SafeMode::ReadOnly {
            return Err(Error::PolicyDenied(
                "this connection is read-only; import cannot write".into(),
            ));
        }
        let detail = self.describe(path).await?;
        let plan = Plan::build(&detail, mappings)?;
        let bytes = tokio::fs::read(file)
            .await
            .map_err(|e| Error::Other(format!("import read failed: {e}")))?;
        let mut reader = csv::ReaderBuilder::new()
            .has_headers(options.has_header)
            .flexible(true)
            .from_reader(&bytes[..]);
        if options.has_header {
            // Headers are positional for the import; reading them here
            // also surfaces a malformed first row early.
            reader.headers().map_err(csv_error)?;
        }
        let mut report = CsvImportReport {
            inserted: 0,
            skipped: Vec::new(),
            skipped_total: 0,
            cancelled: false,
        };
        let mut batch: Vec<Vec<Value>> = Vec::new();
        let mut batch_start: u64 = 0;
        let mut row_no: u64 = 0;
        for record in reader.records() {
            row_no += 1;
            match record {
                Ok(record) => match plan.convert(&record) {
                    Ok(row) => {
                        if batch.is_empty() {
                            batch_start = row_no;
                        }
                        batch.push(row);
                        if batch.len() >= options.batch_rows.max(1) {
                            self.import_batch(
                                path,
                                &plan,
                                &batch,
                                batch_start,
                                options,
                                &mut report,
                            )
                            .await?;
                            batch.clear();
                            if options.cancelled() {
                                report.cancelled = true;
                                return Ok(report);
                            }
                        }
                    }
                    Err(reason) => {
                        self.note_skipped(row_no, reason, options, &mut report)?;
                    }
                },
                Err(e) => {
                    self.note_skipped(
                        row_no,
                        format!("CSV parse failed: {e}"),
                        options,
                        &mut report,
                    )?;
                }
            }
        }
        if !batch.is_empty() {
            self.import_batch(path, &plan, &batch, batch_start, options, &mut report)
                .await?;
        }
        if options.cancelled() {
            report.cancelled = true;
        }
        Ok(report)
    }

    fn note_skipped(
        &self,
        row_no: u64,
        reason: String,
        options: &CsvImportOptions,
        report: &mut CsvImportReport,
    ) -> Result<()> {
        match options.error_policy {
            ImportErrorPolicy::Abort => Err(invalid(format!("row {row_no}: {reason}"))),
            ImportErrorPolicy::SkipAndRecord => {
                report.skipped_total += 1;
                if report.skipped.len() < 1000 {
                    report.skipped.push((row_no, reason));
                }
                Ok(())
            }
        }
    }

    /// One batch, one transaction. A failed batch under SkipAndRecord
    /// retries row by row inside the same transaction.
    async fn import_batch(
        self: &Arc<Self>,
        path: &ObjectPath,
        plan: &Plan,
        batch: &[Vec<Value>],
        batch_start: u64,
        options: &CsvImportOptions,
        report: &mut CsvImportReport,
    ) -> Result<()> {
        let mut lease = self.pool.acquire().await?;
        let begin = self.dialect().begin_transaction_sql();
        let commit = self.dialect().commit_sql();
        let rollback = self.dialect().rollback_sql();
        run_simple(&mut lease, begin).await?;
        let target = ImportTarget { path, plan };
        let outcome = self
            .run_batch_rows(&mut lease, &target, batch, batch_start, options, report)
            .await;
        match outcome {
            Ok(inserted) => {
                run_simple(&mut lease, commit).await?;
                report.inserted += inserted;
                Ok(())
            }
            Err(e) => {
                let _ = run_simple(&mut lease, rollback).await;
                Err(e)
            }
        }
    }

    /// Run a batch's rows on an open transaction. The fast path inserts
    /// the whole batch at once; a failure under SkipAndRecord falls
    /// back to one statement per row, recording the bad ones.
    async fn run_batch_rows(
        &self,
        lease: &mut Lease,
        target: &ImportTarget<'_>,
        batch: &[Vec<Value>],
        batch_start: u64,
        options: &CsvImportOptions,
        report: &mut CsvImportReport,
    ) -> Result<u64> {
        let dialect = self.dialect();
        let sql = insert_sql(dialect, target.path, &target.plan.columns, batch.len());
        let flat: Vec<Value> = batch.iter().flatten().cloned().collect();
        let stream = lease
            .run_statement(&sql, &flat, ExecOptions::default())
            .await?;
        if let Err(e) = stream.collect().await {
            if options.error_policy != ImportErrorPolicy::SkipAndRecord {
                return Err(e);
            }
            // Row-by-row fallback, same transaction.
            return self
                .run_batch_fallback(lease, target, batch, batch_start, report)
                .await;
        }
        Ok(batch.len() as u64)
    }

    async fn run_batch_fallback(
        &self,
        lease: &mut Lease,
        target: &ImportTarget<'_>,
        batch: &[Vec<Value>],
        batch_start: u64,
        report: &mut CsvImportReport,
    ) -> Result<u64> {
        let dialect = self.dialect();
        let single = insert_sql(dialect, target.path, &target.plan.columns, 1);
        let mut inserted = 0u64;
        for (i, row) in batch.iter().enumerate() {
            let stream = lease
                .run_statement(&single, row, ExecOptions::default())
                .await?;
            match stream.collect().await {
                Ok(_) => inserted += 1,
                Err(row_error) => {
                    report.skipped_total += 1;
                    if report.skipped.len() < 1000 {
                        report
                            .skipped
                            .push((batch_start + i as u64, row_error.to_string()));
                    }
                }
            }
        }
        Ok(inserted)
    }
}

/// Run a statement with no parameters and drain its stream.
async fn run_simple(lease: &mut Lease, sql: &str) -> Result<()> {
    let stream = lease
        .run_statement(sql, &[], ExecOptions::default())
        .await?;
    stream.collect().await?;
    Ok(())
}

/// `INSERT INTO {table} ({cols}) VALUES (...), ...` with 1-based
/// placeholders across the whole statement.
fn insert_sql(
    dialect: &dyn Dialect,
    path: &ObjectPath,
    columns: &[ColumnInfo],
    rows: usize,
) -> String {
    let width = columns.len();
    let cols = columns
        .iter()
        .map(|c| dialect.quote_ident(&c.name))
        .collect::<Vec<_>>()
        .join(", ");
    let mut sql = format!(
        "INSERT INTO {} ({cols}) VALUES ",
        dialect.qualified_name(path)
    );
    for r in 0..rows {
        if r > 0 {
            sql.push_str(", ");
        }
        sql.push('(');
        for c in 0..width {
            if c > 0 {
                sql.push_str(", ");
            }
            sql.push_str(&dialect.placeholder(r * width + c + 1));
        }
        sql.push(')');
    }
    sql
}

/// The validated destination of one import run: the table plus its
/// column plan, grouped so batch helpers stay under clippy's argument
/// limit.
struct ImportTarget<'a> {
    path: &'a ObjectPath,
    plan: &'a Plan,
}

/// The validated shape of an import: which columns the INSERT writes,
/// their types for coercion, and which positions stay NULL.
struct Plan {
    /// (target column, its CSV index) for mapped columns.
    mapped: Vec<(ColumnInfo, usize)>,
    /// Columns written as NULL on every row.
    null_columns: Vec<ColumnInfo>,
    /// All written columns, in INSERT order.
    columns: Vec<ColumnInfo>,
}

impl Plan {
    fn build(detail: &TableDetail, mappings: &[ColumnMapping]) -> Result<Self> {
        let mut mapped = Vec::new();
        let mut used: Vec<&str> = Vec::new();
        for mapping in mappings {
            let Some(name) = &mapping.column else {
                continue;
            };
            if used.contains(&name.as_str()) {
                return Err(invalid(format!("column {name} is mapped twice")));
            }
            used.push(name);
            let Some(column) = detail.columns.iter().find(|c| &c.name == name) else {
                return Err(invalid(format!("no such column {name}")));
            };
            mapped.push((column.clone(), mapping.csv_index));
        }
        let mut null_columns = Vec::new();
        for column in &detail.columns {
            if used.contains(&column.name.as_str()) {
                continue;
            }
            if column.auto_increment || column.default.is_some() {
                continue; // The server fills it: omit from the INSERT.
            }
            if column.nullable {
                null_columns.push(column.clone());
                continue;
            }
            return Err(invalid(format!(
                "column {} has no mapping and no default; map it or add a default",
                column.name
            )));
        }
        let mut columns: Vec<ColumnInfo> = mapped.iter().map(|(c, _)| c.clone()).collect();
        columns.extend(null_columns.iter().cloned());
        Ok(Self {
            mapped,
            null_columns,
            columns,
        })
    }

    fn convert(&self, record: &csv::StringRecord) -> std::result::Result<Vec<Value>, String> {
        let mut row = Vec::with_capacity(self.columns.len());
        for (column, csv_index) in &self.mapped {
            let text = record.get(*csv_index).unwrap_or("");
            row.push(coerce_text(text, &column.data_type).map_err(|e| e.to_string())?);
        }
        row.extend(self.null_columns.iter().map(|_| Value::Null));
        Ok(row)
    }
}

/// A SQL dump file, read but not yet run: how many statements it
/// holds, whether running it needs one user confirmation, and any
/// header warnings (a missing header means the file is a foreign
/// dump and runs as-is).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SqlDumpPreview {
    pub statements: usize,
    pub needs_confirmation: bool,
    pub warnings: Vec<String>,
}

/// What a SQL dump import wrote. `statements` counts fully finished
/// statements; on failure the error names the next one, and the
/// statements before it stay committed: a dump import is not one
/// transaction. `cancelled` means the run stopped after the current
/// statement, and the rest never ran.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SqlImportReport {
    pub statements: u64,
    pub warnings: Vec<String>,
    pub cancelled: bool,
}

/// Options for a SQL dump import. `cancel`, when set, is checked
/// between statements: a set flag stops the import after the
/// current statement finishes.
#[derive(Debug, Clone, Default)]
pub struct SqlImportOptions {
    pub cancel: Option<Arc<AtomicBool>>,
}

impl SqlImportOptions {
    fn cancelled(&self) -> bool {
        self.cancel
            .as_ref()
            .is_some_and(|c| c.load(Ordering::Relaxed))
    }
}

impl OpenConnection {
    /// Read a SQL dump file (gunzipping when it starts with the
    /// gzip magic) and check its fastgrid header against this
    /// connection's engine. The engine check only applies to files
    /// carrying the fastgrid header; a foreign dump without one runs
    /// as-is, with a warning.
    ///
    /// The file is read fully into memory before parsing (v1
    /// limitation, same as CSV import).
    async fn load_dump(&self, file: &Path) -> Result<(String, Vec<String>)> {
        let bytes = tokio::fs::read(file)
            .await
            .map_err(|e| Error::Other(format!("import read failed: {e}")))?;
        let text = if bytes.starts_with(&[0x1f, 0x8b]) {
            use tokio::io::AsyncReadExt as _;
            let mut decoder = async_compression::tokio::bufread::GzipDecoder::new(
                tokio::io::BufReader::new(&bytes[..]),
            );
            let mut text = String::new();
            decoder
                .read_to_string(&mut text)
                .await
                .map_err(|e| invalid(format!("dump gzip decode failed: {e}")))?;
            text
        } else {
            String::from_utf8(bytes).map_err(|_| invalid("dump is not valid UTF-8"))?
        };
        let mut warnings = Vec::new();
        let mut is_fastgrid = false;
        let mut engine: Option<String> = None;
        for line in text.lines() {
            let line = line.trim();
            if !line.starts_with("--") {
                break;
            }
            if line == "-- fastgrid dump v1" {
                is_fastgrid = true;
            }
            if let Some(rest) = line.strip_prefix("-- engine:") {
                engine = Some(rest.trim().to_owned());
            }
        }
        match (is_fastgrid, engine) {
            (true, Some(engine)) if engine != self.kind().display_name() => {
                return Err(invalid(format!(
                    "dump is for {engine}, this connection is {}",
                    self.kind().display_name()
                )));
            }
            (true, None) => {
                warnings.push("dump header has no engine line; running as-is".into());
            }
            (false, _) => {
                warnings.push("no fastgrid header; running a foreign dump as-is".into());
            }
            (true, Some(_)) => {}
        }
        Ok((text, warnings))
    }

    /// Preview a SQL dump file: read it, check its header, and
    /// classify it. A read-only connection is refused here, before
    /// anything runs; a dump that needs confirmation reports so,
    /// and the caller confirms once and reruns with `confirmed`.
    pub async fn preview_sql_dump(&self, file: &Path) -> Result<SqlDumpPreview> {
        self.pool.ensure_current()?;
        if self.safe_mode() == SafeMode::ReadOnly {
            return Err(Error::PolicyDenied(
                "this connection is read-only; import cannot write".into(),
            ));
        }
        let (text, warnings) = self.load_dump(file).await?;
        let planned = crate::run::plan(&text, self.kind(), self.safe_mode());
        if planned.statements.is_empty() {
            return Err(invalid("dump contains no statements"));
        }
        let needs_confirmation = matches!(
            planned.first_refusal(),
            Some(PolicyRefusal::NeedsConfirmation { .. })
        );
        Ok(SqlDumpPreview {
            statements: planned.statements.len(),
            needs_confirmation,
            warnings,
        })
    }

    /// Import a SQL dump file: the whole script runs through the
    /// checked run path (classification, safe mode, history and
    /// audit), so a read-only connection refuses, and Alert/Strict
    /// need one confirmation for the file, not one per statement.
    /// Pass `confirmed` after the user confirmed a preview that
    /// reported `needs_confirmation`; the confirmed run still
    /// re-plans, so a file edited between preview and run is
    /// classified again before anything executes.
    pub async fn import_sql_dump(
        self: &Arc<Self>,
        file: &Path,
        options: &SqlImportOptions,
        confirmed: bool,
    ) -> Result<SqlImportReport> {
        self.pool.ensure_current()?;
        if self.safe_mode() == SafeMode::ReadOnly {
            return Err(Error::PolicyDenied(
                "this connection is read-only; import cannot write".into(),
            ));
        }
        let (text, warnings) = self.load_dump(file).await?;
        let planned = crate::run::plan(&text, self.kind(), self.safe_mode());
        if planned.statements.is_empty() {
            return Err(invalid("dump contains no statements"));
        }
        if let Some(refusal) = planned.first_refusal() {
            match refusal {
                PolicyRefusal::ReadOnly { .. } => {
                    return Err(Error::PolicyDenied(crate::run::refusal_message(refusal)));
                }
                PolicyRefusal::NeedsConfirmation { .. } if !confirmed => {
                    return Err(Error::PolicyDenied(crate::run::refusal_message(refusal)));
                }
                _ => {}
            }
        }
        let run_options = if confirmed {
            RunOptions::gui_confirmed(&text, self.kind())
        } else {
            RunOptions::gui()
        };
        let mut stream = self.run(&text, &[], run_options).await?;
        let mut report = SqlImportReport {
            statements: 0,
            warnings,
            cancelled: false,
        };
        while let Some(event) = stream.next().await {
            let event = event.map_err(|e| {
                Error::Other(format!(
                    "import failed at statement {}: {e}",
                    report.statements + 1
                ))
            })?;
            if let Event::ResultEnd { .. } = event {
                report.statements += 1;
            }
            if options.cancelled() {
                report.cancelled = true;
                return Ok(report);
            }
        }
        if options.cancelled() {
            report.cancelled = true;
        }
        Ok(report)
    }
}
