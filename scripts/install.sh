#!/bin/sh
# One-line installer for ellefuanti on macOS.
#
#   curl -fsSL https://raw.githubusercontent.com/richasdev/ellefuanti/main/scripts/install.sh | sh
#
# What this exists to avoid: a first-time double-click on a downloaded, unsigned app shows
# "is damaged and can't be opened" with a single Move to Trash button, and most people stop
# there. Clearing the quarantine flag turns that dead end into a normal launch. Everything
# below is the handful of commands a user would otherwise type by hand, in order.
#
# Deliberately `sh`, not `bash`: this is piped into whatever /bin/sh is, and nothing here
# needs more than POSIX.
set -eu

REPO="richasdev/ellefuanti"
APP="/Applications/ellefuanti.app"
DMG="$(mktemp -t ellefuanti)".dmg
MOUNT=""

case "$(uname -s)" in
    Darwin) ;;
    *) echo "ellefuanti is macOS-only for now." >&2; exit 1 ;;
esac

# Unmount and clean up whatever we got as far as, on success or failure alike. Without this
# a failed download leaves a mounted volume behind for the user to find later and wonder at.
cleanup() {
    [ -n "$MOUNT" ] && hdiutil detach "$MOUNT" -quiet 2>/dev/null || true
    rm -f "$DMG"
}
trap cleanup EXIT INT TERM

echo "Downloading the latest ellefuanti…"
# The URL carries no version, so it always resolves to whatever the current release is —
# a versioned URL here would rot the moment the next release ships.
curl -fL --progress-bar -o "$DMG" \
    "https://github.com/$REPO/releases/latest/download/ellefuanti-macos.dmg"

echo "Mounting…"
# The volume name carries the version ("ellefuanti 0.3.2"), so it is read back from hdiutil
# rather than guessed. Guessing it is how this breaks on the next release.
#
# Two details, both learned by running it: `-quiet` is deliberately absent, because it
# suppresses the very lines being parsed here; and hdiutil separates its columns with tabs,
# so the mount point is everything after the last tab — splitting on spaces would truncate
# a volume name at its first space, which this one has.
MOUNT="$(hdiutil attach "$DMG" -nobrowse |
    awk -F'\t' '/\/Volumes\//{ print $NF }' | tail -1)"
[ -n "$MOUNT" ] || { echo "Could not mount the disk image." >&2; exit 1; }
[ -d "$MOUNT/ellefuanti.app" ] || { echo "No app inside the disk image." >&2; exit 1; }

echo "Installing to $APP…"
# `ditto`, never `cp -R`. cp drops the bundle's _CodeSignature/ directory, and macOS reports
# a bundle whose signature is missing its sealed resources as "damaged" — which is precisely
# the failure this script exists to prevent, so using cp here would be self-defeating.
rm -rf "$APP"
ditto "$MOUNT/ellefuanti.app" "$APP"

# The actual fix: drop the quarantine flag macOS stamps on anything downloaded. Without an
# Apple Developer certificate the app is ad-hoc signed, and quarantine + ad-hoc is what
# produces the dead-end warning.
xattr -dr com.apple.quarantine "$APP" 2>/dev/null || true

echo "Installed. Opening ellefuanti…"
open "$APP"
