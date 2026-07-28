#!/usr/bin/env python3

import json
import subprocess
import sys
from pathlib import Path


if len(sys.argv) < 3:
    print(f"Usage: {sys.argv[0]} <executable> <manifest> [extra-root ...]")
    sys.exit(1)


MANIFEST = Path(sys.argv[2])

# Every Mach-O file already in its final place inside the bundle: the main
# executable, plus any extra roots given on the command line (the gdk-pixbuf
# loaders, which are dlopen'd from a cache file rather than linked, so
# nothing in the executable's own dependency graph reaches them). These are
# scanned for dependencies but never added to the manifest — they are not
# copied, they are already where they belong.
ROOTS = [Path(arg).resolve() for arg in [sys.argv[1]] + sys.argv[3:]]

BREW_PREFIX = Path(
    subprocess.check_output(
        ["brew", "--prefix"],
        text=True
    ).strip()
)


seen = set()
manifest = []


def run_otool(path):
    try:
        output = subprocess.check_output(
            ["otool", "-L", str(path)],
            text=True,
            stderr=subprocess.DEVNULL,
        )
    except subprocess.CalledProcessError:
        return []

    result = []

    for line in output.splitlines()[1:]:
        line = line.strip()

        if not line:
            continue

        result.append(line.split()[0])

    return result


def add_manifest(path):
    real = path.resolve()

    for entry in manifest:
        if Path(entry["real"]) == real:
            return

    manifest.append({
        "source": str(path),
        "bundle_name": path.name,
        "real": str(real),
        "is_symlink": path.is_symlink(),
    })


def resolve_rpath(current, name):

    candidates = [
        current.parent / name,
        BREW_PREFIX / "lib" / name,
    ]

    for candidate in candidates:
        if candidate.exists():
            return candidate

    return None


def collect(path):

    path = Path(path)

    if not path.exists():
        print("Missing:", path)
        return

    real = path.resolve()

    # Cycle detection
    if real in seen:
        return

    seen.add(real)

    print("Scanning:", path)

    if real not in ROOTS:
        add_manifest(path)

    for dep in run_otool(path):

        # System libraries stay on macOS
        if dep.startswith("/System/"):
            continue

        if dep.startswith("/usr/lib/"):
            continue


        # Homebrew absolute path
        if dep.startswith(str(BREW_PREFIX)):
            collect(Path(dep))
            continue


        # Homebrew uses @rpath extensively
        if dep.startswith("@rpath/"):

            name = dep[len("@rpath/"):]

            resolved = resolve_rpath(path, name)

            if resolved:
                print("  ", dep, "->", resolved)
                collect(resolved)
            else:
                print("UNRESOLVED:", dep)

            continue


        # Other @loader_path/@executable_path are normally
        # already inside the bundle or handled later.
        if dep.startswith("@"):
            continue


        print("Ignoring:", dep)


for root in ROOTS:
    collect(root)


MANIFEST.write_text(
    json.dumps(manifest, indent=2)
)


print()
print("Manifest:")
for item in manifest:
    print(" ", item["source"])
