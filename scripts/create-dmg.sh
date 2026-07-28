#!/usr/bin/env bash
set -euo pipefail

APP="${1:?Usage: $0 <App.app>}"

ROOT="$(cd "$(dirname "$0")/.." && pwd)"

# shellcheck source=scripts/macos-common.sh
. "$ROOT/scripts/macos-common.sh"

NAME=$(basename "$APP" .app)
OUT="$(dirname "$APP")/${NAME}.dmg"

rm -f "$OUT"

hdiutil create \
    -volname "$NAME" \
    -srcfolder "$APP" \
    -format UDZO \
    -ov \
    "$OUT"


#
# Sign the disk image itself
#
# Notarization staples its ticket to the .dmg, and Gatekeeper checks the
# image's own signature when a user mounts a downloaded one — signing the
# .app inside is not enough. A local build without a Developer ID just gets
# an unsigned image; notarize-macos.sh is the step that fails loudly on
# missing credentials.
#

if IDENTITY="$(signing_identity 2>/dev/null)"; then

    echo "Signing disk image with: $IDENTITY"

    codesign \
        --force \
        --timestamp \
        --sign "$IDENTITY" \
        "$OUT"

else

    echo "No signing identity available — leaving the disk image unsigned."

fi

echo "$OUT"
