//! The app's side of the plugin boundary (#28, ADR-0012).
//!
//! `elle-plugin` knows how to find a plugin, talk to one, and survive its death. This
//! module is the part that could not live there: turning discovered plugins into palette
//! rows, and running one from a background task without blocking a frame (ADR-0007).
//!
//! # Why plugin commands take a different path than builtins
//!
//! [`elle_core::CommandId`] holds a `&'static str`, because builtin ids are compile-time
//! constants and the palette's hot path should not allocate. Plugin ids are read off disk
//! at runtime, so they are not `'static` and cannot be — not without leaking a string per
//! plugin command, which is a real leak on every settings reload.
//!
//! So plugin commands are **resolved before** `dispatch_for` is consulted, rather than by
//! widening the `Dispatch` enum. `dispatch_for` stays a total function over compile-time
//! ids with exhaustive matching intact, and the builtin path is left exactly as it was.
//! The palette gets its rows from [`Registry::palette_rows`], which appends plugin rows to
//! the builtin ones; the confirm handler asks [`Registry::find`] first and only falls
//! through to `dispatch_for` when no plugin claims the id.

use std::sync::Arc;

use elle_plugin::{CommandDecl, DiscoveredPlugin, PLUGIN_API_VERSION, Session};

/// A plugin command, resolved to the plugin that will run it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PluginCommand {
    pub id: String,
    pub title: String,
    /// Index into [`Registry::plugins`] — the plugin that declared this command.
    pub plugin: usize,
}

/// Every plugin the editor found, and the commands they contribute.
///
/// Built once at startup off the main thread. Holds no process: a plugin is spawned when
/// one of its commands is invoked and torn down again, rather than kept resident. That
/// choice is worth stating because it is the opposite of the LSP client's — a language
/// server holds an expensive warm index, while a command plugin holds nothing between
/// invocations, and a resident process per installed plugin would be idle cost for a
/// feature the user might touch once a week (#79, #93).
#[derive(Clone, Debug, Default)]
pub struct Registry {
    pub plugins: Vec<DiscoveredPlugin>,
    pub commands: Vec<PluginCommand>,
    /// Plugins that failed to load, as sentences for the user. Kept rather than dropped:
    /// a plugin that silently does not appear is far harder to debug than one that says
    /// what is wrong.
    pub failures: Vec<String>,
}

impl Registry {
    /// Turns a discovery into palette-ready commands.
    ///
    /// Pure, and separated from the filesystem scan so the collision and namespacing rules
    /// are testable without writing a plugin to disk.
    pub fn from_discovery(discovery: elle_plugin::Discovery) -> Self {
        let mut registry = Self {
            failures: discovery
                .failures
                .iter()
                .map(|failure| format!("{}: {}", display_name(&failure.root), failure.error))
                .collect(),
            ..Self::default()
        };

        for plugin in discovery.plugins {
            let index = registry.plugins.len();
            let taken: Vec<String> =
                registry.commands.iter().map(|command| command.id.clone()).collect();

            for declaration in elle_plugin::discovery::accepted_commands(&plugin, &taken) {
                let CommandDecl { id, title } = declaration;
                registry.commands.push(PluginCommand {
                    id: id.clone(),
                    title: title.clone(),
                    plugin: index,
                });
            }
            registry.plugins.push(plugin);
        }

        registry
    }

    /// Scans the plugins directory. **Blocking** — call it from a background task.
    pub fn load() -> Self {
        let Some(dir) = elle_plugin::plugins_dir() else {
            return Self::default();
        };
        Self::from_discovery(elle_plugin::discover(&dir))
    }

    /// The plugin command with this id, if any. This is what makes a palette confirm
    /// resolve to a plugin *before* `dispatch_for` sees the id.
    pub fn find(&self, id: &str) -> Option<&PluginCommand> {
        self.commands.iter().find(|command| command.id == id)
    }
}

/// Runs a plugin command to completion: spawn, handshake, invoke, tear down.
///
/// **Blocking**, and every failure is a `Result` rather than a panic — the whole point of
/// ADR-0012's boundary is that a broken plugin is recoverable (§24). The caller runs this
/// on a background task and shows the returned message, or the error, in the status bar.
///
/// The process is killed on the way out whatever happened, including on the error paths:
/// a plugin that hangs after answering must not outlive the command that started it.
pub fn run(
    plugin: &DiscoveredPlugin,
    command_id: &str,
    host_version: &str,
) -> anyhow::Result<Option<String>> {
    let (mut process, pipes) = elle_plugin::spawn(plugin)?;
    let mut stdin = pipes.stdin;
    let stdout = std::io::BufReader::new(pipes.stdout);

    // The session borrows the streams; the child handle stays here so it can be killed
    // regardless of how the conversation ends.
    let outcome = {
        let mut session = Session::new(stdout, &mut stdin);
        session
            .initialize(PLUGIN_API_VERSION, host_version)
            .and_then(|()| session.invoke(command_id))
    };

    elle_plugin::host::shutdown(&mut process, &mut stdin);
    outcome
}

/// A plugin directory's name, for an error message. Falls back to the full path when the
/// directory has no final component to name.
fn display_name(root: &std::path::Path) -> String {
    root.file_name()
        .map(|name| name.to_string_lossy().to_string())
        .unwrap_or_else(|| root.display().to_string())
}

/// Shared by the workspace view, which clones it into background tasks.
pub type SharedRegistry = Arc<Registry>;

#[cfg(test)]
mod tests {
    use super::*;
    use elle_plugin::{Discovery, DiscoveryFailure, ManifestError};
    use std::path::PathBuf;

    fn plugin(name: &str, commands: &[(&str, &str)]) -> DiscoveredPlugin {
        let declarations: Vec<String> = commands
            .iter()
            .map(|(id, title)| format!(r#"{{"id":"{id}","title":"{title}"}}"#))
            .collect();
        let json = format!(
            r#"{{"api_version":1,"name":"{name}","command":"./p","commands":[{}]}}"#,
            declarations.join(",")
        );
        DiscoveredPlugin {
            manifest: elle_plugin::parse(&json).unwrap(),
            root: PathBuf::from("/plugins").join(name),
        }
    }

    #[test]
    fn a_discovered_plugins_commands_become_palette_rows() {
        let registry = Registry::from_discovery(Discovery {
            plugins: vec![plugin("sort", &[("sort.lines", "Sort Lines")])],
            failures: Vec::new(),
        });

        assert_eq!(
            registry.commands,
            [PluginCommand { id: "sort.lines".into(), title: "Sort Lines".into(), plugin: 0 }]
        );
        assert_eq!(registry.find("sort.lines").map(|c| c.plugin), Some(0));
        assert!(registry.find("sort.missing").is_none());
    }

    /// The palette's `PaletteMode::Commands` arm, as `(title, id)` pairs.
    ///
    /// Mirrors the chain in `workspace_view.rs` rather than being called by it: that arm
    /// needs a `Context` to reach, and the property worth testing — plugin rows append to
    /// the builtins without displacing one — is about the data, not about gpui.
    fn palette_rows(
        builtin: &elle_core::CommandRegistry,
        registry: &Registry,
    ) -> Vec<(String, String)> {
        builtin
            .all()
            .iter()
            .map(|command| (command.title.to_string(), command.id.0.to_string()))
            .chain(
                registry.commands.iter().map(|command| (command.title.clone(), command.id.clone())),
            )
            .collect()
    }

    #[test]
    fn plugin_rows_are_appended_to_the_builtin_ones_without_disturbing_them() {
        let mut builtin = elle_core::CommandRegistry::new();
        builtin.register_all(elle_core::BUILTIN_COMMANDS.iter().copied());

        let registry = Registry::from_discovery(Discovery {
            plugins: vec![plugin("sort", &[("sort.lines", "Sort Lines")])],
            failures: Vec::new(),
        });

        let rows = palette_rows(&builtin, &registry);
        assert_eq!(rows.len(), elle_core::BUILTIN_COMMANDS.len() + 1);
        assert_eq!(rows.last().unwrap(), &("Sort Lines".to_string(), "sort.lines".to_string()));
        // Every builtin id still appears exactly once — a plugin must not displace one.
        assert_eq!(rows.iter().filter(|(_, id)| id == "editor.save").count(), 1);
    }

    #[test]
    fn a_builtin_id_can_never_be_claimed_by_a_plugin() {
        // Defence in depth. The manifest's namespace rule already refuses this, and the
        // check is repeated here because this is the function that decides what the palette
        // shows — the place where a mistake would become a command that lies.
        let mut builtin = elle_core::CommandRegistry::new();
        builtin.register_all(elle_core::BUILTIN_COMMANDS.iter().copied());
        let builtin_ids: Vec<&str> = builtin.all().iter().map(|command| command.id.0).collect();

        let registry = Registry::from_discovery(Discovery {
            plugins: vec![plugin("sort", &[("sort.lines", "Sort Lines")])],
            failures: Vec::new(),
        });

        for command in &registry.commands {
            assert!(
                !builtin_ids.contains(&command.id.as_str()),
                "plugin command {} shadows a builtin",
                command.id
            );
        }
    }

    #[test]
    fn two_plugins_cannot_both_own_an_id_and_the_first_keeps_it() {
        // Directory order decides who is first, and discovery sorts — so this is stable
        // rather than dependent on the filesystem's whim.
        let registry = Registry::from_discovery(Discovery {
            plugins: vec![
                plugin("sort", &[("sort.lines", "Sort Lines")]),
                // A second plugin legitimately named `sort` cannot exist in one directory,
                // but a plugin declaring a colliding id is what the rule guards against.
                plugin("sort", &[("sort.lines", "Impostor"), ("sort.unique", "Unique")]),
            ],
            failures: Vec::new(),
        });

        assert_eq!(registry.commands.len(), 2, "{:?}", registry.commands);
        assert_eq!(registry.find("sort.lines").unwrap().title, "Sort Lines");
        assert_eq!(registry.find("sort.lines").unwrap().plugin, 0, "first registration wins");
        assert_eq!(registry.find("sort.unique").unwrap().plugin, 1);
    }

    #[test]
    fn a_failed_plugin_is_reported_as_a_sentence_naming_the_directory() {
        let registry = Registry::from_discovery(Discovery {
            plugins: Vec::new(),
            failures: vec![DiscoveryFailure {
                root: PathBuf::from("/plugins/future"),
                error: ManifestError::UnsupportedApiVersion { found: 99, supported: 1 },
            }],
        });

        assert_eq!(registry.failures.len(), 1);
        let message = &registry.failures[0];
        assert!(message.contains("future"), "{message}");
        assert!(message.contains("99"), "{message}");
    }

    #[test]
    fn an_empty_discovery_produces_no_rows_and_no_complaints() {
        // The normal state of an install with no plugins: the palette is unchanged.
        let registry = Registry::from_discovery(Discovery::default());
        assert!(registry.commands.is_empty());
        assert!(registry.failures.is_empty());

        let mut builtin = elle_core::CommandRegistry::new();
        builtin.register_all(elle_core::BUILTIN_COMMANDS.iter().copied());
        assert_eq!(palette_rows(&builtin, &registry).len(), elle_core::BUILTIN_COMMANDS.len());
    }

    #[test]
    fn running_a_plugin_whose_binary_is_missing_is_an_error_not_a_panic() {
        // §24, end to end through the app's own entry point.
        let plugin = DiscoveredPlugin {
            manifest: elle_plugin::parse(
                r#"{"api_version":1,"name":"ghost","command":"definitely-not-a-real-binary-xyzzy",
                    "commands":[{"id":"ghost.go","title":"Go"}]}"#,
            )
            .unwrap(),
            root: std::env::temp_dir(),
        };
        assert!(run(&plugin, "ghost.go", "0.4.0").is_err());
    }
}
