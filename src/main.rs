use adw::prelude::*;
use std::sync::atomic::Ordering;

mod api;
mod capabilities;
mod config;
mod discovery;
mod icons;
mod ui;

fn main() -> glib::ExitCode {
    let app = adw::Application::builder()
        .application_id("com.github.ozbenh.rustywiim2")
        .build();

    app.add_main_option(
        "debug",
        glib::Char(0),
        glib::OptionFlags::NONE,
        glib::OptionArg::None,
        "Print API protocol messages to stdout",
        None,
    );

    app.connect_handle_local_options(|_, opts| {
        if opts.contains("debug") {
            api::DEBUG.store(true, Ordering::Relaxed);
        }
        -1 // continue normal startup
    });

    app.connect_startup(|_| {
        adw::StyleManager::default().set_color_scheme(adw::ColorScheme::ForceDark);
    });
    app.connect_activate(ui::build_ui);
    app.run()
}
