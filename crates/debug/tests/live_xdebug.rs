//! The test the fixtures cannot be: a real PHP process, with a real Xdebug, over a real
//! socket.
//!
//! Everything else in this crate is checked against packets captured from Xdebug's own
//! `.phpt` suite. That pins the *parsing* — but not whether Xdebug accepts the exact command
//! strings this crate emits, and not whether the connection direction is right. Those are
//! precisely the parts a fixture cannot check, because a fixture is a recording of somebody
//! else's conversation.
//!
//! Two things here were unverifiable until this file existed, and both are now covered:
//! whether the engine accepts our commands, and the shape of `stack_get`'s response — for
//! which no fixture existed anywhere in Xdebug's suite, so its parsing was written from the
//! specification alone.
//!
//! # Skips itself rather than failing when PHP is absent
//!
//! CI runners have no PHP with Xdebug, and a test that fails there would be switched off
//! within a week — costing more than it buys. So the whole thing is gated on finding a
//! working `php` with the extension loaded, and prints why it skipped: a skip that looks
//! like a pass is how a suite quietly stops testing something.
//!
//! # The direction is the whole point
//!
//! Xdebug is not spawned and talked to over stdio like a language server. **PHP dials out to
//! a port we listen on.** So this binds first, then runs PHP, then accepts. Getting that
//! backwards is the failure this file rules out, and no amount of offline parsing could.

use std::process::{Child, Command, Stdio};
use std::time::Duration;

use elle_debug::{Listener, Status};

/// A `php` that actually has Xdebug loaded, or `None`.
///
/// `php -m` rather than trusting `which php`: a PHP without the extension dials nothing, and
/// the test would then hang on `accept` rather than say why.
fn php_has_xdebug() -> bool {
    Command::new("php")
        .arg("-m")
        .output()
        .map(|out| String::from_utf8_lossy(&out.stdout).to_lowercase().contains("xdebug"))
        .unwrap_or(false)
}

/// Runs the probe script with Xdebug pointed at `port`.
///
/// `start_with_request=yes`, not the `trigger` a developer machine often defaults to: a
/// trigger needs an environment variable or cookie the CLI has no reason to carry, and "the
/// test hung" is a poor way to discover that.
fn spawn_php(script: &std::path::Path, port: u16) -> Child {
    Command::new("php")
        .args(["-d", "xdebug.mode=debug"])
        .args(["-d", "xdebug.start_with_request=yes"])
        .args(["-d", &format!("xdebug.client_port={port}")])
        .args(["-d", "xdebug.client_host=127.0.0.1"])
        // Both emptied because a machine-wide prepend runs *before* our script and is what
        // the engine then names in <init> — on this machine Laravel Herd injects
        // `valet/dump-loader.php`, and the test read that instead of the probe. Not a
        // protocol subtlety: just another PHP file, arriving first.
        .args(["-d", "auto_prepend_file="])
        .args(["-d", "auto_append_file="])
        .arg(script)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn php")
}

#[test]
fn a_real_xdebug_stops_on_a_breakpoint_and_reports_its_stack() {
    if !php_has_xdebug() {
        eprintln!("SKIP: no php with xdebug on PATH — this test needs a real engine");
        return;
    }

    // Port 0 lets the OS pick a free one, so a developer already debugging on 9003 does not
    // collide with the test suite.
    let listener = Listener::bind(0).expect("bind a listening port");
    let port = listener.port().expect("read back the bound port");

    let script = std::env::temp_dir().join(format!("elle_dbgp_probe_{port}.php"));
    std::fs::write(&script, "<?php\n$greeting = 'hello';\n$n = 41 + 1;\necho $greeting;\n")
        .expect("write the probe script");

    let mut child = spawn_php(&script, port);

    let mut session = listener
        .accept(Duration::from_secs(10))
        .expect("accept without erroring")
        .expect("xdebug should dial in within ten seconds");

    // The engine speaks first, unprompted: <init> names the file it is about to run.
    assert!(
        session.init().file_uri.contains("elle_dbgp_probe"),
        "init should name our script, got {:?}",
        session.init().file_uri
    );

    // A line breakpoint on the `echo`. Xdebug wants a file:// URI, and getting that wrong is
    // a silent no-op — the run would sail past and the status below would read `Stopping`.
    let uri = format!("file://{}", script.display());
    session.set_breakpoint(&uri, 4).expect("the engine should accept our breakpoint command");

    let stop = session.run().expect("run to the breakpoint");
    assert_eq!(
        stop.status,
        Status::Break,
        "should stop at the breakpoint rather than run to completion: {stop:?}"
    );

    // `stack_get` is the one command whose response shape was written from the spec, because
    // no captured fixture of it exists in Xdebug's own suite. This is its first contact with
    // reality.
    let stack = session.stack().expect("read the stack");
    assert!(!stack.is_empty(), "a stopped engine has at least one frame");
    assert_eq!(stack[0].line, 4, "the top frame sits on the breakpoint's line: {stack:?}");

    // Locals at the top frame: both variables assigned before line 4 should be visible.
    let locals = session.locals(0).expect("read the locals");
    let names: Vec<&str> = locals.iter().map(|p| p.name.as_str()).collect();
    assert!(
        names.contains(&"$greeting") || names.contains(&"greeting"),
        "the assigned variable should be in scope, got {names:?}"
    );

    // Kill rather than `run()` then `wait()`. Letting it continue looks tidier and hangs:
    // the engine answers the final `run` only after the script finishes writing to a stdout
    // this test has pointed at /dev/null, and `READ_TIMEOUT` is five minutes. A debugger's
    // patience is right for a human at a breakpoint and wrong for a test.
    let _ = child.kill();
    let _ = child.wait();
    let _ = std::fs::remove_file(&script);
}
