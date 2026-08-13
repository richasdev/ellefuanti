//! ellefuanti — a native IDE for PHP, Laravel, Livewire and Blade.

mod actions;
mod ai;
mod ai_chat;
mod ai_codex;
mod artisan;
mod completion;
mod context_menu;
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
    let raw = std::env::args().nth(1)?;
    // macOS hands a `.app` launched by the Finder a `-psn_0_12345` process-serial argument.
    // Treating that as a path would make every Finder launch report a missing file.
    if raw.starts_with('-') {
        return None;
    }
    Some(std::path::PathBuf::from(raw))
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

fn main() {
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
