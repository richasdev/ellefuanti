//! ellefuanti — a native IDE for PHP, Laravel, Livewire and Blade.

mod actions;
mod ai;
mod ai_chat;
mod ai_codex;
mod artisan;
mod completion;
mod context_menu;
mod debug_session;
mod debug_view;
mod editor;
mod file_cache;
mod file_icons;
mod find_bar;
mod fonts;
mod git_panel;
mod icons;
mod log_view;
mod lsp_session;
mod menu;
mod palette;
mod perf;
mod plugin_host;
mod preview;
mod preview_view;
mod preview_webview;
#[cfg(test)]
mod render_tests;
mod search_panel;
mod settings;
mod settings_panel;
mod terminal_view;
mod test_view;
mod theme;
mod themes;
mod tooltip;
mod update;
mod workspace_view;

use std::sync::Arc;

use gpui::{
    App, AppContext as _, Application, Bounds, KeyBinding, WindowBounds, WindowOptions, px, size,
};

use crate::actions::Quit;
use crate::workspace_view::WorkspaceView;

/// What to open at launch, from the command line.
///
/// # Why this exists
///
/// `ellefuanti .` is how anyone who lives in a terminal opens an editor, and it is the only
/// launch that inherits the shell's `PATH` — which for a long time was the *sole* way to get
/// a working language server (#123, now fixed on both paths). Without it the app could only
/// be started by double-clicking and then reaching for ⌘O, which is also why every log
/// captured while diagnosing #125 was from a window that had never opened a folder.
///
/// A directory becomes the project root; a file is opened in a tab, and #125's
/// `start_lsp_for_file` gives it a server rooted at its nearest `composer.json`. Anything
/// that is neither is reported rather than ignored: a typo'd path that silently opens an
/// empty window is worse than an error.
///
/// ponytail: `std::env::args` rather than a parser. There is one optional positional
/// argument and no flags. Reach for `clap` at the second one.
fn path_argument() -> Option<std::path::PathBuf> {
    // The first argument that is not a flag. `nth(1)` broke the moment `-w` existed:
    // `ellefuanti -w .` put the path second, and the old read found the flag, skipped it
    // as a `-psn`-style non-path, and opened an empty window.
    std::env::args()
        .skip(1)
        .find(|raw| !raw.starts_with('-'))
        .map(std::path::PathBuf::from)
}

/// `-w` / `--wait`: hold the terminal until the window closes, vim's working rhythm.
///
/// # What this is and is not
///
/// It is the *workflow* of a terminal editor — `ellefuanti -w file.php` blocks the prompt,
/// you edit, you close, the prompt returns — which is what `git config core.editor` and
/// every "wait for the editor" integration needs. It is **not** a terminal UI: gpui draws
/// with Metal into a window, and a curses ellefuanti would be a second renderer, not a
/// flag. Stated here because "abre no terminal como o vim" reads as both, and only one is
/// buildable.
fn wait_flag() -> bool {
    std::env::args().skip(1).any(|arg| arg == "-w" || arg == "--wait")
}

/// Hands the terminal back: `ellefuanti .` must return the prompt, not squat on it.
///
/// The owner's report, verbatim: "pq quando dou um ellefuanti . fica um server aí
/// rodando?" — a GUI binary in the foreground reads as a stuck dev-server, and `code .`
/// taught everyone the prompt comes back. The process re-spawns itself detached (its own
/// process group, streams to null) and the parent exits before gpui ever initialises.
///
/// Two escapes, both deliberate:
/// - stdout not a TTY → stay in the foreground. Every debugging flow in this repo runs
///   `ellefuanti . > log 2>&1 &`, and detaching there would sever the log redirection
///   that five rounds of #125 depended on.
/// - `ELLE_FOREGROUND=1` → stay put. Set by the respawn to stop the loop, and available
///   to anyone who wants the blocking behaviour on purpose.
///
/// The LSP child needs no handling here: it exits on its stdin pipe closing, verified
/// under SIGTERM and SIGKILL both — there is no orphan, only a borrowed prompt.
fn detach_from_terminal() {
    use std::io::IsTerminal;
    // `-w` is the user asking for vim's rhythm: the prompt stays captive until the window
    // closes, so detaching would defeat the flag's whole point.
    if wait_flag() {
        return;
    }
    if std::env::var_os("ELLE_FOREGROUND").is_some() || !std::io::stdout().is_terminal() {
        return;
    }
    let Ok(exe) = std::env::current_exe() else { return };

    // Re-exec, not an in-process fork — and both failed attempts are worth their lines:
    //
    // - In-process fork dies on macOS regardless of signals: the Objective-C runtime is
    //   initialised by static constructors *before* `main`, and a forked child that then
    //   touches Cocoa (gpui's first window) aborts with objc's fork-safety check. GUI
    //   daemonisation on this platform means a fresh process.
    // - Plain re-exec loses a race instead: the parent exits, the pty tears down, and
    //   SIGHUP reaches the child before its `setsid` (in pre_exec) has run. Measured
    //   with a pty harness — the "detached" app died at 0ms, twice, two different ways.
    //
    // The fix is one line in the *parent*: ignore SIGHUP here, because signal
    // dispositions inherit through fork **and exec** — the child is born immune, so the
    // race stops mattering, and `setsid` then detaches it from the terminal for good.
    // A GUI app has no use for SIGHUP, so the disposition staying ignored costs nothing.
    unsafe {
        libc::signal(libc::SIGHUP, libc::SIG_IGN);
    }

    use std::os::unix::process::CommandExt;
    let mut command = std::process::Command::new(exe);
    command
        .args(std::env::args_os().skip(1))
        .env("ELLE_FOREGROUND", "1")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }

    match command.spawn() {
        // The child owns the window; this process's one remaining job is the prompt.
        Ok(_) => std::process::exit(0),
        Err(err) => eprintln!("ellefuanti: could not detach ({err}); running in foreground"),
    }
}

/// Formats one crash report. Split from the hook so it can be tested without panicking.
///
/// Deliberately not `Display` on a struct: there is one caller and one format, and the
/// thing that matters is that the text answers the two questions asked of a crash report —
/// where it broke and which build it was.
fn crash_report(
    payload: &str,
    location: Option<String>,
    thread: &str,
    when: std::time::SystemTime,
) -> String {
    let secs = when
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    format!(
        "\n===== ellefuanti panic =====\n\
         when:     {secs} (unix seconds)\n\
         version:  {}\n\
         thread:   {thread}\n\
         location: {}\n\
         message:  {payload}\n",
        env!("CARGO_PKG_VERSION"),
        location.as_deref().unwrap_or("unknown"),
    )
}

/// Sends panics to a file, because nothing else can see them.
///
/// # Why this exists
///
/// The owner's report was "crasha do nada" — crashes out of nowhere. There was no
/// "nowhere": `detach_from_terminal` re-execs with `.stderr(Stdio::null())` so closing the
/// terminal cannot kill the window, and `main` installed no panic hook. Every panic in a
/// gpui event handler therefore took the window down and wrote its message, its location
/// and its backtrace straight to `/dev/null`. Months of crash reports were unactionable
/// not because the panics were mysterious but because the evidence was discarded by
/// design.
///
/// The default hook is kept and called after ours: when someone runs in the foreground
/// (`ELLE_FOREGROUND=1`, or piped to a log, which is how #125 was debugged) stderr is a
/// real stream and the familiar message should still appear there.
///
/// Appends rather than truncates. The interesting crash is often the *second* one — the
/// first is the trigger, the second is what the broken state does next — and a log that
/// keeps only the newest report throws away the pair. It is one file with no rotation:
/// panics are rare enough that a size limit would be a guess at a problem nobody has.
///
/// ponytail: `RUST_BACKTRACE` is not forced on. A backtrace here costs a symbolised
/// unwind on a process that is already dying, and the location line has been enough for
/// every panic in this repo so far. Set it in the environment when it is not.
fn install_panic_logger() {
    let Some(path) = elle_settings::crash_log_path() else { return };
    let previous = std::panic::take_hook();

    std::panic::set_hook(Box::new(move |info| {
        // Both shapes `panic!` produces. Anything else is a payload no formatter can read.
        let payload = info
            .payload()
            .downcast_ref::<&str>()
            .map(|s| (*s).to_string())
            .or_else(|| info.payload().downcast_ref::<String>().cloned())
            .unwrap_or_else(|| "<non-string panic payload>".to_string());

        let report = crash_report(
            &payload,
            info.location().map(|l| format!("{}:{}:{}", l.file(), l.line(), l.column())),
            std::thread::current().name().unwrap_or("unnamed"),
            std::time::SystemTime::now(),
        );

        // Best-effort by design: this runs while the process is dying, and a hook that
        // panics on its own error would replace a readable crash with an abort. A missing
        // parent directory is the normal case on a first run.
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if let Ok(mut file) =
            std::fs::OpenOptions::new().create(true).append(true).open(&path)
        {
            use std::io::Write as _;
            let _ = file.write_all(report.as_bytes());
            let _ = file.flush();
        }

        previous(info);
    }));
}

/// `--version`/`-v`: answer and exit, before any GUI exists.
///
/// # Why a GUI app needs this at all
///
/// The terminal is a supported way to run this editor (`ellefuanti .`), and the first
/// thing anyone checks in a CLI is which version answered. Without this, `ellefuanti
/// --version` fell through `path_argument`'s leading-dash skip and **launched the whole
/// app** — and because stdout was a pipe rather than a TTY, `detach_from_terminal`
/// stayed in the foreground and the caller's script hung on a GUI it never wanted.
/// Found by running exactly that probe.
///
/// Only version, deliberately: `--help` would promise a CLI surface this binary does not
/// have. The one positional argument is documented where it is parsed.
fn answer_version_and_exit_if_asked() {
    let Some(flag) = std::env::args().nth(1) else { return };
    if flag == "--version" || flag == "-v" {
        println!("ellefuanti {}", env!("CARGO_PKG_VERSION"));
        std::process::exit(0);
    }
}

fn main() {
    // Before `detach_from_terminal`, so a panic in the detach path is captured too — that
    // code re-execs and touches libc, and it is the one place that runs before there is
    // any other way to see a failure.
    install_panic_logger();

    answer_version_and_exit_if_asked();

    detach_from_terminal();

    // First statement, so the startup clock includes everything — including the runtime
    // Metal shader compilation that `runtime_shaders` moves from build time to startup.
    let mut startup = perf::Startup::begin();

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "ellefuanti=info".into()),
        )
        .init();
    startup.phase("logging");

    // Split so the phase names mean what they say. `Application::new()` constructs the
    // platform (on macOS: NSApplication, the Metal device, the text system), while `run`
    // starts the event loop and calls back. Measuring only inside the closure attributed
    // both to one "gpui_init" bucket, which is the sort of label that sends someone
    // optimising the wrong half.
    // The asset source has to be installed here, before any window exists: `svg()` resolves
    // its path through whatever source the Application was built with, and the default
    // `AssetSource for ()` returns None for everything — which paints nothing at all rather
    // than failing, so forgetting this looks like "the icons don't work".
    let app = Application::new().with_assets(icons::Icons);
    startup.phase("platform_init");

    app.run(move |cx: &mut App| {
        startup.phase("event_loop_start");

        // Before any view exists, because `cx.theme()` panics without it and the first
        // render happens inside `open_window` below. Reads settings.json and applies what
        // it finds — a missing or unreadable file is a default theme and a log line, never
        // a failure to launch (#60).
        //
        // This also resolves the font (#49), which is why it has to happen before the
        // window rather than alongside it: the family is chosen by measuring real glyph
        // advances through `cx.text_system()`, and the first frame renders with whatever
        // this decides. The startup warning that used to live further down is now part of
        // that resolution — see `fonts::usable`.
        settings::load_and_apply(cx);
        startup.phase("settings");

        let registry = Arc::new(actions::init(cx));

        // Quit has no key context, so it works before anything has focus.
        cx.bind_keys([KeyBinding::new("cmd-q", Quit, None)]);
        cx.on_action(|_: &Quit, cx: &mut App| cx.quit());
        startup.phase("keymap");

        // After the keymap, not before: gpui reads the bindings to draw the ⌘S beside
        // "Save", so a menu installed first renders without any shortcuts at all.
        menu::init(cx);
        startup.phase("menus");

        let bounds = Bounds::centered(None, size(px(1180.0), px(760.0)), cx);
        let window = cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                titlebar: Some(gpui::TitlebarOptions {
                    title: Some("ellefuanti".into()),
                    // Transparent, so the bar is the theme's colour instead of the
                    // system's — a white strip over a dark theme was the owner's
                    // screenshot. The native title text goes with it; the tab bar
                    // already names the file, and it pads left to clear the traffic
                    // lights (see `render_tab_bar`).
                    appears_transparent: true,
                    ..Default::default()
                }),
                ..Default::default()
            },
            |_window, cx| cx.new(|cx| WorkspaceView::new(registry.clone(), cx)),
        );

        let window = match window {
            Ok(window) => window,
            Err(err) => {
                tracing::error!("could not open a window: {err:#}");
                cx.quit();
                return;
            }
        };

        // Focus is not cosmetic: a context-scoped keybinding does not fire unless the
        // element carrying its key_context is actually focused.
        let focused = window.update(cx, |view, window, cx| {
            window.focus(&gpui::Focusable::focus_handle(view, cx));
        });
        if let Err(err) = focused {
            tracing::error!("could not focus the workspace: {err:#}");
        }

        // After focus, because opening a file moves the keyboard into its editor (#95) and
        // doing that before the workspace has focus leaves it nowhere.
        if let Some(path) = path_argument() {
            // Relative paths are the point — `ellefuanti .` is the case this exists for —
            // and they resolve against the shell's working directory, which is already this
            // process's. Canonicalising here rather than later so the tree, the LSP root and
            // any error message all name the same thing.
            let path = path.canonicalize().unwrap_or(path);
            let opened = window.update(cx, |view, window, cx| {
                view.open_argument(path, window, cx);
            });
            if let Err(err) = opened {
                tracing::error!("could not open the path given on the command line: {err:#}");
            }
        }

        cx.activate(true);
        startup.phase("window");
        startup.ready();
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The report has to answer "where did it break" and "which build was it", because a
    /// crash log that says only "panicked" costs another round-trip with the person who
    /// hit it.
    #[test]
    fn a_crash_report_names_the_place_and_the_build() {
        let report = crash_report(
            "index out of bounds",
            Some("crates/app/src/workspace_view.rs:4210:9".to_string()),
            "main",
            std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_760_000_000),
        );

        assert!(report.contains("index out of bounds"), "{report}");
        assert!(report.contains("workspace_view.rs:4210:9"), "{report}");
        assert!(report.contains("main"), "{report}");
        assert!(report.contains(env!("CARGO_PKG_VERSION")), "{report}");
        assert!(report.contains("1760000000"), "{report}");
    }

    /// A panic with no location still has to produce a readable report — `panic_any` and
    /// panics from foreign frames both reach the hook that way, and a hook that assumed a
    /// location would itself panic while the process was already dying.
    #[test]
    fn a_report_without_a_location_is_still_readable() {
        let report = crash_report("boom", None, "unnamed", std::time::UNIX_EPOCH);
        assert!(report.contains("location: unknown"), "{report}");
        assert!(report.contains("boom"), "{report}");
    }

    /// The end-to-end property that matters: a real panic, caught, reaches the file.
    ///
    /// Written against a temp path rather than `install_panic_logger` itself because the
    /// panic hook is process-global — installing one here would swallow the reports of
    /// every other test in this binary and make their failures unreadable.
    #[test]
    fn the_hook_writes_a_real_panic_to_the_file() {
        let path = std::env::temp_dir().join(format!("elle-crash-test-{}.log", std::process::id()));
        let _ = std::fs::remove_file(&path);

        let sink = path.clone();
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            let payload = info
                .payload()
                .downcast_ref::<&str>()
                .map(|s| (*s).to_string())
                .unwrap_or_default();
            let report = crash_report(
                &payload,
                info.location().map(|l| format!("{}:{}:{}", l.file(), l.line(), l.column())),
                "test",
                std::time::UNIX_EPOCH,
            );
            use std::io::Write as _;
            let mut file =
                std::fs::OpenOptions::new().create(true).append(true).open(&sink).unwrap();
            file.write_all(report.as_bytes()).unwrap();
        }));

        let caught = std::panic::catch_unwind(|| panic!("a deliberate test panic"));
        std::panic::set_hook(previous);

        assert!(caught.is_err(), "the panic must actually have fired");
        let written = std::fs::read_to_string(&path).expect("the hook must have created the file");
        assert!(written.contains("a deliberate test panic"), "{written}");
        assert!(written.contains("main.rs:"), "the location must point at this file: {written}");

        let _ = std::fs::remove_file(&path);
    }
}

