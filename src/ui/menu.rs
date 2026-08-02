use gtk::gio;

/// Build the application hamburger menu button.
///
/// `include_devices` adds a "Devices…" item in its own section at the top,
/// and — same condition, since both only mean anything scoped to one
/// specific device — a "Device Settings…" item in the main section.
/// Device windows include both; the discovery window omits both (it
/// already is the list, and isn't scoped to any one device).
pub(crate) fn build_menu_button(include_devices: bool) -> gtk::MenuButton {
    let menu = gio::Menu::new();

    if include_devices {
        let devices_section = gio::Menu::new();
        devices_section.append(Some("Devices…"), Some("win.devices"));
        menu.append_section(None, &devices_section);
    }

    let main_section = gio::Menu::new();
    main_section.append(Some("Enter Kiosk Mode"), Some("win.kiosk"));
    main_section.append(Some("Preferences…"), Some("win.preferences"));
    if include_devices {
        // Greyed live for a group leader (a leader's window is the
        // group's, and a group isn't a device) — see
        // `device_window::wire_window_lifecycle()`'s `group-changed`
        // wiring, which toggles `win.device-settings`'s enabled state.
        main_section.append(Some("Device Settings…"), Some("win.device-settings"));
    }
    main_section.append(Some("About RustyWiiM"), Some("win.about"));
    menu.append_section(None, &main_section);

    let quit_section = gio::Menu::new();
    quit_section.append(Some("Quit"), Some("app.quit"));
    menu.append_section(None, &quit_section);

    gtk::MenuButton::builder()
        .icon_name("open-menu-symbolic")
        .menu_model(&menu)
        .tooltip_text("Menu")
        .build()
}
