//! Startup and frame-time measurement.
//!
//! §21 sets budgets (cold start < 500 ms, frame < 8.3 ms at 120 fps) and §26 forbids
//! guessing at them. This module is how they get *measured* rather than asserted:
//! `ELLE_PERF=1 cargo run` prints real timings to stderr.
//!
//! Kept deliberately small. It reports what the process observed and does nothing else —
//! no sampling profiler, no flamegraphs, no metrics backend. When a number here looks
//! wrong, reach for Instruments; this only tells you *which* number to chase.

use std::time::{Duration, Instant};

/// Set `ELLE_PERF=1` to print startup and frame timings.
///
/// Off by default because a release user gains nothing from it, and an always-on frame
/// logger is itself a per-frame cost.
pub fn enabled() -> bool {
    std::env::var_os("ELLE_PERF").is_some_and(|value| value != "0")
}

/// Startup phase timings, measured from process entry.
pub struct Startup {
    process_start: Instant,
    last: Instant,
}

impl Startup {
    /// Call as the first statement in `main`, so the clock includes everything the process
    /// does — including the runtime Metal shader compilation that `runtime_shaders` moves
    /// from build time to startup, which is the prime suspect if the budget is missed.
    pub fn begin() -> Self {
        let now = Instant::now();
        Self { process_start: now, last: now }
    }

    /// Records the time since the previous phase.
    pub fn phase(&mut self, name: &str) {
        let now = Instant::now();
        let since_last = now.duration_since(self.last);
        let total = now.duration_since(self.process_start);
        self.last = now;

        if enabled() {
            eprintln!("[perf] startup/{name:<16} +{:>8.2?}  (total {:>8.2?})", since_last, total);
        }
        tracing::debug!(phase = name, ?since_last, ?total, "startup phase");
    }

    /// Called once the window is on screen. Reports against the §21 budget.
    pub fn ready(&self) {
        let total = self.process_start.elapsed();
        // The budget from §21. Cold start is the number a user feels on every launch.
        const COLD_BUDGET: Duration = Duration::from_millis(500);

        let verdict = if total <= COLD_BUDGET { "within" } else { "OVER" };
        if enabled() {
            eprintln!(
                "[perf] startup/total          {total:>8.2?}  ({verdict} the {COLD_BUDGET:?} budget)"
            );
        }
        if total > COLD_BUDGET {
            // A missed budget is a defect, not a curiosity: say so at warn level even
            // without ELLE_PERF, because nobody profiles what they never hear about.
            tracing::warn!(?total, ?COLD_BUDGET, "cold startup exceeded its budget");
        } else {
            tracing::info!(?total, "startup complete");
        }
    }
}

/// Rolling frame-time tracker.
///
/// Reports the slowest frame in each window of samples rather than the mean: an average
/// hides exactly the stutter a user notices, and one 40 ms frame in a hundred is a bug
/// while a 2 ms mean is not evidence of its absence.
pub struct FrameTimer {
    last_frame: Option<Instant>,
    worst: Duration,
    count: u32,
    /// Frames per report. ~2 s at 60 fps: often enough to catch a regression while
    /// scrolling, rare enough not to flood the terminal.
    window: u32,
}

impl FrameTimer {
    pub fn new() -> Self {
        Self { last_frame: None, worst: Duration::ZERO, count: 0, window: 120 }
    }

    /// Call once per rendered frame.
    pub fn tick(&mut self) {
        let now = Instant::now();

        if let Some(last) = self.last_frame {
            let elapsed = now.duration_since(last);
            // Frames arrive only when something changes, so a long idle gap is not a slow
            // frame. Anything beyond a few frames' worth of time is a wake-up, not a stall.
            if elapsed < Duration::from_millis(100) {
                self.worst = self.worst.max(elapsed);
                self.count += 1;
            }
        }
        self.last_frame = Some(now);

        if self.count >= self.window {
            // 8.3 ms is the 120 fps budget; 16.7 ms is 60 fps. Report against the former
            // and name the latter, since missing 120 but holding 60 is a different problem.
            const BUDGET_120: Duration = Duration::from_micros(8_333);
            let worst = self.worst;

            if enabled() {
                let verdict = if worst <= BUDGET_120 {
                    "120fps"
                } else if worst <= Duration::from_micros(16_667) {
                    "60fps"
                } else {
                    "DROPPED"
                };
                eprintln!(
                    "[perf] frame/worst-of-{:<3}     {worst:>8.2?}  ({verdict})",
                    self.window
                );
            }
            if worst > Duration::from_micros(16_667) {
                tracing::warn!(?worst, "a frame missed even the 60fps budget");
            }

            self.worst = Duration::ZERO;
            self.count = 0;
        }
    }
}

impl Default for FrameTimer {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_timer_ignores_idle_gaps() {
        let mut timer = FrameTimer::new();
        timer.tick();
        // Simulate the app sitting idle: the next frame is minutes later, which is a
        // wake-up rather than a 3-minute frame.
        timer.last_frame = Some(Instant::now() - Duration::from_secs(180));
        timer.tick();
        assert_eq!(timer.worst, Duration::ZERO, "an idle gap must not count as a slow frame");
        assert_eq!(timer.count, 0);
    }

    #[test]
    fn frame_timer_records_the_worst_not_the_mean() {
        let mut timer = FrameTimer::new();
        timer.tick();
        for gap_ms in [1u64, 1, 30, 1, 1] {
            timer.last_frame = Some(Instant::now() - Duration::from_millis(gap_ms));
            timer.tick();
        }
        assert!(
            timer.worst >= Duration::from_millis(30),
            "the 30ms outlier must survive, got {:?}",
            timer.worst
        );
    }

    #[test]
    fn startup_phases_accumulate() {
        let mut startup = Startup::begin();
        startup.phase("first");
        let after_first = startup.last;
        startup.phase("second");
        assert!(startup.last >= after_first);
        assert!(startup.process_start <= after_first);
    }
}
