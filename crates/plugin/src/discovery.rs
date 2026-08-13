//! Finding plugins on disk.
//!
//! A plugin is a directory containing a `plugin.json`. Discovery is a single-level scan of
//! the plugins directory — deliberately not recursive, so a plugin that vendors another
//! plugin's source tree does not accidentally install it.
//!
//! Nothing here downloads anything. ADR-0012 records why: installation is manual, because a
//! plugin runs with the user's full privileges and a one-click install would be a
//! meaningful security promise this project is not yet in a position to make.

use std::path::{Path, PathBuf};

use crate::manifest::{MANIFEST_FILE, Manifest, ManifestError, parse};

/// A plugin found on disk: its manifest, and where it lives.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DiscoveredPlugin {
    pub manifest: Manifest,
    /// The plugin's own directory. Relative `command` paths resolve against this.
    pub root: PathBuf,
}

impl DiscoveredPlugin {
    /// The executable to spawn, with a relative `command` resolved against the plugin's
    /// own directory.
    ///
    /// A bare name like `python3` is left alone so it resolves on `PATH`; anything
    /// containing a separator is treated as a path into the plugin's directory. That rule
    /// is what lets a plugin ship its binary beside its manifest without hardcoding an
    /// absolute path it cannot know at packaging time.
    pub fn executable(&self) -> PathBuf {
        let command = Path::new(&self.manifest.command);
        if command.is_absolute() || !self.manifest.command.contains('/') {
            return command.to_path_buf();
        }
        self.root.join(command)
    }
}

/// One directory that failed to load, and why. Surfaced rather than swallowed: a plugin
/// that silently does not appear is far harder to debug than one that says what is wrong.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DiscoveryFailure {
    pub root: PathBuf,
    pub error: ManifestError,
}

/// Everything found under `dir`, with the failures kept beside the successes.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Discovery {
    pub plugins: Vec<DiscoveredPlugin>,
    pub failures: Vec<DiscoveryFailure>,
}

/// Scans `dir` for plugin directories.
///
/// **Blocking** — it reads the filesystem — so call it off the main thread, per ADR-0007.
///
/// A missing plugins directory is not an error: it is the normal state of an install with
/// no plugins, and returning an empty discovery rather than a failure is what keeps the
/// first run quiet.
pub fn discover(dir: &Path) -> Discovery {
    let mut discovery = Discovery::default();

    let Ok(entries) = std::fs::read_dir(dir) else {
        return discovery;
    };

    // Sorted, so the palette's plugin rows do not reshuffle between runs on the whim of
    // directory order.
    let mut roots: Vec<PathBuf> =
        entries.filter_map(|entry| entry.ok()).map(|entry| entry.path()).collect();
    roots.sort();

    for root in roots {
        if !root.is_dir() {
            continue;
        }
        let manifest_path = root.join(MANIFEST_FILE);
        // A directory with no manifest is not a failed plugin, it is not a plugin — an
        // editor-managed cache or a stray folder must not produce an error the user cannot
        // act on.
        if !manifest_path.is_file() {
            continue;
        }

        match std::fs::read_to_string(&manifest_path) {
            Ok(text) => match parse(&text) {
                Ok(manifest) => discovery.plugins.push(DiscoveredPlugin { manifest, root }),
                Err(error) => discovery.failures.push(DiscoveryFailure { root, error }),
            },
            Err(error) => discovery.failures.push(DiscoveryFailure {
                root,
                error: ManifestError::Malformed(error.to_string()),
            }),
        }
    }

    discovery
}

/// Rejects a plugin whose commands collide with ids already registered.
///
/// The namespace rule in the manifest stops a plugin claiming `editor.save`; this stops two
/// *plugins* claiming the same id, which the namespace rule cannot see because it only ever
/// examines one manifest. Returns the ids that survive, in declaration order.
///
/// First registration wins, rather than last. `CommandRegistry::register` replaces by id,
/// so letting the later one through would mean a plugin's behaviour depended on directory
/// sort order — the kind of bug that only shows up on someone else's machine.
pub fn accepted_commands<'a>(
    plugin: &'a DiscoveredPlugin,
    already_taken: &[String],
) -> Vec<&'a crate::manifest::CommandDecl> {
    plugin.manifest.commands.iter().filter(|command| !already_taken.contains(&command.id)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::CommandDecl;

    /// A scratch plugins directory. Uses the process id and a counter so parallel tests
    /// never share a path.
    fn temp_dir(tag: &str) -> PathBuf {
        use std::sync::atomic::{AtomicU32, Ordering};
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let unique = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir =
            std::env::temp_dir().join(format!("elle-plugin-{}-{tag}-{unique}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn write_plugin(dir: &Path, name: &str, manifest: &str) -> PathBuf {
        let root = dir.join(name);
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join(MANIFEST_FILE), manifest).unwrap();
        root
    }

    const SORT: &str = r#"{"api_version":1,"name":"sort","command":"./sort-plugin",
                           "commands":[{"id":"sort.lines","title":"Sort Lines"}]}"#;

    #[test]
    fn a_missing_plugins_directory_is_quiet_rather_than_an_error() {
        // The normal state of an install with no plugins.
        let discovery = discover(&PathBuf::from("/no/such/directory/anywhere"));
        assert!(discovery.plugins.is_empty());
        assert!(discovery.failures.is_empty());
    }

    #[test]
    fn a_directory_with_a_manifest_is_discovered() {
        let dir = temp_dir("ok");
        write_plugin(&dir, "sort", SORT);

        let discovery = discover(&dir);
        assert_eq!(discovery.plugins.len(), 1, "{discovery:?}");
        assert!(discovery.failures.is_empty());
        assert_eq!(discovery.plugins[0].manifest.name, "sort");
        assert_eq!(discovery.plugins[0].root, dir.join("sort"));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_directory_without_a_manifest_is_not_a_failed_plugin() {
        // A cache directory or a stray folder must not produce an error the user cannot act
        // on — it is simply not a plugin.
        let dir = temp_dir("bare");
        std::fs::create_dir_all(dir.join("not-a-plugin")).unwrap();

        let discovery = discover(&dir);
        assert!(discovery.plugins.is_empty());
        assert!(discovery.failures.is_empty(), "{discovery:?}");

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_broken_manifest_is_reported_and_the_good_ones_still_load() {
        // §24: one bad plugin must not cost the user the working ones.
        let dir = temp_dir("mixed");
        write_plugin(&dir, "sort", SORT);
        write_plugin(&dir, "broken", "{not json");
        write_plugin(&dir, "future", r#"{"api_version":99,"name":"f","command":"./f"}"#);

        let discovery = discover(&dir);
        assert_eq!(discovery.plugins.len(), 1, "{discovery:?}");
        assert_eq!(discovery.plugins[0].manifest.name, "sort");
        assert_eq!(discovery.failures.len(), 2, "{discovery:?}");
        assert!(
            discovery.failures.iter().any(|failure| matches!(
                failure.error,
                ManifestError::UnsupportedApiVersion { found: 99, .. }
            )),
            "{discovery:?}"
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn discovery_is_sorted_so_the_palette_does_not_reshuffle_between_runs() {
        let dir = temp_dir("order");
        for name in ["zeta", "alpha", "mid"] {
            let manifest = format!(
                r#"{{"api_version":1,"name":"{name}","command":"./p",
                     "commands":[{{"id":"{name}.go","title":"Go"}}]}}"#
            );
            write_plugin(&dir, name, &manifest);
        }

        let discovery = discover(&dir);
        let names: Vec<&str> = discovery.plugins.iter().map(|p| p.manifest.name.as_str()).collect();
        assert_eq!(names, ["alpha", "mid", "zeta"]);

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_relative_command_resolves_inside_the_plugins_own_directory() {
        // How a plugin ships a binary beside its manifest without knowing where it will be
        // installed.
        let plugin = DiscoveredPlugin {
            manifest: crate::manifest::parse(SORT).unwrap(),
            root: PathBuf::from("/plugins/sort"),
        };
        assert_eq!(plugin.executable(), PathBuf::from("/plugins/sort/./sort-plugin"));
    }

    #[test]
    fn a_bare_command_name_is_left_for_path_to_resolve() {
        // `python3` must not become `/plugins/sort/python3`, or every interpreted plugin
        // would need to vendor its own interpreter.
        let json = r#"{"api_version":1,"name":"sort","command":"python3"}"#;
        let plugin = DiscoveredPlugin {
            manifest: crate::manifest::parse(json).unwrap(),
            root: PathBuf::from("/plugins/sort"),
        };
        assert_eq!(plugin.executable(), PathBuf::from("python3"));
    }

    #[test]
    fn an_absolute_command_is_used_as_given() {
        let json = r#"{"api_version":1,"name":"sort","command":"/usr/bin/python3"}"#;
        let plugin = DiscoveredPlugin {
            manifest: crate::manifest::parse(json).unwrap(),
            root: PathBuf::from("/plugins/sort"),
        };
        assert_eq!(plugin.executable(), PathBuf::from("/usr/bin/python3"));
    }

    #[test]
    fn the_first_plugin_to_claim_an_id_keeps_it() {
        // Two plugins cannot both own an id, and last-wins would make behaviour depend on
        // directory sort order — a bug that only appears on someone else's machine.
        let plugin = DiscoveredPlugin {
            manifest: crate::manifest::parse(
                r#"{"api_version":1,"name":"sort","command":"./p",
                    "commands":[{"id":"sort.lines","title":"Sort Lines"},
                                {"id":"sort.unique","title":"Unique"}]}"#,
            )
            .unwrap(),
            root: PathBuf::from("/plugins/sort"),
        };

        let accepted = accepted_commands(&plugin, &["sort.lines".to_string()]);
        assert_eq!(accepted, [&CommandDecl { id: "sort.unique".into(), title: "Unique".into() }]);

        // With nothing taken, everything is accepted.
        assert_eq!(accepted_commands(&plugin, &[]).len(), 2);
    }
}
