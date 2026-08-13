//! The element whose only job is to hand the OS an input handler (#18).
//!
//! # Why an `Element` at all
//!
//! [`Window::handle_input`] asserts it is called during the **paint** phase
//! (`invalidator.debug_assert_paint()` is its first line), and paint is not something a
//! `Render` implementation gets to run code in — `render` builds a tree and returns, long
//! before anything is painted. The only way into that phase is to *be* an element. This is
//! the smallest one that can exist: it lays out to nothing, paints nothing, and registers a
//! handler.
//!
//! That is also why the issue said IME was blocked on "the same change that replaces the
//! block cursor with a real caret" — both need paint-phase access. The caret half has since
//! shipped in `editor::line`, which is this crate's first `Element`; this is the second, and
//! it exists separately because the two want different lifetimes. A row element is built per
//! visible row inside `uniform_list`'s callback and would register the handler forty times
//! per frame, each with that row's bounds. The editor needs exactly one, so it is a sibling
//! of the row list rather than part of it.
//!
//! # Why it is zero-sized
//!
//! `element_bounds` is the only thing `ElementInputHandler` keeps, and gpui uses it solely
//! to pass through to `bounds_for_range`. [`EditorView`](crate::editor::EditorView) ignores
//! that argument and measures the caret itself — a candidate window has to sit under the
//! caret, and the caret's x comes from a shaped line, which no bounding box knows. So this
//! element has no reason to occupy space, and occupying none keeps it out of the flex pass
//! entirely.

use gpui::{
    App, Bounds, Element, ElementId, Entity, EntityInputHandler, GlobalElementId,
    InspectorElementId, LayoutId, Pixels, Style, Window,
};

/// Registers `view` as the window's input handler for as long as it holds focus.
pub struct InputHandlerElement<V: EntityInputHandler> {
    view: Entity<V>,
    focus_handle: gpui::FocusHandle,
}

impl<V: EntityInputHandler> InputHandlerElement<V> {
    pub fn new(view: Entity<V>, focus_handle: gpui::FocusHandle) -> Self {
        Self { view, focus_handle }
    }
}

impl<V: EntityInputHandler> Element for InputHandlerElement<V> {
    type RequestLayoutState = ();
    type PrepaintState = ();

    fn id(&self) -> Option<ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static std::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, ()) {
        // Explicitly zero in both axes rather than left at the default. A `Style::default()`
        // is `auto`, which in a flex column is "as tall as your content" — nothing, here —
        // but in a flex *row* would let this claim a share of the width and push the editor
        // sideways. Saying zero means it cannot matter which way the parent flexes.
        let mut style = Style::default();
        style.size.width = gpui::Length::Definite(gpui::px(0.0).into());
        style.size.height = gpui::Length::Definite(gpui::px(0.0).into());
        (window.request_layout(style, [], cx), ())
    }

    fn prepaint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        _bounds: Bounds<Pixels>,
        _request_layout: &mut (),
        _window: &mut Window,
        _cx: &mut App,
    ) {
    }

    fn paint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _request_layout: &mut (),
        _prepaint: &mut (),
        window: &mut Window,
        cx: &mut App,
    ) {
        // The focus check is gpui's, not ours: `handle_input` drops the registration on the
        // floor unless the handle is focused. Passing the handle rather than pre-checking
        // keeps one authority on the question — and it is the same authority
        // `EditorView::on_key_down` consults before deciding whether the platform will take
        // a keystroke, which is what stops the two from disagreeing and typing twice.
        window.handle_input(
            &self.focus_handle,
            gpui::ElementInputHandler::new(bounds, self.view.clone()),
            cx,
        );
    }
}

impl<V: EntityInputHandler> gpui::IntoElement for InputHandlerElement<V> {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}
