//! Smooth streaming reveal for the AI chat panel.
//!
//! Model deltas arrive in bursts — the wire sends a paragraph, then nothing, then three
//! more — and appending them raw makes the transcript jump in choppy chunks. Zed's agent
//! panel solves this with a small buffer drained on a timer: received text is *pending*
//! until a tick reveals a slice of it, sized so the whole backlog drains over about
//! [`REVEAL_WINDOW`]. A big burst reveals faster (the rate adapts to the backlog), so the
//! reveal never falls behind the stream; a trickle reveals a character or two per tick,
//! which reads as typing.
//!
//! The buffer is deliberately pure — no timer, no channel, no `Context` — so the whole
//! policy is testable as arithmetic. The panel owns the 16 ms tick and calls
//! [`RevealBuffer::take_reveal`]; ordering with non-text events (an activity row, a
//! proposal card) is the panel's job, via [`RevealBuffer::flush`] before it applies them.

use std::time::Duration;

/// One frame at ~60 fps: how often the panel's reveal task ticks.
pub const REVEAL_TICK: Duration = Duration::from_millis(16);

/// The backlog drains over roughly this long, regardless of its size.
pub const REVEAL_WINDOW: Duration = Duration::from_millis(200);

/// Received-but-not-yet-shown text.
#[derive(Default)]
pub struct RevealBuffer {
    pending: String,
}

impl RevealBuffer {
    /// Adds a delta to the backlog.
    pub fn push(&mut self, delta: &str) {
        self.pending.push_str(delta);
    }

    pub fn is_empty(&self) -> bool {
        self.pending.is_empty()
    }

    /// The slice one tick reveals: a fraction of the backlog sized to drain it over
    /// [`REVEAL_WINDOW`], at least one character, snapped **forward** to a char boundary
    /// so a multibyte character is never split. `None` when there is nothing pending.
    pub fn take_reveal(&mut self, tick: Duration) -> Option<String> {
        if self.pending.is_empty() {
            return None;
        }
        let fraction = tick.as_millis() as f32 / REVEAL_WINDOW.as_millis().max(1) as f32;
        let mut cut = ((self.pending.len() as f32 * fraction).ceil() as usize)
            .clamp(1, self.pending.len());
        while !self.pending.is_char_boundary(cut) {
            cut += 1;
        }
        let rest = self.pending.split_off(cut);
        Some(std::mem::replace(&mut self.pending, rest))
    }

    /// Everything at once — called before any non-text event lands in the transcript,
    /// so the order the user reads is the order things happened.
    pub fn flush(&mut self) -> String {
        std::mem::take(&mut self.pending)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_tick_reveals_a_fraction_of_the_backlog_and_at_least_one_char() {
        let mut buffer = RevealBuffer::default();
        buffer.push(&"x".repeat(1000));
        // 16/200 of 1000 = 80 characters per tick.
        assert_eq!(buffer.take_reveal(REVEAL_TICK).unwrap().len(), 80);
        // A tiny backlog still moves: minimum one character.
        let mut small = RevealBuffer::default();
        small.push("ab");
        assert_eq!(small.take_reveal(REVEAL_TICK).unwrap(), "a");
        assert_eq!(small.take_reveal(REVEAL_TICK).unwrap(), "b");
        assert_eq!(small.take_reveal(REVEAL_TICK), None);
    }

    #[test]
    fn multibyte_characters_are_never_split() {
        let mut buffer = RevealBuffer::default();
        buffer.push("é"); // two bytes; a one-byte cut must widen, not slice
        let out = buffer.take_reveal(REVEAL_TICK).unwrap();
        assert_eq!(out, "é");
        assert!(buffer.is_empty());

        let mut emoji = RevealBuffer::default();
        emoji.push("😀😀"); // 4 bytes each; every reveal lands on a boundary
        let mut all = String::new();
        while let Some(part) = emoji.take_reveal(REVEAL_TICK) {
            all.push_str(&part);
        }
        assert_eq!(all, "😀😀");
    }

    #[test]
    fn reveals_are_monotone_and_lossless() {
        let mut buffer = RevealBuffer::default();
        let input = "the reply, revealed over ticks, arrives whole and in order — ação";
        buffer.push(input);
        let mut all = String::new();
        while let Some(part) = buffer.take_reveal(REVEAL_TICK) {
            assert!(!part.is_empty(), "an empty reveal would spin the task forever");
            all.push_str(&part);
        }
        assert_eq!(all, input);
    }

    #[test]
    fn flush_empties_the_backlog_in_one_piece() {
        let mut buffer = RevealBuffer::default();
        buffer.push("first ");
        buffer.push("second");
        assert_eq!(buffer.flush(), "first second");
        assert!(buffer.is_empty());
        assert_eq!(buffer.flush(), "");
    }
}
