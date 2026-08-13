//! A real plugin process, discovered and driven end to end.
//!
//! The unit tests drive [`elle_plugin::Session`] over in-memory streams, which proves the
//! protocol logic but never spawns anything. This proves the other half: that a directory
//! on disk becomes a running child process, answers a command over real OS pipes, and is
//! cleaned up afterwards.
//!
//! The plugin is a small Python script written by the test, because `python3` ships with
//! macOS and writing one in Rust would mean building a second binary to test the first.
//! A machine without `python3` skips rather than fails — the boundary being tested is the
//! editor's, and an absent interpreter is not a defect in it.

use std::io::BufReader;
use std::path::{Path, PathBuf};

use elle_plugin::{PLUGIN_API_VERSION, Session, discover};

/// The plugin: answers the handshake, echoes command ids back, and fails on demand.
const PLUGIN_SOURCE: &str = r#"#!/usr/bin/env python3
import json, sys
for raw in sys.stdin:
    raw = raw.strip()
    if not raw:
        continue
    try:
        message = json.loads(raw)
    except ValueError:
        continue
    method = message.get("method")
    if method == "initialize":
        # A log line before the reply: the host must skip it, not choke on it.
        sys.stdout.write(json.dumps({"jsonrpc":"2.0","method":"log","params":{"message":"warming up"}}) + "\n")
        sys.stdout.write("a stray debug print\n")
        sys.stdout.write(json.dumps({"jsonrpc":"2.0","id":message["id"],"result":{"ok":True}}) + "\n")
        sys.stdout.flush()
    elif method == "command/invoke":
        cid = message["params"]["id"]
        if cid == "echo.fail":
            body = {"jsonrpc":"2.0","id":message["id"],"error":{"code":-32000,"message":"asked to fail"}}
        elif cid == "echo.quiet":
            body = {"jsonrpc":"2.0","id":message["id"],"result":{}}
        else:
            body = {"jsonrpc":"2.0","id":message["id"],"result":{"message":"ran " + cid}}
        sys.stdout.write(json.dumps(body) + "\n")
        sys.stdout.flush()
    elif method == "shutdown":
        break
"#;

const MANIFEST: &str = r#"{
  "api_version": 1,
  "name": "echo",
  "version": "0.1.0",
  "command": "./echo-plugin",
  "commands": [
    {"id": "echo.hello", "title": "Echo: Hello"},
    {"id": "echo.quiet", "title": "Echo: Quiet"},
    {"id": "echo.fail",  "title": "Echo: Fail On Purpose"}
  ]
}"#;

fn python3_available() -> bool {
    std::process::Command::new("python3")
        .arg("--version")
        .output()
        .is_ok_and(|output| output.status.success())
}

/// Writes the plugin into a fresh directory and returns the plugins root.
fn install_plugin(tag: &str) -> PathBuf {
    use std::sync::atomic::{AtomicU32, Ordering};
    static COUNTER: AtomicU32 = AtomicU32::new(0);
    let unique = COUNTER.fetch_add(1, Ordering::Relaxed);

    let root =
        std::env::temp_dir().join(format!("elle-plugin-e2e-{}-{tag}-{unique}", std::process::id()));
    let plugin = root.join("echo");
    std::fs::create_dir_all(&plugin).unwrap();
    std::fs::write(plugin.join("plugin.json"), MANIFEST).unwrap();

    let executable = plugin.join("echo-plugin");
    std::fs::write(&executable, PLUGIN_SOURCE).unwrap();
    make_executable(&executable);

    root
}

#[cfg(unix)]
fn make_executable(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
}

#[cfg(not(unix))]
fn make_executable(_path: &Path) {}

/// Spawns the discovered plugin and runs one command, exactly as the app does.
fn run_command(root: &Path, command_id: &str) -> anyhow::Result<Option<String>> {
    let discovery = discover(root);
    assert_eq!(discovery.plugins.len(), 1, "{discovery:?}");
    let plugin = &discovery.plugins[0];

    let (mut process, pipes) = elle_plugin::spawn(plugin)?;
    let mut stdin = pipes.stdin;
    let outcome = {
        let mut session = Session::new(BufReader::new(pipes.stdout), &mut stdin);
        session.initialize(PLUGIN_API_VERSION, "0.4.0").and_then(|()| session.invoke(command_id))
    };
    elle_plugin::host::shutdown(&mut process, &mut stdin);
    outcome
}

#[test]
fn a_real_plugin_is_discovered_spawned_and_answers_a_command() {
    if !python3_available() {
        eprintln!("skipping: python3 is not installed");
        return;
    }
    let root = install_plugin("ok");

    let message = run_command(&root, "echo.hello").unwrap();
    assert_eq!(message, Some("ran echo.hello".to_string()));

    std::fs::remove_dir_all(&root).ok();
}

#[test]
fn a_real_plugin_may_answer_a_command_with_nothing_to_say() {
    if !python3_available() {
        eprintln!("skipping: python3 is not installed");
        return;
    }
    let root = install_plugin("quiet");

    assert_eq!(run_command(&root, "echo.quiet").unwrap(), None);

    std::fs::remove_dir_all(&root).ok();
}

#[test]
fn a_real_plugins_failure_reaches_the_caller_as_an_error() {
    if !python3_available() {
        eprintln!("skipping: python3 is not installed");
        return;
    }
    let root = install_plugin("fail");

    let error = run_command(&root, "echo.fail").unwrap_err().to_string();
    assert!(error.contains("asked to fail"), "{error}");

    std::fs::remove_dir_all(&root).ok();
}

#[test]
fn a_real_plugin_that_dies_immediately_does_not_hang_the_caller() {
    // §24 and ADR-0012's whole premise: the editor survives a plugin that crashes. `false`
    // exits non-zero at once, so the handshake meets EOF rather than an answer.
    let root = install_plugin("dead");
    std::fs::write(
        root.join("echo").join("plugin.json"),
        MANIFEST.replace("./echo-plugin", "false"),
    )
    .unwrap();

    let error = run_command(&root, "echo.hello").unwrap_err().to_string();
    assert!(error.contains("exited before answering"), "{error}");

    std::fs::remove_dir_all(&root).ok();
}
