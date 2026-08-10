//! A colour, as a theme file spells it.
//!
//! Plain `u32` RGB rather than gpui's `Rgba` or `Hsla`, because this crate may not depend on
//! gpui (ADR-0004) and the app converts at the boundary with `rgb(..)` — the same call the
//! compiled-in themes already make.

use std::fmt;

/// `#rrggbb` as `0xrrggbb`. Alpha is parsed and discarded; see [`parse`].
pub type Rgb = u32;

/// What was wrong with a colour string, in terms that name the offending text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ColorError {
    pub found: String,
    pub reason: &'static str,
}

impl fmt::Display for ColorError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:?} is not a colour: {}", self.found, self.reason)
    }
}

impl std::error::Error for ColorError {}

/// Parses `#rgb`, `#rrggbb`, `#rgba` or `#rrggbbaa` into `0xrrggbb`.
///
/// **Alpha is dropped, not blended.** VS Code themes use it for overlays —
/// `list.hoverBackground` is `#6e76811a` in GitHub Dark, a 10% white wash over whatever is
/// behind it. This editor's `Theme` holds opaque colours only, and the honest thing to do
/// with an alpha channel there is to ignore it and let the importer's own derived UI
/// colours cover those surfaces, rather than compositing against a background this function
/// cannot see and producing a colour the theme never specified.
///
/// The short forms are in the CSS spec that VS Code's colour strings follow, and cost three
/// lines to support.
pub fn parse(text: &str) -> Result<Rgb, ColorError> {
    let error = |reason| ColorError { found: text.to_string(), reason };

    let digits = text.strip_prefix('#').ok_or_else(|| error("expected a leading '#'"))?;
    if !digits.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err(error("expected hexadecimal digits after '#'"));
    }

    // Doubling each nibble is what makes `#abc` mean `#aabbcc` rather than `#0a0b0c`.
    let expanded: String = match digits.len() {
        3 | 4 => digits.chars().flat_map(|c| [c, c]).collect(),
        6 | 8 => digits.to_string(),
        _ => return Err(error("expected 3, 4, 6 or 8 hex digits")),
    };

    u32::from_str_radix(&expanded[..6], 16).map_err(|_| error("not a hexadecimal number"))
}

/// Formats `0xrrggbb` back as `#rrggbb`, for writing a native theme file.
pub fn format(color: Rgb) -> String {
    format!("#{:06x}", color & 0xff_ffff)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_six_digit_form_is_the_common_case() {
        assert_eq!(parse("#d19a66"), Ok(0xd19a66));
        assert_eq!(parse("#000000"), Ok(0x000000));
        assert_eq!(parse("#FFFFFF"), Ok(0xffffff));
    }

    #[test]
    fn the_short_form_doubles_each_digit() {
        assert_eq!(parse("#abc"), Ok(0xaabbcc));
        assert_eq!(parse("#fff"), Ok(0xffffff));
    }

    /// The real case this exists for: GitHub Dark's `list.hoverBackground` is `#6e76811a`.
    #[test]
    fn alpha_is_dropped_rather_than_blended_against_a_background_we_cannot_see() {
        assert_eq!(parse("#6e76811a"), Ok(0x6e7681));
        assert_eq!(parse("#abcd"), Ok(0xaabbcc), "the short form too");
    }

    #[test]
    fn a_bad_colour_names_the_text_that_was_wrong() {
        let error = parse("d19a66").expect_err("no leading hash");
        assert!(error.to_string().contains("d19a66"), "{error}");
        assert!(error.to_string().contains('#'), "{error}");

        assert!(parse("#gggggg").is_err(), "not hexadecimal");
        assert!(parse("#12345").is_err(), "five digits is not a form");
        assert!(parse("").is_err());
        assert!(parse("#").is_err());
    }

    #[test]
    fn formatting_round_trips_through_parsing() {
        for color in [0x000000, 0xd19a66, 0xffffff, 0x0d1117] {
            assert_eq!(parse(&format(color)), Ok(color));
        }
    }
}
