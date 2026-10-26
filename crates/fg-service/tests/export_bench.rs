//! Streaming-export memory benchmark: 10M narrow rows out of SQLite.
//!
//! The export must stay flat in memory: peak RSS during the second half
//! of the export may barely exceed the peak of the first half, and the
//! absolute peak stays under 200 MB. On Linux the peak comes from
//! `/proc/self/status` VmHWM sampled by a side thread while the export
//! runs; elsewhere the test only asserts completion and the row count.
//!
//! This test takes on the order of a minute (seed by doubling plus the
//! export itself) and writes ~150 MB of scratch files. The row count is
//! deliberately not shrunk: a small export cannot prove O(1) memory.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use fg_core::{ConnectionConfig, ConnectionParams, DbKind, ObjectPath, TableQuery};
use fg_service::{
    ExportControl, ExportOptions, KeychainKeyProvider, MemoryBackend, PasswordUpdate, Service,
    ServiceOptions,
};

const ROWS: u64 = 10_000_000;

/// Peak resident set size (VmHWM) in bytes, on Linux.
#[cfg(target_os = "linux")]
fn peak_rss_bytes() -> u64 {
    let status = std::fs::read_to_string("/proc/self/status").unwrap_or_default();
    for line in status.lines() {
        if let Some(rest) = line.strip_prefix("VmHWM:") {
            let kb: u64 = rest
                .trim()
                .trim_end_matches("kB")
                .trim()
                .parse()
                .unwrap_or(0);
            return kb * 1024;
        }
    }
    0
}

#[tokio::test]
async fn ten_million_row_csv_export_stays_flat() {
    let dir = std::env::temp_dir().join(format!(
        "fg-export-bench-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let db = dir.join("d.db");
    {
        let c = rusqlite::Connection::open(&db).unwrap();
        c.execute_batch("CREATE TABLE big(id INTEGER, v TEXT); INSERT INTO big VALUES (1, 'x');")
            .unwrap();
        // Double 23 times: 2^23 = 8,388,608 rows.
        for _ in 0..23 {
            c.execute_batch("INSERT INTO big SELECT * FROM big;")
                .unwrap();
        }
        // Top up to exactly 10,000,000.
        c.execute_batch("INSERT INTO big SELECT * FROM big LIMIT 1611392;")
            .unwrap();
        let count: i64 = c
            .query_row("SELECT COUNT(*) FROM big", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, ROWS as i64);
    }
    let keys = KeychainKeyProvider::with_backend(MemoryBackend::default());
    let service = Service::start(ServiceOptions::new(dir.join("store.db")), &keys).unwrap();
    let config = ConnectionConfig::new(
        "c",
        DbKind::Sqlite,
        ConnectionParams::File {
            path: db.to_string_lossy().into_owned(),
        },
    );
    let config = service
        .save_connection(config, PasswordUpdate::Keep)
        .await
        .unwrap();
    let conn = service.open(&config.id).await.unwrap();
    let query = TableQuery::new(ObjectPath::new(None, "big"), 100);
    let control = Arc::new(ExportControl::default());
    let out_path: PathBuf = dir.join("out.csv");

    // Sample (rows written, peak RSS) on Linux while the export runs.
    #[cfg(target_os = "linux")]
    let samples = Arc::new(std::sync::Mutex::new(Vec::<(u64, u64)>::new()));
    #[cfg(target_os = "linux")]
    let done = Arc::new(AtomicBool::new(false));
    #[cfg(target_os = "linux")]
    let sampler = {
        let control = Arc::clone(&control);
        let samples = Arc::clone(&samples);
        let done = Arc::clone(&done);
        std::thread::spawn(move || {
            while !done.load(Ordering::Relaxed) {
                let rows = control.rows.load(Ordering::Relaxed);
                let peak = peak_rss_bytes();
                samples.lock().unwrap().push((rows, peak));
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
        })
    };

    let report = conn
        .export_table_to_file(&query, &ExportOptions::default(), &control, &out_path)
        .await
        .unwrap();
    assert_eq!(report.rows, ROWS);

    #[cfg(target_os = "linux")]
    {
        done.store(true, Ordering::Relaxed);
        let _ = sampler.join();
        let samples = samples.lock().unwrap();
        assert!(!samples.is_empty(), "sampler recorded nothing");
        let half = ROWS / 2;
        let first_peak = samples
            .iter()
            .filter(|(rows, _)| *rows < half)
            .map(|(_, peak)| *peak)
            .max()
            .unwrap_or(0);
        let second_peak = samples
            .iter()
            .filter(|(rows, _)| *rows >= half)
            .map(|(_, peak)| *peak)
            .max()
            .unwrap_or(0);
        let absolute = samples.iter().map(|(_, peak)| *peak).max().unwrap_or(0);
        assert!(
            absolute < 200 * 1024 * 1024,
            "absolute peak RSS {absolute} bytes exceeds 200 MB"
        );
        assert!(
            second_peak.saturating_sub(first_peak) < 50 * 1024 * 1024,
            "second-half peak grew by {} bytes over the first half ({first_peak} -> {second_peak})",
            second_peak.saturating_sub(first_peak)
        );
    }

    // The file itself must hold the header plus every row.
    let mut lines = 0u64;
    let mut reader = std::io::BufReader::new(std::fs::File::open(&out_path).unwrap());
    {
        use std::io::BufRead as _;
        let mut line = String::new();
        while reader.read_line(&mut line).unwrap() > 0 {
            lines += 1;
            line.clear();
        }
    }
    assert_eq!(lines, ROWS + 1, "header plus one line per row");

    service.close_all();
    for _ in 0..40 {
        if std::fs::remove_dir_all(&dir).is_ok() || !dir.exists() {
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(25));
    }
}
