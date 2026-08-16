#!/usr/bin/env bash
# Gathers everything needed to diagnose a crash, in one paste-able block.
#
# The app detaches from the terminal and sends stderr to /dev/null, so a panic leaves no
# trace in the shell it was launched from — `install_panic_logger` writes it to crash.log
# instead. This collects that plus the context needed to read it: which build produced it,
# and whether macOS also caught it.
#
# Usage: ./scripts/crash-report.sh
set -uo pipefail

SUPPORT="$HOME/Library/Application Support/ellefuanti"
CRASH="$SUPPORT/crash.log"

echo "=== ellefuanti crash report ==="
echo "date:    $(date)"
echo "app:     $(git -C "$(dirname "$0")/.." describe --tags --always --dirty 2>/dev/null || echo unknown)"
echo

if [ -s "$CRASH" ]; then
  echo "=== crash.log (last 5 panics) ==="
  # Reports are separated by the banner; keep the most recent few rather than the whole
  # file, which accumulates across a long-lived install.
  awk '/^===== ellefuanti panic =====/{n++} n>m-5' m=5 "$CRASH" | tail -60
else
  echo "=== crash.log ==="
  echo "empty or missing — the app has not panicked since this build was installed."
  echo "(if it vanished without a panic, it was killed rather than crashed: check the"
  echo " macOS report below, which also catches aborts from the Objective-C side.)"
fi
echo

echo "=== macOS crash reports (last 2) ==="
find "$HOME/Library/Logs/DiagnosticReports" -name 'ellefuanti*' -mtime -7 2>/dev/null |
  sort | tail -2 | while read -r report; do
    echo "--- $report"
    # The header and the crashing thread's frames: the rest is every other thread's
    # backtrace, which is noise until the first two say nothing.
    sed -n '1,12p;/^Thread [0-9]* Crashed/,/^$/p' "$report" | head -40
  done
[ -z "$(find "$HOME/Library/Logs/DiagnosticReports" -name 'ellefuanti*' -mtime -7 2>/dev/null)" ] &&
  echo "none in the last 7 days"
