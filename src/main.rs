use adw::prelude::*;
use gtk::gio;
use std::sync::Arc;
use std::sync::atomic::Ordering;

mod config;
mod ui;

use rustywiim::device;

/// Wall-clock timestamp prefix (`HH:MM:SS.mmm`, local time) for this binary
/// crate's own `--debug=*` log lines (`config.rs`, `ui/*.rs`) — the library
/// crate (`rustywiim::device`) has its own copy (`device::timestamp()`,
/// `pub(crate)` there), since that one isn't visible across the crate
/// boundary; same one-line format either way.
pub(crate) fn timestamp() -> String {
    chrono::Local::now().format("%H:%M:%S%.3f").to_string()
}

/// Parses one `--connect` occurrence into a `ui::ConnectTarget`, in this
/// order: contains `,` → split into `(ip, port)` → `Port` (the API port is
/// known but not the scheme, so this walks `PROBE_MODES` the way a plain
/// manual-add-by-IP does — rejected if the left side has a scheme, since
/// that's almost certainly a leftover of the old `scheme://ip,scheme://ip`
/// UPnP-override syntax this form replaces — checked *before* the `://`
/// case below, since a stray scheme on the left would otherwise just get
/// silently absorbed into `Explicit`'s host string instead of rejected);
/// contains `://` → `Explicit` (exact scheme, no probing — deliberately
/// minimal parsing, no path/query, just enough to point
/// `device::api::api_base_url()` at an arbitrary host:port, e.g.
/// `wiim-simulator`'s randomly-assigned ports); otherwise a bare IP →
/// `ViaUpnp` (resolves the API address itself via the UPnP advert
/// `wiim-simulator` emits — see `device::upnp::discover_api_address()`).
fn parse_connect_target(spec: &str) -> Result<ui::ConnectTarget, String> {
    if let Some((ip, port)) = spec.split_once(',') {
        if ip.contains("://") {
            return Err(format!(
                "--connect ip,port takes a bare IP before the comma, got {spec:?}"
            ));
        }
        let port: u16 = port
            .parse()
            .map_err(|_| format!("--connect {spec:?}: {port:?} is not a port number"))?;
        return Ok(ui::ConnectTarget::Port { ip: ip.to_string(), port });
    }
    if let Some((scheme, rest)) = spec.split_once("://") {
        let tls = match scheme {
            "http" => device::api::TlsMode::Http,
            "https" => device::api::TlsMode::HttpsWiiM,
            _ => return Err(format!("--connect scheme must be http:// or https://, got {spec:?}")),
        };
        let host_port = rest.split('/').next().unwrap_or(rest);
        if host_port.is_empty() {
            return Err(format!("--connect {spec:?} has no host"));
        }
        return Ok(ui::ConnectTarget::Explicit { ip: host_port.to_string(), tls });
    }
    Ok(ui::ConnectTarget::ViaUpnp { ip: spec.to_string() })
}

/// Generic comma-separated `key`/`key:value` token parser, reusable by any
/// `--option=a,b:c,...`-style flag (first user: `--kiosk`) — same
/// `name`/`name:modifier` shape as `--debug`. Returns a `Vec`, not a
/// `HashMap`: duplicate keys are the caller's call, not this parser's.
fn parse_kv_csv(s: &str) -> Vec<(&str, Option<&str>)> {
    s.split(',').map(|tok| match tok.split_once(':') {
        Some((k, v)) => (k, Some(v)),
        None => (tok, None),
    }).collect()
}

/// Rewrites `--kiosk=<value>` to `--kiosk:opts=<value>` before argv reaches
/// GLib's option parser. Bare `--kiosk` (no `=`) is left untouched: GLib
/// can't accept an optional value on a plain string option, so the two
/// forms need separate registered option names under the hood.
fn rewrite_kiosk_arg(args: impl Iterator<Item = String>) -> Vec<String> {
    args.map(|a| match a.strip_prefix("--kiosk=") {
        Some(value) => format!("--kiosk:opts={value}"),
        None => a,
    }).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_kv_csv_splits_bare_and_kv_tokens() {
        assert_eq!(parse_kv_csv("layout:1,only"), vec![("layout", Some("1")), ("only", None)]);
        assert_eq!(parse_kv_csv("only,layout:2"), vec![("only", None), ("layout", Some("2"))]);
    }

    #[test]
    fn rewrite_kiosk_arg_only_touches_kiosk_equals() {
        let args = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>().into_iter();
        assert_eq!(
            rewrite_kiosk_arg(args(&["rustywiim", "--kiosk=layout:1,only", "--no-config"])),
            vec!["rustywiim", "--kiosk:opts=layout:1,only", "--no-config"],
        );
        assert_eq!(
            rewrite_kiosk_arg(args(&["rustywiim", "--kiosk"])),
            vec!["rustywiim", "--kiosk"],
        );
        assert_eq!(
            rewrite_kiosk_arg(args(&["rustywiim", "--kiosk:opts=only"])),
            vec!["rustywiim", "--kiosk:opts=only"],
        );
    }

    #[test]
    fn connect_target_parses_explicit_scheme_form() {
        match parse_connect_target("http://127.0.0.2:41234").unwrap() {
            ui::ConnectTarget::Explicit { ip, tls } => {
                assert_eq!(ip, "127.0.0.2:41234");
                assert_eq!(tls, device::api::TlsMode::Http);
            }
            other => panic!("expected Explicit, got {other:?}"),
        }
    }

    #[test]
    fn connect_target_parses_ip_port_form() {
        match parse_connect_target("127.0.0.2,41234").unwrap() {
            ui::ConnectTarget::Port { ip, port } => {
                assert_eq!(ip, "127.0.0.2");
                assert_eq!(port, 41234);
            }
            other => panic!("expected Port, got {other:?}"),
        }
    }

    #[test]
    fn connect_target_rejects_a_scheme_before_the_comma() {
        assert!(parse_connect_target("http://127.0.0.2,41234").is_err());
    }

    #[test]
    fn connect_target_parses_bare_ip_as_via_upnp() {
        match parse_connect_target("127.0.0.2").unwrap() {
            ui::ConnectTarget::ViaUpnp { ip } => assert_eq!(ip, "127.0.0.2"),
            other => panic!("expected ViaUpnp, got {other:?}"),
        }
    }
}

/// `bundle-gtk-macos.sh` ships a copy of Homebrew's own GTK/GLib/gdk-pixbuf
/// data files (schemas, icons, pixbuf loaders) inside `Contents/Resources`
/// and `Contents/Frameworks`, since we bundle Homebrew's stack rather than
/// building our own relocatable one — but GLib/GTK/gdk-pixbuf still resolve
/// those by paths baked in at Homebrew's own build time (`/opt/homebrew/...`)
/// unless told otherwise. Bundling the files alone doesn't redirect the
/// lookup, which is why removing `/opt/homebrew` after the fact reproduces
/// missing icons and a gdk-pixbuf loader crash even though everything needed
/// is sitting right there in the bundle. Must run before anything touches
/// GLib/GTK, hence right at the top of `main()`; a no-op for an unbundled
/// dev build (`cargo run`), where `Contents/Resources` doesn't exist.
#[cfg(target_os = "macos")]
fn setup_macos_bundle_env() {
    let Ok(exe) = std::env::current_exe() else {
        return;
    };

    // exe = .../RustyWiiM.app/Contents/MacOS/RustyWiiM
    let Some(contents) = exe.parent().and_then(|macos_dir| macos_dir.parent()) else {
        return;
    };
    let resources = contents.join("Resources");
    let frameworks = contents.join("Frameworks");

    // Not running from inside an app bundle (e.g. `cargo run` in dev) —
    // leave the environment untouched.
    if !resources.join("share").is_dir() {
        return;
    }

    // Single-threaded at this point (before the tokio thread is spawned),
    // so mutating the environment here can't race another thread reading it.
    unsafe {
        let schema_dir = resources.join("share/glib-2.0/schemas");
        if schema_dir.is_dir() {
            std::env::set_var("GSETTINGS_SCHEMA_DIR", &schema_dir);
        }

        let data_dir = resources.join("share");
        let xdg_data_dirs = match std::env::var("XDG_DATA_DIRS") {
            Ok(existing) if !existing.is_empty() => {
                format!("{}:{existing}", data_dir.display())
            }
            _ => format!("{}:/usr/local/share:/usr/share", data_dir.display()),
        };
        std::env::set_var("XDG_DATA_DIRS", xdg_data_dirs);

        // gdk-pixbuf loads its PNG/JPEG/SVG/... format plugins via dlopen
        // based on a generated cache file, not link-time dependencies, so
        // `macho-deps.py`'s otool-based dependency walk never finds them —
        // `bundle-gtk-macos.sh` copies the loader dylibs into `Frameworks`
        // and generates this cache with a `@BUNDLE_FRAMEWORKS@` placeholder
        // in place of the real path, since the bundle's final install
        // location isn't known until now. Patch it and point
        // `GDK_PIXBUF_MODULE_FILE` at the patched copy.
        let loader_cache_template = resources.join("share/gdk-pixbuf-2.0/loaders.cache");
        if let Ok(template) = std::fs::read_to_string(&loader_cache_template) {
            let patched = template.replace("@BUNDLE_FRAMEWORKS@", &frameworks.to_string_lossy());

            let cache_dir = dirs::cache_dir()
                .unwrap_or_else(std::env::temp_dir)
                .join("rustywiim");
            let patched_cache = cache_dir.join("gdk-pixbuf-loaders.cache");

            if std::fs::create_dir_all(&cache_dir).is_ok()
                && std::fs::write(&patched_cache, patched).is_ok()
            {
                std::env::set_var("GDK_PIXBUF_MODULE_FILE", &patched_cache);
            }
        }
    }
}

fn main() -> glib::ExitCode {
    #[cfg(target_os = "macos")]
    setup_macos_bundle_env();

    let app = adw::Application::builder()
        .application_id(ui::APP_ID)
        // Required for gtk::Application::inhibit()/uninhibit() to actually
        // do anything (Kiosk mode's system-screensaver inhibit) — GTK's own
        // docs are explicit that inhibit() silently no-ops without this.
        .register_session(true)
        .build();

    app.add_main_option(
        "debug",
        glib::Char(0),
        glib::OptionFlags::NONE,
        glib::OptionArg::String,
        "Enable debug output: comma-separated list of api, state, device, discovery, upnp, gena, ui, config, or all. \
         api/upnp/gena (and all) may add \":verbose\" (e.g. upnp:verbose) for full request/response content \
         instead of a one-line summary",
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
        glib::OptionArg::StringArray,
        "Connect directly to a device, opening a window for it immediately instead of \
         discovery/config-restored windows (repeatable, one device per occurrence — for a \
         multi-device wiim-simulator fleet). Three forms: scheme://ip[:port] (exact, no \
         probing); ip,port (API port known, scheme walked); or a bare ip, which resolves the \
         API address itself via the UPnP advert wiim-simulator emits, falling back to a plain \
         probe if there isn't one",
        Some("TARGET"),
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
    app.add_main_option(
        "try-all-upnp",
        glib::Char(0),
        glib::OptionFlags::NONE,
        glib::OptionArg::None,
        "Ignore discovery's non-LinkPlay denylists (both the SSDP-header \
         denylist and the consecutive-failure counter) so every announced \
         device is probed every time — for testing discovery against \
         devices normally denylisted on this network",
        None,
    );
    app.add_main_option(
        "kiosk:opts",
        glib::Char(0),
        glib::OptionFlags::NONE,
        glib::OptionArg::String,
        "Start directly in Kiosk mode (a single fullscreen window), with \
         suboptions, comma-separated, any order: \"layout:1\" (Classic) or \
         \"layout:2\" (WideRight, the default), and/or \"only\" (lock the \
         session into Kiosk mode permanently — no exit button, no \"K\" key)",
        Some("OPTS"),
    );
    app.add_main_option(
        "kiosk",
        glib::Char(0),
        glib::OptionFlags::NONE,
        glib::OptionArg::None,
        "Start directly in Kiosk mode. See --kiosk:opts for suboptions \
         (--kiosk=<opts> also accepted — deprecated syntax, alias for \
         --kiosk:opts=<opts>)",
        None,
    );

    app.connect_handle_local_options(|_, opts| {
        if let Ok(Some(list)) = opts.lookup::<String>("debug") {
            for token in list.split(',').map(str::trim) {
                let (name, modifier) = match token.split_once(':') {
                    Some((n, m)) => (n, Some(m)),
                    None => (token, None),
                };
                let verbose = modifier == Some("verbose");
                if let Some(m) = modifier {
                    if m != "verbose" {
                        eprintln!("rustywiim: unknown debug modifier {m:?} for {name:?} (only \"verbose\" is supported)");
                    }
                }
                // `verbose` is a no-op for tokens with no verbose distinction
                // (state/device/discovery/ui/config) — enabling them plainly
                // is still the right behavior, matching "all:verbose applies
                // verbose only to whatever supports it."
                match name {
                    "api"       => {
                        device::api::DEBUG.store(true, Ordering::Relaxed);
                        if verbose { device::api::DEBUG_VERBOSE.store(true, Ordering::Relaxed); }
                    }
                    "state"     => { device::state::DEBUG_STATE.store(true, Ordering::Relaxed); }
                    "device"    => { device::capabilities::DEBUG_DEVICE.store(true, Ordering::Relaxed); }
                    "discovery" => { device::discovery::DEBUG_DISCOVERY.store(true, Ordering::Relaxed); }
                    "upnp"      => {
                        device::upnp::DEBUG_UPNP.store(true, Ordering::Relaxed);
                        if verbose { device::upnp::DEBUG_UPNP_VERBOSE.store(true, Ordering::Relaxed); }
                    }
                    "gena"      => {
                        device::gena::DEBUG_GENA.store(true, Ordering::Relaxed);
                        if verbose { device::gena::DEBUG_GENA_VERBOSE.store(true, Ordering::Relaxed); }
                    }
                    "ui"        => { ui::DEBUG_UI.store(true, Ordering::Relaxed); }
                    "config"    => { config::DEBUG_CONFIG.store(true, Ordering::Relaxed); }
                    "eq"        => { device::eq::DEBUG_EQ.store(true, Ordering::Relaxed); }
                    "all"       => {
                        device::api::DEBUG.store(true, Ordering::Relaxed);
                        device::state::DEBUG_STATE.store(true, Ordering::Relaxed);
                        device::capabilities::DEBUG_DEVICE.store(true, Ordering::Relaxed);
                        device::discovery::DEBUG_DISCOVERY.store(true, Ordering::Relaxed);
                        device::upnp::DEBUG_UPNP.store(true, Ordering::Relaxed);
                        device::gena::DEBUG_GENA.store(true, Ordering::Relaxed);
                        ui::DEBUG_UI.store(true, Ordering::Relaxed);
                        config::DEBUG_CONFIG.store(true, Ordering::Relaxed);
                        device::eq::DEBUG_EQ.store(true, Ordering::Relaxed);
                        if verbose {
                            device::api::DEBUG_VERBOSE.store(true, Ordering::Relaxed);
                            device::upnp::DEBUG_UPNP_VERBOSE.store(true, Ordering::Relaxed);
                            device::gena::DEBUG_GENA_VERBOSE.store(true, Ordering::Relaxed);
                        }
                    }
                    other => {
                        eprintln!("rustywiim: unknown debug token {:?} (valid: api, state, device, discovery, upnp, gena, ui, config, eq, all)", other);
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
        if opts.lookup::<bool>("try-all-upnp").ok().flatten().unwrap_or(false) {
            device::discovery::IGNORE_DENYLIST.store(true, Ordering::Relaxed);
        }
        if opts.lookup::<bool>("kiosk").ok().flatten().unwrap_or(false) {
            ui::set_start_in_kiosk(true);
        }
        // `--kiosk=<suboptions>` is rewritten to `--kiosk:opts=<suboptions>`
        // by `rewrite_kiosk_arg()` before argv reaches here; `kiosk:opts`
        // also stays directly usable on its own.
        if let Ok(Some(csv)) = opts.lookup::<String>("kiosk:opts") {
            ui::set_start_in_kiosk(true); // implies --kiosk
            for (key, value) in parse_kv_csv(&csv) {
                match (key, value) {
                    ("layout", Some("1")) => ui::set_kiosk_layout_override(ui::KioskLayoutOverride::Classic),
                    ("layout", Some("2")) => ui::set_kiosk_layout_override(ui::KioskLayoutOverride::WideRight),
                    ("layout", v) => eprintln!("--kiosk: expected layout:1 or layout:2, got layout:{v:?} — ignoring"),
                    ("only", None) => ui::set_kiosk_only(true),
                    ("only", Some(v)) => eprintln!("--kiosk: \"only\" takes no value, got only:{v:?} — ignoring"),
                    (other, _) => eprintln!("--kiosk: unknown suboption {other:?} — ignoring"),
                }
            }
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
        if let Ok(Some(specs)) = opts.lookup::<Vec<String>>("connect") {
            let mut targets = Vec::with_capacity(specs.len());
            for spec in specs {
                match parse_connect_target(&spec) {
                    Ok(target) => targets.push(target),
                    Err(msg) => {
                        eprintln!("rustywiim: {msg}");
                        return 1;
                    }
                }
            }
            ui::set_direct_connect(targets);
        }
        -1 // continue normal startup
    });

    // One single-threaded tokio runtime shared across all device windows.
    // Using current_thread ensures all async tasks run on a single dedicated
    // OS thread, so API calls to the same device are never truly concurrent.
    // The runtime is driven by a permanent background thread, which blocks on
    // `shutdown_rx` rather than `pending()` so it can be signalled to stop
    // (and joined) on quit instead of being killed mid-flight by process exit.
    // After the shutdown signal arrives, it waits (deterministically, not a
    // blind fixed sleep — see `device::gena::wait_for_all_stops()`) for
    // already-spawned cleanup tasks (concretely: `GenaSession::stop()`'s real
    // `UNSUBSCRIBE` calls, fired from window-close-on-quit) to actually
    // finish, capped at 2s, rather than being dropped mid-flight the instant
    // this thread is joined and the process exits.
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
            .spawn(move || {
                rt2.block_on(async move {
                    let _ = shutdown_rx.await;
                    device::gena::wait_for_all_stops(std::time::Duration::from_secs(2)).await;
                });
            })
            .expect("tokio thread")
    };

    app.connect_startup(|_| {
        adw::StyleManager::default().set_color_scheme(adw::ColorScheme::ForceDark);
    });

    // Quit/close accelerators. macOS additionally binds the Command key,
    // which GDK reports as `<Meta>` (GDK_META_MASK) there — Ctrl keeps
    // working alongside it. Command is listed first on macOS because GTK
    // treats the first entry as the primary accelerator and shows that one
    // in menus, so the hamburger menu reads Cmd-Q rather than Ctrl-Q. Meta
    // is left unbound elsewhere: on Linux it isn't the Command key and that
    // modifier is generally the window manager's to claim.
    let (quit_accels, close_accels): (&[&str], &[&str]) = if cfg!(target_os = "macos") {
        (&["<Meta>Q", "<Ctrl>Q"], &["<Meta>W", "<Ctrl>W"])
    } else {
        (&["<Ctrl>Q"], &["<Ctrl>W"])
    };

    // App-level quit action — used by the quit accelerators and the Quit menu item.
    {
        let quit_action = gio::SimpleAction::new("quit", None);
        let app2 = app.clone();
        quit_action.connect_activate(move |_, _| { app2.quit(); });
        app.add_action(&quit_action);
        app.set_accels_for_action("app.quit", quit_accels);
    }

    // Closes the focused window (action defined per-window in ui/).
    app.set_accels_for_action("win.close", close_accels);

    // Quit automatically when no visible window remains (handles the case
    // where the discovery window is hidden and the last device window closes).
    app.connect_window_removed(|a, _| {
        // A window whose destroy never completed stays registered here and
        // keeps reporting itself visible (`is_visible()` is the widget's
        // own flag, not whether it has a live surface), which both blocks
        // this quit and holds the GtkApplication's per-window use count
        // above zero. Naming what's left makes that case identifiable
        // rather than looking like the app simply ignoring the last close.
        let visible: Vec<String> = a.windows().iter()
            .filter(|w| w.is_visible())
            .map(|w| format!("{}({})", w.type_().name(), w.title().unwrap_or_default()))
            .collect();
        ui::dbg_ui(&format!(
            "window removed: {} registered, {} visible{}",
            a.windows().len(),
            visible.len(),
            if visible.is_empty() { String::new() } else { format!(": {}", visible.join(", ")) },
        ));

        if visible.is_empty() {
            a.quit();
        }
    });

    app.connect_activate(move |app| {
        let state = ui::AppState::new(app, rt.clone());
        ui::AppState::activate(&state);
    });

    let exit_code = app.run_with_args(&rewrite_kiosk_arg(std::env::args()));

    // Unblock the tokio thread's block_on(shutdown_rx) and join it so
    // in-flight tasks unwind via normal Drop instead of being torn down
    // mid-flight when the process exits.
    let _ = shutdown_tx.send(());
    if rt_thread.join().is_err() {
        eprintln!("rustywiim: tokio thread panicked during shutdown");
    }

    exit_code
}
