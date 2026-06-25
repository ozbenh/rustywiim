#![allow(deprecated)] // glib clone! old-style @strong syntax

use std::cell::RefCell;
use std::rc::Rc;

use adw::prelude::*;
use glib::clone;
use gtk::gio;
use gtk::{Align, Box as GtkBox, Button, CssProvider, Label, Orientation, Scale, StringList};

use crate::api::TlsMode;
use crate::capabilities;
use crate::config::Config;
use crate::device_state::DeviceState;
use crate::discovery;
use crate::icons;

const CSS: &str = r#"
window {
    background-color: #0a0a0a;
}
.track-title {
    font-size: 20px;
    font-weight: 800;
    color: #ffffff;
}
.track-artist {
    font-size: 14px;
    font-weight: 600;
    color: #b0b0b0;
}
.track-album {
    font-size: 12px;
    color: #707070;
}
.status-badge {
    font-size: 11px;
    font-weight: 600;
    color: #4ecdc4;
}
.quality-label {
    font-size: 11px;
    color: #505050;
}
.dim-label {
    font-size: 11px;
    color: #606060;
}
.transport-btn {
    min-width: 36px;
    min-height: 36px;
    padding: 0;
    -gtk-icon-size: 16px;
    background-color: #2a2a2a;
    color: #ffffff;
    border: none;
    box-shadow: none;
}
.transport-btn:hover {
    background-color: #3a3a3a;
}
.play-btn {
    min-width: 44px;
    min-height: 44px;
    padding: 0;
    -gtk-icon-size: 20px;
    background-color: #4ecdc4;
    color: #0a0a0a;
    border: none;
    box-shadow: none;
}
.play-btn:hover {
    background-color: #5fd9d0;
}
.vol-scale trough {
    min-height: 3px;
    border-radius: 2px;
    background-color: #2a2a2a;
}
.vol-scale trough highlight {
    background-color: #4a4a4a;
}
.vol-scale slider {
    min-width: 8px;
    min-height: 8px;
    margin: -3px;
    background-color: #808080;
    border-radius: 50%;
    border: none;
    box-shadow: none;
}
.seek-scale trough {
    min-height: 4px;
    border-radius: 2px;
    background-color: #2a2a2a;
}
.seek-scale trough highlight {
    background-color: #4ecdc4;
}
.seek-scale slider {
    min-width: 0;
    min-height: 0;
    margin: 0;
    padding: 0;
    opacity: 0;
}
.section-label {
    font-size: 11px;
    font-weight: 700;
    color: #505050;
    letter-spacing: 1px;
}
.preset-tile {
    border-radius: 8px;
    background-color: #141414;
    padding: 4px;
}
.preset-tile:hover {
    background-color: #202020;
}
.preset-art {
    border-radius: 5px;
    min-width: 40px;
    min-height: 40px;
    max-width: 40px;
    max-height: 40px;
}
.preset-name {
    font-size: 11px;
    color: #a0a0a0;
}
.preset-badge {
    font-size: 9px;
    font-weight: 700;
    color: #1a1a1a;
    background-color: #606060;
    border-radius: 50%;
    min-width: 16px;
    min-height: 16px;
    padding: 1px;
}
.panel-dropdown {
    font-size: 11px;
    min-width: 0;
}
.device-info {
    font-size: 10px;
    color: #484848;
}
.sidebar-toggle:checked {
    color: #4ecdc4;
}
.loop-btn {
    min-width: 36px;
    min-height: 36px;
    padding: 0;
    -gtk-icon-size: 16px;
    background-color: #1a1a1a;
    color: #606060;
}
.loop-btn:hover {
    background-color: #2a2a2a;
}
.loop-active {
    color: #ffffff;
}
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

// ── Loop helpers ──────────────────────────────────────────────────────────────

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
    btns:    Rc<Vec<Button>>,
    pics:    Rc<Vec<gtk::Image>>,
    labels:  Rc<Vec<Label>>,
    last_fp: Rc<RefCell<String>>,
}

#[derive(Clone)]
struct PlaybackWidgets {
    title:     Label,
    artist:    Label,
    album:     Label,
    status:    Label,
    quality:   Label,
    pos:       Label,
    dur:       Label,
    seek:      Scale,
    btn_play:  Button,
    mute_btn:  Button,
    shuffle:   Button,
    repeat:    Button,
    artwork:   gtk::Picture,
    art_stack: gtk::Stack,
    input_icon: gtk::Image,
}

// ── Device UI updates ─────────────────────────────────────────────────────────

/// Reset the UI to the "Connecting…" state.  Called when `device-changed`
/// fires with no device info loaded yet.
fn reset_device_ui(
    pw: &PlaybackWidgets,
    sw: &SourceWidgets,
    ow: &OutputWidgets,
    pp: &PresetWidgets,
    dev_info: &Label,
) {
    pw.title.set_label("Connecting…");
    pw.artist.set_label("");
    pw.album.set_label("");
    pw.status.set_label("");
    pw.quality.set_visible(false);
    pw.artwork.set_paintable(None::<&gtk::gdk::Paintable>);
    pw.art_stack.set_visible_child_name("artwork");
    dev_info.set_label("");

    for btn in pp.btns.iter() { btn.set_visible(false); }
    for lbl in pp.labels.iter() { lbl.set_label(""); }
    for pic in pp.pics.iter() {
        pic.set_paintable(None::<&gtk::gdk::Paintable>);
        pic.set_icon_name(Some("audio-x-generic-symbolic"));
    }

    *sw.updating.borrow_mut() = true;
    sw.dropdown.set_model(Some(&StringList::new(&["—"])));
    sw.dropdown.set_sensitive(false);
    *sw.updating.borrow_mut() = false;
    *sw.ids.borrow_mut()      = Vec::new();
    *sw.enabled.borrow_mut()  = Vec::new();

    *ow.updating.borrow_mut() = true;
    ow.dropdown.set_model(Some(&StringList::new(&["—"])));
    ow.dropdown.set_sensitive(false);
    ow.section.set_visible(false);
    *ow.modes.borrow_mut()       = Vec::new();
    *ow.canon_names.borrow_mut() = Vec::new();
    *ow.updating.borrow_mut()    = false;
}

/// Populate the source dropdown from `DeviceState`'s cached input list.
fn populate_source(ds: &DeviceState, sw: &SourceWidgets, icons: &Rc<icons::IconSet>) {
    let in_enable = ds.audio_inputs();
    let info = match ds.device_info() { Some(i) => i, None => return };
    let caps = match ds.capabilities() { Some(c) => c, None => return };
    let renames = ds.mode_renames();

    let (ids, enabled_flags): (Vec<String>, Vec<bool>) = if !in_enable.is_empty() {
        in_enable.iter().map(|e| (e.mode.clone(), e.is_enabled())).unzip()
    } else {
        let ids = capabilities::detect_inputs(caps.device_id, info.plm_support_value())
            .into_iter().map(|s| s.to_string()).collect::<Vec<_>>();
        let flags = vec![true; ids.len()];
        (ids, flags)
    };

    if ids.is_empty() {
        *sw.updating.borrow_mut() = true;
        sw.dropdown.set_model(Some(&StringList::new(&["—"])));
        sw.dropdown.set_sensitive(false);
        *sw.updating.borrow_mut() = false;
        *sw.ids.borrow_mut()     = Vec::new();
        *sw.enabled.borrow_mut() = Vec::new();
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
    *sw.ids.borrow_mut()     = ids;
    *sw.enabled.borrow_mut() = enabled_flags;
    *sw.updating.borrow_mut() = true;
    sw.dropdown.set_model(Some(&StringList::new(&label_refs)));
    sw.dropdown.set_selected(0);
    sw.dropdown.set_sensitive(true);
    *sw.updating.borrow_mut() = false;

    let _ = icons; // captured for factory, used via Rc in setup closure
}

/// Populate the output dropdown from `DeviceState`'s cached device info.
fn populate_output(ds: &DeviceState, ow: &OutputWidgets) {
    let caps = match ds.capabilities() { Some(c) => c, None => return };
    let output_names = capabilities::detect_outputs(caps.device_id);
    if output_names.is_empty() {
        *ow.updating.borrow_mut() = true;
        ow.dropdown.set_model(Some(&StringList::new(&["—"])));
        ow.dropdown.set_sensitive(false);
        ow.section.set_visible(false);
        *ow.modes.borrow_mut()       = Vec::new();
        *ow.canon_names.borrow_mut() = Vec::new();
        *ow.updating.borrow_mut()    = false;
        return;
    }

    let out_labels: Vec<&str> = output_names.iter()
        .map(|&n| capabilities::output_display_name(n))
        .collect();
    let modes: Vec<u32> = output_names.iter()
        .map(|&n| capabilities::output_canon_to_mode(n).unwrap_or(0))
        .collect();

    *ow.modes.borrow_mut()       = modes;
    *ow.canon_names.borrow_mut() = output_names.to_vec();
    *ow.updating.borrow_mut()    = true;
    ow.dropdown.set_model(Some(&StringList::new(&out_labels)));
    ow.dropdown.set_sensitive(true);
    ow.section.set_visible(true);

    if let Some(os) = ds.output_status() {
        if let Ok(hw) = os.hardware.parse::<u32>() {
            let hw_canon = capabilities::canon_mode_output_name(hw);
            let names = ow.canon_names.borrow();
            if let Some(pos) = names.iter().position(|&n| n == hw_canon) {
                ow.dropdown.set_selected(pos as u32);
            }
        }
    }
    *ow.updating.borrow_mut() = false;
}

/// Apply fully-loaded device info to the UI (labels + dropdowns).
fn apply_device_info(
    ds:      &DeviceState,
    sw:      &SourceWidgets,
    ow:      &OutputWidgets,
    pp:      &PresetWidgets,
    dev_info: &Label,
    icons:   &Rc<icons::IconSet>,
) {
    let info = match ds.device_info() { Some(i) => i, None => return };
    let caps = match ds.capabilities() { Some(c) => c, None => return };

    dev_info.set_label(&format!(
        "{} · {} · FW {}",
        caps.vendor.display_name(), caps.model, info.firmware,
    ));

    populate_source(ds, sw, icons);
    populate_output(ds, ow);

    // Reset preset fingerprint so the preset refresh picks up fresh data.
    *pp.last_fp.borrow_mut() = String::new();
}

// ── Playback UI update ────────────────────────────────────────────────────────

#[derive(Clone)]
struct PlaybackUiState {
    is_playing:   Rc<RefCell<bool>>,
    mute_on:      Rc<RefCell<bool>>,
    shuffle_on:   Rc<RefCell<bool>>,
    repeat_state: Rc<RefCell<u32>>,
    updating_vol: Rc<RefCell<bool>>,
}

fn update_playback_ui(ds: &DeviceState, pw: &PlaybackWidgets, ui: &PlaybackUiState) {
    if let Some(st) = ds.player_status() {
        let playing = st.status == "play";
        *ui.is_playing.borrow_mut() = playing;
        pw.btn_play.set_icon_name(if playing {
            "media-playback-pause-symbolic"
        } else {
            "media-playback-start-symbolic"
        });

        pw.status.set_label(&format_status(&st.status, &st.mode, &st.vendor));

        if let Ok(v) = st.vol.parse::<f64>() {
            *ui.updating_vol.borrow_mut() = true;
            pw.seek.set_value(0.0); // keep seek in sync via separate scale
            let _ = v; // vol handled below via vol_scale capture
            *ui.updating_vol.borrow_mut() = false;
        }

        let muted = st.mute == "1";
        if *ui.mute_on.borrow() != muted {
            *ui.mute_on.borrow_mut() = muted;
            pw.mute_btn.set_icon_name(if muted {
                "audio-volume-muted-symbolic"
            } else {
                "audio-volume-high-symbolic"
            });
        }

        let (dev_shuf, dev_rep) = decode_loop_mode(&st.loop_mode);
        if *ui.shuffle_on.borrow() != dev_shuf {
            *ui.shuffle_on.borrow_mut() = dev_shuf;
            if dev_shuf { pw.shuffle.add_css_class("loop-active"); }
            else         { pw.shuffle.remove_css_class("loop-active"); }
            pw.shuffle.set_tooltip_text(Some(
                if dev_shuf { "Shuffle: On" } else { "Shuffle: Off" }
            ));
        }
        if *ui.repeat_state.borrow() != dev_rep {
            *ui.repeat_state.borrow_mut() = dev_rep;
            let icons = ["media-playlist-repeat-symbolic",
                         "media-playlist-repeat-symbolic",
                         "media-playlist-repeat-song-symbolic"];
            let tips  = ["Repeat: Off", "Repeat: All", "Repeat: One"];
            pw.repeat.set_icon_name(icons[dev_rep as usize]);
            pw.repeat.set_tooltip_text(Some(tips[dev_rep as usize]));
            if dev_rep == 0 { pw.repeat.remove_css_class("loop-active"); }
            else             { pw.repeat.add_css_class("loop-active"); }
        }

        let cur_s = st.curpos.parse::<u64>().unwrap_or(0) / 1000;
        let tot_s = st.totlen.parse::<u64>().unwrap_or(0) / 1000;
        pw.pos.set_label(&format!("{}:{:02}", cur_s / 60, cur_s % 60));
        pw.dur.set_label(&format!("{}:{:02}", tot_s / 60, tot_s % 60));
    }

    if let Some(m) = ds.metadata() {
        let title = if is_unknown(&m.title) { String::new() } else { m.title.clone() };
        pw.title.set_label(if title.is_empty() { "—" } else { &title });
        pw.artist.set_label(if is_unknown(&m.artist) { "" } else { &m.artist });
        pw.album.set_label(if is_unknown(&m.album)  { "" } else { &m.album });

        match format_quality(&m.bit_rate, &m.sample_rate, &m.bit_depth) {
            Some(q) => { pw.quality.set_label(&q); pw.quality.set_visible(true); }
            None    => pw.quality.set_visible(false),
        }
    }

    // Artwork: show if loaded, else keep icon.
    if let Some(bytes) = ds.art_bytes() {
        let gbytes = glib::Bytes::from(&bytes);
        if let Ok(tex) = gtk::gdk::Texture::from_bytes(&gbytes) {
            pw.artwork.set_paintable(Some(&tex));
            pw.art_stack.set_visible_child_name("artwork");
        }
    }
}

// ── Input display update ──────────────────────────────────────────────────────

fn update_input_display(
    ds:    &DeviceState,
    pw:    &PlaybackWidgets,
    sw:    &SourceWidgets,
    icons: &Rc<icons::IconSet>,
) {
    let mode = ds.current_mode();
    let source_id = capabilities::mode_to_input_source(&mode);
    pw.input_icon.set_paintable(Some(icons.source_paintable(source_id)));

    // Sync the source dropdown.
    let sv = sw.ids.borrow();
    if let Some(idx) = sv.iter().position(|s| s == source_id) {
        *sw.updating.borrow_mut() = true;
        sw.dropdown.set_selected(idx as u32);
        *sw.updating.borrow_mut() = false;
    }

    // Show artwork if present, otherwise source icon.
    if ds.art_bytes().is_some() {
        pw.art_stack.set_visible_child_name("artwork");
    } else {
        pw.artwork.set_paintable(None::<&gtk::gdk::Paintable>);
        pw.art_stack.set_visible_child_name("icon");
    }
}

// ── Output display update ─────────────────────────────────────────────────────

fn update_output_display(ds: &DeviceState, ow: &OutputWidgets) {
    let Some(os) = ds.output_status() else { return };
    let Ok(hw) = os.hardware.parse::<u32>() else { return };
    let hw_canon = capabilities::canon_mode_output_name(hw);
    let names = ow.canon_names.borrow();
    if let Some(idx) = names.iter().position(|&n| n == hw_canon) {
        *ow.updating.borrow_mut() = true;
        ow.dropdown.set_selected(idx as u32);
        *ow.updating.borrow_mut() = false;
    }
}

// ── Device popover helpers ────────────────────────────────────────────────────

fn show_manual_ip_dialog(
    window:  &adw::ApplicationWindow,
    ds:      &DeviceState,
    dev_btn: &gtk::MenuButton,
    saved_ip: &Rc<RefCell<String>>,
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
        @strong ds, @strong entry, @strong saved_ip, @strong dev_btn
        => move |_dlg, resp| {
            if resp == "connect" {
                let ip = entry.text().to_string();
                if !ip.is_empty() {
                    *saved_ip.borrow_mut() = ip.clone();
                    dev_btn.set_label(&format!("Manual: {ip}"));
                    ds.set_device(&ip, TlsMode::HttpsWiiM);
                }
            }
        }
    ));
    dialog.present(Some(window));
}

fn build_device_popover(
    devs:     &[discovery::DiscoveredDevice],
    ds:       &DeviceState,
    dev_btn:  &gtk::MenuButton,
    window:   &adw::ApplicationWindow,
    saved_ip: &Rc<RefCell<String>>,
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
            let label = format!("{} ({})", d.name, d.ip);
            let ip = d.ip.clone();
            let tls_mode = d.tls_mode;
            let btn = Button::builder().label(&label).css_classes(["flat"]).build();
            btn.connect_clicked(clone!(
                @strong ds, @strong dev_btn, @strong label
                => move |_| {
                    dev_btn.set_label(&label);
                    dev_btn.popdown();
                    ds.set_device(&ip, tls_mode);
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
        @strong ds, @strong dev_btn, @strong window, @strong saved_ip
        => move |_| {
            dev_btn.popdown();
            show_manual_ip_dialog(&window, &ds, &dev_btn, &saved_ip);
        }
    ));
    vbox.append(&manual_btn);

    let popover = gtk::Popover::new();
    popover.set_child(Some(&vbox));
    popover
}

// ── Preset loader ─────────────────────────────────────────────────────────────

async fn load_presets(
    ds:      &DeviceState,
    pp:      &PresetWidgets,
    icons:   &Rc<icons::IconSet>,
) {
    let client = match ds.client() { Some(c) => c, None => return };
    let rt     = ds.rt();
    let (preset_tx, preset_rx) =
        async_channel::unbounded::<(usize, String, String, Vec<u8>)>();
    let (fp_tx, fp_rx) = async_channel::bounded::<String>(1);

    rt.spawn(async move {
        let (presets_result, routines) =
            tokio::join!(client.get_presets(), client.get_all_routines());
        let Ok(presets) = presets_result else { return };
        let preset_total = presets.preset_num as usize;

        let mut routine_map: std::collections::HashMap<usize, crate::api::Routine> =
            std::collections::HashMap::new();
        for r in routines {
            let slot = r.index as usize + 1;
            if slot >= 1 && slot <= 12 { routine_map.insert(slot, r); }
        }

        let mut seen = std::collections::HashSet::new();
        for p in &presets.preset_list {
            let slot = p.number as usize;
            if slot >= 1 && slot <= 12 { seen.insert(slot); }
        }

        let mut all_slots: std::collections::BTreeSet<usize> =
            std::collections::BTreeSet::new();
        for &slot in &seen { all_slots.insert(slot); }
        for n in 1..=preset_total.min(12) { all_slots.insert(n); }
        for &slot in routine_map.keys() { all_slots.insert(slot); }

        let fp: String = {
            let mut parts: Vec<String> = presets.preset_list.iter()
                .map(|p| format!("{}:{}:{}", p.number, p.name, p.picurl))
                .collect();
            for &n in &all_slots {
                if !seen.contains(&n) {
                    if let Some(r) = routine_map.get(&n) {
                        parts.push(format!("r{}:{}", n, r.name));
                    } else {
                        parts.push(format!("{n}:input-switch"));
                    }
                }
            }
            parts.sort();
            parts.join("|")
        };
        let _ = fp_tx.send(fp).await;

        for p in &presets.preset_list {
            let slot = p.number as usize;
            if slot >= 1 && slot <= 12 {
                let bytes = if !p.picurl.is_empty() {
                    client.fetch_bytes(&p.picurl).await.unwrap_or_default()
                } else {
                    Vec::new()
                };
                let _ = preset_tx
                    .send((slot - 1, p.name.clone(), p.source.clone(), bytes))
                    .await;
            }
        }

        for &n in &all_slots {
            if !seen.contains(&n) {
                if let Some(r) = routine_map.get(&n) {
                    let tag = if let Some(id) = r.audio_input() {
                        format!("input:{id}")
                    } else if let Some(oid) = r.audio_output() {
                        format!("output:{oid}")
                    } else {
                        "other".to_string()
                    };
                    let _ = preset_tx
                        .send((n - 1, r.name.clone(), tag, Vec::new()))
                        .await;
                } else {
                    let _ = preset_tx
                        .send((n - 1, String::new(), "input-switch".to_string(), Vec::new()))
                        .await;
                }
            }
        }
    });

    let new_fp = fp_rx.recv().await.unwrap_or_default();
    if new_fp == *pp.last_fp.borrow() {
        while preset_rx.try_recv().is_ok() {}
        return;
    }
    *pp.last_fp.borrow_mut() = new_fp;

    for btn in pp.btns.iter() { btn.set_visible(false); }
    for lbl in pp.labels.iter() { lbl.set_label(""); }
    for pic in pp.pics.iter() {
        pic.set_paintable(None::<&gtk::gdk::Paintable>);
    }

    while let Ok((idx, name, source, bytes)) = preset_rx.recv().await {
        let is_input_switch = source == "input-switch";
        let input_source_id = source.strip_prefix("input:");
        let output_mode_id  = source.strip_prefix("output:");
        let is_other_rtn    = source == "other";
        let is_routine = input_source_id.is_some() || output_mode_id.is_some() || is_other_rtn;

        if let Some(btn) = pp.btns.get(idx) {
            btn.set_visible(true);
            let tip = if is_input_switch {
                format!("Preset {} — Input selection", idx + 1)
            } else if is_routine {
                format!("Preset {} — {name}", idx + 1)
            } else {
                format!("{name} ({source})")
            };
            btn.set_tooltip_text(Some(&tip));
        }
        if let Some(lbl) = pp.labels.get(idx) {
            lbl.set_label(if is_input_switch { "Input" } else { &name });
        }
        if let Some(pic) = pp.pics.get(idx) {
            if !bytes.is_empty() {
                let gbytes = glib::Bytes::from(&bytes);
                if let Ok(tex) = gtk::gdk::Texture::from_bytes(&gbytes) {
                    pic.set_paintable(Some(&tex));
                }
            } else if let Some(id) = input_source_id {
                pic.set_paintable(Some(icons.source_paintable(id)));
            } else if let Some(oid) = output_mode_id {
                let canon = capabilities::canon_routine_output_name(oid);
                pic.set_paintable(Some(icons.output_paintable(canon)));
            } else {
                pic.set_paintable(Some(icons.source_paintable(&source)));
            }
        }
    }
}

// ── Main UI ───────────────────────────────────────────────────────────────────

pub fn build_ui(app: &adw::Application) {
    let provider = CssProvider::new();
    provider.load_from_string(CSS);
    gtk::style_context_add_provider_for_display(
        &gtk::gdk::Display::default().unwrap(),
        &provider,
        gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
    );

    let icons = Rc::new(icons::IconSet::load());
    let cfg   = Config::load();

    let rt = std::sync::Arc::new(
        tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("tokio runtime"),
    );
    let ds = DeviceState::new(rt);
    if !cfg.last_ip.is_empty() {
        ds.set_device(&cfg.last_ip, TlsMode::HttpsWiiM);
    }
    ds.start_polling();

    // ── Header ───────────────────────────────────────────────────────────────
    let header = adw::HeaderBar::new();

    let sidebar_btn = gtk::ToggleButton::builder()
        .icon_name("sidebar-show-symbolic")
        .active(cfg.panel_visible)
        .tooltip_text("Toggle presets panel")
        .build();
    sidebar_btn.add_css_class("sidebar-toggle");
    header.pack_start(&sidebar_btn);

    let dev_btn = gtk::MenuButton::builder().label("Scanning…").build();
    header.pack_start(&dev_btn);

    let app_menu = gio::Menu::new();
    app_menu.append(Some("About RustyWiiM"), Some("win.about"));
    let app_menu_btn = gtk::MenuButton::builder()
        .icon_name("open-menu-symbolic")
        .menu_model(&app_menu)
        .tooltip_text("Menu")
        .build();
    header.pack_end(&app_menu_btn);

    // ── Left panel: presets ───────────────────────────────────────────────────
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

    let mut preset_btns: Vec<Button>       = Vec::new();
    let mut preset_pics: Vec<gtk::Image>   = Vec::new();
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

    let preset_btns   = Rc::new(preset_btns);
    let preset_pics   = Rc::new(preset_pics);
    let preset_labels = Rc::new(preset_labels);
    let last_preset_fp: Rc<RefCell<String>> = Rc::new(RefCell::new(String::new()));

    let presets_scroll = gtk::ScrolledWindow::builder()
        .child(&presets_box)
        .hscrollbar_policy(gtk::PolicyType::Never)
        .vexpand(true)
        .build();

    // ── Left panel: input / output selectors ──────────────────────────────────
    let sw = SourceWidgets {
        dropdown: gtk::DropDown::from_strings(&["—"]),
        ids:      Rc::new(RefCell::new(Vec::new())),
        enabled:  Rc::new(RefCell::new(Vec::new())),
        updating: Rc::new(RefCell::new(false)),
    };
    sw.dropdown.add_css_class("panel-dropdown");
    sw.dropdown.set_sensitive(false);

    {
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
    }

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

    {
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
    }

    ow.section.append(
        &Label::builder()
            .label("OUTPUT").css_classes(["section-label"]).halign(Align::Start).build(),
    );
    ow.section.append(&ow.dropdown);

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
    left_pane.append(&presets_scroll);
    left_pane.append(&io_box);

    // ── Right pane: now playing ───────────────────────────────────────────────
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
        btn_play: Button::builder()
            .icon_name("media-playback-start-symbolic")
            .css_classes(["play-btn", "circular"]).build(),
        mute_btn: Button::builder()
            .icon_name("audio-volume-high-symbolic")
            .css_classes(["transport-btn", "circular"]).tooltip_text("Mute").build(),
        shuffle:  Button::builder()
            .icon_name("media-playlist-shuffle-symbolic")
            .css_classes(["loop-btn", "circular"]).tooltip_text("Shuffle: Off").build(),
        repeat:   Button::builder()
            .icon_name("media-playlist-repeat-symbolic")
            .css_classes(["loop-btn", "circular"]).tooltip_text("Repeat: Off").build(),
    };

    pw.art_stack.add_named(&pw.artwork, Some("artwork"));
    pw.art_stack.add_named(&pw.input_icon, Some("icon"));
    pw.seek.set_hexpand(true);
    pw.seek.set_draw_value(false);
    pw.seek.add_css_class("seek-scale");
    pw.seek.set_round_digits(0);

    let btn_prev = Button::builder()
        .icon_name("media-skip-backward-symbolic")
        .css_classes(["transport-btn", "circular"]).build();
    let btn_next = Button::builder()
        .icon_name("media-skip-forward-symbolic")
        .css_classes(["transport-btn", "circular"]).build();

    let transport = GtkBox::builder()
        .orientation(Orientation::Horizontal).spacing(12).halign(Align::Center).build();
    transport.prepend(&pw.shuffle);
    transport.append(&btn_prev);
    transport.append(&pw.btn_play);
    transport.append(&btn_next);
    transport.append(&pw.repeat);

    let vol_scale = Scale::with_range(Orientation::Horizontal, 0.0, 100.0, 1.0);
    vol_scale.set_hexpand(true);
    vol_scale.set_draw_value(false);
    vol_scale.add_css_class("vol-scale");
    vol_scale.set_increments(5.0, 20.0);

    let vol_row = GtkBox::builder().orientation(Orientation::Horizontal).spacing(6).build();
    vol_row.append(&pw.mute_btn);
    vol_row.append(&vol_scale);

    let seek_row = GtkBox::builder().orientation(Orientation::Horizontal).spacing(8).build();
    seek_row.append(&pw.pos);
    seek_row.append(&pw.seek);
    seek_row.append(&pw.dur);

    let right_pane = GtkBox::builder()
        .orientation(Orientation::Vertical).spacing(8).hexpand(true)
        .margin_top(8).margin_bottom(8).margin_start(12).margin_end(16)
        .build();
    right_pane.append(&pw.art_stack);
    right_pane.append(&pw.title);
    right_pane.append(&pw.artist);
    right_pane.append(&pw.album);
    right_pane.append(&pw.status);
    right_pane.append(&pw.quality);
    right_pane.append(&seek_row);
    right_pane.append(&transport);
    right_pane.append(&vol_row);

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

    let panel_width = if cfg.paned_position > 0 { cfg.paned_position } else { 200 };
    paned.set_position(panel_width);
    left_pane.set_visible(cfg.panel_visible);

    let dev_info_label = Label::builder()
        .css_classes(["device-info"]).halign(Align::Center)
        .margin_top(4).margin_bottom(4).build();

    let outer = GtkBox::new(Orientation::Vertical, 0);
    outer.append(&paned);
    outer.append(&gtk::Separator::new(Orientation::Horizontal));
    outer.append(&dev_info_label);

    let toolbar = adw::ToolbarView::new();
    toolbar.add_top_bar(&header);
    toolbar.set_content(Some(&outer));

    let win_w = if cfg.window_width  > 0 { cfg.window_width  } else { 680 };
    let win_h = if cfg.window_height > 0 { cfg.window_height } else { 640 };
    let window = adw::ApplicationWindow::builder()
        .application(app).title("RustyWiiM").content(&toolbar)
        .default_width(win_w).default_height(win_h)
        .build();
    if cfg.window_maximized { window.maximize(); }

    // ── Shared UI state ───────────────────────────────────────────────────────
    let ui_state = PlaybackUiState {
        is_playing:   Rc::new(RefCell::new(false)),
        mute_on:      Rc::new(RefCell::new(false)),
        shuffle_on:   Rc::new(RefCell::new(false)),
        repeat_state: Rc::new(RefCell::new(0u32)),
        updating_vol: Rc::new(RefCell::new(false)),
    };

    let pp = PresetWidgets {
        btns:    preset_btns,
        pics:    preset_pics,
        labels:  preset_labels,
        last_fp: last_preset_fp,
    };

    // ── DeviceState signal connections ────────────────────────────────────────
    ds.connect_device_changed({
        let sw = sw.clone();
        let ow = ow.clone();
        let pp = pp.clone();
        let pw = pw.clone();
        let dev_info_label = dev_info_label.clone();
        let icons = icons.clone();
        let ds2 = ds.clone();
        move |ds| {
            if ds.device_info().is_none() {
                reset_device_ui(&pw, &sw, &ow, &pp, &dev_info_label);
            } else {
                apply_device_info(ds, &sw, &ow, &pp, &dev_info_label, &icons);
                // Trigger preset load via DeviceState's capabilities.
                let has_presets = ds.capabilities()
                    .map_or(true, |c| c.supports_presets);
                if has_presets {
                    let ds3  = ds2.clone();
                    let pp2  = pp.clone();
                    let icn2 = icons.clone();
                    glib::spawn_future_local(async move {
                        load_presets(&ds3, &pp2, &icn2).await;
                    });
                }
            }
        }
    });

    ds.connect_playback_changed({
        let pw       = pw.clone();
        let ui_state = PlaybackUiState {
            is_playing:   ui_state.is_playing.clone(),
            mute_on:      ui_state.mute_on.clone(),
            shuffle_on:   ui_state.shuffle_on.clone(),
            repeat_state: ui_state.repeat_state.clone(),
            updating_vol: ui_state.updating_vol.clone(),
        };
        let vol_scale = vol_scale.clone();
        move |ds| {
            // Volume needs direct access to vol_scale, handle separately.
            if let Some(st) = ds.player_status() {
                if let Ok(v) = st.vol.parse::<f64>() {
                    *ui_state.updating_vol.borrow_mut() = true;
                    vol_scale.set_value(v);
                    *ui_state.updating_vol.borrow_mut() = false;
                }
                let cur_s = st.curpos.parse::<u64>().unwrap_or(0) / 1000;
                let tot_s = st.totlen.parse::<u64>().unwrap_or(0) / 1000;
                if tot_s > 0 {
                    pw.seek.set_range(0.0, tot_s as f64);
                    pw.seek.set_value(cur_s as f64);
                }
            }
            update_playback_ui(ds, &pw, &ui_state);
        }
    });

    ds.connect_input_changed({
        let pw    = pw.clone();
        let sw    = sw.clone();
        let icons = icons.clone();
        move |ds| {
            update_input_display(ds, &pw, &sw, &icons);
        }
    });

    ds.connect_output_changed({
        let ow = ow.clone();
        move |ds| {
            update_output_display(ds, &ow);
        }
    });

    // ── Sidebar toggle ────────────────────────────────────────────────────────
    let saved_panel_width  = Rc::new(RefCell::new(panel_width));
    let panel_collapsing   = Rc::new(RefCell::new(false));
    let settle_timer: Rc<RefCell<Option<glib::SourceId>>> = Rc::new(RefCell::new(None));
    let paned_btn_held     = Rc::new(RefCell::new(false));
    const SNAP_PX: i32 = 30;

    paned.connect_position_notify(clone!(
        @strong left_pane, @strong panel_collapsing,
        @strong sidebar_btn, @strong paned,
        @strong saved_panel_width, @strong settle_timer, @strong paned_btn_held
        => move |p| {
            if *panel_collapsing.borrow() { return; }
            let pos = p.position();
            if pos >= SNAP_PX {
                if !left_pane.is_visible() {
                    *panel_collapsing.borrow_mut() = true;
                    left_pane.set_visible(true);
                    *panel_collapsing.borrow_mut() = false;
                }
            } else if left_pane.is_visible() {
                *panel_collapsing.borrow_mut() = true;
                left_pane.set_visible(false);
                *panel_collapsing.borrow_mut() = false;
            }
            if let Some(id) = settle_timer.borrow_mut().take() { id.remove(); }
            let btn2   = sidebar_btn.clone();
            let pane2  = left_pane.clone();
            let paned2 = paned.clone();
            let width2 = saved_panel_width.clone();
            let coll2  = panel_collapsing.clone();
            let timer2 = settle_timer.clone();
            let held2  = paned_btn_held.clone();
            let id = glib::timeout_add_local_once(
                std::time::Duration::from_millis(50),
                move || {
                    *timer2.borrow_mut() = None;
                    let btn_held = *held2.borrow();
                    *held2.borrow_mut() = false;
                    let shown = pane2.is_visible();
                    if btn2.is_active() != shown {
                        *coll2.borrow_mut() = true;
                        btn2.set_active(shown);
                        *coll2.borrow_mut() = false;
                    }
                    if shown && !btn_held {
                        let pos = paned2.position();
                        if pos >= SNAP_PX { *width2.borrow_mut() = pos; }
                    }
                },
            );
            *settle_timer.borrow_mut() = Some(id);
        }
    ));

    {
        let drag_ctrl = gtk::EventControllerLegacy::new();
        drag_ctrl.connect_event(clone!(
            @strong sidebar_btn, @strong left_pane, @strong paned,
            @strong saved_panel_width, @strong panel_collapsing,
            @strong settle_timer, @strong paned_btn_held
            => move |_, event| {
                match event.event_type() {
                    gtk::gdk::EventType::ButtonPress => {
                        *paned_btn_held.borrow_mut() = true;
                    }
                    gtk::gdk::EventType::ButtonRelease => {
                        *paned_btn_held.borrow_mut() = false;
                        if let Some(id) = settle_timer.borrow_mut().take() { id.remove(); }
                        let shown = left_pane.is_visible();
                        if sidebar_btn.is_active() != shown {
                            *panel_collapsing.borrow_mut() = true;
                            sidebar_btn.set_active(shown);
                            *panel_collapsing.borrow_mut() = false;
                        }
                        if shown {
                            let pos = paned.position();
                            if pos >= SNAP_PX { *saved_panel_width.borrow_mut() = pos; }
                        }
                    }
                    _ => {}
                }
                glib::Propagation::Proceed
            }
        ));
        paned.add_controller(drag_ctrl);
    }

    sidebar_btn.connect_toggled(clone!(
        @strong paned, @strong left_pane,
        @strong saved_panel_width, @strong panel_collapsing, @strong settle_timer
        => move |btn| {
            if *panel_collapsing.borrow() { return; }
            if let Some(id) = settle_timer.borrow_mut().take() { id.remove(); }
            if btn.is_active() {
                *panel_collapsing.borrow_mut() = true;
                left_pane.set_visible(true);
                let w = *saved_panel_width.borrow();
                paned.set_position(w);
                *panel_collapsing.borrow_mut() = false;
            } else {
                *panel_collapsing.borrow_mut() = true;
                left_pane.set_visible(false);
                *panel_collapsing.borrow_mut() = false;
            }
        }
    ));

    // ── SSDP discovery ────────────────────────────────────────────────────────
    {
        let (tx, rx) = async_channel::bounded::<Vec<discovery::DiscoveredDevice>>(1);
        ds.rt().spawn(async move {
            let devs = discovery::discover(std::time::Duration::from_secs(4)).await;
            let _ = tx.send(devs).await;
        });

        let saved_ip = Rc::new(RefCell::new(cfg.last_ip.clone()));
        glib::spawn_future_local(clone!(
            @strong ds, @strong dev_btn, @strong window, @strong saved_ip
            => async move {
                if let Ok(devs) = rx.recv().await {
                    let popover = build_device_popover(
                        &devs, &ds, &dev_btn, &window, &saved_ip,
                    );
                    dev_btn.set_popover(Some(&popover));

                    let saved = saved_ip.borrow().clone();
                    if !saved.is_empty() {
                        if let Some(d) = devs.iter().find(|d| d.ip == saved) {
                            dev_btn.set_label(&format!("{} ({})", d.name, d.ip));
                        } else {
                            dev_btn.set_label(&format!("Manual: {saved}"));
                        }
                    } else if !devs.is_empty() {
                        let d = &devs[0];
                        let label = format!("{} ({})", d.name, d.ip);
                        dev_btn.set_label(&label);
                        ds.set_device(&d.ip, d.tls_mode);
                    } else {
                        dev_btn.set_label("No device");
                    }
                }
            }
        ));
    }

    // ── Periodic preset refresh ───────────────────────────────────────────────
    glib::spawn_future_local(clone!(
        @strong ds, @strong pp, @strong icons
        => async move {
            loop {
                glib::timeout_future_seconds(10).await;
                if ds.client().is_none() { continue; }
                if !ds.capabilities().map_or(true, |c| c.supports_presets) { continue; }
                load_presets(&ds, &pp, &icons).await;
            }
        }
    ));

    // ── Transport / control signal handlers ───────────────────────────────────
    pw.btn_play.connect_clicked(clone!(
        @strong ds, @strong ui_state
        => move |_| {
            let playing = *ui_state.is_playing.borrow();
            if let Some(c) = ds.client() {
                ds.rt().spawn(async move {
                    if playing { let _ = c.pause().await; } else { let _ = c.play().await; }
                });
            }
        }
    ));

    btn_prev.connect_clicked(clone!(@strong ds => move |_| {
        if let Some(c) = ds.client() { ds.rt().spawn(async move { let _ = c.prev().await; }); }
    }));

    btn_next.connect_clicked(clone!(@strong ds => move |_| {
        if let Some(c) = ds.client() { ds.rt().spawn(async move { let _ = c.next().await; }); }
    }));

    pw.shuffle.connect_clicked(clone!(
        @strong ds, @strong ui_state, @strong pw
        => move |_| {
            let new_val = !*ui_state.shuffle_on.borrow();
            *ui_state.shuffle_on.borrow_mut() = new_val;
            if new_val { pw.shuffle.add_css_class("loop-active"); }
            else        { pw.shuffle.remove_css_class("loop-active"); }
            pw.shuffle.set_tooltip_text(Some(if new_val { "Shuffle: On" } else { "Shuffle: Off" }));
            let mode = loop_api_mode(new_val, *ui_state.repeat_state.borrow());
            if let Some(c) = ds.client() {
                ds.rt().spawn(async move { let _ = c.set_loop_mode(mode).await; });
            }
        }
    ));

    pw.repeat.connect_clicked(clone!(
        @strong ds, @strong ui_state, @strong pw
        => move |_| {
            let next = (*ui_state.repeat_state.borrow() + 1) % 3;
            *ui_state.repeat_state.borrow_mut() = next;
            let ico = ["media-playlist-repeat-symbolic",
                       "media-playlist-repeat-symbolic",
                       "media-playlist-repeat-song-symbolic"];
            let tip = ["Repeat: Off", "Repeat: All", "Repeat: One"];
            pw.repeat.set_icon_name(ico[next as usize]);
            pw.repeat.set_tooltip_text(Some(tip[next as usize]));
            if next == 0 { pw.repeat.remove_css_class("loop-active"); }
            else          { pw.repeat.add_css_class("loop-active"); }
            let mode = loop_api_mode(*ui_state.shuffle_on.borrow(), next);
            if let Some(c) = ds.client() {
                ds.rt().spawn(async move { let _ = c.set_loop_mode(mode).await; });
            }
        }
    ));

    pw.mute_btn.connect_clicked(clone!(
        @strong ds, @strong ui_state, @strong pw
        => move |_| {
            let new_muted = !*ui_state.mute_on.borrow();
            *ui_state.mute_on.borrow_mut() = new_muted;
            pw.mute_btn.set_icon_name(if new_muted {
                "audio-volume-muted-symbolic"
            } else {
                "audio-volume-high-symbolic"
            });
            if let Some(c) = ds.client() {
                ds.rt().spawn(async move { let _ = c.set_mute(new_muted).await; });
            }
        }
    ));

    vol_scale.connect_value_changed(clone!(@strong ds, @strong ui_state => move |scale| {
        if *ui_state.updating_vol.borrow() { return; }
        let vol = scale.value() as u32;
        if let Some(c) = ds.client() {
            ds.rt().spawn(async move { let _ = c.set_volume(vol).await; });
        }
    }));

    pw.seek.connect_change_value(clone!(@strong ds => move |_, _, value| {
        if let Some(c) = ds.client() {
            ds.rt().spawn(async move { let _ = c.seek(value as u32).await; });
        }
        glib::Propagation::Proceed
    }));

    sw.dropdown.connect_selected_notify(clone!(@strong ds, @strong sw => move |dd| {
        if *sw.updating.borrow() { return; }
        let idx = dd.selected() as usize;
        let ids = sw.ids.borrow();
        if let Some(src) = ids.get(idx).cloned() {
            if let Some(c) = ds.client() {
                ds.rt().spawn(async move { let _ = c.switch_input(&src).await; });
            }
        }
    }));

    ow.dropdown.connect_selected_notify(clone!(@strong ds, @strong ow => move |dd| {
        if *ow.updating.borrow() { return; }
        let idx = dd.selected() as usize;
        let modes = ow.modes.borrow();
        if let Some(&mode) = modes.get(idx) {
            if let Some(c) = ds.client() {
                ds.rt().spawn(async move { let _ = c.set_audio_output(mode).await; });
            }
        }
    }));

    for (i, btn) in pp.btns.iter().enumerate() {
        let num = (i + 1) as u32;
        btn.connect_clicked(clone!(@strong ds => move |_| {
            if let Some(c) = ds.client() {
                ds.rt().spawn(async move { let _ = c.play_preset(num).await; });
            }
        }));
    }

    // ── Window actions ────────────────────────────────────────────────────────
    let quit_action = gio::SimpleAction::new("quit", None);
    quit_action.connect_activate(clone!(@strong window => move |_, _| { window.close(); }));
    window.add_action(&quit_action);
    app.set_accels_for_action("win.quit", &["<Ctrl>Q"]);

    let about_action = gio::SimpleAction::new("about", None);
    about_action.connect_activate(clone!(@strong window => move |_, _| {
        adw::AboutDialog::builder()
            .application_name("RustyWiiM")
            .application_icon("audio-x-generic")
            .version(env!("CARGO_PKG_VERSION"))
            .developer_name("Benjamin Herrenschmidt")
            .copyright("© 2026 Benjamin Herrenschmidt")
            .license_type(gtk::License::MitX11)
            .website("https://github.com/ozbenh/rustywiim")
            .build()
            .present(Some(&window));
    }));
    window.add_action(&about_action);

    // ── Save window state ─────────────────────────────────────────────────────
    window.connect_close_request(clone!(
        @strong paned, @strong saved_panel_width, @strong sidebar_btn, @strong settle_timer
        => move |win| {
            if let Some(id) = settle_timer.borrow_mut().take() { id.remove(); }
            let mut cfg = Config::load();
            cfg.window_maximized = win.is_maximized();
            if !win.is_maximized() {
                cfg.window_width  = win.width();
                cfg.window_height = win.height();
            }
            cfg.panel_visible    = sidebar_btn.is_active();
            cfg.paned_position   = *saved_panel_width.borrow();
            cfg.save();
            glib::Propagation::Proceed
        }
    ));

    window.present();
}
