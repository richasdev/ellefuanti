//! Building and querying the Laravel half of the index (#21).
//!
//! The walk covers `app/Models` and `database/migrations` — where `artisan` puts things.
//! Models elsewhere are missed, which is `extract_model`'s single-file contract carried
//! up a level: an incomplete index that says where each fact came from beats a fuller
//! one that guesses. Cancellable between files, like every walk in this codebase.

use std::path::Path;

use anyhow::Result;
use elle_laravel::{extract_migration_columns, extract_model};
use elle_workspace::CancelFlag;
use rusqlite::Connection;

/// One column, with the strength of the claim behind it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ModelColumn {
    pub name: String,
    pub column_type: String,
    /// `migration` | `cast` | `fillable`.
    pub source: String,
}

/// Rebuilds the Laravel tables from the filesystem.
///
/// Wholesale per build: delete-and-refill inside one transaction. The incremental,
/// dependency-driven pass is #21's follow-up; a rebuild of two directories is milliseconds
/// on real projects and correct by construction, which is the right place to start.
pub fn build(conn: &Connection, root: &Path, cancel: &CancelFlag) -> Result<()> {
    let transaction = conn.unchecked_transaction()?;
    conn.execute("DELETE FROM models", [])?;

    // Models: class facts, columns from casts and fillable.
    let mut migration_columns: Vec<(String, String, String)> = Vec::new();

    for entry in read_dir_sorted(&root.join("app/Models")) {
        if cancel.is_cancelled() {
            return Ok(()); // A cancelled build commits nothing new; the old rows are gone
            // with the transaction dropped, and the next build starts clean.
        }
        let Ok(text) = std::fs::read_to_string(&entry) else { continue };
        let Some(facts) = extract_model(&text) else { continue };

        let relative = entry.strip_prefix(root).unwrap_or(&entry);
        let file_id = crate::records::insert_file(conn, &relative.to_string_lossy(), 0, 0)?;

        conn.execute(
            "INSERT INTO models (file_id, class, table_name) VALUES (?1, ?2, ?3)",
            rusqlite::params![file_id, facts.class, facts.table],
        )?;
        let model_id = conn.last_insert_rowid();

        for name in &facts.fillable {
            conn.execute(
                "INSERT INTO model_columns (model_id, name, type, source) VALUES (?1, ?2, '', 'fillable')",
                rusqlite::params![model_id, name],
            )?;
        }
        for (name, cast) in &facts.casts {
            conn.execute(
                "INSERT INTO model_columns (model_id, name, type, source) VALUES (?1, ?2, ?3, 'cast')",
                rusqlite::params![model_id, name, cast],
            )?;
        }
        for name in &facts.guarded {
            conn.execute(
                "INSERT INTO model_columns (model_id, name, type, source) VALUES (?1, ?2, '', 'guarded')",
                rusqlite::params![model_id, name],
            )?;
        }
        for name in &facts.accessors {
            conn.execute(
                "INSERT INTO model_columns (model_id, name, type, source) VALUES (?1, ?2, '', 'accessor')",
                rusqlite::params![model_id, name],
            )?;
        }
        for (name, kind, target) in &facts.relations {
            conn.execute(
                "INSERT INTO model_relations (model_id, name, kind, target) VALUES (?1, ?2, ?3, ?4)",
                rusqlite::params![model_id, name, kind, target],
            )?;
        }
        for name in &facts.scopes {
            conn.execute(
                "INSERT INTO model_scopes (model_id, name) VALUES (?1, ?2)",
                rusqlite::params![model_id, name],
            )?;
        }
    }

    for entry in read_dir_sorted(&root.join("database/migrations")) {
        if cancel.is_cancelled() {
            return Ok(());
        }
        let Ok(text) = std::fs::read_to_string(&entry) else { continue };
        migration_columns.extend(extract_migration_columns(&text));
    }

    // Migration columns attach to models by table name — declared, or the convention
    // (snake-case plural of the class) applied *here*, at the one seam where the guess
    // is unavoidable and can be seen in one place.
    let models: Vec<(i64, String, Option<String>)> = {
        let mut statement = conn.prepare("SELECT id, class, table_name FROM models")?;
        statement
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))?
            .collect::<std::result::Result<Vec<_>, _>>()?
    };
    for (model_id, class, declared) in &models {
        let effective = declared.clone().unwrap_or_else(|| snake_plural(class));
        for (table, column, column_type) in &migration_columns {
            if *table == effective {
                conn.execute(
                    "INSERT INTO model_columns (model_id, name, type, source)
                     VALUES (?1, ?2, ?3, 'migration')",
                    rusqlite::params![model_id, column, column_type],
                )?;
            }
        }
    }

    transaction.commit()?;
    Ok(())
}

/// The columns a model exposes, migration facts first, then casts, then fillable.
pub fn columns_for_model(conn: &Connection, class: &str) -> Result<Vec<ModelColumn>> {
    let mut statement = conn.prepare(
        "SELECT c.name, c.type, c.source FROM model_columns c
         JOIN models m ON m.id = c.model_id
         WHERE m.class = ?1
         ORDER BY CASE c.source WHEN 'migration' THEN 0 WHEN 'cast' THEN 1 ELSE 2 END, c.rowid",
    )?;
    let rows = statement.query_map([class], |row| {
        Ok(ModelColumn { name: row.get(0)?, column_type: row.get(1)?, source: row.get(2)? })
    })?;
    Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
}

/// The relationships a model declares: `(method, kind, target as written)`.
pub fn relations_for_model(
    conn: &Connection,
    class: &str,
) -> Result<Vec<(String, String, String)>> {
    let mut statement = conn.prepare(
        "SELECT r.name, r.kind, r.target FROM model_relations r
         JOIN models m ON m.id = r.model_id WHERE m.class = ?1 ORDER BY r.rowid",
    )?;
    let rows = statement.query_map([class], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))?;
    Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
}

/// The query scopes a model declares, by call name (`active`, never `scopeActive`).
pub fn scopes_for_model(conn: &Connection, class: &str) -> Result<Vec<String>> {
    let mut statement = conn.prepare(
        "SELECT s.name FROM model_scopes s
         JOIN models m ON m.id = s.model_id WHERE m.class = ?1 ORDER BY s.rowid",
    )?;
    let rows = statement.query_map([class], |row| row.get(0))?;
    Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
}

/// Laravel's table-name convention, naive on purpose: `Person`→`persons`-style
/// irregulars are exactly the models that declare `$table`, which wins over this guess
/// at the one seam that applies it.
fn snake_plural(class: &str) -> String {
    let mut snake = String::new();
    for (i, c) in class.chars().enumerate() {
        if c.is_uppercase() && i > 0 {
            snake.push('_');
        }
        snake.extend(c.to_lowercase());
    }
    if let Some(stem) = snake.strip_suffix('y')
        && !stem.ends_with(['a', 'e', 'i', 'o', 'u'])
    {
        return format!("{stem}ies");
    }
    format!("{snake}s")
}

/// Files of one directory, sorted for deterministic builds. Missing directory = empty:
/// not every project has models, and an index of nothing is a valid index.
fn read_dir_sorted(dir: &Path) -> Vec<std::path::PathBuf> {
    let Ok(entries) = std::fs::read_dir(dir) else { return Vec::new() };
    let mut files: Vec<_> = entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "php"))
        .collect();
    files.sort();
    files
}

#[cfg(test)]
mod tests {
    use super::*;

    fn project() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("app/Models")).unwrap();
        std::fs::create_dir_all(dir.path().join("database/migrations")).unwrap();
        std::fs::write(
            dir.path().join("app/Models/User.php"),
            "<?php\nclass User extends Model {\n  protected $casts = ['is_admin' => 'boolean'];\n  protected $guarded = ['secret'];\n  public function posts() { return $this->hasMany(Post::class); }\n  public function scopeActive($query) { return $query; }\n  public function getFullNameAttribute() { return ''; }\n}\n",
        )
        .unwrap();
        // `$table` deliberately DIFFERENT from the convention (`posts`): the fixture's
        // job is to make "declared wins over guessed" falsifiable, and a declared name
        // that equals the guess cannot fail either way — the first mutation run proved
        // exactly that, passing with the declared table ignored.
        std::fs::write(
            dir.path().join("app/Models/Post.php"),
            "<?php\nclass Post extends Model { protected $table = 'articles'; }\n",
        )
        .unwrap();
        std::fs::write(
            dir.path().join("database/migrations/2026_01_02_create_articles.php"),
            "<?php\nreturn new class extends Migration {\n  public function up(): void {\n    Schema::create('articles', function (Blueprint $table) {\n      $table->id();\n      $table->string('slug');\n    });\n  }\n};\n",
        )
        .unwrap();
        std::fs::write(
            dir.path().join("database/migrations/2026_01_01_create_users.php"),
            "<?php\nreturn new class extends Migration {\n  public function up(): void {\n    Schema::create('users', function (Blueprint $table) {\n      $table->id();\n      $table->string('name');\n      $table->timestamps();\n    });\n  }\n};\n",
        )
        .unwrap();
        dir
    }

    fn open() -> crate::Index {
        crate::Index::in_memory().unwrap()
    }

    #[test]
    fn a_project_indexes_models_columns_and_relations_with_provenance() {
        let dir = project();
        let index = open();
        let conn = index.connection();
        build(conn, dir.path(), &CancelFlag::default()).unwrap();

        let columns = columns_for_model(conn, "User").unwrap();
        // Migration columns first — the strongest claim leads. `User` declares no
        // $table, so the convention (`users`) attached them: the one guess, made at the
        // one audited seam.
        let names: Vec<&str> = columns.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(
            names,
            ["id", "name", "created_at", "updated_at", "is_admin", "secret", "full_name"]
        );
        assert_eq!(columns[0].source, "migration");
        let source_of = |name: &str| {
            columns.iter().find(|c| c.name == name).map(|c| c.source.as_str()).unwrap()
        };
        assert_eq!(source_of("is_admin"), "cast", "provenance survives storage");
        assert_eq!(source_of("secret"), "guarded", "a guard implies the column");
        assert_eq!(
            source_of("full_name"),
            "accessor",
            "an accessor is a property, not a table column — the badge is the difference"
        );

        let relations = relations_for_model(conn, "User").unwrap();
        assert_eq!(relations, [("posts".to_string(), "hasMany".into(), "Post".into())]);

        let scopes = scopes_for_model(conn, "User").unwrap();
        assert_eq!(
            scopes,
            ["active"],
            "the call name, not scopeActive — completion is the consumer"
        );

        // Post's columns attach through its *declared* table (`articles`), not the
        // convention (`posts`) — the assertion that makes declared-wins falsifiable.
        let post: Vec<String> =
            columns_for_model(conn, "Post").unwrap().into_iter().map(|c| c.name).collect();
        assert_eq!(post, ["id", "slug"], "declared table wins over the convention");
    }

    #[test]
    fn rebuilding_replaces_rather_than_accumulates() {
        let dir = project();
        let index = open();
        let conn = index.connection();
        build(conn, dir.path(), &CancelFlag::default()).unwrap();
        build(conn, dir.path(), &CancelFlag::default()).unwrap();

        let columns = columns_for_model(conn, "User").unwrap();
        assert_eq!(columns.len(), 7, "two builds, one set of rows — the index is a cache");
        let scopes = scopes_for_model(conn, "User").unwrap();
        assert_eq!(scopes.len(), 1, "scopes ride the same cascade");
    }

    #[test]
    fn the_naive_plural_is_exactly_that() {
        assert_eq!(snake_plural("User"), "users");
        assert_eq!(snake_plural("BlogPost"), "blog_posts");
        assert_eq!(snake_plural("Category"), "categories");
        // The irregulars are wrong on purpose — those models declare $table, which wins.
        assert_eq!(snake_plural("Person"), "persons");
    }
}
