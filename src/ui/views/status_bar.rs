//! # StatusBarView
//!
//! The window's bottom status bar: device info centred, BLE-remote
//! presence/battery on the left, IP + network icon on the right. Follows
//! the view lifecycle contract (see `views/mod.rs`): subscribes to
//! `device-changed`/`network-changed`/`remote-changed` itself, early-
//! returns while inactive, full refresh (including the offline/
//! disconnected rendering) on activation. Previously
//! `device_window/chrome.rs`'s `build_bottom_bar()` plus the
//! window-driven `update_network_icon()`/`update_remote_display()`/the
//! bottom-bar-specific half of `apply_device_info()`/`reset_device_ui()` —
//! window title/mini-label/connecting-spinner stay window chrome, not part
//! of this view.

pub mod imp {
    use std::cell::{Cell, OnceCell, RefCell};

    use adw::prelude::*;
    use adw::subclass::prelude::*;
    use gtk::glib;

    use crate::device::state::DeviceState;

    #[derive(Default)]
    pub struct StatusBarView {
        pub(super) ds:           OnceCell<DeviceState>,
        pub(super) handlers:     RefCell<Vec<glib::SignalHandlerId>>,
        pub(super) active:       Cell<bool>,
        pub(super) dev_info:     OnceCell<gtk::Label>,
        pub(super) ip_label:     OnceCell<gtk::Label>,
        pub(super) net_icon:     OnceCell<gtk::Image>,
        pub(super) remote_icon:  OnceCell<gtk::Image>,
        pub(super) remote_label: OnceCell<gtk::Label>,
        pub(super) bar:          OnceCell<gtk::CenterBox>,
        /// Unique per-instance CSS class + provider for `set_scale()` —
        /// same technique `playback_full.rs`'s `apply_wide_right_scale()`
        /// uses, so this bar's fonts/icon sizes can be screen-proportional
        /// too instead of the fixed `window.kiosk-window`-scoped values
        /// (dark.css/system.css) that used to be the only sizing Kiosk
        /// mode got.
        pub(super) scale_class:    OnceCell<String>,
        pub(super) scale_provider: OnceCell<gtk::CssProvider>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for StatusBarView {
        const NAME: &'static str = "StatusBarView";
        type Type = super::StatusBarView;
        type ParentType = adw::Bin;
    }

    impl ObjectImpl for StatusBarView {
        fn dispose(&self) {
            if let Some(ds) = self.ds.get() {
                for id in self.handlers.take() {
                    ds.disconnect(id);
                }
            }
        }
    }
    impl WidgetImpl for StatusBarView {}
    impl BinImpl for StatusBarView {}
}

use std::rc::Rc;

use adw::prelude::*;
use adw::subclass::prelude::*;
use gtk::glib;
use gtk::{Align, Box as GtkBox, Label, Orientation};

use crate::device::state::DeviceState;
use crate::ui::icons::IconSet;

glib::wrapper! {
    pub struct StatusBarView(ObjectSubclass<imp::StatusBarView>)
        @extends adw::Bin, gtk::Widget;
}

impl StatusBarView {
    /// Build the status bar bound to `ds`. Starts **inactive** — the
    /// owner's first `set_active(true)` performs the initial render.
    /// `large` (Kiosk mode only) scales up the one piece pure CSS can't
    /// reach: `remote_icon`'s pixel size, since it's a pre-rendered
    /// `gdk::Paintable` set via `set_pixel_size()` rather than an
    /// icon-name lookup that `-gtk-icon-size` could rescale — everything
    /// else (fonts, `net_icon`'s icon size, margins) is handled by
    /// `window.kiosk-window`-scoped CSS instead.
    pub(crate) fn new(ds: &DeviceState, icons: &Rc<IconSet>, large: bool) -> Self {
        let obj: Self = glib::Object::new();
        obj.build(ds, icons, large);
        obj
    }

    fn build(&self, ds: &DeviceState, icons: &Rc<IconSet>, large: bool) {
        let imp = self.imp();
        imp.ds.set(ds.clone()).unwrap();

        let dev_info = Label::builder()
            .css_classes(["device-info"]).halign(Align::Center)
            .hexpand(true)
            .margin_top(4).margin_bottom(4).build();

        // "ip-label" alongside "dim-label" gives modern.css a hook to match
        // this label's exact size/treatment to "device-info" (which doesn't
        // share dim-label's font-size with the pos/dur time labels that
        // also use it).
        let ip_label = Label::builder()
            .css_classes(["dim-label", "ip-label"])
            .margin_end(6).margin_top(4).margin_bottom(4)
            .visible(false)
            .build();

        let net_icon = gtk::Image::builder()
            .icon_size(gtk::IconSize::Normal)
            .css_classes(["net-icon"])
            // 1px less than ip_label's margin_top — the icon otherwise
            // renders a hair lower than the label's text baseline.
            .margin_end(8).margin_top(3).margin_bottom(5)
            .visible(false)
            .build();

        let bottom_end = GtkBox::new(Orientation::Horizontal, 0);
        bottom_end.append(&ip_label);
        bottom_end.append(&net_icon);

        // BLE remote presence/battery — hidden until the first `getStatusEx`
        // result confirms a remote is actually connected (see
        // `update_remote_display()`).
        let remote_icon = gtk::Image::from_paintable(Some(icons.remote_paintable()));
        // 21px: net_icon's IconSize::Normal (16px) plus 2px, then a further
        // +3px per request. +50% again for Kiosk mode's larger bottom bar.
        remote_icon.set_pixel_size(if large { 42 } else { 28 });
        remote_icon.add_css_class("remote-icon");
        remote_icon.set_margin_start(8);
        remote_icon.set_margin_top(4);
        remote_icon.set_margin_bottom(4);
        remote_icon.set_visible(false);

        // Same classes as ip_label above (not just "dim-label") so it's
        // displayed identically.
        let remote_label = Label::builder()
            .css_classes(["dim-label", "ip-label"])
            .margin_start(4).margin_top(4).margin_bottom(4)
            .visible(false)
            .build();

        let bottom_start = GtkBox::new(Orientation::Horizontal, 0);
        bottom_start.append(&remote_icon);
        bottom_start.append(&remote_label);

        let bar = gtk::CenterBox::new();
        bar.set_start_widget(Some(&bottom_start));
        bar.set_center_widget(Some(&dev_info));
        bar.set_end_widget(Some(&bottom_end));
        self.set_child(Some(&bar));

        imp.dev_info.set(dev_info).unwrap();
        imp.ip_label.set(ip_label).unwrap();
        imp.net_icon.set(net_icon).unwrap();
        imp.remote_icon.set(remote_icon).unwrap();
        imp.remote_label.set(remote_label).unwrap();
        imp.bar.set(bar).unwrap();

        let id = ds.connect_device_changed({
            let weak = self.downgrade();
            move |_| {
                let Some(obj) = weak.upgrade() else { return };
                if !obj.imp().active.get() { return; }
                obj.refresh();
            }
        });
        imp.handlers.borrow_mut().push(id);

        let id = ds.connect_network_changed({
            let weak = self.downgrade();
            move |_| {
                let Some(obj) = weak.upgrade() else { return };
                if !obj.imp().active.get() { return; }
                obj.update_network_icon();
            }
        });
        imp.handlers.borrow_mut().push(id);

        let id = ds.connect_remote_changed({
            let weak = self.downgrade();
            move |_| {
                let Some(obj) = weak.upgrade() else { return };
                if !obj.imp().active.get() { return; }
                obj.update_remote_display();
            }
        });
        imp.handlers.borrow_mut().push(id);
    }

    /// See the view lifecycle contract (`views/mod.rs`).
    pub(crate) fn set_active(&self, active: bool) {
        let was = self.imp().active.replace(active);
        if active && !was { self.refresh(); }
    }

    /// Kiosk mode only: scales this bar's fonts/icon sizes proportionally
    /// to `side` (the same WideRight artwork side length already driving
    /// `apply_wide_right_scale()`/`set_edge_margin()` — pass
    /// `compute_wide_right_art_side(w, h)`'s result, same as those). This
    /// is the fix for the bar itself: it used to be a *fixed* size
    /// (`window.kiosk-window`-scoped `device-info`/`ip-label`/`net-icon`
    /// CSS at 18px/18px/24px, `remote_icon`'s hardcoded 42px for `large`)
    /// regardless of actual screen size — confirmed live to look right on
    /// one screen (whatever it was eyeballed against) and disproportionately
    /// big on a much smaller one (a Raspberry Pi's touchscreen vs. a 4K
    /// monitor). Factors below are chosen to land close to those old fixed
    /// values at roughly the screen size they were tuned against, then
    /// scale down from there — floors are for baseline legibility only
    /// (same reasoning as `apply_wide_right_scale()`'s own `status_px`
    /// floor), not because these should stop shrinking on a small screen;
    /// shrinking on a small screen is the actual fix. Not independently
    /// re-tuned against a real screen. Lazily creates its own scoped
    /// `gtk::CssProvider` on first call (same technique
    /// `apply_wide_right_scale()` uses), reused on every subsequent resize.
    pub(crate) fn set_scale(&self, side: i32) {
        let imp = self.imp();
        let needs_provider = imp.scale_provider.get().is_none();
        let class = imp.scale_class.get_or_init(|| {
            let class = format!("statusbar-scale-{:x}", self.as_ptr() as usize);
            self.add_css_class(&class);
            class
        }).clone();
        let provider = imp.scale_provider.get_or_init(gtk::CssProvider::new);
        if needs_provider {
            if let Some(display) = gtk::gdk::Display::default() {
                gtk::style_context_add_provider_for_display(
                    &display, provider, gtk::STYLE_PROVIDER_PRIORITY_USER,
                );
            }
        }

        // One knob (BASE_FACTOR) for the whole bar — net_icon/remote_icon
        // are fixed ratios off `dev_px`, not independently-tuned factors
        // of their own, so a future "make it all a bit bigger/smaller"
        // adjustment only ever touches this one number instead of three
        // that have to be moved in lockstep.
        const BASE_FACTOR: f64 = 0.0156;
        const NET_RATIO:    f64 = 1.34;  // net_px / dev_px
        const REMOTE_RATIO: f64 = 2.33;  // remote_px / dev_px
        let s = side as f64;
        let dev_px    = (s * BASE_FACTOR).round().max(11.0) as i32;
        let net_px    = (s * BASE_FACTOR * NET_RATIO).round().max(13.0) as i32;
        let remote_px = (s * BASE_FACTOR * REMOTE_RATIO).round().max(18.0) as i32;

        imp.remote_icon.get().unwrap().set_pixel_size(remote_px);

        provider.load_from_string(&format!(
            ".{class} .device-info {{ font-size: {dev_px}px; }}\n\
             .{class} .ip-label {{ font-size: {dev_px}px; }}\n\
             .{class} .net-icon {{ -gtk-icon-size: {net_px}px; }}\n"
        ));
    }

    /// Aligns this bar's left/right content with whatever margin the host
    /// is using elsewhere — Kiosk mode's `WideRight` layout, since this is
    /// a separate widget sitting below it rather than part of its own
    /// tree, would otherwise use its own small fixed margins that don't
    /// line up with the playback view's edges above it.
    pub(crate) fn set_edge_margin(&self, margin: i32) {
        let bar = self.imp().bar.get().unwrap();
        bar.set_margin_start(margin);
        bar.set_margin_end(margin);
    }

    /// Full render from the `DeviceState` cache — live or offline.
    fn refresh(&self) {
        let imp = self.imp();
        let Some(ds) = imp.ds.get() else { return };
        self.update_network_icon();
        self.update_remote_display();
        if ds.device_info().is_some() {
            self.apply_device_info();
        } else {
            imp.dev_info.get().unwrap().set_label("");
            imp.ip_label.get().unwrap().set_visible(false);
        }
    }

    fn update_network_icon(&self) {
        let imp = self.imp();
        let Some(ds) = imp.ds.get() else { return };
        let net_icon = imp.net_icon.get().unwrap();
        match ds.netstat() {
            Some(0) => {
                net_icon.set_icon_name(Some("network-wired-symbolic"));
                net_icon.set_tooltip_text(None);
                net_icon.set_visible(true);
            }
            Some(2) => {
                let rssi = ds.rssi().unwrap_or(0);
                net_icon.set_icon_name(Some(wifi_icon_for_rssi(rssi)));
                let ssid = ds.device_info().map(|i| i.ssid_decoded()).unwrap_or_default();
                let tooltip = if ssid.is_empty() {
                    format!("Signal: {rssi} dBm")
                } else {
                    format!("Network: {ssid}\nSignal: {rssi} dBm")
                };
                net_icon.set_tooltip_text(Some(&tooltip));
                net_icon.set_visible(true);
            }
            _ => { net_icon.set_visible(false); }
        }
    }

    /// BLE remote presence/battery. Visible whenever `getStatusEx` has ever
    /// answered the question at all (`remote_info().connected.is_some()`)
    /// — including "known but currently disconnected" — and hidden only
    /// when we truly don't know (field absent from every response so far,
    /// e.g. no BLE remote hardware exists on this model). Hovering shows
    /// battery/signal detail, or "disconnected" when not currently connected.
    fn update_remote_display(&self) {
        let imp = self.imp();
        let Some(ds) = imp.ds.get() else { return };
        let remote_icon = imp.remote_icon.get().unwrap();
        let remote_label = imp.remote_label.get().unwrap();

        let info = ds.remote_info();
        let Some(connected) = info.connected else {
            remote_icon.set_visible(false);
            remote_label.set_visible(false);
            return;
        };

        let battery_text = if connected {
            info.battery.map(|pct| format!("{pct}%")).unwrap_or_default()
        } else {
            String::new()
        };
        let tooltip = if connected {
            format!(
                "Battery: {}\nSignal: {}",
                info.battery.map(|pct| format!("{pct}%")).unwrap_or_else(|| "unknown".to_string()),
                info.rssi.map(|r| format!("{r} dBm")).unwrap_or_else(|| "unknown".to_string()),
            )
        } else {
            "disconnected".to_string()
        };

        remote_label.set_label(&battery_text);
        remote_icon.set_tooltip_text(Some(&tooltip));
        remote_label.set_tooltip_text(Some(&tooltip));

        remote_icon.set_visible(true);
        remote_icon.queue_resize();
        remote_label.set_visible(!battery_text.is_empty());
        remote_label.queue_resize();
    }

    fn apply_device_info(&self) {
        let imp = self.imp();
        let Some(ds) = imp.ds.get() else { return };
        let info = match ds.device_info() { Some(i) => i, None => return };
        let caps = match ds.capabilities() { Some(c) => c, None => return };

        imp.dev_info.get().unwrap().set_label(&format!(
            "{} · {} · FW {}",
            caps.vendor.display_name(), caps.model, info.firmware,
        ));

        // Unlike dev_info (always visible, only its text ever changes),
        // ip_label starts invisible and is shown/hidden here on every
        // device-changed. queue_resize() forces a full fresh layout pass on
        // the reveal rather than risking a stale allocation/clip from
        // before the label was visible.
        let ip = info.ip_addr();
        let ip_label = imp.ip_label.get().unwrap();
        if !ip.is_empty() {
            ip_label.set_label(ip);
            ip_label.set_visible(true);
            ip_label.queue_resize();
        } else {
            ip_label.set_visible(false);
        }
    }
}

/// Signal-strength icon name for an RSSI value (dBm, more negative = weaker).
fn wifi_icon_for_rssi(rssi: i32) -> &'static str {
    match rssi {
        i32::MIN..=-85 | 0 => "network-wireless-offline-symbolic",
        -84..=-75           => "network-wireless-signal-weak-symbolic",
        -74..=-65           => "network-wireless-signal-ok-symbolic",
        -64..=-55           => "network-wireless-signal-good-symbolic",
        _                   => "network-wireless-signal-excellent-symbolic",
    }
}
