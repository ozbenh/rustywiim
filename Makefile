# Local packaging helpers
#
# Both cargo-deb and cargo-generate-rpm are pure-Rust implementations (no
# dpkg-deb/rpmbuild needed to actually build the package file), but
# `cargo deb`'s dependency list (Depends:) is auto-derived by running `ldd`
# against the built binary — that only produces an accurate result run on
# the actual target distro (a real shared-library version match), so these
# targets are meant to be run on Debian/Ubuntu for `deb` and Fedora for
# `rpm`, not cross-built from one to the other.

UNAME_S := $(shell uname -s)

ifeq ($(UNAME_S),Darwin)
IS_MACOS=true
else
IS_MACOS=false
endif



.PHONY: all build deb rpm package clean clean-pkg

.DEFAULT_GOAL := build

# Plain `make`/`make build` is just a thin wrapper — the real build is
# still plain `cargo build`, this exists so the packaging targets below
# read consistently as "make <verb>".
all: build

build:
	cargo build

# Mirrors build.rs's own version-derivation exactly (exact v* tag -> clean
# version; commits past the nearest v* tag -> that version + "+"; no tags
# at all -> Cargo.toml's version + "+"), so the packaged .deb/.rpm version
# always matches what the app itself displays via
# env!("CARGO_PKG_VERSION") in the About dialog. Cargo.toml's own
# [package].version is deliberately NOT used as-is here — cargo-deb and
# cargo-generate-rpm both read it directly by default, which drifts from
# the git-tag-derived version the moment the two aren't bumped in lockstep
# (checked on this repo right now: Cargo.toml says 0.6.4, but HEAD is only
# at v0.5.0+, no exact tag — those are two different numbers).
VERSION := $(shell \
	if git describe --tags --exact-match --match 'v*' HEAD >/dev/null 2>&1; then \
		git describe --tags --exact-match --match 'v*' HEAD | sed 's/^v//'; \
	elif git describe --tags --match 'v*' --abbrev=0 >/dev/null 2>&1; then \
		printf '%s+' "$$(git describe --tags --match 'v*' --abbrev=0 | sed 's/^v//')"; \
	else \
		printf '%s+' "$$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)"; \
	fi)

deb:
	cargo install --locked cargo-deb --quiet || true
	cargo deb --deb-version "$(VERSION)"
	@echo "== .deb (version $(VERSION)) written to target/debian/ =="

rpm:
	cargo install --locked cargo-generate-rpm --quiet || true
	cargo build --release
	strip -s target/release/rustywiim
	cargo generate-rpm -s 'version = "$(VERSION)"'
	@echo "== .rpm (version $(VERSION)) written to target/generate-rpm/ =="

.PHONY: deps-macos build-macos gtk-macos bundle-macos sign-macos run-macos dmg-macos notarize-macos verify-macos release-macos clean-macos

ifeq ($(IS_MACOS),true)
APP := target/release/bundle/osx/RustyWiiM.app
DMG := target/release/bundle/osx/RustyWiiM.dmg

# imagemagick is a real build dependency — build.rs shells out to `magick`
# to generate the .icns. The DMG is built with plain hdiutil, so the
# create-dmg formula is deliberately not installed.
deps-macos:
	brew update
	brew install \
	    gtk4 \
	    libadwaita \
	    adwaita-icon-theme \
	    librsvg \
	    openssl \
	    imagemagick

# The individual pipeline stages below deliberately declare no
# prerequisites, so CI can run each one as its own step (visible progress,
# separate logs) without re-running the stages before it. The chaining
# convenience targets are further down.

build-macos:
	cargo bundle --release --format osx

gtk-macos:
	./scripts/bundle-gtk-macos.sh "$(APP)"

sign-macos:
	./scripts/sign-macos.sh "$(APP)"

dmg-macos:
	./scripts/create-dmg.sh "$(APP)"

notarize-macos:
	./scripts/notarize-macos.sh "$(DMG)"

verify-macos:
	./scripts/verify-macos.sh "$(APP)" "$(DMG)"

bundle-macos:
	$(MAKE) build-macos
	$(MAKE) gtk-macos

run-macos: bundle-macos
	open "$(APP)"

# Recursive invocations rather than prerequisites: prerequisites of a
# single target may run in any order (and concurrently under `make -j`),
# which for these stages would mean signing a bundle that isn't built yet.
# Recipe lines always run in sequence.
release-macos:
	$(MAKE) build-macos
	$(MAKE) gtk-macos
	$(MAKE) sign-macos
	$(MAKE) dmg-macos
	$(MAKE) notarize-macos
	$(MAKE) verify-macos

clean-macos:
	rm -rf target/release/bundle
	rm -rf target/debug/bundle

package: release-macos

else

deps-macos:
build-macos:
gtk-macos:
bundle-macos:
sign-macos:
dmg-macos:
notarize-macos:
verify-macos:
release-macos:
run-macos:
clean-macos:

package:
	@. /etc/os-release; \
	case "$$ID $$ID_LIKE" in \
		*fedora*) $(MAKE) rpm ;; \
		*debian*|*ubuntu*) $(MAKE) deb ;; \
		*) echo "error: unrecognized distro ($$PRETTY_NAME) — use 'make deb' or 'make rpm' directly." >&2; exit 1 ;; \
	esac
endif

clean: clean-macos
	cargo clean


clean-pkg:
	rm -rf target/debian target/generate-rpm

