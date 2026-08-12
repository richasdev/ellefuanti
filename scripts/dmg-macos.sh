#!/bin/sh
# Builds a distributable .dmg from the bundled .app.
#
# hdiutil, not create-dmg: hdiutil ships with macOS, so this needs no dependency a
# contributor has to install first — the same reasoning bundle-macos.sh follows for
# iconutil. The layout is the classic drag-to-install: the .app plus a symlink to
# /Applications, so the user drags one onto the other.
#
# Run scripts/bundle-macos.sh first — this packages whatever .app it produced.
set -eu

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
APP="$ROOT/target/ellefuanti.app"
VERSION="${1:-$(grep -m1 '^version' "$ROOT/Cargo.toml" | sed 's/.*"\(.*\)".*/\1/')}"
DMG="$ROOT/target/ellefuanti-v${VERSION}-macos.dmg"
STAGING="$ROOT/target/dmg-staging"

[ -d "$APP" ] || { echo "no $APP — run scripts/bundle-macos.sh first" >&2; exit 1; }
command -v hdiutil >/dev/null || { echo "hdiutil not found; macOS only" >&2; exit 1; }

# A clean staging directory holds exactly what the mounted volume shows: the app and an
# Applications symlink, nothing else.
rm -rf "$STAGING" "$DMG"
mkdir -p "$STAGING"

# `ditto`, not `cp -R`: cp does not carry a bundle's `_CodeSignature/` directory across,
# and a signed .app that arrives without its seal is *worse* than an unsigned one —
# `spctl` reports "code has no resources but signature indicates they must be present"
# and macOS shows the app as damaged. ditto is Apple's own bundle-preserving copy and
# keeps the signature, extended attributes and resource forks intact.
ditto "$APP" "$STAGING/$(basename "$APP")"
ln -s /Applications "$STAGING/Applications"

# UDZO is the compressed read-only format every macOS opens; the volume name is what the
# Finder shows in the sidebar when it mounts.
hdiutil create \
  -volname "ellefuanti $VERSION" \
  -srcfolder "$STAGING" \
  -ov \
  -format UDZO \
  "$DMG" >/dev/null

rm -rf "$STAGING"
echo "$DMG"
