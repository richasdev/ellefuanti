//! Running a Laravel project's tests, and reading the results honestly.
//!
//! Detects Pest or PHPUnit ([`detect`]), builds the command ([`Runner::command`]), runs it
//! streaming ([`run`]), and folds the output into a [`Report`] a panel can render while the
//! suite is still going.
//!
//! # Wrong results are worse than no test runner
//!
//! This crate reads text produced by a program it does not control, in a format that
//! changes between versions. Everything here is built around one rule: **when the output
//! stops making sense, degrade to showing it raw — never to a wrong verdict and never to a
//! panic** (RISKS.md #4). A line that is not a service message we recognise becomes
//! [`Report::output`], not a passing test. A failure whose location we cannot read is a
//! failure that is not clickable, not a jump to a guessed line. A `testFinished` that
//! follows a `testFailed` leaves the test failed.
//!
//! The format parsed is TeamCity, chosen because it is the only machine-readable output
//! both runners *stream* — `--log-junit` only exists once the process has exited. See
//! [`teamcity`] for the reasoning and for the escaping rules, which were captured from real
//! Pest 3.8.7 and PHPUnit 11.5.56 output rather than assumed.
//!
//! # A project with no test framework
//!
//! is the common case, not an edge case (§24). [`detect`] returns `None`, nothing spawns,
//! nothing is logged, and the panel has nothing to say. No PHP is required to build or test
//! this crate.
//!
//! Blocking and synchronous, so the caller chooses the executor (ADR-0007). No gpui
//! (ADR-0004).

mod detect;
mod report;
mod run;
mod teamcity;

pub use detect::{Command, Framework, Runner, Scope, detect};
pub use report::{Counts, Report, Status, TestCase};
pub use run::{CancelFlag, Outcome, run};
pub use teamcity::{Event, Location};
