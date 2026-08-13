#!/bin/sh
# Fail when idle memory, idle CPU or binary size drifts past its threshold (#84).
#
# This exists because idle memory looked like it went 69 → 105 MB across twenty PRs and
# nothing noticed (#79). Every one of those PRs measured its own cost and every one was cheap
# in isolation; none measured the aggregate. Per-feature measurement does not catch aggregate
# drift, so the aggregate needs its own check.
#
# WHAT BLOCKS AND WHY:
#
#   idle memory   blocking   vmmap Physical footprint, held inside 1 MB across eight runs
#   idle CPU      blocking   CPU-time delta over a wall-clock window, not ps %cpu
#   binary size   blocking   byte-identical for a given source tree
#   startup       reported   the same binary measured 737 ms on its first launch and
#                            216–236 ms on every launch after — 3.4x, no code change,
#                            purely dyld and page-cache state
#
# NEITHER METRIC IS THE OBVIOUS ONE, AND THAT IS THE POINT (#79 closed as a measurement
# artefact). `ps rss` and `ps %cpu` are what everyone reaches for first, and both of them
# invented a regression that had not happened:
#
#   ps rss    counts resident pages including GPU/IOSurface mappings whose residency the
#             kernel varies between launches of the *same* binary. Five launches of one
#             unchanged binary read 83.9–102.5 MB — a 19 MB spread on identical bytes, wider
#             than the "regression" #79 was opened about. vmmap's Physical footprint over
#             those same launches held at 75.7–76.8 MB.
#   ps %cpu   is a decaying average weighted over the process lifetime, not an instantaneous
#             rate. A process that burned CPU at startup keeps reading non-zero and then
#             decays toward zero regardless of what it is doing. Sampled once after a settle
#             it read 0.4–0.6% while the true rate was 0.80% — and read 0.0% at 26 s elapsed,
#             which is how BASELINE.md ended up recording an idle CPU that never existed.
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

# Thresholds. Set above the *measured spread* of each metric, not above a single reading —
# #90 set its limits ~20% above one measurement, which is reasonable practice but leaves the
# metric's own noise floor unknown, and for `ps rss` that floor turned out to be 19 MB wide.
#
# Measured on a quiet machine, one settled process, three samples 4s apart: footprint
# 76.4 / 78.8 / 78.6 MB, true CPU rate 0.75%. Those agree with the independent figures in
# #79 (75.7-76.8 MB, 0.5-0.9%), which is the check that matters — two methods disagreeing
# is what BASELINE.md says to treat as evidence about the harness.
#
# Two readings taken while getting here were wrong, and both failure modes are worth
# knowing: 41.9 MB from sampling at 12s while the process was still allocating, and
# 102.7 MB with a sibling instance still alive. Neither was the binary changing.
#
# The margins are deliberately loose. 95 MB is ~20% over the worst clean sample and still
# well under the ~103 MB a contaminated run produces, so the gate catches real growth
# without firing on a busy machine. 2% sits above gpui's own ~0.5-0.9% display-link floor,
# which is upstream and not ours to fix (#79).
MEM_LIMIT_MB=95
CPU_LIMIT_PCT=2
# 17 -> 19 with #53: tree-sitter-rust is ~1.3 MB and tree-sitter-md ~0.3 MB, measured
# from their rlibs — the same "price of the language at all" trade the bash grammar set
# the precedent for. The gate's job is catching *accidental* growth; a deliberate,
# attributed cost moves the line and says so here.
#
# THIS LIMIT IS PER ARCHITECTURE SLICE, NOT PER FILE, and that distinction is the whole
# reason the number did not move when the release became a universal binary.
#
# A universal binary is two complete copies of the same program in one file, so a fat
# arm64+x86_64 build weighs ~37 MB against an 18.84 MB arm64 slice. Measuring the *file*
# would mean either failing every release or roughly doubling the limit — and doubling it
# would silently re-admit ~18 MB of accidental growth in the thing the gate actually
# watches, which is our code. The limit is the size of one copy of ellefuanti; the number
# of copies stapled together is a packaging decision, not bloat.
#
# So each slice is extracted with `lipo -thin` and measured on its own, and every slice has
# to fit. A thin (single-arch) binary has no slices to extract and is measured directly,
# which is what a local `cargo build --release` produces.
BIN_LIMIT_MB=19

# How long to let the process settle before believing its memory. Measured: footprint is
# still climbing for the first several seconds after launch, so reading it immediately
# measures something mid-allocation rather than the idle state. Three samples, because one
# sample cannot tell "settled" from "caught at a lucky moment".
SETTLE_SECONDS=20
SAMPLES=3
SAMPLE_GAP=5

# The CPU window. A rate needs a wall-clock interval to divide by, and `ps -o time=` only
# resolves to 10 ms, so at ~0.6% a short window is mostly quantisation error: over 2 s the
# process accrues ~12 ms, one or two ticks. Ten seconds accrues ~60 ms and the tick becomes
# noise rather than the signal.
CPU_WINDOW_SECONDS=10

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
#
# `clippy-driver` was missing from this list until #80, and the omission produced exactly the
# failure the guard exists to prevent: the gate reported idle CPU at 8.00% — four times the
# limit — while `pgrep` said the machine was clear, because two `clippy-driver` processes
# were burning 114% between them and matched none of the four names. A guard that names
# compilers individually is only as good as its list, and rustc's toolchain ships more than
# one front end.
#
# `rustdoc` for the same reason: `cargo doc` is a compile by another name.
if pgrep -qx 'cargo|rustc|clippy-driver|rustdoc|cc1plus|clang' 2>/dev/null; then
	echo "a build is running (cargo/rustc/clippy/clang); memory and timing here would be contaminated" >&2
	echo "wait for it to finish, then re-run" >&2
	exit 2
fi

# Load average, not just process names. The list above can only catch compilers someone
# thought to name; a machine running several agents is loaded by things that are not
# compilers at all, and a 20-second settle on a box at load 9 measures the scheduler.
#
# 4.0 rather than 1.0 because this is a multi-core machine and a single busy process is not
# contamination — the reading that prompted this was taken at load 8.7, where three sibling
# agents were building simultaneously.
LOAD=$(sysctl -n vm.loadavg 2>/dev/null | awk '{print $2}')
if [ -n "$LOAD" ] && [ "$(awk -v l="$LOAD" 'BEGIN{print (l > 4.0) ? 1 : 0}')" = "1" ]; then
	echo "load average is $LOAD; memory and timing here would be contaminated" >&2
	echo "wait for the machine to go quiet, then re-run" >&2
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
# Compared in *bytes*, printed in MiB to two decimals. Comparing truncated whole MiB is the
# obvious version and it is wrong: `$((bytes / 1048576))` makes a 14.53 MB binary read as 14,
# which passes a 14 MB limit. That was not a hypothetical — the first run of this gate with
# the limit deliberately lowered to 14 reported 14.53 MB and did not fail, so nearly a whole
# megabyte of growth could land under any limit before the check noticed.
bin_limit_bytes=$((BIN_LIMIT_MB * 1048576))

# Which architectures are in there. `lipo -archs` prints them space-separated for a fat
# file; for a thin one it prints the single arch, which makes the two cases the same loop.
# Failure is not fatal — lipo is macOS-only and this gate already is, but a missing lipo
# should degrade to measuring the file rather than crashing the release.
archs=$(lipo -archs "$BIN" 2>/dev/null || true)

# One slice: measure the file. Extracting it would produce a byte-identical copy.
if [ -z "$archs" ] || [ "$(echo "$archs" | wc -w | tr -d ' ')" -le 1 ]; then
	bin_bytes=$(wc -c <"$BIN" | tr -d ' ')
	printf 'binary       %6s MB   (limit %s MB, %s)\n' \
		"$(awk -v b="$bin_bytes" 'BEGIN { printf "%.2f", b / 1048576 }')" \
		"$BIN_LIMIT_MB" "${archs:-thin}"
	if [ "$bin_bytes" -gt "$bin_limit_bytes" ]; then
		echo "FAIL: binary is $bin_bytes bytes, over the ${BIN_LIMIT_MB} MB limit" >&2
		failed=1
	fi
else
	# Fat: the limit applies to each slice on its own — see BIN_LIMIT_MB above for why the
	# file's total is the wrong number to gate on. Every slice is printed, so a build where
	# one architecture grew and the other did not is visible rather than averaged away.
	slice_dir=$(mktemp -d)
	for arch in $archs; do
		lipo -thin "$arch" "$BIN" -output "$slice_dir/$arch" 2>/dev/null || {
			echo "could not extract the $arch slice with lipo" >&2
			rm -rf "$slice_dir"
			exit 2
		}
		slice_bytes=$(wc -c <"$slice_dir/$arch" | tr -d ' ')
		printf 'binary       %6s MB   (limit %s MB, %s slice)\n' \
			"$(awk -v b="$slice_bytes" 'BEGIN { printf "%.2f", b / 1048576 }')" \
			"$BIN_LIMIT_MB" "$arch"
		if [ "$slice_bytes" -gt "$bin_limit_bytes" ]; then
			echo "FAIL: the $arch slice is $slice_bytes bytes, over the ${BIN_LIMIT_MB} MB limit" >&2
			failed=1
		fi
	done
	rm -rf "$slice_dir"
	# The file total is reported so the download size stays visible, but never gated: it is
	# the sum of the slices and moves whenever an architecture is added or dropped.
	printf 'binary       %6s MB   (universal file total, reported not gated)\n' \
		"$(awk -v b="$(wc -c <"$BIN" | tr -d ' ')" 'BEGIN { printf "%.2f", b / 1048576 }')"
fi

# --- Idle memory, idle CPU and startup ---------------------------------------------------
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
#
# vmmap prints "Physical footprint:  76.5M" — a unit suffix, not bytes, so it is converted to
# KB here and compared in KB. Same reason as the binary above: truncating to whole MB first
# hides up to a megabyte of growth under the limit.
worst_kb=0
i=0
while [ "$i" -lt "$SAMPLES" ]; do
	[ "$i" -eq 0 ] || sleep "$SAMPLE_GAP"
	kb=$(vmmap -summary "$app" 2>/dev/null |
		awk '/^Physical footprint:/ {
			v = $3 + 0; u = substr($3, length($3))
			if (u == "M") v *= 1024; else if (u == "G") v *= 1048576
			else if (u != "K") exit 1          # an unrecognised unit must not read as 0
			printf "%d", v; exit 0
		}')
	# An empty or unparsed reading would silently compare 0 against the limit and pass, which
	# is the truncation bug in another costume: a broken check that prints nothing wrong.
	case $kb in
	'' | *[!0-9]*)
		echo "could not read Physical footprint from vmmap (app dead, or vmmap denied?)" >&2
		exit 2
		;;
	esac
	[ "$kb" -le "$worst_kb" ] || worst_kb=$kb
	i=$((i + 1))
done
printf 'idle memory  %6s MB   (limit %s MB, worst of %s samples over %ss)\n' \
	"$(awk -v k="$worst_kb" 'BEGIN { printf "%.1f", k / 1024 }')" \
	"$MEM_LIMIT_MB" "$SAMPLES" "$((SETTLE_SECONDS + (SAMPLES - 1) * SAMPLE_GAP))"
if [ "$worst_kb" -gt "$((MEM_LIMIT_MB * 1024))" ]; then
	echo "FAIL: idle footprint is ${worst_kb} KB, over the ${MEM_LIMIT_MB} MB limit" >&2
	failed=1
fi

# --- Idle CPU ----------------------------------------------------------------------------
# A true rate: cumulative CPU time (`ps -o time=`) differenced across a known wall-clock
# window. Never `ps %cpu` — see the header for why that number invented a regression.
#
# WHAT THIS THRESHOLD IS FOR. The floor is not zero and is not ours: gpui 0.2.2 starts a
# CVDisplayLink on the first paint and never stops it, so the main thread is woken every
# vsync forever (`platform/mac/window.rs`, `display_layer` calls `start_display_link()` and
# `step` never cancels it). That costs ~0.5–0.9% continuously on an idle window and it is
# identical across every commit in this repo's history, which is exactly why the number never
# moved. The limit below therefore sits *above* that floor deliberately. This gate catches a
# new timer, poll or animation we add on top of gpui's wakeups; it does not and cannot catch
# gpui's own, and lowering it to chase them would just fail on every commit.
cpu_before=$(ps -o time= -p "$app" | tr -d ' ')
sleep "$CPU_WINDOW_SECONDS"
cpu_after=$(ps -o time= -p "$app" | tr -d ' ')
if [ -z "$cpu_after" ]; then
	echo "the app died during the CPU window" >&2
	exit 2
fi
# `ps -o time=` is [[hh:]mm:]ss.cc — split on both separators and weight from the right, so
# the parse does not depend on whether the process has been alive long enough to grow an
# hours field. Printed to two decimals because the interesting range is fractions of 1%.
cpu_pct=$(awk -v a="$cpu_before" -v b="$cpu_after" -v w="$CPU_WINDOW_SECONDS" '
	function secs(t,   n, p, s) {
		n = split(t, p, /[:.]/)
		s = p[n] / 100
		if (n >= 2) s += p[n - 1]
		if (n >= 3) s += p[n - 2] * 60
		if (n >= 4) s += p[n - 3] * 3600
		return s
	}
	BEGIN { printf "%.2f", (secs(b) - secs(a)) / w * 100 }')
printf 'idle CPU     %6s %%    (limit %s %%, true rate over %ss)\n' \
	"$cpu_pct" "$CPU_LIMIT_PCT" "$CPU_WINDOW_SECONDS"
# Compared in hundredths of a percent, integer-only: `[` cannot compare decimals, and the
# tempting `${cpu_pct%%.*}` truncation would let 1.99% pass a 1% limit.
cpu_cc=$(awk -v p="$cpu_pct" 'BEGIN { printf "%d", p * 100 + 0.5 }')
if [ "$cpu_cc" -gt "$(awk -v l="$CPU_LIMIT_PCT" 'BEGIN { printf "%d", l * 100 + 0.5 }')" ]; then
	echo "FAIL: idle CPU is ${cpu_pct}%, over the ${CPU_LIMIT_PCT}% limit" >&2
	echo "gpui's display link accounts for ~0.5-0.9%; anything above this limit is ours" >&2
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
