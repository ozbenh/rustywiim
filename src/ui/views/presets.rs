//! # PresetsView
//!
//! The preset-button list (12 slots, each a badge + artwork/icon + name
//! tile), previously `widgets.rs`'s `PresetWidgets` + the window-driven
//! `on_presets_changed()`. Follows the view lifecycle contract (see
//! `views/mod.rs`): subscribes to `presets-changed`/`device-changed`
//! itself, early-returns while inactive, full refresh on activation —
//! including the offline rendering (all slots hidden/cleared) that used
//! to be `reset_device_ui()`'s preset block.

pub mod imp {
    use std::cell::{Cell, OnceCell, RefCell};

    use adw::prelude::*;
    use adw::subclass::prelude::*;
    use gtk::glib;

    use crate::device::state::DeviceState;
    use crate::ui::icons::IconSet;

    #[derive(Default)]
    pub struct PresetsView {
        pub(super) ds:       OnceCell<DeviceState>,
        pub(super) icons:    OnceCell<std::rc::Rc<IconSet>>,
        pub(super) handlers: RefCell<Vec<glib::SignalHandlerId>>,
        pub(super) active:   Cell<bool>,
        pub(super) btns:     OnceCell<Vec<gtk::Button>>,
        pub(super) pics:     OnceCell<Vec<gtk::Image>>,
        pub(super) labels:   OnceCell<Vec<gtk::Label>>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for PresetsView {
        const NAME: &'static str = "PresetsView";
        type Type = super::PresetsView;
        type ParentType = adw::Bin;
    }

    impl ObjectImpl for PresetsView {
        fn dispose(&self) {
            if let Some(ds) = self.ds.get() {
                for id in self.handlers.take() {
                    ds.disconnect(id);
                }
            }
        }
    }
    impl WidgetImpl for PresetsView {}
    impl BinImpl for PresetsView {}
}

use std::rc::Rc;

use adw::prelude::*;
use adw::subclass::prelude::*;
use gtk::glib;
use gtk::{Align, Box as GtkBox, Button, Label, Orientation};

use crate::device::capabilities;
use crate::device::state::DeviceState;
use crate::ui::icons::IconSet;

glib::wrapper! {
    pub struct PresetsView(ObjectSubclass<imp::PresetsView>)
        @extends adw::Bin, gtk::Widget;
}

impl PresetsView {
    /// Build the presets list bound to `ds`. The widget itself is the
    /// scrollable list (`gtk::ScrolledWindow` root) the owner packs
    /// directly. Starts **inactive** — the owner's first
    /// `set_active(true)` performs the initial render.
    pub(crate) fn new(ds: &DeviceState, icons: &Rc<IconSet>) -> Self {
        let obj: Self = glib::Object::new();
        obj.build(ds, icons);
        obj
    }

    fn build(&self, ds: &DeviceState, icons: &Rc<IconSet>) {
        let imp = self.imp();
        imp.ds.set(ds.clone()).unwrap();
        // No .unwrap(): IconSet has no Debug impl for the Err side, and
        // build() only ever runs once anyway (called from new() alone).
        let _ = imp.icons.set(Rc::clone(icons));

        let presets_box = GtkBox::builder()
            .orientation(Orientation::Vertical).spacing(2)
            .margin_top(8).margin_bottom(4).margin_start(8).margin_end(8)
            .build();
        presets_box.append(
            &Label::builder()
                .label("PRESETS").css_classes(["section-label"])
                .halign(Align::Start).margin_bottom(4)
                .build(),
        );

        let mut btns:   Vec<Button>     = Vec::new();
        let mut pics:   Vec<gtk::Image> = Vec::new();
        let mut labels: Vec<Label>      = Vec::new();

        for i in 1..=12u32 {
            let badge = Label::builder()
                .label(&i.to_string()).css_classes(["preset-badge"])
                .halign(Align::Center).valign(Align::Center)
                .build();
            let pic = gtk::Image::builder()
                .pixel_size(40).icon_name("audio-x-generic-symbolic")
                .build();
            pic.add_css_class("preset-art");
            pic.set_overflow(gtk::Overflow::Hidden);
            let lbl = Label::builder()
                .label("").css_classes(["preset-name"])
                .ellipsize(gtk::pango::EllipsizeMode::End)
                .halign(Align::Start).hexpand(true).width_chars(0)
                .build();
            let tile = GtkBox::builder()
                .orientation(Orientation::Horizontal).spacing(6)
                .css_classes(["preset-tile"]).overflow(gtk::Overflow::Hidden)
                .build();
            tile.append(&badge);
            tile.append(&pic);
            tile.append(&lbl);
            // "preset-btn" only styled under RustyWiiM Modern (see modern.css),
            // to trim its default flat-button horizontal padding — inert
            // elsewhere, same pattern as "panel-card"/"controls-card".
            let btn = Button::builder().child(&tile).css_classes(["flat", "preset-btn"]).build();
            btn.set_tooltip_text(Some(&format!("Preset {i}")));
            btn.set_visible(false);
            btn.connect_clicked({
                let weak = self.downgrade();
                move |_| {
                    let Some(obj) = weak.upgrade() else { return };
                    let Some(ds) = obj.imp().ds.get() else { return };
                    if let Some(c) = ds.client() {
                        ds.rt().spawn(async move { let _ = c.play_preset(i).await; });
                    }
                }
            });
            presets_box.append(&btn);
            btns.push(btn);
            pics.push(pic);
            labels.push(lbl);
        }

        imp.btns.set(btns).unwrap();
        imp.pics.set(pics).unwrap();
        imp.labels.set(labels).unwrap();

        let scroll = gtk::ScrolledWindow::builder()
            .child(&presets_box)
            .hscrollbar_policy(gtk::PolicyType::Never)
            .vexpand(true)
            .build();
        self.set_child(Some(&scroll));

        let id = ds.connect_presets_changed({
            let weak = self.downgrade();
            move |_| {
                let Some(obj) = weak.upgrade() else { return };
                if !obj.imp().active.get() { return; }
                obj.refresh();
            }
        });
        imp.handlers.borrow_mut().push(id);

        // Covers both directions: connect (presets may already be cached
        // from an earlier session of this shared DeviceState) and
        // disconnect (clear the slots — device_info() is None then).
        let id = ds.connect_device_changed({
            let weak = self.downgrade();
            move |_| {
                let Some(obj) = weak.upgrade() else { return };
                if !obj.imp().active.get() { return; }
                obj.refresh();
            }
        });
        imp.handlers.borrow_mut().push(id);
    }

    /// See the view lifecycle contract (`views/mod.rs`).
    pub(crate) fn set_active(&self, active: bool) {
        let was = self.imp().active.replace(active);
        if active && !was { self.refresh(); }
    }

    /// Render all 12 slots from the `DeviceState` cache. While offline
    /// (`device_info()` is None) every slot ends up hidden/cleared — the
    /// clear pass below runs unconditionally and `presets()` is empty
    /// after a disconnect reset.
    fn refresh(&self) {
        use crate::device::api::PresetKind;
        let imp = self.imp();
        let Some(ds) = imp.ds.get() else { return };
        let icons = imp.icons.get().unwrap();
        let btns   = imp.btns.get().unwrap();
        let pics   = imp.pics.get().unwrap();
        let labels = imp.labels.get().unwrap();

        let presets = if ds.device_info().is_some() { ds.presets() } else { Vec::new() };
        let device_id = ds.capabilities().map(|c| c.device_id);

        // Clear all slots first.
        for btn in btns.iter() { btn.set_visible(false); }
        for lbl in labels.iter() { lbl.set_label(""); }
        for pic in pics.iter() {
            pic.set_paintable(None::<&gtk::gdk::Paintable>);
            pic.set_icon_name(Some("audio-x-generic-symbolic"));
            pic.set_pixel_size(40);
            pic.remove_css_class("preset-art-small");
        }

        for entry in &presets {
            let idx = entry.slot.saturating_sub(1);
            if let Some(btn) = btns.get(idx) {
                btn.set_visible(true);
                btn.set_tooltip_text(Some(&entry.tooltip()));
            }
            if let Some(lbl) = labels.get(idx) {
                lbl.set_label(entry.label());
            }
            if let Some(pic) = pics.get(idx) {
                match &entry.kind {
                    PresetKind::Media => {
                        if !entry.art_bytes.is_empty() {
                            let gbytes = glib::Bytes::from(&entry.art_bytes);
                            if let Ok(tex) = gtk::gdk::Texture::from_bytes(&gbytes) {
                                pic.set_paintable(Some(&tex));
                            }
                        }
                    }
                    PresetKind::InputSwitch { input_id } => {
                        pic.set_pixel_size(26);
                        pic.add_css_class("preset-art-small");
                        let icon_key = match device_id {
                            Some(id) => capabilities::icon_canon_for_input(input_id, id),
                            None     => input_id.as_str(),
                        };
                        pic.set_paintable(Some(icons.source_paintable(icon_key)));
                    }
                    PresetKind::OutputSwitch { output_id } => {
                        pic.set_pixel_size(26);
                        pic.add_css_class("preset-art-small");
                        let canon = capabilities::canon_new_output_name(output_id);
                        let icon_canon = device_id
                            .map(|id| capabilities::icon_canon_for_output(canon, id))
                            .unwrap_or(canon);
                        pic.set_paintable(Some(icons.output_paintable(icon_canon)));
                    }
                    PresetKind::OtherRoutine => {
                        pic.set_pixel_size(26);
                        pic.add_css_class("preset-art-small");
                    }
                    PresetKind::Empty => {}
                }
            }
        }
    }
}
