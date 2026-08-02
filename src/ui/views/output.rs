//! # OutputView
//!
//! The output-mode dropdown row — the output half of what used to be
//! `InputOutputView` before it was split into this and `views/input.rs`

pub mod imp {
    use std::cell::{Cell, OnceCell, RefCell};

    use adw::prelude::*;
    use adw::subclass::prelude::*;
    use gtk::glib;

    use crate::device::state::DeviceState;

    #[derive(Default)]
    pub struct OutputView {
        pub(super) ds:       OnceCell<DeviceState>,
        pub(super) handlers: RefCell<Vec<glib::SignalHandlerId>>,
        pub(super) active:   Cell<bool>,

        pub(super) out_dropdown: OnceCell<gtk::DropDown>,
        pub(super) out_modes:    RefCell<Vec<u32>>,
        pub(super) out_canon_names: RefCell<Vec<&'static str>>,
        /// Icon-lookup key per entry, parallel to `out_canon_names` — equal
        /// to it except where `OutputEntry.icon_canon` overrides it (see
        /// `capabilities::icon_canon_for_output`). `out_canon_names` itself
        /// must stay untouched for mode-setting/hardware-match to keep
        /// working.
        pub(super) out_icon_names: RefCell<Vec<&'static str>>,
        pub(super) out_updating:   Cell<bool>,
        /// This device currently has a real output list to show — the
        /// "would this row be visible on its own account" half of
        /// `apply_visibility()`'s decision. Separate from
        /// `hidden_for_leader` because the two reasons a row can be
        /// invisible are independent and shouldn't overwrite each other
        /// (a leader's own output list doesn't stop existing just because
        /// the row is hidden for group reasons, and vice versa).
        pub(super) has_outputs:    Cell<bool>,
        /// Set by the host (`set_group_leader()`) whenever this view's
        /// device currently leads a multiroom group — see this module's
        /// own doc comment for why. `false` for a non-grouped or follower
        /// device, which is every device this view is ever built for
        /// outside `device_window`/`kiosk`'s own group-role gating.
        pub(super) hidden_for_leader: Cell<bool>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for OutputView {
        const NAME: &'static str = "OutputView";
        type Type = super::OutputView;
        type ParentType = adw::Bin;
    }

    impl ObjectImpl for OutputView {
        fn dispose(&self) {
            if let Some(ds) = self.ds.get() {
                for id in self.handlers.take() {
                    ds.disconnect(id);
                }
            }
        }
    }
    impl WidgetImpl for OutputView {}
    impl BinImpl for OutputView {}
}

use std::rc::Rc;

use adw::prelude::*;
use adw::subclass::prelude::*;
use gtk::glib;
use gtk::{Align, Box as GtkBox, Label, Orientation};

use crate::device::{api, capabilities};
use crate::device::state::DeviceState;
use crate::ui::icons::IconSet;
use super::common;

glib::wrapper! {
    pub struct OutputView(ObjectSubclass<imp::OutputView>)
        @extends adw::Bin, gtk::Widget;
}

impl OutputView {
    /// Build the output row bound to `ds`. Starts **inactive** — the
    /// owner's first `set_active(true)` performs the initial render.
    pub(crate) fn new(ds: &DeviceState, icons: &Rc<IconSet>) -> Self {
        let obj: Self = glib::Object::new();
        obj.build(ds, icons);
        obj
    }

    fn build(&self, ds: &DeviceState, icons: &Rc<IconSet>) {
        let imp = self.imp();
        imp.ds.set(ds.clone()).unwrap();
        self.set_visible(false);

        let out_dropdown = gtk::DropDown::from_strings(&["—"]);
        out_dropdown.add_css_class("panel-dropdown");
        out_dropdown.set_sensitive(false);

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
                    let names = view.imp().out_icon_names.borrow();
                    let canon = names.get(pos).copied().unwrap_or("");
                    if let Some(img) = hbox.first_child().and_downcast::<gtk::Image>() {
                        img.set_paintable(Some(icons.output_paintable(canon)));
                    }
                    if let Some(lbl) = hbox.last_child().and_downcast::<Label>() {
                        if let Some(so) = item.item().and_downcast::<gtk::StringObject>() {
                            lbl.set_label(&so.string());
                        }
                    }
                }
            }
        });
        out_dropdown.set_factory(Some(&factory));

        out_dropdown.connect_selected_notify({
            let weak = self.downgrade();
            move |dd| {
                let Some(view) = weak.upgrade() else { return };
                let vimp = view.imp();
                if vimp.out_updating.get() { return; }
                let idx = dd.selected() as usize;
                let modes = vimp.out_modes.borrow();
                if let Some(&mode) = modes.get(idx) {
                    drop(modes);
                    if let Some(ds) = vimp.ds.get() { ds.set_audio_output(mode); }
                }
            }
        });

        let out_box = GtkBox::builder()
            .orientation(Orientation::Vertical).spacing(4)
            .margin_top(4).margin_bottom(8).margin_start(8).margin_end(8)
            .build();
        out_box.append(
            &Label::builder()
                .label("OUTPUT").css_classes(["section-label"]).halign(Align::Start).build(),
        );
        out_box.append(&out_dropdown);
        self.set_child(Some(&out_box));

        imp.out_dropdown.set(out_dropdown).unwrap();

        let id = ds.connect_device_changed({
            let weak = self.downgrade();
            move |_| {
                let Some(view) = weak.upgrade() else { return };
                if !view.imp().active.get() { return; }
                view.refresh();
            }
        });
        imp.handlers.borrow_mut().push(id);

        let id = ds.connect_output_changed({
            let weak = self.downgrade();
            move |_| {
                let Some(view) = weak.upgrade() else { return };
                if !view.imp().active.get() { return; }
                view.select_output();
            }
        });
        imp.handlers.borrow_mut().push(id);

        let id = ds.connect_outputs_changed({
            let weak = self.downgrade();
            move |_| {
                let Some(view) = weak.upgrade() else { return };
                if !view.imp().active.get() { return; }
                view.populate_output();
                view.select_output();
            }
        });
        imp.handlers.borrow_mut().push(id);
    }

    /// See the view lifecycle contract (`views/mod.rs`).
    pub(crate) fn set_active(&self, active: bool) {
        let was = self.imp().active.replace(active);
        if active && !was { self.refresh(); }
    }

    /// Whether this view's device currently leads a multiroom group — a
    /// leader's own output isn't meaningful group-wide (each member has
    /// its own), so the whole row hides regardless of what
    /// `populate_output()` would otherwise decide. The host
    /// (`device_window`/`kiosk`) calls this from its own `group-changed`
    /// handling; this view has no group-role awareness of its own.
    pub(crate) fn set_group_leader(&self, is_leader: bool) {
        self.imp().hidden_for_leader.set(is_leader);
        self.apply_visibility();
    }

    /// Single point deciding whether the whole row is visible — two
    /// independent reasons feed it (see `imp::OutputView`'s field docs),
    /// so neither `populate_output()`/`render_offline()` nor
    /// `set_group_leader()` may call `set_visible()` directly.
    fn apply_visibility(&self) {
        let imp = self.imp();
        self.set_visible(imp.has_outputs.get() && !imp.hidden_for_leader.get());
    }

    /// Full render from the `DeviceState` cache — live or offline.
    fn refresh(&self) {
        let Some(ds) = self.imp().ds.get() else { return };
        if ds.device_info().is_none() {
            self.render_offline();
            return;
        }
        self.populate_output();
        self.select_output();
    }

    /// Dropdown back to its disconnected placeholder — previously
    /// `reset_device_ui()`'s `ow` block.
    fn render_offline(&self) {
        let imp = self.imp();
        let out_dd = imp.out_dropdown.get().unwrap();
        imp.out_updating.set(true);
        out_dd.set_model(Some(&gtk::StringList::new(&["—"])));
        out_dd.set_sensitive(false);
        imp.out_modes.borrow_mut().clear();
        imp.out_canon_names.borrow_mut().clear();
        imp.out_icon_names.borrow_mut().clear();
        imp.out_updating.set(false);
        imp.has_outputs.set(false);
        self.apply_visibility();
    }

    fn populate_output(&self) {
        let imp = self.imp();
        let Some(ds) = imp.ds.get() else { return };
        if ds.capabilities().is_none() { return; }
        let out_dd = imp.out_dropdown.get().unwrap();
        let output_names = ds.outputs();
        if output_names.is_empty() {
            imp.out_updating.set(true);
            out_dd.set_model(Some(&gtk::StringList::new(&["—"])));
            out_dd.set_sensitive(false);
            imp.out_modes.borrow_mut().clear();
            imp.out_canon_names.borrow_mut().clear();
            imp.out_icon_names.borrow_mut().clear();
            imp.out_updating.set(false);
            imp.has_outputs.set(false);
            self.apply_visibility();
            return;
        }

        let out_labels: Vec<&str> = output_names.iter()
            .map(|e: &api::OutputEntry| e.name.as_str())
            .collect();
        let modes: Vec<u32> = output_names.iter()
            .map(|e| capabilities::output_canon_to_mode(e.canon).unwrap_or(0))
            .collect();

        *imp.out_modes.borrow_mut()       = modes;
        *imp.out_canon_names.borrow_mut() = output_names.iter().map(|e| e.canon).collect();
        *imp.out_icon_names.borrow_mut()  = output_names.iter().map(|e| e.icon_canon).collect();
        imp.out_updating.set(true);
        out_dd.set_model(Some(&gtk::StringList::new(&out_labels)));
        imp.has_outputs.set(true);
        self.apply_visibility();

        // `output_status` is None right after connecting (state.rs no
        // longer fetches it eagerly at connect time — see fetch_device_info)
        // until the first slow-poll OutputStatus tick fills it in, a few
        // seconds later. Grey the dropdown out rather than showing an
        // unselected/first-item guess in the meantime; select_output()
        // re-enables it once the real value arrives.
        //
        // Some devices (confirmed: Arylic S10+, AudioCast Pro) will *never*
        // get a real value here — `getSoundCardModeSupportList` (the output
        // *list*, above) works fine on them, but `getNewAudioOutputHardwareMode`
        // (the current-selection query) answers "unknown command" — greying
        // the menu out forever in that case would hide a perfectly real,
        // usable output list behind a permanently-disabled control.
        // `caps.probes_output_status` is state.rs's confirmed-unsupported
        // flag (`false` once given up, still `true` while genuinely still
        // waiting) — only fall back to "enabled, no selection" once it's
        // actually confirmed, not just because a value hasn't arrived yet.
        match ds.output_status() {
            Some(os) => {
                out_dd.set_sensitive(true);
                if let Ok(hw) = os.hardware.parse::<u32>() {
                    let hw_canon = capabilities::canon_mode_output_name(hw);
                    let names = imp.out_canon_names.borrow();
                    if let Some(pos) = names.iter().position(|&n| n == hw_canon) {
                        out_dd.set_selected(pos as u32);
                    }
                }
            }
            None => {
                let confirmed_unsupported = ds.capabilities().is_some_and(|c| !c.probes_output_status);
                out_dd.set_sensitive(confirmed_unsupported);
            }
        }
        imp.out_updating.set(false);
    }

    /// Move the output dropdown's selection to the device's current output.
    fn select_output(&self) {
        let imp = self.imp();
        let Some(ds) = imp.ds.get() else { return };
        // `output_status()`, like `playback_state()`, is deliberately not
        // cleared on disconnect — without this guard a stale cached output
        // would repaint the dropdown as if still connected.
        if ds.device_info().is_none() { return; }
        let Some(os) = ds.output_status() else {
            // Reached on `output-changed` when state.rs's OutputStatus
            // probe just confirmed-gave-up (no real status to select, ever)
            // — see `populate_output()`'s identical comment. Enable the
            // menu anyway rather than leaving it greyed out from that
            // function's own initial "still waiting" state.
            if ds.capabilities().is_some_and(|c| !c.probes_output_status) {
                imp.out_dropdown.get().unwrap().set_sensitive(true);
            }
            return;
        };
        let out_dd = imp.out_dropdown.get().unwrap();
        // Now that we actually know the current output, the dropdown no
        // longer needs to stay greyed out from the connect-time "unknown"
        // state populate_output() set — see that function's comment.
        out_dd.set_sensitive(true);
        let Ok(hw) = os.hardware.parse::<u32>() else { return };
        let hw_canon = capabilities::canon_mode_output_name(hw);
        let names = imp.out_canon_names.borrow();
        if let Some(idx) = names.iter().position(|&n| n == hw_canon) {
            drop(names);
            imp.out_updating.set(true);
            out_dd.set_selected(idx as u32);
            imp.out_updating.set(false);
        }
    }
}
