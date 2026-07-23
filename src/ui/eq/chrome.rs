//! The EQ panel's small, independently swappable chrome widgets —
//! `EqMechanismToggle`/`EqSourcePicker`/`EqChannelToggle`/`EqChannelPicker`/
//! `EqPresetPicker`. Kept as separate small widgets with narrow signal
//! contracts, rather than built inline in the host panel, because this
//! first-cut visual treatment (a segmented toggle, popovers) is expected
//! to be reworked later, and that rework shouldn't risk anything about
//! how bands decode, debounce, or write.
//!
//! Signals use plain `String`/`bool` payloads rather than custom enum
//! GTypes (`EqKind` etc. aren't `glib::Value`-compatible without extra
//! boxing machinery) — string tokens (`"graphic"`/`"parametric"`,
//! `"left"`/`"right"`) are translated to/from the real `device::eq`/
//! `device::capabilities` enums by the host panel, which is the only
//! thing that needs to know both vocabularies.

use adw::prelude::*;
use adw::subclass::prelude::*;
use gtk::glib;

use crate::device::eq::is_valid_preset_name;

// ── EqMechanismToggle ────────────────────────────────────────────────────────

pub mod mechanism_toggle_imp {
    use std::cell::RefCell;
    use std::sync::OnceLock;

    use adw::prelude::*;
    use adw::subclass::prelude::*;
    use glib::subclass::Signal;
    use gtk::glib;

    #[derive(Default)]
    pub struct EqMechanismToggle {
        pub(super) buttons: RefCell<Vec<(String, gtk::ToggleButton)>>,
        pub(super) updating: std::cell::Cell<bool>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for EqMechanismToggle {
        const NAME: &'static str = "EqMechanismToggle";
        type Type = super::EqMechanismToggle;
        type ParentType = adw::Bin;
    }

    impl ObjectImpl for EqMechanismToggle {
        fn signals() -> &'static [Signal] {
            static SIGNALS: OnceLock<Vec<Signal>> = OnceLock::new();
            SIGNALS.get_or_init(|| {
                // "off"/"graphic"/"parametric" — never empty; "off" is a
                // real selectable option, not the absence of a signal.
                vec![Signal::builder("mechanism-selected").param_types([String::static_type()]).build()]
            })
        }
    }
    impl WidgetImpl for EqMechanismToggle {}
    impl BinImpl for EqMechanismToggle {}
}

glib::wrapper! {
    pub struct EqMechanismToggle(ObjectSubclass<mechanism_toggle_imp::EqMechanismToggle>)
        @extends adw::Bin, gtk::Widget;
}

impl Default for EqMechanismToggle {
    fn default() -> Self { Self::new() }
}

impl EqMechanismToggle {
    pub(crate) fn new() -> Self {
        glib::Object::new()
    }

    /// `tokens` is `"graphic"`/`"parametric"` (whichever mechanisms exist
    /// for the current target) — `"off"` is always added as the first
    /// option. Rebuilds the whole button row.
    pub(crate) fn set_mechanisms(&self, tokens: &[&str]) {
        let imp = self.imp();
        let row = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        row.add_css_class("linked");

        let mut buttons = Vec::new();
        let mut all_tokens = vec!["off".to_string()];
        all_tokens.extend(tokens.iter().map(|s| s.to_string()));

        let group_leader: Option<gtk::ToggleButton> = None;
        let mut leader = group_leader;
        for token in &all_tokens {
            let label = match token.as_str() {
                "off" => "Off",
                "graphic" => "Graphic",
                "parametric" => "Parametric",
                other => other,
            };
            let btn = gtk::ToggleButton::builder().label(label).build();
            if let Some(l) = &leader { btn.set_group(Some(l)); } else { leader = Some(btn.clone()); }
            row.append(&btn);
            btn.connect_toggled({
                let weak = self.downgrade();
                let token = token.clone();
                move |b| {
                    let Some(this) = weak.upgrade() else { return };
                    if this.imp().updating.get() || !b.is_active() { return; }
                    this.emit_by_name::<()>("mechanism-selected", &[&token]);
                }
            });
            buttons.push((token.clone(), btn));
        }
        self.set_child(Some(&row));
        *imp.buttons.borrow_mut() = buttons;
    }

    /// External update (e.g. reflecting `TargetOverview` on target
    /// switch) — doesn't re-emit `mechanism-selected`.
    pub(crate) fn set_selected(&self, token: &str) {
        let imp = self.imp();
        imp.updating.set(true);
        for (t, btn) in imp.buttons.borrow().iter() {
            btn.set_active(t == token);
        }
        imp.updating.set(false);
    }

    pub(crate) fn connect_mechanism_selected<F: Fn(&Self, String) + 'static>(&self, f: F) -> glib::SignalHandlerId {
        self.connect_local("mechanism-selected", false, move |args| {
            let this = args[0].get::<Self>().unwrap();
            let token = args[1].get::<String>().unwrap();
            f(&this, token);
            None
        })
    }
}

// ── EqSourcePicker ───────────────────────────────────────────────────────────

pub mod source_picker_imp {
    use std::cell::{OnceCell, RefCell};
    use std::sync::OnceLock;

    use adw::prelude::*;
    use adw::subclass::prelude::*;
    use glib::subclass::Signal;
    use gtk::glib;

    #[derive(Default)]
    pub struct EqSourcePicker {
        pub(super) button:  OnceCell<gtk::Button>,
        pub(super) icon:    OnceCell<gtk::Image>,
        pub(super) label:   OnceCell<gtk::Label>,
        pub(super) popover: OnceCell<gtk::Popover>,
        pub(super) list:    OnceCell<gtk::ListBox>,
        pub(super) sources: RefCell<Vec<String>>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for EqSourcePicker {
        const NAME: &'static str = "EqSourcePicker";
        type Type = super::EqSourcePicker;
        type ParentType = adw::Bin;
    }

    impl ObjectImpl for EqSourcePicker {
        fn constructed(&self) {
            self.parent_constructed();
            // Icon + label, matching `InputOutputView`'s input dropdown —
            // this button shows the same translated name/icon the Source
            // panel already does, not the raw wire token.
            let (content, icon, label) = crate::ui::views::common::icon_label_row(16);
            label.set_label("Source");
            let button = gtk::Button::builder().child(&content).build();
            self.obj().set_child(Some(&button));

            let list = gtk::ListBox::new();
            list.add_css_class("boxed-list");

            let popover_content = gtk::Box::new(gtk::Orientation::Vertical, 4);
            popover_content.set_margin_top(8);
            popover_content.set_margin_bottom(8);
            popover_content.set_margin_start(8);
            popover_content.set_margin_end(8);
            popover_content.set_width_request(220);
            let title = gtk::Label::builder()
                .label("Source:")
                .halign(gtk::Align::Start)
                .css_classes(["heading"])
                .build();
            popover_content.append(&title);
            popover_content.append(&list);

            let popover = gtk::Popover::new();
            popover.set_child(Some(&popover_content));
            popover.set_parent(&button);

            self.button.set(button).ok();
            self.icon.set(icon).ok();
            self.label.set(label).ok();
            self.list.set(list).ok();
            self.popover.set(popover).ok();
        }

        fn dispose(&self) {
            if let Some(pop) = self.popover.get() { pop.unparent(); }
        }

        fn signals() -> &'static [Signal] {
            static SIGNALS: OnceLock<Vec<Signal>> = OnceLock::new();
            SIGNALS.get_or_init(|| {
                vec![Signal::builder("source-selected").param_types([String::static_type()]).build()]
            })
        }
    }
    impl WidgetImpl for EqSourcePicker {}
    impl BinImpl for EqSourcePicker {}
}

glib::wrapper! {
    pub struct EqSourcePicker(ObjectSubclass<source_picker_imp::EqSourcePicker>)
        @extends adw::Bin, gtk::Widget;
}

impl Default for EqSourcePicker {
    fn default() -> Self { Self::new() }
}

impl EqSourcePicker {
    pub(crate) fn new() -> Self {
        let obj: Self = glib::Object::new();
        obj.wire();
        obj
    }

    fn wire(&self) {
        let imp = self.imp();
        let button = imp.button.get().unwrap().clone();
        let popover = imp.popover.get().unwrap().clone();
        button.connect_clicked({
            let popover = popover.clone();
            move |_| {
                if popover.is_visible() { popover.popdown(); } else { popover.popup(); }
            }
        });

        // Connected exactly once here, not inside `set_sources()` (which
        // can run repeatedly over this widget's life) — reads the current
        // source list live from `imp.sources` rather than a value
        // captured at connect time. Reconnecting on every rebuild would
        // silently accumulate duplicate handlers, firing one selection
        // once per accumulated handler (confirmed live: this exact bug in
        // `EqPresetPicker::set_presets()` below caused overlapping
        // concurrent preset-load requests to the same device, which
        // "sometimes worked" depending on how many handlers had piled up
        // — see that fix's own comment for the full story).
        let list = imp.list.get().unwrap().clone();
        list.connect_row_activated({
            let weak = self.downgrade();
            let popover = popover.clone();
            move |_, row| {
                let Some(this) = weak.upgrade() else { return };
                let idx = row.index();
                if idx < 0 { return; }
                let Some(token) = this.imp().sources.borrow().get(idx as usize).cloned() else { return };
                popover.popdown();
                this.emit_by_name::<()>("source-selected", &[&token]);
            }
        });
    }

    /// Rebuilds the source list. `display(source_token) -> (label, icon)`
    /// lets the host panel apply its own canonical-input-name/icon mapping
    /// (the same one `InputOutputView`'s input dropdown uses) without this
    /// widget needing to know about `capabilities::InputEntry` itself.
    /// Also stabilizes the button's own width to the widest label among
    /// `sources` — otherwise it visibly resized on every selection change.
    pub(crate) fn set_sources(&self, sources: &[String], display: impl Fn(&str) -> (String, gtk::gdk::Paintable)) {
        let imp = self.imp();
        let list = imp.list.get().unwrap();
        while let Some(child) = list.first_child() { list.remove(&child); }
        let mut labels: Vec<String> = Vec::new();
        for source in sources {
            let (label, icon) = display(source);
            let row = gtk::ListBoxRow::new();
            let hbox = gtk::Box::new(gtk::Orientation::Horizontal, 8);
            hbox.append(&gtk::Image::builder().pixel_size(20).paintable(&icon).build());
            hbox.append(&list_row_label(&label));
            row.set_child(Some(&hbox));
            row.set_margin_top(4);
            row.set_margin_bottom(4);
            row.set_margin_start(10);
            row.set_margin_end(10);
            list.append(&row);
            labels.push(label);
        }
        *imp.sources.borrow_mut() = sources.to_vec();
        stabilize_label_width(imp.label.get().unwrap(), labels.iter());
    }

    pub(crate) fn set_current(&self, text: &str, icon: &gtk::gdk::Paintable) {
        let imp = self.imp();
        imp.label.get().unwrap().set_label(text);
        imp.icon.get().unwrap().set_paintable(Some(icon));
    }

    pub(crate) fn connect_source_selected<F: Fn(&Self, String) + 'static>(&self, f: F) -> glib::SignalHandlerId {
        self.connect_local("source-selected", false, move |args| {
            let this = args[0].get::<Self>().unwrap();
            let token = args[1].get::<String>().unwrap();
            f(&this, token);
            None
        })
    }
}

// ── EqChannelToggle ──────────────────────────────────────────────────────────

pub mod channel_toggle_imp {
    use std::cell::{Cell, OnceCell};
    use std::sync::OnceLock;

    use adw::prelude::*;
    use adw::subclass::prelude::*;
    use glib::subclass::Signal;
    use gtk::glib;

    #[derive(Default)]
    pub struct EqChannelToggle {
        pub(super) button:  OnceCell<gtk::Button>,
        pub(super) label:   OnceCell<gtk::Label>,
        pub(super) popover: OnceCell<gtk::Popover>,
        pub(super) updating: Cell<bool>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for EqChannelToggle {
        const NAME: &'static str = "EqChannelToggle";
        type Type = super::EqChannelToggle;
        type ParentType = adw::Bin;
    }

    impl ObjectImpl for EqChannelToggle {
        fn constructed(&self) {
            self.parent_constructed();
            // A small two-item menu (Stereo / L-R) rather than a `Switch` —
            // inverted from the original design on request: this binary
            // *mode* choice reads better as an explicit menu (matching
            // Source/Preset), while the Left/Right *channel* choice below
            // (`EqChannelPicker`) is the one that's really a switch.
            let label = gtk::Label::new(Some("Stereo"));
            let button = gtk::Button::builder().child(&label).build();
            // "Mode:" lives inside this widget's own child (rather than
            // being a sibling the host panel adds next to it) so it's
            // never visible on its own — this widget's existing
            // `set_visible()` calls (only shown for a mechanism that
            // actually offers L/R, i.e. never for GEQ/Off) already hide/
            // show it correctly with no extra call sites to touch.
            let row = gtk::Box::new(gtk::Orientation::Horizontal, 6);
            row.append(&gtk::Label::builder().label("Mode:").css_classes(["dim-label"]).build());
            row.append(&button);
            self.obj().set_child(Some(&row));

            let list = gtk::ListBox::new();
            list.add_css_class("boxed-list");
            for text in ["Stereo", "L/R"] {
                let row = gtk::ListBoxRow::new();
                row.set_child(Some(&gtk::Label::builder().label(text).halign(gtk::Align::Start).build()));
                row.set_margin_top(4);
                row.set_margin_bottom(4);
                row.set_margin_start(10);
                row.set_margin_end(10);
                list.append(&row);
            }

            let popover_content = gtk::Box::new(gtk::Orientation::Vertical, 4);
            popover_content.set_margin_top(8);
            popover_content.set_margin_bottom(8);
            popover_content.set_margin_start(8);
            popover_content.set_margin_end(8);
            popover_content.append(&list);

            let popover = gtk::Popover::new();
            popover.set_child(Some(&popover_content));
            popover.set_parent(&button);

            let weak = self.obj().downgrade();
            list.connect_row_activated(move |_, row| {
                let Some(this) = weak.upgrade() else { return };
                let lr = row.index() == 1;
                this.imp().popover.get().unwrap().popdown();
                this.imp().label.get().unwrap().set_label(if lr { "L/R" } else { "Stereo" });
                if this.imp().updating.get() { return; }
                this.emit_by_name::<()>("channel-mode-toggled", &[&lr]);
            });

            self.button.set(button).ok();
            self.label.set(label).ok();
            self.popover.set(popover).ok();
        }

        fn dispose(&self) {
            if let Some(pop) = self.popover.get() { pop.unparent(); }
        }

        fn signals() -> &'static [Signal] {
            static SIGNALS: OnceLock<Vec<Signal>> = OnceLock::new();
            SIGNALS.get_or_init(|| {
                vec![Signal::builder("channel-mode-toggled").param_types([bool::static_type()]).build()]
            })
        }
    }
    impl WidgetImpl for EqChannelToggle {}
    impl BinImpl for EqChannelToggle {}
}

glib::wrapper! {
    pub struct EqChannelToggle(ObjectSubclass<channel_toggle_imp::EqChannelToggle>)
        @extends adw::Bin, gtk::Widget;
}

impl Default for EqChannelToggle {
    fn default() -> Self { Self::new() }
}

impl EqChannelToggle {
    pub(crate) fn new() -> Self {
        let obj: Self = glib::Object::new();
        let button = obj.imp().button.get().unwrap().clone();
        let popover = obj.imp().popover.get().unwrap().clone();
        button.connect_clicked(move |_| {
            if popover.is_visible() { popover.popdown(); } else { popover.popup(); }
        });
        if let Some(label) = obj.imp().label.get() {
            stabilize_label_width(label, ["Stereo", "L/R"].into_iter());
        }
        obj
    }

    pub(crate) fn set_active(&self, lr: bool) {
        let imp = self.imp();
        imp.updating.set(true);
        imp.label.get().unwrap().set_label(if lr { "L/R" } else { "Stereo" });
        imp.updating.set(false);
    }

    pub(crate) fn connect_channel_mode_toggled<F: Fn(&Self, bool) + 'static>(&self, f: F) -> glib::SignalHandlerId {
        self.connect_local("channel-mode-toggled", false, move |args| {
            let this = args[0].get::<Self>().unwrap();
            let lr   = args[1].get::<bool>().unwrap();
            f(&this, lr);
            None
        })
    }
}

// ── EqChannelPicker ──────────────────────────────────────────────────────────

pub mod channel_picker_imp {
    use std::cell::{Cell, RefCell};
    use std::sync::OnceLock;

    use adw::prelude::*;
    use adw::subclass::prelude::*;
    use glib::subclass::Signal;
    use gtk::glib;

    #[derive(Default)]
    pub struct EqChannelPicker {
        /// `(is_right, button)` for both segments — a plain `Vec` rather
        /// than two named `OnceCell`s since `set_selected()`/the toggle
        /// handler both just need "the other one," not either specifically.
        pub(super) buttons:  RefCell<Vec<(bool, gtk::ToggleButton)>>,
        pub(super) updating: Cell<bool>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for EqChannelPicker {
        const NAME: &'static str = "EqChannelPicker";
        type Type = super::EqChannelPicker;
        type ParentType = adw::Bin;
    }

    impl ObjectImpl for EqChannelPicker {
        fn constructed(&self) {
            self.parent_constructed();
            // A segmented two-way switch — a rounded-rect pill split down
            // the middle, only one side ever highlighted — rather than a
            // popover menu: reuses the same `.linked` GtkToggleButton-group
            // visual `EqMechanismToggle` already established for
            // Off/Graphic/Parametric. Inverted from the original design on
            // request: Left/Right is a real binary channel choice that's
            // always meaningful once L/R mode is on, so it reads better as
            // an always-visible switch than a menu that hides the other
            // side.
            let row = gtk::Box::new(gtk::Orientation::Horizontal, 0);
            row.add_css_class("linked");

            let left  = gtk::ToggleButton::builder().label("Left").active(true).build();
            let right = gtk::ToggleButton::builder().label("Right").build();
            right.set_group(Some(&left));
            row.append(&left);
            row.append(&right);
            self.obj().set_child(Some(&row));

            for (is_right, btn) in [(false, &left), (true, &right)] {
                btn.connect_toggled({
                    let weak = self.obj().downgrade();
                    move |b| {
                        let Some(this) = weak.upgrade() else { return };
                        if this.imp().updating.get() || !b.is_active() { return; }
                        let token = if is_right { "right" } else { "left" };
                        this.emit_by_name::<()>("channel-selected", &[&token.to_string()]);
                    }
                });
            }

            *self.buttons.borrow_mut() = vec![(false, left), (true, right)];
        }

        fn signals() -> &'static [Signal] {
            static SIGNALS: OnceLock<Vec<Signal>> = OnceLock::new();
            SIGNALS.get_or_init(|| {
                vec![Signal::builder("channel-selected").param_types([String::static_type()]).build()]
            })
        }
    }
    impl WidgetImpl for EqChannelPicker {}
    impl BinImpl for EqChannelPicker {}
}

glib::wrapper! {
    pub struct EqChannelPicker(ObjectSubclass<channel_picker_imp::EqChannelPicker>)
        @extends adw::Bin, gtk::Widget;
}

impl Default for EqChannelPicker {
    fn default() -> Self { Self::new() }
}

impl EqChannelPicker {
    pub(crate) fn new() -> Self {
        glib::Object::new()
    }

    /// Programmatic sync (e.g. resetting to Left whenever L/R mode is
    /// (re-)entered) — doesn't re-emit `channel-selected`.
    pub(crate) fn set_selected(&self, right: bool) {
        let imp = self.imp();
        imp.updating.set(true);
        for (is_right, btn) in imp.buttons.borrow().iter() {
            btn.set_active(*is_right == right);
        }
        imp.updating.set(false);
    }

    pub(crate) fn connect_channel_selected<F: Fn(&Self, String) + 'static>(&self, f: F) -> glib::SignalHandlerId {
        self.connect_local("channel-selected", false, move |args| {
            let this = args[0].get::<Self>().unwrap();
            let token = args[1].get::<String>().unwrap();
            f(&this, token);
            None
        })
    }
}

// ── EqPresetPicker ───────────────────────────────────────────────────────────

pub mod preset_picker_imp {
    use std::cell::{Cell, OnceCell, RefCell};
    use std::sync::OnceLock;

    use adw::prelude::*;
    use adw::subclass::prelude::*;
    use glib::subclass::Signal;
    use gtk::glib;

    use super::is_valid_preset_name;

    #[derive(Default)]
    pub struct EqPresetPicker {
        pub(super) button:      OnceCell<gtk::Button>,
        pub(super) popover:     OnceCell<gtk::Popover>,
        pub(super) list:        OnceCell<gtk::ListBox>,
        pub(super) save_entry:  OnceCell<gtk::Entry>,
        /// Row index -> device-reported preset name (empty string marks
        /// the non-activatable separator row) — read live by the single
        /// `row_activated` handler connected once in `EqPresetPicker::new()`,
        /// rather than a value captured fresh into a new closure on every
        /// `set_presets()` call (see that method's doc comment for the bug
        /// this replaced).
        pub(super) names: RefCell<Vec<String>>,
        /// Which name is currently "selected" (checkmarked) — `None` while
        /// `dirty` (shows no checkmark; the button reads "Custom"
        /// instead). Kept so `set_active()` can update just the checkmark/
        /// button label without the host needing to rebuild the whole row
        /// list on every band edit.
        pub(super) active: RefCell<Option<String>>,
        pub(super) dirty:  Cell<bool>,
        /// Name -> that row's checkmark `gtk::Image`, rebuilt each
        /// `set_presets()` call alongside the rows themselves — `apply_checkmarks()`
        /// toggles visibility on these directly rather than rebuilding rows.
        pub(super) checkmarks: RefCell<Vec<(String, gtk::Image)>>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for EqPresetPicker {
        const NAME: &'static str = "EqPresetPicker";
        type Type = super::EqPresetPicker;
        type ParentType = adw::Bin;
    }

    impl ObjectImpl for EqPresetPicker {
        fn constructed(&self) {
            self.parent_constructed();
            let button = gtk::Button::builder().label("Preset").build();
            self.obj().set_child(Some(&button));

            let content = gtk::Box::new(gtk::Orientation::Vertical, 6);
            content.set_margin_top(8);
            content.set_margin_bottom(8);
            content.set_margin_start(8);
            content.set_margin_end(8);
            content.set_width_request(220);

            content.append(&gtk::Label::builder()
                .label("Preset:")
                .halign(gtk::Align::Start)
                .css_classes(["heading"])
                .build());

            let list = gtk::ListBox::new();
            list.add_css_class("boxed-list");
            let scroll = gtk::ScrolledWindow::builder()
                .child(&list)
                .hscrollbar_policy(gtk::PolicyType::Never)
                .max_content_height(240)
                .propagate_natural_height(true)
                .build();
            content.append(&scroll);

            content.append(&gtk::Separator::new(gtk::Orientation::Horizontal));

            let save_row = gtk::Box::new(gtk::Orientation::Horizontal, 4);
            let save_entry = gtk::Entry::builder()
                .placeholder_text("New preset name…")
                .hexpand(true)
                .build();
            // Starts insensitive (matching the entry's own initial empty,
            // therefore invalid, state) — `connect_changed` below is what
            // keeps this in sync with `is_valid_preset_name()` as
            // the user types, same check `save_preset()`/`rename_preset()`
            // themselves enforce, so an invalid name never reaches the
            // device path at all rather than failing there.
            let save_button = gtk::Button::builder().label("Save").sensitive(false).build();
            save_row.append(&save_entry);
            save_row.append(&save_button);
            content.append(&save_row);

            // ".error" is a real libadwaita class (a generic accent-color
            // override, not entry/row-specific) — reused here on a plain
            // `Label` for the red validation message, same as the entry's
            // own "error" outline below.
            let save_error_label = gtk::Label::builder()
                .css_classes(["error", "caption"])
                .halign(gtk::Align::Start)
                .label("Only letters, numbers, and underscores allowed")
                .visible(false)
                .build();
            content.append(&save_error_label);

            save_entry.connect_changed({
                let save_button = save_button.clone();
                let save_error_label = save_error_label.clone();
                move |entry| {
                    let text = entry.text();
                    let valid = is_valid_preset_name(&text);
                    save_button.set_sensitive(valid);
                    if valid || text.is_empty() {
                        entry.remove_css_class("error");
                        save_error_label.set_visible(false);
                    } else {
                        entry.add_css_class("error");
                        save_error_label.set_visible(true);
                    }
                }
            });

            let popover = gtk::Popover::new();
            popover.set_child(Some(&content));
            popover.set_parent(&button);

            let weak = self.obj().downgrade();
            save_button.connect_clicked({
                let weak = weak.clone();
                let save_entry = save_entry.clone();
                move |_| {
                    let Some(this) = weak.upgrade() else { return };
                    let name = save_entry.text().to_string();
                    if !is_valid_preset_name(&name) { return; }
                    this.imp().popover.get().unwrap().popdown();
                    save_entry.set_text("");
                    this.emit_by_name::<()>("preset-save-requested", &[&name]);
                }
            });
            save_entry.connect_activate({
                let weak = weak.clone();
                move |entry| {
                    let Some(this) = weak.upgrade() else { return };
                    let name = entry.text().to_string();
                    if !is_valid_preset_name(&name) { return; }
                    this.imp().popover.get().unwrap().popdown();
                    entry.set_text("");
                    this.emit_by_name::<()>("preset-save-requested", &[&name]);
                }
            });

            self.button.set(button).ok();
            self.popover.set(popover).ok();
            self.list.set(list).ok();
            self.save_entry.set(save_entry).ok();
        }

        fn dispose(&self) {
            if let Some(pop) = self.popover.get() { pop.unparent(); }
        }

        fn signals() -> &'static [Signal] {
            static SIGNALS: OnceLock<Vec<Signal>> = OnceLock::new();
            SIGNALS.get_or_init(|| {
                vec![
                    Signal::builder("preset-selected").param_types([String::static_type()]).build(),
                    Signal::builder("preset-save-requested").param_types([String::static_type()]).build(),
                    Signal::builder("preset-rename-requested")
                        .param_types([String::static_type(), String::static_type()]).build(),
                    Signal::builder("preset-delete-requested").param_types([String::static_type()]).build(),
                ]
            })
        }
    }
    impl WidgetImpl for EqPresetPicker {}
    impl BinImpl for EqPresetPicker {}
}

glib::wrapper! {
    pub struct EqPresetPicker(ObjectSubclass<preset_picker_imp::EqPresetPicker>)
        @extends adw::Bin, gtk::Widget;
}

impl Default for EqPresetPicker {
    fn default() -> Self { Self::new() }
}

impl EqPresetPicker {
    pub(crate) fn new() -> Self {
        let obj: Self = glib::Object::new();
        let imp = obj.imp();
        let button = imp.button.get().unwrap().clone();
        let popover = imp.popover.get().unwrap().clone();
        button.connect_clicked({
            let popover = popover.clone();
            move |_| {
                if popover.is_visible() { popover.popdown(); } else { popover.popup(); }
            }
        });

        // Connected exactly once here, not inside `set_presets()` — a
        // real, confirmed bug: `set_presets()` used to call
        // `list.connect_row_activated(...)` itself every time it ran
        // (once per mechanism switch/resync, i.e. repeatedly over this
        // widget's life, not just once), which doesn't replace the
        // previous handler — GTK signals accumulate every connected
        // closure. After N rebuilds, one row click fired the handler N
        // times, sending N overlapping `load_preset()` HTTP requests to
        // the same device at once. This codebase's own hard rule is that
        // these devices handle concurrent connections poorly and HTTP
        // calls must stay sequential — confirmed live: exactly this
        // caused "loading preset" to fire 5x for one click and the
        // device to fail the request, matching the reported symptom
        // ("sometimes works" — however many handlers happened to have
        // piled up by that point). Fixed the same way as the identical
        // latent pattern in `EqSourcePicker::wire()`: connect once, read
        // the current name list live from `imp.names`.
        let list = imp.list.get().unwrap().clone();
        list.connect_row_activated({
            let weak = obj.downgrade();
            move |_, row| {
                let Some(this) = weak.upgrade() else { return };
                let idx = row.index();
                if idx < 0 { return; }
                let Some(name) = this.imp().names.borrow().get(idx as usize).filter(|n| !n.is_empty()).cloned() else { return };
                this.imp().popover.get().unwrap().popdown();
                this.emit_by_name::<()>("preset-selected", &[&name]);
            }
        });
        obj
    }

    /// Rebuilds the preset list: hardwired presets first (no icons — a
    /// built-in preset can't be renamed/deleted), then a separator (if
    /// both are non-empty), then custom (user-saved) ones, each with a
    /// pencil (rename) and trash (delete) icon button. `hardwired` is
    /// empty for PEQ (no built-in parametric presets on any device seen
    /// so far) — this just renders whatever the device reported, no
    /// PEQ-specific special-casing needed here. Row activation (clicking
    /// anywhere on a row *except* its icon buttons) emits `preset-selected`
    /// with the raw device-reported name (never matched against a local
    /// name table — see `device::eq::EqPresetList`'s doc comment).
    /// `active`/`dirty` set the initial checkmark/button label the same
    /// way `set_active()` does — see its own doc comment. Note `active` is
    /// `None` any time the device's current settings don't match a known
    /// preset by name, which reads as "Custom" exactly like `dirty` does —
    /// there's no third, genuinely-neutral state once real data has
    /// arrived, only the button's pre-fetch construction-time default ever
    /// shows the literal placeholder text.
    pub(crate) fn set_presets(&self, hardwired: &[String], custom: &[String], active: Option<&str>, dirty: bool) {
        let imp = self.imp();
        let list = imp.list.get().unwrap();
        while let Some(child) = list.first_child() { list.remove(&child); }

        let mut names: Vec<String> = Vec::new();
        let mut checkmarks: Vec<(String, gtk::Image)> = Vec::new();
        for name in hardwired {
            let (row, check, _) = build_preset_row(name, false);
            list.append(&row);
            names.push(name.clone());
            checkmarks.push((name.clone(), check));
        }
        if !hardwired.is_empty() && !custom.is_empty() {
            let sep_row = gtk::ListBoxRow::builder().selectable(false).activatable(false).build();
            sep_row.set_child(Some(&gtk::Separator::new(gtk::Orientation::Horizontal)));
            list.append(&sep_row);
            names.push(String::new()); // placeholder to keep row index aligned; never activatable
        }
        for name in custom {
            let (row, check, icons) = build_preset_row(name, true);
            if let Some((edit_btn, delete_btn)) = &icons {
                wire_preset_row_icons(edit_btn, delete_btn, self, name);
            }
            list.append(&row);
            names.push(name.clone());
            checkmarks.push((name.clone(), check));
        }

        *imp.names.borrow_mut() = names;
        *imp.active.borrow_mut() = active.map(str::to_string);
        imp.dirty.set(dirty);
        let button = imp.button.get().unwrap();
        button.set_label(if dirty { "Custom" } else { active.unwrap_or("Custom") });
        // Width pinned to the widest of every string this button can ever
        // show ("Custom" included — the fallback whenever nothing matches
        // or the user has edited something) so it doesn't visibly resize
        // every time the active preset changes.
        if let Some(label) = button.child().and_downcast::<gtk::Label>() {
            stabilize_label_width(&label, hardwired.iter().chain(custom.iter()).map(String::as_str).chain(std::iter::once("Custom")));
        }
        *imp.checkmarks.borrow_mut() = checkmarks;
        self.apply_checkmarks();
    }

    /// Updates the button's own label ("Custom" while `dirty` or when
    /// there's no `active` match, else `active`'s name) and which row (if
    /// any) shows a checkmark — without rebuilding the list. Called on
    /// every band edit (to flip to "Custom" immediately) as well as after
    /// a fresh fetch/preset load (to reflect the device's own reported
    /// name), neither of which need a full row rebuild.
    pub(crate) fn set_active(&self, active: Option<&str>, dirty: bool) {
        let imp = self.imp();
        *imp.active.borrow_mut() = active.map(str::to_string);
        imp.dirty.set(dirty);
        let button = imp.button.get().unwrap();
        button.set_label(if dirty { "Custom" } else { active.unwrap_or("Custom") });
        self.apply_checkmarks();
    }

    fn apply_checkmarks(&self) {
        let imp = self.imp();
        let active = imp.active.borrow().clone();
        let dirty = imp.dirty.get();
        for (name, check) in imp.checkmarks.borrow().iter() {
            check.set_visible(!dirty && active.as_deref() == Some(name.as_str()));
        }
    }

    pub(crate) fn connect_preset_selected<F: Fn(&Self, String) + 'static>(&self, f: F) -> glib::SignalHandlerId {
        self.connect_local("preset-selected", false, move |args| {
            let this = args[0].get::<Self>().unwrap();
            let token = args[1].get::<String>().unwrap();
            f(&this, token);
            None
        })
    }

    pub(crate) fn connect_preset_save_requested<F: Fn(&Self, String) + 'static>(&self, f: F) -> glib::SignalHandlerId {
        self.connect_local("preset-save-requested", false, move |args| {
            let this = args[0].get::<Self>().unwrap();
            let token = args[1].get::<String>().unwrap();
            f(&this, token);
            None
        })
    }

    pub(crate) fn connect_preset_rename_requested<F: Fn(&Self, String, String) + 'static>(&self, f: F) -> glib::SignalHandlerId {
        self.connect_local("preset-rename-requested", false, move |args| {
            let this = args[0].get::<Self>().unwrap();
            let old = args[1].get::<String>().unwrap();
            let new = args[2].get::<String>().unwrap();
            f(&this, old, new);
            None
        })
    }

    pub(crate) fn connect_preset_delete_requested<F: Fn(&Self, String) + 'static>(&self, f: F) -> glib::SignalHandlerId {
        self.connect_local("preset-delete-requested", false, move |args| {
            let this = args[0].get::<Self>().unwrap();
            let name = args[1].get::<String>().unwrap();
            f(&this, name);
            None
        })
    }
}

/// A left-aligned, hexpanding row label at a slightly larger-than-body
/// size — used by both `EqSourcePicker` and `EqPresetPicker`'s popover
/// lists so their plain rows read at roughly the same size/weight as a
/// custom-preset row's icon buttons impose, for touch-screen use (Kiosk
/// mode). Pango markup rather than a CSS class: no new stylesheet rule
/// needed across `system.css`/`dark.css`/`modern.css` for what's really
/// just "a bit bigger," not a themed style.
/// Pins `label`'s minimum/maximum width to the longest string in
/// `candidates` (in characters — same font throughout, so this is close
/// enough) so a button showing one of these strings at a time doesn't
/// visibly grow/shrink every time the selection changes. Sized against the
/// full known set, not just whatever happens to be selected right now.
/// Also left-justifies the text within that now-fixed-width box: `width_chars`
/// inflates the label's own natural size (unlike `hexpand`+`halign(Start)`,
/// which only inflates the surrounding *cell*, leaving the label itself at
/// its natural size) — without this, the label's default centered
/// `xalign` would visibly separate the text from whatever sits before it
/// (e.g. `EqSourcePicker`'s icon) by the padding `width_chars` just added.
fn stabilize_label_width(label: &gtk::Label, candidates: impl Iterator<Item = impl AsRef<str>>) {
    let max = candidates.map(|s| s.as_ref().chars().count() as i32).max().unwrap_or(0);
    label.set_width_chars(max);
    label.set_max_width_chars(max);
    label.set_xalign(0.0);
}

fn list_row_label(text: &str) -> gtk::Label {
    gtk::Label::builder()
        .use_markup(true)
        .label(format!("<span size=\"larger\">{}</span>", glib::markup_escape_text(text)))
        .hexpand(true)
        .halign(gtk::Align::Start)
        .build()
}

/// One preset row: [checkmark] [name, hexpand] [pencil] [trash] — the
/// checkmark is always present (so every row lines up under the same
/// column), just hidden unless this is the active, non-dirty preset; the
/// pencil/trash pair only exists for `is_custom` rows (a hardwired preset
/// can't be renamed or deleted). Returns the row, its checkmark image,
/// and — for a custom preset only — its rename/delete buttons, so the
/// caller can wire those directly by handle rather than traversing the
/// row's children back out afterward.
fn build_preset_row(name: &str, is_custom: bool) -> (gtk::ListBoxRow, gtk::Image, Option<(gtk::Button, gtk::Button)>) {
    let row = gtk::ListBoxRow::new();
    row.set_margin_top(4);
    row.set_margin_bottom(4);
    row.set_margin_start(10);
    row.set_margin_end(10);
    let hbox = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    let check = gtk::Image::builder()
        .icon_name("object-select-symbolic")
        .visible(false)
        .build();
    hbox.append(&check);
    hbox.append(&list_row_label(name));
    let icons = if is_custom {
        let edit_btn = gtk::Button::builder()
            .icon_name("document-edit-symbolic")
            .css_classes(["flat", "circular"])
            .tooltip_text("Rename")
            .build();
        let delete_btn = gtk::Button::builder()
            .icon_name("user-trash-symbolic")
            .css_classes(["flat", "circular"])
            .tooltip_text("Delete")
            .build();
        hbox.append(&edit_btn);
        hbox.append(&delete_btn);
        Some((edit_btn, delete_btn))
    } else {
        None
    };
    row.set_child(Some(&hbox));
    (row, check, icons)
}

/// Wires a custom-preset row's rename/delete buttons — connected fresh
/// each time since the *row itself* is a brand-new widget discarded and
/// rebuilt on every `set_presets()` call (unlike the shared, persistent
/// `list` widget's `row_activated` handler, which must stay connected
/// exactly once — see `EqPresetPicker::new()`'s own doc comment for why
/// those are different cases).
fn wire_preset_row_icons(edit_btn: &gtk::Button, delete_btn: &gtk::Button, host: &EqPresetPicker, name: &str) {
    edit_btn.connect_clicked({
        let weak = host.downgrade();
        let name = name.to_string();
        move |btn| {
            let Some(this) = weak.upgrade() else { return };
            let Some(root) = btn.root() else { return };
            show_rename_dialog(&root, &this, &name);
        }
    });
    delete_btn.connect_clicked({
        let weak = host.downgrade();
        let name = name.to_string();
        move |btn| {
            let Some(this) = weak.upgrade() else { return };
            let Some(root) = btn.root() else { return };
            show_delete_confirm(&root, &this, &name);
        }
    });
}

fn show_rename_dialog(root: &gtk::Root, host: &EqPresetPicker, old_name: &str) {
    let Ok(window) = root.clone().downcast::<gtk::Window>() else { return };
    let entry = gtk::Entry::builder().text(old_name).activates_default(true).build();
    let error_label = gtk::Label::builder()
        .css_classes(["error", "caption"])
        .halign(gtk::Align::Start)
        .label("Only letters, numbers, and underscores allowed")
        .visible(false)
        .build();
    let extra = gtk::Box::new(gtk::Orientation::Vertical, 4);
    extra.append(&entry);
    extra.append(&error_label);

    let dialog = adw::AlertDialog::builder()
        .heading("Rename Preset")
        .close_response("cancel")
        .build();
    dialog.add_response("cancel", "Cancel");
    dialog.add_response("rename", "Rename");
    dialog.set_response_appearance("rename", adw::ResponseAppearance::Suggested);
    dialog.set_default_response(Some("rename"));
    dialog.set_extra_child(Some(&extra));

    entry.connect_changed({
        let dialog = dialog.clone();
        move |entry| {
            let text = entry.text();
            let valid = is_valid_preset_name(&text);
            dialog.set_response_enabled("rename", valid);
            if valid || text.is_empty() {
                entry.remove_css_class("error");
                error_label.set_visible(false);
            } else {
                entry.add_css_class("error");
                error_label.set_visible(true);
            }
        }
    });

    dialog.connect_response(None, {
        let weak = host.downgrade();
        let old_name = old_name.to_string();
        move |_dlg, response| {
            if response != "rename" { return; }
            let Some(this) = weak.upgrade() else { return };
            let new_name = entry.text().to_string();
            if !is_valid_preset_name(&new_name) || new_name == old_name { return; }
            this.imp().popover.get().unwrap().popdown();
            this.emit_by_name::<()>("preset-rename-requested", &[&old_name, &new_name]);
        }
    });
    dialog.present(Some(&window));
}

fn show_delete_confirm(root: &gtk::Root, host: &EqPresetPicker, name: &str) {
    let Ok(window) = root.clone().downcast::<gtk::Window>() else { return };
    let dialog = adw::AlertDialog::builder()
        .heading("Delete Preset?")
        .body(format!("This will permanently delete the preset \u{201c}{name}\u{201d}."))
        .close_response("cancel")
        .build();
    dialog.add_response("cancel", "Cancel");
    dialog.add_response("delete", "Delete");
    dialog.set_response_appearance("delete", adw::ResponseAppearance::Destructive);
    dialog.set_default_response(Some("cancel"));

    dialog.connect_response(None, {
        let weak = host.downgrade();
        let name = name.to_string();
        move |_dlg, response| {
            if response != "delete" { return; }
            let Some(this) = weak.upgrade() else { return };
            this.imp().popover.get().unwrap().popdown();
            this.emit_by_name::<()>("preset-delete-requested", &[&name]);
        }
    });
    dialog.present(Some(&window));
}
