//! Forward-only schema migrations, tracked with `PRAGMA user_version`.
//!
//! Rules:
//! * A released migration is **never edited** (a test pins its checksum). Fix mistakes with a new one.
//! * Each migration runs in one transaction together with the version bump.
//! * Before migrating an existing file, the caller makes a backup copy (see `Store::open`).
//! * A store newer than this build is refused untouched: older versions must not guess.
//! * Prefer additive changes (new column/table/JSON field). Anything that rewrites existing data
//!   uses [`Step::Code`] and gets a test with a fixture of the previous version.

use fg_core::{Error, Result};
use rusqlite::{Connection, Transaction};

pub enum Step {
    Sql(&'static str),
    /// For data rewrites that SQL alone cannot express (e.g. reshaping JSON payloads).
    Code(fn(&Transaction<'_>) -> rusqlite::Result<()>),
}

pub struct Migration {
    pub version: u32,
    pub name: &'static str,
    pub step: Step,
}

pub const MIGRATIONS: &[Migration] = &[Migration {
    version: 1,
    name: "init",
    step: Step::Sql(include_str!("../migrations/0001_init.sql")),
}];

pub fn latest_version() -> u32 {
    latest_of(MIGRATIONS)
}

pub(crate) fn latest_of(migrations: &[Migration]) -> u32 {
    migrations.last().map_or(0, |m| m.version)
}

pub fn current_version(conn: &Connection) -> Result<u32> {
    conn.pragma_query_value(None, "user_version", |r| r.get(0))
        .map_err(storage)
}

pub(crate) fn pending(conn: &Connection, migrations: &[Migration]) -> Result<bool> {
    Ok(current_version(conn)? < latest_of(migrations))
}

/// Refuse stores written by a newer build.
pub(crate) fn check_not_newer(conn: &Connection, migrations: &[Migration]) -> Result<()> {
    let current = current_version(conn)?;
    if current > latest_of(migrations) {
        return Err(Error::Storage(format!(
            "the data store was created by a newer version of fastgrid \
             (schema v{current}; this build supports up to v{})",
            latest_of(migrations)
        )));
    }
    Ok(())
}

pub(crate) fn migrate(conn: &mut Connection, migrations: &[Migration]) -> Result<()> {
    check_not_newer(conn, migrations)?;
    let current = current_version(conn)?;
    for m in migrations.iter().filter(|m| m.version > current) {
        let tx = conn.transaction().map_err(storage)?;
        match &m.step {
            Step::Sql(sql) => tx.execute_batch(sql),
            Step::Code(f) => f(&tx),
        }
        .map_err(|e| Error::Storage(format!("migration {} ({}) failed: {e}", m.version, m.name)))?;
        tx.pragma_update(None, "user_version", m.version)
            .map_err(storage)?;
        tx.commit().map_err(storage)?;
    }
    Ok(())
}

pub(crate) fn storage(e: rusqlite::Error) -> Error {
    Error::Storage(e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// FNV-1a, enough to detect edits.
    fn checksum(s: &str) -> u64 {
        s.bytes().fold(0xcbf2_9ce4_8422_2325, |h, b| {
            (h ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3)
        })
    }

    /// (version, checksum) of every released migration. Append when adding one; never change.
    const PINNED: &[(u32, u64)] = &[(1, 15_196_665_856_061_752_563)];

    #[test]
    fn versions_are_contiguous() {
        for (i, m) in MIGRATIONS.iter().enumerate() {
            assert_eq!(
                m.version as usize,
                i + 1,
                "migration {} out of order",
                m.name
            );
        }
    }

    #[test]
    fn released_migrations_are_immutable() {
        assert_eq!(
            PINNED.len(),
            MIGRATIONS.len(),
            "pin every migration's checksum"
        );
        for (m, (version, pinned)) in MIGRATIONS.iter().zip(PINNED) {
            assert_eq!(m.version, *version);
            if let Step::Sql(sql) = &m.step {
                assert_eq!(
                    checksum(sql),
                    *pinned,
                    "migration {} was edited after release; add a new migration instead",
                    m.name
                );
            }
        }
    }
}
