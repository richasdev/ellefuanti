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
    /// Accessor attributes by the property they expose: `getFullNameAttribute` and the
    /// new-style `fullName(): Attribute` both report `full_name`.
    pub accessors: Vec<String>,
    /// `$guarded` entries — column names by implication (a guard on a column that does
    /// not exist guards nothing), the weakest claim of the column sources.
    pub guarded: Vec<String>,
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
    // Fillable, two shapes: the `$fillable` property and Laravel's PHP-8
    // `#[Fillable([...])]` attribute. The attribute wins when present — a model uses one
    // or the other, and a project on the attribute has no `$fillable` to find.
    facts.fillable = quoted_list_after(source, "$fillable");
    if facts.fillable.is_empty() {
        facts.fillable = quoted_list_after(source, "#[Fillable(");
    }
    // Casts, two shapes: the `$casts` property and Laravel 11's `casts(): array` method.
    // The method returns an array literal, so the same pair scanner reads it — anchored
    // on `function casts` so a `$casts` elsewhere is not double-counted.
    facts.casts = pair_list_after(source, "$casts");
    if facts.casts.is_empty() {
        facts.casts = pair_list_after(source, "function casts");
    }
    facts.guarded = quoted_list_after(source, "$guarded");

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

    // Accessors, old style: `function getFullNameAttribute(` → `full_name`. The middle
    // must be non-empty — `getAttribute` itself is Eloquent's machinery, not an accessor.
    let mut from = 0;
    while let Some(at) = source[from..].find("function get") {
        let at = from + at + "function get".len();
        from = at;
        let rest: String =
            source[at..].chars().take_while(|c| c.is_alphanumeric() || *c == '_').collect();
        if let Some(middle) = rest.strip_suffix("Attribute")
            && !middle.is_empty()
        {
            facts.accessors.push(snake(middle));
        }
    }

    // Accessors, new style: `function fullName(): Attribute`. The return type is the
    // marker; the argument list of an accessor is empty in every generated shape, so
    // scanning to the first `)` is the honest approximation.
    let mut from = 0;
    while let Some(at) = source[from..].find("function ") {
        let at = from + at + "function ".len();
        from = at;
        let name: String =
            source[at..].chars().take_while(|c| c.is_alphanumeric() || *c == '_').collect();
        let Some(close) = source[at..].find(')') else { continue };
        let after = source[at + close + 1..].trim_start();
        if let Some(return_type) = after.strip_prefix(':')
            && return_type.trim_start().starts_with("Attribute")
            && !name.is_empty()
        {
            facts.accessors.push(snake(&name));
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

/// `FullName` / `fullName` → `full_name` — the camel-to-snake half of Laravel's
/// attribute naming. The plural half lives in the index (`snake_plural`), which is the
/// one seam allowed to guess; this is a mechanical spelling, not a guess.
fn snake(name: &str) -> String {
    let mut out = String::new();
    for (i, c) in name.chars().enumerate() {
        if c.is_uppercase() && i > 0 {
            out.push('_');
        }
        out.extend(c.to_lowercase());
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
            let key = quoted_at(key.trim())?;
            // The value is usually quoted (`'datetime'`) but Laravel 11 casts an enum
            // with `'status' => PostStatus::class` — a bare expression. Take the quoted
            // form when there is one, else the trimmed token as written, so an enum cast
            // is not silently dropped.
            let value = quoted_at(value.trim()).unwrap_or_else(|| value.trim().to_string());
            (!value.is_empty()).then_some((key, value))
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
    fn the_modern_casts_method_and_fillable_attribute_are_read() {
        // Laravel 11's `casts(): array` method instead of `$casts`, and the PHP-8
        // `#[Fillable([...])]` attribute instead of `$fillable`. Real richas-blog shapes.
        let src = "<?php
class Post extends Model {
  #[Fillable(['title', 'slug'])]
  protected function casts(): array {
    return ['status' => PostStatus::class, 'created_at' => 'datetime'];
  }
}
";
        let facts = extract_model(src).expect("a model");
        assert_eq!(facts.fillable, ["title", "slug"], "the #[Fillable] attribute is read");
        assert_eq!(
            facts.casts,
            [
                ("status".to_string(), "PostStatus::class".into()),
                ("created_at".into(), "datetime".into())
            ],
            "the casts() method's array is read like $casts was"
        );
    }

    #[test]
    fn accessors_are_reported_as_the_attribute_they_expose() {
        // Both accessor styles expose `$user->full_name`; the property name is the item.
        let old_style = "<?php\nclass User extends Model {\n  public function getFullNameAttribute() { return ''; }\n  public function getAttribute($key) { return parent::getAttribute($key); }\n}\n";
        let facts = extract_model(old_style).expect("a model");
        assert_eq!(facts.accessors, ["full_name"]);
        // `getAttribute` itself is Eloquent's own machinery, not an accessor.

        let new_style = "<?php\nclass User extends Model {\n  protected function fullName(): Attribute { return Attribute::make(get: fn () => ''); }\n  protected function plainHelper(): string { return ''; }\n}\n";
        let facts = extract_model(new_style).expect("a model");
        assert_eq!(facts.accessors, ["full_name"]);
    }

    #[test]
    fn a_real_richas_blog_model_reads_end_to_end() {
        // The exact modern shapes from richas-blog's Post.php: #[Fillable] attribute,
        // casts() method with an enum ::class cast, and an imageUrl(): Attribute accessor.
        let src = "<?php
class Post extends Model {
  #[Fillable(['title', 'content', 'slug'])]
  protected function casts(): array {
    return ['status' => PostStatus::class, 'created_at' => 'datetime'];
  }
  protected function imageUrl(): Attribute {
    return Attribute::get(fn () => null);
  }
}
";
        let facts = extract_model(src).expect("a model");
        assert_eq!(facts.fillable, ["title", "content", "slug"]);
        assert!(facts.casts.iter().any(|(k, v)| k == "status" && v == "PostStatus::class"));
        assert!(facts.accessors.contains(&"image_url".to_string()), "imageUrl -> image_url");
    }

    #[test]
    fn guarded_columns_are_facts_too() {
        let src = "<?php\nclass User extends Model {\n  protected $guarded = ['id', 'is_admin'];\n}\n";
        let facts = extract_model(src).expect("a model");
        assert_eq!(facts.guarded, ["id", "is_admin"]);
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
