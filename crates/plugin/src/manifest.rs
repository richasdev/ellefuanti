//! What a plugin declares about itself, and whether the host will honour it.
//!
//! The manifest is the versioned half of the promise ADR-0012 makes. It is parsed here,
//! in plain blocking Rust with no gpui and no process, so the rules that decide whether a
//! plugin loads are testable without spawning anything.
//!
//! The validation is deliberately strict and *refuses* rather than repairs. A manifest
//! declaring an api_version we do not implement is not half-loaded with the parts we
//! recognise: an unknown version means unknown semantics, and guessing at them is how a
//! plugin ends up bound to a command that does something else entirely.

use serde::{Deserialize, Serialize};

/// The plugin API version this build implements.
///
/// A single integer, not semver: the surface is one thing (commands), and a host either
/// speaks it or does not. Bump this when the wire contract changes in a way an existing
/// plugin would notice — a new optional field does not qualify, a renamed method does.
///
/// This is the number in "a plugin API is a promise" (#28). Everything the host will keep
/// promising at version 1 is in this file and in [`crate::protocol`].
pub const PLUGIN_API_VERSION: u32 = 1;

/// The file a plugin directory must contain to be a plugin at all.
pub const MANIFEST_FILE: &str = "plugin.json";

/// A command a plugin contributes to the palette.
///
/// The shape mirrors [`elle_core::Command`] because it becomes one — but with owned
/// `String`s rather than `&'static str`. That difference is the whole reason plugin
/// commands take a separate path through the app: builtin ids are compile-time constants,
/// and these are read off disk at runtime.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommandDecl {
    /// The dotted id the palette searches and the plugin is called back on.
    pub id: String,
    /// Human-readable label, e.g. "Sort Lines".
    pub title: String,
}

/// A plugin's self-description, as read from `plugin.json`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Manifest {
    /// The wire contract this plugin was written against. Checked before anything else.
    pub api_version: u32,
    /// Identifies the plugin, and namespaces the commands it may declare.
    pub name: String,
    /// The plugin's own version. Recorded and shown; the host never interprets it.
    #[serde(default)]
    pub version: String,
    /// The executable to spawn, resolved relative to the plugin's own directory when it
    /// is not absolute — so a plugin ships its binary beside its manifest.
    pub command: String,
    /// Arguments passed to `command`, verbatim.
    #[serde(default)]
    pub args: Vec<String>,
    /// The commands this plugin contributes.
    #[serde(default)]
    pub commands: Vec<CommandDecl>,
}

/// Why a manifest was refused, in terms the user can act on.
///
/// Distinct variants rather than one string because they call for different fixes: an
/// api_version mismatch means "update the plugin or the editor", a namespace violation
/// means "the plugin author must rename its command".
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ManifestError {
    /// Not JSON, or missing a required field.
    Malformed(String),
    /// A version this build does not implement — see [`PLUGIN_API_VERSION`].
    UnsupportedApiVersion { found: u32, supported: u32 },
    /// The name is empty or contains a dot, which would make its namespace ambiguous.
    InvalidName(String),
    /// A command id that does not begin with `<plugin name>.`.
    ForeignCommandId { name: String, id: String },
    /// Nothing to spawn.
    MissingCommand,
}

impl std::fmt::Display for ManifestError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Malformed(detail) => {
                write!(f, "{MANIFEST_FILE} is not a valid manifest: {detail}")
            }
            Self::UnsupportedApiVersion { found, supported } => write!(
                f,
                "plugin declares api_version {found}, but this build implements {supported}"
            ),
            Self::InvalidName(name) => {
                write!(f, "plugin name {name:?} must be non-empty and contain no dots")
            }
            Self::ForeignCommandId { name, id } => {
                write!(
                    f,
                    "plugin {name:?} may only declare commands under {name}., but declares {id:?}"
                )
            }
            Self::MissingCommand => write!(f, "plugin declares no command to run"),
        }
    }
}

impl std::error::Error for ManifestError {}

/// Parses and validates one manifest.
///
/// The order matters: the api_version is checked *first*, so a plugin written against a
/// future version gets told exactly that rather than a confusing complaint about a field
/// whose meaning changed between versions.
pub fn parse(text: &str) -> Result<Manifest, ManifestError> {
    let manifest: Manifest =
        serde_json::from_str(text).map_err(|error| ManifestError::Malformed(error.to_string()))?;

    if manifest.api_version != PLUGIN_API_VERSION {
        return Err(ManifestError::UnsupportedApiVersion {
            found: manifest.api_version,
            supported: PLUGIN_API_VERSION,
        });
    }

    validate(&manifest)?;
    Ok(manifest)
}

/// The rules that hold regardless of how the manifest arrived.
///
/// Split out from [`parse`] so the invariants can be stated once and tested directly,
/// rather than only through a JSON round-trip.
fn validate(manifest: &Manifest) -> Result<(), ManifestError> {
    if manifest.name.is_empty() || manifest.name.contains('.') {
        return Err(ManifestError::InvalidName(manifest.name.clone()));
    }
    if manifest.command.trim().is_empty() {
        return Err(ManifestError::MissingCommand);
    }

    // Namespacing is what keeps a plugin from shadowing `editor.save`. `CommandRegistry`
    // lets a later registration replace an earlier one by design — useful for overriding a
    // title, catastrophic if a third-party plugin can quietly rebind the save command — so
    // the boundary is enforced here, before any plugin command reaches the registry.
    for command in &manifest.commands {
        if !is_namespaced(&manifest.name, &command.id) {
            return Err(ManifestError::ForeignCommandId {
                name: manifest.name.clone(),
                id: command.id.clone(),
            });
        }
    }

    Ok(())
}

/// Whether `id` sits under `name`'s namespace: `sort.lines` for a plugin called `sort`.
///
/// The trailing dot is required, so a plugin named `sort` cannot claim `sorting.everything`
/// by prefix alone.
fn is_namespaced(name: &str, id: &str) -> bool {
    id.strip_prefix(name).is_some_and(|rest| {
        // Something must follow the dot, or the id is the bare namespace.
        rest.starts_with('.') && rest.len() > 1
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn manifest_json(api_version: u32) -> String {
        format!(
            r#"{{"api_version":{api_version},"name":"sort","version":"0.1.0",
                 "command":"./sort-plugin","args":["--stdio"],
                 "commands":[{{"id":"sort.lines","title":"Sort Lines"}}]}}"#
        )
    }

    #[test]
    fn a_well_formed_manifest_parses_into_its_declarations() {
        let manifest = parse(&manifest_json(PLUGIN_API_VERSION)).unwrap();
        assert_eq!(manifest.name, "sort");
        assert_eq!(manifest.version, "0.1.0");
        assert_eq!(manifest.command, "./sort-plugin");
        assert_eq!(manifest.args, ["--stdio"]);
        assert_eq!(
            manifest.commands,
            [CommandDecl { id: "sort.lines".into(), title: "Sort Lines".into() }]
        );
    }

    #[test]
    fn a_future_api_version_is_refused_rather_than_half_loaded() {
        // The promise in #28: an unknown version means unknown semantics. Loading the parts
        // we happen to recognise is how a plugin ends up bound to a command that moved.
        let error = parse(&manifest_json(PLUGIN_API_VERSION + 1)).unwrap_err();
        assert_eq!(
            error,
            ManifestError::UnsupportedApiVersion {
                found: PLUGIN_API_VERSION + 1,
                supported: PLUGIN_API_VERSION,
            }
        );
        // The message has to name both numbers, or the user cannot tell which side is stale.
        let text = error.to_string();
        assert!(text.contains(&(PLUGIN_API_VERSION + 1).to_string()), "{text}");
        assert!(text.contains(&PLUGIN_API_VERSION.to_string()), "{text}");
    }

    #[test]
    fn an_older_api_version_is_refused_too() {
        // Version 0 is not "close enough to 1". Same reasoning in the other direction.
        assert!(matches!(
            parse(&manifest_json(0)),
            Err(ManifestError::UnsupportedApiVersion { found: 0, .. })
        ));
    }

    #[test]
    fn the_version_is_checked_before_anything_else_about_the_manifest() {
        // A manifest that is both stale *and* invalid must report the version, because
        // fixing the other complaint would not make it load.
        let json = r#"{"api_version":99,"name":"","command":""}"#;
        assert!(matches!(parse(json), Err(ManifestError::UnsupportedApiVersion { found: 99, .. })));
    }

    #[test]
    fn a_plugin_cannot_declare_a_command_outside_its_own_namespace() {
        // The load-bearing rule. `CommandRegistry::register` replaces by id, so without
        // this a plugin could rebind `editor.save` to itself and the palette would show
        // one row that quietly does something else.
        let json = r#"{"api_version":1,"name":"sort","command":"./p",
                       "commands":[{"id":"editor.save","title":"Save"}]}"#;
        assert_eq!(
            parse(json).unwrap_err(),
            ManifestError::ForeignCommandId { name: "sort".into(), id: "editor.save".into() }
        );
    }

    #[test]
    fn namespacing_requires_a_real_dot_not_just_a_shared_prefix() {
        // `sort` must not be able to claim `sorting.*` by prefix alone.
        assert!(is_namespaced("sort", "sort.lines"));
        assert!(is_namespaced("sort", "sort.lines.reverse"));
        assert!(!is_namespaced("sort", "sorting.everything"));
        assert!(!is_namespaced("sort", "sort"), "the bare namespace is not a command");
        assert!(!is_namespaced("sort", "sort."), "a dot with nothing after it names nothing");
        assert!(!is_namespaced("sort", "editor.save"));
    }

    #[test]
    fn a_name_with_a_dot_is_refused_because_its_namespace_would_be_ambiguous() {
        let json = r#"{"api_version":1,"name":"my.plugin","command":"./p"}"#;
        assert!(matches!(parse(json), Err(ManifestError::InvalidName(_))));

        let empty = r#"{"api_version":1,"name":"","command":"./p"}"#;
        assert!(matches!(parse(empty), Err(ManifestError::InvalidName(_))));
    }

    #[test]
    fn a_plugin_with_nothing_to_spawn_is_refused() {
        let json = r#"{"api_version":1,"name":"sort","command":"   "}"#;
        assert_eq!(parse(json).unwrap_err(), ManifestError::MissingCommand);
    }

    #[test]
    fn malformed_json_is_an_error_not_a_panic() {
        // §24: a broken plugin on disk must leave the editor working.
        assert!(matches!(parse("{not json"), Err(ManifestError::Malformed(_))));
        assert!(matches!(parse(""), Err(ManifestError::Malformed(_))));
        // Missing the required fields entirely.
        assert!(matches!(parse(r#"{"name":"sort"}"#), Err(ManifestError::Malformed(_))));
    }

    #[test]
    fn optional_fields_may_be_omitted_entirely() {
        // A plugin that contributes nothing yet is still a valid plugin — it is how
        // someone starts writing one, and refusing it would be a hostile first run.
        let manifest = parse(r#"{"api_version":1,"name":"sort","command":"./p"}"#).unwrap();
        assert!(manifest.commands.is_empty());
        assert!(manifest.args.is_empty());
        assert_eq!(manifest.version, "");
    }
}
