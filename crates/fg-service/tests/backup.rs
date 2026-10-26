//! `Service::export_backup` / `import_backup` roundtrip: a saved
//! connection survives an export into a fresh store, and a wrong
//! password fails with the backend's message, verbatim.
//!
//! The store runs on a local file and never skips. Run with
//! `-- --nocapture`.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::PathBuf;

use fg_core::{ConnectionConfig, ConnectionParams, DbKind};
use fg_service::{
    BackupOptions, BackupScope, KeychainKeyProvider, MemoryBackend, PasswordUpdate, Service,
    ServiceOptions,
};

struct Env {
    dir: PathBuf,
    service: Service,
}

impl Env {
    fn new(name: &str) -> Self {
        let dir = std::env::temp_dir().join(format!(
            "fg-backup-{}-{}-{name}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let keys = KeychainKeyProvider::with_backend(MemoryBackend::default());
        let service = Service::start(ServiceOptions::new(dir.join("store.db")), &keys).unwrap();
        Self { dir, service }
    }
}

impl Drop for Env {
    fn drop(&mut self) {
        for _ in 0..40 {
            if std::fs::remove_dir_all(&self.dir).is_ok() || !self.dir.exists() {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(25));
        }
    }
}

#[tokio::test]
async fn export_import_roundtrip_and_wrong_password() {
    let source = Env::new("source");
    let config = ConnectionConfig::new(
        "billing",
        DbKind::Sqlite,
        ConnectionParams::File {
            path: "/data/billing.db".into(),
        },
    );
    source
        .service
        .save_connection(config, PasswordUpdate::Keep)
        .await
        .unwrap();
    let bytes = source
        .service
        .export_backup(
            &BackupOptions {
                scope: BackupScope::Connections,
                include_secrets: true,
            },
            "pw",
        )
        .unwrap();
    assert!(!bytes.is_empty());

    let target = Env::new("target");
    let report = target.service.import_backup(&bytes, "pw").unwrap();
    assert_eq!(report.applied, 1);
    assert_eq!(report.skipped_newer_local, 0);
    let names: Vec<String> = target
        .service
        .connections()
        .await
        .unwrap()
        .iter()
        .map(|c| match c {
            fg_service::StoredConnection::Ok(config) => config.name.clone(),
            fg_service::StoredConnection::Unreadable { id, .. } => format!("unreadable:{id}"),
        })
        .collect();
    assert_eq!(names, vec!["billing"]);

    let wrong = target.service.import_backup(&bytes, "nope");
    assert!(wrong.is_err());
    assert!(
        wrong.unwrap_err().to_string().contains("wrong password"),
        "the backend's message must survive verbatim"
    );
}
