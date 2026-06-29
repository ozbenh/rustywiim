// Settings window — non-modal, GNOME-style split layout.
//
// Left sidebar: section-titled navigation list (gtk::ListBox .navigation-sidebar).
// Right panel:  gtk::Stack of adw::PreferencesPage widgets, one per topic.
// Access to DeviceState is threaded through for pages that need live device data.

#![allow(deprecated)] // glib clone! @strong syntax

use adw::prelude::*;
use gtk::Orientation;

use crate::config::{Config, ThemeMode};
use crate::device_state::DeviceState;

// ── Public handle ─────────────────────────────────────────────────────────────

pub(super) struct SettingsWindow {
    window: adw::Window,
}

impl SettingsWindow {
    pub(super) fn new(ds: &DeviceState, parent: &adw::ApplicationWindow) -> Self {
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

        let appearance_page = build_appearance_page(ds);
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

        let window = adw::Window::builder()
            .title("Settings")
            .transient_for(parent)
            .default_width(720)
            .default_height(520)
            .modal(false)
            .build();
        window.set_content(Some(&toolbar_view));

        Self { window }
    }

    pub(super) fn present(&self) {
        self.window.present();
    }
}

// ── Per-page builders ─────────────────────────────────────────────────────────

fn build_appearance_page(_ds: &DeviceState) -> adw::PreferencesPage {
    let cfg = Config::load();

    let theme_list = gtk::StringList::new(&["System", "System Light", "System Dark", "RustyWiiM"]);
    let theme_row = adw::ComboRow::builder()
        .title("Theme")
        .subtitle("Application colour scheme")
        .model(&theme_list)
        .build();
    theme_row.set_selected(match cfg.theme {
        ThemeMode::System      => 0,
        ThemeMode::SystemLight => 1,
        ThemeMode::SystemDark  => 2,
        ThemeMode::RustyWiiM   => 3,
    });
    theme_row.connect_selected_notify(move |row| {
        let theme = match row.selected() {
            0 => ThemeMode::System,
            1 => ThemeMode::SystemLight,
            2 => ThemeMode::SystemDark,
            _ => ThemeMode::RustyWiiM,
        };
        crate::ui::apply_theme(theme);
        let mut cfg = Config::load();
        cfg.theme = theme;
        cfg.save();
    });

    let group = adw::PreferencesGroup::builder()
        .title("Appearance")
        .build();
    group.add(&theme_row);

    let page = adw::PreferencesPage::new();
    page.add(&group);
    page
}
