#!/usr/bin/env bash
set -eo pipefail

APP="${1:?Usage: $0 path/to/RustyWiiM.app}"

ROOT="$(cd "$(dirname "$0")/.." && pwd)"

CONTENTS="$APP/Contents"
FRAMEWORKS="$CONTENTS/Frameworks"
RESOURCES="$CONTENTS/Resources"

EXEC="$CONTENTS/MacOS/RustyWiiM"
MANIFEST="$ROOT/target/deps.json"

BREW_PREFIX="$(brew --prefix)"

mkdir -p "$FRAMEWORKS"
mkdir -p "$RESOURCES"


#
# Generate dependency graph
#

echo "Generating dylib manifest..."

python3 \
    "$ROOT/scripts/macho-deps.py" \
    "$APP" \
    "$MANIFEST"


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
