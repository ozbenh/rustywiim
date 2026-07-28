#!/usr/bin/env bash
#
# Shared helpers for the macOS bundling/signing scripts. Sourced, not run.
#

# Path to the bundle's main executable, read from Info.plist rather than
# derived from the .app's own name: cargo-bundle names the .app after
# [package.metadata.bundle] name ("RustyWiiM") but the binary inside after
# the Cargo package name ("rustywiim"), so guessing either spelling only
# works because macOS filesystems are case-insensitive by default.
# CFBundleExecutable is authoritative regardless.
bundle_executable()
{
    local app="$1"
    local name

    name="$(
        /usr/libexec/PlistBuddy \
            -c "Print :CFBundleExecutable" \
            "$app/Contents/Info.plist" \
            2>/dev/null
    )" || name=""

    if [ -z "$name" ]; then
        echo "Cannot read CFBundleExecutable from $app/Contents/Info.plist" >&2
        return 1
    fi

    if [ ! -f "$app/Contents/MacOS/$name" ]; then
        echo "Info.plist names '$name' but $app/Contents/MacOS/$name is missing" >&2
        return 1
    fi

    echo "$app/Contents/MacOS/$name"
}


# The Developer ID identity to sign with: $MACOS_SIGN_IDENTITY if set,
# otherwise the first Developer ID Application cert in the keychain.
signing_identity()
{
    if [ -n "${MACOS_SIGN_IDENTITY:-}" ]; then
        echo "$MACOS_SIGN_IDENTITY"
        return 0
    fi

    local found

    found="$(
        security find-identity -v -p codesigning |
        grep "Developer ID Application" |
        head -1 |
        sed -E 's/.*"([^"]+)".*/\1/'
    )"

    if [ -z "$found" ]; then
        echo "No Developer ID Application certificate found" >&2
        return 1
    fi

    echo "$found"
}
