//! Sync engine: push dirty records to, and pull segments from, a dumb backend.
//!
//! Remote layout (see `docs/encryption-and-sync.md`):
//!
//! ```text
//! manifest                 plaintext JSON: format version, vault id, key generation
//! keys/<generation>        sync key wrapped by the sync password (password seal)
//! devices/<id>/seg-<seq>   one sealed segment: a JSON batch of records
//! snapshots/snap-<id>-<ms> sealed full-state snapshots (written by `snapshot`, later)
//! ```
//!
//! Segments are immutable and single-writer: only the owning device ever writes
//! its own directory, so no locks or conditional writes are needed beyond
//! `put_if_absent`. Sequences are parsed numerically from file names, never
//! assumed from listing order. Pulls are resumable: the cursor for each remote
//! device advances after every applied segment, so an interrupted pull resumes
//! where it stopped instead of re-applying everything.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use fg_core::{Error, Result};
use fg_crypto::{Envelope, SecretKey, open, seal};
use fg_store::{ApplyOutcome, Store, SyncRecord};
use serde::{Deserialize, Serialize};

use crate::sync_backend::SyncBackend;
use crate::sync_keys::{generate_sync_key, unwrap_sync_key, wrap_sync_key};

/// Remote format version. A vault written by a newer version refuses to open.
pub const SYNC_FORMAT_VERSION: u32 = 1;

const MANIFEST_KEY: &str = "manifest";
const PUSH_RETRIES: usize = 5;
/// Reserved `sync_state` device id recording the newest snapshot this store
/// has applied (as its creation millis). Real device ids are UUIDs, so this
/// never collides with a device cursor.
const SNAPSHOT_CURSOR_DEVICE: &str = "__snapshot__";

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Manifest {
    format_version: u32,
    vault_id: String,
    key_generation: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Segment {
    format_version: u32,
    device_id: String,
    seq: i64,
    records: Vec<SyncRecord>,
}

/// A sealed full-state capture. `watermarks` maps each device the snapshotting
/// device had consumed to the last segment sequence it had applied, so a device
/// that bootstraps from this snapshot knows exactly which segments it still
/// needs. Records include tombstones: without them a fallback device would
/// resurrect records deleted before the watermark.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct Snapshot {
    format_version: u32,
    device_id: String,
    created_at: i64,
    watermarks: HashMap<String, i64>,
    records: Vec<SyncRecord>,
}

fn segment_key(device_id: &str, seq: i64) -> String {
    format!("devices/{device_id}/seg-{seq:06}")
}

fn key_path(generation: u32) -> String {
    format!("keys/{generation}")
}

fn segment_aad(vault_id: &str, device_id: &str, seq: i64) -> String {
    format!("fg-seg|{vault_id}|{device_id}|{seq}")
}

fn snapshot_key(device_id: &str, created_at: i64) -> String {
    format!("snapshots/snap-{device_id}-{created_at:013}")
}

fn snapshot_aad(vault_id: &str, device_id: &str, created_at: i64) -> String {
    format!("fg-snap|{vault_id}|{device_id}|{created_at}")
}

/// Device ids present in `devices/<id>/...` keys, sorted and deduplicated.
/// Stray keys that do not match the layout are ignored.
pub(crate) fn device_ids_from_keys(keys: &[String]) -> Vec<String> {
    let mut ids = Vec::new();
    for key in keys {
        let mut parts = key.split('/');
        if parts.next() != Some("devices") {
            continue;
        }
        let Some(id) = parts.next() else { continue };
        if parts.next().is_none() || ids.contains(&id.to_string()) {
            continue;
        }
        ids.push(id.to_string());
    }
    ids.sort();
    ids
}

/// Parse a snapshot key into (device_id, created_at). Device ids may contain
/// dashes, so the split is at the last dash.
fn parse_snapshot_key(key: &str) -> Option<(String, i64)> {
    let rest = key.strip_prefix("snapshots/snap-")?;
    let (device_id, millis) = rest.rsplit_once('-')?;
    millis
        .parse::<i64>()
        .ok()
        .map(|ms| (device_id.to_string(), ms))
}

pub(crate) fn now_millis() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

fn hex(bytes: &[u8]) -> String {
    const TABLE: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(TABLE[(byte >> 4) as usize] as char);
        out.push(TABLE[(byte & 0xf) as usize] as char);
    }
    out
}

/// One device's handle on a vault. Holds the unwrapped sync key in memory; the
/// key itself never leaves the device except wrapped under the sync password.
pub struct SyncEngine {
    backend: Arc<dyn SyncBackend>,
    vault_id: String,
    device_id: String,
    key: SecretKey,
    key_generation: u32,
}

/// What a push did. `seq` is `None` when there was nothing dirty to push.
#[derive(Debug, Clone, Default)]
pub struct PushReport {
    pub pushed: usize,
    pub seq: Option<i64>,
}

/// What a pull did. `devices` lists the remote devices that had new segments.
#[derive(Debug, Clone, Default)]
pub struct PullReport {
    pub applied: usize,
    pub skipped: usize,
    pub devices: Vec<String>,
}

/// What a snapshot did. `segments_deleted` counts the segments the snapshot's
/// watermarks made redundant; they are gone from the backend afterwards.
#[derive(Debug, Clone, Default)]
pub struct SnapshotReport {
    pub records: usize,
    pub segments_deleted: usize,
    pub key: String,
}

impl SyncEngine {
    /// Create a new vault on `backend`. The key is written before the manifest,
    /// so an interrupted setup leaves at most an orphan key file, never a vault
    /// that claims to exist but cannot be joined.
    pub fn create(backend: Arc<dyn SyncBackend>, device_id: &str, password: &str) -> Result<Self> {
        let key = generate_sync_key()?;
        let wrapped = wrap_sync_key(password, &key)?;
        if !backend.put_if_absent(&key_path(1), &wrapped)? {
            return Err(Error::Other(
                "a vault already exists at this target; join it instead".into(),
            ));
        }
        let vault_id = hex(key.as_bytes());
        let manifest = Manifest {
            format_version: SYNC_FORMAT_VERSION,
            vault_id: vault_id.clone(),
            key_generation: 1,
        };
        let bytes = serde_json::to_vec(&manifest)
            .map_err(|e| Error::Storage(format!("cannot serialize sync manifest: {e}")))?;
        if !backend.put_if_absent(MANIFEST_KEY, &bytes)? {
            return Err(Error::Other(
                "a vault already exists at this target; join it instead".into(),
            ));
        }
        Ok(Self {
            backend,
            vault_id,
            device_id: device_id.to_string(),
            key,
            key_generation: 1,
        })
    }

    /// Join the vault on `backend`. The sync password unwraps the key; a wrong
    /// password fails here, before any segment is touched.
    pub fn join(backend: Arc<dyn SyncBackend>, device_id: &str, password: &str) -> Result<Self> {
        let bytes = backend
            .get(MANIFEST_KEY)?
            .ok_or_else(|| Error::Other("no sync vault at this target".into()))?;
        let manifest: Manifest = serde_json::from_slice(&bytes)
            .map_err(|_| Error::Other("the sync vault manifest is damaged".into()))?;
        if manifest.format_version != SYNC_FORMAT_VERSION {
            return Err(Error::Unsupported(
                "this vault was created by a newer version of fastgrid".into(),
            ));
        }
        let wrapped = backend
            .get(&key_path(manifest.key_generation))?
            .ok_or_else(|| Error::Other("the vault is missing its sync key".into()))?;
        let key = unwrap_sync_key(password, &wrapped)?;
        Ok(Self {
            backend,
            vault_id: manifest.vault_id,
            device_id: device_id.to_string(),
            key,
            key_generation: manifest.key_generation,
        })
    }

    /// Push up to `limit` dirty records as one new segment. Returns immediately
    /// when nothing is dirty. A sequence collision (a retried push after an
    /// interrupted `mark_clean`) re-lists and retries instead of overwriting.
    pub fn push(&self, store: &Store, limit: usize) -> Result<PushReport> {
        let dirty = store.dirty_records(limit)?;
        if dirty.is_empty() {
            return Ok(PushReport::default());
        }
        for _ in 0..PUSH_RETRIES {
            let seq = self.next_seq(store)?;
            let segment = Segment {
                format_version: SYNC_FORMAT_VERSION,
                device_id: self.device_id.clone(),
                seq,
                records: dirty.clone(),
            };
            let json = serde_json::to_vec(&segment)
                .map_err(|e| Error::Storage(format!("cannot serialize sync segment: {e}")))?;
            let sealed = seal(
                &self.key,
                self.key_generation,
                segment_aad(&self.vault_id, &self.device_id, seq).as_bytes(),
                &json,
            )
            .map_err(|e| Error::Storage(e.to_string()))?;
            if self
                .backend
                .put_if_absent(&segment_key(&self.device_id, seq), &sealed)?
            {
                let pushed: Vec<(String, String, i64)> = dirty
                    .iter()
                    .map(|r| (r.kind.clone(), r.id.clone(), r.rev))
                    .collect();
                store.mark_clean(&pushed)?;
                store.set_sync_cursor(&self.vault_id, &self.device_id, seq)?;
                return Ok(PushReport {
                    pushed: dirty.len(),
                    seq: Some(seq),
                });
            }
        }
        Err(Error::Other(
            "sync push collided repeatedly; try again".into(),
        ))
    }

    /// Pull every new segment from every other device, oldest first. The cursor
    /// for a device advances after each applied segment, so interruption is safe.
    /// A snapshot newer than the last one applied is applied first: without
    /// this, a device whose segments were all cleaned up after a snapshot
    /// would pull nothing and never learn about the snapshot. A gap with no
    /// snapshot to close it is a hard error: silently skipping would lose
    /// records.
    pub fn pull(&self, store: &Store) -> Result<PullReport> {
        let mut report = PullReport::default();
        if let Some(key) = self.newest_snapshot_key()?
            && let Some((_, millis)) = parse_snapshot_key(&key)
            && millis > store.sync_cursor(&self.vault_id, SNAPSHOT_CURSOR_DEVICE)?
        {
            let (applied, skipped) = self.apply_snapshot(store, &key)?;
            report.applied += applied;
            report.skipped += skipped;
            store.set_sync_cursor(&self.vault_id, SNAPSHOT_CURSOR_DEVICE, millis)?;
        }
        for device in self.list_device_ids()? {
            if device == self.device_id {
                continue;
            }
            if self.pull_device(store, &device, &mut report)? {
                report.devices.push(device);
            }
        }
        Ok(report)
    }

    /// Pull one device's new segments. Returns true when it had any.
    fn pull_device(&self, store: &Store, device: &str, report: &mut PullReport) -> Result<bool> {
        let cursor = store.sync_cursor(&self.vault_id, device)?;
        let segments: Vec<(i64, String)> = self
            .list_segments(device)?
            .into_iter()
            .filter(|(seq, _)| *seq > cursor)
            .collect();
        if let Some((first, _)) = segments.first()
            && *first > cursor + 1
        {
            return Err(Error::Other(format!(
                "sync segments are missing for device {device} after seq {cursor}; \
                 the vault may have been cleaned up"
            )));
        }
        let had_segments = !segments.is_empty();
        for (seq, key) in segments {
            let bytes = self
                .backend
                .get(&key)?
                .ok_or_else(|| Error::Other(format!("sync segment {key} vanished during pull")))?;
            let plain = match open(
                &self.key,
                segment_aad(&self.vault_id, device, seq).as_bytes(),
                &bytes,
            ) {
                Ok(plain) => plain,
                Err(_) => return Err(self.open_error(&key, &bytes)),
            };
            let segment: Segment = serde_json::from_slice(&plain)
                .map_err(|_| Error::Other(format!("sync segment {key} is damaged")))?;
            if segment.format_version != SYNC_FORMAT_VERSION
                || segment.device_id != device
                || segment.seq != seq
            {
                return Err(Error::Other(format!(
                    "sync segment {key} does not match its name"
                )));
            }
            for record in &segment.records {
                match store.apply_remote(record)? {
                    ApplyOutcome::Applied => report.applied += 1,
                    ApplyOutcome::Skipped => report.skipped += 1,
                }
            }
            store.set_sync_cursor(&self.vault_id, device, seq)?;
        }
        Ok(had_segments)
    }

    /// Capture the full synced state as a sealed snapshot, then delete the
    /// segments its watermarks cover. Pushes first, so local dirties are in
    /// segments before the snapshot claims to cover them. A device that falls
    /// behind and finds its segments gone bootstraps from this snapshot.
    pub fn snapshot(&self, store: &Store) -> Result<SnapshotReport> {
        self.push(store, usize::MAX)?;
        let mut watermarks: HashMap<String, i64> =
            store.sync_cursors(&self.vault_id)?.into_iter().collect();
        watermarks.remove(SNAPSHOT_CURSOR_DEVICE);
        watermarks.insert(self.device_id.clone(), self.next_seq(store)? - 1);
        let records = store.synced_records()?;
        let created_at = now_millis();
        let snapshot = Snapshot {
            format_version: SYNC_FORMAT_VERSION,
            device_id: self.device_id.clone(),
            created_at,
            watermarks: watermarks.clone(),
            records: records.clone(),
        };
        let json = serde_json::to_vec(&snapshot)
            .map_err(|e| Error::Storage(format!("cannot serialize snapshot: {e}")))?;
        let sealed = seal(
            &self.key,
            self.key_generation,
            snapshot_aad(&self.vault_id, &self.device_id, created_at).as_bytes(),
            &json,
        )
        .map_err(|e| Error::Storage(e.to_string()))?;
        let key = snapshot_key(&self.device_id, created_at);
        self.backend.put_if_absent(&key, &sealed)?;
        let mut deleted = 0;
        for (device, mark) in &watermarks {
            for (seq, seg_key) in self.list_segments(device)? {
                if seq <= *mark {
                    self.backend.delete(&seg_key)?;
                    deleted += 1;
                }
            }
        }
        Ok(SnapshotReport {
            records: records.len(),
            segments_deleted: deleted,
            key,
        })
    }

    /// Re-wrap the same sync key under a new password. Remote data is untouched:
    /// no segment or snapshot is rewritten. The engine keeps working, since the
    /// key itself did not change.
    pub fn change_password(&self, old_password: &str, new_password: &str) -> Result<()> {
        let wrapped = self
            .backend
            .get(&key_path(self.key_generation))?
            .ok_or_else(|| Error::Other("the vault is missing its sync key".into()))?;
        let key = unwrap_sync_key(old_password, &wrapped)?;
        if key.as_bytes() != self.key.as_bytes() {
            return Err(Error::Other(
                "the vault key does not match this device's key".into(),
            ));
        }
        let rewrapped = wrap_sync_key(new_password, &key)?;
        self.backend.put(&key_path(self.key_generation), &rewrapped)
    }

    /// Revoke this key: re-seal every existing segment and snapshot under a
    /// fresh key, then bump the manifest generation. Returns a new engine on
    /// the new key. A device still holding the old key fails new objects with
    /// the rotation error instead of a misleading damage error, because every
    /// object header carries the generation that sealed it. Already-pulled
    /// history cannot be un-read, by design.
    pub fn rotate_key(&self, new_password: &str) -> Result<Self> {
        self.rotate_inner(new_password, true)
    }

    fn rotate_inner(&self, new_password: &str, reseal: bool) -> Result<Self> {
        let generation = self.key_generation + 1;
        let key = generate_sync_key()?;
        let wrapped = wrap_sync_key(new_password, &key)?;
        if !self
            .backend
            .put_if_absent(&key_path(generation), &wrapped)?
        {
            return Err(Error::Other(
                "the next key generation already exists".into(),
            ));
        }
        if reseal {
            self.reseal_all(&key, generation)?;
        }
        let mut manifest = self.read_manifest()?;
        manifest.key_generation = generation;
        let bytes = serde_json::to_vec(&manifest)
            .map_err(|e| Error::Storage(format!("cannot serialize sync manifest: {e}")))?;
        self.backend.put(MANIFEST_KEY, &bytes)?;
        Ok(Self {
            backend: self.backend.clone(),
            vault_id: self.vault_id.clone(),
            device_id: self.device_id.clone(),
            key,
            key_generation: generation,
        })
    }

    /// Re-seal every segment and snapshot under `new_key`. Objects already at
    /// `generation` are skipped, so retrying an interrupted rotation resumes
    /// instead of failing on its own earlier work.
    fn reseal_all(&self, new_key: &SecretKey, generation: u32) -> Result<()> {
        for key in self.backend.list("devices")? {
            let mut parts = key.split('/');
            if parts.next() != Some("devices") {
                continue;
            }
            let Some(device) = parts.next() else { continue };
            let Some(segment) = parts.next() else {
                continue;
            };
            let Some(seq) = segment
                .strip_prefix("seg-")
                .and_then(|s| s.parse::<i64>().ok())
            else {
                continue;
            };
            self.reseal_object(
                &key,
                new_key,
                generation,
                &segment_aad(&self.vault_id, device, seq),
            )?;
        }
        for key in self.backend.list("snapshots")? {
            let Some((device, millis)) = parse_snapshot_key(&key) else {
                continue;
            };
            self.reseal_object(
                &key,
                new_key,
                generation,
                &snapshot_aad(&self.vault_id, &device, millis),
            )?;
        }
        Ok(())
    }

    fn reseal_object(
        &self,
        key: &str,
        new_key: &SecretKey,
        generation: u32,
        aad: &str,
    ) -> Result<()> {
        let bytes = self
            .backend
            .get(key)?
            .ok_or_else(|| Error::Other(format!("sync object {key} vanished during rotation")))?;
        let envelope = Envelope::parse(&bytes)
            .map_err(|_| Error::Other(format!("sync object {key} is damaged; rotation stopped")))?;
        if envelope.key_id == generation {
            return Ok(());
        }
        let plain = envelope.open(&self.key, aad.as_bytes()).map_err(|_| {
            Error::Other(format!(
                "cannot open sync object {key} with the current key; rotation stopped"
            ))
        })?;
        let sealed = seal(new_key, generation, aad.as_bytes(), &plain)
            .map_err(|e| Error::Storage(e.to_string()))?;
        self.backend.put(key, &sealed)
    }

    /// Reset the vault: rotate the key without re-sealing (everything is about
    /// to be deleted anyway), delete every segment and snapshot, and capture
    /// the full local state as a fresh snapshot under the new key. Other
    /// devices re-join with the new password and bootstrap from it.
    pub fn reset(&self, store: &Store, new_password: &str) -> Result<Self> {
        let engine = self.rotate_inner(new_password, false)?;
        for key in self.backend.list("")? {
            if !key.starts_with("keys/") && key != MANIFEST_KEY {
                self.backend.delete(&key)?;
            }
        }
        engine.snapshot(store)?;
        Ok(engine)
    }

    fn read_manifest(&self) -> Result<Manifest> {
        let bytes = self
            .backend
            .get(MANIFEST_KEY)?
            .ok_or_else(|| Error::Other("no sync vault at this target".into()))?;
        serde_json::from_slice(&bytes)
            .map_err(|_| Error::Other("the sync vault manifest is damaged".into()))
    }

    fn newest_snapshot_key(&self) -> Result<Option<String>> {
        let mut best: Option<(i64, String)> = None;
        for key in self.backend.list("snapshots")? {
            if let Some((_, millis)) = parse_snapshot_key(&key)
                && best.as_ref().is_none_or(|(ms, _)| millis > *ms)
            {
                best = Some((millis, key));
            }
        }
        Ok(best.map(|(_, key)| key))
    }

    /// Open a snapshot, apply its records, and set cursors to its watermarks.
    /// Cursors only move forward, so a snapshot older than local progress for
    /// some device leaves that device's cursor alone. Returns (applied, skipped).
    fn apply_snapshot(&self, store: &Store, key: &str) -> Result<(usize, usize)> {
        let (device_id, created_at) = parse_snapshot_key(key)
            .ok_or_else(|| Error::Other(format!("malformed snapshot key: {key}")))?;
        let bytes = self
            .backend
            .get(key)?
            .ok_or_else(|| Error::Other(format!("snapshot {key} vanished during pull")))?;
        let plain = match open(
            &self.key,
            snapshot_aad(&self.vault_id, &device_id, created_at).as_bytes(),
            &bytes,
        ) {
            Ok(plain) => plain,
            Err(_) => return Err(self.open_error(key, &bytes)),
        };
        let snapshot: Snapshot = serde_json::from_slice(&plain)
            .map_err(|_| Error::Other(format!("snapshot {key} is damaged")))?;
        if snapshot.format_version != SYNC_FORMAT_VERSION
            || snapshot.device_id != device_id
            || snapshot.created_at != created_at
        {
            return Err(Error::Other(format!(
                "snapshot {key} does not match its name"
            )));
        }
        let mut applied = 0;
        let mut skipped = 0;
        for record in &snapshot.records {
            match store.apply_remote(record)? {
                ApplyOutcome::Applied => applied += 1,
                ApplyOutcome::Skipped => skipped += 1,
            }
        }
        for (device, seq) in &snapshot.watermarks {
            store.set_sync_cursor(&self.vault_id, device, *seq)?;
        }
        Ok((applied, skipped))
    }

    /// An object that fails to open is either damaged or sealed under another
    /// key generation. The envelope header tells them apart, so the user gets
    /// an actionable error instead of a misleading damage report.
    fn open_error(&self, key: &str, bytes: &[u8]) -> Error {
        if let Ok(envelope) = Envelope::parse(bytes)
            && envelope.key_id != self.key_generation
        {
            return Error::Other(
                "this object was sealed with another sync key generation; \
                 re-join with the current password"
                    .into(),
            );
        }
        Error::Other(format!(
            "cannot open sync object {key}; it is damaged or was sealed with another key"
        ))
    }

    /// Next sequence for this device. The local cursor is the authority: after
    /// a snapshot deletes old segments, the remote listing alone would reuse
    /// sequence numbers that other devices' cursors have already passed, making
    /// the new segments invisible to them. The remote maximum is only a
    /// fallback for a store whose cursors were lost.
    fn next_seq(&self, store: &Store) -> Result<i64> {
        let local = store.sync_cursor(&self.vault_id, &self.device_id)?;
        let remote = self
            .list_segments(&self.device_id)?
            .last()
            .map_or(0, |(seq, _)| *seq);
        Ok(local.max(remote) + 1)
    }

    fn list_segments(&self, device_id: &str) -> Result<Vec<(i64, String)>> {
        let prefix = format!("devices/{device_id}/seg-");
        let mut segments = Vec::new();
        for key in self.backend.list(&format!("devices/{device_id}"))? {
            if let Some(suffix) = key.strip_prefix(&prefix)
                && let Ok(seq) = suffix.parse::<i64>()
            {
                segments.push((seq, key));
            }
        }
        segments.sort();
        Ok(segments)
    }

    /// This device's id, as it appears in segment directories.
    pub fn device_id(&self) -> &str {
        &self.device_id
    }

    /// The vault id from the manifest.
    pub fn vault_id(&self) -> &str {
        &self.vault_id
    }

    fn list_device_ids(&self) -> Result<Vec<String>> {
        Ok(device_ids_from_keys(&self.backend.list("devices")?))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sync_backend::{MemorySyncBackend, SyncBackend};
    use fg_core::{Snippet, SnippetId, UnixMillis};
    use fg_store::{MemoryKeyProvider, Store};

    fn store() -> Store {
        let keys = MemoryKeyProvider::new();
        Store::open_in_memory(&keys).unwrap()
    }

    fn snippet(title: &str, sql: &str) -> Snippet {
        Snippet {
            id: SnippetId::new(),
            connection_id: None,
            title: title.into(),
            sql: sql.into(),
            tags: Vec::new(),
            sort_order: 1,
            created_at: UnixMillis(1),
            updated_at: UnixMillis(1),
        }
    }

    fn titles(store: &Store) -> Vec<String> {
        let mut titles: Vec<String> = store
            .snippets(None)
            .unwrap()
            .iter()
            .filter_map(|s| s.snippet().map(|s| s.title.clone()))
            .collect();
        titles.sort();
        titles
    }

    #[test]
    fn two_devices_converge() {
        let backend: Arc<dyn SyncBackend> = Arc::new(MemorySyncBackend::new());
        let a = store();
        let b = store();
        let engine_a = SyncEngine::create(backend.clone(), "dev-a", "pw").unwrap();
        let engine_b = SyncEngine::join(backend.clone(), "dev-b", "pw").unwrap();

        let first = snippet("from-a", "SELECT 1");
        a.save_snippet(&first).unwrap();
        let push = engine_a.push(&a, 100).unwrap();
        assert_eq!(push.pushed, 1);
        assert_eq!(push.seq, Some(1));

        let pull = engine_b.pull(&b).unwrap();
        assert_eq!(pull.applied, 1);
        assert_eq!(titles(&b), vec!["from-a"]);

        // A pull of nothing, and a second pull on B, are both no-ops.
        assert_eq!(engine_a.pull(&a).unwrap().applied, 0);
        assert_eq!(engine_b.pull(&b).unwrap().applied, 0);

        // B writes, A pulls: both stores hold both snippets.
        let second = snippet("from-b", "SELECT 2");
        b.save_snippet(&second).unwrap();
        engine_b.push(&b, 100).unwrap();
        engine_a.pull(&a).unwrap();
        assert_eq!(titles(&a), vec!["from-a", "from-b"]);
        assert_eq!(titles(&b), vec!["from-a", "from-b"]);

        // Nothing dirty, nothing pushed.
        assert_eq!(engine_a.push(&a, 100).unwrap().pushed, 0);
    }

    #[test]
    fn tombstone_propagates() {
        let backend: Arc<dyn SyncBackend> = Arc::new(MemorySyncBackend::new());
        let a = store();
        let b = store();
        let engine_a = SyncEngine::create(backend.clone(), "dev-a", "pw").unwrap();
        let engine_b = SyncEngine::join(backend.clone(), "dev-b", "pw").unwrap();

        let gone = snippet("doomed", "SELECT 1");
        a.save_snippet(&gone).unwrap();
        engine_a.push(&a, 100).unwrap();
        engine_b.pull(&b).unwrap();
        assert_eq!(titles(&b), vec!["doomed"]);

        a.delete_snippet(&gone.id).unwrap();
        engine_a.push(&a, 100).unwrap();
        engine_b.pull(&b).unwrap();
        assert!(titles(&b).is_empty());
    }

    #[test]
    fn wrong_password_cannot_join() {
        let backend: Arc<dyn SyncBackend> = Arc::new(MemorySyncBackend::new());
        SyncEngine::create(backend.clone(), "dev-a", "pw-1").unwrap();
        let Err(error) = SyncEngine::join(backend, "dev-b", "pw-2") else {
            panic!("join with the wrong password must fail");
        };
        assert!(error.to_string().contains("wrong sync password"));
    }

    #[test]
    fn snapshot_bootstrap_and_cleanup() {
        let backend: Arc<dyn SyncBackend> = Arc::new(MemorySyncBackend::new());
        let a = store();
        let engine_a = SyncEngine::create(backend.clone(), "dev-a", "pw").unwrap();
        a.save_snippet(&snippet("one", "SELECT 1")).unwrap();
        engine_a.push(&a, 100).unwrap();
        a.save_snippet(&snippet("two", "SELECT 2")).unwrap();
        let snap = engine_a.snapshot(&a).unwrap();
        assert_eq!(snap.records, 2);
        assert_eq!(snap.segments_deleted, 2);
        assert!(backend.list("devices").unwrap().is_empty());
        assert_eq!(backend.list("snapshots").unwrap().len(), 1);

        // A fresh device bootstraps from the snapshot alone.
        let c = store();
        let engine_c = SyncEngine::join(backend.clone(), "dev-c", "pw").unwrap();
        engine_c.pull(&c).unwrap();
        assert_eq!(titles(&c), vec!["one", "two"]);
    }

    #[test]
    fn gap_closed_by_snapshot() {
        let backend: Arc<dyn SyncBackend> = Arc::new(MemorySyncBackend::new());
        let a = store();
        let b = store();
        let engine_a = SyncEngine::create(backend.clone(), "dev-a", "pw").unwrap();
        let engine_b = SyncEngine::join(backend.clone(), "dev-b", "pw").unwrap();
        a.save_snippet(&snippet("one", "SELECT 1")).unwrap();
        engine_a.push(&a, 100).unwrap();
        engine_b.pull(&b).unwrap();
        a.save_snippet(&snippet("two", "SELECT 2")).unwrap();
        engine_a.push(&a, 100).unwrap();
        engine_a.snapshot(&a).unwrap();
        a.save_snippet(&snippet("three", "SELECT 3")).unwrap();
        engine_a.push(&a, 100).unwrap();
        engine_b.pull(&b).unwrap();
        assert_eq!(titles(&b), vec!["one", "three", "two"]);
    }

    #[test]
    fn password_change_keeps_data_and_kills_the_old_password() {
        let backend: Arc<dyn SyncBackend> = Arc::new(MemorySyncBackend::new());
        let a = store();
        let engine_a = SyncEngine::create(backend.clone(), "dev-a", "pw-1").unwrap();
        a.save_snippet(&snippet("one", "SELECT 1")).unwrap();
        engine_a.push(&a, 100).unwrap();
        engine_a.change_password("pw-1", "pw-2").unwrap();
        assert!(SyncEngine::join(backend.clone(), "dev-x", "pw-1").is_err());
        let b = store();
        let engine_b = SyncEngine::join(backend.clone(), "dev-b", "pw-2").unwrap();
        engine_b.pull(&b).unwrap();
        assert_eq!(titles(&b), vec!["one"]);
        // The original engine still pushes: the key never changed.
        a.save_snippet(&snippet("two", "SELECT 2")).unwrap();
        engine_a.push(&a, 100).unwrap();
        engine_b.pull(&b).unwrap();
        assert_eq!(titles(&b), vec!["one", "two"]);
        assert!(engine_a.change_password("pw-1", "pw-3").is_err());
    }

    #[test]
    fn rotation_keeps_old_data_and_revokes_the_old_engine() {
        let backend: Arc<dyn SyncBackend> = Arc::new(MemorySyncBackend::new());
        let a = store();
        let engine_a = SyncEngine::create(backend.clone(), "dev-a", "pw-1").unwrap();
        a.save_snippet(&snippet("one", "SELECT 1")).unwrap();
        engine_a.push(&a, 100).unwrap();
        let engine_a2 = engine_a.rotate_key("pw-2").unwrap();
        assert!(SyncEngine::join(backend.clone(), "dev-x", "pw-1").is_err());
        // Old segments were re-sealed: the new key reads them.
        let b = store();
        let engine_b = SyncEngine::join(backend.clone(), "dev-b", "pw-2").unwrap();
        engine_b.pull(&b).unwrap();
        assert_eq!(titles(&b), vec!["one"]);
        // The revoked engine's new segment is rejected as rotated, not damaged.
        a.save_snippet(&snippet("two", "SELECT 2")).unwrap();
        engine_a.push(&a, 100).unwrap();
        let error = engine_b.pull(&b).unwrap_err().to_string();
        assert!(error.contains("another sync key generation"), "{error}");
        // A fresh snapshot closes the gap the revoked segment left behind.
        a.save_snippet(&snippet("three", "SELECT 3")).unwrap();
        engine_a2.push(&a, 100).unwrap();
        engine_a2.snapshot(&a).unwrap();
        engine_b.pull(&b).unwrap();
        assert_eq!(titles(&b), vec!["one", "three", "two"]);
    }

    #[test]
    fn reset_replaces_the_vault() {
        let backend: Arc<dyn SyncBackend> = Arc::new(MemorySyncBackend::new());
        let a = store();
        let engine_a = SyncEngine::create(backend.clone(), "dev-a", "pw-1").unwrap();
        a.save_snippet(&snippet("one", "SELECT 1")).unwrap();
        engine_a.push(&a, 100).unwrap();
        let engine_a2 = engine_a.reset(&a, "pw-2").unwrap();
        assert!(backend.list("devices").unwrap().is_empty());
        assert_eq!(backend.list("snapshots").unwrap().len(), 1);
        assert!(SyncEngine::join(backend.clone(), "dev-x", "pw-1").is_err());
        let _ = engine_a2;
        let c = store();
        let engine_c = SyncEngine::join(backend.clone(), "dev-c", "pw-2").unwrap();
        engine_c.pull(&c).unwrap();
        assert_eq!(titles(&c), vec!["one"]);
    }

    #[test]
    fn offline_divergence_converges_regardless_of_exchange_order() {
        // Conflicting edits: same rev, B's newer timestamp wins, in both orders.
        for order in [0, 1] {
            let backend: Arc<dyn SyncBackend> = Arc::new(MemorySyncBackend::new());
            let a = store();
            let b = store();
            let engine_a = SyncEngine::create(backend.clone(), "dev-a", "pw").unwrap();
            let engine_b = SyncEngine::join(backend.clone(), "dev-b", "pw").unwrap();
            let base = snippet("base", "SELECT 0");
            a.save_snippet(&base).unwrap();
            engine_a.push(&a, 100).unwrap();
            engine_b.pull(&b).unwrap();

            let mut edit_a = base.clone();
            edit_a.title = "a-edit".into();
            edit_a.updated_at = UnixMillis(100);
            let mut edit_b = base.clone();
            edit_b.title = "b-edit".into();
            edit_b.updated_at = UnixMillis(200);
            a.save_snippet(&edit_a).unwrap();
            b.save_snippet(&edit_b).unwrap();

            if order == 0 {
                engine_a.push(&a, 100).unwrap();
                engine_b.pull(&b).unwrap();
                engine_b.push(&b, 100).unwrap();
                engine_a.pull(&a).unwrap();
            } else {
                engine_b.push(&b, 100).unwrap();
                engine_a.pull(&a).unwrap();
                engine_a.push(&a, 100).unwrap();
                engine_b.pull(&b).unwrap();
            }
            assert_eq!(titles(&a), vec!["b-edit"]);
            assert_eq!(titles(&b), vec!["b-edit"]);
        }

        // Delete versus modify: A edits then deletes (tombstone rev 3) while B
        // only edits (rev 2), so the tombstone wins on rev in both orders.
        // (With equal revs the wall-clock timestamp decides instead, which is
        // why the edit-then-delete ordering is used here: it is deterministic.)
        for order in [0, 1] {
            let backend: Arc<dyn SyncBackend> = Arc::new(MemorySyncBackend::new());
            let a = store();
            let b = store();
            let engine_a = SyncEngine::create(backend.clone(), "dev-a", "pw").unwrap();
            let engine_b = SyncEngine::join(backend.clone(), "dev-b", "pw").unwrap();
            let base = snippet("base", "SELECT 0");
            a.save_snippet(&base).unwrap();
            engine_a.push(&a, 100).unwrap();
            engine_b.pull(&b).unwrap();

            let mut edit_a = base.clone();
            edit_a.title = "a-edit".into();
            a.save_snippet(&edit_a).unwrap();
            a.delete_snippet(&base.id).unwrap();
            let mut edit = base.clone();
            edit.title = "edited".into();
            edit.updated_at = UnixMillis(200);
            b.save_snippet(&edit).unwrap();

            if order == 0 {
                engine_a.push(&a, 100).unwrap();
                engine_b.pull(&b).unwrap();
                engine_b.push(&b, 100).unwrap();
                engine_a.pull(&a).unwrap();
            } else {
                engine_b.push(&b, 100).unwrap();
                engine_a.pull(&a).unwrap();
                engine_a.push(&a, 100).unwrap();
                engine_b.pull(&b).unwrap();
            }
            assert!(titles(&a).is_empty());
            assert!(titles(&b).is_empty());
        }
    }

    #[test]
    fn remote_objects_hold_no_plaintext() {
        let backend: Arc<dyn SyncBackend> = Arc::new(MemorySyncBackend::new());
        let a = store();
        let engine_a = SyncEngine::create(backend.clone(), "dev-a", "pw").unwrap();
        a.save_snippet(&snippet("secret-title-xyz", "SELECT 'secret-sql-xyz'"))
            .unwrap();
        engine_a.push(&a, 100).unwrap();

        for key in backend.list("").unwrap() {
            let bytes = backend.get(&key).unwrap().unwrap();
            let text = String::from_utf8_lossy(&bytes);
            assert!(!text.contains("secret-title-xyz"), "{key}");
            assert!(!text.contains("secret-sql-xyz"), "{key}");
        }
    }
}
