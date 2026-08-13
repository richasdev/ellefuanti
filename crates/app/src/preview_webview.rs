//! Hosting a `WKWebView` inside GPUI's window (#31), which is the whole difficulty of the
//! preview pane. Per ADR-0011 this is the only webview in the process, and it draws the
//! user's site — never any part of the IDE's own chrome.
//!
//! # GPUI draws with Metal; a `WKWebView` is an `NSView`. They have to be siblings.
//!
//! GPUI 0.2.2 has no API for embedding a native view, and there is no supported way to draw
//! one *into* its Metal scene — `Window::paint_surface` takes a `CVPixelBuffer`, which
//! `WKWebView` does not produce. So the webview cannot live inside GPUI's rendering at all.
//! What GPUI does offer is one public seam: it implements [`HasWindowHandle`], which hands
//! back the `NSView*` of its own Metal-backed drawing view.
//!
//! That is enough, because of how GPUI arranges the window: its view is a *subview* of the
//! window's `contentView`, and GPUI never touches that view's other children. So the
//! webview goes in as a **sibling above** GPUI's view, and AppKit's ordinary subview rules
//! do the compositing. Above rather than below is deliberate: GPUI's Metal layer is not
//! opaque, but GPUI has no concept of punching a hole in its scene, so a webview underneath
//! would be covered by whatever the pane's rect paints. Above, the webview simply wins its
//! rectangle, which is the behaviour that can be reasoned about.
//!
//! # The consequence: the webview is not clipped by GPUI, so its frame is the contract
//!
//! Because AppKit — not GPUI — decides where this view draws, nothing in GPUI's layout
//! constrains it. A stale frame does not clip or letterbox; it draws the page over whatever
//! GPUI meant to be there. [`PreviewWebView::set_frame`] is therefore called from the
//! pane's `paint`, every frame it is visible, with the rect GPUI actually laid out. That is
//! also why [`PreviewWebView::set_hidden`] exists: a pane that closes must hide the view in
//! the same breath, because there is no layout pass that would have removed it.
//!
//! The coordinate flip lives in [`flip_to_appkit`], which is pure and tested — it is the
//! one piece of this file that can be got wrong silently and checked without a screen.
//!
//! # Everything here is lazy
//!
//! Nothing in this module runs until the pane is first opened; ADR-0011 makes that a
//! requirement rather than an optimisation, and #79/#93 measure it. A user who never opens
//! the preview never constructs a `WKWebView`, never links a page of WebKit into residency,
//! and pays nothing at startup or at idle.

use objc2::rc::Retained;
use objc2::{MainThreadMarker, MainThreadOnly, msg_send};
use objc2_app_kit::{NSView, NSWindowOrderingMode};
use objc2_core_foundation::{CGPoint, CGRect, CGSize};
use objc2_foundation::{NSString, NSURL, NSURLRequest};
use objc2_web_kit::{WKWebView, WKWebViewConfiguration};
use raw_window_handle::{HasWindowHandle, RawWindowHandle};

/// A rectangle in GPUI's coordinates: origin top-left, y growing downwards.
///
/// Deliberately not `gpui::Bounds` so that [`flip_to_appkit`] stays testable without a
/// window — the conversion is arithmetic and deserves to be checked as arithmetic.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PaneRect {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

/// Converts a top-left-origin rect into AppKit's bottom-left-origin coordinates, given the
/// height of the container view.
///
/// AppKit measures y upwards from the bottom of the superview; GPUI measures it downwards
/// from the top. Getting this backwards puts the pane a plausible-looking distance from the
/// wrong edge — it renders, so it never crashes, and it is wrong on every window that is
/// not exactly twice the pane's height. Hence a test rather than a squint.
pub fn flip_to_appkit(rect: PaneRect, container_height: f64) -> PaneRect {
    PaneRect { y: container_height - rect.y - rect.height, ..rect }
}

/// A `WKWebView` living in GPUI's window as a sibling of GPUI's own view.
///
/// Dropping it removes the view from the window: the pane closing must not leave a page
/// drawing over the editor, and there is no GPUI layout pass that would take it away.
pub struct PreviewWebView {
    webview: Retained<WKWebView>,
    /// GPUI's `contentView`, retained so the frame flip has a height to measure against
    /// without a second trip through the window handle each paint.
    container: Retained<NSView>,
}

impl PreviewWebView {
    /// Builds the webview and inserts it above GPUI's view.
    ///
    /// `None` when the window is not the AppKit window this assumes — which on macOS means
    /// GPUI changed underneath us. The pane treats that as "no preview available" and says
    /// so, rather than pretending to show a page.
    ///
    /// # Safety of the objc here
    ///
    /// All of it runs on the main thread, which `MainThreadMarker` proves and which is where
    /// GPUI's UI work already happens. The `ns_view` pointer comes from GPUI's own
    /// `HasWindowHandle` impl and is valid for as long as the window is, which outlives this
    /// struct: the pane is dropped with the window, not after it.
    pub fn new(window: &impl HasWindowHandle, mtm: MainThreadMarker) -> Option<Self> {
        let handle = window.window_handle().ok()?;
        let RawWindowHandle::AppKit(appkit) = handle.as_raw() else {
            return None;
        };

        // SAFETY: GPUI documents this pointer as its NSView, and we are on the main thread.
        let gpui_view: &NSView = unsafe { appkit.ns_view.cast::<NSView>().as_ref() };
        let container = unsafe { gpui_view.superview() }?;

        // SAFETY: main thread, proven by `mtm`.
        let configuration = unsafe { WKWebViewConfiguration::new(mtm) };
        let webview = unsafe {
            WKWebView::initWithFrame_configuration(
                WKWebView::alloc(mtm),
                CGRect::new(CGPoint::new(0.0, 0.0), CGSize::new(0.0, 0.0)),
                &configuration,
            )
        };

        // Above GPUI's view: see the module docs — GPUI cannot punch a hole in its scene,
        // so a webview underneath would simply be painted over.
        unsafe {
            container.add_subview_positioned_relative_to(
                &webview,
                NSWindowOrderingMode::Above,
                Some(gpui_view),
            );
        }

        Some(Self { webview, container })
    }

    /// Moves the view to the rect GPUI laid out for the pane, flipping into AppKit's
    /// coordinates. Called every painted frame — see the module docs for why a stale frame
    /// is not a cosmetic problem here.
    pub fn set_frame(&self, rect: PaneRect) {
        let container_height = self.container.frame().size.height;
        let flipped = flip_to_appkit(rect, container_height);
        self.webview.setFrame(CGRect::new(
            CGPoint::new(flipped.x, flipped.y),
            CGSize::new(flipped.width, flipped.height),
        ));
    }

    /// Shows or hides the view without destroying it, so a closed-and-reopened pane does not
    /// reload the page — and, more importantly, so a hidden pane stops drawing at all.
    pub fn set_hidden(&self, hidden: bool) {
        self.webview.setHidden(hidden);
    }

    /// Loads `url`, which the caller has already put through
    /// [`crate::preview::normalize_url`]. A URL AppKit refuses to parse is dropped rather
    /// than substituted — the pane must never show a page other than the one asked for.
    pub fn load(&self, url: &str) {
        let Some(ns_url) = NSURL::URLWithString(&NSString::from_str(url)) else {
            return;
        };
        let request = NSURLRequest::requestWithURL(&ns_url);
        unsafe { self.webview.loadRequest(&request) };
    }

    pub fn reload(&self) {
        unsafe { self.webview.reload() };
    }

    /// Back and forward are asked of *WebKit*, not of [`crate::preview::History`], because
    /// WebKit's list is the one that includes navigations the page made on its own — a form
    /// post, a redirect, a link the user clicked inside the preview. The pane's own history
    /// records what the address bar was told to load; only this one knows where the page
    /// has actually been.
    pub fn go_back(&self) {
        unsafe { self.webview.goBack() };
    }

    pub fn go_forward(&self) {
        unsafe { self.webview.goForward() };
    }

    pub fn can_go_back(&self) -> bool {
        unsafe { self.webview.canGoBack() }
    }

    pub fn can_go_forward(&self) -> bool {
        unsafe { self.webview.canGoForward() }
    }
}

impl Drop for PreviewWebView {
    fn drop(&mut self) {
        // Without this the page keeps drawing over the editor: the view belongs to AppKit's
        // hierarchy, which no GPUI layout pass will tidy up on our behalf.
        self.webview.removeFromSuperview();
    }
}

/// `addSubview:positioned:relativeTo:` is not in `objc2-app-kit`'s generated surface as a
/// safe method, so it is declared here rather than reached for through a raw `msg_send!` at
/// each call site — one unsafe boundary instead of several.
trait AddSubviewPositioned {
    unsafe fn add_subview_positioned_relative_to(
        &self,
        view: &NSView,
        place: NSWindowOrderingMode,
        other: Option<&NSView>,
    );
}

impl AddSubviewPositioned for NSView {
    unsafe fn add_subview_positioned_relative_to(
        &self,
        view: &NSView,
        place: NSWindowOrderingMode,
        other: Option<&NSView>,
    ) {
        unsafe {
            let _: () = msg_send![
                self,
                addSubview: view,
                positioned: place,
                relativeTo: other,
            ];
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The rect arithmetic is the one part of this file a test can reach — the rest needs a
    /// window on a screen. See the module docs on why getting it wrong is silent.
    #[test]
    fn the_flip_measures_y_from_the_bottom() {
        // A 200-tall pane sitting 100 down from the top of an 800-tall window has its
        // bottom edge 500 up from the bottom.
        let rect = PaneRect { x: 40.0, y: 100.0, width: 300.0, height: 200.0 };
        let flipped = flip_to_appkit(rect, 800.0);
        assert_eq!(flipped, PaneRect { x: 40.0, y: 500.0, width: 300.0, height: 200.0 });
    }

    #[test]
    fn a_pane_against_the_bottom_edge_flips_to_zero() {
        let rect = PaneRect { x: 0.0, y: 600.0, width: 400.0, height: 200.0 };
        assert_eq!(flip_to_appkit(rect, 800.0).y, 0.0);
    }

    #[test]
    fn the_flip_is_its_own_inverse() {
        // Which is the property that makes it safe to apply once and only once.
        let rect = PaneRect { x: 10.0, y: 250.0, width: 100.0, height: 120.0 };
        assert_eq!(flip_to_appkit(flip_to_appkit(rect, 900.0), 900.0), rect);
    }

    #[test]
    fn only_y_moves() {
        let rect = PaneRect { x: 12.5, y: 33.0, width: 44.0, height: 55.0 };
        let flipped = flip_to_appkit(rect, 500.0);
        assert_eq!((flipped.x, flipped.width, flipped.height), (12.5, 44.0, 55.0));
    }
}
