//! A touch on-screen keyboard, ported from a PyGObject/GTK4 prototype
//! (`osk-gemini.py`) rather than designed from scratch — same three modes,
//! same "inject into whichever `gtk::Editable` currently has focus" model
//! (so this widget never needs to know which entry it's typing into, or
//! even that it's being shown alongside `ui::prompt_entry::PromptEntry`
//! specifically — any focused `gtk::Entry`/`gtk::Text` on screen is a
//! valid target), same sticky-shift semantics. Every key button is
//! `set_focusable(false)` — the whole point is typing into some *other*
//! widget without ever stealing focus away from it.
//!
//! Not a GObject: no signals or properties are needed (it drives the
//! focused widget directly instead of emitting events for a caller to
//! act on), so — like `ui/views/common.rs`'s `SwipeText` — this is just a
//! plain constructor function returning a widget tree, with the mutable
//! shift/caps/button-label state captured by the button click closures.

use adw::prelude::*;

use super::prompt_entry::KeyboardType;
use std::cell::{Cell, RefCell};
use std::rc::Rc;

/// Which of the three fixed layouts to build — decided once, at
/// construction time, from the `PromptEntry`'s own `KeyboardType` (see
/// `KeyboardType::osk_layout()` below). Not itself persisted or otherwise
/// user-facing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum OskLayout {
    /// Digits, decimal point, backspace — IP addresses and other numeric
    /// entry.
    Numeric,
    /// Digits, lowercase letters, sticky shift, underscore, space,
    /// backspace — preset names and other short free text.
    AlphaNumeric,
    /// Full 102-key-style PC layout (symbols row, tab, caps lock, enter).
    Full,
}

impl KeyboardType {
    /// Collapses `PromptEntry`'s five call-site-facing hint variants down
    /// to the three layouts this widget actually builds — `NumericDot` and
    /// `Numeric` want the same keys (the numeric layout already has a `.`
    /// key), and `Alpha`/`AlphaUnderscore` likewise share one layout
    /// (`AlphaNumeric`'s already has an `_` key).
    pub(crate) fn osk_layout(self) -> OskLayout {
        match self {
            KeyboardType::Numeric | KeyboardType::NumericDot => OskLayout::Numeric,
            KeyboardType::Alpha | KeyboardType::AlphaUnderscore => OskLayout::AlphaNumeric,
            KeyboardType::Complete => OskLayout::Full,
        }
    }
}

/// Standard QWERTY shift-punctuation mapping — gated to the `Full` layout
/// only (see `on_key_clicked`/`update_labels`). `AlphaNumeric` deliberately
/// never reaches it, even though its digit row overlaps `SHIFT_MAP`'s keys
/// (`1`-`0`): a preset name only ever wants letters/digits/`.`/`_` (see
/// `is_valid_preset_name`), so `AlphaNumeric`'s `Shift` should only ever
/// toggle letter case, never substitute a digit for a symbol.
const SHIFT_MAP: &[(char, char)] = &[
    ('`', '~'), ('1', '!'), ('2', '@'), ('3', '#'), ('4', '$'), ('5', '%'),
    ('6', '^'), ('7', '&'), ('8', '*'), ('9', '('), ('0', ')'), ('-', '_'),
    ('=', '+'), ('[', '{'), (']', '}'), ('\\', '|'), (';', ':'), ('\'', '"'),
    (',', '<'), ('.', '>'), ('/', '?'),
];
fn shift_char(c: char) -> Option<char> {
    SHIFT_MAP.iter().find(|(k, _)| *k == c).map(|(_, v)| *v)
}

/// Keys that never toggle sticky shift off when pressed, and never get a
/// case/shift-mapped label — every non-single-character key.
const MODIFIER_KEYS: &[&str] = &["Shift", "Caps", "Back", "Space", "Enter", "Tab"];

fn layout_rows(layout: OskLayout) -> &'static [&'static [&'static str]] {
    match layout {
        OskLayout::Numeric => &[
            &["1", "2", "3"],
            &["4", "5", "6"],
            &["7", "8", "9"],
            &[".", "0", "Back"],
        ],
        OskLayout::AlphaNumeric => &[
            &["1", "2", "3", "4", "5", "6", "7", "8", "9", "0"],
            &["q", "w", "e", "r", "t", "y", "u", "i", "o", "p"],
            &["a", "s", "d", "f", "g", "h", "j", "k", "l"],
            &["Shift", "z", "x", "c", "v", "b", "n", "m", "Back"],
            &[".", "_", "Space"],
        ],
        OskLayout::Full => &[
            &["`", "1", "2", "3", "4", "5", "6", "7", "8", "9", "0", "-", "=", "Back"],
            &["Tab", "q", "w", "e", "r", "t", "y", "u", "i", "o", "p", "[", "]", "\\"],
            &["Caps", "a", "s", "d", "f", "g", "h", "j", "k", "l", ";", "'", "Enter"],
            &["Shift", "z", "x", "c", "v", "b", "n", "m", ",", ".", "/", "Shift"],
            &["Space"],
        ],
    }
}

/// Builds the keyboard widget for `layout`. The returned widget is
/// self-contained — just append it below/near whatever entry should
/// receive its input and it starts working immediately, no wiring to a
/// specific entry required (see the module doc comment).
pub(crate) fn build(layout: OskLayout) -> gtk::Widget {
    let root = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(6)
        .halign(gtk::Align::Center)
        .margin_top(12)
        .css_classes(["osk"])
        .build();

    let shift_active = Rc::new(Cell::new(false));
    let caps_active = Rc::new(Cell::new(false));
    let buttons: Rc<RefCell<Vec<(gtk::Button, &'static str)>>> = Rc::new(RefCell::new(Vec::new()));

    for row_keys in layout_rows(layout) {
        let row_box = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .spacing(6)
            .halign(gtk::Align::Center)
            .build();
        root.append(&row_box);

        for &key in *row_keys {
            // "Back" gets an icon instead of a text label — `edit-clear-
            // symbolic` as a placeholder stock icon until a dedicated
            // backspace glyph is picked.
            let btn = if key == "Back" {
                gtk::Button::builder().icon_name("edit-clear-symbolic").css_classes(["osk-key"]).build()
            } else {
                gtk::Button::builder().label(key).css_classes(["osk-key"]).build()
            };
            // The one load-bearing line in this whole widget: without it,
            // every key press would first steal focus away from the entry
            // it's meant to be typing into.
            btn.set_focusable(false);

            match key {
                "Space" => btn.set_size_request(220, 52),
                "Shift" | "Caps" | "Back" | "Enter" | "Tab" => btn.set_size_request(84, 52),
                _ => btn.set_size_request(52, 52),
            }

            btn.connect_clicked(glib::clone!(
                #[strong] shift_active, #[strong] caps_active, #[strong] buttons,
                move |btn| on_key_clicked(btn, key, layout, &shift_active, &caps_active, &buttons)
            ));

            row_box.append(&btn);
            buttons.borrow_mut().push((btn, key));
        }
    }

    root.upcast()
}

/// Wires `target` (typically `build()`'s own return value, but any widget
/// works — see below) to automatically show/hide based on whether
/// `window`'s currently focused widget is a `gtk::Editable`: visible
/// while some entry in the window has focus, hidden otherwise. Generic
/// over the *window*, not individual entries — unlike the old
/// `EventControllerFocus`-per-entry prototype this replaces (see
/// `src/experiments/osk/`'s retired `main.rs`), so it needs no per-entry
/// wiring and keeps working if entries are added/removed later; it's
/// really just `on_key_clicked()`'s own "whatever's focused" targeting
/// applied to visibility instead of key delivery.
///
/// `target` doesn't have to be the keyboard widget itself — passing a
/// stable container that a caller swaps the keyboard's child in and out
/// of (as `osk-lab` does, to switch layouts) works too, since this only
/// ever calls `set_visible()` on whatever's passed in.
///
/// `#[allow(dead_code)]`: not called from `PromptEntry` (whose keyboard
/// stays visible for the whole prompt's lifetime, not focus-driven) —
/// exercised instead via `src/experiments/osk-lab/`, kept here ready for
/// whenever something wants a keyboard that isn't tied to one dedicated
/// dialog (e.g. a Kiosk-mode-wide floating keyboard).
#[allow(dead_code)]
pub(crate) fn wire_auto_show(target: &impl IsA<gtk::Widget>, window: &impl IsA<gtk::Window>) {
    let target = target.clone().upcast::<gtk::Widget>();
    let sync = move |window: &gtk::Window| {
        let show = gtk::prelude::GtkWindowExt::focus(window).is_some_and(|w| w.downcast::<gtk::Editable>().is_ok());
        target.set_visible(show);
    };
    let window = window.clone().upcast::<gtk::Window>();
    sync(&window);
    window.connect_focus_widget_notify(move |w| sync(w));
}

fn on_key_clicked(
    btn: &gtk::Button, key: &'static str, layout: OskLayout,
    shift_active: &Rc<Cell<bool>>, caps_active: &Rc<Cell<bool>>,
    buttons: &Rc<RefCell<Vec<(gtk::Button, &'static str)>>>,
) {
    match key {
        "Shift" => {
            shift_active.set(!shift_active.get());
            update_labels(layout, shift_active, caps_active, buttons);
            return;
        }
        "Caps" => {
            caps_active.set(!caps_active.get());
            update_labels(layout, shift_active, caps_active, buttons);
            return;
        }
        // No multi-widget focus chain exists inside a PromptEntry (there's
        // only ever the one entry), so there's nothing meaningful for Tab
        // to move focus to yet — present in the Full layout for visual
        // completeness, otherwise a no-op.
        "Tab" => return,
        _ => {}
    }

    // GTK4 focuses an Entry's internal Gtk.Text child, not the Entry
    // widget itself — but both implement the Editable interface, so
    // downcasting to that (rather than a concrete Entry/Text type) reaches
    // either one the same way.
    let focused = btn.root().and_then(|r| r.focus());
    let Some(editable) = focused.and_then(|w| w.downcast::<gtk::Editable>().ok()) else { return };

    match key {
        "Back" => {
            if editable.selection_bounds().is_some() {
                editable.delete_selection();
            } else {
                let pos = editable.position();
                if pos > 0 {
                    editable.delete_text(pos - 1, pos);
                    editable.set_position(pos - 1);
                }
            }
        }
        "Enter" => {
            if let Ok(entry) = editable.clone().downcast::<gtk::Entry>() {
                entry.emit_activate();
            }
        }
        _ => {
            let ch = if key == "Space" {
                ' '
            } else {
                let mut c = key.chars().next().expect("row keys are never empty");
                let is_upper = shift_active.get() != caps_active.get();
                if c.is_ascii_alphabetic() {
                    if is_upper { c = c.to_ascii_uppercase(); }
                } else if layout == OskLayout::Full && shift_active.get() {
                    if let Some(mapped) = shift_char(c) { c = mapped; }
                }
                c
            };
            if editable.selection_bounds().is_some() { editable.delete_selection(); }
            let mut pos = editable.position();
            editable.insert_text(&ch.to_string(), &mut pos);
            editable.set_position(pos);
        }
    }

    if shift_active.get() && !MODIFIER_KEYS.contains(&key) {
        shift_active.set(false);
        update_labels(layout, shift_active, caps_active, buttons);
    }
}

fn update_labels(
    layout: OskLayout, shift_active: &Rc<Cell<bool>>, caps_active: &Rc<Cell<bool>>,
    buttons: &Rc<RefCell<Vec<(gtk::Button, &'static str)>>>,
) {
    let is_upper = shift_active.get() != caps_active.get();
    for (btn, key) in buttons.borrow().iter() {
        match *key {
            "Shift" => btn.set_css_classes(&["osk-key", "osk-key-active"][..1 + shift_active.get() as usize]),
            "Caps" => btn.set_css_classes(&["osk-key", "osk-key-active"][..1 + caps_active.get() as usize]),
            // Every other multi-char key ("Back", "Space", "Enter", "Tab")
            // has a fixed icon/label that never changes with Shift/Caps —
            // skip them, or e.g. "Back" would read its first char ('B')
            // as if it were a real single-letter key below.
            _ if key.len() != 1 => {}
            _ => {
                let c = key.chars().next().expect("checked above: key.len() == 1");
                if c.is_ascii_alphabetic() {
                    btn.set_label(&(if is_upper { c.to_ascii_uppercase() } else { c }).to_string());
                } else if layout == OskLayout::Full && shift_active.get() {
                    if let Some(mapped) = shift_char(c) { btn.set_label(&mapped.to_string()); }
                } else {
                    // Restores the plain key (e.g. a digit) once Shift is
                    // no longer active — otherwise a symbol shown while
                    // Shift was held (Full layout) would stick after
                    // release instead of reverting.
                    btn.set_label(key);
                }
            }
        }
    }
}
