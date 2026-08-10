//! The parser against output captured from real runners, not output we imagined.
//!
//! The fixtures in `tests/fixtures/` are verbatim stdout from Pest 3.8.7 and PHPUnit
//! 11.5.56 on PHP 8.4.23, with only the absolute paths of the machine that produced them
//! rewritten. They include everything those runners interleave with the service messages —
//! a version banner, a blank line, a progress summary, and in PHPUnit's case a whole
//! human-readable failure report after the last event.
//!
//! That noise is the point. A parser tested only on the service messages someone pasted
//! into a string literal is a parser that has never seen the stream it will actually read,
//! and every one of these surrounding lines is a chance to invent a verdict out of nothing.
//!
//! These tests need no PHP: the output is already captured. Re-capture with
//! `./vendor/bin/pest --teamcity --colors=never` if a future version changes the format —
//! and if it has, the parser is what changes, not the assertion.

use elle_test_runner::{Event, Report, Status};

fn report_of(fixture: &str) -> Report {
    let raw = std::fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures").join(fixture),
    )
    .expect("a fixture");

    let mut report = Report::new();
    for line in raw.lines() {
        if let Some(event) = Event::parse(line) {
            report.push(event);
        }
    }
    report
}

/// Pest 3.8.7, a suite with two passes, two assertion failures, an uncaught exception and a
/// skip. Every count and every location is asserted against what the runner really said.
#[test]
fn a_real_pest_run_is_read_exactly() {
    let report = report_of("pest-3.8.7.txt");
    let counts = report.counts();

    // The runner's own summary line says "4 failed, 1 skipped, 2 passed".
    assert_eq!(counts.passed, 2, "{:#?}", report.tests);
    assert_eq!(counts.failed, 4, "{:#?}", report.tests);
    assert_eq!(counts.skipped, 1, "{:#?}", report.tests);
    assert_eq!(counts.running, 0, "nothing may be left unfinished: {:#?}", report.tests);

    let failed = |name: &str| {
        report
            .tests
            .iter()
            .find(|test| test.name == name)
            .unwrap_or_else(|| panic!("no test named {name}: {:#?}", report.tests))
            .clone()
    };

    // An assertion failure, with the line the expectation is on.
    let assertion = failed("it fails on purpose");
    assert_eq!(assertion.status, Status::Failed);
    assert_eq!(assertion.message.as_deref(), Some("Failed asserting that 2 is identical to 3."));
    let location = assertion.location.expect("a location");
    assert_eq!(location.path, "tests/Unit/ExampleTest.php");
    assert_eq!(location.line, 8);

    // An uncaught exception is a failure too, and points at the throwing line.
    let thrown = failed("it throws an uncaught error");
    assert_eq!(thrown.status, Status::Failed);
    assert_eq!(thrown.message.as_deref(), Some("RuntimeException: boom"));
    assert_eq!(thrown.location.expect("a location").line, 20);

    // The escaped name round-trips out of the wire format.
    let escaped = failed("it has a [bracket] and |pipe| and 'quote' in the name");
    assert_eq!(escaped.status, Status::Failed);

    // A pass stays a pass, and a skip is neither.
    assert_eq!(failed("adds numbers").status, Status::Passed);
    assert_eq!(failed("it is skipped").status, Status::Skipped);

    assert_eq!(report.failed_names().len(), 4);
}

/// PHPUnit 11.5.56. Same envelope, different `details` shape — absolute path, no `at `
/// prefix, trailing `|n` — which is why one parser covering both is a claim that needs a
/// test rather than an assumption.
#[test]
fn a_real_phpunit_run_is_read_exactly() {
    let report = report_of("phpunit-11.5.56.txt");
    let counts = report.counts();

    // The runner's own summary: "Tests: 3, Assertions: 2, Failures: 1, Skipped: 1."
    assert_eq!(counts.passed, 1, "{:#?}", report.tests);
    assert_eq!(counts.failed, 1, "{:#?}", report.tests);
    assert_eq!(counts.skipped, 1, "{:#?}", report.tests);
    assert_eq!(counts.running, 0, "{:#?}", report.tests);

    let failure =
        report.tests.iter().find(|test| test.status == Status::Failed).expect("a failure");
    assert_eq!(failure.name, "test_it_fails");
    let location = failure.location.as_ref().expect("a location");
    assert_eq!(location.path, "/projects/punitfix/tests/CalcTest.php");
    assert_eq!(location.line, 14);
}

/// The guarantee that matters most, stated against real streams (RISKS.md #4).
///
/// Both fixtures carry banners, blank lines, progress summaries and — for PHPUnit — a full
/// human-readable failure report printed *after* the service messages, including a bare
/// `path:14` line that looks a great deal like a location. None of it may become a test.
#[test]
fn the_prose_runners_print_around_the_events_never_becomes_a_verdict() {
    for fixture in ["pest-3.8.7.txt", "phpunit-11.5.56.txt"] {
        let report = report_of(fixture);

        // Every test in the report came from a service message, so every one has a name
        // that appeared in a `testStarted` or `testFailed`.
        for test in &report.tests {
            assert!(!test.name.is_empty(), "{fixture}: a test with no name");
            assert!(
                !test.name.contains("##teamcity"),
                "{fixture}: raw markup leaked into a name: {}",
                test.name
            );
        }

        // The counts equal the number of tests, so no phantom rows were added and none
        // were silently dropped.
        let counts = report.counts();
        assert_eq!(
            counts.passed + counts.failed + counts.skipped + counts.running,
            report.tests.len(),
            "{fixture}: counts disagree with rows"
        );
    }
}

/// Only lines that opened like a service message and could not be read are surfaced as raw
/// output. Ordinary prose is not — a panel that showed the whole banner as "unparsed
/// output" would cry wolf on every successful run.
#[test]
fn ordinary_runner_prose_is_not_reported_as_unreadable_output() {
    for fixture in ["pest-3.8.7.txt", "phpunit-11.5.56.txt"] {
        let report = report_of(fixture);
        assert!(
            report.output.is_empty(),
            "{fixture}: healthy output should need no raw fallback, got {:?}",
            report.output
        );
    }
}
