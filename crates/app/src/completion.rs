//! The completion popup: a list at the cursor, filtered as you type (#61).
//!
//! # Why the source is a field and not a render-time argument
//!
//! #20 is explicit that provenance is a claim about *confidence*: "a column derived from a
//! migration is a different kind of claim than a guess from a method name, and the UI should
//! say so." An item that does not carry where it came from cannot make that claim, and
//! attaching it later means reconstructing it by guesswork — the row would have to infer
//! "this looks like a route name" from the label, which is exactly the confident wrongness
//! RISKS.md #4 is about.
//!
//! So [`CompletionItem::source`] is not optional and has no default. Every constructor names
//! one, because every item genuinely has one: the only way to add a source is to add a
//! [`CompletionSource`] variant, and the match in [`CompletionSource::badge`] then fails to
//! compile until someone decides what it says about itself. That is the same shape as
//! `Resolved<T>` in `elle-laravel` (#46): the type carries the honesty and the UI renders it
//! rather than flattening it.
//!
//! # What this is not
//!
//! Not #20's merge-and-rank engine. Items from both sources go in one list in the order
//! their sources were asked, and filtering preserves that order. Ranking across sources is a
//! model of relative confidence that needs somewhere to be shown before it can be judged —
//! this popup is that somewhere, and it comes first.

use gpui::{
    App, Context, EventEmitter, FocusHandle, Focusable, KeyDownEvent, MouseButton, Pixels,
    SharedString, Window, div, prelude::*, px, uniform_list,
};

use crate::actions::{Backspace, Cancel, Confirm, SelectNext, SelectPrev, context};
use crate::fonts::Fonts;
use crate::theme::{Metrics, Theme, Themed};

/// Where a completion came from, and therefore how much it is worth trusting.
///
/// Not closed to more: #29 wants AI suggestions in this same list, and the whole reason this
/// is an enum with a `badge` rather than a boolean "from the server" is that a third variant
/// must be able to say something different about itself without touching the row layout.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum CompletionSource {
    /// The language server answered. The strongest claim available here — Intelephense has
    /// the whole project indexed and knows what is actually in scope.
    Lsp,
    /// A column from the Laravel index (#22): a migration's word, a cast's, or
    /// `$fillable`'s — the provenance is in the item's detail, because which claim backs
    /// a column is exactly what #20 says the user must be able to see.
    LaravelColumn,
    /// A relationship method from the Laravel index (#22): `posts` because the model
    /// declares `hasMany(Post::class)`. A different claim than a column — it names a
    /// method the class really defines, but the kind/target in the detail are read from
    /// the method body by scan, not proven by execution.
    LaravelRelation,
    /// A route name read out of the project's route files by static analysis (#83).
    ///
    /// A *weaker* claim than it looks, and deliberately labelled so: `route_names` only
    /// returns statically readable names, so a route registered by a service provider is
    /// absent. The list being incomplete is the accepted failure (#83) — the badge is what
    /// stops an absent name reading as "this route does not exist".
    LaravelRoute,
}

impl CompletionSource {
    /// The short label shown in the row. Kept to a few characters because it sits beside
    /// every item and a badge that wraps is a badge that pushes the label out of view.
    pub fn badge(&self) -> &'static str {
        match self {
            CompletionSource::Lsp => "LSP",
            CompletionSource::LaravelRoute => "route",
            CompletionSource::LaravelColumn => "column",
            CompletionSource::LaravelRelation => "relation",
        }
    }

    /// The badge colour, from the theme rather than a literal, so both variants stay legible
    /// under every theme (#48).
    pub fn badge_color(&self, theme: &Theme) -> gpui::Hsla {
        match self {
            // The server's answers are the ordinary case, so they get the quiet colour;
            // a badge on every row in an accent colour is decoration, not information.
            CompletionSource::Lsp => theme.text_muted,
            // Laravel's are the ones worth noticing — they come from this project rather
            // than from the language, and they are the ones with a known blind spot.
            CompletionSource::LaravelRoute => theme.accent,
            // Same voice as routes: project facts, worth noticing, with a known blind
            // spot (the index sees what artisan wrote, not what a package injected).
            CompletionSource::LaravelColumn => theme.accent,
            CompletionSource::LaravelRelation => theme.accent,
        }
    }
}

/// One completion: what is shown, what gets inserted, and where it came from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CompletionItem {
    /// What the user reads.
    pub label: SharedString,
    /// What is written into the buffer. Usually equal to the label, but a server can offer
    /// a label like `strlen(string $string): int` whose insertion is only `strlen`.
    pub insert: String,
    /// Where this came from. No default and no `Option`: see the module docs.
    pub source: CompletionSource,
    /// The server's own extra line — a type, a signature, a namespace. `None` for sources
    /// that have nothing further to say, which is not the same as an empty string.
    pub detail: Option<SharedString>,
}

impl CompletionItem {
    /// An item whose label is also what gets typed.
    pub fn new(label: impl Into<SharedString>, source: CompletionSource) -> Self {
        let label = label.into();
        Self { insert: label.to_string(), label, source, detail: None }
    }

    pub fn with_detail(mut self, detail: Option<String>) -> Self {
        // An empty detail is the same as none. A server that sends `""` would otherwise get
        // a blank second column rendered as though it had said something.
        self.detail = detail.filter(|d| !d.trim().is_empty()).map(SharedString::from);
        self
    }

    /// An item whose insertion differs from its label.
    pub fn with_insert(mut self, insert: String) -> Self {
        self.insert = insert;
        self
    }
}

/// What the popup tells the workspace.
pub enum CompletionEvent {
    /// The user accepted this item; the workspace writes it into the buffer.
    Accepted(CompletionItem),
    Dismissed,
    /// A character was typed while the popup held focus.
    ///
    /// The popup does **not** insert it, and does not narrow its own list on it either.
    /// Both are the workspace's job, because the character has to reach the *buffer* first —
    /// the popup holding focus must not mean the editor stops receiving text. Handling it
    /// here would leave the character in the query and never in the file, which is the bug
    /// this event exists to make impossible.
    Typed(String),
    /// Backspace was pressed while the popup held focus, for the same reason.
    Backspaced,
}

/// Why the popup opened, which decides what it is allowed to say when it has nothing.
///
/// An explicit invoke (⌥⌘I) is a question the user asked, and a question deserves an answer
/// even when the answer is "nothing" — silence there reads as a broken keybinding. A trigger
/// character is not a question: the user typed `->` because they were writing code, and a box
/// saying "No completions" over their cursor is the editor interrupting to report a non-event.
///
/// This is the whole difference a trigger introduces, and it is why the distinction is
/// modelled rather than inferred. The popup fires on *every* keystroke of a declared
/// character — in a string, in a comment, mid-word — and the measured behaviour of a real
/// server is to answer those with an empty list (see `crates/lsp/tests/real_server.rs`).
/// Rendering that emptiness is the difference between a feature and a flicker.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum CompletionTrigger {
    /// The explicit chord (⌥⌘I), or a palette command. The user asked.
    Invoked,
    /// A character the *server declared* as a trigger, typed in the ordinary course of
    /// editing. Never a hardcoded `->` or `::` — see `Capabilities::completion_triggers`.
    Character,
}

impl CompletionTrigger {
    /// Whether an empty list should be shown as a box saying so.
    ///
    /// The caller closes the popup instead when this is false, which is what keeps a trigger
    /// from leaving an empty rectangle over the code on every `->` inside a comment.
    pub fn reports_emptiness(&self) -> bool {
        matches!(self, CompletionTrigger::Invoked)
    }
}

/// How many rows the popup shows before it scrolls.
///
/// Ten because that is what fits without the popup becoming a panel: past that the list
/// covers the code the completion is about, which is the thing that makes a popup worse than
/// the palette it replaces.
pub const MAX_VISIBLE_ROWS: usize = 10;

/// The popup's width. Fixed rather than fit-to-content: a list whose width changes on every
/// keystroke as the longest match changes is a list that flickers while you type.
const POPUP_WIDTH: Pixels = px(420.0);

/// A list of completions anchored at the cursor.
///
/// Holds its own focus handle and its own key context, which is the whole reason arrows do
/// not have to be stolen from the editor — see [`context::COMPLETION`].
pub struct CompletionPopup {
    focus_handle: FocusHandle,
    /// Everything offered, in the order the sources were asked.
    items: Vec<CompletionItem>,
    /// `items` narrowed by `query`.
    filtered: Vec<CompletionItem>,
    /// What the user has typed *since the popup opened*, which is what narrows the list.
    query: String,
    selected: usize,
    /// Whether every source has reported. "No matches" is a claim, and making it while the
    /// language server is still thinking is a false one — the same reasoning as the
    /// palette's `loaded`.
    loaded: bool,
    /// Where the popup sits, in window coordinates: the top-left of the box, already
    /// flipped above the cursor and clamped to the window by [`Self::place`].
    origin: gpui::Point<Pixels>,
    /// Why it opened. Decides whether an empty list is shown or closed — see
    /// [`CompletionTrigger`].
    trigger: CompletionTrigger,
    /// The server said its list was truncated, so the next keystroke must ask again rather
    /// than narrow what is already here.
    ///
    /// Measured, not assumed: against a real Intelephense on a 10k-file project, *every*
    /// bare-word completion came back `isIncomplete: true` capped at exactly 100 items.
    /// Filtering that stale list by one more character showed **1** row where re-requesting
    /// showed 100 — the server re-ranks against the longer prefix and reaches past its own
    /// cap. See `crates/lsp/tests/real_server.rs::a_real_server_marks_a_large_list_incomplete`.
    incomplete: bool,
}

impl EventEmitter<CompletionEvent> for CompletionPopup {}

impl CompletionPopup {
    pub fn new(
        items: Vec<CompletionItem>,
        query: String,
        origin: gpui::Point<Pixels>,
        trigger: CompletionTrigger,
        cx: &mut Context<Self>,
    ) -> Self {
        let filtered = filter_items(&items, &query);
        Self {
            focus_handle: cx.focus_handle(),
            items,
            filtered,
            query,
            selected: 0,
            loaded: false,
            origin,
            trigger,
            incomplete: false,
        }
    }

    pub fn trigger(&self) -> CompletionTrigger {
        self.trigger
    }

    /// Whether the list on screen is a truncation the server wants re-asked.
    ///
    /// Read by the workspace on every keystroke: `true` means issue a fresh request at the
    /// new offset instead of only narrowing. See the field's documentation for the
    /// measurement behind it.
    pub fn is_incomplete(&self) -> bool {
        self.incomplete
    }

    /// Whether the popup currently has nothing to show, so a caller can close it rather
    /// than leave an empty box over the code.
    pub fn is_empty(&self) -> bool {
        self.filtered.is_empty()
    }

    /// Adds items from a source that has just answered, keeping the query already typed.
    ///
    /// Appends rather than replaces because there are two sources and they answer at
    /// different times: the route list is a local parse and the server is a process. A
    /// replace would make whichever answered second erase the other.
    pub fn add_items(&mut self, items: Vec<CompletionItem>, cx: &mut Context<Self>) {
        self.items.extend(items);
        self.refilter();
        cx.notify();
    }

    /// Replaces everything a *single* source previously offered, for a re-request.
    ///
    /// The distinction from [`Self::add_items`] is the whole reason both exist. Appending is
    /// right when two different sources answer the same question; it is wrong when the same
    /// source answers again about a longer prefix, because the earlier answer described a
    /// position the user has typed past. Appending there would leave the truncated
    /// hundred-item list from `str` sitting underneath the fresh list for `strl`, and since
    /// filtering preserves order the stale rows would sort *first*.
    ///
    /// Scoped by source rather than clearing everything: a re-request to the language server
    /// must not delete the route names Laravel found, which nothing has re-asked.
    pub fn replace_items(
        &mut self,
        source: CompletionSource,
        items: Vec<CompletionItem>,
        cx: &mut Context<Self>,
    ) {
        self.items.retain(|item| item.source != source);
        self.items.extend(items);
        self.refilter();
        cx.notify();
    }

    /// Records that the server truncated its list.
    ///
    /// Sticky within one popup on purpose. A server that answers a re-request with a
    /// complete list has genuinely finished, so this is set from each response rather than
    /// only ever raised — passing `false` clears it and the popup goes back to filtering
    /// locally, which is the cheaper path and the one the protocol asks for.
    pub fn set_incomplete(&mut self, incomplete: bool) {
        self.incomplete = incomplete;
    }

    /// Marks every source as having reported, so "No matches" becomes sayable.
    pub fn mark_loaded(&mut self, cx: &mut Context<Self>) {
        self.loaded = true;
        cx.notify();
    }

    /// Whether every source has reported.
    ///
    /// Read by the workspace before closing an empty character-triggered popup: emptiness
    /// only means "there is nothing here" once nobody is still looking.
    pub fn is_loaded(&self) -> bool {
        self.loaded
    }

    /// Replaces the query, for a source that knows the typed span better than the opener did.
    ///
    /// Exactly one caller, and it is not a general setter: a route name is dotted, and the
    /// generic `word_before` scan stops at the `.`. The Laravel source knows the whole
    /// literal is the span being completed, so it corrects both the query and the range the
    /// workspace will overwrite. Correcting only one of the two is the bug this exists to
    /// have fixed — see `request_route_completions`.
    pub fn set_query(&mut self, query: String, cx: &mut Context<Self>) {
        self.query = query;
        self.refilter();
        cx.notify();
    }

    /// The list as the user currently sees it. Used by the tests, which assert on what is
    /// *shown* rather than on what was offered — the filtering is the part that can be
    /// wrong.
    #[cfg(test)]
    pub fn visible_items(&self) -> &[CompletionItem] {
        &self.filtered
    }

    /// Extends the query by a character the user typed in the editor.
    ///
    /// Returns whether the popup should stay open. Normally that is "does anything still
    /// match": a popup showing "No matches" while the user keeps typing code is a popup in
    /// the way, and every editor closes rather than sitting there empty.
    ///
    /// **Unless the list is incomplete**, in which case an empty filter result is not
    /// evidence of anything. The server truncated its answer, so the rows that match the
    /// longer prefix may simply be the ones it cut — and closing here would delete a popup
    /// that is about to be refilled by the re-request the caller is issuing. This is the
    /// concrete failure the flag prevents, and it is not hypothetical: at `str` the measured
    /// response was 100 items with `isIncomplete: true`, and the local filter for `strl`
    /// found 1 of the 100 while the server's own answer for `strl` had 100.
    pub fn push_query(&mut self, text: &str, cx: &mut Context<Self>) -> bool {
        self.query.push_str(text);
        self.refilter();
        cx.notify();
        self.incomplete || !self.filtered.is_empty()
    }

    /// Shortens the query by one character, for a backspace in the editor.
    ///
    /// Returns whether the popup should stay open: backspacing past the point where it was
    /// opened means the user has left the word entirely, so it closes.
    pub fn pop_query(&mut self, cx: &mut Context<Self>) -> bool {
        let popped = self.query.pop().is_some();
        self.refilter();
        cx.notify();
        popped
    }

    fn refilter(&mut self) {
        self.filtered = filter_items(&self.items, &self.query);
        // Clamped rather than reset: narrowing the list while the user is on row 7 should
        // leave them near where they were, not jump them to the top on every keystroke.
        self.selected = self.selected.min(self.filtered.len().saturating_sub(1));
    }

    /// Keys typed while the popup holds focus.
    ///
    /// Only text reaches the query; navigation and dismissal are actions bound in the
    /// `Completion` context, so this never has to guess what `up` means.
    fn on_key_down(&mut self, event: &KeyDownEvent, _w: &mut Window, cx: &mut Context<Self>) {
        let keystroke = &event.keystroke;
        if keystroke.modifiers.platform
            || keystroke.modifiers.control
            || keystroke.modifiers.function
        {
            return;
        }
        // The same list the editor's own `on_key_down` rejects, and it has to be the same
        // list: these are keys that can arrive carrying a `key_char` on some layouts, and a
        // shorter list here means one of them is forwarded to the buffer as text. `delete`,
        // `home`, `end`, `pageup` and `pagedown` are the five that were missing — none is
        // bound in `context::COMPLETION`, so with the popup focused they reach this handler
        // rather than an action, and End would have inserted a character instead of moving
        // the caret.
        if matches!(
            keystroke.key.as_str(),
            "backspace"
                | "delete"
                | "enter"
                | "tab"
                | "escape"
                | "left"
                | "right"
                | "up"
                | "down"
                | "home"
                | "end"
                | "pageup"
                | "pagedown"
        ) {
            return;
        }
        let Some(text) = keystroke.key_char.as_deref() else { return };
        if text.is_empty() || text.chars().all(|c| c.is_control()) {
            return;
        }

        // Reported, not applied. The workspace inserts it into the buffer and then calls
        // `push_query` — so the character lands in the file and in the filter, in that
        // order, and there is exactly one place that decides what a keystroke means.
        cx.emit(CompletionEvent::Typed(text.to_string()));
    }

    fn backspace(&mut self, _: &Backspace, _w: &mut Window, cx: &mut Context<Self>) {
        cx.emit(CompletionEvent::Backspaced);
    }

    fn select_next(&mut self, _: &SelectNext, _w: &mut Window, cx: &mut Context<Self>) {
        if !self.filtered.is_empty() {
            self.selected = (self.selected + 1) % self.filtered.len();
            cx.notify();
        }
    }

    fn select_prev(&mut self, _: &SelectPrev, _w: &mut Window, cx: &mut Context<Self>) {
        if !self.filtered.is_empty() {
            self.selected =
                if self.selected == 0 { self.filtered.len() - 1 } else { self.selected - 1 };
            cx.notify();
        }
    }

    fn confirm(&mut self, _: &Confirm, _w: &mut Window, cx: &mut Context<Self>) {
        if let Some(item) = self.filtered.get(self.selected) {
            cx.emit(CompletionEvent::Accepted(item.clone()));
        } else {
            // Enter with nothing to accept must not swallow the key silently — dismissing
            // hands the next Enter back to the editor as a newline.
            cx.emit(CompletionEvent::Dismissed);
        }
    }

    fn cancel(&mut self, _: &Cancel, _w: &mut Window, cx: &mut Context<Self>) {
        cx.emit(CompletionEvent::Dismissed);
    }
}

impl Focusable for CompletionPopup {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for CompletionPopup {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let entity = cx.entity();
        let count = self.filtered.len();
        let selected = self.selected;
        let fonts = Fonts::get(cx);

        div()
            .key_context(context::COMPLETION)
            .track_focus(&self.focus_handle(cx))
            .on_key_down(cx.listener(Self::on_key_down))
            .on_action(cx.listener(Self::backspace))
            .on_action(cx.listener(Self::select_next))
            .on_action(cx.listener(Self::select_prev))
            .on_action(cx.listener(Self::confirm))
            .on_action(cx.listener(Self::cancel))
            // Absolute, at the origin the workspace measured. Not `justify_center` like the
            // palette: a completion that appears in the middle of the screen is a palette,
            // and the whole point of this issue is that it appears at the cursor.
            .absolute()
            .left(self.origin.x)
            .top(self.origin.y)
            .w(POPUP_WIDTH)
            .max_h(Metrics::ROW_HEIGHT * MAX_VISIBLE_ROWS)
            .flex()
            .flex_col()
            .bg(theme.panel)
            .border_1()
            .border_color(theme.border)
            .rounded_md()
            .shadow_lg()
            // The popup's own text is UI text, not code: it is a widget over the editor, and
            // matching the editor's font would make a long signature wrap where a UI font
            // fits. Set here because a `uniform_list` row does not inherit from an ancestor.
            .text_size(fonts.ui_size)
            .text_color(theme.text)
            .child(if count == 0 {
                div()
                    .p_2()
                    .text_color(theme.text_muted)
                    // The distinction the palette makes for the same reason: with sources
                    // still answering, "No matches" is a claim nobody has established.
                    //
                    // A character-triggered popup reaches here only for the instant before
                    // the workspace closes it — the empty case is *its* answer, not a
                    // message. Rendering the same text either way would flash "No
                    // completions" over the cursor on every `->` typed inside a comment.
                    .child(if self.loaded { "No completions" } else { "Completing…" })
                    .into_any_element()
            } else {
                uniform_list("completion-items", count, move |range, _window, cx| {
                    entity.update(cx, |popup, _cx| {
                        range
                            .filter_map(|index| {
                                let item = popup.filtered.get(index)?.clone();
                                let entity = entity.clone();
                                Some(completion_row(item, index, selected, &theme, entity))
                            })
                            .collect()
                    })
                })
                // An explicit height, never `flex_1`. The wrapper above has `max_h` and no
                // `h`, so its height comes from its content — and `flex_1` is flex-basis 0,
                // which *contributes no content height*. The two resolve to a popup exactly
                // zero pixels tall: state, keyboard and accept all keep working, because
                // none of them live in layout, and the list paints nothing at all. That
                // shipped, and was reported as "funciona, mas não mostra nada" — Enter
                // inserted the member while the screen showed nothing (#112's class again:
                // 9 render tests drew this popup and none could see it had no height).
                //
                // The selector below is what finally lets a headless test see it: gpui's
                // test build records this element's laid-out bounds under the key, and
                // `completion_list_occupies_real_height` asserts them. First real geometry
                // assertion in the suite — see #112 for why there were none before.
                .h(popup_height(count))
                .debug_selector(|| "completion-list".into())
                .into_any_element()
            })
    }
}

/// One row: label, the server's detail, and the source badge.
///
/// The badge is laid out as a sibling with its own space rather than appended to the label
/// text, which is what #61 means by "if it is designed in later it will be bolted onto a row
/// layout that has no space for it". A row built as one string could not right-align it, and
/// a long label would push it off the edge.
fn completion_row(
    item: CompletionItem,
    index: usize,
    selected: usize,
    theme: &Theme,
    entity: gpui::Entity<CompletionPopup>,
) -> gpui::AnyElement {
    let badge_color = item.source.badge_color(theme);
    let accepted = item.clone();

    div()
        .id(("completion-row", index))
        .flex()
        .items_center()
        .gap_2()
        .h(Metrics::ROW_HEIGHT)
        .px_2()
        .cursor_pointer()
        .when(index == selected, |el| el.bg(theme.selected))
        .hover(|el| el.bg(theme.hover))
        .active(|el| el.bg(theme.pressed))
        .on_mouse_down(MouseButton::Left, move |_ev, _window, cx| {
            let accepted = accepted.clone();
            entity.update(cx, |popup, cx| {
                popup.selected = index;
                cx.emit(CompletionEvent::Accepted(accepted));
            });
        })
        // A row is one line, enforced, not assumed. `overflow_hidden` alone clips the box
        // horizontally but lets the *text wrap* first — and wrapped text grows downward,
        // painting over the rows below. Intelephense's detail for `$_SERVER` is an array
        // shape hundreds of characters long, so with real data every global's detail
        // bled across its neighbours and the list read as garbage. `truncate` is
        // no-wrap plus ellipsis: a long detail ends in `…` inside its own row.
        //
        // Untested, and the attempt is worth recording: a `debug_bounds` test asserting the
        // row stays ROW_HEIGHT tall passed *with the bug restored*, because the row's box
        // never grew — `.h()` fixes it — the wrapped glyphs simply painted outside it.
        // Bounds measure boxes, not ink, so this is #112's territory: only eyes catch it.
        //
        // The label takes the space that is left, so the badge keeps its own.
        .child(div().flex_1().truncate().child(item.label.clone()))
        .children(item.detail.clone().map(|detail| {
            div().flex_none().text_color(theme.text_muted).max_w(px(160.0)).truncate().child(detail)
        }))
        .child(div().flex_none().text_color(badge_color).child(item.source.badge()))
        .into_any_element()
}

/// Where the popup's top-left corner goes, given the cursor and the window.
///
/// Split out as a free function taking plain numbers because it is the part that can
/// actually be wrong and the part a headless test can check: gpui's test text system is a
/// fake perfect monospace, so a test cannot verify the *cursor's* x — but given a cursor
/// position, this arithmetic either flips and clamps correctly or it does not, and that is
/// checkable without a screen.
///
/// - Below the cursor's line normally, so the popup does not cover what is being typed.
/// - Flipped *above* when there is not enough room below, which is the bottom of the window.
/// - Clamped at the right edge, so a completion in a long line is not half off-screen.
pub fn place(
    cursor: gpui::Point<Pixels>,
    line_height: Pixels,
    popup_height: Pixels,
    window: gpui::Size<Pixels>,
) -> gpui::Point<Pixels> {
    let below = cursor.y + line_height;

    // Flip only when the popup genuinely does not fit below *and* fits better above. A flip
    // at the bottom of a very short window would otherwise move it somewhere equally bad
    // while also moving it away from where the user last saw it.
    let above = cursor.y - popup_height;
    let y = if below + popup_height > window.height && above >= px(0.0) { above } else { below };

    // Clamped at zero as well as at the right edge: `window.width - POPUP_WIDTH` is negative
    // in a window narrower than the popup, and a negative left puts the list off-screen on
    // the other side.
    let x = cursor.x.min((window.width - POPUP_WIDTH).max(px(0.0))).max(px(0.0));

    gpui::point(x, y)
}

/// The height the popup will occupy for `count` rows, which [`place`] needs before layout.
pub fn popup_height(count: usize) -> Pixels {
    // Never zero: an empty popup still shows "Completing…", and a zero height would make
    // the flip decision on a box that does not exist.
    let rows = count.clamp(1, MAX_VISIBLE_ROWS);
    Metrics::ROW_HEIGHT * rows
}

/// Candidates matching `query`, in their original order.
///
/// A **prefix** match, not the palette's subsequence. Deliberately different, and the
/// difference is the point: a palette is a search over things you half-remember, where
/// scattered initials are how you find `UserController` by typing `uctrl`. A completion is
/// finishing a word you are *already typing*, and every editor treats it that way — typing
/// `str` must not offer `Illuminate\Support\Str` ahead of `strlen`, and subsequence matching
/// would put dozens of unrelated identifiers in front of the one being typed.
///
/// Case-insensitive because PHP's own function names are lowercase while class names are
/// not, and a user typing `arr` means `Arr`.
fn filter_items(items: &[CompletionItem], query: &str) -> Vec<CompletionItem> {
    if query.is_empty() {
        return items.to_vec();
    }
    items.iter().filter(|item| prefix_match(&item.label, query)).cloned().collect()
}

/// Whether `haystack` starts with `needle`, ignoring ASCII case.
///
/// ASCII-only case folding matches the rest of this codebase (`palette::subsequence`) and is
/// correct for the identifiers this completes: PHP identifiers are ASCII by language rule,
/// and a route name that is not still compares correctly byte-for-byte.
fn prefix_match(haystack: &str, needle: &str) -> bool {
    let haystack = haystack.as_bytes();
    let needle = needle.as_bytes();
    needle.len() <= haystack.len() && haystack[..needle.len()].eq_ignore_ascii_case(needle)
}

/// The word already typed before `offset`, which is what the popup opens pre-filtered by.
///
/// Without this, invoking completion in the middle of `str|` would offer every symbol in the
/// project rather than the ones starting `str`. The server is asked at the *cursor* and
/// answers with the whole set for that position; the prefix is applied on our side, which is
/// also what makes filtering-as-you-type work without a second request per keystroke.
pub fn word_before(text: &str, offset: usize) -> &str {
    let offset = offset.min(text.len());
    // Walk back over identifier characters. `$` is deliberately excluded: it starts a PHP
    // variable and the server's own labels include it, so treating it as part of the prefix
    // would make `$us` fail to match `$user`. It is left to the caller's request position.
    let start = text[..offset]
        .char_indices()
        .rev()
        .take_while(|(_, c)| c.is_alphanumeric() || *c == '_')
        .last()
        .map_or(offset, |(index, _)| index);
    &text[start..offset]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(label: &str, source: CompletionSource) -> CompletionItem {
        CompletionItem::new(label.to_string(), source)
    }

    #[test]
    fn a_completion_carries_its_source_rather_than_inferring_it() {
        // The requirement from #20 stated as a test: two items with the *same label* from
        // different sources are different claims, and nothing about the label can tell them
        // apart. If provenance were reconstructed at render time this would be impossible.
        let from_server = item("home", CompletionSource::Lsp);
        let from_routes = item("home", CompletionSource::LaravelRoute);

        assert_eq!(from_server.label, from_routes.label);
        assert_ne!(from_server.source, from_routes.source);
        assert_eq!(from_server.source.badge(), "LSP");
        assert_eq!(from_routes.source.badge(), "route");
    }

    #[test]
    fn filtering_is_a_prefix_match_not_a_subsequence() {
        // The distinction from the palette, and the reason it is deliberate: `uctrl` finds
        // `UserController` in quick open and must NOT find it here, because a completion
        // finishes the word being typed.
        let all = vec![
            item("strlen", CompletionSource::Lsp),
            item("str_replace", CompletionSource::Lsp),
            item("UserController", CompletionSource::Lsp),
            item("array_map", CompletionSource::Lsp),
        ];

        let matched = filter_items(&all, "str");
        assert_eq!(matched.len(), 2);
        assert_eq!(matched[0].label, "strlen");
        assert_eq!(matched[1].label, "str_replace");

        assert!(filter_items(&all, "uctrl").is_empty(), "subsequence must not match here");
    }

    #[test]
    fn filtering_ignores_case_but_not_order() {
        let all =
            vec![item("Arr", CompletionSource::Lsp), item("array_map", CompletionSource::Lsp)];
        assert_eq!(filter_items(&all, "arr").len(), 2);
        assert_eq!(filter_items(&all, "ARR").len(), 2);
        assert_eq!(filter_items(&all, "rr").len(), 0, "a prefix match is anchored");
    }

    #[test]
    fn an_empty_query_keeps_every_item_in_source_order() {
        // Order is the provenance-adjacent property #20 will later replace with ranking:
        // until then, items appear in the order their sources were asked, and filtering
        // must not quietly sort them.
        let all = vec![
            item("zebra", CompletionSource::LaravelRoute),
            item("alpha", CompletionSource::Lsp),
        ];
        let matched = filter_items(&all, "");
        assert_eq!(matched[0].label, "zebra", "must not sort");
        assert_eq!(matched[1].label, "alpha");
    }

    #[test]
    fn items_from_both_sources_share_one_list() {
        // The minimum that makes provenance meaningful: two real sources, one list, each row
        // still knowing which it was.
        let all = vec![
            item("home", CompletionSource::LaravelRoute),
            item("homepage_url", CompletionSource::Lsp),
        ];
        let matched = filter_items(&all, "home");
        assert_eq!(matched.len(), 2);
        assert_eq!(matched[0].source, CompletionSource::LaravelRoute);
        assert_eq!(matched[1].source, CompletionSource::Lsp);
    }

    #[test]
    fn a_prefix_longer_than_the_label_does_not_match() {
        // The bounds case: `needle.len() <= haystack.len()` is what keeps the slice from
        // panicking, and this is the test that fails if it is dropped.
        assert!(!prefix_match("str", "strlen"));
        assert!(prefix_match("strlen", "strlen"));
        assert!(prefix_match("strlen", ""));
    }

    #[test]
    fn the_word_before_the_cursor_is_what_the_list_opens_filtered_by() {
        assert_eq!(word_before("$user->getNam", 13), "getNam");
        assert_eq!(word_before("strlen", 3), "str");
        // At a boundary there is no prefix, so the whole list is offered.
        assert_eq!(word_before("$user->", 7), "");
        assert_eq!(word_before("", 0), "");
        // Past the end clamps rather than panicking.
        assert_eq!(word_before("str", 99), "str");
    }

    #[test]
    fn the_word_before_the_cursor_stops_at_a_non_identifier_character() {
        // `route('ho` — the prefix is `ho`, not `route('ho`. This is what makes the popup
        // inside a string literal filter by the name being typed.
        assert_eq!(word_before("route('ho", 9), "ho");
        assert_eq!(word_before("foo(bar", 7), "bar");
        assert_eq!(word_before("a.b", 3), "b");
        // Underscores and digits are identifier characters; a dash is not.
        assert_eq!(word_before("user_id2", 8), "user_id2");
        assert_eq!(word_before("a-b", 3), "b");
    }

    #[test]
    fn the_word_before_the_cursor_handles_multibyte_text() {
        // Byte offsets into multibyte text: `ação` is 6 bytes. Slicing mid-codepoint would
        // panic, and `char_indices` is what keeps the start on a boundary.
        let text = "ação";
        assert_eq!(word_before(text, text.len()), "ação");
        // A cursor after `aç` (3 bytes) — the prefix is the whole word so far.
        assert_eq!(word_before(text, 3), "aç");
    }

    #[test]
    fn the_popup_goes_below_the_cursor_when_there_is_room() {
        let window = gpui::size(px(1200.0), px(800.0));
        let cursor = gpui::point(px(100.0), px(200.0));
        let placed = place(cursor, px(20.0), px(220.0), window);

        assert_eq!(placed.y, px(220.0), "one line below the cursor");
        assert_eq!(placed.x, px(100.0), "aligned with the cursor");
    }

    #[test]
    fn the_popup_flips_above_the_cursor_near_the_bottom() {
        // The requirement from #61, and the one a user hits constantly: completing on the
        // last visible line must not put the list off the bottom of the window.
        let window = gpui::size(px(1200.0), px(800.0));
        let cursor = gpui::point(px(100.0), px(760.0));
        let placed = place(cursor, px(20.0), px(220.0), window);

        assert_eq!(placed.y, px(540.0), "flipped to sit above the cursor line");
        assert!(placed.y + px(220.0) <= cursor.y, "must not overlap the line being typed");
    }

    #[test]
    fn the_popup_does_not_flip_when_there_is_no_room_above_either() {
        // A window shorter than the popup: flipping would put it at a negative y, which is
        // further off-screen than simply staying below.
        let window = gpui::size(px(1200.0), px(200.0));
        let cursor = gpui::point(px(100.0), px(150.0));
        let placed = place(cursor, px(20.0), px(220.0), window);

        assert_eq!(placed.y, px(170.0), "stays below rather than going negative");
    }

    #[test]
    fn the_popup_clamps_at_the_right_edge() {
        // A completion far along a long line: the box must stay wholly inside the window.
        let window = gpui::size(px(1200.0), px(800.0));
        let cursor = gpui::point(px(1150.0), px(100.0));
        let placed = place(cursor, px(20.0), px(220.0), window);

        assert_eq!(placed.x, px(1200.0) - POPUP_WIDTH);
        assert!(placed.x + POPUP_WIDTH <= window.width);
    }

    #[test]
    fn the_popup_stays_on_screen_in_a_window_narrower_than_itself() {
        // `window.width - POPUP_WIDTH` is negative here. Without the `.max(0)` the popup
        // would be placed off the left edge — worse than the overflow it was avoiding.
        let window = gpui::size(px(200.0), px(800.0));
        let cursor = gpui::point(px(150.0), px(100.0));
        let placed = place(cursor, px(20.0), px(220.0), window);

        assert_eq!(placed.x, px(0.0));
    }

    #[test]
    fn the_popup_height_is_capped_so_a_long_list_does_not_fill_the_window() {
        // #61: the list must not lay out hundreds of items to show ten.
        assert_eq!(popup_height(1), Metrics::ROW_HEIGHT);
        assert_eq!(popup_height(1000), Metrics::ROW_HEIGHT * MAX_VISIBLE_ROWS);
        assert_eq!(popup_height(0), Metrics::ROW_HEIGHT, "empty still shows a status line");
    }

    #[test]
    fn only_a_deliberate_invoke_reports_that_there_is_nothing() {
        // The asymmetry a trigger introduces, and the reason the two are distinguished at
        // all. ⌥⌘I with nothing to offer must say so — silence reads as a broken key. A `->`
        // typed inside a comment must not, because the user asked no question.
        assert!(CompletionTrigger::Invoked.reports_emptiness());
        assert!(!CompletionTrigger::Character.reports_emptiness());
    }
}
