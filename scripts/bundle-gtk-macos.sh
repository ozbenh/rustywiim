#!/usr/bin/env bash
set -euo pipefail

APP="${1:?Usage: $0 path/to/RustyWiiM.app}"

ROOT="$(cd "$(dirname "$0")/.." && pwd)"

# shellcheck source=scripts/macos-common.sh
. "$ROOT/scripts/macos-common.sh"

CONTENTS="$APP/Contents"
FRAMEWORKS="$CONTENTS/Frameworks"
RESOURCES="$CONTENTS/Resources"

EXEC="$(bundle_executable "$APP")"
MANIFEST="$ROOT/target/deps.json"

BREW_PREFIX="$(brew --prefix)"

mkdir -p "$FRAMEWORKS"
mkdir -p "$RESOURCES"


#
# Copy gdk-pixbuf loaders
#
# gdk-pixbuf loads its PNG/JPEG/SVG/... format plugins via dlopen based on a
# generated cache file, not link-time dependencies, so the otool-based
# dependency walk below never finds them — they have to be gathered here,
# before the manifest is generated, and then fed to that walk explicitly so
# their own dylibs (librsvg and friends) get bundled too.
#
# The loaders are taken from the merged Homebrew prefix, not from
# `brew --prefix gdk-pixbuf`: that keg-only path holds gdk-pixbuf's own
# loaders only, while the SVG loader ships with the librsvg formula. Homebrew
# symlinks every keg's loaders into the shared prefix, so this is the one
# directory that sees all of them.
#

PIXBUF_LOADER_DIR="$BREW_PREFIX/lib/gdk-pixbuf-2.0/2.10.0/loaders"

if [ ! -d "$PIXBUF_LOADER_DIR" ]; then
    echo "error: gdk-pixbuf loader directory not found: $PIXBUF_LOADER_DIR" >&2
    echo "       install gdk-pixbuf and librsvg via Homebrew first." >&2
    exit 1
fi

echo "Copying gdk-pixbuf loaders..."

for lib in "$PIXBUF_LOADER_DIR"/*.so; do

    [ -e "$lib" ] || continue

    dst="$FRAMEWORKS/$(basename "$lib")"

    if [ ! -e "$dst" ]; then

        echo "  $(basename "$lib")"

        # -L: the merged prefix entries are symlinks into the kegs.
        cp -pL "$lib" "$dst"

        # Homebrew ships these read-only; install_name_tool needs to write.
        chmod u+w "$dst"
    fi

done

# The Adwaita icon theme and this app's own icons are all SVG, so a bundle
# without the SVG loader renders no icons at all — and does so silently,
# which is exactly the failure this check exists to make loud.
#
# Spelled with an underscore, unlike every C-built sibling: librsvg's loader
# is a Rust cdylib, and cargo normalizes `-` to `_` in library names. Match
# either separator so a future rename doesn't silently pass this check.
if ! ls "$FRAMEWORKS"/libpixbufloader[-_]svg.so >/dev/null 2>&1; then
    echo "error: no SVG pixbuf loader found in $PIXBUF_LOADER_DIR" >&2
    echo "       it ships with the librsvg formula: brew install librsvg" >&2
    exit 1
fi


#
# Generate dependency graph
#

echo "Generating dylib manifest..."

shopt -s nullglob
PIXBUF_LOADERS=("$FRAMEWORKS"/*.so)
shopt -u nullglob

python3 \
    "$ROOT/scripts/macho-deps.py" \
    "$EXEC" \
    "$MANIFEST" \
    "${PIXBUF_LOADERS[@]}"


#
# Copy dylibs from manifest
#
# The manifest contains both:
#   libfoo.dylib -> libfoo.real.dylib
#   libfoo.real.dylib
#
# We copy the real file first, then recreate symlinks.
#

echo "Copying dylibs..."

python3 <<EOF
import json
import shutil
from pathlib import Path

manifest = json.load(open("$MANIFEST"))

frameworks = Path("$FRAMEWORKS")


# Copy real files first
for item in manifest:

    src = Path(item["real"])

    if item["real"] == item["source"]:
        continue

    dst = frameworks / src.name

    if not dst.exists():
        print("copy", src.name)
        shutil.copy2(src, dst)


# Copy normal files
for item in manifest:

    if item["is_symlink"]:
        continue

    src = Path(item["source"])
    dst = frameworks / item["bundle_name"]

    if not dst.exists():
        print("copy", dst.name)
        shutil.copy2(src, dst)


# Recreate symlinks
for item in manifest:

    if not item["is_symlink"]:
        continue

    dst = frameworks / item["bundle_name"]

    if dst.exists() or dst.is_symlink():
        continue

    target = Path(item["real"]).name

    print("link", dst.name, "->", target)

    dst.symlink_to(target)

EOF


#
# Fix Mach-O paths
#

fixup_macho()
{
    local file="$1"
    local rpath="$2"

    # Never modify symlinks
    [ -L "$file" ] && return

    file "$file" | grep -q "Mach-O" || return


    echo "Fixing $(basename "$file")"


    while read -r dep; do

        case "$dep" in

            "$BREW_PREFIX"/*)

                name="$(basename "$dep")"

                install_name_tool \
                    -change "$dep" \
                    "@rpath/$name" \
                    "$file"

                ;;

        esac

    done < <(
        otool -L "$file" |
        tail -n +2 |
        awk '{print $1}'
    )


    #
    # Rewrite dylib install name
    #

    if otool -hv "$file" | grep -q DYLIB; then

        install_name_tool \
            -id "@rpath/$(basename "$file")" \
            "$file"

    fi


    #
    # Add required rpath
    #

    if ! otool -l "$file" | grep -q "$rpath"; then

        install_name_tool \
            -add_rpath "$rpath" \
            "$file"

    fi
}


echo "Fixing executable..."

fixup_macho \
    "$EXEC" \
    "@executable_path/../Frameworks"


# Covers the pixbuf loaders too — they were copied into Frameworks above.
echo "Fixing frameworks..."

find "$FRAMEWORKS" -type f |
while read -r lib; do
    fixup_macho \
        "$lib" \
        "@loader_path"
done


#
# Copy GTK runtime data
#

copy_brew_data()
{
    local package="$1"

    echo "Copying runtime data from $package..."

    brew list --formula --verbose "$package" 2>/dev/null |
    while read -r file; do

        [ -f "$file" ] || continue


        case "$file" in

            "$BREW_PREFIX"/Cellar/*/share/*)

                rel="${file#*/share/}"

                case "$rel" in

                    icons/*|\
                    glib-2.0/schemas/*|\
                    libadwaita-1/*|\
                    locale/*)

                        dest="$RESOURCES/share/$rel"

                        if [ ! -e "$dest" ]; then

                            echo "  $rel"

                            mkdir -p "$(dirname "$dest")"

                            cp -p \
                                "$file" \
                                "$dest"
                        fi

                        ;;

                esac

                ;;

        esac

    done
}


copy_brew_data gtk4
copy_brew_data libadwaita
copy_brew_data glib
copy_brew_data adwaita-icon-theme


#
# Generate gdk-pixbuf loaders cache
#
# The cache bakes in an absolute path per loader, but the bundle's final
# install location isn't known until it runs, so a `@BUNDLE_FRAMEWORKS@`
# placeholder is written in place of the loader directory; main.rs patches
# it before GTK initializes. Queried against the Homebrew originals rather
# than the bundled copies so that the query tool can dlopen each loader with
# its dependencies still resolvable at their build-time paths.
#

echo "Generating gdk-pixbuf loaders cache..."

LOADER_CACHE="$RESOURCES/share/gdk-pixbuf-2.0/loaders.cache"

mkdir -p "$(dirname "$LOADER_CACHE")"

GDK_PIXBUF_MODULEDIR="$PIXBUF_LOADER_DIR" \
    gdk-pixbuf-query-loaders |
    python3 -c "
import sys
loader_dir = sys.argv[1]
sys.stdout.write(sys.stdin.read().replace(loader_dir, '@BUNDLE_FRAMEWORKS@'))
" "$PIXBUF_LOADER_DIR" \
    > "$LOADER_CACHE"

# Checks both that the SVG loader was picked up and that the path
# substitution matched — a cache still holding build-time absolute paths
# points at a directory that won't exist on the user's machine.
if ! grep -qE "@BUNDLE_FRAMEWORKS@/libpixbufloader[-_]svg\.so" "$LOADER_CACHE"; then
    echo "error: generated loaders.cache has no relocated SVG loader entry" >&2
    exit 1
fi


#
# Compile GSettings schemas
#

SCHEMAS="$RESOURCES/share/glib-2.0/schemas"

if [ -d "$SCHEMAS" ]; then

    echo "Compiling schemas..."

    glib-compile-schemas "$SCHEMAS"

fi


echo
echo "GTK bundle complete:"
echo "$APP"
