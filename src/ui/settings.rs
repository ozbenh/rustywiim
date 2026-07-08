// Settings window — non-modal, GNOME-style split layout.
//
// Left sidebar: section-titled navigation list (gtk::ListBox .navigation-sidebar).
// Right panel:  gtk::Stack of adw::PreferencesPage widgets, one per topic.
//
// `ds` carries the associated DeviceState when opened from a device window.
// When `ds` is None the window was opened from the device list and shows only
// application-wide settings.  Device-specific pages (added in the future) must
// check `ds.is_some()` before rendering.

#![allow(deprecated)] // glib clone! @strong syntax

use adw::prelude::*;
use gtk::glib;
use gtk::Orientation;

use crate::config::{self, ThemeMode};
use crate::device::playback::AccessMethod;
use crate::device::state::DeviceState;
use crate::ui::DEBUG_UI;
use std::sync::atomic::Ordering;

fn rgba_to_hex(c: &gtk::gdk::RGBA) -> String {
    format!(
        "#{:02x}{:02x}{:02x}",
        (c.red()   * 255.0).round() as u8,
        (c.green() * 255.0).round() as u8,
        (c.blue()  * 255.0).round() as u8,
    )
}

// ── Public handle ─────────────────────────────────────────────────────────────

pub(crate) struct SettingsWindow {
    window: adw::Window,
    /// Non-None when opened from a device window; None from the device list.
    /// Stored for future device-specific settings pages.
    #[allow(dead_code)]
    pub(crate) ds: Option<DeviceState>,
}

impl SettingsWindow {
    pub(crate) fn new(ds: Option<DeviceState>) -> Self {
        // ── Navigation sidebar ─────────────────────────────────────────────────
        let sidebar_box = gtk::Box::builder()
            .orientation(Orientation::Vertical)
            .build();

        // "Application" section header
        let app_label = gtk::Label::builder()
            .label("Application")
            .xalign(0.0)
            .css_classes(["caption", "dim-label"])
            .margin_start(12).margin_end(12)
            .margin_top(12).margin_bottom(4)
            .build();
        sidebar_box.append(&app_label);

        let sidebar_list = gtk::ListBox::builder()
            .selection_mode(gtk::SelectionMode::Single)
            .css_classes(["navigation-sidebar"])
            .build();

        let appearance_row = adw::ActionRow::builder()
            .title("Appearance")
            .selectable(true)
            .activatable(true)
            .build();
        sidebar_list.append(&appearance_row);
        sidebar_box.append(&sidebar_list);

        // "Device" section — only when opened from a device window (`ds`
        // carries the DeviceState in that case; `None` means opened from the
        // device list, global-settings-only). Its own ListBox (GTK's
        // selection model is per-ListBox), coordinated with `sidebar_list`
        // below so only one row across both sections ever reads as selected.
        let device_list = gtk::ListBox::builder()
            .selection_mode(gtk::SelectionMode::Single)
            .css_classes(["navigation-sidebar"])
            .build();
        if ds.is_some() {
            let device_label = gtk::Label::builder()
                .label("Device")
                .xalign(0.0)
                .css_classes(["caption", "dim-label"])
                .margin_start(12).margin_end(12)
                .margin_top(12).margin_bottom(4)
                .build();
            sidebar_box.append(&device_label);

            let advanced_row = adw::ActionRow::builder()
                .title("Advanced")
                .selectable(true)
                .activatable(true)
                .build();
            device_list.append(&advanced_row);

            let about_row = adw::ActionRow::builder()
                .title("About")
                .selectable(true)
                .activatable(true)
                .build();
            device_list.append(&about_row);

            sidebar_box.append(&device_list);
        }

        let sidebar_scroll = gtk::ScrolledWindow::builder()
            .hscrollbar_policy(gtk::PolicyType::Never)
            .vexpand(true)
            .build();
        sidebar_scroll.set_child(Some(&sidebar_box));

        // ── Content stack ──────────────────────────────────────────────────────
        let content_stack = gtk::Stack::builder()
            .transition_type(gtk::StackTransitionType::None)
            .vexpand(true)
            .hexpand(true)
            .build();

        let appearance_page = build_appearance_page();
        content_stack.add_named(&appearance_page, Some("appearance"));

        if let Some(ref d) = ds {
            let advanced_page = build_advanced_page(d);
            content_stack.add_named(&advanced_page, Some("advanced"));
            let about_page = build_about_page(d);
            content_stack.add_named(&about_page, Some("about"));
        }

        // Select "Appearance" by default
        sidebar_list.select_row(sidebar_list.row_at_index(0).as_ref());

        sidebar_list.connect_row_selected({
            let stack  = content_stack.clone();
            let device_list = device_list.clone();
            move |_, row| {
                let Some(row) = row else { return };
                device_list.unselect_all();
                let name = match row.index() {
                    0 => "appearance",
                    _ => return,
                };
                stack.set_visible_child_name(name);
            }
        });

        device_list.connect_row_selected({
            let stack = content_stack.clone();
            let sidebar_list = sidebar_list.clone();
            move |_, row| {
                let Some(row) = row else { return };
                sidebar_list.unselect_all();
                let name = match row.index() {
                    0 => "advanced",
                    1 => "about",
                    _ => return,
                };
                stack.set_visible_child_name(name);
            }
        });

        // ── Layout: sidebar | content ──────────────────────────────────────────
        let paned = gtk::Paned::new(Orientation::Horizontal);
        paned.set_start_child(Some(&sidebar_scroll));
        paned.set_end_child(Some(&content_stack));
        paned.set_position(220);
        paned.set_shrink_start_child(false);
        paned.set_shrink_end_child(false);
        paned.set_resize_start_child(false);
        paned.set_resize_end_child(true);

        let header = adw::HeaderBar::new();
        let toolbar_view = adw::ToolbarView::new();
        toolbar_view.add_top_bar(&header);
        toolbar_view.set_content(Some(&paned));

        let initial_title = match ds.as_ref().and_then(|d| d.device_info()) {
            Some(i) => format!("Settings ({})", i.device_name),
            None    => "Settings".to_string(),
        };
        let window = adw::Window::builder()
            .title(&initial_title)
            .default_width(720)
            .default_height(520)
            .modal(false)
            .build();
        window.set_content(Some(&toolbar_view));

        if let Some(ref d) = ds {
            d.connect_device_changed(glib::clone!(@weak window => move |ds| {
                let title = match ds.device_info() {
                    Some(i) => format!("Settings ({})", i.device_name),
                    None    => "Settings".to_string(),
                };
                window.set_title(Some(&title));
            }));
        }

        if DEBUG_UI.load(Ordering::Relaxed) {
            let uuid = ds.as_ref().and_then(|d| d.device_info()).map(|i| i.uuid)
                .unwrap_or_else(|| "global".to_string());
            println!("[ui] SettingsWindow created (uuid={uuid})");
            window.connect_destroy(move |_| {
                println!("[ui] SettingsWindow destroyed (uuid={uuid})");
            });
        }

        Self { window, ds }
    }

    pub(crate) fn present(&self) {
        self.window.present();
    }

    pub(crate) fn window_ref(&self) -> &adw::Window { &self.window }

    /// Returns the UUID of the device this window is for, or None for global settings.
    pub(crate) fn device_uuid(&self) -> Option<String> {
        self.ds.as_ref()
            .and_then(|d| d.device_info())
            .map(|i| i.uuid)
            .filter(|u| !u.is_empty())
    }

}

// ── Per-page builders ─────────────────────────────────────────────────────────

/// Theme dropdown entries: display name paired with the `ThemeMode` it
/// selects, in display order. `None` marks a non-selectable visual divider —
/// rendered as a `gtk::Separator` by `build_theme_list_factory` — used here
/// to split "System …" themes from "RustyWiiM …" themes. Single source of
/// truth for the dropdown's contents, so adding/reordering themes doesn't
/// require touching index numbers anywhere else in this file.
const THEMES: &[(&str, Option<ThemeMode>)] = &[
    ("System", Some(ThemeMode::System)),
    ("System Light", Some(ThemeMode::SystemLight)),
    ("System Dark", Some(ThemeMode::SystemDark)),
    ("", None),
    ("RustyWiiM Dark", Some(ThemeMode::RustyWiiM)),
    ("RustyWiiM Modern", Some(ThemeMode::RustyWiiMModern)),
];

fn theme_index(mode: ThemeMode) -> u32 {
    THEMES.iter().position(|(_, m)| *m == Some(mode)).unwrap_or(0) as u32
}

/// Popup-list factory for `theme_row`. Rows whose `THEMES` entry is `None`
/// show a plain `gtk::Separator` and are made non-selectable; every other row
/// shows a plain label. Only affects the open popup — the closed row keeps
/// the default expression-based display.
///
/// `setup` builds both a label and a separator once per `ListItem` and
/// `bind` only ever toggles which is visible — it must NOT call
/// `list_item.set_child()` itself. `bind` fires repeatedly over a row's
/// lifetime (not just once), including in response to pointer hover, and
/// destroying/replacing the child widget on every call raced with GTK's own
/// hover/crossing tracking on the outgoing widget: `gtk_widget_compute_point:
/// assertion 'GTK_IS_WIDGET (widget)' failed` warnings and, at least once, a
/// hard crash, reproducible just by hovering the popup — no selection needed.
fn build_theme_list_factory() -> gtk::SignalListItemFactory {
    let factory = gtk::SignalListItemFactory::new();
    factory.connect_setup(|_, list_item| {
        // `list_item` is `&glib::Object` at this GTK API level (v4_8+); the
        // concrete type is always `gtk::ListItem` for a list-view factory.
        let list_item = list_item.downcast_ref::<gtk::ListItem>().unwrap();
        let outer = gtk::Box::new(Orientation::Vertical, 0);
        let label = gtk::Label::builder().halign(gtk::Align::Start).build();
        let separator = gtk::Separator::new(Orientation::Horizontal);
        outer.append(&label);
        outer.append(&separator);
        list_item.set_child(Some(&outer));
    });
    factory.connect_bind(|_, list_item| {
        let list_item = list_item.downcast_ref::<gtk::ListItem>().unwrap();
        let (name, mode) = THEMES[list_item.position() as usize];
        let is_separator = mode.is_none();
        let outer = list_item.child().and_downcast::<gtk::Box>().unwrap();
        let label = outer.first_child().and_downcast::<gtk::Label>().unwrap();
        let separator = outer.last_child().and_downcast::<gtk::Separator>().unwrap();
        label.set_label(name);
        label.set_visible(!is_separator);
        separator.set_visible(is_separator);
        list_item.set_selectable(!is_separator);
    });
    factory
}

fn build_appearance_page() -> adw::PreferencesPage {
    let theme = config::with(|cfg| cfg.theme);

    let theme_names: Vec<&str> = THEMES.iter().map(|(name, _)| *name).collect();
    let theme_list = gtk::StringList::new(&theme_names);
    let theme_row = adw::ComboRow::builder()
        .title("Theme")
        .subtitle("Application colour scheme")
        .model(&theme_list)
        .list_factory(&build_theme_list_factory())
        .build();
    theme_row.set_selected(theme_index(theme));

    let mini_modern = config::with(|cfg| cfg.mini_modern);
    let mini_modern_row = adw::SwitchRow::builder()
        .title("Modern Theme for Mini Player")
        .subtitle("Experimental — also apply the blurred background to the mini window")
        .active(mini_modern)
        .sensitive(theme == ThemeMode::RustyWiiMModern)
        .build();
    mini_modern_row.connect_active_notify(move |row| {
        config::update(|cfg| cfg.mini_modern = row.is_active());
        crate::ui::update_art_background_visibility();
    });

    theme_row.connect_selected_notify(glib::clone!(@weak mini_modern_row => move |row| {
            // The separator's THEMES entry is None; it's non-selectable so this
            // shouldn't fire for it, but bail rather than guess if it somehow does.
            let Some(theme) = THEMES.get(row.selected() as usize).and_then(|(_, m)| *m) else { return };
            // Persist before apply_theme(): it calls update_art_background_visibility()
            // internally, which reads config.theme back — updating config first avoids
            // computing visibility off the theme that's about to be replaced.
            config::update(|cfg| cfg.theme = theme);
            crate::ui::apply_theme(theme);
            mini_modern_row.set_sensitive(theme == ThemeMode::RustyWiiMModern);
        }
    ));

    let animations = config::with(|cfg| cfg.animations);
    let animations_row = adw::SwitchRow::builder()
        .title("Animations")
        .subtitle("Title/artist/album slide and artwork flip transitions")
        .active(animations)
        .build();
    animations_row.connect_active_notify(move |row| {
        config::update(|cfg| cfg.animations = row.is_active());
    });

    let accent_hex = config::with(|cfg| cfg.accent_color.clone());
    let accent_dialog = gtk::ColorDialog::new();
    let accent_button = gtk::ColorDialogButton::new(Some(accent_dialog));
    if let Ok(rgba) = gtk::gdk::RGBA::parse(&accent_hex) {
        accent_button.set_rgba(&rgba);
    }
    accent_button.set_valign(gtk::Align::Center);
    let accent_row = adw::ActionRow::builder()
        .title("Highlight color (RustyWiiM themes)")
        .subtitle("Song progress, playback status, play/pause and panel toggle")
        .sensitive(theme == ThemeMode::RustyWiiM || theme == ThemeMode::RustyWiiMModern)
        .build();
    accent_row.add_suffix(&accent_button);
    accent_button.connect_rgba_notify(move |btn| {
        let hex = rgba_to_hex(&btn.rgba());
        config::update(|cfg| cfg.accent_color = hex);
        crate::ui::apply_accent_color();
    });

    theme_row.connect_selected_notify(glib::clone!(@weak accent_row => move |row| {
            let Some(theme) = THEMES.get(row.selected() as usize).and_then(|(_, m)| *m) else { return };
            accent_row.set_sensitive(theme == ThemeMode::RustyWiiM || theme == ThemeMode::RustyWiiMModern);
        }
    ));

    // config::reset_ui_settings() persists the defaults in one write; the
    // widget setters below then push those values into the controls, which
    // fires each control's own connect_*_notify handler and writes the same
    // values back — a no-op given config::update()'s diff-before-persist, so
    // this can't drift the widgets and the persisted config apart. Lives in
    // this page's own actions group (not the individual settings' card),
    // specific to Appearance — it only ever resets these four rows.
    let reset_btn = gtk::Button::builder()
        .label("Reset")
        .valign(gtk::Align::Center)
        .build();
    reset_btn.connect_clicked(glib::clone!(
        @weak theme_row, @weak mini_modern_row, @weak animations_row, @weak accent_button
        => move |_| {
            config::reset_ui_settings();
            let (theme, mini_modern, animations, accent_color) = config::with(|cfg| {
                (cfg.theme, cfg.mini_modern, cfg.animations, cfg.accent_color.clone())
            });
            theme_row.set_selected(theme_index(theme));
            mini_modern_row.set_active(mini_modern);
            animations_row.set_active(animations);
            if let Ok(rgba) = gtk::gdk::RGBA::parse(&accent_color) {
                accent_button.set_rgba(&rgba);
            }
        }
    ));

    let group = adw::PreferencesGroup::builder()
        .title("Appearance")
        .build();
    group.add(&theme_row);
    group.add(&mini_modern_row);
    group.add(&animations_row);
    group.add(&accent_row);

    let actions_group = adw::PreferencesGroup::new();
    let reset_row = adw::ActionRow::builder()
        .title("Reset to Defaults")
        .build();
    reset_row.add_suffix(&reset_btn);
    actions_group.add(&reset_row);

    let page = adw::PreferencesPage::new();
    page.add(&group);
    page.add(&actions_group);
    page
}

// ── Device -> Advanced (playback access-method override) ─────────────────────
//
// Field diagnostics, not a supported end-user-facing feature. Lets a user
// experiencing a playback-state bug on real hardware try forcing playback
// state as a whole away from the device profile's default backend and
// report back what does/doesn't work.

/// Display name paired with the override value it writes. `None` ("Default")
/// must always stay index 0 and must always serialize as an absent field
/// (`config::DeviceConfig::playback_access_override`'s `skip_serializing_if`)
/// — never resolve it to a concrete `AccessMethod` before saving, or a
/// future version's changed default would stop reaching users who left
/// this on "Default".
const ACCESS_METHOD_CHOICES: &[(&str, Option<AccessMethod>)] = &[
    ("Default",       None),
    ("HTTP",          Some(AccessMethod::Http)),
    ("UPnP (polled)", Some(AccessMethod::UpnpPolled)),
];

fn access_method_index(v: Option<AccessMethod>) -> u32 {
    ACCESS_METHOD_CHOICES.iter().position(|(_, m)| *m == v).unwrap_or(0) as u32
}

/// Display name for a *resolved* (never "Default" itself) `AccessMethod` —
/// used to show the device profile's actual default under the row, not to
/// populate the dropdown (that's `ACCESS_METHOD_CHOICES` directly).
fn access_method_label(v: AccessMethod) -> &'static str {
    ACCESS_METHOD_CHOICES.iter()
        .find(|(_, m)| *m == Some(v))
        .map(|(name, _)| *name)
        .unwrap_or("?")
}

/// `default` is this device profile's actual resolved default (from
/// `DeviceCapabilities::playback_access()`), appended to the description
/// text — libadwaita's own subtitle styling already renders at small/dim
/// weight, so no custom CSS is needed. Kept on one line rather than a
/// `\n`-separated second line: `ComboRow`'s subtitle label may not be
/// configured for multi-line wrapping, and this couldn't be visually
/// verified here (no display server in this environment) — a guaranteed-
/// safe single line beats an unverified assumption about wrapping.
fn build_access_row(title: &str, subtitle: &str, default: AccessMethod, current: Option<AccessMethod>) -> adw::ComboRow {
    let names: Vec<&str> = ACCESS_METHOD_CHOICES.iter().map(|(n, _)| *n).collect();
    let row = adw::ComboRow::builder()
        .title(title)
        .subtitle(format!("{subtitle} · Default: {}", access_method_label(default)))
        .model(&gtk::StringList::new(&names))
        .build();
    row.set_selected(access_method_index(current));
    row
}

/// Persist `row`'s selection into this device's `playback_access_override`,
/// then push the recomputed override into `ds` immediately so it takes
/// effect on the next poll tick.
fn wire_access_row(row: &adw::ComboRow, uuid: String, ds: DeviceState) {
    row.connect_selected_notify(move |r| {
        let (_, method) = ACCESS_METHOD_CHOICES[r.selected() as usize];
        config::update(|cfg| cfg.device_mut(&uuid).playback_access_override = method);
        ds.set_playback_access_override(method);
    });
}

fn build_advanced_page(ds: &DeviceState) -> adw::PreferencesPage {
    let uuid = ds.device_info().map(|i| i.uuid).unwrap_or_default();
    let over = config::with(|cfg| cfg.device(&uuid).playback_access_override);
    // This device's actual profile default (not just a single global
    // fallback) — every family currently defaults to `UpnpPolled` (HTTP
    // can't deliver artwork/metadata at all for the non-WiiM ones, and WiiM
    // switched over once UPnP polling proved out), but `Http` is still
    // selectable per-device here for diagnosis if a specific unit's UPnP
    // path turns out broken.
    let defaults = ds.capabilities().map(|c| c.playback_access()).unwrap_or(AccessMethod::Http);

    let player_status_row = build_access_row(
        "Player Status", "Playback informations",
        defaults, over,
    );
    wire_access_row(&player_status_row, uuid.clone(), ds.clone());

    let group = adw::PreferencesGroup::builder()
        .title("Playback Access Method")
        .description(
            "Override which backend supplies playback state. \
             Leave this on \"Default\" unless you're troubleshooting \
             a specific problem."
        )
        .build();
    group.add(&player_status_row);

    let page = adw::PreferencesPage::new();
    page.add(&group);
    page
}

// ── Device -> About ───────────────────────────────────────────────────────────
//
// Not live — a snapshot of whatever `DeviceState::device_info()`/
// `capabilities()` already have cached at the moment the settings window
// opens, same as everything else that reads those accessors. Reopen the
// window to see fresher values.

fn about_row(title: &str, value: &str) -> adw::ActionRow {
    adw::ActionRow::builder()
        .title(title)
        .subtitle(if value.is_empty() { "—" } else { value })
        .build()
}

fn build_about_page(ds: &DeviceState) -> adw::PreferencesPage {
    let group = adw::PreferencesGroup::builder().title("Device").build();

    let Some(info) = ds.device_info() else {
        group.add(&about_row("Status", "Not connected yet"));
        let page = adw::PreferencesPage::new();
        page.add(&group);
        return page;
    };
    let caps = ds.capabilities();

    let vendor_model = match &caps {
        Some(c) => format!("{} · {}", c.vendor.display_name(), c.model),
        None    => String::new(),
    };
    let network = match ds.netstat() {
        Some(0) => "Ethernet".to_string(),
        Some(2) => match ds.rssi() {
            Some(rssi) => format!("Wi-Fi ({rssi} dBm)"),
            None       => "Wi-Fi".to_string(),
        },
        _ => String::new(),
    };

    group.add(&about_row("Device Name", &info.device_name));
    group.add(&about_row("Vendor / Model", &vendor_model));
    group.add(&about_row("Firmware", &info.firmware));
    group.add(&about_row("IP Address", info.ip_addr()));
    group.add(&about_row("UUID", &info.uuid));
    group.add(&about_row("Project", &info.project));
    group.add(&about_row("Hardware", &info.hardware));
    group.add(&about_row("Network", &network));

    let page = adw::PreferencesPage::new();
    page.add(&group);
    page
}
