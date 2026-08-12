//! Self-update: the decisions, kept pure so they are testable headless.
//!
//! The moving parts — curl, hdiutil, xattr, the swap under `/Applications` — live in
//! `workspace_view.rs` as background shell steps. What lives here is everything that can
//! be wrong quietly: which release is newer, which asset is the dmg, what the status-bar
//! cell should say. Zero dependencies beyond the already-present `serde_json`.

/// Where updates come from. The Cargo.toml `repository` field names a different owner
/// and is stale; the remote this repo actually pushes to is the source of truth.
pub const RELEASES_API: &str = "https://api.github.com/repos/richasdev/ellefuanti/releases/latest";

/// A parsed `x.y.z`, ordered field by field — which is exactly what derived `Ord` does
/// for a tuple struct, and why this is one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Version(pub u32, pub u32, pub u32);

impl Version {
    /// Accepts `0.3.0` and `v0.3.0`; anything else is `None` rather than a guess.
    /// Pre-release suffixes (`0.3.0-rc1`) are refused on purpose — the releases this
    /// reads are this project's own tags, which never carry one, and treating `-rc1`
    /// as equal to the release it precedes would offer a downgrade as an update.
    pub fn parse(text: &str) -> Option<Version> {
        let text = text.strip_prefix('v').unwrap_or(text);
        let mut parts = text.split('.');
        let major = parts.next()?.parse().ok()?;
        let minor = parts.next()?.parse().ok()?;
        let patch = parts.next()?.parse().ok()?;
        if parts.next().is_some() {
            return None;
        }
        Some(Version(major, minor, patch))
    }
}

impl std::fmt::Display for Version {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}.{}.{}", self.0, self.1, self.2)
    }
}

/// A newer release, as much of it as the UI needs. `dmg_url` is `Option` because a
/// release without a dmg asset is still announceable — the click just opens the page
/// instead of installing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Available {
    pub version: Version,
    pub dmg_url: Option<String>,
    pub html_url: String,
}

/// Reads GitHub's `releases/latest` response. `None` for anything that does not parse —
/// a rate-limit error body, a repo with no releases — because "no update" is the only
/// safe reading of a response we do not understand.
pub fn parse_latest_release(json: &str) -> Option<Available> {
    let value: serde_json::Value = serde_json::from_str(json).ok()?;
    let version = Version::parse(value.get("tag_name")?.as_str()?)?;
    let html_url = value.get("html_url")?.as_str()?.to_string();
    let dmg_url = value
        .get("assets")
        .and_then(|assets| assets.as_array())
        .and_then(|assets| {
            assets.iter().find_map(|asset| {
                let name = asset.get("name")?.as_str()?;
                if name.ends_with("-macos.dmg") {
                    Some(asset.get("browser_download_url")?.as_str()?.to_string())
                } else {
                    None
                }
            })
        });
    Some(Available { version, dmg_url, html_url })
}

/// Whether `available` is strictly newer than the running build. An unparseable
/// `current` answers `false`: a dev build with a mangled version must not be nagged.
pub fn newer_than_current(available: &Available, current: &str) -> bool {
    match Version::parse(current) {
        Some(current) => available.version > current,
        None => false,
    }
}

/// The update lifecycle, one cell's worth of state.
///
/// `Available` is also the failure fallback: an install that died leaves the offer on
/// screen rather than a dead `Downloading` the user cannot retry.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum UpdateState {
    #[default]
    Idle,
    Available(Available),
    Downloading,
    ReadyToRestart,
}

impl UpdateState {
    /// What the status-bar cell says; `None` renders no cell at all — chrome only
    /// exists while there is something to do (#71's dead-target rule).
    pub fn status_label(&self) -> Option<String> {
        match self {
            UpdateState::Idle => None,
            UpdateState::Available(available) => Some(format!("Update v{} ↓", available.version)),
            UpdateState::Downloading => Some("Updating…".to_string()),
            UpdateState::ReadyToRestart => Some("Restart to update".to_string()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_version_parses_with_or_without_the_v() {
        assert_eq!(Version::parse("0.3.0"), Some(Version(0, 3, 0)));
        assert_eq!(Version::parse("v1.12.5"), Some(Version(1, 12, 5)));
    }

    #[test]
    fn garbage_is_not_a_version() {
        for bad in ["", "0.3", "0.3.0.1", "0.3.x", "v0.3.0-rc1", "latest"] {
            assert_eq!(Version::parse(bad), None, "{bad:?} must not parse");
        }
    }

    #[test]
    fn versions_order_field_by_field() {
        assert!(Version(0, 10, 0) > Version(0, 9, 9), "numeric, not lexicographic");
        assert!(Version(1, 0, 0) > Version(0, 99, 99));
        assert!(Version(0, 2, 1) > Version(0, 2, 0));
    }

    const RELEASE: &str = r#"{
        "tag_name": "v0.3.0",
        "html_url": "https://github.com/richasdev/ellefuanti/releases/tag/v0.3.0",
        "assets": [
            {"name": "ellefuanti-0.3.0-macos-arm64.zip",
             "browser_download_url": "https://example.com/a.zip"},
            {"name": "ellefuanti-v0.3.0-macos.dmg",
             "browser_download_url": "https://example.com/a.dmg"}
        ]
    }"#;

    #[test]
    fn the_dmg_asset_is_found_among_others() {
        let release = parse_latest_release(RELEASE).unwrap();
        assert_eq!(release.version, Version(0, 3, 0));
        assert_eq!(release.dmg_url.as_deref(), Some("https://example.com/a.dmg"));
    }

    #[test]
    fn a_release_without_a_dmg_still_announces_via_its_page() {
        let json = r#"{"tag_name": "v0.3.0", "html_url": "https://example.com/rel", "assets": []}"#;
        let release = parse_latest_release(json).unwrap();
        assert_eq!(release.dmg_url, None);
        assert_eq!(release.html_url, "https://example.com/rel");
    }

    #[test]
    fn an_error_body_is_no_update_rather_than_a_panic() {
        assert_eq!(parse_latest_release(r#"{"message": "API rate limit exceeded"}"#), None);
        assert_eq!(parse_latest_release("not json"), None);
    }

    #[test]
    fn newer_means_strictly_newer() {
        let release = parse_latest_release(RELEASE).unwrap();
        assert!(newer_than_current(&release, "0.2.0"));
        assert!(!newer_than_current(&release, "0.3.0"), "equal is not an update");
        assert!(!newer_than_current(&release, "0.4.0"));
        assert!(!newer_than_current(&release, "not-a-version"), "dev builds are not nagged");
    }

    #[test]
    fn each_state_labels_its_cell() {
        assert_eq!(UpdateState::Idle.status_label(), None);
        let release = parse_latest_release(RELEASE).unwrap();
        assert_eq!(
            UpdateState::Available(release).status_label().as_deref(),
            Some("Update v0.3.0 ↓")
        );
        assert_eq!(UpdateState::Downloading.status_label().as_deref(), Some("Updating…"));
        assert_eq!(
            UpdateState::ReadyToRestart.status_label().as_deref(),
            Some("Restart to update")
        );
    }
}
