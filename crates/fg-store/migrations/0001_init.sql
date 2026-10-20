-- fastgrid local store, schema v1. Design: docs/data-model.md, docs/encryption-and-sync.md.
--
-- Everything the user created lives in ONE generic table, `record`. A new kind of data is a new
-- `kind` string, not a schema change, so this schema is meant to stay v1 for a long time.
--
-- * `enc` is the whole payload, sealed with the device key (XChaCha20-Poly1305). Names, hosts,
--   SQL text, secrets: nothing readable is stored outside `enc`.
-- * The only plaintext columns are structural: kind, random ids, parent link, a sort time, and the
--   sync bookkeeping (rev, device_id, updated_at, deleted_at, dirty, key_id).
-- * `id` is always client-generated (UUIDv7, or a derived key built from UUIDs), never an
--   auto-increment number: records from several devices must merge without clashes.
-- * Deletes are tombstones (deleted_at set, enc emptied) so they can sync; they are purged later.
-- * `dirty` is local-only: 1 = changed here and not yet pushed to a sync target.

CREATE TABLE meta (
    key   TEXT PRIMARY KEY NOT NULL,
    value TEXT NOT NULL
) WITHOUT ROWID;

CREATE TABLE record (
    kind       TEXT    NOT NULL,
    id         TEXT    NOT NULL,
    parent_id  TEXT,
    sort_ts    INTEGER,
    rev        INTEGER NOT NULL DEFAULT 1,
    device_id  TEXT    NOT NULL,
    updated_at INTEGER NOT NULL,
    deleted_at INTEGER,
    dirty      INTEGER NOT NULL DEFAULT 1,
    key_id     INTEGER NOT NULL,
    enc        BLOB    NOT NULL,
    PRIMARY KEY (kind, id)
) WITHOUT ROWID;

CREATE INDEX record_by_parent ON record (kind, parent_id, sort_ts DESC, id DESC)
    WHERE deleted_at IS NULL;
CREATE INDEX record_by_time ON record (kind, sort_ts DESC, id DESC)
    WHERE deleted_at IS NULL;
CREATE INDEX record_dirty ON record (dirty) WHERE dirty = 1;

-- Local-only progress of each sync target. Never synced.
CREATE TABLE sync_state (
    remote_id  TEXT    NOT NULL,
    device_id  TEXT    NOT NULL,
    last_seq   INTEGER NOT NULL DEFAULT 0,
    updated_at INTEGER NOT NULL,
    PRIMARY KEY (remote_id, device_id)
) WITHOUT ROWID;
