#![allow(deprecated)] // glib clone! old-style @strong syntax

mod dialogs;
mod icons;
mod scroll_fade_label;
mod playback;
mod settings;
mod widgets;

use dialogs::build_device_popover;
use playback::decode_loop_mode;
use widgets::*;

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;

use adw::prelude::*;
use glib::clone;
use gtk::gio;
use gtk::{Align, Box as GtkBox, CssProvider, Label, Orientation, Scale};

use crate::device::api::TlsMode;
use crate::config::{Config, ThemeMode};
use crate::device::state::{ConnectionState, DeviceState};
use crate::device::discovery::DiscoveryService;

// ── CSS ───────────────────────────────────────────────────────────────────────

const SYSTEM_CSS: &str = include_str!("../css/system.css");
const DARK_CSS: &str   = include_str!("../css/dark.css");

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
pub(crate) fn apply_theme(theme: ThemeMode) {
    apply_color_scheme(theme);
    THEME_PROVIDER.with(|p| {
        if let Some(provider) = p.borrow().as_ref() {
            provider.load_from_string(theme_css(theme));
        }
    });
}

// ── DeviceWindowInner ─────────────────────────────────────────────────────────
// All "content" widget state for one device window, kept together so that every
// GTK signal closure only needs one `Rc::clone(&inner)` capture instead of
// capturing half a dozen independent `Rc<RefCell<...>>` values.

struct DeviceWindowInner {
    ds:             DeviceState,
    discovery:      DiscoveryService,
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
    pub fn new(app: &adw::Application, rt: Arc<tokio::runtime::Runtime>, discovery: DiscoveryService) -> Self {
        let cfg         = Config::load();
        init_css(cfg.theme);

        let icons = Rc::new(icons::IconSet::load());
        let init_dev_cfg = cfg.device(&cfg.last_uuid);

        let ds = DeviceState::new(rt);
        if !cfg.last_ip.is_empty() {
            // Pass the last known SSID so fetch_device_info can abort if the IP
            // was reassigned to a different device.  Discovery will then reconnect
            // to the right device by SSID.  Pass None if no SSID is on record
            // (first run or old config) so we connect unconditionally.
            let expected = if cfg.last_uuid.is_empty() { None } else { Some(cfg.last_uuid.as_str()) };
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
            discovery: discovery.clone(),
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
            applied_window_key: RefCell::new(cfg.last_uuid.clone()),
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
                        playback::schedule_config_save(&i2);
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
                            playback::schedule_config_save(&i);
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
                playback::schedule_config_save(&i);
            }
        });

        // ── SSDP discovery ────────────────────────────────────────────────────────
        {
            let saved_ip = Rc::new(RefCell::new(cfg.last_ip.clone()));
            wire_discovery(
                &inner.discovery.clone(),
                &ds, &dev_btn, &window,
                &saved_ip, &inner,
                cfg.last_uuid.clone(),
            );
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

        let settings_win: Rc<RefCell<Option<settings::SettingsWindow>>> =
            Rc::new(RefCell::new(None));
        let settings_action = gio::SimpleAction::new("settings", None);
        settings_action.connect_activate(clone!(@strong window, @strong ds, @strong settings_win
            => move |_, _| {
                let mut sw = settings_win.borrow_mut();
                if sw.is_none() {
                    *sw = Some(settings::SettingsWindow::new(&ds, &window));
                }
                sw.as_ref().unwrap().present();
            }
        ));
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

/// Connect the `discovery-updated` signal to the device-selection UI.
///
/// On every list change: rebuilds the device popover, updates the button
/// label to reflect the best-match device (UUID preferred over IP), and
/// auto-connects if the DeviceState is still Disconnected.
fn wire_discovery(
    discovery:  &DiscoveryService,
    ds:         &DeviceState,
    dev_btn:    &gtk::MenuButton,
    window:     &adw::ApplicationWindow,
    saved_ip:   &Rc<RefCell<String>>,
    inner:      &Rc<DeviceWindowInner>,
    last_uuid:  String,
) {
    discovery.connect_discovery_updated(clone!(
        @strong ds, @strong dev_btn, @strong window, @strong saved_ip, @strong inner
            => move |svc| {
                let devs = svc.devices();

                // Rebuild the device popover with the current list.
                let popover = build_device_popover(
                    &devs, &ds, &dev_btn, &window, &saved_ip,
                    {
                        let i = Rc::clone(&inner);
                        move |uuid| { i.apply_device_window_state(uuid); }
                    },
                );
                dev_btn.set_popover(Some(&popover));

                let saved = saved_ip.borrow().clone();

                // Prefer UUID match (survives IP changes); fall back to IP.
                let by_uuid = devs.iter().find(|d| {
                    !last_uuid.is_empty() && !d.uuid.is_empty() && d.uuid == last_uuid
                });
                let by_ip = devs.iter().find(|d| !saved.is_empty() && d.ip == saved);
                let best  = by_uuid.or(by_ip);

                // Update the button label.
                match best {
                    Some(d) => dev_btn.set_label(&format!("{} ({})", d.name, d.ip)),
                    None if !saved.is_empty() => dev_btn.set_label(&format!("Manual: {saved}")),
                    None if devs.is_empty()   => dev_btn.set_label("No device"),
                    None => {}
                }

                // Auto-connect only while still Disconnected so user-initiated
                // connections are never overridden.
                if ds.connection_state() == ConnectionState::Disconnected {
                    let target = best.or_else(|| {
                        if saved.is_empty() { devs.first() } else { None }
                    });
                    if let Some(d) = target {
                        dev_btn.set_label(&format!("{} ({})", d.name, d.ip));
                        *saved_ip.borrow_mut() = d.ip.clone();
                        select_device(&ds, &d.ip, &d.uuid, d.tls_mode);
                    }
                }
            }
    ));
}

/// Select a device: save the IP (and UUID when known) to config, then connect.
/// Use this for all user-initiated device changes; the startup reconnect path
/// calls `ds.set_device()` directly because it must not overwrite the config.
pub(super) fn select_device(ds: &DeviceState, ip: &str, uuid: &str, tls: TlsMode) {
    {
        let mut cfg = Config::load();
        cfg.last_ip = ip.to_string();
        if !uuid.is_empty() {
            cfg.last_uuid = uuid.to_string();
        }
        cfg.save();
    }
    ds.set_device(ip, tls, None);
}

// Private helper used within mod.rs new() — also accessible from child modules.
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
