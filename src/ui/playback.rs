#![allow(deprecated)] // glib clone! old-style @strong syntax

use std::rc::Rc;

use adw::prelude::*;

use crate::{device::{api, capabilities}, config};
use crate::device::playback::{AudioQuality, PlaybackStatus, RepeatMode};

use super::*;

// ── String helpers ────────────────────────────────────────────────────────────

fn is_unknown(s: &str) -> bool {
    s.is_empty() || s.eq_ignore_ascii_case("unknown") || s.eq_ignore_ascii_case("unknow")
}

/// "▶ Playing · AirPlay" style label. Presentation only — `status`/
/// `source_name` are already decoded (`device::playback::decode_status_http`/
/// `decode_source_name_http`), so this just picks a glyph and joins.
fn format_status_line(status: &PlaybackStatus, source_name: Option<&str>) -> String {
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

/// "320 kbps / 44.1 kHz / 16-bit" style string. Presentation only — the
/// numeric parsing already happened in `device::playback::decode_quality_http`.
fn format_quality_line(q: &AudioQuality) -> String {
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
    parts.join(" / ")
}

fn vol_icon(muted: bool, vol: f64) -> &'static str {
    if muted || vol == 0.0 { return "audio-volume-muted-symbolic"; }
    if vol <= 33.0 { "audio-volume-low-symbolic" }
    else if vol <= 66.0 { "audio-volume-medium-symbolic" }
    else { "audio-volume-high-symbolic" }
}

// ── Loop helpers ──────────────────────────────────────────────────────────────

fn apply_shuffle_ui(btn: &gtk::Button, on: bool) {
    if on { btn.add_css_class("loop-active"); }
    else   { btn.remove_css_class("loop-active"); }
    btn.set_tooltip_text(Some(if on { "Shuffle: On" } else { "Shuffle: Off" }));
}

fn apply_repeat_ui(btn: &gtk::Button, state: RepeatMode) {
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

// ── impl DeviceWindowInner ────────────────────────────────────────────────────

impl DeviceWindowInner {
    // ── Reset ─────────────────────────────────────────────────────────────────

    pub(super) fn reset_device_ui(&self, title: &str) {
        self.window.set_title(Some("RustyWiiM"));
        self.pw.title.set_text(title);
        self.pw.artist.set_text("");
        self.pw.album.set_text("");
        self.pw.status.set_label("");
        self.pw.quality.set_label("");
        self.pw.artwork.clear();
        self.art_bg.clear();
        self.dev_info_label.set_label("");
        self.ip_label.set_visible(false);

        for btn in self.pp.btns.iter() { btn.set_visible(false); }
        for lbl in self.pp.labels.iter() { lbl.set_label(""); }
        for pic in self.pp.pics.iter() {
            pic.set_paintable(None::<&gtk::gdk::Paintable>);
            pic.set_icon_name(Some("audio-x-generic-symbolic"));
        }

        *self.sw.updating.borrow_mut() = true;
        self.sw.dropdown.set_model(Some(&gtk::StringList::new(&["—"])));
        self.sw.dropdown.set_sensitive(false);
        *self.sw.updating.borrow_mut() = false;
        *self.sw.ids.borrow_mut()      = Vec::new();
        *self.sw.enabled.borrow_mut()  = Vec::new();

        *self.ow.updating.borrow_mut() = true;
        self.ow.dropdown.set_model(Some(&gtk::StringList::new(&["—"])));
        self.ow.dropdown.set_sensitive(false);
        self.ow.section.set_visible(false);
        *self.ow.modes.borrow_mut()       = Vec::new();
        *self.ow.canon_names.borrow_mut() = Vec::new();
        *self.ow.updating.borrow_mut()    = false;
    }

    /// Populate the entire UI from whatever the DeviceState currently has cached.
    /// Called on initial window creation and on every `device-changed` signal.
    /// Safe to call redundantly — all underlying setters are idempotent.
    pub(super) fn populate_all(&self) {
        use crate::device::state::playback_changed;
        self.update_network_icon();
        if self.ds.device_info().is_some() {
            self.apply_device_info();
            self.on_presets_changed();
        } else {
            let title = match self.ds.connection_state() {
                ConnectionState::Connecting => "Connecting…",
                ConnectionState::Failed     => "Disconnected",
                _                           => "",
            };
            self.reset_device_ui(title);
        }
        self.update_playback_ui(playback_changed::ALL);
        self.update_input_display();
        self.update_output_display();
    }

    // ── Source / Output / Network ─────────────────────────────────────────────

    pub(super) fn populate_source(&self) {
        let in_enable = self.ds.audio_inputs();
        let info = match self.ds.device_info() { Some(i) => i, None => return };
        let caps = match self.ds.capabilities() { Some(c) => c, None => return };
        let renames = self.ds.mode_renames();

        let (ids, enabled_flags): (Vec<String>, Vec<bool>) = if !in_enable.is_empty() {
            in_enable.iter().map(|e| (e.mode.clone(), e.is_enabled())).unzip()
        } else {
            let ids = capabilities::detect_inputs(caps.device_id, info.plm_support_value())
                .into_iter().map(|s| s.to_string()).collect::<Vec<_>>();
            let flags = vec![true; ids.len()];
            (ids, flags)
        };

        if ids.is_empty() {
            *self.sw.updating.borrow_mut() = true;
            self.sw.dropdown.set_model(Some(&gtk::StringList::new(&["—"])));
            self.sw.dropdown.set_sensitive(false);
            *self.sw.updating.borrow_mut() = false;
            *self.sw.ids.borrow_mut()      = Vec::new();
            *self.sw.enabled.borrow_mut()  = Vec::new();
            return;
        }

        let labels: Vec<String> = ids.iter().zip(enabled_flags.iter()).map(|(id, _)| {
            let std_name = capabilities::input_display_name(id).to_string();
            if let Some(user) = renames.get(id.as_str()) {
                if !user.is_empty() && user != &std_name {
                    return format!("{} ({})", user, std_name);
                }
            }
            std_name
        }).collect();

        let label_refs: Vec<&str> = labels.iter().map(String::as_str).collect();
        *self.sw.ids.borrow_mut()      = ids;
        *self.sw.enabled.borrow_mut()  = enabled_flags;
        *self.sw.updating.borrow_mut() = true;
        self.sw.dropdown.set_model(Some(&gtk::StringList::new(&label_refs)));
        self.sw.dropdown.set_selected(0);
        self.sw.dropdown.set_sensitive(true);
        *self.sw.updating.borrow_mut() = false;
    }

    pub(super) fn populate_output(&self) {
        if self.ds.capabilities().is_none() { return; }
        let output_names = self.ds.outputs();
        if output_names.is_empty() {
            *self.ow.updating.borrow_mut() = true;
            self.ow.dropdown.set_model(Some(&gtk::StringList::new(&["—"])));
            self.ow.dropdown.set_sensitive(false);
            self.ow.section.set_visible(false);
            *self.ow.modes.borrow_mut()       = Vec::new();
            *self.ow.canon_names.borrow_mut() = Vec::new();
            *self.ow.updating.borrow_mut()    = false;
            return;
        }

        let out_labels: Vec<&str> = output_names.iter()
            .map(|e: &api::OutputEntry| e.name.as_str())
            .collect();
        let modes: Vec<u32> = output_names.iter()
            .map(|e| capabilities::output_canon_to_mode(e.canon).unwrap_or(0))
            .collect();

        *self.ow.modes.borrow_mut()       = modes;
        *self.ow.canon_names.borrow_mut() = output_names.iter().map(|e| e.canon).collect();
        *self.ow.updating.borrow_mut()    = true;
        self.ow.dropdown.set_model(Some(&gtk::StringList::new(&out_labels)));
        self.ow.dropdown.set_sensitive(true);
        self.ow.section.set_visible(true);

        if let Some(os) = self.ds.output_status() {
            if let Ok(hw) = os.hardware.parse::<u32>() {
                let hw_canon = capabilities::canon_mode_output_name(hw);
                let names = self.ow.canon_names.borrow();
                if let Some(pos) = names.iter().position(|&n| n == hw_canon) {
                    self.ow.dropdown.set_selected(pos as u32);
                }
            }
        }
        *self.ow.updating.borrow_mut() = false;
    }

    pub(super) fn update_network_icon(&self) {
        match self.ds.netstat() {
            Some(0) => {
                self.net_icon.set_icon_name(Some("network-wired-symbolic"));
                self.net_icon.set_visible(true);
            }
            Some(2) => {
                let rssi = self.ds.rssi().unwrap_or(0);
                self.net_icon.set_icon_name(Some(wifi_icon_for_rssi(rssi)));
                self.net_icon.set_visible(true);
            }
            _ => { self.net_icon.set_visible(false); }
        }
    }

    pub(super) fn apply_device_info(&self) {
        let info = match self.ds.device_info() { Some(i) => i, None => return };
        let caps = match self.ds.capabilities() { Some(c) => c, None => return };

        self.window.set_title(Some(&format!("RustyWiiM ({})", info.device_name)));

        self.dev_info_label.set_label(&format!(
            "{} · {} · FW {}",
            caps.vendor.display_name(), caps.model, info.firmware,
        ));

        // Unlike dev_info_label (always visible, only its text ever
        // changes), ip_label starts invisible and is shown/hidden here on
        // every device-changed. queue_resize() forces a full fresh layout
        // pass on the reveal rather than risking a stale allocation/clip
        // from before the label was visible — belt-and-suspenders against
        // the top-row clipping seen on this label but not on dev_info_label.
        let ip = info.ip_addr();
        if !ip.is_empty() {
            self.ip_label.set_label(ip);
            self.ip_label.set_visible(true);
            self.ip_label.queue_resize();
        } else {
            self.ip_label.set_visible(false);
        }

        self.populate_source();
        self.populate_output();
        self.apply_device_window_state(&info.uuid);
    }

    // ── Volume helpers ────────────────────────────────────────────────────────

    /// Sync one volume slider + its vol button + mute button from device state.
    /// Skips the `set_value` call while the user is dragging either slider.
    pub(super) fn sync_vol_display(
        &self,
        scale:        &gtk::Scale,
        vol_icon_img: &gtk::Image,
        vol_label:    &gtk::Label,
        mute_btn:     &gtk::Button,
        muted:        bool,
    ) {
        // Fetch the authoritative volume first; used for both the slider position
        // and the icon so they stay consistent even when set_value is inhibited.
        let device_vol = self.ds.get_vol();
        if self.ui_state.drag_timer.borrow().is_none() {
            if let Some(v) = device_vol {
                scale.set_value(v as f64);
            }
        }
        // Use device_vol for the icon rather than scale.value() so that a scale
        // that hasn't been initialised yet (value = 0) doesn't produce a false
        // muted icon.  Fall back to scale.value() only when there is no data.
        let display_vol = device_vol.map(|v| v as f64).unwrap_or_else(|| scale.value());
        vol_icon_img.set_icon_name(Some(vol_icon(muted, display_vol)));
        vol_label.set_label(&format!("{}", display_vol as u32));
        mute_btn.set_icon_name(if muted { "audio-volume-muted-symbolic" } else { "audio-volume-high-symbolic" });
    }

    /// Called when either vol slider value changes due to user interaction.
    /// Updates both vol button icons and sends the rate-limited volume command.
    /// Resets a 500 ms drag-protection timer so poll updates don't jump the
    /// slider while the user is still interacting with it.
    pub(super) fn on_vol_changed(&self, vol: f64) {
        let icon = vol_icon(self.ds.muted(), vol);
        let vol_str = format!("{}", vol as u32);
        self.pw.vol_icon_img.set_icon_name(Some(icon));
        self.pw.vol_label.set_label(&vol_str);
        self.mini.vol_icon_img.set_icon_name(Some(icon));
        self.mini.vol_label.set_label(&vol_str);
        self.ds.do_set_volume(vol as u32);

        // Cancel any pending reset and schedule a fresh one.
        if let Some(id) = self.ui_state.drag_timer.borrow_mut().take() { id.remove(); }
        let timer_cell = Rc::clone(&self.ui_state.drag_timer);
        let id = glib::timeout_add_local_once(std::time::Duration::from_millis(500), move || {
            timer_cell.borrow_mut().take();
        });
        *self.ui_state.drag_timer.borrow_mut() = Some(id);
    }

    /// Nudge the volume by `delta` (clamped to 0..=100) — used by the Up/Down
    /// keyboard shortcuts. Routes through `on_vol_changed` so it gets the
    /// same UI sync + rate-limited device command + drag-protection timer as
    /// a manual slider drag.
    pub(super) fn step_volume(&self, delta: i32) {
        let current = self.ds.get_vol().unwrap_or(0) as i32;
        let new_vol = (current + delta).clamp(0, 100);
        self.on_vol_changed(new_vol as f64);
    }

    // ── Playback ──────────────────────────────────────────────────────────────

    pub(super) fn update_playback_ui(&self, mask: u32) {
        use crate::device::state::playback_changed as PC;
        use crate::device::playback::PlaybackStatus;

        let ps = self.ds.playback_state();

        if mask & (PC::VOLUME | PC::TIME | PC::OTHER) != 0 {
            if mask & PC::VOLUME != 0 {
                self.sync_vol_display(&self.vol_scale.clone(), &self.pw.vol_icon_img, &self.pw.vol_label, &self.pw.mute_btn, ps.muted);
            }
            if mask & PC::TIME != 0 {
                let cur_s = ps.position.as_secs();
                let tot_s = ps.duration.as_secs();
                if tot_s > 0 {
                    self.pw.seek.set_range(0.0, tot_s as f64);
                    self.pw.seek.set_value(cur_s as f64);
                }
                self.pw.pos.set_label(&format!("{}:{:02}", cur_s / 60, cur_s % 60));
                self.pw.dur.set_label(&format!("{}:{:02}", tot_s / 60, tot_s % 60));
            }
            if mask & PC::OTHER != 0 {
                let playing = matches!(ps.status, PlaybackStatus::Playing);
                *self.ui_state.is_playing.borrow_mut() = playing;
                self.pw.btn_play.set_icon_name(if playing {
                    "media-playback-pause-symbolic"
                } else {
                    "media-playback-start-symbolic"
                });
                self.pw.status.set_label(&format_status_line(&ps.status, ps.source_name.as_deref()));
                apply_shuffle_ui(&self.pw.shuffle, ps.shuffle);
                apply_repeat_ui(&self.pw.repeat, ps.repeat);
            }
        }

        if mask & (PC::TITLE | PC::ARTIST | PC::ALBUM | PC::OTHER) != 0 {
            if mask & PC::TITLE != 0 {
                self.pw.title.set_text(if is_unknown(&ps.title) { "" } else { &ps.title });
            }
            if mask & PC::ARTIST != 0 {
                self.pw.artist.set_text(if is_unknown(&ps.artist) { "" } else { &ps.artist });
            }
            if mask & PC::ALBUM != 0 {
                self.pw.album.set_text(if is_unknown(&ps.album) { "" } else { &ps.album });
            }
            if mask & PC::OTHER != 0 {
                // Never hidden — see the comment on PlaybackWidgets::quality's
                // construction. An empty label keeps the same reserved height.
                let q = ps.quality.map(|q| format_quality_line(&q)).unwrap_or_default();
                self.pw.quality.set_label(&q);
            }
        }

        if mask & PC::ARTWORK != 0 {
            self.update_artwork();
        }
    }

    // ── Artwork ───────────────────────────────────────────────────────────────

    /// Decode and display artwork, or fall back to the source icon.
    /// Operates on the full-player or mini widgets depending on `mini_mode`.
    /// Both are FlipCover, which renders art and the fallback icon itself
    /// (flipping or crossfading between them as appropriate) — no separate
    /// stack/icon widget needed.
    fn update_artwork(&self) {
        let mini = *self.mini_mode.borrow();
        let flip   = if mini { &self.mini.artwork } else { &self.pw.artwork };
        // Fed unconditionally regardless of whether Modern (and, for the
        // mini one, mini_modern) is actually active — cheap (a texture
        // clone + queue_draw(), the latter a no-op while invisible), and
        // keeps both in sync so switching the setting on shows current art
        // immediately instead of waiting for the next poll.
        let art_bg = if mini { &self.mini.art_bg } else { &self.art_bg };
        let icon_size = if mini { 36.0 } else { 128.0 };

        let ps = self.ds.playback_state();
        let tex = ps.artwork.as_ref().and_then(|bytes| {
            let gbytes = glib::Bytes::from(bytes.as_ref());
            gtk::gdk::Texture::from_bytes(&gbytes).ok()
        });

        if let Some(tex) = &tex {
            let art_key = ps.art_url.as_deref().unwrap_or("");
            art_bg.set_art(Some(tex), art_key);
            flip.set_art(Some(tex), art_key);
        } else {
            let mode = self.ds.current_mode();
            let source_id = capabilities::mode_to_input_source(mode);
            // Fixed key (not per-source) so switching between different
            // no-art sources doesn't re-trigger the background fade for a
            // gradient that looks the same either way.
            art_bg.set_art(None, "__no_art__");
            flip.set_icon(
                self.icons.source_paintable(source_id), icon_size, &format!("icon:{source_id}"));
        }
    }

    // ── Input / Output display ────────────────────────────────────────────────

    pub(super) fn update_input_display(&self) {
        let mode = self.ds.current_mode();
        let source_id = capabilities::mode_to_input_source(mode);
        let sv = self.sw.ids.borrow();
        if let Some(idx) = sv.iter().position(|s| s == source_id) {
            *self.sw.updating.borrow_mut() = true;
            self.sw.dropdown.set_selected(idx as u32);
            *self.sw.updating.borrow_mut() = false;
        }
        drop(sv);
        self.update_artwork();
    }

    pub(super) fn update_output_display(&self) {
        let Some(os) = self.ds.output_status() else { return };
        let Ok(hw) = os.hardware.parse::<u32>() else { return };
        let hw_canon = capabilities::canon_mode_output_name(hw);
        let names = self.ow.canon_names.borrow();
        if let Some(idx) = names.iter().position(|&n| n == hw_canon) {
            *self.ow.updating.borrow_mut() = true;
            self.ow.dropdown.set_selected(idx as u32);
            *self.ow.updating.borrow_mut() = false;
        }
    }

    // ── Presets ───────────────────────────────────────────────────────────────

    pub(super) fn on_presets_changed(&self) {
        use crate::device::api::PresetKind;
        let presets = self.ds.presets();

        // Clear all slots first.
        for btn in self.pp.btns.iter() { btn.set_visible(false); }
        for lbl in self.pp.labels.iter() { lbl.set_label(""); }
        for pic in self.pp.pics.iter() {
            pic.set_paintable(None::<&gtk::gdk::Paintable>);
            pic.set_icon_name(Some("audio-x-generic-symbolic"));
            pic.set_pixel_size(40);
            pic.remove_css_class("preset-art-small");
        }

        for entry in &presets {
            let idx = entry.slot.saturating_sub(1);
            if let Some(btn) = self.pp.btns.get(idx) {
                btn.set_visible(true);
                btn.set_tooltip_text(Some(&entry.tooltip()));
            }
            if let Some(lbl) = self.pp.labels.get(idx) {
                lbl.set_label(entry.label());
            }
            if let Some(pic) = self.pp.pics.get(idx) {
                match &entry.kind {
                    PresetKind::Media => {
                        if !entry.art_bytes.is_empty() {
                            let gbytes = glib::Bytes::from(&entry.art_bytes);
                            if let Ok(tex) = gtk::gdk::Texture::from_bytes(&gbytes) {
                                pic.set_paintable(Some(&tex));
                            }
                        }
                    }
                    PresetKind::InputSwitch { input_id } => {
                        pic.set_pixel_size(26);
                        pic.add_css_class("preset-art-small");
                        pic.set_paintable(Some(self.icons.source_paintable(input_id)));
                    }
                    PresetKind::OutputSwitch { output_id } => {
                        pic.set_pixel_size(26);
                        pic.add_css_class("preset-art-small");
                        let canon = capabilities::canon_new_output_name(output_id);
                        pic.set_paintable(Some(self.icons.output_paintable(canon)));
                    }
                    PresetKind::OtherRoutine => {
                        pic.set_pixel_size(26);
                        pic.add_css_class("preset-art-small");
                    }
                    PresetKind::Empty => {}
                }
            }
        }
    }

    /// Apply per-device window/panel state for the device identified by
    /// `uuid`.  Guarded by `applied_window_key` so repeated device-changed
    /// fires for the same device don't override the user's manual resizes.
    pub(super) fn apply_device_window_state(&self, uuid: &str) {
        if uuid.is_empty() { return; }
        let prev_uuid = self.applied_window_key.borrow().clone();
        if prev_uuid == uuid { return; }

        // Save the previous device's window state before overwriting the layout.
        // We use prev_uuid directly rather than ds.device_info() because by the
        // time this is called from apply_device_info, device_info() already points
        // to the new device.
        if !prev_uuid.is_empty() {
            let maximized = self.window.is_maximized();
            config::update(|cfg| {
                let dev = cfg.device_mut(&prev_uuid);
                dev.window_maximized = maximized;
                dev.window_width     = if maximized { 0 } else { self.window.width() };
                dev.window_height    = if maximized { 0 } else { self.window.height() };
                dev.panel_visible    = self.sidebar_btn.is_active();
                dev.paned_position   = *self.saved_panel_width.borrow();
                dev.mini_mode        = *self.mini_mode.borrow();
                // Only overwrite if the mini window has actually been shown
                // this session (width() reports 0 for a never-realized
                // window) — otherwise this would clobber a previously saved
                // good value with 0 every time a session never happens to
                // enter mini mode.
                let mw = self.mini_win.width();
                if mw > 0 { dev.mini_window_width = mw; }
            });
        }

        *self.applied_window_key.borrow_mut() = uuid.to_string();

        let dev_cfg = config::with(|cfg| cfg.device(uuid));

        let panel_width = if dev_cfg.paned_position > 0 { dev_cfg.paned_position } else { 200 };
        *self.saved_panel_width.borrow_mut() = panel_width;

        // Guard with panel_collapsing to avoid triggering the sidebar toggle handler.
        *self.panel_collapsing.borrow_mut() = true;
        if dev_cfg.panel_visible {
            self.left_pane.set_visible(true);
            self.paned.set_position(panel_width);
            self.sidebar_btn.set_active(true);
        } else {
            self.left_pane.set_visible(false);
            self.sidebar_btn.set_active(false);
        }
        *self.panel_collapsing.borrow_mut() = false;

        if dev_cfg.window_maximized {
            self.window.maximize();
        } else {
            // set_default_size must come before unmaximize so the compositor
            // uses the stored size when restoring from maximized state.
            if dev_cfg.window_width > 0 && dev_cfg.window_height > 0 {
                self.window.set_default_size(dev_cfg.window_width, dev_cfg.window_height);
            }
            self.window.unmaximize();
        }

        if dev_cfg.mini_window_width > 0 {
            self.mini_win.set_default_width(dev_cfg.mini_window_width);
        }

        // Load this device's Advanced-panel access-method override, if any
        // (see /PLAYBACKSTATE.md's "Debugging overrides" section). Settings'
        // Advanced page re-pushes this immediately on every change; this is
        // just the "loaded with device state" half.
        self.ds.set_playback_access_override(dev_cfg.playback_access_override.as_ref());
    }

    /// Immediately persist the current device's window/panel state.
    /// Loads the full config, updates only the current device's entry, and
    /// saves so no other device's entry is overwritten.
    pub(super) fn save_config_now(&self) {
        let uuid = match self.ds.device_info() {
            Some(di) if !di.uuid.is_empty() => di.uuid,
            _ => return,
        };
        // In mini mode, use the saved pre-mini size rather than the mini window size.
        let in_mini = *self.mini_mode.borrow();
        let maximized = !in_mini && self.window.is_maximized();
        let (w, h) = if in_mini {
            *self.pre_mini_size.borrow()
        } else {
            (self.window.width(), self.window.height())
        };
        config::update(|cfg| {
            cfg.last_uuid = uuid.clone();
            // Update only the window-related fields; preserve pinned / window_open / etc.
            let dev = cfg.device_mut(&uuid);
            dev.window_maximized = maximized;
            dev.window_width     = if maximized { 0 } else { w };
            dev.window_height    = if maximized { 0 } else { h };
            dev.panel_visible    = self.sidebar_btn.is_active();
            dev.paned_position   = *self.saved_panel_width.borrow();
            dev.mini_mode        = *self.mini_mode.borrow();
            // See the matching guard in apply_device_window_state(): only
            // overwrite once the mini window has actually been realized.
            let mw = self.mini_win.width();
            if mw > 0 { dev.mini_window_width = mw; }
        });
    }

    // ── Mini player ───────────────────────────────────────────────────────────

    pub(super) fn update_mini_playback(&self, mask: u32) {
        use crate::device::state::playback_changed as PC;
        use crate::device::playback::PlaybackStatus;

        if mask & PC::OTHER != 0 {
            if let Some(di) = self.ds.device_info() {
                self.mini.device_label.set_label(&di.device_name);
            }
        }

        let ps = self.ds.playback_state();

        if mask & (PC::VOLUME | PC::OTHER) != 0 {
            if mask & PC::VOLUME != 0 {
                self.sync_vol_display(&self.mini.vol_scale.clone(), &self.mini.vol_icon_img, &self.mini.vol_label, &self.mini.mute_btn, ps.muted);
            }
            if mask & PC::OTHER != 0 {
                self.mini.btn_play.set_icon_name(if matches!(ps.status, PlaybackStatus::Playing) {
                    "media-playback-pause-symbolic"
                } else {
                    "media-playback-start-symbolic"
                });
                self.mini.status_label.set_label(
                    &format_status_line(&ps.status, ps.source_name.as_deref()));
            }
        }

        if mask & (PC::TITLE | PC::ARTIST | PC::ALBUM) != 0 {
            // artist_label combines artist + album; recompute if either changed.
            if mask & (PC::ARTIST | PC::ALBUM) != 0 {
                let artist = if is_unknown(&ps.artist) { "" } else { ps.artist.as_ref() };
                let album  = if is_unknown(&ps.album)  { "" } else { ps.album.as_ref() };
                let artist_line = match (artist.is_empty(), album.is_empty()) {
                    (true,  true)  => String::new(),
                    (true,  false) => album.to_owned(),
                    (false, true)  => artist.to_owned(),
                    (false, false) => format!("{artist} \u{00b7} {album}"),
                };
                self.mini.artist_label.set_text(&artist_line);
            }
            if mask & PC::TITLE != 0 {
                self.mini.title_label.set_text(if is_unknown(&ps.title) { "" } else { &ps.title });
            }
        }

        if mask & PC::ARTWORK != 0 {
            self.update_artwork();
        }
    }

    pub(super) fn enter_mini_mode(&self) {
        if *self.mini_mode.borrow() { return; }
        super::dbg_ui(&format!("enter mini mode (uuid={})", self.applied_window_key.borrow()));
        *self.pre_mini_size.borrow_mut() = (self.window.width(), self.window.height());
        *self.mini_mode.borrow_mut() = true;
        self.update_mini_playback(crate::device::state::playback_changed::ALL);
        *self.mini_toggling.borrow_mut() = true;
        self.mini_btn.set_active(true);
        *self.mini_toggling.borrow_mut() = false;
        self.window.set_visible(false);
        self.mini_win.present();
    }

    pub(super) fn exit_mini_mode(&self) {
        if !*self.mini_mode.borrow() { return; }
        super::dbg_ui(&format!("exit mini mode (uuid={})", self.applied_window_key.borrow()));
        *self.mini_mode.borrow_mut() = false;
        *self.mini_toggling.borrow_mut() = true;
        self.mini_btn.set_active(false);
        *self.mini_toggling.borrow_mut() = false;
        self.mini_win.set_visible(false);
        self.window.present();
        self.update_playback_ui(crate::device::state::playback_changed::ALL);
        self.update_input_display();
    }
} // impl DeviceWindowInner

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
fn flash_button(btn: &gtk::Button) {
    btn.add_css_class("key-flash");
    let btn = btn.clone();
    glib::timeout_add_local_once(std::time::Duration::from_millis(200), move || {
        btn.remove_css_class("key-flash");
    });
}

/// Global playback/volume/window-mode keyboard shortcuts, shared by the main
/// and mini windows via the `EventControllerKey`s wired in `mod.rs`.
/// `prev_btn`/`next_btn`/`play_btn` are whichever window's transport buttons
/// received the key, so the flash appears on the window the user is
/// actually looking at.
pub(super) fn handle_transport_key(
    i:        &Rc<DeviceWindowInner>,
    keyval:   gtk::gdk::Key,
    state:    gtk::gdk::ModifierType,
    prev_btn: &gtk::Button,
    next_btn: &gtk::Button,
    play_btn: &gtk::Button,
) -> glib::Propagation {
    // Ignore Ctrl/Alt combinations so this doesn't shadow other accelerators
    // (Ctrl-W, Ctrl-Q, Alt-based window-manager bindings, etc.).
    if state.intersects(gtk::gdk::ModifierType::CONTROL_MASK | gtk::gdk::ModifierType::ALT_MASK) {
        return glib::Propagation::Proceed;
    }
    match keyval {
        gtk::gdk::Key::Left => {
            i.ds.do_prev();
            flash_button(prev_btn);
            glib::Propagation::Stop
        }
        gtk::gdk::Key::Right => {
            i.ds.do_next();
            flash_button(next_btn);
            glib::Propagation::Stop
        }
        gtk::gdk::Key::space => {
            i.ds.do_play_pause();
            flash_button(play_btn);
            glib::Propagation::Stop
        }
        gtk::gdk::Key::Up => {
            i.step_volume(5);
            glib::Propagation::Stop
        }
        gtk::gdk::Key::Down => {
            i.step_volume(-5);
            glib::Propagation::Stop
        }
        gtk::gdk::Key::m | gtk::gdk::Key::M => {
            if *i.mini_mode.borrow() { i.exit_mini_mode(); } else { i.enter_mini_mode(); }
            schedule_config_save(i);
            glib::Propagation::Stop
        }
        _ => glib::Propagation::Proceed,
    }
}

/// Schedule a deferred config save for `inner`, debounced at 500 ms.
/// Cancels any previously scheduled save so only one write happens per burst.
pub(super) fn schedule_config_save(i: &Rc<DeviceWindowInner>) {
    if let Some(id) = i.config_save_timer.borrow_mut().take() { id.remove(); }
    let i2 = Rc::clone(i);
    let id = glib::timeout_add_local_once(
        std::time::Duration::from_millis(500),
        move || {
            *i2.config_save_timer.borrow_mut() = None;
            i2.save_config_now();
        },
    );
    *i.config_save_timer.borrow_mut() = Some(id);
}

/// Slide the paned's divider to `target_pos` (0 = fully closed) instead of
/// jumping instantly, so opening/closing the side panel reads as one motion.
/// Falls back to an instant set when animations are off (config.animations,
/// or GTK's reduce-motion). `panel_collapsing` is held for the animation's
/// duration so `connect_position_notify`'s drag-detection logic ignores the
/// frames this drives — same guard the instant path already relied on.
pub(super) fn animate_panel_to(i: &Rc<DeviceWindowInner>, target_pos: i32) {
    // Two statements, not `if let Some(a) = i.panel_anim.borrow_mut().take() { a.skip(); }`:
    // the RefMut temporary from borrow_mut() stays alive for the whole if-let
    // block (Rust's temporary lifetime rule for if-let scrutinees), so
    // panel_anim would still be borrowed while skip() runs below — and
    // skip() synchronously fires connect_done, which borrows panel_anim
    // again and panics. (Same bug as FlipCover's set_content/dispose/clear.)
    let old_anim = i.panel_anim.borrow_mut().take();
    if let Some(a) = old_anim { a.skip(); }

    if target_pos > 0 {
        // Visible immediately so it's revealed as the panel slides open,
        // rather than popping in once the animation finishes.
        i.left_pane.set_visible(true);
    }

    let from = i.paned.position();
    let animate = from != target_pos
        && config::with(|cfg| cfg.animations)
        && gtk::Settings::default().is_some_and(|s| s.is_gtk_enable_animations());

    if !animate {
        *i.panel_collapsing.borrow_mut() = true;
        i.paned.set_position(target_pos);
        *i.panel_collapsing.borrow_mut() = false;
        if target_pos <= 0 { i.left_pane.set_visible(false); }
        schedule_config_save(i);
        return;
    }

    *i.panel_collapsing.borrow_mut() = true;

    let weak  = Rc::downgrade(i);
    let paned = i.paned.clone();
    let anim_target = adw::CallbackAnimationTarget::new(move |v| {
        paned.set_position(v.round() as i32);
    });
    let anim = adw::TimedAnimation::new(&i.paned, from as f64, target_pos as f64, 200, anim_target);
    anim.set_easing(adw::Easing::EaseInOutCubic);
    anim.connect_done(move |_| {
        let Some(i) = weak.upgrade() else { return };
        *i.panel_collapsing.borrow_mut() = false;
        if target_pos <= 0 { i.left_pane.set_visible(false); }
        *i.panel_anim.borrow_mut() = None;
        schedule_config_save(&i);
    });
    anim.play();
    *i.panel_anim.borrow_mut() = Some(anim);
}

pub(super) fn wifi_icon_for_rssi(rssi: i32) -> &'static str {
    match rssi {
        i32::MIN..=-85 | 0 => "network-wireless-offline-symbolic",
        -84..=-75           => "network-wireless-signal-weak-symbolic",
        -74..=-65           => "network-wireless-signal-ok-symbolic",
        -64..=-55           => "network-wireless-signal-good-symbolic",
        _                   => "network-wireless-signal-excellent-symbolic",
    }
}
