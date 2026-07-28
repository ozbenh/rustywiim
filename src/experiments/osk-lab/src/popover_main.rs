//! Second osk-lab variant: same two-entry setup as `main.rs`, but the
//! keyboard lives inside a `gtk::Popover` anchored to whichever entry
//! currently has focus, instead of always occupying fixed space in the
//! window's own layout.
//!
//! The actual question this exists to answer: does a GTK4 `Popover`'s
//! native popup surface get clipped to its parent window's own bounds, or
//! can it extend past the window edge onto the desktop beyond it? GTK4's
//! popovers are backed by a real `GdkPopup` surface (an `xdg_popup` on
//! Wayland, an override-redirect window on X11) positioned *relative to*
//! the anchor widget but not, as far as the GTK4 docs describe
//! `GdkPopupLayout`, clipped to the parent toplevel's own allocation the
//! way GTK3's embedded popovers were — only constrained to stay on the
//! monitor. If that holds up live (resize this window small, anchor the
//! popover near an edge, and see whether the keyboard spills out past the
//! window frame onto the desktop), no fallback is needed. If it turns out
//! to clip after all, the fallback is a plain undecorated `gtk::Window`
//! positioned by hand near the entry (`gdk::Surface`'s own placement, not
//! a widget-parented popup), with a small CSS-drawn triangle standing in
//! for the arrow `gtk::Popover` already gives for free — which is also
//! why this variant needs no separate "notch" of its own: a `Popover`
//! defaults to `has_arrow(true)` and already points at whatever it's
//! anchored to.
//!
//! Unlike `main.rs`, the popover is shared between both entries rather
//! than built once per entry — it's re-parented (`unparent()` +
//! `set_parent()`) to whichever entry the window's `focus-widget`
//! property currently names, each time it changes, mirroring
//! `osk::wire_auto_show()`'s own "no per-entry wiring" philosophy one
//! step further (show *and* reposition, not just show).
//!
//! Not wired into the main `Cargo.toml` — see `main.rs`'s own doc comment
//! for the standalone-tools convention this follows. Run with
//! `cargo run --bin osk-lab-popover` from this directory.

#[path = "../../../ui/osk.rs"]
mod osk;

mod prompt_entry {
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
}

use adw::prelude::*;
use std::cell::RefCell;

const LAYOUTS: &[(&str, osk::OskLayout)] = &[
    ("Numeric", osk::OskLayout::Numeric),
    ("AlphaNumeric", osk::OskLayout::AlphaNumeric),
    ("Full", osk::OskLayout::Full),
];

fn set_kb(popover: &gtk::Popover, layout: osk::OskLayout) {
    popover.set_child(Some(&osk::build(layout)));
}

/// Runs on every `window`'s `notify::focus-widget` — pops `popover` down
/// when nothing editable has focus, otherwise re-parents it to whatever
/// does (only actually re-parenting when the anchor changed, so refocusing
/// the same entry doesn't unparent/reparent for nothing) and pops it back
/// up.
fn sync_popover(popover: &gtk::Popover, last_anchor: &RefCell<Option<gtk::Widget>>, window: &gtk::Window) {
    let focused = gtk::prelude::GtkWindowExt::focus(window);
    let Some(anchor) = focused.filter(|w| w.clone().downcast::<gtk::Editable>().is_ok()) else {
        popover.popdown();
        return;
    };
    if last_anchor.borrow().as_ref() != Some(&anchor) {
        if popover.parent().is_some() {
            popover.popdown();
            popover.unparent();
        }
        popover.set_parent(&anchor);
        *last_anchor.borrow_mut() = Some(anchor);
    }
    popover.popup();
}

fn main() -> glib::ExitCode {
    let app = adw::Application::builder().application_id("com.example.OskLabPopover").build();

    app.connect_activate(|app| {
        let entry = gtk::Entry::builder().hexpand(true).placeholder_text("Focus me").build();
        let entry2 = gtk::Entry::builder().hexpand(true).placeholder_text("...or me").build();

        let popover = gtk::Popover::builder().position(gtk::PositionType::Bottom).build();
        set_kb(&popover, LAYOUTS[0].1);

        let mode_label = gtk::Label::builder().halign(gtk::Align::Start).build();
        mode_label.set_text(&format!("Mode: {}", LAYOUTS[0].0));

        let mode_menu_box = gtk::Box::new(gtk::Orientation::Vertical, 4);
        let mode_popover = gtk::Popover::builder().child(&mode_menu_box).build();
        let menu_btn = gtk::MenuButton::builder().label("Mode ▾").popover(&mode_popover).build();
        for &(name, layout) in LAYOUTS {
            let btn = gtk::Button::builder().label(name).build();
            btn.connect_clicked(glib::clone!(
                #[strong] popover, #[strong] mode_popover, #[strong] mode_label,
                move |_| {
                    set_kb(&popover, layout);
                    mode_label.set_text(&format!("Mode: {name}"));
                    mode_popover.popdown();
                }
            ));
            mode_menu_box.append(&btn);
        }

        let header = adw::HeaderBar::new();
        header.pack_end(&menu_btn);

        let content = gtk::Box::new(gtk::Orientation::Vertical, 12);
        content.set_margin_top(16);
        content.set_margin_bottom(16);
        content.set_margin_start(16);
        content.set_margin_end(16);
        content.append(&mode_label);
        content.append(&entry);
        content.append(&entry2);
        // A tall spacer so there's real distance between the entries and
        // the window's bottom edge — makes it obvious on screen whether
        // the popped-up keyboard is clipped to that edge or spills past
        // it onto the desktop.
        content.append(&gtk::Box::builder().vexpand(true).build());

        let toolbar = adw::ToolbarView::new();
        toolbar.add_top_bar(&header);
        toolbar.set_content(Some(&content));

        let window = adw::ApplicationWindow::builder()
            .application(app)
            .title("osk-lab (popover)")
            .content(&toolbar)
            .default_width(360)
            .default_height(280)
            .build();

        let last_anchor: RefCell<Option<gtk::Widget>> = RefCell::new(None);
        let window_ref = window.clone().upcast::<gtk::Window>();
        sync_popover(&popover, &last_anchor, &window_ref);
        window_ref.connect_focus_widget_notify(move |w| sync_popover(&popover, &last_anchor, w));

        window.present();
        entry.grab_focus();
    });

    app.run()
}
