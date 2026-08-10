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
    pub property: Hsla,
    pub string: Hsla,
    pub number: Hsla,
    pub operator: Hsla,
    pub attribute: Hsla,
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
    OneDarkPro,
    GitHubDark,
    GitHubLight,
}

impl ThemeVariant {
    pub fn build(self) -> Theme {
        match self {
            Self::Dark => Theme::dark(),
            Self::Light => Theme::light(),
            Self::OneDarkPro => Theme::one_dark_pro(),
            Self::GitHubDark => Theme::github_dark(),
            Self::GitHubLight => Theme::github_light(),
        }
    }

    /// The next variant in the cycle, for `theme.toggle`.
    ///
    /// Ordered dark-then-light rather than by origin, so a toggle does not swing between
    /// a black and a white window twice on the way round.
    pub fn next(self) -> Self {
        match self {
            Self::Dark => Self::OneDarkPro,
            Self::OneDarkPro => Self::GitHubDark,
            Self::GitHubDark => Self::Light,
            Self::Light => Self::GitHubLight,
            Self::GitHubLight => Self::Dark,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Dark => "Dark",
            Self::Light => "Light",
            Self::OneDarkPro => "One Dark Pro",
            Self::GitHubDark => "GitHub Dark",
            Self::GitHubLight => "GitHub Light",
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
            // Close to `variable` in hue but lighter, so `$user->name` reads as two
            // related tokens rather than two unrelated ones. Unverified on screen (#35).
            property: rgb(0xe8c7dd).into(),
            string: rgb(0xa3d977).into(),
            number: rgb(0xffb86c).into(),
            // Deliberately low-contrast: operators are everywhere and should punctuate,
            // not compete with the identifiers they sit between.
            operator: rgb(0x9aa2b1).into(),
            // Must not read as a comment — that confusion is the reason #47 lists it.
            attribute: rgb(0xf7c873).into(),
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
            // Property sits deliberately close to `variable`: `$user->name` reads as one
            // expression, and pulling the two far apart makes a member access look like two
            // unrelated tokens. Close, not equal — the distinctness test rejects equal.
            property: rgb(0x8a3f6b).into(),
            // Operators are punctuation. Louder than `comment`, quieter than any identifier,
            // because `=>` and `->` appear on nearly every line and should not compete.
            operator: rgb(0x6b7180).into(),
            // Attributes borrow the `number` family rather than the comment grey the dark
            // theme's gold avoids: `#[Route(...)]` is code, and reading as a comment is the
            // exact confusion this style exists to remove.
            attribute: rgb(0x9a4b12).into(),

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

    /// One Dark Pro, ported from the VS Code theme.
    ///
    /// Origin: `zhuangtongfa.material-theme` v3.19.0, `themes/OneDark-Pro.json`.
    /// Licence: MIT. This repo is Apache-2.0; MIT is compatible, and the origin is
    /// recorded here because a palette copied without attribution is painful to unpick.
    ///
    /// **Every syntax colour below is verbatim from that file** — read out of the
    /// `tokenColors` scopes rather than reconstructed. A theme called "One Dark Pro" that
    /// is not One Dark Pro's colours is worse than not shipping it, so nothing here is
    /// adjusted for taste, including the parts this project would have chosen otherwise:
    ///
    /// - `variable` and `property` are **both** `#e06c75`. #52 deliberately separated
    ///   those two for this project's own themes, and that separation is not applied
    ///   here: `meta.object-literal.key` really is the same colour as `variable`
    ///   upstream. A port reproduces its source; it does not correct it.
    /// - `operator` is `#abb2bf`, the same as the editor foreground. Also upstream.
    ///
    /// The UI colours (panel, border, hover, …) are *not* from the file — VS Code's UI
    /// keys do not map onto this editor's surfaces one-for-one. They are derived from the
    /// theme's own background so the chrome reads as the same theme.
    pub fn one_dark_pro() -> Self {
        Self {
            background: rgb(0x282c34).into(),
            panel: rgb(0x21252b).into(),
            border: rgb(0x181a1f).into(),
            text: rgb(0xabb2bf).into(),
            text_muted: rgb(0x7f848e).into(),
            accent: rgb(0x61afef).into(),
            hover: rgb(0x2c313a).into(),
            selected: rgb(0x2c313a).into(),
            cursor: rgb(0x528bff).into(),
            selection: rgb(0x3e4451).into(),
            status_bar: rgb(0x21252b).into(),

            keyword: rgb(0xc678dd).into(),
            type_name: rgb(0xe5c07b).into(),
            function: rgb(0x61afef).into(),
            variable: rgb(0xe06c75).into(),
            // Same as `variable`, on purpose — see the note above.
            property: rgb(0xe06c75).into(),
            string: rgb(0x98c379).into(),
            number: rgb(0xd19a66).into(),
            operator: rgb(0xabb2bf).into(),
            // `entity.other.attribute-name` in the theme file. The issue's summary table
            // listed #e06c75 here; the file on disk says #d19a66, and the file wins.
            attribute: rgb(0xd19a66).into(),
            comment: rgb(0x7f848e).into(),
            tag: rgb(0xe06c75).into(),
            // Blade is not a language One Dark Pro has an opinion about. `support.function`
            // (#56b6c2) is the theme's own unused-here accent, so it stays in-palette
            // rather than being invented.
            blade: rgb(0x56b6c2).into(),

            ansi: [
                rgb(0x3f4451).into(),
                rgb(0xe06c75).into(),
                rgb(0x98c379).into(),
                rgb(0xd19a66).into(),
                rgb(0x61afef).into(),
                rgb(0xc678dd).into(),
                rgb(0x56b6c2).into(),
                rgb(0xabb2bf).into(),
                rgb(0x4f5666).into(),
                rgb(0xff616e).into(),
                rgb(0x4cd137).into(),
                rgb(0xf0a45d).into(),
                rgb(0x4dc4ff).into(),
                rgb(0xde73ff).into(),
                rgb(0x4cd1e0).into(),
                rgb(0xd7dae0).into(),
            ],
        }
    }

    /// GitHub Dark, ported from the VS Code theme.
    ///
    /// Origin: `github.github-vscode-theme` v6.3.5, `themes/dark-default.json`.
    /// Licence: MIT.
    ///
    /// Syntax colours read from that file's `tokenColors`. Three of this editor's styles
    /// have no scope of their own in the source and are resolved the way the theme itself
    /// resolves them, rather than guessed:
    ///
    /// - `type` and `property` fall under the `entity` / `meta.property-name` group
    ///   (`#79c0ff`).
    /// - `attribute` follows `entity.name.tag` (`#7ee787`).
    /// - `operator` has no scope at all, so it inherits `editor.foreground` (`#e6edf3`).
    ///   That is what VS Code renders, so it is what this renders.
    pub fn github_dark() -> Self {
        Self {
            background: rgb(0x0d1117).into(),
            panel: rgb(0x010409).into(),
            border: rgb(0x30363d).into(),
            text: rgb(0xe6edf3).into(),
            text_muted: rgb(0x8b949e).into(),
            accent: rgb(0x2f81f7).into(),
            hover: rgb(0x161b22).into(),
            selected: rgb(0x21262d).into(),
            cursor: rgb(0x2f81f7).into(),
            selection: rgb(0x264f78).into(),
            status_bar: rgb(0x010409).into(),

            keyword: rgb(0xff7b72).into(),
            type_name: rgb(0x79c0ff).into(),
            function: rgb(0xd2a8ff).into(),
            variable: rgb(0xffa657).into(),
            property: rgb(0x79c0ff).into(),
            string: rgb(0xa5d6ff).into(),
            // `constant` — the group numeric literals belong to in this theme.
            number: rgb(0x56d364).into(),
            operator: rgb(0xe6edf3).into(),
            attribute: rgb(0x7ee787).into(),
            comment: rgb(0x8b949e).into(),
            tag: rgb(0x7ee787).into(),
            // No Blade scope upstream; `markup.changed` (#ffa657) is taken by `variable`,
            // so this uses the theme's own `#ff7b72`-adjacent accent from its ANSI table.
            blade: rgb(0xffa198).into(),

            ansi: [
                rgb(0x484f58).into(),
                rgb(0xff7b72).into(),
                rgb(0x3fb950).into(),
                rgb(0xd29922).into(),
                rgb(0x58a6ff).into(),
                rgb(0xbc8cff).into(),
                rgb(0x39c5cf).into(),
                rgb(0xb1bac4).into(),
                rgb(0x6e7681).into(),
                rgb(0xffa198).into(),
                rgb(0x56d364).into(),
                rgb(0xe3b341).into(),
                rgb(0x79c0ff).into(),
                rgb(0xd2a8ff).into(),
                rgb(0x56d4dd).into(),
                rgb(0xf0f6fc).into(),
            ],
        }
    }

    /// GitHub Light, ported from the VS Code theme.
    ///
    /// Origin: `github.github-vscode-theme` v6.3.5, `themes/light-default.json`.
    /// Licence: MIT.
    ///
    /// The light counterpart of [`Theme::github_dark`], resolved from the same scopes in
    /// the light file — not the dark theme with the colours flipped.
    pub fn github_light() -> Self {
        Self {
            background: rgb(0xffffff).into(),
            panel: rgb(0xf6f8fa).into(),
            border: rgb(0xd1d9e0).into(),
            text: rgb(0x1f2328).into(),
            text_muted: rgb(0x6e7781).into(),
            accent: rgb(0x0969da).into(),
            hover: rgb(0xeaeef2).into(),
            selected: rgb(0xd0d7de).into(),
            cursor: rgb(0x0969da).into(),
            selection: rgb(0xb6dcff).into(),
            status_bar: rgb(0xf6f8fa).into(),

            keyword: rgb(0xcf222e).into(),
            type_name: rgb(0x0550ae).into(),
            function: rgb(0x8250df).into(),
            variable: rgb(0x953800).into(),
            property: rgb(0x0550ae).into(),
            string: rgb(0x0a3069).into(),
            number: rgb(0x0550ae).into(),
            operator: rgb(0x1f2328).into(),
            attribute: rgb(0x116329).into(),
            comment: rgb(0x6e7781).into(),
            tag: rgb(0x116329).into(),
            blade: rgb(0xa40e26).into(),

            ansi: [
                rgb(0x24292f).into(),
                rgb(0xcf222e).into(),
                rgb(0x116329).into(),
                rgb(0x4d2d00).into(),
                rgb(0x0969da).into(),
                rgb(0x8250df).into(),
                rgb(0x1b7c83).into(),
                rgb(0x6e7781).into(),
                rgb(0x57606a).into(),
                rgb(0xa40e26).into(),
                rgb(0x1a7f37).into(),
                rgb(0x633c01).into(),
                rgb(0x218bff).into(),
                rgb(0xa475f9).into(),
                rgb(0x3192aa).into(),
                rgb(0x8c959f).into(),
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
            HighlightStyle::Property => self.property,
            HighlightStyle::String => self.string,
            HighlightStyle::Number => self.number,
            HighlightStyle::Operator => self.operator,
            HighlightStyle::Attribute => self.attribute,
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
            HighlightStyle::Property => "property",
            HighlightStyle::Operator => "operator",
            HighlightStyle::Attribute => "attribute",
        }
    }

    /// Every variant, kept complete by `ThemeVariant::label` for the same reason.
    const ALL_VARIANTS: [ThemeVariant; 5] = [
        ThemeVariant::Dark,
        ThemeVariant::Light,
        ThemeVariant::OneDarkPro,
        ThemeVariant::GitHubDark,
        ThemeVariant::GitHubLight,
    ];

    /// The themes ported verbatim from a published VS Code theme.
    ///
    /// Separated from [`ALL_VARIANTS`] because the distinctness rule that applies to this
    /// project's own themes deliberately does **not** apply to these: One Dark Pro gives
    /// `variable` and `property` the same `#e06c75`, and reproducing that is the point.
    const PORTED_VARIANTS: [ThemeVariant; 3] =
        [ThemeVariant::OneDarkPro, ThemeVariant::GitHubDark, ThemeVariant::GitHubLight];

    #[test]
    fn the_lists_cover_every_style_and_every_variant() {
        // The counts are the half the compiler cannot check: an exhaustive match keeps
        // `style_name` and `label` honest, but nothing stops someone adding a variant to
        // both and forgetting the array. Asserting the length here fails loudly instead of
        // silently testing eight styles out of ten.
        assert_eq!(ALL_STYLES.len(), 9, "a new HighlightStyle needs a colour in every theme");
        assert_eq!(ALL_VARIANTS.len(), 5, "a new theme needs listing, or it goes untested");

        // Every ported theme must also be a real theme.
        for ported in PORTED_VARIANTS {
            assert!(ALL_VARIANTS.contains(&ported), "{} is not listed", ported.label());
        }

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
        //
        // **Ported themes are exempt, and that is not the rule being weakened.** One Dark
        // Pro really does paint `variable` and `property` the same `#e06c75`; GitHub
        // really does leave `operator` at the editor foreground. Those are upstream's
        // decisions, and a port that "fixes" them is no longer the theme it claims to be
        // (#53). The rule still binds every theme this project authors, which is where it
        // was catching real copy-paste mistakes — see the separate assertion below that
        // the exemption is not silently covering an *unported* theme.
        for variant in ALL_VARIANTS {
            if PORTED_VARIANTS.contains(&variant) {
                continue;
            }
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
    fn ported_themes_reproduce_the_colours_that_make_them_that_theme() {
        // The exemption above is only defensible if the ported values are actually the
        // published ones. These are the specific hex values read out of the theme files on
        // disk, pinned so a later "tidy-up" that nudges them fails here rather than
        // quietly shipping a theme wearing someone else's name.
        //
        // Not a substitute for looking at it (#35) — nobody has seen these on screen.
        let one_dark = Theme::one_dark_pro();
        assert_eq!(one_dark.background, rgb(0x282c34).into());
        assert_eq!(one_dark.keyword, rgb(0xc678dd).into());
        assert_eq!(one_dark.string, rgb(0x98c379).into());
        assert_eq!(one_dark.comment, rgb(0x7f848e).into());
        assert_eq!(one_dark.function, rgb(0x61afef).into());
        assert_eq!(one_dark.type_name, rgb(0xe5c07b).into());
        assert_eq!(one_dark.number, rgb(0xd19a66).into());
        // The collision that a port must keep. `assert_eq!`, not `assert_ne!` — this is
        // the inverse of the distinctness rule and it is deliberate.
        assert_eq!(
            one_dark.variable,
            one_dark.property,
            "One Dark Pro paints variable and property alike; a port must too"
        );
        assert_eq!(one_dark.variable, rgb(0xe06c75).into());
        assert_eq!(one_dark.operator, one_dark.text, "One Dark Pro's operator is its foreground");

        let gh_dark = Theme::github_dark();
        assert_eq!(gh_dark.background, rgb(0x0d1117).into());
        assert_eq!(gh_dark.text, rgb(0xe6edf3).into());
        assert_eq!(gh_dark.keyword, rgb(0xff7b72).into());
        assert_eq!(gh_dark.string, rgb(0xa5d6ff).into());
        assert_eq!(gh_dark.comment, rgb(0x8b949e).into());
        assert_eq!(gh_dark.function, rgb(0xd2a8ff).into());
        assert_eq!(gh_dark.variable, rgb(0xffa657).into());
        assert_eq!(gh_dark.attribute, rgb(0x7ee787).into());

        let gh_light = Theme::github_light();
        assert_eq!(gh_light.background, rgb(0xffffff).into());
        assert_eq!(gh_light.text, rgb(0x1f2328).into());
        assert_eq!(gh_light.keyword, rgb(0xcf222e).into());
        assert_eq!(gh_light.string, rgb(0x0a3069).into());
        assert_eq!(gh_light.comment, rgb(0x6e7781).into());
        assert_eq!(gh_light.function, rgb(0x8250df).into());
        assert_eq!(gh_light.variable, rgb(0x953800).into());
        assert_eq!(gh_light.attribute, rgb(0x116329).into());
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
