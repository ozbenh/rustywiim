//! Formatting/update helpers shared by the view objects (and, until the
//! Phase-2 view split completes, by `ui/playback.rs`'s window-driven
//! update paths) — one home so the full and mini playback displays can't
//! drift on how the same field is rendered.

use adw::prelude::*;

use crate::device::playback::{AudioQuality, PlaybackStatus, RepeatMode};
use crate::device::state::DeviceState;
use crate::ui::icons::IconSet;
use crate::ui::scroll_fade_label::ScrollFadeLabel;
use super::volume::VolumeControl;

/// Two `ScrollFadeLabel`s in a `gtk::Stack`, slid between on text change
/// instead of a hard swap. `set_text()` matches `ScrollFadeLabel`'s own
/// signature, so call sites don't change.
#[derive(Clone)]
pub(crate) struct SwipeText {
    pub stack: gtk::Stack,
    a: ScrollFadeLabel,
    b: ScrollFadeLabel,
}

impl SwipeText {
    pub(crate) fn new(
        initial: &str, css_class: &str, center_when_fits: bool, drop_shadow: bool,
        speed_multiplier: f64,
    ) -> Self {
        let a = ScrollFadeLabel::with_speed_multiplier(initial, speed_multiplier);
        let b = ScrollFadeLabel::with_speed_multiplier("", speed_multiplier);
        for l in [&a, &b] {
            l.add_label_css_class(css_class);
            l.set_hexpand(true);
            l.set_center_when_fits(center_when_fits);
            l.set_drop_shadow(drop_shadow);
        }
        let stack = gtk::Stack::new();
        stack.set_hexpand(true);
        stack.set_transition_duration(250);
        stack.add_named(&a, Some("a"));
        stack.add_named(&b, Some("b"));
        stack.set_visible_child_name("a");
        Self { stack, a, b }
    }

    /// Swap to `text`, sliding the new label in over the old one.
    /// No-op if `text` already matches what's currently shown.
    pub fn set_text(&self, text: &str) {
        let showing_a = self.stack.visible_child_name().as_deref() == Some("a");
        let (outgoing, incoming, name) =
            if showing_a { (&self.a, &self.b, "b") } else { (&self.b, &self.a, "a") };
        // Compare against what's actually on screen (outgoing), not the
        // hidden face — the hidden face still holds leftover text from
        // *two* changes ago, so comparing against it wrongly no-ops (and
        // leaves the stale visible label on screen) whenever a new title
        // happens to coincide with that stale leftover, e.g. a repeated
        // track title or a transient empty-title flicker that reverts.
        if outgoing.text() == text { return; }
        incoming.set_text(text);
        let transition = if crate::config::with(|cfg| cfg.animations) {
            gtk::StackTransitionType::SlideLeft
        } else {
            gtk::StackTransitionType::None
        };
        self.stack.set_visible_child_full(name, transition);
    }

    /// Override both faces' center-when-fits behavior after construction —
    /// for layouts that need left-aligned text regardless of the default
    /// passed to `new()`.
    pub(crate) fn set_center_when_fits(&self, center: bool) {
        self.a.set_center_when_fits(center);
        self.b.set_center_when_fits(center);
    }
}

// ── String helpers ────────────────────────────────────────────────────────────

pub(crate) fn is_unknown(s: &str) -> bool {
    s.is_empty() || s.eq_ignore_ascii_case("unknown") || s.eq_ignore_ascii_case("unknow")
        || s.eq_ignore_ascii_case("<unknown>")
}

/// "▶ Playing" — used by the full and WideRight layouts, which show the
/// service name as its own element (`ServiceLabel`) rather than appended
/// to the status text.
pub(crate) fn format_status_only(status: &PlaybackStatus) -> String {
    match status {
        PlaybackStatus::Playing      => "▶ Playing",
        PlaybackStatus::Paused       => "⏸ Paused",
        PlaybackStatus::Stopped      => "⏹ Stopped",
        PlaybackStatus::Loading      => "⏳ Loading",
        PlaybackStatus::Unknown(raw) => raw.as_str(),
    }.to_string()
}

/// Just the glyph, no word — the Mini window's status label shows only
/// this (service/quality get their own separate badges there instead, same
/// as the full/WideRight layouts). `Unknown` has no icon vocabulary to
/// fall back to, so it shows the raw wire status verbatim, same as
/// elsewhere in this module (an unrecognized-but-readable string beats
/// nothing).
pub(crate) fn format_status_icon_only(status: &PlaybackStatus) -> &str {
    match status {
        PlaybackStatus::Playing      => "▶",
        PlaybackStatus::Paused       => "⏸",
        PlaybackStatus::Stopped      => "⏹",
        PlaybackStatus::Loading      => "⏳",
        PlaybackStatus::Unknown(raw) => raw.as_str(),
    }
}

/// Replaces the plain status text entirely while Bluetooth is the active
/// input — play/pause state isn't meaningful for an external A2DP source
/// the way it is for a real queue, so the connection state is more useful
/// here than "▶ Playing". `device_name` is only meaningful
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

/// "1571 kbps | 48.0 kHz | 24-bit" — bitrate/sample-rate/bit-depth only.
/// No codec-label prefix (`"FLAC · ..."`) — that's the separate
/// `translate_quality_badge()`-driven badge next to the service label/icon
/// instead of glued onto this string.
pub(crate) fn format_quality_line(q: &AudioQuality) -> String {
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
    parts.join(" | ")
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

/// Icon + "Restart Pairing" label. `css_class`/`icon_px` let the mini
/// playback display use a smaller variant than the main window's rather
/// than duplicating the icon+label+button assembly. Not `.transport-btn`
/// (its `border-radius:50%`/`padding:0`/fixed size is tuned for a single
/// glyph and would clip a text label) — a dedicated class instead, styled
/// in `system.css`/`dark.css`/`modern.css`.
pub(crate) fn build_bt_pair_button(css_class: &str, icon_px: i32) -> gtk::Button {
    let icon = gtk::Image::builder()
        .icon_name("bluetooth-symbolic")
        .pixel_size(icon_px)
        .build();
    let label = gtk::Label::builder().label("Restart Pairing").build();
    let content = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal).spacing(6)
        .halign(gtk::Align::Center)
        .build();
    content.append(&icon);
    content.append(&label);
    let btn = gtk::Button::builder()
        .css_classes(["flat", css_class])
        .tooltip_text("Restart Bluetooth pairing")
        .halign(gtk::Align::Center)
        .visible(false)
        .build();
    btn.set_child(Some(&content));
    btn
}

/// Displays the active streaming service's name (`PlaybackState::source_name`)
/// as its own element, separate from the plain status text
/// (`format_status_only()`/`format_status_icon_only()`) — shown as a
/// brand-mark icon when one is registered in `IconSet`
/// (`IconSet::service_paintable()`), falling back to the plain text name
/// for every other service. Used by every layout (Classic/WideRight each
/// pack `widget` into their own spot; Mini sizes the icon down via
/// `set_icon_pixel_size()` to match its own smaller text). `css_class` is
/// applied to both the icon and the label, but only the label actually
/// uses it for anything — the icon's own fill color is baked into its SVG
/// (a plain custom icon, not a GTK symbolic one; see `IconSet::services`'s
/// doc comment for why), so the shared class just keeps both under one
/// name for callers to reason about together.
#[derive(Clone, Debug)]
pub(crate) struct ServiceLabel {
    pub widget: gtk::Box,
    icon:  gtk::Image,
    label: gtk::Label,
}

impl ServiceLabel {
    pub(crate) fn new(css_class: &str) -> Self {
        let icon = gtk::Image::builder()
            .pixel_size(36).visible(false).css_classes([css_class])
            .build();
        // "service-name-pill" (rounded-rect outline, same look as Kiosk
        // mode's device-name button) only on the text fallback — the icon
        // is a plain badge, no backdrop. `valign(Center)`: without it the
        // label's box defaults to `Fill` and stretches to match this row's
        // tallest sibling (e.g. the icon in the other, icon-shown case),
        // so the pill's border paints around that stretched allocation
        // instead of hugging the text's own natural size.
        let label = gtk::Label::builder()
            .css_classes([css_class, "service-name-pill"])
            .valign(gtk::Align::Center)
            .build();
        let widget = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal).spacing(6)
            .halign(gtk::Align::Start)
            .build();
        widget.append(&icon);
        widget.append(&label);
        Self { widget, icon, label }
    }

    /// WideRight/Kiosk-only: the icon's `pixel_size` is a widget property,
    /// not something CSS font-size scaling reaches, so `apply_wide_right_scale()`
    /// sets it directly to keep the icon proportional to the dynamically
    /// scaled `.service-name` text next to it (Classic never calls this —
    /// its icon stays the fixed default set in `new()`).
    pub(crate) fn set_icon_pixel_size(&self, px: i32) {
        self.icon.set_pixel_size(px);
    }

    /// `name` is `PlaybackState::source_name` as-is (already the decoded
    /// display string, e.g. "Spotify"/"TIDAL Connect") — `None`/empty
    /// hides the whole element rather than showing a blank row.
    pub(crate) fn set(&self, name: Option<&str>, icons: &IconSet) {
        let name = name.unwrap_or("");
        if name.is_empty() {
            self.widget.set_visible(false);
            return;
        }
        self.widget.set_visible(true);
        match icons.service_paintable(name) {
            Some(paintable) => {
                self.icon.set_paintable(Some(paintable));
                self.icon.set_visible(true);
                self.label.set_visible(false);
            }
            None => {
                self.icon.set_visible(false);
                self.label.set_visible(true);
                self.label.set_label(name);
            }
        }
    }
}

/// Displays `translate_quality_badge()`'s translated quality-tier string
/// (e.g. "Hi-Res"/"CD"/"MQA") next to `ServiceLabel` — same icon-vs-text
/// pattern as that widget (`IconSet::quality_paintable()` instead of
/// `service_paintable()`), for tiers with a recognizable certification/
/// brand mark (currently just Qobuz's "Hi-Res" tier) rather than every
/// tier getting its own icon.
#[derive(Clone, Debug)]
pub(crate) struct QualityBadge {
    pub widget: gtk::Box,
    icon:  gtk::Image,
    label: gtk::Label,
}

impl QualityBadge {
    pub(crate) fn new(css_class: &str) -> Self {
        // 36 (ServiceLabel's own default) minus ~20% — reads as a smaller,
        // secondary accent next to the service brand mark rather than
        // competing with it at equal size.
        let icon = gtk::Image::builder()
            .pixel_size(29).visible(false).css_classes([css_class])
            .build();
        let label = gtk::Label::builder()
            .css_classes([css_class, "service-name-pill"])
            .valign(gtk::Align::Center)
            .build();
        let widget = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal).spacing(6)
            .halign(gtk::Align::Start).valign(gtk::Align::Center)
            .visible(false)
            .build();
        widget.append(&icon);
        widget.append(&label);
        Self { widget, icon, label }
    }

    /// See `ServiceLabel::set_icon_pixel_size()` — same reason
    /// (`apply_wide_right_scale()` keeps this proportional to the
    /// dynamically scaled `.service-name` text next to it).
    pub(crate) fn set_icon_pixel_size(&self, px: i32) {
        self.icon.set_pixel_size(px);
    }

    /// `text` is `translate_quality_badge()`'s already-translated display
    /// string, e.g. "Hi-Res" — `None`/empty hides the whole element.
    pub(crate) fn set(&self, text: Option<&str>, icons: &IconSet) {
        let text = text.unwrap_or("");
        if text.is_empty() {
            self.widget.set_visible(false);
            return;
        }
        self.widget.set_visible(true);
        match icons.quality_paintable(text) {
            Some(paintable) => {
                self.icon.set_paintable(Some(paintable));
                self.icon.set_visible(true);
                self.label.set_visible(false);
            }
            None => {
                self.icon.set_visible(false);
                self.label.set_visible(true);
                self.label.set_label(text);
            }
        }
    }
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

/// The transport/volume keyboard shortcuts (prev/next/play-pause/volume
/// up-down) shared by every host of a playback view — factored out of
/// `device_window/display.rs`'s `handle_transport_key()` so a host doesn't
/// have to reimplement this against `DeviceState` itself. Host-specific
/// keys (the main/mini window's "M" toggle, Kiosk mode's "K") stay in each
/// host's own key controller, which calls this for everything else.
/// `Proceed` (not `Stop`) when a button isn't sensitive or the key isn't
/// one of these — behaves as if the shortcut didn't exist rather than
/// swallowing the key.
pub(crate) fn handle_transport_key(
    ds:       &DeviceState,
    volume:   &VolumeControl,
    prev_btn: &gtk::Button,
    next_btn: &gtk::Button,
    play_btn: &gtk::Button,
    keyval:   gtk::gdk::Key,
) -> glib::Propagation {
    match keyval {
        gtk::gdk::Key::Left if prev_btn.is_sensitive() => {
            ds.do_prev();
            flash_button(prev_btn);
            glib::Propagation::Stop
        }
        gtk::gdk::Key::Right if next_btn.is_sensitive() => {
            ds.do_next();
            flash_button(next_btn);
            glib::Propagation::Stop
        }
        gtk::gdk::Key::space if play_btn.is_sensitive() => {
            ds.do_play_pause();
            flash_button(play_btn);
            glib::Propagation::Stop
        }
        gtk::gdk::Key::Up => {
            volume.step(5);
            glib::Propagation::Stop
        }
        gtk::gdk::Key::Down => {
            volume.step(-5);
            glib::Propagation::Stop
        }
        _ => glib::Propagation::Proceed,
    }
}
