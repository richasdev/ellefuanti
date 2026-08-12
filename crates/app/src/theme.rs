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

use crate::lsp_session::Severity;

#[derive(Clone)]
pub struct Theme {
    pub background: Hsla,
    pub panel: Hsla,
    pub border: Hsla,
    pub text: Hsla,
    pub text_muted: Hsla,
    pub accent: Hsla,
    /// Files git reports as changed — tree rows and any future badge. Its own field per
    /// the convention: a tint chosen against one background is invisible on another.
    pub modified: Hsla,
    pub hover: Hsla,
    pub selected: Hsla,

    /// The background of a control while the mouse is held down on it (#71).
    ///
    /// A third step past `hover`, not a reuse of `selected`: those two mean different
    /// things and collide on themes where they are the same colour — `one_dark_pro` sets
    /// `hover` and `selected` to the same `0x2c313a`, so a pressed state borrowing either
    /// would be invisible on exactly the theme most likely to be in use. Each variant
    /// pushes one step further from its own background, which is the only way a press
    /// reads on both `0x16171d` and `0xffffff`.
    pub pressed: Hsla,
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

    /// Diagnostic underlines, one per severity the UI distinguishes.
    ///
    /// Separate from `accent` and from the ANSI reds even where a variant happens to reuse
    /// the same hex: an error squiggle and the cursor are different things, and a theme
    /// that wants to change one without the other has to be able to. Four fields rather
    /// than an array because `lsp_session::Severity` is a closed enum and a named field
    /// per variant makes an unhandled severity a compile error instead of an index.
    pub error: Hsla,
    pub warning: Hsla,
    pub information: Hsla,
    pub hint: Hsla,

    /// The vertical rule drawn at each indent level, and the tint over trailing whitespace.
    ///
    /// Two fields rather than a reuse of `border` or `text_muted`, because a guide has a
    /// contrast requirement neither of those meets in both directions: it has to be
    /// *findable* against the background and *quieter* than any character, and a value that
    /// does that on `#282c34` is invisible on `#ffffff`. That is the whole reason #82 calls
    /// this out — one grey cannot serve five variants, so each one picks its own.
    ///
    /// `trailing_whitespace` is a background tint, not a glyph: rendering a `·` per space
    /// would change the text being laid out, which breaks the 1:1 byte mapping every
    /// highlight range in `line_runs` depends on. A background over the real spaces marks
    /// them without inventing characters.
    pub indent_guide: Hsla,
    pub trailing_whitespace: Hsla,

    /// The sixteen ANSI slots the terminal renders: 0-7 normal, 8-15 bright.
    ///
    /// An array rather than sixteen named fields because the terminal indexes it
    /// numerically — `Ansi(9)` comes straight off the wire — and naming each one would
    /// mean a sixteen-arm match that does nothing but arithmetic.
    pub ansi: [Hsla; 16],
}

/// Chrome metrics: the sizes that are not a function of the font.
///
/// Font size, UI font size, editor line height, gutter width and the terminal's row height
/// used to live here as consts too. They are settings now, or derived from one, and moved to
/// [`crate::fonts::Fonts`] — a const cannot be read from a settings file, and the two derived
/// ones had to move because a fixed pixel value is simply *wrong* at another font size: 52px
/// of gutter loses a five-digit line number at 20px, and a 16px terminal row overlaps its own
/// text.
///
/// What is left is genuinely fixed: an activity bar is 44px because that is how wide an
/// icon button is, not because of the text inside it. These become settings when someone
/// asks, not before.
pub struct Metrics;

impl Metrics {
    pub const ACTIVITY_BAR_WIDTH: Pixels = px(44.0);
    /// The sidebar's width *on first launch*. Since the owner's resize request it is a
    /// starting point rather than the width: `WorkspaceView` owns the live value and the
    /// divider drags it.
    pub const SIDEBAR_WIDTH: Pixels = px(240.0);
    /// Same, for the AI chat panel (#99 shipped it fixed at this width).
    pub const AI_CHAT_WIDTH: Pixels = px(380.0);
    /// What a panel may not be dragged below or above.
    ///
    /// A floor because a panel dragged to nothing is a panel the user cannot get back —
    /// the divider goes with it. A ceiling because the editor is the point of the window;
    /// a sidebar that can eat it is a layout bug waiting for a bug report.
    pub const PANEL_MIN_WIDTH: Pixels = px(160.0);
    pub const PANEL_MAX_WIDTH: Pixels = px(720.0);
    pub const TAB_HEIGHT: Pixels = px(32.0);
    pub const STATUS_HEIGHT: Pixels = px(24.0);
    pub const ROW_HEIGHT: Pixels = px(22.0);

    /// Height of the terminal panel, including its tab strip.
    /// ponytail: fixed, not draggable. A splitter needs a drag-handle element and a
    /// persisted layout; both arrive with the settings crate.
    ///
    /// Deliberately still a constant while the grid inside it scales: this is how much room
    /// the panel takes from the editor, which is a layout preference, not a consequence of
    /// the font. A taller font means fewer rows in the same panel, which is what the PTY is
    /// told — see `Fonts::cell_size`.
    pub const TERMINAL_HEIGHT: Pixels = px(260.0);
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
    /// Every built-in, in the order the settings panel's picker cycles them (#100).
    ///
    /// A `match`-adjacent list is a drift risk; the compiler closes it: adding a variant
    /// without extending this fails the exhaustiveness test in this file's tests.
    pub const ALL: [ThemeVariant; 5] =
        [Self::Dark, Self::Light, Self::OneDarkPro, Self::GitHubDark, Self::GitHubLight];

    pub fn build(self) -> Theme {
        match self {
            Self::Dark => Theme::dark(),
            Self::Light => Theme::light(),
            Self::OneDarkPro => Theme::one_dark_pro(),
            Self::GitHubDark => Theme::github_dark(),
            Self::GitHubLight => Theme::github_light(),
        }
    }

    /// Four swatch colours — background, accent, string, keyword — for the settings
    /// panel's theme picker. Lives here rather than in the picker because building a
    /// `Theme` is this module's monopoly (#48's architecture test enforces it).
    pub fn preview(self) -> [Hsla; 4] {
        let theme = self.build();
        [theme.background, theme.accent, theme.string, theme.keyword]
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

    /// The name this variant has in `settings.json`.
    ///
    /// Separate from [`label`](Self::label) even though both are strings, because they
    /// answer to different people: `label` is shown in the command palette and may be
    /// reworded freely, while this is written to a file someone else's build has to read
    /// back. Deriving one from the other would mean a copy edit silently invalidating
    /// every saved setting.
    ///
    /// Lowercase and hyphenated, matching VS Code's convention, so #58's importer maps
    /// names rather than reformatting them.
    pub fn name(self) -> &'static str {
        match self {
            Self::Dark => "dark",
            Self::Light => "light",
            Self::OneDarkPro => "one-dark-pro",
            Self::GitHubDark => "github-dark",
            Self::GitHubLight => "github-light",
        }
    }

    /// The variant a settings file named, or `None` if this build has no such theme.
    ///
    /// `None` rather than defaulting here: the caller wants to say *which* name it did not
    /// recognise, and a function that quietly returns Dark cannot. #58's disk themes turn
    /// this from "unknown, use the default" into "not compiled in, look on disk".
    pub fn from_name(name: &str) -> Option<Self> {
        ALL_VARIANTS.into_iter().find(|variant| variant.name() == name)
    }
}

/// Which theme is active: one of the five compiled in, or one loaded from disk.
///
/// A separate type from [`ThemeVariant`] rather than a sixth arm on it, because the two
/// have different guarantees and collapsing them would give up the one that matters.
/// `ThemeVariant` is closed: a match on it cannot reach a theme that does not exist, and
/// `build()` cannot fail. A disk theme is a name that may or may not still be on the
/// filesystem, which is a different thing wearing the same word.
///
/// The practical consequence is #58's hard constraint — **a missing or corrupt
/// `assets/themes/` must still launch with Dark available.** Because the built-in path never
/// touches this variant, there is no code path on which a bad file can leave the editor
/// without colours.
#[derive(Clone, PartialEq, Debug)]
pub enum ThemeChoice {
    BuiltIn(ThemeVariant),
    /// A theme read from a file, by the name it was selected under.
    Disk(String),
}

impl Default for ThemeChoice {
    fn default() -> Self {
        Self::BuiltIn(ThemeVariant::default())
    }
}

impl ThemeChoice {
    /// The name this choice has in `settings.json`.
    pub fn name(&self) -> &str {
        match self {
            Self::BuiltIn(variant) => variant.name(),
            Self::Disk(name) => name,
        }
    }

    /// A compiled-in variant if the name is one, otherwise a disk theme by that name.
    ///
    /// Built-ins win a name collision. A file called `dark.json` cannot shadow
    /// `Theme::dark()`, which is what keeps "Dark is always available" true even when
    /// someone has dropped a broken theme with that name into the directory.
    pub fn from_name(name: &str) -> Self {
        match ThemeVariant::from_name(name) {
            Some(variant) => Self::BuiltIn(variant),
            None => Self::Disk(name.to_string()),
        }
    }
}

/// Every variant. Moved out of the test module in #60 because `from_name` needs it: one
/// list means a theme cannot be given a `name` and forgotten here, which would be a theme
/// that saves and never loads back.
///
/// The length assertion in the test module is the half the compiler cannot check — an
/// exhaustive match keeps `name` and `label` honest, but nothing stops someone adding an
/// arm to every match and not to this array.
const ALL_VARIANTS: [ThemeVariant; 5] = [
    ThemeVariant::Dark,
    ThemeVariant::Light,
    ThemeVariant::OneDarkPro,
    ThemeVariant::GitHubDark,
    ThemeVariant::GitHubLight,
];

/// The active theme, as gpui stores it.
///
/// Private, and holding rather than being a `Theme`, so the only way into the global is
/// [`set_theme`] and the only way out is [`Themed::theme`]. A view cannot set it and cannot
/// construct one to set.
struct ActiveTheme {
    /// What is active. `BuiltIn` for the five compiled in, `Disk` for a loaded file.
    choice: ThemeChoice,
    /// The variant `theme.toggle` cycles from.
    ///
    /// Kept alongside `choice` because the cycle is over the built-ins only: a disk theme
    /// has no position in `ThemeVariant::next()`, and toggling out of one has to resume
    /// from somewhere. This is that somewhere — the last built-in that was active, or the
    /// default if a disk theme was selected at startup.
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
    /// What is actually active, which a disk theme's name does not fit in a `ThemeVariant`.
    fn theme_choice(&self) -> ThemeChoice;
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

    fn theme_choice(&self) -> ThemeChoice {
        self.global::<ActiveTheme>().choice.clone()
    }
}

/// Installs a theme. Called once at startup and again on every switch.
///
/// `set_global` notifies global observers, but observing is not what repaints — gpui
/// repaints an entity when *it* is notified. The caller does the refresh; see
/// `WorkspaceView::toggle_theme`.
///
/// The choice is persisted here rather than in `WorkspaceView::toggle_theme`, because this
/// is the one funnel every theme change goes through — a second way to switch themes (the
/// palette already has one, #58 will add another) gets persistence for free instead of
/// having to remember to ask for it. See [`crate::settings::persist_theme`] for what makes
/// the startup call and the test calls not write anything.
pub fn set_theme(variant: ThemeVariant, cx: &mut App) {
    install(ThemeChoice::BuiltIn(variant), variant, variant.build(), cx);
}

/// Installs a theme loaded from disk.
///
/// The same funnel as [`set_theme`], so a disk theme persists for the same reason a built-in
/// does and neither call site has to remember to ask.
///
/// `resume` is the built-in `theme.toggle` continues from, since a disk theme has no place
/// in the cycle. The caller passes whatever was active before, which makes toggling out of a
/// disk theme land where the user was rather than always at Dark.
pub fn set_disk_theme(file: &elle_theme::ThemeFile, resume: ThemeVariant, cx: &mut App) {
    install(ThemeChoice::Disk(file.name.clone()), resume, Theme::from_file(file), cx);
}

/// The one place the global is written, and therefore the one place persistence happens.
fn install(choice: ThemeChoice, variant: ThemeVariant, theme: Theme, cx: &mut App) {
    let previous = cx.try_global::<ActiveTheme>().map(|active| active.choice.clone());
    let name = choice.name().to_string();
    cx.set_global(ActiveTheme { choice, variant, theme });

    if previous.as_ref().map(ThemeChoice::name) != Some(name.as_str()) {
        crate::settings::persist_theme_name(&name, cx);
    }
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
            modified: rgb(0xd8a657).into(),
            hover: rgb(0x24272f).into(),
            selected: rgb(0x2d313c).into(),
            // One step past `selected`, continuing the same ramp away from the background.
            pressed: rgb(0x383d4a).into(),
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

            // Red/amber/blue/grey, in this theme's own register rather than pure hues: a
            // saturated 0xff0000 on a 0x16171d background vibrates. Hint is close to
            // `comment` on purpose — an advisory underline should be findable, not loud.
            error: rgb(0xf7768e).into(),
            warning: rgb(0xe0af68).into(),
            information: rgb(0x7dd3fc).into(),
            hint: rgb(0x6b7280).into(),

            // A guide has to be findable and never compete with a character. On this
            // background that is a few steps up from `border` (0x2a2d36) — `border` itself
            // separates panels and is too dark to see inside the text area.
            indent_guide: rgb(0x363a45).into(),
            // Warmer than the guide so trailing space reads as "wrong" rather than "grid".
            trailing_whitespace: rgb(0x4a2b35).into(),

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
            modified: rgb(0xb58900).into(),
            hover: rgb(0xe6e8ef).into(),
            selected: rgb(0xdcdfe9).into(),
            // On a light theme the ramp runs *darker* to move away from the background,
            // which is the whole reason this is a per-variant value and not a filter.
            pressed: rgb(0xccd0dd).into(),
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

            // Darkened against the light background: the dark theme's 0xf7768e is legible
            // on 0x16171d and washes out on 0xfbfbfd. Contrast, not hue, is what changes.
            error: rgb(0xc4314b).into(),
            warning: rgb(0x9a6700).into(),
            information: rgb(0x0b6e8f).into(),
            hint: rgb(0x8a8f9c).into(),

            // The direction reverses on a light theme: the guide is *darker* than the
            // background, not lighter. The dark theme's 0x363a45 would be a near-black bar
            // on 0xfbfbfd — which is the exact failure #82 names.
            indent_guide: rgb(0xe2e4ec).into(),
            trailing_whitespace: rgb(0xf7dfe4).into(),
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
            modified: rgb(0xe5c07b).into(),
            hover: rgb(0x2c313a).into(),
            selected: rgb(0x2c313a).into(),
            // Upstream gives `hover` and `selected` the same value, so a press has to be a
            // colour neither of them is or it would not be visible on this theme at all.
            // Derived like the rest of this theme's UI colours, from its own background.
            pressed: rgb(0x3a4150).into(),
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

            // One Dark Pro's own `editorError.foreground` / `editorWarning.foreground`.
            // Information and hint borrow the theme's blue and comment grey — upstream
            // leaves both at VS Code's defaults, which are not this theme's colours.
            error: rgb(0xe06c75).into(),
            warning: rgb(0xe5c07b).into(),
            information: rgb(0x61afef).into(),
            hint: rgb(0x7f848e).into(),

            // One Dark Pro's own `editorIndentGuide.background` (#3b4048). The one UI key
            // upstream actually defines for this, so it is taken rather than derived.
            indent_guide: rgb(0x3b4048).into(),
            trailing_whitespace: rgb(0x4b3238).into(),

            // The theme file's own `terminal.ansi*` keys, verbatim.
            //
            // **Eight of these were wrong until #58.** The hand-extraction substituted the
            // syntax palette for the terminal one — slot 1 was `#e06c75`, the keyword red,
            // where the file says `#e05561`; slot 2 was the string green; slot 10 was
            // `#4cd137`, a colour that appears nowhere in One Dark Pro at all. The importer
            // reading the same file disagreed, and the file won. Pinned by
            // `crates/theme/tests/vscode.rs`, which reads it rather than trusting this.
            ansi: [
                rgb(0x3f4451).into(), // 0 black
                rgb(0xe05561).into(), // 1 red
                rgb(0x8cc265).into(), // 2 green
                rgb(0xd18f52).into(), // 3 yellow
                rgb(0x4aa5f0).into(), // 4 blue
                rgb(0xc162de).into(), // 5 magenta
                rgb(0x42b3c2).into(), // 6 cyan
                rgb(0xd7dae0).into(), // 7 white
                rgb(0x4f5666).into(), // 8 bright black
                rgb(0xff616e).into(), // 9 bright red
                rgb(0xa5e075).into(), // 10 bright green
                rgb(0xf0a45d).into(), // 11 bright yellow
                rgb(0x4dc4ff).into(), // 12 bright blue
                rgb(0xde73ff).into(), // 13 bright magenta
                rgb(0x4cd1e0).into(), // 14 bright cyan
                rgb(0xe6e6e6).into(), // 15 bright white
            ],
        }
    }

    /// GitHub Dark, ported from the VS Code theme.
    ///
    /// Origin: `github.github-vscode-theme` v6.3.5, `themes/dark-default.json`.
    /// Licence: MIT.
    ///
    /// Syntax colours read from that file's `tokenColors`. Several of this editor's styles
    /// have no scope of their own in the source and are resolved the way the theme itself
    /// resolves them, rather than guessed:
    ///
    /// - `type` and `property` fall under the `entity` / `meta.property-name` group
    ///   (`#79c0ff`).
    /// - `attribute` is also `entity` (`#79c0ff`). **This was `#7ee787` until #58**, on the
    ///   stated reasoning that it "follows `entity.name.tag`". It cannot: an attribute's
    ///   scope is `entity.other.attribute-name`, which diverges from `entity.name.tag` at
    ///   the second segment, so the tag rule never matches it. The only selector in the file
    ///   that does match is the bare `entity`.
    /// - `operator` is the keyword red (`#ff7b72`). **Also corrected in #58**, where it was
    ///   `editor.foreground` on the reasoning that operators have "no scope at all". There
    ///   is no `keyword.operator` *rule*, but the bare `keyword` selector matches
    ///   `keyword.operator` and everything under it — and VS Code's own PHP grammar scopes
    ///   `=`, `->` and `??` as `keyword.operator.assignment.php` and friends, all of which
    ///   begin `keyword.`. So GitHub paints PHP operators red, and `#e6edf3` is a colour
    ///   they never take.
    ///
    /// Both corrections came from `crates/theme`'s importer reading this same file and
    /// disagreeing with the hand-extracted values; the file won.
    pub fn github_dark() -> Self {
        Self {
            background: rgb(0x0d1117).into(),
            panel: rgb(0x010409).into(),
            border: rgb(0x30363d).into(),
            text: rgb(0xe6edf3).into(),
            text_muted: rgb(0x8b949e).into(),
            accent: rgb(0x2f81f7).into(),
            modified: rgb(0xd29922).into(),
            hover: rgb(0x161b22).into(),
            selected: rgb(0x21262d).into(),
            // GitHub's own `--button-default-bgColor-active`, which is the right neighbour
            // for this: it is literally what GitHub paints on a held button.
            pressed: rgb(0x30363d).into(),
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
            // Both from the bare `keyword` and `entity` rules; see the doc comment.
            operator: rgb(0xff7b72).into(),
            attribute: rgb(0x79c0ff).into(),
            comment: rgb(0x8b949e).into(),
            tag: rgb(0x7ee787).into(),
            // No Blade scope upstream; `markup.changed` (#ffa657) is taken by `variable`,
            // so this uses the theme's own `#ff7b72`-adjacent accent from its ANSI table.
            blade: rgb(0xffa198).into(),

            // The theme's own red and amber, which are its `terminal.ansiRed` (#ff7b72) and
            // `terminal.ansiYellow` (#d29922), with its `accent` blue for information and
            // `text_muted` for hints.
            //
            // This comment used to credit `editorError.foreground` and
            // `editorWarning.foreground`. **Neither key exists in the file** — GitHub leaves
            // both to VS Code's built-in defaults, which are not this theme's colours. The
            // values are right and the attribution was not; corrected in #58.
            error: rgb(0xff7b72).into(),
            warning: rgb(0xd29922).into(),
            information: rgb(0x2f81f7).into(),
            hint: rgb(0x8b949e).into(),

            // GitHub Dark's `editorIndentGuide.background1` (#21262d) lifted one step: the
            // upstream value is nearly the editor background and disappears at this size.
            indent_guide: rgb(0x2d333b).into(),
            trailing_whitespace: rgb(0x4a2529).into(),

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
                // `terminal.ansiBrightWhite` is #ffffff in the file; this said #f0f6fc
                // until #58.
                rgb(0xffffff).into(),
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
            modified: rgb(0x9a6700).into(),
            hover: rgb(0xeaeef2).into(),
            selected: rgb(0xd0d7de).into(),
            // The light counterpart, again from GitHub's active-button token.
            pressed: rgb(0xafb8c1).into(),
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
            // The light counterparts of the two corrections in `github_dark`: the bare
            // `keyword` rule for operators, the bare `entity` rule for attributes.
            operator: rgb(0xcf222e).into(),
            attribute: rgb(0x0550ae).into(),
            comment: rgb(0x6e7781).into(),
            tag: rgb(0x116329).into(),
            blade: rgb(0xa40e26).into(),

            // The theme's own red and a darkened amber — legible on white, where the dark
            // theme's values are not. As in `github_dark`, this comment used to credit
            // `editorError.foreground` and `editorWarning.foreground`, and neither key is in
            // the file; corrected in #58.
            error: rgb(0xcf222e).into(),
            warning: rgb(0x9a6700).into(),
            information: rgb(0x0969da).into(),
            hint: rgb(0x6e7781).into(),

            // GitHub Light's `editorIndentGuide.background1` (#d8dee4), and its red-tinted
            // danger background for the trailing tint.
            indent_guide: rgb(0xd8dee4).into(),
            trailing_whitespace: rgb(0xffebe9).into(),

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

    /// Builds a theme from a file loaded by `elle-theme`.
    ///
    /// The translation layer ADR-0004 requires: `elle-theme` is plain Rust and hands back
    /// `u32` RGB, and `Hsla` is a gpui type, so the conversion has to live here.
    ///
    /// Infallible, because `ThemeFile::parse` has already rejected any file missing a
    /// required colour — by the time one of these exists, every key is present. The
    /// `unwrap_or` arms are therefore unreachable rather than merely unlikely, and they
    /// fall back to `text` and `background` rather than to black so that a future key added
    /// to `Theme` without being added to `REQUIRED_COLORS` is a wrong colour somebody can
    /// see, not an invisible one.
    pub fn from_file(file: &elle_theme::ThemeFile) -> Self {
        let color =
            |key: &str, fallback: u32| gpui::rgb(file.color(key).unwrap_or(fallback)).into();

        let background = file.color("background").unwrap_or(0x000000);
        let text = file.color("text").unwrap_or(0xffffff);

        let ansi = file.ansi().unwrap_or([text; 16]).map(|slot| gpui::rgb(slot).into());

        Self {
            background: gpui::rgb(background).into(),
            panel: color("panel", background),
            border: color("border", background),
            text: gpui::rgb(text).into(),
            text_muted: color("text_muted", text),
            accent: color("accent", text),
            // Disk themes may name it; the accent is the fallback that stays visible on
            // any background the theme chose.
            modified: color("modified", file.color("accent").unwrap_or(text)),
            hover: color("hover", background),
            selected: color("selected", background),
            pressed: color("pressed", background),
            cursor: color("cursor", text),
            selection: color("selection", background),
            status_bar: color("status_bar", background),

            keyword: color("keyword", text),
            type_name: color("type", text),
            function: color("function", text),
            variable: color("variable", text),
            property: color("property", text),
            string: color("string", text),
            number: color("number", text),
            operator: color("operator", text),
            attribute: color("attribute", text),
            comment: color("comment", text),
            tag: color("tag", text),
            blade: color("blade", text),

            error: color("error", text),
            warning: color("warning", text),
            information: color("information", text),
            hint: color("hint", text),

            // Optional keys, not required ones, and deliberately so: adding either to
            // `REQUIRED_COLORS` would reject every theme file already on disk, for a rule
            // whose whole subject is a hairline that most theme authors have no opinion
            // about. A file that does set them wins; one that does not falls back to
            // `border` (the closest "quiet line" the file must define) and to `selection`
            // (a tint the file must define that is already meant to sit under text).
            indent_guide: color("indent_guide", file.color("border").unwrap_or(background)),
            trailing_whitespace: color(
                "trailing_whitespace",
                file.color("selection").unwrap_or(background),
            ),

            ansi,
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

    /// Colour for a diagnostic underline.
    ///
    /// The same discipline as [`Theme::syntax`]: the editor asks for a severity and the
    /// theme answers with a colour, so no renderer names one. Exhaustive over a closed
    /// enum, so adding a severity is a compile error here rather than a squiggle that
    /// silently paints as an error.
    pub fn diagnostic(&self, severity: Severity) -> Hsla {
        match severity {
            Severity::Error => self.error,
            Severity::Warning => self.warning,
            Severity::Information => self.information,
            Severity::Hint => self.hint,
        }
    }

    /// Background for a search match that is not the current one (#80).
    ///
    /// **Derived rather than a field.** Two more `Hsla`s would mean two more lines in each
    /// of the five variants, two more keys in `elle-theme`'s on-disk format, two more
    /// mappings in the VS Code importer, and a default for every theme file already
    /// written — a lot of surface for a colour that has exactly one correct answer in
    /// terms of colours the theme already picked.
    ///
    /// `selected` is that answer: it is defined per variant as "one step away from the
    /// background", light themes running darker and dark themes lighter, which is exactly
    /// what a match highlight needs and is the reason a hardcoded tint fails. A wash that
    /// reads on `#282c34` is invisible on `#ffffff`; `selected` is never invisible on its
    /// own background because that is what it was chosen for.
    pub fn search_match(&self) -> Hsla {
        self.selected
    }

    /// Background for the *current* match — the one ⌘G is sitting on.
    ///
    /// `selection` rather than `accent`: the current match becomes the document selection
    /// (`Document::select_match`), so painting it as anything else would show two
    /// different colours for one thing. The other matches then read as "the same, but not
    /// where you are", which is the distinction that matters.
    pub fn current_search_match(&self) -> Hsla {
        self.selection
    }

    /// Background for the bracket under the cursor and its partner (#87).
    ///
    /// #87 painted this as `theme.selection` inline. That was fine on its own and stopped
    /// being fine the moment #80 landed: `current_search_match` is *also* `selection`, so
    /// a bracket inside the hit ⌘G is sitting on would have been invisible against it —
    /// and searching for something like `function foo(` puts the cursor beside a bracket
    /// by definition, so it is a case people would hit rather than a theoretical one.
    ///
    /// `pressed` is the answer for the same reason `selected` was right for a match: it is
    /// defined per variant as one step further along the ramp away from the background,
    /// running darker on light themes and lighter on dark ones. Naming it here also means
    /// the renderer no longer picks a theme colour by hand, which is the rule everywhere
    /// else in this file.
    pub fn bracket_match(&self) -> Hsla {
        self.pressed
    }

    /// Foreground for the `+`/`-` marker in the diff gutter, and for a status row's letter
    /// (#64).
    ///
    /// **Derived, for the same reason as [`Theme::search_match`]** — and here the argument
    /// is stronger, because the obvious alternative is worse than an extra field. The
    /// obvious alternative is a hardcoded green and red, and a hardcoded green on
    /// `#ffffff` and on `#16171d` cannot both clear 4.5:1. `error` and `string` already
    /// solve exactly that problem per variant: every theme picked an `error` red that reads
    /// against its own background because diagnostics needed one, and a `string` colour
    /// that does the same because syntax needed one. Reusing them means a sixth theme gets
    /// legible diff colours by having answered questions it had to answer anyway.
    ///
    /// `string` for additions rather than a dedicated green: on all five variants `string`
    /// *is* the green-family colour, and on the two GitHub themes it is the same family
    /// GitHub uses for additions. Where a variant's `string` is not green the diff is still
    /// correct, because the meaning is carried by the `+` and `-` glyphs
    /// (`LineKind::marker`) and this is the redundant channel.
    ///
    /// **Colour is never the only signal.** `elle_git::LineKind::marker` and
    /// `elle_git::Status::marker` are rendered on every line and every row, so the panel is
    /// fully readable in monochrome. That is #64's requirement and #71's rule, and it is
    /// what makes deriving these two colours safe rather than a compromise.
    pub fn diff_added(&self) -> Hsla {
        self.string
    }

    /// Foreground for a removal, matching [`Theme::diff_added`].
    pub fn diff_removed(&self) -> Hsla {
        self.error
    }

    /// Background wash behind an added line.
    ///
    /// A tint of the foreground rather than another theme colour: it has to sit *under*
    /// syntax-highlighted text without fighting it, which rules out `selected` and
    /// `selection` — both are already spoken for by search and brackets, and a diff line
    /// that looks like a search hit is a lie. Low alpha over the variant's own added
    /// colour reads on any background, light or dark, because the hue comes from a colour
    /// that variant already proved legible against it.
    ///
    /// 0.15 is low enough that `theme.syntax(..)` text keeps its own contrast on top —
    /// which matters, because reusing the highlighter is the whole point of item 2.
    pub fn diff_added_background(&self) -> Hsla {
        Hsla { a: DIFF_WASH_ALPHA, ..self.diff_added() }
    }

    /// Background wash behind a removed line.
    pub fn diff_removed_background(&self) -> Hsla {
        Hsla { a: DIFF_WASH_ALPHA, ..self.diff_removed() }
    }
}

/// Alpha for the diff line washes.
///
/// One constant for both, so an addition and a deletion are equally loud. Named rather than
/// repeated because the two have to move together — a stronger red than green would make
/// deletions look like errors.
const DIFF_WASH_ALPHA: f32 = 0.15;

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
    fn every_theme_makes_a_press_visible_against_what_it_sits_on() {
        // A pressed state is the acknowledgement that a click landed (#71), and it is worth
        // exactly nothing if it is the same colour as the surface underneath it. The two it
        // can collide with are `hover` — because a press always happens *through* a hover,
        // so an equal value means the button does not change at the moment of clicking —
        // and the `panel` it is painted on.
        //
        // Unlike the syntax rule above there is **no exemption for ported themes**. These
        // are UI colours, which the ports already derive rather than read from the theme
        // file (see `one_dark_pro`'s doc comment) — so there is no upstream decision here
        // to be faithful to, and One Dark Pro is precisely the theme where the trap is
        // real: it gives `hover` and `selected` the same value, so a pressed state copied
        // from either would be invisible on it.
        //
        // Distinctness is not the same as *legibility*, which needs a contrast ratio and
        // an eye. Nobody has seen any of these on a screen (#35).
        for variant in ALL_VARIANTS {
            let theme = variant.build();

            assert_ne!(
                theme.pressed,
                theme.hover,
                "{}: a press looks identical to the hover it happens through",
                variant.label()
            );
            assert_ne!(
                theme.pressed,
                theme.panel,
                "{}: a press is invisible against the panel behind it",
                variant.label()
            );
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
            one_dark.variable, one_dark.property,
            "One Dark Pro paints variable and property alike; a port must too"
        );
        assert_eq!(one_dark.variable, rgb(0xe06c75).into());
        assert_eq!(one_dark.operator, one_dark.text, "One Dark Pro's operator is its foreground");

        // One Dark Pro's ANSI table is its own `terminal.ansi*` keys and not its syntax
        // palette — the distinction eight slots got wrong before #58. Slot 1 is the one to
        // watch: it is a hair off the keyword red, which is exactly why the substitution
        // was easy to make and hard to see.
        assert_eq!(one_dark.ansi[1], rgb(0xe05561).into(), "ansiRed, not the #e06c75 keyword");
        assert_eq!(one_dark.ansi[2], rgb(0x8cc265).into(), "ansiGreen, not the string green");
        assert_eq!(one_dark.ansi[10], rgb(0xa5e075).into(), "the old #4cd137 was not in the file");

        let gh_dark = Theme::github_dark();
        assert_eq!(gh_dark.background, rgb(0x0d1117).into());
        assert_eq!(gh_dark.text, rgb(0xe6edf3).into());
        assert_eq!(gh_dark.keyword, rgb(0xff7b72).into());
        assert_eq!(gh_dark.string, rgb(0xa5d6ff).into());
        assert_eq!(gh_dark.comment, rgb(0x8b949e).into());
        assert_eq!(gh_dark.function, rgb(0xd2a8ff).into());
        assert_eq!(gh_dark.variable, rgb(0xffa657).into());
        assert_eq!(gh_dark.tag, rgb(0x7ee787).into());
        // The two #58 corrections. `attribute` resolves through the bare `entity` rule and
        // `operator` through the bare `keyword` rule; neither is the value that was here.
        assert_eq!(gh_dark.attribute, rgb(0x79c0ff).into());
        assert_eq!(gh_dark.operator, rgb(0xff7b72).into());
        assert_ne!(gh_dark.attribute, gh_dark.tag, "attributes and tags are different scopes");

        let gh_light = Theme::github_light();
        assert_eq!(gh_light.background, rgb(0xffffff).into());
        assert_eq!(gh_light.text, rgb(0x1f2328).into());
        assert_eq!(gh_light.keyword, rgb(0xcf222e).into());
        assert_eq!(gh_light.string, rgb(0x0a3069).into());
        assert_eq!(gh_light.comment, rgb(0x6e7781).into());
        assert_eq!(gh_light.function, rgb(0x8250df).into());
        assert_eq!(gh_light.variable, rgb(0x953800).into());
        assert_eq!(gh_light.tag, rgb(0x116329).into());
        assert_eq!(gh_light.attribute, rgb(0x0550ae).into());
        assert_eq!(gh_light.operator, rgb(0xcf222e).into());
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

    #[test]
    fn every_theme_can_show_a_search_match() {
        // The failure the issue names: a tint chosen against `#282c34` is invisible on
        // `#ffffff`. Deriving both colours from per-variant values is what avoids it, and
        // this is what keeps that true for a sixth theme nobody has written yet.
        for variant in ALL_VARIANTS {
            let theme = variant.build();
            assert_ne!(
                theme.search_match(),
                theme.background,
                "{} paints a match the colour of the page it sits on",
                variant.label()
            );
            assert_ne!(
                theme.current_search_match(),
                theme.background,
                "{} paints the current match the colour of the page it sits on",
                variant.label()
            );
            assert_ne!(
                theme.search_match(),
                theme.current_search_match(),
                "{} cannot show which match ⌘G is on",
                variant.label()
            );
            // #87's bracket pair and #80's current match were both `selection` until the
            // two landed together. A bracket inside the hit ⌘G is on has to stay visible,
            // and searching for `function foo(` puts one there by definition.
            assert_ne!(
                theme.bracket_match(),
                theme.current_search_match(),
                "{} hides a bracket inside the current search match",
                variant.label()
            );
        }
    }

    #[test]
    fn every_built_in_variant_is_in_all() {
        // The match is the drift guard: a new variant refuses to compile here until it
        // gains an arm, and the length assert then refuses to pass until `ALL` grows too.
        match ThemeVariant::default() {
            ThemeVariant::Dark
            | ThemeVariant::Light
            | ThemeVariant::OneDarkPro
            | ThemeVariant::GitHubDark
            | ThemeVariant::GitHubLight => {}
        }
        assert_eq!(ThemeVariant::ALL.len(), 5);
        for variant in ThemeVariant::ALL {
            assert!(!variant.name().is_empty());
        }
    }
}
