//! The application service layer, shared by the GUI and the MCP server.
//!
//! It owns: the encrypted local store (and the device key in the OS keychain), the tokio runtime,
//! open connections with their session pools and metadata caches, and the lifecycle of running
//! statements (streaming, cancelling). Everything here is headless; `fg-app` and `fg-mcp` are
//! thin clients of it.
//!
//! All public methods are `async` and may be awaited from any executor: they hop onto the
//! service's own tokio runtime internally.
//!
//! User and agent SQL goes through [`OpenConnection::run`]: it classifies the script, applies
//! the safe-mode policy and the row cap, and records history before a session is leased for a
//! refusal. [`OpenConnection::execute`] applies the same classification and cannot carry a
//! confirmation; it does not record a successful constructed statement.

#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

mod backup;
mod cache;
mod commit;
mod commit_schema;
mod conn;
mod export;
mod import;
mod keychain;
mod pool;
mod run;
mod ssh;
mod task;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use fg_core::{
    AuditEntry, ConnectionConfig, ConnectionGroup, ConnectionId, ConnectionParams, DocumentId,
    Error, ExecSettings, GridSettings, GroupId, ObjectPath, QueryDocument, QueryHistoryEntry,
    Result, SafeMode, SecretKind, SettingGroup, Snippet, SnippetId, TableViewState, WorkspaceState,
};
use fg_driver::driver_for;
use fg_store::{DeviceKeyProvider, Store, StoredDocument, StoredSnippet};
use tokio::runtime::Handle;
use tokio::sync::broadcast;

pub use commit::CommitOutcome;
pub use commit_schema::{ApplySchemaOutcome, SchemaPreview};
pub use conn::{BrowseOptions, OpenConnection, Refresh};
pub use export::{ExportControl, ExportFormat, ExportOptions, ExportReport, ExportScope};
pub use fg_store::AuditFilter;
pub use fg_store::{
    ExportOptions as BackupOptions, ExportScope as BackupScope, ImportReport as BackupReport,
    StoredConnection,
};
pub use import::{
    ColumnMapping, CsvImportOptions, CsvImportReport, CsvPreview, ImportErrorPolicy, preview_csv,
};
pub use keychain::{
    DEV_KEYRING_FILE, FileBackend, KeychainKeyProvider, MemoryBackend, OsKeychain, SecretBackend,
    account_name, default_key_provider,
};
pub use run::{PlannedStatement, PolicyRefusal, RunOptions, RunPlan, RunStream, plan};
pub use ssh::SshTunnel;
pub use task::{TaskCanceller, TaskStream};

use conn::{Rt, SharedRuntime};
use pool::SessionPool;
use run::Recorder;

/// Config generation per connection. A save or delete bumps it; a pool opened under an older
/// value cannot be published or leased.
pub(crate) type Epochs = Arc<Mutex<HashMap<ConnectionId, u64>>>;

struct OpenPauseInner {
    reached: Option<tokio::sync::oneshot::Sender<()>>,
    release: tokio::sync::oneshot::Receiver<()>,
}

struct SavePauseInner {
    reached: Option<tokio::sync::oneshot::Sender<()>>,
    release: tokio::sync::oneshot::Receiver<()>,
    done: Option<tokio::sync::oneshot::Sender<()>>,
}

/// Test seam: `open` waits after reading the config and before connecting.
#[cfg(any(test, feature = "test-seams"))]
#[doc(hidden)]
pub struct OpenPause {
    reached: Option<tokio::sync::oneshot::Receiver<()>>,
    release: tokio::sync::oneshot::Sender<()>,
}

#[cfg(any(test, feature = "test-seams"))]
impl OpenPause {
    /// Wait until `open` has read the config and released its lock.
    pub async fn wait(&mut self) {
        if let Some(reached) = self.reached.take() {
            let _ = reached.await;
        }
    }

    /// Let the paused `open` continue.
    pub fn release(self) {
        let _ = self.release.send(());
    }
}

/// Test seam: a save waits inside the service-owned task. Dropping the caller's future does not
/// cancel that task. [`Self::done`] resolves after invalidation.
#[cfg(any(test, feature = "test-seams"))]
#[doc(hidden)]
pub struct SavePause {
    reached: Option<tokio::sync::oneshot::Receiver<()>>,
    release: Option<tokio::sync::oneshot::Sender<()>>,
    done: Option<tokio::sync::oneshot::Receiver<()>>,
}

#[cfg(any(test, feature = "test-seams"))]
impl SavePause {
    pub async fn wait(&mut self) {
        if let Some(reached) = self.reached.take() {
            let _ = reached.await;
        }
    }

    pub fn release(&mut self) {
        if let Some(release) = self.release.take() {
            let _ = release.send(());
        }
    }

    pub async fn done(&mut self) {
        if let Some(done) = self.done.take() {
            let _ = tokio::time::timeout(Duration::from_secs(5), done).await;
        }
    }
}

#[cfg(any(test, feature = "test-seams"))]
pub use pool::ExecutePause;

/// Things the UI wants to react to without polling.
#[derive(Debug, Clone, PartialEq)]
pub enum ServiceEvent {
    /// A connection was added, edited, deleted or reordered.
    ConnectionsChanged,
    Connected(ConnectionId),
    Disconnected(ConnectionId),
    /// Cached schema metadata of a connection was invalidated.
    MetadataChanged {
        connection: ConnectionId,
    },
}

pub struct ServiceOptions {
    pub store_path: PathBuf,
    /// Use an existing tokio runtime. `None` = the service creates and owns one.
    pub runtime: Option<Handle>,
    pub max_sessions_per_connection: usize,
    /// How long schema metadata is trusted before it is read again.
    pub metadata_ttl: Duration,
    /// Open every connection as [`SafeMode::ReadOnly`], whatever its saved setting says.
    ///
    /// For a headless agent process: the override lands on the in-memory config before the pool
    /// is built, so the engine session is read-only and `run` classifies against read-only too.
    /// It is never written back to the store.
    pub force_read_only: bool,
}

impl ServiceOptions {
    pub fn new(store_path: impl Into<PathBuf>) -> Self {
        Self {
            store_path: store_path.into(),
            runtime: None,
            max_sessions_per_connection: 4,
            metadata_ttl: Duration::from_secs(60),
            force_read_only: false,
        }
    }
}

/// How to treat a connection's database password when saving it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PasswordUpdate {
    Keep,
    Set(String),
    Clear,
}

#[derive(Debug, Clone, PartialEq)]
pub struct TestReport {
    pub latency: Duration,
    pub server_version: String,
}

struct Inner {
    rt: Rt,
    /// Keeps an owned runtime alive for as long as this service, or any stream that cloned it,
    /// still exists. `None` when the caller supplied the runtime.
    _owned_runtime: Option<Arc<SharedRuntime>>,
    store: Arc<Mutex<Store>>,
    open: Mutex<HashMap<ConnectionId, Arc<OpenConnection>>>,
    /// Serialises open, save and delete so a config read cannot publish after a newer save.
    open_gate: tokio::sync::Mutex<()>,
    epochs: Epochs,
    /// Pools created by an open that has not published yet. `close` cancels these too.
    connecting: Mutex<HashMap<ConnectionId, Vec<Arc<SessionPool>>>>,
    open_pause: Mutex<Option<OpenPauseInner>>,
    save_pause: Mutex<Option<SavePauseInner>>,
    recorder: Recorder,
    events: broadcast::Sender<ServiceEvent>,
    max_cell_bytes: Arc<AtomicUsize>,
    max_sessions: usize,
    metadata_ttl: Duration,
    force_read_only: bool,
}

#[derive(Clone)]
pub struct Service {
    inner: Arc<Inner>,
}

pub(crate) fn lock_error() -> Error {
    Error::Storage("the local store lock was poisoned".into())
}

impl Service {
    /// Open the local store (creating it, and the device key, on first run) and start the runtime.
    ///
    /// Fails with `Error::KeyUnavailable` if the store exists but its key is not in the keychain;
    /// the store is left untouched.
    pub fn start(options: ServiceOptions, keys: &dyn DeviceKeyProvider) -> Result<Self> {
        let store = Store::open(&options.store_path, keys)?;
        let grid: GridSettings = store.setting()?;

        let (handle, owned) = match options.runtime {
            Some(h) => (h, None),
            None => {
                let rt = tokio::runtime::Builder::new_multi_thread()
                    .thread_name("fastgrid-rt")
                    .enable_all()
                    .build()
                    .map_err(|e| Error::Other(format!("cannot start the runtime: {e}")))?;
                let handle = rt.handle().clone();
                (handle, Some(SharedRuntime::new(rt)))
            }
        };
        let store = Arc::new(Mutex::new(store));
        let recorder = Recorder::start()?;
        let (events, _) = broadcast::channel(256);
        Ok(Self {
            inner: Arc::new(Inner {
                rt: Rt {
                    handle,
                    shared: owned.clone(),
                },
                _owned_runtime: owned,
                store,
                open: Mutex::new(HashMap::new()),
                open_gate: tokio::sync::Mutex::new(()),
                epochs: Arc::new(Mutex::new(HashMap::new())),
                connecting: Mutex::new(HashMap::new()),
                open_pause: Mutex::new(None),
                save_pause: Mutex::new(None),
                recorder,
                events,
                max_cell_bytes: Arc::new(AtomicUsize::new(grid.max_cell_bytes as usize)),
                max_sessions: options.max_sessions_per_connection,
                metadata_ttl: options.metadata_ttl,
                force_read_only: options.force_read_only,
            }),
        })
    }

    /// Block until history and audit jobs queued so far have been written.
    ///
    /// Returns a storage error when any of those writes failed. Draining the queue is not the
    /// same as every write succeeding.
    pub fn drain_records(&self) -> Result<()> {
        self.inner.recorder.drain()
    }

    /// Test seam. The next `open` pauses after reading the config, before connecting, so a test
    /// can save a stricter config in between.
    #[cfg(any(test, feature = "test-seams"))]
    pub fn _test_pause_next_open(&self) -> OpenPause {
        let (reached_tx, reached_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = tokio::sync::oneshot::channel();
        if let Ok(mut slot) = self.inner.open_pause.lock() {
            *slot = Some(OpenPauseInner {
                reached: Some(reached_tx),
                release: release_rx,
            });
        }
        OpenPause {
            reached: Some(reached_rx),
            release: release_tx,
        }
    }

    /// Test seam. The next history prune returns an error after the audit row is written.
    #[cfg(any(test, feature = "test-seams"))]
    pub fn _test_inject_history_prune_failure(&self) {
        self.inner.recorder.fail_next_prune();
    }

    /// Test seam. The next history append fails, and the audit append is still attempted.
    #[cfg(any(test, feature = "test-seams"))]
    pub fn _test_fail_next_history(&self) {
        self.inner.recorder.fail_next_history();
    }

    /// Test seam. Poisons the store mutex so a settings read fails closed.
    #[cfg(any(test, feature = "test-seams"))]
    pub fn _test_poison_store(&self) {
        let store = Arc::clone(&self.inner.store);
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _guard = store.lock().unwrap_or_else(|err| err.into_inner());
            panic!("test poison");
        }));
    }

    /// Test seam. The next save pauses inside the service-owned task, before it writes, so a test
    /// can drop the caller's future and still observe the invalidation.
    #[cfg(any(test, feature = "test-seams"))]
    pub fn _test_pause_save(&self) -> SavePause {
        let (reached_tx, reached_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = tokio::sync::oneshot::channel();
        let (done_tx, done_rx) = tokio::sync::oneshot::channel();
        if let Ok(mut slot) = self.inner.save_pause.lock() {
            *slot = Some(SavePauseInner {
                reached: Some(reached_tx),
                release: release_rx,
                done: Some(done_tx),
            });
        }
        SavePause {
            reached: Some(reached_rx),
            release: Some(release_tx),
            done: Some(done_rx),
        }
    }

    /// Test seam. The next statement on this connection pauses after the lease and before execute.
    #[cfg(any(test, feature = "test-seams"))]
    pub fn _test_pause_before_execute(&self, id: &ConnectionId) -> Result<pool::ExecutePause> {
        let conn = self
            .get_open(id)
            .ok_or_else(|| Error::Connection("no open connection to pause".into()))?;
        Ok(conn.pool.arm_execute_pause())
    }

    /// Test seam. Write a settings payload that decrypts but does not deserialize.
    #[cfg(any(test, feature = "test-seams"))]
    pub async fn _test_write_unreadable_setting(&self, key: &str, json: &str) -> Result<()> {
        let key = key.to_owned();
        let json = json.to_owned();
        self.with_store(move |store| store.write_raw_record(fg_store::kinds::SETTING, &key, &json))
            .await
    }

    /// Statement-timeout tasks still running. Zero once a finished run has stopped its timer.
    #[cfg(any(test, feature = "test-seams"))]
    pub fn _test_live_statement_timers() -> usize {
        run::live_statement_timers()
    }

    pub fn subscribe(&self) -> broadcast::Receiver<ServiceEvent> {
        self.inner.events.subscribe()
    }

    fn emit(&self, event: ServiceEvent) {
        // No listeners is fine.
        let _ = self.inner.events.send(event);
    }

    /// Run a store operation on the blocking pool (the store is synchronous).
    async fn with_store<T, F>(&self, f: F) -> Result<T>
    where
        T: Send + 'static,
        F: FnOnce(&Store) -> Result<T> + Send + 'static,
    {
        let store = Arc::clone(&self.inner.store);
        self.inner
            .rt
            .handle
            .spawn_blocking(move || {
                let guard = store.lock().map_err(|_| lock_error())?;
                f(&guard)
            })
            .await
            .map_err(|e| Error::Other(format!("background task failed: {e}")))?
    }

    // ---- saved connections -------------------------------------------------------------------

    pub async fn connections(&self) -> Result<Vec<StoredConnection>> {
        self.with_store(Store::load_connections).await
    }

    pub async fn groups(&self) -> Result<Vec<ConnectionGroup>> {
        self.with_store(Store::load_groups).await
    }

    pub async fn save_group(&self, group: ConnectionGroup) -> Result<ConnectionGroup> {
        let saved = self.with_store(move |s| s.save_group(group)).await?;
        self.emit(ServiceEvent::ConnectionsChanged);
        Ok(saved)
    }

    pub async fn delete_group(&self, id: GroupId) -> Result<bool> {
        let deleted = self.with_store(move |s| s.delete_group(&id)).await?;
        self.emit(ServiceEvent::ConnectionsChanged);
        Ok(deleted)
    }

    /// Save a connection and, optionally, its password. An open connection with the old settings
    /// is closed and its in-flight statements are cancelled, so the next open picks up the change.
    ///
    /// Persist, generation bump and close run in a task the service owns. Dropping this future
    /// does not leave the new policy on disk with the old connection still published. The config
    /// and password writes share one store transaction.
    ///
    /// A statement that already passed the policy check on a leased session is cancelled with the
    /// pool. One that has not leased yet fails the generation check and does not execute. The
    /// window is the driver call that has already started and has not yet observed the interrupt;
    /// `close` sends that interrupt before returning.
    pub async fn save_connection(
        &self,
        config: ConnectionConfig,
        password: PasswordUpdate,
    ) -> Result<ConnectionConfig> {
        self.save_connection_with_secrets(config, password, PasswordUpdate::Keep)
            .await
    }

    /// Save a connection with both its secrets: the database password and,
    /// for a tunneled connection, the SSH password or key passphrase. Both
    /// write in the same store transaction as the config.
    pub async fn save_connection_with_secrets(
        &self,
        config: ConnectionConfig,
        password: PasswordUpdate,
        ssh_secret: PasswordUpdate,
    ) -> Result<ConnectionConfig> {
        let inner = Arc::clone(&self.inner);
        let task = self.inner.rt.handle.spawn(async move {
            persist_and_invalidate(inner, config, password, ssh_secret).await
        });
        match task.await {
            Ok(result) => result,
            Err(err) => Err(Error::Other(format!("save task failed: {err}"))),
        }
    }

    /// Delete a connection with its password, history, drafts and UI state.
    ///
    /// In-flight statements are cancelled, same as [`Self::save_connection`].
    pub async fn delete_connection(&self, id: &ConnectionId) -> Result<bool> {
        let _gate = self.inner.open_gate.lock().await;
        self.bump_epoch(id);
        self.close(id);
        let key = id.clone();
        let deleted = self.with_store(move |s| s.delete_connection(&key)).await?;
        self.emit(ServiceEvent::ConnectionsChanged);
        Ok(deleted)
    }

    /// Open an SSH tunnel for `config` when it carries SSH settings, and return
    /// the config the driver should actually connect to (localhost forward,
    /// SSH cleared) plus the tunnel that must stay alive for the session.
    /// Without SSH settings this is a no-op clone with no tunnel.
    async fn tunnel_for(
        &self,
        config: &ConnectionConfig,
        ssh_secret: Option<String>,
    ) -> Result<(ConnectionConfig, Option<ssh::SshTunnel>)> {
        let Some(ssh) = &config.ssh else {
            return Ok((config.clone(), None));
        };
        let (target_host, target_port) = match &config.params {
            ConnectionParams::Network { host, port, .. } => (host.clone(), *port),
            ConnectionParams::File { .. } => {
                return Err(Error::Connection(
                    "an SSH tunnel requires a network connection".into(),
                ));
            }
        };
        let tunnel =
            ssh::SshTunnel::open(ssh, ssh_secret.as_deref(), &target_host, target_port).await?;
        let mut pool_config = config.clone();
        if let ConnectionParams::Network {
            host, port, socket, ..
        } = &mut pool_config.params
        {
            *host = "127.0.0.1".to_owned();
            *port = tunnel.local_port();
            *socket = None;
        }
        pool_config.ssh = None;
        Ok((pool_config, Some(tunnel)))
    }

    /// Try a connection without saving it. `password` is the typed (not yet stored) one, and
    /// `ssh_secret` is the typed SSH password or key passphrase for a tunneled test.
    pub async fn test_connection(
        &self,
        config: ConnectionConfig,
        password: Option<String>,
        ssh_secret: Option<String>,
    ) -> Result<TestReport> {
        // The tunnel must stay alive for the whole test; it drops with the pool.
        let (pool_config, _tunnel) = self.tunnel_for(&config, ssh_secret).await?;
        let pool = SessionPool::new(
            driver_for(pool_config.kind)?,
            pool_config,
            password,
            1,
            0,
            Arc::new(Mutex::new(HashMap::new())),
        );
        let driver_pool = Arc::clone(&pool);
        let report = self
            .inner
            .rt
            .run(async move {
                let session = driver_pool.open_session().await?;
                let started = std::time::Instant::now();
                session.ping().await?;
                let latency = started.elapsed();
                Ok(TestReport {
                    latency,
                    server_version: session.server_version().await?,
                })
            })
            .await;
        pool.close();
        report
    }

    // ---- open connections --------------------------------------------------------------------

    /// Open a saved connection (or return it if already open): reads the password from the
    /// encrypted store, checks that a session can be established, records the use.
    ///
    /// Open, save and delete share one lock around the config read and the publish. The connect
    /// itself does not hold that lock (it can take as long as the connect timeout), so a save
    /// that lands in between bumps the generation and the stale pool is discarded instead of
    /// published.
    pub async fn open(&self, id: &ConnectionId) -> Result<Arc<OpenConnection>> {
        for _ in 0..8 {
            if let Some(existing) = self.get_open(id) {
                return Ok(existing);
            }
            let (config, password, epoch) = {
                let _gate = self.inner.open_gate.lock().await;
                if let Some(existing) = self.get_open(id) {
                    return Ok(existing);
                }
                let epoch = self.epoch_of(id);
                let key = id.clone();
                let (stored, password) = self
                    .with_store(move |s| {
                        let stored = s.get_connection(&key)?;
                        let password = s.get_secret(&key, SecretKind::DatabasePassword)?;
                        Ok((stored, password))
                    })
                    .await?;
                let mut config = match stored {
                    Some(StoredConnection::Ok(c)) => c,
                    Some(StoredConnection::Unreadable { reason, .. }) => {
                        return Err(Error::Unsupported(format!(
                            "this connection was saved by a newer version and cannot be read ({reason})"
                        )));
                    }
                    None => return Err(Error::Other("no such connection".into())),
                };
                if self.inner.force_read_only {
                    config.safe_mode = Some(SafeMode::ReadOnly);
                }
                (config, password, epoch)
            };
            self.wait_open_pause().await;
            if self.epoch_of(id) != epoch {
                continue;
            }

            let driver = driver_for(config.kind)?;
            // An SSH tunnel rewrites the pool's endpoint to the local forward:
            // the driver keeps seeing a plain TCP host and port, and the
            // original config (with its SSH settings) stays on the connection
            // for display. The tunnel lives on the connection, so it outlives
            // every pooled session and closes when the connection closes.
            let ssh_secret = self.load_ssh_secret(&config).await?;
            let (pool_config, tunnel) = self.tunnel_for(&config, ssh_secret).await?;
            let pool = SessionPool::new(
                Arc::clone(&driver),
                pool_config,
                password,
                pool_size(&config, self.inner.max_sessions),
                epoch,
                Arc::clone(&self.inner.epochs),
            );
            let conn = OpenConnection::new(
                self.inner.rt.clone(),
                config,
                driver,
                Arc::clone(&pool),
                tunnel,
                Arc::clone(&self.inner.store),
                self.inner.metadata_ttl,
                Arc::clone(&self.inner.max_cell_bytes),
                self.inner.events.clone(),
                self.inner.recorder.clone(),
            );
            self.track_connecting(id, &pool);
            let ping = conn.measure_ping().await;
            self.untrack_connecting(id, &pool);
            if self.epoch_of(id) != epoch {
                conn.close();
                continue;
            }
            ping?;

            let _gate = self.inner.open_gate.lock().await;
            if self.epoch_of(id) != epoch {
                conn.close();
                continue;
            }
            if let Some(existing) = self.get_open(id) {
                conn.close();
                return Ok(existing);
            }
            let touched = id.clone();
            self.with_store(move |s| s.touch_connection(&touched))
                .await?;
            if self.epoch_of(id) != epoch {
                conn.close();
                continue;
            }
            if let Ok(mut open) = self.inner.open.lock() {
                open.insert(id.clone(), Arc::clone(&conn));
            }
            self.emit(ServiceEvent::Connected(id.clone()));
            return Ok(conn);
        }
        Err(Error::Connection(
            "connection settings changed while opening; retry".into(),
        ))
    }

    /// Reconnect this connection to another database on the same server, without changing the
    /// saved connection: the stored config keeps its old database, so reopening the connection
    /// later returns to it. The old pool is closed and replaced; anyone holding the old
    /// `Arc<OpenConnection>` must drop it (the app's workspace does this by reading the current
    /// connection from a shared cell on every event). Only network connections have databases
    /// to switch between. If the new database cannot be reached, the old database is restored
    /// so a typo'd name does not wedge the connection.
    pub async fn switch_database(
        &self,
        id: &ConnectionId,
        database: &str,
    ) -> Result<Arc<OpenConnection>> {
        let key = id.clone();
        let (stored, password) = self
            .with_store(move |s| {
                let stored = s.get_connection(&key)?;
                let password = s.get_secret(&key, SecretKind::DatabasePassword)?;
                Ok((stored, password))
            })
            .await?;
        let mut config = match stored {
            Some(StoredConnection::Ok(c)) => c,
            Some(StoredConnection::Unreadable { reason, .. }) => {
                return Err(Error::Unsupported(format!(
                    "this connection was saved by a newer version and cannot be read ({reason})"
                )));
            }
            None => return Err(Error::Other("no such connection".into())),
        };
        if self.inner.force_read_only {
            config.safe_mode = Some(SafeMode::ReadOnly);
        }
        let original = config.clone();
        match &mut config.params {
            ConnectionParams::Network { database: db, .. } => {
                *db = Some(database.to_owned());
            }
            ConnectionParams::File { .. } => {
                return Err(Error::Unsupported(
                    "database switching is only available for server connections".into(),
                ));
            }
        }
        // Pools snapshot the epoch at construction and refuse to hand out
        // sessions once it moves, so the bump comes before building.
        self.bump_epoch(id);
        let epoch = self.epoch_of(id);
        match self.connect_with(config, password.clone(), epoch).await {
            Ok(conn) => {
                self.swap_open(id, conn.clone());
                self.emit(ServiceEvent::Connected(id.clone()));
                Ok(conn)
            }
            Err(e) => {
                // The old pool is epoch-invalid now; put the original database back.
                self.bump_epoch(id);
                let epoch = self.epoch_of(id);
                match self.connect_with(original, password, epoch).await {
                    Ok(conn) => {
                        self.swap_open(id, conn);
                        self.emit(ServiceEvent::Connected(id.clone()));
                    }
                    Err(_) => {
                        // Last resort: drop the wedged pool so a fresh open works.
                        self.close(id);
                    }
                }
                Err(e)
            }
        }
    }

    /// Build and ping a pool for `config` without registering it: the caller
    /// owns the epoch and the swap into the open map.
    async fn connect_with(
        &self,
        config: ConnectionConfig,
        password: Option<String>,
        epoch: u64,
    ) -> Result<Arc<OpenConnection>> {
        let driver = driver_for(config.kind)?;
        let ssh_secret = self.load_ssh_secret(&config).await?;
        let (pool_config, tunnel) = self.tunnel_for(&config, ssh_secret).await?;
        let pool = SessionPool::new(
            Arc::clone(&driver),
            pool_config,
            password,
            pool_size(&config, self.inner.max_sessions),
            epoch,
            Arc::clone(&self.inner.epochs),
        );
        let conn = OpenConnection::new(
            self.inner.rt.clone(),
            config,
            driver,
            Arc::clone(&pool),
            tunnel,
            Arc::clone(&self.inner.store),
            self.inner.metadata_ttl,
            Arc::clone(&self.inner.max_cell_bytes),
            self.inner.events.clone(),
            self.inner.recorder.clone(),
        );
        conn.measure_ping().await?;
        Ok(conn)
    }

    /// Replace the open pool for `id`, closing whatever was there.
    fn swap_open(&self, id: &ConnectionId, conn: Arc<OpenConnection>) {
        let old = self
            .inner
            .open
            .lock()
            .ok()
            .and_then(|mut open| open.insert(id.clone(), conn));
        if let Some(old) = old {
            old.close();
        }
    }

    /// SSH secret for a connection: the SSH password, or the key passphrase.
    /// Agent auth needs no secret. A missing secret is `None` here; the tunnel
    /// open reports it with the auth method attached.
    async fn load_ssh_secret(&self, config: &ConnectionConfig) -> Result<Option<String>> {
        let Some(ssh) = &config.ssh else {
            return Ok(None);
        };
        let kind = match ssh.auth {
            fg_core::SshAuth::Password => SecretKind::SshPassword,
            fg_core::SshAuth::PrivateKey { .. } => SecretKind::SshKeyPassphrase,
            fg_core::SshAuth::Agent => return Ok(None),
        };
        let key = config.id.clone();
        self.with_store(move |s| s.get_secret(&key, kind)).await
    }

    fn get_open(&self, id: &ConnectionId) -> Option<Arc<OpenConnection>> {
        self.inner.open.lock().ok()?.get(id).cloned()
    }

    pub fn open_connections(&self) -> Vec<ConnectionId> {
        self.inner
            .open
            .lock()
            .map(|o| o.keys().cloned().collect())
            .unwrap_or_default()
    }

    pub fn close(&self, id: &ConnectionId) {
        let removed = self.inner.open.lock().ok().and_then(|mut o| o.remove(id));
        if let Some(conn) = removed {
            conn.close();
            self.emit(ServiceEvent::Disconnected(id.clone()));
        }
        self.close_connecting(id);
    }

    pub fn close_all(&self) {
        for id in self.open_connections() {
            self.close(&id);
        }
    }

    // ---- UI state and settings ---------------------------------------------------------------

    pub async fn load_workspace(&self, id: &ConnectionId) -> Result<Option<WorkspaceState>> {
        let id = id.clone();
        self.with_store(move |s| s.load_workspace(&id)).await
    }

    pub async fn save_workspace(&self, id: &ConnectionId, state: WorkspaceState) -> Result<()> {
        let id = id.clone();
        self.with_store(move |s| s.save_workspace(&id, &state))
            .await
    }

    pub async fn load_table_view(
        &self,
        id: &ConnectionId,
        path: &ObjectPath,
    ) -> Result<Option<TableViewState>> {
        let (id, path) = (id.clone(), path.clone());
        self.with_store(move |s| s.load_table_view(&id, &path))
            .await
    }

    pub async fn save_table_view(
        &self,
        id: &ConnectionId,
        path: &ObjectPath,
        state: TableViewState,
    ) -> Result<()> {
        let (id, path) = (id.clone(), path.clone());
        self.with_store(move |s| s.save_table_view(&id, &path, &state))
            .await
    }

    pub async fn setting<T: SettingGroup + Send + 'static>(&self) -> Result<T> {
        self.with_store(|s| s.setting::<T>()).await
    }

    pub async fn set_setting<T: SettingGroup + Send + 'static>(&self, value: T) -> Result<()> {
        self.with_store(move |s| s.set_setting(&value)).await
    }

    /// Grid settings also feed the cell-size limit of open connections.
    pub async fn set_grid_settings(&self, grid: GridSettings) -> Result<()> {
        self.inner
            .max_cell_bytes
            .store(grid.max_cell_bytes as usize, Ordering::Relaxed);
        self.set_setting(grid).await
    }

    /// Row cap, statement timeout and history retention for [`OpenConnection::run`].
    ///
    /// Read on every run, so a change applies to connections that are already open. The default
    /// cap is what makes the documented server-side limit real when a caller passes none.
    pub async fn exec_settings(&self) -> Result<ExecSettings> {
        self.with_store(|s| s.setting_strict::<ExecSettings>())
            .await
    }

    /// Persist [`ExecSettings`]. Named so callers do not have to know the settings-group key.
    pub async fn set_exec_settings(&self, settings: ExecSettings) -> Result<()> {
        self.set_setting(settings).await
    }

    /// Newest query history for one connection. `search` is a case-insensitive substring of the
    /// SQL text; matching happens after decrypt, so it cannot be pushed into SQLite.
    pub async fn history(
        &self,
        id: &ConnectionId,
        search: Option<&str>,
        limit: usize,
    ) -> Result<Vec<QueryHistoryEntry>> {
        let id = id.clone();
        let search = search.map(str::to_owned);
        self.with_store(move |s| s.recent_history(&id, search.as_deref(), limit))
            .await
    }

    /// Tombstone every history entry of a connection. Returns how many rows were live.
    pub async fn clear_history(&self, id: &ConnectionId) -> Result<usize> {
        let id = id.clone();
        self.with_store(move |s| s.clear_history(&id)).await
    }

    /// Append one audit row. `run` writes its own; this is for decisions that never became a
    /// run (a confirm dialog that timed out, for example).
    pub async fn record_audit(&self, entry: AuditEntry) -> Result<()> {
        self.with_store(move |s| s.append_audit(&entry)).await
    }

    /// Newest audit entries matching `filter`. The log is append-only; this does not delete.
    pub async fn query_audit(&self, filter: AuditFilter) -> Result<Vec<AuditEntry>> {
        self.with_store(move |s| s.query_audit(&filter)).await
    }

    /// Drafts of one connection, newest `updated_at` first. Unreadable rows are included.
    pub async fn documents(&self, id: &ConnectionId) -> Result<Vec<StoredDocument>> {
        let id = id.clone();
        self.with_store(move |s| s.documents(&id)).await
    }

    /// Insert or replace a query-tab draft. The caller owns the timestamps.
    pub async fn save_document(&self, doc: QueryDocument) -> Result<()> {
        self.with_store(move |s| s.save_document(&doc)).await
    }

    /// Tombstone a draft. `false` when there was no live row. The crash journal goes with it:
    /// a deleted draft must not resurrect through its journal.
    pub async fn delete_document(&self, id: &DocumentId) -> Result<bool> {
        let id = id.clone();
        self.with_store(move |s| {
            let deleted = s.delete_document(&id)?;
            let _ = s.delete_journal(&id);
            Ok(deleted)
        })
        .await
    }

    /// Overwrite the crash journal for a draft: the newest unsaved buffer, written on every
    /// edit and cleared by the next successful save. Best-effort scratch; the saved draft is
    /// the authority, so callers ignore failures here.
    pub async fn save_journal(&self, doc: QueryDocument) -> Result<()> {
        self.with_store(move |s| s.save_journal(&doc)).await
    }

    /// The journal entry for a draft, if any. An unreadable row is reported, not hidden, so
    /// the caller can treat it as absent and never restore from it.
    pub async fn journal(&self, id: &DocumentId) -> Result<Option<StoredDocument>> {
        let id = id.clone();
        self.with_store(move |s| s.journal(&id)).await
    }

    /// Tombstone the journal entry. `false` when there was no live row.
    pub async fn delete_journal(&self, id: &DocumentId) -> Result<bool> {
        let id = id.clone();
        self.with_store(move |s| s.delete_journal(&id)).await
    }

    /// Snippets visible for `connection` (globals plus that connection's own). `None` is every
    /// snippet, so a settings screen can list them without opening a database. Unreadable rows
    /// are included.
    pub async fn snippets(&self, connection: Option<&ConnectionId>) -> Result<Vec<StoredSnippet>> {
        let connection = connection.cloned();
        self.with_store(move |s| s.snippets(connection.as_ref()))
            .await
    }

    /// Insert or replace a snippet. A missing connection id stores it as global.
    pub async fn save_snippet(&self, snippet: Snippet) -> Result<()> {
        self.with_store(move |s| s.save_snippet(&snippet)).await
    }

    /// Tombstone a snippet. `false` when there was no live row.
    pub async fn delete_snippet(&self, id: &SnippetId) -> Result<bool> {
        let id = id.clone();
        self.with_store(move |s| s.delete_snippet(&id)).await
    }
}

impl Service {
    fn epoch_of(&self, id: &ConnectionId) -> u64 {
        self.inner
            .epochs
            .lock()
            .unwrap_or_else(|err| err.into_inner())
            .get(id)
            .copied()
            .unwrap_or(0)
    }

    fn bump_epoch(&self, id: &ConnectionId) {
        let mut epochs = self
            .inner
            .epochs
            .lock()
            .unwrap_or_else(|err| err.into_inner());
        let epoch = epochs.entry(id.clone()).or_insert(0);
        *epoch = epoch.saturating_add(1);
    }

    async fn wait_open_pause(&self) {
        let pause = self
            .inner
            .open_pause
            .lock()
            .ok()
            .and_then(|mut slot| slot.take());
        if let Some(mut pause) = pause {
            if let Some(reached) = pause.reached.take() {
                let _ = reached.send(());
            }
            let _ = pause.release.await;
        }
    }

    fn track_connecting(&self, id: &ConnectionId, pool: &Arc<SessionPool>) {
        if let Ok(mut connecting) = self.inner.connecting.lock() {
            connecting
                .entry(id.clone())
                .or_default()
                .push(Arc::clone(pool));
        }
    }

    fn untrack_connecting(&self, id: &ConnectionId, pool: &Arc<SessionPool>) {
        if let Ok(mut connecting) = self.inner.connecting.lock()
            && let Some(pools) = connecting.get_mut(id)
        {
            pools.retain(|candidate| !Arc::ptr_eq(candidate, pool));
            if pools.is_empty() {
                connecting.remove(id);
            }
        }
    }

    fn close_connecting(&self, id: &ConnectionId) {
        let pools = self
            .inner
            .connecting
            .lock()
            .ok()
            .and_then(|mut connecting| connecting.remove(id));
        if let Some(pools) = pools {
            for pool in pools {
                pool.close();
            }
        }
    }
}

/// In-memory SQLite databases exist per connection, so a second pooled session would see an empty
/// database: they get exactly one session.
async fn persist_and_invalidate(
    inner: Arc<Inner>,
    config: ConnectionConfig,
    password: PasswordUpdate,
    ssh_secret: PasswordUpdate,
) -> Result<ConnectionConfig> {
    let id = config.id.clone();
    let _gate = inner.open_gate.lock().await;
    let done = {
        let pause = inner
            .save_pause
            .lock()
            .ok()
            .and_then(|mut slot| slot.take());
        if let Some(mut pause) = pause {
            if let Some(reached) = pause.reached.take() {
                let _ = reached.send(());
            }
            let _ = pause.release.await;
            pause.done
        } else {
            None
        }
    };
    let saved = with_store_on(&inner, move |store| {
        let password = match password {
            PasswordUpdate::Keep => fg_store::ConnectionPassword::Keep,
            PasswordUpdate::Set(value) => fg_store::ConnectionPassword::Set(value),
            PasswordUpdate::Clear => fg_store::ConnectionPassword::Clear,
        };
        let saved = store.save_connection_with_password(config, password)?;
        // The SSH secret lives beside the database password, keyed by what the
        // auth method needs: the SSH password, or the key passphrase. Agent
        // auth and connections without SSH have no secret to store.
        let kind = match &saved.ssh {
            Some(ssh) => match ssh.auth {
                fg_core::SshAuth::Password => Some(SecretKind::SshPassword),
                fg_core::SshAuth::PrivateKey { .. } => Some(SecretKind::SshKeyPassphrase),
                fg_core::SshAuth::Agent => None,
            },
            None => None,
        };
        if let Some(kind) = kind {
            match ssh_secret {
                PasswordUpdate::Keep => {}
                PasswordUpdate::Set(value) => {
                    store.set_secret(&saved.id, kind, &value)?;
                }
                PasswordUpdate::Clear => {
                    store.delete_secret(&saved.id, kind)?;
                }
            }
        }
        Ok(saved)
    })
    .await?;
    bump_epoch_on(&inner, &id);
    close_on(&inner, &id);
    let _ = inner.events.send(ServiceEvent::ConnectionsChanged);
    if let Some(done) = done {
        let _ = done.send(());
    }
    Ok(saved)
}

async fn with_store_on<T, F>(inner: &Inner, f: F) -> Result<T>
where
    T: Send + 'static,
    F: FnOnce(&Store) -> Result<T> + Send + 'static,
{
    let store = Arc::clone(&inner.store);
    inner
        .rt
        .handle
        .spawn_blocking(move || {
            let guard = store.lock().map_err(|_| lock_error())?;
            f(&guard)
        })
        .await
        .map_err(|e| Error::Other(format!("background task failed: {e}")))?
}

fn bump_epoch_on(inner: &Inner, id: &ConnectionId) {
    let mut epochs = inner.epochs.lock().unwrap_or_else(|err| err.into_inner());
    let epoch = epochs.entry(id.clone()).or_insert(0);
    *epoch = epoch.saturating_add(1);
}

fn close_on(inner: &Inner, id: &ConnectionId) {
    let removed = inner.open.lock().ok().and_then(|mut open| open.remove(id));
    if let Some(conn) = removed {
        conn.close();
        let _ = inner.events.send(ServiceEvent::Disconnected(id.clone()));
    }
    let pools = inner
        .connecting
        .lock()
        .ok()
        .and_then(|mut connecting| connecting.remove(id));
    if let Some(pools) = pools {
        for pool in pools {
            pool.close();
        }
    }
}

fn pool_size(config: &ConnectionConfig, default: usize) -> usize {
    match &config.params {
        ConnectionParams::File { path } if path == ":memory:" => 1,
        _ => default,
    }
}

#[cfg(test)]
mod tests {
    use fg_core::{ConnectionConfig, ConnectionParams, DbKind, Error, SafeMode, Value};
    use fg_driver::ExecOptions;
    use fg_store::StoredConnection;

    use super::*;

    /// `PRAGMA query_only = OFF` is not an allow-listed session setting. Open must refuse it
    /// before the statement can clear the engine guard, and the rows written earlier must remain.
    #[tokio::test]
    async fn read_only_is_reasserted_after_startup_sql() {
        let dir = std::env::temp_dir().join(format!(
            "fg-reassert-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let keys = KeychainKeyProvider::with_backend(MemoryBackend::default());
        let svc = Service::start(ServiceOptions::new(dir.join("store.db")), &keys).unwrap();
        let mut cfg = ConnectionConfig::new(
            "db",
            DbKind::Sqlite,
            ConnectionParams::File {
                path: dir.join("db.db").to_string_lossy().into_owned(),
            },
        );
        cfg.options.insert("create".into(), "true".into());
        let conn = {
            let saved = svc
                .save_connection(cfg, PasswordUpdate::Keep)
                .await
                .unwrap();
            let conn = svc.open(&saved.id).await.unwrap();
            conn.execute(
                "CREATE TABLE t (n INTEGER); INSERT INTO t VALUES (1)",
                &[],
                ExecOptions::default(),
            )
            .await
            .unwrap()
            .collect()
            .await
            .unwrap();
            let id = conn.id().clone();
            drop(conn);
            svc.close(&id);
            id
        };
        let mut cfg = match svc.connections().await.unwrap().remove(0) {
            StoredConnection::Ok(c) => c,
            other => panic!("{other:?}"),
        };
        cfg.safe_mode = Some(SafeMode::ReadOnly);
        cfg.startup_sql = vec!["PRAGMA query_only = OFF".into()];
        svc.save_connection(cfg, PasswordUpdate::Keep)
            .await
            .unwrap();
        let err = match svc.open(&conn).await {
            Err(err) => err,
            Ok(_) => panic!("query_only startup must be refused"),
        };
        assert!(matches!(err, Error::PolicyDenied(_)), "{err:?}");
        let mut plain = match svc.connections().await.unwrap().remove(0) {
            StoredConnection::Ok(c) => c,
            other => panic!("{other:?}"),
        };
        plain.safe_mode = Some(SafeMode::Normal);
        plain.startup_sql.clear();
        svc.save_connection(plain, PasswordUpdate::Keep)
            .await
            .unwrap();
        let conn = svc.open(&conn).await.unwrap();
        let sets = conn
            .execute("SELECT n FROM t", &[], ExecOptions::default())
            .await
            .unwrap()
            .collect()
            .await
            .unwrap();
        assert_eq!(sets[0].rows[0][0], Value::Int(1));
        drop(conn);
        drop(svc);
        let mut last_err = None;
        for _ in 0..40 {
            match std::fs::remove_dir_all(&dir) {
                Ok(()) => return,
                Err(_) if !dir.exists() => return,
                Err(err) => last_err = Some(err),
            }
            std::thread::sleep(std::time::Duration::from_millis(25));
        }
        panic!("cleanup failed after dropping every handle: {last_err:?}");
    }
}
