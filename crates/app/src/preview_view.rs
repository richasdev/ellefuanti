//! The preview pane (#31): a toolbar GPUI draws, and a rectangle AppKit fills.
//!
//! The pane is two things stacked. The top strip — back, forward, reload, the address — is
//! ordinary GPUI, drawn like every other panel and themed like every other panel, because
//! ADR-0011 is explicit that no part of the IDE's chrome moves into HTML. Below it is a
//! rectangle that GPUI lays out and *deliberately leaves empty*, whose only job is to tell
//! [`PreviewWebView`] where to put itself.
//!
//! # Why the page area is a `canvas` that paints nothing
//!
//! The webview is an `NSView` sitting above GPUI's Metal view (see
//! [`crate::preview_webview`]); GPUI cannot clip it, and nothing in GPUI's layout will move
//! it. So the pane needs the one thing GPUI will tell it — the rect this element was
//! actually laid out at — and must hand that to AppKit on every painted frame. `canvas()`
//! is exactly that hook and nothing more: its paint callback receives the laid-out
//! `Bounds<Pixels>` and pushes them at the webview.
//!
//! Doing it every frame rather than on a resize event is the conservative choice, and cheap
//! — a `setFrame:` with an unchanged rect is a no-op inside AppKit. The alternative is
//! subscribing to window resizes and hoping the list of things that can move a pane is
//! complete; it is not (the sidebar drag, the terminal opening, zen mode). Paint is the one
//! moment the true rect is known, so paint is where it is read.
//!
//! # The webview is built on first open, and only then
//!
//! [`PreviewView::new`] constructs no webview. The first `paint` with a window handle does,
//! which is what makes ADR-0011's laziness real rather than nominal: opening a project,
//! editing files, and never touching the preview costs nothing, and the perf gates that
//! measure idle RSS never see WebKit at all.

use gpui::{
    App, Bounds, Context, ElementId, FocusHandle, Focusable, KeyDownEvent, MouseButton, Pixels,
    SharedString, Window, canvas, div, prelude::*,
};

use crate::actions::{Backspace, Cancel, Confirm, context};

use crate::preview::{DEFAULT_DEV_URL, History, normalize_url};
use crate::preview_webview::{PaneRect, PreviewWebView};
use crate::theme::{Theme, Themed};

pub struct PreviewView {
    /// What the address bar shows, which is not always what is loaded — the user can be
    /// halfway through typing a new one.
    address: String,
    history: History,
    /// `None` until the first paint builds it; see the module docs on laziness. Also `None`
    /// forever if the window turns out not to be the AppKit window we can host in, which
    /// [`PreviewView::unavailable`] reports honestly rather than hiding.
    webview: Option<PreviewWebView>,
    /// Set when hosting was attempted and refused, so the pane can say so instead of
    /// showing an empty rectangle that looks like a page that failed to load.
    host_failed: bool,
    /// Set while zen mode hides the pane. Kept as state rather than inferred at paint,
    /// because the whole problem is that paint does not run while it is true.
    zen_hidden: bool,
    focus_handle: FocusHandle,
}

impl PreviewView {
    /// Opens on Laravel's default dev-server address. It is a guess and
    /// [`DEFAULT_DEV_URL`] says so — nothing here detects a running server.
    pub fn new(cx: &mut Context<Self>) -> Self {
        let mut history = History::new();
        history.push(DEFAULT_DEV_URL);
        Self {
            address: DEFAULT_DEV_URL.to_string(),
            history,
            webview: None,
            host_failed: false,
            zen_hidden: false,
            focus_handle: cx.focus_handle(),
        }
    }

    /// True when the webview could not be hosted in this window at all.
    #[cfg(test)]
    pub fn unavailable(&self) -> bool {
        self.host_failed
    }

    #[cfg(test)]
    pub fn address_for_test(&self) -> &str {
        &self.address
    }

    #[cfg(test)]
    pub fn history_for_test(&self) -> &History {
        &self.history
    }

    #[cfg(test)]
    pub fn set_address_for_test(&mut self, address: &str) {
        self.address = address.to_string();
    }

    /// Loads whatever the address bar currently holds, if it names a destination.
    ///
    /// A refusal is silent and leaves the text alone: the user is looking at what they
    /// typed, and rewriting or clearing it would lose their work to tell them something
    /// they can see. Nothing navigates, which is the honest outcome.
    pub fn navigate_to_address(&mut self, cx: &mut Context<Self>) {
        let Some(url) = normalize_url(&self.address) else { return };
        self.history.push(url.clone());
        self.address = url.clone();
        if let Some(webview) = &self.webview {
            webview.load(&url);
        }
        cx.notify();
    }

    /// Back and forward ask WebKit, not [`History`] — see
    /// [`PreviewWebView::go_back`] for why its list is the authoritative one.
    fn go_back(&mut self, cx: &mut Context<Self>) {
        if let Some(webview) = &self.webview {
            webview.go_back();
        }
        if let Some(url) = self.history.go_back() {
            self.address = url.to_string();
        }
        cx.notify();
    }

    fn go_forward(&mut self, cx: &mut Context<Self>) {
        if let Some(webview) = &self.webview {
            webview.go_forward();
        }
        if let Some(url) = self.history.go_forward() {
            self.address = url.to_string();
        }
        cx.notify();
    }

    fn reload(&mut self, cx: &mut Context<Self>) {
        if let Some(webview) = &self.webview {
            webview.reload();
        }
        cx.notify();
    }

    /// Builds the webview if it does not exist yet, then parks it on `bounds`.
    ///
    /// Called from the page area's paint callback, which is the only place the real rect is
    /// known. The first call is also the lazy construction — and the load of the initial
    /// URL, because a webview that exists but was never told to load anything is a white
    /// rectangle that looks like a bug.
    fn place_webview(&mut self, rect: PaneRect, window: &mut Window) {
        if self.webview.is_none() && !self.host_failed {
            let Some(mtm) = objc2::MainThreadMarker::new() else { return };
            match PreviewWebView::new(window, mtm) {
                Some(webview) => {
                    if let Some(url) = self.history.current() {
                        webview.load(url);
                    }
                    self.webview = Some(webview);
                }
                None => self.host_failed = true,
            }
        }
        if let Some(webview) = &self.webview {
            webview.set_frame(rect);
            webview.set_hidden(self.zen_hidden);
        }
    }

    /// Hides or shows the native view when zen mode hides or shows the pane.
    ///
    /// Needed because hiding a GPUI panel means *not rendering it*, and this pane's webview
    /// is an AppKit view that GPUI neither owns nor clips: with the element gone, its paint
    /// never runs, so nothing would take the page off the screen. The entity survives — zen
    /// is a view state, not a close — so the page is still there when zen ends.
    pub fn set_zen_hidden(&mut self, hidden: bool) {
        self.zen_hidden = hidden;
        if let Some(webview) = &self.webview {
            webview.set_hidden(hidden);
        }
    }

    /// One typed character into the address bar.
    ///
    /// The same shape as [`crate::find_bar`]'s field: no selection model, no cursor to
    /// move — a URL is short and retyping it is cheaper than the machinery would be.
    fn on_key_down(&mut self, event: &KeyDownEvent, _window: &mut Window, cx: &mut Context<Self>) {
        let keystroke = &event.keystroke;
        // Cmd-V first, before the modifier guard drops every Cmd chord — pasting a URL in
        // here is the single most likely way this field is ever filled.
        if keystroke.modifiers.platform && keystroke.key == "v" {
            if let Some(pasted) = cx.read_from_clipboard().and_then(|item| item.text()) {
                let pasted = crate::actions::pasted_into_single_line(&pasted);
                if !pasted.is_empty() {
                    self.address.push_str(&pasted);
                    cx.notify();
                }
            }
            return;
        }
        if keystroke.modifiers.platform
            || keystroke.modifiers.control
            || keystroke.modifiers.function
        {
            return;
        }
        if let Some(typed) = keystroke.key_char.as_ref() {
            self.address.push_str(typed);
            cx.notify();
        }
    }

    fn backspace(&mut self, _: &Backspace, _window: &mut Window, cx: &mut Context<Self>) {
        self.address.pop();
        cx.notify();
    }

    /// Enter loads what was typed.
    fn confirm(&mut self, _: &Confirm, _window: &mut Window, cx: &mut Context<Self>) {
        self.navigate_to_address(cx);
    }

    /// Escape puts the bar back to the URL actually showing, which is the only text that
    /// can be restored without guessing — a half-typed address has no other home.
    fn cancel(&mut self, _: &Cancel, _window: &mut Window, cx: &mut Context<Self>) {
        if let Some(current) = self.history.current() {
            self.address = current.to_string();
        }
        cx.notify();
    }

    fn render_toolbar_button(
        &self,
        id: &'static str,
        glyph: &'static str,
        enabled: bool,
        theme: &Theme,
        entity: &gpui::Entity<Self>,
        action: impl Fn(&mut Self, &mut Context<Self>) + 'static,
    ) -> impl IntoElement {
        let entity = entity.clone();
        div()
            .id(ElementId::Name(id.into()))
            .px_2()
            .rounded_sm()
            .text_color(if enabled { theme.text } else { theme.text_muted })
            .when(enabled, |el| el.hover(|el| el.bg(theme.hover)))
            .child(SharedString::from(glyph))
            .on_mouse_down(MouseButton::Left, move |_ev, _window, cx| {
                if enabled {
                    entity.update(cx, |this, cx| action(this, cx));
                }
            })
    }
}

impl Focusable for PreviewView {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for PreviewView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let focused = self.focus_handle.is_focused(window);
        let can_go_back = self.webview.as_ref().is_some_and(PreviewWebView::can_go_back)
            || self.history.can_go_back();
        let can_go_forward = self.webview.as_ref().is_some_and(PreviewWebView::can_go_forward)
            || self.history.can_go_forward();
        let address = SharedString::from(self.address.clone());
        let host_failed = self.host_failed;
        let entity = cx.entity();

        div()
            .key_context(context::PREVIEW)
            .track_focus(&self.focus_handle(cx))
            .on_key_down(cx.listener(Self::on_key_down))
            .on_action(cx.listener(Self::backspace))
            .on_action(cx.listener(Self::confirm))
            .on_action(cx.listener(Self::cancel))
            .flex()
            .flex_col()
            .size_full()
            .bg(theme.panel)
            .child(
                // The chrome, drawn by GPUI like every other panel — ADR-0011's rule that
                // no part of the IDE's UI moves into HTML.
                div()
                    .flex()
                    .items_center()
                    .gap_1()
                    .px_2()
                    .py_1()
                    .border_b_1()
                    .border_color(theme.border)
                    .child(self.render_toolbar_button(
                        "preview-back",
                        "←",
                        can_go_back,
                        theme,
                        &entity,
                        |this, cx| this.go_back(cx),
                    ))
                    .child(self.render_toolbar_button(
                        "preview-forward",
                        "→",
                        can_go_forward,
                        theme,
                        &entity,
                        |this, cx| this.go_forward(cx),
                    ))
                    .child(self.render_toolbar_button(
                        "preview-reload",
                        "⟳",
                        true,
                        theme,
                        &entity,
                        |this, cx| this.reload(cx),
                    ))
                    .child(
                        // Clicking the bar focuses the pane, which is what routes keystrokes
                        // here — the pane has one field, so focus and "editing the address"
                        // are the same state and do not need to be tracked separately.
                        div()
                            .id(ElementId::Name("preview-address".into()))
                            .flex_1()
                            .px_2()
                            .rounded_sm()
                            .bg(theme.background)
                            .text_color(theme.text)
                            .when(focused, |el| el.border_1().border_color(theme.accent))
                            .child(address)
                            .on_mouse_down(MouseButton::Left, {
                                let focus_handle = self.focus_handle.clone();
                                move |_ev, window, _cx| window.focus(&focus_handle)
                            }),
                    ),
            )
            .child(if host_failed {
                // Said in words rather than left as a blank rectangle, which would read as
                // a page that failed to load rather than a pane that could not be built.
                div()
                    .flex_1()
                    .p_3()
                    .text_color(theme.text_muted)
                    .child("The preview pane could not be hosted in this window.")
                    .into_any_element()
            } else {
                // Painted by AppKit, not by GPUI: this element exists to be measured. Its
                // paint callback is the only place the true rect is known, so it is where
                // the webview is built and moved.
                canvas(
                    |_bounds, _window, _cx| (),
                    // `canvas` takes an owned `FnOnce`, so this goes through the entity
                    // rather than `cx.listener` (which hands out a `&Bounds` borrow).
                    move |bounds: Bounds<Pixels>, (), window: &mut Window, cx: &mut gpui::App| {
                        entity.update(cx, |this, _cx| {
                            this.place_webview(
                                PaneRect {
                                    x: f64::from(bounds.origin.x),
                                    y: f64::from(bounds.origin.y),
                                    width: f64::from(bounds.size.width),
                                    height: f64::from(bounds.size.height),
                                },
                                window,
                            );
                        });
                    },
                )
                .flex_1()
                .into_any_element()
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The view's own logic, exercised without a window. Anything below this line that
    /// needs a `WKWebView` is untestable here by construction — see the module docs.
    #[gpui::test]
    fn it_opens_on_the_laravel_default(cx: &mut gpui::TestAppContext) {
        let view = cx.new(PreviewView::new);
        view.read_with(cx, |view, _cx| {
            assert_eq!(view.address_for_test(), DEFAULT_DEV_URL);
            assert_eq!(view.history_for_test().current(), Some(DEFAULT_DEV_URL));
        });
    }

    #[gpui::test]
    fn navigating_records_the_normalized_url(cx: &mut gpui::TestAppContext) {
        let view = cx.new(PreviewView::new);
        view.update(cx, |view, cx| {
            view.set_address_for_test("localhost:8000/orders");
            view.navigate_to_address(cx);
            // The scheme the user did not type is added, once, and the bar agrees with
            // what was loaded.
            assert_eq!(view.address_for_test(), "http://localhost:8000/orders");
            assert_eq!(view.history_for_test().current(), Some("http://localhost:8000/orders"));
            assert!(view.history_for_test().can_go_back());
        });
    }

    #[gpui::test]
    fn an_address_that_names_nothing_navigates_nowhere(cx: &mut gpui::TestAppContext) {
        let view = cx.new(PreviewView::new);
        view.update(cx, |view, cx| {
            view.set_address_for_test("not an address");
            view.navigate_to_address(cx);
            // Still on the default, and the typed text is left alone for the user to fix.
            assert_eq!(view.history_for_test().current(), Some(DEFAULT_DEV_URL));
            assert_eq!(view.address_for_test(), "not an address");
        });
    }

    #[gpui::test]
    fn zen_hiding_is_remembered_so_a_later_paint_keeps_it_hidden(cx: &mut gpui::TestAppContext) {
        // The bug this guards: zen stops rendering the pane, so its paint stops running, so
        // nothing takes the native view off the screen. The flag is what a later paint
        // reads — inferring it at paint time could never work, because paint is precisely
        // what does not happen while zen is on.
        let view = cx.new(PreviewView::new);
        view.update(cx, |view, _cx| {
            view.set_zen_hidden(true);
            assert!(view.zen_hidden, "zen must be recorded, not inferred at paint");
            view.set_zen_hidden(false);
            assert!(!view.zen_hidden, "leaving zen must show the page again");
        });
    }

    #[gpui::test]
    fn no_webview_is_built_until_the_pane_paints(cx: &mut gpui::TestAppContext) {
        // ADR-0011's laziness, asserted rather than assumed: constructing the view must
        // not construct a WKWebView.
        let view = cx.new(PreviewView::new);
        view.read_with(cx, |view, _cx| {
            assert!(view.webview.is_none());
            assert!(!view.unavailable());
        });
    }
}
