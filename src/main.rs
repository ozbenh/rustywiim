use adw::prelude::*;
use gtk::gio;
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
        let state = ui::AppState::new(app, rt.clone());
        ui::AppState::activate(&state);
    });

    app.run()
}
