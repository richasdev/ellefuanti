//! What composer.json declares that the palette can use (#26).
//!
//! One question for now: the project's scripts, so "Composer: Run Script…" can list
//! them. Same contract as every reader here — the file's own words, nothing invented,
//! and a malformed composer.json yields an empty list rather than an error dialog
//! (a project mid-edit has a malformed composer.json several times a day).

/// The script names `composer run-script` would accept, alphabetical.
///
/// Alphabetical by an explicit sort, because `serde_json`'s map order is a FEATURE
/// FLAG (`preserve_order`) subject to cargo's feature unification — this crate alone
/// got sorted keys while the app binary, through some other dependency, got insertion
/// order, and the same function returned different orders in different builds. An
/// explicit sort is the only order that survives the workspace.
///
/// Values are commands or arrays of commands; only the *names* matter to a palette that
/// types `composer run-script <name> ` into the terminal — the definition is composer's
/// to execute, visibly, when the user presses Enter.
pub fn composer_scripts(composer_json: &str) -> Vec<String> {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(composer_json) else {
        return Vec::new();
    };
    let Some(scripts) = value.get("scripts").and_then(|scripts| scripts.as_object()) else {
        return Vec::new();
    };
    let mut names: Vec<String> = scripts.keys().cloned().collect();
    names.sort();
    names
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scripts_come_back_by_name_alphabetical() {
        let json = r#"{
            "name": "app",
            "scripts": {
                "test": "pest",
                "lint": ["pint", "phpstan"],
                "post-install-cmd": "@php artisan optimize"
            }
        }"#;
        assert_eq!(composer_scripts(json), ["lint", "post-install-cmd", "test"]);
    }

    #[test]
    fn malformed_or_scriptless_json_is_an_empty_list_not_an_error() {
        assert!(composer_scripts("{ not json").is_empty());
        assert!(composer_scripts(r#"{"name": "app"}"#).is_empty());
        assert!(composer_scripts(r#"{"scripts": "not-an-object"}"#).is_empty());
    }
}
