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
/// fallback (`views/playback_full.rs`'s `update_artwork()`) was visibly
/// pixelated — a 2× upscale of that 64px raster, worst on the jack icon's
/// fine line detail.
/// `LOOKUP_SIZE` below is the largest logical size any caller actually
/// displays one of these at — every smaller use (devlist row, mini window,
/// remote icon) downscales from it instead, which stays sharp.
/// `LOOKUP_SCALE` requests a 2×-native raster on top of that so a HiDPI
/// (scale-2) display doesn't hit the exact same problem one step up.

use gtk::gdk;
use gtk::gio;
use gtk::prelude::*;
use std::collections::HashMap;
use std::rc::Rc;

/// Must stay ≥ the largest logical `pixel_size`/`icon_size` any caller
/// actually requests — currently that's `views/playback_full.rs`'s main
/// window no-art `FlipCover` fallback (128.0, fixed) and, since Kiosk's
/// WideRight layout scales the service/quality badge icons proportionally
/// to screen size (`apply_wide_right_scale()`'s `service_icon_px`), a
/// value that grows with the display — confirmed live on a 4K screen
/// (~1267px WideRight artwork side) reaching ~149px, already past the old
/// 128. Bumped to 256 for headroom above that, not just to match it
/// exactly — see this module's doc comment for why undershooting this
/// constant means a highlighted rasterized-then-upscaled icon (this was
/// confirmed live for the Hi-Res Audio quality badge specifically: its
/// gradient/photographic detail shows upscaling blur far more readily
/// than the flat single-color brand marks do).
const LOOKUP_SIZE:  i32 = 256;
const LOOKUP_SCALE: i32 = 2;

fn theme_icon(theme: &gtk::IconTheme, name: &str) -> gdk::Paintable {
    theme.lookup_icon(
        name, &[], LOOKUP_SIZE, LOOKUP_SCALE,
        gtk::TextDirection::None,
        gtk::IconLookupFlags::empty(),
    )
    .upcast::<gdk::Paintable>()
}

/// Read `name`'s raw bytes straight out of the embedded GResource bundle
/// (bypassing `gtk::IconTheme` entirely) — same resource path convention
/// `theme.rs`'s `init_icon_resource()` registers
/// (`/<APP_ID as path>/icons/scalable/apps/<name>.svg`), just fetched as
/// plain data instead of looked up/rasterized as an icon. `name` is the
/// same GResource alias already used for the (unrelated) `-symbolic`
/// real-icon-theme path elsewhere — that suffix means nothing here, it's
/// just whatever string the alias happens to be.
fn load_svg_resource(name: &str) -> Rc<[u8]> {
    let path = format!(
        "/{}/icons/scalable/apps/{name}.svg",
        super::APP_ID.replace('.', "/"),
    );
    let bytes = gio::resources_lookup_data(&path, gio::ResourceLookupFlags::NONE)
        .unwrap_or_else(|e| panic!("bad embedded GResource — missing {path}: {e}"));
    Rc::from(bytes.as_ref())
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

    /// Streaming-service name → the brand mark's raw SVG source bytes,
    /// keyed by the lowercased display string (e.g. `"spotify"`,
    /// `"tidal connect"`) — see `service_names` in `load()`;
    /// `service_svg()` lowercases its own query the same way, for a
    /// case-insensitive match against a raw wire vendor string too, not
    /// just the fully-normalized display form. Only covers services with
    /// a bundled brand-mark SVG; anything else (Radio Paradise, vTuner,
    /// ...) simply isn't a key here. No fallback field, unlike
    /// `sources`/`outputs` above: `None` here is a real, meaningful answer
    /// ("no icon for this one, show text"), not a gap to paper over —
    /// every caller (`ServiceLabel` in `ui/views/common.rs`) falls back to
    /// the plain text name.
    ///
    /// Unlike `sources`/`outputs`/`qualities`, these are **not** pre-baked
    /// via `theme_icon()`/`gtk::IconTheme::lookup_icon()` into a fixed
    /// `gdk::Paintable` — that path bakes a *square* raster regardless of
    /// the source SVG's own proportions (confirmed live:
    /// `GtkIconPaintable::intrinsic_aspect_ratio()` reports exactly `1`
    /// no matter what), which breaks for the several of these that are
    /// genuinely wide wordmarks, not square marks (see
    /// `ui/brand_icon.rs`'s doc comment for the full history and the
    /// confirmed aspect ratios). `BrandIcon` instead re-parses and
    /// re-rasterizes this raw source with `resvg` on every paint, at the
    /// exact target color/size needed — so what's cached here is just the
    /// source bytes, read once via `gio::resources_lookup_data()` (cheap
    /// to clone out, `Rc`-shared).
    services: HashMap<String, Rc<[u8]>>,

    /// `translate_quality_badge()`'s translated display string (e.g.
    /// `"Hi-Res"`), lowercased, → paintable — same shape/rationale as
    /// `services` above (no fallback field; `QualityBadge` in
    /// `ui/views/common.rs` falls back to the plain text pill). Currently
    /// just Qobuz's "Hi-Res" tier (codes `7`/`27`); add more entries here
    /// as other badge icons show up.
    qualities: HashMap<String, gdk::Paintable>,
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
            ("line-in-jack",  "rustywiim-jack-inout"),
            ("optical",       "rustywiim-optical-inout"),
            ("coaxial",       "rustywiim-coax-inout"),
            ("HDMI",          "rustywiim-hdmi-inout"),
            ("wifi",          "network-wireless-symbolic"),
            ("bluetooth",     "bluetooth-symbolic"),
            ("phono",         "media-record-symbolic"),
            ("udisk",         "drive-harddisk-usb-symbolic"),
        ];
        let output_names: &[(&'static str, &str)] = &[
            ("line-out",      "rustywiim-rca-inout"),
            ("optical-out",   "rustywiim-optical-inout"),
            ("coax-out",      "rustywiim-coax-inout"),
            ("jack-out",      "rustywiim-jack-inout"),
            ("bluetooth-out", "bluetooth-symbolic"),
            ("headphone-out", "audio-headphones-symbolic"),
            ("usb-out",       "drive-harddisk-usb-symbolic"),
            ("speaker-out",   "audio-speakers-symbolic"),
        ];
        // Keyed by the display strings `device::playback::vendor_display()`/
        // `decode_source_name_http()`/`decode_source_name_upnp()` actually
        // produce (`PlaybackState::source_name`) — case-insensitively (both
        // these keys and `service_svg()`'s query are lowercased), so
        // a raw not-yet-normalized vendor string matches too, not just the
        // fully-normalized display form. `"TIDAL Connect"` and `"TIDAL"`
        // (the Connect-specific and plain-radio source names — see
        // `mode_from_play_medium()`/`vendor_display()`) share one icon.
        // Services with no bundled SVG yet (Radio Paradise, vTuner, ...)
        // are simply absent — see `service_svg()`'s doc comment for
        // the text-fallback path.
        let service_names: &[(&'static str, &str)] = &[
            ("Spotify",       "rustywiim-svc-spotify-symbolic"),
            ("TIDAL Connect", "rustywiim-svc-tidal-symbolic"),
            ("TIDAL",         "rustywiim-svc-tidal-symbolic"),
            ("Qobuz",         "rustywiim-svc-qobuz-symbolic"),
            ("Qobuz Connect", "rustywiim-svc-qobuz-symbolic"),
            ("Deezer",        "rustywiim-svc-deezer-symbolic"),
            ("Pandora",       "rustywiim-svc-pandora-symbolic"),
            ("Napster",       "rustywiim-svc-napster-symbolic"),
            ("iHeartRadio",   "rustywiim-svc-iheartradio-symbolic"),
            ("TuneIn",        "rustywiim-svc-tunein-symbolic"),
            ("Amazon Music",  "rustywiim-svc-amazon-symbolic"),
        ];
        // Keyed by `translate_quality_badge()`'s translated display string
        // (case-insensitively, same as `service_names` above).
        let quality_names: &[(&'static str, &str)] = &[
            ("Hi-Res", "rustywiim-hires-audio-logo"),
        ];

        let sources: HashMap<&'static str, gdk::Paintable> = source_names.iter()
            .map(|&(id, name)| (id, theme_icon(&theme, name)))
            .collect();
        let outputs: HashMap<&'static str, gdk::Paintable> = output_names.iter()
            .map(|&(id, name)| (id, theme_icon(&theme, name)))
            .collect();
        // Lowercased keys — `service_svg()` also lowercases its query, so
        // a lookup against a raw not-yet-normalized vendor string (e.g.
        // `"newtunein"`, the wire spelling `vendor_display()` itself
        // already translates to `"TuneIn"` before this is normally
        // reached) still matches case-insensitively rather than requiring
        // the exact display-string casing.
        let services: HashMap<String, Rc<[u8]>> = service_names.iter()
            .map(|&(id, name)| (id.to_lowercase(), load_svg_resource(name)))
            .collect();
        let qualities: HashMap<String, gdk::Paintable> = quality_names.iter()
            .map(|&(id, name)| (id.to_lowercase(), theme_icon(&theme, name)))
            .collect();

        let source_fallback = theme_icon(&theme, "audio-card-symbolic");
        let output_fallback = theme_icon(&theme, "rustywiim-audio-output");
        let remote           = theme_icon(&theme, "rustywiim-remote");

        Self { sources, source_fallback, outputs, output_fallback, remote, services, qualities }
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

    /// Raw SVG source for a streaming service's brand mark —
    /// case-insensitive match against its display name (e.g. `"Spotify"`,
    /// `"TIDAL Connect"` — `PlaybackState::source_name` as-is) or a raw
    /// not-yet-normalized vendor string. `None` (not a fallback icon) when
    /// no matching SVG is registered — see `services`'s own doc comment;
    /// callers are expected to fall back to the service's text name. Feed
    /// this straight to `BrandIcon::set_svg()`, which re-rasterizes it at
    /// the right color/size itself — this is source bytes, not a
    /// ready-to-display paintable.
    pub fn service_svg(&self, name: &str) -> Option<Rc<[u8]>> {
        self.services.get(&name.to_lowercase()).cloned()
    }

    /// Paintable for a translated quality-badge label (e.g. `"Hi-Res"` —
    /// `translate_quality_badge()`'s output, not a raw wire code) —
    /// case-insensitive, same shape as `service_svg()`. `None` when
    /// no matching icon is registered; callers fall back to the plain text
    /// pill.
    pub fn quality_paintable(&self, label: &str) -> Option<&gdk::Paintable> {
        self.qualities.get(&label.to_lowercase())
    }
}
