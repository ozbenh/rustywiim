use adw::prelude::*;
use gtk::gio;
use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::Ordering;

mod config;
mod device;


mod ui;

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

    // Ctrl-W closes the focused window (action defined per-window below).
    app.set_accels_for_action("win.close", &["<Ctrl>W"]);

    // Quit automatically when no visible window remains (handles the case
    // where the discovery window is hidden and the last device window closes).
    app.connect_window_removed(|a, _| {
        if !a.windows().iter().any(|w| w.is_visible()) {
            a.quit();
        }
    });

    app.connect_activate(move |app| {
        // Create the discovery service here so it runs on the GTK main thread
        // (glib::spawn_future_local requires the main context to be active).
        let disc_svc = device::discovery::DiscoveryService::new(rt.clone());
        disc_svc.start();
        let disc_mgr = ui::devlist::DiscoveryManager::new(rt.clone(), disc_svc.clone());

        // Registry of open device windows — used to present existing windows
        // instead of creating duplicates when activating a device from the list.
        let registry: Rc<RefCell<Vec<ui::DeviceWindow>>> = Rc::new(RefCell::new(Vec::new()));

        // Lazy discovery window — created on first open, hidden on close.
        let disc_win: Rc<RefCell<Option<ui::devlist::DiscoveryWindow>>> =
            Rc::new(RefCell::new(None));

        // open_device is populated after show_devices is created (both close a
        // reference cycle that is intentional: both live for the app's lifetime).
        let open_device: Rc<RefCell<Option<Rc<dyn Fn(&ui::devlist::ManagedEntry)>>>> =
            Rc::new(RefCell::new(None));

        let show_devices: Rc<dyn Fn()> = {
            let disc_win    = Rc::clone(&disc_win);
            let open_device = Rc::clone(&open_device);
            let disc_mgr    = disc_mgr.clone();
            let app         = app.clone();
            Rc::new(move || {
                let mut dw = disc_win.borrow_mut();
                if dw.is_none() {
                    let open_fn = open_device.borrow()
                        .as_ref()
                        .expect("open_device not yet initialised")
                        .clone();
                    *dw = Some(ui::devlist::DiscoveryWindow::new(&app, &disc_mgr, open_fn));
                }
                dw.as_ref().unwrap().present();
            })
        };

        // Build the open_device callback now that show_devices is available.
        *open_device.borrow_mut() = Some({
            let app          = app.clone();
            let rt           = rt.clone();
            let show_devices = Rc::clone(&show_devices);
            let registry     = Rc::clone(&registry);
            Rc::new(move |entry: &ui::devlist::ManagedEntry| {
                // If a window already exists for this UUID, bring it to front.
                {
                    let reg = registry.borrow();
                    for w in reg.iter() {
                        if w.uuid().map_or(false, |u| u == entry.uuid) {
                            w.present();
                            return;
                        }
                    }
                }
                let spec = ui::DeviceSpec {
                    ip:       entry.ip.clone(),
                    uuid:     entry.uuid.clone(),
                    tls_mode: entry.tls_mode,
                };
                let dw = ui::DeviceWindow::new_for_device(&app, rt.clone(), Rc::clone(&show_devices), spec);
                // Mark window open in config.
                if !entry.uuid.is_empty() {
                    let mut cfg = config::Config::load();
                    cfg.device_mut(&entry.uuid).window_open = true;
                    cfg.save();
                }
                let gtk_win = dw.window.clone();
                dw.present();
                registry.borrow_mut().push(dw);
                let win_key  = gtk_win.clone();
                let reg_weak = Rc::clone(&registry);
                gtk_win.connect_close_request(move |_| {
                    reg_weak.borrow_mut().retain(|w| w.window != win_key);
                    glib::Propagation::Proceed
                });
            })
        });

        // ── One-time migration: move legacy last_ip/last_uuid into per-device entry.
        {
            let mut cfg = config::Config::load();
            if cfg.migrate() {
                cfg.save();
            }
        }

        disc_mgr.start();

        // ── Restore open windows from config ─────────────────────────────────────
        let cfg = config::Config::load();
        let mut device_windows_opened = 0usize;

        // Open a window for every device that was open when the app last exited.
        for (uuid, dev_cfg) in &cfg.devices {
            if !dev_cfg.window_open { continue; }
            let Some(ref ip) = dev_cfg.last_ip else { continue };
            if ip.is_empty() { continue; }
            let spec = ui::DeviceSpec {
                ip:       ip.clone(),
                uuid:     uuid.clone(),
                tls_mode: device::api::TlsMode::HttpsWiiM,
            };
            let dw = ui::DeviceWindow::new_for_device(app, rt.clone(), Rc::clone(&show_devices), spec);
            let gtk_win = dw.window.clone();
            dw.present();
            registry.borrow_mut().push(dw);
            let win_key  = gtk_win.clone();
            let reg_weak = Rc::clone(&registry);
            gtk_win.connect_close_request(move |_| {
                reg_weak.borrow_mut().retain(|w| w.window != win_key);
                glib::Propagation::Proceed
            });
            device_windows_opened += 1;
        }

        // Open the discovery window if it was open before, or if there are no
        // device windows to show (including first run with an empty config).
        if cfg.discovery_open || device_windows_opened == 0 {
            (show_devices)();
        }
    });
    app.run()
}
