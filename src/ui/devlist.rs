/// Device-list window — chrome (header, "Add device" dialog, menu, window
/// lifecycle/geometry persistence) around one embedded
/// `views::devlist::DeviceListView`, which owns the actual list rendering.
/// Owns no tracking state of its own; see
/// `device::manager::DeviceManager`'s doc comment for the full backend story
/// (SSDP consumption, presence, config seed-in/report-out).

use std::rc::Rc;

use adw::prelude::*;
use glib::clone;
use gtk::{glib, Orientation};

use crate::config;
use crate::device::discovery::{DiscoveredDevice, ProbeFailure};
use crate::device::manager::{DeviceManager, ManagedEntry};
use crate::device::state::DeviceState;
use crate::ui::icons::IconSet;
use super::views::devlist::DeviceListView;

pub struct DiscoveryWindow {
    pub window: adw::ApplicationWindow,
}

impl DiscoveryWindow {
    pub fn new(
        app:                  &adw::Application,
        manager:              &DeviceManager,
        open_device:          Rc<dyn Fn(&ManagedEntry)>,
        enter_kiosk:          Rc<dyn Fn()>,
        open_preferences:     Rc<dyn Fn()>,
        open_device_settings: Rc<dyn Fn(DeviceState)>,
        forget_device:        Rc<dyn Fn(&str)>,
    ) -> Self {
        let (init_w, init_h) = config::with(|cfg| (
            if cfg.discovery_window_width  > 0 { cfg.discovery_window_width  } else { 500 },
            if cfg.discovery_window_height > 0 { cfg.discovery_window_height } else { 440 },
        ));

        // One IconSet for the whole window, shared with the embedded
        // DeviceListView — same pattern as a device window's own `icons`
        // field, not rebuilt per row/rebuild.
        let icons = Rc::new(IconSet::load());
        let window = adw::ApplicationWindow::builder()
            .application(app)
            .title("RustyWiiM")
            .default_width(init_w)
            .default_height(init_h)
            .build();

        let header = adw::HeaderBar::new();

        // Custom title widget so the spinner can sit immediately to the right
        // of the "Scanning…" subtitle text.  adw::WindowTitle has no widget
        // slot in its subtitle row so we build the layout by hand.
        let title_label = gtk::Label::builder()
            .label("RustyWiiM")
            .css_classes(["title"])
            .build();

        let subtitle_label = gtk::Label::builder()
            .label("Scanning\u{2026}")
            .css_classes(["subtitle"])
            .build();

        let spinner = gtk::Spinner::builder()
            .spinning(true)
            .build();
        spinner.set_size_request(12, 12);

        let subtitle_row = gtk::Box::builder()
            .orientation(Orientation::Horizontal)
            .spacing(4)
            .halign(gtk::Align::Center)
            .build();
        subtitle_row.append(&subtitle_label);
        subtitle_row.append(&spinner);

        let title_box = gtk::Box::builder()
            .orientation(Orientation::Vertical)
            .valign(gtk::Align::Center)
            .build();
        title_box.append(&title_label);
        title_box.append(&subtitle_row);

        header.set_title_widget(Some(&title_box));

        let add_btn = gtk::Button::builder()
            .icon_name("list-add-symbolic")
            .tooltip_text("Add device by IP address…")
            .build();
        header.pack_end(&add_btn);
        header.pack_end(&super::menu::build_menu_button(false));
        super::wire_window_actions(&window, Rc::clone(&open_preferences));

        // Device list — the actual rendering/live-updating is entirely
        // `DeviceListView`'s job now; this window just embeds it and
        // decides what "a row was selected" means (open a device window).
        let device_list = DeviceListView::new(manager, &icons);

        // When this window is behind another (typically a device window)
        // and the user clicks a row, the window manager raises/focuses
        // *this* window as a side effect of that very click before GTK
        // delivers the row-activation — so without this guard, one click
        // both brought the picker forward *and* raised the clicked row's
        // device window on top of it again, defeating the click entirely.
        // Track when this window last became active; a row-activation
        // arriving within `REFOCUS_GRACE` of that is treated as "just
        // regained focus from this click" and swallowed (bringing the
        // picker forward is enough) — only a second, deliberate click once
        // the picker is already the active window goes through.
        const REFOCUS_GRACE: std::time::Duration = std::time::Duration::from_millis(300);
        let last_activated: Rc<std::cell::Cell<Option<std::time::Instant>>> = Rc::new(std::cell::Cell::new(None));
        window.connect_notify_local(Some("is-active"), clone!(#[strong] last_activated, move |win, _| {
            if win.is_active() {
                last_activated.set(Some(std::time::Instant::now()));
            }
        }));
        device_list.connect_device_selected(clone!(#[strong] manager, #[strong] open_device, #[strong] last_activated, move |_, key| {
            if last_activated.get().is_some_and(|t| t.elapsed() < REFOCUS_GRACE) {
                return;
            }
            if let Some(entry) = manager.entry_for(key) {
                open_device(&entry);
            }
        }));

        // A row's settings cog — a standalone device's own row, or a group
        // member's line (that device has no window of its own — opening
        // one from this list opens the group — so this is the only way to
        // reach its own settings).
        device_list.connect_device_settings(clone!(
            #[strong] manager, #[strong] open_device_settings, move |_, key| {
                if let Some(ds) = manager.device_state_for(key) {
                    open_device_settings(ds);
                }
            }
        ));

        // Offline-only trashcan — see DeviceListView's own doc comment.
        // Confirms first (`crate::ui::confirm_forget_device()`): removal
        // also drops the device's per-device settings, not just its list
        // entry.
        device_list.connect_device_forget(clone!(
            #[strong] manager, #[strong] forget_device, #[strong] window, move |_, key| {
                let name = crate::ui::display_name_for(key, manager.device_state_for(key).as_ref());
                let name = if name.is_empty() { key.to_string() } else { name };
                crate::ui::confirm_forget_device(&window, &name, key.to_string(), Rc::clone(&forget_device));
            }
        ));

        let content = gtk::Box::builder()
            .orientation(Orientation::Vertical)
            .build();
        content.append(&device_list);

        let toolbar_view = adw::ToolbarView::new();
        toolbar_view.add_top_bar(&header);
        toolbar_view.set_content(Some(&content));
        window.set_content(Some(&toolbar_view));
        // Same plain CSS gradient background as Settings/Preferences/EQ
        // under RustyWiiM Modern (`modern.css`'s `window.modern-bg-window`
        // rule) — added after construction/`set_content()`, not via the
        // builder's `.css_classes([...])`, for the same reason those
        // windows do: the builder property would replace the whole class
        // list libadwaita sets up during construction (rounded corners/CSD
        // shadow), not just add to it.
        window.add_css_class("modern-bg-window");

        // Scanning indicator: clear only when the SSDP scan cycle reports in.
        let scanning = Rc::new(std::cell::Cell::new(true));
        manager.connect_scan_complete(clone!(
            #[strong] subtitle_row, #[strong] scanning, #[strong] spinner
               , move || {
                    if scanning.replace(false) {
                        spinner.set_spinning(false);
                        subtitle_row.set_visible(false);
                    }
                }
        ));

        // "Add device" button.
        add_btn.connect_clicked(clone!(#[strong] manager, #[strong] window, move |_| {
            Self::show_add_dialog(&manager, &window);
        }));

        // win.close action — lets Ctrl-W (set app-wide) close this window.
        {
            let close_act = gtk::gio::SimpleAction::new("close", None);
            close_act.connect_activate(clone!(#[strong] window, move |_, _| { window.close(); }));
            window.add_action(&close_act);
        }

        // win.kiosk — enters Kiosk mode unbound (no device pre-selected).
        {
            let kiosk_act = gtk::gio::SimpleAction::new("kiosk", None);
            kiosk_act.connect_activate(move |_, _| { enter_kiosk(); });
            window.add_action(&kiosk_act);
        }

        // Hide when other windows are visible; quit (propagate) when last.
        window.connect_close_request(clone!(#[strong(rename_to = _window)] window, move |w| {
            let (ww, wh) = (w.width(), w.height());
            config::update(|cfg| {
                // Not cleared while entering Kiosk mode — this close is a
                // transient side effect of that transition, not the user
                // actually dismissing the device list, and `exit_kiosk()`
                // needs `discovery_open` to still reflect reality
                // afterward (mirrors `DeviceWindowInner::cleanup()`'s
                // identical `ENTERING_KIOSK` guard on `window_open`).
                if !crate::ui::ENTERING_KIOSK.load(std::sync::atomic::Ordering::Relaxed) {
                    cfg.discovery_open = false;
                }
                if ww > 0 { cfg.discovery_window_width  = ww; }
                if wh > 0 { cfg.discovery_window_height = wh; }
            });

            let others_visible = w.application().map_or(false, |app| {
                app.windows().iter().any(|other| {
                    other.upcast_ref::<gtk::Widget>() != w.upcast_ref::<gtk::Widget>()
                        && other.is_visible()
                })
            });
            if others_visible {
                w.set_visible(false);
                glib::Propagation::Stop
            } else {
                glib::Propagation::Proceed
            }
        }));

        Self { window }
    }

    pub fn present(&self) {
        config::update(|cfg| cfg.discovery_open = true);
        self.window.present();
    }

    /// `PromptEntry`-hosted, not an `adw::AlertDialog`, since 2026-07-28 —
    /// same swap `eq/chrome.rs`'s `show_preset_name_prompt()` made earlier,
    /// for the same reason: it's the one path in this codebase that also
    /// gets the on-screen keyboard (`KeyboardType::Numeric` — an IP address
    /// is digits and dots).
    fn show_add_dialog(manager: &DeviceManager, parent: &adw::ApplicationWindow) {
        let entry = crate::ui::prompt_entry::PromptEntry::new();
        entry.set_prompt("Enter the IP address of a WiiM device:");
        entry.set_placeholder("192.168.1.x");
        entry.set_ok_label("Add");
        entry.set_keyboard_type(crate::ui::prompt_entry::KeyboardType::Numeric);

        let window = crate::ui::prompt_entry::present_prompt_window("Add Device", &entry);

        entry.connect_confirmed(clone!(
            #[strong] manager, #[strong] window, #[strong] parent,
            move |_, ip| {
                window.close();

                let rt = manager.rt();
                let (tx, rx) = async_channel::bounded::<Result<DiscoveredDevice, ProbeFailure>>(1);
                let ip2 = ip.clone();
                rt.spawn(async move {
                    let result = crate::device::discovery::DiscoveryService::probe_device(&ip2).await;
                    let _ = tx.send(result).await;
                });

                glib::spawn_future_local(clone!(#[strong] manager, #[strong] parent, async move {
                    match rx.recv().await {
                        Ok(Ok(dev)) => manager.add_manual(dev.name, dev.ip, dev.uuid, dev.tls_mode),
                        // Full per-attempt connection errors already went to the
                        // debug log (--debug=discovery) from probe_device()'s own
                        // identify_device()/probe_api() calls — this is just the
                        // short summary, matching discovery.rs's give-up line.
                        Ok(Err(failure)) => Self::show_add_failure_dialog(&parent, &failure.describe(&ip)),
                        // Sender dropped — nothing left to report to.
                        Err(_) => {}
                    }
                }));
            }
        ));
        entry.connect_cancelled(clone!(#[strong] window, move |_| window.close()));
    }

    /// One-button "couldn't add that device" dialog — `probe_device()`'s
    /// `ProbeFailure::describe()` already reads as a complete sentence, so
    /// this needs no heading beyond the generic one.
    fn show_add_failure_dialog(parent: &adw::ApplicationWindow, message: &str) {
        let dialog = adw::AlertDialog::builder()
            .heading("Couldn't Add Device")
            .body(message)
            .close_response("ok")
            .build();
        dialog.add_response("ok", "OK");
        dialog.set_default_response(Some("ok"));
        dialog.present(Some(parent));
    }
}
