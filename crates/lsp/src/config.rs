//! How to launch and configure a language server.
//!
//! This type is the whole substitutability story (RISKS.md #2). A backend is described
//! entirely by *data* — a command, its arguments, its initialization options — so
//! swapping Intelephense for phpactor, or for an eventual first-party engine, is a
//! different `ServerConfig` value and no code change anywhere in this crate.
//!
//! The rule this enforces: **there is no `if server == "intelephense"` anywhere.** The
//! moment a backend's quirk needs special handling in shared code, substitutability is
//! gone on paper only, which is precisely the failure RISKS.md #2 describes. If a
//! backend needs something, it is expressed here as configuration or it is not
//! expressed at all.
//!
//! Note what is deliberately *absent*: no `enum Backend`, no licence-key field, no
//! PHP-specific anything. `initialization_options` is opaque JSON precisely so that
//! Intelephense's licence key and phpactor's completely different option tree are the
//! same kind of thing to this crate — a blob it forwards without reading.

use std::collections::HashMap;
use std::path::PathBuf;

use serde_json::Value;

/// Everything needed to start one language server and say hello to it.
#[derive(Clone, Debug)]
pub struct ServerConfig {
    /// A name for logs and error messages only. Never branched on.
    pub name: String,
    /// Executable to spawn. Resolved via `PATH` if not absolute.
    pub command: String,
    pub args: Vec<String>,
    /// Extra environment variables for the child process.
    pub env: HashMap<String, String>,
    /// Working directory, and the project root reported during `initialize`.
    pub root: PathBuf,
    /// The `initializationOptions` blob, forwarded verbatim.
    ///
    /// Opaque on purpose: this crate must not know what a licence key is, nor which
    /// backend needs one.
    pub initialization_options: Option<Value>,
    /// Language ids this server handles, used by the caller to route documents.
    pub language_ids: Vec<String>,
}

impl ServerConfig {
    pub fn new(
        name: impl Into<String>,
        command: impl Into<String>,
        root: impl Into<PathBuf>,
    ) -> Self {
        Self {
            name: name.into(),
            command: command.into(),
            args: Vec::new(),
            env: HashMap::new(),
            root: root.into(),
            initialization_options: None,
            language_ids: Vec::new(),
        }
    }

    pub fn with_args<I, S>(mut self, args: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.args = args.into_iter().map(Into::into).collect();
        self
    }

    pub fn with_env(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.env.insert(key.into(), value.into());
        self
    }

    pub fn with_initialization_options(mut self, options: Value) -> Self {
        self.initialization_options = Some(options);
        self
    }

    pub fn with_language_ids<I, S>(mut self, ids: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.language_ids = ids.into_iter().map(Into::into).collect();
        self
    }

    pub fn handles_language(&self, language_id: &str) -> bool {
        self.language_ids.iter().any(|id| id == language_id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Both of these live in a test, not in the crate's API, and that placement is the
    /// point: shipping a `ServerConfig::intelephense()` constructor would put a backend
    /// name into the client's surface and start the erosion RISKS.md #2 warns about.
    /// The app crate builds these from user settings.
    #[test]
    fn two_php_backends_are_described_by_data_alone() {
        let intelephense = ServerConfig::new("intelephense", "intelephense", "/srv/app")
            .with_args(["--stdio"])
            .with_language_ids(["php"])
            .with_initialization_options(serde_json::json!({
                "storagePath": "/tmp/intelephense",
                "licenceKey": "SOME-KEY",
            }));

        let phpactor = ServerConfig::new("phpactor", "phpactor", "/srv/app")
            .with_args(["language-server"])
            .with_language_ids(["php"])
            .with_initialization_options(serde_json::json!({
                "language_server_php_cs_fixer.enabled": false,
            }));

        // The two differ only in field values. Nothing in this crate can tell them
        // apart, which is exactly the property that makes them drop-in replacements.
        assert_eq!(intelephense.language_ids, phpactor.language_ids);
        assert!(intelephense.handles_language("php"));
        assert!(phpactor.handles_language("php"));
        assert!(!phpactor.handles_language("blade"));
        assert_ne!(intelephense.command, phpactor.command);
    }

    #[test]
    fn initialization_options_are_forwarded_unread() {
        let config = ServerConfig::new("any", "any", "/tmp")
            .with_initialization_options(serde_json::json!({"anything": [1, 2, 3]}));
        // The crate stores the blob and never inspects its shape.
        assert_eq!(config.initialization_options.unwrap()["anything"][2], 3);
    }

    #[test]
    fn env_and_args_are_configurable() {
        let config = ServerConfig::new("n", "c", "/tmp")
            .with_args(["--stdio", "--verbose"])
            .with_env("PHP_MEMORY", "512M");
        assert_eq!(config.args, ["--stdio", "--verbose"]);
        assert_eq!(config.env["PHP_MEMORY"], "512M");
    }
}
