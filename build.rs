use std::path::PathBuf;
use std::path::Path;
use std::process::Command;
use std::{fs, env};

#[cfg(target_os = "macos")]
fn build_macos_icons() {
    let out_dir = PathBuf::from(env::var("OUT_DIR").unwrap());

    // Temporary build directory
    let iconset = out_dir.join("RustyWiiM.iconset");

    // Final output for cargo-bundle
    let target_dir = PathBuf::from("target/icons");
    let icns = target_dir.join("RustyWiiM.icns");

    let svg = Path::new("src/ui/icons/rustywiim-icon.svg");

    // Always recreate the iconset
    if iconset.exists() {
        fs::remove_dir_all(&iconset).unwrap();
    }
    fs::create_dir_all(&iconset).unwrap();

    // Ensure the output directory exists
    fs::create_dir_all(&target_dir).unwrap();

    for size in [16, 32, 128, 256, 512] {
        let png1 = iconset.join(format!("icon_{}x{}.png", size, size));
        let png2 = iconset.join(format!("icon_{}x{}@2x.png", size, size));

        convert_svg(svg, size, &png1);
        convert_svg(svg, size * 2, &png2);
    }

    Command::new("iconutil")
        .args([
            "-c",
            "icns",
            iconset.to_str().unwrap(),
            "-o",
            icns.to_str().unwrap(),
        ])
        .status()
        .expect("iconutil failed");

    println!("cargo:rerun-if-changed={}", svg.display());

    println!(
        "cargo:rustc-env=RUSTYWIIM_ICNS={}",
        icns.display()
    );
}
#[cfg(target_os = "macos")]
fn convert_svg(svg: &Path, size: u32, output: &PathBuf) {
    Command::new("magick")
        .args([
            "-background",
            "none",
            svg.to_str().unwrap(),
            "-resize",
            &format!("{}x{}", size, size),
            output.to_str().unwrap(),
        ])
        .status()
        .expect("ImageMagick failed");
}
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
    println!("cargo:rerun-if-changed=src/ui/icons/jack-inout.svg");
    println!("cargo:rerun-if-changed=src/ui/icons/hdmi-inout.svg");
    println!("cargo:rerun-if-changed=src/ui/icons/audio-output.svg");
    println!("cargo:rerun-if-changed=src/ui/icons/wiim-remote.svg");
    println!("cargo:rerun-if-changed=src/ui/icons/svc-spotify.svg");
    println!("cargo:rerun-if-changed=src/ui/icons/svc-tidal.svg");
    println!("cargo:rerun-if-changed=src/ui/icons/svc-qobuz.svg");
    println!("cargo:rerun-if-changed=src/ui/icons/svc-deezer.svg");
    println!("cargo:rerun-if-changed=src/ui/icons/svc-pandora.svg");
    println!("cargo:rerun-if-changed=src/ui/icons/svc-napster.svg");
    println!("cargo:rerun-if-changed=src/ui/icons/svc-iheartradio.svg");
    println!("cargo:rerun-if-changed=src/ui/icons/svc-tunein.svg");
    println!("cargo:rerun-if-changed=src/ui/icons/svc-amazon.svg");
    println!("cargo:rerun-if-changed=src/ui/themes/wood/wood-grain.svg");
    println!("cargo:rerun-if-changed=src/ui/themes/wood/wood-panel.svg");
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
    // Regenerate the MacOS rasterized icon
    #[cfg(target_os = "macos")]
    build_macos_icons();
}
