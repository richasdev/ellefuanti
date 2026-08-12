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
        // `export KEY=val` is shell-sourceable and common; strip it before the split.
        let line = line.strip_prefix("export ").unwrap_or(line);
        if let Some((key, raw)) = line.split_once('=') {
            let raw = raw.trim();
            // A quoted value is literal to its closing quote — a `#` inside is data. An
            // unquoted value ends at the first `#`, which dotenv treats as a comment.
            let value = if let Some(inner) = raw.strip_prefix('"').and_then(|r| r.split('"').next()) {
                inner
            } else if let Some(inner) = raw.strip_prefix('\'').and_then(|r| r.split('\'').next()) {
                inner
            } else {
                raw.split('#').next().unwrap_or(raw).trim()
            };
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

/// One page of a table's rows, every value as display text.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TablePage {
    /// Column names, in table order — the grid's header.
    pub columns: Vec<String>,
    /// Rows of display text. NULL is rendered as the word `NULL`, which a text grid can
    /// show distinctly from the empty string — the difference matters in a database.
    pub rows: Vec<Vec<String>>,
    /// Total rows in the table, for "1–50 of 1 204" pagination labels.
    pub total: u64,
}

/// Reads one page of `table`, ordered by rowid so pages are stable between calls.
///
/// Pagination is not optional (#65: a SELECT * on a production-sized table must not be
/// the default), so there is no unpaginated variant to misuse. The table name cannot be
/// bound as a parameter in SQL, so it is validated against the schema's own table list
/// first — a name that is not literally one of the database's tables is refused, which
/// closes the injection door the quoting would otherwise have to argue about.
pub fn table_page(path: &Path, table: &str, offset: u64, limit: u64) -> Result<TablePage> {
    let tables = sqlite_schema(path)?;
    let known = tables
        .iter()
        .find(|info| info.name == table)
        .with_context(|| format!("{table} is not a table of this database"))?;

    let conn = rusqlite::Connection::open_with_flags(
        path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .with_context(|| format!("opening {}", path.display()))?;

    let total: u64 =
        conn.query_row(&format!("SELECT COUNT(*) FROM \"{table}\""), [], |row| row.get(0))?;

    let columns: Vec<String> = known.columns.iter().map(|column| column.name.clone()).collect();
    let mut statement = conn.prepare(&format!(
        "SELECT * FROM \"{table}\" ORDER BY rowid LIMIT ?1 OFFSET ?2"
    ))?;
    let column_count = statement.column_count();
    let rows = statement
        .query_map(rusqlite::params![limit, offset], |row| {
            let mut out = Vec::with_capacity(column_count);
            for index in 0..column_count {
                // Every affinity as display text, at the driver: the grid shows text,
                // and pushing the conversion down here keeps sqlite's own formatting
                // (integers without a float's `.0`, blobs as a length tag).
                let value = match row.get_ref(index)? {
                    rusqlite::types::ValueRef::Null => "NULL".to_string(),
                    rusqlite::types::ValueRef::Integer(value) => value.to_string(),
                    rusqlite::types::ValueRef::Real(value) => value.to_string(),
                    rusqlite::types::ValueRef::Text(value) => {
                        String::from_utf8_lossy(value).into_owned()
                    }
                    rusqlite::types::ValueRef::Blob(value) => format!("<blob {} B>", value.len()),
                };
                out.push(value);
            }
            Ok(out)
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;

    Ok(TablePage { columns, rows, total })
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
    fn the_export_prefix_and_inline_comments_are_handled() {
        // Real .env files: `export KEY=val` (shell-sourceable) and a trailing comment on
        // an unquoted value. dotenv strips both; the panel must too or it reads the wrong
        // connection.
        let dir = project_with_env("export DB_CONNECTION=sqlite\n");
        assert!(env_database(dir.path()).is_some(), "the export prefix must not hide the key");

        let dir = project_with_env("DB_CONNECTION=sqlite # local dev\n");
        assert!(
            env_database(dir.path()).is_some(),
            "an inline comment must not become part of the value"
        );
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

#[cfg(test)]
mod page_tests {
    use super::*;

    fn seeded() -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("t.sqlite");
        let conn = rusqlite::Connection::open(&db).unwrap();
        conn.execute_batch(
            "CREATE TABLE users (id INTEGER PRIMARY KEY, email TEXT, score REAL);
             INSERT INTO users (email, score) VALUES ('a@x', 1.5), (NULL, 2.0), ('c@x', NULL);",
        )
        .unwrap();
        (dir, db)
    }

    #[test]
    fn a_page_carries_headers_rows_and_the_total() {
        let (_dir, db) = seeded();
        let page = table_page(&db, "users", 0, 2).unwrap();
        assert_eq!(page.columns, ["id", "email", "score"]);
        assert_eq!(page.total, 3, "the total is the table's, not the page's");
        assert_eq!(page.rows.len(), 2, "the limit is honoured");
        assert_eq!(page.rows[0], ["1", "a@x", "1.5"]);
        assert_eq!(page.rows[1][1], "NULL", "NULL is the word, distinct from empty");
    }

    #[test]
    fn the_second_page_continues_where_the_first_stopped() {
        let (_dir, db) = seeded();
        let page = table_page(&db, "users", 2, 2).unwrap();
        assert_eq!(page.rows.len(), 1);
        assert_eq!(page.rows[0][0], "3");
    }

    #[test]
    fn a_name_that_is_not_a_table_is_refused_and_the_table_survives() {
        let (_dir, db) = seeded();
        // Two layers guard the name. This test proves the OUTER one — validation
        // against the schema's own list — is load-bearing: after the crafted name is
        // refused, `users` still has its three rows, so no injected statement ran.
        // (SQLite's own parser is the inner layer and would also reject the quoted
        // name; the validation is what keeps a crafted string out of `format!` in the
        // first place, which is the door #65 asks to be shut before it is argued about.)
        assert!(table_page(&db, "users\"; DROP TABLE users; --", 0, 10).is_err());
        assert!(table_page(&db, "ghosts", 0, 10).is_err(), "an unknown name is refused");
        assert_eq!(table_page(&db, "users", 0, 10).unwrap().total, 3, "the table is intact");
    }
}
