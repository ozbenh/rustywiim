#![allow(deprecated)] // glib clone! old-style @strong syntax

use std::cell::RefCell;
use std::rc::Rc;

use adw::prelude::*;
use glib::clone;
use gtk::{Button, Orientation};

use crate::device::api::TlsMode;
use crate::device::state::DeviceState;
use crate::device::discovery;
use super::select_device;

pub(super) fn show_manual_ip_dialog(
    window:     &adw::ApplicationWindow,
    ds:         &DeviceState,
    dev_btn:    &gtk::MenuButton,
    manual_btn: &Button,
    saved_ip:   &Rc<RefCell<String>>,
) {
    let current = saved_ip.borrow().clone();
    let dialog = adw::AlertDialog::builder()
        .heading("Connect to WiiM")
        .body("Enter the IP address of your WiiM device.")
        .close_response("cancel")
        .build();
    dialog.add_response("cancel", "Cancel");
    dialog.add_response("connect", "Connect");
    dialog.set_response_appearance("connect", adw::ResponseAppearance::Suggested);
    dialog.set_default_response(Some("connect"));

    let entry = gtk::Entry::builder()
        .placeholder_text("192.168.1.x")
        .text(&current)
        .activates_default(true)
        .build();
    dialog.set_extra_child(Some(&entry));

    dialog.connect_response(None, clone!(
        @strong ds, @strong entry, @strong saved_ip, @strong dev_btn, @strong manual_btn
            => move |_dlg, resp| {
                if resp == "connect" {
                    let ip = entry.text().to_string();
                    if !ip.is_empty() {
                        *saved_ip.borrow_mut() = ip.clone();
                        let label = format!("Manual: {ip}");
                        dev_btn.set_label(&label);
                        manual_btn.set_label(&label);
                        select_device(&ds, &ip, "", TlsMode::HttpsWiiM);
                    }
                }
            }
    ));
    dialog.present(Some(window));
}

pub(super) fn build_device_popover(
    devs:      &[discovery::DiscoveredDevice],
    ds:        &DeviceState,
    dev_btn:   &gtk::MenuButton,
    window:    &adw::ApplicationWindow,
    saved_ip:  &Rc<RefCell<String>>,
    on_select: impl Fn(&str) + Clone + 'static,
) -> gtk::Popover {
    let vbox = gtk::Box::new(Orientation::Vertical, 0);
    vbox.set_margin_top(4);
    vbox.set_margin_bottom(4);

    if devs.is_empty() {
        let lbl = gtk::Label::builder()
            .label("No devices found")
            .sensitive(false)
            .margin_top(6).margin_bottom(6).margin_start(12).margin_end(12)
            .build();
        vbox.append(&lbl);
    } else {
        for d in devs {
            let label    = format!("{} ({})", d.name, d.ip);
            let ip       = d.ip.clone();
            let uuid     = d.uuid.clone();
            let tls_mode = d.tls_mode;
            let on_sel   = on_select.clone();
            let btn = Button::builder().label(&label).css_classes(["flat"]).build();
            btn.connect_clicked(clone!(
                @strong ds, @strong dev_btn, @strong label
                    => move |_| {
                        on_sel(&uuid);
                        dev_btn.set_label(&label);
                        dev_btn.popdown();
                        select_device(&ds, &ip, &uuid, tls_mode);
                    }
            ));
            vbox.append(&btn);
        }
    }

    vbox.append(&gtk::Separator::new(Orientation::Horizontal));

    let saved = saved_ip.borrow().clone();
    let manual_label = if !saved.is_empty() && !devs.iter().any(|d| d.ip == saved) {
        format!("Manual: {saved}")
    } else {
        "Manual IP…".to_string()
    };
    let manual_btn = Button::builder().label(&manual_label).css_classes(["flat"]).build();
    manual_btn.connect_clicked(clone!(
        @strong ds, @strong dev_btn, @strong window, @strong saved_ip, @strong manual_btn
            => move |_| {
                dev_btn.popdown();
                show_manual_ip_dialog(&window, &ds, &dev_btn, &manual_btn, &saved_ip);
            }
    ));
    vbox.append(&manual_btn);

    let popover = gtk::Popover::new();
    popover.set_child(Some(&vbox));
    popover
}

