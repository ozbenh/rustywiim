#!/bin/bash
set -euo pipefail

APP="${1:?Usage: $0 <App.app>}"

NAME=$(basename "$APP" .app)
OUT="$(dirname "$APP")/${NAME}.dmg"

rm -f "$OUT"

hdiutil create \
    -volname "$NAME" \
    -srcfolder "$APP" \
    -format UDZO \
    -ov \
    "$OUT"

echo "$OUT"
