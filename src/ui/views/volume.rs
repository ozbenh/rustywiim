//! # VolumeControl
//!
//! The volume cluster: a button showing the current volume icon + numeric
//! level, opening a popover with a vertical slider and a mute button.
//! This exact widget shape used to exist three times, hand-built and
//! hand-synchronized (`PlaybackWidgets`, `MiniWidgets`, and devlist's
//! `RowWidgets` — the third still does, pending the devlist rework).
//!
//! Follows the view lifecycle contract (see `views/mod.rs`): bound to one
//! `DeviceState` at construction, subscribes to `playback-changed`
//! itself (VOLUME bit), early-returns while inactive, and re-syncs on
//! activation. While the device is *offline* the whole cluster is
//! disabled and shows level 0 (`render_offline()`); while connected it
//! stays live even with nothing playable — volume/mute isn't tied to
//! playback content, only to having a device to talk to.

pub mod imp {
    use std::cell::{Cell, OnceCell, RefCell};

    use adw::prelude::*;
    use adw::subclass::prelude::*;
    use gtk::glib;

    use crate::device::state::DeviceState;

    #[derive(Default)]
    pub struct VolumeControl {
        pub(super) ds:       OnceCell<DeviceState>,
        pub(super) handlers: RefCell<Vec<glib::SignalHandlerId>>,
        pub(super) active:   Cell<bool>,
        pub(super) icon_img: OnceCell<gtk::Image>,
        pub(super) level:    OnceCell<gtk::Label>,
        pub(super) scale:    OnceCell<gtk::Scale>,
        pub(super) mute_btn: OnceCell<gtk::Button>,
        pub(super) popover:  OnceCell<gtk::Popover>,
        /// Set while the user is dragging the slider (or within 500 ms
        /// after) so poll updates don't jump it back mid-drag — the same
        /// pattern every previous copy of this cluster used, now
        /// per-instance instead of shared between the full/mini widgets.
        pub(super) drag_timer: RefCell<Option<glib::SourceId>>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for VolumeControl {
        const NAME: &'static str = "VolumeControl";
        type Type = super::VolumeControl;
        type ParentType = adw::Bin;
    }

    impl ObjectImpl for VolumeControl {
        fn dispose(&self) {
            // Runs on any teardown path, including a detached content tree
            // finalized by plain refcounting — which is exactly the path
            // where a `set_parent()`-attached popover otherwise never gets
            // unparented and GTK logs "Finalizing GtkButton … but it still
            // has children left: GtkPopover". The button (our Bin child) is
            // still alive here: glib chains up to adw::Bin's own dispose
            // (which drops the child) only after this returns.
            if let Some(pop) = self.popover.get() {
                pop.unparent();
            }
            if let Some(ds) = self.ds.get() {
                for id in self.handlers.take() {
                    ds.disconnect(id);
                }
            }
            if let Some(id) = self.drag_timer.take() {
                id.remove();
            }
        }
    }
    impl WidgetImpl for VolumeControl {}
    impl BinImpl for VolumeControl {}
}

use adw::prelude::*;
use adw::subclass::prelude::*;
use gtk::glib;
use gtk::{Align, Box as GtkBox, Button, Label, Orientation, Scale};

use crate::device::state::{playback_changed, DeviceState};
use super::common::vol_icon;

glib::wrapper! {
    pub struct VolumeControl(ObjectSubclass<imp::VolumeControl>)
        @extends adw::Bin, gtk::Widget;
}

impl VolumeControl {
    /// Build a volume cluster bound to `ds`. `mini` selects the mini
    /// panel's smaller styling (`mini-*` CSS classes, smaller icon/slider)
    /// over the full panel's. Starts **inactive** — the owner's first
    /// `set_active(true)` is what performs the initial sync.
    pub(crate) fn new(ds: &DeviceState, mini: bool) -> Self {
        let obj: Self = glib::Object::new();
        obj.build(ds, mini);
        obj
    }

    fn build(&self, ds: &DeviceState, mini: bool) {
        let imp = self.imp();
        imp.ds.set(ds.clone()).unwrap();

        // Two style variants of the same structure — these are exactly the
        // values the old build_vol_popover() (full) and the volume part of
        // build_mini_transport() (mini) used, so theming is unaffected.
        let (icon_px, btn_box_spacing, scale_h, scale_w, pop_margin) =
            if mini { (11, 1, 120, 20, 4) } else { (16, 2, 150, 24, 6) };

        let icon_img = gtk::Image::builder()
            .icon_name("audio-volume-high-symbolic")
            .pixel_size(icon_px)
            .build();
        let level = Label::builder()
            .label("—")
            .width_chars(3)
            .xalign(1.0)
            .css_classes(["vol-level"])
            .build();
        if mini { level.add_css_class("mini-vol-label"); }
        let btn_box = GtkBox::builder()
            .orientation(Orientation::Horizontal)
            .spacing(btn_box_spacing)
            .build();
        btn_box.append(&icon_img);
        btn_box.append(&level);
        let vol_btn = Button::builder()
            .css_classes(if mini {
                &["mini-transport-btn", "mini-vol-btn", "flat"][..]
            } else {
                &["transport-btn", "flat", "vol-btn"][..]
            })
            .tooltip_text("Volume")
            .build();
        vol_btn.set_child(Some(&btn_box));

        let scale = Scale::with_range(Orientation::Vertical, 0.0, 100.0, 1.0);
        scale.set_inverted(true);
        scale.set_vexpand(true);
        scale.set_height_request(scale_h);
        scale.set_draw_value(false);
        scale.set_width_request(scale_w);
        scale.set_round_digits(0);
        scale.add_css_class(if mini { "mini-vol-pop" } else { "vol-pop" });
        scale.set_increments(5.0, 20.0);

        let mute_btn = Button::builder()
            .icon_name("audio-volume-muted-symbolic")
            .css_classes(if mini {
                &["mini-transport-btn"][..]
            } else {
                &["transport-btn", "circular"][..]
            })
            .tooltip_text("Mute")
            .halign(Align::Center)
            .build();

        let pop_box = GtkBox::builder()
            .orientation(Orientation::Vertical)
            .margin_top(pop_margin).margin_bottom(pop_margin)
            .margin_start(pop_margin).margin_end(pop_margin)
            .spacing(4)
            .build();
        pop_box.append(&scale);
        pop_box.append(&mute_btn);
        let popover = gtk::Popover::new();
        if mini { popover.add_css_class("mini-vol-popover"); }
        popover.set_child(Some(&pop_box));
        popover.set_parent(&vol_btn);

        self.set_child(Some(&vol_btn));

        vol_btn.connect_clicked({
            let popover = popover.clone();
            let scale = scale.clone();
            move |btn| {
                if popover.is_visible() { popover.popdown(); return; }
                // Sized from the actual space available above the button
                // rather than a fixed guess — a fixed min-height (Kiosk
                // mode's own touch-friendly sizing used to be exactly
                // that, in CSS) can exceed what's really available on a
                // given screen, and a Wayland kiosk compositor may not
                // reposition/shrink an oversized popup to fit the way a
                // full desktop compositor would — confirmed live, that
                // made the popover fail to appear at all rather than
                // merely render smaller than intended.
                if let Some(root) = btn.root() {
                    let origin = gtk::graphene::Point::new(0.0, 0.0);
                    if let Some(p) = btn.compute_point(&root, &origin) {
                        let y = p.y() as f64;
                        // gtk::Popover's default position is above the
                        // parent, so the button's own y-offset from the
                        // window's top is exactly the vertical budget —
                        // minus room for the mute button/margins/popover
                        // chrome that also sit below the scale itself.
                        let is_kiosk = root.has_css_class("kiosk-window");
                        let desired: f64 = if is_kiosk { 420.0 } else if mini { 140.0 } else { 170.0 };
                        let available = (y - 90.0).max(60.0);
                        scale.set_height_request(desired.min(available).round() as i32);
                    }
                }
                popover.popup();
            }
        });

        mute_btn.connect_clicked({
            let weak = self.downgrade();
            move |_| {
                let Some(obj) = weak.upgrade() else { return };
                let Some(ds) = obj.imp().ds.get() else { return };
                ds.do_set_mute(!ds.muted());
            }
        });

        scale.connect_change_value({
            let weak = self.downgrade();
            move |_, _, vol| {
                if let Some(obj) = weak.upgrade() { obj.on_user_vol(vol); }
                glib::Propagation::Proceed
            }
        });

        let id = ds.connect_playback_changed({
            let weak = self.downgrade();
            move |_, mask| {
                let Some(obj) = weak.upgrade() else { return };
                if !obj.imp().active.get() { return; }
                if mask & playback_changed::VOLUME != 0 { obj.sync_display(); }
            }
        });
        imp.handlers.borrow_mut().push(id);

        // The connection-time catch-up. The first VOLUME-carrying
        // `playback-changed` can fire while `device_info()` is still None
        // (sync_display() renders the disabled offline state then), and
        // later polls only set the VOLUME bit again when the level
        // actually *changes* — so without this, a device whose volume
        // never moves after connect kept the offline rendering forever
        // (seen live against wiim-simulator; a real device's signal
        // timing usually masks it).
        let id = ds.connect_device_changed({
            let weak = self.downgrade();
            move |_| {
                let Some(obj) = weak.upgrade() else { return };
                if !obj.imp().active.get() { return; }
                obj.sync_display();
            }
        });
        imp.handlers.borrow_mut().push(id);

        imp.icon_img.set(icon_img).unwrap();
        imp.level.set(level).unwrap();
        imp.scale.set(scale).unwrap();
        imp.mute_btn.set(mute_btn).unwrap();
        imp.popover.set(popover).unwrap();
    }

    /// See the view lifecycle contract (`views/mod.rs`): handlers no-op
    /// while inactive; flipping inactive → active re-syncs the display so
    /// updates missed while inactive can't leave it stale.
    pub(crate) fn set_active(&self, active: bool) {
        let was = self.imp().active.replace(active);
        if active && !was { self.sync_display(); }
    }

    /// Nudge the volume by `delta` (clamped to 0..=100) — used by the
    /// Up/Down keyboard shortcuts. Routes through the same path as a
    /// manual slider drag so it gets the same optimistic UI update,
    /// rate-limited device command, and drag-protection timer. No-op while
    /// offline, matching the disabled on-screen controls.
    pub(crate) fn step(&self, delta: i32) {
        let Some(ds) = self.imp().ds.get() else { return };
        if ds.device_info().is_none() { return; }
        let new_vol = (ds.get_vol() as i32 + delta).clamp(0, 100);
        self.on_user_vol(new_vol as f64);
    }

    /// Sync the whole cluster from device state — live or offline. Skips
    /// the slider reposition while the user is dragging it.
    fn sync_display(&self) {
        let imp = self.imp();
        let Some(ds) = imp.ds.get() else { return };
        if ds.device_info().is_none() {
            self.render_offline();
            return;
        }
        self.set_sensitive(true);
        let muted = ds.muted();
        // Fetch the authoritative volume first; used for both the slider
        // position and the icon so they stay consistent even when
        // set_value is inhibited.
        let vol = ds.get_vol() as f64;
        if imp.drag_timer.borrow().is_none() {
            imp.scale.get().unwrap().set_value(vol);
        }
        imp.icon_img.get().unwrap().set_icon_name(Some(vol_icon(muted, vol)));
        imp.level.get().unwrap().set_label(&format!("{}", vol as u32));
        imp.mute_btn.get().unwrap().set_icon_name(
            if muted { "audio-volume-muted-symbolic" } else { "audio-volume-high-symbolic" });
    }

    /// The offline rendering: whole cluster disabled, level shown as 0.
    /// Offline is different from "connected but nothing playable" — volume
    /// deliberately stays live in the latter (not tied to playable
    /// content), but with no device to talk to the control is meaningless.
    fn render_offline(&self) {
        let imp = self.imp();
        self.set_sensitive(false);
        // A popover left open across the disconnect isn't closed by the
        // sensitivity change alone.
        imp.popover.get().unwrap().popdown();
        imp.scale.get().unwrap().set_value(0.0);
        imp.icon_img.get().unwrap().set_icon_name(Some(vol_icon(false, 0.0)));
        imp.level.get().unwrap().set_label("0");
        imp.mute_btn.get().unwrap().set_icon_name("audio-volume-muted-symbolic");
    }

    /// A user-initiated volume change (slider drag or keyboard step):
    /// update icon + level immediately, send the rate-limited device
    /// command, and (re)arm the 500 ms drag-protection timer so poll
    /// updates don't jump the slider while the user is still interacting.
    fn on_user_vol(&self, vol: f64) {
        let imp = self.imp();
        let Some(ds) = imp.ds.get() else { return };
        imp.icon_img.get().unwrap().set_icon_name(Some(vol_icon(ds.muted(), vol)));
        imp.level.get().unwrap().set_label(&format!("{}", vol as u32));
        ds.do_set_volume(vol as u32);

        if let Some(id) = imp.drag_timer.borrow_mut().take() { id.remove(); }
        let weak = self.downgrade();
        let id = glib::timeout_add_local_once(std::time::Duration::from_millis(500), move || {
            if let Some(obj) = weak.upgrade() {
                obj.imp().drag_timer.borrow_mut().take();
            }
        });
        *imp.drag_timer.borrow_mut() = Some(id);
    }
}
