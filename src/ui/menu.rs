use gtk::gio;

/// Build the application hamburger menu button.
///
/// `include_devices` adds a "Devices…" item in its own section at the top.
/// Device windows include it; the discovery window omits it (it already is the list).
pub(crate) fn build_menu_button(include_devices: bool) -> gtk::MenuButton {
    let menu = gio::Menu::new();

    if include_devices {
        let devices_section = gio::Menu::new();
        devices_section.append(Some("Devices…"), Some("win.devices"));
        menu.append_section(None, &devices_section);
    }

    let main_section = gio::Menu::new();
    main_section.append(Some("Settings…"), Some("win.settings"));
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
