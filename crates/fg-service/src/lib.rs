//! The application service layer, shared by the GUI and the MCP server.
//!
//! It owns: the encrypted local store (and the device key in the OS keychain), the tokio runtime,
//! open connections with their session pools and metadata caches, and the lifecycle of running
//! statements (streaming, cancelling). Everything here is headless; `fg-app` and `fg-mcp` are
//! thin clients of it.
//!
//! All public methods are `async` and may be awaited from any executor: they hop onto the
//! service's own tokio runtime internally.

#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

mod cache;
mod conn;
mod keychain;
mod pool;
mod task;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use fg_core::{
    ConnectionConfig, ConnectionGroup, ConnectionId, ConnectionParams, Error, GridSettings,
    GroupId, ObjectPath, Result, SecretKind, SettingGroup, TableViewState, WorkspaceState,
};
use fg_driver::driver_for;
use fg_store::{DeviceKeyProvider, Store, StoredConnection};
use tokio::runtime::{Handle, Runtime};
use tokio::sync::broadcast;

pub use conn::{BrowseOptions, OpenConnection, Refresh};
pub use keychain::{KeychainKeyProvider, MemoryBackend, OsKeychain, SecretBackend, account_name};
pub use task::{TaskCanceller, TaskStream};

use conn::Rt;
use pool::SessionPool;

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
}

impl ServiceOptions {
    pub fn new(store_path: impl Into<PathBuf>) -> Self {
        Self {
            store_path: store_path.into(),
            runtime: None,
            max_sessions_per_connection: 4,
            metadata_ttl: Duration::from_secs(60),
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

/// Dropping the service must not drop a runtime from inside async code (tokio panics); shut it
/// down in the background instead.
struct OwnedRuntime(Option<Runtime>);

impl Drop for OwnedRuntime {
    fn drop(&mut self) {
        if let Some(rt) = self.0.take() {
            rt.shutdown_background();
        }
    }
}

struct Inner {
    rt: Rt,
    _owned_runtime: OwnedRuntime,
    store: Arc<Mutex<Store>>,
    open: Mutex<HashMap<ConnectionId, Arc<OpenConnection>>>,
    open_gate: tokio::sync::Mutex<()>,
    events: broadcast::Sender<ServiceEvent>,
    max_cell_bytes: Arc<AtomicUsize>,
    max_sessions: usize,
    metadata_ttl: Duration,
}

#[derive(Clone)]
pub struct Service {
    inner: Arc<Inner>,
}

fn lock_error() -> Error {
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
                (rt.handle().clone(), Some(rt))
            }
        };
        let (events, _) = broadcast::channel(256);
        Ok(Self {
            inner: Arc::new(Inner {
                rt: Rt { handle },
                _owned_runtime: OwnedRuntime(owned),
                store: Arc::new(Mutex::new(store)),
                open: Mutex::new(HashMap::new()),
                open_gate: tokio::sync::Mutex::new(()),
                events,
                max_cell_bytes: Arc::new(AtomicUsize::new(grid.max_cell_bytes as usize)),
                max_sessions: options.max_sessions_per_connection,
                metadata_ttl: options.metadata_ttl,
            }),
        })
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
    /// is closed, so the next open picks up the change.
    pub async fn save_connection(
        &self,
        config: ConnectionConfig,
        password: PasswordUpdate,
    ) -> Result<ConnectionConfig> {
        let id = config.id.clone();
        let saved = self
            .with_store(move |s| {
                let saved = s.save_connection(config)?;
                match password {
                    PasswordUpdate::Keep => {}
                    PasswordUpdate::Set(p) => {
                        s.set_secret(&saved.id, SecretKind::DatabasePassword, &p)?;
                    }
                    PasswordUpdate::Clear => {
                        s.delete_secret(&saved.id, SecretKind::DatabasePassword)?;
                    }
                }
                Ok(saved)
            })
            .await?;
        self.close(&id);
        self.emit(ServiceEvent::ConnectionsChanged);
        Ok(saved)
    }

    /// Delete a connection with its password, history, drafts and UI state.
    pub async fn delete_connection(&self, id: &ConnectionId) -> Result<bool> {
        self.close(id);
        let key = id.clone();
        let deleted = self.with_store(move |s| s.delete_connection(&key)).await?;
        self.emit(ServiceEvent::ConnectionsChanged);
        Ok(deleted)
    }

    /// Try a connection without saving it. `password` is the typed (not yet stored) one.
    pub async fn test_connection(
        &self,
        config: ConnectionConfig,
        password: Option<String>,
    ) -> Result<TestReport> {
        let pool = SessionPool::new(driver_for(config.kind)?, config, password, 1);
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
    pub async fn open(&self, id: &ConnectionId) -> Result<Arc<OpenConnection>> {
        if let Some(existing) = self.get_open(id) {
            return Ok(existing);
        }
        let _gate = self.inner.open_gate.lock().await;
        if let Some(existing) = self.get_open(id) {
            return Ok(existing);
        }

        let key = id.clone();
        let (stored, password) = self
            .with_store(move |s| {
                let stored = s.get_connection(&key)?;
                let password = s.get_secret(&key, SecretKind::DatabasePassword)?;
                Ok((stored, password))
            })
            .await?;
        let config = match stored {
            Some(StoredConnection::Ok(c)) => c,
            Some(StoredConnection::Unreadable { reason, .. }) => {
                return Err(Error::Unsupported(format!(
                    "this connection was saved by a newer version and cannot be read ({reason})"
                )));
            }
            None => return Err(Error::Other("no such connection".into())),
        };

        let driver = driver_for(config.kind)?;
        let pool = SessionPool::new(
            Arc::clone(&driver),
            config.clone(),
            password,
            pool_size(&config, self.inner.max_sessions),
        );
        let conn = OpenConnection::new(
            self.inner.rt.clone(),
            config,
            driver,
            Arc::clone(&pool),
            self.inner.metadata_ttl,
            Arc::clone(&self.inner.max_cell_bytes),
            self.inner.events.clone(),
        );
        // Fail now, not on the first query: this connects, runs startup SQL, applies read-only.
        conn.measure_ping().await?;

        let touched = id.clone();
        self.with_store(move |s| s.touch_connection(&touched))
            .await?;
        if let Ok(mut open) = self.inner.open.lock() {
            open.insert(id.clone(), Arc::clone(&conn));
        }
        self.emit(ServiceEvent::Connected(id.clone()));
        Ok(conn)
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
}

/// In-memory SQLite databases exist per connection, so a second pooled session would see an empty
/// database: they get exactly one session.
fn pool_size(config: &ConnectionConfig, default: usize) -> usize {
    match &config.params {
        ConnectionParams::File { path } if path == ":memory:" => 1,
        _ => default,
    }
}
