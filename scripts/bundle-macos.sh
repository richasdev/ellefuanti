#!/bin/sh
# Assemble ellefuanti.app around an already-built binary.
#
# A .app is a directory with three files in the right places, so that is all this does.
# cargo-bundle would add a build dependency and a second source of truth for the plist to
# do the same job.
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
	# The default features on purpose: the --no-default-features shader-precompiling
	# variant needs the full Xcode toolchain (`xcrun metal`, absent from the Command Line
	# Tools) and measurably buys nothing — see ADR-0002's resolution and #57.
	echo "build one first: cargo build --release -p ellefuanti" >&2
	exit 1
fi

# iconutil is the only supported way to write a .icns and ships with macOS.
command -v iconutil >/dev/null || { echo "iconutil not found; macOS only" >&2; exit 1; }

rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"

cp "$ROOT/assets/macos/Info.plist" "$APP/Contents/"
cp "$BIN" "$APP/Contents/MacOS/ellefuanti"

# Stamp the real version over the committed plist's placeholder. Cargo.toml is the one
# source of truth for the version; a hardcoded plist shipped v0.2.1 apps whose Finder
# "Get Info" said 0.1.0.
VERSION=$(grep -m1 '^version' "$ROOT/Cargo.toml" | sed 's/.*"\(.*\)".*/\1/')
/usr/libexec/PlistBuddy -c "Set :CFBundleShortVersionString $VERSION" \
	-c "Set :CFBundleVersion $VERSION" "$APP/Contents/Info.plist"

# Built here rather than committed: a .icns is a binary blob that no reviewer can diff,
# and it is fully derived from assets/macos/ellefuanti.iconset, which IS reviewable.
iconutil -c icns "$ROOT/assets/macos/ellefuanti.iconset" -o "$APP/Contents/Resources/ellefuanti.icns"

# Shipped disk themes (#58's loader looks in Contents/Resources/themes). Only the theme
# files themselves — the README is repository documentation, not a resource.
mkdir -p "$APP/Contents/Resources/themes"
cp "$ROOT"/assets/themes/*.json "$APP/Contents/Resources/themes/"

# Ad-hoc sign the whole bundle, *last* — after every resource is in place, because
# signing seals the resources as they are and any later copy invalidates the seal.
#
# This is not code signing in the Apple-Developer sense and does not stop Gatekeeper
# asking about an unidentified developer. It fixes a different and worse failure: the
# Rust linker leaves the executable with a bare `linker-signed` ad-hoc signature that
# covers the binary and nothing else — `Info.plist=not bound`, `Sealed Resources=none`.
# A quarantined bundle in that state is not merely unsigned, it is *malformed*, and the
# message macOS picks for malformed is "is damaged and can't be opened" — the one users
# keep reporting, and the reason the README has to teach an `xattr` incantation.
#
# `codesign --sign -` binds the plist and seals Resources/, so `codesign --verify
# --deep --strict` passes and the bundle is a well-formed unsigned app rather than a
# broken one. Signing and notarisation with a real certificate remain the actual fix
# for the Gatekeeper prompt; this removes the "damaged" claim, which is a lie.
codesign --force --deep --sign - "$APP"
codesign --verify --deep --strict "$APP"

# Without this the Finder may keep showing a stale or generic icon for a path it has
# already cached, which looks exactly like the icon not working.
touch "$APP"

echo "$APP"
