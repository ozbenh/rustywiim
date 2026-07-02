#![allow(deprecated)] // glib clone! old-style @strong syntax

use std::rc::Rc;

use adw::prelude::*;

use crate::{device::{api, capabilities}, config};


use super::*;

// ── String helpers ────────────────────────────────────────────────────────────

fn is_unknown(s: &str) -> bool {
    s.is_empty() || s.eq_ignore_ascii_case("unknown") || s.eq_ignore_ascii_case("unknow")
}

fn vendor_display(vendor: &str) -> &'static str {
    let v: String = vendor.to_lowercase().chars().filter(|c| !c.is_whitespace()).collect();
    match v.as_str() {
        "newtunein" | "tunein"              => "TuneIn",
        "iheartradio" | "iheart"            => "iHeartRadio",
        "spotify"                            => "Spotify",
        "tidal"                              => "TIDAL",
        "amazon" | "amazonmusic"             => "Amazon Music",
        "deezer"                             => "Deezer",
        "qobuz"                              => "Qobuz",
        "pandora"                            => "Pandora",
        "napster"                            => "Napster",
        "radioparadise"                      => "Radio Paradise",
        "vtuner"                             => "vTuner",
        "linkplayradio"                      => "Radio",
        "custompushurl"                      => "URL",
        "cast"                               => "Chromecast",
        _                                    => "",
    }
}

fn mode_source(mode: &str) -> &'static str {
    match mode {
        "0"             => "Idle",
        "1"             => "AirPlay",
        "2"             => "DLNA",
        "5"             => "Chromecast",
        "10" | "20"     => "WiFi",
        "11" | "42" | "51" => "USB",
        "31"            => "Spotify",
        "32"            => "TIDAL Connect",
        "34"            => "Lyrion",
        "36"            => "Qobuz",
        "40" | "60"     => "Line-In",
        "41"            => "Bluetooth",
        "43"            => "Optical",
        "44"            => "RCA",
        "49"            => "HDMI",
        "54"            => "Phono",
        "99"            => "Follower",
        _               => "",
    }
}

fn format_status(status: &str, mode: &str, vendor: &str) -> String {
    let state = match status {
        "play"    => "▶ Playing",
        "pause"   => "⏸ Paused",
        "stop"    => "⏹ Stopped",
        "loading" => "⏳ Loading",
        other     => other,
    };
    let source_name = match mode {
        "10" | "20" | "0" | "5" => {
            let vn = vendor_display(vendor);
            if !vn.is_empty() { vn } else { mode_source(mode) }
        }
        _ => mode_source(mode),
    };
    let suffix = match source_name {
        "" | "Idle" => String::new(),
        s           => format!(" · {s}"),
    };
    format!("{state}{suffix}")
}

fn format_quality(bit_rate: &str, sample_rate: &str, bit_depth: &str) -> Option<String> {
    let br = bit_rate.trim();
    let sr = sample_rate.trim();
    let bd = bit_depth.trim();
    let has_br = !br.is_empty() && br != "0";
    let has_sr = !sr.is_empty() && sr != "0";
    if !has_br && !has_sr { return None; }
    let mut parts = Vec::new();
    if has_br {
        let kbps = br.parse::<f64>().unwrap_or(0.0);
        parts.push(format!("{kbps:.0} kbps"));
    }
    if has_sr {
        let khz = sr.parse::<f64>().unwrap_or(0.0) / 1000.0;
        parts.push(format!("{khz:.1} kHz"));
    }
    if !bd.is_empty() && bd != "0" {
        parts.push(format!("{bd}-bit"));
    }
    Some(parts.join(" / "))
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

fn apply_repeat_ui(btn: &gtk::Button, state: u32) {
    let icons = ["media-playlist-repeat-symbolic",
                 "media-playlist-repeat-symbolic",
                 "media-playlist-repeat-song-symbolic"];
    let tips  = ["Repeat: Off", "Repeat: All", "Repeat: One"];
    btn.set_icon_name(icons[state as usize]);
    btn.set_tooltip_text(Some(tips[state as usize]));
    if state == 0 { btn.remove_css_class("loop-active"); }
    else           { btn.add_css_class("loop-active"); }
}

pub(super) fn decode_loop_mode(mode: &str) -> (bool, u32) {
    match mode {
        "4" => (false, 0),
        "0" => (false, 1),
        "1" => (false, 2),
        "3" => (true,  0),
        "2" => (true,  1),
        "5" => (true,  2),
        _   => (false, 0),
    }
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
        self.pw.artwork.set_paintable(None::<&gtk::gdk::Paintable>);
        self.pw.art_stack.set_visible_child_name("artwork");
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

        let ip = info.ip_addr();
        if !ip.is_empty() {
            self.ip_label.set_label(ip);
            self.ip_label.set_visible(true);
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

    // ── Playback ──────────────────────────────────────────────────────────────

    pub(super) fn update_playback_ui(&self, mask: u32) {
        use crate::device::state::playback_changed as PC;

        if mask & (PC::VOLUME | PC::TIME | PC::OTHER) != 0 {
            if let Some(st) = self.ds.player_status() {
                if mask & PC::VOLUME != 0 {
                    let muted = st.mute;
                    self.sync_vol_display(&self.vol_scale.clone(), &self.pw.vol_icon_img, &self.pw.vol_label, &self.pw.mute_btn, muted);
                }
                if mask & PC::TIME != 0 {
                    let cur_s = st.curpos / 1000;
                    let tot_s = st.totlen / 1000;
                    if tot_s > 0 {
                        self.pw.seek.set_range(0.0, tot_s as f64);
                        self.pw.seek.set_value(cur_s as f64);
                    }
                    self.pw.pos.set_label(&format!("{}:{:02}", cur_s / 60, cur_s % 60));
                    self.pw.dur.set_label(&format!("{}:{:02}", tot_s / 60, tot_s % 60));
                }
                if mask & PC::OTHER != 0 {
                    let playing = st.status == "play";
                    *self.ui_state.is_playing.borrow_mut() = playing;
                    self.pw.btn_play.set_icon_name(if playing {
                        "media-playback-pause-symbolic"
                    } else {
                        "media-playback-start-symbolic"
                    });
                    self.pw.status.set_label(&format_status(&st.status, &st.mode, &st.vendor));
                    let (dev_shuf, dev_rep) = decode_loop_mode(&st.loop_mode);
                    apply_shuffle_ui(&self.pw.shuffle, dev_shuf);
                    apply_repeat_ui(&self.pw.repeat, dev_rep);
                }
            }
        }

        if mask & (PC::TITLE | PC::ARTIST | PC::ALBUM | PC::OTHER) != 0 {
            if let Some(m) = self.ds.metadata() {
                if mask & PC::TITLE != 0 {
                    self.pw.title.set_text(if is_unknown(&m.title) { "" } else { &m.title });
                }
                if mask & PC::ARTIST != 0 {
                    self.pw.artist.set_text(if is_unknown(&m.artist) { "" } else { &m.artist });
                }
                if mask & PC::ALBUM != 0 {
                    self.pw.album.set_text(if is_unknown(&m.album) { "" } else { &m.album });
                }
                if mask & PC::OTHER != 0 {
                    // Never hidden — see the comment on PlaybackWidgets::quality's
                    // construction. An empty label keeps the same reserved height.
                    let q = format_quality(&m.bit_rate, &m.sample_rate, &m.bit_depth);
                    self.pw.quality.set_label(q.as_deref().unwrap_or(""));
                }
            }
        }

        if mask & PC::ARTWORK != 0 {
            self.update_artwork();
        }
    }

    // ── Artwork ───────────────────────────────────────────────────────────────

    /// Decode and display artwork, or fall back to the source icon.
    /// Operates on the full-player or mini widgets depending on `mini_mode`.
    fn update_artwork(&self) {
        let mini = *self.mini_mode.borrow();
        let art_stack  = if mini { &self.mini.art_stack  } else { &self.pw.art_stack  };
        let input_icon = if mini { &self.mini.input_icon } else { &self.pw.input_icon };

        if let Some(bytes) = self.ds.art_bytes() {
            let gbytes = glib::Bytes::from(bytes.as_ref());
            if let Ok(tex) = gtk::gdk::Texture::from_bytes(&gbytes) {
                if mini { self.mini.artwork.set_paintable(Some(&tex)); }
                else    { self.pw.artwork.set_paintable(Some(&tex)); }
                art_stack.set_visible_child_name("artwork");
                return;
            }
        }
        let mode = self.ds.current_mode();
        let source_id = capabilities::mode_to_input_source(&mode);
        input_icon.set_paintable(Some(self.icons.source_paintable(source_id)));
        if mini { self.mini.artwork.set_paintable(None::<&gtk::gdk::Paintable>); }
        else    { self.pw.artwork.set_paintable(None::<&gtk::gdk::Paintable>); }
        art_stack.set_visible_child_name("icon");
    }

    // ── Input / Output display ────────────────────────────────────────────────

    pub(super) fn update_input_display(&self) {
        let mode = self.ds.current_mode();
        let source_id = capabilities::mode_to_input_source(&mode);
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
        });
    }

    // ── Mini player ───────────────────────────────────────────────────────────

    pub(super) fn update_mini_playback(&self, mask: u32) {
        use crate::device::state::playback_changed as PC;

        if mask & PC::OTHER != 0 {
            if let Some(di) = self.ds.device_info() {
                self.mini.device_label.set_label(&di.device_name);
            }
        }

        if mask & (PC::VOLUME | PC::OTHER) != 0 {
            if let Some(st) = self.ds.player_status() {
                if mask & PC::VOLUME != 0 {
                    let muted = st.mute;
                    self.sync_vol_display(&self.mini.vol_scale.clone(), &self.mini.vol_icon_img, &self.mini.vol_label, &self.mini.mute_btn, muted);
                }
                if mask & PC::OTHER != 0 {
                    self.mini.btn_play.set_icon_name(if st.status == "play" {
                        "media-playback-pause-symbolic"
                    } else {
                        "media-playback-start-symbolic"
                    });
                    self.mini.status_label.set_label(
                        &format_status(&st.status, &st.mode, &st.vendor));
                }
            }
        }

        if mask & (PC::TITLE | PC::ARTIST | PC::ALBUM) != 0 {
            if let Some(m) = self.ds.metadata() {
                // artist_label combines artist + album; recompute if either changed.
                if mask & (PC::ARTIST | PC::ALBUM) != 0 {
                    let artist = if is_unknown(&m.artist) { "" } else { m.artist.as_str() };
                    let album  = if is_unknown(&m.album)  { "" } else { m.album.as_str() };
                    let artist_line = match (artist.is_empty(), album.is_empty()) {
                        (true,  true)  => String::new(),
                        (true,  false) => album.to_owned(),
                        (false, true)  => artist.to_owned(),
                        (false, false) => format!("{artist} \u{00b7} {album}"),
                    };
                    self.mini.artist_label.set_text(&artist_line);
                }
                if mask & PC::TITLE != 0 {
                    self.mini.title_label.set_text(if is_unknown(&m.title) { "" } else { &m.title });
                }
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

pub(super) fn wifi_icon_for_rssi(rssi: i32) -> &'static str {
    match rssi {
        i32::MIN..=-85 | 0 => "network-wireless-offline-symbolic",
        -84..=-75           => "network-wireless-signal-weak-symbolic",
        -74..=-65           => "network-wireless-signal-ok-symbolic",
        -64..=-55           => "network-wireless-signal-good-symbolic",
        _                   => "network-wireless-signal-excellent-symbolic",
    }
}
