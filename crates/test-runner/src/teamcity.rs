//! Parsing the TeamCity service messages Pest and PHPUnit emit under `--teamcity`.
//!
//! # Why this format
//!
//! It is the only machine-readable format both runners **stream**. `--log-junit` writes a
//! well-formed XML document, but only once the process exits: a suite that takes minutes
//! shows nothing until it is over, and a suite that crashes half way through writes no file
//! at all. TeamCity emits one line per event as it happens, which is what a panel that
//! fills in while you watch needs — and, unlike scraping the human-readable output, it is a
//! format with a specification rather than a layout that changes when someone improves the
//! progress bar.
//!
//! # What this parser promises
//!
//! It never guesses. A line that is not a well-formed service message is not a test result
//! and is reported as [`Event::Unparsed`] rather than dropped or turned into a passing
//! test. That is the whole discipline here: this is text from a program we do not control,
//! in a format that changes between versions, and **a wrong test result is worse than no
//! test runner** (RISKS.md #4). The caller shows unparsed lines as raw output. It does not
//! matter how much of a run we fail to understand, as long as we never claim a test passed
//! when it did not, or point at a file:line that is not where the failure is.
//!
//! Blocking, synchronous, no gpui (ADR-0004), no runtime (ADR-0007).

/// One decoded service message.
///
/// Only the events that change what the panel shows are modelled. Suite start/finish and
/// `testCount` carry no per-test verdict, so they parse successfully and are discarded by
/// [`Event::parse`] returning `None` — deliberately different from [`Event::Unparsed`],
/// which means "we did not understand this", not "this said nothing we needed".
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Event {
    /// A test began. Used to know a name exists before its verdict arrives, so a run that
    /// dies mid-test can still show which test it died in.
    Started { name: String },
    /// A test finished without a `testFailed` or `testIgnored` before it — a pass.
    Finished { name: String, duration_ms: Option<u64> },
    /// A test failed. `message` is the assertion text; `location` is where it failed, when
    /// the runner told us and we could read it.
    Failed { name: String, message: String, location: Option<Location> },
    /// A test was skipped or marked incomplete.
    Ignored { name: String, message: String },
    /// A line we did not understand. Kept verbatim so the caller can show it.
    Unparsed { line: String },
}

/// A file and 1-based line a failure points at.
///
/// The path is exactly what the runner printed, which is relative to the project root for
/// Pest and absolute for PHPUnit. Resolving it is the caller's job, because only the caller
/// knows the root — and resolving it wrongly is how a click opens the wrong file.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Location {
    pub path: String,
    pub line: u32,
}

impl Event {
    /// Decodes one line of runner output.
    ///
    /// Returns `None` for lines that are valid but carry nothing the panel shows (suite
    /// boundaries, `testCount`, and the blank lines and summary text both runners mix into
    /// the same stream). Returns [`Event::Unparsed`] only for a line that *looked* like a
    /// service message and could not be read — the case worth surfacing.
    pub fn parse(line: &str) -> Option<Self> {
        let trimmed = line.trim();
        let Some(rest) = trimmed.strip_prefix("##teamcity[") else {
            // Not a service message at all. Pest and PHPUnit both print a banner, a
            // progress line and a summary alongside these, and none of that is an error.
            return None;
        };
        let Some(body) = rest.strip_suffix(']') else {
            // It opened like a service message and never closed. That is a line we should
            // have understood, so say so rather than silently dropping it.
            return Some(Self::Unparsed { line: line.to_string() });
        };

        // `##teamcity[foo]` with no attributes: well-formed, but no verdict in it.
        let (kind, attributes) = body.split_once(' ')?;
        let attributes = Attributes::parse(attributes);

        match kind {
            "testStarted" => Some(Self::Started { name: attributes.get("name")? }),
            "testFinished" => Some(Self::Finished {
                name: attributes.get("name")?,
                // A duration we cannot read is a missing timing, not a failed parse: the
                // verdict is the part that must never be wrong.
                duration_ms: attributes.get("duration").and_then(|d| d.parse().ok()),
            }),
            "testFailed" => Some(Self::Failed {
                name: attributes.get("name")?,
                message: attributes.get("message").unwrap_or_default(),
                location: attributes.get("details").as_deref().and_then(parse_location),
            }),
            "testIgnored" => Some(Self::Ignored {
                name: attributes.get("name")?,
                message: attributes.get("message").unwrap_or_default(),
            }),
            // Suite boundaries and testCount are well-formed and carry no verdict.
            _ => None,
        }
    }
}

/// The `key='value'` pairs of a service message, still escaped.
struct Attributes<'a> {
    raw: &'a str,
}

impl<'a> Attributes<'a> {
    fn parse(raw: &'a str) -> Self {
        Self { raw }
    }

    /// Reads one attribute, unescaping it.
    ///
    /// Scans rather than splitting on `'` because a value may contain an escaped quote
    /// (`|'`), and splitting would cut the value in half at the first one. Both runners
    /// really do emit these — an `expect('a')->toBe('b')` failure carries `actual='|'a|''`.
    fn get(&self, key: &str) -> Option<String> {
        let mut rest = self.raw;
        loop {
            let equals = rest.find('=')?;
            let name = rest[..equals].trim_start();
            let after = rest.get(equals + 1..)?;
            let value = after.strip_prefix('\'')?;

            // Find the closing quote: the first `'` not preceded by an odd run of `|`.
            let mut end = None;
            let bytes = value.as_bytes();
            for (index, byte) in bytes.iter().enumerate() {
                if *byte != b'\'' {
                    continue;
                }
                let pipes = value[..index].bytes().rev().take_while(|b| *b == b'|').count();
                if pipes % 2 == 0 {
                    end = Some(index);
                    break;
                }
            }
            let end = end?;

            if name == key {
                return Some(unescape(&value[..end]));
            }
            rest = value.get(end + 1..)?;
        }
    }
}

/// Undoes TeamCity's escaping.
///
/// The scheme is verified against real output rather than assumed: Pest 3.8.7 escapes a
/// test named `it has a [bracket] and |pipe| and 'quote'` as
/// `it has a |[bracket|] and ||pipe|| and |'quote|'`, and puts `|n` where a failure message
/// had a newline. Anything after `|` that is not in this table is passed through as itself,
/// which is what leaves an unrecognised escape looking odd rather than truncating the text.
fn unescape(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    let mut chars = value.chars();
    while let Some(character) = chars.next() {
        if character != '|' {
            out.push(character);
            continue;
        }
        match chars.next() {
            Some('n') => out.push('\n'),
            Some('r') => out.push('\r'),
            Some('\'') => out.push('\''),
            Some('|') => out.push('|'),
            Some('[') => out.push('['),
            Some(']') => out.push(']'),
            Some(other) => out.push(other),
            // A trailing `|` with nothing after it. Keep it rather than inventing a
            // character; the line is already odd and dropping bytes would hide that.
            None => out.push('|'),
        }
    }
    out
}

/// Reads `file:line` out of a `details` attribute.
///
/// The two runners disagree on the shape, which is exactly why this is a function with
/// tests and not an inline split. Pest 3.8.7 writes `at tests/Unit/ExampleTest.php:8`
/// (relative, `at ` prefix); PHPUnit 11.5.56 writes
/// `/abs/path/tests/CalcTest.php:14` with no prefix and a trailing newline. Both may carry
/// several frames, and the **first** is the failing one.
///
/// Returns `None` unless the line really does end in `:<digits>`. A `details` that is a
/// stack trace we cannot read is a failure with no location — the failure still shows,
/// it just is not clickable. That is the honest result, and much better than pointing the
/// user at a line number we invented.
fn parse_location(details: &str) -> Option<Location> {
    let frame = details.lines().map(str::trim).find(|line| !line.is_empty())?;
    let frame = frame.strip_prefix("at ").unwrap_or(frame);
    let (path, line) = frame.rsplit_once(':')?;
    let line: u32 = line.parse().ok()?;
    if path.is_empty() || line == 0 {
        return None;
    }
    Some(Location { path: path.to_string(), line })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Captured verbatim from Pest 3.8.7. If this ever fails, the format moved and the
    /// parser — not the test — is what has to change.
    #[test]
    fn a_pest_failure_yields_its_message_and_location() {
        let line = "##teamcity[testFailed name='it fails on purpose' message='Failed asserting that 2 is identical to 3.' details='at tests/Unit/ExampleTest.php:8' flowId='22860']";

        assert_eq!(
            Event::parse(line),
            Some(Event::Failed {
                name: "it fails on purpose".to_string(),
                message: "Failed asserting that 2 is identical to 3.".to_string(),
                location: Some(Location {
                    path: "tests/Unit/ExampleTest.php".to_string(),
                    line: 8,
                }),
            })
        );
    }

    /// Captured verbatim from PHPUnit 11.5.56. Note the absolute path, the absent `at `
    /// prefix and the trailing `|n` — none of which Pest emits. One parser covers both
    /// only because this case is pinned.
    #[test]
    fn a_phpunit_failure_yields_its_message_and_location() {
        let line = "##teamcity[testFailed name='test_it_fails' message='Failed asserting that 2 is identical to 3.' details='/tmp/punitfix/tests/CalcTest.php:14|n' duration='4' flowId='26576']";

        assert_eq!(
            Event::parse(line),
            Some(Event::Failed {
                name: "test_it_fails".to_string(),
                message: "Failed asserting that 2 is identical to 3.".to_string(),
                location: Some(Location {
                    path: "/tmp/punitfix/tests/CalcTest.php".to_string(),
                    line: 14,
                }),
            })
        );
    }

    /// The escaping table, captured from a Pest run whose test name and failure values
    /// contained every character the scheme escapes. The `actual='|'a|''` in this line is
    /// the reason attribute reading scans for an unescaped quote instead of splitting.
    #[test]
    fn every_escape_sequence_round_trips() {
        let line = "##teamcity[testFailed name='it has a |[bracket|] and ||pipe|| and |'quote|' in the name' message='one|ntwo' details='at tests/Unit/EscapeTest.php:4' type='comparisonFailure' actual='|'a|'' expected='|'b|'' flowId='23699']";

        assert_eq!(
            Event::parse(line),
            Some(Event::Failed {
                name: "it has a [bracket] and |pipe| and 'quote' in the name".to_string(),
                message: "one\ntwo".to_string(),
                location: Some(Location { path: "tests/Unit/EscapeTest.php".to_string(), line: 4 }),
            })
        );
    }

    #[test]
    fn a_pass_and_a_skip_are_distinguished() {
        assert_eq!(
            Event::parse("##teamcity[testFinished name='adds numbers' duration='0' flowId='1']"),
            Some(Event::Finished { name: "adds numbers".to_string(), duration_ms: Some(0) })
        );
        assert_eq!(
            Event::parse("##teamcity[testIgnored name='it is skipped' message='nope' flowId='1']"),
            Some(Event::Ignored { name: "it is skipped".to_string(), message: "nope".to_string() })
        );
    }

    /// RISKS.md #4, and the test to read first if this parser is ever changed.
    ///
    /// Every line here is either not a service message or is one we cannot read. None of
    /// them may become a verdict. A diff that makes this fail has taught the parser to
    /// invent results out of noise, which is the one failure mode worse than not running
    /// tests at all.
    #[test]
    fn nothing_that_is_not_a_verdict_becomes_one() {
        // Ordinary output both runners interleave with the service messages.
        for line in [
            "",
            "   ",
            "PHPUnit 11.5.56 by Sebastian Bergmann and contributors.",
            "  Tests:    2 failed, 1 skipped, 2 passed (3 assertions)",
            "##teamcity[testCount count='5' flowId='22860']",
            "##teamcity[testSuiteStarted name='Default' flowId='22860']",
            "##teamcity[testSuiteFinished name='Default' flowId='22860']",
            "##teamcity[enteredTheMatrix]",
        ] {
            assert_eq!(Event::parse(line), None, "must not be a verdict: {line:?}");
        }

        // Malformed service messages: understood as "we failed to read this", never as a
        // test result, and the raw text is preserved for the caller to show.
        for line in [
            "##teamcity[testFailed name='unterminated",
            "##teamcity[testFinished name=]",
            "##teamcity[testStarted flowId='1']",
        ] {
            match Event::parse(line) {
                None | Some(Event::Unparsed { .. }) => {}
                other => panic!("must not invent a verdict from {line:?}: {other:?}"),
            }
        }
    }

    /// A failure whose `details` is not a readable frame is still a failure — it is just
    /// not clickable. Silence about the location, never a guess (RISKS.md #4).
    #[test]
    fn an_unreadable_location_leaves_the_failure_without_one() {
        for details in ["", "no line number here", "at tests/Foo.php:notanumber", "at :12"] {
            assert_eq!(parse_location(details), None, "must not resolve {details:?}");
        }

        let line = "##teamcity[testFailed name='x' message='m' details='mysterious' flowId='1']";
        assert_eq!(
            Event::parse(line),
            Some(Event::Failed { name: "x".to_string(), message: "m".to_string(), location: None })
        );
    }

    /// A stack has many frames and the first is where the failure is. Picking the last
    /// would point at the test runner's own internals.
    #[test]
    fn the_first_frame_is_the_failing_one() {
        let details = "at tests/Unit/ExampleTest.php:8\nat vendor/pest/src/Runner.php:412";
        assert_eq!(
            parse_location(details),
            Some(Location { path: "tests/Unit/ExampleTest.php".to_string(), line: 8 })
        );
    }
}
