//! The window root: activity bar, file tree, tabs, editor area, status bar, palette.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use elle_core::CommandRegistry;
use elle_laravel::{HttpMethod, Resolved, Route, extract_routes};
use elle_lsp::lsp_types::{
    CompletionResponse, DocumentSymbolResponse, GotoDefinitionResponse, Location, Uri,
};
use elle_test_runner::CancelFlag as TestCancelFlag;
use elle_text::Point;
use elle_workspace::{CancelFlag, FileTree, read_file, write_file};
use gpui::{
    App, Context, CursorStyle, Entity, FocusHandle, Focusable, MouseButton, PathPromptOptions,
    SharedString, Task, Window, div, prelude::*, px, rgb, svg, uniform_list,
};

use crate::actions::{
    CloseTab, Complete, CompleteLaravel, DecreaseFontSize, Dispatch, Find, FindInProject, FindNext,
    FindPrev, FindReferences, FormatDocument, GoToDefinition, GoToRoute, GoToSymbol, PushToRemote,
    IncreaseFontSize, QuickFix, RenameSymbol,
    NavigateBack, NavigateForward, NewFile, NewTerminal, OpenFolder, OpenSettings, Replace,
    RerunFailedTests, ResetFontSize, RunTests, RunTestsInFile, Save, ShowGitLog, SwitchBranch,
    ToggleCommandPalette,
    ToggleHiddenFiles, ToggleQuickOpen, ToggleTerminal, ToggleTestPanel, ToggleTheme, context,
    dispatch_for,
};
use crate::completion::{
    CompletionEvent, CompletionItem, CompletionPopup, CompletionSource, CompletionTrigger,
    word_before,
};
use crate::context_menu::{self, MenuAction, Overlay, OverlayEvent};
use crate::editor::{Document, EditorEvent, EditorView, search_project};
use crate::file_cache;
use crate::find_bar::{FindBar, FindEvent, Status};
use crate::fonts::Fonts;
use crate::git_panel::{DiffRenderer, GitEvent, GitPanel, PanelState, render_diff};
use crate::icons;
use crate::lsp_session::{LSP_POLL_INTERVAL, Lsp, LspState};
use crate::palette::{Palette, PaletteEvent, PaletteMode};
use crate::perf::FrameTimer;
use crate::search_panel::{SearchPanel, SearchPanelEvent, SearchState};
use crate::settings_panel::{SettingsPanel, SettingsPanelEvent};
use crate::terminal_view::{TerminalView, TerminalViewEvent};
use crate::test_view::{RunState, TestView};
use crate::theme::{Metrics, Theme, Themed};

/// Which panel the sidebar is showing.
///
/// Three variants because three panels are enabled; the other four in the activity bar are
/// still disabled and still have nothing to switch to. An enum rather than a bool so the
/// next one is a compile error at the match rather than an inverted flag — which is what
/// #80 got for free when it added `Search` to the enum #64 had already introduced.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
enum Sidebar {
    #[default]
    Explorer,
    Search,
    Git,
    Database,
    Docker,
}

/// An open tab.
struct Tab {
    path: Option<PathBuf>,
    editor: Entity<EditorView>,
}

/// What a tree row's drag carries: enough to move the entry when it lands on a
/// directory. A plain value, not an entity — gpui clones it into the drag state.
#[derive(Clone)]
struct DraggedTreeEntry {
    path: PathBuf,
}

/// What a tab's drag carries: its index at drag start, resolved against the tabs vec
/// at drop. Stale-index safety is the drop handler's bounds check, not a lookup — the
/// strip cannot reorder itself mid-drag.
#[derive(Clone, Copy)]
struct DraggedTab {
    index: usize,
}

/// The kinds of background work the workspace runs, one slot each.
///
/// The slot is the unit of cancellation: starting a second folder load supersedes the
/// first, and starting a save supersedes an earlier save of the same buffer. Work of a
/// *different* kind is unrelated and must be left alone.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
enum Job {
    OpenFolder,
    OpenFile,
    Save,
    QuickOpenIndex,
    RouteIndex,
    /// The autosave debounce timer (#25). Its own slot so each keystroke supersedes the
    /// pending save — the file is written once the user pauses, not on every key.
    AutosaveDebounce,
    /// Reading the project database's schema for the sidebar (#65). Its own slot:
    /// clicking Database twice must supersede, not race.
    DbSchema,
    /// Reading the Laravel log for the panel (#25). Same superseding reasoning.
    LogRead,
    /// Asking docker compose for its services (#25). Own slot, same reasoning.
    DockerPs,
    /// The self-updater's download-and-swap pipeline. Its own slot so opening a file
    /// mid-download cannot cancel an update half-way through replacing the app.
    UpdateInstall,
    /// Asking the project's artisan for its command list (#23). Its own slot: the palette
    /// that consumes it may be swapped to another mode while artisan is still answering,
    /// and the swap must cancel the ask rather than race it.
    ArtisanList,
    ClosePrompt,
    /// A create, rename or delete started from the tree's context menu (#126).
    ///
    /// One slot: these are all started by a click on a modal overlay, so two cannot be in
    /// flight at once, and a second one superseding the first is the right behaviour anyway.
    FileOperation,
    /// A git stage/unstage or commit (#64). One slot: they are click-driven and serial.
    GitWrite,
    /// A find-in-project sweep, and the debounce timer in front of it (#80).
    ///
    /// **One slot for both on purpose.** The timer and the search it starts are the same
    /// piece of work seen at two moments, and a keystroke has to supersede whichever of the
    /// two is currently live. Two slots would let a keystroke cancel a pending timer while a
    /// search started by the *previous* timer carried on and landed with a stale query — the
    /// exact "queueing instead of cancelling" ADR-0007 rules out. The [`CancelFlag`] handles
    /// the half a dropped task cannot: dropping the task stops us awaiting, not the blocking
    /// walk behind it.
    ProjectSearch,
    /// Resolving a `route()`/`config()`/`view()`/component ⌘click to a file (#83).
    ///
    /// Its own slot, not `LspQuery`'s: a Laravel jump and a language-server jump never race
    /// — `go_to_definition_at_cursor` only asks the server when Laravel declined — and
    /// sharing the slot would mean a Blade ⌘click cancelled a definition lookup that is
    /// still the answer to a different question the user asked.
    LaravelTarget,
    /// One slot for every language-server *query* — definition, references, symbols.
    ///
    /// Shared deliberately. These are all "answer a question about where the cursor is",
    /// and asking a new one means the previous answer is no longer wanted: F12 then ⇧F12
    /// must abandon the definition lookup, not race it. That is the cancellation ADR-0007
    /// asks for, and a slot per request kind would instead let three stale answers land.
    LspQuery,
    /// Starting a language server, and then the loop that drains its notifications. One
    /// slot for both because they are strictly sequential — the poll loop only exists once
    /// a start succeeded, and a new start must supersede whatever the old server was doing.
    Lsp,
    /// Reading `git status` for the source control panel (#64).
    ///
    /// Its own slot because a refresh is triggered by focus and by save, and those can land
    /// close together — ⌘S while the window is regaining focus. Superseding is exactly
    /// right there: the second read sees everything the first would have, so the first is
    /// waste the moment the second starts.
    GitStatus,
    /// Reading one file's diff. Separate from `GitStatus` because clicking a row must not
    /// cancel the status refresh that is repopulating the list underneath it — they answer
    /// different questions and both answers are wanted.
    GitDiff,
    /// A test run (#25).
    ///
    /// Its own slot, and emphatically not shared with anything else: a suite takes minutes,
    /// and every other job here finishes in milliseconds. Sharing would mean a ⌘S half way
    /// through a run cancelled it — the precise failure this enum was split up to stop.
    ///
    /// Starting a second run *does* supersede the first, which is what the slot is for: two
    /// concurrent suites would fight over the same database and report interleaved results.
    TestRun,
    /// Waiting for a completion the language server is computing (#61).
    ///
    /// Its own slot rather than `LspQuery`'s, for the reason `completion_query` documents:
    /// completion supersedes *completion*, and sharing would make a keystroke in the popup
    /// cancel a find-references sweep the user is still waiting on.
    Completion,
    /// The popup's route-name lookup (#61).
    ///
    /// Deliberately *not* `RouteIndex`, which the route palette uses. Sharing was justified
    /// by "only one of the two can be open at a time" — a claim about UI state that nothing
    /// enforces, and `JobSlots::start` replaces an occupied slot by assignment, so the loser
    /// is cancelled silently. The popup would then never receive its items while the LSP
    /// side had already called `mark_loaded`, settling on "No completions": a false claim,
    /// which is the exact thing `loaded` was introduced to prevent.
    CompletionRoutes,
    /// The Laravel-column source (#22), its own slot for `CompletionRoutes`'s reason.
    CompletionColumns,
}

/// One palette row for a route: `GET       /users/{user}  users.show`.
///
/// The whole point of `Resolved<T>` is that this cannot quietly turn "we could not read
/// this" into something that looks like an answer. An unresolved URI renders as the
/// expression that defeated the parser, in angle brackets — `<$legacyUri>` tells the reader
/// both that it is dynamic *and* what to go look at, where an empty cell or a guessed path
/// would be a lie they might act on (RISKS.md #4).
fn route_label(route: &Route) -> String {
    fn show(value: &Resolved<String>) -> String {
        match value {
            Resolved::Known(text) => text.clone(),
            Resolved::Unknown(source) => format!("<{source}>"),
        }
    }

    // Spelled out rather than `{:?}`: Debug renders the data-carrying variants as
    // `Resource("GET")` and `Match(["PUT", "PATCH"])`, which leaks Rust syntax into the UI
    // and blows past the column width that keeps the list scannable.
    let method = match &route.method {
        HttpMethod::Match(verbs) => verbs.join("|"),
        // A resource entry maps to one verb; which one is all the reader needs.
        HttpMethod::Resource(verb) => (*verb).to_string(),
        other => format!("{other:?}").to_uppercase(),
    };
    let mut label = format!("{method:<8}{}", show(&route.uri));
    // A route that never called ->name() gets no name column at all; one that called it with
    // an expression we could not read gets the expression. Those are different facts and the
    // row should not flatten them together.
    if let Some(name) = &route.name {
        label.push_str("  ");
        label.push_str(&show(name));
    }
    label
}

/// How long a navigation waits for the server before giving up.
///
/// Generous on purpose: a cold Intelephense indexing `vendor/` genuinely takes seconds to
/// answer its first question, and a timeout tuned for a warm server would make the feature
/// look broken exactly when the user first tries it. The window stays responsive throughout
/// — see [`WorkspaceView::poll_query`] — so the only cost of waiting is the wait.
const NAVIGATION_TIMEOUT: Duration = Duration::from_secs(30);

/// How often a pending navigation is checked for an answer.
///
/// Fast enough that a warm server's reply feels immediate, slow enough that a cold one does
/// not cost 30 seconds of busy polling. Only one navigation is ever in flight.
const NAVIGATION_POLL_INTERVAL: Duration = Duration::from_millis(16);

/// How many jumps Back can retrace.
///
/// A bound rather than an unbounded `Vec`, because this grows for the whole life of the
/// session and nothing else ever trims it. Fifty is far past what anyone retraces by hand.
const MAX_HISTORY: usize = 50;

/// How long a keystroke in the project-search field waits before searching (#80).
///
/// **Derived from the measurement, not picked round.** `editor::project_search` records a
/// full sweep at **7.2 ms** on crm-livewire-v3 (279 files) and 4.3 ms on this repo, of which
/// 2.9 ms and 1.4 ms are the directory walk. Those are the numbers this interval has to
/// clear, and the reasoning runs in both directions:
///
/// - **Why debounce at all**, when the search is already cancellable and already off the UI
///   thread? Because cancellation stops waste *accumulating*, not *starting*. Sustained
///   typing is roughly 8 keystrokes per second — 125 ms apart — so an undebounced field
///   starts a fresh walk of the whole project every 125 ms and cancels it 125 ms later,
///   having read a few hundred files for nothing. On a project several times the size of
///   the one measured, the searches never finish and the user sees only "Searching…".
/// - **Why not longer.** The whole search is 7 ms. An interval much past a typing pause
///   would mean the wait, not the work, is what the user is watching — the panel would feel
///   slower than the thing it is protecting. 250 ms is a hair above the ~200 ms gap between
///   words that separates "still typing" from "stopped", which is the signal being detected.
/// - **Why not shorter.** At 100 ms a moderate typist still triggers on gaps between
///   keystrokes rather than between words, which is most of the cost with none of the
///   benefit.
///
/// Return and the three option toggles skip it entirely — those are statements that the
/// query is finished, and making them wait would be the control ignoring an explicit act.
const SEARCH_DEBOUNCE: Duration = Duration::from_millis(250);

/// How long the editor waits after the last keystroke before autosaving (#25).
///
/// The owner wanted "a cada alteração já salva" — effectively immediate. A short 150ms
/// debounce is imperceptible yet still groups a fast burst of keystrokes into one write
/// rather than one write per character (which on a large file is real disk churn and a
/// git-status refresh per key). So it *feels* like save-on-change without hammering the
/// disk. The window-blur save stays as the backstop for leaving mid-burst.
const AUTOSAVE_DEBOUNCE: Duration = Duration::from_millis(150);

/// Where the cursor has been, so Back and Forward can retrace it.
///
/// A cursor stack, not a browser history: `back` pushes where you *are* onto the forward
/// stack as it pops where you were, which is what makes ⌃- then ⌃⇧- a round trip.
///
/// Only *jumps* are recorded — a go-to-definition, a reference, a symbol. Ordinary cursor
/// movement is not, because a Back that stepped through every arrow key press would be
/// useless for the thing it exists to do: getting out of a definition you jumped into.
#[derive(Default)]
struct JumpHistory {
    back: Vec<(PathBuf, Point)>,
    forward: Vec<(PathBuf, Point)>,
}

impl JumpHistory {
    /// Records a place being jumped *away from*.
    fn push(&mut self, location: (PathBuf, Point)) {
        self.back.push(location);
        if self.back.len() > MAX_HISTORY {
            self.back.remove(0);
        }
        // A new jump abandons the forward trail, exactly as a browser does: the places you
        // could have gone forward to are no longer on the path you actually took.
        self.forward.clear();
    }

    /// Pops the last place jumped from, given where the cursor is now.
    ///
    /// Entries whose file has since been deleted or moved are dropped rather than returned.
    /// The alternative is worse than it sounds: `open_path_at` loads asynchronously and
    /// cannot report failure back here, so returning a dead path would leave the cursor
    /// where it is while the history believed the jump happened — every later Back and
    /// Forward then off by one, with nothing on screen to explain why.
    fn back(&mut self, here: (PathBuf, Point)) -> Option<(PathBuf, Point)> {
        let previous = Self::pop_reachable(&mut self.back)?;
        self.forward.push(here);
        Some(previous)
    }

    /// Undoes a [`JumpHistory::back`].
    fn forward(&mut self, here: (PathBuf, Point)) -> Option<(PathBuf, Point)> {
        let next = Self::pop_reachable(&mut self.forward)?;
        self.back.push(here);
        Some(next)
    }

    /// Pops the newest entry that still names a file on disk, discarding any that do not.
    fn pop_reachable(stack: &mut Vec<(PathBuf, Point)>) -> Option<(PathBuf, Point)> {
        while let Some(entry) = stack.pop() {
            if entry.0.exists() {
                return Some(entry);
            }
        }
        None
    }
}

/// The first place a go-to-definition response points at.
///
/// The response has three shapes and servers use all of them. Taking the first is what an
/// F12 means: jump, do not ask. A symbol with several definitions — an interface and its
/// implementations — is what ⇧F12 is for, and offering a picker on every F12 would slow the
/// common case down to serve the rare one.
fn first_location(response: &GotoDefinitionResponse) -> Option<(PathBuf, u32, u32)> {
    let (uri, range) = match response {
        GotoDefinitionResponse::Scalar(location) => (&location.uri, location.range),
        GotoDefinitionResponse::Array(locations) => {
            let first = locations.first()?;
            (&first.uri, first.range)
        }
        // A link carries the *target selection* range, which is the identifier itself
        // rather than the whole declaration — the better landing spot when it is offered.
        GotoDefinitionResponse::Link(links) => {
            let first = links.first()?;
            (&first.target_uri, first.target_selection_range)
        }
    };

    let path = elle_lsp::uri_to_path(uri).ok()?;
    // The raw LSP coordinates, deliberately unconverted: the character is a UTF-16 offset
    // and a `Point` column is a byte offset, and converting needs the target file's text —
    // which this function does not have and, for a file not yet open, nobody has until the
    // load lands. `Target::Lsp` carries them to the moment a `Document` exists, where
    // `point_from_lsp` does it properly. This used to land at column 0 with the honest
    // note that the buffer was out of reach; the owner read that as "quase" — right line,
    // wrong place — which next to every other IDE is a defect, not a behaviour.
    Some((path, range.start.line, range.start.character))
}

/// Whether a file can hold Laravel references, and if so whether to read it as Blade.
///
/// `Some(true)` is Blade, `Some(false)` is PHP, `None` is neither and no Laravel feature
/// should look at it. The distinction matters because the two are read by completely
/// different machinery — a tree for PHP, a scanner for Blade (ADR-0006) — and handing a
/// template to the PHP reader gets a single `text` node and no references at all.
///
/// A free function so the gate is one decision shared by ⌘click and completion rather than
/// two `matches!` that can drift, and so it is testable without a window.
fn laravel_dialect(path: &std::path::Path) -> Option<bool> {
    match elle_syntax::language_for_path(path) {
        elle_syntax::Language::Blade => Some(true),
        elle_syntax::Language::Php => Some(false),
        _ => None,
    }
}

/// A palette id that carries a place in a file: `path:row`, row 0-based.
///
/// The palette's contract is one string per row, and widening it to a typed payload would
/// mean the palette knowing what its rows mean — which is the one thing it is arranged not
/// to know. Encoding the row into the id keeps the widget generic.
///
/// Appended rather than prepended so the common `path` prefix still matches when the user
/// types a directory: quick open filters on the id as well as the label.
fn target_id(path: &std::path::Path, row: usize) -> String {
    format!("{}:{row}", path.display())
}

/// Splits a [`target_id`] back into a path and a row.
///
/// A plain path with no suffix is not an error — quick open's ids are bare paths, and both
/// kinds of row arrive at the same confirm handler. Windows drive letters are not a concern
/// here (macOS only, ADR-0004), but the parse still requires the suffix to be *entirely*
/// digits, so a file called `notes:draft.php` is read as a path rather than a bad row.
fn split_target_id(id: &str) -> (PathBuf, Option<Point>) {
    match id.rsplit_once(':') {
        Some((path, row)) if !row.is_empty() && row.bytes().all(|b| b.is_ascii_digit()) => {
            match row.parse::<usize>() {
                Ok(row) => (PathBuf::from(path), Some(Point::new(row, 0))),
                // Only an overflowing row reaches this, and a line number that large is a
                // corrupt id, not a place to jump to.
                Err(_) => (PathBuf::from(id), None),
            }
        }
        _ => (PathBuf::from(id), None),
    }
}

/// Cleans a git error for the one-line status bar.
///
/// git's CLI writes multi-line stderr, and `anyhow`'s `{err:#}` flattens a chain into
/// `outer: inner: innermost` — either way the raw string is a paragraph where the status bar
/// has one line. The owner asked for these "formatted better"; the shape that helps is: show
/// the *first meaningful* line, and drop git's own `error:`/`fatal:` prefix noise, which
/// repeats what the user already knows (they ran a git action) and buries the actual sentence
/// behind a category label.
///
/// What is deliberately **kept**: everything after the first meaningful line's prefix, in
/// full. A pre-commit/pre-push hook's rejection and a remote's `! [rejected]` reason are the
/// message the user needs to act on — those are FOR them, the same reason `run_git_write`
/// surfaces hook stderr verbatim. This trims labels, never content.
fn clean_git_error(raw: &str) -> String {
    // The first line that says something. git prints blank lines and bare `hint:`/`warning:`
    // scaffolding around the real reason; the first line carrying an actual message is the
    // one to show. Fall back to the whole trimmed string if nothing stands out.
    let first_meaningful = raw
        .lines()
        .map(str::trim)
        .find(|line| {
            !line.is_empty()
                && !line.eq_ignore_ascii_case("error")
                && !line.eq_ignore_ascii_case("fatal")
        })
        .unwrap_or_else(|| raw.trim());

    // Strip a single leading category label. `fatal:` and `error:` name a severity the status
    // bar already implies; stripping one leaves the sentence. Only one is stripped, so a
    // message that legitimately contains "error: " later in its text keeps it.
    let cleaned = strip_git_prefix(first_meaningful);

    // Empty only if the whole error was blank lines and bare labels — then the raw string,
    // trimmed, is still better than an empty status.
    if cleaned.is_empty() { raw.trim().to_string() } else { cleaned.to_string() }
}

/// Drops one leading `fatal:`/`error:`/`git:` label and the space after it, case-insensitively.
fn strip_git_prefix(line: &str) -> &str {
    for prefix in ["fatal:", "error:", "git:"] {
        if line.len() >= prefix.len() && line[..prefix.len()].eq_ignore_ascii_case(prefix) {
            return line[prefix.len()..].trim_start();
        }
    }
    line
}

/// One in-flight [`Task`] per [`Job`].
///
/// Dropping a gpui `Task` cancels it: `async_task::Task::drop` calls `set_canceled`, so a
/// future that has not been polled to completion is dropped where it stands and one that
/// was never scheduled never runs at all. That is the intended cancellation mechanism
/// (ADR-0007) — but it only means what it should if the task being dropped is the task the
/// new work supersedes.
///
/// A single shared slot made every operation cancel every other one: ⌘S then ⌘O dropped
/// the save (either the write never started, or it finished and the buffer was never
/// marked clean); ⌘O on a large project then ⌘S dropped the folder load so the tree never
/// appeared; a quick-open walk was dropped by any file open, leaving its blocking walk
/// running with nothing left to consume it. Keying by job is what makes "a new request
/// drops the old one" true of the *same* request rather than of whatever ran last.
///
/// Generic over the handle so the eviction rule — the part that can actually be wrong —
/// is testable with a payload whose drops are observable. `Task<()>` is not: gpui gives no
/// way to ask a task whether it was cancelled.
struct JobSlots<T> {
    slots: Vec<(Job, T)>,
}

/// The workspace's slots, holding real gpui tasks.
type Jobs = JobSlots<Task<()>>;

impl<T> Default for JobSlots<T> {
    fn default() -> Self {
        Self { slots: Vec::new() }
    }
}

impl<T> JobSlots<T> {
    /// Starts `task` as the current work for `job`, cancelling only that job's predecessor.
    fn start(&mut self, job: Job, task: T) {
        match self.slots.iter_mut().find(|(slot, _)| *slot == job) {
            Some(entry) => entry.1 = task,
            None => self.slots.push((job, task)),
        }
    }

    /// Drops the task for `job`, if any, cancelling it.
    fn cancel(&mut self, job: Job) {
        self.slots.retain(|(slot, _)| *slot != job);
    }
}

pub struct WorkspaceView {
    focus_handle: FocusHandle,
    registry: Arc<CommandRegistry>,
    tree: Option<FileTree>,
    /// Scrolls the file tree so a revealed file is on screen (the mira button, #71 cousin).
    tree_scroll: gpui::UniformListScrollHandle,
    /// Scrolls the tab strip so the active tab is visible — activating a tab from the
    /// tree or palette with twenty open otherwise selects it off-screen (owner request).
    tab_scroll: gpui::ScrollHandle,
    /// Where the self-updater is in its lifecycle; the status bar renders from this.
    update_state: crate::update::UpdateState,
    /// The periodic release check, held so it lives exactly as long as the workspace.
    update_check: Option<gpui::Task<()>>,
    /// Keeps the FS watcher and its debounce task alive so the tree follows Finder,
    /// terminals and the app's own file operations without a manual refresh (owner
    /// request). The sender is held for tests, which have no real FSEvents to fire.
    /// Dropping the tuple (a new root, or shutdown) un-watches and ends the task.
    tree_watcher: Option<(notify::RecommendedWatcher, smol::channel::Sender<()>, gpui::Task<()>)>,
    tabs: Vec<Tab>,
    active_tab: usize,
    palette: Option<Entity<Palette>>,
    /// The settings panel (#100). `Some` while ⌘, has it open.
    settings_panel: Option<Entity<SettingsPanel>>,
    /// The file tree's context menu, name prompt or delete confirmation (#126).
    ///
    /// One slot for all three because they are steps of one interaction: the menu opens the
    /// prompt, the prompt replaces it. Two slots would let a stale menu outlive the dialog
    /// it opened, which is visible as a menu sitting behind a confirmation.
    overlay: Option<Entity<Overlay>>,
    /// The buffer version last sent to the language server, per open file (#59's follow-up).
    ///
    /// What keeps diagnostics honest between saves: `poll_lsp` compares each PHP tab's
    /// buffer version against this on every tick and resyncs the ones that moved, which
    /// gives per-keystroke sync a free 250ms debounce — the timer already existed. Without
    /// it, squiggles describe the buffer as it was at the last save (or the last completion
    /// request, since #125's resync), and a squiggle over code the user has since fixed
    /// reads as the editor being wrong about working code.
    ///
    /// Cleared when a server starts: a fresh server was just told everything via
    /// `sync_open_documents`, and stale entries from the previous server would suppress the
    /// first resync of every file.
    lsp_synced: std::collections::HashMap<PathBuf, elle_text::Version>,
    /// What the overlay is about — the row that was right-clicked, and what the pending
    /// action is going to do to it.
    ///
    /// Held here rather than inside the overlay because the overlay is replaced between the
    /// menu and the prompt, and this has to survive that. A path, never a row index: the
    /// tree is rebuilt by saves and git polls, so an index is a different file by the time
    /// an await returns (the same reasoning `MenuAction` records).
    pending: Option<PendingFileAction>,
    /// The find/replace bar (#80). `Some` only while it is open.
    ///
    /// One bar for the window rather than one per tab, which is what VS Code does and
    /// what keeps ⌘F from resurrecting a stale query when you switch tabs. Switching tabs
    /// re-applies the query to the newly active document rather than closing the bar.
    find: Option<Entity<FindBar>>,
    /// The find-in-project panel (#80). Built on first use and kept thereafter.
    ///
    /// Not eagerly in [`WorkspaceView::new`] like `git` beside it, and the difference is
    /// forced rather than stylistic: a result click has to *open a file*, opening focuses
    /// the editor since #102, and focusing needs a `Window` — so the subscription must be
    /// `subscribe_in`, and `new` has no window to give it. The git panel's events never
    /// open anything, so plain `subscribe` suffices there.
    ///
    /// Kept once built, for the reason the git panel is: switching to Explorer and back
    /// must not throw away a results list that cost a project walk to produce. Selecting a
    /// different sidebar does cancel any search still *in flight* — the results you can see
    /// survive, the work you can no longer see does not.
    search_panel: Option<Entity<SearchPanel>>,
    /// Cancels the project search in flight. Separate from [`Job::ProjectSearch`] for the
    /// reason `quick_open_cancel` is separate from its slot: dropping a `Task` stops the
    /// await, not the blocking walk behind it.
    search_cancel: Option<CancelFlag>,
    /// The bottom terminal panel. `Some` only while it is open — a panel that is closed is
    /// absent, not hidden, so its poll timer and its shells stop existing with it.
    terminal: Option<Entity<TerminalView>>,
    /// The bottom test panel (#25). `Some` only while it is open.
    ///
    /// Unlike the terminal, closing this does *not* stop the work: a run in flight keeps
    /// going and its results are still there when the panel is reopened, because a suite
    /// takes minutes and closing the panel to read some code is not a request to abandon
    /// it. Cancelling is a separate, explicit thing — [`Job::TestRun`] plus the flag below.
    tests: Option<Entity<TestView>>,
    /// The Laravel log panel (#25). Same lifecycle as the terminal: an entity while
    /// open, dropped when closed, re-read on refocus while up.
    logs: Option<Entity<crate::log_view::LogView>>,
    /// Cancels an in-flight test run. Separate from the task slot for the usual reason
    /// (ADR-0007): dropping the `Task` stops the await, not the PHP process behind it.
    test_cancel: Option<TestCancelFlag>,
    /// Transient message for the status bar (a failed save, mostly).
    status: Option<SharedString>,
    /// In-flight background work, one slot per [`Job`]. Held rather than detached so a new
    /// request of the same kind drops the old one, which is how ADR-0007's cancellation
    /// actually happens — see [`JobSlots`] for why a single shared slot was wrong.
    jobs: Jobs,
    /// Frame pacing, measured at the window root so it sees every repaint.
    frames: FrameTimer,
    /// Cancels an in-flight quick-open walk. Separate from the task slot because dropping a
    /// Task stops the await, not the blocking walk behind it.
    quick_open_cancel: Option<CancelFlag>,
    /// The language server for the open project, and its diagnostics.
    ///
    /// Owned by the workspace rather than by each editor because there is one server per
    /// *project*, not per file, and because it is the workspace that knows when a folder
    /// was opened or closed. Dropping the workspace drops this, which shuts the server
    /// down — that is what keeps a quit from leaving an orphan behind.
    lsp: Lsp,
    /// Where the cursor has been, for Back and Forward.
    history: JumpHistory,
    /// The navigation request currently in flight, if any.
    ///
    /// Held separately from [`Job::LspQuery`] because dropping the task and cancelling the
    /// request are two different things and only the first happens for free. A dropped task
    /// stops *us* waiting; the server carries on computing an answer nobody will read, and
    /// the pending entry it was inserted into stays in the connection's map for good. This
    /// is the handle that lets a superseding navigation say `$/cancelRequest` and reclaim
    /// both — ADR-0007's "cancellation, not queueing", which needs the id to mean anything.
    in_flight_query: Option<elle_lsp::RequestId>,
    /// Where a rename prompt was opened from: the document and byte offset the typed
    /// new name will apply to. Captured at prompt-open because the cursor may move
    /// under the overlay; taken (not read) at confirm so a dismissed prompt leaves
    /// nothing armed.
    pending_rename: Option<(elle_lsp::lsp_types::Uri, usize)>,
    /// The edits behind the open quick-fix palette, index-paired with its rows.
    pending_code_actions: Vec<elle_lsp::lsp_types::WorkspaceEdit>,
    /// The schema panel's state (#65): `None` before the first load, then the tables or
    /// the honest failure line. Kept across sidebar switches like git's panel — a
    /// re-read is a refocus or a re-click, not every switch.
    db_schema: Option<std::result::Result<Vec<elle_db::TableInfo>, String>>,
    /// The Docker panel's state (#25): `None` before first entry, then services or the
    /// daemon's own words about why not.
    docker_services: Option<std::result::Result<Vec<(String, bool)>, String>>,
    /// Which schema tables show their columns expanded (#65). Empty = all collapsed,
    /// which is the clean default the owner asked for — a list of table names, columns
    /// on demand. Clicking a table name toggles it here.
    db_expanded: std::collections::HashSet<String>,
    /// The table whose rows fill the editor area (#65), like the git diff does for a
    /// selected file: not a tab (nothing to edit or close), just a read-only view that
    /// shows while a table is picked and vanishes when the sidebar leaves Database.
    db_table: Option<(String, std::result::Result<elle_db::TablePage, String>)>,
    /// The cell being edited in the DB grid (#65): `(row index, column index, buffer)`.
    /// One cell at a time — click to edit, Enter to write it by rowid, Esc to cancel.
    db_editing: Option<(usize, usize, String)>,
    /// The source control panel (#64), and whether it is the visible sidebar.
    ///
    /// Always constructed rather than `Option`, unlike the terminal and the find bar: those
    /// own a subprocess and a query, so a closed one should not exist. This owns a status
    /// list and nothing else, and keeping it alive means switching back to it does not
    /// re-read the repository — which matters because the refresh is event-driven and a
    /// freshly built panel would have no event to wait for.
    git: Entity<GitPanel>,
    /// Which sidebar the activity bar has selected. Explorer until someone clicks Git.
    sidebar: Sidebar,
    /// Cancels an in-flight status walk, for the same reason `quick_open_cancel` exists:
    /// dropping the Task stops the await, not the libgit2 walk behind it (ADR-0007).
    git_cancel: Option<CancelFlag>,
    /// The parsed diff for the selected file, held here rather than in the panel because
    /// building it is blocking work the workspace owns.
    git_diff: Option<DiffRenderer>,
    /// Holds the window-activation observer that drives the focus refresh (#64).
    ///
    /// Registered on the first render rather than in `new`, because
    /// `observe_window_activation` needs a `&mut Window` and `new` has none. Adding a
    /// `Window` parameter was the alternative and was rejected: it would touch all ten
    /// `WorkspaceView::new` call sites in `render_tests.rs`, and two sibling agents are
    /// editing that file right now — a signature change there is a merge conflict bought
    /// for nothing. A `Subscription` dropped is a subscription cancelled, so holding it
    /// here is also what keeps the observer alive for exactly as long as the workspace.
    window_activation: Option<gpui::Subscription>,
    /// The completion popup, while one is open (#61).
    completion: Option<Entity<CompletionPopup>>,
    /// The buffer offset the open popup will overwrite from, when an item is accepted.
    ///
    /// What gets replaced is `word_start..cursor`, so this is the *start of the word*, not
    /// the cursor: typing narrows the list and moves the cursor, and the accepted item has
    /// to overwrite everything typed since the popup opened rather than being appended to
    /// it — otherwise typing `str` and accepting `strlen` produces `strstrlen`.
    ///
    /// Held rather than recomputed on accept because the buffer is the thing being written
    /// to, and recomputing would re-read a document that may have moved underneath. It is
    /// still re-validated at the point of the edit, since holding it does not make it true.
    ///
    /// Paired with the editor it is an offset *into*. An offset with no document attached is
    /// only meaningful while the active tab cannot change, and it can: clicking a tab sets
    /// `active_tab` directly. Resolving the target through `active_editor()` at accept time
    /// therefore wrote the completion into whichever file was frontmost *then*, at a byte
    /// offset that meant something in a different file — and the bounds check could not
    /// catch it, because a longer buffer accepts the offset happily.
    ///
    /// The handle is the same fix `close_tab_at` already uses for the same reason: indices
    /// shift and titles are not unique, so a tab is identified by its `Entity`.
    completion_word_start: Option<(Entity<EditorView>, usize)>,
    /// Keeps the popup's focus-out listener alive for exactly as long as the popup.
    ///
    /// A `Subscription` dropped is a subscription cancelled, so clearing this in
    /// `dismiss_completion` is what stops the listener — and holding it here is what keeps
    /// it registered while the popup is open.
    completion_focus_out: Option<gpui::Subscription>,
    /// The completion request currently in flight, so the next keystroke can cancel it.
    ///
    /// **A separate slot from [`Self::in_flight_query`] deliberately.** That one is shared
    /// by definition, references and symbols because those genuinely supersede each other —
    /// they are all "answer a question about the cursor" and a new one means the old answer
    /// is unwanted. Completion is not one of those: invoking completion while a
    /// find-references sweep runs must not abandon the sweep, and typing a character while
    /// the popup is open must cancel the *previous completion* and nothing else. Sharing the
    /// slot would have made every keystroke in the popup cancel an unrelated navigation.
    completion_query: Option<elle_lsp::RequestId>,
}

impl WorkspaceView {
    pub fn new(registry: Arc<CommandRegistry>, cx: &mut Context<Self>) -> Self {
        let git = cx.new(GitPanel::new);
        // Row clicks come back as events rather than as a callback holding a handle on the
        // workspace, the same shape the find bar and the palette use.
        cx.subscribe(&git, |this, _panel, event: &GitEvent, cx| this.on_git_event(event, cx))
            .detach();

        Self {
            focus_handle: cx.focus_handle(),
            registry,
            tree: None,
            tree_scroll: gpui::UniformListScrollHandle::new(),
            tab_scroll: gpui::ScrollHandle::new(),
            update_state: crate::update::UpdateState::default(),
            update_check: None,
            tree_watcher: None,
            tabs: Vec::new(),
            active_tab: 0,
            palette: None,
            overlay: None,
            pending: None,
            settings_panel: None,
            lsp_synced: std::collections::HashMap::new(),
            find: None,
            search_panel: None,
            search_cancel: None,
            terminal: None,
            tests: None,
            logs: None,
            test_cancel: None,
            status: None,
            jobs: Jobs::default(),
            frames: FrameTimer::new(),
            quick_open_cancel: None,
            lsp: Lsp::new(),
            history: JumpHistory::default(),
            in_flight_query: None,
            pending_rename: None,
            pending_code_actions: Vec::new(),
            db_schema: None,
            docker_services: None,
            db_table: None,
            db_editing: None,
            db_expanded: std::collections::HashSet::new(),
            git,
            sidebar: Sidebar::default(),
            git_cancel: None,
            git_diff: None,
            window_activation: None,
            completion: None,
            completion_word_start: None,
            completion_focus_out: None,
            completion_query: None,
        }
    }

    fn active_editor(&self) -> Option<&Entity<EditorView>> {
        self.tabs.get(self.active_tab).map(|tab| &tab.editor)
    }

    /// Window title: the open folder, plus a dirty marker.
    fn title(&self, cx: &App) -> String {
        let folder = self.tree.as_ref().map(|t| t.root_name());
        let file = self.tabs.get(self.active_tab).map(|tab| {
            let dirty = if tab.editor.read(cx).is_dirty() { "• " } else { "" };
            format!("{dirty}{}", tab.editor.read(cx).document.title())
        });
        match (file, folder) {
            (Some(file), Some(folder)) => format!("{file} — {folder}"),
            (Some(file), None) => file,
            (None, Some(folder)) => folder,
            (None, None) => "ellefuanti".to_string(),
        }
    }

    // --- folder opening ----------------------------------------------------------

    fn open_folder(&mut self, _: &OpenFolder, _w: &mut Window, cx: &mut Context<Self>) {
        // prompt_for_paths returns a oneshot::Receiver, not a Task: three nested layers
        // (channel cancelled / io error / user cancelled).
        let paths = cx.prompt_for_paths(PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: None,
        });

        let task = cx.spawn(async move |this, cx| {
            let Ok(Ok(Some(paths))) = paths.await else { return };
            let Some(root) = paths.into_iter().next() else { return };

            // FileTree::new does blocking IO, so it runs on the background pool. The UI
            // stays interactive while a large folder is read.
            let tree = cx.background_spawn(async move { FileTree::new(root) }).await;

            this.update(cx, |this, cx| {
                match tree {
                    Ok(tree) => this.adopt_tree(tree, cx),
                    Err(err) => this.status = Some(format!("{err:#}").into()),
                }
                cx.notify();
            })
            .ok();
        });
        self.jobs.start(Job::OpenFolder, task);
    }

    /// Makes `tree` the open project: the tree, the server, git, and the terminal's cwd.
    ///
    /// Split out of `open_folder` so the command line can reach it (`ellefuanti .`) without
    /// duplicating any of it. A second copy is how one door ends up starting a language
    /// server and the other one quietly not — which is the shape of #125, where `start_lsp`
    /// had exactly one caller and nobody noticed.
    fn adopt_tree(&mut self, tree: FileTree, cx: &mut Context<Self>) {
        let root = tree.root().to_path_buf();
        self.tree = Some(tree);
        self.status = None;
        self.start_tree_watcher(root, cx);
        // A run belongs to the project it was started in. Its results name files in the old
        // tree and its `--filter` names the old suite, so carrying either into a new project
        // would show verdicts about code the user is no longer looking at. The panel goes
        // with it, and is rebuilt against the new root when it is next opened — which is
        // also what re-runs detection for a project that may not have Pest.
        self.cancel_test_run();
        self.tests = None;
        // A new project gets a new server, pointed at the new root. The old one is dropped
        // by `set_root`, which kills its process.
        self.start_lsp(cx);
        // First of the three refresh triggers (#64). The other two are save and window
        // focus; there is no timer.
        self.refresh_git_status(cx);
        // The Laravel index (#21): models, columns with provenance, relationships.
        // Fire-and-forget on the background pool — a failed build is a debug line, not a
        // user problem: the index is a cache and every consumer must already survive its
        // absence (ADR-0008). Rebuilt again on every model/migration save, same helper.
        if let Some(root) = self.tree.as_ref().map(|tree| tree.root().to_path_buf()) {
            rebuild_laravel_index(root, cx);
        }
        // An open terminal points at wherever it was started, which before this was the
        // *previous* project — or nowhere, for a panel opened before any folder was.
        // Sessions already running keep their own directory: a shell has state and its own
        // `cd`, and moving it out from under someone mid-command would be worse than
        // leaving it. New sessions land in the new project.
        let root = self.tree.as_ref().map(|tree| tree.root().to_path_buf());
        if let Some(terminal) = self.terminal.as_ref() {
            terminal.update(cx, |terminal, _| terminal.set_cwd(root));
        }
    }

    /// Opens whatever the command line named: a folder as the project, a file in a tab.
    ///
    /// The blocking `FileTree::new` runs on the background pool, like every other open — a
    /// large project must not delay the first frame (ADR-0007).
    pub fn open_argument(&mut self, path: PathBuf, window: &mut Window, cx: &mut Context<Self>) {
        if path.is_file() {
            self.open_path(path, window, cx);
            return;
        }

        if !path.is_dir() {
            // Neither, so it does not exist. Said out loud: a mistyped path that opens an
            // empty window looks like the editor failed to start.
            self.status = Some(format!("{} does not exist", path.display()).into());
            cx.notify();
            return;
        }

        let task = cx.spawn(async move |this, cx| {
            let tree = cx.background_spawn(async move { FileTree::new(path) }).await;
            this.update(cx, |this, cx| {
                match tree {
                    Ok(tree) => this.adopt_tree(tree, cx),
                    Err(err) => this.status = Some(format!("{err:#}").into()),
                }
                cx.notify();
            })
            .ok();
        });
        self.jobs.start(Job::OpenFolder, task);
    }

    /// Opens a path the terminal reported ⌘-clicked, if it actually names a file (#70).
    ///
    /// Relative paths resolve against the project root — a stack trace inside a project
    /// prints paths relative to it, because that is where the command ran. A path that
    /// resolves to nothing is dropped silently rather than reported: the detector matches
    /// *shapes*, plain prose with a slash in it qualifies, and a status-bar error for
    /// every such click would blame the user for the heuristic's reach. Declining to open
    /// is the honest floor (RISKS.md #4) — nothing was claimed, so nothing is retracted.
    ///
    /// The line lands through `open_path_at`'s target, the same door every jump uses.
    /// One-based in the trace, zero-based in the editor, saturating so `file.php:0` —
    /// which some tools print — lands on the first line instead of underflowing.
    fn open_terminal_link(
        &mut self,
        path: PathBuf,
        line: Option<u32>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let resolved = if path.is_absolute() {
            path
        } else {
            let Some(root) = self.tree.as_ref().map(|tree| tree.root().to_path_buf()) else {
                return;
            };
            root.join(path)
        };

        if !resolved.is_file() {
            return;
        }

        let target = line.map(|line| Point { row: (line.saturating_sub(1)) as usize, column: 0 });
        self.open_path_at(resolved, target, window, cx);
    }

    // --- source control (#64) ------------------------------------------------------

    /// Registers the window-activation observer, once.
    ///
    /// The third refresh trigger, and the one that covers every change made *outside* this
    /// editor — a `git commit` in the terminal, a branch switch, a `git pull`. It works
    /// because to notice a stale panel you must first look at the window, and looking at
    /// the window is what raises this event.
    ///
    /// Idempotent by the `is_none` guard, so calling it from `render` costs one branch per
    /// frame after the first. `observe_window_activation` fires on deactivation too; the
    /// `is_window_active` check is what keeps it to one refresh per return rather than two
    /// per round trip.
    fn observe_window_focus(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.window_activation.is_some() {
            return;
        }
        self.window_activation = Some(cx.observe_window_activation(window, |this, window, cx| {
            if window.is_window_active() {
                this.window_became_active(cx);
            } else {
                // The blur half: autosave (#25 follow-up from the owner's first rename
                // session). Leaving the window is the moment work silently sitting in
                // a buffer starts to rot — and the moment every reference IDE saves.
                this.autosave_dirty_tabs(cx);
            }
        }));
    }

    /// Everything that refreshes when the user comes back to the window: git status
    /// (#64) and the Laravel index (#21). Both exist for the same reason — the changes
    /// made *outside* this editor (a commit in the terminal, an `artisan make:model`)
    /// have no other event to ride, and to notice staleness you must first look at the
    /// window, which is this event.
    fn window_became_active(&mut self, cx: &mut Context<Self>) {
        self.refresh_git_status(cx);
        if let Some(root) = self.tree.as_ref().map(|tree| tree.root().to_path_buf()) {
            rebuild_laravel_index(root, cx);
        }
        // A migration ran in the terminal: same to-notice-you-must-look reasoning, but
        // only while the panel is actually up — a hidden panel refreshing is idle cost.
        if self.sidebar == Sidebar::Database {
            self.load_db_schema(cx);
        }
        if self.sidebar == Sidebar::Docker {
            self.load_docker_services(cx);
        }
        // And the log: the error you came back to read may have just been written.
        self.refresh_log_panel(cx);
    }

    /// The focus trigger through the real handler, for tests — headless windows never
    /// see a real activation event.
    #[cfg(test)]
    pub fn window_became_active_for_test(&mut self, cx: &mut Context<Self>) {
        self.window_became_active(cx);
    }

    /// The blur half — what autosave rides.
    #[cfg(test)]
    pub fn window_lost_focus_for_test(&mut self, cx: &mut Context<Self>) {
        self.autosave_dirty_tabs(cx);
    }

    /// Cancels an in-flight status walk. Same shape as `cancel_quick_open_walk`.
    fn cancel_git_status(&mut self) {
        if let Some(cancel) = self.git_cancel.take() {
            cancel.cancel();
        }
    }

    /// Re-reads `git status` for the open folder.
    ///
    /// **Called on folder open, on window focus, and after a successful save — never on a
    /// timer.** See the module docs on [`crate::git_panel`] for why those three and not a
    /// poll: a timer would burn CPU on a panel nobody is looking at, and the perf gate now
    /// measures idle CPU, so it would show up as a regression rather than as a nicety.
    ///
    /// Runs even when the Git panel is not the visible sidebar. That is one `git status` on
    /// focus for a panel you cannot see, which sounds wasteful and is the cheaper of the two
    /// options: the alternative is a visible stall the first time you click Git on a large
    /// repository, and the status is what the activity bar would need anyway to show a
    /// change count. It is bounded work with no timer behind it.
    fn refresh_git_status(&mut self, cx: &mut Context<Self>) {
        let Some(root) = self.tree.as_ref().map(|tree| tree.root().to_path_buf()) else {
            self.git.update(cx, |panel, cx| panel.set_state(PanelState::NoFolder, cx));
            return;
        };

        self.cancel_git_status();
        let cancel = CancelFlag::new();
        self.git_cancel = Some(cancel.clone());

        // Say "Loading…" only when there is nothing to show yet. A refresh over an existing
        // list keeps the list up: blanking it on every ⌘S would make the panel flicker on
        // the event that fires most.
        self.git.update(cx, |panel, cx| {
            if matches!(panel.state(), PanelState::NoFolder) {
                panel.set_state(PanelState::Loading, cx);
            }
        });

        let panel = self.git.clone();
        let task = cx.spawn(async move |this, cx| {
            let walk_cancel = cancel.clone();
            // libgit2 blocks, so it goes to the background pool (ADR-0007). `status`
            // returns `None` for a folder that is not a repository, which is the common
            // case and not an error — no dialog, no log line.
            let result = cx
                .background_spawn(
                    async move { elle_git::status(&root, &|| walk_cancel.is_cancelled()) },
                )
                .await;

            // A superseded walk's answer is stale by definition; dropping it silently is
            // the point of the flag.
            if cancel.is_cancelled() {
                return;
            }

            let state = match result {
                Some(status) => PanelState::Repo(status),
                None => PanelState::NotARepo,
            };

            panel.update(cx, |panel, cx| panel.set_state(state, cx)).ok();
            this.update(cx, |this, cx| {
                // A file that stopped having changes takes its diff with it.
                if this.git.read(cx).selected().is_none() {
                    this.git_diff = None;
                }
                cx.notify();
            })
            .ok();
        });
        self.jobs.start(Job::GitStatus, task);
    }

    /// Reads and parses the diff for one file.
    ///
    /// Both halves run on the background pool: libgit2 produces the hunks and tree-sitter
    /// highlights both sides of them, and neither belongs on the UI thread. The panel is
    /// handed a finished `DiffRenderer` rather than parsing during render.
    fn load_git_diff(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        let Some(root) = self.tree.as_ref().map(|tree| tree.root().to_path_buf()) else { return };

        let panel = self.git.clone();
        let task = cx.spawn(async move |this, cx| {
            let (diff, renderer) = cx
                .background_spawn(async move {
                    let diff = elle_git::diff_file(&root, &path);
                    // Parsing here rather than on the main thread is the whole reason this
                    // pair is built together: `DiffRenderer::new` runs tree-sitter twice.
                    let renderer = diff.as_ref().map(DiffRenderer::new);
                    (diff, renderer)
                })
                .await;

            panel.update(cx, |panel, cx| panel.set_diff(diff, cx)).ok();
            this.update(cx, |this, cx| {
                this.git_diff = renderer;
                cx.notify();
            })
            .ok();
        });
        self.jobs.start(Job::GitDiff, task);
    }

    fn on_git_event(&mut self, event: &GitEvent, cx: &mut Context<Self>) {
        match event {
            GitEvent::DiffRequested { path } => self.load_git_diff(path.clone(), cx),
            GitEvent::StageRequested { path, stage } => {
                self.run_git_write(GitWrite::Stage { path: path.clone(), stage: *stage }, cx)
            }
            GitEvent::CommitRequested { message } => {
                self.run_git_write(GitWrite::Commit { message: message.clone() }, cx)
            }
        }
    }

    /// Runs one git write on the background executor and refreshes status after (#64).
    ///
    /// One funnel for both writes so the after-story cannot diverge: whatever happened,
    /// the panel re-reads reality rather than patching its own copy — the same
    /// state-follows-disk rule the tree's refresh established. A failure lands in the
    /// status bar verbatim, because for commit the interesting failures are the *user's
    /// own hooks* talking (`elle_git::commit` returns their stderr for exactly this).
    fn run_git_write(&mut self, write: GitWrite, cx: &mut Context<Self>) {
        let Some(root) = self.tree.as_ref().map(|tree| tree.root().to_path_buf()) else { return };

        let task = cx.spawn(async move |this, cx| {
            let done = cx
                .background_spawn(async move {
                    match write {
                        GitWrite::Stage { path, stage } => {
                            elle_git::stage(&root, &path, stage).map(|()| String::new())
                        }
                        GitWrite::Commit { message } => elle_git::commit(&root, &message),
                    }
                })
                .await;

            this.update(cx, |this, cx| {
                match done {
                    Ok(output) => {
                        // The commit box empties only on success; a hook's refusal must
                        // not eat the message the user typed.
                        this.git.update(cx, |panel, cx| panel.clear_commit_message(cx));
                        // Say what happened, on the status line. The owner read a silent
                        // commit as "só esconde" — git's own summary ("[main abc123] msg")
                        // is the proof the commit landed. A staged-only write (no output)
                        // stays quiet as before.
                        let summary = output.lines().next().unwrap_or("").trim();
                        if !summary.is_empty() {
                            this.status = Some(format!("✓ {summary}  ·  ⇧⌥P to push").into());
                        }
                    }
                    Err(err) => {
                        this.status = Some(clean_git_error(&format!("{err:#}")).into())
                    }
                }
                this.refresh_git_status(cx);
                cx.notify();
            })
            .ok();
        });
        self.jobs.start(Job::GitWrite, task);
    }

    fn toggle_hidden_files(
        &mut self,
        _: &ToggleHiddenFiles,
        _w: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(tree) = self.tree.as_mut() {
            let show = !tree.show_hidden();
            if let Err(err) = tree.set_show_hidden(show) {
                self.status = Some(format!("{err:#}").into());
            }
            cx.notify();
        }
    }

    /// Switches to the next theme: the five compiled in, then any loaded from disk.
    ///
    /// `refresh_windows` rather than `cx.notify()`: notify marks *this* entity dirty, and
    /// the editor, terminal and palette are sibling entities that would keep their old
    /// colours until something else happened to redraw them. A theme change is the one
    /// case where every window really is stale, which is what `refresh_windows` means.
    ///
    /// Disk themes join this cycle rather than getting a command of their own (#58): the
    /// keybinding already exists, and a theme nobody can reach is a theme that may as well
    /// not have loaded.
    fn toggle_theme(&mut self, _: &ToggleTheme, _w: &mut Window, cx: &mut Context<Self>) {
        let label = crate::themes::cycle(cx);
        self.status = Some(format!("Theme: {label}").into());
        cx.refresh_windows();
    }

    /// ⌘+ / ⌘- / ⌘0 (#49).
    ///
    /// Turned out to be cheap, which is why it is here: nothing caches a size. Every view
    /// reads `Fonts::get(cx)` inside its own `render`, so a new size plus the
    /// `refresh_windows` a theme switch already needed *is* the re-layout — gpui rebuilds
    /// the element tree and taffy measures it again. No re-layout plumbing was added.
    ///
    /// One step is 1px rather than a ratio. At the sizes anyone reads code at, 13→14 is the
    /// adjustment people actually want, and a 1.1x step gives 14.3 and a half-pixel
    /// baseline for nothing.
    fn zoom(&mut self, delta: Option<f32>, cx: &mut Context<Self>) {
        let size = crate::settings::adjust_font_size(delta, cx);
        self.status = Some(format!("Font size: {}px", f32::from(size)).into());
        cx.refresh_windows();
    }

    fn toggle_tree_entry(&mut self, index: usize, cx: &mut Context<Self>) {
        if let Some(tree) = self.tree.as_mut() {
            if let Err(err) = tree.toggle(index) {
                self.status = Some(format!("{err:#}").into());
            }
            cx.notify();
        }
    }

    /// The explorer header's "expand all" button.
    ///
    /// `expand_all` walks and reads the whole project, which is why it is a button and not
    /// something on any hot path (see the method's own note). Runs inline rather than on the
    /// background pool because it follows a deliberate click at human speed — the same
    /// choice `toggle` makes for a single directory, scaled up.
    fn expand_all_tree(&mut self, cx: &mut Context<Self>) {
        if let Some(tree) = self.tree.as_mut() {
            if let Err(err) = tree.expand_all() {
                self.status = Some(format!("{err:#}").into());
            }
            cx.notify();
        }
    }

    /// Reveals the active file in the tree — the "mira" button (owner request).
    ///
    /// Expands the file's ancestor folders and scrolls it into view, switching the
    /// sidebar to Explorer so there is a tree to reveal it in. Does nothing without an
    /// open file with a path (a scratch buffer has none), which is the honest no-op.
    fn reveal_active_file(&mut self, cx: &mut Context<Self>) {
        let Some(path) = self.tabs.get(self.active_tab).and_then(|tab| tab.path.clone()) else {
            return;
        };
        self.sidebar = Sidebar::Explorer;
        let Some(row) = self.tree.as_mut().and_then(|tree| tree.reveal(&path)) else {
            cx.notify();
            return;
        };
        self.tree_scroll.scroll_to_item(row, gpui::ScrollStrategy::Center);
        cx.notify();
    }

    /// The explorer header's "collapse all" button.
    fn collapse_all_tree(&mut self, cx: &mut Context<Self>) {
        if let Some(tree) = self.tree.as_mut() {
            if let Err(err) = tree.collapse_all() {
                self.status = Some(format!("{err:#}").into());
            }
            cx.notify();
        }
    }

    /// Scrolls the tab strip so the active tab is on screen. Called after every
    /// `active_tab` assignment — activation and visibility are one gesture; a tab
    /// selected off-screen looks like nothing happened.
    fn scroll_active_tab_into_view(&self) {
        self.tab_scroll.scroll_to_item(self.active_tab);
    }

    /// A tab dragged from `from` lands in slot `to`; the file the user was in stays
    /// active wherever its tab went (`active_after_reorder`'s contract, tested there).
    fn reorder_tab(&mut self, from: usize, to: usize, cx: &mut Context<Self>) {
        if from == to || from >= self.tabs.len() || to >= self.tabs.len() {
            return;
        }
        let tab = self.tabs.remove(from);
        self.tabs.insert(to, tab);
        self.active_tab = active_after_reorder(self.active_tab, from, to);
        self.scroll_active_tab_into_view();
        cx.notify();
    }

    /// The database header's "expand all": every table shows its columns — the bulk
    /// counterpart of clicking each name (#65), mirroring the explorer's pair.
    fn expand_all_db(&mut self, cx: &mut Context<Self>) {
        if let Some(Ok(tables)) = self.db_schema.as_ref() {
            self.db_expanded = tables.iter().map(|table| table.name.clone()).collect();
            cx.notify();
        }
    }

    /// Back to the clean list of table names.
    fn collapse_all_db(&mut self, cx: &mut Context<Self>) {
        self.db_expanded.clear();
        cx.notify();
    }

    // --- file opening ------------------------------------------------------------

    /// The active tab's editor handle, for render tests that need to inspect it.
    #[cfg(test)]
    pub fn active_editor_for_test(&self) -> Option<Entity<EditorView>> {
        self.active_editor().cloned()
    }

    /// Runs the action handlers a render test needs, which are otherwise private.
    ///
    /// Calling the real handlers rather than reimplementing them: a test that opened the
    /// terminal by assigning `self.terminal` would pass while the actual command was
    /// broken, which is the failure this whole issue is about.
    #[cfg(test)]
    pub fn toggle_terminal_for_test(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.toggle_terminal(&ToggleTerminal, window, cx);
    }

    /// The explicit invoke, through the real action handler, for the same reason (#61).
    #[cfg(test)]
    pub fn complete_for_test(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.complete(&Complete, window, cx);
    }

    #[cfg(test)]
    pub fn completion_for_test(&self) -> Option<Entity<CompletionPopup>> {
        self.completion.clone()
    }

    /// A character typed while the popup holds focus, through the real path.
    #[cfg(test)]
    pub fn completion_typed_for_test(
        &mut self,
        text: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.completion_typed(text, window, cx);
    }

    #[cfg(test)]
    pub fn accept_completion_for_test(
        &mut self,
        item: CompletionItem,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.accept_completion(item, window, cx);
    }

    #[cfg(test)]
    pub fn dismiss_completion_for_test(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.dismiss_completion(window, cx);
    }

    /// The status-bar message, which for a missing language server must stay `None` (#74).
    #[cfg(test)]
    pub fn status_for_test(&self) -> Option<SharedString> {
        self.status.clone()
    }

    /// ⌘W through the real handler — the path that has to take the popup with it.
    #[cfg(test)]
    pub fn close_tab_for_test(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.close_tab(&CloseTab, window, cx);
    }

    /// The ✕ button's path, which reaches `close_tab_at` without going through ⌘W.
    #[cfg(test)]
    pub fn close_tab_at_for_test(
        &mut self,
        index: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.close_tab_at(index, window, cx);
    }

    /// The terminal panel entity, for tests that assert where a command would land.
    #[cfg(test)]
    pub fn terminal_for_test(&self) -> Option<Entity<TerminalView>> {
        self.terminal.clone()
    }

    /// ⌘S through the real handler — the path that must rebuild the Laravel index (#21).
    #[cfg(test)]
    pub fn save_for_test(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.save(&Save, window, cx);
    }

    /// Arms the quick-fix palette's pending edits, for tests — the request half needs
    /// a live server; the apply half is what can corrupt a file and is what gets pinned.
    #[cfg(test)]
    pub fn set_pending_code_actions_for_test(
        &mut self,
        edits: Vec<elle_lsp::lsp_types::WorkspaceEdit>,
        _cx: &mut Context<Self>,
    ) {
        self.pending_code_actions = edits;
    }

    /// The activity-bar Docker click, through the real load path.
    #[cfg(test)]
    pub fn show_docker_panel_for_test(&mut self, cx: &mut Context<Self>) {
        self.load_docker_services(cx);
        self.sidebar = Sidebar::Docker;
        cx.notify();
    }

    /// Opens a table's rows and returns what the grid would show, for tests.
    #[cfg(test)]
    pub fn open_db_table_for_test(
        &mut self,
        table: &str,
        cx: &mut Context<Self>,
    ) {
        self.open_db_table(table.to_string(), cx);
    }

    #[cfg(test)]
    pub fn db_insert_row_for_test(&mut self, cx: &mut Context<Self>) {
        self.db_insert_row(cx);
    }

    #[cfg(test)]
    pub fn db_edit_flow_for_test(
        &mut self,
        row: usize,
        col: usize,
        typed: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.begin_db_edit(row, col, window, cx);
        // Replace the seeded value — "set this cell to `typed`".
        if let Some((_, _, buffer)) = &mut self.db_editing {
            buffer.clear();
        }
        for ch in typed.chars() {
            self.db_edit_typed(&ch.to_string(), cx);
        }
        self.db_edit_commit(cx);
    }

    #[cfg(test)]
    pub fn toggle_db_table_for_test(&mut self, table: &str, cx: &mut Context<Self>) {
        self.toggle_db_table(table.to_string(), cx);
    }

    #[cfg(test)]
    pub fn db_expanded_for_test(&self, table: &str) -> bool {
        self.db_expanded.contains(table)
    }

    #[cfg(test)]
    pub fn expand_all_db_for_test(&mut self, cx: &mut Context<Self>) {
        self.expand_all_db(cx);
    }

    #[cfg(test)]
    pub fn collapse_all_db_for_test(&mut self, cx: &mut Context<Self>) {
        self.collapse_all_db(cx);
    }

    #[cfg(test)]
    #[allow(clippy::type_complexity)] // a test observer; the shape is the panel's own
    pub fn db_table_for_test(
        &self,
    ) -> Option<(String, std::result::Result<Vec<Vec<String>>, String>)> {
        self.db_table.as_ref().map(|(name, result)| {
            (
                name.clone(),
                result.as_ref().map(|page| page.rows.clone()).map_err(|m| m.clone()),
            )
        })
    }

    #[cfg(test)]
    pub fn docker_services_for_test(
        &self,
    ) -> Option<std::result::Result<Vec<(String, bool)>, String>> {
        self.docker_services.clone()
    }

    /// The log toggle, through the real handler.
    #[cfg(test)]
    pub fn toggle_log_panel_for_test(&mut self, cx: &mut Context<Self>) {
        self.toggle_log_panel(cx);
    }

    #[cfg(test)]
    pub fn log_panel_for_test(&self) -> Option<Entity<crate::log_view::LogView>> {
        self.logs.clone()
    }

    /// The activity-bar Database click, through the real load path.
    #[cfg(test)]
    pub fn show_database_panel_for_test(&mut self, cx: &mut Context<Self>) {
        self.load_db_schema(cx);
        self.sidebar = Sidebar::Database;
        cx.notify();
    }

    /// What the schema panel would render: table names, or the failure line.
    #[cfg(test)]
    pub fn db_schema_for_test(&self) -> Option<std::result::Result<Vec<String>, String>> {
        self.db_schema.as_ref().map(|result| {
            result
                .as_ref()
                .map(|tables| tables.iter().map(|table| table.name.clone()).collect())
                .map_err(|message| message.clone())
        })
    }

    /// The rename applier, for tests — a real `WorkspaceEdit` needs a live server.
    #[cfg(test)]
    pub fn apply_workspace_edit_for_test(
        &mut self,
        edit: elle_lsp::lsp_types::WorkspaceEdit,
        cx: &mut Context<Self>,
    ) -> anyhow::Result<usize> {
        self.apply_workspace_edit(edit, cx)
    }

    /// The open palette entity, for tests that drive its input directly.
    #[cfg(test)]
    pub fn palette_for_test(&self) -> Option<Entity<Palette>> {
        self.palette.clone()
    }

    #[cfg(test)]
    pub fn toggle_palette_for_test(
        &mut self,
        mode: PaletteMode,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.toggle_palette(mode, window, cx);
    }

    /// Feeds the popup items as though a source had answered.
    ///
    /// The sources themselves are asynchronous and one of them is a language server, so a
    /// test that wanted real items would be testing Intelephense. What is worth pinning here
    /// is everything *after* the answer arrives — filtering, selection, and the insertion
    /// that a wrong replace-range would corrupt.
    #[cfg(test)]
    pub fn offer_completions_for_test(
        &mut self,
        items: Vec<CompletionItem>,
        cx: &mut Context<Self>,
    ) {
        if let Some(popup) = self.completion.clone() {
            popup.update(cx, |popup, cx| {
                popup.add_items(items, cx);
                popup.mark_loaded(cx);
            });
        }
    }

    /// Feeds the popup an answer the server called *incomplete*, as Intelephense does.
    ///
    /// Separate from [`Self::offer_completions_for_test`] rather than a boolean on it,
    /// because the two describe different server behaviour and a test naming which one it
    /// means is a test that says what it is about.
    #[cfg(test)]
    pub fn offer_incomplete_completions_for_test(
        &mut self,
        items: Vec<CompletionItem>,
        cx: &mut Context<Self>,
    ) {
        if let Some(popup) = self.completion.clone() {
            popup.update(cx, |popup, cx| {
                popup.set_incomplete(true);
                popup.replace_items(CompletionSource::Lsp, items, cx);
                popup.mark_loaded(cx);
            });
        }
    }

    /// A character typed into the *editor* with no popup open — the trigger path (#61).
    ///
    /// Goes through the real `editor_typed`, so the test exercises the decision about
    /// whether the character is a declared trigger rather than reimplementing it.
    #[cfg(test)]
    pub fn editor_typed_for_test(
        &mut self,
        text: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.editor_typed(text, window, cx);
    }

    /// Whether the server declared `text` a trigger, for a test that has no live server.
    #[cfg(test)]
    pub fn is_completion_trigger_for_test(&self, text: &str) -> bool {
        self.is_completion_trigger(text)
    }

    /// The trigger-opening rule as a pure function, which is the only way both of its inputs
    /// can be varied — see the test that explains why.
    #[cfg(test)]
    pub fn should_open_on_trigger_for_test(popup_is_open: bool, declared: bool) -> bool {
        Self::should_open_on_trigger(popup_is_open, declared)
    }

    /// Backspace while the popup holds focus, through the real path.
    #[cfg(test)]
    pub fn completion_backspace_for_test(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.completion_backspace(window, cx);
    }

    #[cfg(test)]
    pub fn toggle_command_palette_for_test(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.toggle_command_palette(&ToggleCommandPalette, window, cx);
    }

    /// Opens the test panel through the real handler (#25).
    #[cfg(test)]
    pub fn toggle_test_panel_for_test(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.toggle_test_panel(&ToggleTestPanel, window, cx);
    }

    /// Puts a finished run into the panel so a render test can paint results.
    ///
    /// Feeds real [`elle_test_runner::Event`]s through the same `push` the live run uses,
    /// rather than assigning a `Report` — a test that built the report directly would pass
    /// while the event folding was broken.
    #[cfg(test)]
    pub fn seed_test_results_for_test(
        &mut self,
        events: Vec<elle_test_runner::Event>,
        state: RunState,
        cx: &mut Context<Self>,
    ) {
        let Some(panel) = self.tests.clone() else { return };
        panel.update(cx, |panel, cx| {
            for event in events {
                panel.push(event, cx);
            }
            panel.finish(state, cx);
        });
    }

    #[cfg(test)]
    pub fn toggle_theme_for_test(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.toggle_theme(&ToggleTheme, window, cx);
    }

    /// ⌘F / ⌘⌥F through the real handler (#80).
    #[cfg(test)]
    pub fn find_for_test(&mut self, replacing: bool, window: &mut Window, cx: &mut Context<Self>) {
        if replacing {
            self.replace(&Replace, window, cx);
        } else {
            self.find(&Find, window, cx);
        }
    }

    #[cfg(test)]
    pub fn find_bar_for_test(&self) -> Option<Entity<FindBar>> {
        self.find.clone()
    }

    /// Escape, through the real handler.
    #[cfg(test)]
    pub fn dismiss_find_for_test(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.dismiss_find(window, cx);
    }

    /// Parks focus on the workspace root, the state a dismissed palette leaves behind.
    ///
    /// Focus is what #95 is about, so a test asserting an open *restores* it has to be able
    /// to take it away first — otherwise the tab's own open already focused the editor and
    /// the assertion holds regardless.
    #[cfg(test)]
    pub fn focus_root_for_test(&mut self, window: &mut Window, _cx: &mut Context<Self>) {
        window.focus(&self.focus_handle);
    }

    /// ⌘⇧F through the real handler (#80). Toggles, like the action does.
    #[cfg(test)]
    pub fn find_in_project_for_test(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.find_in_project(&FindInProject, window, cx);
    }

    #[cfg(test)]
    pub fn search_panel_for_test(&self) -> Option<Entity<SearchPanel>> {
        self.search_panel.clone()
    }

    /// Whether the sidebar is currently showing the search panel.
    ///
    /// Not `search_panel_for_test().is_some()`, and the difference is the whole point since
    /// #64's `Sidebar` enum: the panel is **kept** once built, so its existence says nothing
    /// about what is on screen. A test asserting the toggle worked has to ask which sidebar
    /// is selected, or it passes whether or not the toggle does anything.
    #[cfg(test)]
    pub fn search_panel_is_showing_for_test(&self) -> bool {
        self.sidebar == Sidebar::Search
    }

    /// Points the workspace at a folder without going through the file dialog.
    ///
    /// The tail of `open_folder` with the prompt and the background read removed, which is
    /// the same shape as `open_document_for_test`. A project search needs a root, and there
    /// is no other way for a headless test to give it one.
    #[cfg(test)]
    pub fn open_folder_for_test(&mut self, root: std::path::PathBuf, cx: &mut Context<Self>) {
        self.tree = FileTree::new(root).ok();
        cx.notify();
    }

    /// Opens a folder *and* starts the language server, as ⌘O does.
    ///
    /// Separate from `open_folder_for_test` because that one deliberately stops at the tree:
    /// most tests want a root and would otherwise spawn Intelephense. This is for the tests
    /// that are about the server itself — and its absence is why #125 survived. Every test
    /// used the tree-only seam, so `start_lsp` was never reached by anything, and the fact
    /// that ⌘O was its only caller went unnoticed.
    #[cfg(test)]
    pub fn open_folder_and_start_lsp_for_test(
        &mut self,
        root: std::path::PathBuf,
        cx: &mut Context<Self>,
    ) {
        self.tree = FileTree::new(root).ok();
        self.start_lsp(cx);
        cx.notify();
    }

    /// The language server's state, for asserting that one was actually attempted.
    #[cfg(test)]
    pub fn lsp_state_for_test(&self) -> LspState {
        self.lsp.state().clone()
    }

    /// Right-clicks a tree row, through the same handler the row's mouse-down uses.
    ///
    /// The position is arbitrary — nothing headless can assert where a menu was drawn (the
    /// text system is a fake monospace, see `fonts`) — but it goes through the real path so
    /// what is being tested is the real open, not a reimplementation of it.
    #[cfg(test)]
    pub fn right_click_tree_row_for_test(
        &mut self,
        index: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.open_tree_menu(index, gpui::Point::default(), window, cx);
    }

    /// The entries the open context menu is offering, or `None` if no menu is open.
    #[cfg(test)]
    pub fn menu_actions_for_test(&self, cx: &App) -> Option<Vec<crate::context_menu::MenuAction>> {
        let overlay = self.overlay.as_ref()?;
        overlay.read(cx).entries_for_test()
    }

    /// Picks a menu entry, through the handler the click and the Enter key both use.
    #[cfg(test)]
    pub fn pick_menu_action_for_test(
        &mut self,
        action: crate::context_menu::MenuAction,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.on_menu_action(action, window, cx);
    }

    /// Confirms the open name prompt with `name`.
    #[cfg(test)]
    pub fn confirm_name_for_test(
        &mut self,
        name: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.on_name_confirmed(name.to_string(), window, cx);
    }

    /// Accepts the open delete confirmation.
    #[cfg(test)]
    pub fn confirm_delete_for_test(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.on_delete_confirmed(window, cx);
    }

    /// Whether any tree overlay is open.
    #[cfg(test)]
    pub fn overlay_is_open_for_test(&self) -> bool {
        self.overlay.is_some()
    }

    /// Confirms a palette row by id, through the handler Enter and a click both use.
    #[cfg(test)]
    pub fn confirm_palette_for_test(
        &mut self,
        id: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.confirm_palette(id.to_string(), window, cx);
    }

    /// The active tab's language, which is what the status bar's last cell shows.
    #[cfg(test)]
    pub fn active_language_for_test(&self, cx: &App) -> Option<elle_syntax::Language> {
        Some(self.active_editor()?.read(cx).document.language())
    }

    /// The rows the open palette is showing.
    #[cfg(test)]
    pub fn palette_labels_for_test(&self, cx: &App) -> Vec<String> {
        self.palette.as_ref().map(|palette| palette.read(cx).labels_for_test()).unwrap_or_default()
    }

    /// Dismisses the open overlay, as Escape and a click outside both do.
    #[cfg(test)]
    pub fn dismiss_overlay_for_test(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.dismiss_overlay(window, cx);
    }

    /// A definition landing, through the same door the LSP answer takes.
    #[cfg(test)]
    pub fn open_path_at_lsp_for_test(
        &mut self,
        path: PathBuf,
        line: u32,
        character: u32,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.open_path_at_lsp(path, line, character, window, cx);
    }

    /// The active cursor as a full point, for asserting a landing column.
    #[cfg(test)]
    pub fn cursor_point_for_test(&self, cx: &App) -> Option<Point> {
        Some(self.active_editor()?.read(cx).document.cursor_point())
    }

    /// The active tab's path, for asserting which file a jump opened.
    #[cfg(test)]
    pub fn reveal_active_file_for_test(&mut self, cx: &mut Context<Self>) {
        self.reveal_active_file(cx);
    }

    #[cfg(test)]
    pub fn tree_entry_paths_for_test(&self) -> Vec<PathBuf> {
        self.tree.as_ref().map(|t| t.entries().iter().map(|e| e.path.clone()).collect()).unwrap_or_default()
    }

    #[cfg(test)]
    pub fn active_tab_path_for_test(&self) -> Option<PathBuf> {
        self.tabs.get(self.active_tab).and_then(|tab| tab.path.clone())
    }

    /// Places the cursor at a byte offset and runs go-to-definition, the way a ⌘click does.
    ///
    /// A ⌘click moves the caret and then emits `GoToDefinition`; F12 acts on the caret
    /// already there. Both land in `go_to_definition_at_cursor`, so a test that moves the
    /// caret and calls it exercises the same gate, dialect decision and reference read the
    /// real gestures do — without a pixel-to-offset hit test that the fake text system
    /// cannot make honest (see `hover_for_offset`).
    #[cfg(test)]
    pub fn go_to_definition_at_offset_for_test(
        &mut self,
        offset: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(editor) = self.active_editor() {
            editor.update(cx, |editor, _cx| editor.document.move_to(offset, false));
        }
        self.go_to_definition_at_cursor(window, cx);
    }

    /// ⌘, through the real handler.
    #[cfg(test)]
    pub fn toggle_settings_panel_for_test(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.open_settings(&OpenSettings, window, cx);
    }

    #[cfg(test)]
    pub fn settings_panel_for_test(&self) -> Option<Entity<SettingsPanel>> {
        self.settings_panel.clone()
    }

    /// A terminal link arriving, through the same resolver the subscription calls.
    #[cfg(test)]
    pub fn open_terminal_link_for_test(
        &mut self,
        path: PathBuf,
        line: Option<u32>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.open_terminal_link(path, line, window, cx);
    }

    /// Where the active editor's cursor is, for asserting a jump landed.
    #[cfg(test)]
    pub fn cursor_row_for_test(&self, cx: &App) -> Option<usize> {
        let editor = self.active_editor()?;
        Some(editor.read(cx).document.cursor_point().row)
    }

    /// The tree's visible row names, for asserting a refresh landed.
    #[cfg(test)]
    pub fn tree_names_for_test(&self) -> Vec<String> {
        self.tree
            .as_ref()
            .map(|tree| tree.entries().iter().map(|entry| entry.name.clone()).collect())
            .unwrap_or_default()
    }

    /// How many tabs are open, for asserting a delete closed the right ones.
    #[cfg(test)]
    pub fn tab_count_for_test(&self) -> usize {
        self.tabs.len()
    }

    /// Clicks a result row, through the same handler the row's mouse-down uses.
    ///
    /// Takes a `Window` since #102: opening a result now focuses the editor it opened, and
    /// a test seam that skipped that would be testing a *different* open from the real one
    /// — which is the bug #95 was about.
    #[cfg(test)]
    pub fn open_search_result_for_test(
        &mut self,
        file: usize,
        line: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.open_search_result(file, line, window, cx);
    }

    /// Publishes diagnostics as if a server had sent them, and pushes them to the editors.
    ///
    /// This is the tail of `apply_lsp_events` with the server removed, for the same reason
    /// `open_document_for_test` is the tail of `open_path`: a render test cannot spawn
    /// Intelephense, and the thing worth testing is that diagnostics reach a real layout
    /// pass — not that a subprocess starts.
    #[cfg(test)]
    pub fn publish_diagnostics_for_test(
        &mut self,
        path: &std::path::Path,
        diagnostics: &[elle_lsp::lsp_types::Diagnostic],
        cx: &mut Context<Self>,
    ) {
        let Some(uri) = crate::lsp_session::uri_for(path) else { return };
        let text = self
            .tabs
            .iter()
            .find(|tab| tab.path.as_deref() == Some(path))
            .map(|tab| tab.editor.read(cx).document.buffer.text())
            .unwrap_or_default();

        self.lsp.set_state(LspState::Running);
        self.lsp.set_diagnostics(uri, diagnostics, &text);
        self.push_diagnostics_to_editors(cx);
        cx.notify();
    }

    /// The status-bar text for the language server, for tests that assert on silence.
    #[cfg(test)]
    pub fn lsp_label_for_test(&self) -> String {
        lsp_label(&self.lsp, self.active_tab_wants_a_server())
    }

    /// Whether the file in front of the user is one a language server would handle.
    ///
    /// What turns §24's silence about a missing server into a single honest word (#125):
    /// nobody needs telling there is no PHP server while they are looking at a `.txt`, and
    /// everybody needs telling while they are looking at a `.php`.
    fn active_tab_wants_a_server(&self) -> bool {
        self.tabs
            .get(self.active_tab)
            .and_then(|tab| tab.path.as_deref())
            .is_some_and(crate::lsp_session::handles)
    }

    /// Puts an already-built document into a tab, synchronously.
    ///
    /// `open_path` reads from disk on the background executor, which a render test cannot
    /// drive without a real file and a real await. This is the same tail of that function
    /// with the IO removed, so the view under test reaches the state a real open produces.
    ///
    /// That includes focus. Leaving it out would make this helper a *different* open from
    /// the real one, and #95 is precisely the bug where two opens disagreed about focus.
    #[cfg(test)]
    pub fn open_document_for_test(
        &mut self,
        document: Document,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let path = document.path.clone();
        let editor = self.new_editor(document, window, cx);
        window.focus(&editor.read(cx).focus_handle(cx));
        self.tabs.push(Tab { path, editor });
        self.active_tab = self.tabs.len() - 1;
        self.scroll_active_tab_into_view();
        cx.notify();
    }

    /// Builds an editor and subscribes to what it reports.
    ///
    /// Every tab goes through here so none can be created without the subscription — a
    /// ⌘click that silently does nothing in tabs opened one particular way is the kind of
    /// bug that survives a long time, because the feature demonstrably works elsewhere.
    fn new_editor(
        &self,
        document: Document,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Entity<EditorView> {
        let editor = cx.new(|cx| EditorView::new(document, cx));
        // `subscribe_in` rather than `subscribe`: a ⌘click ends in an open, and an open now
        // has to move focus (#95), which needs a window all the way down.
        cx.subscribe_in(&editor, window, |this, _editor, event, window, cx| match event {
            // The editor has already moved the cursor, so the origin this reads is the
            // clicked position — which is both where the query is about and where Back
            // should return to.
            EditorEvent::GoToDefinition => this.go_to_definition_at_cursor(window, cx),
            // A character reached the buffer. Only the workspace knows whether the server
            // declared it a completion trigger, so only the workspace can decide (#61).
            EditorEvent::Typed(text) => this.editor_typed(text, window, cx),
        })
        .detach();
        editor
    }

    /// Opens a file in a tab, or activates the tab already showing it.
    pub fn open_path(&mut self, path: PathBuf, window: &mut Window, cx: &mut Context<Self>) {
        self.open_path_at(path, None, window, cx);
    }

    /// Opens a file and, if `target` is given, puts the cursor there.
    ///
    /// # Why the target is threaded through here rather than added per feature
    ///
    /// Every navigation in the app is "open this file, then land somewhere in it": a route
    /// in the palette, a go-to-definition, a reference, a symbol. The open is asynchronous —
    /// the file is read on the background executor — so a caller that wanted a cursor
    /// position had nowhere to put it and could only open the file and give up on the line.
    /// That is exactly what #68's route palette did.
    ///
    /// Doing it per feature is how three subtly different jump paths appear, each with its
    /// own answer to the tab-already-open case and its own clamping. There is one answer
    /// here: `EditorView::reveal`, reached identically whether the tab was loaded just now
    /// or was already in front of the user.
    ///
    /// The target is a [`Point`], not a byte offset. A line number is what every producer
    /// actually has — a route index, an LSP position, a stack frame — and resolving it to
    /// an offset needs the buffer, which does not exist until the load finishes.
    ///
    /// # Why this takes a `Window`
    ///
    /// Opening a file is also a focus change, and #95 is what it costs to treat those as
    /// separate concerns: this function landed the cursor on the right line and left the
    /// keyboard wherever it was, so every command-driven jump — F12, ⇧F12, ⌘⇧O, the route
    /// palette — looked like it had worked and then swallowed the next keystroke. The tree's
    /// click path focused because it happened to have a `Window`; this one did not have one
    /// to focus with. Both doors now lead here, so there is one answer for both.
    pub fn open_path_at(
        &mut self,
        path: PathBuf,
        target: Option<Point>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.open_path_target(path, target.map(Target::Point), window, cx);
    }

    /// Opens a file at a position still in the server's units.
    ///
    /// The definition/declaration door. Takes the LSP position raw rather than a `Point`
    /// because the conversion needs the target file's text — see [`Target`].
    fn open_path_at_lsp(
        &mut self,
        path: PathBuf,
        line: u32,
        character: u32,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.open_path_target(path, Some(Target::Lsp { line, character }), window, cx);
    }

    fn open_path_target(
        &mut self,
        path: PathBuf,
        target: Option<Target>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(index) = self.tabs.iter().position(|tab| tab.path.as_ref() == Some(&path)) {
            self.active_tab = index;
            self.scroll_active_tab_into_view();
            self.clear_hover_cards(cx);
            // A file already open still has to move: "go to definition" on something in the
            // current file is the common case, and leaving the cursor where it was would
            // make the command look broken.
            if let Some(target) = target {
                self.tabs[index].editor.update(cx, |editor, cx| {
                    let point = target.resolve(&editor.document);
                    editor.reveal(point);
                    cx.notify();
                });
            }
            // The common case for go-to-definition, and so the one that has to focus: the
            // tab was already in front of the user and nothing else would move focus for us.
            window.focus(&self.tabs[index].editor.read(cx).focus_handle(cx));
            cx.notify();
            return;
        }

        let load_path = path.clone();
        // `spawn_in` rather than `spawn`: the focus below happens after an await, so the
        // task needs a window context to reach it.
        let task = cx.spawn_in(window, async move |this, cx| {
            let loaded = cx.background_spawn(async move { read_file(&load_path) }).await;

            this.update_in(cx, |this, window, cx| {
                match loaded {
                    Ok(file) => {
                        match Document::new(Some(path.clone()), &file.text, file.trailing_newline) {
                            Ok(document) => {
                                let text = document.buffer.text();
                                let editor = this.new_editor(document, window, cx);
                                // Before the tab is pushed, so the first frame the user sees
                                // is already at the target rather than painting the top of
                                // the file and jumping a frame later.
                                if let Some(target) = target {
                                    editor.update(cx, |editor, _| {
                                        let point = target.resolve(&editor.document);
                                        editor.reveal(point);
                                    });
                                }
                                window.focus(&editor.read(cx).focus_handle(cx));
                                this.tabs.push(Tab { path: Some(path.clone()), editor });
                                this.active_tab = this.tabs.len() - 1;
                                this.scroll_active_tab_into_view();
                                this.status = None;
                                this.open_on_lsp(&path, &text, cx);
                            }
                            // Focus stays where it was on a failure: the file the user asked
                            // for is not on screen, so moving the keyboard into whatever is
                            // would type into the wrong buffer.
                            Err(err) => this.status = Some(format!("{err:#}").into()),
                        }
                    }
                    Err(err) => this.status = Some(format!("{err:#}").into()),
                }
                cx.notify();
            })
            .ok();
        });
        self.jobs.start(Job::OpenFile, task);
    }

    /// Opens an empty, pathless buffer in a new tab.
    ///
    /// This is the missing half of save-as: `save_as` and `prompt_for_new_path` were already
    /// here, but nothing could ever reach them because every tab was born from a file on
    /// disk and so always had a path. A tab with `path: None` is what makes `save` fall
    /// through to the prompt, so creating one is the entire wiring — no new save path.
    ///
    /// Unlike `open_path` there is no dedup: ⌘N twice means the user wants two scratch
    /// buffers, and they have no path to match on anyway.
    ///
    /// Takes a `Window` because a new buffer, like an opened file, has to end up holding the
    /// keyboard — ⌘N followed by typing must land in the scratch buffer, not wherever focus
    /// happened to be (#95).
    pub fn new_file(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        match Document::untitled() {
            Ok(document) => {
                let editor = self.new_editor(document, window, cx);
                window.focus(&editor.read(cx).focus_handle(cx));
                self.tabs.push(Tab { path: None, editor });
                self.active_tab = self.tabs.len() - 1;
                self.scroll_active_tab_into_view();
                self.status = None;
            }
            // Plain text needs no grammar, so this is unreachable in practice — but it is a
            // Result, and swallowing it would leave ⌘N doing nothing with no explanation.
            Err(err) => self.status = Some(format!("{err:#}").into()),
        }
        cx.notify();
    }

    /// Saves every dirty tab that has a path, silently — the autosave pass.
    ///
    /// One background task writes them all: the per-tab `save` shares a superseding
    /// job slot, which for N tabs would keep only the last write's continuation. A
    /// pathless scratch buffer is skipped — autosave must never open a dialog.
    /// Failures land on the status line with the path; successes are silent.
    fn autosave_dirty_tabs(&mut self, cx: &mut Context<Self>) {
        if !crate::settings::current(cx).autosave() {
            return;
        }
        let dirty: Vec<(std::path::PathBuf, Entity<EditorView>, String, elle_text::Version)> = self
            .tabs
            .iter()
            .filter_map(|tab| {
                let path = tab.path.clone()?;
                let editor = tab.editor.clone();
                if !editor.read(cx).document.buffer.is_dirty() {
                    return None;
                }
                let snapshot = editor.read(cx).document.snapshot_for_save();
                Some((path, editor, snapshot.text, snapshot.version))
            })
            .collect();
        if dirty.is_empty() {
            return;
        }

        let writes: Vec<(std::path::PathBuf, String)> =
            dirty.iter().map(|(path, _, text, _)| (path.clone(), text.clone())).collect();
        let task = cx.spawn(async move |this, cx| {
            let outcomes = cx
                .background_spawn(async move {
                    writes
                        .into_iter()
                        .map(|(path, text)| {
                            let result = write_file(&path, &text);
                            (path, text, result)
                        })
                        .collect::<Vec<_>>()
                })
                .await;
            this.update(cx, |this, cx| {
                for ((path, text, result), (_, editor, _, version)) in
                    outcomes.into_iter().zip(dirty)
                {
                    match result {
                        Ok(()) => {
                            this.notify_lsp_of_change(&path, &text);
                            editor.update(cx, |editor, _| {
                                editor.document.buffer.mark_saved_at(version)
                            });
                        }
                        Err(err) => {
                            this.status =
                                Some(format!("autosave failed: {}: {err:#}", path.display()).into());
                        }
                    }
                }
                this.refresh_git_status(cx);
                cx.notify();
            })
            .ok();
        });
        // Detached rather than slotted: a second blur while writes are in flight must
        // not cancel the first continuation and strand written files marked dirty.
        task.detach();
    }

    // --- saving ------------------------------------------------------------------

    fn save(&mut self, _: &Save, _w: &mut Window, cx: &mut Context<Self>) {
        let Some(tab) = self.tabs.get(self.active_tab) else { return };
        let Some(path) = tab.path.clone() else {
            self.save_as(cx);
            return;
        };

        let editor = tab.editor.clone();
        let snapshot = editor.read(cx).document.snapshot_for_save();
        let (text, version) = (snapshot.text, snapshot.version);

        let synced = (path.clone(), text.clone());

        let task = cx.spawn(async move |this, cx| {
            let written = cx.background_spawn(async move { write_file(&path, &text) }).await;

            this.update(cx, |this, cx| {
                match written {
                    Ok(()) => {
                        // Resync before marking clean, so a server that answers instantly
                        // reports on the text that is now on disk.
                        this.notify_lsp_of_change(&synced.0, &synced.1);
                        // Only clear dirty state after the write actually succeeded, and
                        // only for the text that was actually written.
                        editor
                            .update(cx, |editor, _| editor.document.buffer.mark_saved_at(version));
                        this.status = None;
                        // Second refresh trigger: a save is the only way this editor
                        // changes the working tree, so it is the only internal event that
                        // can invalidate the status (#64).
                        this.refresh_git_status(cx);
                        // Saving a model or migration is the in-editor event that can
                        // stale the Laravel index; the wholesale rebuild is milliseconds
                        // (#21's documented starting point). `is_under` canonicalises —
                        // a tab's path and the tree's root spell the temp dir
                        // differently on macOS, the /var-vs-/private/var trap.
                        if let Some(root) = this.tree.as_ref().map(|tree| tree.root().to_path_buf())
                            && (is_under(&synced.0, &root.join("app/Models"))
                                || is_under(&synced.0, &root.join("database/migrations")))
                        {
                            rebuild_laravel_index(root, cx);
                        }
                    }
                    // The buffer is untouched on failure, so the user loses nothing.
                    Err(err) => this.status = Some(format!("save failed: {err:#}").into()),
                }
                cx.notify();
            })
            .ok();
        });
        self.jobs.start(Job::Save, task);
    }

    /// Asks for a location, then saves a buffer that has no path yet.
    ///
    /// Suggests the project root, so a new file lands somewhere sensible instead of
    /// wherever the process happens to be running.
    fn save_as(&mut self, cx: &mut Context<Self>) {
        let Some(tab) = self.tabs.get(self.active_tab) else { return };
        let editor = tab.editor.clone();

        let directory = self
            .tree
            .as_ref()
            .map(|tree| tree.root().to_path_buf())
            .unwrap_or_else(std::env::temp_dir);

        let chosen = cx.prompt_for_new_path(&directory, Some("untitled.php"));

        let task = cx.spawn(async move |this, cx| {
            // Same three nested layers as prompt_for_paths: channel dropped, IO error,
            // user cancelled. All three mean "do nothing".
            let Ok(Ok(Some(path))) = chosen.await else { return };

            // Serialise *after* the dialog closes, not before. gpui's save panel is not
            // app-modal (`beginWithCompletionHandler:`), so the editor keeps accepting
            // keystrokes for however long the user browses for a folder — a snapshot taken
            // before the prompt would write text the user has already moved past, and the
            // version guard below would then correctly refuse to clear the dirty flag,
            // leaving a file on disk that silently lags the buffer.
            let Ok(snapshot) =
                editor.read_with(cx, |editor, _| editor.document.snapshot_for_save())
            else {
                return;
            };
            let (text, version) = (snapshot.text, snapshot.version);

            let write_path = path.clone();
            let written = cx.background_spawn(async move { write_file(&write_path, &text) }).await;

            this.update(cx, |this, cx| {
                match written {
                    Ok(()) => {
                        editor.update(cx, |editor, _| {
                            editor.document.buffer.mark_saved_at(version);
                            // The document now has a home: adopt the path so the next ⌘S
                            // writes straight through, and so a buffer saved as `.php`
                            // starts highlighting as PHP. A grammar that fails to load
                            // leaves it as plain text rather than failing the save.
                            if let Err(err) = editor.document.set_path(path.clone()) {
                                tracing::warn!(
                                    "saved, but no grammar for {}: {err:#}",
                                    path.display()
                                );
                            }
                        });
                        // Keep the tab's own record in step, or a later save would open
                        // this dialog again for a file that now has a path.
                        if let Some(tab) = this.tabs.iter_mut().find(|tab| tab.editor == editor) {
                            tab.path = Some(path);
                        }
                        this.status = None;
                    }
                    Err(err) => this.status = Some(format!("save failed: {err:#}").into()),
                }
                cx.notify();
            })
            .ok();
        });
        self.jobs.start(Job::Save, task);
    }

    fn close_tab(&mut self, _: &CloseTab, window: &mut Window, cx: &mut Context<Self>) {
        self.close_tab_at(self.active_tab, window, cx);
    }

    /// Closes the tab at `index`, asking first if it has unsaved changes.
    ///
    /// Losing a user's edits silently is not a simplification worth making, so this is the
    /// one place in the app that deliberately interrupts. Uses the platform dialog rather
    /// than a custom modal: it is native, focus-correct and accessible for free, and an
    /// in-app modal would be more code for a worse result.
    fn close_tab_at(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        // Here rather than in `close_tab`, because ⌘W is not the only way in: the ✕ on a tab
        // calls this directly, and that is the more common gesture. A popup left standing
        // over a closed document keeps `completion_word_start` — a byte offset into a buffer
        // that is gone — and the next accept would write it into whichever tab inherited the
        // active slot.
        //
        // Before the dirty-file prompt below rather than after, because that path is
        // asynchronous: the popup must go when the user asks to close, not when they answer
        // a dialog whose answer may be Cancel.
        self.dismiss_completion(window, cx);

        let Some(tab) = self.tabs.get(index) else { return };

        if !tab.editor.read(cx).is_dirty() {
            self.remove_tab(index, cx);
            return;
        }

        let title = tab.editor.read(cx).document.title();
        // The entity handle identifies the tab across the await below. Titles are not
        // unique (`app/Models/User.php` and `tests/User.php` share one) and indices shift,
        // so matching on either could close the wrong file — precisely the data loss this
        // prompt exists to prevent.
        let editor = tab.editor.clone();

        // Button order matters: index 0 is the default on macOS, so Cancel sits there.
        // A stray Return must not be the keystroke that discards someone's work.
        let answer = window.prompt(
            gpui::PromptLevel::Warning,
            &format!("{title} has unsaved changes."),
            Some("Closing this tab will discard them."),
            &["Cancel", "Discard Changes"],
            cx,
        );

        let task = cx.spawn(async move |this, cx| {
            // A dropped receiver means the dialog vanished without an answer; treat that
            // as Cancel, because the safe default is to keep the buffer.
            let Ok(choice) = answer.await else { return };
            if choice != 1 {
                return;
            }
            this.update(cx, |this, cx| {
                // Re-resolve by entity handle: indices shift and titles are not unique, so
                // matching on either could close the wrong file. A tab that has gone away
                // in the meantime resolves to None and this is a no-op, which is right.
                if let Some(current) = this.tabs.iter().position(|tab| tab.editor == editor) {
                    this.remove_tab(current, cx);
                }
            })
            .ok();
        });
        self.jobs.start(Job::ClosePrompt, task);
    }

    fn remove_tab(&mut self, index: usize, cx: &mut Context<Self>) {
        if index >= self.tabs.len() {
            return;
        }
        let closed = self.tabs.remove(index);
        if let Some(path) = closed.path.as_deref() {
            self.close_on_lsp(path);
        }
        self.active_tab = active_after_close(self.active_tab, index, self.tabs.len());
        self.scroll_active_tab_into_view();
        self.clear_hover_cards(cx);
        cx.notify();
    }

    // --- terminal ----------------------------------------------------------------

    /// Opens the panel with one session, or closes it if it is already open.
    ///
    /// Closing drops the `TerminalView`, which drops its sessions, which kills the shells.
    /// That is the intended meaning of closing the panel: §24's isolation works because a
    /// terminal owns nothing the editor needs.
    fn toggle_terminal(&mut self, _: &ToggleTerminal, window: &mut Window, cx: &mut Context<Self>) {
        // Takes focus, so the completion popup must not be left behind unfocused and
        // therefore undismissable. Stated here as well as in the popup's focus-out listener
        // — see `open_find` for why both.
        self.dismiss_completion(window, cx);
        match self.terminal.take() {
            Some(_) => {
                // Focus returns to the editor the eye is now on, or the keymap stays dead —
                // the same rule the palette and every overlay follow (#171/#172). Parking it
                // on the workspace root left the next keystroke reaching nothing.
                self.focus_editor_or_workspace(window, cx);
            }
            None => {
                let terminal = cx.new(TerminalView::new);
                terminal.update(cx, |terminal, cx| {
                    terminal.set_cwd(self.tree.as_ref().map(|tree| tree.root().to_path_buf()));
                    // A panel that opens with no session would just show a placeholder;
                    // the user asked for a terminal, so start one.
                    terminal.open_session(cx);
                });
                // ⌘-clicked paths come back as events (#70): the terminal sees one line of
                // output and cannot resolve `app/User.php` against anything, so it reports
                // the claim and this side — which holds the project root — decides.
                cx.subscribe_in(
                    &terminal,
                    window,
                    |this, _terminal, event, window, cx| match event {
                        TerminalViewEvent::OpenPath { path, line } => {
                            this.open_terminal_link(path.clone(), *line, window, cx);
                        }
                    },
                )
                .detach();
                window.focus(&terminal.read(cx).focus_handle(cx));
                self.terminal = Some(terminal);
            }
        }
        cx.notify();
    }

    /// Adds a session, opening the panel first if it was closed.
    fn new_terminal(&mut self, _: &NewTerminal, window: &mut Window, cx: &mut Context<Self>) {
        match self.terminal.clone() {
            Some(terminal) => {
                terminal.update(cx, |terminal, cx| terminal.open_session(cx));
                window.focus(&terminal.read(cx).focus_handle(cx));
                cx.notify();
            }
            // Opening the panel already creates a session, so this is the same path.
            None => self.toggle_terminal(&ToggleTerminal, window, cx),
        }
    }

    // --- test runner (#25) ---------------------------------------------------------

    /// Opens the test panel, or closes it if it is already open.
    ///
    /// Closing does not cancel a run. Unlike the terminal — whose shells exist only to be
    /// looked at — a suite is work the user started and may want to keep running while they
    /// read the code it is testing. Cancelling is [`Self::cancel_test_run`], and it happens
    /// when a new run supersedes this one or the folder changes.
    fn toggle_test_panel(
        &mut self,
        _: &ToggleTestPanel,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match self.tests.take() {
            // Focus follows the eye back to the editor, not the workspace root — closing the
            // panel must not leave the keyboard dead (#171/#172's rule for every dismiss).
            Some(_) => self.focus_editor_or_workspace(window, cx),
            None => {
                self.tests = Some(self.new_test_panel(cx));
            }
        }
        cx.notify();
    }

    /// Builds the panel, detects the project's runner, and wires clicks to `open_path_at`.
    fn new_test_panel(&mut self, cx: &mut Context<Self>) -> Entity<TestView> {
        let root = self.tree.as_ref().map(|tree| tree.root().to_path_buf());
        let panel = cx.new(TestView::new);
        let workspace = cx.entity();

        panel.update(cx, |panel, _| {
            // Detection is a handful of `is_file` calls, so it happens inline rather than
            // on the background executor. `None` — no test framework in this project — is
            // the common case and produces no work, no message and no error (§24).
            panel.runner = root.as_deref().and_then(elle_test_runner::detect);

            let root = root.clone();
            panel.on_jump(move |test, window, cx| {
                let Some(location) = test.location.clone() else { return };
                let Some(root) = root.clone() else { return };
                // Pest prints paths relative to the project root, PHPUnit absolute ones.
                // Joining an absolute path onto the root is a no-op in `PathBuf`, so this
                // handles both without asking which runner produced it.
                let path = root.join(&location.path);
                if !path.is_file() {
                    // The runner named a file we cannot find. Saying nothing is the honest
                    // answer — opening a plausible-looking neighbour would be worse than
                    // not moving at all (RISKS.md #4).
                    return;
                }
                // `Location::line` is 1-based; `Point` rows are 0-based. One jump path for
                // the whole app (#88), not a second one invented here.
                let point = Point::new(location.line.saturating_sub(1) as usize, 0);
                workspace.update(cx, |workspace, cx| {
                    workspace.open_path_at(path, Some(point), window, cx)
                });
            });
        });
        panel
    }

    fn run_tests(&mut self, _: &RunTests, window: &mut Window, cx: &mut Context<Self>) {
        self.start_test_run(elle_test_runner::Scope::All, window, cx);
    }

    /// Runs the tests in the active tab's file.
    fn run_tests_in_file(
        &mut self,
        _: &RunTestsInFile,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(path) = self.tabs.get(self.active_tab).and_then(|tab| tab.path.clone()) else {
            // An unsaved tab has no file for the runner to be pointed at.
            return;
        };
        let root = self.tree.as_ref().map(|tree| tree.root().to_path_buf());
        // Relative to the root, so the command shown is one the user could paste into a
        // terminal in that folder.
        let scoped = root
            .as_deref()
            .and_then(|root| path.strip_prefix(root).ok())
            .map(PathBuf::from)
            .unwrap_or(path);

        self.start_test_run(elle_test_runner::Scope::File(scoped), window, cx);
    }

    /// Re-runs only the tests that failed in the last run.
    fn rerun_failed_tests(
        &mut self,
        _: &RerunFailedTests,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let failed = self
            .tests
            .as_ref()
            .map(|panel| panel.read(cx).report.failed_names())
            .unwrap_or_default();

        // Nothing failed, so there is nothing to rerun. Falling through to a scope that
        // selects everything would turn "rerun the failures" into a full suite run — the
        // trap `Scope::is_empty` exists to make visible.
        if failed.is_empty() {
            return;
        }
        self.start_test_run(elle_test_runner::Scope::Names(failed), window, cx);
    }

    /// Cancels a run in flight, if there is one.
    fn cancel_test_run(&mut self) {
        if let Some(cancel) = self.test_cancel.take() {
            cancel.cancel();
        }
        // The flag stops the read loop and kills the child; dropping the task stops us
        // awaiting it. Both are needed (ADR-0007).
        self.jobs.cancel(Job::TestRun);
    }

    /// Spawns a run and streams its results into the panel.
    fn start_test_run(
        &mut self,
        scope: elle_test_runner::Scope,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // Running tests is the one action that opens the panel by itself: the results have
        // to land somewhere the user can see them.
        let panel = match self.tests.clone() {
            Some(panel) => panel,
            None => {
                let panel = self.new_test_panel(cx);
                self.tests = Some(panel.clone());
                window.focus(&panel.read(cx).focus_handle(cx));
                panel
            }
        };

        let Some(runner) = panel.read(cx).runner.clone() else {
            // No Pest and no PHPUnit in this project. The panel says so and nothing is
            // spawned — no dialog, no log line, no error (§24).
            panel.update(cx, |panel, cx| {
                panel.finish(
                    RunState::Failed {
                        message: "No Pest or PHPUnit found in vendor/bin".to_string(),
                    },
                    cx,
                );
            });
            cx.notify();
            return;
        };

        // A second run supersedes the first: two suites at once would fight over the same
        // database and interleave their output into one report.
        self.cancel_test_run();

        let command = runner.command(&scope);
        let shown = command.display();
        panel.update(cx, |panel, cx| panel.begin(shown, &scope, cx));

        let cancel = TestCancelFlag::new();
        self.test_cancel = Some(cancel.clone());

        let task = cx.spawn(async move |this, cx| {
            // A channel rather than calling back into the panel from the background
            // thread: `on_event` runs on the executor, and touching an `Entity` there is
            // not allowed. The receiver marshals each event onto the foreground thread, so
            // the panel fills in *as* the suite runs rather than all at once at the end.
            //
            // Unbounded and async: unbounded because a fast suite emits events faster than
            // the UI repaints and a bounded channel would block the reader thread on the
            // frame rate; async because awaiting the receiver parks this task until an
            // event actually arrives. No timer and no poll — an editor between runs does
            // no work at all, which is what the idle-footprint gate measures (#79, #93).
            let (sender, receiver) = smol::channel::unbounded();
            let run_command = command.clone();
            let run_cancel = cancel.clone();
            let outcome = cx.background_spawn(async move {
                let result = elle_test_runner::run(&run_command, &run_cancel, |event| {
                    // A send error means the receiver is gone, which means the run was
                    // abandoned. The cancel flag is what actually stops it.
                    let _ = sender.send_blocking(event);
                });
                // Dropping the sender here closes the channel, which is what ends the
                // receive loop below — including when the run failed to start at all.
                result
            });

            // Parks until an event arrives, and ends when the sender is dropped.
            while let Ok(event) = receiver.recv().await {
                panel.update(cx, |panel, cx| panel.push(event, cx)).ok();
            }

            let outcome = outcome.await;
            this.update(cx, |this, cx| {
                this.test_cancel = None;
                let state = match outcome {
                    Ok(elle_test_runner::Outcome::Exited { code }) => {
                        RunState::Finished { command: command.display(), code }
                    }
                    Ok(elle_test_runner::Outcome::Cancelled) => {
                        RunState::Cancelled { command: command.display() }
                    }
                    // Could not start the runner at all — distinct from a suite that
                    // failed, and worded so the difference is visible.
                    Err(error) => RunState::Failed { message: format!("{error:#}") },
                };
                panel.update(cx, |panel, cx| panel.finish(state, cx));
                cx.notify();
            })
            .ok();
        });
        self.jobs.start(Job::TestRun, task);
    }

    // --- find and replace (#80) ----------------------------------------------------

    fn find(&mut self, _: &Find, window: &mut Window, cx: &mut Context<Self>) {
        self.open_find(false, window, cx);
    }

    fn replace(&mut self, _: &Replace, window: &mut Window, cx: &mut Context<Self>) {
        self.open_find(true, window, cx);
    }

    /// ⌘F / ⌘⌥F. Opens the bar, or **refocuses** one already open.
    ///
    /// Refocusing rather than reopening is the constraint from the issue, and it is not
    /// just a nicety: ⌘F with the bar already open and a query typed must not throw the
    /// query away. ⌘⌥F on a find-only bar additionally reveals the replace row.
    fn open_find(&mut self, replacing: bool, window: &mut Window, cx: &mut Context<Self>) {
        // Nothing to search. Opening a bar over the empty-state placeholder would be a
        // control that cannot do anything.
        if self.active_editor().is_none() {
            return;
        }

        // ⌘F is workspace-scoped, so it arrives with the completion popup holding focus.
        // See `open_completion` for why this is stated at each focus-taking command as well
        // as in the popup's own focus-out listener: the listener is the general rule and
        // cannot be exercised headlessly, and these are the specific paths a test can pin.
        self.dismiss_completion(window, cx);

        if let Some(bar) = self.find.clone() {
            bar.update(cx, |bar, cx| bar.reopen(replacing, cx));
            window.focus(&bar.read(cx).focus_handle(cx));
            self.apply_search(cx);
            cx.notify();
            return;
        }

        let bar = cx.new(|cx| FindBar::new(replacing, cx));

        // Seed from the selection, the way every editor does — but only a single-line one.
        if let Some(editor) = self.active_editor()
            && let Some(text) = editor.read(cx).document.selected_text()
        {
            bar.update(cx, |bar, _cx| bar.seed(&text));
        }

        cx.subscribe_in(&bar, window, |this, _bar, event, window, cx| match event {
            FindEvent::QueryChanged => this.apply_search(cx),
            FindEvent::Navigate { forward } => this.navigate_match(*forward, cx),
            FindEvent::ReplaceOne => this.replace_one(cx),
            FindEvent::ReplaceAll => this.replace_all(cx),
            FindEvent::Dismissed => this.dismiss_find(window, cx),
        })
        .detach();

        window.focus(&bar.read(cx).focus_handle(cx));
        self.find = Some(bar);
        // Apply immediately: a seeded query must highlight before the user types anything.
        self.apply_search(cx);
        cx.notify();
    }

    /// Pushes the bar's query into the active document and pulls the count back out.
    ///
    /// The document owns the matches (`Document::search`) and the bar owns the query, so
    /// this is the one place the two meet. Called on every keystroke in the find field —
    /// which is where the search cost lands, and why `editor/find.rs` documents it — and
    /// once per render, so a tab switch re-searches the file now on screen.
    ///
    /// **Notifies only when something changed.** That is load-bearing rather than tidy:
    /// `Render` calls this, and an unconditional `cx.notify()` from inside a render is an
    /// infinite repaint loop.
    fn apply_search(&mut self, cx: &mut Context<Self>) {
        let Some(bar) = self.find.clone() else { return };
        let Some(editor) = self.active_editor().cloned() else { return };

        let query = bar.read(cx).query().clone();
        let status = editor.update(cx, |editor, cx| {
            if editor.document.set_search_query(query) {
                cx.notify();
            }
            search_status(&editor.document)
        });
        bar.update(cx, |bar, cx| bar.set_status(status, cx));
    }

    fn navigate_match(&mut self, forward: bool, cx: &mut Context<Self>) {
        let Some(editor) = self.active_editor().cloned() else { return };
        editor.update(cx, |editor, cx| {
            editor.select_match(forward, cx);
        });
        self.refresh_find_status(cx);
    }

    fn replace_one(&mut self, cx: &mut Context<Self>) {
        let (Some(bar), Some(editor)) = (self.find.clone(), self.active_editor().cloned()) else {
            return;
        };
        let replacement = bar.read(cx).replacement().to_string();
        editor.update(cx, |editor, cx| {
            editor.document.replace_current(&replacement);
            cx.notify();
        });
        self.refresh_find_status(cx);
    }

    fn replace_all(&mut self, cx: &mut Context<Self>) {
        let (Some(bar), Some(editor)) = (self.find.clone(), self.active_editor().cloned()) else {
            return;
        };
        let replacement = bar.read(cx).replacement().to_string();
        let count = editor.update(cx, |editor, cx| {
            let count = editor.document.replace_all(&replacement);
            cx.notify();
            count
        });
        // A count in the status bar rather than a dialog: replace-all is undoable in one
        // step, so the only thing the user needs is confirmation that it happened.
        self.status = Some(match count {
            0 => "Nothing to replace".into(),
            1 => "Replaced 1 occurrence".into(),
            count => format!("Replaced {count} occurrences").into(),
        });
        self.refresh_find_status(cx);
        cx.notify();
    }

    /// Re-reads the count from the document after something moved.
    fn refresh_find_status(&mut self, cx: &mut Context<Self>) {
        let (Some(bar), Some(editor)) = (self.find.clone(), self.active_editor().cloned()) else {
            return;
        };
        let status = editor.update(cx, |editor, _cx| search_status(&editor.document));
        bar.update(cx, |bar, cx| bar.set_status(status, cx));
    }

    /// Escape: closes the bar, clears the highlights, and returns focus to the editor.
    ///
    /// Explicitly **not** a tab close. `escape` is bound in the `Find` context only, so it
    /// cannot reach anything else while the bar has focus, and the editor gets focus back
    /// rather than the workspace root — pressing escape and then typing must insert text.
    ///
    /// Clearing the query is a choice, and it costs macOS's "⌘G still works after escape".
    /// The alternative costs more: matches highlighted across a file with no visible
    /// control explaining why, and no way to clear them short of searching for something
    /// else. VS Code clears; so does this.
    fn dismiss_find(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.find = None;
        if let Some(editor) = self.active_editor().cloned() {
            editor.update(cx, |editor, cx| {
                editor.document.clear_search();
                cx.notify();
            });
            window.focus(&editor.read(cx).focus_handle(cx));
        } else {
            window.focus(&self.focus_handle);
        }
        cx.notify();
    }

    /// ⌘G / ⌘⇧G, from anywhere in the workspace.
    ///
    /// Workspace-scoped so it works with focus back in the editor — ⌘F, type, click into
    /// the text, ⌘G is the common loop. A no-op with no query set, which is the state
    /// after escape.
    fn find_next(&mut self, _: &FindNext, _window: &mut Window, cx: &mut Context<Self>) {
        self.navigate_match(true, cx);
    }

    fn find_prev(&mut self, _: &FindPrev, _window: &mut Window, cx: &mut Context<Self>) {
        self.navigate_match(false, cx);
    }

    // --- find in project (#80) -----------------------------------------------------

    /// ⌘⇧F, the Search activity-bar entry, and the `editor.find_in_project` command.
    ///
    /// A **toggle**, matching what the activity bar does everywhere else: pressing it with
    /// Search already selected *and focused* goes back to the file tree. Pressing it while
    /// the panel is showing but unfocused refocuses instead — the same rule ⌘F follows, and
    /// for the same reason: ⌘⇧F after clicking into the editor must not send the user back
    /// to the tree when what they meant was "put me in the search field".
    fn find_in_project(&mut self, _: &FindInProject, window: &mut Window, cx: &mut Context<Self>) {
        // Takes focus, so the completion popup must not be left behind unfocused and
        // therefore undismissable. Stated here as well as in the popup's focus-out listener
        // — see `open_find` for why both.
        self.dismiss_completion(window, cx);
        self.toggle_search_panel(window, cx);
    }

    fn toggle_search_panel(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let panel = self.show_search_panel(window, cx);
        let handle = panel.read(cx).focus_handle(cx);

        // Already showing *and* focused: the press means "put it away".
        if self.sidebar == Sidebar::Search && handle.is_focused(window) {
            self.sidebar = Sidebar::Explorer;
            // The results stay on the panel — only work still in flight is abandoned, since
            // nothing will be on screen to receive it.
            self.cancel_project_search();
            window.focus(&self.focus_handle);
            cx.notify();
            return;
        }

        self.sidebar = Sidebar::Search;
        window.focus(&handle);

        // Seed from the selection, the way ⌘F does — but only when it actually changed the
        // query, so reopening on the same word does not re-walk the project, and an empty
        // selection does not kick off a search for the empty string.
        let seeded = self
            .active_editor()
            .and_then(|editor| editor.read(cx).document.selected_text())
            .is_some_and(|text| panel.update(cx, |panel, _cx| panel.seed(&text)));
        if seeded {
            // Immediately, not debounced: the user selected the word and pressed the key,
            // which is as explicit a "search for this" as pressing return.
            self.schedule_project_search(false, cx);
        }
        cx.notify();
    }

    /// The search panel, built on first use.
    ///
    /// Lazy because the subscription has to be `subscribe_in` — a result click opens a file
    /// and opening focuses (#102), which needs a `Window` all the way down — and
    /// `WorkspaceView::new` has no window to hand it. The git panel is built eagerly there
    /// because its events never open anything.
    fn show_search_panel(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Entity<SearchPanel> {
        if let Some(panel) = self.search_panel.clone() {
            return panel;
        }

        let panel = cx.new(SearchPanel::new);
        cx.subscribe_in(&panel, window, |this, _panel, event, window, cx| match event {
            SearchPanelEvent::QueryChanged => this.schedule_project_search(true, cx),
            SearchPanelEvent::SearchNow => this.schedule_project_search(false, cx),
            SearchPanelEvent::OpenResult { file, line } => {
                this.open_search_result(*file, *line, window, cx)
            }
            SearchPanelEvent::Dismissed => this.unfocus_search_panel(window, cx),
        })
        .detach();

        self.search_panel = Some(panel.clone());
        panel
    }

    /// Escape in the panel: focus goes back to the editor, the panel and its results stay.
    ///
    /// Deliberately not `close_search_panel`. The find bar's escape closes and clears
    /// because its matches are painted over the document and there is no other way to be
    /// rid of them; a project-search result list is a thing you read while editing, and
    /// destroying a second of work on a stray key is the opposite of what escape should do.
    fn unfocus_search_panel(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(editor) = self.active_editor().cloned() {
            window.focus(&editor.read(cx).focus_handle(cx));
        } else {
            window.focus(&self.focus_handle);
        }
        cx.notify();
    }

    /// Starts a project search, after `SEARCH_DEBOUNCE` when `debounce` is set.
    ///
    /// The three things ADR-0007 and #80 require, in one place:
    ///
    /// - **Cancellation, not queueing.** The old search's flag is raised before the new one
    ///   is built, so a superseded sweep abandons within one file rather than finishing and
    ///   overwriting the newer results. `Job::ProjectSearch` then drops the old task.
    /// - **Off the UI thread.** `cx.background_spawn`; the measured 7 ms would drop a frame.
    /// - **Debounced.** See [`SEARCH_DEBOUNCE`] for the interval and why it is that number.
    ///
    /// The panel goes to `Searching` immediately — *before* the debounce elapses, not after
    /// — so the header changes on the keystroke rather than a quarter second later. It keeps
    /// showing the previous results underneath while it does, which is why `SearchState`
    /// carries them.
    fn schedule_project_search(&mut self, debounce: bool, cx: &mut Context<Self>) {
        let Some(panel) = self.search_panel.clone() else { return };
        let Some(root) = self.tree.as_ref().map(|tree| tree.root().to_path_buf()) else { return };

        self.cancel_project_search();

        let query = panel.read(cx).query().clone();
        if query.is_empty() {
            // Not "search for nothing": an empty field means the panel goes back to Idle
            // without walking the project. `search_project` would return early anyway; the
            // point is not to spawn a task and not to say "Searching…".
            panel.update(cx, |panel, cx| panel.set_state(SearchState::Idle, cx));
            return;
        }

        let previous = match panel.read(cx).state() {
            SearchState::Idle => Default::default(),
            SearchState::Searching(results) | SearchState::Done(results) => results.clone(),
        };
        panel.update(cx, |panel, cx| panel.set_state(SearchState::Searching(previous), cx));

        let cancel = CancelFlag::new();
        self.search_cancel = Some(cancel.clone());

        let task = cx.spawn(async move |this, cx| {
            if debounce {
                cx.background_executor().timer(SEARCH_DEBOUNCE).await;
                // The timer is inside the task rather than in a separate slot precisely so
                // that a superseding keystroke drops it here, before any work starts.
                if cancel.is_cancelled() {
                    return;
                }
            }

            let scan_cancel = cancel.clone();
            let results = cx
                .background_spawn(async move { search_project(&root, &query, &scan_cancel) })
                .await;

            // A cancelled sweep returns whatever it had. Showing a partial list for a query
            // the user has already typed past is worse than showing the previous complete
            // one, so it is dropped rather than displayed.
            if cancel.is_cancelled() || results.cancelled {
                return;
            }

            panel.update(cx, |panel, cx| panel.set_state(SearchState::Done(results), cx)).ok();
            this.update(cx, |_, cx| cx.notify()).ok();
        });
        self.jobs.start(Job::ProjectSearch, task);
    }

    fn cancel_project_search(&mut self) {
        if let Some(cancel) = self.search_cancel.take() {
            cancel.cancel();
        }
        self.jobs.cancel(Job::ProjectSearch);
    }

    /// Clicking a result row: open the file and put the cursor on the hit.
    ///
    /// `open_path_at` (#88), not a second jump path. The plumbing that carries an optional
    /// `Point` through the asynchronous open already exists and already handles the file
    /// being open, not open, or open in another tab — reimplementing it here is how two
    /// jump paths drift into behaving differently.
    fn open_search_result(
        &mut self,
        file: usize,
        line: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(panel) = self.search_panel.as_ref() else { return };
        let Some(results) = (match panel.read(cx).state() {
            SearchState::Idle => None,
            SearchState::Searching(results) | SearchState::Done(results) => Some(results),
        }) else {
            return;
        };
        let Some(matches) = results.files.get(file) else { return };
        let Some(hit) = matches.lines.get(line) else { return };

        // Both zero-based: `LineMatch::row` is, and `column` is a *byte* offset in the raw
        // line, which is what `Point` means by a column everywhere else in this codebase.
        let point = Point::new(hit.row as usize, hit.column as usize);
        let path = matches.path.clone();
        // `open_path_at` takes a `Window` since #102, and this is one of the callers that
        // change anticipated: a result click is a command-driven jump, so the keyboard has
        // to follow the cursor into the file or the next keystroke goes nowhere.
        self.open_path_at(path, Some(point), window, cx);
    }

    // --- palette -----------------------------------------------------------------

    fn toggle_command_palette(
        &mut self,
        _: &ToggleCommandPalette,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.toggle_palette(PaletteMode::Commands, window, cx);
    }

    fn toggle_quick_open(
        &mut self,
        _: &ToggleQuickOpen,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.toggle_palette(PaletteMode::Files, window, cx);
    }

    /// Route search, for the View menu. Unbound: every route-ish chord is taken, and #62
    /// only needed a menu item, not a shortcut.
    fn go_to_route(&mut self, _: &GoToRoute, window: &mut Window, cx: &mut Context<Self>) {
        self.toggle_palette(PaletteMode::Routes, window, cx);
    }

    /// ⌥⌘I: open the completion popup at the cursor (#61).
    ///
    /// Both sources are asked, and each answers about a different thing: Laravel knows route
    /// names inside a `route('…')` and nothing else, the language server knows identifiers
    /// and knows nothing about a string literal. They rarely both have something to say, and
    /// when they do the list shows both with their badges — which is the whole point of
    /// carrying the source in the item.
    ///
    /// Silent with no tab, and silent with no server: the popup opens for whatever answered,
    /// and if nothing answered it closes itself rather than sitting there saying "No
    /// completions" about a question the user's setup cannot answer (#74, §24).
    fn complete(&mut self, _: &Complete, window: &mut Window, cx: &mut Context<Self>) {
        self.open_completion(CompletionTrigger::Invoked, window, cx);
    }

    /// The `laravel.route_name` palette command (#83), now opening the popup.
    ///
    /// Kept as its own entry point because the command row exists and people may have
    /// learned it; it is no longer bound to a key, since ⌥⌘I is now the general
    /// completion this command was standing in for.
    fn complete_laravel(
        &mut self,
        _: &CompleteLaravel,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.open_completion(CompletionTrigger::Invoked, window, cx);
    }

    // --- completion (#61) ---------------------------------------------------------------

    /// A character typed into the editor with no popup open: open one if the server asked.
    ///
    /// # Where the list of trigger characters comes from
    ///
    /// [`Capabilities::completion_triggers`](elle_lsp::Capabilities::completion_triggers),
    /// which is the server's own declaration read off the `initialize` response. **Nothing
    /// here knows that PHP spells member access `->`.** A real Intelephense declares
    /// `["$", ">", ":", "\\", "/", "'", "\"", "*", ".", "<"]` — ten single characters, not
    /// the two-character sequences a hardcoded implementation would have matched, which is
    /// itself the argument against hardcoding: the obvious guess is the wrong shape.
    ///
    /// A different backend declaring a different set therefore works with no change here,
    /// which is the substitutability RISKS.md #2 is about, and
    /// `crates/app/tests/architecture.rs` fails the build if a backend name ever appears
    /// alongside this logic.
    ///
    /// # Why a trigger is not just the explicit chord fired automatically
    ///
    /// It fires on every keystroke of a matching character in every context — inside a
    /// string, inside a comment, in the middle of a word. Three things follow, and each is
    /// handled somewhere different:
    ///
    /// - **Context.** Measured rather than guessed: against a real Intelephense, `->` inside
    ///   a single-quoted string, a double-quoted string, a line comment and a block comment
    ///   each returned **zero** items. The server already knows PHP's grammar and we do not
    ///   need to re-derive it — attempting to would mean the editor holding a second, worse
    ///   model of when a completion is appropriate. So the request goes out and the empty
    ///   answer closes the popup.
    /// - **Emptiness.** [`CompletionTrigger::Character`] does not render "No completions",
    ///   because the answer to a question nobody asked is not worth a box.
    /// - **Cost.** No debounce, and that is a measurement rather than a preference. On a
    ///   10,061-file project with a 199 MB `vendor/`, the *first* completion request issued
    ///   478 ms after spawning Intelephense answered in **15 ms**, and the warm p50 was
    ///   1.4 ms. A 250 ms debounce — find-in-project's figure from #103 — would add sixteen
    ///   times the measured cost as pure latency to hide work that is not there. #103's
    ///   number is a fact about walking a directory tree, not a constant, and importing it
    ///   here would be the mistake `BASELINE.md` opens by warning about.
    fn editor_typed(&mut self, text: &str, window: &mut Window, cx: &mut Context<Self>) {
        // Already open: this event cannot fire, because the popup holds focus and the
        // character reaches `completion_typed` instead. Guarded anyway — the two paths
        // narrowing the same popup would double every character in the query.
        //
        // Two ways a keystroke opens the popup. A server *trigger* character (`$`, `->`
        // via `>`, `::` via `:`) opens it always. But the owner's report was that the
        // LSP "felt weak" because it only popped on those symbols — VS Code/Zed pop while
        // you type an identifier. So a *word* character opens it too, once a small prefix
        // exists, so completing an ordinary name works without ⌥⌘I. The prefix floor
        // keeps a single letter from opening a hundred-row list on every keystroke.
        let declared = self.is_completion_trigger(text)
            || self.should_open_on_word_char(text, cx);
        self.schedule_autosave(cx);
        if !Self::should_open_on_trigger(self.completion.is_some(), declared) {
            return;
        }
        self.open_completion(CompletionTrigger::Character, window, cx);
    }

    /// Arms (or re-arms) the autosave debounce — a save one second after the last
    /// keystroke, when autosave is on. Each call supersedes the pending timer through the
    /// job slot, so typing continuously never saves mid-word; pausing does. This is the
    /// trigger the owner expected (VS Code's afterDelay); the window-blur save stays as a
    /// backstop for when you leave without pausing.
    fn schedule_autosave(&mut self, cx: &mut Context<Self>) {
        if !crate::settings::current(cx).autosave() {
            return;
        }
        let task = cx.spawn(async move |this, cx| {
            cx.background_executor().timer(AUTOSAVE_DEBOUNCE).await;
            this.update(cx, |this, cx| this.autosave_dirty_tabs(cx)).ok();
        });
        self.jobs.start(Job::AutosaveDebounce, task);
    }

    /// Whether typing `text` (one keystroke) should open the popup as an
    /// autocomplete-as-you-type, the VS Code behaviour (#20 follow-up).
    ///
    /// Only when a language server is present (buffer words alone are too weak a source
    /// to interrupt with), only for a word character, and only once the word under the
    /// cursor is at least `MIN_AUTOCOMPLETE_PREFIX` long — so `u`, `us` stay quiet and
    /// `use` opens. That floor is what keeps this from firing a request on every letter.
    fn should_open_on_word_char(&self, text: &str, cx: &Context<Self>) -> bool {
        if self.lsp.client().is_none() {
            return false;
        }
        let Some(editor) = self.active_editor() else { return false };
        let document = &editor.read(cx).document;
        let offset = document.selection.head;
        let buffer_text = document.buffer.text();
        let prefix = crate::completion::word_before(&buffer_text, offset);
        word_char_reaches_prefix_floor(text, prefix)
    }

    /// Whether a character typed in the editor should open a popup.
    ///
    /// Split out from [`Self::editor_typed`] as a pure predicate so both of its conditions
    /// are testable, which they are not inside the handler: a headless test has no language
    /// server, so `declared` is always false there and the already-open guard can never be
    /// reached through the real path. That is not a hypothetical gap — the first version of
    /// this was written inside `editor_typed`, and the test named for the already-open case
    /// passed with the guard deleted.
    ///
    /// The ordering the caller uses is the cheap check first. This function states the rule
    /// independently of that, so it stays true if the order ever changes.
    fn should_open_on_trigger(popup_is_open: bool, declared: bool) -> bool {
        declared && !popup_is_open
    }

    /// Whether the server declared `text` as a completion trigger.
    ///
    /// A whole-string comparison against each declared trigger rather than a per-character
    /// scan. The specification lets a server declare a multi-character trigger, and a
    /// `contains` over characters would fire on the `>` inside `=>` while claiming to
    /// implement whatever the server actually said. One keystroke produces one `key_char`,
    /// so equality is the honest test of "the user just typed this trigger".
    fn is_completion_trigger(&self, text: &str) -> bool {
        let Some(client) = self.lsp.client() else { return false };
        client.capabilities().completion_triggers.iter().any(|trigger| trigger == text)
    }

    /// Opens the popup at the cursor and asks every source.
    ///
    /// The order here is load-bearing. The popup is created *first*, with whatever is
    /// synchronously available, because both sources are asynchronous and a popup that
    /// appears only once the server answers is a popup that appears after the user has
    /// typed three more characters. It fills in as answers land.
    fn open_completion(
        &mut self,
        trigger: CompletionTrigger,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // A second invoke with the popup already open re-asks rather than toggling: the
        // list is not a panel being shown, it is an answer about a position, and pressing
        // the key again means "I have typed since you asked".
        self.dismiss_completion(window, cx);

        let Some(editor) = self.active_editor().cloned() else { return };

        let (word_start, prefix, offset) = {
            let document = &editor.read(cx).document;
            let offset = document.selection.head;
            let text = document.buffer.text();
            let prefix = word_before(&text, offset).to_string();
            (offset - prefix.len(), prefix, offset)
        };

        let Some(origin) = self.completion_origin(&editor, window, cx) else { return };

        let popup = cx.new(|cx| CompletionPopup::new(Vec::new(), prefix, origin, trigger, cx));
        cx.subscribe_in(&popup, window, |this, _popup, event, window, cx| match event {
            CompletionEvent::Accepted(item) => this.accept_completion(item.clone(), window, cx),
            CompletionEvent::Dismissed => this.dismiss_completion(window, cx),
            CompletionEvent::Typed(text) => this.completion_typed(text, window, cx),
            CompletionEvent::Backspaced => this.completion_backspace(window, cx),
        })
        .detach();

        // Focus moves to the popup, which is what activates its key context and therefore
        // what makes `up`/`down` reach the list. Typing is forwarded back to the editor by
        // `completion_typed`, so the buffer still receives every character.
        let handle = popup.read(cx).focus_handle(cx);
        window.focus(&handle);

        // Anything that takes focus away closes the popup. The failure this prevents is a
        // popup left on screen *unfocused*: its key context is then inactive, so Escape no
        // longer reaches it and it cannot be dismissed at all, while it still holds an
        // offset a later accept would write at.
        //
        // Belt and braces, deliberately. This subscription is the general rule and covers
        // panels nobody has written yet, but it fires from gpui's focus *path*, which is
        // assembled during paint — so it cannot be exercised by a headless test, and a rule
        // that cannot be tested should not be the only thing holding. The specific commands
        // that take focus (⌘F, ⌃`, ⌘⇧F, the palette, closing a tab) therefore each dismiss
        // explicitly too, and those calls are what the tests pin.
        //
        // The subscription is dropped with the popup, so it costs nothing while none is open.
        let this = cx.entity().downgrade();
        let subscription = window.on_focus_out(&handle, cx, move |_event, window, cx| {
            this.update(cx, |this, cx| this.dismiss_completion(window, cx)).ok();
        });
        self.completion_focus_out = Some(subscription);
        self.completion = Some(popup.clone());
        self.completion_word_start = Some((editor.clone(), word_start));

        self.request_route_completions(popup.clone(), cx);
        self.request_column_completions(popup.clone(), cx);
        self.request_wire_completions(popup.clone(), cx);
        self.request_lsp_completions(popup, offset, window, cx);
        cx.notify();
    }

    /// Closes a character-triggered popup that every source has answered and left empty.
    ///
    /// The three conditions are each load-bearing and none is redundant:
    ///
    /// - **Triggered by a character**, not invoked. ⌥⌘I must still show "No completions",
    ///   because the user asked and silence would read as a dead keybinding.
    /// - **Loaded**, so this is not fired while a source is still thinking. Closing early
    ///   would make the popup flicker shut just as the server's answer lands.
    /// - **Empty**, which is the whole point.
    ///
    /// This is where the string-and-comment question is answered, and the answer is that we
    /// do not answer it — the *server* does. Intelephense returns nothing for `->` inside a
    /// string or a comment, so no popup appears there, and it does so knowing PHP's grammar
    /// including heredocs, interpolation and nested comments. Re-deriving that here would
    /// mean the editor keeping a second, worse model of PHP syntax and disagreeing with the
    /// server about it, which is the same class of confident wrongness RISKS.md #4 forbids.
    fn close_if_empty_trigger(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(popup) = self.completion.clone() else { return };
        let popup = popup.read(cx);
        let should_close =
            !popup.trigger().reports_emptiness() && popup.is_loaded() && popup.is_empty();
        if should_close {
            self.dismiss_completion(window, cx);
        }
    }

    /// Where on screen the popup goes, in window coordinates.
    ///
    /// The cursor position arrives already window-absolute, because the editor *measured*
    /// it from a laid-out row rather than adding up the chrome above itself. That is the
    /// whole reason there is no tab-bar or find-bar height in this function: the find bar's
    /// height varies with whether it is showing a replace field, and a constant here would
    /// be the same class of bug `text_origin_x` was introduced to fix.
    fn completion_origin(
        &self,
        editor: &Entity<EditorView>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<gpui::Point<gpui::Pixels>> {
        let fonts = Fonts::get(cx);
        let cursor = editor.read(cx).cursor_position(window, cx)?;

        // Placed for a full-height list up front rather than re-placed as items arrive: a
        // popup that jumps from below the cursor to above it when the server answers is
        // worse than one that occasionally sits higher than it needed to.
        Some(crate::completion::place(
            cursor,
            fonts.line_height(),
            crate::completion::popup_height(crate::completion::MAX_VISIBLE_ROWS),
            window.viewport_size(),
        ))
    }

    /// Offers a Livewire component's actions or properties inside a `wire:` value (#24).
    ///
    /// Blade only, and only when the view resolves to a component class by the
    /// convention (`livewire/user-table.blade.php` → `app/Livewire/UserTable.php`) —
    /// silence otherwise, because a guessed class is a wrong list wearing a badge. The
    /// scanner decides which list the attribute wants; offering both would bury the
    /// three real actions under every property.
    fn request_wire_completions(&mut self, popup: Entity<CompletionPopup>, cx: &mut Context<Self>) {
        let Some(root) = self.tree.as_ref().map(|tree| tree.root().to_path_buf()) else { return };
        let Some(tab) = self.tabs.get(self.active_tab) else { return };
        let Some(path) = tab.path.clone() else { return };
        if laravel_dialect(&path) != Some(true) {
            return;
        }
        let editor = tab.editor.clone();
        let document = &editor.read(cx).document;
        let text = document.buffer.text();
        let offset = document.selection.head;

        let Some((target, range)) = elle_laravel::wire_context_at(&text, offset) else { return };
        // Widen range and query together — both or neither (the route source's rule).
        let offset = char_boundary_at_or_below(&text, offset);
        if range.start <= offset && range.end <= offset {
            self.completion_word_start = Some((editor.clone(), range.start));
            let typed = text[range.start..offset].to_string();
            popup.update(cx, |popup, cx| popup.set_query(typed, cx));
        }

        let task = cx.background_spawn(async move {
            let class_path = elle_laravel::livewire_class_path(&root, &path)?;
            let source = std::fs::read_to_string(class_path).ok()?;
            elle_laravel::extract_livewire(&source)
        });
        let task = cx.spawn(async move |_this, cx| {
            let Some(facts) = task.await else { return };
            let (names, kind) = match target {
                elle_laravel::WireTarget::Action => (facts.actions, "action"),
                elle_laravel::WireTarget::Property => (facts.properties, "property"),
            };
            let items: Vec<CompletionItem> = names
                .into_iter()
                .map(|name| {
                    CompletionItem::new(name, CompletionSource::Livewire)
                        .with_detail(Some(format!("{kind} · {}", facts.class)))
                })
                .collect();
            if items.is_empty() {
                return;
            }
            popup.update(cx, |popup, cx| popup.add_items(items, cx)).ok();
        });
        self.jobs.start(Job::CompletionColumns, task);
    }

    /// Asks Laravel for route names, when the cursor is inside a `route('…')`.
    ///
    /// Nothing at all otherwise, which is the same rule #83 established: route names are an
    /// answer to "what goes in this string literal", and offering them anywhere else would
    /// put a hundred irrelevant rows above the identifier the user is actually typing.
    ///
    /// Only the names that were statically readable, which `route_names` already enforces.
    /// **An incomplete list is the acceptable failure here** (#83) and the reason completion
    /// is the feature rather than a diagnostic: a route registered by a service provider is
    /// missing from this list, and the user types it themselves. The same gap expressed as
    /// "this route does not exist" would be a false claim about working code (RISKS.md #4).
    /// The `route` badge is what keeps that honest at the point the user reads the list.
    ///
    /// Runs in `Job::CompletionRoutes`, its *own* slot — see that variant for why sharing
    /// the route palette's was a silent way for the popup to end up claiming there were no
    /// completions when it had simply lost its task.
    fn request_route_completions(
        &mut self,
        popup: Entity<CompletionPopup>,
        cx: &mut Context<Self>,
    ) {
        let Some(root) = self.tree.as_ref().map(|tree| tree.root().to_path_buf()) else { return };
        let Some(tab) = self.tabs.get(self.active_tab) else { return };
        let Some(path) = tab.path.clone() else { return };
        let Some(blade) = laravel_dialect(&path) else { return };
        let tab_editor = tab.editor.clone();

        let document = &tab_editor.read(cx).document;
        let source = document.buffer.text();
        let offset = document.selection.head;

        let Some(reference) = elle_laravel::reference_at(&source, offset, blade) else { return };
        if reference.kind != elle_laravel::ReferenceKind::Route {
            return;
        }

        // Widen the replaced range to the whole literal, and widen the popup's query to
        // match it. **Both, or neither** — they are two views of one span, and moving only
        // the range is a bug I wrote and caught here: a route name is dotted, `word_before`
        // stops at the `.`, so in `route('users.sh|')` the query was `sh` while the range
        // started at `users`. Accepting `users.show` then wrote the full name over a range
        // beginning at `u`, giving `users.users.show` — an off-by-one-word that only shows
        // up on names with a dot, which is most of them.
        //
        // The cursor must be *inside* the literal, both ends. `reference_at` matches
        // inclusively on `start..=end`, so an invoke with the caret mid-name — after `users.`
        // in an existing `route('users.show')` — is reachable, and there the text after the
        // cursor is not part of what the accept replaces. Taking the whole literal as the
        // query while replacing only `start..cursor` gives `users.showshow`: the same
        // doubling, from the other side. Declining leaves the generic word scan in charge,
        // which is narrower but never wrong.
        if reference.range.start <= offset && reference.range.end <= offset {
            let offset = char_boundary_at_or_below(&source, offset);
            self.completion_word_start = Some((tab_editor.clone(), reference.range.start));
            let typed = source[reference.range.start..offset].to_string();
            popup.update(cx, |popup, cx| popup.set_query(typed, cx));
        }

        let task = cx.spawn(async move |_this, cx| {
            let names = cx.background_spawn(async move { elle_laravel::route_names(&root) }).await;
            let items = names
                .into_iter()
                // The source is named at construction, which is the only place it can be
                // known for certain — these came from a route file, and nothing downstream
                // has to infer it from the shape of the string.
                .map(|name| CompletionItem::new(name, CompletionSource::LaravelRoute))
                .collect();
            popup.update(cx, |popup, cx| popup.add_items(items, cx)).ok();
        });
        self.jobs.start(Job::CompletionRoutes, task);
    }

    /// Offers the model's own columns, when the file being edited *is* a model (#22).
    ///
    /// The first consumer of #21's index, shaped by the same honesty rules as routes:
    /// items appear only where the claim holds (the class under edit extends Model —
    /// judged by `extract_model` on the live buffer, so an ordinary class gets nothing),
    /// each item's detail carries its provenance (`string · migration` is a different
    /// promise than `boolean · cast`), and an empty or missing index contributes
    /// nothing silently — the index is a cache and this source must survive its absence
    /// (ADR-0008).
    ///
    /// Buffer-text detection rather than a path heuristic: a model outside `app/Models`
    /// still completes, and a helper class inside it still does not.
    fn request_column_completions(
        &mut self,
        popup: Entity<CompletionPopup>,
        cx: &mut Context<Self>,
    ) {
        let Some(root) = self.tree.as_ref().map(|tree| tree.root().to_path_buf()) else { return };
        let Some(editor) = self.active_editor() else { return };
        let text = editor.read(cx).document.buffer.text();
        let offset = editor.read(cx).document.selection.head;

        // A `where('…')` context wins over the file-wide source: it is the more precise
        // claim (the literal names a column of *that* class), and running both would put
        // every column in the list twice. Only when no context holds does "the file is a
        // model" offer the whole surface.
        if let Some(context) = elle_laravel::column_context_at(&text, offset) {
            let class = match context.target {
                elle_laravel::ColumnTarget::Class(name) => name,
                // `$this->where(` — the class is whatever model this file declares; a
                // `$this` in a non-model class gets nothing, same honesty as the scanner.
                elle_laravel::ColumnTarget::This => {
                    let Some(facts) = elle_laravel::extract_model(&text) else { return };
                    facts.class
                }
            };
            // Widen range and query together — both or neither, the rule the route
            // source established (see `request_route_completions` for the two bugs the
            // halves each cause). Declined when the caret sits mid-literal.
            if context.range.start <= offset && context.range.end <= offset {
                let offset = char_boundary_at_or_below(&text, offset);
                self.completion_word_start = Some((editor.clone(), context.range.start));
                let typed = text[context.range.start..offset].to_string();
                popup.update(cx, |popup, cx| popup.set_query(typed, cx));
            }
            // The scanner says which list this literal wants; the other one would be a
            // wrong answer wearing a confident badge.
            match context.expects {
                elle_laravel::Argument::Column => self.request_columns_of(class, root, popup, cx),
                elle_laravel::Argument::Relation => {
                    self.request_relations_of(class, root, popup, cx)
                }
            }
            return;
        }

        // `User::ac` mid-typing: the scanner reads the class off the prefix, and the
        // items are the scopes by *call* name — the one list where the declared method
        // name (the server's answer) is exactly what the user must not type.
        if let Some((class, _)) = elle_laravel::scope_context_at(&text, offset) {
            let task = cx.background_spawn(async move {
                let path = crate::file_cache::index_path(&root)?;
                let (index, _) = elle_index::Index::open(&path).ok()?;
                elle_index::laravel::scopes_for_model(index.connection(), &class).ok()
            });
            let task = cx.spawn(async move |_this, cx| {
                let Some(scopes) = task.await else { return };
                let items: Vec<CompletionItem> = scopes
                    .into_iter()
                    .map(|name| CompletionItem::new(name, CompletionSource::LaravelScope))
                    .collect();
                if items.is_empty() {
                    return;
                }
                popup.update(cx, |popup, cx| popup.add_items(items, cx)).ok();
            });
            self.jobs.start(Job::CompletionColumns, task);
            return;
        }

        let Some(facts) = elle_laravel::extract_model(&text) else { return };
        let class = facts.class;

        let task = cx.background_spawn(async move {
            let path = crate::file_cache::index_path(&root)?;
            let (index, _) = elle_index::Index::open(&path).ok()?;
            let conn = index.connection();
            let columns = elle_index::laravel::columns_for_model(conn, &class).ok()?;
            let relations = elle_index::laravel::relations_for_model(conn, &class).ok()?;
            Some((columns, relations))
        });

        let task = cx.spawn(async move |_this, cx| {
            let Some((columns, relations)) = task.await else { return };
            let mut items: Vec<CompletionItem> = columns.into_iter().map(column_item).collect();
            // Relationships after columns: both are index facts, but a column is data on
            // every row while a relationship is one method — and the detail says what the
            // method body claims (`hasMany · Post`), which is a scan's word, not a proof.
            items.extend(relations.into_iter().map(relation_item));
            if items.is_empty() {
                return;
            }
            popup.update(cx, |popup, cx| popup.add_items(items, cx)).ok();
        });
        self.jobs.start(Job::CompletionColumns, task);
    }

    /// Fetches one class's columns from the index and feeds them to the popup — the tail
    /// both column sources share. Columns only, no relationships: `where('…')` accepts a
    /// column name, and offering `posts` there would be a wrong answer wearing a badge.
    fn request_columns_of(
        &mut self,
        class: String,
        root: std::path::PathBuf,
        popup: Entity<CompletionPopup>,
        cx: &mut Context<Self>,
    ) {
        let task = cx.background_spawn(async move {
            let path = crate::file_cache::index_path(&root)?;
            let (index, _) = elle_index::Index::open(&path).ok()?;
            elle_index::laravel::columns_for_model(index.connection(), &class).ok()
        });
        let task = cx.spawn(async move |_this, cx| {
            let Some(columns) = task.await else { return };
            let items: Vec<CompletionItem> = columns.into_iter().map(column_item).collect();
            if items.is_empty() {
                return;
            }
            popup.update(cx, |popup, cx| popup.add_items(items, cx)).ok();
        });
        self.jobs.start(Job::CompletionColumns, task);
    }

    /// Fetches one class's relationships from the index and feeds them to the popup —
    /// what a `with('…')`-shaped literal wants. Relations only, for the mirror of
    /// `request_columns_of`'s reason.
    fn request_relations_of(
        &mut self,
        class: String,
        root: std::path::PathBuf,
        popup: Entity<CompletionPopup>,
        cx: &mut Context<Self>,
    ) {
        let task = cx.background_spawn(async move {
            let path = crate::file_cache::index_path(&root)?;
            let (index, _) = elle_index::Index::open(&path).ok()?;
            elle_index::laravel::relations_for_model(index.connection(), &class).ok()
        });
        let task = cx.spawn(async move |_this, cx| {
            let Some(relations) = task.await else { return };
            let items: Vec<CompletionItem> = relations.into_iter().map(relation_item).collect();
            if items.is_empty() {
                return;
            }
            popup.update(cx, |popup, cx| popup.add_items(items, cx)).ok();
        });
        self.jobs.start(Job::CompletionColumns, task);
    }

    /// Adds the buffer's own words to the popup — the no-server degradation (#20).
    ///
    /// Synchronous: the words come from the buffer already in memory, so there is no IO
    /// to move off the main thread, and a task would only add a frame of latency to the
    /// exact case (no server) where this is the whole list.
    fn offer_buffer_words(
        &mut self,
        popup: &Entity<CompletionPopup>,
        offset: usize,
        cx: &mut Context<Self>,
    ) {
        let Some(editor) = self.active_editor() else { return };
        let text = editor.read(cx).document.buffer.text();
        let typed = crate::completion::word_before(&text, offset);
        // The typed word is the only signal this source has. With none — the cursor
        // after `$user->`, say — every word of the file is noise, so nothing is offered;
        // mid-word (⌘⌥I on `use|`) is the moment this list is an answer.
        if typed.is_empty() {
            return;
        }
        let items: Vec<CompletionItem> = crate::completion::buffer_words(&text, typed)
            .into_iter()
            .map(|word| CompletionItem::new(word, CompletionSource::Buffer))
            .collect();
        if items.is_empty() {
            return;
        }
        popup.update(cx, |popup, cx| popup.add_items(items, cx));
    }

    /// Asks the language server, without blocking, and cancels whatever it supersedes.
    ///
    /// This is the first caller of `request_completion` (#45), which has existed uncalled
    /// since it was written. The non-blocking variant is what #61 asks for: the request is
    /// issued, its id is kept, and the *next* keystroke sends `$/cancelRequest` for it
    /// before issuing its own. Nothing queues.
    ///
    /// Takes a `Window` because the continuation may need to *close* the popup — a trigger
    /// that found nothing — and dismissing moves focus back to the editor.
    fn request_lsp_completions(
        &mut self,
        popup: Entity<CompletionPopup>,
        offset: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // No server is the common case and stays silent — it must not say "no language
        // server" (#74). What it degrades to is the buffer's own words (#20): the weakest
        // source, but a popup of this file's identifiers beats a popup of nothing.
        let Some((uri, _)) = self.navigation_origin(cx) else {
            self.offer_buffer_words(&popup, offset, cx);
            popup.update(cx, |popup, cx| popup.mark_loaded(cx));
            return;
        };
        // Cancelled *before* the new request goes out, not after. Issuing first left a
        // window in which the previous request's task could resolve, clear the shared slot,
        // and make the cancel that followed find `None` and send nothing — leaking one
        // uncancelled request per keystroke, which is precisely what this design exists to
        // prevent.
        self.cancel_completion_query();

        // The buffer as it is *now*, for the resync below.
        let text = self.active_editor().map(|editor| editor.read(cx).document.buffer.text());

        let Some(client) = self.lsp.client_mut() else {
            // Same degradation as above — an origin without a live client is still a
            // buffer full of words.
            self.offer_buffer_words(&popup, offset, cx);
            popup.update(cx, |popup, cx| popup.mark_loaded(cx));
            return;
        };

        // Resync the document before asking. **Without this the popup can never show
        // anything in a real session**, and it took a live one to find out: `did_change`
        // goes to the server on save, not on keystroke (see `document_saved`), so its copy
        // of the file is the one from open. The user types `$this->`, the request asks
        // about an offset where the server's copy has no `$this->` — usually pointing into
        // older text, or past the end of a line — and Intelephense correctly answers an
        // empty list. `close_if_empty_trigger` then closes a popup that never rendered a
        // frame, ~1ms after it opened (the measured warm p50 is 1.4ms), which on screen is
        // indistinguishable from the popup never opening. That is the remaining piece of
        // #125 after the server itself was fixed to start.
        //
        // Full-text rather than incremental, for `document_saved`'s reason: the workspace
        // sees the buffer after the edit and holds no `Edit`, and PHP files are small.
        // Cost is one notification per request issued — completion-rate, not typing-rate.
        if let Some(text) = &text
            && let Err(err) = client.did_change_full(&uri, text)
        {
            tracing::debug!("could not resync before completing: {err:#}");
        }

        let id = match client.request_completion(&uri, offset) {
            Ok(id) => id,
            Err(err) => {
                tracing::debug!("could not ask for completions: {err:#}");
                popup.update(cx, |popup, cx| popup.mark_loaded(cx));
                return;
            }
        };

        let query = id.clone();
        let task = cx.spawn_in(window, async move |this, cx| {
            let found = Self::poll_query::<CompletionResponse>(&this, &query, cx).await;
            // Compare before clearing. An unconditional clear lets a slow task wipe the slot
            // belonging to the request that *superseded* it, after which the next keystroke
            // cancels nothing and that newer request leaks instead.
            this.update(cx, |this, _| {
                if this.completion_query.as_ref() == Some(&query) {
                    this.completion_query = None;
                }
            })
            .ok();

            let (items, incomplete) = match found {
                Ok(Some(response)) => completion_items(response),
                // No answer is an ordinary outcome — a keyword, a comment, a position the
                // server has nothing for. The popup simply has no LSP rows.
                Ok(None) => (Vec::new(), false),
                Err(err) => {
                    tracing::debug!("completion request failed: {err:#}");
                    (Vec::new(), false)
                }
            };

            popup
                .update(cx, |popup, cx| {
                    // Replace rather than append: this may be a *re-request* for a longer
                    // prefix, and the previous truncated answer describes a position the
                    // user has typed past. Scoped to the LSP source so the route names
                    // Laravel found — which nobody re-asked — survive.
                    popup.set_incomplete(incomplete);
                    popup.replace_items(CompletionSource::Lsp, items, cx);
                    popup.mark_loaded(cx);
                })
                .ok();

            // A trigger that turned up nothing closes rather than reporting. This is the
            // string-and-comment case: measured against a real Intelephense, `->` inside a
            // string or either kind of comment answers with an empty list, and the honest
            // rendering of "the server had nothing to say about a character you typed while
            // writing code" is no popup at all.
            this.update_in(cx, |this, window, cx| this.close_if_empty_trigger(window, cx)).ok();
        });

        // The cancel already happened, above, before the request went out.
        self.completion_query = Some(id);
        self.jobs.start(Job::Completion, task);
    }

    /// Tells the server to stop computing a completion nobody will read.
    ///
    /// The two halves ADR-0007 asks for, and the reason this is not just a dropped task:
    /// dropping stops *us waiting*, `$/cancelRequest` stops the *server working* and
    /// reclaims the pending slot the abandoned request would otherwise hold for the life of
    /// the process. At typing speed this fires on every keystroke.
    fn cancel_completion_query(&mut self) {
        let Some(id) = self.completion_query.take() else { return };
        if let Some(client) = self.lsp.client_mut() {
            client.cancel(&id);
        }
    }

    /// A character typed while the popup has focus: insert it, and narrow the list.
    ///
    /// Both, and in that order. The popup holding focus must not mean the buffer stops
    /// receiving text — that would make the popup a modal state where typing is swallowed,
    /// which is the failure the palette-based stopgap had and the reason a popup was worth
    /// building.
    fn completion_typed(&mut self, text: &str, window: &mut Window, cx: &mut Context<Self>) {
        let Some(editor) = self.active_editor().cloned() else { return };
        let landed_as_typed = editor.update(cx, |editor, cx| editor.insert_typed(text, cx));
        // Typing with the popup open still edits the buffer, so it must re-arm autosave
        // too — otherwise a whole completion-driven edit session never triggers a save.
        self.schedule_autosave(cx);

        // The keystroke did something other than insert its own character — typed over an
        // auto-inserted closer, or opened a pair and wrote two. Mirroring it into the filter
        // anyway would leave the query describing a span the buffer does not have, and an
        // accept would then overwrite the bracket. Closing is the honest response: whatever
        // the user is doing with brackets, it is not finishing the word this list is about.
        if !landed_as_typed {
            self.dismiss_completion(window, cx);
            return;
        }

        let Some(popup) = self.completion.clone() else { return };
        // Nothing matches any more: the user has typed past the list, so it closes rather
        // than following them down the line as an empty box.
        let still_matching = popup.update(cx, |popup, cx| popup.push_query(text, cx));
        if !still_matching {
            self.dismiss_completion(window, cx);
            return;
        }

        // A request still in flight was asked about the offset *before* this character. Its
        // answer is for a position that no longer exists, and the narrowed list is already
        // on screen — so it is dropped rather than allowed to land and repopulate the popup
        // with items for the wrong prefix.
        //
        // This is the "typing quickly must drop in-flight work rather than queue it" half of
        // #20, and it is what makes the cancellation real rather than nominal: without it,
        // fast typing leaves one `$/cancelRequest`-less request per keystroke on the server.
        self.supersede_completion_query();

        // …and if the list on screen is a truncation, superseding is not enough: there has
        // to be a *new* request, because the rows matching the longer prefix may be the ones
        // the server cut off. See [`Self::rerequest_if_incomplete`].
        self.rerequest_if_incomplete(&popup, window, cx);
    }

    /// Re-asks the server when it said its previous answer was truncated (#61).
    ///
    /// # Why filtering the list we already have is not good enough
    ///
    /// `isIncomplete: true` is the server saying "this is not the whole answer". Treating it
    /// as though it were — narrowing the rows already on screen — silently under-reports, and
    /// the size of the gap is not marginal. Measured against a real Intelephense on a
    /// 10,061-file project:
    ///
    /// | prefix | server's own answer | filtering the previous answer |
    /// | ------ | ------------------- | ----------------------------- |
    /// | `str`  | 100 items, incomplete | —                           |
    /// | `strl` | 100 items, incomplete | **1 item**                  |
    ///
    /// Both lists contain `strlen`, so this is not the popup showing something *wrong* — it
    /// is the popup showing one row where the server had a hundred, because the other
    /// ninety-nine sat past a cap the server re-ranks against each new prefix. Under-reporting
    /// is the failure mode RISKS.md #4 names: a short list reads as "that is all there is".
    ///
    /// # Why this is affordable
    ///
    /// It fires per keystroke, so it is exactly the volume the cancellation machinery was
    /// built for — and the preceding `supersede_completion_query` has already dropped the
    /// task and sent `$/cancelRequest` before this issues anything. The measured cost of a
    /// completion request against that same project was 1.4 ms at the warm median and 15 ms
    /// for the very first request on a server 478 ms old, so there is no queue to build up.
    fn rerequest_if_incomplete(
        &mut self,
        popup: &Entity<CompletionPopup>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !popup.read(cx).is_incomplete() {
            return;
        }
        // The offset *after* the character just inserted, which is where the user now is.
        // Reading it from the editor rather than tracking it here is deliberate: the buffer
        // is the authority on where the caret ended up, and bracket auto-closing means an
        // arithmetic guess would be wrong exactly when it mattered.
        let Some(editor) = self.active_editor().cloned() else { return };
        let offset = editor.read(cx).document.selection.head;
        self.request_lsp_completions(popup.clone(), offset, window, cx);
    }

    /// Backspace while the popup has focus: delete, and widen the list.
    ///
    /// Backspacing past the point the popup opened at closes it — at that point the user is
    /// no longer editing the word the list is about.
    fn completion_backspace(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(editor) = self.active_editor().cloned() else { return };
        editor.update(cx, |editor, cx| editor.backspace_typed(cx));

        let Some(popup) = self.completion.clone() else { return };
        let still_open = popup.update(cx, |popup, cx| popup.pop_query(cx));
        if !still_open {
            self.dismiss_completion(window, cx);
            return;
        }
        // Same reasoning as typing: the cursor moved, so an in-flight answer is about a
        // position that no longer exists.
        self.supersede_completion_query();
        // And the same re-request, for the same reason in the other direction: a *shorter*
        // prefix matches more, so a truncated list is even less of the answer than it was.
        self.rerequest_if_incomplete(&popup, window, cx);
    }

    /// Drops a completion request whose answer is no longer wanted, without closing the
    /// popup.
    ///
    /// Distinct from [`Self::cancel_completion_query`] only in intent, and that is worth a
    /// name: this one is called *while the popup stays open*, when the list on screen is
    /// already the better answer. Both halves ADR-0007 asks for still happen — the task is
    /// dropped so nothing awaits the answer, and `$/cancelRequest` tells the server to stop
    /// computing it and reclaims its pending slot.
    fn supersede_completion_query(&mut self) {
        if self.completion_query.is_none() {
            return;
        }
        self.cancel_completion_query();
        self.jobs.cancel(Job::Completion);
    }

    /// Writes the accepted item over the word the popup was opened on.
    ///
    /// The replaced range is `word_start..cursor`, so everything typed while the list was
    /// narrowing is overwritten rather than appended to — typing `str` then accepting
    /// `strlen` must not produce `strstrlen`.
    ///
    /// Selecting the range and inserting over it, rather than splicing the buffer, so the
    /// edit joins the undo history the way #83's insertion does: ⌘Z after a completion
    /// undoes exactly the completion.
    fn accept_completion(
        &mut self,
        item: CompletionItem,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let target = self.completion_word_start.clone();
        // Taken before dismissing, which clears it — the same ordering trap #83 documents.
        self.dismiss_completion(window, cx);

        // The editor the popup was *opened on*, not whatever is frontmost now. Clicking a
        // tab changes the active one without touching the popup, and resolving the target
        // here through `active_editor()` is how a completion ends up written into the wrong
        // file at an offset that meant something in another one.
        let Some((editor, start)) = target else { return };

        // And it must still be an open tab: accepting into a document that has been closed
        // would edit a buffer nothing is showing.
        if !self.tabs.iter().any(|tab| tab.editor == editor) {
            return;
        }

        editor.update(cx, |editor, cx| {
            let document = &mut editor.document;
            let end = document.selection.head;
            // Re-validated against the buffer as it is *now*. An async reload can move
            // offsets under an open popup, and writing an identifier into the middle of
            // unrelated code is far worse than a completion that declines to fire.
            if start > end || end > document.buffer.len_bytes() {
                return;
            }
            document.move_to(start, false);
            document.move_to(end, true);
            document.insert(&item.insert);
            cx.notify();
        });
    }

    /// Closes the popup, returning focus to the editor and stopping the server's work.
    fn dismiss_completion(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // Nothing open: not even the focus move, which would steal focus from whatever
        // has it — this is called unconditionally from paths that may have no popup.
        if self.completion.take().is_none() {
            return;
        }
        // Dropped *first*, and that ordering is the whole reason this is not re-entrant:
        // the focus move at the end of this function is itself a focus-out on the popup's
        // handle, so a live subscription would call straight back into here. The `take`
        // above already guards it — the second entry finds no popup and returns — but
        // dropping the listener means the second entry never happens at all.
        self.completion_focus_out = None;
        self.completion_word_start = None;
        // The answer has nowhere to go now, so the server must stop computing it.
        self.cancel_completion_query();
        self.jobs.cancel(Job::Completion);

        // Focus goes back to the *editor*, not to the workspace: the user was mid-edit and
        // escape must leave them where they were typing. This is the difference from the
        // palette, whose escape returns to the workspace.
        if let Some(editor) = self.active_editor().cloned() {
            window.focus(&editor.read(cx).focus_handle(cx));
        } else {
            window.focus(&self.focus_handle);
        }
        cx.notify();
    }

    /// Opens settings.json in a tab — #60 built the file, so ⌘, edits it as text.
    ///
    /// No settings *UI*: the file is the interface for now, and a form over four keys is
    /// the kind of thing that has to be rebuilt the moment a fifth key is not a string.
    fn open_settings(&mut self, _: &OpenSettings, window: &mut Window, cx: &mut Context<Self>) {
        // ⌘, is the panel now (#100); a second press dismisses, like the palette. The JSON
        // stays one click away inside it, and keeps its own palette command — the panel
        // must not become a wall between the user and their file.
        if self.settings_panel.take().is_some() {
            // Same destination as the panel's click-away close (#172): the editor, not the
            // workspace root. Closing by re-pressing ⌘, used to strand the keyboard here
            // while the click-away path did not — one panel, two exits, two behaviours.
            self.focus_editor_or_workspace(window, cx);
            cx.notify();
            return;
        }
        self.dismiss_completion(window, cx);

        let panel = cx.new(SettingsPanel::new);
        cx.subscribe_in(&panel, window, |this, _panel, event, window, cx| match event {
            SettingsPanelEvent::Dismissed => {
                this.settings_panel = None;
                // Esc / Cancel inside the panel lands focus back on the editor too — the
                // click-away path already did, and the two must not disagree (#172).
                this.focus_editor_or_workspace(window, cx);
                cx.notify();
            }
            SettingsPanelEvent::OpenJson => {
                this.settings_panel = None;
                this.open_settings_file(window, cx);
                cx.notify();
            }
        })
        .detach();
        window.focus(&panel.read(cx).focus_handle(cx));
        self.settings_panel = Some(panel);
        cx.notify();
    }

    /// Opens settings.json in a tab — the hand-editing path the panel's button reaches.
    fn open_settings_file(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        match crate::settings::path_for_editing(cx) {
            Some(path) => self.open_path(path, window, cx),
            // Only when HOME is unset or the file is unparseable — both already logged, and
            // both mean there is no file it would be safe to open.
            None => {
                self.status = Some("no settings file to open — see the log".into());
                cx.notify();
            }
        }
    }

    fn toggle_palette(&mut self, mode: PaletteMode, window: &mut Window, cx: &mut Context<Self>) {
        // Same mode reopened means "dismiss"; a different mode swaps the contents.
        if self.palette.as_ref().is_some_and(|p| p.read(cx).mode() == mode) {
            self.dismiss_palette(window, cx);
            return;
        }

        // The palette's chords are workspace-scoped, so ⌘P reaches here with the completion
        // popup open and holding focus. Two overlays both believing they own the keyboard is
        // a state with no correct behaviour — and the popup would be left anchored over a
        // palette that had focus, still holding an offset a later accept could write to.
        // The palette is the one the user just asked for, so the popup goes.
        self.dismiss_completion(window, cx);

        // Swapping modes replaces the palette entity, so a Files walk still running has
        // lost its consumer just as surely as a dismissal would have. ⌘P then ⌘⇧P used to
        // leave that walk grinding through `vendor/` with nowhere to put the result.
        self.cancel_quick_open_walk();

        let items = match mode {
            PaletteMode::Commands => self
                .registry
                .all()
                .iter()
                .map(|command| (command.title.to_string(), command.id.0.to_string()))
                .collect(),
            // Known up front, and short: eleven fixed choices with no IO behind them.
            // The current language is marked rather than filtered out, so the list always
            // says what the buffer is now as well as what it could be (#127).
            PaletteMode::Languages => {
                let current =
                    self.active_editor().map(|editor| editor.read(cx).document.language());
                elle_syntax::ALL_LANGUAGES
                    .iter()
                    .map(|language| {
                        let name = language.name();
                        let label = if Some(*language) == current {
                            format!("{name}  ✓")
                        } else {
                            name.to_string()
                        };
                        (label, name.to_string())
                    })
                    .collect()
            }
            // Everything else arrives asynchronously — the palette opens empty and fills in.
            PaletteMode::Files
            | PaletteMode::Routes
            | PaletteMode::Symbols
            | PaletteMode::References
            | PaletteMode::Artisan
            | PaletteMode::WorkspaceSymbols
            | PaletteMode::Rename
            | PaletteMode::CodeActions
            | PaletteMode::Branches
            | PaletteMode::ComposerScripts
            | PaletteMode::GitLog => Vec::new(),
        };

        let palette = cx.new(|cx| Palette::new(mode, items, cx));

        // The palette reports outcomes as events rather than calling back into the
        // workspace, so it stays a self-contained widget with no knowledge of what its
        // rows mean. The subscription is dropped with the palette entity.
        cx.subscribe_in(&palette, window, |this, palette, event, window, cx| match event {
            PaletteEvent::Confirmed(id) => this.confirm_palette(id.clone(), window, cx),
            PaletteEvent::Dismissed => this.dismiss_palette(window, cx),
            // Only the live-source mode re-asks; static modes already filtered locally.
            PaletteEvent::QueryChanged(query) => {
                if palette.read(cx).mode() == PaletteMode::WorkspaceSymbols {
                    this.load_workspace_symbol_items(palette.clone(), query.clone(), cx);
                }
            }
        })
        .detach();

        window.focus(&palette.read(cx).focus_handle(cx));
        self.palette = Some(palette.clone());

        match mode {
            PaletteMode::Files => self.load_quick_open_items(palette, cx),
            PaletteMode::Routes => self.load_route_items(palette, cx),
            PaletteMode::Symbols => self.load_symbol_items(palette, cx),
            // References are filled by the request that opened the palette, not from here:
            // the offset they are about is the cursor position at the moment the user
            // pressed the key, which `toggle_palette` does not know.
            PaletteMode::References => {}
            PaletteMode::Artisan => self.load_artisan_items(palette, cx),
            PaletteMode::WorkspaceSymbols => {
                self.load_workspace_symbol_items(palette, String::new(), cx)
            }
            PaletteMode::Branches => self.load_branch_items(palette, cx),
            PaletteMode::ComposerScripts => self.load_composer_script_items(palette, cx),
            PaletteMode::GitLog => self.load_git_log_items(palette, cx),
            PaletteMode::Commands
            | PaletteMode::Languages
            | PaletteMode::Rename
            | PaletteMode::CodeActions => {}
        }

        cx.notify();
    }

    /// Fills the palette with the project's files, from the persisted index when it is
    /// current and from a live walk otherwise.
    ///
    /// The palette opens immediately with an empty list rather than waiting: on a large
    /// project the walk takes long enough that blocking on it would be the difference
    /// between an instant palette and a visible stall (§13, §22).
    ///
    /// A cache hit is 3.5-4.5x faster than the walk on real projects — see
    /// [`crate::file_cache`] for the measurements and for why a mismatch re-walks rather
    /// than repairing rows.
    fn load_quick_open_items(&mut self, palette: Entity<Palette>, cx: &mut Context<Self>) {
        let Some(root) = self.tree.as_ref().map(|tree| tree.root().to_path_buf()) else { return };

        // Cancel a walk still running from a previous open. Dropping the Task stops us
        // awaiting it, but the blocking walk on the background thread would run to
        // completion regardless — the flag is what actually stops it (ADR-0007).
        self.cancel_quick_open_walk();
        let cancel = CancelFlag::new();
        self.quick_open_cancel = Some(cancel.clone());

        let task = cx.spawn(async move |this, cx| {
            let load_cancel = cancel.clone();
            let write_root = root.clone();
            let (files, source) =
                cx.background_spawn(async move { file_cache::load(&root, &load_cancel) }).await;

            // A cancelled load returns whatever it had; showing a partial list for a
            // palette the user already closed would be noise.
            if cancel.is_cancelled() {
                return;
            }

            let items = files
                .iter()
                .map(|file| (file.relative.clone(), file.path.display().to_string()))
                .collect();

            palette.update(cx, |palette, cx| palette.set_items(items, cx)).ok();
            this.update(cx, |_, cx| cx.notify()).ok();

            // Persist only what we just walked, and only *after* the palette is filled —
            // the user is never waiting on this write. A cache hit has nothing new to say.
            if source == file_cache::Source::Walk {
                cx.background_spawn(async move { file_cache::store(&write_root, &files, &cancel) })
                    .await;
            }
        });
        self.jobs.start(Job::QuickOpenIndex, task);
    }

    /// Parses `routes/*.php` on the background executor and fills the palette.
    ///
    /// Same shape as the quick-open walk above: tree-sitter over every route file is not
    /// instant on a real project, and blocking on it would stall the palette open.
    ///
    /// A project with no `routes/` is the common case — most folders anyone opens are not
    /// Laravel projects — so an empty list is a normal outcome here, not an error worth
    /// telling the user about.
    fn load_route_items(&mut self, palette: Entity<Palette>, cx: &mut Context<Self>) {
        let Some(root) = self.tree.as_ref().map(|tree| tree.root().to_path_buf()) else { return };

        let task = cx.spawn(async move |this, cx| {
            let items = cx
                .background_spawn(async move {
                    let mut items = Vec::new();
                    let Ok(entries) = std::fs::read_dir(root.join("routes")) else {
                        return items;
                    };

                    let mut files: Vec<_> = entries
                        .flatten()
                        .map(|entry| entry.path())
                        .filter(|path| path.extension().is_some_and(|ext| ext == "php"))
                        .collect();
                    // Stable order, so the list does not reshuffle between opens.
                    files.sort();

                    for path in files {
                        let Ok(source) = std::fs::read_to_string(&path) else { continue };
                        for route in extract_routes(&source).routes {
                            // `route.line` is 1-based; `Point` rows are 0-based.
                            items.push((
                                route_label(&route),
                                target_id(&path, route.line.saturating_sub(1)),
                            ));
                        }
                    }
                    items
                })
                .await;

            palette.update(cx, |palette, cx| palette.set_items(items, cx)).ok();
            this.update(cx, |_, cx| cx.notify()).ok();
        });
        self.jobs.start(Job::RouteIndex, task);
    }

    /// Fills the artisan palette from `php artisan list --raw` run against this project.
    ///
    /// The project's own artisan is the only honest source: a package-registered command
    /// appears, a command this Laravel version lacks does not. When artisan does not
    /// answer — not a Laravel project, no php, artisan errored — the palette stays empty,
    /// which its "No matches" already says (`artisan::list` documents why silence).
    fn load_artisan_items(&mut self, palette: Entity<Palette>, cx: &mut Context<Self>) {
        let Some(root) = self.tree.as_ref().map(|tree| tree.root().to_path_buf()) else { return };

        let task = cx.spawn(async move |this, cx| {
            let commands =
                cx.background_spawn(async move { crate::artisan::list(&root) }).await;
            let items = commands
                .unwrap_or_default()
                .into_iter()
                .map(|(name, description)| {
                    let label = if description.is_empty() {
                        name.clone()
                    } else {
                        format!("{name} — {description}")
                    };
                    (label, name)
                })
                .collect();
            palette.update(cx, |palette, cx| palette.set_items(items, cx)).ok();
            this.update(cx, |_, cx| cx.notify()).ok();
        });
        self.jobs.start(Job::ArtisanList, task);
    }

    // --- navigation (#81) -------------------------------------------------------------
    //
    // Every action here is silent with no server running. That is not laziness about error
    // handling, it is §24 and #74's established behaviour: nobody has Intelephense on a
    // fresh machine, most folders anyone opens are not PHP projects, and an editor that
    // says "no language server" every time F12 is pressed is complaining about software the
    // user never asked for. The failure goes to the log.
    //
    // All three queries go through `start_query`, which does the two halves of superseding
    // a navigation that do not imply each other: `Job::LspQuery` drops the task awaiting the
    // old answer, and `cancel_in_flight_query` sends `$/cancelRequest` so the server stops
    // computing it and the connection reclaims its pending slot. Dropping the task alone
    // leaves the server working and leaks that slot for the life of the process.

    /// The active tab's URI and cursor offset — what every navigation request is about.
    ///
    /// `None` when there is no tab, no path, or the file is not one the server was told
    /// about. Blade is the case that matters: `lsp_session::handles` excludes it, so
    /// asking about a template would be asking about a document the server has never seen.
    fn navigation_origin(&self, cx: &App) -> Option<(Uri, usize)> {
        let tab = self.tabs.get(self.active_tab)?;
        let path = tab.path.as_deref()?;
        if !crate::lsp_session::handles(path) {
            return None;
        }
        let uri = crate::lsp_session::uri_for(path)?;
        Some((uri, tab.editor.read(cx).document.selection.head))
    }

    /// Tells the server to stop working on the navigation still in flight, if any.
    ///
    /// Both halves matter and neither implies the other. Dropping the task stops us
    /// *waiting*; `$/cancelRequest` stops the server *computing* and — because
    /// `Connection::cancel` removes the local entry first — reclaims the slot in the pending
    /// map that an abandoned request would otherwise occupy for the life of the process.
    fn cancel_in_flight_query(&mut self) {
        let Some(id) = self.in_flight_query.take() else { return };
        if let Some(client) = self.lsp.client_mut() {
            client.cancel(&id);
        }
    }

    /// Makes `task` the current navigation, cancelling whatever it supersedes.
    ///
    /// Every navigation goes through here so that "a new question abandons the old one" is
    /// true of the server as well as of us. Doing it at each call site is how one of the
    /// three quietly ends up leaking the request it replaced.
    fn start_query(&mut self, id: elle_lsp::RequestId, task: Task<()>) {
        self.cancel_in_flight_query();
        self.in_flight_query = Some(id);
        self.jobs.start(Job::LspQuery, task);
    }

    /// Where the cursor is now, for the jump history.
    fn current_location(&self, cx: &App) -> Option<(PathBuf, Point)> {
        let tab = self.tabs.get(self.active_tab)?;
        let path = tab.path.clone()?;
        Some((path, tab.editor.read(cx).document.cursor_point()))
    }

    /// Jumps to a Laravel route, config key, view or component under the cursor (#83).
    ///
    /// Returns whether it took the navigation, so the caller can fall through to the
    /// language server when it did not. It only ever claims a click it can actually
    /// complete: `elle_laravel::reference_at` returns `None` for anything not a plain
    /// literal, so a `route($name)` falls through rather than being swallowed.
    ///
    /// **Blade is the case this exists for.** `navigation_origin` deliberately excludes
    /// `.blade.php` — the language server was never told about those files — so before this,
    /// ⌘click in a template did nothing at all. Reading the reference does not go through
    /// that gate, which is what makes `@include` and `<x-…>` navigable.
    ///
    /// Only the *reading* is synchronous, over a buffer already in memory: a tree-sitter
    /// parse of one open file, on a click, not a keystroke. Resolution stats the filesystem
    /// and parses `routes/*.php`, so it goes to the background executor (ADR-0007) — which
    /// is also why this returns `true` before knowing whether a target was found. Claiming
    /// the click is the right call either way: the alternative is asking a language server
    /// about a string literal, which has no answer.
    fn go_to_laravel_target(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        let Some(root) = self.tree.as_ref().map(|tree| tree.root().to_path_buf()) else {
            return false;
        };
        let Some(tab) = self.tabs.get(self.active_tab) else { return false };
        let Some(path) = tab.path.clone() else { return false };

        let Some(blade) = laravel_dialect(&path) else { return false };

        let document = &tab.editor.read(cx).document;
        let source = document.buffer.text();
        let offset = document.selection.head;

        let Some(reference) = elle_laravel::reference_at(&source, offset, blade) else {
            return false;
        };

        let origin = self.current_location(cx);
        let task = cx.spawn_in(window, async move |this, cx| {
            let found = cx
                .background_spawn(async move { elle_laravel::resolve(&root, &path, &reference) })
                .await;

            this.update_in(cx, |this, window, cx| {
                // Nothing found means nothing said. A view comes from a configurable
                // finder, a component from a registered namespace, a route from any
                // service provider — so "we could not find it" is the only true statement
                // available, and it is not worth a status line (RISKS.md #4, §24).
                let Some(target) = found else { return };
                if let Some(origin) = origin {
                    this.history.push(origin);
                }
                // `Target::line` is 1-based; `Point` rows are 0-based. `None` opens the
                // file without moving the cursor, which is the honest result when the key
                // resolved to a file but not to a line inside it.
                let point = target.line.map(|line| Point::new(line.saturating_sub(1), 0));
                this.open_path_at(target.path, point, window, cx);
                cx.notify();
            })
            .ok();
        });
        self.jobs.start(Job::LaravelTarget, task);
        true
    }

    /// Go to definition (F12, and the Go menu).
    fn go_to_definition(
        &mut self,
        _: &GoToDefinition,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.go_to_definition_at_cursor(window, cx);
    }

    /// Pushes the current branch (⇧⌥P, and the palette's Git: Push). One place so the
    /// chord and the command cannot diverge; the CLI carries the user's credentials.
    fn push_to_remote(&mut self, _: &PushToRemote, _window: &mut Window, cx: &mut Context<Self>) {
        self.status = Some("Pushing…".into());
        cx.notify();
        self.run_git_operation(
            |root| {
                elle_git::push(&root).map(|out| {
                    let out = out.trim();
                    if out.is_empty() { "✓ Pushed".to_string() } else { out.to_string() }
                })
            },
            cx,
        );
    }

    /// Opens the branch palette (the panel's Branch button, and `git.switch_branch`).
    ///
    /// A thin action so the git panel's pointer button and the command palette's keyboard
    /// entry land on the *same* `toggle_palette(Branches)` — the switch logic itself stays in
    /// one place, `elle_git::switch_branch`, reached only when a branch is confirmed.
    fn switch_branch(&mut self, _: &SwitchBranch, window: &mut Window, cx: &mut Context<Self>) {
        self.toggle_palette(PaletteMode::Branches, window, cx);
    }

    /// Opens the git log palette (the panel's History button, and `git.log`).
    fn show_git_log(&mut self, _: &ShowGitLog, window: &mut Window, cx: &mut Context<Self>) {
        self.toggle_palette(PaletteMode::GitLog, window, cx);
    }

    /// Formats the whole document through the language server (#19, ⇧⌥F).
    ///
    /// Silent with no server, like every navigation command (§24). The buffer is
    /// resynced first — the server formats *its* copy, so its copy must be this one
    /// (the same resync completion needs, for the same reason). The reply is applied
    /// only if the buffer has not changed since the ask: edits are byte ranges into a
    /// specific text, and applying them to a different one would corrupt the file the
    /// user is typing in.
    fn format_document(
        &mut self,
        _: &FormatDocument,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some((uri, _)) = self.navigation_origin(cx) else { return };
        let Some(tab) = self.tabs.get(self.active_tab) else { return };
        let Some(path) = tab.path.clone() else { return };
        let editor = tab.editor.clone();
        let text = editor.read(cx).document.buffer.text();

        self.notify_lsp_of_change(&path, &text);
        let Some(client) = self.lsp.client_mut() else { return };
        // PSR-12's four spaces — PHP's own convention, and what every generated Laravel
        // file uses. A settings key can arrive when someone asks for tabs.
        let id = match client.request_formatting(&uri, 4, true) {
            Ok(id) => id,
            Err(err) => {
                tracing::debug!("could not ask for formatting: {err:#}");
                return;
            }
        };

        self.status = Some("Formatting…".into());
        cx.notify();

        let query = id.clone();
        let task = cx.spawn_in(window, async move |this, cx| {
            let reply = Self::poll_query::<Vec<elle_lsp::lsp_types::TextEdit>>(&this, &query, cx)
                .await
                .unwrap_or_default();

            this.update_in(cx, |this, _window, cx| {
                this.status = None;
                // `None` is the server declining, which is not an error (RISKS #4).
                let Some(edits) = reply else {
                    cx.notify();
                    return;
                };
                editor.update(cx, |editor, cx| {
                    if editor.document.buffer.text() != text {
                        // Typed since the ask: the edits describe a text that no longer
                        // exists. Dropping them is the only application that cannot
                        // corrupt anything.
                        return;
                    }
                    let index = elle_lsp::LineIndex::new(&text);
                    let byte_edits = edits
                        .into_iter()
                        .map(|edit| {
                            (
                                index.byte_range(
                                    &text,
                                    edit.range,
                                    elle_lsp::OffsetEncoding::Utf16,
                                ),
                                edit.new_text,
                            )
                        })
                        .collect();
                    editor.document.apply_edits(byte_edits);
                    cx.notify();
                });
                cx.notify();
            })
            .ok();
        });
        self.jobs.start(Job::LspQuery, task);
    }

    /// Asks for quick fixes at the cursor and lists them in the palette (#19, ⌘.).
    ///
    /// The request runs *before* the palette opens — a palette that appears and then
    /// fills is right for navigation lists, wrong for a menu of two fixes. Silent with
    /// no server; "No quick fixes here" when the server answers empty, because the user
    /// asked and silence would read as a dead keybinding (the ⌥⌘I rule). Command-only
    /// entries (no edit) are skipped: applying one means workspace/executeCommand,
    /// which is a different contract — recorded here rather than half-built.
    fn quick_fix(&mut self, _: &QuickFix, window: &mut Window, cx: &mut Context<Self>) {
        let Some((uri, offset)) = self.navigation_origin(cx) else { return };
        let Some(tab) = self.tabs.get(self.active_tab) else { return };
        let Some(path) = tab.path.clone() else { return };
        let text = tab.editor.read(cx).document.buffer.text();
        let selection = tab.editor.read(cx).document.selection.range();
        let range = if selection.is_empty() { offset..offset } else { selection };

        // The server decides what to offer from the diagnostics we send back.
        let diagnostics = self
            .lsp
            .diagnostics_for(&uri)
            .map(|file| file.raw_in_range(range.clone()))
            .unwrap_or_default();

        self.notify_lsp_of_change(&path, &text);
        let Some(client) = self.lsp.client_mut() else { return };
        let id = match client.request_code_actions(&uri, range, &diagnostics) {
            Ok(id) => id,
            Err(err) => {
                tracing::debug!("could not ask for code actions: {err:#}");
                return;
            }
        };

        let query = id.clone();
        let task = cx.spawn_in(window, async move |this, cx| {
            let reply =
                Self::poll_query::<elle_lsp::lsp_types::CodeActionResponse>(&this, &query, cx)
                    .await
                    .unwrap_or_default();
            this.update_in(cx, |this, window, cx| {
                this.in_flight_query = None;
                let mut titles = Vec::new();
                let mut edits = Vec::new();
                for entry in reply.unwrap_or_default() {
                    if let elle_lsp::lsp_types::CodeActionOrCommand::CodeAction(action) = entry
                        && let Some(edit) = action.edit
                    {
                        titles.push((action.title, edits.len().to_string()));
                        edits.push(edit);
                    }
                }
                if edits.is_empty() {
                    this.status = Some("No quick fixes here".into());
                    cx.notify();
                    return;
                }
                this.pending_code_actions = edits;
                this.toggle_palette(PaletteMode::CodeActions, window, cx);
                if let Some(palette) = this.palette.clone() {
                    palette.update(cx, |palette, cx| palette.set_items(titles, cx));
                }
                cx.notify();
            })
            .ok();
        });
        self.start_query(id, task);
    }

    /// Opens the rename prompt for the symbol under the cursor (#19, F2).
    ///
    /// Silent when there is no server or no word under the cursor, like every
    /// navigation command. The prompt opens pre-filled with the current name — most
    /// renames edit a word rather than retype it — and the position is captured now,
    /// because the cursor may move under the overlay.
    fn rename_symbol(&mut self, _: &RenameSymbol, window: &mut Window, cx: &mut Context<Self>) {
        let Some((uri, offset)) = self.navigation_origin(cx) else { return };
        match self.lsp.client_mut() {
            None => return,
            Some(client) if !client.capabilities().rename => {
                // Said BEFORE the prompt opens: F2 into a prompt whose Enter can only
                // ever do nothing reads as a broken feature. Intelephense without a
                // licence key is the common way to land here, and the server's own
                // capability answer is the honest thing to relay.
                self.status = Some("The language server does not offer rename".into());
                cx.notify();
                return;
            }
            Some(_) => {}
        }
        let Some(editor) = self.active_editor().cloned() else { return };
        let (word, span) = {
            let document = &editor.read(cx).document;
            let Some(span) = document.word_span_at(offset) else { return };
            (document.buffer.text()[span.clone()].to_string(), span)
        };
        let _ = span;

        self.pending_rename = Some((uri, offset));
        self.toggle_palette(PaletteMode::Rename, window, cx);
        if let Some(palette) = self.palette.clone() {
            palette.update(cx, |palette, cx| palette.preset_query(&word, cx));
        }
    }

    /// Sends the rename and applies the server's `WorkspaceEdit` — all files or none.
    fn perform_rename(
        &mut self,
        uri: elle_lsp::lsp_types::Uri,
        offset: usize,
        new_name: String,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // The server renames against *its* copy — resync, the formatting lesson.
        if let Some(tab) = self.tabs.get(self.active_tab)
            && let Some(path) = tab.path.clone()
        {
            let text = tab.editor.read(cx).document.buffer.text();
            self.notify_lsp_of_change(&path, &text);
        }
        let Some(client) = self.lsp.client_mut() else { return };
        let id = match client.request_rename(&uri, offset, &new_name) {
            Ok(id) => id,
            Err(err) => {
                tracing::debug!("could not ask for a rename: {err:#}");
                return;
            }
        };

        self.status = Some("Renaming…".into());
        cx.notify();

        let query = id.clone();
        let task = cx.spawn(async move |this, cx| {
            let reply = Self::poll_query::<elle_lsp::lsp_types::WorkspaceEdit>(&this, &query, cx)
                .await
                .unwrap_or_default();
            this.update(cx, |this, cx| {
                this.in_flight_query = None;
                match reply {
                    // The user acted (F2, a name, Enter), so a null reply gets words —
                    // the ⌥⌘I rule; silence here read as "rename is broken" in the
                    // owner's first real use.
                    None => {
                        this.status = Some("The server offered no rename here".into());
                    }
                    Some(edit) => match this.apply_workspace_edit(edit, cx) {
                        Ok(files) => {
                            this.status = Some(format!("Renamed in {files} file(s)").into());
                        }
                        Err(err) => {
                            // Refused whole — the one honest failure mode for a rename.
                            this.status = Some(format!("Rename not applied: {err}").into());
                        }
                    },
                }
                cx.notify();
            })
            .ok();
        });
        self.start_query(id, task);
    }

    /// Applies a `WorkspaceEdit` to open buffers and closed files — all or nothing.
    ///
    /// Two phases on purpose. Everything is read and converted first, and any failure —
    /// a file operation in the edit, an unreadable file, an overlapping batch — aborts
    /// before a single byte has changed anywhere. Only then are buffers edited and
    /// files written: a rename applied to *some* of its files is corruption wearing a
    /// success message.
    fn apply_workspace_edit(
        &mut self,
        edit: elle_lsp::lsp_types::WorkspaceEdit,
        cx: &mut Context<Self>,
    ) -> anyhow::Result<usize> {
        use anyhow::Context as _;
        let changes = crate::lsp_session::workspace_edit_changes(edit)
            .context("the edit includes file operations this editor does not apply")?;

        enum Planned {
            Buffer(Entity<EditorView>, Vec<(std::ops::Range<usize>, String)>, PathBuf, String),
            Disk(PathBuf, String),
        }

        // Phase 1: plan everything, touching nothing.
        let mut plan = Vec::new();
        for (path, edits) in changes {
            let open_tab = self.tabs.iter().find(|tab| {
                tab.path.as_ref().is_some_and(|tab_path| {
                    tab_path == &path
                        || tab_path.canonicalize().ok() == path.canonicalize().ok()
                })
            });
            match open_tab {
                Some(tab) => {
                    let editor = tab.editor.clone();
                    let text = editor.read(cx).document.buffer.text();
                    let index = elle_lsp::LineIndex::new(&text);
                    let mut byte_edits: Vec<(std::ops::Range<usize>, String)> = edits
                        .into_iter()
                        .map(|edit| {
                            (
                                index.byte_range(
                                    &text,
                                    edit.range,
                                    elle_lsp::OffsetEncoding::Utf16,
                                ),
                                edit.new_text,
                            )
                        })
                        .collect();
                    byte_edits.sort_by_key(|(range, _)| (range.start, range.end));
                    if byte_edits.windows(2).any(|pair| pair[0].0.end > pair[1].0.start) {
                        anyhow::bail!("overlapping edits in {}", path.display());
                    }
                    plan.push(Planned::Buffer(editor, byte_edits, path, text));
                }
                None => {
                    let text = read_file(&path)
                        .with_context(|| format!("could not read {}", path.display()))?
                        .text;
                    let new_text = crate::lsp_session::apply_lsp_edits_to_text(&text, edits)
                        .ok_or_else(|| {
                            anyhow::anyhow!("overlapping edits in {}", path.display())
                        })?;
                    plan.push(Planned::Disk(path, new_text));
                }
            }
        }

        // Phase 2: apply. Buffer edits cannot fail past this point; a disk write that
        // fails mid-way is reported, which is the one residual risk of writing files at
        // all — recorded rather than hidden.
        let files = plan.len();
        let mut wrote_disk = false;
        for planned in plan {
            match planned {
                Planned::Buffer(editor, byte_edits, path, _old_text) => {
                    let new_text = editor.update(cx, |editor, cx| {
                        editor.document.apply_edits(byte_edits);
                        cx.notify();
                        editor.document.buffer.text()
                    });
                    // Keep the server's copy in step with the buffer it just renamed.
                    self.notify_lsp_of_change(&path, &new_text);
                }
                Planned::Disk(path, new_text) => {
                    write_file(&path, &new_text)
                        .with_context(|| format!("could not write {}", path.display()))?;
                    wrote_disk = true;
                }
            }
        }
        // Disk writes changed the working tree; the panel must not wait for a focus
        // round-trip to notice (the owner's report: "nem vai pro source control").
        if wrote_disk {
            self.refresh_git_status(cx);
        }
        // And the buffers the edit touched save themselves (when autosave is on), so
        // the rename reaches disk and the git panel in one gesture — the report was
        // exactly this gap.
        self.autosave_dirty_tabs(cx);
        cx.notify();
        Ok(files)
    }

    /// Go to definition from wherever the cursor is.
    ///
    /// Shared by F12 and ⌘click rather than duplicated, so the two cannot drift into
    /// answering differently — the click has already moved the cursor by the time this
    /// runs, which is what makes "wherever the cursor is" the right question for both.
    ///
    /// Laravel goes first, and only because it answers a *different* question: `route('x')`
    /// is a string literal, so a language server has nothing to say about it, and the two
    /// can never both have an answer for the same click. When Laravel has none the request
    /// goes to the server exactly as before, so nothing that worked before this stops.
    fn go_to_definition_at_cursor(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.go_to_laravel_target(window, cx) {
            return;
        }

        let Some((uri, offset)) = self.navigation_origin(cx) else { return };
        let Some(client) = self.lsp.client_mut() else { return };

        let id = match client.request_definition(&uri, offset) {
            Ok(id) => id,
            Err(err) => {
                tracing::debug!("could not ask for a definition: {err:#}");
                return;
            }
        };

        // Said before the answer arrives, because on a cold server this is the seconds the
        // user would otherwise spend wondering whether the key registered.
        self.status = Some("Finding definition…".into());
        cx.notify();

        let origin = self.current_location(cx);
        let query = id.clone();
        let task = cx.spawn_in(window, async move |this, cx| {
            let found = Self::poll_query::<GotoDefinitionResponse>(&this, &query, cx).await;

            this.update_in(cx, |this, window, cx| {
                this.status = None;
                // Answered, so there is nothing left to cancel. Clearing it keeps a later
                // navigation from sending `$/cancelRequest` for an id the server has
                // already retired.
                this.in_flight_query = None;
                match found {
                    // A server with no answer is the ordinary outcome for a keyword, a
                    // comment, or a symbol it has not indexed. Saying so beats a silent
                    // no-op that is indistinguishable from a dropped keystroke.
                    Ok(None) => this.status = Some("No definition found".into()),
                    Ok(Some(response)) => match first_location(&response) {
                        Some((path, line, character)) => {
                            if let Some(origin) = origin {
                                this.history.push(origin);
                            }
                            this.open_path_at_lsp(path, line, character, window, cx);
                        }
                        None => this.status = Some("No definition found".into()),
                    },
                    // The server died or answered with an error. §24: log it, carry on.
                    Err(err) => tracing::debug!("definition lookup failed: {err:#}"),
                }
                cx.notify();
            })
            .ok();
        });
        self.start_query(id, task);
    }

    /// Find usages (⇧F12), into the palette as a result list.
    fn find_references(&mut self, _: &FindReferences, window: &mut Window, cx: &mut Context<Self>) {
        let Some((uri, _)) = self.navigation_origin(cx) else { return };

        // The palette opens *before* anything is asked, and opens *first* because
        // `toggle_palette` treats a second ⇧F12 as "dismiss". Sending the request first
        // meant that press launched a whole-project search and then threw the palette it
        // would have filled away — the request abandoned, uncancelled, with the user
        // watching the panel vanish.
        self.toggle_palette(PaletteMode::References, window, cx);
        let Some(palette) = self.palette.clone() else { return };

        // Re-read the offset after the toggle: opening the palette moves focus, and the
        // origin has to be the cursor as it was, not as some later frame finds it.
        let Some((_, offset)) = self.navigation_origin(cx) else { return };
        let Some(client) = self.lsp.client_mut() else { return };

        // `true`: the declaration is a usage the reader wants in the list. Excluding it
        // makes "3 usages" mean something different from what the count in every other IDE
        // means, for no gain.
        let id = match client.request_references(&uri, offset, true) {
            Ok(id) => id,
            Err(err) => {
                tracing::debug!("could not ask for references: {err:#}");
                return;
            }
        };

        let root = self.tree.as_ref().map(|tree| tree.root().to_path_buf());
        let query = id.clone();
        let task = cx.spawn(async move |this, cx| {
            let found = Self::poll_query::<Vec<Location>>(&this, &query, cx).await;
            this.update(cx, |this, _| this.in_flight_query = None).ok();

            let locations = match found {
                Ok(Some(locations)) => locations,
                Ok(None) => Vec::new(),
                Err(err) => {
                    tracing::debug!("reference search failed: {err:#}");
                    Vec::new()
                }
            };

            let items: Vec<(String, String)> = locations
                .iter()
                .filter_map(|location| {
                    let path = elle_lsp::uri_to_path(&location.uri).ok()?;
                    let row = location.range.start.line as usize;
                    // Project-relative where we can: an absolute path per row pushes the
                    // part that differs off the right of a 520px palette.
                    let shown = root
                        .as_deref()
                        .and_then(|root| path.strip_prefix(root).ok())
                        .unwrap_or(&path);
                    // 1-based in the label, because that is what the rest of the world
                    // calls line 1 — the id stays 0-based for `Point`.
                    Some((format!("{}:{}", shown.display(), row + 1), target_id(&path, row)))
                })
                .collect();

            palette.update(cx, |palette, cx| palette.set_items(items, cx)).ok();
            this.update(cx, |_, cx| cx.notify()).ok();
        });
        self.start_query(id, task);
    }

    /// Go to symbol in the active file (⌘⇧O).
    fn go_to_symbol(&mut self, _: &GoToSymbol, window: &mut Window, cx: &mut Context<Self>) {
        // Opened before the request is sent, so an unindexed file still shows an empty
        // palette rather than nothing happening at all.
        self.toggle_palette(PaletteMode::Symbols, window, cx);
    }

    /// Fills the symbol palette from the active file's document symbols.
    fn load_symbol_items(&mut self, palette: Entity<Palette>, cx: &mut Context<Self>) {
        let Some((uri, _)) = self.navigation_origin(cx) else { return };
        let Some(path) = self.tabs.get(self.active_tab).and_then(|tab| tab.path.clone()) else {
            return;
        };
        let Some(client) = self.lsp.client_mut() else { return };

        let id = match client.request_document_symbols(&uri) {
            Ok(id) => id,
            Err(err) => {
                tracing::debug!("could not ask for document symbols: {err:#}");
                return;
            }
        };

        let query = id.clone();
        let task = cx.spawn(async move |this, cx| {
            let found = Self::poll_query::<DocumentSymbolResponse>(&this, &query, cx).await;
            this.update(cx, |this, _| this.in_flight_query = None).ok();

            let items = match found {
                Ok(Some(response)) => crate::lsp_session::flatten_symbols(&response)
                    .into_iter()
                    .map(|symbol| {
                        // Two spaces per level. A class's methods have to *look* nested or
                        // the list reads as a flat jumble of names.
                        let label = format!("{}{}", "  ".repeat(symbol.depth), symbol.label);
                        (label, target_id(&path, symbol.line as usize))
                    })
                    .collect(),
                Ok(None) => Vec::new(),
                Err(err) => {
                    tracing::debug!("symbol lookup failed: {err:#}");
                    Vec::new()
                }
            };

            palette.update(cx, |palette, cx| palette.set_items(items, cx)).ok();
            this.update(cx, |_, cx| cx.notify()).ok();
        });
        self.start_query(id, task);
    }

    /// Fills the workspace-symbols palette from the server, superseding the previous ask.
    ///
    /// Called once on open (empty query — some servers answer it with a capped project
    /// overview, Intelephense answers nothing, and either is a fine starting screen) and
    /// again on every keystroke. `start_query` cancels the in-flight predecessor, which
    /// is the whole point: the server is the matcher, so typing fast must drop stale
    /// asks rather than queue them (#20's rule, applied to #19's search).
    fn load_workspace_symbol_items(
        &mut self,
        palette: Entity<Palette>,
        query_text: String,
        cx: &mut Context<Self>,
    ) {
        let Some(client) = self.lsp.client_mut() else { return };
        let id = match client.request_workspace_symbols(&query_text) {
            Ok(id) => id,
            Err(err) => {
                tracing::debug!("could not ask for workspace symbols: {err:#}");
                return;
            }
        };

        let query = id.clone();
        let task = cx.spawn(async move |this, cx| {
            let found = Self::poll_query::<elle_lsp::lsp_types::WorkspaceSymbolResponse>(
                &this, &query, cx,
            )
            .await;
            this.update(cx, |this, _| this.in_flight_query = None).ok();

            let items = match found {
                Ok(Some(response)) => crate::lsp_session::workspace_symbol_items(&response)
                    .into_iter()
                    .map(|(label, path, line)| (label, target_id(&path, line as usize)))
                    .collect(),
                Ok(None) => Vec::new(),
                Err(err) => {
                    tracing::debug!("workspace symbol lookup failed: {err:#}");
                    Vec::new()
                }
            };

            palette.update(cx, |palette, cx| palette.set_items(items, cx)).ok();
            this.update(cx, |_, cx| cx.notify()).ok();
        });
        self.start_query(id, task);
    }

    /// Waits for a request to be answered without blocking the main thread.
    ///
    /// The loop is the whole point. `Client` lives in the view, so it can only be touched
    /// inside `this.update(…)` — and that closure runs on the main thread, where a blocking
    /// wait would park the window for as long as a cold Intelephense takes to think.
    /// ADR-0007 forbids exactly that, and the compiler does not catch it. So: check, yield
    /// to the executor, check again.
    ///
    /// Dropping the task (a superseding navigation, or the window closing) stops the loop
    /// where it stands; the pending request is then cancelled by the next `cancel` or
    /// simply abandoned, which costs the server one wasted answer and nobody a stall.
    async fn poll_query<T: elle_lsp::DeserializeOwned + 'static>(
        this: &gpui::WeakEntity<Self>,
        id: &elle_lsp::RequestId,
        cx: &mut gpui::AsyncApp,
    ) -> anyhow::Result<Option<T>> {
        let deadline = std::time::Instant::now() + NAVIGATION_TIMEOUT;

        loop {
            let polled = this.update(cx, |this, _| {
                let Some(client) = this.lsp.client_mut() else {
                    // The server died or was replaced while we were waiting. That is *not*
                    // the same as the server answering with nothing, and reporting it as
                    // such would put "No definition found" on screen — a positive claim
                    // about a question nobody ever got to ask. An error is logged and
                    // silent, which is what §24 asks for.
                    anyhow::bail!("the language server went away before it answered");
                };
                client.poll_response::<T>(id)
            })?;

            match polled? {
                Some(answer) => return Ok(answer),
                None => {
                    if std::time::Instant::now() >= deadline {
                        // Stop asking and tell the server to stop working. A server this
                        // slow has already lost the user's attention.
                        this.update(cx, |this, _| {
                            if let Some(client) = this.lsp.client_mut() {
                                client.cancel(id);
                            }
                        })
                        .ok();
                        anyhow::bail!("the language server did not answer in time");
                    }
                    cx.background_executor().timer(NAVIGATION_POLL_INTERVAL).await;
                }
            }
        }
    }

    /// Back (⌃-): return to where the last jump started.
    fn navigate_back(&mut self, _: &NavigateBack, window: &mut Window, cx: &mut Context<Self>) {
        let Some(here) = self.current_location(cx) else { return };
        let Some((path, point)) = self.history.back(here) else { return };
        self.open_path_at(path, Some(point), window, cx);
    }

    /// Forward (⌃⇧-): undo a Back.
    fn navigate_forward(
        &mut self,
        _: &NavigateForward,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(here) = self.current_location(cx) else { return };
        let Some((path, point)) = self.history.forward(here) else { return };
        self.open_path_at(path, Some(point), window, cx);
    }

    // --- the file tree's context menu (#126) ------------------------------------------

    /// Opens the context menu for the tree row at `index`, at the mouse position.
    ///
    /// The row is also selected on the way, because a menu that appears next to a row it is
    /// not visibly about is how people delete the wrong file. Right-click does not open the
    /// file — that is what left-click is for, and opening a 4 MB file because someone wanted
    /// to rename it is a surprise with a visible cost.
    fn open_tree_menu(
        &mut self,
        index: usize,
        position: gpui::Point<gpui::Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(tree) = self.tree.as_ref() else { return };
        let Some(entry) = tree.entries().get(index) else { return };

        let path = entry.path.clone();
        let is_dir = entry.is_dir();

        self.pending = Some(PendingFileAction { path, is_dir, kind: PendingKind::Menu });
        self.show_overlay(
            cx.new(|cx| Overlay::menu(context_menu::actions_for(is_dir), position, cx)),
            window,
            cx,
        );
    }

    /// Opens the context menu for the project root — the tree's empty space.
    ///
    /// The root has no row, so without this there is no way to create a file at the top
    /// level of a project: every row-bound menu creates *inside* the clicked directory.
    fn open_tree_root_menu(
        &mut self,
        position: gpui::Point<gpui::Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(root) = self.tree.as_ref().map(|tree| tree.root().to_path_buf()) else { return };
        self.pending =
            Some(PendingFileAction { path: root, is_dir: true, kind: PendingKind::Menu });
        self.show_overlay(
            cx.new(|cx| Overlay::menu(context_menu::actions_for_root(), position, cx)),
            window,
            cx,
        );
    }

    /// Puts an overlay on screen, focuses it, and subscribes to what it reports.
    ///
    /// Everything that takes focus dismisses the completion popup first, for the reason
    /// spelled out at the render root: the popup cannot see focus leave, so an overlay
    /// opened over it would leave it on screen and unreachable.
    fn show_overlay(
        &mut self,
        overlay: Entity<Overlay>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.dismiss_completion(window, cx);

        cx.subscribe_in(&overlay, window, |this, _overlay, event, window, cx| match event {
            OverlayEvent::Picked(action) => this.on_menu_action(action.clone(), window, cx),
            OverlayEvent::Named(name) => this.on_name_confirmed(name.clone(), window, cx),
            OverlayEvent::Accepted => this.on_delete_confirmed(window, cx),
            OverlayEvent::Dismissed => this.dismiss_overlay(window, cx),
        })
        .detach();

        window.focus(&overlay.read(cx).focus_handle(cx));
        self.overlay = Some(overlay);
        cx.notify();
    }

    /// Closes whatever overlay is open and gives the keyboard back.
    fn dismiss_overlay(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.overlay = None;
        self.pending = None;
        match self.active_editor() {
            Some(editor) => {
                let handle = editor.read(cx).focus_handle(cx);
                window.focus(&handle);
            }
            None => window.focus(&self.focus_handle),
        }
        cx.notify();
    }

    /// A menu entry was chosen: either act now, or open the prompt that the action needs.
    fn on_menu_action(&mut self, action: MenuAction, window: &mut Window, cx: &mut Context<Self>) {
        let Some(pending) = self.pending.clone() else { return };
        let name = file_name_of(&pending.path);

        match action {
            // The two that need a name typed first.
            MenuAction::NewFile | MenuAction::NewDirectory => {
                let kind = if action == MenuAction::NewFile {
                    PendingKind::CreateFile
                } else {
                    PendingKind::CreateDirectory
                };
                let subject = if kind == PendingKind::CreateFile { "file in" } else { "folder in" };
                self.pending = Some(PendingFileAction { kind, ..pending });
                self.show_overlay(
                    cx.new(|cx| Overlay::prompt("New", format!("{subject} {name}"), "", cx)),
                    window,
                    cx,
                );
            }
            MenuAction::Rename => {
                self.pending = Some(PendingFileAction { kind: PendingKind::Rename, ..pending });
                // Pre-filled with the current name: retyping a long class file to change
                // one letter is what makes a rename box feel broken.
                self.show_overlay(
                    cx.new(|cx| Overlay::prompt("Rename", name.clone(), name, cx)),
                    window,
                    cx,
                );
            }
            // The one that cannot be undone, and so the only one that asks.
            MenuAction::Delete => {
                self.pending = Some(PendingFileAction { kind: PendingKind::Delete, ..pending });
                let detail = if pending.is_dir {
                    "This folder and everything in it will be deleted permanently."
                } else {
                    "This file will be deleted permanently."
                };
                self.show_overlay(
                    cx.new(|cx| Overlay::confirm(format!("Delete {name}?"), detail, cx)),
                    window,
                    cx,
                );
            }
            // The two that act immediately: neither destroys anything, so neither asks.
            MenuAction::RevealInFinder => {
                self.reveal_in_finder(&pending.path, cx);
                self.dismiss_overlay(window, cx);
            }
            MenuAction::CopyPath => {
                cx.write_to_clipboard(gpui::ClipboardItem::new_string(
                    pending.path.display().to_string(),
                ));
                self.dismiss_overlay(window, cx);
            }
        }
    }

    /// Shows a path in Finder.
    ///
    /// ponytail: shells out to `open -R` rather than calling `NSWorkspace`. It is one
    /// process for an action the user waits on anyway, and it keeps this file free of
    /// Objective-C bridging for a feature that is not on any hot path. ADR-0004 is not at
    /// stake — this is the app crate — but `domain_crates_have_no_platform_conditionals` is
    /// why it could not live in `elle-workspace` even if it wanted to.
    #[cfg(target_os = "macos")]
    fn reveal_in_finder(&mut self, path: &std::path::Path, cx: &mut Context<Self>) {
        if let Err(err) = std::process::Command::new("open").arg("-R").arg(path).spawn() {
            self.status = Some(format!("could not reveal {}: {err}", path.display()).into());
            cx.notify();
        }
    }

    #[cfg(not(target_os = "macos"))]
    fn reveal_in_finder(&mut self, _path: &std::path::Path, _cx: &mut Context<Self>) {}

    /// A name was typed and confirmed. Runs the create or rename it was for.
    ///
    /// The file operation is blocking, so it goes to the background executor (ADR-0007) and
    /// the tree is refreshed on the main thread when it returns.
    fn on_name_confirmed(&mut self, name: String, window: &mut Window, cx: &mut Context<Self>) {
        let Some(pending) = self.pending.clone() else { return };
        self.dismiss_overlay(window, cx);

        let path = pending.path.clone();
        let kind = pending.kind;

        /// What the background half hands back, so the main thread knows which follow-up
        /// the operation earned — created files open, renames retarget their tabs.
        enum Outcome {
            Created(PathBuf),
            Renamed { from: PathBuf, to: PathBuf },
            Nothing,
        }

        let task = cx.spawn_in(window, async move |this, cx| {
            let name_for_op = name.clone();
            let done = cx
                .background_spawn(async move {
                    match kind {
                        PendingKind::CreateFile => {
                            let target = path.join(name_for_op);
                            elle_workspace::create_file(&target).map(|()| Outcome::Created(target))
                        }
                        PendingKind::CreateDirectory => {
                            elle_workspace::create_directory(&path.join(name_for_op))
                                .map(|()| Outcome::Nothing)
                        }
                        // Not opened afterwards — the user was already looking at whatever
                        // they were looking at — but any tab already showing it must follow
                        // the file to its new name, which is what `Renamed` carries.
                        PendingKind::Rename => elle_workspace::rename(&path, &name_for_op)
                            .map(|to| Outcome::Renamed { from: path, to }),
                        // Unreachable: a delete never goes through the name prompt.
                        PendingKind::Menu | PendingKind::Delete => Ok(Outcome::Nothing),
                    }
                })
                .await;

            this.update_in(cx, |this, window, cx| {
                match done {
                    // A new file is opened, because creating one and then having to find it
                    // in the tree is the kind of missing half-step that makes a feature feel
                    // unfinished. A new *folder* is not: there is nothing in it to show.
                    Ok(Outcome::Created(target)) => {
                        this.refresh_tree(cx);
                        this.open_path(target, window, cx);
                    }
                    Ok(Outcome::Renamed { from, to }) => {
                        this.refresh_tree(cx);
                        this.retarget_tabs(&from, &to, cx);
                    }
                    Ok(Outcome::Nothing) => this.refresh_tree(cx),
                    Err(err) => this.status = Some(format!("{err:#}").into()),
                }
                cx.notify();
            })
            .ok();
        });
        self.jobs.start(Job::FileOperation, task);
    }

    /// Points every open tab under `from` at its new home under `to`.
    ///
    /// # Why a rename must chase the tabs
    ///
    /// A tab keeps the path it was opened with. Rename `User.php` to `User2.php` with its
    /// tab open and the tab still says `User.php` — so the next ⌘S writes the buffer to the
    /// *old* name, quietly resurrecting the file the user just renamed away. That is the
    /// same undo-by-save the delete path had (see `close_tabs_under`), reached through the
    /// other file operation. Delete closes the tabs; rename has a better option, because
    /// the file still exists — the tab follows it.
    ///
    /// `from` may be a directory, in which case every tab underneath moves — renaming
    /// `app/` must not strand a tab on `app/Models/User.php`.
    ///
    /// Following the file means three updates per tab, not one:
    /// - the tab's own path, which is what ⌘S writes to;
    /// - the document's path via `set_path`, which re-detects the language — renaming
    ///   `notes.txt` to `notes.php` must start highlighting, the same rule save-as follows;
    /// - the language server's book-keeping: the old URI is closed and the new one opened,
    ///   because to the server a rename *is* those two events, and diagnostics keyed to the
    ///   old URI would otherwise point at a file that no longer exists. The sync ledger
    ///   entry goes with it.
    fn retarget_tabs(
        &mut self,
        from: &std::path::Path,
        to: &std::path::Path,
        cx: &mut Context<Self>,
    ) {
        // Decided first, applied second: the LSP calls below need `&mut self` and the
        // decision needs the tabs — the same split every multi-tab operation here uses.
        let moves: Vec<(usize, PathBuf)> = self
            .tabs
            .iter()
            .enumerate()
            .filter_map(|(index, tab)| {
                let open = tab.path.as_deref()?;
                if !is_under(open, from) {
                    return None;
                }
                // Canonical forms for the arithmetic, not the raw ones: `is_under` already
                // tolerates the two spellings of one file (`/var` vs `/private/var` — the
                // tab keeps whatever opened it, the tree canonicalises), and comparing the
                // raw paths right after would re-open the exact hole on the very next
                // line. That happened: the first version of this closure did `open ==
                // from` literally, and the retarget silently skipped every tab whose
                // spelling differed — found by this function's own test, not by reading.
                let real_open = canonical_prefix(open).ok()?;
                let real_from = canonical_prefix(from).ok()?;
                let new_path = if real_open == real_from {
                    to.to_path_buf()
                } else {
                    // Inside a renamed directory: keep the remainder of the path.
                    to.join(real_open.strip_prefix(&real_from).ok()?)
                };
                Some((index, new_path))
            })
            .collect();

        for (index, new_path) in moves {
            let old_path = self.tabs[index].path.clone();
            if let Some(old) = old_path.as_deref() {
                self.close_on_lsp(old);
                self.lsp_synced.remove(old);
            }

            let text = self.tabs[index].editor.update(cx, |editor, cx| {
                // A failed grammar load falls back to plain text inside `set_path`; the
                // rename itself already happened on disk either way.
                let _ = editor.document.set_path(new_path.clone());
                cx.notify();
                editor.document.buffer.text()
            });

            self.tabs[index].path = Some(new_path.clone());
            self.open_on_lsp(&new_path, &text, cx);
        }
    }

    /// A tree row was dropped on a directory (or the empty space, meaning the root).
    ///
    /// Synchronous, unlike delete: a rename within one filesystem is a metadata operation
    /// at human-drag speed, and `move_entry` carries the refusals (own subtree, existing
    /// name, outside the root) that make it safe to call with whatever was dropped.
    /// The moved file's open tabs follow via the rename machinery above — same operation
    /// as far as a buffer is concerned.
    fn drop_tree_entry(&mut self, source: PathBuf, dest_dir: PathBuf, cx: &mut Context<Self>) {
        let Some(root) = self.tree.as_ref().map(|tree| tree.root().to_path_buf()) else {
            return;
        };
        match elle_workspace::move_entry(&source, &dest_dir, &root) {
            Ok(dest) if dest == source => {} // dropped where it already lives
            Ok(dest) => {
                self.retarget_tabs(&source, &dest, cx);
                self.refresh_tree(cx);
            }
            Err(err) => {
                self.status = Some(format!("{err:#}").into());
                cx.notify();
            }
        }
    }

    /// Files dragged in from Finder (owner request): a folder becomes the open project,
    /// a file becomes a tab. Both doors already exist — this is only a third handle on
    /// them, which is what keeps a dropped folder and ⌘O indistinguishable afterwards.
    fn external_drop(
        &mut self,
        paths: &[PathBuf],
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        for path in paths {
            if path.is_dir() {
                match FileTree::new(path.clone()) {
                    Ok(tree) => self.adopt_tree(tree, cx),
                    Err(err) => {
                        self.status = Some(format!("{err:#}").into());
                        cx.notify();
                    }
                }
            } else {
                self.open_path(path.clone(), window, cx);
            }
        }
    }

    /// The delete confirmation was accepted.
    ///
    /// The `root` handed to `elle_workspace::delete` is what keeps a recursive delete inside
    /// the project even if the path in hand is stale — see that function for why a tree path
    /// is not trusted on its own.
    fn on_delete_confirmed(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(pending) = self.pending.clone() else { return };
        let Some(root) = self.tree.as_ref().map(|tree| tree.root().to_path_buf()) else { return };
        self.dismiss_overlay(window, cx);

        let path = pending.path.clone();
        let task = cx.spawn_in(window, async move |this, cx| {
            let deleted_path = path.clone();
            let done = cx
                .background_spawn(async move { elle_workspace::delete(&deleted_path, &root) })
                .await;

            this.update(cx, |this, cx| {
                match done {
                    Ok(()) => {
                        // A tab showing a file that no longer exists would save it back into
                        // being on the next ⌘S, quietly undoing the delete.
                        this.close_tabs_under(&path, cx);
                        this.refresh_tree(cx);
                    }
                    Err(err) => this.status = Some(format!("{err:#}").into()),
                }
                cx.notify();
            })
            .ok();
        });
        self.jobs.start(Job::FileOperation, task);
    }

    /// Closes any tab whose file was inside `path`, which may be a file or a directory.
    ///
    /// # Why this does not go through `close_tab_at`
    ///
    /// That is the ⌘W path, and it prompts "… has unsaved changes. Closing this tab will
    /// discard them." on a dirty buffer. Asked about a file that has *just been deleted*,
    /// every part of that dialog is wrong: there is nothing to discard the changes against,
    /// "Cancel" cannot put the file back, and answering it does not change what is on disk.
    /// It would also be asked *after* the deletion the user already confirmed, which reads
    /// as the editor asking twice and doing something different the second time.
    ///
    /// So the tabs are removed outright. The user's confirmation to delete the file is the
    /// consent for losing its buffer — that is what the confirmation said, naming the file
    /// and the word *permanently*.
    ///
    /// The one thing genuinely lost here is unsaved changes to a file the user chose to
    /// delete. Recovering those would mean offering to save a buffer whose path is gone,
    /// which is save-as, which is a different question than the one being asked.
    fn close_tabs_under(&mut self, path: &std::path::Path, cx: &mut Context<Self>) {
        // Collected first: closing shifts every later index, so deciding and acting in one
        // pass closes the wrong tabs — the same shift `tab_after_close` exists for.
        let doomed: Vec<usize> = self
            .tabs
            .iter()
            .enumerate()
            .filter(|(_, tab)| tab.path.as_deref().is_some_and(|open| is_under(open, path)))
            .map(|(index, _)| index)
            .collect();

        // Last first, so each removal cannot shift an index still to come.
        for index in doomed.into_iter().rev() {
            self.remove_tab(index, cx);
        }
    }

    /// Drops every editor's hover card.
    ///
    /// Called when the active tab changes: the card is anchored at window coordinates that
    /// meant something over the *previous* tab's text, and a tab revisited later must not
    /// greet the user with a card about where their mouse rested minutes ago.
    fn clear_hover_cards(&mut self, cx: &mut Context<Self>) {
        for tab in &self.tabs {
            tab.editor.update(cx, |editor, cx| {
                if editor.hover_diagnostic.take().is_some() {
                    cx.notify();
                }
            });
        }
    }

    /// Re-reads the tree after a file operation, keeping the user's expansions.
    fn refresh_tree(&mut self, cx: &mut Context<Self>) {
        if let Some(tree) = self.tree.as_mut()
            && let Err(err) = tree.refresh()
        {
            self.status = Some(format!("{err:#}").into());
        }
        cx.notify();
    }

    /// Watches the root so the tree follows Finder, terminals and `mkdir` without a
    /// manual refresh (owner request).
    ///
    /// The watcher's callback runs on notify's own thread, where touching an `Entity`
    /// is not allowed, so it only pokes a channel — the same marshalling shape as the
    /// test runner's event stream. A foreground task debounces 300 ms on gpui's
    /// executor (test-controllable time, like the search debounce) and drains the
    /// burst, so a `composer install` that touches a thousand paths costs a handful of
    /// refreshes rather than a thousand.
    ///
    /// `.git` churn is filtered at the callback: every `git status` the git panel runs
    /// rewrites index files, and refreshing the tree for those would make the watcher
    /// its own noisiest client. Watcher setup failure is non-fatal — the tree just
    /// stays manual-refresh, which is what it was before this existed.
    fn start_tree_watcher(&mut self, root: std::path::PathBuf, cx: &mut Context<Self>) {
        use notify::Watcher as _;

        self.tree_watcher = None; // a replaced root un-watches before re-watching
        let (tx, rx) = smol::channel::unbounded::<()>();
        let git_dir = root.join(".git");
        let callback_tx = tx.clone();
        let mut watcher =
            match notify::recommended_watcher(move |event: notify::Result<notify::Event>| {
                let Ok(event) = event else { return };
                if !event.paths.is_empty() && event.paths.iter().all(|p| p.starts_with(&git_dir))
                {
                    return;
                }
                // A full channel cannot happen (unbounded); a closed one means the
                // workspace moved on, and the watcher is about to be dropped with it.
                let _ = callback_tx.try_send(());
            }) {
                Ok(watcher) => watcher,
                Err(err) => {
                    eprintln!("ellefuanti: tree watcher unavailable ({err}); refresh stays manual");
                    return;
                }
            };
        if let Err(err) = watcher.watch(&root, notify::RecursiveMode::Recursive) {
            eprintln!(
                "ellefuanti: cannot watch {} ({err}); refresh stays manual",
                root.display()
            );
            return;
        }

        let timer_executor = cx.background_executor().clone();
        let task = cx.spawn(async move |this, cx| {
            while rx.recv().await.is_ok() {
                timer_executor.timer(std::time::Duration::from_millis(300)).await;
                while rx.try_recv().is_ok() {} // coalesce the burst into one refresh
                if this.update(cx, |this, cx| this.refresh_tree(cx)).is_err() {
                    break; // the workspace is gone; so is the point
                }
            }
        });
        self.tree_watcher = Some((watcher, tx, task));
    }

    // --- self-update (owner request) ------------------------------------------------

    /// Starts the periodic release check, once — from `render` like the activation
    /// observer, because that is the established "after the window exists" hook.
    ///
    /// The transport is `curl`, which every macOS ships: an HTTP crate for one GET
    /// every six hours is exactly the dependency this codebase keeps refusing. A
    /// failed check (offline, rate-limited, GitHub down) is silence, not a toast —
    /// the user did not ask a question, so there is no answer to owe them. Guarded
    /// out of tests entirely: 600 render tests each spawning a network process is a
    /// flake factory.
    fn start_update_check(&mut self, cx: &mut Context<Self>) {
        if self.update_check.is_some() || cfg!(test) {
            return;
        }
        let timer_executor = cx.background_executor().clone();
        self.update_check = Some(cx.spawn(async move |this, cx| {
            loop {
                let output = cx
                    .background_spawn(async {
                        smol::process::Command::new("curl")
                            .args(["-fsSL", crate::update::RELEASES_API])
                            .output()
                            .await
                    })
                    .await;
                let available = output
                    .ok()
                    .filter(|output| output.status.success())
                    .and_then(|output| String::from_utf8(output.stdout).ok())
                    .and_then(|body| crate::update::parse_latest_release(&body))
                    .filter(|release| {
                        crate::update::newer_than_current(release, env!("CARGO_PKG_VERSION"))
                    });
                if let Some(available) = available {
                    let alive = this.update(cx, |this, cx| {
                        // Only ever move forward from an offer: a re-check must not
                        // clobber an install in flight or a finished one.
                        if matches!(
                            this.update_state,
                            crate::update::UpdateState::Idle
                                | crate::update::UpdateState::Available(_)
                        ) {
                            this.update_state =
                                crate::update::UpdateState::Available(available);
                            cx.notify();
                        }
                    });
                    if alive.is_err() {
                        break;
                    }
                }
                timer_executor.timer(std::time::Duration::from_secs(6 * 60 * 60)).await;
            }
        }));
    }

    /// The status-bar cell was clicked while an update was on offer.
    ///
    /// The full install only makes sense for the installed app: a `cargo run` or a
    /// Gatekeeper-translocated copy is not at `/Applications/ellefuanti.app`, and
    /// swapping that path out from under a build that is not running from it updates
    /// nothing the user is looking at. Those cases — and a release with no dmg asset —
    /// open the release page instead, which is the notify-only behaviour.
    fn update_clicked(&mut self, cx: &mut Context<Self>) {
        match self.update_state.clone() {
            crate::update::UpdateState::Available(release) => {
                let installed = std::env::current_exe()
                    .map(|exe| exe.starts_with("/Applications/ellefuanti.app"))
                    .unwrap_or(false);
                match release.dmg_url.clone() {
                    Some(dmg_url) if installed => {
                        self.start_update_install(release, dmg_url, cx);
                    }
                    _ => {
                        let _ = std::process::Command::new("open")
                            .arg(&release.html_url)
                            .spawn();
                    }
                }
            }
            crate::update::UpdateState::ReadyToRestart => self.restart_into_update(cx),
            _ => {}
        }
    }

    /// Downloads the dmg and swaps `/Applications/ellefuanti.app` for its contents.
    ///
    /// One `sh -euc` script rather than N spawned steps: `-e` is the error handling —
    /// first failing tool aborts the rest, so a half-mounted image never gets copied
    /// from. The swap order (`rm` old, `mv` staged) is safe for the running app because
    /// macOS keeps the open binary's inode alive until exit. The `xattr -dr` at the end
    /// is the same quarantine clearing the README tells users to do by hand — done by
    /// the copy of the app they already chose to trust.
    fn start_update_install(
        &mut self,
        release: crate::update::Available,
        dmg_url: String,
        cx: &mut Context<Self>,
    ) {
        self.update_state = crate::update::UpdateState::Downloading;
        cx.notify();

        let script = format!(
            r#"set -eu
STAGE="$(mktemp -d)"
trap 'hdiutil detach "$STAGE/mnt" >/dev/null 2>&1 || true; rm -rf "$STAGE"' EXIT
curl -fsSL -o "$STAGE/update.dmg" "{dmg_url}"
mkdir "$STAGE/mnt"
hdiutil attach -nobrowse -readonly -mountpoint "$STAGE/mnt" "$STAGE/update.dmg" >/dev/null
rm -rf "/Applications/ellefuanti.app.update"
cp -R "$STAGE/mnt/ellefuanti.app" "/Applications/ellefuanti.app.update"
hdiutil detach "$STAGE/mnt" >/dev/null
rm -rf "/Applications/ellefuanti.app"
mv "/Applications/ellefuanti.app.update" "/Applications/ellefuanti.app"
xattr -dr com.apple.quarantine "/Applications/ellefuanti.app" || true
"#
        );
        let task = cx.spawn(async move |this, cx| {
            let output = cx
                .background_spawn(async move {
                    smol::process::Command::new("sh").args(["-c", &script]).output().await
                })
                .await;
            let result = match output {
                Ok(output) if output.status.success() => Ok(()),
                Ok(output) => Err(String::from_utf8_lossy(&output.stderr).trim().to_string()),
                Err(err) => Err(err.to_string()),
            };
            this.update(cx, |this, cx| {
                match result {
                    Ok(()) => this.update_state = crate::update::UpdateState::ReadyToRestart,
                    Err(err) => {
                        // Back to the offer, not to Idle: the cell stays retryable
                        // instead of vanishing until the next six-hour check.
                        this.status = Some(format!("Update failed: {err}").into());
                        this.update_state = crate::update::UpdateState::Available(release);
                    }
                }
                cx.notify();
            })
            .ok();
        });
        // Its own slot: dropping the workspace drops the task; the shell script's trap
        // cleans the temp dir either way.
        self.jobs.start(Job::UpdateInstall, task);
    }

    /// "Restart to update": relaunch the installed app and quit this process. The
    /// `sleep 1` gives this process time to exit so `open -n` does not just focus the
    /// dying instance.
    fn restart_into_update(&mut self, cx: &mut Context<Self>) {
        let _ = std::process::Command::new("sh")
            .args(["-c", "sleep 1; open -n /Applications/ellefuanti.app"])
            .spawn();
        cx.quit();
    }

    #[cfg(test)]
    pub fn set_update_state_for_test(
        &mut self,
        state: crate::update::UpdateState,
        cx: &mut Context<Self>,
    ) {
        self.update_state = state;
        cx.notify();
    }

    #[cfg(test)]
    pub fn update_label_for_test(&self) -> Option<String> {
        self.update_state.status_label()
    }

    /// Simulates one FS event, since a headless test has no FSEvents to fire. Goes
    /// through the real channel, debounce and refresh path.
    #[cfg(test)]
    pub fn poke_tree_watcher_for_test(&self) -> bool {
        match self.tree_watcher.as_ref() {
            Some((_, tx, _)) => tx.try_send(()).is_ok(),
            None => false,
        }
    }

    /// Starts the real watcher, which `open_folder_for_test` deliberately does not:
    /// most tests want a root, not an FSEvents stream per test.
    #[cfg(test)]
    pub fn start_tree_watcher_for_test(
        &mut self,
        root: std::path::PathBuf,
        cx: &mut Context<Self>,
    ) {
        self.start_tree_watcher(root, cx);
    }

    /// Sets the syntax language for the active tab (#127).
    ///
    /// # Why this does not also rename the file
    ///
    /// Choosing "PHP" for an untitled buffer says how to colour it, not where it should
    /// live. Inventing `untitled.php` on the strength of it would put a file on disk the
    /// user never asked for and pre-empt the save-as dialog they are going to get anyway.
    /// The language is a view of the buffer; the path is a decision about the filesystem.
    ///
    /// The choice does not survive a save, and that is deliberate rather than missing: once
    /// the buffer has a path, `set_path` re-detects from the extension, which is the answer
    /// the user just gave by choosing a name. A language override that outlived the save
    /// would mean a file called `.php` that refuses to highlight as PHP because of a menu
    /// choice made ten minutes earlier.
    ///
    /// ponytail: the override is per-document and unrecorded, so reopening a `.txt` full of
    /// SQL means choosing again. Persisting it needs somewhere to write per-file state
    /// (#60's settings layer, or the index), which is a store this does not have yet.
    fn set_active_language(&mut self, language: elle_syntax::Language, cx: &mut Context<Self>) {
        let Some(editor) = self.active_editor() else { return };

        let failed = editor.update(cx, |editor, cx| {
            let failed = editor.document.set_language(language).err();
            // A repaint has to be asked for explicitly: the buffer's text has not changed,
            // so nothing else marks the view dirty. Highlights are read straight off the
            // tree during render — there is no cache to invalidate — so redrawing is the
            // whole of applying the new grammar.
            cx.notify();
            failed
        });

        // A grammar that will not load leaves the document as plain text, which is visible
        // on screen — so saying nothing would look like the choice was ignored.
        if let Some(err) = failed {
            self.status = Some(format!("{err:#}").into());
        }
        cx.notify();
    }

    // --- language server ------------------------------------------------------------

    /// Starts a language server for the open folder, if one is configured.
    ///
    /// Nothing waits on this. The handshake runs on the background executor and can take
    /// half a minute against a cold Intelephense indexing `vendor/`; the editor is usable
    /// throughout, which is the whole of §24's "slow to start" case (ADR-0007).
    ///
    /// **A failure here is silent.** Not having a language server installed is the normal
    /// state of a fresh machine, and most folders anyone opens are not PHP projects at
    /// all. A status-bar error on every folder open would be a permanent complaint about
    /// software the user never asked for. The failure goes to the log, where someone
    /// looking for it can find it.
    fn start_lsp(&mut self, cx: &mut Context<Self>) {
        let Some(root) = self.tree.as_ref().map(|tree| tree.root().to_path_buf()) else { return };
        self.start_lsp_at(root, cx);
    }

    /// Starts a server for a file opened with no folder behind it.
    ///
    /// # Why a file gets a server at all
    ///
    /// A language server is given a project root at `initialize` and answers questions
    /// against it. With no folder open there was nothing to hand it, so the previous
    /// behaviour was to start nothing — ⌘O on a single `.php` file produced an editor with
    /// no completion, no diagnostics and no explanation (#125). Opening a file the server
    /// handles is as clear a signal that one is wanted as opening a folder is.
    ///
    /// Called on every file open, so it must be cheap and idempotent: a server already
    /// running keeps running, and a project opened later replaces it through `open_folder`.
    fn start_lsp_for_file(&mut self, path: &std::path::Path, cx: &mut Context<Self>) {
        // A folder is the better root, and it already started a server. Nothing to do.
        if self.tree.is_some() || self.lsp.client().is_some() {
            return;
        }
        // Only the first attempt. Without this, every file opened on a machine with no
        // server installed re-runs the binary search and re-spawns — cheap individually,
        // but it is a retry loop keyed on user actions, and §24 says a missing server must
        // be uneventful rather than persistent.
        if !matches!(self.lsp.state(), LspState::Idle) {
            return;
        }
        let Some(root) = project_root_for(path) else { return };
        self.start_lsp_at(root, cx);
    }

    /// Starts a server rooted at `root`, replacing whatever was running.
    fn start_lsp_at(&mut self, root: PathBuf, cx: &mut Context<Self>) {
        // A new server knows nothing; the sync ledger must not claim otherwise.
        self.lsp_synced.clear();
        self.lsp.set_root(Some(root.clone()));

        let Some(config) = crate::lsp_session::config_for(&root) else {
            // Either `ELLE_LSP_COMMAND=""` — switched off on purpose — or the binary is not
            // installed anywhere this process can see. `config_for` logs the second; both
            // land here as the same silent state, because from the editor's point of view
            // they are the same fact: there is no server, and typing must carry on (§24).
            self.lsp.set_state(LspState::Unavailable);
            return;
        };

        self.lsp.set_state(LspState::Starting);

        let task = cx.spawn(async move |this, cx| {
            let started =
                cx.background_spawn(async move { crate::lsp_session::start(&config) }).await;

            let client = match started {
                Ok(client) => client,
                Err(err) => {
                    // The common case, and it must stay quiet. `debug` rather than `warn`:
                    // "intelephense is not installed" is not a warning about this editor.
                    tracing::debug!("no language server: {err:#}");
                    this.update(cx, |this, _| this.lsp.set_state(LspState::Unavailable)).ok();
                    return;
                }
            };

            let opened = this.update(cx, |this, cx| {
                this.lsp.adopt(client);
                // Tabs opened before the server was ready are unknown to it, so tell it
                // about them now. Without this, a file open at launch never gets
                // diagnostics until it is closed and reopened.
                this.sync_open_documents(cx);
                cx.notify();
            });

            if opened.is_err() {
                return;
            }
            Self::poll_lsp(this, cx).await;
        });
        self.jobs.start(Job::Lsp, task);
    }

    /// Drains server notifications until the server dies or the workspace goes away.
    ///
    /// # Why this polls instead of blocking on the reader
    ///
    /// `Client::wait_for_events` blocks until the server pushes something, which is the
    /// nicer shape — no timer, no idle wakeups. It cannot be used here. The client lives
    /// inside the view, so reaching it means `this.update(…)`, and *that closure runs on
    /// the main thread*: a blocking wait inside it would park the UI for the length of the
    /// timeout, turning "the server is quiet" into dropped frames. ADR-0007's rule that a
    /// blocking call must never happen on the render thread is exactly this case, and the
    /// compiler does not catch it.
    ///
    /// So the loop waits on a background timer and then takes whatever has arrived without
    /// blocking, which is the same pattern `TerminalView` uses for its PTY. The cost is a
    /// wakeup every [`LSP_POLL_INTERVAL`] while a server is running; the alternative would
    /// need the client to own a channel into gpui, which is a runtime this crate's domain
    /// layer deliberately does not have.
    async fn poll_lsp(this: gpui::WeakEntity<Self>, cx: &mut gpui::AsyncApp) {
        loop {
            cx.background_executor().timer(LSP_POLL_INTERVAL).await;

            // `drain_events` takes what is already queued and returns immediately, so the
            // main thread is held only for as long as applying the batch takes.
            let keep_going = this.update(cx, |this, cx| {
                let Some(events) = this.lsp.client_mut().map(|client| client.drain_events()) else {
                    return false;
                };
                // Before applying, so a resync issued now produces diagnostics one tick
                // from now rather than two. Riding this timer is what gives per-keystroke
                // sync a 250ms debounce without a debouncer: the tick was already paid for.
                this.sync_changed_documents(cx);
                this.apply_lsp_events(events, cx)
            });

            // An error means the workspace is gone — the window closed. Falling out of the
            // loop lets the task finish, and dropping the `Lsp` field kills the process.
            match keep_going {
                Ok(true) => {}
                _ => return,
            }
        }
    }

    /// Applies a batch of server notifications. Returns whether to keep polling.
    fn apply_lsp_events(
        &mut self,
        events: Vec<elle_lsp::ServerEvent>,
        cx: &mut Context<Self>,
    ) -> bool {
        let mut changed = false;

        for event in events {
            if let elle_lsp::ServerEvent::Diagnostics { uri, diagnostics, .. } = event {
                // Resolved against our copy of the text, not the server's — the ranges are
                // painted over our buffer. `lsp_session::set_diagnostics` explains why.
                let text = self
                    .tabs
                    .iter()
                    .find(|tab| {
                        tab.path.as_deref().and_then(crate::lsp_session::uri_for).as_ref()
                            == Some(&uri)
                    })
                    .map(|tab| tab.editor.read(cx).document.buffer.text());

                // A publish for a file we do not have open is not an error — servers report
                // on the whole project. Storing it keeps the status-bar total honest.
                self.lsp.set_diagnostics(uri, &diagnostics, text.as_deref().unwrap_or(""));
                changed = true;
            }
        }

        if changed {
            self.push_diagnostics_to_editors(cx);
            cx.notify();
        }

        // A server that died between batches must not be polled forever, and the editor
        // has to carry on without it.
        if !self.lsp.is_running() {
            self.handle_lsp_death(cx);
            return false;
        }
        true
    }

    /// A server that stopped: restart it, or give up and say so.
    ///
    /// Restarting is bounded (`MAX_RESTARTS`). An unbounded restart of a server that dies
    /// on startup is a fork bomb, and an editor spawning processes in a loop is worse than
    /// an editor with no LSP — which is the state §24 says must always remain workable.
    fn handle_lsp_death(&mut self, cx: &mut Context<Self>) {
        let reason = self
            .lsp
            .client_mut()
            .and_then(|client| client.failure())
            .unwrap_or_else(|| "the language server exited".to_string());

        self.lsp.shut_down();
        self.push_diagnostics_to_editors(cx);

        if self.lsp.may_restart() {
            self.lsp.record_restart();
            tracing::warn!("{reason}; restarting the language server");
            self.start_lsp(cx);
            return;
        }

        // Now it *is* worth telling the user: this is not "you never installed one", it is
        // "the one you have keeps dying", and they cannot see that from anywhere else.
        tracing::warn!("{reason}; giving up after {} restarts", self.lsp.restarts());
        self.lsp.set_state(LspState::Failed(reason));
        cx.notify();
    }

    /// Tells the server about every open PHP tab.
    fn sync_open_documents(&mut self, cx: &mut Context<Self>) {
        let documents: Vec<_> = self
            .tabs
            .iter()
            .filter_map(|tab| {
                let path = tab.path.as_deref()?;
                if !crate::lsp_session::handles(path) {
                    return None;
                }
                let uri = crate::lsp_session::uri_for(path)?;
                Some((uri, tab.editor.read(cx).document.buffer.text()))
            })
            .collect();

        let Some(client) = self.lsp.client_mut() else { return };
        for (uri, text) in documents {
            // §24 all the way down: a notification that fails to send means the server has
            // gone, and the next poll notices. It is not worth interrupting the user over.
            if let Err(err) = client.did_open(uri, "php", &text) {
                tracing::debug!("could not open a document on the language server: {err:#}");
            }
        }
    }

    /// Hands each editor the diagnostics for its own file.
    ///
    /// A push rather than a pull: an editor that reached into the workspace for its
    /// diagnostics would need a handle to it, and every tab would then have an opinion
    /// about a server dying. See `EditorView::diagnostics`.
    fn push_diagnostics_to_editors(&mut self, cx: &mut Context<Self>) {
        for tab in &self.tabs {
            let items = tab
                .path
                .as_deref()
                .and_then(crate::lsp_session::uri_for)
                .and_then(|uri| self.lsp.diagnostics_for(&uri))
                .map(|file| {
                    file.items
                        .iter()
                        .map(|d| {
                            (d.range.clone(), d.severity, SharedString::from(d.message.clone()))
                        })
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();

            tab.editor.update(cx, |editor, cx| {
                editor.set_diagnostics(items);
                cx.notify();
            });
        }
    }

    /// The diagnostic message under the cursor in the active tab, if any.
    fn cursor_diagnostic(&self, cx: &App) -> Option<String> {
        let tab = self.tabs.get(self.active_tab)?;
        let uri = crate::lsp_session::uri_for(tab.path.as_deref()?)?;
        let document = &tab.editor.read(cx).document;
        let offset = document.selection.head;

        let diagnostics = self.lsp.diagnostics_for(&uri)?;
        if let Some(exact) = diagnostics.at(offset) {
            return Some(exact.message.clone());
        }

        // Nothing under the cursor itself: fall back to the cursor's *line*. Requiring the
        // cursor to land inside the squiggle's exact bytes made the message effectively
        // undiscoverable — the owner asked for "the reason for the error" while the reason
        // was already wired to a place their cursor never quite reached. Clicking anywhere
        // on a marked line is the gesture people actually make.
        let row = document.buffer.offset_to_point(offset).row;
        let start = document.buffer.point_to_offset(elle_text::Point { row, column: 0 });
        let end = start + document.buffer.line_len(row);
        diagnostics.on_line(start..end).map(|d| d.message.clone())
    }

    /// Resyncs every open PHP buffer the server's copy has fallen behind.
    ///
    /// Called from `poll_lsp`'s tick. Sends nothing for a buffer whose version is already
    /// in the ledger — typing pauses, the tick keeps firing, and an unchanged file must
    /// cost nothing. Full-text for `document_saved`'s reasons: no `Edit` in hand here, and
    /// PHP files are small.
    ///
    /// Every tab rather than only the active one, deliberately: replace-in-project and a
    /// future format-on-save can touch buffers that are not in front of the user, and a
    /// rule with an exception ("active only, except…") is how one door falls behind.
    fn sync_changed_documents(&mut self, cx: &mut Context<Self>) {
        let pending: Vec<(PathBuf, String, elle_text::Version)> = self
            .tabs
            .iter()
            .filter_map(|tab| {
                let path = tab.path.as_deref()?;
                if !crate::lsp_session::handles(path) {
                    return None;
                }
                let buffer = &tab.editor.read(cx).document.buffer;
                let version = buffer.version();
                if self.lsp_synced.get(path) == Some(&version) {
                    return None;
                }
                Some((path.to_path_buf(), buffer.text(), version))
            })
            .collect();

        for (path, text, version) in pending {
            let Some(uri) = crate::lsp_session::uri_for(&path) else { continue };
            let Some(client) = self.lsp.client_mut() else { return };
            if let Err(err) = client.did_change_full(&uri, &text) {
                tracing::debug!("could not resync {}: {err:#}", path.display());
                continue;
            }
            // Recorded only after a successful send, so a failed one is retried next tick
            // rather than silently never.
            self.lsp_synced.insert(path, version);
        }
    }

    /// Tells the server about one newly opened file, starting one if this is the first.
    ///
    /// A no-op with no server running, which is the ordinary case and deliberately not
    /// worth a branch at the call site.
    fn open_on_lsp(&mut self, path: &std::path::Path, text: &str, cx: &mut Context<Self>) {
        if !crate::lsp_session::handles(path) {
            return;
        }
        // #125's first cause. `start_lsp` was reachable only from `open_folder`, so a PHP
        // file opened any other way — ⌘O on the file itself, a jump from the palette, a
        // window that never had ⌘O run in it — got no server *and no log line*, which is
        // why the report read as "the popup never opens" rather than "the LSP never
        // started". Opening a file the server handles is as clear a signal that one is
        // wanted as opening a folder is.
        self.start_lsp_for_file(path, cx);

        let Some(uri) = crate::lsp_session::uri_for(path) else { return };
        let Some(client) = self.lsp.client_mut() else { return };

        if let Err(err) = client.did_open(uri, "php", text) {
            tracing::debug!("could not open a document on the language server: {err:#}");
        }
    }

    /// Tells the server a file was closed, so it stops reporting on it.
    fn close_on_lsp(&mut self, path: &std::path::Path) {
        let Some(uri) = crate::lsp_session::uri_for(path) else { return };
        if let Some(client) = self.lsp.client_mut() {
            let _ = client.did_close(&uri);
        }
        // And drop its squiggles. An empty publish is how the rest of this code says
        // "nothing to report here", so closing a file goes through the same path.
        self.lsp.set_diagnostics(uri, &[], "");
    }

    /// Tells the server a document changed.
    ///
    /// Full-text resync rather than an incremental change: the workspace sees the buffer
    /// *after* the edit and has no `Edit` in hand, and reconstructing one to look
    /// incremental would be a second chance to diverge from the server's copy. PHP files
    /// are small enough that the bandwidth is not the constraint — see
    /// `Client::did_change_full`.
    ///
    /// ponytail: this is called on save, not on keystroke. Diagnostics therefore update
    /// when you save, which is a defensible place to start and not where it should end;
    /// per-keystroke sync wants a debounce, and a debounce wants a timer this does not have
    /// yet. Doing it on save first means the wiring is exercised without putting a
    /// notification on the typing path before there is anything to measure it with.
    fn notify_lsp_of_change(&mut self, path: &std::path::Path, text: &str) {
        if !crate::lsp_session::handles(path) {
            return;
        }
        let Some(uri) = crate::lsp_session::uri_for(path) else { return };
        let Some(client) = self.lsp.client_mut() else { return };

        if let Err(err) = client.did_change_full(&uri, text) {
            tracing::debug!("could not sync a document to the language server: {err:#}");
        }
    }

    /// Stops any quick-open walk. Called whenever the palette that would consume its
    /// results goes away — dismissed *or* replaced by a different mode.
    fn cancel_quick_open_walk(&mut self) {
        if let Some(cancel) = self.quick_open_cancel.take() {
            cancel.cancel();
        }
        // The flag stops the blocking walk; dropping the task stops us awaiting it. Both
        // are needed, and only doing the first leaves a task holding the old palette alive.
        self.jobs.cancel(Job::QuickOpenIndex);
    }

    /// Puts focus back on the editor when a tab is open, and on the workspace root otherwise.
    ///
    /// The one move #171/#172 made the rule for every overlay dismiss: after the language
    /// menu closed the owner reported the keyboard felt dead, because a workspace with no
    /// editor focus forwards neither typing nor the reopen chord to where the eye is. Closing
    /// a bottom panel (terminal, tests) and the settings dialog leaves the eye on the editor
    /// just the same, so they route here rather than parking focus on the root. The workspace
    /// handle is the fallback for a window with no tab.
    fn focus_editor_or_workspace(&self, window: &mut Window, cx: &mut Context<Self>) {
        match self.active_editor() {
            Some(editor) => {
                let handle = editor.read(cx).focus_handle(cx);
                window.focus(&handle);
            }
            None => window.focus(&self.focus_handle),
        }
    }

    fn dismiss_palette(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // A walk still running is now pure waste — nothing will consume its results.
        self.cancel_quick_open_walk();
        self.palette = None;
        self.focus_editor_or_workspace(window, cx);
        cx.notify();
    }

    /// Runs whatever the palette confirmed.
    fn confirm_palette(&mut self, id: String, window: &mut Window, cx: &mut Context<Self>) {
        let mode = self.palette.as_ref().map(|p| p.read(cx).mode());
        self.dismiss_palette(window, cx);

        match mode {
            // Every one of these hands back a path, and all but quick open append the row
            // to land on. Quick open's ids are bare paths and decode to no target, which
            // opens the file at the top — the behaviour it always had.
            Some(
                PaletteMode::Files
                | PaletteMode::Routes
                | PaletteMode::Symbols
                | PaletteMode::References
                | PaletteMode::WorkspaceSymbols,
            ) => {
                let (path, target) = split_target_id(&id);
                // A symbol or a usage is a jump, so Back must return here. A file opened
                // from quick open is not: the user chose a destination, they did not follow
                // a reference out of somewhere they were reading.
                if target.is_some()
                    && matches!(mode, Some(PaletteMode::Symbols | PaletteMode::References))
                    && let Some(origin) = self.current_location(cx)
                {
                    self.history.push(origin);
                }
                self.open_path_at(path, target, window, cx);
            }
            // #23. Confirming *types* `php artisan <name> ` into the terminal — no
            // newline, so nothing executes that the user did not visibly finish and
            // press Enter on. The panel opens if it was closed; the command has to land
            // somewhere the user is looking.
            Some(PaletteMode::Artisan) => {
                if self.terminal.is_none() {
                    self.toggle_terminal(&ToggleTerminal, window, cx);
                }
                if let Some(terminal) = self.terminal.clone() {
                    terminal.update(cx, |terminal, cx| {
                        terminal.feed_text(&crate::artisan::command_line(&id), cx);
                    });
                    window.focus(&terminal.read(cx).focus_handle(cx));
                }
            }
            // #26. The id is the script name; confirming types, never runs.
            Some(PaletteMode::ComposerScripts) => {
                self.type_terminal_command(&format!("composer run-script {id} "), window, cx);
            }
            // #64. The log is read-only for now — a commit detail view is the next
            // slice, and until it exists confirming a row does nothing rather than
            // pretending to.
            Some(PaletteMode::GitLog) => {}
            // #64. The id is the branch name; the dirty-tree guard lives in the crate.
            Some(PaletteMode::Branches) => self.run_git_operation(
                move |root| elle_git::switch_branch(&root, &id).map(|_| format!("On {id}")),
                cx,
            ),
            // #19. The id is the chosen action's index into the pending edits.
            Some(PaletteMode::CodeActions) => {
                let pending = std::mem::take(&mut self.pending_code_actions);
                if let Some(edit) = id.parse::<usize>().ok().and_then(|i| pending.into_iter().nth(i)) {
                    match self.apply_workspace_edit(edit, cx) {
                        Ok(files) => {
                            self.status = Some(format!("Fix applied in {files} file(s)").into())
                        }
                        Err(err) => {
                            self.status = Some(format!("Fix not applied: {err}").into())
                        }
                    }
                    cx.notify();
                }
            }
            // #19. The "id" is the typed new name; the position it applies to was
            // captured when the prompt opened.
            Some(PaletteMode::Rename) => {
                if let Some((uri, offset)) = self.pending_rename.take() {
                    self.perform_rename(uri, offset, id, window, cx);
                }
            }
            // #127. The id is the language's own `name()`, so the mapping back is a lookup
            // over the same table the list was built from — no parallel list of strings to
            // drift out of step with `Language`.
            Some(PaletteMode::Languages) => {
                let Some(language) =
                    elle_syntax::ALL_LANGUAGES.iter().find(|language| language.name() == id)
                else {
                    return;
                };
                self.set_active_language(*language, cx);
            }
            Some(PaletteMode::Commands) => {
                // Dispatch through the same enum the keymap uses, so a palette entry and
                // a keybinding cannot drift apart.
                match dispatch_for(elle_core::CommandId(leak_id(&self.registry, &id))) {
                    Dispatch::OpenFolder => self.open_folder(&OpenFolder, window, cx),
                    Dispatch::NewFile => self.new_file(window, cx),
                    Dispatch::Save => self.save(&Save, window, cx),
                    Dispatch::CloseTab => self.close_tab(&CloseTab, window, cx),
                    Dispatch::QuickOpen => self.toggle_palette(PaletteMode::Files, window, cx),
                    Dispatch::Routes => self.toggle_palette(PaletteMode::Routes, window, cx),
                    Dispatch::CompleteRouteName => {
                        self.complete_laravel(&CompleteLaravel, window, cx)
                    }
                    Dispatch::GoToSymbol => self.go_to_symbol(&GoToSymbol, window, cx),
                    // Reopens the palette in a different mode. Safe from here: the palette
                    // that dispatched this was already dismissed at the top of the function.
                    Dispatch::SetLanguage => {
                        self.toggle_palette(PaletteMode::Languages, window, cx)
                    }
                    Dispatch::OpenSettingsFile => self.open_settings_file(window, cx),
                    Dispatch::GoToDefinition => self.go_to_definition(&GoToDefinition, window, cx),
                    Dispatch::FindReferences => self.find_references(&FindReferences, window, cx),
                    Dispatch::NavigateBack => self.navigate_back(&NavigateBack, window, cx),
                    Dispatch::NavigateForward => {
                        self.navigate_forward(&NavigateForward, window, cx)
                    }
                    Dispatch::NewTerminal => self.new_terminal(&NewTerminal, window, cx),
                    Dispatch::ToggleTerminal => self.toggle_terminal(&ToggleTerminal, window, cx),
                    Dispatch::ToggleTheme => self.toggle_theme(&ToggleTheme, window, cx),
                    Dispatch::ToggleHiddenFiles => {
                        self.toggle_hidden_files(&ToggleHiddenFiles, window, cx)
                    }
                    Dispatch::OpenSettings => self.open_settings(&OpenSettings, window, cx),
                    Dispatch::Find => self.find(&Find, window, cx),
                    Dispatch::Replace => self.replace(&Replace, window, cx),
                    Dispatch::ToggleTestPanel => {
                        self.toggle_test_panel(&ToggleTestPanel, window, cx)
                    }
                    Dispatch::RunTests => self.run_tests(&RunTests, window, cx),
                    Dispatch::RunTestsInFile => self.run_tests_in_file(&RunTestsInFile, window, cx),
                    Dispatch::RerunFailedTests => {
                        self.rerun_failed_tests(&RerunFailedTests, window, cx)
                    }
                    Dispatch::FindInProject => self.find_in_project(&FindInProject, window, cx),
                    // Reopens the palette in artisan mode, like SetLanguage above.
                    Dispatch::Artisan => self.toggle_palette(PaletteMode::Artisan, window, cx),
                    Dispatch::FormatDocument => {
                        self.format_document(&FormatDocument, window, cx)
                    }
                    Dispatch::GoToWorkspaceSymbol => {
                        self.toggle_palette(PaletteMode::WorkspaceSymbols, window, cx)
                    }
                    Dispatch::RenameSymbol => self.rename_symbol(&RenameSymbol, window, cx),
                    Dispatch::QuickFix => self.quick_fix(&QuickFix, window, cx),
                    Dispatch::ToggleLogPanel => self.toggle_log_panel(cx),
                    Dispatch::ComposerInstall => {
                        self.type_terminal_command("composer install ", window, cx)
                    }
                    Dispatch::ComposerUpdate => {
                        self.type_terminal_command("composer update ", window, cx)
                    }
                    Dispatch::ComposerRequire => {
                        self.type_terminal_command("composer require ", window, cx)
                    }
                    Dispatch::ComposerScript => {
                        self.toggle_palette(PaletteMode::ComposerScripts, window, cx)
                    }
                    Dispatch::DockerUp => self.type_docker_command("up -d", window, cx),
                    Dispatch::DockerStop => self.type_docker_command("stop", window, cx),
                    Dispatch::DockerLogs => self.type_docker_command("logs -f", window, cx),
                    Dispatch::GitFetch => self.run_git_operation(
                        |root| elle_git::fetch(&root).map(|_| "Fetched".to_string()),
                        cx,
                    ),
                    Dispatch::GitPush => self.push_to_remote(&PushToRemote, window, cx),
                    Dispatch::GitSwitchBranch => {
                        self.toggle_palette(PaletteMode::Branches, window, cx)
                    }
                    Dispatch::GitLog => self.toggle_palette(PaletteMode::GitLog, window, cx),
                    Dispatch::FoldAll => {
                        if let Some(editor) = self.active_editor().cloned() {
                            editor.update(cx, |editor, cx| editor.fold_all(cx));
                        }
                    }
                    Dispatch::UnfoldAll => {
                        if let Some(editor) = self.active_editor().cloned() {
                            editor.update(cx, |editor, cx| editor.unfold_all(cx));
                        }
                    }
                    Dispatch::Quit => cx.quit(),
                    Dispatch::Unhandled => {
                        self.status = Some(format!("{id} is not implemented yet").into());
                        cx.notify();
                    }
                }
            }
            None => {}
        }
    }
}

/// Turns a server's completion response into items that know where they came from (#61).
///
/// The source is stamped here, at the boundary where it is *known* — these came off the
/// wire from the language server and nothing else in the program has to infer it. That is
/// the difference #20 asks for: a row rendering an `LSP` badge is reporting a fact recorded
/// at the point of arrival, not a guess made from the shape of the label.
///
/// `insert_text` is preferred over the label when the server gives one, because they
/// genuinely differ: Intelephense labels a method `getName` but can ask for `getName()` to
/// be inserted, and a label like `strlen(string $string): int` is a signature to read rather
/// than text to type.
/// Returns the items and whether the server called its own list incomplete.
///
/// `is_incomplete` is carried now rather than dropped (#61's second half). It means "this is
/// not the whole answer — ask again as the prefix grows", and against a real Intelephense on
/// a 10,061-file project **every** bare-word completion set it, capped at exactly 100 items.
/// A bare `Array` response has no such flag and is complete by definition, which is why the
/// two arms differ rather than defaulting.
/// Rebuilds the Laravel index for `root` on the background pool, fire-and-forget.
///
/// Both triggers — folder open and a model/migration save — go through here, so they
/// cannot drift on how failure is handled: a failed build is a debug line, because the
/// index is a cache and every consumer already survives its absence (ADR-0008).
fn rebuild_laravel_index(root: PathBuf, cx: &mut Context<WorkspaceView>) {
    cx.background_spawn(async move {
        let Some(path) = crate::file_cache::index_path(&root) else { return };
        match elle_index::Index::open(&path) {
            Ok((index, _)) => {
                if let Err(err) = elle_index::laravel::build(
                    index.connection(),
                    &root,
                    &elle_workspace::CancelFlag::default(),
                ) {
                    tracing::debug!("laravel index build failed: {err:#}");
                }
            }
            Err(err) => tracing::debug!("laravel index unavailable: {err:#}"),
        }
    })
    .detach();
}

/// Reads at most `max_bytes` from the end of a file, cut to a full-line boundary.
///
/// A log viewer wants the recent tail of a possibly-huge file; reading the whole thing
/// to show the last screenful is waste. The first partial line at the cut is dropped so
/// a half-read entry cannot be mis-parsed as a header. Blocking — runs on the background
/// pool with the rest of the log read.
fn read_file_tail(path: &std::path::Path, max_bytes: u64) -> std::io::Result<String> {
    use std::io::{Read, Seek, SeekFrom};
    let mut file = std::fs::File::open(path)?;
    let len = file.metadata()?.len();
    if len <= max_bytes {
        let mut text = String::new();
        file.read_to_string(&mut text)?;
        return Ok(text);
    }
    file.seek(SeekFrom::Start(len - max_bytes))?;
    let mut bytes = Vec::with_capacity(max_bytes as usize);
    file.read_to_end(&mut bytes)?;
    // Drop everything up to and including the first newline — the seam is mid-line.
    let start = bytes.iter().position(|b| *b == b'\n').map_or(0, |i| i + 1);
    Ok(String::from_utf8_lossy(&bytes[start..]).into_owned())
}

/// The char boundary at or below `offset` in `text` — belt against a cursor offset that
/// a pixel hit-test placed inside a multi-byte character. Slicing `&str` at a
/// non-boundary byte panics, and this codebase edits accented Portuguese constantly, so
/// every `text[..offset]` on a cursor offset routes through here. (The editor keeps the
/// caret on boundaries, but a defence this cheap against a crash this total is worth it.)
fn char_boundary_at_or_below(text: &str, offset: usize) -> usize {
    let mut offset = offset.min(text.len());
    while offset > 0 && !text.is_char_boundary(offset) {
        offset -= 1;
    }
    offset
}

/// Whether one keystroke `text` is a word character AND the word already typed reaches
/// the autocomplete floor — the pure half of `should_open_on_word_char`, testable
/// without a language server (which the handler needs and a headless test lacks).
fn word_char_reaches_prefix_floor(text: &str, prefix: &str) -> bool {
    let mut chars = text.chars();
    let Some(c) = chars.next() else { return false };
    if chars.next().is_some() || !(c.is_alphanumeric() || c == '_') {
        return false;
    }
    prefix.chars().count() >= MIN_AUTOCOMPLETE_PREFIX
}

/// One indexed column as a popup item, provenance in the detail (`string · migration`).
/// A cast with no migration behind it says just `cast` — an empty type is not a type.
fn column_item(column: elle_index::laravel::ModelColumn) -> CompletionItem {
    let detail = if column.column_type.is_empty() {
        column.source
    } else {
        format!("{} · {}", column.column_type, column.source)
    };
    CompletionItem::new(column.name, CompletionSource::LaravelColumn).with_detail(Some(detail))
}

/// One indexed relationship as a popup item — the detail is what the method body says
/// (`hasMany · Post`), a scan's word, not a proof.
fn relation_item((name, kind, target): (String, String, String)) -> CompletionItem {
    CompletionItem::new(name, CompletionSource::LaravelRelation)
        .with_detail(Some(format!("{kind} · {target}")))
}

fn completion_items(response: CompletionResponse) -> (Vec<CompletionItem>, bool) {
    let (items, incomplete) = match response {
        CompletionResponse::Array(items) => (items, false),
        CompletionResponse::List(list) => (list.items, list.is_incomplete),
    };

    let items = items
        .into_iter()
        .map(|item| {
            let insert = item.insert_text.clone().unwrap_or_else(|| item.label.clone());
            CompletionItem::new(item.label, CompletionSource::Lsp)
                .with_insert(insert)
                .with_detail(item.detail)
        })
        .collect();

    (items, incomplete)
}

/// Where an open should land, in whichever unit the producer actually has.
///
/// Two variants because two kinds of producer exist and neither should convert at the
/// wrong moment. A [`Point`] producer (terminal links, palette rows) already has byte
/// columns. An LSP producer has UTF-16 characters, and converting those needs the line's
/// text — which for a file not yet open does not exist until the load lands. Carrying the
/// LSP position through and converting where a `Document` is in hand is what lets a
/// definition land *on the identifier* instead of at column zero, which is where these
/// jumps landed while the conversion had nowhere to happen.
#[derive(Clone, Copy, Debug)]
enum Target {
    Point(Point),
    Lsp { line: u32, character: u32 },
}

impl Target {
    /// The byte-column point, resolved against the document being revealed.
    fn resolve(self, document: &Document) -> Point {
        match self {
            Target::Point(point) => point,
            Target::Lsp { line, character } => document.point_from_lsp(line as usize, character),
        }
    }
}

/// One git write, queued for the background executor (#64 items 3–4).
enum GitWrite {
    Stage { path: PathBuf, stage: bool },
    Commit { message: String },
}

/// Which step of the context-menu interaction is in flight (#126).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum PendingKind {
    /// The menu is open and nothing has been chosen.
    Menu,
    CreateFile,
    CreateDirectory,
    Rename,
    Delete,
}

/// The row a context menu is about, and what is about to happen to it.
#[derive(Clone, Debug)]
struct PendingFileAction {
    /// The clicked row's path. For a create, the directory to create *inside*.
    path: PathBuf,
    is_dir: bool,
    kind: PendingKind,
}

/// Whether `path` is `ancestor` or sits inside it.
///
/// # Why this is not `path.starts_with(ancestor)`
///
/// The two paths reach this comparison from different places and are not spelled the same
/// way. `FileTree::new` canonicalises its root, so every path the tree hands out is
/// canonical; a tab's path is whatever opened it, which for quick open is a path built from
/// an index and for a test is a raw `TempDir` path. On macOS the system temp directory is a
/// symlink, so those two spellings are `/private/var/…` and `/var/…` — `starts_with` says
/// no, and a tab on a file that was just deleted stays open.
///
/// That tab is the problem this comparison exists for: it holds a buffer whose file is gone,
/// and the next ⌘S writes it back, quietly undoing the delete the user confirmed.
///
/// Canonicalising resolves both spellings to one. It is done on the *deleted* path's
/// ancestor and on the tab's path, and either may now fail — a deleted file cannot be
/// canonicalised at all — so both fall back to the literal comparison, which is correct
/// whenever the two spellings already agree.
fn is_under(path: &std::path::Path, ancestor: &std::path::Path) -> bool {
    if path.starts_with(ancestor) {
        return true;
    }
    // `ancestor` has just been deleted, so it cannot be canonicalised directly. Its parent
    // still exists, which is enough to normalise the prefix.
    let (Some(parent), Some(name)) = (ancestor.parent(), ancestor.file_name()) else {
        return false;
    };
    let (Ok(real_parent), Ok(real_path)) = (parent.canonicalize(), canonical_prefix(path)) else {
        return false;
    };
    real_path.starts_with(real_parent.join(name))
}

/// Canonicalises as much of `path` as still exists.
///
/// A deleted file's own path cannot be canonicalised, so this resolves the deepest ancestor
/// that does exist and re-appends the rest. Without it, every comparison against a path that
/// has just been removed fails at the first step.
fn canonical_prefix(path: &std::path::Path) -> std::io::Result<PathBuf> {
    if let Ok(real) = path.canonicalize() {
        return Ok(real);
    }
    let (Some(parent), Some(name)) = (path.parent(), path.file_name()) else {
        return Ok(path.to_path_buf());
    };
    Ok(canonical_prefix(parent)?.join(name))
}

/// The last component of a path, for a message the user reads.
///
/// Falls back to the whole path rather than to an empty string: "Delete ?" is worse than a
/// long question, and a path with no final component is a root, which is worth showing in
/// full precisely because deleting it would be the most destructive thing on offer.
fn file_name_of(path: &std::path::Path) -> String {
    path.file_name()
        .map(|name| name.to_string_lossy().to_string())
        .unwrap_or_else(|| path.display().to_string())
}

/// Files that mark the directory containing them as a project root.
///
/// `composer.json` is the PHP answer and the one that matters here. `.git` is the general
/// fallback for a file in a repository that has no manifest — a plain PHP script in a
/// scripts repo, which is a real case and one where the repository is the right scope.
///
/// Order is significance, not precedence: the walk stops at the *nearest* ancestor holding
/// any of these, so a `composer.json` inside a git repository wins by being closer.
const ROOT_MARKERS: [&str; 2] = ["composer.json", ".git"];

/// The project root for a file opened without a folder, or `None` if it has no parent.
///
/// # Why this walks up rather than taking the parent directory
///
/// The obvious answer — hand the server the file's own directory — is wrong in the case
/// that actually happens. A Laravel model lives in `app/Models`, so opening one would root
/// the server there: it would index that one directory, resolve none of the framework, and
/// answer `$this->` with nothing. The user would see a server that started and still knew
/// nothing, which is harder to diagnose than no server at all.
///
/// Walking up to the nearest [`ROOT_MARKERS`] entry gets the answer the user means. The
/// parent directory remains the fallback for a file that belongs to no project, where
/// something is still better than refusing to start.
///
/// This does not replace opening the folder. The tree is still what gives the workspace a
/// root for search, git and everything else; this is only about not leaving the server
/// pointed at a leaf directory.
fn project_root_for(path: &std::path::Path) -> Option<PathBuf> {
    let start = path.parent()?;
    let marked =
        start.ancestors().find(|dir| ROOT_MARKERS.iter().any(|marker| dir.join(marker).exists()));

    // The parent directory when nothing is marked: a loose file still gets a server, it is
    // just scoped to where it sits.
    Some(marked.unwrap_or(start).to_path_buf())
}

/// The status-bar text for the language server, which is usually nothing at all.
///
/// The rule this encodes is §24's, at the last step before pixels: **not having a language
/// server is not a problem the user has.** Nobody has Intelephense on a fresh machine and
/// most folders anyone opens are not PHP projects, so `Unavailable` renders as an empty
/// string — no icon, no "LSP: off", nothing to dismiss or wonder about. The editor with no
/// server looks exactly like the editor before this feature existed, which is the point.
///
/// What does get said:
/// - problems, when there are any, because that is the feature;
/// - `Starting…`, because a server indexing `vendor/` for thirty seconds otherwise looks
///   like nothing happening;
/// - a failure, but only after the restart budget is spent — "the server you installed
///   keeps dying" is something the user cannot learn anywhere else.
///
/// # The one exception, added by #125
///
/// `Unavailable` stays silent *except* when the file in front of the user is one a server
/// would handle. The §24 rule is that a missing server is not a problem the user has — and
/// that is true of a text file, a folder of images, or any of the projects that are not PHP.
/// It stops being true the moment a `.php` file is open: there, the user is entitled to
/// expect completion, and silence is what made #125 take a round trip to diagnose. The
/// report was "the popup never opens", because nothing on screen distinguished "no server
/// installed" from "server running and returning nothing".
///
/// It is still not a complaint. It says what is true, in the cell that already exists, only
/// while a file it applies to is open — no dialog, no icon, nothing to dismiss.
///
/// `php_open` rather than a `&Document`: the caller knows which tab is active and this
/// function stays testable without one.
fn lsp_label(lsp: &Lsp, php_open: bool) -> String {
    match lsp.state() {
        // A file the server would have handled is open and there is no server. Say so.
        LspState::Unavailable if php_open => "No language server".to_string(),
        // The two silent states, and the reason this function exists.
        LspState::Idle | LspState::Unavailable => String::new(),
        LspState::Starting => "Starting…".to_string(),
        LspState::Failed(_) => "LSP unavailable".to_string(),
        LspState::Running => match lsp.totals() {
            (0, 0) => String::new(),
            (errors, 0) => format!("{errors} ✕"),
            (0, warnings) => format!("{warnings} ⚠"),
            (errors, warnings) => format!("{errors} ✕  {warnings} ⚠"),
        },
    }
}

/// What the find bar's count area should say for `document`.
///
/// A free function so the mapping from document state to bar state — the part with the
/// three-way distinction between "no query", "no results" and "bad pattern" in it — is
/// testable without a window.
fn search_status(document: &Document) -> Status {
    let matches = document.search.matches();
    if matches.is_too_large() {
        return Status::TooLarge;
    }
    if matches.is_invalid() {
        return Status::InvalidRegex;
    }
    match document.search.position() {
        None => Status::Empty,
        Some((current, total)) => Status::Counted { current, total },
    }
}

/// Which tab is active after the one at `closed` is removed, leaving `remaining` tabs.
///
/// Closing a tab *before* the active one shifts every later tab down by one, so keeping
/// `active` where it is would silently switch the user to a different file. Clamping alone
/// was enough while ⌘W was the only way to close — it always closed the active tab, the one
/// case where clamping is right — but the per-tab ✕ closes an arbitrary index, so the shift
/// is now reachable by a single click on any tab left of the current one.
fn active_after_close(active: usize, closed: usize, remaining: usize) -> usize {
    // Follow the same file when it slides down; closing the active tab itself falls through
    // to the clamp, which lands on the neighbour that took its place (or the new last tab).
    let active = if closed < active { active - 1 } else { active };
    active.min(remaining.saturating_sub(1))
}

/// Which tab is active after the one at `from` is dragged to `to` — the strip's reorder.
///
/// The identity rule: the *file* the user was in stays active, wherever its tab lands.
/// Moving the active tab means the active index follows it; dragging another tab across
/// the active one shifts it by one, the same slide `active_after_close` handles.
fn active_after_reorder(active: usize, from: usize, to: usize) -> usize {
    if active == from {
        to
    } else if from < active && to >= active {
        active - 1
    } else if from > active && to <= active {
        active + 1
    } else {
        active
    }
}

/// Recovers the `&'static str` id for a title the palette returned.
///
/// `CommandId` holds a `&'static str` so the registry costs no allocation; the palette
/// hands back an owned String. Looking it up in the registry recovers the static without
/// leaking memory, which is why this is a lookup rather than a `Box::leak`.
fn leak_id(registry: &CommandRegistry, id: &str) -> &'static str {
    registry
        .all()
        .iter()
        .find(|command| command.id.0 == id)
        .map(|command| command.id.0)
        .unwrap_or("")
}

impl Focusable for WorkspaceView {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for WorkspaceView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.frames.tick();

        // Registers once and then costs one `is_none` per frame. See
        // `observe_window_focus` for why here and not in `new`.
        self.observe_window_focus(window, cx);
        self.start_update_check(cx);

        // Re-apply the find query to whichever document is active (#80).
        //
        // Switching tabs with the bar open has to search the *new* file, and `active_tab`
        // is assigned from six places — a tab click, a quick-open confirm, a close, two
        // open paths. Hooking all six means an `activate_tab` helper and six call sites
        // rewritten, which is a restructure this change does not need and a conflict with
        // #81, which is editing the same file.
        //
        // Doing it here instead is safe because it is idempotent and *version-guarded*:
        // `Document::set_search_query` returns immediately unless the query or the buffer
        // version moved, so the steady state is one comparison. It only costs a scan on
        // the frame where the active document genuinely changed, which is the frame that
        // needs one. `frames.tick()` already establishes that this render does work.
        if self.find.is_some() {
            self.apply_search(cx);
        }

        let theme = cx.theme().clone();
        window.set_window_title(&self.title(cx));

        div()
            .key_context(context::WORKSPACE)
            .track_focus(&self.focus_handle(cx))
            .on_action(cx.listener(Self::open_folder))
            .on_action(cx.listener(|this, _: &NewFile, window, cx| this.new_file(window, cx)))
            .on_action(cx.listener(Self::save))
            .on_action(cx.listener(Self::close_tab))
            .on_action(cx.listener(Self::toggle_command_palette))
            .on_action(cx.listener(Self::toggle_quick_open))
            .on_action(cx.listener(Self::find))
            .on_action(cx.listener(Self::replace))
            .on_action(cx.listener(Self::find_next))
            .on_action(cx.listener(Self::find_prev))
            .on_action(cx.listener(Self::find_in_project))
            .on_action(cx.listener(Self::go_to_route))
            .on_action(cx.listener(Self::complete))
            .on_action(cx.listener(Self::complete_laravel))
            .on_action(cx.listener(Self::go_to_symbol))
            .on_action(cx.listener(Self::go_to_definition))
            .on_action(cx.listener(Self::format_document))
            .on_action(cx.listener(Self::push_to_remote))
            .on_action(cx.listener(Self::switch_branch))
            .on_action(cx.listener(Self::show_git_log))
            .on_action(cx.listener(Self::rename_symbol))
            .on_action(cx.listener(Self::quick_fix))
            .on_action(cx.listener(Self::find_references))
            .on_action(cx.listener(Self::navigate_back))
            .on_action(cx.listener(Self::navigate_forward))
            .on_action(cx.listener(Self::open_settings))
            .on_action(cx.listener(Self::toggle_hidden_files))
            .on_action(cx.listener(Self::toggle_terminal))
            .on_action(cx.listener(Self::new_terminal))
            .on_action(cx.listener(Self::toggle_test_panel))
            .on_action(cx.listener(Self::run_tests))
            .on_action(cx.listener(Self::run_tests_in_file))
            .on_action(cx.listener(Self::rerun_failed_tests))
            .on_action(cx.listener(Self::toggle_theme))
            .on_action(cx.listener(|this, _: &IncreaseFontSize, _w, cx| this.zoom(Some(1.0), cx)))
            .on_action(cx.listener(|this, _: &DecreaseFontSize, _w, cx| this.zoom(Some(-1.0), cx)))
            .on_action(cx.listener(|this, _: &ResetFontSize, _w, cx| this.zoom(None, cx)))
            // Finder drops land on the whole window: a file opens, a folder becomes the
            // project. Registered here rather than on a target strip because there is no
            // wrong place to drop a file onto an editor.
            .on_drop(cx.listener(|this, paths: &gpui::ExternalPaths, window, cx| {
                this.external_drop(paths.paths(), window, cx);
            }))
            .relative()
            .size_full()
            .flex()
            .flex_col()
            .bg(theme.background)
            .text_size(Fonts::get(cx).ui_size)
            .text_color(theme.text)
            // The window's own titlebar strip (#owner report): with `appears_transparent`
            // the whole layout rose to y=0 and the activity bar's first icon sat under
            // the traffic lights — the screenshot was a file-explorer icon overlapping
            // the close button. This strip is the height the lights need, painted the
            // theme's colour (which was the point of going transparent), and it carries
            // the window title the transparency took away.
            .child(
                div()
                    .h(px(28.0))
                    .flex_none()
                    .flex()
                    .items_center()
                    .justify_center()
                    .bg(theme.panel)
                    .border_b_1()
                    .border_color(theme.border)
                    .text_color(theme.text_muted)
                    .text_size(px(12.0))
                    // Painting our own strip took the real titlebar's behaviour with it
                    // (#owner report: double-click no longer fills the screen). This
                    // hands the gesture back to the platform rather than hard-coding
                    // zoom, because macOS lets the user pick what a titlebar
                    // double-click does — Zoom, Fill, Minimize, or nothing — and
                    // `titlebar_double_click` reads that preference.
                    .on_mouse_down(MouseButton::Left, |event, window, _cx| {
                        if event.click_count == 2 {
                            window.titlebar_double_click();
                        }
                    })
                    // The app's name, not the folder's — the owner's call, and the tab
                    // bar plus the tree header already say what is open.
                    .child("ellefuanti"),
            )
            .child(
                div()
                    .flex()
                    .flex_1()
                    .overflow_hidden()
                    .child(self.render_activity_bar(&theme, cx))
                    .child(self.render_sidebar(&theme, cx))
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .flex_1()
                            .overflow_hidden()
                            .child(self.render_tab_bar(&theme, cx))
                            // Between the tabs and the text, which is where every editor
                            // puts it — and in the flow rather than absolutely positioned
                            // like the palette, so it *pushes* the text down instead of
                            // covering the first two lines of the file being searched.
                            .children(self.find.clone())
                            .child(self.render_editor_area(&theme, cx))
                            // Below the editor and inside its column, so the sidebar keeps
                            // its full height — the layout every other IDE uses.
                            .children(self.terminal.clone())
                            // Under the terminal, in the same column and by the same
                            // reasoning. Both can be open at once: a run and the shell you
                            // started it from are things people look at together.
                            .children(self.tests.clone())
                            // And the log under those, same column, same reasoning.
                            .children(self.logs.clone()),
                    ),
            )
            .child(self.render_status_bar(&theme, cx))
            .children(self.palette.clone().map(|palette| {
                // The overlay is absolutely positioned over everything, so it does not
                // reflow the layout underneath while it is open. A click on the backdrop
                // (the area around the centred panel) dismisses it — the every-overlay
                // "click outside to close" the owner expected; the panel itself stops
                // propagation so a click *on* a row still selects.
                let entity = cx.entity();
                div()
                    .id("palette-backdrop")
                    .absolute()
                    .top_0()
                    .left_0()
                    .size_full()
                    .flex()
                    .justify_center()
                    // Cross-axis start, not the flex default of `stretch`: the panel is
                    // content-sized (`max_h`, no `h`), and stretch would override that and
                    // pull it to the full window height. `justify_center` still centres it
                    // horizontally; `items_start` keeps its own `mt(90)` as the top offset.
                    .items_start()
                    .on_mouse_down(MouseButton::Left, move |_ev, window, cx| {
                        entity.update(cx, |this, cx| this.dismiss_palette(window, cx));
                    })
                    .child(
                        // The panel swallows the backdrop's click so selecting a row does
                        // not also dismiss.
                        div()
                            .id("palette-panel")
                            .on_mouse_down(MouseButton::Left, |_ev, _window, cx| {
                                cx.stop_propagation()
                            })
                            .child(palette),
                    )
            }))
            // The completion popup, in the same overlay layer and for the same reason, but
            // *not* centred: it positions itself at the cursor, which is the whole point of
            // #61 and the one thing the palette could not do. The wrapper is a full-window
            // absolute box so the popup's own `left`/`top` are window coordinates — the
            // coordinates the editor measured.
            //
            // After the palette, so if both were somehow open the completion would draw on
            // top. They are kept mutually exclusive by every path that takes focus calling
            // `dismiss_completion` — **not** by a focus-out handler, which does not exist.
            // An earlier version of this comment claimed the latter, which was a description
            // of an invariant nothing enforced: with the popup unable to see focus leave, a
            // ⌘F would have left it on screen, unfocused, its key context inactive and so
            // undismissable, still holding an offset to write at.
            .children(
                self.completion
                    .clone()
                    .map(|popup| div().absolute().top_0().left_0().size_full().child(popup)),
            )
            // The diagnostic hover card (#59): the message, at the mouse, one line below the
            // squiggle it is about. The editor computes it (only it can turn a mouse position
            // into a byte offset); this renders it, because the card sits at window
            // coordinates over every panel — the same split the completion popup uses.
            //
            // Read directly from the active editor per frame rather than mirrored into
            // workspace state: a mirror is a second copy that can go stale, and this is
            // exactly the kind of derived value render passes exist to derive.
            .children(self.active_editor().and_then(|editor| {
                let hover = editor.read(cx).hover_diagnostic.clone()?;
                Some(
                    div().absolute().top_0().left_0().size_full().child(
                        div()
                            .absolute()
                            .left(hover.position.x)
                            .top(hover.position.y)
                            .max_w(px(480.0))
                            .px_2()
                            .py_1()
                            .bg(theme.panel)
                            .border_1()
                            .border_color(theme.border)
                            .rounded(px(4.0))
                            .shadow_lg()
                            .text_color(theme.text)
                            .text_size(Fonts::get(cx).ui_size)
                            .child(hover.message),
                    ),
                )
            }))
            // The settings panel (#100): centred like the palette, modal like the tree's
            // overlays. Dismiss-on-click-outside is the panel's own `on_mouse_down_out`.
            .children(self.settings_panel.clone().map(|panel| {
                let entity = cx.entity();
                div()
                    .id("settings-backdrop")
                    .absolute()
                    .top_0()
                    .left_0()
                    .size_full()
                    .flex()
                    .items_center()
                    .justify_center()
                    // Click-away closes, like every overlay. ⌘, toggles it too, and Esc
                    // inside it — three ways out, none of them a trap.
                    .on_mouse_down(MouseButton::Left, move |_ev, window, cx| {
                        entity.update(cx, |this, cx| {
                            this.settings_panel = None;
                            match this.active_editor() {
                                Some(editor) => {
                                    let handle = editor.read(cx).focus_handle(cx);
                                    window.focus(&handle);
                                }
                                None => window.focus(&this.focus_handle),
                            }
                            cx.notify();
                        });
                    })
                    .child(
                        div()
                            .id("settings-panel")
                            .on_mouse_down(MouseButton::Left, |_ev, _window, cx| {
                                cx.stop_propagation()
                            })
                            .child(panel),
                    )
            }))
            // The tree's menu, name prompt and delete confirmation (#126), above everything
            // else because each is modal: while one is open it is the only thing the user
            // can answer.
            //
            // The wrapper is a full-window absolute box so a child's own `left`/`top` mean
            // window coordinates — the coordinates the tree row measured, and the same
            // arrangement the completion popup above relies on. Dismissing on a click
            // elsewhere is the overlay's own `on_mouse_down_out`, not a handler here; see
            // `context_menu` for why a handler on this box would eat the entry clicks.
            .children(self.overlay.clone().map(|overlay| {
                let centred = overlay.read(cx).is_centred();
                let entity = cx.entity();
                div()
                    .id("overlay-backdrop")
                    .absolute()
                    .top_0()
                    .left_0()
                    .size_full()
                    // Clicking anywhere off the menu/dialog closes it — the every-overlay
                    // dismissal the owner expected. A confirmation still requires its
                    // explicit Cancel/Confirm for the *action*, but the click-away simply
                    // closes it, which is the safe reading (nothing destructive fires).
                    .on_mouse_down(MouseButton::Left, move |_ev, window, cx| {
                        entity.update(cx, |this, cx| this.dismiss_overlay(window, cx));
                    })
                    // A menu draws at the click; a prompt and a confirmation are dialogs
                    // about the whole window and centre themselves.
                    .when(centred, |el| el.flex().items_center().justify_center())
                    .child(
                        div()
                            .id("overlay-panel")
                            .on_mouse_down(MouseButton::Left, |_ev, _window, cx| {
                                cx.stop_propagation()
                            })
                            .child(overlay),
                    )
            }))
    }
}

/// The activity bar's entries, in order, and which sidebar each one selects.
///
/// Later panels are shown disabled rather than hidden (`None`), so the shape of the product
/// is legible from the first commit (§6) without pretending they work. Git was turned on by
/// #64; **Search by #80**, which needed no new mechanism — #64 had already built the exact
/// abstraction, a `Sidebar` the activity bar selects, so find-in-project became a variant
/// rather than a second switch beside it.
///
/// Paired with `icons::ACTIVITY_ICONS` positionally, and `panels_and_icons_stay_aligned` is
/// what keeps that honest — a `zip` stops at the shorter side, so an added panel with no
/// icon would silently vanish off the bar rather than fail.
///
/// The pairing is against `ACTIVITY_ICONS`, not the whole of `icons::ICONS`, since the file
/// tree gave the binary a dozen more glyphs that have nothing to do with this bar.
///
/// A const rather than a local inside `render_activity_bar`, since #80: that test was
/// checking `icons::ICONS` against a list of names **retyped inside the test**, which
/// guards the icons and leaves the array the renderer actually zips unguarded. Renaming a
/// panel here would have kept the test green while every glyph shifted one place. There is
/// one list now, and the test reads it.
/// A fixed cell width for the database grid (#65). Fixed, not content-sized: a wide
/// text column (a `resume`, a JSON blob) would otherwise steal the whole pane and push
/// every other column off screen — the owner's report. Overflow clips and the grid
/// scrolls horizontally, the TablePlus/Excel behaviour.
const DB_CELL_WIDTH: gpui::Pixels = px(180.0);

/// The most log entries the panel keeps (#25) — a viewer, not an archive. Paired with
/// the tail read below so a megabyte log costs a bounded read and a bounded row count.
const LOG_MAX_ENTRIES: usize = 500;

/// How many characters of a word must be typed before autocomplete opens on its own
/// (#20 follow-up). VS Code uses 1 with a debounce; without a debounce, 3 keeps a stray
/// letter from opening a full list on every keystroke while still feeling immediate on a
/// real identifier. ⌥⌘I opens it at any length for the user who wants it sooner.
const MIN_AUTOCOMPLETE_PREFIX: usize = 3;

/// Rows per page in the database table view (#65) — a screenful, not a SELECT * on a
/// production table (the #65 rule). Paging beyond the first is the next slice.
const DB_PAGE_SIZE: u64 = 200;

const ACTIVITY_PANELS: [(&str, Option<Sidebar>); 7] = [
    ("Explorer", Some(Sidebar::Explorer)),
    ("Search", Some(Sidebar::Search)),
    ("Git", Some(Sidebar::Git)),
    ("Laravel", None),
    ("Database", Some(Sidebar::Database)),
    ("Docker", Some(Sidebar::Docker)),
    ("Tests", None),
];

impl WorkspaceView {
    fn render_activity_bar(&self, theme: &Theme, cx: &mut Context<Self>) -> impl IntoElement {
        let panels = ACTIVITY_PANELS;

        let entity = cx.entity();
        let active = self.sidebar;

        div()
            .w(Metrics::ACTIVITY_BAR_WIDTH)
            .flex_none()
            .flex()
            .flex_col()
            .items_center()
            .gap_1()
            .py_2()
            .bg(theme.panel)
            .border_r_1()
            .border_color(theme.border)
            .children(panels.into_iter().zip(icons::ACTIVITY_ICONS).map(
                |((name, target), icon)| {
                    let enabled = target.is_some();
                    let is_active = target == Some(active);
                    let entity = entity.clone();

                    div()
                        .id(name)
                        .size(px(32.0))
                        .flex()
                        .items_center()
                        .justify_center()
                        .rounded_md()
                        // The icon is a glyph with no label, so hovering said nothing —
                        // the owner's report. A disabled panel's tooltip names it too and
                        // adds "(coming soon)", which is the honest reason it does not
                        // respond to a click.
                        .tooltip(crate::tooltip::Tooltip::text(
                            crate::tooltip::activity_label(name, enabled),
                        ))
                        .when(enabled, |el| {
                            el.cursor_pointer()
                                .hover(|el| el.bg(theme.hover))
                                .active(|el| el.bg(theme.pressed))
                        })
                        // The selected panel is the one whose sidebar you are looking at.
                        // Before #64 this was every enabled panel, because there was only one.
                        .when(is_active, |el| el.bg(theme.selected).text_color(theme.accent))
                        // An enabled but unselected panel: readable, not shouting, and clearly
                        // not the disabled treatment below — full opacity is the difference.
                        .when(enabled && !is_active, |el| el.text_color(theme.text))
                        // A disabled panel says so by *not* responding: no hover, no pointer.
                        // The `not-allowed` cursor is what distinguishes "not ready yet" from
                        // "your click missed", and it is not a colour — the dimmer glyph alone
                        // would leave anyone who cannot separate the two greys with no signal
                        // at all (#71). `opacity` on top of the muted text is the second,
                        // non-colour channel: a disabled icon is visibly fainter in any theme.
                        .when(!enabled, |el| {
                            el.text_color(theme.text_muted)
                                .opacity(0.5)
                                .cursor(CursorStyle::OperationNotAllowed)
                        })
                        // Now there *is* a second panel to switch to, which is what the note
                        // here used to say was missing. Only the enabled ones take a click; a
                        // disabled panel still acknowledges the press and changes nothing.
                        //
                        // Search takes the same door ⌘⇧F does rather than just assigning the
                        // variant: the panel is built lazily and its query field has to take
                        // focus, or clicking the icon shows a text field that swallows typing.
                        // Leaving the sidebar cancels any search still in flight — results you
                        // can see survive, work you can no longer see does not.
                        .when_some(target, |el, target| {
                            el.on_mouse_down(MouseButton::Left, move |_ev, window, cx| {
                                entity.update(cx, |this, cx| {
                                    if target == Sidebar::Search {
                                        this.show_search_panel(window, cx);
                                        if let Some(panel) = this.search_panel.clone() {
                                            window.focus(&panel.read(cx).focus_handle(cx));
                                        }
                                    } else if this.sidebar == Sidebar::Search {
                                        this.cancel_project_search();
                                    }
                                    // The database panel loads on entry, never at
                                    // startup (#65): a down or absent database must
                                    // cost nothing until someone asks to look at it.
                                    if target == Sidebar::Database {
                                        this.load_db_schema(cx);
                                    }
                                    if target == Sidebar::Docker {
                                        this.load_docker_services(cx);
                                    }
                                    this.sidebar = target;
                                    cx.notify();
                                });
                            })
                        })
                        // 16px inside a 32px hit target: the icon is the glyph, the square is
                        // the thing you can hit, and VS Code uses the same ratio.
                        //
                        // The colour is set on the svg itself, and it has to be: gpui
                        // rasterises the SVG to an alpha mask and fills it with
                        // `style.text.color` **on that element**, which does not inherit
                        // from this parent. The comment here used to claim it did, and the
                        // bar rendered nothing at all — the same defect the tree and tab
                        // icons had. Reusing the three states above rather than a flat
                        // colour keeps the icon dimmed with its label when disabled.
                        .child(svg().path(icon.path).size(px(16.0)).text_color(if is_active {
                            theme.accent
                        } else if enabled {
                            theme.text
                        } else {
                            theme.text_muted
                        }))
                },
            ))
    }

    /// Opens or closes the Laravel log panel (#25); opening reads the newest log file.
    fn toggle_log_panel(&mut self, cx: &mut Context<Self>) {
        match self.logs.take() {
            Some(_) => cx.notify(),
            None => {
                let panel = cx.new(crate::log_view::LogView::new);
                let workspace = cx.entity();
                panel.update(cx, |panel, _| {
                    panel.on_jump(move |path, line, window, cx| {
                        let path = path.to_path_buf();
                        // A trace names files from the machine the log was written on;
                        // one that does not exist here gets silence, not a guess
                        // (the test panel's rule, RISKS #4).
                        if !path.is_file() {
                            return;
                        }
                        let point = Point::new(line.saturating_sub(1) as usize, 0);
                        workspace.update(cx, |this, cx| {
                            this.open_path_at(path, Some(point), window, cx);
                        });
                    });
                });
                self.logs = Some(panel);
                self.refresh_log_panel(cx);
            }
        }
        cx.notify();
    }

    /// Re-reads the newest `storage/logs/*.log` into the open panel.
    ///
    /// Newest by file name, which for Laravel's daily channel (`laravel-YYYY-MM-DD.log`)
    /// is also newest by date — and for the single-file channel there is only one.
    fn refresh_log_panel(&mut self, cx: &mut Context<Self>) {
        let Some(panel) = self.logs.clone() else { return };
        let Some(root) = self.tree.as_ref().map(|tree| tree.root().to_path_buf()) else { return };
        let task = cx.spawn(async move |this, cx| {
            let parsed = cx
                .background_spawn(async move {
                    let dir = root.join("storage/logs");
                    let mut logs: Vec<std::path::PathBuf> = std::fs::read_dir(&dir)
                        .map(|entries| {
                            entries
                                .flatten()
                                .map(|entry| entry.path())
                                .filter(|path| {
                                    path.extension().is_some_and(|ext| ext == "log")
                                })
                                .collect()
                        })
                        .unwrap_or_default();
                    logs.sort();
                    let newest = logs.pop()?;
                    // Read only the file's tail, not the whole thing: a real laravel.log
                    // reaches megabytes and a viewer wants the recent entries. The window
                    // is generous (256 KiB ≈ hundreds of entries) and cut at the first
                    // full line so a half-read entry at the seam is dropped, not
                    // mis-parsed.
                    let text = read_file_tail(&newest, 256 * 1024).ok()?;
                    let name = newest.file_name()?.to_string_lossy().into_owned();
                    Some((elle_laravel::parse_laravel_log_tail(&text, LOG_MAX_ENTRIES), name))
                })
                .await;
            panel
                .update(cx, |panel, cx| match parsed {
                    Some((entries, name)) => panel.set_entries(entries, Some(name), cx),
                    None => panel.set_entries(Vec::new(), None, cx),
                })
                .ok();
            this.update(cx, |_, cx| cx.notify()).ok();
        });
        self.jobs.start(Job::LogRead, task);
    }

    /// Fills the branch palette from the repository.
    fn load_branch_items(&mut self, palette: Entity<Palette>, cx: &mut Context<Self>) {
        let Some(root) = self.tree.as_ref().map(|tree| tree.root().to_path_buf()) else { return };
        let task = cx.spawn(async move |this, cx| {
            let branches =
                cx.background_spawn(async move { elle_git::branches(&root) }).await;
            let items = branches
                .unwrap_or_default()
                .into_iter()
                .map(|(name, current)| {
                    let label = if current { format!("{name}  ✓") } else { name.clone() };
                    (label, name)
                })
                .collect();
            palette.update(cx, |palette, cx| palette.set_items(items, cx)).ok();
            this.update(cx, |_, cx| cx.notify()).ok();
        });
        self.jobs.start(Job::RouteIndex, task);
    }

    /// Runs one git write on the background pool and reports its outcome (#64).
    ///
    /// One funnel for fetch/push/switch: the status line carries the CLI's own message
    /// on failure (a remote's rejection is for the user — the commit rule), and the
    /// panel refreshes after, success or not, because a failed operation may still have
    /// moved refs (a fetch that pruned, say).
    fn run_git_operation(
        &mut self,
        operation: impl FnOnce(std::path::PathBuf) -> anyhow::Result<String> + Send + 'static,
        cx: &mut Context<Self>,
    ) {
        let Some(root) = self.tree.as_ref().map(|tree| tree.root().to_path_buf()) else { return };
        let task = cx.spawn(async move |this, cx| {
            let outcome = cx.background_spawn(async move { operation(root) }).await;
            this.update(cx, |this, cx| {
                this.status = Some(match outcome {
                    Ok(message) => {
                        let message = message.trim();
                        if message.is_empty() { "Done".to_string() } else { message.to_string() }
                    }
                    Err(err) => clean_git_error(&format!("{err:#}")),
                }
                .into());
                this.refresh_git_status(cx);
                cx.notify();
            })
            .ok();
        });
        self.jobs.start(Job::GitWrite, task);
    }

    /// Toggles a schema table's expanded columns and opens its rows (#65).
    ///
    /// One click does both: expand the column list in the panel and load the rows in the
    /// main view. Clicking an already-expanded table collapses its columns but keeps the
    /// rows open — the columns are the clutter to hide, the rows are what you came for.
    fn toggle_db_table(&mut self, table: String, cx: &mut Context<Self>) {
        if !self.db_expanded.insert(table.clone()) {
            self.db_expanded.remove(&table);
        }
        self.open_db_table(table, cx);
    }

    /// Opens a table's first page of rows in the editor area (#65).
    /// Starts editing a DB cell — the buffer seeds from its current text (#65).
    fn begin_db_edit(&mut self, row: usize, col: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some((_, Ok(page))) = &self.db_table else { return };
        // Only an editable row (one with a rowid) can be edited; a WITHOUT ROWID table's
        // rows have no key and stay read-only.
        if page.rowids.get(row).copied().flatten().is_none() {
            self.status = Some("This table has no rowid — its cells are read-only".into());
            cx.notify();
            return;
        }
        let seed = page.rows.get(row).and_then(|r| r.get(col)).cloned().unwrap_or_default();
        // A NULL cell edits from empty, not the literal word.
        let seed = if seed == "NULL" { String::new() } else { seed };
        self.db_editing = Some((row, col, seed));
        // The grid takes the keyboard so the typed characters reach the cell buffer.
        window.focus(&self.focus_handle);
        cx.notify();
    }

    /// A character typed into the cell editor.
    fn db_edit_typed(&mut self, text: &str, cx: &mut Context<Self>) {
        if let Some((_, _, buffer)) = &mut self.db_editing {
            buffer.push_str(text);
            cx.notify();
        }
    }

    fn db_edit_backspace(&mut self, cx: &mut Context<Self>) {
        if let Some((_, _, buffer)) = &mut self.db_editing {
            buffer.pop();
            cx.notify();
        }
    }

    fn db_edit_cancel(&mut self, cx: &mut Context<Self>) {
        if self.db_editing.take().is_some() {
            cx.notify();
        }
    }

    /// Commits the cell edit by rowid via `update_cell`, then re-reads the page (#65).
    ///
    /// The write is guarded in the crate (rowid-addressed, name-validated, one row). An
    /// empty buffer writes an empty string; the user who wants a real NULL types `NULL`,
    /// mirroring how the grid shows one.
    /// Adds a blank row to the open table and re-reads it (#65). The user then fills the
    /// cells with the inline editor — the TablePlus "+ row" shape.
    fn db_insert_row(&mut self, cx: &mut Context<Self>) {
        let Some((table, Ok(_))) = &self.db_table else { return };
        let table = table.clone();
        let Some(root) = self.tree.as_ref().map(|tree| tree.root().to_path_buf()) else { return };
        let task = cx.spawn(async move |this, cx| {
            let outcome = cx
                .background_spawn(async move {
                    let path =
                        elle_db::env_database(&root).ok_or_else(|| "no sqlite database".to_string())?;
                    // No values from the caller: insert_row seeds NOT NULL columns with
                    // an empty string so a blank add-row never trips a constraint (the
                    // owner's courses.name error). The user then edits the cells.
                    elle_db::insert_row(&path, &table, &[])
                        .map_err(|err| format!("{err:#}"))
                        .map(|_rowid| table)
                })
                .await;
            this.update(cx, |this, cx| match outcome {
                Ok(table) => this.open_db_table(table, cx),
                Err(message) => {
                    this.status = Some(format!("insert failed: {message}").into());
                    cx.notify();
                }
            })
            .ok();
        });
        self.jobs.start(Job::DbSchema, task);
    }

    fn db_edit_commit(&mut self, cx: &mut Context<Self>) {
        let Some((row, col, buffer)) = self.db_editing.take() else { return };
        let Some((table, Ok(page))) = &self.db_table else { return };
        let (Some(rowid), Some(column)) =
            (page.rowids.get(row).copied().flatten(), page.columns.get(col).cloned())
        else {
            return;
        };
        let table = table.clone();
        let Some(root) = self.tree.as_ref().map(|tree| tree.root().to_path_buf()) else { return };

        let task = cx.spawn(async move |this, cx| {
            let outcome = cx
                .background_spawn(async move {
                    let path =
                        elle_db::env_database(&root).ok_or_else(|| "no sqlite database".to_string())?;
                    elle_db::update_cell(&path, &table, &column, rowid, &buffer)
                        .map_err(|err| format!("{err:#}"))
                        .map(|()| table)
                })
                .await;
            this.update(cx, |this, cx| {
                match outcome {
                    Ok(table) => this.open_db_table(table, cx), // re-read to show the write
                    Err(message) => {
                        this.status = Some(format!("update failed: {message}").into());
                        cx.notify();
                    }
                }
            })
            .ok();
        });
        self.jobs.start(Job::DbSchema, task);
    }

    fn open_db_table(&mut self, table: String, cx: &mut Context<Self>) {
        let Some(root) = self.tree.as_ref().map(|tree| tree.root().to_path_buf()) else { return };
        // Show the header immediately (selected state), fill the rows when the read lands.
        self.db_table = Some((table.clone(), Ok(elle_db::TablePage {
            columns: Vec::new(),
            rows: Vec::new(),
            rowids: Vec::new(),
            total: 0,
        })));
        self.db_editing = None;
        cx.notify();
        let task = cx.spawn(async move |this, cx| {
            let page = cx
                .background_spawn(async move {
                    let path = elle_db::env_database(&root)
                        .ok_or_else(|| "no sqlite database".to_string())?;
                    // The first page; paging is the panel's next slice.
                    elle_db::table_page(&path, &table, 0, DB_PAGE_SIZE)
                        .map_err(|err| format!("{err:#}"))
                        .map(|page| (table, page))
                })
                .await;
            this.update(cx, |this, cx| {
                this.db_table = Some(match page {
                    Ok((table, page)) => (table, Ok(page)),
                    Err(message) => (
                        this.db_table.as_ref().map(|(name, _)| name.clone()).unwrap_or_default(),
                        Err(message),
                    ),
                });
                cx.notify();
            })
            .ok();
        });
        self.jobs.start(Job::DbSchema, task);
    }

    /// The table-rows grid for the editor area — headers, then rows, NULL distinct.
    fn render_db_table_view(&self, theme: &Theme, cx: &mut Context<Self>) -> gpui::Div {
        let self_entity_for_cells = cx.entity();
        // Key handling for the cell editor: only meaningful while a cell is open. Bound
        // on the grid's own div so the editor keymap does not swallow the keystrokes.
        let editing = self.db_editing.is_some();
        let Some((name, result)) = &self.db_table else {
            return div();
        };
        let key_entity = self_entity_for_cells.clone();
        let mut outer = div().size_full().flex().flex_col().overflow_hidden();
        if editing {
            // While a cell is open the grid takes the keyboard: printable keys extend the
            // buffer, Backspace trims it, Enter writes it by rowid, Escape cancels. Bound
            // here rather than through the editor keymap so the two do not fight.
            outer = outer
                .track_focus(&self.focus_handle)
                .on_key_down(move |event, _window, cx| {
                    let key = event.keystroke.key.as_str();
                    key_entity.update(cx, |this, cx| match key {
                        "enter" => this.db_edit_commit(cx),
                        "escape" => this.db_edit_cancel(cx),
                        "backspace" => this.db_edit_backspace(cx),
                        _ => {
                            if let Some(text) = event.keystroke.key_char.as_deref()
                                && !text.is_empty()
                                && !text.chars().all(|c| c.is_control())
                            {
                                this.db_edit_typed(text, cx);
                            }
                        }
                    });
                });
        }
        let Ok(page) = result else {
            let Err(message) = result else { unreachable!() };
            return outer.px_3().py_2().child(
                div().text_color(theme.text_muted).child(SharedString::from(message.clone())),
            );
        };

        let header = format!(
            "{}  —  {} row(s){}",
            name,
            page.total,
            if page.total > DB_PAGE_SIZE { ", showing first page" } else { "" }
        );

        // A real grid, not a list: every cell is a fixed-width box that clips its text
        // (no wrap — the `resume` column was pushing every other column off screen), rows
        // are one line tall, and the whole grid scrolls horizontally when the columns
        // sum wider than the pane. This is the TablePlus/Excel shape the owner asked for.
        let row_height = px(24.0);
        let cell = move |text: &str, header: bool, striped: bool| {
            let mut el = div()
                .w(DB_CELL_WIDTH)
                .h(row_height)
                .flex_none()
                .px_2()
                .flex()
                .items_center()
                .overflow_hidden()
                .whitespace_nowrap()
                .text_ellipsis()
                .border_r_1()
                .border_color(theme.border)
                .child(SharedString::from(text.to_string()));
            if header {
                el = el.bg(theme.panel).text_color(theme.text);
            } else {
                el = el.text_color(theme.text_muted);
                if striped {
                    el = el.bg(theme.hover);
                }
            }
            el
        };

        // The header row and the data rows share one horizontally scrolling column, so a
        // scroll moves both together — a header that stayed put while the rows slid would
        // mislabel every value.
        let scroll = div()
            .id("db-grid-scroll")
            .flex_1()
            .overflow_scroll()
            .child(
                div()
                    .flex()
                    .flex_col()
                    .min_w_full()
                    // Header row: sticky look via the panel background, a bottom border to
                    // divide it from the data.
                    .child(
                        div()
                            .flex()
                            .flex_none()
                            .border_b_1()
                            .border_color(theme.border)
                            .children(
                                page.columns.iter().map(|c| cell(c, true, false)),
                            ),
                    )
                    .children(page.rows.iter().enumerate().map(|(row_index, row)| {
                        let editing = self.db_editing.as_ref();
                        let self_entity_for_cells = self_entity_for_cells.clone();
                        div()
                            .flex()
                            .flex_none()
                            .border_b_1()
                            .border_color(theme.border)
                            .children(row.iter().enumerate().map(move |(col_index, value)| {
                                // The cell in edit shows its buffer with a caret; every
                                // other cell is the read-only clipped text, clickable to
                                // start editing it.
                                if let Some((r, c, buffer)) = editing
                                    && *r == row_index
                                    && *c == col_index
                                {
                                    div()
                                        .w(DB_CELL_WIDTH)
                                        .h(row_height)
                                        .flex_none()
                                        .px_2()
                                        .flex()
                                        .items_center()
                                        .overflow_hidden()
                                        .whitespace_nowrap()
                                        .border_1()
                                        .border_color(theme.accent)
                                        .bg(theme.background)
                                        .text_color(theme.text)
                                        .child(SharedString::from(buffer.clone()))
                                        .child(
                                            div().w(px(2.0)).h(px(16.0)).bg(theme.cursor),
                                        )
                                        .into_any_element()
                                } else {
                                    let entity = self_entity_for_cells.clone();
                                    cell(value, false, row_index % 2 == 1)
                                        .id(gpui::ElementId::Name(
                                            format!("db-cell-{row_index}-{col_index}").into(),
                                        ))
                                        .cursor_pointer()
                                        .on_mouse_down(
                                            MouseButton::Left,
                                            move |_ev, window, cx| {
                                                entity.update(cx, |this, cx| {
                                                    this.begin_db_edit(
                                                        row_index, col_index, window, cx,
                                                    )
                                                });
                                            },
                                        )
                                        .into_any_element()
                                }
                            }))
                    })),
            );

        let add_entity = self_entity_for_cells.clone();
        outer
            .child(
                div()
                    .h(Metrics::TAB_HEIGHT)
                    .flex_none()
                    .flex()
                    .items_center()
                    .gap_3()
                    .px_3()
                    .child(div().text_color(theme.text_muted).child(SharedString::from(header)))
                    // "+ Add row" inserts a blank row the cell editor then fills — the
                    // create half of #65's data editing. Text label, not colour, so the
                    // affordance reads in any theme.
                    .child(
                        div()
                            .id("db-add-row")
                            .px_2()
                            .rounded_sm()
                            .cursor_pointer()
                            .text_color(theme.text)
                            .hover(|el| el.bg(theme.hover))
                            .child("+ Add row")
                            .on_mouse_down(MouseButton::Left, move |_ev, _window, cx| {
                                add_entity.update(cx, |this, cx| this.db_insert_row(cx));
                            }),
                    ),
            )
            .child(scroll)
    }

    /// Reads the project database's schema on the background pool, superseding.
    fn load_db_schema(&mut self, cx: &mut Context<Self>) {
        let Some(root) = self.tree.as_ref().map(|tree| tree.root().to_path_buf()) else { return };
        let task = cx.spawn(async move |this, cx| {
            let result = cx
                .background_spawn(async move {
                    let Some(path) = elle_db::env_database(&root) else {
                        // Which of the reasons applies is knowable and said: the message
                        // names the two honest cases without echoing any credential.
                        return Err("No sqlite database found (.env names another driver, \
                                    or the file does not exist)"
                            .to_string());
                    };
                    elle_db::sqlite_schema(&path).map_err(|err| format!("{err:#}"))
                })
                .await;
            this.update(cx, |this, cx| {
                this.db_schema = Some(result);
                this.db_expanded.clear();
                cx.notify();
            })
            .ok();
        });
        self.jobs.start(Job::DbSchema, task);
    }

    /// The schema browser's rows (#65): tables, their columns, types and the two
    /// shape markers. Text markers, not colour — `pk` and `?` survive every theme.
    fn render_db_panel(&self, theme: &Theme, self_entity: &Entity<Self>) -> impl IntoElement {
        // Scrolls vertically: an expanded schema (every table's columns) is taller than the
        // sidebar, and `overflow_hidden` clipped the tail — the owner could not see it all.
        // `overflow_y_scroll` needs an id (InteractiveElement).
        let body = div()
            .id("db-schema-scroll")
            .flex_1()
            .flex()
            .flex_col()
            .overflow_y_scroll()
            .px_3()
            .py_2()
            .gap_1();
        match &self.db_schema {
            None => body.child(
                div().text_color(theme.text_muted).child("Reading the database schema…"),
            ),
            Some(Err(message)) => {
                body.child(div().text_color(theme.text_muted).child(SharedString::from(message.clone())))
            }
            Some(Ok(tables)) if tables.is_empty() => {
                body.child(div().text_color(theme.text_muted).child("The database has no tables"))
            }
            Some(Ok(tables)) => body.children(tables.iter().map(|table| {
                let entity = self_entity.clone();
                let name = table.name.clone();
                let selected = self.db_table.as_ref().is_some_and(|(open, _)| open == &name);
                let expanded = self.db_expanded.contains(&name);
                // The whole table starts collapsed — a clean list of names, columns on
                // demand (the owner's "menos poluído"). The chevron is a text glyph, not
                // colour, so the state reads in any theme.
                let chevron = if expanded { "▾ " } else { "▸ " };
                div()
                    .flex()
                    .flex_col()
                    .child(
                        // Click a table name to expand its columns AND open its rows in
                        // the editor area — one gesture, the git-diff read-only pattern.
                        div()
                            .id(gpui::ElementId::Name(format!("db-table-{name}").into()))
                            .px_1()
                            .rounded_sm()
                            .when(selected, |el| el.bg(theme.hover))
                            .hover(|el| el.bg(theme.hover))
                            .text_color(theme.text)
                            .child(SharedString::from(format!("{chevron}{}", table.name)))
                            .on_mouse_down(MouseButton::Left, move |_ev, _window, cx| {
                                entity.update(cx, |this, cx| this.toggle_db_table(name.clone(), cx));
                            }),
                    )
                    .when(expanded, |el| el.children(table.columns.iter().map(|column| {
                        let mut label = format!("  {}  {}", column.name, column.column_type);
                        if column.primary_key {
                            label.push_str("  pk");
                        }
                        if column.nullable {
                            label.push_str("  ?");
                        }
                        div().text_color(theme.text_muted).child(SharedString::from(label))
                    })))
            })),
        }
    }

    /// Asks docker compose for the project's services, on entry and refocus (#25).
    ///
    /// Never at startup, and a daemon that is down or absent lands as its own words in
    /// the panel — "a broken Docker daemon cannot break the editor" is the issue's rule
    /// and this is its whole enforcement: background call, text outcome, no retry.
    fn load_docker_services(&mut self, cx: &mut Context<Self>) {
        let Some(root) = self.tree.as_ref().map(|tree| tree.root().to_path_buf()) else { return };
        let task = cx.spawn(async move |this, cx| {
            let result = cx
                .background_spawn(async move {
                    if elle_docker::detect(&root).is_none() {
                        return Err("Not a Docker project (no Dockerfile or compose file)"
                            .to_string());
                    }
                    elle_docker::services(&root).map_err(|err| format!("{err:#}"))
                })
                .await;
            this.update(cx, |this, cx| {
                this.docker_services = Some(result);
                cx.notify();
            })
            .ok();
        });
        self.jobs.start(Job::DockerPs, task);
    }

    /// The Docker panel's rows: service names with a text state marker. The actions
    /// (up/stop/logs) live in the palette and TYPE into the terminal — the #146 rule.
    fn render_docker_panel(&self, theme: &Theme) -> impl IntoElement {
        let body = div()
            .id("docker-scroll")
            .flex_1()
            .flex()
            .flex_col()
            .overflow_y_scroll()
            .px_3()
            .py_2()
            .gap_1();
        match &self.docker_services {
            None => body.child(div().text_color(theme.text_muted).child("Asking docker compose…")),
            Some(Err(message)) => body
                .child(div().text_color(theme.text_muted).child(SharedString::from(message.clone()))),
            Some(Ok(services)) if services.is_empty() => {
                body.child(div().text_color(theme.text_muted).child("No compose services"))
            }
            Some(Ok(services)) => body.children(services.iter().map(|(name, running)| {
                let marker = if *running { "running" } else { "stopped" };
                div()
                    .text_color(if *running { theme.text } else { theme.text_muted })
                    .child(SharedString::from(format!("{name}  ·  {marker}")))
            })),
        }
    }

    /// Types a command into the terminal, opening it if needed — the artisan door
    /// (#146): the command sits visibly on the prompt line, arguments are the user's to
    /// add, Enter is theirs to press. Artisan, docker and composer all walk through it.
    fn type_terminal_command(&mut self, command: &str, window: &mut Window, cx: &mut Context<Self>) {
        if self.terminal.is_none() {
            self.toggle_terminal(&ToggleTerminal, window, cx);
        }
        if let Some(terminal) = self.terminal.clone() {
            terminal.update(cx, |terminal, cx| {
                terminal.feed_text(command, cx);
            });
            window.focus(&terminal.read(cx).focus_handle(cx));
        }
    }

    fn type_docker_command(&mut self, command: &str, window: &mut Window, cx: &mut Context<Self>) {
        self.type_terminal_command(&format!("docker compose {command} "), window, cx);
    }

    /// Fills the palette with the commit graph — the log as a scrollable list (#64).
    fn load_git_log_items(&mut self, palette: Entity<Palette>, cx: &mut Context<Self>) {
        let Some(root) = self.tree.as_ref().map(|tree| tree.root().to_path_buf()) else { return };
        let task = cx.spawn(async move |this, cx| {
            let entries =
                cx.background_spawn(async move { elle_git::log(&root, 200).unwrap_or_default() }).await;
            let items = entries
                .into_iter()
                .map(|entry| {
                    // The graph column, the short hash, the subject — the same three the
                    // terminal shows, so the two agree. A graph-only connector line is a
                    // label with no id; confirming it is a no-op, which is right.
                    let label = if entry.hash.is_empty() {
                        entry.graph
                    } else {
                        format!("{}{}  {}", entry.graph, entry.hash, entry.subject)
                    };
                    (label, entry.hash)
                })
                .collect();
            palette.update(cx, |palette, cx| palette.set_items(items, cx)).ok();
            this.update(cx, |_, cx| cx.notify()).ok();
        });
        self.jobs.start(Job::RouteIndex, task);
    }

    /// Fills the composer-script palette from composer.json's own scripts.
    fn load_composer_script_items(&mut self, palette: Entity<Palette>, cx: &mut Context<Self>) {
        let Some(root) = self.tree.as_ref().map(|tree| tree.root().to_path_buf()) else { return };
        let task = cx.spawn(async move |this, cx| {
            let scripts = cx
                .background_spawn(async move {
                    let text =
                        std::fs::read_to_string(root.join("composer.json")).unwrap_or_default();
                    elle_laravel::composer_scripts(&text)
                })
                .await;
            let items = scripts.into_iter().map(|name| (name.clone(), name)).collect();
            palette.update(cx, |palette, cx| palette.set_items(items, cx)).ok();
            this.update(cx, |_, cx| cx.notify()).ok();
        });
        self.jobs.start(Job::RouteIndex, task);
    }

    /// The sidebar: the file tree, source control, or find-in-project.
    ///
    /// One sidebar with several possible contents rather than stacked columns, which is
    /// what VS Code does and what keeps the editor area the same width whichever is
    /// showing. The header names whichever panel is up, so there is never ambiguity about
    /// what the column below it is a list *of*.
    fn render_sidebar(&mut self, theme: &Theme, cx: &mut Context<Self>) -> impl IntoElement {
        let header = match self.sidebar {
            Sidebar::Explorer => self
                .tree
                .as_ref()
                .map(|tree| tree.root_name().to_uppercase())
                .unwrap_or_else(|| "NO FOLDER OPEN".to_string()),
            Sidebar::Git => "SOURCE CONTROL".to_string(),
            Sidebar::Search => "SEARCH".to_string(),
            Sidebar::Database => "DATABASE".to_string(),
            Sidebar::Docker => "DOCKER".to_string(),
        };

        div()
            .w(Metrics::SIDEBAR_WIDTH)
            .flex_none()
            .flex()
            .flex_col()
            .overflow_hidden()
            .bg(theme.panel)
            .border_r_1()
            .border_color(theme.border)
            .child(
                div()
                    .h(Metrics::TAB_HEIGHT)
                    .flex_none()
                    .flex()
                    .items_center()
                    .px_3()
                    .text_color(theme.text_muted)
                    // The name takes the room it needs and yields the rest: `flex_1` +
                    // `min_w_0` + `truncate` so a long folder name is clipped rather than
                    // shoving the buttons off the panel's right edge.
                    .child(div().flex_1().min_w_0().truncate().child(SharedString::from(header)))
                    // The expand-all / collapse-all pair, on the right of the header the
                    // way VS Code puts them — but only for the Explorer with a folder open,
                    // because there is no tree to fold otherwise. Search's and Git's headers
                    // stay a plain label.
                    .when(
                        self.sidebar == Sidebar::Explorer && self.tree.is_some(),
                        |el| el.child(self.render_explorer_header_buttons(theme, cx)),
                    )
                    // The same pair for the schema panel: there is only something to fold
                    // once tables actually loaded.
                    .when(
                        self.sidebar == Sidebar::Database
                            && matches!(&self.db_schema, Some(Ok(tables)) if !tables.is_empty()),
                        |el| el.child(self.render_db_header_buttons(theme, cx)),
                    ),
            )
            .child(match self.sidebar {
                // The panel is its own entity, so switching away and back does not
                // re-read the repository — the refresh is event-driven and a rebuilt
                // panel would have no event to wait for.
                Sidebar::Git => self.git.clone().into_any_element(),
                // Same reasoning for search: switching to Explorer and back must not throw
                // away a results list that cost a project walk to produce.
                //
                // Searching with no folder open would walk nothing, so the hint stands in
                // rather than a panel that answers "No results" to every query — which
                // reads as a broken search rather than a missing project.
                Sidebar::Search if self.tree.is_none() => div()
                    .p_3()
                    .text_color(theme.text_muted)
                    .child("Press ⌘O to open a folder")
                    .into_any_element(),
                // `None` is unreachable in practice — the sidebar only becomes `Search` by
                // way of `show_search_panel`, which builds it. Rendering nothing rather
                // than unwrapping, because a panic in a render is the worst place for one.
                Sidebar::Search => match self.search_panel.clone() {
                    Some(panel) => panel.into_any_element(),
                    None => div().into_any_element(),
                },
                Sidebar::Database => {
                    self.render_db_panel(theme, &cx.entity()).into_any_element()
                }
                Sidebar::Docker => self.render_docker_panel(theme).into_any_element(),
                Sidebar::Explorer => match self.tree.as_ref() {
                    // Wrapped so the empty space *below* the rows is right-clickable: that
                    // opens the root menu, which is the only way to create at the top
                    // level (#126). Rows stop propagation, so this fires only for clicks
                    // that miss every row. `flex_1` makes the wrapper own the leftover
                    // height — a wrapper hugging the rows would leave the space below it
                    // belonging to nothing.
                    Some(tree) if !tree.is_empty() => {
                        let entity = cx.entity();
                        let drop_entity = entity.clone();
                        let root = tree.root().to_path_buf();
                        div()
                            // A flex column, not a block: the rows list sizes itself with
                            // `flex_1`, which in a non-flex parent is the exact zero-height
                            // resolution that made the completion popup invisible.
                            .flex_1()
                            .flex()
                            .flex_col()
                            .on_mouse_down(MouseButton::Right, move |event, window, cx| {
                                entity.update(cx, |this, cx| {
                                    this.open_tree_root_menu(event.position, window, cx);
                                });
                            })
                            // The space below the rows means "the root": dropping there
                            // moves the entry to the top level, the same way the
                            // right-click there creates at the top level (#126).
                            .on_drop(move |dragged: &DraggedTreeEntry, _window, cx| {
                                drop_entity.update(cx, |this, cx| {
                                    this.drop_tree_entry(
                                        dragged.path.clone(),
                                        root.clone(),
                                        cx,
                                    );
                                });
                            })
                            .child(self.render_tree_rows(tree.len(), theme, cx))
                            .into_any_element()
                    }
                    Some(_) => {
                        // Open but empty: the menu is the only thing there is to offer.
                        let entity = cx.entity();
                        div()
                            .flex_1()
                            .on_mouse_down(MouseButton::Right, move |event, window, cx| {
                                entity.update(cx, |this, cx| {
                                    this.open_tree_root_menu(event.position, window, cx);
                                });
                            })
                            .child(
                                div()
                                    .p_3()
                                    .text_color(theme.text_muted)
                                    .child("Empty folder — right-click to create a file"),
                            )
                            .into_any_element()
                    }
                    None => div()
                        .p_3()
                        .text_color(theme.text_muted)
                        .child("Press ⌘O to open a folder")
                        .into_any_element(),
                },
            })
    }

    /// The explorer header's expand-all / collapse-all buttons.
    ///
    /// Two glyph buttons, no labels, so each carries a tooltip the way the activity-bar
    /// icons do — a bare glyph on hover said nothing there and would say nothing here. The
    /// colour is set on the `svg()` itself because gpui fills an SVG's alpha mask from
    /// `style.text.color` on that element and does not inherit it from this row (the trap
    /// the tree, tab and activity-bar icons all document).
    fn render_explorer_header_buttons(
        &self,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let entity = cx.entity();
        let muted = theme.text_muted;
        let hover = theme.hover;
        let pressed = theme.pressed;

        // (icon, tooltip, whether it expands). One closure builds both buttons so their
        // hit target, hover and press treatment cannot drift apart.
        let button = move |icon: &'static str, label: &'static str, expand: bool| {
            let entity = entity.clone();
            div()
                .id(label)
                .size(px(22.0))
                .flex()
                .flex_none()
                .items_center()
                .justify_center()
                .rounded_md()
                .cursor_pointer()
                .hover(|el| el.bg(hover))
                .active(|el| el.bg(pressed))
                .tooltip(crate::tooltip::Tooltip::text(label))
                .on_mouse_down(MouseButton::Left, move |_ev, _window, cx| {
                    entity.update(cx, |this, cx| {
                        if expand {
                            this.expand_all_tree(cx);
                        } else {
                            this.collapse_all_tree(cx);
                        }
                    });
                })
                .child(svg().path(icon).size(px(16.0)).text_color(muted))
        };

        // The reveal (mira) button is a separate handler, so it is its own element rather
        // than a third arm of the expand/collapse closure.
        let reveal_entity = cx.entity();
        let reveal = div()
            .id("reveal-file")
            .size(px(22.0))
            .flex()
            .flex_none()
            .items_center()
            .justify_center()
            .rounded_md()
            .cursor_pointer()
            .hover(|el| el.bg(hover))
            .active(|el| el.bg(pressed))
            .tooltip(crate::tooltip::Tooltip::text("Reveal active file in tree"))
            .on_mouse_down(MouseButton::Left, move |_ev, _window, cx| {
                reveal_entity.update(cx, |this, cx| this.reveal_active_file(cx));
            })
            .child(svg().path(icons::REVEAL_FILE).size(px(16.0)).text_color(muted));

        div()
            .flex()
            .flex_none()
            .items_center()
            .gap_1()
            .child(reveal)
            .child(button(icons::EXPAND_ALL, "Expand All", true))
            .child(button(icons::COLLAPSE_ALL, "Collapse All", false))
    }

    /// The database header's expand-all / collapse-all pair — the explorer's buttons
    /// (above) pointed at the schema's expanded-tables set instead of the tree. Same
    /// glyphs, same tooltips, so the two panels read as one system.
    fn render_db_header_buttons(&self, theme: &Theme, cx: &mut Context<Self>) -> impl IntoElement {
        let entity = cx.entity();
        let muted = theme.text_muted;
        let hover = theme.hover;
        let pressed = theme.pressed;

        let button = move |icon: &'static str, label: &'static str, expand: bool| {
            let entity = entity.clone();
            div()
                .id(label)
                .size(px(22.0))
                .flex()
                .flex_none()
                .items_center()
                .justify_center()
                .rounded_md()
                .cursor_pointer()
                .hover(|el| el.bg(hover))
                .active(|el| el.bg(pressed))
                .tooltip(crate::tooltip::Tooltip::text(label))
                .on_mouse_down(MouseButton::Left, move |_ev, _window, cx| {
                    entity.update(cx, |this, cx| {
                        if expand {
                            this.expand_all_db(cx);
                        } else {
                            this.collapse_all_db(cx);
                        }
                    });
                })
                .child(svg().path(icon).size(px(16.0)).text_color(muted))
        };

        div()
            .flex()
            .flex_none()
            .items_center()
            .gap_1()
            .child(button(icons::EXPAND_ALL, "Expand All", true))
            .child(button(icons::COLLAPSE_ALL, "Collapse All", false))
    }

    fn render_tree_rows(
        &self,
        count: usize,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let entity = cx.entity();
        let text = theme.text;
        let muted = theme.text_muted;
        let hover = theme.hover;
        let pressed = theme.pressed;
        let modified_color = theme.modified;
        // Paths git reports as changed, for the row tint and dot (#71's cousin, owner
        // request). Built once per frame from the panel's already-polled state — the tree
        // must not run `git status` itself, and the panel's three refresh triggers are
        // the freshness story. Absolute paths, same spelling as tree entries (both come
        // from the canonicalised root).
        let modified: std::collections::HashSet<PathBuf> =
            self.git.read(cx).state().files().iter().map(|file| file.path.clone()).collect();

        let selection = theme.selection;
        let tree_scroll = self.tree_scroll.clone();
        uniform_list("file-tree", count, move |range, _window, cx| {
            entity.update(cx, |this, _cx| {
                let Some(tree) = this.tree.as_ref() else { return Vec::new() };
                // The active tab's file gets a persistent row tint — "which file am I
                // in" answered by the tree itself (owner request). Read here, not
                // captured outside: the closure re-runs per frame and the active tab
                // changes under it.
                let active_path =
                    this.tabs.get(this.active_tab).and_then(|tab| tab.path.clone());

                range
                    .filter_map(|index| {
                        let entry = tree.entries().get(index)?;
                        let entity = entity.clone();
                        let path = entry.path.clone();
                        let is_dir = entry.is_dir();
                        let expanded = entry.expanded;
                        // A directory's glyph says open or closed — the same fact the
                        // chevron carries, deliberately doubled, because that is how VS
                        // Code draws it and the redundancy costs nothing.
                        // A directory's glyph is a themed Codicon; a file's may be a
                        // coloured Ayu icon, which carries its own colour instead of
                        // taking the row's. `None` means "use the row's colour".
                        let (icon, icon_color) = if is_dir {
                            (if expanded { icons::FOLDER_OPENED } else { icons::FOLDER }, None)
                        } else {
                            icons::for_file(&entry.name)
                        };
                        let is_modified = modified.contains(&path);
                        let is_active = active_path.as_deref() == Some(path.as_path());

                        Some(
                            div()
                                .id(("tree", index))
                                .flex()
                                .items_center()
                                .h(Metrics::ROW_HEIGHT)
                                .px_2()
                                // Indent by depth; the flat list makes this just arithmetic.
                                .pl(px(8.0 + entry.depth as f32 * 12.0))
                                .hover(|el| el.bg(hover))
                                .active(|el| el.bg(pressed))
                                // Every row can be picked up; the label under the pointer
                                // is the entry's name (the tooltip card, repurposed —
                                // gpui only wants some `Render` to float).
                                .on_drag(
                                    DraggedTreeEntry { path: path.clone() },
                                    |dragged, _offset, _window, cx| {
                                        let name = dragged
                                            .path
                                            .file_name()
                                            .map(|n| n.to_string_lossy().into_owned())
                                            .unwrap_or_default();
                                        cx.new(|_| crate::tooltip::Tooltip::new(name))
                                    },
                                )
                                // Only directories receive: dropping a file on a file has
                                // no meaning this tree wants to invent. The tint says
                                // "this folder will take it" before the mouse commits.
                                .when(is_dir, |el| {
                                    let drop_entity = entity.clone();
                                    let dest = path.clone();
                                    el.drag_over::<DraggedTreeEntry>(move |style, _, _, _| {
                                        style.bg(selection)
                                    })
                                    .on_drop(move |dragged: &DraggedTreeEntry, _window, cx| {
                                        drop_entity.update(cx, |this, cx| {
                                            this.drop_tree_entry(
                                                dragged.path.clone(),
                                                dest.clone(),
                                                cx,
                                            );
                                        });
                                    })
                                })
                                // Persistent, unlike hover: the tint follows the active
                                // tab so the tree always answers "which file am I in".
                                .when(is_active, |el| el.bg(selection))
                                .cursor_pointer()
                                .text_color(if modified.contains(&path) {
                                    modified_color
                                } else if is_dir || is_active {
                                    text
                                } else {
                                    muted
                                })
                                .on_mouse_down(MouseButton::Left, {
                                    let entity = entity.clone();
                                    move |_ev, window, cx| {
                                        entity.update(cx, |this, cx| {
                                            if is_dir {
                                                this.toggle_tree_entry(index, cx);
                                            } else {
                                                this.open_path(path.clone(), window, cx);
                                            }
                                        });
                                    }
                                })
                                // #126. The menu opens at the mouse, which is where every
                                // other file tree puts it and the only position that does
                                // not require the user to look for it.
                                //
                                // `event.position` is in window coordinates, which is what
                                // the overlay wrapper at the render root expects — it is a
                                // full-window absolute box precisely so a child's own
                                // `left`/`top` mean window coordinates (the completion
                                // popup is positioned the same way).
                                .on_mouse_down(MouseButton::Right, move |event, window, cx| {
                                    // The wrapper behind the rows opens the *root* menu on
                                    // the same event; without this, gpui's inside-out
                                    // delivery would let it replace this row's menu a
                                    // moment after it opened.
                                    cx.stop_propagation();
                                    entity.update(cx, |this, cx| {
                                        this.open_tree_menu(index, event.position, window, cx);
                                    });
                                })
                                // The disclosure slot. Fixed width whether or not there is
                                // a chevron in it, so a file's name starts at the same x as
                                // its sibling folder's — before this the file branch padded
                                // with two spaces, which only lines up in a monospace font
                                // and the sidebar is not one.
                                //
                                // A file gets an *empty* slot rather than a faint chevron:
                                // a chevron is an affordance, and drawing one on a row that
                                // does not expand is a lie about what a click does.
                                .child(div().w(px(16.0)).flex_none().flex().items_center().when(
                                    is_dir,
                                    |el| {
                                        el.child(
                                            svg()
                                                .path(if expanded {
                                                    icons::CHEVRON_DOWN
                                                } else {
                                                    icons::CHEVRON_RIGHT
                                                })
                                                .size(px(16.0))
                                                // Set here, not inherited: `svg()` fills its
                                                // alpha mask from `style.text.color` on the
                                                // element itself, and the row does not pass
                                                // one down — the same trap the file glyph
                                                // below documents (#121/#122). Without this
                                                // the chevron painted with no colour and the
                                                // disclosure triangle simply did not appear.
                                                // Muted rather than `text`: it is a secondary
                                                // affordance next to the filename, not a peer.
                                                // `muted` is the owned copy — the closure is
                                                // `move` and `'static`, so it cannot borrow
                                                // `theme`.
                                                .text_color(muted),
                                        )
                                    },
                                ))
                                // The type glyph. A themed Codicon takes its colour from
                                // `text_color` on the row above — gpui rasterises the SVG
                                // to an alpha mask and fills it with `style.text.color`,
                                // so every theme variant recolours it for free.
                                //
                                // An Ayu icon overrides that with its own colour, because
                                // for a file-type icon the colour *is* the identity (#115).
                                // That is the one place in this window where a glyph
                                // deliberately ignores the theme — see `file_icons`.
                                //
                                // Set on the row rather than on the sidebar root because
                                // this callback runs *outside* the root's style scope
                                // (`CONTEXT.md`, the `uniform_list` trap that cost four
                                // line-height PRs). A style that is not on the row is not
                                // on the row.
                                .child(
                                    div().mr_1().flex_none().flex().items_center().child(
                                        svg()
                                            .path(icon)
                                            .size(px(16.0))
                                            // The theme colour first, then the icon's own
                                            // if it has one. `svg()` paints its alpha mask
                                            // with `style.text.color`, and a row does not
                                            // pass one down — so a monochrome icon left
                                            // with `when_some(None)` was painted with no
                                            // colour at all and simply did not appear.
                                            // Only the Ayu icons, which carry their own,
                                            // were visible.
                                            .text_color(text)
                                            .when_some(icon_color, |el, c| el.text_color(rgb(c))),
                                    ),
                                )
                                .child(SharedString::from(entry.name.clone()))
                                // The non-colour half of the modified signal, per the
                                // convention that nothing is said by colour alone: the
                                // tint says it at a glance, the dot says it to everyone.
                                .when(is_modified, |el| {
                                    el.child(
                                        div()
                                            .flex_none()
                                            .pl_1()
                                            .text_color(modified_color)
                                            .child("●"),
                                    )
                                })
                                .into_any_element(),
                        )
                    })
                    .collect()
            })
        })
        .track_scroll(tree_scroll)
        .flex_1()
        // Recorded bounds for `the_file_tree_occupies_real_height`: this list rides
        // `flex_1`, which resolves to zero height the moment an ancestor stops being a
        // flex column — the completion popup shipped invisible through exactly that, and
        // the root-menu wrapper added around this list is one refactor away from it.
        .debug_selector(|| "file-tree-list".into())
    }

    fn render_tab_bar(&self, theme: &Theme, cx: &mut Context<Self>) -> impl IntoElement {
        let entity = cx.entity();
        let active = self.active_tab;

        div()
            // `id` + `overflow_x_scroll` is what gives the strip a horizontal scroll: the
            // scroll methods live on `InteractiveElement`, which a div only becomes with an
            // id (the same shape the db grid uses at `db-grid-scroll`). Without this the row
            // is a plain flex container and many tabs either squeeze below legibility or
            // overflow off the window edge with no way to reach them — the report.
            .id("tab-strip")
            // The handle is what `scroll_active_tab_into_view` drives; tracking gives
            // it the children's bounds so `scroll_to_item` can aim at a tab.
            .track_scroll(&self.tab_scroll)
            .h(Metrics::TAB_HEIGHT)
            .flex_none()
            .flex()
            .items_center()
            .overflow_x_scroll()
            .bg(theme.panel)
            .border_b_1()
            .border_color(theme.border)
            // Clearance for the macOS traffic lights: the titlebar is transparent (the
            // owner's screenshot was a white system strip over a dark theme), so this row
            // is the top of the window and the buttons overlay its left edge. 78px is
            // the standard close/min/zoom footprint with breathing room. This is padding,
            // not a flex child, so the scroll container never shrinks it away.
            .pl(px(78.0))
            .children(self.tabs.iter().enumerate().map(|(index, tab)| {
                let dirty = tab.editor.read(cx).is_dirty();
                let title = tab.editor.read(cx).document.title();
                // The same mapping the tree uses, from the same function — a tab and its
                // row in the tree showing different glyphs for one file would be worse
                // than neither having an icon. `title` is the file name (`state.rs:199`),
                // which is exactly what `for_file` wants; an untitled buffer's "untitled"
                // has no extension and lands on the generic file icon, which is right.
                let (icon, icon_color) = icons::for_file(&title);
                let entity = entity.clone();
                let close_entity = entity.clone();
                let drop_entity = entity.clone();
                let drag_title = title.clone();
                let accent = theme.accent;

                div()
                    .id(("tab", index))
                    // Tabs reorder by drag (owner request). The dragged value is the
                    // index at pick-up; the strip cannot reorder under an open drag, so
                    // resolving it at drop is safe with a bounds check.
                    .on_drag(DraggedTab { index }, move |_dragged, _offset, _window, cx| {
                        cx.new(|_| crate::tooltip::Tooltip::new(drag_title.clone()))
                    })
                    // The accent border says "it lands here" — dropping on a tab puts
                    // the dragged one in its place.
                    .drag_over::<DraggedTab>(move |style, _, _, _| {
                        style.border_l_2().border_color(accent)
                    })
                    .on_drop(move |dragged: &DraggedTab, _window, cx| {
                        drop_entity.update(cx, |this, cx| {
                            this.reorder_tab(dragged.index, index, cx);
                        });
                    })
                    .flex()
                    // `flex_none` is the other half of the scroll fix: in a plain flex row
                    // the tabs shrink to share the width, which is the "squeeze" — with many
                    // open, every tab loses its label before the strip ever overflows. Fixed
                    // basis instead: each tab keeps a legible size and the strip scrolls once
                    // they no longer fit. A min-width floors short names (`a.php`) so a tab is
                    // always big enough to click; the label truncates below so a long name
                    // does not run a single tab off the screen.
                    .flex_none()
                    .min_w(px(120.0))
                    .max_w(px(240.0))
                    .items_center()
                    .gap_2()
                    .h_full()
                    .px_3()
                    .cursor_pointer()
                    .when(index == active, |el| {
                        el.bg(theme.background).border_b_2().border_color(theme.accent)
                    })
                    // Hover only on the tabs you can actually switch to. The active tab
                    // lighting up under the pointer would suggest clicking it does
                    // something, and it does not.
                    .when(index != active, |el| {
                        el.text_color(theme.text_muted)
                            .hover(|el| el.bg(theme.hover).text_color(theme.text))
                            .active(|el| el.bg(theme.pressed))
                    })
                    .on_mouse_down(MouseButton::Left, move |_ev, _window, cx| {
                        entity.update(cx, |this, cx| {
                            this.active_tab = index;
                            this.scroll_active_tab_into_view();
                            this.clear_hover_cards(cx);
                            cx.notify();
                        });
                    })
                    // The type glyph, ahead of the name.
                    //
                    // A *fixed* 16px box, and the reason matters: #40 put the dirty dot and
                    // the close button in one shared slot precisely so a tab's width never
                    // changes as it becomes dirty or the pointer crosses it. An icon whose
                    // width depended on anything — presence, hover, state — would reopen
                    // that. This one is unconditional and constant, so every tab is exactly
                    // 16px wider than before and none of them ever move again.
                    //
                    // An Ayu icon supplies its own colour; a Codicon takes the tab's.
                    .child(
                        div().w(px(16.0)).flex_none().flex().items_center().child(
                            svg()
                                .path(icon)
                                .size(px(16.0))
                                // Set here, not inherited: `svg()` fills its alpha mask
                                // from `style.text.color` on the element itself, and the
                                // tab's own `text_color` does not reach it. Without this a
                                // Codicon had no colour and painted nothing.
                                .text_color(if index == active {
                                    theme.text
                                } else {
                                    theme.text_muted
                                })
                                .when_some(icon_color, |el, c| el.text_color(rgb(c))),
                        ),
                    )
                    // The name takes the space between the icon and the close slot, and
                    // truncates rather than widening the tab: `max_w` above only caps the tab
                    // if the label yields, so this is what keeps a long file name inside it
                    // (`whitespace_nowrap` + ellipsis, the tab-strip counterpart of the db
                    // grid cell's clamp).
                    .child(
                        div()
                            .flex_1()
                            .whitespace_nowrap()
                            .overflow_hidden()
                            .text_ellipsis()
                            .child(SharedString::from(title)),
                    )
                    .child(
                        // One slot for both the dirty marker and the close button, so the
                        // tab width never shifts as the pointer crosses it. A dirty tab
                        // still shows ✕ on hover — hiding the close affordance on exactly
                        // the tabs where closing matters most would be backwards.
                        div()
                            .id(("close", index))
                            .w(px(14.0))
                            .flex()
                            .items_center()
                            .justify_center()
                            .rounded_sm()
                            .text_color(if dirty { theme.accent } else { theme.text_muted })
                            .child(if dirty { "•" } else { "✕" })
                            .cursor_pointer()
                            .hover(|el| el.bg(theme.hover).text_color(theme.text))
                            .active(|el| el.bg(theme.pressed))
                            .on_mouse_down(MouseButton::Left, move |_ev, window, cx| {
                                close_entity.update(cx, |this, cx| {
                                    this.close_tab_at(index, window, cx);
                                });
                                // Without this the tab's own handler also fires and
                                // activates the tab being closed.
                                cx.stop_propagation();
                            }),
                    )
            }))
    }

    fn render_editor_area(&self, theme: &Theme, cx: &mut Context<Self>) -> impl IntoElement {
        // The diff takes the editor area while the Git panel has a row selected, and only
        // then. It is not a tab: a tab implies something you can edit and close, and this
        // is a read-only view of a file that already has a tab of its own. Selecting a row
        // shows it, switching back to Explorer or selecting nothing puts the editor back —
        // no state to clean up and nothing that can strand a buffer.
        let diff = match (self.sidebar, self.git_diff.as_ref()) {
            (Sidebar::Git, Some(renderer)) => {
                self.git.read(cx).diff().map(|file| (file.clone(), renderer))
            }
            _ => None,
        };

        let db_table = matches!(self.sidebar, Sidebar::Database)
            .then(|| self.db_table.as_ref())
            .flatten();

        div().flex_1().overflow_hidden().child(match (diff, db_table, self.active_editor()) {
            (Some((file, renderer)), _, _) => {
                render_diff(&file, renderer, theme, cx).into_any_element()
            }
            (None, Some(_), _) => self.render_db_table_view(theme, cx).into_any_element(),
            (None, None, Some(editor)) => editor.clone().into_any_element(),
            (None, None, None) => div()
                .size_full()
                .flex()
                .items_center()
                .justify_center()
                .text_color(theme.text_muted)
                .child("⌘O open folder   ⌘P quick open   ⌘⇧P commands")
                .into_any_element(),
        })
    }

    fn render_status_bar(&self, theme: &Theme, cx: &mut Context<Self>) -> impl IntoElement {
        let (position, language) = match self.active_editor() {
            Some(editor) => {
                let editor = editor.read(cx);
                let point = editor.document.cursor_point();
                (
                    format!("Ln {}, Col {}", point.row + 1, point.column + 1),
                    editor.document.language().name().to_string(),
                )
            }
            None => (String::new(), String::new()),
        };

        // Only shown while the panel is open, so it reads as state rather than chrome.
        let terminals = self
            .terminal
            .as_ref()
            .map(|terminal| match terminal.read(cx).session_count() {
                1 => "1 terminal".to_string(),
                count => format!("{count} terminals"),
            })
            .unwrap_or_default();

        let diagnostics = lsp_label(&self.lsp, self.active_tab_wants_a_server());

        // Like the terminal count, only while the panel is open. A project with no test
        // framework — and one that has simply not run its tests — says nothing here, the
        // same rule `lsp_label` follows for a project with no language server (§24).
        let tests = self.tests.as_ref().map(|tests| tests.read(cx).summary()).unwrap_or_default();

        // A squiggle the user cannot read only says "something is wrong near here". Putting
        // the cursor on it spells it out, which is the cheapest thing that makes the
        // underlines actually useful — a hover card or a problems panel is a bigger feature
        // (#59 keeps this to diagnostics only).
        //
        // `self.status` wins the cell: a failed save is a thing the user just did, and it
        // must not be buried under a diagnostic that was already there.
        let message = self.status.clone().unwrap_or_else(|| {
            self.cursor_diagnostic(cx).map(SharedString::from).unwrap_or_default()
        });

        div()
            .h(Metrics::STATUS_HEIGHT)
            .flex_none()
            .flex()
            .items_center()
            .gap_4()
            .px_3()
            .bg(theme.status_bar)
            .border_t_1()
            .border_color(theme.border)
            .text_color(theme.text_muted)
            .child(
                div()
                    .flex_1()
                    // An error is the one thing in the status bar worth colouring.
                    .when(self.status.is_some(), |el| el.text_color(theme.accent))
                    .child(message),
            )
            // The updater's cell: absent until there is something to do, accent-coloured
            // because it is the one cell that asks for a click (owner request — the
            // "restart to update" every reference IDE shows).
            .when_some(self.update_state.status_label(), |el, label| {
                let entity = cx.entity();
                el.child(
                    div()
                        .id("status-update")
                        .px_1()
                        .rounded(px(3.0))
                        .cursor_pointer()
                        .text_color(theme.accent)
                        .hover(|el| el.bg(theme.hover))
                        .on_mouse_down(MouseButton::Left, move |_ev, _window, cx| {
                            entity.update(cx, |this, cx| this.update_clicked(cx));
                        })
                        .child(SharedString::from(label)),
                )
            })
            .child(SharedString::from(diagnostics))
            .child(SharedString::from(tests))
            .child(SharedString::from(terminals))
            .child(SharedString::from(position))
            // #127. The language cell is a button, which is where every comparable editor
            // puts this control and the only affordance an untitled buffer has: it has no
            // path, so nothing detects a language for it and it can never be anything but
            // plain text otherwise.
            //
            // Only when a tab is open. An empty cell that is nonetheless clickable is the
            // kind of dead target #71 is about.
            .when(!language.is_empty(), |el| {
                let entity = cx.entity();
                el.child(
                    div()
                        .id("status-language")
                        .px_1()
                        .rounded(px(3.0))
                        .cursor_pointer()
                        .hover(|el| el.bg(theme.hover))
                        .on_mouse_down(MouseButton::Left, move |_ev, window, cx| {
                            entity.update(cx, |this, cx| {
                                this.toggle_palette(PaletteMode::Languages, window, cx);
                            });
                        })
                        .child(SharedString::from(language)),
                )
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::rc::Rc;

    #[test]
    fn a_fatal_prefix_is_dropped_from_a_git_error() {
        // The status bar already implies "this is a git failure"; the `fatal:` label just
        // pushes the actual sentence right. What is left is the reason, in full.
        assert_eq!(
            clean_git_error("fatal: not a git repository"),
            "not a git repository"
        );
        assert_eq!(clean_git_error("error: pathspec 'x' did not match"), "pathspec 'x' did not match");
    }

    #[test]
    fn only_the_first_meaningful_line_is_shown() {
        // git stderr is a paragraph; the status bar is one line. The first line that carries
        // a message wins, and blank/bare-label lines ahead of it are skipped.
        let raw = "\nfatal: unable to access 'https://…': Could not resolve host\nhint: check your network";
        assert_eq!(clean_git_error(raw), "unable to access 'https://…': Could not resolve host");
    }

    #[test]
    fn a_hook_rejection_is_kept_intact() {
        // A pre-commit hook's message is FOR the user — the whole point of surfacing it. Only
        // the leading label is trimmed; the hook's own words survive untouched.
        let raw = "error: failed to push some refs\nremote: rejected by pre-receive hook: run the linter first";
        // First meaningful line is the summary; its label is dropped, its content kept.
        assert_eq!(clean_git_error(raw), "failed to push some refs");
    }

    #[test]
    fn a_message_with_no_prefix_survives_unchanged() {
        assert_eq!(clean_git_error("Everything up-to-date"), "Everything up-to-date");
    }

    #[test]
    fn a_later_error_word_in_the_sentence_is_not_stripped() {
        // Only ONE leading label comes off. A message that mentions "error:" further along
        // keeps it — stripping every occurrence would eat real content.
        assert_eq!(
            clean_git_error("fatal: the commit hook printed error: bad config"),
            "the commit hook printed error: bad config"
        );
    }

    #[test]
    fn an_all_label_error_falls_back_to_the_raw_text() {
        // Degenerate input (only blank lines and bare labels) still yields something rather
        // than an empty status bar.
        assert_eq!(clean_git_error("fatal\n\nerror"), "fatal\n\nerror");
    }

    #[test]
    fn autocomplete_opens_on_a_word_char_only_past_the_prefix_floor() {
        // A trigger char is handled separately; this is the type-an-identifier path.
        // Below the floor stays quiet (no popup on every letter); at the floor it opens.
        assert!(!word_char_reaches_prefix_floor("e", "us"), "'us' + 'e' -> prefix 'us', too short");
        assert!(word_char_reaches_prefix_floor("e", "use"), "'use' + 'e' -> prefix 'use', opens");
        // Not a single word character.
        assert!(!word_char_reaches_prefix_floor("(", "getName"), "a symbol is the trigger path");
        assert!(!word_char_reaches_prefix_floor("ab", "getName"), "one keystroke, not a paste");
        assert!(!word_char_reaches_prefix_floor(" ", "getName"), "whitespace does not open it");
        // A digit or underscore is a word character.
        assert!(word_char_reaches_prefix_floor("_", "obj_"), "underscore is a word char");
    }

    #[test]
    fn read_file_tail_returns_the_end_at_a_line_boundary() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("big.log");
        // Ten numbered lines; ask for a window that only covers the last few, small
        // enough that the cut lands mid-line so the boundary trim is exercised.
        let body: String = (0..10).map(|i| format!("line {i}
")).collect();
        std::fs::write(&path, &body).unwrap();

        // A window bigger than the file returns it whole.
        assert_eq!(read_file_tail(&path, 10_000).unwrap(), body);

        // A ~25-byte window covers the last 3-ish lines; the partial first line is
        // dropped, so every line returned is complete.
        let tail = read_file_tail(&path, 25).unwrap();
        assert!(tail.ends_with("line 9
"));
        assert!(!tail.contains("line 0"), "the old lines are outside the window");
        for line in tail.lines() {
            assert!(line.starts_with("line "), "no half-line at the seam: {line:?}");
        }
    }

    /// A `CompletionList` carrying the server's own `isIncomplete`.
    fn incomplete_list(labels: &[&str], is_incomplete: bool) -> CompletionResponse {
        CompletionResponse::List(elle_lsp::lsp_types::CompletionList {
            is_incomplete,
            items: labels
                .iter()
                .map(|label| elle_lsp::lsp_types::CompletionItem {
                    label: (*label).to_string(),
                    ..Default::default()
                })
                .collect(),
        })
    }

    #[test]
    fn the_servers_incompleteness_survives_the_trip_off_the_wire() {
        // The flag #61's re-request is driven by, tested where it is actually decoded.
        //
        // This is a unit test rather than a render test for a reason worth recording: this
        // function only runs with a live server, so **no headless test reaches it**. Making
        // `completion_items` return `false` unconditionally left the entire 1000-test suite
        // green — checked, not assumed — which is exactly the vacuum this repository keeps
        // finding. The decode is pure, so it can be tested directly, and now is.
        let (items, incomplete) = completion_items(incomplete_list(&["strlen", "strpos"], true));
        assert_eq!(items.len(), 2);
        assert!(incomplete, "a truncated list must be reported as truncated");
        assert!(items.iter().all(|item| item.source == CompletionSource::Lsp));

        let (_, complete) = completion_items(incomplete_list(&["strlen"], false));
        assert!(!complete, "and a complete one must not be");
    }

    #[test]
    fn a_bare_array_response_is_complete_by_definition() {
        // The protocol gives an `Array` response no `isIncomplete` field at all, so there is
        // nothing to read and the honest default is "this is the whole answer". Defaulting
        // the other way would make every such server re-requested on every keystroke for no
        // reason.
        let response = CompletionResponse::Array(vec![elle_lsp::lsp_types::CompletionItem {
            label: "strlen".into(),
            ..Default::default()
        }]);
        let (items, incomplete) = completion_items(response);
        assert_eq!(items.len(), 1);
        assert!(!incomplete);
    }

    /// `render_activity_bar` zips its panel list against `icons::ACTIVITY_ICONS`, and `zip`
    /// stops at the shorter side — so adding a panel without adding an icon would silently
    /// drop the last panel off the bar rather than fail. Assert the lengths match.
    #[test]
    fn a_dotted_route_name_needs_the_whole_literal_not_the_word_before_the_cursor() {
        // The bug this pins was real and shipped in an earlier draft of #61. A route name is
        // dotted; `word_before` stops at the `.`, so in `route('users.sh|')` the popup's
        // query was `sh` while the range it would overwrite began at `users`. Accepting
        // `users.show` then wrote the full name over a range starting at `u` and produced
        // `users.users.show`.
        //
        // The fix is that the Laravel source corrects *both* — the query and the range — and
        // this states the property that makes them consistent: whatever span the popup will
        // overwrite, the query must be exactly the text already typed inside it.
        let source = "<?php\nroute('users.sh');\n";
        let cursor = source.find("');").expect("the cursor sits just before the closing quote");

        let reference = elle_laravel::reference_at(source, cursor, false)
            .expect("the cursor is inside a route() literal");
        assert_eq!(reference.kind, elle_laravel::ReferenceKind::Route);

        // What the generic scan would have used, and why it is not enough on its own.
        assert_eq!(
            crate::completion::word_before(source, cursor),
            "sh",
            "the generic scan stops at the dot — this is the input to the bug, not the bug"
        );

        // What the route path uses instead: the literal's own span.
        let range = reference.range.clone();
        let typed = &source[range.start..cursor];
        assert_eq!(typed, "users.sh", "the query must cover the whole literal typed so far");

        // And the property that matters — replacing `range.start..cursor` with the accepted
        // name yields the name itself, not a doubled prefix.
        let mut buffer = source.to_string();
        buffer.replace_range(range.start..cursor, "users.show");
        assert!(buffer.contains("route('users.show')"), "got {buffer:?}");
        assert!(!buffer.contains("users.users.show"), "the prefix must not be doubled");
    }

    /// `render_activity_bar` zips its panel list against `icons::ICONS`, and `zip` stops at
    /// the shorter side — so adding a panel without adding an icon would silently drop the
    /// last panel off the bar rather than fail. Assert the lengths match.
    ///
    /// The names are asserted too, because equal lengths in the wrong order is the other
    /// way to get this wrong, and it is the more confusing one: every icon renders, each
    /// against the wrong panel.
    ///
    /// The slice, not the whole table: the file tree and tab bar added a dozen glyphs to
    /// `icons::ICONS` that this bar never draws. `icons.rs` has the matching test that the
    /// slice really is the bar's seven and stops there.
    #[test]
    fn panels_and_icons_stay_aligned() {
        assert_eq!(
            icons::ACTIVITY_ICONS.len(),
            ACTIVITY_PANELS.len(),
            "the activity bar renders {} panels; add the matching icon to icons::ICONS and \
             widen the ACTIVITY_ICON_COUNT prefix",
            ACTIVITY_PANELS.len()
        );

        for (icon, (name, _)) in icons::ACTIVITY_ICONS.iter().zip(ACTIVITY_PANELS) {
            assert_eq!(
                icon.path,
                format!("icons/{}.svg", name.to_lowercase()),
                "icons::ACTIVITY_ICONS is out of order: every panel would get the wrong glyph"
            );
        }
    }

    /// The tree and the tab bar choose a file's glyph from the *same* input.
    ///
    /// Two mappings would drift — a file would show one icon in the sidebar and another in
    /// its tab, and whoever noticed would have no way to tell which was right. Both call
    /// `icons::for_file`, so the only way they can still disagree is by feeding it
    /// different strings: the tree passes the tree entry's `name`, the tab bar passes
    /// `document.title()`. This asserts those are the same string for a real path, which is
    /// the assumption the shared mapping rests on and the one that is not obvious.
    ///
    /// Both sides are checked against a real `FileTree` over real files, because the tab's
    /// half of the claim is `Path::file_name` (`editor/state.rs:199`, what `title()` does)
    /// and the tree's half is whatever `FileTree` decides to put in `Entry::name`. Asserting
    /// they agree is only worth anything if one of them is not retyped from the other.
    #[test]
    fn a_tab_and_its_tree_row_are_named_the_same_thing() {
        let dir = tempfile::tempdir().expect("a tempdir");
        let names = ["User.php", "welcome.blade.php", "composer.lock", "logo.png", "Makefile"];
        for name in names {
            std::fs::write(dir.path().join(name), "").expect("a file");
        }

        let tree = FileTree::new(dir.path()).expect("a tree");
        let entries: Vec<_> = tree.entries().iter().filter(|e| !e.is_dir()).collect();
        assert_eq!(entries.len(), names.len(), "the fixture did not land as expected");

        for entry in entries {
            // What the tab bar passes to `for_file`, computed from the path independently
            // of whatever the tree stored.
            let title = entry.path.file_name().unwrap().to_string_lossy().to_string();

            assert_eq!(
                icons::for_file(&entry.name),
                icons::for_file(&title),
                "{:?} would draw a different glyph in its tab than in the tree",
                entry.path
            );
        }
        // That each of those paths is actually in `ICONS` — the blank-square failure — is
        // `icons::tests::every_named_path_is_in_the_table`, which has the trait in scope
        // and a far wider sample of names than this fixture.
    }

    /// Every enabled panel selects a *distinct* sidebar, and every `Sidebar` is reachable.
    ///
    /// Two failure modes, neither of which the alignment test above can see. Two entries
    /// pointing at the same variant would light up together and switch to the same panel;
    /// a variant with no entry would be a sidebar the user has no way to reach — which is
    /// exactly what `Sidebar::Search` was between #64 and #80, and the reason the enum was
    /// worth reading rather than a bool.
    #[test]
    fn every_enabled_panel_selects_a_distinct_and_reachable_sidebar() {
        let targets: Vec<Sidebar> = ACTIVITY_PANELS.iter().filter_map(|(_, t)| *t).collect();

        let mut unique = targets.clone();
        unique.sort_by_key(|s| format!("{s:?}"));
        unique.dedup();
        assert_eq!(unique.len(), targets.len(), "two entries select the same sidebar: {targets:?}");

        for sidebar in [Sidebar::Explorer, Sidebar::Search, Sidebar::Git] {
            assert!(
                targets.contains(&sidebar),
                "{sidebar:?} has no activity-bar entry, so nothing can select it"
            );
        }
    }

    // --- palette ids that carry a place -------------------------------------------

    #[test]
    fn a_target_id_round_trips_through_the_palette() {
        // The palette can only carry a string, so a jump target has to survive being
        // flattened into one and parsed back. Row 0 is included because `saturating_sub`
        // on a 1-based line number produces it and `0` must not be read as "no target".
        for row in [0, 1, 41, 100_000] {
            let id = target_id(std::path::Path::new("/srv/app/routes/web.php"), row);
            let (path, target) = split_target_id(&id);
            assert_eq!(path, PathBuf::from("/srv/app/routes/web.php"));
            assert_eq!(target, Some(Point::new(row, 0)), "row {row} did not survive the id");
        }
    }

    #[test]
    fn a_bare_path_decodes_to_no_target() {
        // Quick open's ids are plain paths and must keep opening at the top of the file.
        // Both kinds of row reach the same confirm handler, so this is not a hypothetical.
        let (path, target) = split_target_id("/srv/app/Models/User.php");
        assert_eq!(path, PathBuf::from("/srv/app/Models/User.php"));
        assert_eq!(target, None);
    }

    #[test]
    fn a_colon_in_a_filename_is_not_read_as_a_row() {
        // A colon is legal in a macOS filename. Splitting on it unconditionally would open
        // the wrong path — silently, and only for the user who names files this way.
        let (path, target) = split_target_id("/srv/notes:draft.php");
        assert_eq!(path, PathBuf::from("/srv/notes:draft.php"));
        assert_eq!(target, None, "the suffix is not all digits, so it is part of the name");

        // And the same name *with* a row still splits at the last colon.
        let (path, target) = split_target_id("/srv/notes:draft.php:7");
        assert_eq!(path, PathBuf::from("/srv/notes:draft.php"));
        assert_eq!(target, Some(Point::new(7, 0)));
    }

    // --- go to definition: reading the three response shapes ------------------------

    fn location(path: &str, line: u32) -> Location {
        Location {
            uri: crate::lsp_session::uri_for(std::path::Path::new(path)).unwrap(),
            range: elle_lsp::lsp_types::Range {
                start: elle_lsp::lsp_types::Position { line, character: 4 },
                end: elle_lsp::lsp_types::Position { line, character: 8 },
            },
        }
    }

    #[test]
    fn every_definition_response_shape_yields_a_place_to_jump() {
        // The protocol has three and servers use all of them. Handling only the one the
        // server of the day sends is a feature that silently does nothing after a swap.
        let scalar = GotoDefinitionResponse::Scalar(location("/srv/app/User.php", 12));
        // The character comes through raw — UTF-16, converted only where a document
        // exists (`Target::resolve`), which is what lets the jump land on the identifier.
        assert_eq!(first_location(&scalar), Some((PathBuf::from("/srv/app/User.php"), 12, 4)));

        // An array takes the first: F12 means jump, not "ask me which".
        let array = GotoDefinitionResponse::Array(vec![
            location("/srv/app/User.php", 12),
            location("/srv/app/Other.php", 99),
        ]);
        assert_eq!(first_location(&array), Some((PathBuf::from("/srv/app/User.php"), 12, 4)));
    }

    #[test]
    fn a_link_response_lands_on_the_identifier_not_the_declaration() {
        // `target_range` covers the whole declaration including its doc comment;
        // `target_selection_range` is the name. Landing on the comment reads as an
        // off-by-several to the user, and the wrong field is the easy one to reach for.
        let link = GotoDefinitionResponse::Link(vec![elle_lsp::lsp_types::LocationLink {
            origin_selection_range: None,
            target_uri: crate::lsp_session::uri_for(std::path::Path::new("/srv/app/User.php"))
                .unwrap(),
            target_range: elle_lsp::lsp_types::Range {
                start: elle_lsp::lsp_types::Position { line: 8, character: 0 },
                end: elle_lsp::lsp_types::Position { line: 20, character: 0 },
            },
            target_selection_range: elle_lsp::lsp_types::Range {
                start: elle_lsp::lsp_types::Position { line: 12, character: 4 },
                end: elle_lsp::lsp_types::Position { line: 12, character: 8 },
            },
        }]);

        assert_eq!(
            first_location(&link),
            Some((PathBuf::from("/srv/app/User.php"), 12, 4)),
            "must use the selection range, not the enclosing one"
        );
    }

    #[test]
    fn an_empty_definition_response_is_not_a_jump() {
        // Servers answer with an empty array for a keyword or an unindexed symbol. Taking
        // `[0]` unguarded would panic on exactly that, in the middle of ordinary use.
        assert_eq!(first_location(&GotoDefinitionResponse::Array(vec![])), None);
        assert_eq!(first_location(&GotoDefinitionResponse::Link(vec![])), None);
    }

    // --- jump history ---------------------------------------------------------------
    //
    // These use real files on a real disk rather than invented paths, because `back` and
    // `forward` skip entries whose file has gone — a history of fictional paths would
    // exercise only the skipping and never the retracing.

    /// A directory of real `.php` files to point history entries at.
    fn files(names: &[&str]) -> (tempfile::TempDir, Vec<PathBuf>) {
        let dir = tempfile::tempdir().unwrap();
        let paths = names
            .iter()
            .map(|name| {
                let path = dir.path().join(name);
                std::fs::write(&path, "<?php\n").unwrap();
                path
            })
            .collect();
        (dir, paths)
    }

    fn at(path: &std::path::Path, row: usize) -> (PathBuf, Point) {
        (path.to_path_buf(), Point::new(row, 0))
    }

    #[test]
    fn back_returns_to_where_the_jump_started() {
        let (_dir, paths) = files(&["a.php", "b.php"]);
        let mut history = JumpHistory::default();
        history.push(at(&paths[0], 10));

        // Standing at the definition, Back returns to the call site.
        assert_eq!(history.back(at(&paths[1], 50)), Some(at(&paths[0], 10)));
        // And nothing is left to go back to.
        assert_eq!(history.back(at(&paths[0], 10)), None);
    }

    #[test]
    fn forward_undoes_a_back() {
        // The round trip that makes the pair worth having: ⌃- then ⌃⇧- puts you back where
        // you were, not somewhere a third of the way through the trail.
        let (_dir, paths) = files(&["a.php", "b.php"]);
        let mut history = JumpHistory::default();
        history.push(at(&paths[0], 10));

        let back = history.back(at(&paths[1], 50)).unwrap();
        assert_eq!(back, at(&paths[0], 10));
        assert_eq!(history.forward(back), Some(at(&paths[1], 50)));
    }

    #[test]
    fn a_new_jump_abandons_the_forward_trail() {
        // Browser behaviour, and for the browser's reason: once you go somewhere else, the
        // places you could have gone forward to are not on the path you took.
        let (_dir, paths) = files(&["a.php", "b.php", "c.php"]);
        let mut history = JumpHistory::default();
        history.push(at(&paths[0], 10));
        history.back(at(&paths[1], 50));
        assert!(!history.forward.is_empty());

        history.push(at(&paths[2], 1));
        assert!(history.forward.is_empty(), "a new jump must clear the forward stack");
    }

    #[test]
    fn forward_with_nothing_to_undo_does_nothing() {
        let (_dir, paths) = files(&["a.php"]);
        let mut history = JumpHistory::default();
        assert_eq!(history.forward(at(&paths[0], 1)), None);
    }

    #[test]
    fn a_deleted_file_is_skipped_rather_than_desynchronising_the_trail() {
        // `open_path_at` is asynchronous and cannot report failure back to the history, so
        // handing it a path that no longer exists would leave the cursor put while the
        // history believed the jump happened — every later Back off by one.
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("real.php");
        std::fs::write(&real, "<?php\n").unwrap();

        let mut history = JumpHistory::default();
        history.push((real.clone(), Point::new(3, 0)));
        // Pushed later, so it is popped first — and it is gone.
        history.push((dir.path().join("deleted.php"), Point::new(7, 0)));

        let here = at(std::path::Path::new("/srv/current.php"), 1);
        assert_eq!(
            history.back(here),
            Some((real, Point::new(3, 0))),
            "the missing entry must be skipped, not returned"
        );
    }

    #[test]
    fn a_history_of_nothing_but_deleted_files_is_a_no_op() {
        // And must not loop forever draining the stack, nor return a path that cannot open.
        let mut history = JumpHistory::default();
        history.push(at(std::path::Path::new("/srv/definitely/not/here.php"), 1));
        history.push(at(std::path::Path::new("/srv/also/gone.php"), 2));

        assert_eq!(history.back(at(std::path::Path::new("/srv/current.php"), 0)), None);
    }

    #[test]
    fn the_history_is_bounded() {
        // It grows for the whole session and nothing else trims it. Unbounded, a long
        // session with a lot of navigation is a slow leak of paths nobody will retrace.
        let mut history = JumpHistory::default();
        for row in 0..MAX_HISTORY + 10 {
            history.push(at(std::path::Path::new("/srv/a.php"), row));
        }

        assert_eq!(history.back.len(), MAX_HISTORY);
        // The *oldest* entries are the ones dropped, so Back still retraces the most
        // recent jumps rather than ancient ones.
        assert_eq!(history.back.last().unwrap().1.row, MAX_HISTORY + 9);
    }

    /// Stands in for a `Task`, recording its own cancellation.
    ///
    /// A dropped gpui `Task` *is* a cancelled task (`async_task::Task::drop` calls
    /// `set_canceled`), but gpui offers no way to observe that, so the eviction rule is
    /// tested against a handle whose drop is visible.
    struct FakeTask {
        job: Job,
        cancelled: Rc<RefCell<Vec<Job>>>,
    }

    impl Drop for FakeTask {
        fn drop(&mut self) {
            self.cancelled.borrow_mut().push(self.job);
        }
    }

    struct Harness {
        slots: JobSlots<FakeTask>,
        cancelled: Rc<RefCell<Vec<Job>>>,
    }

    impl Harness {
        fn new() -> Self {
            Self { slots: JobSlots::default(), cancelled: Rc::new(RefCell::new(Vec::new())) }
        }

        fn start(&mut self, job: Job) {
            let task = FakeTask { job, cancelled: self.cancelled.clone() };
            self.slots.start(job, task);
        }

        fn cancelled(&self) -> Vec<Job> {
            self.cancelled.borrow().clone()
        }
    }

    #[test]
    fn starting_a_save_does_not_cancel_an_in_flight_folder_load() {
        // The headline bug. `pending` was one slot, so ⌘O on a large project followed by
        // ⌘S dropped the folder-load task: the walk finished on the background pool and
        // its result was thrown away, so the sidebar stayed on "NO FOLDER OPEN" forever
        // with no error shown anywhere.
        let mut h = Harness::new();
        h.start(Job::OpenFolder);
        h.start(Job::Save);

        assert!(h.cancelled().is_empty(), "unrelated work must survive: {:?}", h.cancelled());
    }

    #[test]
    fn starting_a_folder_load_does_not_cancel_an_in_flight_save() {
        // The same bug in the direction that loses data. A save in flight that gets
        // dropped either never runs its write at all, or completes the write while
        // `mark_saved` never runs — leaving a saved file the editor still reports as dirty,
        // and a close prompt for changes that are already on disk.
        let mut h = Harness::new();
        h.start(Job::Save);
        h.start(Job::OpenFolder);

        assert!(h.cancelled().is_empty(), "the save must survive: {:?}", h.cancelled());
    }

    #[test]
    fn opening_a_file_does_not_cancel_a_quick_open_walk() {
        // ⌘P starts a walk; clicking a file in the tree used to drop that task while
        // leaving its CancelFlag unset, so the blocking walk ground on with nothing left
        // to consume it.
        let mut h = Harness::new();
        h.start(Job::QuickOpenIndex);
        h.start(Job::OpenFile);

        assert!(h.cancelled().is_empty());
    }

    #[test]
    fn a_close_prompt_survives_every_other_operation() {
        // A dialog awaiting an answer is the worst thing to drop: the await is abandoned,
        // the answer is discarded, and "Discard Changes" silently does nothing.
        let mut h = Harness::new();
        h.start(Job::ClosePrompt);
        h.start(Job::Save);
        h.start(Job::OpenFolder);
        h.start(Job::OpenFile);
        h.start(Job::QuickOpenIndex);

        assert!(h.cancelled().is_empty(), "{:?}", h.cancelled());
    }

    #[test]
    fn a_second_request_of_the_same_kind_cancels_the_first() {
        // The behaviour that must be *kept*: superseding work of the same kind is exactly
        // what ADR-0007 asks for. Two folder loads in a row means the second one wins.
        let mut h = Harness::new();
        h.start(Job::OpenFolder);
        h.start(Job::OpenFolder);

        assert_eq!(h.cancelled(), vec![Job::OpenFolder]);
    }

    #[test]
    fn cancelling_a_job_drops_only_that_job() {
        let mut h = Harness::new();
        h.start(Job::QuickOpenIndex);
        h.start(Job::Save);

        h.slots.cancel(Job::QuickOpenIndex);

        assert_eq!(h.cancelled(), vec![Job::QuickOpenIndex]);
    }

    #[test]
    fn cancelling_a_job_that_is_not_running_is_a_no_op() {
        let mut h = Harness::new();
        h.slots.cancel(Job::QuickOpenIndex);
        assert!(h.cancelled().is_empty());
    }

    #[test]
    fn closing_a_tab_left_of_the_active_one_keeps_the_same_file_active() {
        // Three tabs with the third active, closing the first: the clamp happens to agree
        // here (both give 1), because the active tab was last and clamping to the new end
        // lands on the right file by luck. Kept as a regression guard, not as evidence.
        assert_eq!(active_after_close(2, 0, 2), 1);
        // This is the case that actually proves the bug — the only one where the clamp
        // cannot hide it. Five tabs, fourth active, close the second: the three later tabs
        // shift down, so the active file is now at 2. Clamping alone returns 3, which is a
        // different file. The user closed one file and silently got shown another.
        assert_eq!(active_after_close(3, 1, 4), 2);
    }

    #[test]
    fn closing_a_tab_right_of_the_active_one_leaves_it_alone() {
        // Nothing before the active tab moved, so neither should the selection.
        assert_eq!(active_after_close(0, 1, 2), 0);
        assert_eq!(active_after_close(1, 3, 3), 1);
    }

    #[test]
    fn closing_the_active_tab_selects_the_one_that_took_its_place() {
        // ⌘W's case, and the only one the old clamp got right: the neighbour that slid into
        // the slot becomes active.
        assert_eq!(active_after_close(1, 1, 3), 1);
        // Closing the last tab has no successor, so it falls back to the new last one.
        assert_eq!(active_after_close(2, 2, 2), 1);
        // Closing the only tab leaves nothing; index 0 is what an empty tab bar renders as.
        assert_eq!(active_after_close(0, 0, 0), 0);
    }

    #[test]
    fn reordering_tabs_keeps_the_same_file_active() {
        // Dragging the active tab: the selection follows it to where it lands.
        assert_eq!(active_after_reorder(1, 1, 3), 3);
        assert_eq!(active_after_reorder(3, 3, 0), 0);
        // Dragging another tab across the active one shifts it by one slot.
        assert_eq!(active_after_reorder(2, 0, 3), 1, "left-of-active dragged past it");
        assert_eq!(active_after_reorder(1, 3, 0), 2, "right-of-active dragged before it");
        // A drag that never crosses the active tab leaves it alone.
        assert_eq!(active_after_reorder(0, 1, 2), 0);
        assert_eq!(active_after_reorder(3, 1, 2), 3);
    }

    #[test]
    fn every_job_gets_its_own_slot() {
        // Guards against a slot count regression: if two distinct jobs ever collided the
        // whole point of keying by job is gone.
        let mut h = Harness::new();
        let all = [
            Job::OpenFolder,
            Job::OpenFile,
            Job::Save,
            Job::QuickOpenIndex,
            Job::ClosePrompt,
            Job::Lsp,
            Job::TestRun,
        ];
        for job in all {
            h.start(job);
        }
        assert_eq!(h.slots.slots.len(), all.len());
        assert!(h.cancelled().is_empty());
    }

    #[test]
    fn starting_a_language_server_cancels_only_the_previous_one() {
        // Opening a second folder must stop the first project's poll loop — it holds a
        // client for a server pointed at a directory nobody is looking at any more. It
        // must not touch a save or a walk in flight.
        let mut h = Harness::new();
        h.start(Job::Save);
        h.start(Job::Lsp);
        h.start(Job::QuickOpenIndex);
        assert!(h.cancelled().is_empty());

        h.start(Job::Lsp);
        assert_eq!(h.cancelled(), vec![Job::Lsp]);
    }

    /// A test run is the longest-lived job here — minutes, against milliseconds for
    /// everything else — so it is the one with the most chances to be cancelled by
    /// something unrelated. Saving, opening a file, walking for quick open and starting a
    /// language server all happen constantly *while* a suite runs, and none of them is a
    /// request to abandon it (#25, ADR-0007).
    #[test]
    fn nothing_the_user_does_while_a_suite_runs_cancels_it() {
        let mut h = Harness::new();
        h.start(Job::TestRun);

        for job in [Job::Save, Job::OpenFile, Job::QuickOpenIndex, Job::Lsp, Job::OpenFolder] {
            h.start(job);
        }
        assert!(
            !h.cancelled().contains(&Job::TestRun),
            "a test run was cancelled by unrelated work: {:?}",
            h.cancelled()
        );

        // But a second run does supersede the first: two suites at once would fight over
        // the same database and interleave their results.
        h.start(Job::TestRun);
        assert_eq!(h.cancelled(), vec![Job::TestRun]);
    }
}

#[cfg(test)]
mod lsp_status_tests {
    use super::*;
    use crate::lsp_session::Lsp;

    /// §24 at the last step before pixels, and the single most important assertion in this
    /// file: **an editor with no language server must look exactly like an editor from
    /// before this feature existed.** No icon, no "LSP: off", nothing to dismiss.
    ///
    /// This is the path almost every user is on — nobody has Intelephense on a fresh
    /// machine, and most folders anyone opens are not PHP projects. If it ever starts
    /// saying something, it says it to everybody, forever.
    #[test]
    fn a_missing_language_server_says_nothing_at_all() {
        let mut lsp = Lsp::new();

        // Never attempted: no folder open.
        assert_eq!(lsp_label(&lsp, false), "");

        // Attempted and the binary was not there.
        lsp.set_state(LspState::Unavailable);
        assert_eq!(lsp_label(&lsp, false), "", "a missing server is not the user's problem");
    }

    #[test]
    fn a_missing_server_is_named_while_a_php_file_is_open() {
        // #125's third cause, and the narrow exception to the test above. §24's silence is
        // right for someone who never wanted a PHP server; it is wrong for someone looking
        // at a PHP file and waiting for a completion popup, which is exactly the state the
        // report came from. Nothing on screen separated "no server" from "server running
        // and returning nothing", so the bug was filed against the popup.
        let mut lsp = Lsp::new();
        lsp.set_state(LspState::Unavailable);

        assert_eq!(lsp_label(&lsp, true), "No language server");
    }

    #[test]
    fn the_exception_does_not_leak_into_the_states_that_are_fine() {
        // The risk in adding any message here is that it becomes permanent chrome. These
        // are the states where a PHP file is open and everything is working: they must stay
        // exactly as silent with the flag set as without it.
        let mut lsp = Lsp::new();

        assert_eq!(lsp_label(&lsp, true), "", "no attempt yet is not a failure");

        lsp.set_state(LspState::Running);
        assert_eq!(lsp_label(&lsp, true), "", "a clean file says nothing");
    }

    #[test]
    fn a_running_server_with_nothing_to_report_also_says_nothing() {
        // A clean file is the normal state. "0 problems" is chrome.
        let mut lsp = Lsp::new();
        lsp.set_state(LspState::Running);
        assert_eq!(lsp_label(&lsp, false), "");
    }

    #[test]
    fn a_slow_start_is_visible_because_otherwise_nothing_is_happening() {
        // Intelephense indexing a large vendor/ tree takes tens of seconds, during which
        // an empty status bar is indistinguishable from a broken one.
        let mut lsp = Lsp::new();
        lsp.set_state(LspState::Starting);
        assert_eq!(lsp_label(&lsp, false), "Starting…");
    }

    #[test]
    fn a_server_that_kept_dying_is_reported_once_the_budget_is_spent() {
        // The one failure worth saying out loud: not "you never installed one", but "the
        // one you have will not stay up", which the user cannot learn anywhere else.
        let mut lsp = Lsp::new();
        lsp.set_state(LspState::Failed("it exited".into()));
        assert_eq!(lsp_label(&lsp, false), "LSP unavailable");
    }

    #[test]
    fn problem_counts_read_as_counts() {
        use elle_lsp::lsp_types::{Diagnostic, DiagnosticSeverity, Position, Range};

        fn at(line: u32, severity: DiagnosticSeverity) -> Diagnostic {
            Diagnostic {
                range: Range {
                    start: Position { line, character: 0 },
                    end: Position { line, character: 1 },
                },
                severity: Some(severity),
                message: "x".into(),
                ..Default::default()
            }
        }

        let mut lsp = Lsp::new();
        lsp.set_state(LspState::Running);
        let uri: elle_lsp::lsp_types::Uri = "file:///a.php".parse().unwrap();
        let text = "a\nb\nc\n";

        lsp.set_diagnostics(uri.clone(), &[at(0, DiagnosticSeverity::ERROR)], text);
        assert_eq!(lsp_label(&lsp, false), "1 ✕");

        lsp.set_diagnostics(
            uri.clone(),
            &[at(0, DiagnosticSeverity::ERROR), at(1, DiagnosticSeverity::WARNING)],
            text,
        );
        assert_eq!(lsp_label(&lsp, false), "1 ✕  1 ⚠");

        lsp.set_diagnostics(uri.clone(), &[at(0, DiagnosticSeverity::WARNING)], text);
        assert_eq!(lsp_label(&lsp, false), "1 ⚠");

        // And back to silence when the file is fixed.
        lsp.set_diagnostics(uri, &[], text);
        assert_eq!(lsp_label(&lsp, false), "");
    }

    // --- the root a file opened without a folder gets (#125) ----------------------

    #[test]
    fn a_file_in_a_project_roots_the_server_at_the_project() {
        // The case the obvious implementation gets wrong. Rooting the server at the file's
        // own directory would index `app/Models` alone: the framework would not resolve,
        // and the server would start and still answer nothing — which is harder to
        // diagnose than no server, because something visibly happened.
        let project = tempfile::tempdir().unwrap();
        std::fs::write(project.path().join("composer.json"), "{}").unwrap();
        let models = project.path().join("app/Models");
        std::fs::create_dir_all(&models).unwrap();

        assert_eq!(project_root_for(&models.join("User.php")), Some(project.path().to_path_buf()));
    }

    #[test]
    fn the_nearest_marker_wins_over_the_outer_one() {
        // A package inside a repository is its own project. Walking to the outermost marker
        // would hand the server the monorepo and index everything in it.
        let outer = tempfile::tempdir().unwrap();
        std::fs::create_dir(outer.path().join(".git")).unwrap();
        let inner = outer.path().join("packages/thing");
        std::fs::create_dir_all(inner.join("src")).unwrap();
        std::fs::write(inner.join("composer.json"), "{}").unwrap();

        assert_eq!(project_root_for(&inner.join("src/Thing.php")), Some(inner));
    }

    #[test]
    fn a_repository_with_no_manifest_still_roots_at_the_repository() {
        let repo = tempfile::tempdir().unwrap();
        std::fs::create_dir(repo.path().join(".git")).unwrap();
        let scripts = repo.path().join("scripts");
        std::fs::create_dir(&scripts).unwrap();

        assert_eq!(project_root_for(&scripts.join("import.php")), Some(repo.path().to_path_buf()));
    }

    #[test]
    fn a_loose_file_falls_back_to_its_own_directory() {
        // Nothing to walk up to. A server rooted here is worth more than no server, which
        // is what #125 reported: the editor started nothing and said nothing.
        let dir = tempfile::tempdir().unwrap();

        assert_eq!(
            project_root_for(&dir.path().join("scratch.php")),
            Some(dir.path().to_path_buf())
        );
    }

    #[test]
    fn a_path_with_no_parent_gets_no_root() {
        assert_eq!(project_root_for(std::path::Path::new("/")), None);
    }

    // --- which tabs a delete closes (#126) ----------------------------------------

    #[test]
    fn a_file_inside_a_deleted_directory_is_under_it() {
        let dir = tempfile::tempdir().unwrap();
        let app = dir.path().join("app");
        std::fs::create_dir(&app).unwrap();

        assert!(is_under(&app.join("User.php"), &app));
        assert!(is_under(&app, &app), "the deleted path itself counts");
        assert!(!is_under(&dir.path().join("artisan"), &app), "a sibling must survive");
    }

    #[test]
    fn two_spellings_of_the_same_path_still_match() {
        // The case that let a tab survive its file being deleted. `FileTree` canonicalises
        // its root, a tab's path is whatever opened it, and on macOS the temp directory is
        // a symlink — so the same file is `/private/var/…` from one and `/var/…` from the
        // other, and a plain `starts_with` says they are unrelated.
        let dir = tempfile::tempdir().unwrap();
        let canonical = dir.path().canonicalize().unwrap();

        // Only meaningful where the two actually differ, which is macOS; elsewhere this
        // degenerates to the plain comparison and still holds.
        let raw_file = dir.path().join("User.php");
        std::fs::write(&raw_file, "<?php\n").unwrap();

        assert!(
            is_under(&raw_file, &canonical),
            "a tab opened by one spelling must be closed by a delete named in the other"
        );
    }

    #[test]
    fn a_deleted_directory_still_matches_its_open_files() {
        // `delete` has already run by the time this is asked, so neither the directory nor
        // the file inside it can be canonicalised. Resolving the deepest ancestor that does
        // still exist is what keeps the comparison working.
        let dir = tempfile::tempdir().unwrap();
        let app = dir.path().join("app");
        std::fs::create_dir(&app).unwrap();
        let file = app.join("User.php");
        std::fs::write(&file, "<?php\n").unwrap();

        let canonical_app = app.canonicalize().unwrap();
        std::fs::remove_dir_all(&app).unwrap();

        assert!(
            is_under(&file, &canonical_app),
            "a tab must still be recognised as inside a directory that is already gone"
        );
    }
}

#[cfg(test)]
mod route_label_tests {
    use super::*;

    /// The row must never present an unreadable value as if it were read. This is RISKS.md
    /// #4 at the last step before pixels: everything upstream can be honest and a label that
    /// prints `Unknown` as an empty string throws it all away.
    #[test]
    fn an_unresolved_uri_shows_the_expression_that_defeated_us() {
        let route = Route {
            method: elle_laravel::HttpMethod::Get,
            uri: Resolved::Unknown("$legacyUri".into()),
            name: Some(Resolved::Unknown("$legacyName".into())),
            action: Resolved::Unknown("[$c, 'h']".into()),
            middleware: vec![],
            line: 36,
        };
        let label = route_label(&route);
        assert!(label.contains("<$legacyUri>"), "got {label:?}");
        assert!(label.contains("<$legacyName>"), "got {label:?}");
        assert!(!label.contains("  <$legacyUri>  \n"), "no empty placeholder");
    }

    /// `None` (never called ->name()) and `Some(Unknown)` (called it with an expression) are
    /// different facts. Flattening them into one blank column loses the distinction the
    /// extractor went to trouble to preserve.
    #[test]
    fn a_route_with_no_name_gets_no_name_column() {
        let route = Route {
            method: elle_laravel::HttpMethod::Post,
            uri: Resolved::Known("/users".into()),
            name: None,
            action: Resolved::Known(elle_laravel::RouteAction::Closure),
            middleware: vec![],
            line: 7,
        };
        let label = route_label(&route);
        assert!(label.contains("/users"));
        assert!(!label.contains('<'), "nothing unresolved here: {label:?}");
        assert_eq!(label.trim_end(), label, "no trailing pad for an absent name");
    }
}

/// The app-side half of Laravel navigation (#83): which files it looks at, and how a click
/// that Laravel declines still reaches the language server.
///
/// Resolution itself is `elle-laravel`'s and tested there against a real project tree. What
/// can only be wrong *here* is the gate and the fall-through.
#[cfg(test)]
mod laravel_navigation_tests {
    use super::*;

    #[test]
    fn only_php_and_blade_are_read_for_laravel_references() {
        assert_eq!(laravel_dialect(std::path::Path::new("routes/web.php")), Some(false));
        assert_eq!(
            laravel_dialect(std::path::Path::new("resources/views/x.blade.php")),
            Some(true),
            ".blade.php must be read as Blade, not PHP — the two use different readers"
        );

        // Everything else is not a Laravel source file, and ⌘click there must go straight
        // to the language server as it always did.
        for path in ["app.js", "composer.json", "README.md", "style.css", "Makefile"] {
            assert_eq!(laravel_dialect(std::path::Path::new(path)), None, "{path}");
        }
    }

    /// The fall-through contract, at the level it can actually be checked: a click that is
    /// not on a readable Laravel literal produces no reference, which is what makes
    /// `go_to_laravel_target` return `false` and the LSP request go out.
    ///
    /// This is the property that keeps #88's go-to-definition working. Laravel claiming a
    /// click it cannot complete would break navigation for every ordinary PHP symbol.
    #[test]
    fn a_click_that_is_not_a_laravel_reference_leaves_the_click_to_the_language_server() {
        let source = "<?php\nclass User extends Model {}\n$u = new User();\nroute($dynamic);\n";

        for needle in ["User", "Model", "new", "$dynamic"] {
            let offset = source.find(needle).expect(needle);
            assert_eq!(
                elle_laravel::reference_at(source, offset, false),
                None,
                "{needle:?} is the language server's to answer, not ours"
            );
        }
    }

    /// A Blade template is the case that did not work at all before this: the language
    /// server is never told about `.blade.php`, so `navigation_origin` returns `None` and
    /// F12 was silent there. The Laravel reader does not go through that gate.
    #[test]
    fn a_blade_template_is_navigable_even_though_the_language_server_ignores_it() {
        let path = std::path::Path::new("resources/views/welcome.blade.php");
        assert!(!crate::lsp_session::handles(path), "the premise: no server sees this file");
        assert_eq!(laravel_dialect(path), Some(true), "but Laravel navigation still reads it");

        let source = "@include('partials.header')\n";
        let offset = source.find("partials.header").unwrap();
        let reference = elle_laravel::reference_at(source, offset, true).expect("an include");
        assert_eq!(reference.name, "partials.header");
    }

    /// The completion writes over the literal, so the range it carries has to be the
    /// literal's own bytes — an off-by-one here rewrites the quote characters and produces
    /// `route(users.showx')`.
    #[test]
    fn the_completion_range_covers_the_literal_and_not_its_quotes() {
        let source = "<?php\n$url = route('users.show');\n";
        let offset = source.find("users.show").unwrap();
        let reference = elle_laravel::reference_at(source, offset, false).expect("a route");

        assert_eq!(&source[reference.range.clone()], "users.show");
        assert_eq!(source.as_bytes()[reference.range.start - 1], b'\'');
        assert_eq!(source.as_bytes()[reference.range.end], b'\'');
    }

    /// An empty `route('')` is the normal state when someone has just typed the call and
    /// wants the list. It must read as a route reference with an empty name rather than as
    /// no reference at all, or completion would do nothing exactly when it is most wanted.
    #[test]
    fn an_empty_route_literal_still_opens_the_completion() {
        let source = "<?php\n$url = route('');\n";
        let offset = source.find("''").unwrap() + 1;
        let reference = elle_laravel::reference_at(source, offset, false).expect("a route");

        assert_eq!(reference.kind, elle_laravel::ReferenceKind::Route);
        assert_eq!(reference.name, "");
        assert!(reference.range.is_empty(), "nothing to overwrite, so the range is empty");
    }
}
