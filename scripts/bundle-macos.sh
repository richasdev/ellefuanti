#!/bin/sh
# Assemble ellefuanti.app around an already-built binary.
#
# A .app is a directory with three files in the right places, so that is all this does.
# cargo-bundle would add a build dependency and a second source of truth for the plist to
# do the same job, and it does not know about the --no-default-features release path.
#
# Deliberately does NOT build anything: `cargo run` stays the everyday loop, and this only
# runs when someone actually wants a bundle.
#
# Usage: scripts/bundle-macos.sh [path/to/binary] [output-dir]
set -eu

ROOT=$(cd "$(dirname "$0")/.." && pwd)
BIN=${1:-$ROOT/target/release/ellefuanti}
OUTDIR=${2:-$ROOT/target}
APP=$OUTDIR/ellefuanti.app

if [ ! -x "$BIN" ]; then
	echo "no binary at $BIN" >&2
	echo "build one first, e.g. cargo build --release -p ellefuanti --no-default-features" >&2
	exit 1
fi

# iconutil is the only supported way to write a .icns and ships with macOS.
command -v iconutil >/dev/null || { echo "iconutil not found; macOS only" >&2; exit 1; }

rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"

cp "$ROOT/assets/macos/Info.plist" "$APP/Contents/"
cp "$BIN" "$APP/Contents/MacOS/ellefuanti"

# Built here rather than committed: a .icns is a binary blob that no reviewer can diff,
# and it is fully derived from assets/macos/ellefuanti.iconset, which IS reviewable.
iconutil -c icns "$ROOT/assets/macos/ellefuanti.iconset" -o "$APP/Contents/Resources/ellefuanti.icns"

# Without this the Finder may keep showing a stale or generic icon for a path it has
# already cached, which looks exactly like the icon not working.
touch "$APP"

echo "$APP"
