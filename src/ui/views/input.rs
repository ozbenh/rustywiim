//! # InputView
//!
//! The input-source dropdown row — the input half of what used to be
//! `InputOutputView` (see `views/output.rs`'s doc comment for why the two
//! were split). Follows the view lifecycle contract (`views/mod.rs`):
//! subscribes to `device-changed`/`input-changed`/`inputs-changed` itself,
//! and renders the offline state (dropdown "—"/insensitive) that used to be
//! `reset_device_ui()`'s `sw` block.
//!
//! Input-change side effects on the *playback* display (the source-icon
//! artwork fallback) are deliberately not this view's business — the
//! window (later the playback views) keeps its own `input-changed`
//! subscription for those.

pub mod imp {
    use std::cell::{Cell, OnceCell, RefCell};

    use adw::prelude::*;
    use adw::subclass::prelude::*;
    use gtk::glib;

    use crate::device::state::DeviceState;

    #[derive(Default)]
    pub struct InputView {
        pub(super) ds:       OnceCell<DeviceState>,
        pub(super) handlers: RefCell<Vec<glib::SignalHandlerId>>,
        pub(super) active:   Cell<bool>,

        pub(super) in_dropdown:  OnceCell<gtk::DropDown>,
        pub(super) in_ids:       RefCell<Vec<String>>,
        /// Icon lookup key per entry — usually identical to the matching
        /// `in_ids` entry, except where `capabilities::icon_canon_for_input()`
        /// swaps it (e.g. a jack-style "line-in" on some devices) —
        /// resolved once in `populate_input()` (which has the device
        /// context the factory's `connect_bind` closure, built before any
        /// device is even connected, doesn't) rather than in the factory.
        pub(super) in_icon_keys: RefCell<Vec<String>>,
        pub(super) in_enabled:   RefCell<Vec<bool>>,
        /// Reentrancy guard: set around programmatic `set_selected()` calls
        /// so `connect_selected_notify` doesn't echo them back to the
        /// device as a user switch.
        pub(super) in_updating:  Cell<bool>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for InputView {
        const NAME: &'static str = "InputView";
        type Type = super::InputView;
        type ParentType = adw::Bin;
    }

    impl ObjectImpl for InputView {
        fn dispose(&self) {
            if let Some(ds) = self.ds.get() {
                for id in self.handlers.take() {
                    ds.disconnect(id);
                }
            }
        }
    }
    impl WidgetImpl for InputView {}
    impl BinImpl for InputView {}
}

use std::rc::Rc;

use adw::prelude::*;
use adw::subclass::prelude::*;
use gtk::glib;
use gtk::{Align, Box as GtkBox, Label, Orientation};

use crate::device::capabilities;
use crate::device::state::DeviceState;
use crate::ui::icons::IconSet;
use super::common;

glib::wrapper! {
    pub struct InputView(ObjectSubclass<imp::InputView>)
        @extends adw::Bin, gtk::Widget;
}

impl InputView {
    /// Build the input row bound to `ds`. Starts **inactive** — the
    /// owner's first `set_active(true)` performs the initial render.
    pub(crate) fn new(ds: &DeviceState, icons: &Rc<IconSet>) -> Self {
        let obj: Self = glib::Object::new();
        obj.build(ds, icons);
        obj
    }

    fn build(&self, ds: &DeviceState, icons: &Rc<IconSet>) {
        let imp = self.imp();
        imp.ds.set(ds.clone()).unwrap();

        let in_dropdown = gtk::DropDown::from_strings(&["—"]);
        in_dropdown.add_css_class("panel-dropdown");
        in_dropdown.set_sensitive(false);

        let factory = gtk::SignalListItemFactory::new();
        factory.connect_setup(|_, obj| {
            let Some(item) = obj.downcast_ref::<gtk::ListItem>() else { return };
            let (hbox, _, _) = common::icon_label_row(16);
            item.set_child(Some(&hbox));
        });
        factory.connect_bind({
            let weak = self.downgrade();
            let icons = Rc::clone(icons);
            move |_, obj| {
                let Some(view) = weak.upgrade() else { return };
                let Some(item) = obj.downcast_ref::<gtk::ListItem>() else { return };
                let pos = item.position() as usize;
                if let Some(hbox) = item.child().and_downcast::<GtkBox>() {
                    let vimp = view.imp();
                    let enabled   = vimp.in_enabled.borrow().get(pos).copied().unwrap_or(true);
                    let icon_keys = vimp.in_icon_keys.borrow();
                    let icon_key  = icon_keys.get(pos).map(String::as_str).unwrap_or("");
                    if let Some(img) = hbox.first_child().and_downcast::<gtk::Image>() {
                        img.set_paintable(Some(icons.source_paintable(icon_key)));
                    }
                    if let Some(lbl) = hbox.last_child().and_downcast::<Label>() {
                        if let Some(so) = item.item().and_downcast::<gtk::StringObject>() {
                            lbl.set_label(&so.string());
                        }
                        lbl.set_sensitive(enabled);
                    }
                    item.set_activatable(enabled);
                }
            }
        });
        factory.connect_unbind(|_, obj| {
            let Some(item) = obj.downcast_ref::<gtk::ListItem>() else { return };
            item.set_activatable(true);
            if let Some(hbox) = item.child().and_downcast::<GtkBox>() {
                if let Some(lbl) = hbox.last_child().and_downcast::<Label>() {
                    lbl.set_sensitive(true);
                }
            }
        });
        in_dropdown.set_factory(Some(&factory));

        in_dropdown.connect_selected_notify({
            let weak = self.downgrade();
            move |dd| {
                let Some(view) = weak.upgrade() else { return };
                let vimp = view.imp();
                if vimp.in_updating.get() { return; }
                let idx = dd.selected() as usize;
                let ids = vimp.in_ids.borrow();
                if let Some(src) = ids.get(idx).cloned() {
                    drop(ids);
                    if let Some(ds) = vimp.ds.get() { ds.switch_input(src); }
                }
            }
        });

        let in_box = GtkBox::builder()
            .orientation(Orientation::Vertical).spacing(4)
            .margin_top(4).margin_bottom(4).margin_start(8).margin_end(8)
            .build();
        in_box.append(
            &Label::builder()
                .label("INPUT").css_classes(["section-label"])
                .halign(Align::Start).build(),
        );
        in_box.append(&in_dropdown);
        self.set_child(Some(&in_box));

        imp.in_dropdown.set(in_dropdown).unwrap();

        let id = ds.connect_device_changed({
            let weak = self.downgrade();
            move |_| {
                let Some(view) = weak.upgrade() else { return };
                if !view.imp().active.get() { return; }
                view.refresh();
            }
        });
        imp.handlers.borrow_mut().push(id);

        let id = ds.connect_input_changed({
            let weak = self.downgrade();
            move |_| {
                let Some(view) = weak.upgrade() else { return };
                if !view.imp().active.get() { return; }
                view.select_input();
            }
        });
        imp.handlers.borrow_mut().push(id);

        // The available-input *list* (or a per-input enabled flag) changed —
        // e.g. state.rs force-enabling an input it caught in active use
        // despite being reported disabled. Rebuild the menu, then reselect
        // (a full rebuild resets the selection to 0).
        let id = ds.connect_inputs_changed({
            let weak = self.downgrade();
            move |_| {
                let Some(view) = weak.upgrade() else { return };
                if !view.imp().active.get() { return; }
                view.populate_input();
                view.select_input();
            }
        });
        imp.handlers.borrow_mut().push(id);
    }

    /// See the view lifecycle contract (`views/mod.rs`).
    pub(crate) fn set_active(&self, active: bool) {
        let was = self.imp().active.replace(active);
        if active && !was { self.refresh(); }
    }

    /// Full render from the `DeviceState` cache — live or offline.
    fn refresh(&self) {
        let Some(ds) = self.imp().ds.get() else { return };
        if ds.device_info().is_none() {
            self.render_offline();
            return;
        }
        self.populate_input();
        self.select_input();
    }

    /// Dropdown back to its disconnected placeholder — previously
    /// `reset_device_ui()`'s `sw` block.
    fn render_offline(&self) {
        let imp = self.imp();
        let in_dd = imp.in_dropdown.get().unwrap();
        imp.in_updating.set(true);
        in_dd.set_model(Some(&gtk::StringList::new(&["—"])));
        in_dd.set_sensitive(false);
        imp.in_updating.set(false);
        imp.in_ids.borrow_mut().clear();
        imp.in_icon_keys.borrow_mut().clear();
        imp.in_enabled.borrow_mut().clear();
    }

    fn populate_input(&self) {
        let imp = self.imp();
        let Some(ds) = imp.ds.get() else { return };
        let caps = match ds.capabilities() { Some(c) => c, None => return };
        let renames = ds.mode_renames();
        let in_dd = imp.in_dropdown.get().unwrap();

        // `caps.inputs` is already the one reconciled list — static
        // plm_support-based detection, amended by a live getAudioInputEnable
        // probe if that succeeded, further self-corrected in state.rs
        // against the actively-in-use input. No more either/or fallback
        // between two competing sources.
        let ids: Vec<String> = caps.inputs.iter().map(|e| e.id.clone()).collect();
        let enabled_flags: Vec<bool> = caps.inputs.iter().map(|e| e.enabled).collect();

        if ids.is_empty() {
            imp.in_updating.set(true);
            in_dd.set_model(Some(&gtk::StringList::new(&["—"])));
            in_dd.set_sensitive(false);
            imp.in_updating.set(false);
            imp.in_ids.borrow_mut().clear();
            imp.in_icon_keys.borrow_mut().clear();
            imp.in_enabled.borrow_mut().clear();
            return;
        }

        let labels: Vec<String> = ids.iter().zip(enabled_flags.iter()).map(|(id, _)| {
            let std_name = capabilities::input_display_name(Some(caps.device_id), id).to_string();
            if let Some(user) = renames.get(id.as_str()) {
                if !user.is_empty() && user != &std_name {
                    return format!("{} ({})", user, std_name);
                }
            }
            std_name
        }).collect();
        let icon_keys: Vec<String> = ids.iter()
            .map(|id| capabilities::icon_canon_for_input(id, caps.device_id).to_string())
            .collect();

        let label_refs: Vec<&str> = labels.iter().map(String::as_str).collect();
        *imp.in_ids.borrow_mut()       = ids;
        *imp.in_icon_keys.borrow_mut() = icon_keys;
        *imp.in_enabled.borrow_mut()   = enabled_flags;
        imp.in_updating.set(true);
        in_dd.set_model(Some(&gtk::StringList::new(&label_refs)));
        in_dd.set_selected(0);
        in_dd.set_sensitive(true);
        imp.in_updating.set(false);
    }

    /// Move the input dropdown's selection to the device's current input.
    fn select_input(&self) {
        let imp = self.imp();
        let Some(ds) = imp.ds.get() else { return };
        if ds.device_info().is_none() { return; }
        let mode = ds.current_mode();
        let source_id = capabilities::mode_to_input_source(mode);
        let ids = imp.in_ids.borrow();
        if let Some(idx) = ids.iter().position(|s| s == source_id) {
            drop(ids);
            imp.in_updating.set(true);
            imp.in_dropdown.get().unwrap().set_selected(idx as u32);
            imp.in_updating.set(false);
        }
    }
}
