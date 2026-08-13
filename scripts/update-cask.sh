#!/bin/sh
# Point the Homebrew cask at a released version.
#
#   scripts/update-cask.sh 0.4.0
#
# The cask pins `sha256` to one exact file, so `version` and the checksum must move
# together — a stale checksum makes `brew install` fail verification, which is worse than
# being a version behind. This script exists so that pairing is one command rather than
# three manual edits and a hand-copied hash.
#
# Why this is not in release.yml: pushing to the tap is a push to a *different* repository,
# which needs a personal access token stored as a secret. Until that exists, this is run by
# hand after a release — and being a script rather than a checklist item is the difference
# between "sometimes forgotten" and "one command".
set -eu

VERSION="${1:-}"
[ -n "$VERSION" ] || { echo "usage: $0 <version>   e.g. $0 0.4.0" >&2; exit 1; }

REPO="richasdev/ellefuanti"
TAP_DIR="${TAP_DIR:-$HOME/homebrew-ellefuanti}"
DMG="ellefuanti-v${VERSION}-macos.dmg"
URL="https://github.com/$REPO/releases/download/v${VERSION}/${DMG}"

# Verify the release actually has the asset before touching the cask. Writing a cask that
# points at a 404 is the failure this ordering prevents.
echo "checking $URL"
curl -fsSLI "$URL" >/dev/null || {
    echo "no such asset — is v$VERSION released, and did CI upload the .dmg?" >&2
    exit 1
}

TMP="$(mktemp -t ellefuanti-cask)"
trap 'rm -f "$TMP"' EXIT INT TERM

echo "downloading to checksum it"
curl -fsSL -o "$TMP" "$URL"
SHA="$(shasum -a 256 "$TMP" | awk '{print $1}')"
echo "sha256: $SHA"

[ -d "$TAP_DIR" ] || {
    echo "no tap checkout at $TAP_DIR" >&2
    echo "clone it first: git clone git@github.com:$REPO-tap.git $TAP_DIR" >&2
    echo "(or set TAP_DIR to where you keep homebrew-ellefuanti)" >&2
    exit 1
}

CASK="$TAP_DIR/Casks/ellefuanti.rb"
[ -f "$CASK" ] || { echo "no cask at $CASK" >&2; exit 1; }

# `sed -i ''` is the BSD spelling; GNU sed would take `-i` alone. This is a macOS-only
# project, so the BSD form is the correct one rather than a portability oversight.
sed -i '' "s/^  version \".*\"$/  version \"${VERSION}\"/" "$CASK"
sed -i '' "s/^  sha256 \".*\"$/  sha256 \"${SHA}\"/" "$CASK"

# Prove the edit landed rather than trusting sed's exit code: a pattern that matched
# nothing exits 0 and changes nothing, which would silently leave the old version in place.
grep -q "version \"${VERSION}\"" "$CASK" || { echo "version did not update" >&2; exit 1; }
grep -q "sha256 \"${SHA}\"" "$CASK" || { echo "sha256 did not update" >&2; exit 1; }

ruby -c "$CASK" >/dev/null || { echo "cask is no longer valid Ruby" >&2; exit 1; }

echo
echo "updated $CASK to v$VERSION"
echo "next:"
echo "  cd $TAP_DIR && git commit -am 'ellefuanti $VERSION' && git push"
