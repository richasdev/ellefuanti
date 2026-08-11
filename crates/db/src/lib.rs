//! The project database's schema, read-only (#65, governed by ADR-0010).
//!
//! Blocking and executor-agnostic like every domain crate (ADR-0007) — the app wraps
//! calls in `cx.background_spawn`. SQLite only, per ADR-0010: Laravel's default
//! connection since v11, already in the binary via the index's bundled rusqlite, and
//! the first slice of the viewer needs zero new drivers.
//!
//! **The credentials rule (#65) is enforced by shape**: this crate reads `.env` only to
//! find a *file path*, never a password — `DB_PASSWORD` and friends are not parsed at
//! all, so they cannot leak into an error, a log, or a UI string from here. When the
//! MySQL/Postgres drivers arrive behind the same trait, the config struct they take is
//! where that discipline has to be re-stated.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

/// One column of one table, as sqlite reports it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TableColumn {
    pub name: String,
    /// The declared type, verbatim (`INTEGER`, `varchar`). Sqlite's affinity rules mean
    /// this is what the migration wrote, not a normalised form — reported as-is.
    pub column_type: String,
    pub primary_key: bool,
    pub nullable: bool,
}

/// One table and its columns.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TableInfo {
    pub name: String,
    pub columns: Vec<TableColumn>,
}

/// The sqlite database a Laravel project's `.env` points at, if it is a sqlite one.
///
/// `DB_CONNECTION=sqlite` (or absent — Laravel's own default since v11) resolves
/// `DB_DATABASE` against the root when relative, falling back to the framework's
/// `database/database.sqlite`. Any other connection returns `None`: slice 1 is sqlite
/// only (ADR-0010), and *saying nothing* beats claiming a MySQL server this crate
/// cannot reach.
pub fn env_database(root: &Path) -> Option<PathBuf> {
    let env = std::fs::read_to_string(root.join(".env")).unwrap_or_default();
    let mut connection = None;
    let mut database = None;
    for line in env.lines() {
        let line = line.trim();
        if line.starts_with('#') {
            continue;
        }
        if let Some((key, value)) = line.split_once('=') {
            let value = value.trim().trim_matches('"').trim_matches('\'');
            match key.trim() {
                "DB_CONNECTION" => connection = Some(value.to_string()),
                "DB_DATABASE" => database = Some(value.to_string()),
                _ => {}
            }
        }
    }

    if connection.as_deref().unwrap_or("sqlite") != "sqlite" {
        return None;
    }
    let path = match database {
        Some(database) if !database.is_empty() => {
            let path = PathBuf::from(database);
            if path.is_absolute() { path } else { root.join(path) }
        }
        _ => root.join("database/database.sqlite"),
    };
    path.is_file().then_some(path)
}

/// Every user table and its columns, alphabetical, from a sqlite file.
///
/// Opened read-only: a schema *browser* must be incapable of writing by construction,
/// not by discipline — the flag is the guarantee #65's destructive-operation section
/// asks for, applied to the only operations this slice performs.
pub fn sqlite_schema(path: &Path) -> Result<Vec<TableInfo>> {
    let conn = rusqlite::Connection::open_with_flags(
        path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .with_context(|| format!("opening {}", path.display()))?;

    let mut names: Vec<String> = conn
        .prepare(
            "SELECT name FROM sqlite_master
             WHERE type = 'table' AND name NOT LIKE 'sqlite_%' ORDER BY name",
        )?
        .query_map([], |row| row.get(0))?
        .collect::<std::result::Result<_, _>>()?;
    names.sort();

    let mut tables = Vec::with_capacity(names.len());
    for name in names {
        // `pragma_table_info` binds the name as data — no string-built SQL, so a table
        // named `users"; DROP TABLE x` is a name, not a statement.
        let mut statement = conn.prepare("SELECT name, type, pk, \"notnull\" FROM pragma_table_info(?1)")?;
        let columns = statement
            .query_map([&name], |row| {
                Ok(TableColumn {
                    name: row.get(0)?,
                    column_type: row.get(1)?,
                    primary_key: row.get::<_, i64>(2)? != 0,
                    nullable: row.get::<_, i64>(3)? == 0,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        if columns.is_empty() {
            bail!("table {name} reported no columns — not a database this reader understands");
        }
        tables.push(TableInfo { name, columns });
    }
    Ok(tables)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn project_with_env(env: &str) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(".env"), env).unwrap();
        std::fs::create_dir_all(dir.path().join("database")).unwrap();
        std::fs::write(dir.path().join("database/database.sqlite"), "").unwrap();
        dir
    }

    #[test]
    fn the_env_resolves_sqlite_and_only_sqlite() {
        let dir = project_with_env("DB_CONNECTION=sqlite\n");
        assert_eq!(
            env_database(dir.path()),
            Some(dir.path().join("database/database.sqlite")),
            "the framework default path fills in"
        );

        let dir = project_with_env("# no DB_CONNECTION at all\n");
        assert!(env_database(dir.path()).is_some(), "absent means Laravel's default: sqlite");

        let dir = project_with_env("DB_CONNECTION=mysql\nDB_DATABASE=app\nDB_PASSWORD=secret\n");
        assert_eq!(env_database(dir.path()), None, "slice 1 says nothing about MySQL");
    }

    #[test]
    fn a_declared_database_path_wins_and_a_missing_file_is_none() {
        let dir = project_with_env("DB_CONNECTION=sqlite\nDB_DATABASE=storage/app.sqlite\n");
        std::fs::create_dir_all(dir.path().join("storage")).unwrap();
        std::fs::write(dir.path().join("storage/app.sqlite"), "").unwrap();
        assert_eq!(env_database(dir.path()), Some(dir.path().join("storage/app.sqlite")));

        let dir = project_with_env("DB_CONNECTION=sqlite\nDB_DATABASE=/nowhere/x.sqlite\n");
        assert_eq!(env_database(dir.path()), None, "a path that is not a file is silence");
    }

    #[test]
    fn the_schema_reads_tables_columns_and_their_shapes() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("t.sqlite");
        let conn = rusqlite::Connection::open(&db).unwrap();
        conn.execute_batch(
            "CREATE TABLE users (id INTEGER PRIMARY KEY, email TEXT NOT NULL, bio TEXT);
             CREATE TABLE posts (id INTEGER PRIMARY KEY, user_id INTEGER NOT NULL);",
        )
        .unwrap();
        drop(conn);

        let tables = sqlite_schema(&db).unwrap();
        assert_eq!(tables.len(), 2);
        assert_eq!(tables[0].name, "posts", "alphabetical, stable");
        let users = &tables[1];
        assert_eq!(users.columns.len(), 3);
        let id = &users.columns[0];
        assert!(id.primary_key);
        let email = &users.columns[1];
        assert_eq!(email.column_type, "TEXT");
        assert!(!email.nullable, "NOT NULL survives the read");
        assert!(users.columns[2].nullable);
    }

    #[test]
    fn the_reader_cannot_write() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("t.sqlite");
        rusqlite::Connection::open(&db)
            .unwrap()
            .execute_batch("CREATE TABLE t (id INTEGER)")
            .unwrap();

        let conn = rusqlite::Connection::open_with_flags(
            &db,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )
        .unwrap();
        assert!(
            conn.execute("DROP TABLE t", []).is_err(),
            "read-only is a connection flag, not a discipline"
        );
    }
}
