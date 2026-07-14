//! Formatting/update helpers shared by the view objects (and, until the
//! Phase-2 view split completes, by `ui/playback.rs`'s window-driven
//! update paths) — one home so the full and mini playback displays can't
//! drift on how the same field is rendered.

use adw::prelude::*;

use crate::device::playback::{AudioQuality, PlaybackStatus, RepeatMode};

// ── String helpers ────────────────────────────────────────────────────────────

pub(crate) fn is_unknown(s: &str) -> bool {
    s.is_empty() || s.eq_ignore_ascii_case("unknown") || s.eq_ignore_ascii_case("unknow")
        || s.eq_ignore_ascii_case("<unknown>")
}

/// "▶ Playing · AirPlay" style label. Presentation only — `status`/
/// `source_name` are already decoded (`device::playback::decode_status_http`/
/// `decode_source_name_http`), so this just picks a glyph and joins.
pub(crate) fn format_status_line(status: &PlaybackStatus, source_name: Option<&str>) -> String {
    let state = match status {
        PlaybackStatus::Playing      => "▶ Playing",
        PlaybackStatus::Paused       => "⏸ Paused",
        PlaybackStatus::Stopped      => "⏹ Stopped",
        PlaybackStatus::Loading      => "⏳ Loading",
        PlaybackStatus::Unknown(raw) => raw.as_str(),
    };
    match source_name {
        Some(s) => format!("{state} · {s}"),
        None    => state.to_string(),
    }
}

/// Replaces `format_status_line` entirely while Bluetooth is the active
/// input — play/pause state isn't meaningful for an external A2DP source
/// the way it is for a real queue, so the connection state is more useful
/// here than "▶ Playing · Bluetooth". `device_name` is only meaningful
/// when `connected` (see `PlaybackState::bt_device_name`'s doc comment);
/// a connected sink that didn't report a name still says "connected"
/// rather than nothing. `pairing` (only meaningful while disconnected —
/// a sink that's actively connected isn't simultaneously discoverable)
/// takes priority over the plain "disconnected" text, since it's a more
/// specific/useful thing to tell the user.
pub(crate) fn format_bt_status_line(connected: bool, device_name: Option<&str>, pairing: bool) -> String {
    match (connected, device_name) {
        (true, Some(name)) => format!("Bluetooth: {name}"),
        (true, None)       => "Bluetooth: connected".to_string(),
        (false, _) if pairing => "Bluetooth: Ready to pair".to_string(),
        (false, _)         => "Bluetooth disconnected".to_string(),
    }
}

/// "FLAC · 1571 kbps / 48.0 kHz / 24-bit" style string. The codec/quality
/// badge prefix is only ever present when the UPnP backend supplied it (see
/// `device::playback::decode_quality_upnp`) — presentation only, all the
/// numeric parsing already happened in `decode_quality_http`/`decode_quality_upnp`.
pub(crate) fn format_quality_line(q: &AudioQuality, codec_label: Option<&str>) -> String {
    let mut parts = Vec::new();
    if let Some(kbps) = q.bit_rate_kbps {
        parts.push(format!("{kbps:.0} kbps"));
    }
    if let Some(khz) = q.sample_rate_khz {
        parts.push(format!("{khz:.1} kHz"));
    }
    if let Some(bd) = q.bit_depth {
        parts.push(format!("{bd}-bit"));
    }
    let line = parts.join(" / ");
    match codec_label {
        Some(label) if !line.is_empty() => format!("{label} · {line}"),
        Some(label)                     => label.to_string(),
        None                             => line,
    }
}

pub(crate) fn vol_icon(muted: bool, vol: f64) -> &'static str {
    if muted || vol == 0.0 { return "audio-volume-muted-symbolic"; }
    if vol <= 33.0 { "audio-volume-low-symbolic" }
    else if vol <= 66.0 { "audio-volume-medium-symbolic" }
    else { "audio-volume-high-symbolic" }
}

// ── Loop helpers ──────────────────────────────────────────────────────────────

pub(crate) fn apply_shuffle_ui(btn: &gtk::Button, on: bool) {
    if on { btn.add_css_class("loop-active"); }
    else   { btn.remove_css_class("loop-active"); }
    btn.set_tooltip_text(Some(if on { "Shuffle: On" } else { "Shuffle: Off" }));
}

pub(crate) fn apply_repeat_ui(btn: &gtk::Button, state: RepeatMode) {
    let (icon, tip) = match state {
        RepeatMode::Off => ("media-playlist-repeat-symbolic",      "Repeat: Off"),
        RepeatMode::All => ("media-playlist-repeat-symbolic",      "Repeat: All"),
        RepeatMode::One => ("media-playlist-repeat-song-symbolic", "Repeat: One"),
    };
    btn.set_icon_name(icon);
    btn.set_tooltip_text(Some(tip));
    if state == RepeatMode::Off { btn.remove_css_class("loop-active"); }
    else                        { btn.add_css_class("loop-active"); }
}

/// Briefly apply the "key-flash" CSS class to `btn`, then remove it — the
/// visual acknowledgement for a keyboard-triggered prev/next/play-pause.
/// Deliberately our own class, not libadwaita's built-in `.suggested-action`:
/// `.transport-btn`/`.play-btn` (and their mini-window equivalents) set an
/// explicit `background-color` in `dark.css`/`system.css`, loaded at
/// `STYLE_PROVIDER_PRIORITY_APPLICATION` — GTK's cascade resolves provider
/// priority *before* selector specificity, so that plain background-color
/// always wins over `.suggested-action` (a lower-priority, THEME-level
/// libadwaita class) regardless of which class list order or specificity.
/// `key-flash` is defined in our own stylesheets with compound selectors
/// that outrank the base rule instead.
pub(crate) fn flash_button(btn: &gtk::Button) {
    btn.add_css_class("key-flash");
    let btn = btn.clone();
    glib::timeout_add_local_once(std::time::Duration::from_millis(200), move || {
        btn.remove_css_class("key-flash");
    });
}
