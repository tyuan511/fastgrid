//! `OpenConnection::import_csv` against a real SQLite file: mapping,
//! defaults, NULL handling, both error policies, batch rollback, and
//! a 1M-row import.
//!
//! SQLite runs on a local file and never skips. Run with `-- --nocapture`.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::PathBuf;
use std::sync::Arc;

use fg_core::{ConnectionConfig, ConnectionParams, DbKind, ObjectPath};
use fg_service::{
    ColumnMapping, CsvImportOptions, ImportErrorPolicy, KeychainKeyProvider, MemoryBackend,
    OpenConnection, PasswordUpdate, Service, ServiceOptions,
};

struct Env {
    dir: PathBuf,
    service: Service,
}

impl Env {
    fn new(name: &str) -> Self {
        let dir = std::env::temp_dir().join(format!(
            "fg-import-{}-{}-{name}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let db = dir.join("d.db");
        let c = rusqlite::Connection::open(&db).unwrap();
        c.execute_batch(
            "CREATE TABLE t(id INTEGER PRIMARY KEY, name TEXT NOT NULL, n INTEGER, note TEXT DEFAULT 'dflt');",
        )
        .unwrap();
        let keys = KeychainKeyProvider::with_backend(MemoryBackend::default());
        let service = Service::start(ServiceOptions::new(dir.join("store.db")), &keys).unwrap();
        Self { dir, service }
    }

    async fn open(&self) -> Arc<OpenConnection> {
        let c = ConnectionConfig::new(
            "c",
            DbKind::Sqlite,
            ConnectionParams::File {
                path: self.dir.join("d.db").to_string_lossy().into_owned(),
            },
        );
        let c = self
            .service
            .save_connection(c, PasswordUpdate::Keep)
            .await
            .unwrap();
        self.service.open(&c.id).await.unwrap()
    }

    fn count(&self) -> i64 {
        let c = rusqlite::Connection::open(self.dir.join("d.db")).unwrap();
        c.query_row("SELECT COUNT(*) FROM t", [], |r| r.get(0))
            .unwrap()
    }

    fn names(&self) -> Vec<String> {
        let c = rusqlite::Connection::open(self.dir.join("d.db")).unwrap();
        let mut stmt = c.prepare("SELECT name, note FROM t ORDER BY id").unwrap();
        let rows = stmt
            .query_map([], |r| {
                Ok((
                    r.get::<_, String>(0).unwrap(),
                    r.get::<_, Option<String>>(1).unwrap(),
                ))
            })
            .unwrap();
        rows.map(|r| {
            let (name, note) = r.unwrap();
            format!("{name}:{}", note.unwrap_or_default())
        })
        .collect()
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

fn path() -> ObjectPath {
    ObjectPath::new(None, "t")
}

fn map(csv_index: usize, column: &str) -> ColumnMapping {
    ColumnMapping {
        csv_index,
        column: Some(column.into()),
    }
}

fn write_csv(env: &Env, name: &str, text: &str) -> PathBuf {
    let p = env.dir.join(name);
    std::fs::write(&p, text).unwrap();
    p
}

#[tokio::test]
async fn header_mapping_defaults_and_nulls() {
    let env = Env::new("basic");
    let conn = env.open().await;
    // `note` is unmapped but has a default, so it is omitted; `n` is
    // unmapped and nullable, so it lands as NULL.
    let file = write_csv(&env, "a.csv", "name,n\namy,7\nbo,\n");
    let report = conn
        .import_csv(
            &path(),
            &[map(0, "name"), map(1, "n")],
            &CsvImportOptions::default(),
            &file,
        )
        .await
        .unwrap();
    assert_eq!(report.inserted, 2);
    assert!(report.skipped.is_empty());
    assert_eq!(env.names(), vec!["amy:dflt", "bo:dflt"]);
    env.service.close_all();
}

#[tokio::test]
async fn unmapped_not_null_without_default_fails_before_writing() {
    let env = Env::new("validate");
    let conn = env.open().await;
    // `name` is NOT NULL with no default and no mapping.
    let file = write_csv(&env, "b.csv", "n\n7\n");
    let result = conn
        .import_csv(&path(), &[map(0, "n")], &CsvImportOptions::default(), &file)
        .await;
    assert!(result.is_err(), "validation must refuse the import");
    assert_eq!(env.count(), 0, "nothing was written");
    env.service.close_all();
}

#[tokio::test]
async fn skip_and_record_keeps_good_rows() {
    let env = Env::new("skip");
    let conn = env.open().await;
    let file = write_csv(&env, "c.csv", "name,n\namy,7\nbo,notanumber\ncy,9\n");
    let options = CsvImportOptions {
        error_policy: ImportErrorPolicy::SkipAndRecord,
        ..CsvImportOptions::default()
    };
    let report = conn
        .import_csv(&path(), &[map(0, "name"), map(1, "n")], &options, &file)
        .await
        .unwrap();
    assert_eq!(report.inserted, 2);
    assert_eq!(report.skipped_total, 1);
    assert_eq!(report.skipped.len(), 1);
    assert_eq!(report.skipped[0].0, 2, "second data row");
    assert_eq!(env.count(), 2);
    env.service.close_all();
}

#[tokio::test]
async fn abort_rolls_back_only_its_batch() {
    let env = Env::new("abort");
    let conn = env.open().await;
    // The duplicate key fails at INSERT time, so batch 2 starts and
    // then rolls back; batch 1 stays committed.
    let file = write_csv(&env, "d.csv", "id,name\n1,amy\n2,bo\n1,cy\n3,dee\n");
    let options = CsvImportOptions {
        batch_rows: 2,
        error_policy: ImportErrorPolicy::Abort,
        ..CsvImportOptions::default()
    };
    let result = conn
        .import_csv(&path(), &[map(0, "id"), map(1, "name")], &options, &file)
        .await;
    assert!(result.is_err(), "the bad row aborts the import");
    // Batch 1 (amy, bo) committed; batch 2 rolled back.
    assert_eq!(env.count(), 2);
    env.service.close_all();
}

#[tokio::test]
async fn skip_and_record_fallback_keeps_batch_good_rows() {
    let env = Env::new("fallback");
    let conn = env.open().await;
    // The bad row is a duplicate primary key: the batch INSERT fails,
    // the row-by-row fallback keeps the good rows of that batch.
    let file = write_csv(&env, "e.csv", "id,name\n1,amy\n1,bo\n2,cy\n");
    let options = CsvImportOptions {
        batch_rows: 10,
        error_policy: ImportErrorPolicy::SkipAndRecord,
        ..CsvImportOptions::default()
    };
    let report = conn
        .import_csv(&path(), &[map(0, "id"), map(1, "name")], &options, &file)
        .await
        .unwrap();
    assert_eq!(report.inserted, 2);
    assert_eq!(report.skipped_total, 1);
    assert_eq!(report.skipped[0].0, 2);
    assert_eq!(env.count(), 2);
    env.service.close_all();
}

#[tokio::test]
async fn one_million_rows_import() {
    let env = Env::new("million");
    let conn = env.open().await;
    let file = env.dir.join("big.csv");
    {
        use std::io::Write as _;
        let mut f = std::io::BufWriter::new(std::fs::File::create(&file).unwrap());
        writeln!(f, "name,n").unwrap();
        for i in 0..1_000_000 {
            writeln!(f, "item{i},{i}").unwrap();
        }
    }
    let report = conn
        .import_csv(
            &path(),
            &[map(0, "name"), map(1, "n")],
            &CsvImportOptions::default(),
            &file,
        )
        .await
        .unwrap();
    assert_eq!(report.inserted, 1_000_000);
    assert!(report.skipped.is_empty());
    assert_eq!(env.count(), 1_000_000);
    env.service.close_all();
}
