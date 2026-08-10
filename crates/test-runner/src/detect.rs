//! Deciding whether a project has a test runner, and what to type to invoke it.
//!
//! Detection is a filesystem question with a yes/no answer, so it is stated as one:
//! [`detect`] returns `None` for a folder with no runner, and that is not an error. **A
//! project with no PHP and no test framework is the common case, not an edge case** (§24) —
//! it produces no dialog, no log line and no work, because nothing here runs until someone
//! asks for a test run.

use std::path::{Path, PathBuf};

/// Which runner a project uses.
///
/// Pest is checked first because a Pest project has both binaries installed — `pestphp/pest`
/// depends on `phpunit/phpunit` — and running `phpunit` there fails with *"Please run
/// [./vendor/bin/pest] instead."*, which was verified against Pest 3.8.7 rather than
/// assumed. Preferring `phpunit` would therefore break every Pest project.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Framework {
    Pest,
    PhpUnit,
}

impl Framework {
    /// The name to show a human.
    pub fn label(self) -> &'static str {
        match self {
            Self::Pest => "Pest",
            Self::PhpUnit => "PHPUnit",
        }
    }

    /// The binary, relative to the project root.
    fn binary(self) -> &'static str {
        match self {
            Self::Pest => "vendor/bin/pest",
            Self::PhpUnit => "vendor/bin/phpunit",
        }
    }
}

/// A project's test runner, and where it lives.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Runner {
    pub framework: Framework,
    /// Absolute path to the binary, so spawning does not depend on the working directory.
    pub binary: PathBuf,
    pub root: PathBuf,
}

/// What to run.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub enum Scope {
    /// The whole suite.
    #[default]
    All,
    /// Every test in one file. The path is passed to the runner as given.
    File(PathBuf),
    /// One test, selected by `--filter`.
    ///
    /// The name is a literal test name, and both runners treat `--filter` as a regular
    /// expression, so it is quoted by [`Scope::args`] before it reaches the command line.
    Name(String),
    /// A set of tests, selected by one alternated `--filter` — the rerun-failed case.
    ///
    /// One command rather than one per test: a suite pays its bootstrap cost (autoloader,
    /// framework boot, database migrations) on every invocation, and twenty reruns would
    /// pay it twenty times. Verified against Pest 3.8.7, where an alternation of two names
    /// selects exactly those two.
    Names(Vec<String>),
}

impl Scope {
    /// The arguments that select this scope.
    fn args(&self) -> Vec<String> {
        match self {
            Self::All => Vec::new(),
            Self::File(path) => vec![path.display().to_string()],
            // `--filter` is a regex in both runners, so a test named `it adds 1 + 1` has to
            // be quoted or the `+` makes it a pattern that matches something else.
            //
            // Anchored at the end only, which was measured rather than assumed: Pest 3.8.7
            // matches the filter against a *qualified* name (`Tests\Unit\ExampleTest::adds
            // numbers`), so `--filter=/^adds numbers$/` selects zero tests while
            // `--filter=adds numbers$` selects exactly the one. A leading `^` would make
            // "run this one test" silently run nothing at all.
            Self::Name(name) => vec![format!("--filter={}$", escape_regex(name))],
            // An empty set would produce `--filter=$`, which matches everything — "rerun
            // the nothing that failed" would run the whole suite. Nothing selected means
            // nothing to run, and the caller checks for it.
            Self::Names(names) if names.is_empty() => Vec::new(),
            Self::Names(names) => {
                let alternation =
                    names.iter().map(|name| escape_regex(name)).collect::<Vec<_>>().join("|");
                vec![format!("--filter=({alternation})$")]
            }
        }
    }

    /// Whether this scope selects nothing at all, so the caller can decline to run rather
    /// than run everything.
    pub fn is_empty(&self) -> bool {
        matches!(self, Self::Names(names) if names.is_empty())
    }
}

/// Escapes every character that means something to PCRE.
fn escape_regex(name: &str) -> String {
    const SPECIAL: &[char] =
        &['\\', '.', '+', '*', '?', '[', ']', '^', '$', '(', ')', '{', '}', '|', '/', '-', '#'];
    let mut out = String::with_capacity(name.len());
    for character in name.chars() {
        if SPECIAL.contains(&character) {
            out.push('\\');
        }
        out.push(character);
    }
    out
}

impl Runner {
    /// The full command line, as the user would type it.
    ///
    /// This is what the panel shows. #23's rule: a runner that hides its command is
    /// impossible to debug when it disagrees with the terminal, so the string shown and the
    /// arguments spawned are built here, once, and cannot drift apart.
    pub fn command(&self, scope: &Scope) -> Command {
        let mut args = vec![
            "--teamcity".to_string(),
            // The panel parses this; ANSI escapes in the middle of a service message would
            // be noise inside the values we read.
            "--colors=never".to_string(),
        ];
        args.extend(scope.args());
        Command { program: self.binary.clone(), args, root: self.root.clone() }
    }
}

/// A command to run, and the text of it.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Command {
    pub program: PathBuf,
    pub args: Vec<String>,
    pub root: PathBuf,
}

impl Command {
    /// The command as a human would type it, relative to the project root.
    ///
    /// Shows `./vendor/bin/pest …` rather than the absolute path, because that is the line
    /// a user can paste into a terminal in that folder and get the same result.
    pub fn display(&self) -> String {
        let program = self
            .program
            .strip_prefix(&self.root)
            .map(|relative| format!("./{}", relative.display()))
            .unwrap_or_else(|_| self.program.display().to_string());
        let mut out = program;
        for arg in &self.args {
            out.push(' ');
            out.push_str(arg);
        }
        out
    }
}

/// Finds the test runner in `root`, if there is one.
///
/// `None` means "this project has no test runner", which is a perfectly ordinary thing for
/// a folder to be and is reported by showing nothing at all.
pub fn detect(root: &Path) -> Option<Runner> {
    for framework in [Framework::Pest, Framework::PhpUnit] {
        let binary = root.join(framework.binary());
        // The file has to actually be there. `composer.json` mentioning pest proves only
        // that someone intends to install it — `composer install` may never have run, and
        // spawning a binary that does not exist is the error this check exists to avoid.
        if binary.is_file() {
            return Some(Runner { framework, binary, root: root.to_path_buf() });
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn project(binaries: &[&str]) -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("a tempdir");
        for binary in binaries {
            let path = dir.path().join(binary);
            fs::create_dir_all(path.parent().expect("a parent")).expect("creating vendor/bin");
            fs::write(&path, "#!/bin/sh\n").expect("writing a fake binary");
        }
        dir
    }

    /// §24. The common case: a folder that is not a PHP project at all. No panic, no error,
    /// nothing to report.
    #[test]
    fn a_project_with_no_test_framework_detects_nothing() {
        let dir = project(&[]);
        assert_eq!(detect(dir.path()), None);

        // Including a folder that does not exist, which is what an unopened workspace is.
        assert_eq!(detect(&dir.path().join("nope")), None);
    }

    /// A `composer.json` asking for Pest is an intention, not an installation. Detecting on
    /// it would spawn a binary that is not there on every project that never ran
    /// `composer install` — the exact state `/tmp/laravel-teste` is in.
    #[test]
    fn a_declared_but_uninstalled_framework_detects_nothing() {
        let dir = project(&[]);
        fs::write(dir.path().join("composer.json"), r#"{"require-dev":{"pestphp/pest":"^3.0"}}"#)
            .expect("writing composer.json");

        assert_eq!(detect(dir.path()), None);
    }

    /// Verified against a real install: `composer require pestphp/pest` puts *both*
    /// binaries in `vendor/bin`, and running `phpunit` in that project is an error telling
    /// you to run `pest`. So finding phpunit first would break every Pest project.
    #[test]
    fn pest_wins_when_both_binaries_are_installed() {
        let dir = project(&["vendor/bin/pest", "vendor/bin/phpunit"]);
        let runner = detect(dir.path()).expect("a runner");
        assert_eq!(runner.framework, Framework::Pest);

        let only_phpunit = project(&["vendor/bin/phpunit"]);
        assert_eq!(detect(only_phpunit.path()).expect("a runner").framework, Framework::PhpUnit);
    }

    /// #23: the command shown is the command run.
    #[test]
    fn the_displayed_command_is_the_one_that_runs() {
        let dir = project(&["vendor/bin/pest"]);
        let runner = detect(dir.path()).expect("a runner");

        let all = runner.command(&Scope::All);
        assert_eq!(all.display(), "./vendor/bin/pest --teamcity --colors=never");
        assert_eq!(all.program, dir.path().join("vendor/bin/pest"));

        let file = runner.command(&Scope::File("tests/Unit/ExampleTest.php".into()));
        assert_eq!(
            file.display(),
            "./vendor/bin/pest --teamcity --colors=never tests/Unit/ExampleTest.php"
        );
    }

    /// Both runners treat `--filter` as a regex. A test name is not one, and a name
    /// containing `+` or `(` would otherwise select the wrong tests or none at all.
    ///
    /// The anchoring is end-only, and that is measured, not stylistic: Pest 3.8.7 filters
    /// on a class-qualified name, so a leading `^` matches nothing and "run this one test"
    /// quietly runs zero. Verified against the real binary.
    #[test]
    fn a_test_name_is_quoted_into_the_filter_rather_than_pasted() {
        let dir = project(&["vendor/bin/pest"]);
        let runner = detect(dir.path()).expect("a runner");

        let scoped = runner.command(&Scope::Name("it adds 1 + 1 (twice)".to_string()));
        assert_eq!(
            scoped.display(),
            "./vendor/bin/pest --teamcity --colors=never --filter=it adds 1 \\+ 1 \\(twice\\)$"
        );
    }

    /// Rerunning failures is one command, not one per test — a suite pays its bootstrap
    /// cost every invocation.
    #[test]
    fn rerunning_failures_selects_them_in_a_single_alternated_filter() {
        let dir = project(&["vendor/bin/pest"]);
        let runner = detect(dir.path()).expect("a runner");

        let scope = Scope::Names(vec!["it fails".to_string(), "adds numbers".to_string()]);
        assert_eq!(
            runner.command(&scope).display(),
            "./vendor/bin/pest --teamcity --colors=never --filter=(it fails|adds numbers)$"
        );
    }

    /// The trap in the rerun path: an empty alternation is `--filter=$`, which matches
    /// *every* test. "Rerun the nothing that failed" must not run the whole suite.
    #[test]
    fn rerunning_an_empty_set_of_failures_selects_nothing_rather_than_everything() {
        let dir = project(&["vendor/bin/pest"]);
        let runner = detect(dir.path()).expect("a runner");

        let scope = Scope::Names(Vec::new());
        assert!(scope.is_empty(), "the caller must be able to see there is nothing to run");
        // And if it were run anyway, it carries no filter that could match everything.
        let command = runner.command(&scope);
        assert!(!command.args.iter().any(|arg| arg.starts_with("--filter")), "{:?}", command.args);
    }
}
