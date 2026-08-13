//! The plugin boundary: discovery, manifests, the wire protocol, and the process host.
//!
//! ADR-0012 settles what a plugin *is* — a child process speaking newline-delimited
//! JSON-RPC over stdio, chosen because WASM measured 9.83 MB against 0.09 MB of binary
//! headroom, and because a dynamic library cannot satisfy §24's rule that a crashing plugin
//! must not take the editor down.
//!
//! This crate must never depend on gpui (ADR-0004) and never spawns a task of its own
//! (ADR-0007): everything is plain blocking Rust, driven by the app from a background task.
//!
//! # What a plugin may extend, at version 1
//!
//! **Commands, and nothing else.** #28 also lists panels, themes, language support and
//! completion providers; ADR-0012 deliberately does not authorise them, because a plugin
//! API is a promise and this one is kept small enough to be worth making. Each of the
//! others is a materially different API and gets its own decision.

pub mod discovery;
pub mod host;
pub mod manifest;
pub mod protocol;

pub use discovery::{DiscoveredPlugin, Discovery, DiscoveryFailure, discover};
pub use host::{PluginPipes, PluginProcess, Session, spawn};
pub use manifest::{
    CommandDecl, MANIFEST_FILE, Manifest, ManifestError, PLUGIN_API_VERSION, parse,
};
pub use protocol::{PluginEvent, parse_line};

use std::path::PathBuf;

/// Where the editor looks for plugins.
///
/// Beside the settings file rather than inside the app bundle, so plugins survive an
/// upgrade — a bundle is replaced wholesale on update, and plugins installed into one
/// would vanish with it.
///
/// `None` when there is no home directory to hang it off, which is the same answer the
/// settings layer gives in that situation: no directory means no plugins, not a crash.
pub fn plugins_dir() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .map(|home| PathBuf::from(home).join(".config").join("ellefuanti").join("plugins"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plugins_live_beside_the_settings_rather_than_inside_the_bundle() {
        // An app bundle is replaced wholesale on update; plugins installed into one would
        // vanish with it.
        let dir = plugins_dir().expect("the test environment has a HOME");
        assert!(dir.ends_with("ellefuanti/plugins"), "{}", dir.display());
        assert!(!dir.to_string_lossy().contains(".app/"), "{}", dir.display());
    }
}
