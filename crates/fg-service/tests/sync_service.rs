//! Sync through the service API: two services sharing a local folder vault.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::{Path, PathBuf};

use fg_core::{Snippet, SnippetId, SyncBackendKind, SyncSettings, UnixMillis};
use fg_service::{KeychainKeyProvider, MemoryBackend, Service, ServiceOptions};

struct Env {
    dir: PathBuf,
    keys_a: KeychainKeyProvider<MemoryBackend>,
    keys_b: KeychainKeyProvider<MemoryBackend>,
}

impl Env {
    fn new() -> Self {
        let dir = std::env::temp_dir().join(format!(
            "fg-sync-service-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        Self {
            dir,
            keys_a: KeychainKeyProvider::with_backend(MemoryBackend::default()),
            keys_b: KeychainKeyProvider::with_backend(MemoryBackend::default()),
        }
    }

    fn service_a(&self) -> Service {
        Service::start(ServiceOptions::new(self.dir.join("a.db")), &self.keys_a).unwrap()
    }

    fn service_b(&self) -> Service {
        Service::start(ServiceOptions::new(self.dir.join("b.db")), &self.keys_b).unwrap()
    }

    fn sync_dir(&self) -> PathBuf {
        self.dir.join("vault")
    }
}

impl Drop for Env {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn settings(folder: &Path) -> SyncSettings {
    SyncSettings {
        enabled: true,
        backend: SyncBackendKind::LocalFolder,
        local_folder: folder.to_string_lossy().into_owned(),
        ..SyncSettings::default()
    }
}

fn snippet(title: &str) -> Snippet {
    Snippet {
        id: SnippetId::new(),
        connection_id: None,
        title: title.into(),
        sql: "SELECT 1".into(),
        tags: Vec::new(),
        sort_order: 1,
        created_at: UnixMillis(1),
        updated_at: UnixMillis(1),
    }
}

async fn titles(service: &Service) -> Vec<String> {
    let mut titles: Vec<String> = service
        .snippets(None)
        .await
        .unwrap()
        .iter()
        .filter_map(|s| s.snippet().map(|s| s.title.clone()))
        .collect();
    titles.sort();
    titles
}

#[tokio::test]
async fn two_services_converge_and_rotate() {
    let env = Env::new();
    let a = env.service_a();
    let b = env.service_b();
    let folder = env.sync_dir();

    a.set_sync_settings(settings(&folder)).await.unwrap();
    b.set_sync_settings(settings(&folder)).await.unwrap();
    a.sync_setup_create("pw").await.unwrap();
    b.sync_setup_join("pw").await.unwrap();

    a.save_snippet(snippet("from-a")).await.unwrap();
    let (push, pull) = a.sync_now().await.unwrap();
    // Two records: the sync settings themselves (they sync by design, so a
    // second device picks up the same target) plus the snippet.
    assert_eq!(push.pushed, 2);
    assert_eq!(pull.applied, 0);
    b.sync_now().await.unwrap();
    assert_eq!(titles(&b).await, vec!["from-a"]);

    // Status is recorded, and the device list shows both sides once B pushes.
    let status = a.sync_status();
    assert!(status.last_push.is_some());
    assert!(status.last_error.is_none());
    b.save_snippet(snippet("from-b")).await.unwrap();
    b.sync_now().await.unwrap();
    a.sync_now().await.unwrap();
    assert_eq!(titles(&a).await, vec!["from-a", "from-b"]);
    let devices = a.sync_devices().await.unwrap();
    assert_eq!(devices.len(), 2);
    assert_eq!(devices.iter().filter(|d| d.is_local).count(), 1);

    // Password change keeps the same key: B's engine keeps working.
    a.sync_change_password("pw", "pw-2").await.unwrap();
    b.save_snippet(snippet("after-change")).await.unwrap();
    b.sync_now().await.unwrap();
    a.sync_now().await.unwrap();
    assert_eq!(titles(&a).await, vec!["after-change", "from-a", "from-b"]);

    // Rotation revokes B's engine: once A pushes under the new key, B's old
    // engine rejects the new segment. Re-joining with the new password
    // recovers.
    a.sync_rotate_key("pw-3").await.unwrap();
    a.save_snippet(snippet("after-rotate")).await.unwrap();
    a.sync_now().await.unwrap();
    let error = b.sync_now().await.unwrap_err().to_string();
    assert!(error.contains("another sync key generation"), "{error}");
    b.sync_setup_join("pw-3").await.unwrap();
    b.sync_now().await.unwrap();
    assert_eq!(
        titles(&b).await,
        vec!["after-change", "after-rotate", "from-a", "from-b"]
    );
}
