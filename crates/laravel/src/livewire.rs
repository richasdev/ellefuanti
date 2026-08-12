//! What a Livewire component class declares: properties and actions (#24).
//!
//! Same single-file scan contract as `extract_model` (ADR-0006 amendment): the shapes
//! `php artisan make:livewire` writes, read off the source text, incomplete-by-design.
//! A property added by a trait or an action defined magically is invisible here, and
//! the consumer's badge is what keeps that honest.

/// What one Livewire class file declares.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LivewireFacts {
    /// The class's short name — `UserTable`.
    pub class: String,
    /// Public properties, wire:model's targets, in declaration order.
    pub properties: Vec<String>,
    /// Public methods, wire:click's targets. Lifecycle hooks (`mount`, `render`,
    /// `boot`, `updated…`, `hydrate…`) are excluded — offering `render` to a
    /// `wire:click` would be an answer nobody wants.
    pub actions: Vec<String>,
}

/// Extracts Livewire facts from one PHP source, or `None` when it is not a component.
///
/// "Is a component" is judged by `extends` naming `Component` — what `make:livewire`
/// generates. The same accepted miss as `extract_model`: an aliased base class slips
/// through, and missing it beats inventing it.
pub fn extract_livewire(source: &str) -> Option<LivewireFacts> {
    let class_at = source.find("class ")?;
    let rest = &source[class_at + 6..];
    let class: String = rest.chars().take_while(|c| c.is_alphanumeric() || *c == '_').collect();
    if class.is_empty() {
        return None;
    }
    let header_end = rest.find('{').unwrap_or(rest.len());
    let extends = rest[..header_end].split("extends").nth(1).map(str::trim).unwrap_or("");
    let base = extends.split_whitespace().next().unwrap_or("");
    if base.rsplit('\\').next().unwrap_or(base) != "Component" {
        return None;
    }

    let mut facts = LivewireFacts { class, ..Default::default() };

    // Public properties: `public $name`, `public string $name`, `public ?User $user`.
    let mut from = 0;
    while let Some(at) = source[from..].find("public ") {
        let at = from + at + "public ".len();
        from = at;
        let rest = source[at..].trim_start();
        if let Some(after_fn) = rest.strip_prefix("function ") {
            let name: String =
                after_fn.chars().take_while(|c| c.is_alphanumeric() || *c == '_').collect();
            if !name.is_empty() && !is_lifecycle(&name) {
                facts.actions.push(name);
            }
            continue;
        }
        // A property: the first `$identifier` before the line ends. Skipping the type
        // annotation by looking for the sigil handles `public ?Collection $rows`.
        let line_end = rest.find(['\n', ';']).unwrap_or(rest.len());
        if let Some(sigil) = rest[..line_end].find('$') {
            let name: String = rest[sigil + 1..]
                .chars()
                .take_while(|c| c.is_alphanumeric() || *c == '_')
                .collect();
            if !name.is_empty() {
                facts.properties.push(name);
            }
        }
    }

    Some(facts)
}

/// Livewire's lifecycle and framework methods — declared public, never a wire target.
fn is_lifecycle(name: &str) -> bool {
    matches!(name, "mount" | "render" | "boot" | "booted" | "hydrate" | "dehydrate")
        || name.starts_with("updated")
        || name.starts_with("updating")
}

/// The component class file for a Livewire Blade view, by convention.
///
/// `resources/views/livewire/user-table.blade.php` → `app/Livewire/UserTable.php`,
/// checking the legacy `app/Http/Livewire` too. Nested views map to nested classes
/// (`livewire/admin/user-table` → `app/Livewire/Admin/UserTable.php`). `None` when the
/// file is not under `livewire/` or no candidate exists on disk — silence, not a guess.
pub fn livewire_class_path(
    root: &std::path::Path,
    view: &std::path::Path,
) -> Option<std::path::PathBuf> {
    let relative = view.strip_prefix(root).unwrap_or(view);
    let mut parts: Vec<&str> = relative.iter().filter_map(|part| part.to_str()).collect();
    let position = parts.iter().position(|part| *part == "livewire")?;
    parts.drain(..=position);
    let stem = parts.pop()?.strip_suffix(".blade.php")?;

    let mut class_relative = std::path::PathBuf::new();
    for part in parts {
        class_relative.push(studly(part));
    }
    class_relative.push(format!("{}.php", studly(stem)));

    ["app/Livewire", "app/Http/Livewire"]
        .iter()
        .map(|base| root.join(base).join(&class_relative))
        .find(|candidate| candidate.is_file())
}

/// `user-table` → `UserTable`.
fn studly(kebab: &str) -> String {
    kebab
        .split(['-', '_'])
        .map(|word| {
            let mut chars = word.chars();
            match chars.next() {
                Some(first) => first.to_uppercase().chain(chars).collect::<String>(),
                None => String::new(),
            }
        })
        .collect()
}

/// What a `wire:` attribute's value names — the two lists #24 asks for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WireTarget {
    /// `wire:click`, `wire:submit`, … — a public method.
    Action,
    /// `wire:model` and its modifier forms — a public property.
    Property,
}

/// A cursor inside the quoted value of a `wire:` attribute:
/// `(target kind, value's byte range)`.
///
/// A scanner in the ADR-0006 family: it looks backwards from the cursor for
/// `wire:xxx="` on the same attribute and forward for the closing quote. `wire:` inside
/// ordinary text does not match — the shape requires the `="` and an unclosed value
/// runs to the cursor, which is the mid-typing moment completion exists for.
pub fn wire_context_at(
    source: &str,
    offset: usize,
) -> Option<(WireTarget, std::ops::Range<usize>)> {
    let before = &source[..offset.min(source.len())];
    let attr_at = before.rfind("wire:")?;
    let after_wire = &before[attr_at + "wire:".len()..];
    // The attribute name: up to `=`; modifiers ride after a dot (`wire:model.live`).
    let name_end = after_wire.find('=')?;
    let name = &after_wire[..name_end];
    if name.contains(char::is_whitespace) {
        return None; // `wire:` belongs to an earlier attribute; the cursor is elsewhere.
    }
    let base = name.split('.').next().unwrap_or(name);

    let value_start = attr_at + "wire:".len() + name_end + 1;
    let quote = source[value_start..].chars().next().filter(|c| *c == '"' || *c == '\'')?;
    let content_start = value_start + 1;
    if offset < content_start {
        return None; // On the `=` or the quote itself, not in the value.
    }
    let content_end =
        source[content_start..].find(quote).map(|at| content_start + at).unwrap_or(source.len());
    if offset > content_end {
        return None;
    }

    let target = match base {
        "model" => WireTarget::Property,
        // The event attributes Livewire documents; `wire:navigate` and friends take no
        // expression and are absent on purpose.
        "click" | "submit" | "change" | "keydown" | "keyup" | "blur" | "focus" | "mouseenter"
        | "mouseleave" => WireTarget::Action,
        _ => return None,
    };
    Some((target, content_start..content_end))
}

#[cfg(test)]
mod tests {
    use super::*;

    const COMPONENT: &str = r#"<?php

namespace App\Livewire;

use Livewire\Component;

class UserTable extends Component
{
    public string $search = '';
    public ?int $perPage = 10;
    public $plain;

    public function mount() {}
    public function render() { return view('livewire.user-table'); }
    public function updatedSearch() {}

    public function sortBy(string $column) {}
    public function delete(int $id) {}

    protected function helper() {}
    private function secret() {}
}
"#;

    #[test]
    fn a_component_yields_properties_and_actions_without_lifecycle_noise() {
        let facts = extract_livewire(COMPONENT).expect("a component");
        assert_eq!(facts.class, "UserTable");
        assert_eq!(facts.properties, ["search", "perPage", "plain"]);
        assert_eq!(
            facts.actions,
            ["sortBy", "delete"],
            "mount/render/updatedSearch are the framework's, not wire targets; \
             protected and private are not reachable from a template at all"
        );
    }

    #[test]
    fn non_components_are_none() {
        assert_eq!(extract_livewire("<?php class Foo extends Model {}"), None);
        assert_eq!(extract_livewire("<?php class Bar {}"), None);
    }

    #[test]
    fn the_view_resolves_to_its_class_by_convention() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir_all(root.join("app/Livewire/Admin")).unwrap();
        std::fs::write(root.join("app/Livewire/UserTable.php"), "x").unwrap();
        std::fs::write(root.join("app/Livewire/Admin/AuditLog.php"), "x").unwrap();

        let view = root.join("resources/views/livewire/user-table.blade.php");
        assert_eq!(livewire_class_path(root, &view), Some(root.join("app/Livewire/UserTable.php")));
        let nested = root.join("resources/views/livewire/admin/audit-log.blade.php");
        assert_eq!(
            livewire_class_path(root, &nested),
            Some(root.join("app/Livewire/Admin/AuditLog.php"))
        );
        let not_livewire = root.join("resources/views/welcome.blade.php");
        assert_eq!(livewire_class_path(root, &not_livewire), None);
        let missing = root.join("resources/views/livewire/ghost.blade.php");
        assert_eq!(livewire_class_path(root, &missing), None, "no class on disk, no guess");
    }

    fn wire(fixture: &str) -> Option<(WireTarget, String)> {
        let offset = fixture.find('|').expect("a cursor");
        let source = fixture.replace('|', "");
        wire_context_at(&source, offset).map(|(target, range)| (target, source[range].to_string()))
    }

    #[test]
    fn wire_click_wants_an_action_and_model_wants_a_property() {
        assert_eq!(
            wire(r#"<button wire:click="del|ete">x</button>"#),
            Some((WireTarget::Action, "delete".into()))
        );
        assert_eq!(
            wire(r#"<input wire:model="sea|rch">"#),
            Some((WireTarget::Property, "search".into()))
        );
    }

    #[test]
    fn modifiers_do_not_change_the_target() {
        assert_eq!(
            wire(r#"<input wire:model.live.debounce="q|">"#),
            Some((WireTarget::Property, "q".into()))
        );
    }

    #[test]
    fn an_unclosed_value_is_the_mid_typing_moment() {
        assert_eq!(wire(r#"<button wire:click="so|"#), Some((WireTarget::Action, "so".into())));
    }

    #[test]
    fn outside_the_value_or_off_the_list_is_nothing() {
        assert_eq!(wire(r#"<button wire:cl|ick="x">"#), None, "on the name, not the value");
        assert_eq!(wire(r#"<div wire:navigate />  te|xt"#), None);
        assert_eq!(wire(r#"wire: mentioned in pro|se"#), None);
        assert_eq!(wire(r#"<a wire:click="x"> after|"#), None, "past the closing quote");
    }
}
