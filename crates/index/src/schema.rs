//! The schema, its version, and the one-way door of changing it.

use anyhow::Result;
use rusqlite::Connection;

/// Bumped whenever `SCHEMA` changes in a way that makes an existing file unreadable —
/// which, given the recovery strategy, is *every* change. There is no migration path and
/// deliberately so: the index is derived data (ADR-0008), so "migrate" and "delete and
/// rebuild" produce the same rows, and only one of them can be got wrong.
///
/// Forgetting to bump this is the failure mode to watch for. It does not corrupt anything —
/// the old file is opened against new query code, and the mismatch surfaces as a query
/// error rather than silently wrong autocomplete.
pub const SCHEMA_VERSION: i64 = 1;

/// Every table, created in one transaction.
///
/// Only `files`, `symbols` and `dependencies` live here. The Laravel tables (models,
/// routes, migrations, Livewire, Blade) arrive with the analysers that populate them —
/// a table nothing writes to is a schema that cannot be validated.
const SCHEMA: &str = r#"
CREATE TABLE schema_version (
    -- One row, enforced by the CHECK: a version table that can hold two versions has no
    -- opinion about which is current.
    id      INTEGER PRIMARY KEY CHECK (id = 1),
    version INTEGER NOT NULL
);

CREATE TABLE files (
    id       INTEGER PRIMARY KEY,
    -- Project-relative, forward slashes, matching elle-workspace::IndexedFile::relative.
    -- Absolute paths would invalidate the whole index when the project moves.
    path     TEXT NOT NULL UNIQUE,
    -- Filesystem mtime and size, for deciding whether a file needs reanalysis without
    -- reading it. Cheap and wrong in rare cases (same size, same second) — the incremental
    -- pass that relies on this is #21's follow-up work and can add a hash if it must.
    mtime_ns INTEGER NOT NULL,
    size     INTEGER NOT NULL
);

CREATE TABLE symbols (
    id      INTEGER PRIMARY KEY,
    file_id INTEGER NOT NULL REFERENCES files(id) ON DELETE CASCADE,
    -- Unqualified name, for "go to symbol" typing.
    name    TEXT NOT NULL,
    -- Fully-qualified where PHP has one (App\Models\User), else same as name.
    fqn     TEXT NOT NULL,
    kind    TEXT NOT NULL,
    -- Byte offsets into the file, so opening a symbol does not re-parse to find it.
    -- Byte, not line/column: the editor's buffer is byte-indexed and converting once at
    -- the point of use beats storing a position that goes stale differently.
    start   INTEGER NOT NULL,
    end     INTEGER NOT NULL
);

CREATE INDEX symbols_by_file ON symbols(file_id);
CREATE INDEX symbols_by_name ON symbols(name);
CREATE INDEX symbols_by_fqn ON symbols(fqn);

-- The dependency graph: "if `from_file` changed, `to_file` may need reanalysis".
-- Edges are directed and deduplicated; a file importing another twice is one edge.
CREATE TABLE dependencies (
    from_file INTEGER NOT NULL REFERENCES files(id) ON DELETE CASCADE,
    to_file   INTEGER NOT NULL REFERENCES files(id) ON DELETE CASCADE,
    kind      TEXT NOT NULL,
    PRIMARY KEY (from_file, to_file, kind)
) WITHOUT ROWID;

-- Traversal runs in both directions: forward to find what a file needs, reverse to find
-- what needs it. The primary key covers the forward case; this covers the reverse.
CREATE INDEX dependencies_reverse ON dependencies(to_file);
"#;

/// Creates every table and stamps the version. Caller guarantees the database is empty.
pub fn create(conn: &Connection) -> Result<()> {
    let tx = conn.unchecked_transaction()?;
    tx.execute_batch(SCHEMA)?;
    tx.execute("INSERT INTO schema_version (id, version) VALUES (1, ?1)", [SCHEMA_VERSION])?;
    tx.commit()?;
    Ok(())
}

/// The version stamped in this database, or `None` if it is not one of ours.
///
/// Everything unexpected collapses to `None`: no `schema_version` table, no row, a corrupt
/// header. The caller's response to all of them is identical — throw the file away — so
/// distinguishing them would only produce error types nobody branches on.
pub fn version(conn: &Connection) -> Option<i64> {
    conn.query_row("SELECT version FROM schema_version WHERE id = 1", [], |row| row.get(0)).ok()
}
