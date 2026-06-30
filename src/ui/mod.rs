#![allow(deprecated)] // glib clone! old-style @strong syntax

pub mod devlist;
mod icons;
pub(crate) mod menu;
mod scroll_fade_label;
mod playback;
pub(crate) mod settings;
mod widgets;

use playback::decode_loop_mode;
use widgets::*;

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use adw::prelude::*;
use glib::clone;
use gtk::gio;
use gtk::{Align, Box as GtkBox, CssProvider, Label, Orientation, Scale};

use crate::device::api::TlsMode;
use crate::config::Config;
use crate::config::ThemeMode;
use crate::device::discovery::DiscoveryService;
use crate::device::manager::DeviceManager;
use crate::device::state::{ConnectionState, DeviceState, DEBUG_STATE};

// ── Shared window actions ─────────────────────────────────────────────────────

/// Register `win.about` and `win.settings` on any ApplicationWindow.
/// Both the device window, discovery window, and mini window share these actions.
/// `ds` is `None` for the discovery window (settings window title has no device name).
pub(crate) fn wire_window_actions(
    window:        &impl glib::object::IsA<gtk::ApplicationWindow>,
    ds:            Option<DeviceState>,
    open_settings: Rc<dyn Fn(Option<DeviceState>)>,
) {
    let window = window.upcast_ref::<gtk::ApplicationWindow>().clone();
    let about_action = gio::SimpleAction::new("about", None);
    let win = window.clone();
    about_action.connect_activate(move |_, _| {
        adw::AboutDialog::builder()
            .application_name("RustyWiiM")
            .application_icon("audio-x-generic")
            .version(concat!(env!("CARGO_PKG_VERSION"), " (", env!("GIT_HASH"), ")"))
            .developer_name("Benjamin Herrenschmidt")
            .copyright("© 2026 Benjamin Herrenschmidt")
            .license_type(gtk::License::MitX11)
            .website("https://github.com/ozbenh/rustywiim")
            .build()
            .present(Some(&win));
    });
    window.add_action(&about_action);

    let settings_action = gio::SimpleAction::new("settings", None);
    settings_action.connect_activate(move |_, _| { open_settings(ds.clone()); });
    window.add_action(&settings_action);
}

// ── DeviceSpec ────────────────────────────────────────────────────────────────

/// Describes a specific device to connect to when creating a new device window.
pub struct DeviceSpec {
    pub ip:       String,
    pub uuid:     String,
    pub tls_mode: TlsMode,
}

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

/// Walk the widget tree rooted at `widget` and call `queue_draw()` on every
/// node.  `queue_draw()` on a container does NOT cascade to children in GTK4 —
/// each widget owns its snapshot cache independently, and only the widgets
/// that are individually marked dirty will be re-snapshot'd on the next frame.
fn queue_draw_recursive(widget: &gtk::Widget) {
    widget.queue_draw();
    let mut child = widget.first_child();
    while let Some(c) = child {
        queue_draw_recursive(&c);
        child = c.next_sibling();
    }
}

/// Switch the active CSS theme at runtime.
pub(crate) fn apply_theme(theme: ThemeMode) {
    apply_color_scheme(theme);

    // Replace the provider object rather than mutating it with load_from_string.
    // GTK can miss detecting a rule *removal* from the same provider object
    // (e.g. `window { background-color }` present in dark.css but absent in
    // system.css), leaving computed style caches stale.
    let display = gtk::gdk::Display::default().unwrap();
    THEME_PROVIDER.with(|p| {
        let mut borrow = p.borrow_mut();
        if let Some(old) = borrow.take() {
            gtk::style_context_remove_provider_for_display(&display, &old);
        }
        let provider = CssProvider::new();
        provider.load_from_string(theme_css(theme));
        gtk::style_context_add_provider_for_display(
            &display, &provider, gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
        );
        *borrow = Some(provider);
    });

    // Mark every widget in every window dirty so the next frame re-snapshot's
    // everything from the updated CSS.  Two passes: immediate + LOW-priority
    // idle (after any async Adwaita colour-scheme work at DEFAULT_IDLE priority).
    for win in gtk::Window::list_toplevels() {
        queue_draw_recursive(&win);
    }
    glib::idle_add_local_full(glib::Priority::LOW, || {
        for win in gtk::Window::list_toplevels() {
            queue_draw_recursive(&win);
        }
        glib::ControlFlow::Break
    });
}

// ── DeviceWindowInner ─────────────────────────────────────────────────────────
// All "content" widget state for one device window, kept together so that every
// GTK signal closure only needs one `Rc::clone(&inner)` capture instead of
// capturing half a dozen independent `Rc<RefCell<...>>` values.

struct DeviceWindowInner {
    ds:               DeviceState,
    show_devices_fn:  Rc<dyn Fn()>,
    sw:             SourceWidgets,
    ow:             OutputWidgets,
    pw:             PlaybackWidgets,
    pp:             PresetWidgets,
    dev_info_label: Label,
    ip_label:       Label,
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
    mini_win:          gtk::ApplicationWindow,
}

// ── DeviceWindow ──────────────────────────────────────────────────────────────

/// One device window.  Owns the GTK window and all content widgets.
#[derive(Clone)]
pub struct DeviceWindow {
    pub window: adw::ApplicationWindow,
    inner:      Rc<DeviceWindowInner>,
}

impl DeviceWindow {
    #[allow(dead_code)]
    pub fn ds(&self) -> &DeviceState { &self.inner.ds }

    /// UUID of the currently connected device, or `None` if not yet connected
    /// or the UUID is empty.
    pub fn uuid(&self) -> Option<String> {
        self.inner.ds.device_info()
            .map(|i| i.uuid)
            .filter(|u| !u.is_empty())
    }

    /// Build a device window connected to a specific device.
    pub fn new_for_device(
        app:            &adw::Application,
        device_manager: DeviceManager,
        show_devices_fn: Rc<dyn Fn()>,
        open_settings:  Rc<dyn Fn(Option<DeviceState>)>,
        spec:           DeviceSpec,
    ) -> Self {
        Self::new_inner(app, device_manager, show_devices_fn, open_settings, Some(spec))
    }

    fn new_inner(
        app:             &adw::Application,
        device_manager:  DeviceManager,
        show_devices_fn: Rc<dyn Fn()>,
        open_settings:   Rc<dyn Fn(Option<DeviceState>)>,
        device_spec:     Option<DeviceSpec>,
    ) -> Self {
        let cfg = Config::load();
        let icons = Rc::new(icons::IconSet::load());

        // Pick the device UUID to use for loading per-device window config.
        // The no-spec path no longer falls back to last_uuid (phased out).
        let cfg_uuid: String = device_spec.as_ref()
            .map(|s| s.uuid.clone())
            .filter(|u| !u.is_empty())
            .unwrap_or_default();

        let init_dev_cfg = cfg.device(&cfg_uuid);

        let ds = match device_spec.as_ref() {
            Some(spec) => device_manager.get(&spec.uuid, &spec.ip, spec.tls_mode),
            None => {
                // No device spec: create a standalone state that isn't wired to
                // any device yet; polling still starts so the UI can be shown.
                let ds = DeviceState::new(device_manager.rt());
                ds.start_polling();
                ds
            }
        };

        let (header, sidebar_btn, mini_btn) = build_header(init_dev_cfg.panel_visible);
        let (pp, presets_scroll) = build_presets_panel();
        let sw = build_source_widgets(&icons);
        let ow = build_output_widgets(&icons);
        let left_pane = build_left_pane(&sw, &ow, &presets_scroll);
        let (pw, vol_scale) = build_playback_widgets();
        let right_pane = build_right_pane(&pw);
        let (mini, mini_win) = build_mini_window(app);

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

        let ip_label = Label::builder()
            .css_classes(["ip-label", "dim-label"])
            .margin_end(6).margin_top(4).margin_bottom(4)
            .visible(false)
            .build();

        let net_icon = gtk::Image::builder()
            .icon_size(gtk::IconSize::Normal)
            .css_classes(["net-icon"])
            .margin_end(8).margin_top(4).margin_bottom(4)
            .visible(false)
            .build();

        let bottom_end = GtkBox::new(Orientation::Horizontal, 0);
        bottom_end.append(&ip_label);
        bottom_end.append(&net_icon);

        let bottom_bar = gtk::CenterBox::new();
        bottom_bar.set_center_widget(Some(&dev_info_label));
        bottom_bar.set_end_widget(Some(&bottom_end));

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
        window.add_css_class("player-window");
        if init_dev_cfg.window_maximized { window.maximize(); }

        // ── Shared UI state ───────────────────────────────────────────────────────
        let ui_state = PlaybackUiState {
            is_playing:   Rc::new(RefCell::new(false)),
            drag_timer:   Rc::new(RefCell::new(None)),
        };

        let inner = Rc::new(DeviceWindowInner {
            ds: ds.clone(),
            show_devices_fn,
            sw,
            ow,
            pw,
            pp,
            dev_info_label,
            ip_label,
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
            applied_window_key: RefCell::new(cfg_uuid.clone()),
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
                playback::schedule_config_save(&i);
            }
        });

        inner.mini.restore_btn.connect_clicked({
            let i = Rc::clone(&inner);
            move |_| {
                i.exit_mini_mode();
                playback::schedule_config_save(&i);
            }
        });

        inner.mini.close_btn.connect_clicked(clone!(@strong window => move |_| {
            gtk::prelude::WidgetExt::realize(&window); // close() is a no-op on an unrealized window
            window.close();
        }));

        {
            let gesture = gtk::GestureClick::builder().button(1).build();
            gesture.connect_pressed({
                let i = Rc::clone(&inner);
                move |_, n_press, _, _| {
                    if n_press >= 2 {
                        i.exit_mini_mode();
                        playback::schedule_config_save(&i);
                    }
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
                playback::schedule_config_save(&i);
                glib::Propagation::Stop
            }
        });

        // ── Window actions ────────────────────────────────────────────────────────
        // Main window: win.close (Ctrl-W), win.devices, win.about, win.settings.
        let close_action = gio::SimpleAction::new("close", None);
        let win_for_close = window.clone();
        close_action.connect_activate(move |_, _| { win_for_close.close(); });
        window.add_action(&close_action);

        let devices_action = gio::SimpleAction::new("devices", None);
        let i_for_dev = Rc::clone(&inner);
        devices_action.connect_activate(move |_, _| { (i_for_dev.show_devices_fn)(); });
        window.add_action(&devices_action);

        wire_window_actions(&window, Some(ds.clone()), Rc::clone(&open_settings));

        // Mini window is a gtk::ApplicationWindow, so app.* actions (Ctrl-Q) work
        // automatically.  Wire win.close and win.devices directly; win.about and
        // win.settings come from wire_window_actions.
        {
            let mini_close = gio::SimpleAction::new("close", None);
            let win = window.clone();
            mini_close.connect_activate(move |_, _| {
                gtk::prelude::WidgetExt::realize(&win);
                win.close();
            });
            mini_win.add_action(&mini_close);

            let mini_devices = gio::SimpleAction::new("devices", None);
            let i = Rc::clone(&inner);
            mini_devices.connect_activate(move |_, _| { (i.show_devices_fn)(); });
            mini_win.add_action(&mini_devices);
        }
        wire_window_actions(&mini_win, Some(ds.clone()), open_settings);

        // ── Save window state ─────────────────────────────────────────────────────
        window.connect_close_request({
            let i = Rc::clone(&inner);
            move |_win| {
                if let Some(id) = i.settle_timer.borrow_mut().take() { id.remove(); }
                if let Some(id) = i.config_save_timer.borrow_mut().take() { id.remove(); }
                i.save_config_now();
                // Mark window as closed so it is not reopened on next launch.
                let uuid = i.ds.device_info()
                    .map(|di| di.uuid)
                    .filter(|u| !u.is_empty())
                    .unwrap_or_else(|| i.applied_window_key.borrow().clone());
                if !uuid.is_empty() {
                    let mut cfg = Config::load();
                    cfg.device_mut(&uuid).window_open = false;
                    cfg.save();
                }
                i.mini_win.destroy();
                glib::Propagation::Proceed
            }
        });

        if init_dev_cfg.mini_mode {
            // Set up mini-mode state without calling enter_mini_mode(), which would
            // try to hide a window not yet shown and call mini_win.present() too early.
            // DeviceWindow::present() (called by the caller) will show the mini window.
            *inner.mini_mode.borrow_mut() = true;
            *inner.mini_toggling.borrow_mut() = true;
            inner.mini_btn.set_active(true);
            *inner.mini_toggling.borrow_mut() = false;
            // Seed pre_mini_size from saved config so exit_mini_mode() can restore
            // the right size even before the main window has ever been realised.
            *inner.pre_mini_size.borrow_mut() = (win_w, win_h);
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

// ── AppState ──────────────────────────────────────────────────────────────────
// Owns all top-level window state.  Every signal-handler closure captures
// either a strong Rc<AppState> or a Weak clone for the close-request handlers.

fn dbg_state(msg: &str) {
    if DEBUG_STATE.load(Ordering::Relaxed) {
        println!("[app] {msg}");
    }
}

pub(crate) struct AppState {
    app:            adw::Application,
    disc_mgr:       devlist::DiscoveryManager,
    device_manager: DeviceManager,
    registry:       RefCell<Vec<DeviceWindow>>,
    settings_reg:   RefCell<Vec<settings::SettingsWindow>>,
    disc_win:       RefCell<Option<devlist::DiscoveryWindow>>,
}

impl AppState {
    // `disc_svc.start()` must run inside `connect_activate` so that
    // `glib::spawn_future_local` has an active main context.
    pub(crate) fn new(app: &adw::Application, rt: Arc<tokio::runtime::Runtime>) -> Rc<Self> {
        let disc_svc = DiscoveryService::new(rt.clone());
        disc_svc.start();
        let disc_mgr = devlist::DiscoveryManager::new(rt.clone(), disc_svc.clone());

        Rc::new(Self {
            app:            app.clone(),
            disc_mgr,
            device_manager: DeviceManager::new(rt),
            registry:       RefCell::new(Vec::new()),
            settings_reg:   RefCell::new(Vec::new()),
            disc_win:       RefCell::new(None),
        })
    }

    /// Open (or re-present) the settings window for `ds`, deduplicating by UUID.
    fn open_settings(self_rc: &Rc<Self>, ds: Option<DeviceState>) {
        let ds_uuid = ds.as_ref()
            .and_then(|d| d.device_info())
            .map(|i| i.uuid.clone())
            .filter(|u| !u.is_empty());
        {
            let reg = self_rc.settings_reg.borrow();
            for sw in reg.iter() {
                if sw.device_uuid() == ds_uuid {
                    dbg_state(&format!("settings: presenting existing for {:?}", ds_uuid));
                    sw.present();
                    return;
                }
            }
        }
        dbg_state(&format!("settings: opening new for {:?}", ds_uuid));
        let s = settings::SettingsWindow::new(ds);
        let win_clone  = s.window_ref().clone();
        let weak_self  = Rc::downgrade(self_rc);
        let close_uuid = ds_uuid.clone();
        s.window_ref().connect_close_request(move |_| {
            dbg_state(&format!("settings: closed for {:?}", close_uuid));
            if let Some(state) = weak_self.upgrade() {
                state.settings_reg.borrow_mut().retain(|w| w.window_ref() != &win_clone);
            }
            glib::Propagation::Proceed
        });
        s.present();
        self_rc.settings_reg.borrow_mut().push(s);
    }

    /// Show (or lazily create) the device-list window.
    fn show_devices(self_rc: &Rc<Self>) {
        let mut dw = self_rc.disc_win.borrow_mut();
        if dw.is_none() {
            dbg_state("device list: creating window");
            let open_device_fn = {
                let state = Rc::clone(self_rc);
                Rc::new(move |entry: &devlist::ManagedEntry| Self::open_device(&state, entry))
                    as Rc<dyn Fn(&devlist::ManagedEntry)>
            };
            let open_settings_fn = {
                let state = Rc::clone(self_rc);
                Rc::new(move |ds| Self::open_settings(&state, ds))
                    as Rc<dyn Fn(Option<DeviceState>)>
            };
            *dw = Some(devlist::DiscoveryWindow::new(
                &self_rc.app,
                &self_rc.disc_mgr,
                open_device_fn,
                open_settings_fn,
            ));
        }
        dbg_state("device list: presenting");
        dw.as_ref().unwrap().present();
    }

    /// Present the existing device window for `entry`, or open a new one.
    fn open_device(self_rc: &Rc<Self>, entry: &devlist::ManagedEntry) {
        {
            let reg = self_rc.registry.borrow();
            for w in reg.iter() {
                if w.uuid().map_or(false, |u| u == entry.uuid) {
                    dbg_state(&format!("device window: presenting existing for {} ({})", entry.name, entry.uuid));
                    w.present();
                    return;
                }
            }
        }
        dbg_state(&format!("device window: opening {} ({}) @ {}", entry.name, entry.uuid, entry.ip));
        if !entry.uuid.is_empty() {
            let mut cfg = Config::load();
            cfg.device_mut(&entry.uuid).window_open = true;
            cfg.save();
        }
        Self::open_device_spec(self_rc, DeviceSpec {
            ip:       entry.ip.clone(),
            uuid:     entry.uuid.clone(),
            tls_mode: entry.tls_mode,
        });
    }

    /// Create a device window for `spec`, register it, and present it.
    fn open_device_spec(self_rc: &Rc<Self>, spec: DeviceSpec) {
        let log_uuid = spec.uuid.clone();
        let log_ip   = spec.ip.clone();
        dbg_state(&format!("device window: creating uuid={log_uuid} @ {log_ip}"));
        let show_fn = {
            let state = Rc::clone(self_rc);
            Rc::new(move || Self::show_devices(&state)) as Rc<dyn Fn()>
        };
        let open_settings_fn = {
            let state = Rc::clone(self_rc);
            Rc::new(move |ds| Self::open_settings(&state, ds)) as Rc<dyn Fn(Option<DeviceState>)>
        };
        let dw = DeviceWindow::new_for_device(
            &self_rc.app,
            self_rc.device_manager.clone(),
            show_fn,
            open_settings_fn,
            spec,
        );
        let gtk_win   = dw.window.clone();
        dw.present();
        self_rc.registry.borrow_mut().push(dw);
        let win_key   = gtk_win.clone();
        let weak_self = Rc::downgrade(self_rc);
        gtk_win.connect_close_request(move |_| {
            dbg_state(&format!("device window: closed uuid={log_uuid}"));
            if let Some(s) = weak_self.upgrade() {
                s.registry.borrow_mut().retain(|w| w.window != win_key);
            }
            glib::Propagation::Proceed
        });
    }

    /// Restore device windows that were open at last exit. Returns count opened.
    fn restore_windows(self_rc: &Rc<Self>) -> usize {
        let cfg = Config::load();
        let mut count = 0;
        for (uuid, dev_cfg) in &cfg.devices {
            if !dev_cfg.window_open { continue; }
            let Some(ref ip) = dev_cfg.last_ip else { continue };
            if ip.is_empty() { continue; }
            dbg_state(&format!("restore: window for uuid={uuid} @ {ip}"));
            Self::open_device_spec(self_rc, DeviceSpec {
                ip:       ip.clone(),
                uuid:     uuid.clone(),
                tls_mode: TlsMode::HttpsWiiM,
            });
            count += 1;
        }
        count
    }

    /// Called once from `app.connect_activate`.
    pub(crate) fn activate(self_rc: &Rc<Self>) {
        self_rc.disc_mgr.start();

        {
            let mut cfg = Config::load();
            if cfg.migrate() { cfg.save(); }
            init_css(cfg.theme);
        }

        // Replace the app.quit action (set up in main.rs) with one that saves
        // every device window's config before quitting.  app.quit() destroys
        // windows via gtk_window_destroy(), which never fires close-request, so
        // save_config_now() would never be called otherwise.  We save here while
        // the windows are still alive and their sizes are still readable.
        {
            let s = Rc::downgrade(self_rc);
            let app = self_rc.app.clone();
            let quit_action = gio::SimpleAction::new("quit", None);
            quit_action.connect_activate(move |_, _| {
                if let Some(s) = s.upgrade() {
                    for dw in s.registry.borrow().iter() {
                        dw.inner.save_config_now();
                    }
                }
                app.quit();
            });
            self_rc.app.add_action(&quit_action);
        }

        let restored = Self::restore_windows(self_rc);
        dbg_state(&format!("activate: restored {restored} device window(s)"));

        let cfg = Config::load();
        if cfg.discovery_open || restored == 0 {
            dbg_state("activate: showing device list");
            Self::show_devices(self_rc);
        }
    }
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
