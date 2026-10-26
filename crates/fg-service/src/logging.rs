//! File logging for the GUI and the CLI.
//!
//! `tracing` events from the service (and, once the binaries link this
//! module, from the app and CLI crates) go to `<data_dir>/logs/fastgrid.log`.
//! Nothing here ever prints to stdout: the CLI's stdout carries protocol
//! bytes, and the GUI has no console. The file rotates by size so a long
//! session cannot fill the disk, and old rotations are deleted.
//!
//! `FASTGRID_LOG` overrides the filter (`warn,fg_service=info,...` by
//! default); the diagnostics export reads the same file's tail.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// The log file's name inside `<data_dir>/logs/`.
pub const LOG_FILE: &str = "fastgrid.log";
/// Rotate once the current file passes this size.
const MAX_BYTES: u64 = 5 * 1024 * 1024;
/// Rotated files kept (`fastgrid.log.1` newest). Older ones are deleted.
const KEEP_ROTATED: usize = 3;

/// The log file's path for a data dir, whether or not logging started.
pub fn log_path(data_dir: &Path) -> PathBuf {
    data_dir.join("logs").join(LOG_FILE)
}

/// Install a process-wide file logger under `<data_dir>/logs/` and return
/// the log path. A failure leaves tracing uninstalled (events are dropped,
/// exactly as before) and the caller decides whether that matters; startup
/// must never fail because logging did. A second call is a no-op: the
/// first subscriber wins, and the path for the second dir is still
/// returned so callers do not branch on it.
pub fn init_file_logging(data_dir: &Path) -> io::Result<PathBuf> {
    let logs = data_dir.join("logs");
    fs::create_dir_all(&logs)?;
    let writer = RotatingWriter::new(&logs)?;
    let path = writer.path();
    let filter = tracing_subscriber::EnvFilter::try_from_env("FASTGRID_LOG").unwrap_or_else(|_| {
        tracing_subscriber::EnvFilter::new(
            "warn,fg_service=info,fg_app=info,fg_cli=info,fg_mcp=info,fg_driver=info",
        )
    });
    let subscriber = tracing_subscriber::fmt()
        // `MakeWriter` is implemented for `Mutex<W: Write>`, not for a
        // bare writer: the guard is what actually receives the bytes.
        .with_writer(std::sync::Mutex::new(writer))
        .with_ansi(false)
        .with_env_filter(filter)
        .finish();
    // Already installed (a test, or a second binary entry point in one
    // process): keep the first subscriber, still report this path.
    let _ = tracing::subscriber::set_global_default(subscriber);
    Ok(path)
}

struct Inner {
    file: File,
    written: u64,
}

/// An append writer that rotates by size. One lock guards the file and
/// its byte count together, so a rotation can never interleave with a
/// write landing in the file that was just renamed away.
pub struct RotatingWriter {
    dir: PathBuf,
    inner: Mutex<Inner>,
}

impl RotatingWriter {
    /// Create `<dir>/fastgrid.log` (and `dir` itself) if missing.
    pub fn new(dir: &Path) -> io::Result<Self> {
        fs::create_dir_all(dir)?;
        let path = dir.join(LOG_FILE);
        let file = OpenOptions::new().create(true).append(true).open(&path)?;
        let written = file.metadata().map(|m| m.len()).unwrap_or(0);
        Ok(Self {
            dir: dir.to_owned(),
            inner: Mutex::new(Inner { file, written }),
        })
    }

    /// The current log file's path.
    pub fn path(&self) -> PathBuf {
        self.dir.join(LOG_FILE)
    }

    fn rotate_locked(&self, inner: &mut Inner) -> io::Result<()> {
        inner.file.flush()?;
        for n in (1..=KEEP_ROTATED).rev() {
            let from = if n == 1 {
                self.dir.join(LOG_FILE)
            } else {
                self.dir.join(format!("{LOG_FILE}.{}", n - 1))
            };
            let to = self.dir.join(format!("{LOG_FILE}.{n}"));
            if from.exists() {
                // The oldest rotation is overwritten, not kept forever.
                let _ = fs::remove_file(&to);
                fs::rename(&from, &to)?;
            }
        }
        inner.file = OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .open(self.dir.join(LOG_FILE))?;
        inner.written = 0;
        Ok(())
    }
}

impl Write for RotatingWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let mut inner = self
            .inner
            .lock()
            .map_err(|_| io::Error::other("log writer lock poisoned"))?;
        if inner.written + buf.len() as u64 > MAX_BYTES {
            self.rotate_locked(&mut inner)?;
        }
        let n = inner.file.write(buf)?;
        inner.written += n as u64;
        Ok(n)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner
            .lock()
            .map_err(|_| io::Error::other("log writer lock poisoned"))?
            .file
            .flush()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_logs(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("fg-log-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn rotation_keeps_only_three_old_files() {
        let dir = temp_logs("rotate");
        let mut writer = RotatingWriter::new(&dir).unwrap();
        // Just over four files' worth, in chunks that each force a rotation
        // once the 5 MiB cap is crossed.
        let chunk = vec![b'x'; 1024 * 1024];
        for _ in 0..22 {
            writer.write_all(&chunk).unwrap();
        }
        writer.flush().unwrap();
        assert!(dir.join(LOG_FILE).exists());
        assert!(dir.join(format!("{LOG_FILE}.1")).exists());
        assert!(dir.join(format!("{LOG_FILE}.2")).exists());
        assert!(dir.join(format!("{LOG_FILE}.3")).exists());
        assert!(!dir.join(format!("{LOG_FILE}.4")).exists());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_subscriber_on_the_writer_records_events() {
        let dir = temp_logs("events");
        let writer = RotatingWriter::new(&dir).unwrap();
        let subscriber = tracing_subscriber::fmt()
            .with_writer(std::sync::Mutex::new(writer))
            .with_ansi(false)
            .with_max_level(tracing::Level::INFO)
            .finish();
        tracing::subscriber::with_default(subscriber, || {
            tracing::info!("hello from the log test");
        });
        let text = fs::read_to_string(dir.join(LOG_FILE)).unwrap();
        assert!(text.contains("hello from the log test"), "{text}");
        let _ = fs::remove_dir_all(&dir);
    }
}
