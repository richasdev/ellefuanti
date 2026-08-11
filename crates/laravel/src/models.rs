//! Extracting what a Laravel model declares: its table, columns and relationships.
//!
//! The same contract as `extract_routes` (#68): single-file, source-text, and **an
//! incomplete answer is the accepted failure**. A column added by a package trait, a
//! relationship built dynamically, a table name computed at runtime — none of these are
//! visible here, and the index that stores these facts must carry their provenance so a
//! consumer can say "from the migration" versus "guessed from a cast" (#20's rule that
//! provenance is modelled, not bolted on).
//!
//! Scanning, not parsing: the shapes below are the ones `artisan make:model` and every
//! Laravel codebase actually write, and the scanner reads the buffer text directly so it
//! cannot desync from any tree. The project index (#21) is the consumer.

/// What one model file declares.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ModelFacts {
    /// The class's short name — `User`.
    pub class: String,
    /// `protected $table = 'users'`, when declared. Absent means Laravel's convention
    /// (snake-case plural) applies, and *the consumer* applies it — this module reports
    /// what the file says, not what the framework infers.
    pub table: Option<String>,
    /// `$fillable` entries, in declaration order.
    pub fillable: Vec<String>,
    /// `$casts` keys with their declared cast — `('is_admin', 'boolean')`.
    pub casts: Vec<(String, String)>,
    /// Relationship methods: `(method name, kind, target class as written)`.
    pub relations: Vec<(String, String, String)>,
    /// Query scopes by their *call* name: `scopeActive` is stored as `active`, because
    /// the name the user types is the one completion answers with.
    pub scopes: Vec<String>,
}

/// The relationship builders worth recognising — the set Eloquent documents.
const RELATION_KINDS: [&str; 8] = [
    "hasMany",
    "hasOne",
    "belongsToMany",
    "belongsTo",
    "morphMany",
    "morphOne",
    "morphTo",
    "hasManyThrough",
];

/// Extracts model facts from one PHP source, or `None` when it is not a model at all.
///
/// "Is a model" is judged by `extends` naming `Model` or `Authenticatable` — the two
/// bases `artisan` generates. A base class aliased to something else slips through,
/// which is the single-file contract: better to miss it than to invent one.
pub fn extract_model(source: &str) -> Option<ModelFacts> {
    let class_line_at = source.find("class ")?;
    let rest = &source[class_line_at + 6..];
    let class: String = rest.chars().take_while(|c| c.is_alphanumeric() || *c == '_').collect();
    if class.is_empty() {
        return None;
    }

    // The extends clause, on the same declaration (up to the opening brace).
    let header_end = rest.find('{').unwrap_or(rest.len());
    let header = &rest[..header_end];
    let extends = header.split("extends").nth(1).map(str::trim).unwrap_or("");
    let base = extends.split_whitespace().next().unwrap_or("");
    let base_short = base.rsplit('\\').next().unwrap_or(base);
    if !matches!(base_short, "Model" | "Authenticatable" | "Pivot") {
        return None;
    }

    let mut facts = ModelFacts { class, ..Default::default() };

    facts.table = quoted_after(source, "$table");
    facts.fillable = quoted_list_after(source, "$fillable");
    facts.casts = pair_list_after(source, "$casts");

    // Relationship methods: `function posts() { return $this->hasMany(Post::class); }`.
    // Scanned by builder name because that is the invariant part; the method name is
    // found by walking *backwards* to the `function` that contains the call.
    for kind in RELATION_KINDS {
        let needle = format!("$this->{kind}(");
        let mut from = 0;
        while let Some(at) = source[from..].find(&needle) {
            let at = from + at;
            from = at + needle.len();

            let target = source[at + needle.len()..]
                .split([':', ')', ','])
                .next()
                .unwrap_or("")
                .trim()
                .trim_start_matches('\\')
                .to_string();

            let method = source[..at]
                .rfind("function ")
                .map(|f| {
                    source[f + 9..]
                        .chars()
                        .take_while(|c| c.is_alphanumeric() || *c == '_')
                        .collect::<String>()
                })
                .unwrap_or_default();

            if !method.is_empty() && !target.is_empty() {
                facts.relations.push((method, kind.to_string(), target));
            }
        }
    }

    // Scopes: `function scopeActive(` → `active`. The prefix must be followed by an
    // uppercase letter — a method merely *named* `scope` (or `scoped_thing`) is not one.
    let mut from = 0;
    while let Some(at) = source[from..].find("function scope") {
        let at = from + at + "function scope".len();
        from = at;
        let rest: String =
            source[at..].chars().take_while(|c| c.is_alphanumeric() || *c == '_').collect();
        let mut chars = rest.chars();
        if let Some(first) = chars.next()
            && first.is_uppercase()
        {
            facts.scopes.push(first.to_lowercase().chain(chars).collect());
        }
    }

    Some(facts)
}

/// Columns a migration declares: `(table, column, type)` triples.
///
/// Reads `Schema::create('users', ...)` and `Schema::table('users', ...)` blocks and the
/// `$table->type('name')` calls inside them. `$table->id()` becomes `id`, timestamps
/// become their two columns — the shapes `artisan make:migration` writes. Dropped
/// columns are not tracked; the index rebuild sees the file set as it is, and a column
/// dropped in a later migration will be *added* by the earlier one — that inaccuracy is
/// accepted and recorded here rather than half-fixed.
pub fn extract_migration_columns(source: &str) -> Vec<(String, String, String)> {
    let mut out = Vec::new();

    for opener in ["Schema::create(", "Schema::table("] {
        let mut from = 0;
        while let Some(at) = source[from..].find(opener) {
            let at = from + at;
            from = at + opener.len();

            let Some(table) = quoted_at(&source[at + opener.len()..]) else { continue };

            // The block: to the matching end of this call, approximated by the next
            // `Schema::` or end of file — migrations put one block per call and the
            // approximation only over-reads trailing whitespace.
            let block_end = source[from..].find("Schema::").map_or(source.len(), |p| from + p);
            let block = &source[from..block_end];

            let mut cursor = 0;
            while let Some(call) = block[cursor..].find("$table->") {
                let call = cursor + call + "$table->".len();
                cursor = call;
                let method: String = block[call..]
                    .chars()
                    .take_while(|c| c.is_alphanumeric() || *c == '_')
                    .collect();
                let after = &block[call + method.len()..];

                match method.as_str() {
                    "id" => out.push((table.clone(), "id".to_string(), "id".to_string())),
                    "timestamps" => {
                        out.push((table.clone(), "created_at".into(), "timestamp".into()));
                        out.push((table.clone(), "updated_at".into(), "timestamp".into()));
                    }
                    "softDeletes" => {
                        out.push((table.clone(), "deleted_at".into(), "timestamp".into()));
                    }
                    "rememberToken" => {
                        out.push((table.clone(), "remember_token".into(), "string".into()));
                    }
                    // `foreignId('user_id')` and every `->string('name')` shape: the
                    // first quoted argument is the column, the method is the type.
                    _ => {
                        if let Some(column) = after.strip_prefix('(').and_then(quoted_at) {
                            out.push((table.clone(), column, method));
                        }
                    }
                }
            }
        }
    }

    out
}

/// The first single- or double-quoted string in `text`, if it starts nearby.
fn quoted_at(text: &str) -> Option<String> {
    let trimmed = text.trim_start();
    let quote = trimmed.chars().next().filter(|c| *c == '\'' || *c == '"')?;
    let inner = &trimmed[1..];
    inner.find(quote).map(|end| inner[..end].to_string())
}

/// `$name = 'value'` — the quoted value after a property name.
fn quoted_after(source: &str, property: &str) -> Option<String> {
    let at = source.find(property)?;
    let after = &source[at + property.len()..];
    let eq = after.find('=')?;
    quoted_at(&after[eq + 1..])
}

/// `$name = ['a', 'b']` — every quoted entry of the array after a property name.
fn quoted_list_after(source: &str, property: &str) -> Vec<String> {
    let Some(at) = source.find(property) else { return Vec::new() };
    let after = &source[at + property.len()..];
    let Some(open) = after.find('[') else { return Vec::new() };
    let Some(close) = after[open..].find(']') else { return Vec::new() };
    let body = &after[open + 1..open + close];

    body.split(',').filter_map(|piece| quoted_at(piece.trim())).collect()
}

/// `$casts = ['key' => 'type']` — the pairs of the array after a property name.
fn pair_list_after(source: &str, property: &str) -> Vec<(String, String)> {
    let Some(at) = source.find(property) else { return Vec::new() };
    let after = &source[at + property.len()..];
    let Some(open) = after.find('[') else { return Vec::new() };
    let Some(close) = after[open..].find(']') else { return Vec::new() };
    let body = &after[open + 1..open + close];

    body.split(',')
        .filter_map(|pair| {
            let (key, value) = pair.split_once("=>")?;
            Some((quoted_at(key.trim())?, quoted_at(value.trim())?))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const USER: &str = r#"<?php

namespace App\Models;

use Illuminate\Foundation\Auth\User as Authenticatable;

class User extends Authenticatable
{
    protected $table = 'users';
    protected $fillable = ['name', 'email', 'password'];
    protected $casts = ['is_admin' => 'boolean', 'settings' => 'array'];

    public function posts()
    {
        return $this->hasMany(Post::class);
    }

    public function company()
    {
        return $this->belongsTo(\App\Models\Company::class, 'company_id');
    }
}
"#;

    #[test]
    fn a_generated_model_yields_its_whole_surface() {
        let facts = extract_model(USER).expect("User is a model");
        assert_eq!(facts.class, "User");
        assert_eq!(facts.table.as_deref(), Some("users"));
        assert_eq!(facts.fillable, ["name", "email", "password"]);
        assert_eq!(
            facts.casts,
            [("is_admin".to_string(), "boolean".into()), ("settings".into(), "array".into())]
        );
        assert_eq!(
            facts.relations,
            [
                ("posts".to_string(), "hasMany".into(), "Post".into()),
                // Written fully qualified in the source; reported as written, leading
                // slash trimmed — resolution is the index's job, not the extractor's.
                ("company".into(), "belongsTo".into(), "App\\Models\\Company".into()),
            ]
        );
    }

    #[test]
    fn scopes_are_reported_by_their_call_name() {
        // `scopeActive` is *called* as `active()` — the index stores the name the user
        // types, because completion is the consumer.
        let src = "<?php\nclass User extends Model {\n  public function scopeActive($query) { return $query; }\n  public function scopePopularIn($query, $region) { return $query; }\n  public function scope($query) { return $query; }\n}\n";
        let facts = extract_model(src).expect("a model");
        assert_eq!(facts.scopes, ["active", "popularIn"]);
        // Bare `scope` is not a scope method — there is no name left after the prefix.
    }

    #[test]
    fn non_models_are_none_not_empty() {
        // A controller extends Controller; reporting it as a model with no columns would
        // be a positive claim the analysis cannot support.
        assert_eq!(extract_model("<?php class UserController extends Controller {}"), None);
        assert_eq!(extract_model("<?php function helper() {}"), None);
        // No `$table`: the convention is the *consumer's* to apply.
        let bare = extract_model("<?php class Post extends Model {}").unwrap();
        assert_eq!(bare.table, None);
        assert!(bare.fillable.is_empty());
    }

    #[test]
    fn migrations_yield_table_column_type_triples() {
        let src = r#"<?php
return new class extends Migration {
    public function up(): void
    {
        Schema::create('posts', function (Blueprint $table) {
            $table->id();
            $table->string('title');
            $table->text('body')->nullable();
            $table->foreignId('user_id')->constrained();
            $table->timestamps();
        });
    }
};
"#;
        let cols = extract_migration_columns(src);
        let names: Vec<&str> = cols.iter().map(|(_, c, _)| c.as_str()).collect();
        assert_eq!(names, ["id", "title", "body", "user_id", "created_at", "updated_at"]);
        assert!(cols.iter().all(|(t, _, _)| t == "posts"));
        // The type is the builder method — the honest name for what the file says.
        assert!(cols.contains(&("posts".into(), "user_id".into(), "foreignId".into())));
    }
}
