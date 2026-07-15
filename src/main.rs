use adw::prelude::*;
use gtk::gio;
use std::sync::Arc;
use std::sync::atomic::Ordering;

mod config;
mod ui;

use rustywiim::device;

/// Parses `--connect`'s `scheme://ip[:port]` into (ip-with-optional-port,
/// TlsMode). Deliberately minimal — no path/query, just enough to point
/// `device::api::api_base_url()` at an arbitrary host:port (e.g.
/// `wiim-simulator`'s randomly-assigned ports), which already accepts an
/// embedded port in `ip` for `Http`/`HttpsAny`/`HttpsWiiM`.
fn parse_connect_url(url: &str) -> Option<(String, device::api::TlsMode)> {
    let (scheme, rest) = url.split_once("://")?;
    let tls = match scheme {
        "http" => device::api::TlsMode::Http,
        "https" => device::api::TlsMode::HttpsWiiM,
        _ => return None,
    };
    let host_port = rest.split('/').next().unwrap_or(rest);
    if host_port.is_empty() {
        return None;
    }
    Some((host_port.to_string(), tls))
}

/// Extracts just the `host[:port]` portion from a `scheme://host[:port]`
/// URL — used for `--connect`'s optional second, comma-separated UPnP URL
/// (`http://api-host:port,http://upnp-host:port`), which only needs an
/// address for `device::upnp::UpnpClient::discover()` to try (that call
/// already tries both http/https schemes on its own, same as the normal
/// no-override case) — no `TlsMode` to resolve, unlike the API URL.
fn extract_host_port(url: &str) -> Option<String> {
    let (_, rest) = url.split_once("://")?;
    let host_port = rest.split('/').next().unwrap_or(rest);
    if host_port.is_empty() { None } else { Some(host_port.to_string()) }
}

fn main() -> glib::ExitCode {
    let app = adw::Application::builder()
        .application_id(ui::APP_ID)
        .build();

    app.add_main_option(
        "debug",
        glib::Char(0),
        glib::OptionFlags::NONE,
        glib::OptionArg::String,
        "Enable debug output: comma-separated list of api, state, device, discovery, upnp, ui, config, or all",
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
    app.add_main_option(
        "connect",
        glib::Char(0),
        glib::OptionFlags::NONE,
        glib::OptionArg::String,
        "Connect directly to scheme://ip[:port] (e.g. http://127.0.0.1:8080 for wiim-simulator), \
         opening a device window for it immediately instead of discovery. Optionally followed by \
         a comma and a second scheme://ip[:port] for the UPnP listener (e.g. wiim-simulator's \
         --upnp-port), tried instead of the two standard UPnP ports",
        Some("URL"),
    );
    app.add_main_option(
        "no-config",
        glib::Char(0),
        glib::OptionFlags::NONE,
        glib::OptionArg::None,
        "Don't load or save the config file — every run behaves like a fresh install",
        None,
    );
    app.add_main_option(
        "config-file",
        glib::Char(0),
        glib::OptionFlags::NONE,
        glib::OptionArg::Filename,
        "Use an alternate config file path instead of the default (for testing)",
        Some("PATH"),
    );

    app.connect_handle_local_options(|_, opts| {
        if let Ok(Some(list)) = opts.lookup::<String>("debug") {
            for token in list.split(',').map(str::trim) {
                match token {
                    "api"       => { device::api::DEBUG.store(true, Ordering::Relaxed); }
                    "state"     => { device::state::DEBUG_STATE.store(true, Ordering::Relaxed); }
                    "device"    => { device::capabilities::DEBUG_DEVICE.store(true, Ordering::Relaxed); }
                    "discovery" => { device::discovery::DEBUG_DISCOVERY.store(true, Ordering::Relaxed); }
                    "upnp"      => { device::upnp::DEBUG_UPNP.store(true, Ordering::Relaxed); }
                    "ui"        => { ui::DEBUG_UI.store(true, Ordering::Relaxed); }
                    "config"    => { config::DEBUG_CONFIG.store(true, Ordering::Relaxed); }
                    "all"       => {
                        device::api::DEBUG.store(true, Ordering::Relaxed);
                        device::state::DEBUG_STATE.store(true, Ordering::Relaxed);
                        device::capabilities::DEBUG_DEVICE.store(true, Ordering::Relaxed);
                        device::discovery::DEBUG_DISCOVERY.store(true, Ordering::Relaxed);
                        device::upnp::DEBUG_UPNP.store(true, Ordering::Relaxed);
                        ui::DEBUG_UI.store(true, Ordering::Relaxed);
                        config::DEBUG_CONFIG.store(true, Ordering::Relaxed);
                    }
                    other => {
                        eprintln!("rustywiim: unknown debug token {:?} (valid: api, state, device, discovery, upnp, ui, config, all)", other);
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
        if opts.lookup::<bool>("no-config").ok().flatten().unwrap_or(false) {
            config::set_no_config(true);
        }
        // `OptionArg::Filename` options surface as a GVariant bytestring
        // ("ay"), not a UTF-8 string ("s") — looking this up as `String`
        // (as every other string-valued option here does) silently never
        // matches the stored variant's type, so `lookup` always returned
        // `Ok(None)` and this override never took effect at all, no matter
        // how `--config-file` was spelled. `PathBuf` has the matching
        // `FromVariant` impl (via `g_variant_get_bytestring`).
        if let Ok(Some(path)) = opts.lookup::<std::path::PathBuf>("config-file") {
            config::set_config_path_override(path);
        }
        if let Ok(Some(url)) = opts.lookup::<String>("connect") {
            let (api_url, upnp_url) = match url.split_once(',') {
                Some((a, u)) => (a, Some(u)),
                None => (url.as_str(), None),
            };
            match parse_connect_url(api_url) {
                Some((ip, tls_mode)) => {
                    ui::set_direct_connect(ip, tls_mode);
                    if let Some(upnp_url) = upnp_url {
                        match extract_host_port(upnp_url) {
                            Some(host_port) => device::upnp::set_discover_override(host_port),
                            None => {
                                eprintln!(
                                    "rustywiim: --connect's UPnP URL must be scheme://ip[:port], got {upnp_url:?}"
                                );
                                return 1;
                            }
                        }
                    }
                }
                None => {
                    eprintln!(
                        "rustywiim: --connect expects scheme://ip[:port] (e.g. http://127.0.0.1:8080), got {api_url:?}"
                    );
                    return 1;
                }
            }
        }
        -1 // continue normal startup
    });

    // One single-threaded tokio runtime shared across all device windows.
    // Using current_thread ensures all async tasks run on a single dedicated
    // OS thread, so API calls to the same device are never truly concurrent.
    // The runtime is driven by a permanent background thread, which blocks on
    // `shutdown_rx` rather than `pending()` so it can be signalled to stop
    // (and joined) on quit instead of being killed mid-flight by process exit.
    let rt = Arc::new(
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("tokio runtime"),
    );
    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
    let rt_thread = {
        let rt2 = Arc::clone(&rt);
        std::thread::Builder::new()
            .name("tokio-rt".into())
            .spawn(move || { let _ = rt2.block_on(shutdown_rx); })
            .expect("tokio thread")
    };

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

    let exit_code = app.run();

    // Unblock the tokio thread's block_on(shutdown_rx) and join it so
    // in-flight tasks unwind via normal Drop instead of being torn down
    // mid-flight when the process exits.
    let _ = shutdown_tx.send(());
    if rt_thread.join().is_err() {
        eprintln!("rustywiim: tokio thread panicked during shutdown");
    }

    exit_code
}
