//! Opening the index, and throwing it away when it is not usable.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result};
use rusqlite::Connection;

use crate::schema::{self, SCHEMA_VERSION};

/// An open index database.
///
/// Blocking, and unaware of any executor — the caller wraps it in `cx.background_spawn`
/// (ADR-0007). Not `Sync`: `rusqlite::Connection` is not, and one connection per background
/// task is simpler than a pool for a database with a single writer.
pub struct Index {
    conn: Connection,
    path: PathBuf,
}

/// How `Index::open` got the database it returned. The caller cares: a rebuild means the
/// index is empty and every file needs analysing, where `Reused` means most do not.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Opened {
    /// No file was there. Fresh schema, no data.
    Created,
    /// An existing file at the current schema version.
    Reused,
    /// A file was there and was discarded — wrong version, corrupt, or unreadable.
    /// Fresh schema, no data, and the caller must repopulate from the filesystem.
    Rebuilt,
}

impl Index {
    /// Opens the index at `path`, creating or rebuilding it as needed.
    ///
    /// This never returns a database at the wrong schema version and never repairs one in
    /// place. ADR-0008 makes the index a cache, never a source of truth, which buys exactly
    /// this: the recovery path for *any* problem is `fs::remove_file` and start over. A
    /// half-repaired index produces wrong autocomplete, which is worse than none (§24).
    ///
    /// Errors are reserved for the filesystem refusing to cooperate — an unwritable
    /// directory, a file that cannot be deleted. Those the caller cannot recover from by
    /// retrying, and the editor must go on working without an index.
    pub fn open(path: impl AsRef<Path>) -> Result<(Self, Opened)> {
        let path = path.as_ref().to_path_buf();
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("creating index directory {}", parent.display()))?;
        }

        let existed = path.exists();
        if existed {
            match Self::open_existing(&path) {
                Some(index) => return Ok((index, Opened::Reused)),
                None => {
                    tracing::info!(path = %path.display(), "discarding unusable project index");
                    fs::remove_file(&path)
                        .with_context(|| format!("discarding index {}", path.display()))?;
                    // SQLite may have left a WAL and shared-memory file beside it. Leaving
                    // them lets a stale WAL replay into the fresh database.
                    for suffix in ["-wal", "-shm"] {
                        let mut sidecar = path.clone().into_os_string();
                        sidecar.push(suffix);
                        let _ = fs::remove_file(PathBuf::from(sidecar));
                    }
                }
            }
        }

        let index = Self::create(&path)?;
        Ok((index, if existed { Opened::Rebuilt } else { Opened::Created }))
    }

    /// An in-memory index, for tests and for callers with no writable state directory.
    /// Gone when dropped, which is the normal fate of a cache anyway.
    pub fn in_memory() -> Result<Self> {
        let conn = Connection::open_in_memory()?;
        configure(&conn)?;
        schema::create(&conn)?;
        Ok(Self { conn, path: PathBuf::from(":memory:") })
    }

    /// `Some` only for a database we can open, is at the current version, and passes a
    /// cheap integrity check. Anything else is `None` — see `schema::version`.
    fn open_existing(path: &Path) -> Option<Self> {
        let conn = Connection::open(path).ok()?;
        configure(&conn).ok()?;
        if schema::version(&conn)? != SCHEMA_VERSION {
            return None;
        }
        // `quick_check` over `integrity_check`: it skips the index-vs-table cross
        // validation, which is the expensive half, and this runs on every launch. A
        // corruption it misses surfaces later as a query error, and the response to that
        // is the same rebuild.
        let ok: String = conn.query_row("PRAGMA quick_check(1)", [], |row| row.get(0)).ok()?;
        (ok == "ok").then_some(Self { conn, path: path.to_path_buf() })
    }

    fn create(path: &Path) -> Result<Self> {
        let conn =
            Connection::open(path).with_context(|| format!("creating index {}", path.display()))?;
        configure(&conn)?;
        schema::create(&conn).with_context(|| format!("writing schema to {}", path.display()))?;
        Ok(Self { conn, path: path.to_path_buf() })
    }

    /// Deletes every row, keeping the schema and the file.
    ///
    /// The rebuild path for a caller that has already opened the index and then decided the
    /// contents are stale — a `git checkout`, say. Reopening would work too; this avoids
    /// dropping a connection the caller is holding.
    pub fn clear(&self) -> Result<()> {
        // `files` cascades into `symbols` and `dependencies`, so one DELETE is the lot.
        self.conn.execute("DELETE FROM files", [])?;
        Ok(())
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The raw connection, for the analysers that will write through it (#22, #23, #24).
    pub fn connection(&self) -> &Connection {
        &self.conn
    }
}

/// PRAGMAs every connection needs, set before any schema work.
fn configure(conn: &Connection) -> Result<()> {
    // Foreign keys are off by default in SQLite, and the cascade from `files` into
    // `symbols` and `dependencies` is what makes reanalysing one file a single DELETE.
    conn.pragma_update(None, "foreign_keys", "ON")?;
    // WAL lets the UI read while a background analysis pass writes. That concurrency is
    // the whole reason the index can be populated without blocking the editor (§13).
    conn.pragma_update(None, "journal_mode", "WAL")?;
    // NORMAL rather than FULL: a power cut can lose the last transaction, and the cost of
    // losing it is reanalysing some files. Not worth an fsync per commit.
    conn.pragma_update(None, "synchronous", "NORMAL")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write as _;

    fn index_path(dir: &tempfile::TempDir) -> PathBuf {
        dir.path().join("state/index.sqlite")
    }

    #[test]
    fn a_missing_database_is_created_with_its_directory() {
        let dir = tempfile::tempdir().unwrap();
        let path = index_path(&dir);

        let (index, opened) = Index::open(&path).unwrap();

        assert_eq!(opened, Opened::Created);
        assert_eq!(schema::version(index.connection()), Some(SCHEMA_VERSION));
        assert!(path.exists(), "the index file should exist after open");
    }

    #[test]
    fn reopening_an_intact_database_reuses_it() {
        let dir = tempfile::tempdir().unwrap();
        let path = index_path(&dir);

        let (first, _) = Index::open(&path).unwrap();
        let id = crate::insert_file(first.connection(), "app/Models/User.php", 1, 2).unwrap();
        drop(first);

        let (second, opened) = Index::open(&path).unwrap();

        assert_eq!(opened, Opened::Reused);
        assert_eq!(crate::file_id(second.connection(), "app/Models/User.php").unwrap(), Some(id));
    }

    #[test]
    fn a_version_mismatch_discards_the_database_and_rebuilds() {
        // The scenario this exists for: the schema changed between releases and the user's
        // on-disk index is from the old one. Nothing is migrated; it is thrown away.
        let dir = tempfile::tempdir().unwrap();
        let path = index_path(&dir);

        let (index, _) = Index::open(&path).unwrap();
        crate::insert_file(index.connection(), "app/Models/User.php", 1, 2).unwrap();
        index
            .connection()
            .execute("UPDATE schema_version SET version = ?1", [SCHEMA_VERSION + 1])
            .unwrap();
        drop(index);

        let (rebuilt, opened) = Index::open(&path).unwrap();

        assert_eq!(opened, Opened::Rebuilt);
        assert_eq!(schema::version(rebuilt.connection()), Some(SCHEMA_VERSION));
        assert_eq!(
            crate::file_id(rebuilt.connection(), "app/Models/User.php").unwrap(),
            None,
            "a rebuilt index starts empty — the caller repopulates from the filesystem"
        );
    }

    #[test]
    fn an_older_version_is_discarded_too_rather_than_upgraded() {
        // Symmetry matters: downgrading the app must not try to read a newer file, and
        // upgrading must not try to read an older one. Both are just "not ours".
        let dir = tempfile::tempdir().unwrap();
        let path = index_path(&dir);

        let (index, _) = Index::open(&path).unwrap();
        index.connection().execute("UPDATE schema_version SET version = 0", []).unwrap();
        drop(index);

        let (_, opened) = Index::open(&path).unwrap();

        assert_eq!(opened, Opened::Rebuilt);
    }

    #[test]
    fn a_file_that_is_not_a_database_is_discarded() {
        let dir = tempfile::tempdir().unwrap();
        let path = index_path(&dir);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, "this is not a SQLite database, it is a text file").unwrap();

        let (index, opened) = Index::open(&path).unwrap();

        assert_eq!(opened, Opened::Rebuilt);
        assert_eq!(schema::version(index.connection()), Some(SCHEMA_VERSION));
    }

    #[test]
    fn a_database_missing_the_version_table_is_discarded() {
        // A plausible corruption shape, and the one that would be most tempting to "fix":
        // real SQLite, real tables, no idea what version it is.
        let dir = tempfile::tempdir().unwrap();
        let path = index_path(&dir);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch("CREATE TABLE files (id INTEGER PRIMARY KEY, path TEXT)").unwrap();
        drop(conn);

        let (index, opened) = Index::open(&path).unwrap();

        assert_eq!(opened, Opened::Rebuilt);
        assert_eq!(schema::version(index.connection()), Some(SCHEMA_VERSION));
    }

    #[test]
    fn a_truncated_database_is_discarded() {
        let dir = tempfile::tempdir().unwrap();
        let path = index_path(&dir);

        let (index, _) = Index::open(&path).unwrap();
        crate::insert_file(index.connection(), "app/Models/User.php", 1, 2).unwrap();
        drop(index);

        // Overwrite the header with garbage, keeping the file's length. Deleting bytes
        // would be caught by the open; this has to reach the integrity check.
        let mut file = fs::OpenOptions::new().write(true).open(&path).unwrap();
        file.write_all(&[0xFF; 200]).unwrap();
        drop(file);

        let (rebuilt, opened) = Index::open(&path).unwrap();

        assert_eq!(opened, Opened::Rebuilt);
        assert_eq!(crate::file_id(rebuilt.connection(), "app/Models/User.php").unwrap(), None);
    }

    #[test]
    fn only_one_version_row_can_exist() {
        let index = Index::in_memory().unwrap();

        let second = index
            .connection()
            .execute("INSERT INTO schema_version (id, version) VALUES (2, 99)", []);

        assert!(second.is_err(), "the CHECK on id should reject a second version row");
    }

    #[test]
    fn clearing_empties_every_table_but_keeps_the_schema() {
        let index = Index::in_memory().unwrap();
        let user = crate::insert_file(index.connection(), "app/Models/User.php", 1, 2).unwrap();
        let post = crate::insert_file(index.connection(), "app/Models/Post.php", 1, 2).unwrap();
        crate::insert_symbol(
            index.connection(),
            user,
            &crate::Symbol {
                name: "User".into(),
                fqn: "App\\Models\\User".into(),
                kind: "class".into(),
                start: 0,
                end: 10,
            },
        )
        .unwrap();
        crate::insert_dependency(index.connection(), post, user, "use").unwrap();

        index.clear().unwrap();

        assert_eq!(crate::file_id(index.connection(), "app/Models/User.php").unwrap(), None);
        assert!(crate::symbols_in_file(index.connection(), user).unwrap().is_empty());
        assert!(crate::dependents_of(index.connection(), user).unwrap().is_empty());
        assert_eq!(schema::version(index.connection()), Some(SCHEMA_VERSION));
    }
}
