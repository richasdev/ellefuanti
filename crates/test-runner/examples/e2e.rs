//! Manual end-to-end check against a real Pest or PHPUnit install.
//!
//! The unit tests deliberately need no PHP; this is how the crate was verified against the
//! real thing. Point it at a project that has `vendor/bin/pest`:
//!
//! ```sh
//! cargo run -p elle-test-runner --example e2e -- /path/to/a/laravel/project
//! ```

use std::path::Path;

use elle_test_runner::{CancelFlag, Outcome, Report, Scope, detect, run};

fn main() {
    let Some(root) = std::env::args().nth(1) else {
        eprintln!("usage: e2e <project-root>");
        std::process::exit(2);
    };
    let root = Path::new(&root);

    let Some(runner) = detect(root) else {
        // The §24 path: a project with no test framework, which is not an error.
        println!("no test framework in {}", root.display());
        return;
    };
    println!("framework: {}", runner.framework.label());

    let command = runner.command(&Scope::All);
    println!("command:   {}", command.display());

    let mut report = Report::new();
    let outcome = run(&command, &CancelFlag::new(), |event| report.push(event)).expect("a run");
    println!("outcome:   {outcome:?}");
    println!("summary:   {}", report.counts().summary());
    for test in &report.tests {
        let where_ = test
            .location
            .as_ref()
            .map(|location| format!("  ({}:{})", location.path, location.line))
            .unwrap_or_default();
        println!("  {} {}{where_}", test.status.glyph(), test.name);
    }
    println!("unparsed lines: {}", report.output.len());

    // Cancelling a real run, from another thread, while it is in flight.
    let cancel = CancelFlag::new();
    let ticker = cancel.clone();
    std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(20));
        ticker.cancel();
    });
    let started = std::time::Instant::now();
    let cancelled = run(&runner.command(&Scope::All), &cancel, |_| {}).expect("a run");
    println!("\ncancelled: {cancelled:?} after {:?}", started.elapsed());
    assert_eq!(cancelled, Outcome::Cancelled);
}
