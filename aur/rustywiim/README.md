# AUR package for rustywiim

This directory holds the PKGBUILD for the **AUR (Arch User Repository)** package of rustywiim. The AUR package install the following files:

- `/usr/bin/rustywiim`
- `/usr/lib/rustywiim/{wiim-capture,wiim-capdump}`
- `/usr/share/applications/io.github.ozbenh.rustywiim.desktop`
- `/usr/share/icons/hicolor/scalable/apps/io.github.ozbenh.rustywiim.svg`
- `/usr/share/licenses/rustywiim/LICENSE`

## Bumping to a new release

1. Update `pkgver` to the new version (e.g. `0.14.0`), keep `pkgrel` at `1`.
2. Update the `sha256sums` variable value:
   ```
   curl -sSL -o /tmp/rw.tgz https://github.com/ozbenh/rustywiim/archive/refs/tags/v<new-ver>.tar.gz
   sha256sum /tmp/rw.tgz
   ```
3. `pkgdesc`/`depends`/`makedepends` only change if the project's dependencies change.

## Testing the build

```
makepkg -si   # build and install; the .pkg.tar.zst stays here for manual install
# or just:    makepkg -s    # build only, then: sudo pacman -U rustywiim-*.pkg.tar.zst
```

## Submitting / updating the AUR entry (owner only)

The AUR repo must contain **only** the `PKGBUILD` (this README and any local build artifacts don't belong there). This must be done from the maintainer's AUR account:

```
# In a directory belonging to the maintainer, e.g. ~/aur/rustywiim:
cp <this-dir>/PKGBUILD .
git init
git add PKGBUILD
git commit -m "Update AUR package to x.x.x version"
aur-helper push --yes
```

First submission creates the AUR entry (`aur.archlinux.org/packages/rustywiim`); each later bump is a new commit + `aur-helper push`.
