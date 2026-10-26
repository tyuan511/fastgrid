//! Service-side sync: the engine holder, run status, and the async methods
//! the UI calls. The engine (with the unwrapped sync key) lives only in
//! service memory for the session: restarting the app drops it, and the user
//! joins again with the sync password. Changing sync settings drops the
//! engine too, so a retargeted backend can never silently sync to the wrong
//! vault with a stale engine.
//!
//! Lock order is engine state first, then the store. No path takes them in
//! the other order, so the two locks cannot deadlock.

use std::sync::Arc;

use fg_core::{Error, Result, SyncBackendKind, SyncSettings};
use fg_store::{Store, SyncCredentials};

use crate::Service;
use crate::lock_error;
use crate::sync::{
    PullReport, PushReport, SnapshotReport, SyncEngine, device_ids_from_keys, now_millis,
};
use crate::sync_backend::{LocalFolderBackend, SyncBackend};
use crate::sync_s3::{S3Backend, S3Config};
use crate::sync_webdav::{WebDavBackend, WebDavConfig};

/// What the last sync runs did. In memory only: persisting it would churn
/// synced records on every run.
#[derive(Debug, Clone, Default)]
pub struct SyncStatus {
    pub last_push: Option<PushReport>,
    pub last_pull: Option<PullReport>,
    pub last_snapshot: Option<SnapshotReport>,
    pub last_error: Option<String>,
    pub last_sync_at: Option<i64>,
}

/// One known device: a remote segment directory, or this device itself.
#[derive(Debug, Clone)]
pub struct SyncDevice {
    pub device_id: String,
    /// Last segment sequence consumed from this device (0 = never pulled).
    pub last_seq: i64,
    pub is_local: bool,
}

#[derive(Default)]
pub struct SyncState {
    pub engine: Option<SyncEngine>,
    pub status: SyncStatus,
}

/// Build the configured backend from settings plus saved credentials. The
/// sync password is never an input here: listing device directories and
/// opening the vault manifest need no key, and create/join take the password
/// separately.
pub(crate) fn backend_from(
    settings: &SyncSettings,
    credentials: &SyncCredentials,
) -> Result<Arc<dyn SyncBackend>> {
    match settings.backend {
        SyncBackendKind::LocalFolder => {
            if settings.local_folder.is_empty() {
                return Err(Error::InvalidInput(
                    "no local sync folder is configured".into(),
                ));
            }
            Ok(Arc::new(LocalFolderBackend::new(&settings.local_folder)))
        }
        SyncBackendKind::S3 => {
            let access = credentials
                .s3_access_key
                .as_deref()
                .ok_or_else(|| Error::InvalidInput("no S3 access key is saved for sync".into()))?;
            let secret = credentials
                .s3_secret_key
                .as_deref()
                .ok_or_else(|| Error::InvalidInput("no S3 secret key is saved for sync".into()))?;
            if settings.s3_endpoint.is_empty() || settings.s3_bucket.is_empty() {
                return Err(Error::InvalidInput(
                    "S3 endpoint and bucket must both be configured".into(),
                ));
            }
            Ok(Arc::new(S3Backend::new(S3Config {
                endpoint: settings.s3_endpoint.clone(),
                bucket: settings.s3_bucket.clone(),
                region: settings.s3_region.clone(),
                access_key: access.to_string(),
                secret_key: secret.to_string(),
            })))
        }
        SyncBackendKind::WebDav => {
            let username = credentials.webdav_username.as_deref().ok_or_else(|| {
                Error::InvalidInput("no WebDAV username is saved for sync".into())
            })?;
            let password = credentials.webdav_password.as_deref().ok_or_else(|| {
                Error::InvalidInput("no WebDAV password is saved for sync".into())
            })?;
            if settings.webdav_url.is_empty() {
                return Err(Error::InvalidInput("no WebDAV URL is configured".into()));
            }
            Ok(Arc::new(WebDavBackend::new(WebDavConfig {
                base_url: settings.webdav_url.clone(),
                username: username.to_string(),
                password: password.to_string(),
            })))
        }
        _ => Err(Error::Unsupported("unknown sync backend".into())),
    }
}

fn engine_required(state: &SyncState) -> Result<&SyncEngine> {
    state
        .engine
        .as_ref()
        .ok_or_else(|| Error::Other("sync is not set up; create or join a vault first".into()))
}

impl Service {
    async fn with_sync<T, F>(&self, f: F) -> Result<T>
    where
        T: Send + 'static,
        F: FnOnce(&mut SyncState, &Store) -> Result<T> + Send + 'static,
    {
        let store = Arc::clone(&self.inner.store);
        let sync = Arc::clone(&self.inner.sync);
        self.inner
            .rt
            .handle
            .spawn_blocking(move || {
                let mut state = sync.lock().map_err(|_| lock_error())?;
                let guard = store.lock().map_err(|_| lock_error())?;
                f(&mut state, &guard)
            })
            .await
            .map_err(|e| Error::Other(format!("background task failed: {e}")))?
    }

    /// The saved sync settings (backend choice and target parameters).
    pub async fn sync_settings(&self) -> Result<SyncSettings> {
        self.with_store(|store| store.setting::<SyncSettings>())
            .await
    }

    /// Save sync settings. Drops the live engine: the new target needs a
    /// fresh create or join, it must never inherit the old vault's key.
    pub async fn set_sync_settings(&self, settings: SyncSettings) -> Result<()> {
        self.with_sync(move |state, store| {
            store.set_setting(&settings)?;
            state.engine = None;
            Ok(())
        })
        .await
    }

    /// The saved backend credentials (never the sync password).
    pub async fn sync_credentials(&self) -> Result<SyncCredentials> {
        self.with_store(|store| store.sync_credentials()).await
    }

    /// Save backend credentials. They stay on this device.
    pub async fn set_sync_credentials(&self, credentials: SyncCredentials) -> Result<()> {
        self.with_store(move |store| store.set_sync_credentials(&credentials))
            .await
    }

    /// Create a vault on the configured backend and hold its engine.
    pub async fn sync_setup_create(&self, password: &str) -> Result<()> {
        let password = password.to_string();
        self.with_sync(move |state, store| {
            let backend = backend_from(
                &store.setting::<SyncSettings>()?,
                &store.sync_credentials()?,
            )?;
            let device_id = store.device_id().to_string();
            state.engine = Some(SyncEngine::create(backend, &device_id, &password)?);
            state.status.last_error = None;
            Ok(())
        })
        .await
    }

    /// Join the vault on the configured backend and hold its engine.
    pub async fn sync_setup_join(&self, password: &str) -> Result<()> {
        let password = password.to_string();
        self.with_sync(move |state, store| {
            let backend = backend_from(
                &store.setting::<SyncSettings>()?,
                &store.sync_credentials()?,
            )?;
            let device_id = store.device_id().to_string();
            state.engine = Some(SyncEngine::join(backend, &device_id, &password)?);
            state.status.last_error = None;
            Ok(())
        })
        .await
    }

    /// Push dirty records, then pull new segments. Records the outcome in
    /// status; on failure the error is recorded there too and returned.
    pub async fn sync_now(&self) -> Result<(PushReport, PullReport)> {
        self.with_sync(|state, store| {
            let result: Result<(PushReport, PullReport)> = (|| {
                let engine = engine_required(state)?;
                let push = engine.push(store, usize::MAX)?;
                let pull = engine.pull(store)?;
                Ok((push, pull))
            })();
            match result {
                Ok((push, pull)) => {
                    state.status.last_push = Some(push.clone());
                    state.status.last_pull = Some(pull.clone());
                    state.status.last_error = None;
                    state.status.last_sync_at = Some(now_millis());
                    Ok((push, pull))
                }
                Err(error) => {
                    state.status.last_error = Some(error.to_string());
                    Err(error)
                }
            }
        })
        .await
    }

    /// Capture a snapshot (and clean up the segments it covers).
    pub async fn sync_snapshot(&self) -> Result<SnapshotReport> {
        self.with_sync(|state, store| {
            let result = (|| {
                let engine = engine_required(state)?;
                engine.snapshot(store)
            })();
            match result {
                Ok(report) => {
                    state.status.last_snapshot = Some(report.clone());
                    state.status.last_error = None;
                    Ok(report)
                }
                Err(error) => {
                    state.status.last_error = Some(error.to_string());
                    Err(error)
                }
            }
        })
        .await
    }

    /// Re-wrap the same sync key under a new password. Remote data untouched.
    pub async fn sync_change_password(&self, old_password: &str, new_password: &str) -> Result<()> {
        let old_password = old_password.to_string();
        let new_password = new_password.to_string();
        self.with_sync(move |state, _store| {
            let engine = engine_required(state)?;
            engine.change_password(&old_password, &new_password)
        })
        .await
    }

    /// Revoke the current key: rotate to a fresh key and hold the new engine.
    /// Devices still on the old key fail new objects with the rotation error.
    pub async fn sync_rotate_key(&self, new_password: &str) -> Result<()> {
        let new_password = new_password.to_string();
        self.with_sync(move |state, _store| {
            let rotated = {
                let engine = engine_required(state)?;
                engine.rotate_key(&new_password)?
            };
            state.engine = Some(rotated);
            state.status.last_error = None;
            Ok(())
        })
        .await
    }

    /// Reset the vault: new key, remote wiped, fresh snapshot. Holds the new
    /// engine; other devices re-join with the new password.
    pub async fn sync_reset(&self, new_password: &str) -> Result<()> {
        let new_password = new_password.to_string();
        self.with_sync(move |state, store| {
            let reset = {
                let engine = engine_required(state)?;
                engine.reset(store, &new_password)?
            };
            state.engine = Some(reset);
            state.status.last_error = None;
            Ok(())
        })
        .await
    }

    /// Whether a sync engine is live for this session (create or join has
    /// run since start, and settings have not been changed since).
    pub fn sync_is_setup(&self) -> bool {
        self.inner
            .sync
            .lock()
            .map(|state| state.engine.is_some())
            .unwrap_or(false)
    }

    /// The last sync status (in-memory, no store read).
    pub fn sync_status(&self) -> SyncStatus {
        self.inner
            .sync
            .lock()
            .map(|state| state.status.clone())
            .unwrap_or_default()
    }

    /// Known devices: remote segment directories plus this device, with the
    /// local cursor for each. Requires a live engine (it knows the vault id).
    pub async fn sync_devices(&self) -> Result<Vec<SyncDevice>> {
        self.with_sync(|state, store| {
            let engine = engine_required(state)?;
            let backend = backend_from(
                &store.setting::<SyncSettings>()?,
                &store.sync_credentials()?,
            )?;
            let mut devices: Vec<SyncDevice> = device_ids_from_keys(&backend.list("devices")?)
                .into_iter()
                .map(|device_id| SyncDevice {
                    last_seq: store
                        .sync_cursor(engine.vault_id(), &device_id)
                        .unwrap_or(0),
                    is_local: device_id == engine.device_id(),
                    device_id,
                })
                .collect();
            if !devices.iter().any(|d| d.is_local) {
                devices.push(SyncDevice {
                    device_id: engine.device_id().to_string(),
                    last_seq: store
                        .sync_cursor(engine.vault_id(), engine.device_id())
                        .unwrap_or(0),
                    is_local: true,
                });
            }
            devices.sort_by(|a, b| a.device_id.cmp(&b.device_id));
            Ok(devices)
        })
        .await
    }
}
