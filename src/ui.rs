#![allow(deprecated)] // glib clone! old-style @strong syntax, still works in glib 0.20

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use adw::prelude::*;
use glib::clone;
use gtk::gio;
use gtk::{Align, Box as GtkBox, Button, CssProvider, Label, Orientation, Scale, StringList};

use crate::api::WiimClient;
use crate::capabilities;
use crate::config::Config;
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
    max-width: 36px;
    max-height: 36px;
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
    max-width: 44px;
    max-height: 44px;
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
    max-width: 36px;
    max-height: 36px;
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

// ── Helpers ───────────────────────────────────────────────────────────────────

fn is_unknown(s: &str) -> bool {
    s.is_empty() || s.eq_ignore_ascii_case("unknown") || s.eq_ignore_ascii_case("unknow")
}

fn vendor_display(vendor: &str) -> &'static str {
    let v: String = vendor.to_lowercase().chars().filter(|c| !c.is_whitespace()).collect();
    match v.as_str() {
        "newtunein" | "tunein" => "TuneIn",
        "iheartradio" | "iheart" => "iHeartRadio",
        "spotify" => "Spotify",
        "tidal" => "TIDAL",
        "amazon" | "amazonmusic" => "Amazon Music",
        "deezer" => "Deezer",
        "qobuz" => "Qobuz",
        "pandora" => "Pandora",
        "napster" => "Napster",
        "radioparadise" => "Radio Paradise",
        "vtuner" => "vTuner",
        "linkplayradio" => "Radio",
        "custompushurl" => "URL",
        "cast" => "Chromecast",
        _ => "",
    }
}

fn mode_source(mode: &str) -> &'static str {
    match mode {
        "0" => "Idle",
        "1" => "AirPlay",
        "2" => "DLNA",
        "5" => "Chromecast",
        "10" | "20" => "WiFi",
        "11" | "42" | "51" => "USB",
        "31" => "Spotify",
        "32" => "TIDAL Connect",
        "34" => "Lyrion",
        "36" => "Qobuz",
        "40" | "60" => "Line-In",
        "41" => "Bluetooth",
        "43" => "Optical",
        "44" => "RCA",
        "49" => "HDMI",
        "54" => "Phono",
        "99" => "Follower",
        _ => "",
    }
}

fn format_status(status: &str, mode: &str, vendor: &str) -> String {
    let state = match status {
        "play" => "▶ Playing",
        "pause" => "⏸ Paused",
        "stop" => "⏹ Stopped",
        "loading" => "⏳ Loading",
        other => other,
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
        s => format!(" · {s}"),
    };
    format!("{state}{suffix}")
}

fn format_quality(bit_rate: &str, sample_rate: &str, bit_depth: &str) -> Option<String> {
    let br = bit_rate.trim();
    let sr = sample_rate.trim();
    let bd = bit_depth.trim();
    let has_br = !br.is_empty() && br != "0";
    let has_sr = !sr.is_empty() && sr != "0";
    if !has_br && !has_sr {
        return None;
    }
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

// ── State ─────────────────────────────────────────────────────────────────────

struct AppState {
    client: Option<WiimClient>,
    capabilities: Option<capabilities::DeviceCapabilities>,
    updating_volume: bool,
    updating_source: bool,
    updating_output: bool,
    current_art_url: String,
    current_mode: String,
}

// ── Device connection ─────────────────────────────────────────────────────────

fn connect_device(
    state: &Rc<RefCell<AppState>>,
    ip: &str,
    dev_tx: &async_channel::Sender<()>,
) {
    state.borrow_mut().client = Some(WiimClient::new(ip));
    let mut cfg = Config::load();
    cfg.last_ip = ip.to_string();
    cfg.save();
    let _ = dev_tx.send_blocking(());
}

fn show_manual_ip_dialog(
    window: &adw::ApplicationWindow,
    state: &Rc<RefCell<AppState>>,
    dev_btn: &gtk::MenuButton,
    saved_ip: &Rc<RefCell<String>>,
    dev_tx: &async_channel::Sender<()>,
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

    dialog.connect_response(
        None,
        clone!(
            @strong state, @strong entry, @strong saved_ip,
            @strong dev_btn, @strong dev_tx
            => move |_dlg, resp| {
                if resp == "connect" {
                    let ip = entry.text().to_string();
                    if !ip.is_empty() {
                        *saved_ip.borrow_mut() = ip.clone();
                        dev_btn.set_label(&format!("Manual: {ip}"));
                        connect_device(&state, &ip, &dev_tx);
                    }
                }
            }
        ),
    );
    dialog.present(Some(window));
}

fn build_device_popover(
    devs: &[discovery::DiscoveredDevice],
    state: &Rc<RefCell<AppState>>,
    dev_btn: &gtk::MenuButton,
    window: &adw::ApplicationWindow,
    saved_ip: &Rc<RefCell<String>>,
    dev_tx: &async_channel::Sender<()>,
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
            let btn = Button::builder().label(&label).css_classes(["flat"]).build();
            btn.connect_clicked(clone!(
                @strong state, @strong dev_btn, @strong dev_tx, @strong label
                => move |_| {
                    dev_btn.set_label(&label);
                    dev_btn.popdown();
                    connect_device(&state, &ip, &dev_tx);
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
        @strong state, @strong dev_btn, @strong window, @strong saved_ip, @strong dev_tx
        => move |_| {
            dev_btn.popdown();
            show_manual_ip_dialog(&window, &state, &dev_btn, &saved_ip, &dev_tx);
        }
    ));
    vbox.append(&manual_btn);

    let popover = gtk::Popover::new();
    popover.set_child(Some(&vbox));
    popover
}

// ── Loop / shuffle helpers ────────────────────────────────────────────────────

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

// ── Preset loader ─────────────────────────────────────────────────────────────
// Fetches the preset list from the device, detects input-switch slots that the
// device omits from the response, and updates the preset buttons.  No-ops if
// the list fingerprint matches the last displayed one.

async fn load_presets(
    state: &Rc<RefCell<AppState>>,
    rt: &Arc<tokio::runtime::Runtime>,
    preset_btns: &Rc<Vec<Button>>,
    preset_pics: &Rc<Vec<gtk::Image>>,
    preset_labels: &Rc<Vec<Label>>,
    last_fp: &Rc<RefCell<String>>,
    icons: &Rc<icons::IconSet>,
) {
    // (slot_index_0based, display_name, source_tag, artwork_bytes)
    // source_tag conventions:
    //   "<source-id>"    — regular music preset; bytes may contain artwork JPEG/PNG
    //   "input-switch"   — legacy input-switch preset (no specific source known)
    //   "input:<id>"     — input-selection routine (source ID, e.g. "optical")
    //   "output"         — audio output-selection routine
    //   "other"          — routine with no recognised input/output action
    let (preset_tx, preset_rx) =
        async_channel::unbounded::<(usize, String, String, Vec<u8>)>();
    let (fp_tx, fp_rx) = async_channel::bounded::<String>(1);
    {
        let s = state.borrow();
        if let Some(ref c) = s.client {
            let c = c.clone();
            rt.spawn(async move {
                // Fetch both presets and routines concurrently.
                let (presets_result, routines) =
                    tokio::join!(c.get_presets(), c.get_all_routines());

                let Ok(presets) = presets_result else { return };

                let preset_total = presets.preset_num as usize;

                // Build a map of 1-based slot → routine.
                // getAllRoutines uses 0-based indices, so add 1.
                let mut routine_map: std::collections::HashMap<usize, crate::api::Routine> =
                    std::collections::HashMap::new();
                for r in routines {
                    let slot = r.index as usize + 1; // 0-based → 1-based
                    if slot >= 1 && slot <= 12 { routine_map.insert(slot, r); }
                }

                // Track which 1-based slots have regular presets.
                let mut seen = std::collections::HashSet::new();
                for p in &presets.preset_list {
                    let slot = p.number as usize;
                    if slot >= 1 && slot <= 12 { seen.insert(slot); }
                }

                // All slots we need to display: regular presets + preset_num range +
                // every routine slot (routines may live outside the preset_num range).
                let mut all_slots: std::collections::BTreeSet<usize> = std::collections::BTreeSet::new();
                for &slot in &seen { all_slots.insert(slot); }
                for n in 1..=preset_total.min(12) { all_slots.insert(n); }
                for &slot in routine_map.keys() { all_slots.insert(slot); }

                // Fingerprint — order-independent, covers both presets and routines.
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

                // Send regular presets with artwork.
                for p in &presets.preset_list {
                    let slot = p.number as usize;
                    if slot >= 1 && slot <= 12 {
                        let bytes = if !p.picurl.is_empty() {
                            c.fetch_bytes(&p.picurl).await.unwrap_or_default()
                        } else {
                            Vec::new()
                        };
                        let _ = preset_tx.send((slot - 1, p.name.clone(), p.source.clone(), bytes)).await;
                    }
                }

                // Send routines or input-switch sentinels for unfilled slots.
                for &n in &all_slots {
                    if !seen.contains(&n) {
                        if let Some(r) = routine_map.get(&n) {
                            // Classify: input takes priority over output.
                            // Icon lookup happens on the GTK thread via `icons`.
                            let tag = if let Some(id) = r.audio_input() {
                                format!("input:{id}")
                            } else if let Some(oid) = r.audio_output() {
                                format!("output:{oid}")
                            } else {
                                "other".to_string()
                            };
                            let _ = preset_tx.send((n - 1, r.name.clone(), tag, Vec::new())).await;
                        } else {
                            let _ = preset_tx.send((n - 1, String::new(), "input-switch".to_string(), Vec::new())).await;
                        }
                    }
                }
            });
        }
    }

    // Check fingerprint first; if unchanged, drain and ignore.
    let new_fp = fp_rx.recv().await.unwrap_or_default();
    if new_fp == *last_fp.borrow() {
        while preset_rx.try_recv().is_ok() {}
        return;
    }
    *last_fp.borrow_mut() = new_fp;

    // Reset visible state before applying new data.
    for btn in preset_btns.iter() { btn.set_visible(false); }
    for lbl in preset_labels.iter() { lbl.set_label(""); }
    for pic in preset_pics.iter() {
        pic.set_paintable(None::<&gtk::gdk::Paintable>);
    }

    while let Ok((idx, name, source, bytes)) = preset_rx.recv().await {
        let is_input_switch = source == "input-switch";
        let input_source_id = source.strip_prefix("input:");
        let output_mode_id  = source.strip_prefix("output:");
        let is_other_rtn    = source == "other";
        let is_routine      = input_source_id.is_some() || output_mode_id.is_some() || is_other_rtn;

        if let Some(btn) = preset_btns.get(idx) {
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
        if let Some(lbl) = preset_labels.get(idx) {
            lbl.set_label(if is_input_switch { "Input" } else { &name });
        }
        if let Some(pic) = preset_pics.get(idx) {
            if !bytes.is_empty() {
                // Regular preset — create texture from fetched artwork bytes.
                let gbytes = glib::Bytes::from(&bytes);
                if let Ok(texture) = gtk::gdk::Texture::from_bytes(&gbytes) {
                    pic.set_paintable(Some(&texture));
                }
            } else if let Some(id) = input_source_id {
                // Input-selection routine; source_paintable handles the fallback.
                pic.set_paintable(Some(icons.source_paintable(id)));
            } else if let Some(oid) = output_mode_id {
                // Translate raw API string → standard name, then look up icon.
                let std = capabilities::canon_routine_output_name(oid);
                pic.set_paintable(Some(icons.output_paintable(std)));
            } else {
                // "input-switch", "other", or regular preset without artwork.
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

    let cfg = Config::load();

    let state = Rc::new(RefCell::new(AppState {
        client: None,
        capabilities: None,
        updating_volume: false,
        updating_source: false,
        updating_output: false,
        current_art_url: String::new(),
        current_mode: String::new(),
    }));

    let rt = Arc::new(
        tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("tokio runtime"),
    );

    // Fingerprint of the last displayed preset list, used to detect changes.
    let last_preset_fp: Rc<RefCell<String>> = Rc::new(RefCell::new(String::new()));

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

    // ── App menu (About, …) ───────────────────────────────────────────────────
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
        .orientation(Orientation::Vertical)
        .spacing(2)
        .margin_top(8)
        .margin_bottom(4)
        .margin_start(8)
        .margin_end(8)
        .build();
    presets_box.append(
        &Label::builder()
            .label("PRESETS")
            .css_classes(["section-label"])
            .halign(Align::Start)
            .margin_bottom(4)
            .build(),
    );

    // 12 preset slots, hidden until data arrives
    let mut preset_btns: Vec<Button> = Vec::new();
    let mut preset_pics: Vec<gtk::Image> = Vec::new();
    let mut preset_labels: Vec<Label> = Vec::new();

    for i in 1..=12u32 {
        let badge = Label::builder()
            .label(&i.to_string())
            .css_classes(["preset-badge"])
            .halign(Align::Center)
            .valign(Align::Center)
            .build();

        let pic = gtk::Image::builder()
            .pixel_size(40)
            .icon_name("audio-x-generic-symbolic")
            .build();
        pic.add_css_class("preset-art");
        pic.set_overflow(gtk::Overflow::Hidden);

        let lbl = Label::builder()
            .label("")
            .css_classes(["preset-name"])
            .ellipsize(gtk::pango::EllipsizeMode::End)
            .halign(Align::Start)
            .hexpand(true)
            .width_chars(0)
            .build();

        let tile = GtkBox::builder()
            .orientation(Orientation::Horizontal)
            .spacing(6)
            .css_classes(["preset-tile"])
            .overflow(gtk::Overflow::Hidden)
            .build();
        tile.append(&badge);
        tile.append(&pic);
        tile.append(&lbl);

        let btn = Button::builder()
            .child(&tile)
            .css_classes(["flat"])
            .build();
        btn.set_tooltip_text(Some(&format!("Preset {i}")));
        btn.set_visible(false);
        presets_box.append(&btn);

        preset_btns.push(btn);
        preset_pics.push(pic);
        preset_labels.push(lbl);
    }

    let preset_btns = Rc::new(preset_btns);
    let preset_pics = Rc::new(preset_pics);
    let preset_labels = Rc::new(preset_labels);

    let presets_scroll = gtk::ScrolledWindow::builder()
        .child(&presets_box)
        .hscrollbar_policy(gtk::PolicyType::Never)
        .vexpand(true)
        .build();

    // ── Left panel: input / output selectors ──────────────────────────────────
    let source_vec:     Rc<RefCell<Vec<String>>> = Rc::new(RefCell::new(Vec::new()));
    let source_enabled: Rc<RefCell<Vec<bool>>>   = Rc::new(RefCell::new(Vec::new()));
    let output_modes:        Rc<RefCell<Vec<u32>>>          = Rc::new(RefCell::new(Vec::new()));
    let output_canon_names:  Rc<RefCell<Vec<&'static str>>> = Rc::new(RefCell::new(Vec::new()));

    let source_dropdown = gtk::DropDown::from_strings(&["—"]);
    source_dropdown.add_css_class("panel-dropdown");
    source_dropdown.set_sensitive(false);

    // Custom factory: icon + label, with disabled inputs greyed-out.
    {
        let factory = gtk::SignalListItemFactory::new();
        factory.connect_setup(|_, obj| {
            let item = obj.downcast_ref::<gtk::ListItem>().unwrap();
            let hbox = GtkBox::builder()
                .orientation(Orientation::Horizontal)
                .spacing(6)
                .build();
            hbox.append(&gtk::Image::builder().pixel_size(16).build());
            hbox.append(&Label::builder().halign(Align::Start).build());
            item.set_child(Some(&hbox));
        });
        factory.connect_bind(clone!(
            @strong source_enabled, @strong source_vec, @strong icons
            => move |_, obj| {
                let item = obj.downcast_ref::<gtk::ListItem>().unwrap();
                let pos  = item.position() as usize;
                if let Some(hbox) = item.child().and_downcast::<GtkBox>() {
                    let enabled  = source_enabled.borrow().get(pos).copied().unwrap_or(true);
                    let src_id   = source_vec.borrow();
                    let id       = src_id.get(pos).map(String::as_str).unwrap_or("");
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
        source_dropdown.set_factory(Some(&factory));
    }

    let output_section = GtkBox::builder()
        .orientation(Orientation::Vertical)
        .spacing(4)
        .visible(false)
        .build();
    let output_dropdown = gtk::DropDown::from_strings(&["—"]);
    output_dropdown.add_css_class("panel-dropdown");
    output_dropdown.set_sensitive(false);

    // Factory: each output entry shows a small icon alongside the label.
    {
        let factory = gtk::SignalListItemFactory::new();
        factory.connect_setup(|_, obj| {
            let item = obj.downcast_ref::<gtk::ListItem>().unwrap();
            let hbox = GtkBox::builder()
                .orientation(Orientation::Horizontal)
                .spacing(6)
                .build();
            hbox.append(&gtk::Image::builder().pixel_size(16).build());
            hbox.append(&Label::builder().halign(Align::Start).build());
            item.set_child(Some(&hbox));
        });
        factory.connect_bind(clone!(@strong output_canon_names, @strong icons => move |_, obj| {
            let item = obj.downcast_ref::<gtk::ListItem>().unwrap();
            let pos  = item.position() as usize;
            if let Some(hbox) = item.child().and_downcast::<GtkBox>() {
                let names = output_canon_names.borrow();
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
        output_dropdown.set_factory(Some(&factory));
    }

    output_section.append(
        &Label::builder()
            .label("OUTPUT")
            .css_classes(["section-label"])
            .halign(Align::Start)
            .build(),
    );
    output_section.append(&output_dropdown);

    let io_box = GtkBox::builder()
        .orientation(Orientation::Vertical)
        .spacing(4)
        .margin_top(4)
        .margin_bottom(8)
        .margin_start(8)
        .margin_end(8)
        .build();
    io_box.append(&gtk::Separator::new(Orientation::Horizontal));
    let input_label = Label::builder()
        .label("INPUT")
        .css_classes(["section-label"])
        .halign(Align::Start)
        .margin_top(6)
        .build();
    io_box.append(&input_label);
    io_box.append(&source_dropdown);
    io_box.append(&output_section);

    let left_pane = GtkBox::builder()
        .orientation(Orientation::Vertical)
        .build();
    left_pane.append(&presets_scroll);
    left_pane.append(&io_box);

    // ── Right pane: now playing ───────────────────────────────────────────────
    let artwork = gtk::Picture::new();
    artwork.set_content_fit(gtk::ContentFit::Contain);
    artwork.set_can_shrink(true);
    artwork.set_halign(Align::Center);
    artwork.set_vexpand(true);

    // Fixed-size icon shown for physical inputs instead of artwork.
    // pixel_size caps the rendered size; the Image never grows beyond it.
    let input_icon = gtk::Image::builder()
        .pixel_size(128)
        .halign(Align::Center)
        .valign(Align::Center)
        .build();

    let artwork_stack = gtk::Stack::new();
    artwork_stack.set_vexpand(true);
    artwork_stack.set_transition_type(gtk::StackTransitionType::Crossfade);
    artwork_stack.set_transition_duration(200);
    artwork_stack.add_named(&artwork, Some("artwork"));
    artwork_stack.add_named(&input_icon, Some("icon"));

    let title_label = Label::builder()
        .label("Not connected")
        .css_classes(["track-title"])
        .ellipsize(gtk::pango::EllipsizeMode::End)
        .halign(Align::Center)
        .justify(gtk::Justification::Center)
        .build();
    let artist_label = Label::builder()
        .css_classes(["track-artist"])
        .ellipsize(gtk::pango::EllipsizeMode::End)
        .halign(Align::Center)
        .build();
    let album_label = Label::builder()
        .css_classes(["track-album"])
        .ellipsize(gtk::pango::EllipsizeMode::End)
        .halign(Align::Center)
        .build();
    let status_label = Label::builder()
        .css_classes(["status-badge"])
        .halign(Align::Center)
        .build();
    let quality_label = Label::builder()
        .css_classes(["quality-label"])
        .halign(Align::Center)
        .visible(false)
        .build();

    // Seek bar
    let pos_label = Label::builder().label("0:00").css_classes(["dim-label"]).build();
    let dur_label = Label::builder().label("0:00").css_classes(["dim-label"]).build();
    let seek_scale = Scale::with_range(Orientation::Horizontal, 0.0, 100.0, 1.0);
    seek_scale.set_hexpand(true);
    seek_scale.set_draw_value(false);
    seek_scale.add_css_class("seek-scale");
    seek_scale.set_round_digits(0);

    let seek_row = GtkBox::builder()
        .orientation(Orientation::Horizontal)
        .spacing(8)
        .build();
    seek_row.append(&pos_label);
    seek_row.append(&seek_scale);
    seek_row.append(&dur_label);

    // Transport
    let btn_prev = Button::builder()
        .icon_name("media-skip-backward-symbolic")
        .css_classes(["transport-btn", "circular"])
        .build();
    let btn_play = Button::builder()
        .icon_name("media-playback-start-symbolic")
        .css_classes(["play-btn", "circular"])
        .build();
    let btn_next = Button::builder()
        .icon_name("media-skip-forward-symbolic")
        .css_classes(["transport-btn", "circular"])
        .build();

    let shuffle_btn = Button::builder()
        .icon_name("media-playlist-shuffle-symbolic")
        .css_classes(["loop-btn", "circular"])
        .tooltip_text("Shuffle: Off")
        .build();
    let repeat_btn = Button::builder()
        .icon_name("media-playlist-repeat-symbolic")
        .css_classes(["loop-btn", "circular"])
        .tooltip_text("Repeat: Off")
        .build();
    let shuffle_on = Rc::new(RefCell::new(false));
    let repeat_state = Rc::new(RefCell::new(0u32)); // 0=none, 1=all, 2=one

    let transport = GtkBox::builder()
        .orientation(Orientation::Horizontal)
        .spacing(12)
        .halign(Align::Center)
        .build();
    transport.prepend(&shuffle_btn);
    transport.append(&btn_prev);
    transport.append(&btn_play);
    transport.append(&btn_next);
    transport.append(&repeat_btn);

    // Volume
    let vol_scale = Scale::with_range(Orientation::Horizontal, 0.0, 100.0, 1.0);
    vol_scale.set_hexpand(true);
    vol_scale.set_draw_value(false);
    vol_scale.add_css_class("vol-scale");
    vol_scale.set_increments(5.0, 20.0);

    let mute_btn = Button::builder()
        .icon_name("audio-volume-high-symbolic")
        .css_classes(["transport-btn", "circular"])
        .tooltip_text("Mute")
        .build();

    let vol_row = GtkBox::builder()
        .orientation(Orientation::Horizontal)
        .spacing(6)
        .build();
    vol_row.append(&mute_btn);
    vol_row.append(&vol_scale);

    let right_pane = GtkBox::builder()
        .orientation(Orientation::Vertical)
        .spacing(8)
        .hexpand(true)
        .margin_top(8)
        .margin_bottom(8)
        .margin_start(12)
        .margin_end(16)
        .build();
    right_pane.append(&artwork_stack);
    right_pane.append(&title_label);
    right_pane.append(&artist_label);
    right_pane.append(&album_label);
    right_pane.append(&status_label);
    right_pane.append(&quality_label);
    right_pane.append(&seek_row);
    right_pane.append(&transport);
    right_pane.append(&vol_row);

    // ── Paned split ───────────────────────────────────────────────────────────
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
    // Hide the widget rather than setting position=0; this removes the handle entirely.
    left_pane.set_visible(cfg.panel_visible);

    // ── Device info footer ────────────────────────────────────────────────────
    let dev_info_label = Label::builder()
        .css_classes(["device-info"])
        .halign(Align::Center)
        .margin_top(4)
        .margin_bottom(4)
        .build();

    let outer = GtkBox::new(Orientation::Vertical, 0);
    outer.append(&paned);
    outer.append(&gtk::Separator::new(Orientation::Horizontal));
    outer.append(&dev_info_label);

    let toolbar = adw::ToolbarView::new();
    toolbar.add_top_bar(&header);
    toolbar.set_content(Some(&outer));

    let win_w = if cfg.window_width > 0 { cfg.window_width } else { 680 };
    let win_h = if cfg.window_height > 0 { cfg.window_height } else { 640 };

    let window = adw::ApplicationWindow::builder()
        .application(app)
        .title("RustyWiiM")
        .content(&toolbar)
        .default_width(win_w)
        .default_height(win_h)
        .build();

    if cfg.window_maximized {
        window.maximize();
    }

    // ── Init client from saved config ─────────────────────────────────────────
    if !cfg.last_ip.is_empty() {
        state.borrow_mut().client = Some(WiimClient::new(&cfg.last_ip));
    }

    // ── Channels ──────────────────────────────────────────────────────────────
    let (dev_tx, dev_rx) = async_channel::unbounded::<()>();
    let (poll_tx, poll_rx) = async_channel::unbounded::<(
        Option<crate::api::PlayerStatus>,
        Option<crate::api::MetaData>,
        Option<crate::api::AudioOutputStatus>,
    )>();
    let (art_tx, art_rx) = async_channel::unbounded::<Vec<u8>>();

    if state.borrow().client.is_some() {
        let _ = dev_tx.send_blocking(());
    }

    // ── Sidebar toggle ────────────────────────────────────────────────────────
    // saved_panel_width = the "restore-to" width, committed only at drag-end when
    // the panel is still visible.  It is intentionally NOT updated when collapsing
    // (drag or button), so re-expanding always returns to the pre-collapse size.
    let saved_panel_width = Rc::new(RefCell::new(panel_width));
    // Re-entrancy guard for programmatic set_visible / set_active calls.
    let panel_collapsing = Rc::new(RefCell::new(false));
    // Settle timer: fires 50 ms after position stops changing (= mouse released or
    // user paused mid-drag).  This is the reliable width-commit path because
    // ButtonRelease from a GtkPaned handle drag does not bubble to parent controllers.
    let settle_timer: Rc<RefCell<Option<glib::SourceId>>> = Rc::new(RefCell::new(None));
    // Tracks whether the mouse button is currently pressed on the paned, as
    // reported by EventControllerLegacy.  When true, the settle timer skips the
    // width commit (avoids saving an intermediate position if the user pauses
    // mid-drag on systems where ButtonPress events bubble here).
    // On systems where neither ButtonPress nor ButtonRelease arrives, this flag
    // stays false and the settle timer commits width unconditionally — the
    // reliable fallback.
    let paned_btn_held: Rc<RefCell<bool>> = Rc::new(RefCell::new(false));

    const SNAP_PX: i32 = 30;

    // Live preview: show/hide left_pane immediately as position crosses the threshold.
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
            } else {
                // pos < SNAP_PX, including pos == 0 (fast drag skips 1..SNAP_PX-1)
                if left_pane.is_visible() {
                    *panel_collapsing.borrow_mut() = true;
                    left_pane.set_visible(false);
                    *panel_collapsing.borrow_mut() = false;
                }
            }

            if let Some(id) = settle_timer.borrow_mut().take() { id.remove(); }
            let btn2   = sidebar_btn.clone();
            let pane2  = left_pane.clone();
            let paned2 = paned.clone();
            let width2 = saved_panel_width.clone();
            let coll2  = panel_collapsing.clone();
            let timer2 = settle_timer.clone();
            let held2  = paned_btn_held.clone();
            let id = glib::timeout_add_local_once(Duration::from_millis(50), move || {
                *timer2.borrow_mut() = None;
                // Snapshot and reset the held flag.  If ButtonPress fired but Release
                // was missed, this failsafe allows future drags to commit correctly.
                let btn_held = *held2.borrow();
                *held2.borrow_mut() = false;
                let shown = pane2.is_visible();
                if btn2.is_active() != shown {
                    *coll2.borrow_mut() = true;
                    btn2.set_active(shown);
                    *coll2.borrow_mut() = false;
                }
                // Skip width commit while button is known to be held (mid-drag pause).
                // When btn_held is false (either EventControllerLegacy isn't tracking
                // button state, or the button really is released), commit the width.
                if shown && !btn_held {
                    let pos = paned2.position();
                    if pos >= SNAP_PX { *width2.borrow_mut() = pos; }
                }
            });
            *settle_timer.borrow_mut() = Some(id);
        }
    ));

    // Secondary path: track button state and commit immediately on ButtonRelease
    // for systems where these events do bubble through the paned widget.
    // ButtonPress sets paned_btn_held so the settle timer won't save mid-drag.
    // ButtonRelease clears it and commits width directly, cancelling the timer.
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

    // Button click: show/hide panel using the stored restore-width.
    // Does NOT update saved_panel_width — that is only the drag-end's job.
    sidebar_btn.connect_toggled(clone!(
        @strong paned, @strong left_pane,
        @strong saved_panel_width, @strong panel_collapsing, @strong settle_timer
        => move |btn| {
            if *panel_collapsing.borrow() { return; }
            if let Some(id) = settle_timer.borrow_mut().take() { id.remove(); }
            if btn.is_active() {
                *panel_collapsing.borrow_mut() = true;
                left_pane.set_visible(true);
                // Separate binding: set_position fires notify synchronously;
                // holding the Ref across that call would cause a double-borrow panic.
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
        rt.spawn(async move {
            let devs = discovery::discover(Duration::from_secs(4)).await;
            let _ = tx.send(devs).await;
        });

        let saved_ip = Rc::new(RefCell::new(cfg.last_ip.clone()));
        glib::spawn_future_local(clone!(
            @strong state, @strong dev_btn, @strong window,
            @strong saved_ip, @strong dev_tx
            => async move {
                if let Ok(devs) = rx.recv().await {
                    let popover = build_device_popover(
                        &devs, &state, &dev_btn, &window, &saved_ip, &dev_tx,
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
                        connect_device(&state, &d.ip, &dev_tx);
                    } else {
                        dev_btn.set_label("No device");
                    }
                }
            }
        ));
    }

    // ── Polling timer ─────────────────────────────────────────────────────────
    glib::timeout_add_local(
        Duration::from_secs(1),
        clone!(@strong state, @strong rt, @strong poll_tx => move || {
            let s = state.borrow();
            if let Some(ref c) = s.client {
                let c = c.clone();
                let tx = poll_tx.clone();
                rt.spawn(async move {
                    let status = c.get_status().await.ok();
                    let meta = c.get_meta_info().await.ok();
                    let output = c.get_audio_output().await.ok();
                    let _ = tx.send((status, meta, output)).await;
                });
            }
            glib::ControlFlow::Continue
        }),
    );

    // ── Poll result handler ───────────────────────────────────────────────────
    let is_playing = Rc::new(RefCell::new(false));
    let mute_on = Rc::new(RefCell::new(false));

    glib::spawn_future_local(clone!(
        @strong state, @strong rt,
        @strong title_label, @strong artist_label, @strong album_label,
        @strong status_label, @strong quality_label, @strong vol_scale, @strong seek_scale,
        @strong pos_label, @strong dur_label,
        @strong artwork, @strong artwork_stack, @strong input_icon, @strong art_tx,
        @strong btn_play, @strong is_playing,
        @strong mute_btn, @strong mute_on,
        @strong shuffle_btn, @strong shuffle_on,
        @strong repeat_btn, @strong repeat_state,
        @strong source_dropdown, @strong source_vec,
        @strong output_dropdown, @strong output_canon_names,
        @strong icons
        => async move {
            while let Ok((status, meta, output)) = poll_rx.recv().await {
                if let Some(st) = status {
                    let playing = st.status == "play";
                    *is_playing.borrow_mut() = playing;
                    btn_play.set_icon_name(if playing {
                        "media-playback-pause-symbolic"
                    } else {
                        "media-playback-start-symbolic"
                    });

                    status_label.set_label(&format_status(&st.status, &st.mode, &st.vendor));

                    if let Ok(v) = st.vol.parse::<f64>() {
                        state.borrow_mut().updating_volume = true;
                        vol_scale.set_value(v);
                        state.borrow_mut().updating_volume = false;
                    }

                    let muted = st.mute == "1";
                    if *mute_on.borrow() != muted {
                        *mute_on.borrow_mut() = muted;
                        mute_btn.set_icon_name(if muted {
                            "audio-volume-muted-symbolic"
                        } else {
                            "audio-volume-high-symbolic"
                        });
                    }

                    let (dev_shuf, dev_rep) = decode_loop_mode(&st.loop_mode);
                    if *shuffle_on.borrow() != dev_shuf {
                        *shuffle_on.borrow_mut() = dev_shuf;
                        if dev_shuf { shuffle_btn.add_css_class("loop-active"); } else { shuffle_btn.remove_css_class("loop-active"); }
                        shuffle_btn.set_tooltip_text(Some(if dev_shuf { "Shuffle: On" } else { "Shuffle: Off" }));
                    }
                    if *repeat_state.borrow() != dev_rep {
                        *repeat_state.borrow_mut() = dev_rep;
                        let icons = ["media-playlist-repeat-symbolic", "media-playlist-repeat-symbolic", "media-playlist-repeat-song-symbolic"];
                        let tips = ["Repeat: Off", "Repeat: All", "Repeat: One"];
                        repeat_btn.set_icon_name(icons[dev_rep as usize]);
                        repeat_btn.set_tooltip_text(Some(tips[dev_rep as usize]));
                        if dev_rep == 0 { repeat_btn.remove_css_class("loop-active"); } else { repeat_btn.add_css_class("loop-active"); }
                    }

                    let cur_s = st.curpos.parse::<u64>().unwrap_or(0) / 1000;
                    let tot_s = st.totlen.parse::<u64>().unwrap_or(0) / 1000;
                    pos_label.set_label(&format!("{}:{:02}", cur_s / 60, cur_s % 60));
                    dur_label.set_label(&format!("{}:{:02}", tot_s / 60, tot_s % 60));
                    if tot_s > 0 {
                        seek_scale.set_range(0.0, tot_s as f64);
                        seek_scale.set_value(cur_s as f64);
                    }

                    // Sync input dropdown to current mode
                    let current_src = capabilities::mode_to_input_source(&st.mode);
                    let sv = source_vec.borrow();
                    if let Some(idx) = sv.iter().position(|s| s == current_src) {
                        state.borrow_mut().updating_source = true;
                        source_dropdown.set_selected(idx as u32);
                        state.borrow_mut().updating_source = false;
                    }

                    // Switch artwork vs. input icon based on mode.
                    let prev_mode = state.borrow().current_mode.clone();
                    let mode_changed = st.mode != prev_mode;
                    if mode_changed {
                        state.borrow_mut().current_mode = st.mode.clone();
                        state.borrow_mut().current_art_url.clear();
                        artwork.set_paintable(None::<&gtk::gdk::Paintable>);
                    }
                    let source_id = capabilities::mode_to_input_source(&st.mode);
                    input_icon.set_paintable(Some(icons.source_paintable(source_id)));
                    if artwork.paintable().is_some() {
                        artwork_stack.set_visible_child_name("artwork");
                    } else {
                        artwork_stack.set_visible_child_name("icon");
                    }

                    // Sync output dropdown from audio output hardware mode
                    if let Some(ref os) = output {
                        if let Ok(hw) = os.hardware.parse::<u32>() {
                            let hw_canon = capabilities::canon_mode_output_name(hw);
                            let names = output_canon_names.borrow();
                            if let Some(idx) = names.iter().position(|&n| n == hw_canon) {
                                state.borrow_mut().updating_output = true;
                                output_dropdown.set_selected(idx as u32);
                                state.borrow_mut().updating_output = false;
                            }
                        }
                    }
                }

                if let Some(m) = meta {
                    let title = if is_unknown(&m.title) { String::new() } else { m.title.clone() };
                    title_label.set_label(if title.is_empty() { "—" } else { &title });
                    artist_label.set_label(if is_unknown(&m.artist) { "" } else { &m.artist });
                    album_label.set_label(if is_unknown(&m.album) { "" } else { &m.album });

                    match format_quality(&m.bit_rate, &m.sample_rate, &m.bit_depth) {
                        Some(q) => { quality_label.set_label(&q); quality_label.set_visible(true); }
                        None    => quality_label.set_visible(false),
                    }

                    let art_url = m.art_uri().to_string();
                    if !art_url.is_empty() && art_url != state.borrow().current_art_url {
                        state.borrow_mut().current_art_url = art_url.clone();
                        let s = state.borrow();
                        if let Some(ref c) = s.client {
                            let c = c.clone();
                            let tx = art_tx.clone();
                            rt.spawn(async move {
                                if let Ok(bytes) = c.fetch_bytes(&art_url).await {
                                    let _ = tx.send(bytes).await;
                                }
                            });
                        }
                    }
                }
            }
        }
    ));

    // ── Artwork loader ────────────────────────────────────────────────────────
    glib::spawn_future_local(clone!(@strong artwork, @strong artwork_stack => async move {
        while let Ok(bytes) = art_rx.recv().await {
            let gbytes = glib::Bytes::from(&bytes);
            if let Ok(texture) = gtk::gdk::Texture::from_bytes(&gbytes) {
                artwork.set_paintable(Some(&texture));
                artwork_stack.set_visible_child_name("artwork");
            }
        }
    }));

    // ── Device-changed handler ────────────────────────────────────────────────
    glib::spawn_future_local(clone!(
        @strong state, @strong rt,
        @strong title_label, @strong artist_label, @strong album_label,
        @strong status_label, @strong quality_label,
        @strong artwork, @strong artwork_stack, @strong dev_info_label,
        @strong source_dropdown, @strong source_vec, @strong source_enabled,
        @strong output_dropdown, @strong output_modes, @strong output_canon_names, @strong output_section,
        @strong preset_btns, @strong preset_pics, @strong preset_labels,
        @strong last_preset_fp, @strong icons
        => async move {
            while let Ok(()) = dev_rx.recv().await {
                // Reset UI
                title_label.set_label("Connecting…");
                artist_label.set_label("");
                album_label.set_label("");
                status_label.set_label("");
                quality_label.set_visible(false);
                dev_info_label.set_label("");
                artwork.set_paintable(None::<&gtk::gdk::Texture>);
                artwork_stack.set_visible_child_name("artwork");
                {
                    let mut s = state.borrow_mut();
                    s.current_art_url.clear();
                    s.current_mode.clear();
                    s.capabilities = None;
                }

                // Hide all presets
                for btn in preset_btns.iter() { btn.set_visible(false); }
                for lbl in preset_labels.iter() { lbl.set_label(""); }
                for pic in preset_pics.iter() {
                    pic.set_paintable(None::<&gtk::gdk::Paintable>);
                    pic.set_icon_name(Some("audio-x-generic-symbolic"));
                }

                // Reset dropdowns
                state.borrow_mut().updating_source = true;
                source_dropdown.set_model(Some(&StringList::new(&["—"])));
                source_dropdown.set_sensitive(false);
                state.borrow_mut().updating_source = false;
                *source_vec.borrow_mut() = Vec::new();
                *source_enabled.borrow_mut() = Vec::new();

                state.borrow_mut().updating_output = true;
                output_dropdown.set_model(Some(&StringList::new(&["—"])));
                output_dropdown.set_sensitive(false);
                output_section.set_visible(false);
                *output_modes.borrow_mut() = Vec::new();
                *output_canon_names.borrow_mut() = Vec::new();
                state.borrow_mut().updating_output = false;

                // Fetch device info
                type InfoPayload = (
                    crate::api::DeviceInfo,
                    Option<crate::api::AudioOutputStatus>,
                    Vec<crate::api::AudioInputEntry>,
                    std::collections::HashMap<String, String>,
                );
                let (info_tx, info_rx) = async_channel::bounded::<InfoPayload>(1);
                {
                    let s = state.borrow();
                    if let Some(ref c) = s.client {
                        let c = c.clone();
                        rt.spawn(async move {
                            if let Ok(info) = c.get_device_info().await {
                                let output    = c.get_audio_output().await.ok();
                                let in_enable = c.get_audio_input_enable().await;
                                let renames   = c.get_mode_rename().await;
                                let _ = info_tx.send((info, output, in_enable, renames)).await;
                            }
                        });
                    }
                }
                if let Ok((info, output_status, in_enable, renames)) = info_rx.recv().await {
                    let caps = capabilities::DeviceCapabilities::from_device_info(&info);
                    dev_info_label.set_label(&format!(
                        "{} · {} · FW {}",
                        caps.vendor.display_name(), caps.model, info.firmware,
                    ));
                    state.borrow_mut().capabilities = Some(caps.clone());

                    // Populate input dropdown.
                    // Prefer the live API list (getAudioInputEnable); fall back to the
                    // static capability table when the device doesn't support that call.
                    let (ids, enabled_flags): (Vec<String>, Vec<bool>) = if !in_enable.is_empty() {
                        in_enable.iter()
                            .map(|e| (e.mode.clone(), e.is_enabled()))
                            .unzip()
                    } else {
                        let ids = capabilities::detect_inputs(&info.project)
                            .iter().map(|s| s.to_string()).collect::<Vec<_>>();
                        let flags = vec![true; ids.len()];
                        (ids, flags)
                    };
                    let input_labels: Vec<String> = ids.iter().zip(enabled_flags.iter())
                        .map(|(id, _)| {
                            let standard = capabilities::input_display_name(id).to_string();
                            if let Some(user) = renames.get(id.as_str()) {
                                if !user.is_empty() && user != &standard {
                                    return format!("{} ({})", user, standard);
                                }
                            }
                            standard
                        })
                        .collect();
                    let input_label_refs: Vec<&str> = input_labels.iter().map(String::as_str).collect();
                    *source_vec.borrow_mut()     = ids;
                    *source_enabled.borrow_mut() = enabled_flags;
                    state.borrow_mut().updating_source = true;
                    source_dropdown.set_model(Some(&StringList::new(&input_label_refs)));
                    source_dropdown.set_selected(0);
                    source_dropdown.set_sensitive(true);
                    state.borrow_mut().updating_source = false;

                    // Populate output dropdown
                    let output_names = capabilities::detect_outputs(&info.project);
                    if !output_names.is_empty() {
                        let out_labels: Vec<&str> = output_names.iter()
                            .map(|&n| capabilities::output_display_name(n))
                            .collect();
                        let modes: Vec<u32> = output_names.iter()
                            .map(|&n| capabilities::output_canon_to_mode(n).unwrap_or(0))
                            .collect();
                        *output_modes.borrow_mut()       = modes;
                        *output_canon_names.borrow_mut() = output_names;
                        state.borrow_mut().updating_output = true;
                        output_dropdown.set_model(Some(&StringList::new(&out_labels)));
                        output_dropdown.set_sensitive(true);
                        output_section.set_visible(true);
                        if let Some(ref os) = output_status {
                            if let Ok(hw) = os.hardware.parse::<u32>() {
                                let hw_canon = capabilities::canon_mode_output_name(hw);
                                let names = output_canon_names.borrow();
                                if let Some(pos) = names.iter().position(|&n| n == hw_canon) {
                                    output_dropdown.set_selected(pos as u32);
                                }
                            }
                        }
                        state.borrow_mut().updating_output = false;
                    }
                }

                // Fetch presets if the device supports them.
                let has_presets = state.borrow().capabilities
                    .as_ref().map_or(true, |c| c.supports_presets);
                if has_presets {
                    *last_preset_fp.borrow_mut() = String::new();
                    load_presets(&state, &rt,
                                 &preset_btns, &preset_pics, &preset_labels,
                                 &last_preset_fp, &icons).await;
                }
            }
        }
    ));

    // ── Periodic preset refresh ───────────────────────────────────────────────
    glib::spawn_future_local(clone!(
        @strong state, @strong rt,
        @strong preset_btns, @strong preset_pics, @strong preset_labels,
        @strong last_preset_fp, @strong icons
        => async move {
            loop {
                glib::timeout_future_seconds(10).await;
                let s = state.borrow();
                if s.client.is_none() { continue; }
                if !s.capabilities.as_ref().map_or(true, |c| c.supports_presets) { continue; }
                drop(s);
                load_presets(&state, &rt,
                             &preset_btns, &preset_pics, &preset_labels,
                             &last_preset_fp, &icons).await;
            }
        }
    ));

    // ── Signal handlers ───────────────────────────────────────────────────────
    btn_play.connect_clicked(clone!(@strong state, @strong rt, @strong is_playing => move |_| {
        let playing = *is_playing.borrow();
        let s = state.borrow();
        if let Some(ref c) = s.client {
            let c = c.clone();
            rt.spawn(async move {
                if playing { let _ = c.pause().await; } else { let _ = c.play().await; }
            });
        }
    }));
    btn_prev.connect_clicked(clone!(@strong state, @strong rt => move |_| {
        let s = state.borrow();
        if let Some(ref c) = s.client { let c = c.clone(); rt.spawn(async move { let _ = c.prev().await; }); }
    }));
    btn_next.connect_clicked(clone!(@strong state, @strong rt => move |_| {
        let s = state.borrow();
        if let Some(ref c) = s.client { let c = c.clone(); rt.spawn(async move { let _ = c.next().await; }); }
    }));

    shuffle_btn.connect_clicked(clone!(
        @strong state, @strong rt, @strong shuffle_on, @strong repeat_state, @strong shuffle_btn
        => move |_| {
            let new_val = !*shuffle_on.borrow();
            *shuffle_on.borrow_mut() = new_val;
            if new_val { shuffle_btn.add_css_class("loop-active"); } else { shuffle_btn.remove_css_class("loop-active"); }
            shuffle_btn.set_tooltip_text(Some(if new_val { "Shuffle: On" } else { "Shuffle: Off" }));
            let mode = loop_api_mode(new_val, *repeat_state.borrow());
            let s = state.borrow();
            if let Some(ref c) = s.client { let c = c.clone(); rt.spawn(async move { let _ = c.set_loop_mode(mode).await; }); }
        }
    ));

    repeat_btn.connect_clicked(clone!(
        @strong state, @strong rt, @strong shuffle_on, @strong repeat_state, @strong repeat_btn
        => move |_| {
            let next = (*repeat_state.borrow() + 1) % 3;
            *repeat_state.borrow_mut() = next;
            let icons = ["media-playlist-repeat-symbolic", "media-playlist-repeat-symbolic", "media-playlist-repeat-song-symbolic"];
            let tips = ["Repeat: Off", "Repeat: All", "Repeat: One"];
            repeat_btn.set_icon_name(icons[next as usize]);
            repeat_btn.set_tooltip_text(Some(tips[next as usize]));
            if next == 0 { repeat_btn.remove_css_class("loop-active"); } else { repeat_btn.add_css_class("loop-active"); }
            let mode = loop_api_mode(*shuffle_on.borrow(), next);
            let s = state.borrow();
            if let Some(ref c) = s.client { let c = c.clone(); rt.spawn(async move { let _ = c.set_loop_mode(mode).await; }); }
        }
    ));

    mute_btn.connect_clicked(clone!(@strong state, @strong rt, @strong mute_on, @strong mute_btn => move |_| {
        let new_muted = !*mute_on.borrow();
        *mute_on.borrow_mut() = new_muted;
        mute_btn.set_icon_name(if new_muted { "audio-volume-muted-symbolic" } else { "audio-volume-high-symbolic" });
        let s = state.borrow();
        if let Some(ref c) = s.client { let c = c.clone(); rt.spawn(async move { let _ = c.set_mute(new_muted).await; }); }
    }));

    vol_scale.connect_value_changed(clone!(@strong state, @strong rt => move |scale| {
        if state.borrow().updating_volume { return; }
        let vol = scale.value() as u32;
        let s = state.borrow();
        if let Some(ref c) = s.client { let c = c.clone(); rt.spawn(async move { let _ = c.set_volume(vol).await; }); }
    }));

    seek_scale.connect_change_value(clone!(@strong state, @strong rt => move |_, _, value| {
        let s = state.borrow();
        if let Some(ref c) = s.client { let c = c.clone(); rt.spawn(async move { let _ = c.seek(value as u32).await; }); }
        glib::Propagation::Proceed
    }));

    source_dropdown.connect_selected_notify(clone!(@strong state, @strong rt, @strong source_vec => move |dd| {
        if state.borrow().updating_source { return; }
        let idx = dd.selected() as usize;
        let sv = source_vec.borrow();
        if let Some(src) = sv.get(idx) {
            let src = src.clone();
            let s = state.borrow();
            if let Some(ref c) = s.client { let c = c.clone(); rt.spawn(async move { let _ = c.switch_input(&src).await; }); }
        }
    }));

    output_dropdown.connect_selected_notify(clone!(@strong state, @strong rt, @strong output_modes => move |dd| {
        if state.borrow().updating_output { return; }
        let idx = dd.selected() as usize;
        let modes = output_modes.borrow();
        if let Some(&mode) = modes.get(idx) {
            let s = state.borrow();
            if let Some(ref c) = s.client { let c = c.clone(); rt.spawn(async move { let _ = c.set_audio_output(mode).await; }); }
        }
    }));

    for (i, btn) in preset_btns.iter().enumerate() {
        let num = (i + 1) as u32;
        btn.connect_clicked(clone!(@strong state, @strong rt => move |_| {
            let s = state.borrow();
            if let Some(ref c) = s.client { let c = c.clone(); rt.spawn(async move { let _ = c.play_preset(num).await; }); }
        }));
    }

    // ── Quit action / Ctrl+Q ─────────────────────────────────────────────────
    let quit_action = gio::SimpleAction::new("quit", None);
    quit_action.connect_activate(clone!(@strong window => move |_, _| { window.close(); }));
    window.add_action(&quit_action);
    app.set_accels_for_action("win.quit", &["<Ctrl>Q"]);

    // ── About action ─────────────────────────────────────────────────────────
    let about_action = gio::SimpleAction::new("about", None);
    about_action.connect_activate(clone!(@strong window => move |_, _| {
        let dialog = adw::AboutDialog::builder()
            .application_name("RustyWiiM")
            .application_icon("audio-x-generic")
            .version(env!("CARGO_PKG_VERSION"))
            .developer_name("Benjamin Herrenschmidt")
            .copyright("© 2026 Benjamin Herrenschmidt")
            .license_type(gtk::License::MitX11)
            .website("https://github.com/ozbenh/rustywiim")
            .build();
        dialog.present(Some(&window));
    }));
    window.add_action(&about_action);

    // ── Save window state on close ────────────────────────────────────────────
    window.connect_close_request(clone!(
        @strong paned, @strong saved_panel_width, @strong sidebar_btn, @strong settle_timer
        => move |win| {
        if let Some(id) = settle_timer.borrow_mut().take() { id.remove(); }
        let mut cfg = Config::load();
        cfg.window_maximized = win.is_maximized();
        if !win.is_maximized() {
            cfg.window_width = win.width();
            cfg.window_height = win.height();
        }
        cfg.panel_visible = sidebar_btn.is_active();
        // saved_panel_width is the last committed restore-width, independent of
        // whether the panel is currently collapsed.  Save it directly.
        cfg.paned_position = *saved_panel_width.borrow();
        cfg.save();
        glib::Propagation::Proceed
    }));

    window.present();
}
