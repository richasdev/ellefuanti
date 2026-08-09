//! ellefuanti — a native IDE for PHP, Laravel, Livewire and Blade.

mod actions;
mod editor;
mod palette;
mod theme;
mod workspace_view;

use std::sync::Arc;

use gpui::{
    App, AppContext as _, Application, Bounds, KeyBinding, WindowBounds, WindowOptions, px, size,
};

use crate::actions::Quit;
use crate::theme::Theme;
use crate::workspace_view::WorkspaceView;

fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "ellefuanti=info".into()),
        )
        .init();

    Application::new().run(|cx: &mut App| {
        let registry = Arc::new(actions::init(cx));

        // Quit has no key context, so it works before anything has focus.
        cx.bind_keys([KeyBinding::new("cmd-q", Quit, None)]);
        cx.on_action(|_: &Quit, cx: &mut App| cx.quit());

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

        // Touch the theme once at startup so a bad colour or font choice fails loudly here
        // rather than on the first frame.
        let _ = Theme::dark();

        cx.activate(true);
        tracing::info!("ellefuanti {} ready", env!("CARGO_PKG_VERSION"));
    });
}
