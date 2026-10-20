use std::sync::Arc;

use fg_core::{ColumnMeta, Error, Result, Value};
use tokio::sync::mpsc;

/// One piece of an execution's output. A statement batch produces, per statement:
/// `ResultStart`, zero or more `Rows`, then `ResultEnd`. Statements that return no rows
/// (INSERT, DDL, ...) have empty `columns`.
#[derive(Debug, Clone, PartialEq)]
pub enum Event {
    ResultStart {
        columns: Vec<ColumnMeta>,
    },
    Rows(Vec<Vec<Value>>),
    ResultEnd {
        affected_rows: Option<u64>,
        /// The row limit in `ExecOptions::max_rows` cut the result short.
        truncated: bool,
    },
}

#[derive(Debug, Clone, Copy)]
pub struct ExecOptions {
    /// Rows per `Event::Rows` batch.
    pub batch_size: usize,
    /// Stop a result set after this many rows and flag it truncated.
    pub max_rows: Option<usize>,
}

impl Default for ExecOptions {
    fn default() -> Self {
        Self {
            batch_size: 500,
            max_rows: None,
        }
    }
}

/// Cancels the session's running statement. Cheap to clone, safe from any task.
#[derive(Clone)]
pub struct CancelHandle(Arc<dyn Fn() + Send + Sync>);

impl CancelHandle {
    pub fn new(f: impl Fn() + Send + Sync + 'static) -> Self {
        Self(Arc::new(f))
    }

    pub fn cancel(&self) {
        (self.0)();
    }
}

/// Receiving side of an execution. Dropping it tells the driver to stop producing.
pub struct ExecutionStream {
    rx: mpsc::Receiver<Result<Event>>,
    cancel: CancelHandle,
}

impl ExecutionStream {
    /// Channel capacity gives backpressure: drivers block when the UI falls behind.
    pub fn channel(cancel: CancelHandle) -> (mpsc::Sender<Result<Event>>, Self) {
        let (tx, rx) = mpsc::channel(4);
        (tx, Self { rx, cancel })
    }

    pub async fn next(&mut self) -> Option<Result<Event>> {
        self.rx.recv().await
    }

    pub fn cancel(&self) {
        self.cancel.cancel();
    }

    /// Drain everything into memory. For tests, MCP and small results; the grid consumes
    /// the stream incrementally instead.
    pub async fn collect(mut self) -> Result<Vec<ResultSet>> {
        let mut sets: Vec<ResultSet> = Vec::new();
        while let Some(event) = self.next().await {
            match event? {
                Event::ResultStart { columns } => sets.push(ResultSet {
                    columns,
                    rows: Vec::new(),
                    affected_rows: None,
                    truncated: false,
                }),
                Event::Rows(mut rows) => match sets.last_mut() {
                    Some(set) => set.rows.append(&mut rows),
                    None => return Err(Error::Other("rows before result start".into())),
                },
                Event::ResultEnd {
                    affected_rows,
                    truncated,
                } => {
                    if let Some(set) = sets.last_mut() {
                        set.affected_rows = affected_rows;
                        set.truncated = truncated;
                    }
                }
            }
        }
        Ok(sets)
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct ResultSet {
    pub columns: Vec<ColumnMeta>,
    pub rows: Vec<Vec<Value>>,
    pub affected_rows: Option<u64>,
    pub truncated: bool,
}

/// What the driver should do after pushing a row.
#[derive(Debug, PartialEq)]
pub enum Push {
    Continue,
    /// Send these rows as an `Event::Rows`, then keep going.
    Flush(Vec<Vec<Value>>),
    /// Row limit exceeded: send these rows (maybe empty) and stop reading the result set.
    Stop(Vec<Vec<Value>>),
}

/// Shared batching and row-limit logic so every driver behaves identically.
pub struct Batcher {
    batch_size: usize,
    max_rows: Option<usize>,
    buf: Vec<Vec<Value>>,
    count: usize,
    truncated: bool,
}

impl Batcher {
    pub fn new(options: ExecOptions) -> Self {
        Self {
            batch_size: options.batch_size.max(1),
            max_rows: options.max_rows,
            buf: Vec::new(),
            count: 0,
            truncated: false,
        }
    }

    pub fn push(&mut self, row: Vec<Value>) -> Push {
        if self.max_rows.is_some_and(|max| self.count >= max) {
            // A row beyond the limit exists, so the result really is truncated.
            self.truncated = true;
            return Push::Stop(std::mem::take(&mut self.buf));
        }
        self.count += 1;
        self.buf.push(row);
        if self.buf.len() >= self.batch_size {
            Push::Flush(std::mem::take(&mut self.buf))
        } else {
            Push::Continue
        }
    }

    /// Remaining buffered rows and whether the result was truncated.
    pub fn finish(self) -> (Vec<Vec<Value>>, bool) {
        (self.buf, self.truncated)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(i: i64) -> Vec<Value> {
        vec![Value::Int(i)]
    }

    #[test]
    fn flushes_every_batch_and_returns_the_rest() {
        let mut b = Batcher::new(ExecOptions {
            batch_size: 2,
            max_rows: None,
        });
        assert_eq!(b.push(row(1)), Push::Continue);
        assert_eq!(b.push(row(2)), Push::Flush(vec![row(1), row(2)]));
        assert_eq!(b.push(row(3)), Push::Continue);
        assert_eq!(b.finish(), (vec![row(3)], false));
    }

    #[test]
    fn truncates_only_when_an_extra_row_exists() {
        let opts = ExecOptions {
            batch_size: 10,
            max_rows: Some(2),
        };
        let mut exact = Batcher::new(opts);
        exact.push(row(1));
        exact.push(row(2));
        assert_eq!(exact.finish(), (vec![row(1), row(2)], false));

        let mut over = Batcher::new(opts);
        over.push(row(1));
        over.push(row(2));
        assert_eq!(over.push(row(3)), Push::Stop(vec![row(1), row(2)]));
        assert_eq!(over.finish(), (vec![], true));
    }
}
