use adw::prelude::*;
use gtk::gio;
use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use device::state::DeviceState;

mod config;
mod device;
mod ui;

// ── Application state ─────────────────────────────────────────────────────────
//
// All shared mutable state lives here as an `Rc<AppState>`.  Signal-handler
// closures capture either a strong clone (for the device-window registry) or a
// `Weak` clone (for the window close-request handlers).  This replaces the
// previous pile of individual `Rc<RefCell<...>>` captures and the
// `open_device: Rc<RefCell<Option<...>>>` deferred-initialisation hack.

struct AppState {
    app:            adw::Application,
    disc_mgr:       ui::devlist::DiscoveryManager,
    device_manager: device::manager::DeviceManager,
    registry:       RefCell<Vec<ui::DeviceWindow>>,
    settings_reg:   RefCell<Vec<ui::settings::SettingsWindow>>,
    disc_win:       RefCell<Option<ui::devlist::DiscoveryWindow>>,
}

impl AppState {
    // `disc_svc.start()` must run inside `connect_activate` so that
    // `glib::spawn_future_local` has an active main context.
    fn new(app: &adw::Application, rt: Arc<tokio::runtime::Runtime>) -> Rc<Self> {
        let disc_svc = device::discovery::DiscoveryService::new(rt.clone());
        disc_svc.start();
        let disc_mgr = ui::devlist::DiscoveryManager::new(rt.clone(), disc_svc.clone());

        Rc::new(Self {
            app:            app.clone(),
            disc_mgr,
            device_manager: device::manager::DeviceManager::new(rt),
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
                    sw.present();
                    return;
                }
            }
        }
        let s = ui::settings::SettingsWindow::new(ds);
        let win_clone = s.window_ref().clone();
        let weak_self = Rc::downgrade(self_rc);
        s.window_ref().connect_close_request(move |_| {
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
            let open_device_fn = {
                let state = Rc::clone(self_rc);
                Rc::new(move |entry: &ui::devlist::ManagedEntry| Self::open_device(&state, entry))
                    as Rc<dyn Fn(&ui::devlist::ManagedEntry)>
            };
            let open_settings_fn = {
                let state = Rc::clone(self_rc);
                Rc::new(move |ds| Self::open_settings(&state, ds))
                    as Rc<dyn Fn(Option<DeviceState>)>
            };
            *dw = Some(ui::devlist::DiscoveryWindow::new(
                &self_rc.app,
                &self_rc.disc_mgr,
                open_device_fn,
                open_settings_fn,
            ));
        }
        dw.as_ref().unwrap().present();
    }

    /// Present the existing device window for `entry`, or open a new one.
    fn open_device(self_rc: &Rc<Self>, entry: &ui::devlist::ManagedEntry) {
        {
            let reg = self_rc.registry.borrow();
            for w in reg.iter() {
                if w.uuid().map_or(false, |u| u == entry.uuid) {
                    w.present();
                    return;
                }
            }
        }
        if !entry.uuid.is_empty() {
            let mut cfg = config::Config::load();
            cfg.device_mut(&entry.uuid).window_open = true;
            cfg.save();
        }
        Self::open_device_spec(self_rc, ui::DeviceSpec {
            ip:       entry.ip.clone(),
            uuid:     entry.uuid.clone(),
            tls_mode: entry.tls_mode,
        });
    }

    /// Create a device window for `spec`, register it, and present it.
    /// Shared by the device-list open callback and the startup restore path.
    fn open_device_spec(self_rc: &Rc<Self>, spec: ui::DeviceSpec) {
        let show_fn = {
            let state = Rc::clone(self_rc);
            Rc::new(move || Self::show_devices(&state)) as Rc<dyn Fn()>
        };
        let open_settings_fn = {
            let state = Rc::clone(self_rc);
            Rc::new(move |ds| Self::open_settings(&state, ds)) as Rc<dyn Fn(Option<DeviceState>)>
        };
        let dw = ui::DeviceWindow::new_for_device(
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
            if let Some(s) = weak_self.upgrade() {
                s.registry.borrow_mut().retain(|w| w.window != win_key);
            }
            glib::Propagation::Proceed
        });
    }

    /// Restore device windows that were open at last exit. Returns count opened.
    fn restore_windows(self_rc: &Rc<Self>) -> usize {
        let cfg = config::Config::load();
        let mut count = 0;
        for (uuid, dev_cfg) in &cfg.devices {
            if !dev_cfg.window_open { continue; }
            let Some(ref ip) = dev_cfg.last_ip else { continue };
            if ip.is_empty() { continue; }
            Self::open_device_spec(self_rc, ui::DeviceSpec {
                ip:       ip.clone(),
                uuid:     uuid.clone(),
                tls_mode: device::api::TlsMode::HttpsWiiM,
            });
            count += 1;
        }
        count
    }

    /// Called once from `app.connect_activate`.
    fn activate(self_rc: &Rc<Self>) {
        self_rc.disc_mgr.start();

        {
            let mut cfg = config::Config::load();
            if cfg.migrate() { cfg.save(); }
        }

        let restored = Self::restore_windows(self_rc);

        let cfg = config::Config::load();
        if cfg.discovery_open || restored == 0 {
            Self::show_devices(self_rc);
        }
    }
}

// ── Entry point ───────────────────────────────────────────────────────────────

fn main() -> glib::ExitCode {
    let app = adw::Application::builder()
        .application_id("com.github.ozbenh.rustywiim2")
        .build();

    app.add_main_option(
        "debug",
        glib::Char(0),
        glib::OptionFlags::NONE,
        glib::OptionArg::String,
        "Enable debug output: comma-separated list of api, state, device, discovery, or all",
        Some("LIST"),
    );
    app.add_main_option(
        "tls",
        glib::Char(0),
        glib::OptionFlags::NONE,
        glib::OptionArg::String,
        "Override TLS mode: wiim (default), audio-pro, any, http",
        Some("MODE"),
    );

    app.connect_handle_local_options(|_, opts| {
        if let Ok(Some(list)) = opts.lookup::<String>("debug") {
            for token in list.split(',').map(str::trim) {
                match token {
                    "api"       => { device::api::DEBUG.store(true, Ordering::Relaxed); }
                    "state"     => { device::state::DEBUG_STATE.store(true, Ordering::Relaxed); }
                    "device"    => { device::capabilities::DEBUG_DEVICE.store(true, Ordering::Relaxed); }
                    "discovery" => { device::discovery::DEBUG_DISCOVERY.store(true, Ordering::Relaxed); }
                    "all"       => {
                        device::api::DEBUG.store(true, Ordering::Relaxed);
                        device::state::DEBUG_STATE.store(true, Ordering::Relaxed);
                        device::capabilities::DEBUG_DEVICE.store(true, Ordering::Relaxed);
                        device::discovery::DEBUG_DISCOVERY.store(true, Ordering::Relaxed);
                    }
                    other => {
                        eprintln!("rustywiim: unknown debug token {:?} (valid: api, state, device, discovery, all)", other);
                    }
                }
            }
        }
        if let Ok(Some(mode)) = opts.lookup::<String>("tls") {
            let tls = match mode.as_str() {
                "http"      => device::api::TlsMode::Http,
                "any"       => device::api::TlsMode::HttpsAny,
                "audio-pro" => device::api::TlsMode::HttpsAudioPro,
                _           => device::api::TlsMode::HttpsWiiM,
            };
            device::api::TLS_MODE.store(tls as usize, Ordering::Relaxed);
        }
        -1 // continue normal startup
    });

    // One single-threaded tokio runtime shared across all device windows.
    // Using current_thread ensures all async tasks run on a single dedicated
    // OS thread, so API calls to the same device are never truly concurrent.
    // The runtime is driven by a permanent background thread.
    let rt = Arc::new(
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("tokio runtime"),
    );
    {
        let rt2 = Arc::clone(&rt);
        std::thread::Builder::new()
            .name("tokio-rt".into())
            .spawn(move || rt2.block_on(std::future::pending::<()>()))
            .expect("tokio thread");
    }

    app.connect_startup(|_| {
        adw::StyleManager::default().set_color_scheme(adw::ColorScheme::ForceDark);
    });

    // App-level quit action — used by Ctrl-Q and the Quit menu item.
    {
        let quit_action = gio::SimpleAction::new("quit", None);
        let app2 = app.clone();
        quit_action.connect_activate(move |_, _| { app2.quit(); });
        app.add_action(&quit_action);
        app.set_accels_for_action("app.quit", &["<Ctrl>Q"]);
    }

    // Ctrl-W closes the focused window (action defined per-window in ui/).
    app.set_accels_for_action("win.close", &["<Ctrl>W"]);

    // Quit automatically when no visible window remains (handles the case
    // where the discovery window is hidden and the last device window closes).
    app.connect_window_removed(|a, _| {
        if !a.windows().iter().any(|w| w.is_visible()) {
            a.quit();
        }
    });

    app.connect_activate(move |app| {
        let state = AppState::new(app, rt.clone());
        AppState::activate(&state);
    });

    app.run()
}
