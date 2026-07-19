//! Kiosk mode's single dedicated window: a fullscreen, undecorated surface
//! showing exactly one device at a time, meant for single-surface kiosk
//! compositors that can't juggle rustywiim's normal multi-window setup
//! (one `DeviceWindow` per open device plus a separate `DiscoveryWindow`).
//!
//! Not a GObject — a plain chrome struct like `DiscoveryWindow`, since
//! this owns window lifecycle/CSS/keyboard wiring, not a self-contained
//! bindable widget. Shows exactly one device's basic `PlaybackView` at a
//! time, deliberately minimal for a first cut (no side panel, no
//! `ArtBackground`); a transparent top-right button showing the bound
//! device's name opens a popover containing a `DeviceListView` to switch
//! devices.
//!
//! Keyboard shortcuts are owned entirely by this window, not shared with
//! `DeviceWindow`'s own controller — "K" exits kiosk mode here; there is
//! deliberately no "M" (kiosk has no mini mode). The common transport/
//! volume keys delegate to `views::common::handle_transport_key()`, the
//! same helper `DeviceWindow` uses, rather than being reimplemented here.
//!
//! Not yet wired into `AppState` (no enter/exit kiosk mode, no menu/key/
//! `--kiosk` triggers) — that's the next step; `#![allow(dead_code)]`
//! below is temporary until it lands.
#![allow(dead_code)]

use std::cell::RefCell;
use std::rc::Rc;

use adw::prelude::*;
use glib::clone;

use crate::device::discovery_manager::DiscoveryManager;
use crate::device::state::{DeviceState, FullModeGuard};
use crate::ui::icons::IconSet;
use crate::ui::views;
use crate::ui::views::devlist::DeviceListView;
use crate::ui::views::playback_full::PlaybackView;

/// The currently-bound device's view plus the `FullModeGuard` keeping its
/// polling at full fidelity for as long as Kiosk mode is looking at it —
/// dropping this (on unbind/rebind, or the whole window closing) releases
/// it, same as a `DeviceWindow` does for its own `DeviceState`.
struct BoundDevice {
    key:        String,
    ds:         DeviceState,
    view:       PlaybackView,
    _full_mode: FullModeGuard,
}

pub(crate) struct KioskWindow {
    window:     adw::ApplicationWindow,
    manager:    DiscoveryManager,
    icons:      Rc<IconSet>,
    overlay:    gtk::Overlay,
    placeholder: gtk::Widget,
    device_btn: gtk::Button,
    popover:    gtk::Popover,
    bound:      RefCell<Option<BoundDevice>>,
}

impl KioskWindow {
    pub(crate) fn new(
        app:        &adw::Application,
        manager:    &DiscoveryManager,
        icons:      &Rc<IconSet>,
        exit_kiosk: Rc<dyn Fn()>,
    ) -> Rc<Self> {
        let window = adw::ApplicationWindow::builder()
            .application(app)
            .title("RustyWiiM")
            .decorated(false)
            .resizable(false)
            .css_classes(["kiosk-window"])
            .build();

        let placeholder = gtk::Label::builder()
            .label("No device selected")
            .css_classes(["dim-label"])
            .halign(gtk::Align::Center)
            .valign(gtk::Align::Center)
            .build()
            .upcast::<gtk::Widget>();

        let overlay = gtk::Overlay::new();
        overlay.set_child(Some(&placeholder));

        let device_btn = gtk::Button::builder()
            .label("Select device")
            .css_classes(["kiosk-device-btn"])
            .halign(gtk::Align::End)
            .valign(gtk::Align::Start)
            .build();
        overlay.add_overlay(&device_btn);

        let device_list = DeviceListView::new(manager, icons);
        let popover = gtk::Popover::new();
        popover.add_css_class("kiosk-devlist-popover");
        popover.set_child(Some(&device_list));
        popover.set_parent(&device_btn);

        window.set_content(Some(&overlay));

        let this = Rc::new(Self {
            window: window.clone(),
            manager: manager.clone(),
            icons: Rc::clone(icons),
            overlay,
            placeholder,
            device_btn: device_btn.clone(),
            popover: popover.clone(),
            bound: RefCell::new(None),
        });

        device_btn.connect_clicked(clone!(#[weak] popover, move |_| {
            if popover.is_visible() { popover.popdown(); } else { popover.popup(); }
        }));
        device_list.connect_device_selected({
            let weak = Rc::downgrade(&this);
            move |_, key| {
                let Some(this) = weak.upgrade() else { return };
                this.bind_device(Some(key));
                this.popover.popdown();
            }
        });

        // Keyboard: "K" exits kiosk mode (this window's own key, not shared
        // with DeviceWindow's controller — no "M" here at all, kiosk has no
        // mini mode). Everything else delegates to the shared transport-key
        // helper against whatever's currently bound.
        let key_ctrl = gtk::EventControllerKey::new();
        key_ctrl.set_propagation_phase(gtk::PropagationPhase::Capture);
        key_ctrl.connect_key_pressed({
            let weak = Rc::downgrade(&this);
            move |_, keyval, _keycode, state| {
                let Some(this) = weak.upgrade() else { return glib::Propagation::Proceed };
                if state.intersects(gtk::gdk::ModifierType::CONTROL_MASK | gtk::gdk::ModifierType::ALT_MASK) {
                    return glib::Propagation::Proceed;
                }
                if let gtk::gdk::Key::k | gtk::gdk::Key::K = keyval {
                    exit_kiosk();
                    return glib::Propagation::Stop;
                }
                let Some((ds, view)) = this.bound.borrow().as_ref().map(|b| (b.ds.clone(), b.view.clone())) else {
                    return glib::Propagation::Proceed;
                };
                let (prev, play, next) = view.transport_buttons();
                views::common::handle_transport_key(&ds, &view.volume(), &prev, &next, &play, keyval)
            }
        });
        window.add_controller(key_ctrl);

        this
    }

    /// Resolves `key` (a `device_key()` result — see `DiscoveryManager`)
    /// through `manager.device_state_for()`, tears down whatever device was
    /// previously bound (dropping its `PlaybackView` and `FullModeGuard`
    /// together — `views/*`'s `dispose()` handles the view's own handler
    /// cleanup), and builds a fresh `PlaybackView` for the new one. `None`
    /// shows the "no device selected" placeholder instead.
    pub(crate) fn bind_device(&self, key: Option<&str>) {
        // Release whichever device was bound before, regardless of what
        // (if anything) replaces it — mirrors DeviceWindow's own
        // set_window_open bookkeeping so DiscoveryManager's prune logic
        // doesn't think a stale device is still "open" here.
        if let Some(old) = self.bound.borrow_mut().take() {
            self.manager.set_window_open(&old.key, false);
        }

        let Some(key) = key else {
            self.overlay.set_child(Some(&self.placeholder));
            self.device_btn.set_label("Select device");
            return;
        };
        let Some(ds) = self.manager.device_state_for(key) else {
            self.overlay.set_child(Some(&self.placeholder));
            self.device_btn.set_label("Select device");
            return;
        };
        self.manager.set_window_open(key, true);
        let name = self.manager.entry_for(key).map(|e| e.name).unwrap_or_else(|| key.to_string());
        self.device_btn.set_label(&name);

        let view = PlaybackView::new(&ds, &self.icons, None);
        // Views start inactive (views/mod.rs's shared contract) — this is
        // what actually performs the initial render.
        view.set_active(true);
        self.overlay.set_child(Some(&view));
        *self.bound.borrow_mut() = Some(BoundDevice {
            key: key.to_string(),
            _full_mode: ds.acquire_full(),
            ds,
            view,
        });
    }

    pub(crate) fn present(&self) {
        self.window.present();
        self.window.fullscreen();
    }

    pub(crate) fn close(&self) {
        if let Some(old) = self.bound.borrow_mut().take() {
            self.manager.set_window_open(&old.key, false);
        }
        self.window.close();
    }
}

