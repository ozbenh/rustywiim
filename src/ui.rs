#![allow(deprecated)] // glib clone! old-style @strong syntax

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;

use adw::prelude::*;
use glib::clone;
use gtk::gio;
use gtk::{Align, Box as GtkBox, Button, CssProvider, Label, Orientation, Scale, StringList};

use crate::api::{OutputEntry, TlsMode};
use crate::capabilities;
use crate::config::{Config, DeviceConfig, ThemeMode};
use crate::device_state::{ConnectionState, DeviceState};
use crate::discovery;
use crate::icons;

// Structural-only CSS used in System theme mode.
// No color overrides — Adwaita handles all colours.
const SYSTEM_CSS: &str = r#"
.track-title  { font-size: 20px; font-weight: 800; }
.track-artist { font-size: 14px; font-weight: 600; }
.track-album  { font-size: 12px; }
.status-badge { font-size: 11px; font-weight: 600; }
.quality-label { font-size: 11px; }
.dim-label    { font-size: 11px; }
.section-label { font-size: 11px; font-weight: 700; letter-spacing: 1px; }
.panel-dropdown { font-size: 11px; min-width: 0; }
.device-info  { font-size: 10px; }
.transport-btn {
    min-width: 36px; min-height: 36px;
    padding: 0; -gtk-icon-size: 16px;
    border: none; border-radius: 50%; box-shadow: none;
}
.play-btn {
    min-width: 44px; min-height: 44px;
    padding: 0; -gtk-icon-size: 20px;
    border: none; border-radius: 50%; box-shadow: none;
}
.loop-btn { min-width: 36px; min-height: 36px; padding: 0; -gtk-icon-size: 16px; }
.vol-pop trough   { min-width: 3px; border-radius: 2px; }
.vol-pop slider   { min-width: 8px; min-height: 8px; margin: -3px; border-radius: 50%; border: none; box-shadow: none; }
.mini-vol-btn { background-color: transparent; }
.mini-vol-pop trough { min-width: 3px; border-radius: 2px; }
.mini-vol-pop slider { min-width: 6px; min-height: 6px; margin: -2px; border-radius: 50%; border: none; box-shadow: none; }
.mini-vol-popover > contents { min-width: 0; padding: 0; }
.seek-scale trough { min-height: 4px; border-radius: 2px; }
.seek-scale slider { min-width: 0; min-height: 0; margin: 0; padding: 0; opacity: 0; }
.preset-tile  { border-radius: 8px; padding: 4px; }
.preset-art   { border-radius: 5px; min-width: 40px; min-height: 40px; }
.preset-art-small { min-width: 26px; min-height: 26px; padding: 7px; }
.preset-name  { font-size: 11px; }
.preset-badge { font-size: 9px; font-weight: 700; border-radius: 50%; min-width: 16px; min-height: 16px; padding: 1px; }
.loop-active  { color: @accent_color; }
.mini-window  { border-radius: 10px; }
.mini-art {
    min-width: 48px; min-height: 48px;
    max-width: 48px; max-height: 48px;
    overflow: hidden; border-radius: 6px;
}
.mini-title        { font-size: 13px; font-weight: 700; }
.mini-artist       { font-size: 11px; }
.mini-status-label { font-size: 10px; }
.mini-device-label { font-size: 11px; font-weight: 700; }
.mini-restore-btn {
    min-width: 18px; min-height: 18px;
    padding: 1px; -gtk-icon-size: 10px;
    border: none; border-radius: 50%; box-shadow: none; background-color: transparent;
}
.mini-transport-btn {
    min-width: 22px; min-height: 22px;
    padding: 0; -gtk-icon-size: 11px;
    border: none; border-radius: 50%; box-shadow: none; background-color: transparent;
}
.mini-transport-btn:hover { background-color: rgba(127, 127, 127, 0.15); }
.mini-play-btn {
    min-width: 26px; min-height: 26px;
    padding: 0; -gtk-icon-size: 12px;
    border: none; border-radius: 50%; box-shadow: none;
}
"#;

// Full custom dark theme.  Loaded over SYSTEM_CSS in Custom mode.
const DARK_CSS: &str = r#"
window { background-color: #0a0a0a; }
.track-title  { font-size: 20px; font-weight: 800; color: #ffffff; }
.track-artist { font-size: 14px; font-weight: 600; color: #b0b0b0; }
.track-album  { font-size: 12px; color: #707070; }
.status-badge { font-size: 11px; font-weight: 600; color: #4ecdc4; }
.quality-label { font-size: 11px; color: #505050; }
.dim-label    { font-size: 11px; color: #606060; }
.transport-btn {
    min-width: 36px; min-height: 36px;
    padding: 0; -gtk-icon-size: 16px;
    background-color: transparent; color: #ffffff;
    border: none; border-radius: 50%; box-shadow: none;
}
.transport-btn:hover { background-color: #2a2a2a; }
.play-btn {
    min-width: 44px; min-height: 44px;
    padding: 0; -gtk-icon-size: 20px;
    background-color: #4ecdc4; color: #0a0a0a;
    border: none; border-radius: 50%; box-shadow: none;
}
.play-btn:hover { background-color: #5fd9d0; }
.vol-btn { background-color: transparent; color: #ffffff; }
.vol-btn:hover { background-color: #2a2a2a; }
.vol-pop trough   { min-width: 3px; border-radius: 2px; background-color: #2a2a2a; }
.vol-pop trough highlight { background-color: #4ecdc4; }
.vol-pop slider   { min-width: 8px; min-height: 8px; margin: -3px; background-color: #808080; border-radius: 50%; border: none; box-shadow: none; }
.mini-vol-btn { background-color: transparent; color: #cccccc; }
.mini-vol-btn:hover { background-color: #2a2a2a; }
.mini-vol-pop trough { min-width: 3px; border-radius: 2px; background-color: #2a2a2a; }
.mini-vol-pop trough highlight { background-color: #4ecdc4; }
.mini-vol-pop slider { min-width: 6px; min-height: 6px; margin: -2px; background-color: #808080; border-radius: 50%; border: none; box-shadow: none; }
.mini-vol-popover > contents { min-width: 0; padding: 0; }
.seek-scale trough { min-height: 4px; border-radius: 2px; background-color: #2a2a2a; }
.seek-scale trough highlight { background-color: #4ecdc4; }
.seek-scale slider { min-width: 0; min-height: 0; margin: 0; padding: 0; opacity: 0; }
.section-label { font-size: 11px; font-weight: 700; color: #505050; letter-spacing: 1px; }
.preset-tile  { border-radius: 8px; background-color: #141414; padding: 4px; }
.preset-tile:hover { background-color: #202020; }
.preset-art   { border-radius: 5px; min-width: 40px; min-height: 40px; }
.preset-art-small { min-width: 26px; min-height: 26px; padding: 7px; }
.preset-name  { font-size: 11px; color: #a0a0a0; }
.preset-badge { font-size: 9px; font-weight: 700; color: #1a1a1a; background-color: #606060; border-radius: 50%; min-width: 16px; min-height: 16px; padding: 1px; }
.panel-dropdown { font-size: 11px; min-width: 0; }
.device-info  { font-size: 10px; color: #686868; }
.net-icon     { color: #686868; }
.sidebar-toggle:checked { color: #4ecdc4; }
.loop-btn     { min-width: 36px; min-height: 36px; padding: 0; -gtk-icon-size: 16px; background-color: transparent; color: #606060; }
.loop-btn:hover { background-color: #2a2a2a; color: #ffffff; }
.loop-active  { color: #4ecdc4; }
.mini-window  { background-color: #111111; border-radius: 10px; }
.mini-art {
    min-width: 48px; min-height: 48px;
    max-width: 48px; max-height: 48px;
    overflow: hidden; border-radius: 6px;
}
.mini-title        { font-size: 13px; font-weight: 700; color: #ffffff; }
.mini-artist       { font-size: 11px; color: #909090; }
.mini-status-label { font-size: 10px; color: #606060; }
.mini-device-label { font-size: 11px; font-weight: 700; color: #606060; }
.mini-restore-btn {
    min-width: 18px; min-height: 18px;
    padding: 1px; -gtk-icon-size: 10px;
    background-color: transparent; color: #555555;
    border: none; border-radius: 50%; box-shadow: none;
}
.mini-restore-btn:hover { color: #ffffff; background-color: #2a2a2a; }
.mini-transport-btn {
    min-width: 22px; min-height: 22px;
    padding: 0; -gtk-icon-size: 11px;
    background-color: transparent; color: #cccccc;
    border: none; border-radius: 50%; box-shadow: none;
}
.mini-transport-btn:hover { background-color: #2a2a2a; }
.mini-play-btn {
    min-width: 26px; min-height: 26px;
    padding: 0; -gtk-icon-size: 12px;
    background-color: #4ecdc4; color: #0a0a0a;
    border: none; border-radius: 50%; box-shadow: none;
}
.mini-play-btn:hover { background-color: #5fd9d0; }
"#;

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

fn apply_shuffle_ui(btn: &Button, on: bool) {
    if on { btn.add_css_class("loop-active"); }
    else   { btn.remove_css_class("loop-active"); }
    btn.set_tooltip_text(Some(if on { "Shuffle: On" } else { "Shuffle: Off" }));
}

fn apply_repeat_ui(btn: &Button, state: u32) {
    let icons = ["media-playlist-repeat-symbolic",
                 "media-playlist-repeat-symbolic",
                 "media-playlist-repeat-song-symbolic"];
    let tips  = ["Repeat: Off", "Repeat: All", "Repeat: One"];
    btn.set_icon_name(icons[state as usize]);
    btn.set_tooltip_text(Some(tips[state as usize]));
    if state == 0 { btn.remove_css_class("loop-active"); }
    else           { btn.add_css_class("loop-active"); }
}

fn loop_api_mode(shuffle: bool, repeat: u32) -> i32 {
    match (shuffle, repeat) {
        (false, 0) => 4,
        (false, 1) => 0,
        (false, 2) => 1,
        (true,  0) => 3,
        (true,  1) => 2,
        (true,  2) => 5,
        _           => 4,
    }
}

fn decode_loop_mode(mode: &str) -> (bool, u32) {
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

// ── Widget bundles ────────────────────────────────────────────────────────────
// Grouping related widgets + associated state into structs keeps signal-handler
// signatures short and the closures easy to read.

#[derive(Clone)]
struct SourceWidgets {
    dropdown: gtk::DropDown,
    ids:      Rc<RefCell<Vec<String>>>,
    enabled:  Rc<RefCell<Vec<bool>>>,
    updating: Rc<RefCell<bool>>,
}

#[derive(Clone)]
struct OutputWidgets {
    dropdown:    gtk::DropDown,
    section:     GtkBox,
    modes:       Rc<RefCell<Vec<u32>>>,
    canon_names: Rc<RefCell<Vec<&'static str>>>,
    updating:    Rc<RefCell<bool>>,
}

#[derive(Clone)]
struct PresetWidgets {
    btns:   Rc<Vec<Button>>,
    pics:   Rc<Vec<gtk::Image>>,
    labels: Rc<Vec<Label>>,
}

#[derive(Clone)]
struct PlaybackWidgets {
    title:      Label,
    artist:     Label,
    album:      Label,
    status:     Label,
    quality:    Label,
    pos:        Label,
    dur:        Label,
    seek:       Scale,
    btn_prev:    Button,
    btn_play:    Button,
    btn_next:    Button,
    shuffle:     Button,
    repeat:      Button,
    vol_btn:     Button,
    vol_popover: gtk::Popover,
    mute_btn:    Button,
    artwork:     gtk::Picture,
    art_stack:   gtk::Stack,
    input_icon:  gtk::Image,
}

// ── Playback UI state ─────────────────────────────────────────────────────────

#[derive(Clone)]
struct PlaybackUiState {
    is_playing:   Rc<RefCell<bool>>,
    // Set while the user is dragging the volume slider (or within 500ms after).
    // Prevents poll updates from jumping the slider back mid-drag.
    drag_timer:   Rc<RefCell<Option<glib::SourceId>>>,
}

struct MiniWidgets {
    root:          gtk::WindowHandle,
    art_stack:     gtk::Stack,
    artwork:       gtk::Picture,
    input_icon:    gtk::Image,
    device_label:  Label,
    restore_btn:   Button,
    title_label:   Label,
    artist_label:  Label,
    status_label:  Label,
    btn_prev:      Button,
    btn_play:      Button,
    btn_next:      Button,
    vol_btn:       Button,
    vol_popover:   gtk::Popover,
    mute_btn:      Button,
    vol_scale:     Scale,
}

// ── DeviceWindowInner ─────────────────────────────────────────────────────────
// All "content" widget state for one device window, kept together so that every
// GTK signal closure only needs one `Rc::clone(&inner)` capture instead of
// capturing half a dozen independent `Rc<RefCell<...>>` values.

struct DeviceWindowInner {
    ds:             DeviceState,
    sw:             SourceWidgets,
    ow:             OutputWidgets,
    pw:             PlaybackWidgets,
    pp:             PresetWidgets,
    dev_info_label: Label,
    net_icon:       gtk::Image,
    icons:          Rc<icons::IconSet>,
    vol_scale:      Scale,
    ui_state:       PlaybackUiState,
    // Window / panel state — kept here so device-change and close handlers
    // only need one Rc<Inner> capture.
    window:              adw::ApplicationWindow,
    paned:               gtk::Paned,
    left_pane:           gtk::Box,
    sidebar_btn:         gtk::ToggleButton,
    saved_panel_width:   Rc<RefCell<i32>>,
    panel_collapsing:    Rc<RefCell<bool>>,
    settle_timer:        Rc<RefCell<Option<glib::SourceId>>>,
    /// Deferred config-save timer: cancelled and rescheduled on every
    /// state change so only one disk write happens after a burst of events.
    config_save_timer:   Rc<RefCell<Option<glib::SourceId>>>,
    /// SSID for which window state was last applied; guards against
    /// re-applying on every device-changed fire for the same device.
    applied_window_key: RefCell<String>,
    // ── Mini player ───────────────────────────────────────────────────────────
    mini:              MiniWidgets,
    mini_mode:         RefCell<bool>,
    mini_toggling:     RefCell<bool>,
    pre_mini_size:     RefCell<(i32, i32)>,
    mini_btn:          gtk::ToggleButton,
    mini_win:          gtk::Window,
}

impl DeviceWindowInner {
    // ── Reset ─────────────────────────────────────────────────────────────────

    fn reset_device_ui(&self, title: &str) {
        self.pw.title.set_label(title);
        self.pw.artist.set_label("");
        self.pw.album.set_label("");
        self.pw.status.set_label("");
        self.pw.quality.set_visible(false);
        self.pw.artwork.set_paintable(None::<&gtk::gdk::Paintable>);
        self.pw.art_stack.set_visible_child_name("artwork");
        self.dev_info_label.set_label("");

        for btn in self.pp.btns.iter() { btn.set_visible(false); }
        for lbl in self.pp.labels.iter() { lbl.set_label(""); }
        for pic in self.pp.pics.iter() {
            pic.set_paintable(None::<&gtk::gdk::Paintable>);
            pic.set_icon_name(Some("audio-x-generic-symbolic"));
        }

        *self.sw.updating.borrow_mut() = true;
        self.sw.dropdown.set_model(Some(&StringList::new(&["—"])));
        self.sw.dropdown.set_sensitive(false);
        *self.sw.updating.borrow_mut() = false;
        *self.sw.ids.borrow_mut()      = Vec::new();
        *self.sw.enabled.borrow_mut()  = Vec::new();

        *self.ow.updating.borrow_mut() = true;
        self.ow.dropdown.set_model(Some(&StringList::new(&["—"])));
        self.ow.dropdown.set_sensitive(false);
        self.ow.section.set_visible(false);
        *self.ow.modes.borrow_mut()       = Vec::new();
        *self.ow.canon_names.borrow_mut() = Vec::new();
        *self.ow.updating.borrow_mut()    = false;
    }

    // ── Source / Output / Network ─────────────────────────────────────────────

    fn populate_source(&self) {
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
            self.sw.dropdown.set_model(Some(&StringList::new(&["—"])));
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
        self.sw.dropdown.set_model(Some(&StringList::new(&label_refs)));
        self.sw.dropdown.set_selected(0);
        self.sw.dropdown.set_sensitive(true);
        *self.sw.updating.borrow_mut() = false;
    }

    fn populate_output(&self) {
        if self.ds.capabilities().is_none() { return; }
        let output_names = self.ds.outputs();
        if output_names.is_empty() {
            *self.ow.updating.borrow_mut() = true;
            self.ow.dropdown.set_model(Some(&StringList::new(&["—"])));
            self.ow.dropdown.set_sensitive(false);
            self.ow.section.set_visible(false);
            *self.ow.modes.borrow_mut()       = Vec::new();
            *self.ow.canon_names.borrow_mut() = Vec::new();
            *self.ow.updating.borrow_mut()    = false;
            return;
        }

        let out_labels: Vec<&str> = output_names.iter()
            .map(|e: &OutputEntry| e.name.as_str())
            .collect();
        let modes: Vec<u32> = output_names.iter()
            .map(|e| capabilities::output_canon_to_mode(e.canon).unwrap_or(0))
            .collect();

        *self.ow.modes.borrow_mut()       = modes;
        *self.ow.canon_names.borrow_mut() = output_names.iter().map(|e| e.canon).collect();
        *self.ow.updating.borrow_mut()    = true;
        self.ow.dropdown.set_model(Some(&StringList::new(&out_labels)));
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

    fn update_network_icon(&self) {
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

    fn apply_device_info(&self) {
        let info = match self.ds.device_info() { Some(i) => i, None => return };
        let caps = match self.ds.capabilities() { Some(c) => c, None => return };

        self.dev_info_label.set_label(&format!(
            "{} · {} · FW {}",
            caps.vendor.display_name(), caps.model, info.firmware,
        ));

        self.populate_source();
        self.populate_output();
        self.apply_device_window_state(&info.ssid);
    }

    // ── Volume helpers ────────────────────────────────────────────────────────

    /// Sync one volume slider + its vol button + mute button from device state.
    /// Skips the `set_value` call while the user is dragging either slider.
    fn sync_vol_display(&self, scale: &Scale, vol_btn: &Button, mute_btn: &Button, muted: bool) {
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
        vol_btn.set_icon_name(vol_icon(muted, display_vol));
        mute_btn.set_icon_name(if muted { "audio-volume-muted-symbolic" } else { "audio-volume-high-symbolic" });
    }

    /// Called when either vol slider value changes due to user interaction.
    /// Updates both vol button icons and sends the rate-limited volume command.
    /// Resets a 500 ms drag-protection timer so poll updates don't jump the
    /// slider while the user is still interacting with it.
    fn on_vol_changed(&self, vol: f64) {
        let icon = vol_icon(self.ds.muted(), vol);
        self.pw.vol_btn.set_icon_name(icon);
        self.mini.vol_btn.set_icon_name(icon);
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

    fn update_playback_ui(&self) {
        if let Some(st) = self.ds.player_status() {
            let muted = st.mute == "1";
            self.sync_vol_display(&self.vol_scale.clone(), &self.pw.vol_btn, &self.pw.mute_btn, muted);

            let cur_s = st.curpos.parse::<u64>().unwrap_or(0) / 1000;
            let tot_s = st.totlen.parse::<u64>().unwrap_or(0) / 1000;
            if tot_s > 0 {
                self.pw.seek.set_range(0.0, tot_s as f64);
                self.pw.seek.set_value(cur_s as f64);
            }

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

            self.pw.pos.set_label(&format!("{}:{:02}", cur_s / 60, cur_s % 60));
            self.pw.dur.set_label(&format!("{}:{:02}", tot_s / 60, tot_s % 60));
        }

        if let Some(m) = self.ds.metadata() {
            let title = if is_unknown(&m.title) { String::new() } else { m.title.clone() };
            self.pw.title.set_label(if title.is_empty() { "—" } else { &title });
            self.pw.artist.set_label(if is_unknown(&m.artist) { "" } else { &m.artist });
            self.pw.album.set_label(if is_unknown(&m.album)  { "" } else { &m.album });

            match format_quality(&m.bit_rate, &m.sample_rate, &m.bit_depth) {
                Some(q) => { self.pw.quality.set_label(&q); self.pw.quality.set_visible(true); }
                None    => self.pw.quality.set_visible(false),
            }
        }

        if let Some(bytes) = self.ds.art_bytes() {
            let gbytes = glib::Bytes::from(&bytes);
            if let Ok(tex) = gtk::gdk::Texture::from_bytes(&gbytes) {
                self.pw.artwork.set_paintable(Some(&tex));
                self.pw.art_stack.set_visible_child_name("artwork");
            }
        }

    }

    // ── Input / Output display ────────────────────────────────────────────────

    fn update_input_display(&self) {
        let mode = self.ds.current_mode();
        let source_id = capabilities::mode_to_input_source(&mode);
        self.pw.input_icon.set_paintable(Some(self.icons.source_paintable(source_id)));

        let sv = self.sw.ids.borrow();
        if let Some(idx) = sv.iter().position(|s| s == source_id) {
            *self.sw.updating.borrow_mut() = true;
            self.sw.dropdown.set_selected(idx as u32);
            *self.sw.updating.borrow_mut() = false;
        }

        if self.ds.art_bytes().is_some() {
            self.pw.art_stack.set_visible_child_name("artwork");
        } else {
            self.pw.artwork.set_paintable(None::<&gtk::gdk::Paintable>);
            self.pw.art_stack.set_visible_child_name("icon");
        }

    }

    fn update_output_display(&self) {
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

    fn on_presets_changed(&self) {
        use crate::api::PresetKind;
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
    /// `ssid`.  Guarded by `applied_window_key` so repeated device-changed
    /// fires for the same device don't override the user's manual resizes.
    fn apply_device_window_state(&self, ssid: &str) {
        if ssid.is_empty() { return; }
        let prev_ssid = self.applied_window_key.borrow().clone();
        if prev_ssid == ssid { return; }

        // Save the previous device's window state before overwriting the layout.
        // We use prev_ssid directly rather than ds.device_info() because by the
        // time this is called from apply_device_info, device_info() already points
        // to the new device.
        if !prev_ssid.is_empty() {
            let dev_cfg = DeviceConfig {
                window_maximized: self.window.is_maximized(),
                window_width:     if self.window.is_maximized() { 0 } else { self.window.width() },
                window_height:    if self.window.is_maximized() { 0 } else { self.window.height() },
                panel_visible:    self.sidebar_btn.is_active(),
                paned_position:   *self.saved_panel_width.borrow(),
                mini_mode:        *self.mini_mode.borrow(),
            };
            let mut cfg = Config::load();
            cfg.save_device(&prev_ssid, dev_cfg);
            cfg.save();
        }

        *self.applied_window_key.borrow_mut() = ssid.to_string();

        let dev_cfg = Config::load().device(ssid);

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
    fn save_config_now(&self) {
        let ssid = match self.ds.device_info() {
            Some(di) if !di.ssid.is_empty() => di.ssid,
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
        let dev_cfg = DeviceConfig {
            window_maximized: maximized,
            window_width:     if maximized { 0 } else { w },
            window_height:    if maximized { 0 } else { h },
            panel_visible:    self.sidebar_btn.is_active(),
            paned_position:   *self.saved_panel_width.borrow(),
            mini_mode:        *self.mini_mode.borrow(),
        };
        let mut cfg = Config::load();
        cfg.last_ssid = ssid.clone();
        cfg.save_device(ssid, dev_cfg);
        cfg.save();
    }
    // ── Mini player ───────────────────────────────────────────────────────────

    fn update_mini_playback(&self) {
        if let Some(di) = self.ds.device_info() {
            self.mini.device_label.set_label(&di.device_name);
        }
        if let Some(st) = self.ds.player_status() {
            let muted = st.mute == "1";
            self.sync_vol_display(&self.mini.vol_scale.clone(), &self.mini.vol_btn, &self.mini.mute_btn, muted);
            self.mini.btn_play.set_icon_name(if st.status == "play" {
                "media-playback-pause-symbolic"
            } else {
                "media-playback-start-symbolic"
            });
            self.mini.status_label.set_label(
                &format_status(&st.status, &st.mode, &st.vendor));
        }
        if let Some(m) = self.ds.metadata() {
            let title = if is_unknown(&m.title) { "—".to_string() } else { m.title.clone() };
            self.mini.title_label.set_label(&title);
            let artist = if is_unknown(&m.artist) { String::new() } else { m.artist.clone() };
            let album  = if is_unknown(&m.album)  { String::new() } else { m.album.clone() };
            let artist_line = match (artist.is_empty(), album.is_empty()) {
                (true,  true)  => String::new(),
                (true,  false) => album,
                (false, true)  => artist,
                (false, false) => format!("{artist} \u{00b7} {album}"),
            };
            self.mini.artist_label.set_label(&artist_line);
        }
        if let Some(bytes) = self.ds.art_bytes() {
            let gbytes = glib::Bytes::from(&bytes);
            if let Ok(tex) = gtk::gdk::Texture::from_bytes(&gbytes) {
                self.mini.artwork.set_paintable(Some(&tex));
                self.mini.art_stack.set_visible_child_name("artwork");
                return;
            }
        }
        let mode = self.ds.current_mode();
        let source_id = capabilities::mode_to_input_source(&mode);
        self.mini.input_icon.set_paintable(Some(self.icons.source_paintable(source_id)));
        self.mini.artwork.set_paintable(None::<&gtk::gdk::Paintable>);
        self.mini.art_stack.set_visible_child_name("icon");
    }

    fn enter_mini_mode(&self) {
        if *self.mini_mode.borrow() { return; }
        *self.pre_mini_size.borrow_mut() = (self.window.width(), self.window.height());
        self.update_mini_playback();
        *self.mini_mode.borrow_mut() = true;
        *self.mini_toggling.borrow_mut() = true;
        self.mini_btn.set_active(true);
        *self.mini_toggling.borrow_mut() = false;
        self.window.set_visible(false);
        self.mini_win.present();
    }

    fn exit_mini_mode(&self) {
        if !*self.mini_mode.borrow() { return; }
        *self.mini_mode.borrow_mut() = false;
        *self.mini_toggling.borrow_mut() = true;
        self.mini_btn.set_active(false);
        *self.mini_toggling.borrow_mut() = false;
        self.mini_win.set_visible(false);
        self.window.present();
        self.update_playback_ui();
        self.update_input_display();
    }
} // impl DeviceWindowInner

/// Schedule a deferred config save for `inner`, debounced at 500 ms.
/// Cancels any previously scheduled save so only one write happens per burst.
fn schedule_config_save(i: &Rc<DeviceWindowInner>) {
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


fn wifi_icon_for_rssi(rssi: i32) -> &'static str {
    match rssi {
        i32::MIN..=-85 | 0 => "network-wireless-offline-symbolic",
        -84..=-75           => "network-wireless-signal-weak-symbolic",
        -74..=-65           => "network-wireless-signal-ok-symbolic",
        -64..=-55           => "network-wireless-signal-good-symbolic",
        _                   => "network-wireless-signal-excellent-symbolic",
    }
}

// ── Device popover helpers ────────────────────────────────────────────────────

fn show_manual_ip_dialog(
    window:     &adw::ApplicationWindow,
    ds:         &DeviceState,
    dev_btn:    &gtk::MenuButton,
    manual_btn: &Button,
    saved_ip:   &Rc<RefCell<String>>,
) {
    let current = saved_ip.borrow().clone();
    let dialog = adw::AlertDialog::builder()
        .heading("Connect to WiiM")
        .body("Enter the IP address of your WiiM device.")
        .close_response("cancel")
        .build();
    dialog.add_response("cancel", "Cancel");
    dialog.add_response("connect", "Connect");
    dialog.set_response_appearance("connect", adw::ResponseAppearance::Suggested);
    dialog.set_default_response(Some("connect"));

    let entry = gtk::Entry::builder()
        .placeholder_text("192.168.1.x")
        .text(&current)
        .activates_default(true)
        .build();
    dialog.set_extra_child(Some(&entry));

    dialog.connect_response(None, clone!(
        @strong ds, @strong entry, @strong saved_ip, @strong dev_btn, @strong manual_btn
            => move |_dlg, resp| {
                if resp == "connect" {
                    let ip = entry.text().to_string();
                    if !ip.is_empty() {
                        *saved_ip.borrow_mut() = ip.clone();
                        let label = format!("Manual: {ip}");
                        dev_btn.set_label(&label);
                        manual_btn.set_label(&label);
                        ds.set_device(&ip, TlsMode::HttpsWiiM, None);
                    }
                }
            }
    ));
    dialog.present(Some(window));
}

fn build_device_popover(
    devs:      &[discovery::DiscoveredDevice],
    ds:        &DeviceState,
    dev_btn:   &gtk::MenuButton,
    window:    &adw::ApplicationWindow,
    saved_ip:  &Rc<RefCell<String>>,
    on_select: impl Fn(&str) + Clone + 'static,
) -> gtk::Popover {
    let vbox = GtkBox::new(Orientation::Vertical, 0);
    vbox.set_margin_top(4);
    vbox.set_margin_bottom(4);

    if devs.is_empty() {
        let lbl = Label::builder()
            .label("No devices found")
            .sensitive(false)
            .margin_top(6).margin_bottom(6).margin_start(12).margin_end(12)
            .build();
        vbox.append(&lbl);
    } else {
        for d in devs {
            let label    = format!("{} ({})", d.name, d.ip);
            let ip       = d.ip.clone();
            let ssid     = d.ssid.clone();
            let tls_mode = d.tls_mode;
            let on_sel   = on_select.clone();
            let btn = Button::builder().label(&label).css_classes(["flat"]).build();
            btn.connect_clicked(clone!(
                @strong ds, @strong dev_btn, @strong label
                    => move |_| {
                        on_sel(&ssid);
                        dev_btn.set_label(&label);
                        dev_btn.popdown();
                        ds.set_device(&ip, tls_mode, None);
                    }
            ));
            vbox.append(&btn);
        }
    }

    vbox.append(&gtk::Separator::new(Orientation::Horizontal));

    let saved = saved_ip.borrow().clone();
    let manual_label = if !saved.is_empty() && !devs.iter().any(|d| d.ip == saved) {
        format!("Manual: {saved}")
    } else {
        "Manual IP…".to_string()
    };
    let manual_btn = Button::builder().label(&manual_label).css_classes(["flat"]).build();
    manual_btn.connect_clicked(clone!(
        @strong ds, @strong dev_btn, @strong window, @strong saved_ip, @strong manual_btn
            => move |_| {
                dev_btn.popdown();
                show_manual_ip_dialog(&window, &ds, &dev_btn, &manual_btn, &saved_ip);
            }
    ));
    vbox.append(&manual_btn);

    let popover = gtk::Popover::new();
    popover.set_child(Some(&vbox));
    popover
}

// ── Widget builder functions ──────────────────────────────────────────────────

fn build_header(init_panel_visible: bool) -> (adw::HeaderBar, gtk::ToggleButton, gtk::MenuButton, gtk::ToggleButton) {
    let header = adw::HeaderBar::new();

    let sidebar_btn = gtk::ToggleButton::builder()
        .icon_name("sidebar-show-symbolic")
        .active(init_panel_visible)
        .tooltip_text("Toggle presets panel")
        .build();
    sidebar_btn.add_css_class("sidebar-toggle");
    header.pack_start(&sidebar_btn);

    let dev_btn = gtk::MenuButton::builder().label("Scanning…").build();
    header.pack_start(&dev_btn);

    let app_menu = gio::Menu::new();
    app_menu.append(Some("Settings…"), Some("win.settings"));
    app_menu.append(Some("About RustyWiiM"), Some("win.about"));
    let app_menu_btn = gtk::MenuButton::builder()
        .icon_name("open-menu-symbolic")
        .menu_model(&app_menu)
        .tooltip_text("Menu")
        .build();
    header.pack_end(&app_menu_btn);

    let mini_btn = gtk::ToggleButton::builder()
        .icon_name("view-restore-symbolic")
        .tooltip_text("Mini player")
        .build();
    header.pack_end(&mini_btn);

    (header, sidebar_btn, dev_btn, mini_btn)
}

fn build_presets_panel() -> (PresetWidgets, gtk::ScrolledWindow) {
    let presets_box = GtkBox::builder()
        .orientation(Orientation::Vertical).spacing(2)
        .margin_top(8).margin_bottom(4).margin_start(8).margin_end(8)
        .build();
    presets_box.append(
        &Label::builder()
            .label("PRESETS").css_classes(["section-label"])
            .halign(Align::Start).margin_bottom(4)
            .build(),
    );

    let mut preset_btns:   Vec<Button>     = Vec::new();
    let mut preset_pics:   Vec<gtk::Image> = Vec::new();
    let mut preset_labels: Vec<Label>      = Vec::new();

    for i in 1..=12u32 {
        let badge = Label::builder()
            .label(&i.to_string()).css_classes(["preset-badge"])
            .halign(Align::Center).valign(Align::Center)
            .build();
        let pic = gtk::Image::builder()
            .pixel_size(40).icon_name("audio-x-generic-symbolic")
            .build();
        pic.add_css_class("preset-art");
        pic.set_overflow(gtk::Overflow::Hidden);
        let lbl = Label::builder()
            .label("").css_classes(["preset-name"])
            .ellipsize(gtk::pango::EllipsizeMode::End)
            .halign(Align::Start).hexpand(true).width_chars(0)
            .build();
        let tile = GtkBox::builder()
            .orientation(Orientation::Horizontal).spacing(6)
            .css_classes(["preset-tile"]).overflow(gtk::Overflow::Hidden)
            .build();
        tile.append(&badge);
        tile.append(&pic);
        tile.append(&lbl);
        let btn = Button::builder().child(&tile).css_classes(["flat"]).build();
        btn.set_tooltip_text(Some(&format!("Preset {i}")));
        btn.set_visible(false);
        presets_box.append(&btn);
        preset_btns.push(btn);
        preset_pics.push(pic);
        preset_labels.push(lbl);
    }

    let pp = PresetWidgets {
        btns:   Rc::new(preset_btns),
        pics:   Rc::new(preset_pics),
        labels: Rc::new(preset_labels),
    };

    let presets_scroll = gtk::ScrolledWindow::builder()
        .child(&presets_box)
        .hscrollbar_policy(gtk::PolicyType::Never)
        .vexpand(true)
        .build();

    (pp, presets_scroll)
}

fn build_source_widgets(icons: &Rc<icons::IconSet>) -> SourceWidgets {
    let icons = Rc::clone(icons);
    let sw = SourceWidgets {
        dropdown: gtk::DropDown::from_strings(&["—"]),
        ids:      Rc::new(RefCell::new(Vec::new())),
        enabled:  Rc::new(RefCell::new(Vec::new())),
        updating: Rc::new(RefCell::new(false)),
    };
    sw.dropdown.add_css_class("panel-dropdown");
    sw.dropdown.set_sensitive(false);

    let factory = gtk::SignalListItemFactory::new();
    factory.connect_setup(|_, obj| {
        let item = obj.downcast_ref::<gtk::ListItem>().unwrap();
        let hbox = GtkBox::builder()
            .orientation(Orientation::Horizontal).spacing(6).build();
        hbox.append(&gtk::Image::builder().pixel_size(16).build());
        hbox.append(&Label::builder().halign(Align::Start).build());
        item.set_child(Some(&hbox));
    });
    factory.connect_bind(clone!(
        @strong sw, @strong icons
            => move |_, obj| {
                let item = obj.downcast_ref::<gtk::ListItem>().unwrap();
                let pos  = item.position() as usize;
                if let Some(hbox) = item.child().and_downcast::<GtkBox>() {
                    let enabled = sw.enabled.borrow().get(pos).copied().unwrap_or(true);
                    let ids     = sw.ids.borrow();
                    let id      = ids.get(pos).map(String::as_str).unwrap_or("");
                    if let Some(img) = hbox.first_child().and_downcast::<gtk::Image>() {
                        img.set_paintable(Some(icons.source_paintable(id)));
                    }
                    if let Some(lbl) = hbox.last_child().and_downcast::<Label>() {
                        if let Some(so) = item.item().and_downcast::<gtk::StringObject>() {
                            lbl.set_label(&so.string());
                        }
                        lbl.set_sensitive(enabled);
                    }
                    item.set_activatable(enabled);
                }
            }
    ));
    factory.connect_unbind(|_, obj| {
        let item = obj.downcast_ref::<gtk::ListItem>().unwrap();
        item.set_activatable(true);
        if let Some(hbox) = item.child().and_downcast::<GtkBox>() {
            if let Some(lbl) = hbox.last_child().and_downcast::<Label>() {
                lbl.set_sensitive(true);
            }
        }
    });
    sw.dropdown.set_factory(Some(&factory));
    sw
}

fn build_output_widgets(icons: &Rc<icons::IconSet>) -> OutputWidgets {
    let icons = Rc::clone(icons);
    let ow = OutputWidgets {
        dropdown:    gtk::DropDown::from_strings(&["—"]),
        section:     GtkBox::builder()
            .orientation(Orientation::Vertical).spacing(4).visible(false).build(),
        modes:       Rc::new(RefCell::new(Vec::new())),
        canon_names: Rc::new(RefCell::new(Vec::new())),
        updating:    Rc::new(RefCell::new(false)),
    };
    ow.dropdown.add_css_class("panel-dropdown");
    ow.dropdown.set_sensitive(false);

    let factory = gtk::SignalListItemFactory::new();
    factory.connect_setup(|_, obj| {
        let item = obj.downcast_ref::<gtk::ListItem>().unwrap();
        let hbox = GtkBox::builder()
            .orientation(Orientation::Horizontal).spacing(6).build();
        hbox.append(&gtk::Image::builder().pixel_size(16).build());
        hbox.append(&Label::builder().halign(Align::Start).build());
        item.set_child(Some(&hbox));
    });
    factory.connect_bind(clone!(@strong ow, @strong icons => move |_, obj| {
        let item = obj.downcast_ref::<gtk::ListItem>().unwrap();
        let pos  = item.position() as usize;
        if let Some(hbox) = item.child().and_downcast::<GtkBox>() {
            let names = ow.canon_names.borrow();
            let canon = names.get(pos).copied().unwrap_or("");
            if let Some(img) = hbox.first_child().and_downcast::<gtk::Image>() {
                img.set_paintable(Some(icons.output_paintable(canon)));
            }
            if let Some(lbl) = hbox.last_child().and_downcast::<Label>() {
                if let Some(so) = item.item().and_downcast::<gtk::StringObject>() {
                    lbl.set_label(&so.string());
                }
            }
        }
    }));
    ow.dropdown.set_factory(Some(&factory));

    ow.section.append(
        &Label::builder()
            .label("OUTPUT").css_classes(["section-label"]).halign(Align::Start).build(),
    );
    ow.section.append(&ow.dropdown);

    ow
}

fn build_left_pane(sw: &SourceWidgets, ow: &OutputWidgets, presets_scroll: &gtk::ScrolledWindow) -> gtk::Box {
    let io_box = GtkBox::builder()
        .orientation(Orientation::Vertical).spacing(4)
        .margin_top(4).margin_bottom(8).margin_start(8).margin_end(8)
        .build();
    io_box.append(&gtk::Separator::new(Orientation::Horizontal));
    io_box.append(
        &Label::builder()
            .label("INPUT").css_classes(["section-label"])
            .halign(Align::Start).margin_top(6).build(),
    );
    io_box.append(&sw.dropdown);
    io_box.append(&ow.section);

    let left_pane = GtkBox::builder().orientation(Orientation::Vertical).build();
    left_pane.append(presets_scroll);
    left_pane.append(&io_box);
    left_pane
}

fn build_playback_widgets() -> (PlaybackWidgets, Scale) {
    // vol_btn must exist before we can set it as the popover's parent.
    let vol_btn = Button::builder()
        .icon_name("audio-volume-high-symbolic")
        .css_classes(["transport-btn", "circular", "flat", "vol-btn"])
        .tooltip_text("Volume")
        .build();

    let vol_scale = Scale::with_range(Orientation::Vertical, 0.0, 100.0, 1.0);
    vol_scale.set_inverted(true);
    vol_scale.set_vexpand(true);
    vol_scale.set_height_request(150);
    vol_scale.set_draw_value(false);
    vol_scale.set_width_request(24);
    vol_scale.set_round_digits(0);
    vol_scale.add_css_class("vol-pop");
    vol_scale.set_increments(5.0, 20.0);

    let mute_btn = Button::builder()
        .icon_name("audio-volume-muted-symbolic")
        .css_classes(["transport-btn", "circular"])
        .tooltip_text("Mute")
        .halign(Align::Center)
        .build();

    let vol_pop_box = GtkBox::builder()
        .orientation(Orientation::Vertical)
        .margin_top(6).margin_bottom(6).margin_start(6).margin_end(6)
        .spacing(4)
        .build();
    vol_pop_box.append(&vol_scale);
    vol_pop_box.append(&mute_btn);
    let vol_popover = gtk::Popover::new();
    vol_popover.set_child(Some(&vol_pop_box));
    vol_popover.set_parent(&vol_btn);

    let pw = PlaybackWidgets {
        artwork:    gtk::Picture::builder()
            .content_fit(gtk::ContentFit::Contain).can_shrink(true)
            .halign(Align::Center).vexpand(true).build(),
        input_icon: gtk::Image::builder()
            .pixel_size(128).halign(Align::Center).valign(Align::Center).build(),
        art_stack: {
            let s = gtk::Stack::new();
            s.set_vexpand(true);
            s.set_transition_type(gtk::StackTransitionType::Crossfade);
            s.set_transition_duration(200);
            s
        },
        title:    Label::builder().label("Not connected").css_classes(["track-title"])
            .ellipsize(gtk::pango::EllipsizeMode::End)
            .halign(Align::Center).justify(gtk::Justification::Center).build(),
        artist:   Label::builder().css_classes(["track-artist"])
            .ellipsize(gtk::pango::EllipsizeMode::End).halign(Align::Center).build(),
        album:    Label::builder().css_classes(["track-album"])
            .ellipsize(gtk::pango::EllipsizeMode::End).halign(Align::Center).build(),
        status:   Label::builder().css_classes(["status-badge"]).halign(Align::Center).build(),
        quality:  Label::builder().css_classes(["quality-label"]).halign(Align::Center)
            .visible(false).build(),
        pos:      Label::builder().label("0:00").css_classes(["dim-label"]).build(),
        dur:      Label::builder().label("0:00").css_classes(["dim-label"]).build(),
        seek:     Scale::with_range(Orientation::Horizontal, 0.0, 100.0, 1.0),
        btn_prev: Button::builder()
            .icon_name("media-skip-backward-symbolic")
            .css_classes(["transport-btn", "circular", "flat"]).build(),
        btn_play: Button::builder()
            .icon_name("media-playback-start-symbolic")
            .css_classes(["play-btn", "circular", "suggested-action"]).build(),
        btn_next: Button::builder()
            .icon_name("media-skip-forward-symbolic")
            .css_classes(["transport-btn", "circular", "flat"]).build(),
        shuffle:  Button::builder()
            .icon_name("media-playlist-shuffle-symbolic")
            .css_classes(["loop-btn", "circular", "flat"]).tooltip_text("Shuffle: Off").build(),
        repeat:   Button::builder()
            .icon_name("media-playlist-repeat-symbolic")
            .css_classes(["loop-btn", "circular", "flat"]).tooltip_text("Repeat: Off").build(),
        vol_btn,
        vol_popover,
        mute_btn,
    };

    pw.art_stack.add_named(&pw.artwork, Some("artwork"));
    pw.art_stack.add_named(&pw.input_icon, Some("icon"));
    pw.seek.set_hexpand(true);
    pw.seek.set_draw_value(false);
    pw.seek.add_css_class("seek-scale");
    pw.seek.set_round_digits(0);

    (pw, vol_scale)
}

fn build_right_pane(pw: &PlaybackWidgets) -> gtk::Box {
    // Transport buttons are centred.
    let transport = GtkBox::builder()
        .orientation(Orientation::Horizontal).spacing(12).halign(Align::Center).build();
    transport.prepend(&pw.shuffle);
    transport.append(&pw.btn_prev);
    transport.append(&pw.btn_play);
    transport.append(&pw.btn_next);
    transport.append(&pw.repeat);

    // Vol button sits at the right edge of the seek row, aligned with the bar's right end.
    let seek_row = GtkBox::builder().orientation(Orientation::Horizontal).spacing(8).build();
    seek_row.append(&pw.pos);
    seek_row.append(&pw.seek);
    seek_row.append(&pw.dur);
    pw.vol_btn.set_margin_start(4);
    seek_row.append(&pw.vol_btn);

    // Overlay adds a radial vignette frame over the artwork that fades into the panel background.
    let art_overlay = gtk::Overlay::new();
    art_overlay.set_vexpand(true);
    art_overlay.set_child(Some(&pw.art_stack));
    let art_frame = GtkBox::builder()
        .hexpand(true).vexpand(true)
        .css_classes(["art-frame"])
        .can_target(false)
        .build();
    art_overlay.add_overlay(&art_frame);

    let right_pane = GtkBox::builder()
        .orientation(Orientation::Vertical).spacing(8).hexpand(true)
        .margin_top(8).margin_bottom(8).margin_start(12).margin_end(16)
        .build();
    right_pane.append(&art_overlay);
    right_pane.append(&pw.title);
    right_pane.append(&pw.artist);
    right_pane.append(&pw.album);
    right_pane.append(&pw.status);
    right_pane.append(&pw.quality);
    right_pane.append(&seek_row);
    right_pane.append(&transport);

    right_pane
}

fn build_mini_window() -> (MiniWidgets, gtk::Window) {
    let mini_artwork = gtk::Picture::builder()
        .content_fit(gtk::ContentFit::Cover).can_shrink(true)
        .halign(Align::Fill).valign(Align::Fill)
        .hexpand(true).vexpand(true)
        .build();
    let mini_input_icon = gtk::Image::builder()
        .pixel_size(36).halign(Align::Center).valign(Align::Center)
        .build();
    let mini_art_stack = {
        let s = gtk::Stack::new();
        s.set_hexpand(false);
        s.set_vexpand(false);
        s.set_valign(Align::Center);
        s.add_css_class("mini-art");
        s.set_transition_type(gtk::StackTransitionType::Crossfade);
        s.set_transition_duration(200);
        s
    };
    mini_art_stack.add_named(&mini_artwork, Some("artwork"));
    mini_art_stack.add_named(&mini_input_icon, Some("icon"));

    let mini_device_label = Label::builder()
        .label("").css_classes(["mini-device-label"])
        .halign(Align::Start).hexpand(true)
        .ellipsize(gtk::pango::EllipsizeMode::End)
        .build();
    let mini_restore_btn = Button::builder()
        .icon_name("view-fullscreen-symbolic")
        .css_classes(["mini-restore-btn"])
        .tooltip_text("Restore")
        .build();
    let mini_top_bar = GtkBox::builder()
        .orientation(Orientation::Horizontal).spacing(4)
        .margin_start(14).margin_end(12).margin_top(10).margin_bottom(4)
        .build();
    mini_top_bar.append(&mini_device_label);
    mini_top_bar.append(&mini_restore_btn);

    let mini_title_label = Label::builder()
        .label("—").css_classes(["mini-title"])
        .halign(Align::Start).hexpand(true)
        .ellipsize(gtk::pango::EllipsizeMode::End)
        .build();
    let mini_artist_label = Label::builder()
        .label("").css_classes(["mini-artist"])
        .halign(Align::Start).hexpand(true)
        .ellipsize(gtk::pango::EllipsizeMode::End)
        .build();

    let mini_btn_prev = Button::builder()
        .icon_name("media-skip-backward-symbolic")
        .css_classes(["mini-transport-btn", "flat"]).build();
    let mini_btn_play = Button::builder()
        .icon_name("media-playback-start-symbolic")
        .css_classes(["mini-play-btn", "suggested-action"]).build();
    let mini_btn_next = Button::builder()
        .icon_name("media-skip-forward-symbolic")
        .css_classes(["mini-transport-btn", "flat"]).build();

    // Volume button with popover — must be created before mini_transport append.
    let mini_vol_btn = Button::builder()
        .icon_name("audio-volume-high-symbolic")
        .css_classes(["mini-transport-btn", "mini-vol-btn", "flat"])
        .tooltip_text("Volume")
        .build();
    let mini_vol_scale = Scale::with_range(Orientation::Vertical, 0.0, 100.0, 1.0);
    mini_vol_scale.set_inverted(true);
    mini_vol_scale.set_vexpand(true);
    mini_vol_scale.set_height_request(120);
    mini_vol_scale.set_draw_value(false);
    mini_vol_scale.set_width_request(20);
    mini_vol_scale.set_round_digits(0);
    mini_vol_scale.add_css_class("mini-vol-pop");
    mini_vol_scale.set_increments(5.0, 20.0);
    let mini_mute_btn = Button::builder()
        .icon_name("audio-volume-muted-symbolic")
        .css_classes(["mini-transport-btn"])
        .tooltip_text("Mute")
        .halign(Align::Center)
        .build();
    let mini_vol_pop_box = GtkBox::builder()
        .orientation(Orientation::Vertical)
        .margin_top(4).margin_bottom(4).margin_start(4).margin_end(4)
        .spacing(4)
        .build();
    mini_vol_pop_box.append(&mini_vol_scale);
    mini_vol_pop_box.append(&mini_mute_btn);
    let mini_vol_popover = gtk::Popover::new();
    mini_vol_popover.add_css_class("mini-vol-popover");
    mini_vol_popover.set_child(Some(&mini_vol_pop_box));
    mini_vol_popover.set_parent(&mini_vol_btn);

    let mini_transport_center = GtkBox::builder()
        .orientation(Orientation::Horizontal).spacing(6).build();
    mini_transport_center.append(&mini_btn_prev);
    mini_transport_center.append(&mini_btn_play);
    mini_transport_center.append(&mini_btn_next);

    mini_vol_btn.set_margin_end(6);
    let mini_vol_end = GtkBox::builder()
        .hexpand(true).halign(Align::End).valign(Align::Center).build();
    mini_vol_end.append(&mini_vol_btn);

    let mini_status_label = Label::builder()
        .label("").css_classes(["mini-status-label"])
        .halign(Align::Start).hexpand(true)
        .ellipsize(gtk::pango::EllipsizeMode::End)
        .build();
    let mini_transport = GtkBox::builder()
        .orientation(Orientation::Horizontal).hexpand(true).build();
    mini_transport.append(&mini_status_label);
    mini_transport.append(&mini_transport_center);
    mini_transport.append(&mini_vol_end);

    let mini_info_box = GtkBox::builder()
        .orientation(Orientation::Vertical).spacing(4)
        .valign(Align::Center).hexpand(true)
        .build();
    mini_info_box.append(&mini_title_label);
    mini_info_box.append(&mini_artist_label);
    mini_info_box.append(&mini_transport);

    let mini_main_row = GtkBox::builder()
        .orientation(Orientation::Horizontal).spacing(12)
        .margin_start(14).margin_end(14).margin_bottom(14)
        .build();
    mini_main_row.append(&mini_art_stack);
    mini_main_row.append(&mini_info_box);

    let mini_outer = GtkBox::builder()
        .orientation(Orientation::Vertical).spacing(0)
        .build();
    mini_outer.append(&mini_top_bar);
    mini_outer.append(&mini_main_row);

    let mini_root = gtk::WindowHandle::new();
    mini_root.set_child(Some(&mini_outer));

    let mini_win = gtk::Window::builder()
        .decorated(false)
        .resizable(false)
        .default_width(360)
        .title("RustyWiiM")
        .child(&mini_root)
        .build();
    mini_win.add_css_class("mini-window");

    let mini = MiniWidgets {
        root:          mini_root,
        art_stack:     mini_art_stack,
        artwork:       mini_artwork,
        input_icon:    mini_input_icon,
        device_label:  mini_device_label,
        restore_btn:   mini_restore_btn,
        title_label:   mini_title_label,
        artist_label:  mini_artist_label,
        status_label:  mini_status_label,
        btn_prev:      mini_btn_prev,
        btn_play:      mini_btn_play,
        btn_next:      mini_btn_next,
        vol_btn:       mini_vol_btn,
        vol_popover:   mini_vol_popover,
        mute_btn:      mini_mute_btn,
        vol_scale:     mini_vol_scale,
    };

    (mini, mini_win)
}

// ── Main UI ───────────────────────────────────────────────────────────────────

// ── CSS ───────────────────────────────────────────────────────────────────────

thread_local! {
    static THEME_PROVIDER: RefCell<Option<CssProvider>> = const { RefCell::new(None) };
}

fn theme_css(theme: ThemeMode) -> &'static str {
    match theme {
        ThemeMode::RustyWiiM => DARK_CSS,
        _                    => SYSTEM_CSS,
    }
}

fn apply_color_scheme(theme: ThemeMode) {
    let scheme = match theme {
        ThemeMode::System      => adw::ColorScheme::Default,
        ThemeMode::SystemLight => adw::ColorScheme::ForceLight,
        ThemeMode::SystemDark  => adw::ColorScheme::ForceDark,
        ThemeMode::RustyWiiM  => adw::ColorScheme::ForceDark,
    };
    adw::StyleManager::default().set_color_scheme(scheme);
}

/// Initialise the CSS provider for the current process.  Must be called once.
fn init_css(theme: ThemeMode) {
    apply_color_scheme(theme);
    let provider = CssProvider::new();
    provider.load_from_string(theme_css(theme));
    gtk::style_context_add_provider_for_display(
        &gtk::gdk::Display::default().unwrap(),
        &provider,
        gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
    );
    THEME_PROVIDER.with(|p| *p.borrow_mut() = Some(provider));
}

/// Switch the active CSS theme at runtime.
fn apply_theme(theme: ThemeMode) {
    apply_color_scheme(theme);
    THEME_PROVIDER.with(|p| {
        if let Some(provider) = p.borrow().as_ref() {
            provider.load_from_string(theme_css(theme));
        }
    });
}

fn show_settings_dialog(window: &adw::ApplicationWindow) {
    let cfg = Config::load();

    let theme_list = gtk::StringList::new(&["System", "System Light", "System Dark", "RustyWiiM"]);
    let theme_row = adw::ComboRow::builder()
        .title("Theme")
        .subtitle("Application colour scheme")
        .model(&theme_list)
        .build();
    theme_row.set_selected(match cfg.theme {
        ThemeMode::System      => 0,
        ThemeMode::SystemLight => 1,
        ThemeMode::SystemDark  => 2,
        ThemeMode::RustyWiiM  => 3,
    });
    theme_row.connect_selected_notify(move |row| {
        let theme = match row.selected() {
            0 => ThemeMode::System,
            1 => ThemeMode::SystemLight,
            2 => ThemeMode::SystemDark,
            _ => ThemeMode::RustyWiiM,
        };
        apply_theme(theme);
        let mut cfg = Config::load();
        cfg.theme = theme;
        cfg.save();
    });

    let group = adw::PreferencesGroup::builder()
        .title("Appearance")
        .build();
    group.add(&theme_row);

    let page = adw::PreferencesPage::new();
    page.add(&group);

    let dialog = adw::PreferencesDialog::new();
    dialog.add(&page);
    dialog.present(Some(window));
}

// ── DeviceWindow ──────────────────────────────────────────────────────────────

/// One device window.  Owns the GTK window and all content widgets.
/// Future work: keep a list of these in a top-level app struct for multi-device support.
pub struct DeviceWindow {
    pub window: adw::ApplicationWindow,
    inner:      Rc<DeviceWindowInner>,
}

impl DeviceWindow {
    pub fn ds(&self) -> &DeviceState { &self.inner.ds }

    /// Build and wire a complete device window.  The tokio `rt` is shared across
    /// all windows so there is only one thread-pool for the whole process.
    pub fn new(app: &adw::Application, rt: Arc<tokio::runtime::Runtime>) -> Self {
        let cfg         = Config::load();
        init_css(cfg.theme);

        let icons = Rc::new(icons::IconSet::load());
        let init_dev_cfg = cfg.device(&cfg.last_ssid);

        let ds = DeviceState::new(rt);
        if !cfg.last_ip.is_empty() {
            // Pass the last known SSID so fetch_device_info can abort if the IP
            // was reassigned to a different device.  Discovery will then reconnect
            // to the right device by SSID.  Pass None if no SSID is on record
            // (first run or old config) so we connect unconditionally.
            let expected = if cfg.last_ssid.is_empty() { None } else { Some(cfg.last_ssid.as_str()) };
            ds.set_device(&cfg.last_ip, TlsMode::HttpsWiiM, expected);
        }
        ds.start_polling();

        let (header, sidebar_btn, dev_btn, mini_btn) = build_header(init_dev_cfg.panel_visible);
        let (pp, presets_scroll) = build_presets_panel();
        let sw = build_source_widgets(&icons);
        let ow = build_output_widgets(&icons);
        let left_pane = build_left_pane(&sw, &ow, &presets_scroll);
        let (pw, vol_scale) = build_playback_widgets();
        let right_pane = build_right_pane(&pw);
        let (mini, mini_win) = build_mini_window();

        // ── Paned split + sidebar logic ───────────────────────────────────────────
        let paned = gtk::Paned::new(Orientation::Horizontal);
        paned.set_start_child(Some(&left_pane));
        paned.set_end_child(Some(&right_pane));
        paned.set_shrink_start_child(true);
        paned.set_shrink_end_child(false);
        paned.set_resize_start_child(false);
        paned.set_resize_end_child(true);
        paned.set_margin_top(4);
        paned.set_margin_bottom(8);

        let panel_width = if init_dev_cfg.paned_position > 0 { init_dev_cfg.paned_position } else { 200 };
        paned.set_position(panel_width);
        left_pane.set_visible(init_dev_cfg.panel_visible);

        let saved_panel_width  = Rc::new(RefCell::new(panel_width));
        let panel_collapsing   = Rc::new(RefCell::new(false));
        let settle_timer:      Rc<RefCell<Option<glib::SourceId>>> = Rc::new(RefCell::new(None));
        let config_save_timer: Rc<RefCell<Option<glib::SourceId>>> = Rc::new(RefCell::new(None));

        let dev_info_label = Label::builder()
            .css_classes(["device-info"]).halign(Align::Center)
            .hexpand(true)
            .margin_top(4).margin_bottom(4).build();

        let net_icon = gtk::Image::builder()
            .icon_size(gtk::IconSize::Normal)
            .css_classes(["net-icon"])
            .margin_end(8).margin_top(4).margin_bottom(4)
            .visible(false)
            .build();

        let bottom_bar = gtk::CenterBox::new();
        bottom_bar.set_center_widget(Some(&dev_info_label));
        bottom_bar.set_end_widget(Some(&net_icon));

        let outer = GtkBox::new(Orientation::Vertical, 0);
        outer.append(&paned);
        outer.append(&gtk::Separator::new(Orientation::Horizontal));
        outer.append(&bottom_bar);

        let full_toolbar = adw::ToolbarView::new();
        full_toolbar.add_top_bar(&header);
        full_toolbar.set_content(Some(&outer));

        let win_w = if init_dev_cfg.window_width  > 0 { init_dev_cfg.window_width  } else { 680 };
        let win_h = if init_dev_cfg.window_height > 0 { init_dev_cfg.window_height } else { 640 };
        let window = adw::ApplicationWindow::builder()
            .application(app).title("RustyWiiM").content(&full_toolbar)
            .default_width(win_w).default_height(win_h)
            .build();
        if init_dev_cfg.window_maximized { window.maximize(); }

        // ── Shared UI state ───────────────────────────────────────────────────────
        let ui_state = PlaybackUiState {
            is_playing:   Rc::new(RefCell::new(false)),
            drag_timer:   Rc::new(RefCell::new(None)),
        };

        let inner = Rc::new(DeviceWindowInner {
            ds: ds.clone(),
            sw,
            ow,
            pw,
            pp,
            dev_info_label,
            net_icon,
            icons,
            vol_scale,
            ui_state,
            window: window.clone(),
            paned:  paned.clone(),
            left_pane: left_pane.clone(),
            sidebar_btn: sidebar_btn.clone(),
            saved_panel_width,
            panel_collapsing,
            settle_timer,
            config_save_timer,
            applied_window_key: RefCell::new(cfg.last_ssid.clone()),
            mini,
            mini_mode:         RefCell::new(false),
            mini_toggling:     RefCell::new(false),
            pre_mini_size:     RefCell::new((0, 0)),
            mini_btn:          mini_btn.clone(),
            mini_win:          mini_win.clone(),
        });

        // ── DeviceState signal connections ────────────────────────────────────────
        ds.connect_device_changed({
            let i = Rc::clone(&inner);
            move |_| {
                i.update_network_icon();
                if i.ds.device_info().is_none() {
                    let title = match i.ds.connection_state() {
                        ConnectionState::Connecting   => "Connecting…",
                        ConnectionState::Failed       => "Disconnected",
                        _                             => "",
                    };
                    i.reset_device_ui(title);
                } else {
                    i.apply_device_info();
                    i.on_presets_changed();
                }
            }
        });

        ds.connect_network_changed({
            let i = Rc::clone(&inner);
            move |_| { i.update_network_icon(); }
        });

        ds.connect_playback_changed({
            let i = Rc::clone(&inner);
            move |_| {
                if *i.mini_mode.borrow() { i.update_mini_playback(); } else { i.update_playback_ui(); }
            }
        });

        ds.connect_input_changed({
            let i = Rc::clone(&inner);
            move |_| {
                if *i.mini_mode.borrow() { i.update_mini_playback(); } else { i.update_input_display(); }
            }
        });

        ds.connect_output_changed({
            let i = Rc::clone(&inner);
            move |_| { i.update_output_display(); }
        });

        ds.connect_outputs_changed({
            let i = Rc::clone(&inner);
            move |_| { i.populate_output(); i.update_output_display(); }
        });

        ds.connect_presets_changed({
            let i = Rc::clone(&inner);
            move |_| { i.on_presets_changed(); }
        });

        // ── Sidebar toggle ────────────────────────────────────────────────────────
        let paned_btn_held = Rc::new(RefCell::new(false));
        const SNAP_PX: i32 = 30;

        inner.paned.connect_position_notify({
            let i    = Rc::clone(&inner);
            let held = Rc::clone(&paned_btn_held);
            move |p| {
                if *i.panel_collapsing.borrow() { return; }
                let pos = p.position();
                if pos >= SNAP_PX {
                    if !i.left_pane.is_visible() {
                        *i.panel_collapsing.borrow_mut() = true;
                        i.left_pane.set_visible(true);
                        *i.panel_collapsing.borrow_mut() = false;
                    }
                } else if i.left_pane.is_visible() {
                    *i.panel_collapsing.borrow_mut() = true;
                    i.left_pane.set_visible(false);
                    *i.panel_collapsing.borrow_mut() = false;
                }
                if let Some(id) = i.settle_timer.borrow_mut().take() { id.remove(); }
                let i2    = Rc::clone(&i);
                let held2 = Rc::clone(&held);
                let id = glib::timeout_add_local_once(
                    std::time::Duration::from_millis(50),
                    move || {
                        *i2.settle_timer.borrow_mut() = None;
                        let btn_held = *held2.borrow();
                        *held2.borrow_mut() = false;
                        let shown = i2.left_pane.is_visible();
                        if i2.sidebar_btn.is_active() != shown {
                            *i2.panel_collapsing.borrow_mut() = true;
                            i2.sidebar_btn.set_active(shown);
                            *i2.panel_collapsing.borrow_mut() = false;
                        }
                        if shown && !btn_held {
                            let pos = i2.paned.position();
                            if pos >= SNAP_PX { *i2.saved_panel_width.borrow_mut() = pos; }
                        }
                        schedule_config_save(&i2);
                    },
                );
                *i.settle_timer.borrow_mut() = Some(id);
            }
        });

        {
            let drag_ctrl = gtk::EventControllerLegacy::new();
            drag_ctrl.connect_event({
                let i    = Rc::clone(&inner);
                let held = Rc::clone(&paned_btn_held);
                move |_, event| {
                    match event.event_type() {
                        gtk::gdk::EventType::ButtonPress => {
                            *held.borrow_mut() = true;
                        }
                        gtk::gdk::EventType::ButtonRelease => {
                            *held.borrow_mut() = false;
                            if let Some(id) = i.settle_timer.borrow_mut().take() { id.remove(); }
                            let shown = i.left_pane.is_visible();
                            if i.sidebar_btn.is_active() != shown {
                                *i.panel_collapsing.borrow_mut() = true;
                                i.sidebar_btn.set_active(shown);
                                *i.panel_collapsing.borrow_mut() = false;
                            }
                            if shown {
                                let pos = i.paned.position();
                                if pos >= SNAP_PX { *i.saved_panel_width.borrow_mut() = pos; }
                            }
                            schedule_config_save(&i);
                        }
                        _ => {}
                    }
                    glib::Propagation::Proceed
                }
            });
            inner.paned.add_controller(drag_ctrl);
        }

        inner.sidebar_btn.connect_toggled({
            let i = Rc::clone(&inner);
            move |btn| {
                if *i.panel_collapsing.borrow() { return; }
                if let Some(id) = i.settle_timer.borrow_mut().take() { id.remove(); }
                if btn.is_active() {
                    *i.panel_collapsing.borrow_mut() = true;
                    i.left_pane.set_visible(true);
                    let w = *i.saved_panel_width.borrow();
                    i.paned.set_position(w);
                    *i.panel_collapsing.borrow_mut() = false;
                } else {
                    *i.panel_collapsing.borrow_mut() = true;
                    i.left_pane.set_visible(false);
                    *i.panel_collapsing.borrow_mut() = false;
                }
                schedule_config_save(&i);
            }
        });

        // ── SSDP discovery ────────────────────────────────────────────────────────
        {
            let (tx, rx) = async_channel::bounded::<Vec<discovery::DiscoveredDevice>>(1);
            ds.rt().spawn(async move {
                let devs = discovery::discover(std::time::Duration::from_secs(4)).await;
                let _ = tx.send(devs).await;
            });

            let last_ssid_for_disc = cfg.last_ssid.clone();
            let saved_ip           = Rc::new(RefCell::new(cfg.last_ip.clone()));
            let inner_for_popover  = Rc::clone(&inner);
            glib::spawn_future_local(clone!(
                @strong ds, @strong dev_btn, @strong window, @strong saved_ip
                    => async move {
                        if let Ok(devs) = rx.recv().await {
                            let popover = build_device_popover(
                                &devs, &ds, &dev_btn, &window, &saved_ip,
                                {
                                    let i = inner_for_popover;
                                    move |ssid| { i.apply_device_window_state(ssid); }
                                },
                            );
                            dev_btn.set_popover(Some(&popover));

                            let saved = saved_ip.borrow().clone();

                            // Prefer SSID match (survives IP changes); fall back to IP match.
                            let by_ssid = devs.iter().find(|d| {
                                !last_ssid_for_disc.is_empty()
                                    && !d.ssid.is_empty()
                                    && d.ssid == last_ssid_for_disc
                            });
                            let by_ip = devs.iter().find(|d| !saved.is_empty() && d.ip == saved);
                            let best = by_ssid.or(by_ip);

                            // Update the button label to reflect the discovered device
                            // (may now be at a different IP from last_ip).
                            match best {
                                Some(d) => dev_btn.set_label(&format!("{} ({})", d.name, d.ip)),
                                None if !saved.is_empty() => dev_btn.set_label(&format!("Manual: {saved}")),
                                None if devs.is_empty()   => dev_btn.set_label("No device"),
                                None => {}
                            }

                            // Auto-connect only if not already connecting/connected.
                            // Disconnected means either no last_ip or the SSID check failed.
                            if ds.connection_state() == ConnectionState::Disconnected {
                                let target = best.or_else(|| {
                                    // No SSID/IP match and no prior device — pick the only one.
                                    if saved.is_empty() { devs.first() } else { None }
                                });
                                if let Some(d) = target {
                                    dev_btn.set_label(&format!("{} ({})", d.name, d.ip));
                                    *saved_ip.borrow_mut() = d.ip.clone();
                                    ds.set_device(&d.ip, d.tls_mode, None);
                                }
                            }
                        }
                    }
            ));
        }

        // ── Transport / control signal handlers ───────────────────────────────────
        inner.pw.btn_play.connect_clicked({
            let i = Rc::clone(&inner);
            move |_| { i.ds.do_play_pause(); }
        });

        inner.pw.btn_prev.connect_clicked({
            let i = Rc::clone(&inner);
            move |_| { i.ds.do_prev(); }
        });

        inner.pw.btn_next.connect_clicked({
            let i = Rc::clone(&inner);
            move |_| { i.ds.do_next(); }
        });

        inner.pw.shuffle.connect_clicked({
            let i = Rc::clone(&inner);
            move |_| {
                let (shuf, rep) = i.ds.player_status()
                    .map(|s| decode_loop_mode(&s.loop_mode))
                    .unwrap_or((false, 0));
                i.ds.do_set_loop_mode(loop_api_mode(!shuf, rep));
            }
        });

        inner.pw.repeat.connect_clicked({
            let i = Rc::clone(&inner);
            move |_| {
                let (shuf, rep) = i.ds.player_status()
                    .map(|s| decode_loop_mode(&s.loop_mode))
                    .unwrap_or((false, 0));
                i.ds.do_set_loop_mode(loop_api_mode(shuf, (rep + 1) % 3));
            }
        });

        inner.pw.vol_btn.connect_clicked({
            let i = Rc::clone(&inner);
            move |_| {
                if i.pw.vol_popover.is_visible() { i.pw.vol_popover.popdown(); }
                else { i.pw.vol_popover.popup(); }
            }
        });

        inner.pw.mute_btn.connect_clicked({
            let i = Rc::clone(&inner);
            move |_| { i.ds.do_set_mute(!i.ds.muted()); }
        });

        inner.vol_scale.connect_change_value({
            let i = Rc::clone(&inner);
            move |_, _, vol| { i.on_vol_changed(vol); glib::Propagation::Proceed }
        });

        inner.pw.seek.connect_change_value({
            let i = Rc::clone(&inner);
            move |_, _, value| {
                if let Some(c) = i.ds.client() {
                    i.ds.rt().spawn(async move { let _ = c.seek(value as u32).await; });
                }
                glib::Propagation::Proceed
            }
        });

        inner.sw.dropdown.connect_selected_notify({
            let i = Rc::clone(&inner);
            move |dd| {
                if *i.sw.updating.borrow() { return; }
                let idx = dd.selected() as usize;
                let ids = i.sw.ids.borrow();
                if let Some(src) = ids.get(idx).cloned() {
                    i.ds.switch_input(src);
                }
            }
        });

        inner.ow.dropdown.connect_selected_notify({
            let i = Rc::clone(&inner);
            move |dd| {
                if *i.ow.updating.borrow() { return; }
                let idx = dd.selected() as usize;
                let modes = i.ow.modes.borrow();
                if let Some(&mode) = modes.get(idx) {
                    i.ds.set_audio_output(mode);
                }
            }
        });

        for (idx, btn) in inner.pp.btns.iter().enumerate() {
            let num = (idx + 1) as u32;
            let i = Rc::clone(&inner);
            btn.connect_clicked(move |_| {
                if let Some(c) = i.ds.client() {
                    i.ds.rt().spawn(async move { let _ = c.play_preset(num).await; });
                }
            });
        }

        // ── Mini player signals ───────────────────────────────────────────────────
        inner.mini_btn.connect_toggled({
            let i = Rc::clone(&inner);
            move |btn| {
                if *i.mini_toggling.borrow() { return; }
                if btn.is_active() { i.enter_mini_mode(); } else { i.exit_mini_mode(); }
            }
        });

        inner.mini.restore_btn.connect_clicked({
            let i = Rc::clone(&inner);
            move |_| { i.exit_mini_mode(); }
        });

        {
            let gesture = gtk::GestureClick::builder().button(1).build();
            gesture.connect_pressed({
                let i = Rc::clone(&inner);
                move |_, n_press, _, _| {
                    if n_press >= 2 { i.exit_mini_mode(); }
                }
            });
            inner.mini.root.add_controller(gesture);
        }

        inner.mini.btn_play.connect_clicked({
            let i = Rc::clone(&inner);
            move |_| { i.ds.do_play_pause(); }
        });

        inner.mini.btn_prev.connect_clicked({
            let i = Rc::clone(&inner);
            move |_| { i.ds.do_prev(); }
        });

        inner.mini.btn_next.connect_clicked({
            let i = Rc::clone(&inner);
            move |_| { i.ds.do_next(); }
        });

        inner.mini.vol_btn.connect_clicked({
            let i = Rc::clone(&inner);
            move |_| {
                if i.mini.vol_popover.is_visible() { i.mini.vol_popover.popdown(); }
                else { i.mini.vol_popover.popup(); }
            }
        });

        inner.mini.mute_btn.connect_clicked({
            let i = Rc::clone(&inner);
            move |_| { i.ds.do_set_mute(!i.ds.muted()); }
        });

        inner.mini.vol_scale.connect_change_value({
            let i = Rc::clone(&inner);
            move |_, _, vol| { i.on_vol_changed(vol); glib::Propagation::Proceed }
        });

        // ── Mini window signals ───────────────────────────────────────────────────
        // X / Alt+F4 on the mini window → exit mini mode (don't destroy the window).
        inner.mini_win.connect_close_request({
            let i = Rc::clone(&inner);
            move |_win| {
                i.exit_mini_mode();
                glib::Propagation::Stop
            }
        });

        // Ctrl+Q while the mini window is focused → quit the app.
        {
            let key_ctrl = gtk::EventControllerKey::new();
            key_ctrl.connect_key_pressed(clone!(@strong window => move |_, key, _, mods| {
                if key == gtk::gdk::Key::q
                    && mods.contains(gtk::gdk::ModifierType::CONTROL_MASK)
                {
                    window.close();
                    return glib::Propagation::Stop;
                }
                glib::Propagation::Proceed
            }));
            inner.mini_win.add_controller(key_ctrl);
        }

        // ── Window actions ────────────────────────────────────────────────────────
        let quit_action = gio::SimpleAction::new("quit", None);
        quit_action.connect_activate(clone!(@strong window => move |_, _| { window.close(); }));
        window.add_action(&quit_action);
        app.set_accels_for_action("win.quit", &["<Ctrl>Q"]);

        let settings_action = gio::SimpleAction::new("settings", None);
        settings_action.connect_activate(clone!(@strong window => move |_, _| {
            show_settings_dialog(&window);
        }));
        window.add_action(&settings_action);

        let about_action = gio::SimpleAction::new("about", None);
        about_action.connect_activate(clone!(@strong window => move |_, _| {
            adw::AboutDialog::builder()
                .application_name("RustyWiiM")
                .application_icon("audio-x-generic")
                .version(concat!(env!("CARGO_PKG_VERSION"), " (", env!("GIT_HASH"), ")"))
                .developer_name("Benjamin Herrenschmidt")
                .copyright("© 2026 Benjamin Herrenschmidt")
                .license_type(gtk::License::MitX11)
                .website("https://github.com/ozbenh/rustywiim")
                .build()
                .present(Some(&window));
        }));
        window.add_action(&about_action);

        // ── Save window state ─────────────────────────────────────────────────────
        window.connect_close_request({
            let i = Rc::clone(&inner);
            move |_win| {
                if let Some(id) = i.settle_timer.borrow_mut().take() { id.remove(); }
                if let Some(id) = i.config_save_timer.borrow_mut().take() { id.remove(); }
                i.save_config_now();
                i.mini_win.destroy();
                glib::Propagation::Proceed
            }
        });

        if init_dev_cfg.mini_mode {
            inner.enter_mini_mode();
        }

        Self { window, inner }
    }

    pub fn present(&self) {
        if *self.inner.mini_mode.borrow() {
            self.inner.mini_win.present();
        } else {
            self.window.present();
        }
    }
}
