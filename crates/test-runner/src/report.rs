//! Accumulating a stream of [`Event`]s into something a panel can render.
//!
//! The rule this type exists to enforce: **a test is only reported as passing if the runner
//! said it finished and never said it failed.** Everything else — a run that died half way,
//! a test whose verdict we could not read, output in a format we do not recognise — leaves
//! the test in a state that is not "passed" (RISKS.md #4).

use crate::teamcity::{Event, Location};

/// What happened to one test.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Status {
    /// Started, and no verdict has arrived yet. A run that ends with tests still in this
    /// state died part way through — which is shown as unfinished, not as failure and
    /// certainly not as success.
    Running,
    Passed,
    Failed,
    Skipped,
}

impl Status {
    /// A glyph that carries the verdict without colour.
    ///
    /// Pass/fail must be legible to a user who cannot distinguish the theme's green from
    /// its red, so the shape is the signal and colour is reinforcement. Same discipline as
    /// the status bar's `✕`/`⚠` diagnostic counts.
    pub fn glyph(self) -> &'static str {
        match self {
            Self::Running => "•",
            Self::Passed => "✓",
            Self::Failed => "✕",
            Self::Skipped => "○",
        }
    }

    /// A word that carries the verdict without colour or glyph, for screen readers and for
    /// anywhere a glyph alone would be ambiguous.
    pub fn label(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Passed => "passed",
            Self::Failed => "failed",
            Self::Skipped => "skipped",
        }
    }
}

/// One test in a run.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct TestCase {
    pub name: String,
    pub status: Status,
    /// The failure text, when it failed.
    pub message: Option<String>,
    /// Where to jump on click, when the runner told us and we could read it.
    pub location: Option<Location>,
    pub duration_ms: Option<u64>,
}

/// Everything a run has produced so far.
///
/// Built incrementally: the panel renders it while the run is still going, which is the
/// point of streaming.
#[derive(Clone, Default, PartialEq, Eq, Debug)]
pub struct Report {
    pub tests: Vec<TestCase>,
    /// Lines that were not service messages we could read, in the order they arrived.
    ///
    /// This is the degradation path. If a future Pest changes its output and the parser
    /// stops recognising it, the panel shows this instead of showing wrong results.
    pub output: Vec<String>,
}

impl Report {
    pub fn new() -> Self {
        Self::default()
    }

    /// Folds one event in.
    pub fn push(&mut self, event: Event) {
        match event {
            Event::Started { name } => {
                // A rerun of a name already present replaces its old verdict rather than
                // appending a second row for the same test.
                match self.index_of(&name) {
                    Some(index) => {
                        self.tests[index] = TestCase {
                            name,
                            status: Status::Running,
                            message: None,
                            location: None,
                            duration_ms: None,
                        }
                    }
                    None => self.tests.push(TestCase {
                        name,
                        status: Status::Running,
                        message: None,
                        location: None,
                        duration_ms: None,
                    }),
                }
            }
            Event::Finished { name, duration_ms } => {
                if let Some(index) = self.index_of(&name) {
                    let test = &mut self.tests[index];
                    test.duration_ms = duration_ms;
                    // `testFinished` arrives after `testFailed` and `testIgnored` too, so
                    // it may only promote a test that has no verdict yet. Treating it as
                    // "passed" unconditionally would mark every failure green — the exact
                    // wrong-result failure mode this crate is written to avoid.
                    if test.status == Status::Running {
                        test.status = Status::Passed;
                    }
                }
                // A `testFinished` for a test we never saw start is not a pass. It is a
                // gap in what we understood, and inventing a row for it would be inventing
                // a result.
            }
            Event::Failed { name, message, location } => {
                if let Some(index) = self.index_of(&name) {
                    let test = &mut self.tests[index];
                    test.status = Status::Failed;
                    test.message = Some(message);
                    test.location = location;
                } else {
                    self.tests.push(TestCase {
                        name,
                        status: Status::Failed,
                        message: Some(message),
                        location,
                        duration_ms: None,
                    });
                }
            }
            Event::Ignored { name, message } => {
                if let Some(index) = self.index_of(&name) {
                    let test = &mut self.tests[index];
                    test.status = Status::Skipped;
                    test.message = Some(message);
                }
            }
            Event::Unparsed { line } => self.output.push(line),
        }
    }

    fn index_of(&self, name: &str) -> Option<usize> {
        self.tests.iter().position(|test| test.name == name)
    }

    pub fn counts(&self) -> Counts {
        let mut counts = Counts::default();
        for test in &self.tests {
            match test.status {
                Status::Running => counts.running += 1,
                Status::Passed => counts.passed += 1,
                Status::Failed => counts.failed += 1,
                Status::Skipped => counts.skipped += 1,
            }
        }
        counts
    }

    /// The names of the tests that failed, for a rerun.
    pub fn failed_names(&self) -> Vec<String> {
        self.tests
            .iter()
            .filter(|test| test.status == Status::Failed)
            .map(|test| test.name.clone())
            .collect()
    }
}

/// A tally of a run.
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
pub struct Counts {
    pub passed: usize,
    pub failed: usize,
    pub skipped: usize,
    pub running: usize,
}

impl Counts {
    /// A one-line summary, glyph and number, no colour needed to read it.
    pub fn summary(self) -> String {
        if self.passed + self.failed + self.skipped + self.running == 0 {
            return String::new();
        }
        let mut parts = Vec::new();
        if self.passed > 0 {
            parts.push(format!("{} ✓", self.passed));
        }
        if self.failed > 0 {
            parts.push(format!("{} ✕", self.failed));
        }
        if self.skipped > 0 {
            parts.push(format!("{} ○", self.skipped));
        }
        parts.join("  ")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn report(events: Vec<Event>) -> Report {
        let mut report = Report::new();
        for event in events {
            report.push(event);
        }
        report
    }

    /// The single most important guarantee in this crate: `testFinished` arrives for failed
    /// tests too, so treating it as a pass would paint every failure green.
    ///
    /// This is the test to read first. A diff that makes it fail has made the runner lie.
    #[test]
    fn a_finished_event_after_a_failure_does_not_turn_it_into_a_pass() {
        let report = report(vec![
            Event::Started { name: "it fails".to_string() },
            Event::Failed {
                name: "it fails".to_string(),
                message: "Failed asserting that 2 is identical to 3.".to_string(),
                location: Some(Location { path: "tests/ExampleTest.php".to_string(), line: 8 }),
            },
            Event::Finished { name: "it fails".to_string(), duration_ms: Some(3) },
        ]);

        assert_eq!(report.tests[0].status, Status::Failed);
        assert_eq!(report.counts(), Counts { failed: 1, ..Default::default() });
    }

    #[test]
    fn a_skip_is_neither_a_pass_nor_a_failure() {
        let report = report(vec![
            Event::Started { name: "it is skipped".to_string() },
            Event::Ignored { name: "it is skipped".to_string(), message: "not today".to_string() },
            Event::Finished { name: "it is skipped".to_string(), duration_ms: Some(0) },
        ]);

        assert_eq!(report.tests[0].status, Status::Skipped);
        assert_eq!(report.counts(), Counts { skipped: 1, ..Default::default() });
    }

    /// A run that dies part way leaves the test it died in visible and unfinished. Not
    /// passed, not failed — we genuinely do not know, and saying so is the honest answer.
    #[test]
    fn a_run_that_dies_mid_test_leaves_it_unfinished_rather_than_passed() {
        let report = report(vec![
            Event::Started { name: "first".to_string() },
            Event::Finished { name: "first".to_string(), duration_ms: Some(1) },
            Event::Started { name: "second".to_string() },
            Event::Unparsed { line: "PHP Fatal error: allowed memory size exhausted".to_string() },
        ]);

        assert_eq!(report.counts(), Counts { passed: 1, running: 1, ..Default::default() });
        assert_eq!(report.tests[1].status, Status::Running);
        assert_eq!(report.output, vec!["PHP Fatal error: allowed memory size exhausted"]);
    }

    /// The degradation path (RISKS.md #4). If the format changes entirely, the panel has
    /// the raw output and claims nothing about any test.
    #[test]
    fn output_we_cannot_parse_becomes_raw_text_and_no_results() {
        let report = report(vec![
            Event::Unparsed { line: "some future format".to_string() },
            Event::Unparsed { line: "we do not understand".to_string() },
        ]);

        assert!(report.tests.is_empty());
        assert_eq!(report.counts(), Counts::default());
        assert_eq!(report.counts().summary(), "");
        assert_eq!(report.output.len(), 2);
    }

    /// A `testFinished` with no matching `testStarted` is a gap in our understanding, not a
    /// passing test to invent.
    #[test]
    fn a_verdict_for_a_test_we_never_saw_start_invents_nothing() {
        let report =
            report(vec![Event::Finished { name: "ghost".to_string(), duration_ms: Some(1) }]);

        assert!(report.tests.is_empty(), "{:?}", report.tests);
    }

    #[test]
    fn failed_names_drive_the_rerun_and_a_rerun_replaces_the_old_verdict() {
        let mut report = report(vec![
            Event::Started { name: "a".to_string() },
            Event::Finished { name: "a".to_string(), duration_ms: Some(1) },
            Event::Started { name: "b".to_string() },
            Event::Failed { name: "b".to_string(), message: "boom".to_string(), location: None },
        ]);
        assert_eq!(report.failed_names(), vec!["b".to_string()]);

        // Rerunning `b`, which now passes, must update the row rather than add a second.
        report.push(Event::Started { name: "b".to_string() });
        report.push(Event::Finished { name: "b".to_string(), duration_ms: Some(2) });

        assert_eq!(report.tests.len(), 2);
        assert_eq!(report.tests[1].status, Status::Passed);
        assert!(report.failed_names().is_empty());
        // The stale failure message is gone with the verdict it belonged to.
        assert_eq!(report.tests[1].message, None);
    }

    /// Pass/fail is never colour alone: a glyph and a word carry it too.
    #[test]
    fn every_status_is_legible_without_colour() {
        let statuses = [Status::Running, Status::Passed, Status::Failed, Status::Skipped];
        let glyphs: Vec<_> = statuses.iter().map(|status| status.glyph()).collect();
        let labels: Vec<_> = statuses.iter().map(|status| status.label()).collect();

        for (index, glyph) in glyphs.iter().enumerate() {
            assert!(!glyph.is_empty());
            assert!(!glyphs[index + 1..].contains(glyph), "two statuses share the glyph {glyph}");
        }
        for (index, label) in labels.iter().enumerate() {
            assert!(!labels[index + 1..].contains(label), "two statuses share {label}");
        }
    }

    #[test]
    fn the_summary_reads_without_colour() {
        let counts = Counts { passed: 2, failed: 1, skipped: 1, running: 0 };
        assert_eq!(counts.summary(), "2 ✓  1 ✕  1 ○");
    }
}
