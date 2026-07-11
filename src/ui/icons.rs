/// Centralised icon handling for RustyWiiM.
///
/// `IconSet::load()` must be called once from `build_ui`, after the GDK
/// display is available.  From that point every icon is a `gdk::Paintable`
/// resolved by name via `gtk::IconTheme::lookup_icon()` — both this app's own
/// custom full-colour icons (RCA/optical/coax/output-fallback/remote,
/// embedded in the `rustywiim.gresource` bundle `init_icon_resource()`
/// registers, same mechanism as the app icon) and Adwaita's own symbolic
/// icons resolve through the exact same call, for consistency. No icon-name
/// strings are scattered through the rest of the codebase.
///
/// Unlike a `gdk::Texture` decoded once from raw bytes, an SVG-backed
/// `GtkIconPaintable` renders crisply at whatever size it's *scaled down
/// to* — but `lookup_icon()`'s `size`/`scale` arguments still bake a raster
/// texture at that resolution once, up front (`gtk_icon_theme_lookup_icon()`
/// itself: "desired icon size, in application pixels" / "the window scale
/// this will be displayed on" — not "render at any size forever"). Confirmed
/// live: with `size` hardcoded to 64, the main window's 128px no-art
/// fallback (`icon_size` in `ui/playback.rs`) was visibly pixelated — a 2×
/// upscale of that 64px raster, worst on the jack icon's fine line detail.
/// `LOOKUP_SIZE` below is the largest logical size any caller actually
/// displays one of these at — every smaller use (devlist row, mini window,
/// remote icon) downscales from it instead, which stays sharp.
/// `LOOKUP_SCALE` requests a 2×-native raster on top of that so a HiDPI
/// (scale-2) display doesn't hit the exact same problem one step up.

use gtk::gdk;
use gtk::prelude::*;
use std::collections::HashMap;

/// Must stay in sync with `ui/playback.rs`'s `icon_size` for the main
/// window's no-art `FlipCover` fallback (currently 128.0) — see this
/// module's doc comment for why that's the size that has to drive the
/// shared lookup resolution.
const LOOKUP_SIZE:  i32 = 128;
const LOOKUP_SCALE: i32 = 2;

fn theme_icon(theme: &gtk::IconTheme, name: &str) -> gdk::Paintable {
    theme.lookup_icon(
        name, &[], LOOKUP_SIZE, LOOKUP_SCALE,
        gtk::TextDirection::None,
        gtk::IconLookupFlags::empty(),
    )
    .upcast::<gdk::Paintable>()
}

// ── IconSet ───────────────────────────────────────────────────────────────────

/// All application icons, pre-loaded at startup as `gdk::Paintable`.
pub struct IconSet {
    /// Input source ID → paintable.  Custom SVG assets take priority over
    /// Adwaita.  Filled at startup for every known source ID.
    sources: HashMap<&'static str, gdk::Paintable>,
    /// Returned by `source_paintable` when the ID is not in `sources`.
    source_fallback: gdk::Paintable,

    /// Output mode string → paintable.  Currently empty; add entries here as
    /// per-output-mode icons are introduced.
    outputs: HashMap<&'static str, gdk::Paintable>,
    /// Returned by `output_paintable` when the mode is not in `outputs`.
    output_fallback: gdk::Paintable,

    /// BLE remote icon, shown in the main window's bottom status bar.
    remote: gdk::Paintable,
}

impl IconSet {
    /// Pre-load every icon.  Call once from `build_ui`, after the GDK display
    /// is initialised (and after `init_icon_resource()` has registered the
    /// icon GResource — see this module's doc comment).
    pub fn load() -> Self {
        let display = gdk::Display::default().expect("GDK display not available");
        let theme   = gtk::IconTheme::for_display(&display);

        // Every input-source ID this app knows an icon for, custom
        // (`rustywiim-*`, from `rustywiim.gresource.xml`) or Adwaita
        // symbolic — one lookup mechanism for both, see the module doc
        // comment. Multiple IDs sharing one icon (e.g. the RCA graphic
        // covering `"line-in"`/`"line-in-2"`/`"RCA"`) just repeat the name.
        let source_names: &[(&'static str, &str)] = &[
            ("line-in",       "rustywiim-rca-inout"),
            ("line-in-2",     "rustywiim-rca-inout"),
            ("RCA",           "rustywiim-rca-inout"),
            ("optical",       "rustywiim-optical-inout"),
            ("coaxial",       "rustywiim-coax-inout"),
            ("wifi",          "network-wireless-symbolic"),
            ("bluetooth",     "bluetooth-symbolic"),
            ("phono",         "media-record-symbolic"),
            ("udisk",         "drive-harddisk-usb-symbolic"),
            ("HDMI",          "tv-symbolic"),
            // Find (or make) a better icon for a stereo jack
            ("line-in-jack",  "audio-headphones-symbolic"),
        ];
        let output_names: &[(&'static str, &str)] = &[
            ("line-out",      "rustywiim-rca-inout"),
            ("optical-out",   "rustywiim-optical-inout"),
            ("coax-out",      "rustywiim-coax-inout"),
            ("bluetooth-out", "bluetooth-symbolic"),
            ("headphone-out", "audio-headphones-symbolic"),
            ("usb-out",       "drive-harddisk-usb-symbolic"),
            ("speaker-out",   "audio-speakers-symbolic"),
        ];

        let sources: HashMap<&'static str, gdk::Paintable> = source_names.iter()
            .map(|&(id, name)| (id, theme_icon(&theme, name)))
            .collect();
        let outputs: HashMap<&'static str, gdk::Paintable> = output_names.iter()
            .map(|&(id, name)| (id, theme_icon(&theme, name)))
            .collect();

        let source_fallback = theme_icon(&theme, "audio-card-symbolic");
        let output_fallback = theme_icon(&theme, "rustywiim-audio-output");
        let remote           = theme_icon(&theme, "rustywiim-remote");

        Self { sources, source_fallback, outputs, output_fallback, remote }
    }

    // ── Accessors ─────────────────────────────────────────────────────────────

    /// Paintable for an input source ID (e.g. `"optical"`, `"line-in"`,
    /// `"wifi"`).  Falls back to `source_fallback` for unknown IDs.
    pub fn source_paintable(&self, id: &str) -> &gdk::Paintable {
        self.sources.get(id).unwrap_or(&self.source_fallback)
    }

    /// Paintable for an audio output mode string (e.g. `"AUDIO_OUTPUT_COAX_MODE"`).
    ///
    /// Returns the per-mode icon if one has been registered, otherwise the
    /// output fallback (currently the custom audio-output SVG).
    pub fn output_paintable(&self, id: &str) -> &gdk::Paintable {
        self.outputs.get(id).unwrap_or(&self.output_fallback)
    }

    /// The BLE remote icon (`icons/wiim-remote.svg`).
    pub fn remote_paintable(&self) -> &gdk::Paintable {
        &self.remote
    }
}
