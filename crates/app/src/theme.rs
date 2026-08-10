//! Colours and metrics.
//!
//! One struct of values, not a theme engine. Loading themes from disk is deferred until
//! there are enough real themes to tell us what the file format needs (#48); until then
//! the variants are compiled in, which costs no parser, no error path and no hot-reload.
//!
//! # Where the theme lives
//!
//! The active theme is a gpui [`Global`], reachable as `cx.theme()` through the [`Themed`]
//! extension trait. It used to be a `Theme::dark()` call in each `render`, which meant a
//! second theme would have reached exactly the views someone remembered to update.
//!
//! A global rather than a field threaded down the view tree, because there is nothing to
//! thread it through: `EditorView`, `TerminalView` and `Palette` are sibling `Entity`s with
//! their own `Render` impls, and `Render::render` takes no arguments beyond the context. A
//! field per view would mean the workspace pushing the new theme into each child on every
//! switch, and a view added later that nobody remembers to push to is silently the old
//! theme — the bug this change exists to remove, reintroduced with more code.
//!
//! `Theme` is not itself the global. The global is a private newtype, so `cx.theme()` is
//! the only way to obtain one and `cx.set_global(some_theme)` does not compile. That is
//! what makes "no view constructs its own theme" enforceable rather than a convention, and
//! it is the pattern gpui's own `Global` docs recommend for exactly this.

use elle_syntax::HighlightStyle;
use elle_terminal::CellColor;
use gpui::{App, Global, Hsla, Pixels, Rgba, px, rgb};

#[derive(Clone)]
pub struct Theme {
    pub background: Hsla,
    pub panel: Hsla,
    pub border: Hsla,
    pub text: Hsla,
    pub text_muted: Hsla,
    pub accent: Hsla,
    pub hover: Hsla,
    pub selected: Hsla,
    pub cursor: Hsla,
    pub selection: Hsla,
    pub status_bar: Hsla,

    pub keyword: Hsla,
    pub type_name: Hsla,
    pub function: Hsla,
    pub variable: Hsla,
    pub string: Hsla,
    pub number: Hsla,
    pub comment: Hsla,
    pub tag: Hsla,
    pub blade: Hsla,

    /// The sixteen ANSI slots the terminal renders: 0-7 normal, 8-15 bright.
    ///
    /// An array rather than sixteen named fields because the terminal indexes it
    /// numerically — `Ansi(9)` comes straight off the wire — and naming each one would
    /// mean a sixteen-arm match that does nothing but arithmetic.
    pub ansi: [Hsla; 16],
}

/// Editor metrics. Line height is derived from font size so zoom stays consistent.
pub struct Metrics;

impl Metrics {
    pub const FONT_SIZE: Pixels = px(13.0);
    pub const UI_FONT_SIZE: Pixels = px(12.0);
    pub const LINE_HEIGHT: Pixels = px(20.0);
    pub const ACTIVITY_BAR_WIDTH: Pixels = px(44.0);
    pub const SIDEBAR_WIDTH: Pixels = px(240.0);
    pub const GUTTER_WIDTH: Pixels = px(52.0);
    pub const TAB_HEIGHT: Pixels = px(32.0);
    pub const STATUS_HEIGHT: Pixels = px(24.0);
    pub const ROW_HEIGHT: Pixels = px(22.0);

    /// Height of the terminal panel, including its tab strip.
    /// ponytail: fixed, not draggable. A splitter needs a drag-handle element and a
    /// persisted layout; both arrive with the settings crate.
    pub const TERMINAL_HEIGHT: Pixels = px(260.0);
    /// Line height inside the terminal grid. Tighter than the editor's, which is what
    /// makes a terminal look like a terminal rather than a document.
    pub const TERMINAL_LINE_HEIGHT: Pixels = px(16.0);
}

/// Which compiled-in theme is active.
///
/// An enum rather than a name string: the palette command toggles between variants, and a
/// match on this cannot reach a theme that does not exist. The classic themes of #48 add
/// arms here; nothing else in the crate learns their names.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum ThemeVariant {
    #[default]
    Dark,
    Light,
}

impl ThemeVariant {
    pub fn build(self) -> Theme {
        match self {
            Self::Dark => Theme::dark(),
            Self::Light => Theme::light(),
        }
    }

    /// The next variant in the cycle, for `theme.toggle`.
    ///
    /// With two themes this is a swap. It is written as a cycle because that is what it
    /// becomes when the classics land, and because a `match` here still cannot miss one.
    pub fn next(self) -> Self {
        match self {
            Self::Dark => Self::Light,
            Self::Light => Self::Dark,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Dark => "Dark",
            Self::Light => "Light",
        }
    }
}

/// The active theme, as gpui stores it.
///
/// Private, and holding rather than being a `Theme`, so the only way into the global is
/// [`set_theme`] and the only way out is [`Themed::theme`]. A view cannot set it and cannot
/// construct one to set.
struct ActiveTheme {
    variant: ThemeVariant,
    theme: Theme,
}

impl Global for ActiveTheme {}

/// Reads the active theme off the context.
///
/// Implemented on `App`, which `Context<V>` derefs to, so `cx.theme()` works unchanged
/// inside every `Render::render`.
pub trait Themed {
    fn theme(&self) -> &Theme;
    fn theme_variant(&self) -> ThemeVariant;
}

impl Themed for App {
    /// Panics if [`set_theme`] has not run. That is deliberate: a window opened without a
    /// theme has no colours at all, so failing at startup beats painting invisible text.
    fn theme(&self) -> &Theme {
        &self.global::<ActiveTheme>().theme
    }

    fn theme_variant(&self) -> ThemeVariant {
        self.global::<ActiveTheme>().variant
    }
}

/// Installs a theme. Called once at startup and again on every switch.
///
/// `set_global` notifies global observers, but observing is not what repaints — gpui
/// repaints an entity when *it* is notified. The caller does the refresh; see
/// `WorkspaceView::toggle_theme`.
pub fn set_theme(variant: ThemeVariant, cx: &mut App) {
    cx.set_global(ActiveTheme { variant, theme: variant.build() });
}

impl Theme {
    /// The default dark theme.
    pub fn dark() -> Self {
        Self {
            background: rgb(0x16171d).into(),
            panel: rgb(0x1b1d24).into(),
            border: rgb(0x2a2d36).into(),
            text: rgb(0xd7dae0).into(),
            text_muted: rgb(0x767c8a).into(),
            accent: rgb(0xff5c8a).into(),
            hover: rgb(0x24272f).into(),
            selected: rgb(0x2d313c).into(),
            cursor: rgb(0xff5c8a).into(),
            selection: rgb(0x33405c).into(),
            status_bar: rgb(0x1b1d24).into(),

            keyword: rgb(0xc77dff).into(),
            type_name: rgb(0x7dd3fc).into(),
            function: rgb(0x82aaff).into(),
            variable: rgb(0xf0a6c8).into(),
            string: rgb(0xa3d977).into(),
            number: rgb(0xffb86c).into(),
            comment: rgb(0x5c6370).into(),
            tag: rgb(0x8b93a5).into(),
            blade: rgb(0xff9e64).into(),

            // Tuned to this theme rather than the hardware VT100 palette: a literal
            // 0x0000ff blue is unreadable on a dark background, and `ls` uses it for
            // directories on every Laravel project.
            ansi: [
                rgb(0x3b4048).into(), // 0 black — lifted off the background so it is visible
                rgb(0xff5c8a).into(), // 1 red
                rgb(0xa3d977).into(), // 2 green
                rgb(0xffb86c).into(), // 3 yellow
                rgb(0x82aaff).into(), // 4 blue
                rgb(0xc77dff).into(), // 5 magenta
                rgb(0x7dd3fc).into(), // 6 cyan
                rgb(0xd7dae0).into(), // 7 white
                rgb(0x5c6370).into(), // 8 bright black
                rgb(0xff8fab).into(), // 9 bright red
                rgb(0xbdea9a).into(), // 10 bright green
                rgb(0xffd29b).into(), // 11 bright yellow
                rgb(0xa8c4ff).into(), // 12 bright blue
                rgb(0xdcaaff).into(), // 13 bright magenta
                rgb(0xa9e4fd).into(), // 14 bright cyan
                rgb(0xffffff).into(), // 15 bright white
            ],
        }
    }

    /// A light theme.
    ///
    /// Not one of the classics of #48 and not trying to be: this exists so that every
    /// surface differs from `dark()` — panels, borders, text, syntax, and the ANSI table —
    /// and a view that still constructs its own theme is a dark rectangle in a light
    /// window. Polished light themes come with the classics.
    pub fn light() -> Self {
        Self {
            background: rgb(0xfbfbfd).into(),
            panel: rgb(0xf1f2f6).into(),
            border: rgb(0xd8dae2).into(),
            text: rgb(0x24262e).into(),
            text_muted: rgb(0x6b7180).into(),
            accent: rgb(0xd6336c).into(),
            hover: rgb(0xe6e8ef).into(),
            selected: rgb(0xdcdfe9).into(),
            cursor: rgb(0xd6336c).into(),
            selection: rgb(0xc7d7f5).into(),
            status_bar: rgb(0xf1f2f6).into(),

            keyword: rgb(0x8125c9).into(),
            type_name: rgb(0x0b6e8f).into(),
            function: rgb(0x2b52c4).into(),
            variable: rgb(0xa33566).into(),
            string: rgb(0x3f7d20).into(),
            number: rgb(0xa2591a).into(),
            comment: rgb(0x8a8f9c).into(),
            tag: rgb(0x5c6270).into(),
            blade: rgb(0xb54708).into(),

            // The dark theme lifts slot 0 off the background and brightens blue, because
            // `0x0000ff` on `0x16171d` is unreadable. Neither fix applies here and copying
            // them would be actively wrong: slot 0 is *black*, which is the readable colour
            // on a light background, and a lightened blue is the one that disappears. The
            // whole table is darkened instead, so contrast runs the other way — same
            // question, opposite answer. The bright slots (8-15) are still lighter than
            // their normal counterparts because programs use them for emphasis, but they
            // stop well short of white, which would vanish entirely.
            ansi: [
                rgb(0x2b2d34).into(), // 0 black — the real thing; it reads on white
                rgb(0xc02a3f).into(), // 1 red
                rgb(0x3f7d20).into(), // 2 green
                rgb(0x9a6400).into(), // 3 yellow — darkened; pure yellow is invisible here
                rgb(0x2b52c4).into(), // 4 blue — darkened, not brightened
                rgb(0x8125c9).into(), // 5 magenta
                rgb(0x0b6e8f).into(), // 6 cyan
                rgb(0x6b7180).into(), // 7 white — a mid grey, since white-on-white is not a colour
                rgb(0x585d6a).into(), // 8 bright black
                rgb(0xd6336c).into(), // 9 bright red
                rgb(0x4f9a2a).into(), // 10 bright green
                rgb(0xb5811a).into(), // 11 bright yellow
                rgb(0x3a6ae0).into(), // 12 bright blue
                rgb(0x9a3ade).into(), // 13 bright magenta
                rgb(0x1188ad).into(), // 14 bright cyan
                rgb(0x24262e).into(), // 15 bright white — the darkest, mirroring dark's brightest
            ],
        }
    }

    /// Colour for a terminal cell.
    ///
    /// The terminal crate resolves the 256-colour cube and palette overrides itself and
    /// hands back either a literal RGB or a symbolic slot; only the symbolic ones reach
    /// the theme, which is what lets a theme restyle `ls` output without the parser
    /// knowing about themes.
    pub fn terminal(&self, color: CellColor) -> Hsla {
        match color {
            CellColor::Foreground => self.text,
            CellColor::Background => self.background,
            // Defensive index: the slot comes off the wire as a u8. A malformed SGR must
            // not panic the render (§24), so an out-of-range slot falls back to text.
            CellColor::Ansi(slot) => *self.ansi.get(slot as usize).unwrap_or(&self.text),
            CellColor::Rgb(r, g, b) => {
                Rgba { r: r as f32 / 255.0, g: g as f32 / 255.0, b: b as f32 / 255.0, a: 1.0 }
                    .into()
            }
        }
    }

    /// Colour for a syntax style. Highlighting never names a colour itself, so a theme
    /// change needs no reparse.
    pub fn syntax(&self, style: HighlightStyle) -> Hsla {
        match style {
            HighlightStyle::Keyword => self.keyword,
            HighlightStyle::Type => self.type_name,
            HighlightStyle::Function => self.function,
            HighlightStyle::Variable => self.variable,
            HighlightStyle::String => self.string,
            HighlightStyle::Number => self.number,
            HighlightStyle::Comment => self.comment,
            HighlightStyle::Tag => self.tag,
            HighlightStyle::BladeDirective => self.blade,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every `HighlightStyle`, listed once.
    ///
    /// `elle_syntax` exposes no `ALL`, and ADR-0004 keeps the theme out of that crate, so
    /// the list lives here — where it has to be kept honest by something other than care.
    /// [`style_name`] is that something: an exhaustive match, so a tenth `HighlightStyle`
    /// is a compile error pointing at the function next to the list that needs updating.
    ///
    /// `Theme::syntax` gets the same guarantee for free, being an exhaustive match over
    /// named struct fields — both halves checked by the compiler. That is why there is no
    /// test asserting it handles every style: it cannot not. What needs testing is
    /// everything *downstream* of it, which is what this list is for.
    const ALL_STYLES: [HighlightStyle; 9] = [
        HighlightStyle::Keyword,
        HighlightStyle::Type,
        HighlightStyle::Function,
        HighlightStyle::Variable,
        HighlightStyle::String,
        HighlightStyle::Number,
        HighlightStyle::Comment,
        HighlightStyle::Tag,
        HighlightStyle::BladeDirective,
    ];

    /// Names a style for a failure message, and by being exhaustive keeps [`ALL_STYLES`]
    /// complete. `Debug` would print the name too, but `Debug` cannot fail to compile
    /// when a variant is added, which is the whole job.
    fn style_name(style: HighlightStyle) -> &'static str {
        match style {
            HighlightStyle::Keyword => "keyword",
            HighlightStyle::Type => "type",
            HighlightStyle::Function => "function",
            HighlightStyle::Variable => "variable",
            HighlightStyle::String => "string",
            HighlightStyle::Number => "number",
            HighlightStyle::Comment => "comment",
            HighlightStyle::Tag => "tag",
            HighlightStyle::BladeDirective => "blade directive",
        }
    }

    /// Every variant, kept complete by `ThemeVariant::label` for the same reason.
    const ALL_VARIANTS: [ThemeVariant; 2] = [ThemeVariant::Dark, ThemeVariant::Light];

    #[test]
    fn the_lists_cover_every_style_and_every_variant() {
        // The counts are the half the compiler cannot check: an exhaustive match keeps
        // `style_name` and `label` honest, but nothing stops someone adding a variant to
        // both and forgetting the array. Asserting the length here fails loudly instead of
        // silently testing eight styles out of ten.
        assert_eq!(ALL_STYLES.len(), 9, "a new HighlightStyle needs a colour in every theme");
        assert_eq!(ALL_VARIANTS.len(), 2, "a new theme needs listing, or it goes untested");

        // Also a guard that no two entries are duplicates, which would quietly shrink
        // coverage while the length still looked right.
        for (i, a) in ALL_STYLES.iter().enumerate() {
            assert!(!ALL_STYLES[i + 1..].contains(a), "{} is listed twice", style_name(*a));
        }
        for (i, a) in ALL_VARIANTS.iter().enumerate() {
            assert!(!ALL_VARIANTS[i + 1..].contains(a), "{} is listed twice", a.label());
        }
    }

    #[test]
    fn every_theme_gives_every_syntax_style_a_distinct_colour() {
        // `Theme::syntax` cannot miss a style — it is an exhaustive match over named
        // fields, both of which the compiler checks. What it *can* do is return the same
        // colour twice, which is what happens when a new theme is written by copying
        // another and one field is left pointing at its neighbour. Highlighting then
        // silently stops distinguishing, say, strings from comments.
        //
        // Distinctness is a real constraint, not a stylistic one: two styles the theme
        // paints identically are two styles the parser worked to tell apart for nothing.
        for variant in ALL_VARIANTS {
            let theme = variant.build();
            for (i, a) in ALL_STYLES.iter().enumerate() {
                for b in &ALL_STYLES[i + 1..] {
                    assert_ne!(
                        theme.syntax(*a),
                        theme.syntax(*b),
                        "{}: {} and {} are the same colour",
                        variant.label(),
                        style_name(*a),
                        style_name(*b)
                    );
                }
            }
        }
    }

    #[test]
    fn no_theme_paints_text_the_same_colour_as_its_background() {
        // The cheapest possible readability check, and the only one a machine can make
        // honestly: identical foreground and background is invisible text, which is a bug
        // in any theme. Whether the *contrast* is comfortable is issue #35 and needs eyes.
        for variant in ALL_VARIANTS {
            let theme = variant.build();
            assert_ne!(theme.text, theme.background, "{}: invisible body text", variant.label());
            assert_ne!(theme.text, theme.panel, "{}: invisible text on panels", variant.label());
            assert_ne!(theme.cursor, theme.background, "{}: invisible cursor", variant.label());

            for (slot, colour) in theme.ansi.iter().enumerate() {
                assert_ne!(
                    *colour,
                    theme.background,
                    "{}: ANSI slot {slot} is invisible in the terminal",
                    variant.label()
                );
            }
        }
    }

    #[test]
    fn the_light_theme_is_not_the_dark_one_with_a_new_name() {
        // The acceptance test for #48's plumbing, as far as a machine can state it: every
        // surface a view paints has to actually differ, or a call site still building its
        // own `Theme::dark()` would look correct under both.
        let dark = Theme::dark();
        let light = Theme::light();

        assert_ne!(dark.background, light.background);
        assert_ne!(dark.panel, light.panel);
        assert_ne!(dark.border, light.border);
        assert_ne!(dark.text, light.text);
        assert_ne!(dark.status_bar, light.status_bar);

        for style in ALL_STYLES {
            assert_ne!(
                dark.syntax(style),
                light.syntax(style),
                "{} is unchanged between themes",
                style_name(style)
            );
        }

        for slot in 0..16u8 {
            assert_ne!(
                dark.terminal(CellColor::Ansi(slot)),
                light.terminal(CellColor::Ansi(slot)),
                "ANSI slot {slot} is unchanged"
            );
        }
    }

    #[test]
    fn cycling_variants_returns_to_where_it_started() {
        // `theme.toggle` walks `next()`; a cycle that never comes back would strand the
        // user on whichever theme it dead-ends at.
        let mut variant = ThemeVariant::default();
        for _ in 0..ALL_VARIANTS.len() {
            variant = variant.next();
        }
        assert_eq!(variant, ThemeVariant::default());
    }

    #[test]
    fn an_out_of_range_ansi_slot_falls_back_in_every_theme() {
        // §24: a malformed SGR must not panic the render, and that is a per-theme promise
        // because each theme owns its own table.
        for variant in ALL_VARIANTS {
            let theme = variant.build();
            assert_eq!(theme.terminal(CellColor::Ansi(200)), theme.text);
        }
    }
}
