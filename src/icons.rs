/// Centralised icon handling for RustyWiiM.
///
/// `IconSet::load()` must be called once from `build_ui`, after the GDK
/// display is available.  From that point every icon is a pre-loaded
/// `gdk::Paintable`; custom SVG assets take priority, Adwaita symbolic icons
/// fill the gaps.  No icon-name strings are scattered through the rest of the
/// codebase.

use gtk::gdk;
use gtk::prelude::*;
use std::collections::HashMap;

// ── Embedded assets ───────────────────────────────────────────────────────────

/// "Box with outward arrow" icon, used as the output-set fallback.
static AUDIO_OUTPUT_SVG: &[u8] = include_bytes!("icons/audio-output.svg");

/// RCA connector icon
static RCA_INOUT_SVG: &[u8] = include_bytes!("icons/rca-inout.svg");

/// Optical connector icon
static OPTICAL_INOUT_SVG: &[u8] = include_bytes!("icons/optical-inout.svg");

/// Coax connector icon
static COAX_INOUT_SVG: &[u8] = include_bytes!("icons/coax-inout.svg");


// ── Internal loaders ──────────────────────────────────────────────────────────

fn try_texture(bytes: &[u8]) -> Option<gdk::Paintable> {
    let gbytes = glib::Bytes::from(bytes);
    gdk::Texture::from_bytes(&gbytes).ok()
        .map(|t| t.upcast::<gdk::Paintable>())
}

fn theme_icon(theme: &gtk::IconTheme, name: &str) -> gdk::Paintable {
    theme.lookup_icon(
        name, &[], 64, 1,
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
}

impl IconSet {
    /// Pre-load every icon.  Call once from `build_ui`, after the GDK display
    /// is initialised.  Custom SVG assets take priority; Adwaita symbolic icons
    /// fill any gaps.
    pub fn load() -> Self {
        let display = gdk::Display::default().expect("GDK display not available");
        let theme   = gtk::IconTheme::for_display(&display);

        let mut sources: HashMap<&'static str, gdk::Paintable> = HashMap::new();
        let mut outputs: HashMap<&'static str, gdk::Paintable> = HashMap::new();

        // Custom full-colour assets take priority; insert before Adwaita pass.
        // Keys in `outputs` use canonical names from `canon_routine_output_name()`.
        if let Some(p) = try_texture(RCA_INOUT_SVG) {
            sources.insert("line-in",    p.clone());
            sources.insert("line-in-2",  p.clone());
            outputs.insert("line-out",   p);
        }
        if let Some(p) = try_texture(OPTICAL_INOUT_SVG) {
            sources.insert("optical",    p.clone());
            outputs.insert("optical-out", p);
        }
        if let Some(p) = try_texture(COAX_INOUT_SVG) {
            sources.insert("coaxial",   p.clone());
            outputs.insert("coax-out",  p);
        }
       
        // Adwaita symbolic fallbacks for every known source ID.
        // `or_insert_with` means a custom asset already inserted above wins.
        let adwaita_sources: &[(&'static str, &str)] = &[
            ("wifi",      "network-wireless-symbolic"),
            ("bluetooth", "bluetooth-symbolic"),
            ("phono",     "media-record-symbolic"),
            ("udisk",     "drive-harddisk-usb-symbolic"),
            ("HDMI",      "tv-symbolic"),
        ];
        let adwaita_outputs: &[(&'static str, &str)] = &[
            ("bluetooth-out", "bluetooth-symbolic"),
            ("headphone-out", "audio-headphones-symbolic"),
            ("usb-out",       "drive-harddisk-usb-symbolic"),
            ("hdmi-out",      "video-display-symbolic"),
        ];
        for &(id, name) in adwaita_sources {
            sources.entry(id).or_insert_with(|| theme_icon(&theme, name));
        }
        for &(id, name) in adwaita_outputs {
            outputs.entry(id).or_insert_with(|| theme_icon(&theme, name));
        }

        let source_fallback = theme_icon(&theme, "audio-card-symbolic");


        let output_fallback = try_texture(AUDIO_OUTPUT_SVG)
            .unwrap_or_else(|| theme_icon(&theme, "audio-speakers-symbolic"));

        Self { sources, source_fallback, outputs, output_fallback }
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
}
