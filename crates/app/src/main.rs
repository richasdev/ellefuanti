//! ellefuanti — a native IDE for PHP, Laravel, Livewire and Blade.

mod actions;
mod editor;
mod palette;
mod perf;
#[cfg(test)]
mod render_tests;
mod terminal_view;
mod theme;
mod workspace_view;

use std::sync::Arc;

use gpui::{
    App, AppContext as _, Application, Bounds, KeyBinding, WindowBounds, WindowOptions, px, size,
};

use crate::actions::Quit;
use crate::workspace_view::WorkspaceView;

fn main() {
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
    let app = Application::new();
    startup.phase("platform_init");

    app.run(move |cx: &mut App| {
        startup.phase("event_loop_start");

        let registry = Arc::new(actions::init(cx));

        // Quit has no key context, so it works before anything has focus.
        cx.bind_keys([KeyBinding::new("cmd-q", Quit, None)]);
        cx.on_action(|_: &Quit, cx: &mut App| cx.quit());
        startup.phase("keymap");

        let bounds = Bounds::centered(None, size(px(1180.0), px(760.0)), cx);
        let window = cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                titlebar: Some(gpui::TitlebarOptions {
                    title: Some("ellefuanti".into()),
                    appears_transparent: false,
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

        // A missing font family does NOT error in gpui: text layout falls back to a
        // proportional font and every column calculation in the editor silently goes
        // wrong, which presents as a layout bug rather than a missing font. Check once,
        // loudly, at startup.
        if !cx.text_system().all_font_names().iter().any(|name| name == editor::FONT_FAMILY) {
            tracing::error!(
                font = editor::FONT_FAMILY,
                "monospace font not found; text will fall back to a proportional font and \
                 column positions will be wrong"
            );
        }

        cx.activate(true);
        startup.phase("window");
        startup.ready();
    });
}
