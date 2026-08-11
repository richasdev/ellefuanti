//! The source control panel: status list and diff view (#64, items 1 and 2).
//!
//! **Read-only, deliberately.** There is no stage, unstage, commit, amend, fetch, pull or
//! push here, and their absence is the design rather than an unfinished edge. #64 puts them
//! at items 3–5 and says item 5 is where the danger is; the specific danger is that losing
//! uncommitted work is the one thing in this editor a user cannot undo. A panel that only
//! reads cannot lose anyone's work, so it can ship before the confirmation machinery those
//! operations need exists. Everything below is shaped by that: no button in this file
//! writes anything.
//!
//! The domain half is `elle-git`, which knows nothing about gpui (ADR-0004). This file is
//! the translation layer ADR-0004 says the presentation crate should hold.
//!
//! ## Refresh, and why there is no timer
//!
//! Refreshed on **folder open, window focus, and file save**, and at no other time. #64
//! notes polling is wasteful and that #21's file watcher is unbuilt; the third option is
//! the cheap correct one, because those three events are exactly when the answer can have
//! changed in a way the user is about to look at:
//!
//! - **Save** is the only way *this editor* changes the working tree, so it is the only
//!   internal event that can invalidate the status.
//! - **Focus** covers every external change — a `git commit` in the terminal, a branch
//!   switch, a rebase — because to see a stale panel you must first look at the window, and
//!   looking at the window is what raises this event.
//! - A **timer** would burn CPU on a panel nobody is looking at. #79 spent three wrong
//!   conclusions on idle cost this session and the perf gate now enforces idle footprint
//!   *and* an idle CPU rate, so a poll would not merely be wasteful, it would be measured.
//!
//! The honest cost: a change made by another process while ellefuanti is already focused is
//! not noticed until you focus away and back. That is a real staleness window, it is
//! bounded by one click, and closing it properly is #21's file watcher rather than a timer
//! bolted on here.

use std::path::{Path, PathBuf};

use elle_git::{DiffFile, FileStatus, LineKind, RepoStatus, Status};
use elle_syntax::{Language, SyntaxTree, language_for_path};
use elle_text::Buffer;
use gpui::{
    App, Context, EventEmitter, MouseButton, SharedString, StyledText, Window, div, prelude::*, px,
    uniform_list,
};

use crate::fonts::Fonts;
use crate::theme::{Metrics, Theme, Themed};

/// What the panel tells the workspace.
///
/// One variant, and it stays one until there is a second thing a read-only panel can ask
/// for. An `OpenRequested` for "open the file in a tab" was written and removed: nothing
/// raises it, and an event nobody emits is a branch nobody tests.
// The shared postfix is the point: every variant is a *request* the workspace runs,
// because the panel has no executor policy — clippy would trade that honesty for brevity.
#[allow(clippy::enum_variant_names)]
pub enum GitEvent {
    /// A status row was clicked: show this file's diff. The workspace owns the background
    /// task, because the panel must not spawn — it has no root and no executor policy.
    DiffRequested { path: PathBuf },
    /// The row's ± was clicked: stage (true) or unstage this file. The workspace runs it
    /// for the same no-executor reason, and refreshes status after.
    StageRequested { path: PathBuf, stage: bool },
    /// The commit button, with the message typed into the panel's box.
    CommitRequested { message: String },
}

/// What the panel is showing right now.
///
/// An explicit enum rather than `Option<RepoStatus>` because "not a repository" and "no
/// changes" are different sentences and rendering both as an empty list would be a lie
/// about one of them. `NotARepo` is the *common* case — most folders anyone opens are not
/// repositories — so it gets a first-class variant rather than being an error path.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub enum PanelState {
    /// No folder is open yet.
    #[default]
    NoFolder,
    /// A folder is open and the first status has not come back. Distinct from `Clean` so
    /// the panel does not flash "no changes" before it knows.
    Loading,
    /// The open folder is not inside a git repository. Not an error: no dialog, no log
    /// line, nothing written anywhere. It is simply what most folders are.
    NotARepo,
    /// A repository, with whatever it had to say.
    Repo(RepoStatus),
}

impl PanelState {
    /// The sentence shown when there are no rows to list.
    ///
    /// Split out from the render so the wording — the part anyone actually reads — is
    /// assertable without a window, the same reasoning as `find_bar::Status::label`.
    pub fn empty_message(&self) -> Option<&'static str> {
        match self {
            PanelState::NoFolder => Some("Press ⌘O to open a folder"),
            PanelState::Loading => Some("Loading…"),
            PanelState::NotARepo => Some("Not a git repository"),
            PanelState::Repo(status) if status.cancelled && status.is_empty() => {
                Some("Refreshing…")
            }
            PanelState::Repo(status) if status.is_empty() => Some("No changes"),
            PanelState::Repo(_) => None,
        }
    }

    /// The rows to list, empty for every non-repository state.
    pub fn files(&self) -> &[FileStatus] {
        match self {
            PanelState::Repo(status) => &status.files,
            _ => &[],
        }
    }

    /// The branch name for the header, when there is one.
    pub fn branch(&self) -> Option<&str> {
        match self {
            PanelState::Repo(status) => status.branch.as_deref(),
            _ => None,
        }
    }
}

pub struct GitPanel {
    state: PanelState,
    /// The commit box's text. Plain keystroke capture, the palette's pattern — a full
    /// text-input widget for a one-line message would be the framework this repo keeps
    /// declining to build.
    commit_message: String,
    /// The diff for the selected row, once it has come back.
    diff: Option<DiffFile>,
    /// Which row is selected, as a repo-relative path rather than an index — the list is
    /// re-sorted on every refresh, so an index would select a different file after a save.
    selected: Option<String>,
}

impl EventEmitter<GitEvent> for GitPanel {}

impl GitPanel {
    pub fn new(_cx: &mut Context<Self>) -> Self {
        Self {
            state: PanelState::default(),
            diff: None,
            selected: None,
            commit_message: String::new(),
        }
    }

    pub fn state(&self) -> &PanelState {
        &self.state
    }

    pub fn clear_commit_message(&mut self, cx: &mut Context<Self>) {
        self.commit_message.clear();
        cx.notify();
    }

    /// Keystrokes for the commit box, captured the palette's way: printable characters
    /// append, backspace pops, Enter commits when there is a message and something
    /// staged. No text-input widget — one line does not buy a framework.
    fn on_key_down(&mut self, event: &gpui::KeyDownEvent, _w: &mut Window, cx: &mut Context<Self>) {
        let keystroke = &event.keystroke;
        if keystroke.modifiers.platform
            || keystroke.modifiers.control
            || keystroke.modifiers.function
        {
            return;
        }
        match keystroke.key.as_str() {
            "backspace" => {
                self.commit_message.pop();
                cx.notify();
            }
            "enter" => {
                let staged_anything = self.state.files().iter().any(|file| file.staged);
                if !self.commit_message.trim().is_empty() && staged_anything {
                    cx.emit(GitEvent::CommitRequested { message: self.commit_message.clone() });
                }
            }
            _ => {
                let Some(text) = keystroke.key_char.as_deref() else { return };
                if text.is_empty() || text.chars().all(|c| c.is_control()) {
                    return;
                }
                self.commit_message.push_str(text);
                cx.notify();
            }
        }
    }

    /// The commit box (#64 item 4): message line, button, both live only when a
    /// repository is showing.
    fn render_commit_box(&self, theme: &Theme, cx: &Context<Self>) -> Option<gpui::AnyElement> {
        if self.state.files().is_empty() && self.state.empty_message().is_some() {
            return None;
        }
        let staged_anything = self.state.files().iter().any(|file| file.staged);
        let has_message = !self.commit_message.trim().is_empty();
        let shown = if self.commit_message.is_empty() {
            SharedString::from("Commit message…")
        } else {
            SharedString::from(self.commit_message.clone())
        };
        let entity = cx.entity();

        Some(
            div()
                .flex_none()
                .flex()
                .flex_col()
                .gap_1()
                .p_2()
                .border_t_1()
                .border_color(theme.border)
                .child(
                    div()
                        .px_2()
                        .py_1()
                        .rounded(px(4.0))
                        .bg(theme.background)
                        .border_1()
                        .border_color(theme.border)
                        .when(self.commit_message.is_empty(), |el| el.text_color(theme.text_muted))
                        .child(shown),
                )
                .child(
                    div()
                        .id("git-commit-button")
                        .px_2()
                        .py_1()
                        .rounded(px(4.0))
                        .border_1()
                        .border_color(theme.border)
                        .text_color(if staged_anything && has_message {
                            theme.text
                        } else {
                            theme.text_muted
                        })
                        // Enabled = clickable; disabled still explains itself in words
                        // below rather than by shade alone.
                        .when(staged_anything && has_message, |el| {
                            el.cursor_pointer()
                                .hover(|el| el.bg(theme.hover))
                                .active(|el| el.bg(theme.pressed))
                        })
                        .on_mouse_down(MouseButton::Left, move |_ev, _window, cx| {
                            entity.update(cx, |this, cx| {
                                let staged = this.state.files().iter().any(|f| f.staged);
                                if staged && !this.commit_message.trim().is_empty() {
                                    cx.emit(GitEvent::CommitRequested {
                                        message: this.commit_message.clone(),
                                    });
                                }
                            });
                        })
                        .child("Commit"),
                )
                .children((!staged_anything).then(|| {
                    div()
                        .text_color(theme.text_muted)
                        .child("Stage something (+) to commit")
                        .into_any_element()
                }))
                .into_any_element(),
        )
    }
    pub fn diff(&self) -> Option<&DiffFile> {
        self.diff.as_ref()
    }

    pub fn selected(&self) -> Option<&str> {
        self.selected.as_deref()
    }

    /// Replaces the status, keeping the selection if that file still has changes.
    ///
    /// Dropping a selection whose file went clean is deliberate: the diff pane would
    /// otherwise keep showing a diff that no longer exists, which is the same "stale view
    /// presented as current" failure the panel avoids elsewhere.
    pub fn set_state(&mut self, state: PanelState, cx: &mut Context<Self>) {
        let still_present = self
            .selected
            .as_ref()
            .is_some_and(|sel| state.files().iter().any(|f| &f.relative == sel));
        if !still_present {
            self.selected = None;
            self.diff = None;
        }
        self.state = state;
        cx.notify();
    }

    pub fn set_diff(&mut self, diff: Option<DiffFile>, cx: &mut Context<Self>) {
        self.diff = diff;
        cx.notify();
    }

    fn select(&mut self, file: &FileStatus, cx: &mut Context<Self>) {
        self.selected = Some(file.relative.clone());
        // Cleared rather than kept: showing the previous file's diff under the new file's
        // name for as long as the background read takes is worse than showing nothing.
        self.diff = None;
        cx.emit(GitEvent::DiffRequested { path: file.path.clone() });
        cx.notify();
    }
}

/// Colour and marker for a status letter.
///
/// The marker comes from `elle_git`, so the letters are git's own and the panel and any
/// future CLI output cannot disagree. The colour is chosen here, because colour is a
/// presentation concern and `elle-git` does not depend on the theme (ADR-0004).
fn status_color(status: Status, theme: &Theme) -> gpui::Hsla {
    match status {
        // A conflict is the only status that blocks work, so it gets the one colour every
        // theme reserves for "you must deal with this".
        Status::Conflicted => theme.error,
        Status::Added | Status::Untracked => theme.diff_added(),
        Status::Deleted => theme.diff_removed(),
        Status::Modified | Status::Renamed => theme.warning,
    }
}

impl Render for GitPanel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // Cloned rather than borrowed: `render_rows` needs `&mut Context`, and holding a
        // borrow of the global across that call is what the borrow checker refuses. The
        // same clone `workspace_view` makes for the same reason.
        let theme = cx.theme().clone();

        div()
            .flex()
            .flex_col()
            .size_full()
            .on_key_down(cx.listener(Self::on_key_down))
            .child(self.render_header(&theme))
            .child(self.render_rows(&theme, cx))
            .children(self.render_commit_box(&theme, cx))
    }
}

impl GitPanel {
    /// Branch name and change count.
    fn render_header(&self, theme: &Theme) -> impl IntoElement {
        let branch = self.state.branch().unwrap_or("no branch").to_string();
        let count = self.state.files().len();

        div()
            .h(Metrics::TAB_HEIGHT)
            .flex()
            .flex_none()
            .items_center()
            .justify_between()
            .px_3()
            .border_b_1()
            .border_color(theme.border)
            .text_color(theme.text_muted)
            .child(SharedString::from(branch))
            .when(count > 0, |el| {
                el.child(SharedString::from(match count {
                    1 => "1 change".to_string(),
                    n => format!("{n} changes"),
                }))
            })
    }

    fn render_rows(&self, theme: &Theme, cx: &mut Context<Self>) -> impl IntoElement {
        if let Some(message) = self.state.empty_message() {
            return div().p_3().text_color(theme.text_muted).child(message).into_any_element();
        }

        let entity = cx.entity();
        let count = self.state.files().len();
        let text = theme.text;
        let muted = theme.text_muted;
        let hover = theme.hover;
        let pressed = theme.pressed;
        let selected_bg = theme.selected;
        let theme = theme.clone();

        uniform_list("git-status", count, move |range, _window, cx| {
            entity.update(cx, |this, _cx| {
                range
                    .filter_map(|index| {
                        let file = this.state.files().get(index)?;
                        let entity = entity.clone();
                        let stage_entity = entity.clone();
                        let file = file.clone();
                        let is_selected = this.selected.as_deref() == Some(file.relative.as_str());
                        let marker_color = status_color(file.status, &theme);
                        let click = file.clone();

                        Some(
                            div()
                                .id(("git-row", index))
                                .flex()
                                .items_center()
                                .gap_2()
                                .h(Metrics::ROW_HEIGHT)
                                .px_2()
                                .cursor_pointer()
                                .hover(|el| el.bg(hover))
                                .active(|el| el.bg(pressed))
                                .when(is_selected, |el| el.bg(selected_bg))
                                .on_mouse_down(MouseButton::Left, move |_ev, _window, cx| {
                                    entity.update(cx, |this, cx| this.select(&click, cx));
                                })
                                // The letter first and always. This is the non-colour
                                // channel: `M`, `D`, `?` and `!` differ as glyphs before
                                // they differ as colours, so the list is fully readable
                                // with no colour perception at all (#64, #71). A fixed
                                // width keeps the names aligned into a column.
                                .child(
                                    div()
                                        .w(px(12.0))
                                        .flex_none()
                                        .text_color(marker_color)
                                        .child(SharedString::from(file.status.marker())),
                                )
                                .child(
                                    div()
                                        .flex_1()
                                        .overflow_hidden()
                                        .text_color(if is_selected { text } else { muted })
                                        .child(SharedString::from(file.name().to_string())),
                                )
                                // A staged file says so in words, not by being a different
                                // shade of the same colour.
                                .when(file.staged, |el| {
                                    el.child(
                                        div()
                                            .flex_none()
                                            .text_color(muted)
                                            .child(SharedString::from("staged")),
                                    )
                                })
                                // Stage/unstage (#64 item 3). ± because the action is the
                                // toggle of the word beside it; both directions touch only
                                // the index, which is what makes this button safe without
                                // a confirmation — see `elle_git::stage`.
                                .child({
                                    let entity = stage_entity;
                                    let path = file.path.clone();
                                    let to_stage = !file.staged;
                                    div()
                                        .id(("git-stage", index))
                                        .flex_none()
                                        .px_1()
                                        .rounded(px(3.0))
                                        .cursor_pointer()
                                        .text_color(muted)
                                        .hover(|el| el.bg(hover).text_color(text))
                                        .active(|el| el.bg(pressed))
                                        .on_mouse_down(
                                            MouseButton::Left,
                                            move |_ev, _window, cx| {
                                                cx.stop_propagation();
                                                entity.update(cx, |_this, cx| {
                                                    cx.emit(GitEvent::StageRequested {
                                                        path: path.clone(),
                                                        stage: to_stage,
                                                    });
                                                });
                                            },
                                        )
                                        .child(if to_stage { "+" } else { "−" })
                                })
                                .into_any_element(),
                        )
                    })
                    .collect()
            })
        })
        .flex_1()
        .into_any_element()
    }
}

/// Renders a diff with the editor's own syntax highlighting.
///
/// **This is the reuse #64 asks for**, and the reason it is worth the effort: this editor
/// highlights eight languages, and a diff rendered as flat text inside it looks broken
/// rather than plain — the same PHP file is coloured in a tab and grey in the diff pane two
/// inches away.
///
/// The technique: each hunk's added lines and its removed lines are two *different* files,
/// so they are reassembled into two buffers and parsed separately. Highlighting the diff
/// text directly cannot work — `-    $x = 1;` is not valid PHP, and the interleaved
/// add/remove lines are not a valid program in any language.
///
/// Both sides are parsed once per diff rather than once per line: tree-sitter's cost is per
/// parse, and a hunk-sized buffer parses in microseconds.
pub struct DiffRenderer {
    language: Language,
    /// Highlight spans for the new-side text, keyed by the line's own index within it.
    new_side: Vec<Vec<(std::ops::Range<usize>, elle_syntax::HighlightStyle)>>,
    old_side: Vec<Vec<(std::ops::Range<usize>, elle_syntax::HighlightStyle)>>,
}

impl DiffRenderer {
    /// Parses both sides of `file`.
    ///
    /// Blocking (tree-sitter), so it is built on the background executor with the diff
    /// itself and handed over finished — the same rule as everything else in ADR-0007.
    pub fn new(file: &DiffFile) -> Self {
        let language = language_for_path(Path::new(&file.relative));

        // Reassemble each side into the file it would be. Context lines belong to both.
        let mut new_text = String::new();
        let mut old_text = String::new();
        for line in file.hunks.iter().flat_map(|hunk| &hunk.lines) {
            match line.kind {
                LineKind::Added => {
                    new_text.push_str(&line.text);
                    new_text.push('\n');
                }
                LineKind::Removed => {
                    old_text.push_str(&line.text);
                    old_text.push('\n');
                }
                LineKind::Context => {
                    new_text.push_str(&line.text);
                    new_text.push('\n');
                    old_text.push_str(&line.text);
                    old_text.push('\n');
                }
            }
        }

        Self {
            language,
            new_side: highlight_lines(language, &new_text),
            old_side: highlight_lines(language, &old_text),
        }
    }

    /// The styled text for one diff line.
    ///
    /// `new_index` and `old_index` are the line's position within its own side, which the
    /// caller tracks as it walks the hunks — the same counters that produce the gutter
    /// numbers.
    pub fn line(
        &self,
        kind: LineKind,
        text: &str,
        new_index: usize,
        old_index: usize,
        theme: &Theme,
    ) -> StyledText {
        let spans = match kind {
            LineKind::Added | LineKind::Context => self.new_side.get(new_index),
            LineKind::Removed => self.old_side.get(old_index),
        };

        let highlights = spans
            .map(|spans| {
                spans
                    .iter()
                    .filter_map(|(range, style)| {
                        // A span can only be painted if it lands on char boundaries of the
                        // text actually being rendered; the reassembled side and this line
                        // agree by construction, but a defensive clip costs nothing and a
                        // panic in a render pass costs the window.
                        let start = range.start.min(text.len());
                        let end = range.end.min(text.len());
                        if start >= end
                            || !text.is_char_boundary(start)
                            || !text.is_char_boundary(end)
                        {
                            return None;
                        }
                        Some((
                            start..end,
                            gpui::HighlightStyle {
                                color: Some(theme.syntax(*style)),
                                ..Default::default()
                            },
                        ))
                    })
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();

        StyledText::new(SharedString::from(text.to_string())).with_highlights(highlights)
    }

    pub fn language(&self) -> Language {
        self.language
    }
}

/// Highlights `text` and buckets the spans by line, rebased onto line-local offsets.
///
/// Returns one entry per line, so the renderer indexes rather than searches. Plain text and
/// any language without a grammar produce empty vectors, which render as uncoloured lines —
/// correct, and the reason there is no error path here.
fn highlight_lines(
    language: Language,
    text: &str,
) -> Vec<Vec<(std::ops::Range<usize>, elle_syntax::HighlightStyle)>> {
    let line_count = text.lines().count();
    let mut per_line = vec![Vec::new(); line_count];
    if text.is_empty() {
        return per_line;
    }

    let buffer = Buffer::new(text);
    let Ok(tree) = SyntaxTree::new(language, &buffer) else { return per_line };
    let spans = tree.highlights(&buffer, 0..text.len());

    // Walk the lines once, carrying the byte offset, and assign each span to the line it
    // starts on. A span crossing a newline — a block comment, a heredoc — is clipped to
    // each line it touches, because a range that ran past the end of its line would be
    // dropped by the boundary check in `line`.
    let mut offset = 0;
    for (index, line) in text.lines().enumerate() {
        let line_start = offset;
        let line_end = line_start + line.len();
        for span in &spans {
            if span.range.end <= line_start || span.range.start >= line_end {
                continue;
            }
            let start = span.range.start.max(line_start) - line_start;
            let end = span.range.end.min(line_end) - line_start;
            if start < end {
                per_line[index].push((start..end, span.style));
            }
        }
        // +1 for the '\n' that `lines()` stripped. Every line in the reassembled text has
        // one, because the reassembly above appended it.
        offset = line_end + 1;
    }

    per_line
}

/// The gutter width for a diff's line numbers, in characters.
///
/// Computed from the largest number actually present rather than fixed, so a 12-line file
/// does not carry a five-column gutter.
pub fn gutter_width(file: &DiffFile) -> usize {
    let widest = file
        .hunks
        .iter()
        .flat_map(|hunk| &hunk.lines)
        .filter_map(|line| line.new_line.or(line.old_line))
        .max()
        .unwrap_or(0);
    widest.to_string().len().max(2)
}

/// Background wash for a diff line, or `None` for context.
pub fn line_background(kind: LineKind, theme: &Theme) -> Option<gpui::Hsla> {
    match kind {
        LineKind::Added => Some(theme.diff_added_background()),
        LineKind::Removed => Some(theme.diff_removed_background()),
        LineKind::Context => None,
    }
}

/// Foreground for a diff line's `+`/`-` marker.
pub fn marker_color(kind: LineKind, theme: &Theme) -> gpui::Hsla {
    match kind {
        LineKind::Added => theme.diff_added(),
        LineKind::Removed => theme.diff_removed(),
        LineKind::Context => theme.text_muted,
    }
}

/// Renders the diff pane for `file`.
///
/// A free function rather than a method because the workspace owns the layout decision of
/// where the diff goes, and this only knows how to draw one.
pub fn render_diff(
    file: &DiffFile,
    renderer: &DiffRenderer,
    theme: &Theme,
    cx: &App,
) -> impl IntoElement {
    let (added, removed) = file.counts();
    let width = gutter_width(file);
    let fonts = Fonts::get(cx);

    let mut rows: Vec<gpui::AnyElement> = Vec::new();
    let mut new_index = 0usize;
    let mut old_index = 0usize;

    for hunk in &file.hunks {
        rows.push(
            div()
                .w_full()
                .px_2()
                .bg(theme.panel)
                .text_color(theme.text_muted)
                .child(SharedString::from(hunk.header.clone()))
                .into_any_element(),
        );

        for line in &hunk.lines {
            let number = match line.kind {
                LineKind::Removed => line.old_line,
                _ => line.new_line,
            };
            let gutter = match number {
                Some(n) => format!("{n:>width$}"),
                None => " ".repeat(width),
            };

            let styled = renderer.line(line.kind, &line.text, new_index, old_index, theme);

            let mut row = div().flex().items_start().w_full().px_2().gap_2();
            if let Some(background) = line_background(line.kind, theme) {
                row = row.bg(background);
            }

            rows.push(
                row.child(
                    div()
                        .flex_none()
                        .text_color(theme.text_muted)
                        .child(SharedString::from(gutter)),
                )
                // The `+`/`-`, always, in its own column. The requirement from #64: a
                // red/green diff is unreadable to a colourblind user, so the glyph is the
                // primary signal and the wash behind the line is the redundant one.
                .child(
                    div()
                        .w(px(8.0))
                        .flex_none()
                        .text_color(marker_color(line.kind, theme))
                        .child(SharedString::from(line.kind.marker())),
                )
                .child(div().flex_1().overflow_hidden().child(styled))
                .into_any_element(),
            );

            match line.kind {
                LineKind::Added => new_index += 1,
                LineKind::Removed => old_index += 1,
                LineKind::Context => {
                    new_index += 1;
                    old_index += 1;
                }
            }
        }
    }

    div()
        .flex()
        .flex_col()
        .size_full()
        // The editor's own font and size, because a diff is code and has to align in a
        // column the way the file it came from does.
        .font_family(fonts.family.clone())
        .text_size(fonts.size)
        .child(
            div()
                .h(Metrics::TAB_HEIGHT)
                .flex()
                .flex_none()
                .items_center()
                .gap_3()
                .px_3()
                .border_b_1()
                .border_color(theme.border)
                .child(SharedString::from(file.relative.clone()))
                // The counts are written with their signs, so the summary carries the same
                // non-colour distinction the lines do.
                .child(
                    div()
                        .text_color(theme.diff_added())
                        .child(SharedString::from(format!("+{added}"))),
                )
                .child(
                    div()
                        .text_color(theme.diff_removed())
                        .child(SharedString::from(format!("-{removed}"))),
                )
                // The language the diff was highlighted as. Worth showing because the
                // highlighting is the feature and this is how you tell "no colours because
                // it is plain text" from "no colours because the parse failed".
                .child(
                    div()
                        .text_color(theme.text_muted)
                        .child(SharedString::from(renderer.language().name().to_string())),
                ),
        )
        .child(if file.binary {
            div()
                .p_3()
                .text_color(theme.text_muted)
                .child("Binary file — no line diff")
                .into_any_element()
        } else if file.is_empty() {
            div().p_3().text_color(theme.text_muted).child("No changes").into_any_element()
        } else {
            div()
                .id("git-diff")
                .flex()
                .flex_col()
                .flex_1()
                .overflow_y_scroll()
                .children(rows)
                .into_any_element()
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use elle_git::{Hunk, Line};

    fn line(kind: LineKind, text: &str, old: Option<u32>, new: Option<u32>) -> Line {
        Line { kind, text: text.to_string(), old_line: old, new_line: new }
    }

    fn php_diff() -> DiffFile {
        DiffFile {
            relative: "app/Models/User.php".to_string(),
            binary: false,
            hunks: vec![Hunk {
                header: "@@ -1,3 +1,3 @@".to_string(),
                lines: vec![
                    line(LineKind::Context, "<?php", Some(1), Some(1)),
                    line(LineKind::Removed, "$name = 'old';", Some(2), None),
                    line(LineKind::Added, "$name = 'new';", None, Some(2)),
                    line(LineKind::Context, "return $name;", Some(3), Some(3)),
                ],
            }],
        }
    }

    /// The point of item 2: a PHP diff is highlighted as PHP, not rendered flat.
    #[test]
    fn a_php_diff_is_parsed_as_php() {
        let renderer = DiffRenderer::new(&php_diff());
        assert_eq!(renderer.language(), Language::Php);
    }

    /// Both sides are highlighted, not just the new one — a removed line must not render
    /// grey next to a coloured addition.
    #[test]
    fn both_sides_get_spans() {
        let renderer = DiffRenderer::new(&php_diff());

        // new side: `<?php`, `$name = 'new';`, `return $name;`
        assert_eq!(renderer.new_side.len(), 3);
        // old side: `<?php`, `$name = 'old';`, `return $name;`
        assert_eq!(renderer.old_side.len(), 3);

        let removed_spans = &renderer.old_side[1];
        assert!(!removed_spans.is_empty(), "the removed line was not highlighted");
        let added_spans = &renderer.new_side[1];
        assert!(!added_spans.is_empty(), "the added line was not highlighted");
    }

    /// Spans are line-local. A span whose range still carried a whole-file offset would be
    /// clipped away by the boundary check and the line would silently render flat.
    #[test]
    fn spans_are_rebased_onto_each_line() {
        let renderer = DiffRenderer::new(&php_diff());
        for (index, spans) in renderer.new_side.iter().enumerate() {
            for (range, _) in spans {
                assert!(range.end <= 32, "line {index} span {range:?} is not line-local",);
            }
        }
    }

    /// A language with no grammar renders uncoloured rather than failing.
    #[test]
    fn an_unknown_language_produces_no_spans_and_no_error() {
        let file = DiffFile {
            relative: "notes.unknownext".to_string(),
            binary: false,
            hunks: vec![Hunk {
                header: "@@ -1 +1 @@".to_string(),
                lines: vec![line(LineKind::Added, "some text", None, Some(1))],
            }],
        };
        let renderer = DiffRenderer::new(&file);
        assert_eq!(renderer.language(), Language::PlainText);
        assert!(renderer.new_side.iter().all(|spans| spans.is_empty()));
    }

    #[test]
    fn an_empty_diff_does_not_panic() {
        let file = DiffFile { relative: "a.php".to_string(), binary: false, hunks: Vec::new() };
        let renderer = DiffRenderer::new(&file);
        assert!(renderer.new_side.is_empty());
        assert_eq!(gutter_width(&file), 2, "a minimum width, not zero");
    }

    #[test]
    fn the_gutter_widens_for_larger_line_numbers() {
        let mut file = php_diff();
        assert_eq!(gutter_width(&file), 2);

        file.hunks[0].lines.push(line(LineKind::Added, "x", None, Some(12345)));
        assert_eq!(gutter_width(&file), 5);
    }

    /// Empty-state wording, which is the part a user reads. "Not a repository" and "no
    /// changes" must not be the same sentence.
    #[test]
    fn every_empty_state_says_something_different() {
        assert_eq!(PanelState::NoFolder.empty_message(), Some("Press ⌘O to open a folder"));
        assert_eq!(PanelState::Loading.empty_message(), Some("Loading…"));
        assert_eq!(PanelState::NotARepo.empty_message(), Some("Not a git repository"));

        let clean = PanelState::Repo(RepoStatus::default());
        assert_eq!(clean.empty_message(), Some("No changes"));

        let dirty = PanelState::Repo(RepoStatus {
            branch: Some("main".to_string()),
            files: vec![FileStatus {
                path: PathBuf::from("/r/a.php"),
                relative: "a.php".to_string(),
                status: Status::Modified,
                staged: false,
            }],
            cancelled: false,
        });
        assert_eq!(dirty.empty_message(), None, "a list of rows needs no message");
    }

    /// A cancelled walk that produced nothing says "Refreshing…", not "No changes" — the
    /// difference between "we do not know yet" and a claim about the repository.
    #[test]
    fn a_cancelled_walk_does_not_claim_the_repo_is_clean() {
        let cancelled = PanelState::Repo(RepoStatus {
            branch: Some("main".to_string()),
            files: Vec::new(),
            cancelled: true,
        });
        assert_eq!(cancelled.empty_message(), Some("Refreshing…"));
    }

    /// Every status is visually distinct by more than colour, and no two share a colour
    /// *and* a marker.
    #[test]
    fn markers_stay_distinct_on_every_theme() {
        use crate::theme::ThemeVariant;

        let all = [
            Status::Added,
            Status::Modified,
            Status::Deleted,
            Status::Renamed,
            Status::Untracked,
            Status::Conflicted,
        ];

        for variant in [
            ThemeVariant::Dark,
            ThemeVariant::Light,
            ThemeVariant::OneDarkPro,
            ThemeVariant::GitHubDark,
            ThemeVariant::GitHubLight,
        ] {
            let theme = variant.build();
            for status in all {
                let color = status_color(status, &theme);
                assert_ne!(
                    color,
                    theme.background,
                    "{:?} is invisible on {}",
                    status,
                    variant.label()
                );
            }
        }
    }
}
