use adw::prelude::*;
use std::sync::Arc;
use std::sync::atomic::Ordering;

mod api;
mod capabilities;
mod config;
mod device_state;
mod discovery;
mod icons;
mod scroll_fade_label;
mod ui;

fn main() -> glib::ExitCode {
    let app = adw::Application::builder()
        .application_id("com.github.ozbenh.rustywiim2")
        .build();

    app.add_main_option(
        "debug-api",
        glib::Char(0),
        glib::OptionFlags::NONE,
        glib::OptionArg::None,
        "Print API protocol messages to stdout",
        None,
    );
    app.add_main_option(
        "debug-state",
        glib::Char(0),
        glib::OptionFlags::NONE,
        glib::OptionArg::None,
        "Print device state changes and signals to stdout",
        None,
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
        if opts.contains("debug-api") {
            api::DEBUG.store(true, Ordering::Relaxed);
        }
        if opts.contains("debug-state") {
            device_state::DEBUG_STATE.store(true, Ordering::Relaxed);
        }
        if let Ok(Some(mode)) = opts.lookup::<String>("tls") {
            let tls = match mode.as_str() {
                "http"      => api::TlsMode::Http,
                "any"       => api::TlsMode::HttpsAny,
                "audio-pro" => api::TlsMode::HttpsAudioPro,
                _           => api::TlsMode::HttpsWiiM,
            };
            api::TLS_MODE.store(tls as usize, Ordering::Relaxed);
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
    app.connect_activate(move |app| {
        ui::DeviceWindow::new(app, rt.clone()).present();
    });
    app.run()
}
