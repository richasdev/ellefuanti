#!/bin/sh
# Fail when idle memory or binary size drifts past its threshold (#84).
#
# This exists because idle RSS went 69 → 105 MB across twenty PRs and nothing noticed (#79).
# Every one of those PRs measured its own cost and every one was cheap in isolation; none
# measured the aggregate. Per-feature measurement does not catch aggregate drift, so the
# aggregate needs its own check.
#
# WHAT BLOCKS AND WHY:
#
#   idle RSS      blocking   held inside 100–103 MB across six runs of one binary
#   binary size   blocking   byte-identical for a given source tree
#   startup       reported   the same binary measured 737 ms on its first launch and
#                            216–236 ms on every launch after — 3.4x, no code change,
#                            purely dyld and page-cache state
#
# The startup number is printed on every run so a 2× move is visible, but it never fails the
# build. BASELINE.md's own history is two entries about trusting a timing number that was
# measuring the harness; a gate that flaps gets disabled, which is worse than no gate.
#
# Usage: scripts/perf-gate.sh [--build]
#   --build   build the release binary first (otherwise an existing one is required)
set -eu

ROOT=$(cd "$(dirname "$0")/.." && pwd)
BIN=$ROOT/target/release/ellefuanti

# Thresholds. Set above the measured value with headroom for run-to-run variation, not at
# it — a gate that fires on noise is a gate someone deletes. See BASELINE.md for the
# measurements these come from and the reasoning behind each margin.
RSS_LIMIT_MB=125
BIN_LIMIT_MB=17

# How long to let the process settle before believing its RSS. Measured: RSS is still
# climbing for the first several seconds after launch, so reading it immediately measures
# something mid-allocation rather than the idle state. Three samples, because one sample
# cannot tell "settled" from "caught at a lucky moment".
SETTLE_SECONDS=20
SAMPLES=3
SAMPLE_GAP=5

if [ "${1:-}" = "--build" ]; then
	cargo build --release -p ellefuanti
fi

# After the build, not instead of it. The first RSS measurement taken for #79 was wrong
# because a parallel cargo build was saturating the machine, and `--build` finishing says
# nothing about whether *someone else's* build is still running — on a machine with several
# agents or a background `cargo check`, it usually is. Refuse rather than report a
# contaminated number: a wrong measurement that looks fine is the exact failure this gate
# exists to prevent.
#
# -x matches the executable name exactly, so a shell whose command line merely contains the
# word "cargo" (an editor, a wrapper script) does not trip it — only a real compiler does.
if pgrep -qx 'cargo|rustc|cc1plus|clang' 2>/dev/null; then
	echo "a build is running (cargo/rustc/clang); memory and timing here would be contaminated" >&2
	echo "wait for it to finish, then re-run" >&2
	exit 2
fi

if [ ! -x "$BIN" ]; then
	echo "no release binary at $BIN" >&2
	echo "run: scripts/perf-gate.sh --build" >&2
	exit 2
fi

failed=0

# --- Binary size -----------------------------------------------------------------------
# Deterministic and free to measure, and it moved 7.63 → 14.53 MB in one session with no
# single change owning more than a fraction of it. Reported always, so a jump is a decision.
bin_bytes=$(wc -c <"$BIN" | tr -d ' ')
# Compared in *bytes*, printed in MiB to two decimals. Comparing truncated whole MiB is the
# obvious version and it is wrong: `$((bytes / 1048576))` makes a 14.53 MB binary read as 14,
# which passes a 14 MB limit. That was not a hypothetical — the first run of this gate with
# the limit deliberately lowered to 14 reported 14.53 MB and did not fail, so nearly a whole
# megabyte of growth could land under any limit before the check noticed.
bin_limit_bytes=$((BIN_LIMIT_MB * 1048576))
printf 'binary       %6s MB   (limit %s MB)\n' \
	"$(awk -v b="$bin_bytes" 'BEGIN { printf "%.2f", b / 1048576 }')" "$BIN_LIMIT_MB"
if [ "$bin_bytes" -gt "$bin_limit_bytes" ]; then
	echo "FAIL: binary is $bin_bytes bytes, over the ${BIN_LIMIT_MB} MB limit" >&2
	failed=1
fi

# --- Idle RSS and startup --------------------------------------------------------------
# Redirect to a file rather than piping: a pipe through grep inside a backgrounded job
# buffers, and killing the app then discards everything it wrote (learned in ci.yml).
log=$(mktemp)
ELLE_PERF=1 "$BIN" >"$log" 2>&1 &
app=$!
# shellcheck disable=SC2064  # $app and $log must expand now, not at trap time.
trap "kill $app 2>/dev/null || true; rm -f $log" EXIT INT TERM

sleep "$SETTLE_SECONDS"
if ! kill -0 "$app" 2>/dev/null; then
	echo "the app exited before it could be measured; its output was:" >&2
	cat "$log" >&2
	exit 2
fi

# The worst of the samples, not the mean: the question is whether idle memory ever crosses
# the threshold, and averaging a spike away is how a regression gets reported as fine.
worst_kb=0
i=0
while [ "$i" -lt "$SAMPLES" ]; do
	[ "$i" -eq 0 ] || sleep "$SAMPLE_GAP"
	kb=$(ps -o rss= -p "$app" | tr -d ' ')
	[ -n "$kb" ] || { echo "the app died mid-sample" >&2; exit 2; }
	[ "$kb" -le "$worst_kb" ] || worst_kb=$kb
	i=$((i + 1))
done
# Compared in KB for the same reason as the binary above: truncating to whole MB first hides
# up to a megabyte of growth under the limit.
printf 'idle RSS     %6s MB   (limit %s MB, worst of %s samples over %ss)\n' \
	"$(awk -v k="$worst_kb" 'BEGIN { printf "%.1f", k / 1024 }')" \
	"$RSS_LIMIT_MB" "$SAMPLES" "$((SETTLE_SECONDS + (SAMPLES - 1) * SAMPLE_GAP))"
if [ "$worst_kb" -gt "$((RSS_LIMIT_MB * 1024))" ]; then
	echo "FAIL: idle RSS is ${worst_kb} KB, over the ${RSS_LIMIT_MB} MB limit" >&2
	failed=1
fi

# Reported, never blocking — see the header. Absent on a headless runner where the window
# never opens, which is a fact about the runner and not a reason to fail.
startup=$(grep 'startup/total' "$log" || true)
if [ -n "$startup" ]; then
	printf 'startup      %s\n' "$(echo "$startup" | sed 's/.*startup\/total *//')"
else
	echo 'startup      not reported (no window opened — headless?)'
fi
echo 'startup is reported, not gated: it is wall-clock and moves 2x with OS caching alone.'

exit "$failed"
