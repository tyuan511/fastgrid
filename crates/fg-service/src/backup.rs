//! Encrypted backup: thin [`Service`] wrappers over the store's bundle
//! format. The UI calls these; the file format, encryption and conflict
//! rules live in `fg-store`.
//!
//! The backend error strings are surfaced verbatim by the launcher
//! panel: a wrong password reads "wrong password, or the file is
//! damaged", and a file from a newer fastgrid reads "this backup was
//! written by a newer version of fastgrid". Rewording them here would
//! only hide the cause.

use fg_core::Result;
use fg_store::{ExportOptions, ImportReport};

use crate::Service;
use crate::lock_error;

impl Service {
    /// Export an encrypted backup file's bytes for `options`.
    pub fn export_backup(&self, options: &ExportOptions, password: &str) -> Result<Vec<u8>> {
        let store = self.inner.store.lock().map_err(|_| lock_error())?;
        store.export_bundle(options, password)
    }

    /// Import an encrypted backup file's bytes, reporting what landed.
    pub fn import_backup(&self, bytes: &[u8], password: &str) -> Result<ImportReport> {
        let store = self.inner.store.lock().map_err(|_| lock_error())?;
        store.import_bundle(bytes, password)
    }
}
