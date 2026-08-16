//! The Laravel scanners against sources that are not ASCII, at every offset.
//!
//! # Why
//!
//! These functions take raw PHP/Blade source and byte offsets straight from the editor,
//! and they work by hand-written `find`/slice scanning rather than a parse tree — 90 of
//! the codebase's unguarded byte-index slices live in this crate. The 2026-08-14 crash was
//! exactly that shape one crate over (`blade_skip`), and it reached the user through
//! rendering; these reach the user through completion, go-to-definition and hover, which is
//! every keystroke in a PHP file.
//!
//! Offsets are fuzzed rather than chosen. `reference_at` and `wire_context_at` are called
//! with wherever the caret happens to be, and a caret lands mid-character as easily as not
//! — that is the whole bug class.

use elle_laravel::{
    extract_livewire, extract_migration_columns, extract_model, reference_at, reference_at_in_tree,
};
use elle_syntax::{Language, SyntaxTree};
use elle_text::Buffer;

/// Real-shaped Laravel sources with the accents and CJK that this owner's projects
/// actually contain — Portuguese comments and Japanese content, in the places a scanner
/// looks: class headers, property values, string arguments, Blade attributes.
const SOURCES: &[&str] = &[
    // A model with accented table/fillable values and a Japanese comment.
    r#"<?php
namespace App\Models;
// Modelo de configuração — 設定
class Configuração extends Model
{
    protected $table = 'configurações';
    protected $fillable = ['título', 'descrição', '日本語'];
    protected $casts = ['ativo' => 'boolean'];
    public function usuário() { return $this->belongsTo(Usuário::class); }
    public function scopeAtivo($query) { return $query->where('ativo', true); }
    public function getNomeCompletoAttribute() { return $this->título; }
}
"#,
    // Livewire with accented public properties and wire: attributes.
    r#"<?php
namespace App\Livewire;
class Configuração extends Component
{
    public string $título = 'ação';
    public $descrição;
    public function render() { return view('livewire.configuração'); }
}
"#,
    // A migration with multi-byte column names.
    r#"<?php
Schema::create('configurações', function (Blueprint $table) {
    $table->id();
    $table->string('título');
    $table->text('descrição')->nullable();
    $table->string('日本語');
});
"#,
    // Blade with helpers, components and interpolation around multi-byte text.
    r#"<div wire:model="título" class="caixa">
    <x-ação-botão :label="__('Configuração')" />
    {{ route('configurações.índice') }}
    @include('partials.cabeçalho')
    <p>日本語のテキスト 👨‍👩‍👧‍👦</p>
</div>
"#,
    // Degenerate shapes: unterminated constructs right after multi-byte text, which is
    // what a half-typed line looks like.
    "<?php\nclass Ação extends\n",
    "<div wire:model=\"ação\n",
    "{{ route('configuração\n",
    "<?php\n$x = 'ação",
];

#[test]
fn every_offset_into_multibyte_source_is_safe() {
    for source in SOURCES {
        // The whole-source extractors: no offset, but they slice internally.
        let _ = extract_model(source);
        let _ = extract_livewire(source);
        let _ = extract_migration_columns(source);

        // The offset-taking ones, at every byte including the ones inside characters —
        // a caret lands where the user clicks, and `offset_at` hands these whatever it
        // computed. Past the end too: a stale offset outlives an edit.
        for offset in 0..=source.len() + 8 {
            for blade in [true, false] {
                let _ = reference_at(source, offset, blade);
            }
        }
    }
}

/// The same sweep over real files, when this machine has any.
///
/// Fixtures are written by people who type ASCII; the corpus is not. This is the check
/// that found the Blade crash, pointed at the other scanner family.
#[test]
fn real_php_sources_are_safe_at_every_offset() {
    let Some(root) = std::env::var_os("ELLE_PHP_CORPUS") else {
        eprintln!("ELLE_PHP_CORPUS unset; skipping the corpus check");
        return;
    };

    let mut files = Vec::new();
    collect(std::path::Path::new(&root), &mut files, 0);
    assert!(!files.is_empty(), "no PHP files under {}", root.to_string_lossy());

    let mut checked = 0;
    for path in &files {
        let Ok(source) = std::fs::read_to_string(path) else { continue };
        // Whole-file scanners first.
        let _ = extract_model(&source);
        let _ = extract_livewire(&source);
        let _ = extract_migration_columns(&source);

        let blade = path.to_string_lossy().ends_with(".blade.php");

        // The tree-reusing path, parsed once per file rather than once per offset. The
        // first version of this test called the parsing variant at every stride and took
        // minutes on 300 files — a test slow enough that nobody runs it is a test that is
        // not protecting anything. Equivalence between the two paths is pinned in the
        // crate's unit tests, so exercising the fast one here loses no coverage.
        let buffer = Buffer::new(&source);
        let syntax = SyntaxTree::new(Language::Php, &buffer).ok();
        let tree = syntax.as_ref().and_then(|syntax| syntax.tree());

        // Striding rather than every byte: these files run to tens of kilobytes and the
        // point is coverage of *positions*, not of every index in a large file.
        let mut offset = 0;
        while offset <= source.len() {
            match tree {
                Some(tree) => {
                    let _ = reference_at_in_tree(&source, offset, blade, tree);
                }
                None => {
                    let _ = reference_at(&source, offset, blade);
                }
            }
            offset += 7; // coprime with 2, 3 and 4 — lands inside characters of every width
        }
        checked += 1;
    }

    eprintln!("scanned {checked} real PHP sources");
    assert!(checked > 0, "found {} files but could read none", files.len());
}

const MAX_DEPTH: usize = 8;
const MAX_FILES: usize = 300;

fn collect(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>, depth: usize) {
    if depth > MAX_DEPTH || out.len() >= MAX_FILES {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            let name = path.file_name().unwrap_or_default().to_string_lossy().to_string();
            if name != "vendor" && name != "node_modules" && !name.starts_with('.') {
                collect(&path, out, depth + 1);
            }
        } else if path.to_string_lossy().ends_with(".php") {
            out.push(path);
        }
    }
}
