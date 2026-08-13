//! The preview pane's logic (#31): what URL to load, and where "back" goes.
//!
//! Everything in this file is pure. The webview itself is macOS and objc and can only be
//! exercised with a real window on a real screen; the two things that are actually easy to
//! get wrong — deciding what a typed address means, and keeping a history that agrees with
//! the buttons drawn next to it — are decided here instead, where a test can reach them.
//! [`crate::preview_webview`] holds the part that must touch AppKit, and holds as little as
//! it can get away with.
//!
//! # The address bar guesses, because a user typing `localhost:8000` means a URL
//!
//! [`normalize_url`] is deliberately forgiving in one direction only. `localhost:8000` and
//! `127.0.0.1/orders` are addresses a person types and unambiguously means as `http://`, so
//! they get a scheme. What it will not do is invent a *destination*: empty input, a lone
//! scheme, or something with a space in it is refused rather than turned into a search
//! query. A preview pane that silently navigated somewhere other than what was typed would
//! be lying about what the user is looking at, and this pane's whole value is fidelity.
//!
//! `http://` rather than `https://` for a scheme-less guess is not carelessness: the thing
//! being previewed is a dev server on this machine, and `artisan serve` speaks plain HTTP.
//! Guessing `https` would fail the common case to be pedantic about a threat model — a
//! loopback address — that does not apply.
//!
//! # History is a cursor over a list, not two stacks
//!
//! [`History`] keeps every visited URL in order with an index pointing at the current one,
//! which makes "can I go back" a comparison rather than a bookkeeping exercise. Navigating
//! somewhere new after going back **truncates the forward entries** — the branch the user
//! walked away from is gone, which is what every browser does and what the forward button
//! must agree with, or it offers a destination that no longer exists.

/// Laravel's own default. `php artisan serve` binds `127.0.0.1:8000` unless told otherwise,
/// so it is the one guess that is right more often than any other for this app's users.
///
/// It is a *guess*, not a detection: nothing here checks whether a server is listening.
/// Reading the running dev server out of the project (a `.env` `APP_URL`, a live port scan,
/// or watching for `artisan serve` in the terminal) is real work and deliberately out of
/// scope for #31 — the pane opens on a sensible address and the user can retype it.
pub const DEFAULT_DEV_URL: &str = "http://localhost:8000";

/// Turns what a user typed into a URL to load, or `None` when it does not name one.
///
/// The rules, in order:
/// - Blank (or whitespace) is not an address.
/// - Anything with an explicit `scheme://` is taken as-is, trimmed.
/// - Anything else gets `http://`, because a dev server is what this pane previews.
/// - Internal whitespace is refused: it is prose or a search query, not an address, and
///   this pane does not have a search engine to send it to.
pub fn normalize_url(input: &str) -> Option<String> {
    let trimmed = input.trim();
    if trimmed.is_empty() || trimmed.contains(char::is_whitespace) {
        return None;
    }
    match trimmed.split_once("://") {
        // A scheme with nothing after it names no destination — `http://` is not a page.
        Some((scheme, rest)) => {
            (!scheme.is_empty() && !rest.is_empty()).then(|| trimmed.to_string())
        }
        None => Some(format!("http://{trimmed}")),
    }
}

/// Where the pane's back and forward buttons point.
///
/// Empty until the first [`push`](History::push); `current` is `None` in that state, which
/// is the pane's "nothing loaded yet" and renders as a blank view rather than a fake page.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct History {
    entries: Vec<String>,
    /// Index into `entries` of the page being shown. Only `None` while `entries` is empty.
    cursor: Option<usize>,
}

impl History {
    pub fn new() -> Self {
        Self::default()
    }

    /// The URL currently being shown, or `None` before anything has been loaded.
    pub fn current(&self) -> Option<&str> {
        self.cursor.map(|index| self.entries[index].as_str())
    }

    /// Records a navigation to `url` and makes it current.
    ///
    /// Re-navigating to the URL already showing is *not* recorded. Otherwise a reload, or a
    /// double-press of Enter in the address bar, would stack duplicate entries and make back
    /// appear to do nothing — the button would be enabled and the page would not change.
    pub fn push(&mut self, url: impl Into<String>) {
        let url = url.into();
        if self.current() == Some(url.as_str()) {
            return;
        }
        // Going somewhere new abandons the forward branch, exactly as a browser does.
        if let Some(index) = self.cursor {
            self.entries.truncate(index + 1);
        }
        self.entries.push(url);
        self.cursor = Some(self.entries.len() - 1);
    }

    pub fn can_go_back(&self) -> bool {
        matches!(self.cursor, Some(index) if index > 0)
    }

    pub fn can_go_forward(&self) -> bool {
        matches!(self.cursor, Some(index) if index + 1 < self.entries.len())
    }

    /// Steps back and returns the URL now current, or `None` if there was nowhere to go.
    pub fn go_back(&mut self) -> Option<&str> {
        if !self.can_go_back() {
            return None;
        }
        let index = self.cursor? - 1;
        self.cursor = Some(index);
        Some(self.entries[index].as_str())
    }

    /// Steps forward and returns the URL now current, or `None` if there was nowhere to go.
    pub fn go_forward(&mut self) -> Option<&str> {
        if !self.can_go_forward() {
            return None;
        }
        let index = self.cursor? + 1;
        self.cursor = Some(index);
        Some(self.entries[index].as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_bare_host_and_port_becomes_an_http_url() {
        // What a person actually types after `artisan serve` prints its line.
        assert_eq!(normalize_url("localhost:8000").as_deref(), Some("http://localhost:8000"));
        assert_eq!(normalize_url("127.0.0.1/orders").as_deref(), Some("http://127.0.0.1/orders"));
        assert_eq!(normalize_url("  localhost  ").as_deref(), Some("http://localhost"));
    }

    #[test]
    fn an_explicit_scheme_is_left_alone() {
        // Including https — the guess only applies when there is nothing to respect.
        assert_eq!(normalize_url("https://example.test").as_deref(), Some("https://example.test"));
        assert_eq!(
            normalize_url("http://localhost:8000/a?b=c#d").as_deref(),
            Some("http://localhost:8000/a?b=c#d")
        );
    }

    #[test]
    fn what_does_not_name_a_destination_is_refused() {
        // Refusing beats guessing: this pane has no search engine to fall back to, and
        // navigating somewhere the user did not type would misrepresent what they see.
        assert_eq!(normalize_url(""), None);
        assert_eq!(normalize_url("   "), None);
        assert_eq!(normalize_url("http://"), None);
        assert_eq!(normalize_url("://nope"), None);
        assert_eq!(normalize_url("how do i fix this"), None);
    }

    #[test]
    fn the_default_is_artisan_serves_address() {
        // A guess, and only a guess — but the right one for a Laravel project.
        assert_eq!(normalize_url(DEFAULT_DEV_URL).as_deref(), Some(DEFAULT_DEV_URL));
    }

    #[test]
    fn a_fresh_history_shows_nothing_and_offers_nothing() {
        let history = History::new();
        assert_eq!(history.current(), None);
        assert!(!history.can_go_back());
        assert!(!history.can_go_forward());
    }

    #[test]
    fn back_and_forward_walk_the_entries() {
        let mut history = History::new();
        history.push("http://localhost:8000");
        history.push("http://localhost:8000/orders");
        history.push("http://localhost:8000/orders/1");

        assert!(history.can_go_back());
        assert!(!history.can_go_forward());

        assert_eq!(history.go_back(), Some("http://localhost:8000/orders"));
        assert_eq!(history.go_back(), Some("http://localhost:8000"));
        assert_eq!(history.go_back(), None, "the first page has nothing behind it");
        assert_eq!(history.current(), Some("http://localhost:8000"));

        assert_eq!(history.go_forward(), Some("http://localhost:8000/orders"));
        assert_eq!(history.current(), Some("http://localhost:8000/orders"));
    }

    #[test]
    fn navigating_after_going_back_drops_the_forward_branch() {
        // The button must not offer a page the user has walked away from.
        let mut history = History::new();
        history.push("http://a.test");
        history.push("http://b.test");
        history.go_back();
        history.push("http://c.test");

        assert_eq!(history.current(), Some("http://c.test"));
        assert!(!history.can_go_forward(), "b.test is gone, and forward must agree");
        assert_eq!(history.go_back(), Some("http://a.test"));
    }

    #[test]
    fn reloading_the_same_url_does_not_stack_an_entry() {
        // Otherwise back would be enabled and pressing it would appear to do nothing.
        let mut history = History::new();
        history.push("http://localhost:8000");
        history.push("http://localhost:8000");
        history.push("http://localhost:8000");

        assert!(!history.can_go_back());
        assert_eq!(history.current(), Some("http://localhost:8000"));
    }

    #[test]
    fn returning_to_an_earlier_url_by_typing_it_is_a_new_entry() {
        // Distinct from the reload case: the URL is not the one currently showing, so it
        // is a real navigation and back must return to where the user came from.
        let mut history = History::new();
        history.push("http://a.test");
        history.push("http://b.test");
        history.push("http://a.test");

        assert_eq!(history.current(), Some("http://a.test"));
        assert_eq!(history.go_back(), Some("http://b.test"));
    }
}
