//! AI edit prediction — the ghost text's brain, Zed's shape adapted.
//!
//! Three parts, deliberately separated:
//!
//! - [`state`]: the prediction itself, its validity stamp, and **interpolation** — typing
//!   a prefix of the suggestion shrinks it in place instead of discarding it, the single
//!   biggest quality jump over the first ghost (`interpolate_edits` in Zed's
//!   `edit_prediction_types`).
//! - [`provider`]: the request — context window, prompt, `curl` + SSE, cleaning. Moved
//!   whole from `editor/ghost.rs`; the lifecycle in `editor/view.rs` is its only caller.
//! - The *lifecycle* (when to fire, when to land, when to throw away) stays in
//!   `editor/view.rs`, next to the cursor it serves.
//!
//! Rendering is unchanged: first line spliced into the cursor row, continuation lines as
//! a workspace overlay. What changed is the state machine underneath it.

pub mod provider;
pub mod state;
