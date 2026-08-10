//! Reading and writing the three core tables.
//!
//! Free functions over a `&Connection` rather than methods on `Index`, because the
//! analysers (#22, #23, #24) will write inside their own transactions and need the
//! connection anyway. Nothing here is a repository trait: there is one implementation and
//! there is not going to be a second.

use anyhow::Result;
use rusqlite::{Connection, OptionalExtension as _};

/// A file's row id. Everything else keys off it, and it is the only handle the graph uses.
pub type FileId = i64;

/// One indexed symbol. No id field: callers address symbols by file, not individually,
/// because reanalysis replaces a file's symbols wholesale.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Symbol {
    pub name: String,
    pub fqn: String,
    /// Free-form: "class", "interface", "method", "function". Not an enum, because the
    /// analysers that decide the vocabulary are not written yet, and guessing it now means
    /// a migration when PHP surprises us (§21 forbids guessing).
    pub kind: String,
    pub start: i64,
    pub end: i64,
}

/// Inserts a file, or updates its mtime and size if the path is already known. Returns the
/// id either way, so callers do not need to know which happened.
pub fn insert_file(conn: &Connection, path: &str, mtime_ns: i64, size: i64) -> Result<FileId> {
    conn.execute(
        "INSERT INTO files (path, mtime_ns, size) VALUES (?1, ?2, ?3)
         ON CONFLICT(path) DO UPDATE SET mtime_ns = excluded.mtime_ns, size = excluded.size",
        rusqlite::params![path, mtime_ns, size],
    )?;
    // Not `last_insert_rowid()`: on the UPDATE branch it holds whatever the connection
    // inserted last, which is a different file.
    Ok(conn.query_row("SELECT id FROM files WHERE path = ?1", [path], |row| row.get(0))?)
}

pub fn file_id(conn: &Connection, path: &str) -> Result<Option<FileId>> {
    Ok(conn
        .query_row("SELECT id FROM files WHERE path = ?1", [path], |row| row.get(0))
        .optional()?)
}

/// mtime and size as last indexed, for deciding whether a file changed under us.
pub fn file_stamp(conn: &Connection, path: &str) -> Result<Option<(i64, i64)>> {
    Ok(conn
        .query_row("SELECT mtime_ns, size FROM files WHERE path = ?1", [path], |row| {
            Ok((row.get(0)?, row.get(1)?))
        })
        .optional()?)
}

/// Drops a file and, by cascade, its symbols and every edge touching it. What a deletion
/// on disk maps to.
pub fn remove_file(conn: &Connection, path: &str) -> Result<()> {
    conn.execute("DELETE FROM files WHERE path = ?1", [path])?;
    Ok(())
}

pub fn insert_symbol(conn: &Connection, file: FileId, symbol: &Symbol) -> Result<()> {
    conn.execute(
        "INSERT INTO symbols (file_id, name, fqn, kind, start, end)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        rusqlite::params![file, symbol.name, symbol.fqn, symbol.kind, symbol.start, symbol.end],
    )?;
    Ok(())
}

/// Forgets a file's symbols, for the reanalysis that is about to write new ones. Separate
/// from `remove_file` because the file itself and its incoming edges survive.
pub fn clear_symbols(conn: &Connection, file: FileId) -> Result<()> {
    conn.execute("DELETE FROM symbols WHERE file_id = ?1", [file])?;
    Ok(())
}

pub fn symbols_in_file(conn: &Connection, file: FileId) -> Result<Vec<Symbol>> {
    let mut stmt = conn.prepare(
        "SELECT name, fqn, kind, start, end FROM symbols WHERE file_id = ?1 ORDER BY start",
    )?;
    let rows = stmt.query_map([file], |row| {
        Ok(Symbol {
            name: row.get(0)?,
            fqn: row.get(1)?,
            kind: row.get(2)?,
            start: row.get(3)?,
            end: row.get(4)?,
        })
    })?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

/// Every file declaring `fqn`. A `Vec` rather than an `Option` because a project with two
/// classes of the same FQN is broken but real, and the index should show both rather than
/// pick one.
pub fn files_declaring(conn: &Connection, fqn: &str) -> Result<Vec<String>> {
    let mut stmt = conn.prepare(
        "SELECT DISTINCT files.path FROM symbols
         JOIN files ON files.id = symbols.file_id
         WHERE symbols.fqn = ?1 ORDER BY files.path",
    )?;
    let rows = stmt.query_map([fqn], |row| row.get(0))?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

/// Records "`from` depends on `to`". Re-recording the same edge is a no-op, so an analyser
/// need not check first.
pub fn insert_dependency(conn: &Connection, from: FileId, to: FileId, kind: &str) -> Result<()> {
    conn.execute(
        "INSERT OR IGNORE INTO dependencies (from_file, to_file, kind) VALUES (?1, ?2, ?3)",
        rusqlite::params![from, to, kind],
    )?;
    Ok(())
}

/// Drops the edges *out of* `file`, for reanalysis to replace. Incoming edges belong to
/// other files' analyses and are not ours to delete.
pub fn clear_dependencies_from(conn: &Connection, file: FileId) -> Result<()> {
    conn.execute("DELETE FROM dependencies WHERE from_file = ?1", [file])?;
    Ok(())
}

/// What `file` depends on.
pub fn dependencies_of(conn: &Connection, file: FileId) -> Result<Vec<FileId>> {
    let mut stmt = conn.prepare(
        "SELECT DISTINCT to_file FROM dependencies WHERE from_file = ?1 ORDER BY to_file",
    )?;
    let rows = stmt.query_map([file], |row| row.get(0))?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

/// What depends on `file` — the direction that matters when it changes, and the reason
/// `dependencies_reverse` exists.
///
/// One hop, not the transitive closure. Whether reanalysis needs to walk further is a
/// question for the incremental pass, which is not written yet.
pub fn dependents_of(conn: &Connection, file: FileId) -> Result<Vec<FileId>> {
    let mut stmt = conn.prepare(
        "SELECT DISTINCT from_file FROM dependencies WHERE to_file = ?1 ORDER BY from_file",
    )?;
    let rows = stmt.query_map([file], |row| row.get(0))?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Index;

    fn symbol(name: &str, kind: &str, start: i64) -> Symbol {
        Symbol {
            name: name.into(),
            fqn: format!("App\\Models\\{name}"),
            kind: kind.into(),
            start,
            end: start + 10,
        }
    }

    #[test]
    fn a_file_and_its_symbols_round_trip() {
        let index = Index::in_memory().unwrap();
        let conn = index.connection();

        let file = insert_file(conn, "app/Models/User.php", 1_700_000_000, 512).unwrap();
        insert_symbol(conn, file, &symbol("User", "class", 6)).unwrap();
        insert_symbol(conn, file, &symbol("posts", "method", 120)).unwrap();

        assert_eq!(file_stamp(conn, "app/Models/User.php").unwrap(), Some((1_700_000_000, 512)));
        assert_eq!(
            symbols_in_file(conn, file).unwrap(),
            vec![symbol("User", "class", 6), symbol("posts", "method", 120)]
        );
    }

    #[test]
    fn reindexing_a_file_keeps_its_id_and_updates_the_stamp() {
        // The id has to be stable: dependency edges point at it, and an edit that changed
        // it would orphan the graph.
        let index = Index::in_memory().unwrap();
        let conn = index.connection();

        let first = insert_file(conn, "app/Models/User.php", 1, 100).unwrap();
        let second = insert_file(conn, "app/Models/User.php", 2, 200).unwrap();

        assert_eq!(first, second);
        assert_eq!(file_stamp(conn, "app/Models/User.php").unwrap(), Some((2, 200)));
    }

    #[test]
    fn reindexing_returns_the_right_id_when_another_file_was_inserted_between() {
        // Guards the `last_insert_rowid()` trap: on the UPDATE branch it would return the
        // id of whatever was inserted last, which is Post, not User.
        let index = Index::in_memory().unwrap();
        let conn = index.connection();

        let user = insert_file(conn, "app/Models/User.php", 1, 100).unwrap();
        let post = insert_file(conn, "app/Models/Post.php", 1, 100).unwrap();
        let user_again = insert_file(conn, "app/Models/User.php", 2, 200).unwrap();

        assert_eq!(user_again, user);
        assert_ne!(user_again, post);
    }

    #[test]
    fn clearing_symbols_leaves_the_file_and_its_incoming_edges() {
        let index = Index::in_memory().unwrap();
        let conn = index.connection();

        let user = insert_file(conn, "app/Models/User.php", 1, 100).unwrap();
        let post = insert_file(conn, "app/Models/Post.php", 1, 100).unwrap();
        insert_symbol(conn, user, &symbol("User", "class", 6)).unwrap();
        insert_dependency(conn, post, user, "use").unwrap();

        clear_symbols(conn, user).unwrap();

        assert!(symbols_in_file(conn, user).unwrap().is_empty());
        assert_eq!(file_id(conn, "app/Models/User.php").unwrap(), Some(user));
        assert_eq!(dependents_of(conn, user).unwrap(), vec![post]);
    }

    #[test]
    fn deleting_a_file_cascades_into_symbols_and_edges() {
        // The cascade is why `foreign_keys` is ON. Without it the rows would linger and
        // point at nothing.
        let index = Index::in_memory().unwrap();
        let conn = index.connection();

        let user = insert_file(conn, "app/Models/User.php", 1, 100).unwrap();
        let post = insert_file(conn, "app/Models/Post.php", 1, 100).unwrap();
        insert_symbol(conn, user, &symbol("User", "class", 6)).unwrap();
        insert_dependency(conn, post, user, "use").unwrap();
        insert_dependency(conn, user, post, "use").unwrap();

        remove_file(conn, "app/Models/User.php").unwrap();

        assert_eq!(file_id(conn, "app/Models/User.php").unwrap(), None);
        assert!(symbols_in_file(conn, user).unwrap().is_empty());
        assert!(dependencies_of(conn, post).unwrap().is_empty());
        assert!(dependents_of(conn, post).unwrap().is_empty());
    }

    #[test]
    fn a_dependency_edge_survives_reopening_the_database() {
        // The point of persisting at all: the graph is still there next launch, so a cold
        // start does not have to rebuild it (ADR-0008).
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("index.sqlite");

        let (first, _) = Index::open(&path).unwrap();
        let controller =
            insert_file(first.connection(), "app/Http/Controllers/UserController.php", 1, 100)
                .unwrap();
        let user = insert_file(first.connection(), "app/Models/User.php", 1, 100).unwrap();
        insert_dependency(first.connection(), controller, user, "use").unwrap();
        drop(first);

        let (second, opened) = Index::open(&path).unwrap();
        let conn = second.connection();

        assert_eq!(opened, crate::Opened::Reused);
        let user = file_id(conn, "app/Models/User.php").unwrap().unwrap();
        let controller = file_id(conn, "app/Http/Controllers/UserController.php").unwrap().unwrap();
        assert_eq!(dependents_of(conn, user).unwrap(), vec![controller]);
        assert_eq!(dependencies_of(conn, controller).unwrap(), vec![user]);
    }

    #[test]
    fn recording_the_same_edge_twice_is_one_edge() {
        let index = Index::in_memory().unwrap();
        let conn = index.connection();

        let post = insert_file(conn, "app/Models/Post.php", 1, 100).unwrap();
        let user = insert_file(conn, "app/Models/User.php", 1, 100).unwrap();
        insert_dependency(conn, post, user, "use").unwrap();
        insert_dependency(conn, post, user, "use").unwrap();

        assert_eq!(dependencies_of(conn, post).unwrap(), vec![user]);
    }

    #[test]
    fn edges_of_different_kinds_between_the_same_files_coexist() {
        // `kind` is in the primary key on purpose: a controller can both import a model
        // and render a view that names it, and losing one loses a reanalysis trigger.
        let index = Index::in_memory().unwrap();
        let conn = index.connection();

        let controller =
            insert_file(conn, "app/Http/Controllers/UserController.php", 1, 1).unwrap();
        let user = insert_file(conn, "app/Models/User.php", 1, 1).unwrap();
        insert_dependency(conn, controller, user, "use").unwrap();
        insert_dependency(conn, controller, user, "route").unwrap();

        let count: i64 =
            conn.query_row("SELECT COUNT(*) FROM dependencies", [], |row| row.get(0)).unwrap();
        assert_eq!(count, 2);
        assert_eq!(dependencies_of(conn, controller).unwrap(), vec![user]);
    }

    #[test]
    fn clearing_outgoing_edges_leaves_incoming_ones() {
        let index = Index::in_memory().unwrap();
        let conn = index.connection();

        let controller =
            insert_file(conn, "app/Http/Controllers/UserController.php", 1, 1).unwrap();
        let user = insert_file(conn, "app/Models/User.php", 1, 1).unwrap();
        insert_dependency(conn, controller, user, "use").unwrap();

        clear_dependencies_from(conn, user).unwrap();

        assert_eq!(dependents_of(conn, user).unwrap(), vec![controller]);
    }

    #[test]
    fn an_edge_to_an_unknown_file_is_rejected() {
        // Better to fail the write than to grow a graph pointing at ids that were never
        // files. The analyser's job is to index the target first.
        let index = Index::in_memory().unwrap();
        let conn = index.connection();
        let user = insert_file(conn, "app/Models/User.php", 1, 1).unwrap();

        assert!(insert_dependency(conn, user, 9999, "use").is_err());
    }

    #[test]
    fn a_symbol_is_found_by_its_fully_qualified_name() {
        let index = Index::in_memory().unwrap();
        let conn = index.connection();

        let file = insert_file(conn, "app/Models/User.php", 1, 1).unwrap();
        insert_symbol(conn, file, &symbol("User", "class", 6)).unwrap();

        assert_eq!(
            files_declaring(conn, "App\\Models\\User").unwrap(),
            vec!["app/Models/User.php"]
        );
        assert!(files_declaring(conn, "App\\Models\\Missing").unwrap().is_empty());
    }

    #[test]
    fn an_unknown_path_has_no_id_and_no_stamp() {
        let index = Index::in_memory().unwrap();

        assert_eq!(file_id(index.connection(), "nope.php").unwrap(), None);
        assert_eq!(file_stamp(index.connection(), "nope.php").unwrap(), None);
    }
}
