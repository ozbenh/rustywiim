# Local packaging helpers
#
# Both cargo-deb and cargo-generate-rpm are pure-Rust implementations (no
# dpkg-deb/rpmbuild needed to actually build the package file), but
# `cargo deb`'s dependency list (Depends:) is auto-derived by running `ldd`
# against the built binary — that only produces an accurate result run on
# the actual target distro (a real shared-library version match), so these
# targets are meant to be run on Debian/Ubuntu for `deb` and Fedora for
# `rpm`, not cross-built from one to the other.

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

# Builds whichever package format matches the OS you're on.
package:
	@. /etc/os-release; \
	case "$$ID $$ID_LIKE" in \
		*fedora*) $(MAKE) rpm ;; \
		*debian*|*ubuntu*) $(MAKE) deb ;; \
		*) echo "error: unrecognized distro ($$PRETTY_NAME) — use 'make deb' or 'make rpm' directly." >&2; exit 1 ;; \
	esac

clean:
	cargo clean

clean-pkg:
	rm -rf target/debian target/generate-rpm
