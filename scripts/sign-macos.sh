#!/usr/bin/env bash
set -euo pipefail

APP="${1:?Usage: $0 <app>}"

ROOT="$(cd "$(dirname "$0")/.." && pwd)"

# shellcheck source=scripts/macos-common.sh
. "$ROOT/scripts/macos-common.sh"

IDENTITY="$(signing_identity)"

echo "Using identity: $IDENTITY"

# --timestamp requests a secure timestamp from Apple's timestamp server;
# notarization rejects signatures without one.
sign_macho()
{
    local file="$1"

    echo "Signing $(basename "$file")"

    codesign \
        --force \
        --timestamp \
        --options runtime \
        --sign "$IDENTITY" \
        "$file"
}


is_macho()
{
    file "$1" | grep -q "Mach-O"
}


sign_macho_dir()
{
    local dir="$1"
    local description="$2"

    [ -d "$dir" ] || return 0

    echo "Signing $description..."

    find "$dir" -type f -print0 |
    while IFS= read -r -d '' file; do

        if ! is_macho "$file"; then
            continue
        fi

        sign_macho "$file"

    done || true
}

sign_frameworks()
{
    sign_macho_dir \
        "$APP/Contents/Frameworks" \
        "embedded frameworks"
}


sign_plugins()
{
    sign_macho_dir \
        "$APP/Contents/PlugIns" \
        "plugins"
}


sign_executable()
{
    echo "Signing executable..."

    sign_macho "$(bundle_executable "$APP")"
}


sign_bundle()
{
    echo "Signing application bundle..."

    codesign \
        --force \
        --timestamp \
        --options runtime \
        --sign "$IDENTITY" \
        "$APP"
}


verify_bundle()
{
    echo "Verifying signature..."

    codesign \
        --verify \
        --deep \
        --strict \
        --verbose=2 \
        "$APP"
}

sign_frameworks
sign_plugins
sign_executable
sign_bundle
verify_bundle
