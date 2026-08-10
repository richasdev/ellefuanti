//! The test results panel: what a run is doing, and where its failures are.
//!
//! Presentation only (ADR-0004). Detection, spawning and parsing are `elle-test-runner`'s;
//! this file decides what that looks like and what a click does.
//!
//! # What it refuses to do
//!
//! Show a verdict it does not have. A run that produced output the parser did not recognise
//! shows that output verbatim, under a heading that says so, rather than an empty green
//! list — because "no failures found" and "we could not read the results" mean opposite
//! things to someone about to push (RISKS.md #4).
//!
//! Pass and fail are never colour alone: every row carries a glyph, and the summary is
//! readable as text.

use gpui::prelude::*;
use gpui::{App, FocusHandle, Focusable, MouseButton, SharedString, Window, div, px};

use elle_test_runner::{Report, Runner, Scope, Status, TestCase};

use crate::actions::context;
use crate::theme::{Metrics, Theme, Themed as _};

/// What the panel is doing.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum RunState {
    /// No run has happened yet in this session.
    Idle,
    /// A run is in flight. The command is held so it can be shown while it runs.
    Running { command: String },
    /// A run finished on its own.
    Finished { command: String, code: Option<i32> },
    /// A run was cancelled by the user.
    Cancelled { command: String },
    /// The run could not be started at all — no framework installed, or the binary would
    /// not spawn. Distinct from a suite that failed, because the user's next move differs.
    Failed { message: String },
}

impl RunState {
    fn command(&self) -> Option<&str> {
        match self {
            Self::Running { command }
            | Self::Finished { command, .. }
            | Self::Cancelled { command } => Some(command),
            Self::Idle | Self::Failed { .. } => None,
        }
    }
}

/// What a click on a failing test row does.
///
/// A named type because the workspace is what owns the tabs: the panel holds a callback
/// into it rather than opening files itself, so there stays exactly one jump path (#88).
type JumpHandler = Box<dyn Fn(&TestCase, &mut App)>;

/// The bottom test panel.
pub struct TestView {
    focus_handle: FocusHandle,
    /// The runner detected for the open project, if any. `None` means this project has no
    /// test framework, which is an ordinary thing for a project to be (§24).
    pub runner: Option<Runner>,
    pub state: RunState,
    pub report: Report,
    /// Where a click should jump, resolved against the project root by the workspace.
    on_jump: Option<JumpHandler>,
}

impl TestView {
    pub fn new(cx: &mut Context<Self>) -> Self {
        Self {
            focus_handle: cx.focus_handle(),
            runner: None,
            state: RunState::Idle,
            report: Report::new(),
            on_jump: None,
        }
    }

    /// Installs the callback a failure row invokes when clicked.
    ///
    /// A callback rather than the panel opening the file itself: the panel does not own the
    /// tabs, and `open_path_at` is the workspace's single jump path (#88). Inventing a
    /// second one here is exactly what that issue exists to prevent.
    pub fn on_jump(&mut self, jump: impl Fn(&TestCase, &mut App) + 'static) {
        self.on_jump = Some(Box::new(jump));
    }

    /// Starts a fresh report for a run of `scope`.
    pub fn begin(&mut self, command: String, scope: &Scope, cx: &mut Context<Self>) {
        // A scoped rerun replaces only what it re-ran, so the rows for tests that are not
        // in this run would otherwise show verdicts from a previous one with no way to tell
        // them apart. Clearing is the honest option for every scope but a rerun of names,
        // where keeping the old rows is the point.
        if !matches!(scope, Scope::Names(_) | Scope::Name(_)) {
            self.report = Report::new();
        }
        self.state = RunState::Running { command };
        cx.notify();
    }

    pub fn push(&mut self, event: elle_test_runner::Event, cx: &mut Context<Self>) {
        self.report.push(event);
        cx.notify();
    }

    pub fn finish(&mut self, state: RunState, cx: &mut Context<Self>) {
        self.state = state;
        cx.notify();
    }

    /// A one-line summary for the status bar.
    pub fn summary(&self) -> String {
        summarise(&self.state, &self.report)
    }
}

/// The status-bar text for a run state.
///
/// A free function so the rule it encodes — most of all that a project with no test run
/// says *nothing* — is testable without a gpui `App` to make a `FocusHandle` from.
fn summarise(state: &RunState, report: &Report) -> String {
    match state {
        RunState::Running { .. } => {
            let progress = report.counts().summary();
            if progress.is_empty() {
                "Tests: running…".to_string()
            } else {
                format!("Tests: {progress} running…")
            }
        }
        RunState::Finished { .. } | RunState::Cancelled { .. } => {
            let summary = report.counts().summary();
            if summary.is_empty() { String::new() } else { format!("Tests: {summary}") }
        }
        // A project with no test runner has nothing to say about tests, exactly as a
        // project with no language server says nothing about diagnostics.
        RunState::Idle | RunState::Failed { .. } => String::new(),
    }
}

impl Focusable for TestView {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for TestView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();

        div()
            .key_context(context::TESTS)
            .track_focus(&self.focus_handle(cx))
            .h(Metrics::TERMINAL_HEIGHT)
            .flex_none()
            .flex()
            .flex_col()
            .bg(theme.background)
            .border_t_1()
            .border_color(theme.border)
            .child(self.render_header(&theme))
            .child(self.render_body(&theme, cx))
    }
}

impl TestView {
    /// The header: the summary, and the command that produced it.
    ///
    /// #23's rule — the command is shown, always, exactly as it was run. A runner that
    /// hides its command is impossible to debug when it disagrees with the terminal.
    fn render_header(&self, theme: &Theme) -> impl IntoElement {
        let counts = self.report.counts();
        let status: SharedString = match &self.state {
            RunState::Idle => "No tests run yet".into(),
            RunState::Running { .. } => {
                let progress = counts.summary();
                if progress.is_empty() {
                    "Running…".into()
                } else {
                    format!("Running…  {progress}").into()
                }
            }
            RunState::Cancelled { .. } => format!("Cancelled  {}", counts.summary()).into(),
            RunState::Finished { code, .. } => {
                let summary = counts.summary();
                match code {
                    // A zero exit with no tests is not a pass; it usually means the filter
                    // matched nothing. Saying "passed" there would be a lie.
                    Some(0) if counts.passed + counts.failed + counts.skipped == 0 => {
                        "No tests matched".into()
                    }
                    Some(0) => format!("Passed  {summary}").into(),
                    _ if summary.is_empty() => "The runner exited without reporting tests".into(),
                    _ => format!("Failed  {summary}").into(),
                }
            }
            RunState::Failed { message } => message.clone().into(),
        };

        // Colour reinforces the words and the glyphs; it never carries the verdict alone.
        let tint = match &self.state {
            RunState::Failed { .. } => theme.error,
            RunState::Finished { .. } if counts.failed > 0 => theme.error,
            _ => theme.text,
        };

        div()
            .flex_none()
            .flex()
            .flex_col()
            .gap_1()
            .px_3()
            .py_2()
            .bg(theme.panel)
            .border_b_1()
            .border_color(theme.border)
            .child(div().text_color(tint).child(status))
            .children(self.state.command().map(|command| {
                div()
                    .text_color(theme.text_muted)
                    .text_size(px(11.0))
                    .child(SharedString::from(command.to_string()))
            }))
    }

    fn render_body(&self, theme: &Theme, cx: &mut Context<Self>) -> impl IntoElement {
        let entity = cx.entity();

        div()
            .id("test-results")
            .flex_1()
            .overflow_y_scroll()
            .px_3()
            .py_1()
            .children(self.report.tests.iter().enumerate().map(|(index, test)| {
                let entity = entity.clone();
                let clickable = test.location.is_some();
                let colour = match test.status {
                    Status::Failed => theme.error,
                    Status::Skipped => theme.text_muted,
                    Status::Running => theme.text_muted,
                    Status::Passed => theme.text,
                };

                div()
                    .id(("test-row", index))
                    .flex()
                    .flex_col()
                    .py_1()
                    .when(clickable, |el| {
                        el.cursor_pointer().hover(|el| el.bg(theme.hover)).on_mouse_down(
                            MouseButton::Left,
                            move |_event, _window, cx| {
                                entity.update(cx, |this, cx| {
                                    let Some(test) = this.report.tests.get(index) else {
                                        return;
                                    };
                                    if let Some(jump) = &this.on_jump {
                                        let test = test.clone();
                                        jump(&test, cx);
                                    }
                                });
                            },
                        )
                    })
                    .child(
                        div()
                            .flex()
                            .gap_2()
                            .text_color(colour)
                            // The glyph, not the colour, is what says pass or fail.
                            .child(SharedString::from(test.status.glyph()))
                            .child(SharedString::from(test.name.clone())),
                    )
                    // The failure message, indented under its test.
                    .children(test.message.as_ref().filter(|_| test.status == Status::Failed).map(
                        |message| {
                            div()
                                .pl_4()
                                .text_size(px(11.0))
                                .text_color(theme.text_muted)
                                .child(SharedString::from(first_line(message)))
                        },
                    ))
            }))
            // The degradation path. Output we could not parse is shown as itself, under a
            // heading that says what it is — never silently dropped, and never allowed to
            // look like a run that found no failures.
            .children((!self.report.output.is_empty()).then(|| {
                div()
                    .flex()
                    .flex_col()
                    .pt_2()
                    .mt_2()
                    .border_t_1()
                    .border_color(theme.border)
                    .child(
                        div()
                            .text_color(theme.text_muted)
                            .text_size(px(11.0))
                            .child("Output that could not be read as test results:"),
                    )
                    .children(self.report.output.iter().take(MAX_RAW_LINES).map(|line| {
                        div()
                            .text_size(px(11.0))
                            .text_color(theme.text)
                            .child(SharedString::from(line.clone()))
                    }))
                    .children((self.report.output.len() > MAX_RAW_LINES).then(|| {
                        div().text_size(px(11.0)).text_color(theme.text_muted).child(
                            SharedString::from(format!(
                                "… and {} more lines",
                                self.report.output.len() - MAX_RAW_LINES
                            )),
                        )
                    }))
            }))
    }
}

/// How many unparsed lines to show before truncating.
///
/// A PHP fatal error carries a stack trace hundreds of frames deep, and rendering all of it
/// would build hundreds of elements for a panel this size. The count of what was dropped is
/// shown so the truncation is visible rather than silent.
const MAX_RAW_LINES: usize = 50;

/// The first line of a failure message.
///
/// Assertion diffs run to many lines and the panel gives each test one. The full text is
/// still in the report; this is only what the row shows.
fn first_line(message: &str) -> String {
    message.lines().next().unwrap_or_default().trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use elle_test_runner::{Event, Location};

    fn report(events: Vec<Event>) -> Report {
        let mut report = Report::new();
        for event in events {
            report.push(event);
        }
        report
    }

    /// A run whose output we could not read must not look like a run that found no
    /// failures. Those mean opposite things to someone about to push (RISKS.md #4).
    #[test]
    fn unreadable_output_is_kept_for_display_rather_than_dropped() {
        let report = report(vec![
            Event::Unparsed { line: "PHP Fatal error: syntax error".to_string() },
            Event::Unparsed { line: "  in /app/tests/Unit/Foo.php on line 3".to_string() },
        ]);

        assert!(report.tests.is_empty());
        assert_eq!(report.output.len(), 2);
        // And there is no summary that could be mistaken for "all green".
        assert_eq!(report.counts().summary(), "");
    }

    #[test]
    fn a_failure_message_is_shortened_to_its_first_line_for_the_row() {
        assert_eq!(
            first_line("Failed asserting that two arrays are identical.\nArray &0 [\n  'a' => 1,"),
            "Failed asserting that two arrays are identical."
        );
        assert_eq!(first_line(""), "");
    }

    /// The status bar says nothing about tests in a project that has none — the same rule
    /// the language server follows for a project with no LSP (§24).
    ///
    /// Written against [`summarise`] rather than a `TestView`, because constructing one
    /// needs a `FocusHandle` and therefore a gpui `App`. The rule being checked is entirely
    /// in the state-to-text mapping, so that is what is tested.
    #[test]
    fn a_project_with_no_run_says_nothing_in_the_status_bar() {
        assert_eq!(summarise(&RunState::Idle, &Report::new()), "");
        assert_eq!(
            summarise(
                &RunState::Failed { message: "Pest is not installed".to_string() },
                &Report::new()
            ),
            ""
        );
    }

    /// A run in progress and a finished run read differently, and both read without colour.
    #[test]
    fn the_status_bar_distinguishes_a_run_in_progress_from_a_finished_one() {
        let finished = report(vec![
            Event::Started { name: "a".to_string() },
            Event::Finished { name: "a".to_string(), duration_ms: Some(1) },
            Event::Started { name: "b".to_string() },
            Event::Failed { name: "b".to_string(), message: "x".to_string(), location: None },
        ]);

        assert_eq!(
            summarise(&RunState::Running { command: "./vendor/bin/pest".to_string() }, &finished),
            "Tests: 1 ✓  1 ✕ running…"
        );
        assert_eq!(
            summarise(
                &RunState::Finished { command: "./vendor/bin/pest".to_string(), code: Some(1) },
                &finished
            ),
            "Tests: 1 ✓  1 ✕"
        );
    }

    /// Only a failure with a location is clickable. A failure whose line we could not read
    /// is still shown — it just does not pretend to know where to go.
    #[test]
    fn only_failures_with_a_readable_location_are_clickable() {
        let report = report(vec![
            Event::Started { name: "located".to_string() },
            Event::Failed {
                name: "located".to_string(),
                message: "boom".to_string(),
                location: Some(Location { path: "tests/Foo.php".to_string(), line: 4 }),
            },
            Event::Started { name: "unlocated".to_string() },
            Event::Failed {
                name: "unlocated".to_string(),
                message: "boom".to_string(),
                location: None,
            },
        ]);

        assert!(report.tests[0].location.is_some());
        assert!(report.tests[1].location.is_none());
        assert_eq!(report.tests[1].status, Status::Failed, "still shown as a failure");
    }
}
