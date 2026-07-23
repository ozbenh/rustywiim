//! `PromptEntry` — a prompt label + validated text entry + Ok/Cancel
//! buttons, as one self-contained widget. Built to replace the ad-hoc
//! entry+button UI the EQ preset save/rename flow used to have in two
//! separate places, but deliberately not EQ-specific: kept general enough
//! to reuse for any other touch-screen text-entry need later, since Kiosk
//! mode still has no on-screen-keyboard story of its own.
//!
//! This widget itself has zero window-awareness — it's just a `gtk::Widget`,
//! shown today via [`present_prompt_window`] (a plain window), but nothing
//! about the widget assumes that; hosting it as a `gtk::Overlay` child
//! inside another window's own content (e.g. Kiosk mode, once an on-screen
//! keyboard exists to go with it) wouldn't need to change anything here.

pub mod imp {
    use std::cell::{Cell, OnceCell, RefCell};
    use std::sync::OnceLock;

    use adw::prelude::*;
    use adw::subclass::prelude::*;
    use glib::subclass::Signal;
    use gtk::glib;

    type Validator = Box<dyn Fn(&str) -> Option<String>>;

    #[derive(Default)]
    pub struct PromptEntry {
        pub(super) prompt_label:  OnceCell<gtk::Label>,
        pub(super) entry:         OnceCell<gtk::Entry>,
        pub(super) error_label:   OnceCell<gtk::Label>,
        pub(super) ok_button:     OnceCell<gtk::Button>,
        pub(super) cancel_button: OnceCell<gtk::Button>,
        /// `None` means "always valid" — not every prompt needs one.
        pub(super) validator: RefCell<Option<Validator>>,
        /// Not yet read by anything (no on-screen-keyboard implementation
        /// exists yet) — recorded so callers can already declare what kind
        /// of input a given prompt wants, ready for whenever Kiosk mode's
        /// own on-screen keyboard exists to act on it.
        pub(super) keyboard_type: Cell<super::KeyboardType>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for PromptEntry {
        const NAME: &'static str = "PromptEntry";
        type Type = super::PromptEntry;
        type ParentType = adw::Bin;
    }

    impl ObjectImpl for PromptEntry {
        fn constructed(&self) {
            self.parent_constructed();

            let prompt_label = gtk::Label::builder()
                .halign(gtk::Align::Start)
                .wrap(true)
                .build();

            // Enter confirms directly via `connect_activate` below, not
            // `activates_default` — this widget has no window of its own
            // to hold a "default widget" for that mechanism to reach.
            let entry = gtk::Entry::builder()
                .hexpand(true)
                .build();

            // ".error" is a real libadwaita class (a generic accent-color
            // override, not entry/row-specific) — used on both the entry
            // itself (outline) and this label (text colour).
            let error_label = gtk::Label::builder()
                .css_classes(["error", "caption"])
                .halign(gtk::Align::Start)
                .visible(false)
                .build();

            let cancel_button = gtk::Button::builder().label("Cancel").build();
            let ok_button = gtk::Button::builder()
                .label("OK")
                .css_classes(["suggested-action"])
                .sensitive(false)
                .build();

            let button_row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
            button_row.set_halign(gtk::Align::End);
            button_row.append(&cancel_button);
            button_row.append(&ok_button);

            let content = gtk::Box::new(gtk::Orientation::Vertical, 8);
            content.set_margin_top(16);
            content.set_margin_bottom(16);
            content.set_margin_start(16);
            content.set_margin_end(16);
            content.append(&prompt_label);
            content.append(&entry);
            content.append(&error_label);
            content.append(&button_row);
            self.obj().set_child(Some(&content));

            entry.connect_changed({
                let weak = self.obj().downgrade();
                move |_| {
                    let Some(this) = weak.upgrade() else { return };
                    this.imp().revalidate();
                }
            });
            entry.connect_activate({
                let weak = self.obj().downgrade();
                move |_| {
                    let Some(this) = weak.upgrade() else { return };
                    this.imp().try_confirm();
                }
            });
            ok_button.connect_clicked({
                let weak = self.obj().downgrade();
                move |_| {
                    let Some(this) = weak.upgrade() else { return };
                    this.imp().try_confirm();
                }
            });
            cancel_button.connect_clicked({
                let weak = self.obj().downgrade();
                move |_| {
                    let Some(this) = weak.upgrade() else { return };
                    this.emit_by_name::<()>("cancelled", &[]);
                }
            });

            // Escape cancels regardless of which child has focus — Capture
            // phase, same as the window-level transport-key shortcuts
            // elsewhere in this codebase, so it isn't at the mercy of
            // whether some descendant widget would otherwise consume the
            // key itself first.
            let key_controller = gtk::EventControllerKey::new();
            key_controller.set_propagation_phase(gtk::PropagationPhase::Capture);
            key_controller.connect_key_pressed({
                let weak = self.obj().downgrade();
                move |_, keyval, _, _| {
                    if keyval != gtk::gdk::Key::Escape { return glib::Propagation::Proceed; }
                    let Some(this) = weak.upgrade() else { return glib::Propagation::Proceed };
                    this.emit_by_name::<()>("cancelled", &[]);
                    glib::Propagation::Stop
                }
            });
            self.obj().add_controller(key_controller);

            self.prompt_label.set(prompt_label).ok();
            self.entry.set(entry).ok();
            self.error_label.set(error_label).ok();
            self.ok_button.set(ok_button).ok();
            self.cancel_button.set(cancel_button).ok();
        }

        fn signals() -> &'static [Signal] {
            static SIGNALS: OnceLock<Vec<Signal>> = OnceLock::new();
            SIGNALS.get_or_init(|| {
                vec![
                    Signal::builder("confirmed").param_types([String::static_type()]).build(),
                    Signal::builder("cancelled").build(),
                ]
            })
        }
    }
    impl WidgetImpl for PromptEntry {}
    impl BinImpl for PromptEntry {}

    impl PromptEntry {
        /// Re-runs the validator against the entry's current text,
        /// updating the Ok button's sensitivity and the red error message
        /// together — called on every keystroke, plus once whenever the
        /// validator or the text itself is set programmatically, so those
        /// paths can't leave the two out of sync with what's actually
        /// showing.
        pub(super) fn revalidate(&self) {
            let entry = self.entry.get().unwrap();
            let text = entry.text();
            let error = self.validator.borrow().as_ref().and_then(|f| f(&text));
            let error_label = self.error_label.get().unwrap();
            match &error {
                Some(msg) => {
                    entry.add_css_class("error");
                    error_label.set_label(msg);
                    error_label.set_visible(true);
                }
                None => {
                    entry.remove_css_class("error");
                    error_label.set_visible(false);
                }
            }
            self.ok_button.get().unwrap().set_sensitive(error.is_none() && !text.is_empty());
        }

        pub(super) fn try_confirm(&self) {
            let entry = self.entry.get().unwrap();
            let text = entry.text().to_string();
            if text.is_empty() { return; }
            let invalid = self.validator.borrow().as_ref().is_some_and(|f| f(&text).is_some());
            if invalid { return; }
            self.obj().emit_by_name::<()>("confirmed", &[&text]);
        }
    }
}

use adw::prelude::*;
use adw::subclass::prelude::*;
use gtk::glib;

glib::wrapper! {
    pub struct PromptEntry(ObjectSubclass<imp::PromptEntry>)
        @extends adw::Bin, gtk::Widget;
}

impl Default for PromptEntry {
    fn default() -> Self { Self::new() }
}

/// Which kind of on-screen keyboard a prompt wants, once one exists —
/// entirely unused today (`PromptEntry::set_keyboard_type()`'s doc
/// comment), just declared ahead of time so call sites don't need
/// revisiting once it is. `#[allow(dead_code)]`: only `AlphaUnderscore`
/// is actually requested by anything yet, but these aren't placeholders
/// to delete — they're the exact set the future keyboard is expected to
/// need.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[allow(dead_code)]
pub(crate) enum KeyboardType {
    Numeric,
    NumericDot,
    Alpha,
    AlphaUnderscore,
    #[default]
    Complete,
}

impl PromptEntry {
    pub(crate) fn new() -> Self {
        glib::Object::new()
    }

    /// The line of text shown above the entry (e.g. "Preset name:").
    pub(crate) fn set_prompt(&self, text: &str) {
        self.imp().prompt_label.get().unwrap().set_text(text);
    }

    /// Pre-fills the entry (e.g. the existing name, for a rename prompt)
    /// and re-validates immediately, so a pre-filled invalid value (which
    /// shouldn't happen in practice, but costs nothing to handle) doesn't
    /// leave the Ok button in a stale sensitive state.
    pub(crate) fn set_text(&self, text: &str) {
        self.imp().entry.get().unwrap().set_text(text);
        self.imp().revalidate();
    }

    pub(crate) fn set_ok_label(&self, text: &str) {
        self.imp().ok_button.get().unwrap().set_label(text);
    }

    pub(crate) fn set_keyboard_type(&self, kind: KeyboardType) {
        self.imp().keyboard_type.set(kind);
    }

    /// `f` returns `Some(error message)` for invalid text, `None` for
    /// valid — checked on every keystroke (`Ok` stays insensitive, and the
    /// message shows in red, whenever it returns `Some`). Re-validates
    /// immediately against whatever text is already in the entry.
    pub(crate) fn set_validator(&self, f: impl Fn(&str) -> Option<String> + 'static) {
        *self.imp().validator.borrow_mut() = Some(Box::new(f));
        self.imp().revalidate();
    }

    pub(crate) fn grab_focus_entry(&self) {
        self.imp().entry.get().unwrap().grab_focus();
    }

    /// Fired when the user confirms (Ok, or Enter in the entry) with text
    /// that passed the validator — never fired for empty or invalid text.
    pub(crate) fn connect_confirmed<F: Fn(&Self, String) + 'static>(&self, f: F) -> glib::SignalHandlerId {
        self.connect_local("confirmed", false, move |args| {
            let this = args[0].get::<Self>().unwrap();
            let text = args[1].get::<String>().unwrap();
            f(&this, text);
            None
        })
    }

    pub(crate) fn connect_cancelled<F: Fn(&Self) + 'static>(&self, f: F) -> glib::SignalHandlerId {
        self.connect_local("cancelled", false, move |args| {
            let this = args[0].get::<Self>().unwrap();
            f(&this);
            None
        })
    }
}

/// Hosts a [`PromptEntry`] in a plain window — the "for now" half of this
/// widget's own doc comment: everything about validation/confirm/cancel
/// lives in the widget itself, so swapping this specific hosting for a
/// `gtk::Overlay` inside another window later (e.g. Kiosk mode, once an
/// on-screen keyboard exists to pair with it) wouldn't need to touch
/// `PromptEntry` at all, just this function. No `.transient_for()`, same
/// reasoning as every other secondary window in this codebase — cage
/// doesn't map one (see `EqPanel::present()`'s own window-construction
/// comment). Doesn't wire Ok/Cancel to close the window itself — the
/// caller already needs its own `connect_confirmed()`/`connect_cancelled()`
/// to act on the result, and closing alongside that is one line either
/// way, so this doesn't impose a hidden extra handler on top of the
/// caller's own.
pub(crate) fn present_prompt_window(title: &str, entry: &PromptEntry) -> adw::Window {
    let header = adw::HeaderBar::new();
    let toolbar = adw::ToolbarView::new();
    toolbar.add_top_bar(&header);
    toolbar.set_content(Some(entry));

    let window = adw::Window::builder()
        .title(title)
        .content(&toolbar)
        .default_width(360)
        .build();
    // Same Modern-theme gradient background every other secondary window
    // gets (a plain CSS background-image, no ArtBackground widget
    // involved — see `EqPanel::present()`'s own comment for the full
    // story) — `.add_css_class()` after construction, never
    // `.css_classes([...])` in the builder.
    window.add_css_class("modern-bg-window");

    window.present();
    entry.grab_focus_entry();
    window
}
