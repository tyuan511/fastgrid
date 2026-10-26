//! Policy gate for arbitrary SQL.
//!
//! [`plan`] is pure so the UI can decide whether to show a confirm dialog before anything is
//! leased. [`OpenConnection::run`] is the only path that both enforces that plan and records
//! every attempt. A refusal returns [`Error::PolicyDenied`] without acquiring a session.
//! [`OpenConnection::execute`] applies the same classification and cannot carry a confirmation.
//!
//! Recording goes through one process-lifetime writer thread. Dropping a stream only notifies
//! that writer; it does not spawn, and it does not need the tokio runtime to still be alive.
//!
//! Waiting for a record ack must not need a reactor on the caller's thread either: the wait runs on
//! the service runtime's blocking pool, because this crate's public methods are allowed to be
//! awaited from a foreign executor such as GPUI's. See [`await_record`].

use std::ops::Range;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use fg_core::{
    Actor, AuditEntry, AuditId, ConnectionId, DangerReason, DbKind, Decision, Error, ExecSettings,
    HistoryId, Outcome, QueryHistoryEntry, QuerySource, Result, SafeMode, StatementClass,
    UnixMillis, Value,
};
use fg_driver::{Event, ExecOptions};
use fg_sql::analyze;
use fg_store::Store;
use tokio::runtime::Handle;
use tokio::sync::{Notify, mpsc as async_mpsc, oneshot};
use tokio::task::JoinHandle;

use crate::ServiceEvent;
use crate::conn::{OpenConnection, SharedRuntime};
use crate::lock_error;
use crate::mcp::{MCP_WRITE_APPROVAL_TIMEOUT, McpWriteStatement};
use crate::task::TaskStream;

/// Per-call knobs for [`OpenConnection::run`].
///
/// Construct this with [`RunOptions::gui`], [`RunOptions::gui_confirmed`] or [`RunOptions::agent`].
/// The agent constructor cannot carry a confirmation: approval is a UI capability, not a claim an
/// external caller can make. Fields are private so a caller cannot flip that after the fact.
///
/// Trust boundary, until M2.5 plugs a real caller identity into the transport: only the GUI may
/// call [`RunOptions::gui_confirmed`], and only the MCP/CLI transport may call [`RunOptions::agent`].
/// A process that can build either constructor can still lie. The transport must not expose the
/// GUI constructor to an agent.
#[derive(Clone, PartialEq, Eq)]
pub struct RunOptions {
    kind: RunKind,
    max_rows: Option<u64>,
    statement_timeout_secs: Option<u32>,
}

#[derive(Clone, PartialEq, Eq)]
enum RunKind {
    /// The person at the keyboard. Audited only when it is not the editor source below.
    Gui { approval: Option<ScriptDigest> },
    /// An external agent. Always audited under [`Actor::Mcp`] with this client id.
    Agent { client: String },
}

/// Fingerprint of the exact script a person confirmed. Not a secret; it only stops an approval
/// from being replayed onto different SQL.
#[derive(Clone, PartialEq, Eq, Debug)]
struct ScriptDigest(u64);

impl std::fmt::Debug for RunOptions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RunOptions")
            .field("max_rows", &self.max_rows)
            .field("statement_timeout_secs", &self.statement_timeout_secs)
            .field("confirmed", &self.kind_is_confirmed())
            .finish_non_exhaustive()
    }
}

impl Default for RunOptions {
    fn default() -> Self {
        Self::gui()
    }
}

impl RunOptions {
    /// Editor run with no confirmation. Dangerous statements are refused in alert and strict mode.
    pub fn gui() -> Self {
        Self {
            kind: RunKind::Gui { approval: None },
            max_rows: None,
            statement_timeout_secs: None,
        }
    }

    /// Editor run after a person confirmed this exact script.
    ///
    /// The digest is of the script text and its planned statements. Passing the options to `run`
    /// with different SQL is refused. Password re-authentication is not part of this value: the
    /// controller verifies the password before building these options, so by the time a confirmed
    /// run is constructed the password step has already passed.
    pub fn gui_confirmed(sql: &str, kind: DbKind) -> Self {
        Self {
            kind: RunKind::Gui {
                approval: Some(script_digest(sql, kind)),
            },
            max_rows: None,
            statement_timeout_secs: None,
        }
    }

    /// Agent run. Cannot carry a confirmation: approval for a write
    /// comes from the service's pending-write registry, granted by a
    /// person in the GUI, never claimed by the caller. `client` is the
    /// audit actor, not a fixed label.
    ///
    /// An empty client id is rejected when the run starts, so a missing identity cannot hide in
    /// a row tagged `"mcp"`.
    pub fn agent(client: impl Into<String>) -> Self {
        Self {
            kind: RunKind::Agent {
                client: client.into(),
            },
            max_rows: None,
            statement_timeout_secs: None,
        }
    }

    /// Per-call row cap. `None` adds no cap; it does not clear the stored one. The effective cap
    /// is the minimum of the `Some` values.
    pub fn with_max_rows(mut self, max_rows: Option<u64>) -> Self {
        self.max_rows = max_rows;
        self
    }

    /// Per-call statement timeout in seconds. Merged with [`ExecSettings`] and the connection's
    /// `timeouts.statement_secs`; the strictest `Some` wins.
    pub fn with_statement_timeout_secs(mut self, secs: Option<u32>) -> Self {
        self.statement_timeout_secs = secs;
        self
    }

    fn kind_is_confirmed(&self) -> bool {
        matches!(self.kind, RunKind::Gui { approval: Some(_) })
    }

    fn source(&self) -> QuerySource {
        match self.kind {
            RunKind::Gui { .. } => QuerySource::Editor,
            RunKind::Agent { .. } => QuerySource::Mcp,
        }
    }
}

/// One statement of a script, with the offsets the editor needs to highlight it.
#[derive(Clone, PartialEq, Eq)]
pub struct PlannedStatement {
    /// Statement text, without the terminating `;`.
    pub sql: String,
    /// Byte range of `sql` inside the original script. Leading comments belong to the statement,
    /// so a highlight lines up with what the user sees.
    pub offset: Range<usize>,
    /// Coarse class from `fg-sql`. Unknown counts as a write.
    pub class: StatementClass,
    /// Why alert mode asks, when this statement is dangerous. Strict mode asks for every write
    /// even when this is `None`.
    pub danger: Option<DangerReason>,
    /// Alert/strict: the UI should ask before running. A read-only refusal is not a confirmation.
    pub needs_confirmation: bool,
}

impl std::fmt::Debug for PlannedStatement {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // SQL text stays out of Debug so a log of a plan cannot capture the statement.
        f.debug_struct("PlannedStatement")
            .field("class", &self.class)
            .field("danger", &self.danger)
            .field("needs_confirmation", &self.needs_confirmation)
            .field("offset", &self.offset)
            .field("sql", &"<redacted>")
            .finish()
    }
}

/// Why [`plan`] will not let a script run as-is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PolicyRefusal {
    /// A statement may write and the connection is read-only. Confirmation cannot override this.
    ReadOnly {
        statement: usize,
        class: StatementClass,
    },
    /// Alert (dangerous only) or strict (every write). A confirmed re-run of the same script may
    /// proceed. In Strict mode the GUI additionally asks for the connection password before that
    /// re-run when one is stored.
    NeedsConfirmation {
        statement: usize,
        class: StatementClass,
        danger: Option<DangerReason>,
    },
}

/// The classification of a script, plus the first reason it cannot run without a decision.
#[derive(Clone, PartialEq, Eq)]
pub struct RunPlan {
    /// Every statement, in order, even when an earlier one is already a refusal.
    pub statements: Vec<PlannedStatement>,
    /// First hard refusal or confirmation demand, if any. Later statements are still listed so
    /// the editor can mark every one of them.
    pub refusal: Option<PolicyRefusal>,
}

impl std::fmt::Debug for RunPlan {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RunPlan")
            .field("statements", &self.statements.len())
            .field("refusal", &self.refusal)
            .finish_non_exhaustive()
    }
}

impl RunPlan {
    /// True when the UI should show a confirm dialog. A read-only refusal is not this: it cannot
    /// be confirmed, so callers must also check [`Self::first_refusal`].
    pub fn requires_confirmation(&self) -> bool {
        self.statements.iter().any(|s| s.needs_confirmation)
    }

    /// The refusal `run` will enforce, if the caller has not confirmed a confirmation demand.
    pub fn first_refusal(&self) -> Option<PolicyRefusal> {
        self.refusal
    }
}

/// Classify `sql` for `safe_mode` without IO.
///
/// Splitting comes from `fg-sql`, so strings, comments, dollar quotes and trigger bodies stay
/// one statement. An unknown safe mode collapses to read-only. A whitespace-only script has no
/// statements and no refusal: there is nothing to run and nothing to refuse.
///
/// Strict is stronger than alert: every statement whose class may write needs confirmation, not
/// only dangerous ones. Password re-authentication is not implemented yet; confirmation is the
/// gate that stands in for it. Read-only is stronger still, because confirmation cannot override
/// it. Permissiveness is Normal > Alert > Strict > ReadOnly.
pub fn plan(sql: &str, kind: DbKind, safe_mode: SafeMode) -> RunPlan {
    let mode = safe_mode.effective();
    let mut statements = Vec::new();
    let mut refusal = None;
    for (index, (span, info)) in analyze(sql, kind).statements.into_iter().enumerate() {
        let text = span.sql(sql);
        // Pure-read utilities (PRAGMA table_info) are not writes and do not mutate persistent
        // state. Read-only still refuses assignment pragmas, SET, and anything dangerous.
        let pure_read = matches!(
            info.class,
            StatementClass::Read | StatementClass::Transaction
        ) || (info.class == StatementClass::Utility
            && crate::pool::utility_is_pure_read(text, kind)
            && info.danger.is_none());
        let needs_confirmation = match mode {
            SafeMode::Alert => info.danger.is_some(),
            SafeMode::Strict => info.class.may_write() && !pure_read,
            _ => false,
        };
        if refusal.is_none() {
            if mode == SafeMode::ReadOnly && !pure_read && info.class.may_write() {
                refusal = Some(PolicyRefusal::ReadOnly {
                    statement: index,
                    class: info.class,
                });
            } else if needs_confirmation {
                refusal = Some(PolicyRefusal::NeedsConfirmation {
                    statement: index,
                    class: info.class,
                    danger: info.danger,
                });
            }
        }
        statements.push(PlannedStatement {
            sql: span.sql(sql).to_owned(),
            offset: span.text,
            class: info.class,
            danger: info.danger,
            needs_confirmation,
        });
    }
    RunPlan {
        statements,
        refusal,
    }
}

/// A refusal that blocks this call. A confirmed GUI run may pass a confirmation demand; nothing
/// passes a read-only refusal.
pub(crate) fn blocking_refusal(plan: &RunPlan, confirmed: bool) -> Option<PolicyRefusal> {
    plan.first_refusal().filter(|refusal| match refusal {
        PolicyRefusal::ReadOnly { .. } => true,
        PolicyRefusal::NeedsConfirmation { .. } => !confirmed,
    })
}

pub(crate) fn refusal_message(refusal: PolicyRefusal) -> String {
    match refusal {
        PolicyRefusal::ReadOnly { statement, class } => format!(
            "statement {statement} is classified {class:?} and this connection is read-only"
        ),
        PolicyRefusal::NeedsConfirmation {
            statement,
            class,
            danger,
        } => match danger {
            Some(danger) => format!("statement {statement} needs confirmation ({danger:?})"),
            None => format!("statement {statement} is classified {class:?} and needs confirmation"),
        },
    }
}

/// Effective cap. `None` means unlimited from that source, so any `Some` wins, and two `Some`
/// values take the minimum. A caller cannot loosen a stored cap.
pub(crate) fn stricter<T: Ord>(values: impl IntoIterator<Item = Option<T>>) -> Option<T> {
    values.into_iter().flatten().min()
}

fn rows_cap(explicit: Option<u64>, settings: Option<u64>) -> Option<usize> {
    // A cap that does not fit `usize` cannot be passed to the driver. Saturating keeps the
    // result bounded on 32-bit instead of failing a legitimate "as large as possible" setting.
    stricter([explicit, settings]).map(|n| usize::try_from(n).unwrap_or(usize::MAX))
}

/// Effective statement timeout. Strictest of the per-call option, [`ExecSettings`], and
/// [`fg_core::Timeouts::statement_secs`]. `None` means unlimited from that source. When the
/// engine's `Capabilities::statement_timeout` is false, `run` arms its own per-statement timer
/// and cancels that statement; the budget resets at each `ResultStart` and does not accumulate
/// across a script, and time spent waiting for the consumer is not counted. The driver field is
/// not trusted to do it. Engines that enforce the timeout themselves still receive one value for
/// the script; SQLite is not one of them.
pub(crate) fn effective_statement_timeout(
    call: Option<u32>,
    settings: Option<u32>,
    connection: Option<u32>,
) -> Option<u32> {
    stricter([call, settings, connection])
}

fn elapsed_ms(started: Instant) -> u64 {
    u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX)
}

fn script_digest(sql: &str, kind: DbKind) -> ScriptDigest {
    // FNV-1a over the exact script and each planned statement. Stable for the life of the
    // process and independent of hasher randomization, so a confirmation cannot be replayed
    // onto different SQL. It is not a password.
    let mut hash = 0xcbf29ce484222325u64;
    let mix = |hash: &mut u64, bytes: &[u8]| {
        for byte in bytes {
            *hash ^= u64::from(*byte);
            *hash = hash.wrapping_mul(0x100000001b3);
        }
        *hash ^= 0xff;
    };
    mix(&mut hash, sql.as_bytes());
    let planned = plan(sql, kind, SafeMode::Normal);
    for stmt in &planned.statements {
        mix(&mut hash, stmt.sql.as_bytes());
    }
    hash ^= planned.statements.len() as u64;
    ScriptDigest(hash)
}

fn actor_for(options: &RunOptions) -> Actor {
    match &options.kind {
        RunKind::Agent { client } => Actor::Mcp {
            client: client.clone(),
        },
        RunKind::Gui { .. } => Actor::User,
    }
}

fn decision_for(outcome: Outcome, confirmed: bool) -> Decision {
    if outcome == Outcome::NotRun {
        Decision::Denied
    } else if confirmed {
        Decision::Confirmed
    } else {
        Decision::Allowed
    }
}

/// What an audit row should describe: the refused statement, else the first write, else the first
/// statement. One run is one audit row, matching the one history row.
fn subject(plan: &RunPlan) -> (StatementClass, Option<DangerReason>) {
    if let Some(refusal) = plan.refusal {
        let index = match refusal {
            PolicyRefusal::ReadOnly { statement, .. }
            | PolicyRefusal::NeedsConfirmation { statement, .. } => statement,
        };
        if let Some(stmt) = plan.statements.get(index) {
            return (stmt.class, stmt.danger);
        }
    }
    let class = plan
        .statements
        .iter()
        .find(|s| s.class.may_write())
        .or_else(|| plan.statements.first())
        .map(|s| s.class)
        .unwrap_or(StatementClass::Unknown);
    let danger = plan.statements.iter().find_map(|s| s.danger);
    (class, danger)
}

const TIMEOUT_REASON: &str = "statement exceeded the configured timeout";

struct RecordJob {
    store: Arc<Mutex<Store>>,
    history: QueryHistoryEntry,
    audit: Option<AuditEntry>,
    keep: usize,
    fail_prune: bool,
    fail_history: bool,
}

impl RecordJob {
    fn write(self) -> Result<()> {
        let guard = self.store.lock().map_err(|_| lock_error())?;
        // History and audit are both mandatory. A history failure must not skip the audit row.
        let history = if self.fail_history {
            Err(Error::Storage("history write failed".into()))
        } else {
            guard.append_history(&self.history)
        };
        let audit = if let Some(audit) = &self.audit {
            guard.append_audit(audit)
        } else {
            Ok(())
        };
        if history.is_err() || audit.is_err() {
            return Err(combine_record_errors(history, audit));
        }
        if self.fail_prune {
            return Err(Error::Storage("history prune failed".into()));
        }
        if let Err(err) = guard.prune_history(&self.history.connection_id, self.keep) {
            return Err(Error::Storage(format!(
                "history was recorded but pruning failed: {err}"
            )));
        }
        Ok(())
    }
}

fn combine_record_errors(history: Result<()>, audit: Result<()>) -> Error {
    match (history, audit) {
        (Err(history), Err(audit)) => Error::Storage(format!(
            "history write failed: {history}; audit write failed: {audit}"
        )),
        (Err(err), Ok(())) => Error::Storage(format!("history write failed: {err}")),
        (Ok(()), Err(err)) => Error::Storage(format!("audit write failed: {err}")),
        (Ok(()), Ok(())) => Error::Storage("record write failed".into()),
    }
}

/// Outcome of one record write, as the writer thread reports it. `Err` is the string form of the
/// [`Error`] the write produced; the waiting side turns it back into [`Error::Storage`].
type RecordAck = Sender<std::result::Result<(), String>>;

enum RecordMsg {
    Write {
        job: Box<RecordJob>,
        ack: Option<RecordAck>,
    },
    Flush {
        ack: Sender<()>,
    },
}

/// Handle to the single record writer. Cloning only clones the sender. Drop never spawns and
/// never touches a runtime, so it cannot panic because a worker thread failed to start.
#[derive(Clone)]
pub(crate) struct Recorder {
    tx: Sender<RecordMsg>,
    fail_prune: Arc<AtomicBool>,
    fail_history: Arc<AtomicBool>,
    /// Write failures since the last [`Self::drain`], including ones already reported to a caller.
    write_errors: Arc<Mutex<Vec<String>>>,
}

impl Recorder {
    pub fn start() -> Result<Self> {
        let (tx, rx) = mpsc::channel();
        let write_errors = Arc::new(Mutex::new(Vec::new()));
        let errors = Arc::clone(&write_errors);
        std::thread::Builder::new()
            .name("fastgrid-record".into())
            .spawn(move || {
                while let Ok(msg) = rx.recv() {
                    match msg {
                        RecordMsg::Write { job, ack } => {
                            let result = job.write().map_err(|err| err.to_string());
                            if let Err(err) = &result
                                && let Ok(mut slot) = errors.lock()
                            {
                                slot.push(err.clone());
                            }
                            if let Some(ack) = ack {
                                let _ = ack.send(result);
                            }
                        }
                        RecordMsg::Flush { ack } => {
                            let _ = ack.send(());
                        }
                    }
                }
            })
            .map_err(|err| Error::Other(format!("cannot start the record writer: {err}")))?;
        Ok(Self {
            tx,
            fail_prune: Arc::new(AtomicBool::new(false)),
            fail_history: Arc::new(AtomicBool::new(false)),
            write_errors,
        })
    }

    /// Queue a row. A send error means the writer thread has exited; that is a failed record, not
    /// a condition Drop may ignore when a caller is waiting on `ack`.
    fn enqueue(&self, mut job: RecordJob, ack: Option<RecordAck>) -> Result<()> {
        if self.fail_prune.swap(false, Ordering::SeqCst) {
            job.fail_prune = true;
        }
        if self.fail_history.swap(false, Ordering::SeqCst) {
            job.fail_history = true;
        }
        self.tx
            .send(RecordMsg::Write {
                job: Box::new(job),
                ack,
            })
            .map_err(|_| Error::Storage("the record writer has stopped".into()))
    }

    /// Test seam. Without `test-seams` these would be unused, and an unused setter is a lint
    /// failure for any crate that builds `fg-service` without the feature (single-package clippy).
    #[cfg(any(test, feature = "test-seams"))]
    pub fn fail_next_prune(&self) {
        self.fail_prune.store(true, Ordering::SeqCst);
    }

    #[cfg(any(test, feature = "test-seams"))]
    pub fn fail_next_history(&self) {
        self.fail_history.store(true, Ordering::SeqCst);
    }

    /// Block until jobs queued before this call have been written, then report write failures
    /// accumulated since the previous drain. Queue progress alone is not success.
    ///
    /// This is a synchronous API on purpose: [`crate::Service::drain_records`] is called on
    /// shutdown, where the caller is not awaiting anything. Do not call it from inside an async
    /// task that a foreign executor drives.
    pub fn drain(&self) -> Result<()> {
        let (ack_tx, ack_rx) = mpsc::channel();
        self.tx
            .send(RecordMsg::Flush { ack: ack_tx })
            .map_err(|_| Error::Storage("the record writer has stopped".into()))?;
        ack_rx
            .recv_timeout(Duration::from_secs(5))
            .map_err(|_| Error::Storage("timed out draining records".into()))?;
        let errors = self
            .write_errors
            .lock()
            .map(|mut slot| std::mem::take(&mut *slot))
            .unwrap_or_default();
        if errors.is_empty() {
            Ok(())
        } else {
            Err(Error::Storage(format!(
                "record write failed: {}",
                errors.join("; ")
            )))
        }
    }
}

struct MetaParts {
    store: Arc<Mutex<Store>>,
    connection_id: ConnectionId,
    connection_name: String,
    namespace: Option<String>,
    sql: String,
    source: QuerySource,
    confirmed: bool,
    actor: Actor,
    class: StatementClass,
    danger: Option<DangerReason>,
    history_keep: usize,
    executed_at: UnixMillis,
    started: Instant,
    audit: bool,
}

impl MetaParts {
    fn job(&self, outcome: Outcome, rows: Option<u64>, error: Option<String>) -> RecordJob {
        let duration_ms = Some(elapsed_ms(self.started));
        let history = QueryHistoryEntry {
            id: HistoryId::new(),
            connection_id: self.connection_id.clone(),
            namespace: self.namespace.clone(),
            sql: self.sql.clone(),
            executed_at: self.executed_at,
            duration_ms,
            outcome,
            row_count: rows,
            error: error.clone(),
            source: self.source,
        };
        let audit = self.audit.then(|| AuditEntry {
            id: AuditId::new(),
            at: self.executed_at,
            actor: self.actor.clone(),
            connection_id: Some(self.connection_id.clone()),
            connection_name: self.connection_name.clone(),
            statement: self.sql.clone(),
            class: self.class,
            danger: self.danger,
            decision: decision_for(outcome, self.confirmed),
            reason: error.clone(),
            outcome,
            error,
            duration_ms,
            row_count: rows,
        });
        RecordJob {
            store: Arc::clone(&self.store),
            history,
            audit,
            keep: self.history_keep,
            fail_prune: false,
            fail_history: false,
        }
    }
}

/// Shared with the execution task. The last drop writes one row if nobody has yet, using the
/// terminal outcome when the driver already produced one. Drop only sends to the writer thread.
struct RecordSlot {
    recorder: Recorder,
    meta: MetaParts,
    terminal: Mutex<Option<Terminal>>,
    rows: AtomicU64,
    truncated: AtomicBool,
    started_execute: AtomicBool,
    timed_out: Arc<AtomicBool>,
    sent: AtomicBool,
    canceller: Mutex<Option<crate::task::TaskCanceller>>,
}

#[derive(Clone)]
struct Terminal {
    outcome: Outcome,
    error: Option<String>,
}

impl RecordSlot {
    fn set_terminal(&self, outcome: Outcome, error: Option<String>) {
        if let Ok(mut slot) = self.terminal.lock()
            && slot.is_none()
        {
            *slot = Some(Terminal { outcome, error });
        }
    }

    fn add_rows(&self, n: u64) {
        self.rows.fetch_add(n, Ordering::Relaxed);
    }

    fn mark_truncated(&self) {
        self.truncated.store(true, Ordering::Relaxed);
    }

    fn record(&self, ack: Option<RecordAck>) {
        if self.sent.swap(true, Ordering::SeqCst) {
            return;
        }
        let terminal = self
            .terminal
            .lock()
            .unwrap_or_else(|err| err.into_inner())
            .clone();
        let (outcome, error) = match terminal {
            Some(terminal) => (terminal.outcome, terminal.error),
            None if self.started_execute.load(Ordering::SeqCst) => (Outcome::Cancelled, None),
            None => (Outcome::NotRun, None),
        };
        let error = if outcome == Outcome::Cancelled
            && error.is_none()
            && self.timed_out.load(Ordering::SeqCst)
        {
            Some(TIMEOUT_REASON.to_owned())
        } else {
            error
        };
        let job = self
            .meta
            .job(outcome, Some(self.rows.load(Ordering::Relaxed)), error);
        // Drop must not panic if the writer has already exited. Callers that can return an
        // error use the `Result` from `enqueue` instead of this path.
        if let Err(err) = self.recorder.enqueue(job, ack) {
            tracing::warn!(error = %err, "failed to queue query history");
        }
    }
}

impl Drop for RecordSlot {
    fn drop(&mut self) {
        // Notify the existing writer. No spawn, no runtime call: a shutting-down runtime cannot
        // make this panic, and a failed send is ignored.
        self.record(None);
    }
}

fn warn_record_failed() {
    tracing::warn!("failed to record query history");
}

const RECORD_WAIT: Duration = Duration::from_secs(5);

/// Await a record ack without blocking the caller's thread and without needing a tokio reactor on
/// it. The bounded wait runs on the service runtime's blocking pool, because `recv_timeout` must
/// not run on the executor that is awaiting this: that executor is allowed to be GPUI's or any
/// other, and `tokio::time::timeout` panics when there is no reactor on the current thread.
///
/// The recorder is a plain `std` thread, so a blocking `recv_timeout` is the natural wait; it just
/// has to happen off the caller. A storage error, a stopped writer, or a timeout is returned; none
/// of those are success.
async fn await_record(rt: &Handle, ack: Receiver<std::result::Result<(), String>>) -> Result<()> {
    let waited = rt
        .spawn_blocking(move || ack.recv_timeout(RECORD_WAIT))
        .await;
    match waited {
        // The blocking task was dropped without running, which means the runtime is shutting
        // down. Reporting a timeout would claim the write did not happen when it may have.
        Err(_) => {
            warn_record_failed();
            Err(Error::Storage(
                "the service runtime stopped before the record was confirmed".into(),
            ))
        }
        Ok(received) => record_result(received),
    }
}

/// Turn what the writer thread reported into the caller's result. Kept separate from the wait so
/// every branch can be pinned without waiting out [`RECORD_WAIT`].
fn record_result(
    received: std::result::Result<std::result::Result<(), String>, mpsc::RecvTimeoutError>,
) -> Result<()> {
    match received {
        Ok(Ok(())) => Ok(()),
        Ok(Err(err)) => {
            warn_record_failed();
            Err(Error::Storage(format!("the record failed: {err}")))
        }
        Err(mpsc::RecvTimeoutError::Disconnected) => {
            warn_record_failed();
            Err(Error::Storage(
                "the record writer stopped before the row was written".into(),
            ))
        }
        Err(mpsc::RecvTimeoutError::Timeout) => {
            warn_record_failed();
            Err(Error::Storage("timed out recording query history".into()))
        }
    }
}

async fn load_exec_settings(conn: &OpenConnection) -> Result<ExecSettings> {
    let store = Arc::clone(&conn.store);
    let loaded = conn.rt.handle.spawn_blocking(move || {
        let guard = store.lock().map_err(|_| lock_error())?;
        guard.setting_strict::<ExecSettings>()
    });
    match loaded.await {
        Ok(Ok(settings)) => Ok(settings),
        Ok(Err(err)) => Err(err),
        Err(err) => Err(Error::Other(format!("cannot read exec settings: {err}"))),
    }
}

fn meta_for(
    conn: &OpenConnection,
    sql: &str,
    options: &RunOptions,
    planned: &RunPlan,
    settings: &ExecSettings,
) -> MetaParts {
    let (class, danger) = subject(planned);
    MetaParts {
        store: Arc::clone(&conn.store),
        connection_id: conn.config.id.clone(),
        connection_name: conn.config.name.clone(),
        namespace: conn.config.default_namespace.clone(),
        sql: sql.to_owned(),
        source: options.source(),
        confirmed: options.kind_is_confirmed(),
        actor: actor_for(options),
        class,
        danger,
        history_keep: settings.history_keep,
        executed_at: UnixMillis::now(),
        started: Instant::now(),
        audit: !matches!(options.kind, RunKind::Gui { .. }),
    }
}

/// Record a refusal from `execute`, which has no confirmation and no per-call options.
pub(crate) async fn record_blocked(
    conn: &OpenConnection,
    sql: &str,
    planned: &RunPlan,
    refusal: PolicyRefusal,
) -> Result<()> {
    let keep = load_exec_settings(conn)
        .await
        .map(|settings| settings.history_keep)
        .unwrap_or(usize::MAX);
    let options = RunOptions::gui();
    let mut meta = meta_for(
        conn,
        sql,
        &options,
        planned,
        &ExecSettings {
            history_keep: keep,
            ..ExecSettings::default()
        },
    );
    // A blocked `execute` is not an editor keystroke; record it so the attempt is not silent.
    // Agent identity is not available on this path. `run` is what carries a client id.
    meta.audit = false;
    let (ack_tx, ack_rx) = mpsc::channel();
    conn.recorder.enqueue(
        meta.job(Outcome::NotRun, None, Some(refusal_message(refusal))),
        Some(ack_tx),
    )?;
    await_record(&conn.rt.handle, ack_rx).await
}

/// Record a refused agent write, then return it as a policy denial.
/// A standalone function, not a closure: a closure returning an async
/// block ties the future to the closure's own borrow, which the
/// compiler rejects here.
async fn deny_agent_write(
    recorder: &Recorder,
    handle: &Handle,
    meta: &MetaParts,
    message: &str,
) -> Result<RunStream> {
    let (ack_tx, ack_rx) = mpsc::channel();
    recorder.enqueue(
        meta.job(Outcome::NotRun, None, Some(message.to_owned())),
        Some(ack_tx),
    )?;
    await_record(handle, ack_rx).await?;
    Err(Error::PolicyDenied(message.to_owned()))
}

impl OpenConnection {
    /// Run user or agent SQL: classify, enforce safe mode, cap rows, then stream.
    ///
    /// A read-only write, or a statement that needs confirmation and has not been confirmed for
    /// this exact script, returns [`Error::PolicyDenied`] and records `NotRun` without leasing a
    /// session. Parameters plus more than one statement are [`Error::InvalidInput`]. Caps are the
    /// stricter of the stored [`ExecSettings`] and `options`. A settings read failure fails the
    /// call; it does not fall open to the default cap.
    ///
    /// Dropping the returned future while it waits for a session does not run the statement. The
    /// execution task records `NotRun` itself. Dropping the stream later records the outcome the
    /// driver already produced, or `Cancelled` if the driver had not finished.
    pub async fn run(
        self: &Arc<Self>,
        sql: &str,
        params: &[Value],
        options: RunOptions,
    ) -> Result<RunStream> {
        if let RunKind::Agent { client } = &options.kind
            && client.is_empty()
        {
            return Err(Error::InvalidInput("an agent run needs a client id".into()));
        }
        self.pool.ensure_current()?;
        let planned = plan(sql, self.kind(), self.safe_mode());
        if !params.is_empty() && planned.statements.len() > 1 {
            return Err(Error::InvalidInput(
                "parameters are only supported for a single statement".into(),
            ));
        }
        let confirmed = match &options.kind {
            RunKind::Gui {
                approval: Some(digest),
            } => {
                if script_digest(sql, self.kind()) != *digest {
                    let settings = load_exec_settings(self).await?;
                    let meta = meta_for(self, sql, &options, &planned, &settings);
                    let (ack_tx, ack_rx) = mpsc::channel();
                    self.recorder.enqueue(
                        meta.job(
                            Outcome::NotRun,
                            None,
                            Some("confirmation does not match this script".into()),
                        ),
                        Some(ack_tx),
                    )?;
                    await_record(&self.rt.handle, ack_rx).await?;
                    return Err(Error::PolicyDenied(
                        "confirmation does not match this script".into(),
                    ));
                }
                true
            }
            RunKind::Gui { approval: None } => false,
            RunKind::Agent { .. } => false,
        };

        // Fail closed: a settings read error must not widen the cap to the permissive default.
        let settings = load_exec_settings(self).await?;
        let meta = meta_for(self, sql, &options, &planned, &settings);

        // Agent writes need a person, not just a policy row. This gate
        // runs before the generic refusal so an approved write can pass
        // an Alert/Strict confirmation demand; read-only still refuses
        // through the generic path below, and approval never overrides
        // it. The transport cannot claim approval: it arrives here from
        // the service's pending-write registry, resolved in the GUI.
        let mut agent_approved = false;
        if let RunKind::Agent { client } = &options.kind
            && planned.statements.iter().any(|s| s.class.may_write())
            && self.safe_mode().effective() != SafeMode::ReadOnly
        {
            if !self.config.mcp.allow_write {
                return deny_agent_write(
                    &self.recorder,
                    &self.rt.handle,
                    &meta,
                    "agent writes are not enabled for this connection",
                )
                .await;
            }
            let statements = planned
                .statements
                .iter()
                .map(|s| McpWriteStatement {
                    class: s.class,
                    danger: s.danger,
                })
                .collect();
            let (request, rx) = self.mcp_writes.request(
                client.clone(),
                self.config.id.clone(),
                self.config.name.clone(),
                sql.to_owned(),
                statements,
            );
            if self.events.receiver_count() == 0 {
                // No GUI is listening: headless, so deny at once rather
                // than stalling the agent for the whole timeout.
                self.mcp_writes.forget(request.id);
                return deny_agent_write(
                    &self.recorder,
                    &self.rt.handle,
                    &meta,
                    "no GUI is attached to confirm agent writes",
                )
                .await;
            }
            let request_id = request.id;
            let _ = self
                .events
                .send(ServiceEvent::McpWriteRequested { request });
            match tokio::time::timeout(MCP_WRITE_APPROVAL_TIMEOUT, rx).await {
                Ok(Ok(true)) => agent_approved = true,
                Ok(Ok(false)) => {
                    return deny_agent_write(
                        &self.recorder,
                        &self.rt.handle,
                        &meta,
                        "the agent write was denied",
                    )
                    .await;
                }
                _ => {
                    self.mcp_writes.forget(request_id);
                    return deny_agent_write(
                        &self.recorder,
                        &self.rt.handle,
                        &meta,
                        "the agent write was not approved in time",
                    )
                    .await;
                }
            }
        }

        if let Some(refusal) = blocking_refusal(&planned, confirmed || agent_approved) {
            let (ack_tx, ack_rx) = mpsc::channel();
            self.recorder.enqueue(
                meta.job(Outcome::NotRun, None, Some(refusal_message(refusal))),
                Some(ack_tx),
            )?;
            await_record(&self.rt.handle, ack_rx).await?;
            return Err(Error::PolicyDenied(refusal_message(refusal)));
        }

        let max_cell = self.max_cell_bytes.load(Ordering::Relaxed);
        // Agent runs also merge the connection's MCP caps here, so the
        // transport cannot loosen them by passing larger per-call
        // numbers; the transport's own defaults stay transport-side.
        let agent = matches!(options.kind, RunKind::Agent { .. });
        let call_rows = if agent {
            stricter([options.max_rows, self.config.mcp.max_rows.map(u64::from)])
        } else {
            options.max_rows
        };
        let call_timeout = if agent {
            stricter([
                options.statement_timeout_secs,
                self.config.mcp.statement_timeout_secs,
            ])
        } else {
            options.statement_timeout_secs
        };
        let timeout = effective_statement_timeout(
            call_timeout,
            settings.statement_timeout_secs,
            self.config.timeouts.statement_secs,
        );
        let engine_timeout = self.capabilities().statement_timeout;
        let exec = ExecOptions {
            max_rows: rows_cap(call_rows, settings.max_rows),
            max_cell_bytes: (max_cell > 0).then_some(max_cell),
            statement_timeout_secs: engine_timeout.then_some(timeout).flatten(),
            ..ExecOptions::default()
        };

        let slot = Arc::new(RecordSlot {
            recorder: self.recorder.clone(),
            meta,
            terminal: Mutex::new(None),
            rows: AtomicU64::new(0),
            truncated: AtomicBool::new(false),
            started_execute: AtomicBool::new(false),
            timed_out: Arc::new(AtomicBool::new(false)),
            sent: AtomicBool::new(false),
            canceller: Mutex::new(None),
        });
        let cancel = Arc::new(AtomicBool::new(false));
        let (event_tx, event_rx) = async_mpsc::channel(8);
        let (ack_tx, ack_rx) = mpsc::channel();
        let (ready_tx, ready_rx) = oneshot::channel::<Result<()>>();

        let conn = Arc::clone(self);
        let sql_owned = sql.to_owned();
        let params = params.to_vec();
        let slot_task = Arc::clone(&slot);
        let cancel_task = Arc::clone(&cancel);
        let handle = self.rt.handle.spawn(async move {
            run_task(
                conn,
                sql_owned,
                params,
                exec,
                timeout.filter(|_| !engine_timeout),
                cancel_task,
                slot_task,
                event_tx,
                ack_tx,
                ready_tx,
            )
            .await;
        });

        let mut guard = StartGuard {
            cancel,
            handle,
            slot: Arc::clone(&slot),
            armed: true,
        };
        let ready = ready_rx.await;
        guard.armed = false;
        match ready {
            Ok(Ok(())) => Ok(RunStream {
                events: event_rx,
                slot,
                ack: Some(ack_rx),
                record_error: None,
                runtime: self.rt.shared.clone(),
                rt: self.rt.handle.clone(),
                _guard: guard,
            }),
            Ok(Err(err)) => {
                // The task records before sending the error. Await so the row exists on return,
                // and surface a write failure instead of swallowing `Ok(Err)`.
                match await_record(&self.rt.handle, ack_rx).await {
                    Ok(()) => Err(err),
                    Err(record) => Err(record),
                }
            }
            Err(_) => {
                guard.armed = true;
                drop(guard);
                Err(Error::Cancelled)
            }
        }
    }
}

struct StartGuard {
    cancel: Arc<AtomicBool>,
    handle: JoinHandle<()>,
    slot: Arc<RecordSlot>,
    armed: bool,
}

impl Drop for StartGuard {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        // Set the flag before abort so a task that just left `acquire` does not execute.
        self.cancel.store(true, Ordering::SeqCst);
        self.handle.abort();
        // If the task is still inside `acquire`, abort drops it and this slot (or the task's
        // clone) writes NotRun. Sending here as well is idempotent via `sent`.
        self.slot.record(None);
    }
}

fn fail_start(
    slot: &RecordSlot,
    ack_tx: RecordAck,
    ready_tx: oneshot::Sender<Result<()>>,
    outcome: Outcome,
    err: Error,
) {
    slot.set_terminal(outcome, Some(err.to_string()));
    slot.record(Some(ack_tx));
    let _ = ready_tx.send(Err(err));
}

#[allow(clippy::too_many_arguments)]
async fn run_task(
    conn: Arc<OpenConnection>,
    sql: String,
    params: Vec<Value>,
    exec: ExecOptions,
    service_timeout: Option<u32>,
    cancel: Arc<AtomicBool>,
    slot: Arc<RecordSlot>,
    event_tx: async_mpsc::Sender<Result<Event>>,
    ack_tx: RecordAck,
    ready_tx: oneshot::Sender<Result<()>>,
) {
    if cancel.load(Ordering::SeqCst) {
        slot.record(Some(ack_tx));
        let _ = ready_tx.send(Err(Error::Cancelled));
        return;
    }
    let lease = match conn.pool.acquire().await {
        Ok(lease) => lease,
        Err(err) => {
            fail_start(&slot, ack_tx, ready_tx, Outcome::Error, err);
            return;
        }
    };
    if cancel.load(Ordering::SeqCst) {
        drop(lease);
        slot.record(Some(ack_tx));
        let _ = ready_tx.send(Err(Error::Cancelled));
        return;
    }
    slot.started_execute.store(true, Ordering::SeqCst);
    // The lease re-checks the generation under the same lock `close` waits for, then starts the
    // driver. A save that landed after `acquire` is refused here and never reaches `reset`.
    let stream = match lease.run_statement(&sql, &params, exec).await {
        Ok(stream) => stream,
        Err(err) => {
            let outcome = if matches!(err, Error::Connection(_)) {
                Outcome::Cancelled
            } else {
                Outcome::Error
            };
            fail_start(&slot, ack_tx, ready_tx, outcome, err);
            return;
        }
    };
    let stream = match TaskStream::new(stream, lease) {
        Ok(stream) => stream,
        Err(err) => {
            fail_start(&slot, ack_tx, ready_tx, Outcome::Error, err);
            return;
        }
    };
    let canceller = stream.canceller();
    if let Ok(mut slot_cancel) = slot.canceller.lock() {
        *slot_cancel = Some(canceller.clone());
    }
    let clock = service_timeout.map(|secs| {
        StatementClock::spawn(
            Duration::from_secs(u64::from(secs)),
            canceller,
            Arc::clone(&slot.timed_out),
        )
    });
    // The caller is still waiting. Hand back the stream before reading events so a slow consumer
    // applies backpressure instead of deadlocking this task against `run`.
    if ready_tx.send(Ok(())).is_err() {
        drop(stream);
        if let Some(clock) = clock {
            clock.shutdown().await;
        }
        slot.set_terminal(Outcome::Cancelled, None);
        slot.record(Some(ack_tx));
        return;
    }
    forward(stream, event_tx, slot, ack_tx, clock).await;
}

/// How long a dropped consumer may keep reading already-queued driver output. One absolute
/// deadline, not a window that resets on each event.
const DRAIN_BOUND: Duration = Duration::from_millis(200);

async fn forward(
    mut stream: TaskStream,
    event_tx: async_mpsc::Sender<Result<Event>>,
    slot: Arc<RecordSlot>,
    ack_tx: RecordAck,
    clock: Option<StatementClock>,
) {
    let mut statement_rows = 0u64;
    while let Some(item) = stream.next().await {
        match &item {
            Err(err) => {
                let (outcome, text) = map_terminal(err, slot.timed_out.load(Ordering::SeqCst));
                slot.set_terminal(outcome, text);
                let _ = event_tx.send(item).await;
                break;
            }
            Ok(event) => {
                if matches!(event, Event::ResultStart { .. })
                    && let Some(clock) = &clock
                {
                    clock.arm_full();
                }
                observe_event(&slot, event, &mut statement_rows);
                if matches!(event, Event::ResultEnd { .. })
                    && let Some(clock) = &clock
                {
                    clock.disarm();
                }
                // Waiting on the consumer is not statement time.
                let paused = clock.as_ref().and_then(StatementClock::pause);
                let sent = event_tx.send(item).await;
                if let (Some(clock), Some(left)) = (&clock, paused) {
                    clock.resume(left);
                }
                if sent.is_err() {
                    if slot
                        .terminal
                        .lock()
                        .unwrap_or_else(|err| err.into_inner())
                        .is_none()
                    {
                        let pending = stream.next().await;
                        drain_known_outcome(&mut stream, pending, &slot, &mut statement_rows).await;
                    }
                    break;
                }
            }
        }
    }
    if slot
        .terminal
        .lock()
        .unwrap_or_else(|err| err.into_inner())
        .is_none()
    {
        slot.set_terminal(Outcome::Ok, None);
    }
    if let Some(clock) = clock {
        clock.shutdown().await;
    }
    slot.record(Some(ack_tx));
}

async fn drain_known_outcome(
    stream: &mut TaskStream,
    pending: Option<Result<Event>>,
    slot: &RecordSlot,
    statement_rows: &mut u64,
) {
    let timed_out = slot.timed_out.load(Ordering::SeqCst);
    let outcome = drain_pending(
        pending,
        |event| observe_event(slot, event, statement_rows),
        DRAIN_BOUND,
        timed_out,
        stream,
    )
    .await;
    match outcome {
        DrainStop::Terminal(outcome, error) => slot.set_terminal(outcome, error),
        DrainStop::Cancelled => {
            stream.cancel();
            slot.set_terminal(Outcome::Cancelled, None);
        }
    }
}

enum DrainStop {
    Terminal(Outcome, Option<String>),
    Cancelled,
}

/// Read events until the producer ends, a terminal error arrives, or `bound` elapses. The bound
/// is measured from the start of the drain, not from the last item. `pull` is the driver's next
/// event; the caller cancels when this returns [`DrainStop::Cancelled`].
async fn drain_pending(
    mut pending: Option<Result<Event>>,
    mut observe: impl FnMut(&Event),
    bound: Duration,
    timed_out: bool,
    stream: &mut TaskStream,
) -> DrainStop {
    let deadline = Instant::now() + bound;
    loop {
        match pending {
            Some(Err(err)) => {
                let (outcome, error) = map_terminal(&err, timed_out);
                return DrainStop::Terminal(outcome, error);
            }
            None => return DrainStop::Terminal(Outcome::Ok, None),
            Some(Ok(event)) => {
                observe(&event);
                let left = deadline.saturating_duration_since(Instant::now());
                if left.is_zero() {
                    return DrainStop::Cancelled;
                }
                pending = match tokio::time::timeout(left, stream.next()).await {
                    Ok(item) => item,
                    Err(_) => return DrainStop::Cancelled,
                };
            }
        }
    }
}

/// Same deadline rule as [`drain_pending`], for a channel producer the unit test can pace.
#[cfg(test)]
async fn drain_channel(
    mut pending: Option<Result<Event>>,
    mut rx: tokio::sync::mpsc::Receiver<Result<Event>>,
    mut observe: impl FnMut(&Event),
    bound: Duration,
) -> DrainStop {
    let deadline = Instant::now() + bound;
    loop {
        match pending {
            Some(Err(err)) => {
                let (outcome, error) = map_terminal(&err, false);
                return DrainStop::Terminal(outcome, error);
            }
            None => return DrainStop::Terminal(Outcome::Ok, None),
            Some(Ok(event)) => {
                observe(&event);
                let left = deadline.saturating_duration_since(Instant::now());
                if left.is_zero() {
                    return DrainStop::Cancelled;
                }
                pending = match tokio::time::timeout(left, rx.recv()).await {
                    Ok(item) => item,
                    Err(_) => return DrainStop::Cancelled,
                };
            }
        }
    }
}

/// Per-statement deadline. Disarmed between statements and while the consumer is the one waiting,
/// and aborted as soon as the run task finishes so it does not keep the canceller alive.
struct StatementClock {
    limit: Duration,
    started: Instant,
    deadline_ms: Arc<AtomicU64>,
    wake: Arc<Notify>,
    stop: Arc<Notify>,
    handle: JoinHandle<()>,
}

impl StatementClock {
    fn spawn(
        limit: Duration,
        canceller: crate::task::TaskCanceller,
        timed_out: Arc<AtomicBool>,
    ) -> Self {
        let deadline_ms = Arc::new(AtomicU64::new(0));
        let wake = Arc::new(Notify::new());
        let stop = Arc::new(Notify::new());
        let deadline_task = Arc::clone(&deadline_ms);
        let wake_task = Arc::clone(&wake);
        let stop_task = Arc::clone(&stop);
        let started = Instant::now();
        let handle = tokio::spawn(async move {
            let _live = TimerLive::enter();
            loop {
                let deadline = deadline_task.load(Ordering::SeqCst);
                let now = millis_since(started);
                if deadline != 0 && now >= deadline {
                    timed_out.store(true, Ordering::SeqCst);
                    canceller.cancel();
                    return;
                }
                let sleep = if deadline == 0 {
                    Duration::from_secs(3600)
                } else {
                    Duration::from_millis(deadline.saturating_sub(now))
                };
                tokio::select! {
                    _ = tokio::time::sleep(sleep) => {}
                    _ = wake_task.notified() => {}
                    _ = stop_task.notified() => return,
                }
            }
        });
        let clock = Self {
            limit,
            started,
            deadline_ms,
            wake,
            stop,
            handle,
        };
        clock.arm_full();
        clock
    }

    fn arm_full(&self) {
        let deadline = millis_since(self.started).saturating_add(duration_ms(self.limit));
        self.deadline_ms.store(deadline, Ordering::SeqCst);
        self.wake.notify_one();
    }

    fn disarm(&self) {
        self.deadline_ms.store(0, Ordering::SeqCst);
        self.wake.notify_one();
    }

    fn pause(&self) -> Option<Duration> {
        let deadline = self.deadline_ms.swap(0, Ordering::SeqCst);
        if deadline == 0 {
            return None;
        }
        let now = millis_since(self.started);
        Some(Duration::from_millis(deadline.saturating_sub(now)))
    }

    fn resume(&self, remaining: Duration) {
        let deadline = millis_since(self.started).saturating_add(duration_ms(remaining));
        self.deadline_ms.store(deadline, Ordering::SeqCst);
        self.wake.notify_one();
    }

    async fn shutdown(self) {
        self.stop.notify_one();
        let _ = self.handle.await;
    }
}

fn millis_since(started: Instant) -> u64 {
    u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX)
}

fn duration_ms(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

static LIVE_TIMERS: AtomicUsize = AtomicUsize::new(0);

struct TimerLive;

impl TimerLive {
    fn enter() -> Self {
        LIVE_TIMERS.fetch_add(1, Ordering::SeqCst);
        Self
    }
}

impl Drop for TimerLive {
    fn drop(&mut self) {
        LIVE_TIMERS.fetch_sub(1, Ordering::SeqCst);
    }
}

#[cfg(any(test, feature = "test-seams"))]
pub(crate) fn live_statement_timers() -> usize {
    LIVE_TIMERS.load(Ordering::SeqCst)
}

fn map_terminal(err: &Error, timed_out: bool) -> (Outcome, Option<String>) {
    if matches!(err, Error::Cancelled) {
        let error = timed_out.then(|| TIMEOUT_REASON.to_owned());
        (Outcome::Cancelled, error)
    } else {
        (Outcome::Error, Some(err.to_string()))
    }
}

fn observe_event(slot: &RecordSlot, event: &Event, statement_rows: &mut u64) {
    match event {
        Event::ResultStart { .. } => *statement_rows = 0,
        Event::Rows(rows) => {
            let n = u64::try_from(rows.len()).unwrap_or(u64::MAX);
            *statement_rows = statement_rows.saturating_add(n);
            slot.add_rows(n);
        }
        Event::ResultEnd {
            truncated,
            affected_rows,
            ..
        } => {
            if *truncated {
                slot.mark_truncated();
            }
            if *statement_rows == 0 {
                slot.add_rows(affected_rows.unwrap_or(0));
            }
            *statement_rows = 0;
        }
        _ => {}
    }
}

/// A policy-checked run. Dropping it records one history row for the outcome already known to
/// the execution task, or `Cancelled` if the driver had not finished, and cancels the statement.
pub struct RunStream {
    events: async_mpsc::Receiver<Result<Event>>,
    slot: Arc<RecordSlot>,
    ack: Option<Receiver<std::result::Result<(), String>>>,
    record_error: Option<String>,
    /// Holds the service runtime until this stream is dropped, so a record in flight is not
    /// abandoned because the last `Service` went away first.
    runtime: Option<Arc<SharedRuntime>>,
    /// The service runtime, for the bounded record wait. A `Handle` is independent of the caller's
    /// executor, so `next()` and `collect()` work when awaited from GPUI's executor.
    rt: Handle,
    _guard: StartGuard,
}

impl std::fmt::Debug for RunStream {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RunStream")
            .field("rows", &self.slot.rows.load(Ordering::Relaxed))
            .field("truncated", &self.truncated())
            .finish_non_exhaustive()
    }
}

impl RunStream {
    /// Next event. `None` once the script finished; the history row is written by the execution
    /// task, and this waits for that write so [`Self::record_error`] is set before `None`.
    pub async fn next(&mut self) -> Option<Result<Event>> {
        match self.events.recv().await {
            Some(item) => Some(item),
            None => {
                self.wait_record().await;
                None
            }
        }
    }

    /// Cancel handle for this run only. A late cancel does not hit the next statement that
    /// reuses the session; [`crate::task::TaskCanceller`] is what makes that true.
    pub fn canceller(&self) -> crate::task::TaskCanceller {
        self.slot
            .canceller
            .lock()
            .unwrap_or_else(|err| err.into_inner())
            .clone()
            .unwrap_or_else(crate::task::TaskCanceller::noop)
    }

    /// Read the whole script into memory. Returns an error if the statement failed, or if it ran
    /// but the history/audit row could not be written.
    pub async fn collect(mut self) -> Result<Vec<fg_driver::ResultSet>> {
        use fg_driver::ResultSet;
        let mut sets = Vec::new();
        let mut failed = None;
        while let Some(event) = self.next().await {
            let event = match event {
                Ok(event) => event,
                Err(err) => {
                    failed = Some(err);
                    break;
                }
            };
            match event {
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
                Event::Notice(_) => {}
                _ => {}
            }
        }
        // `next` waits for the record only when the channel ends. A statement error returns
        // before that, so wait here too: the row should exist before `collect` returns.
        self.wait_record().await;
        if let Some(err) = failed {
            return Err(err);
        }
        if let Some(err) = &self.record_error {
            return Err(Error::Storage(format!(
                "the statement ran but the record failed: {err}"
            )));
        }
        Ok(sets)
    }

    /// True once any result set hit the row cap.
    pub fn truncated(&self) -> bool {
        self.slot.truncated.load(Ordering::Relaxed)
    }

    /// Set when history or audit could not be written. `None` until the run has finished
    /// recording. A prune failure is visible here and does not drop the audit row.
    pub fn record_error(&self) -> Option<&str> {
        self.record_error.as_deref()
    }

    async fn wait_record(&mut self) {
        if let Some(ack) = self.ack.take()
            && let Err(err) = await_record(&self.rt, ack).await
        {
            self.record_error = Some(err.to_string());
        }
    }
}

impl Drop for RunStream {
    fn drop(&mut self) {
        // Dropping the event receiver only unblocks the forwarder once it next sends. A query
        // that has not produced an event yet would keep running. Cancel through the handle the
        // task installed; that does not spawn and does not touch the runtime.
        if let Some(canceller) = self
            .slot
            .canceller
            .lock()
            .unwrap_or_else(|err| err.into_inner())
            .clone()
        {
            canceller.cancel();
        }
        let _ = &self.runtime;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sql_of<'a>(script: &'a str, stmt: &PlannedStatement) -> &'a str {
        &script[stmt.offset.clone()]
    }

    /// Poll a future on this thread with no runtime anywhere, waking on `Thread::unpark`. The
    /// integration tests in `tests/foreign_executor.rs` use the same shape for the whole service.
    fn block_on<F: std::future::Future>(future: F) -> F::Output {
        use std::task::{Context, Poll, Wake, Waker};
        struct ThreadWaker(std::thread::Thread);
        impl Wake for ThreadWaker {
            fn wake(self: Arc<Self>) {
                self.0.unpark();
            }
            fn wake_by_ref(self: &Arc<Self>) {
                self.0.unpark();
            }
        }
        let waker = Waker::from(Arc::new(ThreadWaker(std::thread::current())));
        let mut context = Context::from_waker(&waker);
        let mut future = std::pin::pin!(future);
        loop {
            match future.as_mut().poll(&mut context) {
                Poll::Ready(value) => return value,
                Poll::Pending => std::thread::park(),
            }
        }
    }

    /// Every branch of the record wait keeps its old meaning: only `Ok(())` is success, and a
    /// failed write, a dead writer and a timeout are all storage errors.
    #[test]
    fn a_record_wait_reports_every_failure_as_storage() {
        assert!(record_result(Ok(Ok(()))).is_ok());
        for (received, needle) in [
            (
                Ok(Err("history write failed".to_owned())),
                "the record failed",
            ),
            (
                Err(mpsc::RecvTimeoutError::Disconnected),
                "the record writer stopped",
            ),
            (Err(mpsc::RecvTimeoutError::Timeout), "timed out recording"),
        ] {
            let err = match record_result(received) {
                Err(err) => err,
                Ok(()) => panic!("a failed record must not look like success"),
            };
            assert!(matches!(err, Error::Storage(_)), "{err:?}");
            assert!(err.to_string().contains(needle), "{err}");
        }
    }

    /// The wait must not need a reactor on the caller's thread: it hops to the runtime's blocking
    /// pool. This is the bug a `#[tokio::test]` cannot see.
    #[test]
    fn a_record_wait_works_without_a_reactor_on_the_caller() {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .unwrap();
        let (ack_tx, ack_rx) = mpsc::channel();
        let writer = std::thread::spawn(move || {
            let _ = ack_tx.send(Ok(()));
        });
        let outcome = block_on(await_record(rt.handle(), ack_rx));
        writer.join().unwrap();
        assert!(outcome.is_ok(), "{outcome:?}");
    }

    #[tokio::test]
    async fn statement_clock_does_not_outlive_shutdown() {
        let clock = StatementClock::spawn(
            Duration::from_secs(30),
            crate::task::TaskCanceller::noop(),
            Arc::new(AtomicBool::new(false)),
        );
        tokio::task::yield_now().await;
        assert_eq!(live_statement_timers(), 1);
        let first = clock.deadline_ms.load(Ordering::SeqCst);
        tokio::time::sleep(Duration::from_millis(20)).await;
        clock.arm_full();
        let second = clock.deadline_ms.load(Ordering::SeqCst);
        assert!(
            second > first,
            "ResultStart must reset the per-statement deadline ({first} -> {second})"
        );
        clock.shutdown().await;
        assert_eq!(live_statement_timers(), 0);
    }

    #[tokio::test]
    async fn drain_uses_one_deadline_and_counts_rows() {
        let (tx, mut rx) = tokio::sync::mpsc::channel(4);
        let producer = tokio::spawn(async move {
            for i in 0..40 {
                let event = Event::Rows(vec![vec![Value::Int(i)]]);
                if tx.send(Ok(event)).await.is_err() {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        });
        let started = Instant::now();
        let mut rows = 0u64;
        let first = rx.recv().await;
        let stop = drain_channel(
            first,
            rx,
            |event| {
                if let Event::Rows(batch) = event {
                    rows += u64::try_from(batch.len()).unwrap_or(0);
                }
            },
            Duration::from_millis(200),
        )
        .await;
        let elapsed = started.elapsed();
        producer.abort();
        assert!(
            elapsed < Duration::from_millis(350),
            "drain ran for {elapsed:?}, past the absolute bound"
        );
        assert!(rows > 0 && rows < 40, "counted {rows} rows");
        assert!(matches!(stop, DrainStop::Cancelled));
    }

    #[test]
    fn classification_and_offsets_map_into_the_original_text() {
        let sql = "SELECT 1; UPDATE t SET a = 1 WHERE b = 2; DROP TABLE t";
        let plan = plan(sql, DbKind::Sqlite, SafeMode::Normal);
        assert_eq!(plan.statements.len(), 3);
        assert!(plan.first_refusal().is_none());
        assert!(!plan.requires_confirmation());
        assert_eq!(plan.statements[0].class, StatementClass::Read);
        assert_eq!(plan.statements[1].class, StatementClass::Write);
        assert_eq!(plan.statements[2].class, StatementClass::Ddl);
        assert_eq!(plan.statements[2].danger, Some(DangerReason::DropObject));
        for stmt in &plan.statements {
            assert_eq!(sql_of(sql, stmt), stmt.sql.as_str());
        }
        assert_eq!(plan.statements[0].offset.start, 0);
        assert!(plan.statements[2].offset.start > plan.statements[1].offset.end);
    }

    #[test]
    fn comments_strings_and_trigger_bodies_are_not_split() {
        let sqlite = "SELECT 'a;b', \"c;d\" -- g;\n, /* h; */ 1; SELECT 2";
        let split = plan(sqlite, DbKind::Sqlite, SafeMode::Normal);
        assert_eq!(split.statements.len(), 2);
        assert!(split.statements[0].sql.contains("'a;b'"));
        assert_eq!(
            sql_of(sqlite, &split.statements[0]),
            split.statements[0].sql
        );

        let pg = "CREATE FUNCTION f() RETURNS int AS $$ BEGIN RETURN 1; END; $$ LANGUAGE plpgsql; SELECT f()";
        let split = plan(pg, DbKind::Postgres, SafeMode::Normal);
        assert_eq!(split.statements.len(), 2, "{split:?}");
        assert!(split.statements[0].sql.contains("RETURN 1;"));
        assert_eq!(split.statements[1].sql, "SELECT f()");

        let trigger = "CREATE TRIGGER t AFTER INSERT ON a BEGIN INSERT INTO log VALUES (1); UPDATE a SET x = 2; END; SELECT 1";
        let split = plan(trigger, DbKind::Sqlite, SafeMode::Alert);
        assert_eq!(split.statements.len(), 2, "{split:?}");
        assert_eq!(split.statements[1].sql, "SELECT 1");
        assert!(split.statements[0].sql.contains("INSERT INTO log"));
    }

    #[test]
    fn empty_and_whitespace_scripts_have_no_statements() {
        for sql in [
            "",
            "   ",
            "\n\t",
            "  ;;  \n",
            "-- only a comment\n/* and another */",
        ] {
            let plan = plan(sql, DbKind::Sqlite, SafeMode::ReadOnly);
            assert!(plan.statements.is_empty(), "{sql:?}");
            assert!(plan.first_refusal().is_none(), "{sql:?}");
            assert!(!plan.requires_confirmation());
        }
    }

    #[test]
    fn danger_is_flagged_in_alert_and_strict_but_not_normal() {
        let sql = "SELECT 1; DELETE FROM t; DROP TABLE t; INSERT INTO t VALUES (1)";
        let alert = plan(sql, DbKind::Sqlite, SafeMode::Alert);
        assert!(alert.statements[0].danger.is_none());
        assert!(!alert.statements[0].needs_confirmation);
        assert_eq!(
            alert.statements[1].danger,
            Some(DangerReason::DeleteWithoutWhere)
        );
        assert!(alert.statements[1].needs_confirmation);
        assert_eq!(alert.statements[2].danger, Some(DangerReason::DropObject));
        assert!(alert.statements[2].needs_confirmation);
        assert!(
            !alert.statements[3].needs_confirmation,
            "a plain insert is not dangerous"
        );
        assert!(alert.requires_confirmation());
        assert_eq!(
            alert.first_refusal(),
            Some(PolicyRefusal::NeedsConfirmation {
                statement: 1,
                class: StatementClass::Write,
                danger: Some(DangerReason::DeleteWithoutWhere),
            })
        );

        let strict = plan(sql, DbKind::Sqlite, SafeMode::Strict);
        assert!(strict.statements[1].needs_confirmation);
        assert!(
            strict.statements[3].needs_confirmation,
            "strict confirms every write, not only dangerous ones"
        );
        assert_eq!(
            strict.first_refusal(),
            Some(PolicyRefusal::NeedsConfirmation {
                statement: 1,
                class: StatementClass::Write,
                danger: Some(DangerReason::DeleteWithoutWhere),
            })
        );

        let normal = plan(sql, DbKind::Sqlite, SafeMode::Normal);
        assert!(normal.statements.iter().all(|s| !s.needs_confirmation));
        assert!(normal.first_refusal().is_none());
        assert!(!normal.requires_confirmation());
    }

    #[test]
    fn safe_mode_permissiveness_is_normal_alert_strict_read_only() {
        let sql = "SELECT 1; INSERT INTO t VALUES (1); DELETE FROM t; DROP TABLE t";
        let first = |mode| plan(sql, DbKind::Sqlite, mode).first_refusal();
        assert!(first(SafeMode::Normal).is_none());
        assert_eq!(
            first(SafeMode::Alert),
            Some(PolicyRefusal::NeedsConfirmation {
                statement: 2,
                class: StatementClass::Write,
                danger: Some(DangerReason::DeleteWithoutWhere),
            })
        );
        assert_eq!(
            first(SafeMode::Strict),
            Some(PolicyRefusal::NeedsConfirmation {
                statement: 1,
                class: StatementClass::Write,
                danger: None,
            })
        );
        assert_eq!(
            first(SafeMode::ReadOnly),
            Some(PolicyRefusal::ReadOnly {
                statement: 1,
                class: StatementClass::Write,
            })
        );
        // A statement a stricter mode allows, every looser mode allows. Read-only allows none
        // of the writes; strict allows none without confirmation; alert allows the insert.
        let allows_insert = |mode| {
            let plan = plan(sql, DbKind::Sqlite, mode);
            !plan.statements[1].needs_confirmation && plan.first_refusal().is_none()
                || matches!(
                    plan.first_refusal(),
                    Some(PolicyRefusal::NeedsConfirmation { statement, .. }) if statement > 1
                )
        };
        assert!(allows_insert(SafeMode::Normal));
        assert!(allows_insert(SafeMode::Alert));
        assert!(!allows_insert(SafeMode::Strict));
        assert!(!allows_insert(SafeMode::ReadOnly));
        assert!(blocking_refusal(&plan(sql, DbKind::Sqlite, SafeMode::ReadOnly), true).is_some());
        assert!(blocking_refusal(&plan(sql, DbKind::Sqlite, SafeMode::Strict), true).is_none());
    }

    #[test]
    fn confirmation_digest_changes_when_the_script_changes() {
        let kind = DbKind::Sqlite;
        let approved = RunOptions::gui_confirmed("DELETE FROM t", kind);
        let other = RunOptions::gui_confirmed("DELETE FROM u", kind);
        assert_ne!(approved, other);
        assert_eq!(
            script_digest("DELETE FROM t", kind),
            script_digest("DELETE FROM t", kind)
        );
    }

    #[test]
    fn limits_take_the_stricter_some() {
        assert_eq!(stricter([Some(1_000_000u64), Some(2)]), Some(2));
        assert_eq!(stricter([Some(2u64), None]), Some(2));
        assert_eq!(stricter([None, Some(2u64)]), Some(2));
        assert_eq!(stricter::<u64>([None, None]), None);
        assert_eq!(
            effective_statement_timeout(Some(10), Some(2), Some(5)),
            Some(2)
        );
        assert_eq!(effective_statement_timeout(None, None, Some(1)), Some(1));
        assert_eq!(effective_statement_timeout(None, None, None), None);
    }

    #[test]
    fn read_only_and_unknown_refuse_writes_without_asking() {
        let sql = "SELECT 1; BEGIN; UPDATE t SET a = 1 WHERE id = 1; PRAGMA foreign_keys = ON";
        for mode in [SafeMode::ReadOnly, SafeMode::Unknown] {
            let plan = plan(sql, DbKind::Sqlite, mode);
            assert_eq!(plan.statements[0].class, StatementClass::Read);
            assert_eq!(plan.statements[1].class, StatementClass::Transaction);
            assert!(!plan.statements[0].needs_confirmation);
            assert!(!plan.statements[2].needs_confirmation);
            assert_eq!(
                plan.first_refusal(),
                Some(PolicyRefusal::ReadOnly {
                    statement: 2,
                    class: StatementClass::Write,
                }),
                "{mode:?}"
            );
            assert!(!plan.requires_confirmation());
        }

        let only_reads = plan("SELECT 1; COMMIT", DbKind::Sqlite, SafeMode::ReadOnly);
        assert!(only_reads.first_refusal().is_none());
    }

    #[test]
    fn planned_debug_does_not_include_sql_text() {
        let sql = "SELECT secret_token FROM passwords";
        let plan = plan(sql, DbKind::Sqlite, SafeMode::Normal);
        let rendered = format!("{plan:?} {:?}", plan.statements[0]);
        assert!(!rendered.contains("secret_token"), "{rendered}");
        assert!(rendered.contains("redacted"), "{rendered}");
    }
}
