use std::process::Command;

fn git(args: &[&str]) -> Option<String> {
    Command::new("git")
        .args(args)
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
}

fn main() {
    // Rerun when the checked-out commit or any ref changes.
    println!("cargo:rerun-if-changed=.git/HEAD");
    println!("cargo:rerun-if-changed=.git/packed-refs");

    let hash = git(&["rev-parse", "--short", "HEAD"]).unwrap_or_else(|| "unknown".into());

    // Derive the displayed version from git tags:
    //   Exactly on v*  tag  →  strip 'v'           e.g. "1.2.3"
    //   Near a    v*  tag   →  strip 'v', add '+'  e.g. "1.2.3+"
    //   No v* tags at all   →  Cargo.toml version + '+'
    //
    // This value overrides CARGO_PKG_VERSION as seen by env!() in the code.
    let version = if let Some(tag) = git(&[
        "describe", "--tags", "--exact-match", "--match", "v*", "HEAD",
    ]) {
        // HEAD is exactly tagged — clean release build.
        tag.trim_start_matches('v').to_string()
    } else if let Some(nearest) = git(&[
        "describe", "--tags", "--match", "v*", "--abbrev=0",
    ]) {
        // Commits beyond the nearest release tag.
        format!("{}+", nearest.trim_start_matches('v'))
    } else {
        // No release tags anywhere — fall back to Cargo.toml with '+'.
        let base = std::env::var("CARGO_PKG_VERSION").unwrap_or_else(|_| "0.0.0".to_string());
        format!("{base}+")
    };

    // Override CARGO_PKG_VERSION so env!("CARGO_PKG_VERSION") in the crate
    // returns the git-derived version rather than the Cargo.toml placeholder.
    println!("cargo:rustc-env=CARGO_PKG_VERSION={version}");
    println!("cargo:rustc-env=GIT_HASH={hash}");

    // Compile the icon GResource bundle (app icon + every custom in-app
    // vector icon — RCA/optical/coax/output-fallback/remote). Embedded via
    // include_bytes! in ui/mod.rs (not shipped as a separate file), so
    // every icon is available in-process, rendered as a real vector via
    // IconTheme::lookup_icon(), even for a bare `cargo run`/unpackaged
    // binary — no system icon-theme install needed for that. Requires
    // glib-compile-resources at build time only (part of
    // libglib2.0-dev-bin on Debian/Ubuntu, glib2-devel on Fedora) — not a
    // runtime dependency.
    println!("cargo:rerun-if-changed=src/ui/rustywiim.gresource.xml");
    println!("cargo:rerun-if-changed=src/ui/icons/rustywiim-icon.svg");
    println!("cargo:rerun-if-changed=src/ui/icons/rca-inout.svg");
    println!("cargo:rerun-if-changed=src/ui/icons/optical-inout.svg");
    println!("cargo:rerun-if-changed=src/ui/icons/coax-inout.svg");
    println!("cargo:rerun-if-changed=src/ui/icons/audio-output.svg");
    println!("cargo:rerun-if-changed=src/ui/icons/wiim-remote.svg");
    println!("cargo:rerun-if-changed=src/ui/icons/jack-inout.svg");
    let out_dir = std::env::var("OUT_DIR").unwrap();
    let status = Command::new("glib-compile-resources")
        .args([
            "--sourcedir=src/ui",
            &format!("--target={out_dir}/rustywiim.gresource"),
            "src/ui/rustywiim.gresource.xml",
        ])
        .status()
        .expect(
            "failed to run glib-compile-resources — install libglib2.0-dev-bin \
             (Debian/Ubuntu) or glib2-devel (Fedora)",
        );
    if !status.success() {
        panic!("glib-compile-resources failed");
    }
}
