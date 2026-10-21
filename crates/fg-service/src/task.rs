//! A running statement: its result stream, the session lease it holds, and a cancel handle that
//! can only ever affect *this* task.
//!
//! The driver's cancel is session-wide ("interrupt whatever this session is doing"). If a cancel
//! arrived late, it could kill the next task that reused the session - the bug this module exists
//! to prevent. Two rules make that impossible:
//! * the session stays leased until the task is finished, and
//! * a cancel is forwarded to the driver only while the task is still `Running`, under the same
//!   lock `finish` takes before releasing the lease.

use std::sync::{Arc, Mutex};

use fg_core::{Error, Result};
use fg_driver::{CancelHandle, Event, ExecutionStream, ResultSet};

use crate::pool::Lease;

#[derive(Clone, Copy, PartialEq, Eq)]
enum State {
    Running,
    Finished,
}

struct Control {
    state: Mutex<State>,
    driver_cancel: CancelHandle,
}

/// Cancels one task. Cheap to clone; safe to call from any thread, any number of times, before
/// or after the task ended.
#[derive(Clone)]
pub struct TaskCanceller(Arc<Control>);

impl TaskCanceller {
    /// A canceller that does nothing. Used when a stream has no driver task yet.
    pub(crate) fn noop() -> Self {
        Self(Arc::new(Control {
            state: Mutex::new(State::Finished),
            driver_cancel: CancelHandle::new(|| {}),
        }))
    }

    pub fn cancel(&self) {
        if let Ok(state) = self.0.state.lock()
            && *state == State::Running
        {
            self.0.driver_cancel.cancel();
        }
    }
}

pub struct TaskStream {
    stream: ExecutionStream,
    lease: Option<Lease>,
    control: Arc<Control>,
}

impl TaskStream {
    pub(crate) fn new(stream: ExecutionStream, lease: Lease) -> Result<Self> {
        let control = Arc::new(Control {
            state: Mutex::new(State::Running),
            driver_cancel: lease.session()?.cancel_handle(),
        });
        Ok(Self {
            stream,
            lease: Some(lease),
            control,
        })
    }

    pub fn canceller(&self) -> TaskCanceller {
        TaskCanceller(Arc::clone(&self.control))
    }

    pub fn cancel(&self) {
        self.canceller().cancel();
    }

    /// Next piece of output. `None` once the statement(s) finished; the session is then free.
    pub async fn next(&mut self) -> Option<Result<Event>> {
        let item = self.stream.next().await;
        match &item {
            None => self.finish(false),
            Some(Err(e)) => {
                if matches!(e, Error::Connection(_))
                    && let Some(lease) = self.lease.as_mut()
                {
                    lease.mark_broken();
                }
                self.finish(false);
            }
            Some(Ok(_)) => {}
        }
        item
    }

    /// Read everything into memory (small results, tests, MCP).
    pub async fn collect(mut self) -> Result<Vec<ResultSet>> {
        let mut sets: Vec<ResultSet> = Vec::new();
        while let Some(event) = self.next().await {
            match event? {
                Event::ResultStart { columns, .. } => sets.push(ResultSet {
                    columns,
                    rows: Vec::new(),
                    affected_rows: None,
                    last_insert_id: None,
                    truncated: false,
                    elapsed_ms: 0,
                    warnings: Vec::new(),
                }),
                Event::Rows(mut rows) => {
                    if let Some(set) = sets.last_mut() {
                        set.rows.append(&mut rows);
                    }
                }
                Event::ResultEnd {
                    affected_rows,
                    last_insert_id,
                    truncated,
                    elapsed_ms,
                    warnings,
                } => {
                    if let Some(set) = sets.last_mut() {
                        set.affected_rows = affected_rows;
                        set.last_insert_id = last_insert_id;
                        set.truncated = truncated;
                        set.elapsed_ms = elapsed_ms;
                        set.warnings = warnings;
                    }
                }
                _ => {}
            }
        }
        Ok(sets)
    }

    /// Mark finished and give the session back. If the consumer walked away before the end
    /// (`abandoned`), the statement is cancelled first so it does not keep running unseen.
    fn finish(&mut self, abandoned: bool) {
        if let Ok(mut state) = self.control.state.lock()
            && *state == State::Running
        {
            if abandoned {
                self.control.driver_cancel.cancel();
            }
            *state = State::Finished;
        }
        self.lease = None;
    }
}

impl Drop for TaskStream {
    fn drop(&mut self) {
        let unfinished = self.lease.is_some();
        self.finish(unfinished);
    }
}
