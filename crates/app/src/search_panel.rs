//! The find-in-project panel (#80): the sidebar's second tab, and the Search activity-bar
//! entry that has been disabled since the first commit.
//!
//! Shaped like [`crate::find_bar::FindBar`] — a self-contained widget that reports what the
//! user did as events and knows nothing about what those events mean — because that is the
//! pattern the find bar already established and this is the same kind of control. What it
//! is *not* is a `FindBar` with a list bolted on: they share no state, they answer different
//! keys (⏎ here runs the search rather than advancing a cursor), and the panel has no
//! concept of a current match because there is no cursor in a list of files.
//!
//! # Replace-in-project is deliberately absent
//!
//! There is no replace field here and no "Replace All" button, and that is a scope decision
//! rather than an omission to fill in later. Replacing across a project writes to files that
//! are not open, most of which the user has not read, and the undo story for that is not
//! `Document::undo` — it is either a multi-file transaction or a preview-and-apply flow with
//! its own confirmation. Shipping a button that edits forty files with no way back would be
//! the single most destructive thing in this editor. It gets its own PR.
//!
//! # Streaming, cancellation and the debounce
//!
//! All three live in `workspace_view.rs`, because they are about scheduling and this widget
//! has no executor. What lives here is the *display* of their outcomes: [`SearchState`] has
//! a `Searching` variant so a slow project shows "Searching…" rather than an empty list that
//! looks like "no results", which is the difference between a tool that is working and one
//! that appears broken.

use gpui::{
    App, Context, EventEmitter, FocusHandle, Focusable, KeyDownEvent, MouseButton, SharedString,
    Window, div, prelude::*, px, uniform_list,
};

use crate::actions::{
    Backspace, Cancel, Confirm, ToggleCaseSensitive, ToggleRegex, ToggleWholeWord, context,
};
use crate::editor::{ProjectResults, SearchQuery};
use crate::find_bar::Option_;
use crate::fonts::Fonts;
use crate::theme::{Metrics, Theme, Themed};

/// What the panel tells the workspace.
pub enum SearchPanelEvent {
    /// The query changed. The workspace debounces this — see `SEARCH_DEBOUNCE`.
    QueryChanged,
    /// ⏎: search now, without waiting out the debounce.
    SearchNow,
    /// A result row was clicked: open this file at this position.
    OpenResult { file: usize, line: usize },
    /// Escape: hand focus back to the editor. The panel stays open, unlike the find bar —
    /// a results list you spent seven seconds populating must not vanish on a stray key.
    Dismissed,
}

/// Where a search is in its life.
///
/// Four states rather than an `Option<ProjectResults>`, because "not started", "running"
/// and "finished with nothing" are three different things to a reader and collapsing them
/// is how a panel ends up claiming "No results" while it is still working.
#[derive(Clone, Debug, Default)]
pub enum SearchState {
    /// Nothing typed yet.
    #[default]
    Idle,
    /// A search is in flight. Carries the previous results so the list does not blank out
    /// and reflow on every keystroke — the rows stay put and the header says "Searching…".
    Searching(ProjectResults),
    Done(ProjectResults),
}

impl SearchState {
    fn results(&self) -> Option<&ProjectResults> {
        match self {
            SearchState::Idle => None,
            SearchState::Searching(results) | SearchState::Done(results) => Some(results),
        }
    }

    /// The one line under the query field, and whether it reads as a problem.
    ///
    /// Split out from the render for the same reason `find_bar::Status::label` is: the
    /// wording is the part a user actually reads, and it should be assertable without
    /// opening a window.
    pub fn summary(&self) -> (SharedString, bool) {
        match self {
            SearchState::Idle => ("".into(), false),
            SearchState::Searching(_) => ("Searching…".into(), false),
            SearchState::Done(results) if results.invalid => ("Invalid pattern".into(), true),
            SearchState::Done(results) if results.is_empty() => ("No results".into(), true),
            SearchState::Done(results) => {
                let matches = results.match_count();
                let files = results.file_count();
                let text = format!(
                    "{matches} result{} in {files} file{}{}",
                    plural(matches),
                    plural(files),
                    if results.truncated { " (showing the first" } else { "" },
                );
                // The truncation notice is appended rather than replacing the count,
                // because "1000 results" with no qualifier is a number the user would
                // believe. `crates/editor/project_search.rs` caps at MAX_RESULTS.
                let text = if results.truncated { format!("{text} {matches})") } else { text };
                (text.into(), false)
            }
        }
    }
}

fn plural(count: usize) -> &'static str {
    if count == 1 { "" } else { "s" }
}

/// One row of the flattened result list.
///
/// The results are a tree — files containing lines — and `uniform_list` wants a flat,
/// fixed-height sequence. Flattening once per results change rather than per frame is what
/// keeps the per-frame cost proportional to the *viewport* and not to the 1,000 hits behind
/// it, which is the property #10 established for the file tree and #52 for highlighting.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Row {
    /// A file header: index into `ProjectResults::files`.
    File(usize),
    /// A result line: `(file index, line index within that file)`.
    Line(usize, usize),
}

pub struct SearchPanel {
    focus_handle: FocusHandle,
    query: SearchQuery,
    state: SearchState,
    /// The flattened [`Row`] list, rebuilt when results change and never in a render.
    rows: Vec<Row>,
}

impl EventEmitter<SearchPanelEvent> for SearchPanel {}

impl SearchPanel {
    pub fn new(cx: &mut Context<Self>) -> Self {
        Self {
            focus_handle: cx.focus_handle(),
            query: SearchQuery::default(),
            state: SearchState::default(),
            rows: Vec::new(),
        }
    }

    pub fn query(&self) -> &SearchQuery {
        &self.query
    }

    pub fn state(&self) -> &SearchState {
        &self.state
    }

    /// Seeds the field from the editor's selection, like ⌘F does.
    ///
    /// Single-line only, same rule as [`crate::find_bar::FindBar::seed`]: a multi-line
    /// selection means the user selected a block, not that they want to search for one.
    /// Returns whether anything changed, so the caller only kicks off a search when it did.
    pub fn seed(&mut self, text: &str) -> bool {
        if text.is_empty() || text.contains('\n') || self.query.pattern == text {
            return false;
        }
        self.query.pattern = text.to_string();
        true
    }

    pub fn set_state(&mut self, state: SearchState, cx: &mut Context<Self>) {
        self.rows = flatten(state.results());
        self.state = state;
        cx.notify();
    }

    /// Types into the query field, as `on_key_down` would.
    ///
    /// The same seam `FindBar::type_query_for_test` opens and for the same reason: gpui's
    /// test harness cannot synthesise a `KeyDownEvent` carrying a `key_char`, so a test
    /// that wanted to drive the panel from keystrokes could not. This sets the field the
    /// keystroke path sets and emits the event it emits.
    #[cfg(test)]
    pub fn type_query_for_test(&mut self, pattern: &str, cx: &mut Context<Self>) {
        self.query.pattern = pattern.to_string();
        cx.emit(SearchPanelEvent::QueryChanged);
        cx.notify();
    }

    #[cfg(test)]
    pub fn row_count_for_test(&self) -> usize {
        self.rows.len()
    }

    fn on_key_down(&mut self, event: &KeyDownEvent, _w: &mut Window, cx: &mut Context<Self>) {
        // The same filter as the find bar's, which took three tries there and is reused
        // rather than rewritten: modified chords are actions, named keys are actions, and
        // only `key_char` is text.
        let keystroke = &event.keystroke;
        if keystroke.modifiers.platform
            || keystroke.modifiers.control
            || keystroke.modifiers.function
        {
            return;
        }
        if matches!(
            keystroke.key.as_str(),
            "enter" | "escape" | "up" | "down" | "backspace" | "tab" | "left" | "right"
        ) {
            return;
        }
        let Some(text) = keystroke.key_char.as_deref() else { return };
        if text.is_empty() || text.chars().all(|c| c.is_control()) {
            return;
        }

        self.query.pattern.push_str(text);
        cx.emit(SearchPanelEvent::QueryChanged);
        cx.notify();
    }

    fn backspace(&mut self, _: &Backspace, _w: &mut Window, cx: &mut Context<Self>) {
        self.query.pattern.pop();
        cx.emit(SearchPanelEvent::QueryChanged);
        cx.notify();
    }

    /// ⏎: run the search immediately rather than waiting out the debounce.
    ///
    /// The debounce exists so that *typing* does not start a search per keystroke. Pressing
    /// return is an explicit "I have finished typing", and making the user wait another
    /// 250 ms after it would be the control ignoring them.
    fn confirm(&mut self, _: &Confirm, _w: &mut Window, cx: &mut Context<Self>) {
        cx.emit(SearchPanelEvent::SearchNow);
    }

    fn cancel(&mut self, _: &Cancel, _w: &mut Window, cx: &mut Context<Self>) {
        cx.emit(SearchPanelEvent::Dismissed);
    }

    pub fn toggle_option(&mut self, option: Option_, cx: &mut Context<Self>) {
        let flag = match option {
            Option_::CaseSensitive => &mut self.query.case_sensitive,
            Option_::WholeWord => &mut self.query.whole_word,
            Option_::Regex => &mut self.query.regex,
        };
        *flag = !*flag;
        // A toggle is a deliberate act, not a keystroke in a stream, so it searches now
        // rather than debouncing — the same reasoning as ⏎ above.
        cx.emit(SearchPanelEvent::SearchNow);
        cx.notify();
    }

    fn toggle_case_sensitive(
        &mut self,
        _: &ToggleCaseSensitive,
        _w: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.toggle_option(Option_::CaseSensitive, cx);
    }

    fn toggle_whole_word(&mut self, _: &ToggleWholeWord, _w: &mut Window, cx: &mut Context<Self>) {
        self.toggle_option(Option_::WholeWord, cx);
    }

    fn toggle_regex(&mut self, _: &ToggleRegex, _w: &mut Window, cx: &mut Context<Self>) {
        self.toggle_option(Option_::Regex, cx);
    }
}

/// Flattens the file/line tree into the fixed-height row sequence `uniform_list` wants.
fn flatten(results: Option<&ProjectResults>) -> Vec<Row> {
    let Some(results) = results else { return Vec::new() };
    let mut rows = Vec::new();
    for (file, matches) in results.files.iter().enumerate() {
        rows.push(Row::File(file));
        rows.extend((0..matches.lines.len()).map(|line| Row::Line(file, line)));
    }
    rows
}

impl Focusable for SearchPanel {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for SearchPanel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let fonts = Fonts::get(cx);

        div()
            .key_context(context::SEARCH_PANEL)
            .track_focus(&self.focus_handle(cx))
            .on_key_down(cx.listener(Self::on_key_down))
            .on_action(cx.listener(Self::backspace))
            .on_action(cx.listener(Self::confirm))
            .on_action(cx.listener(Self::cancel))
            .on_action(cx.listener(Self::toggle_case_sensitive))
            .on_action(cx.listener(Self::toggle_whole_word))
            .on_action(cx.listener(Self::toggle_regex))
            .flex()
            .flex_col()
            .size_full()
            .overflow_hidden()
            .text_size(fonts.ui_size)
            .text_color(theme.text)
            .child(self.render_query_row(&theme, cx))
            .child(self.render_summary(&theme))
            .child(self.render_rows(&theme, cx))
    }
}

impl SearchPanel {
    fn render_query_row(&self, theme: &Theme, cx: &mut Context<Self>) -> impl IntoElement {
        let empty = self.query.pattern.is_empty();
        let text = if empty { "Search".to_string() } else { self.query.pattern.clone() };

        div()
            .flex()
            .flex_none()
            .flex_col()
            .gap_1()
            .px_2()
            .py_1()
            .child(
                // No caret and no selection, the same admission `find_bar::text_field`
                // makes: gpui has no text input element, and faking one with a blinking
                // block that cannot be clicked into would be worse than a field that is
                // honest about being append-and-backspace only.
                div()
                    .h(px(22.0))
                    .flex()
                    .items_center()
                    .px_2()
                    .rounded_sm()
                    .bg(theme.background)
                    .border_1()
                    .border_color(theme.accent)
                    .when(empty, |el| el.text_color(theme.text_muted))
                    .child(SharedString::from(text)),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_1()
                    .child(self.render_toggle(
                        "Aa",
                        Option_::CaseSensitive,
                        self.query.case_sensitive,
                        theme,
                        cx,
                    ))
                    .child(self.render_toggle(
                        "ab",
                        Option_::WholeWord,
                        self.query.whole_word,
                        theme,
                        cx,
                    ))
                    .child(self.render_toggle(".*", Option_::Regex, self.query.regex, theme, cx)),
            )
    }

    /// A toggle for one search option. The find bar's, with the same two-channel active
    /// state (#71): a background *and* a text colour, never colour alone.
    fn render_toggle(
        &self,
        label: &'static str,
        option: Option_,
        active: bool,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let entity = cx.entity();
        div()
            // Prefixed rather than bare, because `find_bar` uses the same three labels for
            // the same three toggles and both widgets are on screen at once whenever the
            // find bar is open with Search selected. Two elements with one id is a click
            // that reaches whichever gpui found first.
            .id(match option {
                Option_::CaseSensitive => "search-panel-case",
                Option_::WholeWord => "search-panel-word",
                Option_::Regex => "search-panel-regex",
            })
            .size(px(20.0))
            .flex()
            .items_center()
            .justify_center()
            .rounded_sm()
            .cursor_pointer()
            .when(active, |el| el.bg(theme.selected).text_color(theme.accent))
            .when(!active, |el| el.text_color(theme.text_muted))
            .hover(|el| el.bg(theme.hover))
            .active(|el| el.bg(theme.pressed))
            .on_mouse_down(MouseButton::Left, move |_ev, _window, cx| {
                entity.update(cx, |panel, cx| panel.toggle_option(option, cx));
            })
            .child(SharedString::from(label))
    }

    fn render_summary(&self, theme: &Theme) -> impl IntoElement {
        let (text, problem) = self.state.summary();
        div()
            .flex_none()
            .px_3()
            .py_1()
            .text_color(if problem { theme.error } else { theme.text_muted })
            .child(text)
    }

    /// The result list.
    ///
    /// `uniform_list` rather than a `children(...)` over every row, and that is the whole
    /// reason [`Row`] exists: a query like `function` finds 659 hits on a small project and
    /// 1,000 on a medium one (the cap), and laying out a thousand elements per frame is how
    /// a panel drops frames while merely being open. `uniform_list` builds only the visible
    /// window, so the cost tracks the viewport — #10's property, applied here.
    fn render_rows(&self, theme: &Theme, cx: &mut Context<Self>) -> impl IntoElement {
        let entity = cx.entity();
        let count = self.rows.len();

        let text = theme.text;
        let muted = theme.text_muted;
        let accent = theme.accent;
        let hover = theme.hover;
        let pressed = theme.pressed;
        // The hit highlight. `search_match()` is `selected`, defined per variant as one step
        // from the background — the same colour the in-file highlight uses, which is what
        // makes a hit in the panel and the same hit in the editor read as one thing. A
        // hardcoded yellow would be invisible on `#ffffff` and glaring on `#282c34`.
        let highlight = theme.search_match();

        uniform_list("project-search-results", count, move |range, _window, cx| {
            entity.update(cx, |this, _cx| {
                let Some(results) = this.state.results() else { return Vec::new() };

                range
                    .filter_map(|index| {
                        let element = match this.rows.get(index)? {
                            Row::File(file) => {
                                let matches = results.files.get(*file)?;
                                div()
                                    .id(("search-file", index))
                                    .flex()
                                    .items_center()
                                    .gap_2()
                                    .h(Metrics::ROW_HEIGHT)
                                    .px_2()
                                    .text_color(text)
                                    .child(SharedString::from(matches.relative.clone()))
                                    .child(div().text_color(muted).child(SharedString::from(
                                        matches.match_count().to_string(),
                                    )))
                                    .into_any_element()
                            }
                            Row::Line(file, line) => {
                                let matches = results.files.get(*file)?;
                                let hit = matches.lines.get(*line)?;
                                let entity = entity.clone();
                                let (file, line) = (*file, *line);

                                div()
                                    .id(("search-line", index))
                                    .flex()
                                    .items_center()
                                    .gap_2()
                                    .h(Metrics::ROW_HEIGHT)
                                    .pl_5()
                                    .pr_2()
                                    .cursor_pointer()
                                    .hover(|el| el.bg(hover))
                                    .active(|el| el.bg(pressed))
                                    .on_mouse_down(MouseButton::Left, move |_ev, _window, cx| {
                                        entity.update(cx, |_, cx| {
                                            cx.emit(SearchPanelEvent::OpenResult { file, line });
                                        });
                                    })
                                    .child(
                                        div()
                                            .flex_none()
                                            .w(px(34.0))
                                            .text_color(muted)
                                            // One-based for display; `row` is zero-based
                                            // because that is what `Point` wants, and the
                                            // conversion happens exactly here.
                                            .child(SharedString::from((hit.row + 1).to_string())),
                                    )
                                    .children(segments(&hit.text, &hit.ranges).map(
                                        |(part, is_hit)| {
                                            div()
                                                .flex_none()
                                                .when(is_hit, |el| {
                                                    el.bg(highlight).text_color(accent)
                                                })
                                                .when(!is_hit, |el| el.text_color(muted))
                                                .child(SharedString::from(part))
                                        },
                                    ))
                                    .into_any_element()
                            }
                        };
                        Some(element)
                    })
                    .collect()
            })
        })
        .flex_1()
    }
}

/// Splits a result line into `(text, is_a_hit)` runs.
///
/// A `div` per run rather than one `StyledText` with highlight ranges, because a row is
/// three or four runs and the flex layout already puts them side by side. It also keeps the
/// multibyte failure mode out of the render entirely: every boundary here comes from
/// `ranges`, which `project_search` guarantees is on a char boundary and which this asserts
/// again by construction — the slices are taken with `get`, so an out-of-range range yields
/// nothing rather than the debug panic a direct index would give.
fn segments<'a>(
    text: &'a str,
    ranges: &'a [std::ops::Range<usize>],
) -> impl Iterator<Item = (String, bool)> + 'a {
    let mut cursor = 0usize;
    let mut ranges = ranges.iter();
    let mut pending: Option<(String, bool)> = None;
    let mut done = false;

    std::iter::from_fn(move || {
        if let Some(item) = pending.take() {
            return Some(item);
        }
        if done {
            return None;
        }
        match ranges.next() {
            Some(range) => {
                let before = text.get(cursor..range.start).unwrap_or("");
                let hit = text.get(range.clone()).unwrap_or("");
                cursor = range.end;
                if hit.is_empty() {
                    return Some((before.to_string(), false));
                }
                pending = Some((hit.to_string(), true));
                Some((before.to_string(), false))
            }
            None => {
                done = true;
                let tail = text.get(cursor..).unwrap_or("");
                Some((tail.to_string(), false))
            }
        }
    })
    .filter(|(part, _)| !part.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::editor::{FileMatches, LineMatch};
    use std::path::PathBuf;

    /// A `LineMatch` for a test, built from a slice rather than a `Vec`.
    ///
    /// A slice because `vec![0..6]` at a call site trips clippy's `single_range_in_vec_init`
    /// — the lint that catches `vec![0..6]` written where `vec![0; 6]` was meant. The lint
    /// is right that the two are easy to confuse, and taking `&[_]` makes the call sites
    /// read `&[0..6]`, which is unambiguous rather than merely silenced.
    fn line(row: u32, text: &str, ranges: &[std::ops::Range<usize>]) -> LineMatch {
        LineMatch::for_test(row, text, ranges.to_vec())
    }

    /// A `LineMatch` with exactly one hit — the shape most of these tests want.
    ///
    /// Separate from `line` because `&[0..6]`, a one-element array of ranges written
    /// inline, trips `single_range_in_vec_init`: the lint that catches `vec![0..6]` written
    /// where `vec![0; 6]` was meant. It is a real ambiguity, so the fix is to say which one
    /// is meant rather than to allow the lint.
    fn hit(row: u32, text: &str, range: std::ops::Range<usize>) -> LineMatch {
        LineMatch::for_test(row, text, vec![range])
    }

    /// `segments`, called with one highlighted range.
    ///
    /// Exists for the same reason `line` takes a slice: a one-element array of ranges
    /// written inline (`&[2..8]`) trips `single_range_in_vec_init`, the lint that catches
    /// `vec![0..6]` where `vec![0; 6]` was meant. Naming the single-range case removes the
    /// ambiguity the lint is pointing at instead of asserting the lint is wrong.
    fn one(text: &str, range: std::ops::Range<usize>) -> Vec<(String, bool)> {
        segments(text, std::slice::from_ref(&range)).collect()
    }

    fn results(files: Vec<(&str, Vec<LineMatch>)>) -> ProjectResults {
        ProjectResults {
            files: files
                .into_iter()
                .map(|(relative, lines)| FileMatches {
                    path: PathBuf::from(format!("/root/{relative}")),
                    relative: relative.to_string(),
                    lines,
                })
                .collect(),
            ..Default::default()
        }
    }

    // --- the summary line ----------------------------------------------------------

    #[test]
    fn an_idle_panel_says_nothing() {
        // Not "0 results": nothing has been asked, so nothing has been answered.
        let (text, problem) = SearchState::Idle.summary();
        assert_eq!(text, "");
        assert!(!problem);
    }

    #[test]
    fn a_running_search_says_so_rather_than_showing_no_results() {
        // The failure this rules out is the one that makes a tool look broken: a project
        // that takes 400 ms showing "No results" for 400 ms before showing 200 of them.
        let (text, problem) = SearchState::Searching(ProjectResults::default()).summary();
        assert_eq!(text, "Searching…");
        assert!(!problem, "working is not a problem");
    }

    #[test]
    fn a_finished_search_counts_hits_and_files_separately() {
        // Two hits on one line in one file is "2 results in 1 file" — the counts are
        // different numbers and conflating them misreports both.
        let state = SearchState::Done(results(vec![(
            "app/Models/User.php",
            vec![line(3, "$needle and $needle", &[1..7, 13..19])],
        )]));
        assert_eq!(state.summary().0, "2 results in 1 file");
    }

    #[test]
    fn one_result_is_singular_in_both_halves() {
        let state = SearchState::Done(results(vec![("a.php", vec![hit(0, "needle", 0..6)])]));
        assert_eq!(state.summary().0, "1 result in 1 file");
    }

    #[test]
    fn no_results_reads_as_a_problem() {
        let (text, problem) = SearchState::Done(ProjectResults::default()).summary();
        assert_eq!(text, "No results");
        assert!(problem);
    }

    #[test]
    fn an_invalid_pattern_is_distinct_from_no_results() {
        // Different text because the fix is different: one means "it is not there", the
        // other means "you have not finished typing a regex".
        let invalid = ProjectResults { invalid: true, ..Default::default() };
        let (text, problem) = SearchState::Done(invalid).summary();
        assert_eq!(text, "Invalid pattern");
        assert!(problem);
    }

    #[test]
    fn a_truncated_search_admits_it() {
        // "1000 results" with no qualifier is a number the user would believe. RISKS.md #4.
        let truncated = ProjectResults {
            files: results(vec![("a.php", vec![hit(0, "needle", 0..6)])]).files,
            truncated: true,
            ..Default::default()
        };
        let (text, _) = SearchState::Done(truncated).summary();
        assert!(text.contains("showing the first"), "{text}");
    }

    // --- flattening ----------------------------------------------------------------

    #[test]
    fn each_file_contributes_a_header_row_plus_one_row_per_line() {
        let results = results(vec![
            ("a.php", vec![hit(0, "x", 0..1), hit(5, "x", 0..1)]),
            ("b.php", vec![hit(2, "x", 0..1)]),
        ]);
        let rows = flatten(Some(&results));

        assert_eq!(
            rows,
            vec![Row::File(0), Row::Line(0, 0), Row::Line(0, 1), Row::File(1), Row::Line(1, 0)]
        );
    }

    #[test]
    fn an_absent_result_set_flattens_to_nothing() {
        assert!(flatten(None).is_empty());
        assert!(flatten(Some(&ProjectResults::default())).is_empty());
    }

    // --- highlight segmentation ----------------------------------------------------

    #[test]
    fn a_line_splits_into_plain_and_highlighted_runs() {
        let parts = one("a needle b", 2..8);
        assert_eq!(
            parts,
            vec![
                ("a ".to_string(), false),
                ("needle".to_string(), true),
                (" b".to_string(), false)
            ]
        );
    }

    #[test]
    fn two_hits_produce_two_highlighted_runs() {
        let parts: Vec<_> = segments("aa bb aa", &[0..2, 6..8]).collect();
        assert_eq!(
            parts,
            vec![("aa".to_string(), true), (" bb ".to_string(), false), ("aa".to_string(), true)]
        );
    }

    #[test]
    fn a_hit_covering_the_whole_line_produces_one_run() {
        let parts = one("needle", 0..6);
        assert_eq!(parts, vec![("needle".to_string(), true)]);
    }

    #[test]
    fn a_line_with_no_hits_is_one_plain_run() {
        let parts: Vec<_> = segments("plain", &[]).collect();
        assert_eq!(parts, vec![("plain".to_string(), false)]);
    }

    #[test]
    fn multibyte_runs_are_sliced_on_char_boundaries() {
        // `função` is **8** bytes for 6 characters — `ç` and `ã` are two each — so the hit
        // is `2..10`, not `2..8`. Writing `2..9` here the first time produced `funçã`,
        // which is exactly the class of bug this test exists for: a range computed in
        // characters slices inside a codepoint, and in a debug build the render panics.
        let parts = one("a função b", 2..10);
        assert_eq!(
            parts,
            vec![
                ("a ".to_string(), false),
                ("função".to_string(), true),
                (" b".to_string(), false),
            ]
        );
    }

    #[test]
    fn a_range_past_the_end_yields_nothing_rather_than_panicking() {
        // Defence in depth: `project_search` will not produce this, and `segments` still
        // must not be the thing that panics if it ever does.
        let parts = one("short", 2..999);
        assert_eq!(parts, vec![("sh".to_string(), false)]);
    }

    #[test]
    fn a_range_landing_mid_codepoint_yields_nothing_rather_than_panicking() {
        // `ç` occupies bytes 0..2, so 0..1 is inside it. `str::get` returns None rather
        // than panicking, which is the entire reason it is used instead of indexing.
        let parts = one("ção", 0..1);
        assert!(parts.iter().all(|(part, hit)| !*hit || !part.is_empty()));
    }
}
