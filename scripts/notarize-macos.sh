#!/usr/bin/env bash
set -euo pipefail

DMG="${1:?Usage: $0 <dmg>}"

echo "Notarizing: $DMG"


#
# Choose authentication method
#

if [ -n "${APPLE_NOTARY_PROFILE:-}" ]; then

    echo "Using keychain profile: $APPLE_NOTARY_PROFILE"

    NOTARY_ARGS=(
        --keychain-profile
        "$APPLE_NOTARY_PROFILE"
    )

elif [ -n "${APPLE_ID:-}" ] &&
     [ -n "${APPLE_TEAM_ID:-}" ] &&
     [ -n "${APPLE_APP_PASSWORD:-}" ]; then

    echo "Using Apple ID credentials"

    NOTARY_ARGS=(
        --apple-id
        "$APPLE_ID"
        --team-id
        "$APPLE_TEAM_ID"
        --password
        "$APPLE_APP_PASSWORD"
    )

else

    echo "No notarization credentials found."
    echo
    echo "For local use:"
    echo "  export APPLE_NOTARY_PROFILE=RustyWiiM"
    echo
    echo "For CI:"
    echo "  set APPLE_ID"
    echo "  set APPLE_TEAM_ID"
    echo "  set APPLE_APP_PASSWORD"
    exit 1

fi


#
# Submit
#

echo "Submitting to Apple..."

xcrun notarytool submit \
    "$DMG" \
    "${NOTARY_ARGS[@]}" \
    --wait


#
# Staple ticket
#

echo "Stapling notarization ticket..."

xcrun stapler staple "$DMG"


#
# Verify
#

echo "Validating staple..."

xcrun stapler validate "$DMG"

echo
echo "Notarization complete:"
echo "$DMG"
