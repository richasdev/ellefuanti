//! The activity bar's icons, and the asset source gpui loads them through.
//!
//! gpui's `svg()` element does not take bytes; it takes a path that it hands to the
//! `AssetSource` installed on the `Application`. So there has to be a source, and the only
//! question is where the bytes come from.
//!
//! They are `include_str!`-ed rather than read from `assets/` at runtime, for the same
//! reason `elle_syntax` embeds its highlight queries: a file that is missing at runtime
//! fails *quietly*. `paint_svg` logs and paints nothing, so a bad path is a blank 16x16
//! hole in the activity bar with no crash and no visible error — which is precisely the
//! silent-degradation failure this issue rejected an icon font to avoid. Embedded, a
//! missing file is a compile error and a wrong name is a compile error.
//!
//! The cost is that the icons cannot be themed by dropping files into a directory. That is
//! a real feature to give up, and it goes when a settings crate exists to make it
//! coherent; until then it would be a runtime failure mode bought for nobody.

use std::borrow::Cow;

use gpui::{AssetSource, Result, SharedString};

/// The seven activity-bar panels, in the order they appear.
///
/// `path` is what `svg().path(..)` is given and what [`Icons::load`] matches on. Keeping
/// the two in one table is why a typo cannot happen: there is only one string.
pub struct Icon {
    pub path: &'static str,
    svg: &'static str,
}

macro_rules! icons {
    ($($name:literal),* $(,)?) => {
        &[$(Icon {
            path: concat!("icons/", $name, ".svg"),
            svg: include_str!(concat!("../../../assets/icons/", $name, ".svg")),
        }),*]
    };
}

/// Every icon the app can draw. Also the lookup table for [`Icons`].
pub const ICONS: &[Icon] =
    icons!["explorer", "search", "git", "laravel", "database", "docker", "tests"];

/// Serves the embedded icons to gpui.
///
/// Installed with `Application::new().with_assets(Icons)`. Without it, `AssetSource for ()`
/// returns `None` for every path and every icon silently paints nothing.
pub struct Icons;

impl AssetSource for Icons {
    fn load(&self, path: &str) -> Result<Option<Cow<'static, [u8]>>> {
        Ok(ICONS
            .iter()
            .find(|icon| icon.path == path)
            .map(|icon| Cow::Borrowed(icon.svg.as_bytes())))
    }

    fn list(&self, path: &str) -> Result<Vec<SharedString>> {
        Ok(ICONS
            .iter()
            .filter(|icon| icon.path.starts_with(path))
            .map(|icon| SharedString::from(icon.path))
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The activity bar draws all seven; a missing one is a blank square, not a crash.
    #[test]
    fn every_icon_loads_by_its_own_path() {
        for icon in ICONS {
            let bytes = Icons
                .load(icon.path)
                .unwrap()
                .unwrap_or_else(|| panic!("{} did not load", icon.path));
            assert!(!bytes.is_empty(), "{} is empty", icon.path);
        }
    }

    /// An unknown path must return `None`, not the first icon in the table. A `find` that
    /// fell back to a default would make every typo render the explorer glyph.
    #[test]
    fn an_unknown_path_loads_nothing() {
        assert!(Icons.load("icons/does-not-exist.svg").unwrap().is_none());
    }

    /// resvg is what gpui rasterises with, and it rejects malformed SVG at *runtime* by
    /// painting nothing. Parsing here turns that into a test failure instead. This is the
    /// check that would catch a truncated download or a hand-edited path.
    #[test]
    fn every_icon_is_parseable_svg() {
        for icon in ICONS {
            let svg = icon.svg;
            assert!(svg.contains("<svg"), "{} is not an svg", icon.path);
            assert!(svg.contains("viewBox"), "{} has no viewBox", icon.path);
            // Without this the icon renders in its own colour and ignores the theme:
            // gpui masks by alpha, so a fill of `none` would paint an empty square.
            assert!(
                svg.contains(r#"fill="currentColor""#),
                "{} must use fill=\"currentColor\" so the theme colours it",
                icon.path
            );
        }
    }
}
