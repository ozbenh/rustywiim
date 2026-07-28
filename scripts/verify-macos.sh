#!/usr/bin/env bash
set -euo pipefail

#
# Post-release sanity check: everything a user's machine will check before
# it agrees to run the app, run here instead of finding out from a tester.
#

APP="${1:?Usage: $0 <app> <dmg>}"
DMG="${2:?Usage: $0 <app> <dmg>}"


echo "Verifying code signature..."

codesign \
    --verify \
    --deep \
    --strict \
    --verbose=2 \
    "$APP"


# Gatekeeper's own assessment, which additionally requires the notarization
# ticket to be present and accepted — a valid signature alone passes the
# check above but is still refused on a user's machine.
echo
echo "Assessing with Gatekeeper..."

spctl \
    -a \
    -t exec \
    -vvv \
    "$APP"


echo
echo "Validating stapled notarization ticket..."

xcrun stapler validate "$DMG"


echo
echo "Verification passed:"
echo "  $APP"
echo "  $DMG"
