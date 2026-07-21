#!/bin/sh
# Regenerates pics/thumbs/*.png from the full-size screenshots in pics/ —
# resized to 220px wide (aspect ratio preserved), matching what README.md's
# gallery links expect. Run this after adding/replacing a screenshot.
set -e

cd "$(dirname "$0")"
mkdir -p thumbs

for src in *.png; do
    magick "$src" -resize 220x "thumbs/$src"
    echo "thumbs/$src"
done
