//! Ghost-text autocomplete (#29): the suggestion, its validity rule, and the request.
//!
//! # Deliberately separate from the completion popup
//!
//! #20's popup makes provenance claims item by item; a ghost makes exactly one claim —
//! "a model guessed this" — and renders it dimmed so it can never be confused with an
//! LSP answer. The two share nothing but the cursor, so they share no code.
//!
//! # What leaves the machine
//!
//! A window of the **current file only**: up to [`PREFIX_LINES`] lines ending exactly at
//! the cursor and up to [`SUFFIX_LINES`] lines after it. Never another file, never the
//! project — the roadmap's rule for #29, and narrower on purpose than the chat panel's
//! explicit chips (#99). Nothing here fires unless `ai.autocomplete` is on, and that is
//! off by default.
//!
//! # Cancellation is dropping (#93)
//!
//! The request future owns the `curl` child with `kill_on_drop`, and the view owns the
//! request as a `gpui::Task`. A newer trigger replaces the task; the drop cancels the
//! future, the future drops the child, and the kernel kills the process. No flags, no
//! reaper thread — the same "dropping it cancels it" contract the caret blink documents.

use crate::ai::{self, Provider, StreamEvent, Turn};
use crate::editor::Document;
use elle_text::Version;

/// Lines of context before the cursor. The prefix ends exactly at the cursor byte, so
/// the model completes from mid-token when the user pauses mid-token.
pub const PREFIX_LINES: usize = 60;

/// Lines of context after the cursor, so the model can see what it must not repeat.
pub const SUFFIX_LINES: usize = 20;

/// The explicit insertion marker between prefix and suffix in the user turn.
pub const CURSOR_MARKER: &str = "<|cursor|>";

/// Typing pause before a request fires. ~400ms is the roadmap's number: long enough that
/// a burst of keystrokes costs zero requests, short enough to feel like completion.
pub const DEBOUNCE: std::time::Duration = std::time::Duration::from_millis(400);

/// A completion is an insertion, not an essay. 256 tokens is a handful of lines, and a
/// hard cap is also the cost cap — this fires on typing pauses, not on demand.
pub const MAX_TOKENS: u32 = 256;

/// Completion-tuned, because the chat endpoint is the only endpoint the provider layer
/// speaks (#99 kept it to one). The "no repetition of the prefix" line is load-bearing:
/// models echo the current line otherwise, and [`clean_completion`] can only strip the
/// echoes this prompt has made rare.
pub const SYSTEM_PROMPT: &str = "You are a code-completion engine. Given a code prefix \
    and suffix around a <|cursor|> marker, output ONLY the text to insert at the cursor. \
    No prose, no markdown fences, no repetition of the prefix.";

/// One suggestion, pinned to the exact buffer state it was made for.
///
/// The stamp is the whole correctness story: a ghost is only ever *shown* or *accepted*
/// through [`GhostSuggestion::is_valid_for`], so a stale one — the buffer edited, the
/// cursor moved, a selection made — simply stops existing rather than inserting at an
/// offset that now means something else.
pub struct GhostSuggestion {
    /// The text Tab inserts at the cursor, already cleaned.
    pub text: String,
    /// The cursor offset the suggestion was made at.
    pub at_offset: usize,
    /// The buffer version the suggestion was made against.
    pub version: Version,
}

impl GhostSuggestion {
    /// Whether the document is still exactly the one this suggestion was made for.
    ///
    /// Version covers every edit (undo included — versions only move forward); the head
    /// check covers movement without edits; the emptiness and multi-cursor checks cover
    /// states where "insert at the cursor" is not one well-defined place.
    pub fn is_valid_for(&self, document: &Document) -> bool {
        document.buffer.version() == self.version
            && document.selection.is_empty()
            && document.selection.head == self.at_offset
            && !document.has_multiple_cursors()
    }
}

/// The user turn: prefix, marker, suffix — and nothing else from the machine.
///
/// `cursor` must lie on a char boundary (the selection head always does).
pub fn build_user_turn(text: &str, cursor: usize) -> String {
    let (prefix, suffix) = context_window(text, cursor);
    format!("{prefix}{CURSOR_MARKER}{suffix}")
}

/// The context slices: at most [`PREFIX_LINES`] lines ending exactly at the cursor, at
/// most [`SUFFIX_LINES`] lines after it. Counting newlines rather than using the buffer's
/// line index keeps this a pure function over `&str`, which is what makes it testable
/// without a `Document`.
fn context_window(text: &str, cursor: usize) -> (&str, &str) {
    let cursor = cursor.min(text.len());
    let (before, after) = text.split_at(cursor);

    // The prefix holds the cursor's (partial) line plus full lines above it; the window
    // starts one byte after the PREFIX_LINESth newline counting backwards.
    let mut start = 0;
    let mut seen = 0;
    for (index, byte) in before.bytes().enumerate().rev() {
        if byte == b'\n' {
            seen += 1;
            if seen == PREFIX_LINES {
                start = index + 1;
                break;
            }
        }
    }

    // The suffix ends just after the SUFFIX_LINESth newline, so the last included line
    // arrives complete rather than cut mid-way — a truncated line reads as code to finish.
    let mut end = after.len();
    let mut seen = 0;
    for (index, byte) in after.bytes().enumerate() {
        if byte == b'\n' {
            seen += 1;
            if seen == SUFFIX_LINES {
                end = index + 1;
                break;
            }
        }
    }

    (&before[start..], &after[..end])
}

/// Post-processes a raw model reply into insertable text. Pure, and the empty string
/// means "show nothing" — the caller never renders an empty ghost.
///
/// `line_before_cursor` is the cursor line's text up to the cursor: the part of the
/// prefix models echo most, despite the system prompt. Stripping it (exact, or with the
/// indentation dropped) turns "echo plus completion" back into just the completion.
pub fn clean_completion(raw: &str, line_before_cursor: &str) -> String {
    let mut text = raw;

    // A leading fence line (```php, ```) is markdown wrapping, not code. Only when a
    // fence is actually present is leading whitespace trimmed — a bare completion may
    // legitimately begin with a newline or indentation.
    let trimmed = text.trim_start();
    if trimmed.starts_with("```") {
        text = trimmed.find('\n').map_or("", |index| &trimmed[index + 1..]);
    }

    // The closing fence, if the reply ends with one.
    let end_trimmed = text.trim_end();
    if let Some(base) = end_trimmed.strip_suffix("```") {
        text = base;
    }

    // The echo of the current line's tail. Exact first; then with the indentation
    // stripped, because models re-type `$foo->` but rarely re-type the leading spaces.
    if !line_before_cursor.is_empty() {
        if let Some(rest) = text.strip_prefix(line_before_cursor) {
            text = rest;
        } else {
            let unindented = line_before_cursor.trim_start();
            if !unindented.is_empty()
                && let Some(rest) = text.strip_prefix(unindented)
            {
                text = rest;
            }
        }
    }

    // Trailing whitespace-only lines carry nothing Tab should insert.
    let text = text.trim_end();
    if text.trim().is_empty() { String::new() } else { text.to_string() }
}

/// Runs one completion request end to end: auth, body, `curl`, SSE accumulation.
///
/// **Blocking-adjacent and cancellable**: `resolve_auth` shells out to the Keychain, so
/// this must run via `background_spawn` (ADR-0007), and the child is `kill_on_drop` so
/// dropping this future *is* the cancel — see the module doc. Streaming display is not
/// wanted for a ghost (#29 shows only complete suggestions), so deltas are accumulated
/// and returned whole.
///
/// Errors are strings for the log, not the user: a completion that fails is silence,
/// the same rule the update check follows — nobody asked a question, so no answer is owed.
pub async fn fetch_completion(
    provider: Provider,
    base_url: String,
    model: String,
    user_turn: String,
) -> Result<String, String> {
    use smol::io::{AsyncBufReadExt, AsyncWriteExt};
    use smol::stream::StreamExt;

    let auth = ai::resolve_auth(provider, &base_url)?;
    let wire = provider.wire();
    let body = ai::chat_body(
        wire,
        &model,
        SYSTEM_PROMPT,
        &[Turn { role: "user", content: user_turn }],
        MAX_TOKENS,
    );

    let mut child = smol::process::Command::new("curl")
        .args(ai::curl_args(&auth))
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .map_err(|err| format!("could not run curl: {err}"))?;

    // The body rides stdin (`--data-binary @-`), so it never shows in `ps` — the same
    // reason `curl_args` chose it. Dropping the handle closes the pipe, which is what
    // tells curl the body is complete.
    if let Some(mut stdin) = child.stdin.take() {
        stdin
            .write_all(body.as_bytes())
            .await
            .map_err(|err| format!("could not send request body: {err}"))?;
    }

    let stdout = child.stdout.take().ok_or("curl stdout unavailable")?;
    let mut lines = smol::io::BufReader::new(stdout).lines();
    let mut completion = String::new();
    // Lines that are not SSE: on an auth failure or a bad model the server answers with
    // a plain JSON error body, kept so the log can say why instead of "empty".
    let mut stray = String::new();

    while let Some(line) = lines.next().await {
        let line = line.map_err(|err| format!("stream read failed: {err}"))?;
        match ai::parse_sse(wire, &line) {
            Some(StreamEvent::Delta(delta)) => completion.push_str(&delta),
            Some(StreamEvent::Done) => break,
            Some(StreamEvent::Error(message)) => return Err(message),
            None => stray.push_str(&line),
        }
    }

    // Reap rather than leak: the child exited (or is about to), and `status` collects it.
    let _ = child.status().await;

    if completion.is_empty()
        && let Some(message) = ai::parse_error_body(&stray)
    {
        return Err(message);
    }
    Ok(completion)
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- the context window ----------------------------------------------------------

    #[test]
    fn the_prefix_ends_exactly_at_the_cursor_and_the_marker_sits_between() {
        let text = "line one\nline two\nline three\n";
        let cursor = text.find("two").unwrap() + 1; // mid-token: "t|wo"
        let turn = build_user_turn(text, cursor);
        assert_eq!(turn, format!("line one\nline t{CURSOR_MARKER}wo\nline three\n"));
    }

    #[test]
    fn the_prefix_is_capped_at_sixty_lines() {
        let mut text = String::new();
        for i in 0..200 {
            text.push_str(&format!("line {i}\n"));
        }
        let cursor = text.len();
        let (prefix, suffix) = context_window(&text, cursor);
        // The cursor sits after line 199's newline, on an empty final line — that empty
        // line plus 59 full lines above it is the 60-line window, starting at line 141.
        assert!(prefix.starts_with("line 141\n"), "prefix starts at {:?}", &prefix[..12]);
        assert_eq!(prefix.lines().count(), PREFIX_LINES - 1);
        assert!(suffix.is_empty());
    }

    #[test]
    fn the_suffix_is_capped_at_twenty_lines_and_ends_on_a_complete_line() {
        let mut text = String::from("cursor here\n");
        for i in 0..100 {
            text.push_str(&format!("after {i}\n"));
        }
        let (prefix, suffix) = context_window(&text, 0);
        assert!(prefix.is_empty());
        assert_eq!(suffix.lines().count(), SUFFIX_LINES);
        // Complete lines only: the window ends just after a newline, never mid-line.
        assert!(suffix.ends_with("after 18\n"), "suffix ends {:?}", &suffix[suffix.len() - 12..]);
    }

    #[test]
    fn a_small_file_is_sent_whole() {
        let text = "one\ntwo\nthree";
        let cursor = text.find("three").unwrap();
        let (prefix, suffix) = context_window(text, cursor);
        assert_eq!(prefix, "one\ntwo\n");
        assert_eq!(suffix, "three");
    }

    // --- clean_completion ------------------------------------------------------------

    #[test]
    fn fences_are_stripped_from_both_ends() {
        assert_eq!(clean_completion("```php\n$a = 1;\n```", ""), "$a = 1;");
        assert_eq!(clean_completion("```\nreturn $x;\n```\n", ""), "return $x;");
    }

    #[test]
    fn an_echo_of_the_line_tail_is_dropped() {
        // The model re-typed the prefix it was told not to repeat.
        assert_eq!(clean_completion("$user->na", "$user->na"), "");
        assert_eq!(clean_completion("$user->name;", "$user->"), "name;");
        // And the indentation-free echo, which is the common shape.
        assert_eq!(clean_completion("$user->save();", "    $user->"), "save();");
    }

    #[test]
    fn whitespace_only_output_becomes_the_empty_string() {
        assert_eq!(clean_completion("", ""), "");
        assert_eq!(clean_completion("\n  \n", ""), "");
        assert_eq!(clean_completion("```\n\n```", ""), "");
    }

    #[test]
    fn trailing_blank_lines_are_trimmed_but_leading_structure_is_kept() {
        // A completion that *starts* with a newline is starting a new line — meaningful.
        assert_eq!(clean_completion("\n    return $x;\n\n  \n", ""), "\n    return $x;");
    }

    // --- the validity stamp ----------------------------------------------------------

    #[test]
    fn a_ghost_dies_with_any_edit_or_cursor_move() {
        let mut document = Document::new(None, "<?php\n$a = 1;\n", true).expect("plain doc");
        let offset = document.buffer.text().find("1;").unwrap() + 2;
        document.move_to(offset, false);

        let ghost = GhostSuggestion {
            text: "\n$b = 2;".to_string(),
            at_offset: offset,
            version: document.buffer.version(),
        };
        assert!(ghost.is_valid_for(&document), "fresh ghost at the cursor is valid");

        // A cursor move alone invalidates…
        document.move_to(0, false);
        assert!(!ghost.is_valid_for(&document));
        // …moving back revalidates (same buffer, same offset — the suggestion still
        // describes exactly this insertion point)…
        document.move_to(offset, false);
        assert!(ghost.is_valid_for(&document));
        // …a selection invalidates ("at the cursor" is no longer one place)…
        document.move_to(0, true);
        assert!(!ghost.is_valid_for(&document));
        document.move_to(offset, false);
        // …and any edit invalidates for good: versions only move forward, so even undo
        // cannot resurrect a stale ghost.
        document.insert("x");
        document.undo();
        document.move_to(offset, false);
        assert!(!ghost.is_valid_for(&document));
    }
}
