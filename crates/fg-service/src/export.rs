//! Streaming export: table results (or one read statement) to CSV, JSON,
//! or SQL, without holding rows in memory.
//!
//! Truncation is always disabled here. A truncated cell or a capped row
//! count would silently corrupt the export, so the stream runs with
//! `max_rows: None` and `max_cell_bytes: None`, and a result set that
//! still reports truncation is refused rather than written out.
//!
//! Value rendering is deliberately boring and lossless where the format
//! allows it: CSV escapes per RFC 4180, JSON keeps integers exact and
//! decimals verbatim (as strings), SQL uses the engine's `Dialect::literal`
//! (exact, never the preview-only literal from fg-sql's dml module).
//! Binary cells render as lowercase hex, matching the grid's cell text.
//! A `Value::Truncated` cannot occur with truncation disabled; if one
//! ever arrives it renders as empty (CSV) or null (JSON) rather than
//! failing a long export mid-way.

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use fg_core::{ColumnMeta, Dialect, Error, Result, StatementClass, TableQuery, Value};
use fg_driver::{Event, ExecOptions};
use fg_sql::{BuildOptions, build_select, classify, split_statements};
use tokio::io::AsyncWriteExt;

use crate::conn::OpenConnection;

/// Output format for an export.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExportFormat {
    Csv,
    Json,
    Sql,
}

/// What a table export covers. Structure output is SQL only: a CREATE
/// TABLE statement has no CSV or JSON form.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExportScope {
    Data,
    Structure,
    Both,
}

/// Options for one export. `sql_batch` is the rows per INSERT statement.
#[derive(Debug, Clone, Copy)]
pub struct ExportOptions {
    pub format: ExportFormat,
    pub scope: ExportScope,
    pub gzip: bool,
    pub sql_batch: usize,
}

impl Default for ExportOptions {
    fn default() -> Self {
        Self {
            format: ExportFormat::Csv,
            scope: ExportScope::Data,
            gzip: false,
            sql_batch: 100,
        }
    }
}

/// Live progress for an export, shared with the UI. `cancel` is checked
/// between batches; setting it aborts the export and drops the stream,
/// which cancels the running statement on the server side.
#[derive(Default)]
pub struct ExportControl {
    pub rows: AtomicU64,
    pub cancel: AtomicBool,
}

/// What an export wrote.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExportReport {
    pub rows: u64,
    pub bytes: u64,
}

fn no_truncation() -> ExecOptions {
    ExecOptions {
        batch_size: 1000,
        ..ExecOptions::default()
    }
}

fn invalid(msg: impl Into<String>) -> Error {
    Error::InvalidInput(msg.into())
}

impl OpenConnection {
    /// Export a table: the query's filters and sort apply, paging does
    /// not (the whole result streams out). `query.limit`/`offset` are
    /// ignored and replaced by an unbounded range.
    pub async fn export_table(
        self: &Arc<Self>,
        query: &TableQuery,
        options: &ExportOptions,
        control: &ExportControl,
        out: &mut (impl tokio::io::AsyncWrite + Unpin),
    ) -> Result<ExportReport> {
        if options.scope != ExportScope::Data && options.format != ExportFormat::Sql {
            return Err(invalid("structure export is only available as SQL"));
        }
        let mut report = ExportReport { rows: 0, bytes: 0 };
        if options.scope != ExportScope::Data {
            let ddl = self.ddl(&query.path).await?.ok_or_else(|| {
                Error::Unsupported("this engine does not provide DDL text".into())
            })?;
            let text = format!("{ddl};\n");
            out.write_all(text.as_bytes()).await.map_err(io_other)?;
            report.bytes += text.len() as u64;
            if options.scope == ExportScope::Structure {
                return Ok(report);
            }
        }
        let detail = self.describe(&query.path).await?;
        let mut full = query.clone();
        full.limit = i64::MAX as u64;
        full.offset = 0;
        let built = build_select(
            &full,
            self.dialect(),
            &BuildOptions {
                tie_breaker: detail
                    .primary_key()
                    .into_iter()
                    .map(str::to_owned)
                    .collect(),
                allow_raw_where: true,
            },
        )?;
        let stream = self
            .execute_unchecked(&built.sql, &built.params, no_truncation())
            .await?;
        let data = run_stream(
            stream,
            self.dialect(),
            options,
            Some(&query.path),
            control,
            out,
        )
        .await?;
        report.rows += data.rows;
        report.bytes += data.bytes;
        Ok(report)
    }

    /// Export the results of one read statement (SELECT and friends).
    /// SQL output needs a target table, so it is refused here: export a
    /// table instead.
    pub async fn export_sql(
        self: &Arc<Self>,
        sql: &str,
        options: &ExportOptions,
        control: &ExportControl,
        out: &mut (impl tokio::io::AsyncWrite + Unpin),
    ) -> Result<ExportReport> {
        if options.format == ExportFormat::Sql {
            return Err(invalid("SQL output needs a table; export a table instead"));
        }
        if options.scope != ExportScope::Data {
            return Err(invalid("structure scope needs a table export"));
        }
        let statements = split_statements(sql, self.kind());
        if statements.len() != 1 {
            return Err(invalid("export runs exactly one statement"));
        }
        if classify(sql, self.kind()).class != StatementClass::Read {
            return Err(invalid("only read statements can be exported"));
        }
        let stream = self.execute_unchecked(sql, &[], no_truncation()).await?;
        run_stream(stream, self.dialect(), options, None, control, out).await
    }

    /// [`OpenConnection::export_table`] writing to a file, gzipped when
    /// `options.gzip` is set.
    pub async fn export_table_to_file(
        self: &Arc<Self>,
        query: &TableQuery,
        options: &ExportOptions,
        control: &ExportControl,
        path: &Path,
    ) -> Result<ExportReport> {
        let mut file = tokio::fs::File::create(path).await.map_err(io_other)?;
        if options.gzip {
            let mut encoder = async_compression::tokio::write::GzipEncoder::new(file);
            let report = self
                .export_table(query, options, control, &mut encoder)
                .await;
            if report.is_ok() {
                encoder.shutdown().await.map_err(io_other)?;
            } else {
                let _ = encoder.shutdown().await;
            }
            report
        } else {
            self.export_table(query, options, control, &mut file).await
        }
    }

    /// [`OpenConnection::export_sql`] writing to a file, gzipped when
    /// `options.gzip` is set.
    pub async fn export_sql_to_file(
        self: &Arc<Self>,
        sql: &str,
        options: &ExportOptions,
        control: &ExportControl,
        path: &Path,
    ) -> Result<ExportReport> {
        let mut file = tokio::fs::File::create(path).await.map_err(io_other)?;
        if options.gzip {
            let mut encoder = async_compression::tokio::write::GzipEncoder::new(file);
            let report = self.export_sql(sql, options, control, &mut encoder).await;
            if report.is_ok() {
                encoder.shutdown().await.map_err(io_other)?;
            } else {
                let _ = encoder.shutdown().await;
            }
            report
        } else {
            self.export_sql(sql, options, control, &mut file).await
        }
    }
}

fn io_other(e: std::io::Error) -> Error {
    Error::Other(format!("export I/O failed: {e}"))
}

/// Consume a stream into `out`. Drains to `None`: dropping the stream
/// early would cancel the statement, so a successful export always
/// reads to the end.
async fn run_stream(
    mut stream: crate::task::TaskStream,
    dialect: &dyn Dialect,
    options: &ExportOptions,
    table: Option<&fg_core::ObjectPath>,
    control: &ExportControl,
    out: &mut (impl tokio::io::AsyncWrite + Unpin),
) -> Result<ExportReport> {
    let mut writer = Writer {
        out,
        bytes: 0,
        started: false,
        started_json_row: false,
        columns: Vec::new(),
        sql_rows: Vec::new(),
    };
    let mut rows: u64 = 0;
    while let Some(event) = stream.next().await {
        let event = event?;
        match event {
            Event::ResultStart { columns, .. } => {
                writer.columns = columns;
                match options.format {
                    ExportFormat::Csv => {
                        if !writer.started {
                            writer.started = true;
                            writer.write_csv_header().await?;
                        }
                    }
                    ExportFormat::Json => {
                        if !writer.started {
                            writer.started = true;
                            writer.write_str("[").await?;
                        }
                    }
                    ExportFormat::Sql => {}
                }
            }
            Event::Rows(batch) => {
                // One write per batch, not per row: 10M rows at one
                // syscall each would dominate the whole export.
                let mut text = String::new();
                for row in batch {
                    match options.format {
                        ExportFormat::Csv => writer.push_csv_row(&row, &mut text),
                        ExportFormat::Json => writer.push_json_row(&row, &mut text),
                        ExportFormat::Sql => {
                            writer.sql_rows.push(row);
                            if writer.sql_rows.len() >= options.sql_batch.max(1) {
                                writer.flush_sql(dialect, table).await?;
                            }
                        }
                    }
                    rows += 1;
                    control.rows.store(rows, Ordering::Relaxed);
                }
                if !text.is_empty() {
                    writer.write_str(&text).await?;
                }
                if control.cancel.load(Ordering::Relaxed) {
                    return Err(Error::Cancelled);
                }
            }
            Event::ResultEnd { truncated, .. } => {
                if truncated {
                    return Err(Error::Other(
                        "export stream reported truncation; refusing partial output".into(),
                    ));
                }
            }
            Event::Notice(_) => {}
            _ => {}
        }
    }
    match options.format {
        ExportFormat::Json => {
            if writer.started {
                writer.write_str("]").await?;
            } else {
                writer.write_str("[]").await?;
            }
        }
        ExportFormat::Sql => writer.flush_sql(dialect, table).await?,
        ExportFormat::Csv => {}
    }
    Ok(ExportReport {
        rows,
        bytes: writer.bytes,
    })
}

struct Writer<'a, W> {
    out: &'a mut W,
    bytes: u64,
    /// A header / opening bracket was written (CSV header, JSON `[`).
    started: bool,
    /// A JSON row was written, so the next one needs a comma.
    started_json_row: bool,
    columns: Vec<ColumnMeta>,
    sql_rows: Vec<Vec<Value>>,
}

impl<W: tokio::io::AsyncWrite + Unpin> Writer<'_, W> {
    async fn write_str(&mut self, text: &str) -> Result<()> {
        self.out
            .write_all(text.as_bytes())
            .await
            .map_err(io_other)?;
        self.bytes += text.len() as u64;
        Ok(())
    }

    async fn write_csv_header(&mut self) -> Result<()> {
        let line = self
            .columns
            .iter()
            .map(|c| csv_cell(&Value::Text(c.name.clone())))
            .collect::<Vec<_>>()
            .join(",");
        self.write_str(&line).await?;
        self.write_str("\n").await
    }

    fn push_csv_row(&self, row: &[Value], out: &mut String) {
        out.push_str(&row.iter().map(csv_cell).collect::<Vec<_>>().join(","));
        out.push('\n');
    }

    fn push_json_row(&mut self, row: &[Value], out: &mut String) {
        if self.started_json_row {
            out.push(',');
        }
        self.started_json_row = true;
        out.push('{');
        for (i, (column, value)) in self.columns.iter().zip(row.iter()).enumerate() {
            if i > 0 {
                out.push(',');
            }
            let key = serde_json::to_string(&column.name)
                .unwrap_or_else(|_| format!("\"{}\"", column.name));
            out.push_str(&key);
            out.push(':');
            out.push_str(&serde_json::to_string(&json_value(value)).unwrap_or_default());
        }
        out.push('}');
    }

    async fn flush_sql(
        &mut self,
        dialect: &dyn Dialect,
        table: Option<&fg_core::ObjectPath>,
    ) -> Result<()> {
        if self.sql_rows.is_empty() {
            return Ok(());
        }
        let Some(table) = table else {
            return Err(invalid("SQL output needs a table"));
        };
        let columns = self
            .columns
            .iter()
            .map(|c| dialect.quote_ident(&c.name))
            .collect::<Vec<_>>()
            .join(", ");
        let mut statement = format!(
            "INSERT INTO {} ({columns}) VALUES ",
            dialect.qualified_name(table)
        );
        for (i, row) in self.sql_rows.iter().enumerate() {
            if i > 0 {
                statement.push_str(", ");
            }
            statement.push('(');
            for (j, value) in row.iter().enumerate() {
                if j > 0 {
                    statement.push_str(", ");
                }
                statement.push_str(&dialect.literal(value));
            }
            statement.push(')');
        }
        statement.push_str(";\n");
        self.sql_rows.clear();
        self.write_str(&statement).await
    }
}

/// One CSV cell, RFC 4180: quote when the text holds a comma, quote,
/// newline, or carriage return, doubling inner quotes.
fn csv_cell(value: &Value) -> String {
    let text = match value {
        Value::Null => return String::new(),
        Value::Bool(b) => return b.to_string(),
        Value::Int(i) => return i.to_string(),
        Value::UInt(u) => return u.to_string(),
        Value::Float(f) => {
            if f.is_finite() {
                return f.to_string();
            }
            return String::new();
        }
        Value::Decimal(s) => s.clone(),
        Value::Text(s) => s.clone(),
        Value::Bytes(b) => hex_bytes(b),
        Value::Json(s) => s.clone(),
        Value::Uuid(s)
        | Value::Date(s)
        | Value::Time(s)
        | Value::Timestamp(s)
        | Value::TimestampTz(s)
        | Value::Interval(s) => s.clone(),
        Value::Array(items) => {
            serde_json::to_string(&json_value(&Value::Array(items.clone()))).unwrap_or_default()
        }
        // Unreachable with truncation disabled; never fail an export over it.
        Value::Truncated(_) => return String::new(),
        Value::Other { text, .. } => text.clone(),
        _ => return String::new(),
    };
    if text.contains([',', '"', '\n', '\r']) {
        format!("\"{}\"", text.replace('"', "\"\""))
    } else {
        text
    }
}

fn hex_bytes(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push_str(&format!("{b:02x}"));
    }
    out
}

/// JSON form of a value. Decimals stay verbatim strings (exactness over
/// numeric typing); embedded JSON text is re-emitted as JSON when it
/// parses, and as a plain string when it does not.
fn json_value(value: &Value) -> serde_json::Value {
    use serde_json::Value as Json;
    match value {
        Value::Null => Json::Null,
        Value::Bool(b) => Json::Bool(*b),
        Value::Int(i) => Json::Number((*i).into()),
        Value::UInt(u) => Json::Number((*u).into()),
        Value::Float(f) => serde_json::Number::from_f64(*f)
            .map(Json::Number)
            .unwrap_or(Json::Null),
        Value::Decimal(s) => Json::String(s.clone()),
        Value::Text(s) => Json::String(s.clone()),
        Value::Bytes(b) => Json::String(hex_bytes(b)),
        Value::Json(s) => serde_json::from_str(s).unwrap_or_else(|_| Json::String(s.clone())),
        Value::Uuid(s)
        | Value::Date(s)
        | Value::Time(s)
        | Value::Timestamp(s)
        | Value::TimestampTz(s)
        | Value::Interval(s) => Json::String(s.clone()),
        Value::Array(items) => Json::Array(items.iter().map(json_value).collect()),
        // Unreachable with truncation disabled; never fail an export over it.
        Value::Truncated(_) => Json::Null,
        Value::Other { text, .. } => Json::String(text.clone()),
        _ => Json::Null,
    }
}
