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

        // Select "Appearance" by default
        sidebar_list.select_row(sidebar_list.row_at_index(0).as_ref());

        sidebar_list.connect_row_selected({
            let stack = content_stack.clone();
            move |_, row| {
                if let Some(row) = row {
                    let name = match row.index() {
                        0 => "appearance",
                        _ => return,
                    };
                    stack.set_visible_child_name(name);
                }
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

fn build_appearance_page() -> adw::PreferencesPage {
    let theme = config::with(|cfg| cfg.theme);

    let theme_list = gtk::StringList::new(
        &["System", "System Light", "System Dark", "RustyWiiM", "RustyWiiM Modern"]);
    let theme_row = adw::ComboRow::builder()
        .title("Theme")
        .subtitle("Application colour scheme")
        .model(&theme_list)
        .build();
    theme_row.set_selected(match theme {
        ThemeMode::System         => 0,
        ThemeMode::SystemLight    => 1,
        ThemeMode::SystemDark     => 2,
        ThemeMode::RustyWiiM      => 3,
        ThemeMode::RustyWiiMModern => 4,
    });

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
            let theme = match row.selected() {
                0 => ThemeMode::System,
                1 => ThemeMode::SystemLight,
                2 => ThemeMode::SystemDark,
                3 => ThemeMode::RustyWiiM,
                _ => ThemeMode::RustyWiiMModern,
            };
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
        .title("Highlight Color")
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
            let theme = match row.selected() {
                0 => ThemeMode::System,
                1 => ThemeMode::SystemLight,
                2 => ThemeMode::SystemDark,
                3 => ThemeMode::RustyWiiM,
                _ => ThemeMode::RustyWiiMModern,
            };
            accent_row.set_sensitive(theme == ThemeMode::RustyWiiM || theme == ThemeMode::RustyWiiMModern);
        }
    ));

    let group = adw::PreferencesGroup::builder()
        .title("Appearance")
        .build();
    group.add(&theme_row);
    group.add(&mini_modern_row);
    group.add(&animations_row);
    group.add(&accent_row);

    let page = adw::PreferencesPage::new();
    page.add(&group);
    page
}
