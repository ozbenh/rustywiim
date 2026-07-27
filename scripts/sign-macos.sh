#!/usr/bin/env bash
set -euo pipefail

APP="${1:?Usage: $0 <app>}"

if [ -z "${MACOS_SIGN_IDENTITY:-}" ]; then
    MACOS_SIGN_IDENTITY=$(security find-identity -v -p codesigning |
        grep "Developer ID Application" |
        head -1 |
        sed -E 's/.*"([^"]+)".*/\1/')
fi

if [ -z "$MACOS_SIGN_IDENTITY" ]; then
    echo "No Developer ID Application certificate found"
    exit 1
fi

IDENTITY="${MACOS_SIGN_IDENTITY:?MACOS_SIGN_IDENTITY not set}"

echo "Using identity: $IDENTITY"

sign_macho()
{
    local file="$1"

    echo "Signing $(basename "$file")"

    codesign \
        --force \
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

    sign_macho \
        "$APP/Contents/MacOS/$(basename "$APP" .app | tr '[:upper:]' '[:lower:]')"
}


sign_bundle()
{
    echo "Signing application bundle..."

    codesign \
        --force \
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

