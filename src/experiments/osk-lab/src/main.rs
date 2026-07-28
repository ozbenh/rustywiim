//! Standalone click-through test harness for `ui::osk` (rustywiim's
//! on-screen keyboard widget) — two plain `gtk::Entry` fields to type into
//! (to demonstrate `wire_auto_show()` needs no per-entry wiring — either
//! one just works), plus a popover to switch between all three layouts
//! live, so the widget can be exercised without going through the full
//! app (EQ preset rename / Add Device flows). Also exercises
//! `osk::wire_auto_show()` — shows the keyboard only while one of the two
//! entries has focus, hidden otherwise — which `PromptEntry` itself
//! doesn't use (its keyboard stays visible for the whole prompt's
//! lifetime instead); this is its only real exercise today.
//!
//! `osk.rs` itself is pulled in via `#[path]` — the literal same file
//! rustywiim's main binary compiles, not a reimplementation, so whatever
//! this shows is exactly what the real app shows. Its `mod prompt_entry`
//! is not: `osk.rs`'s only reason to reference that module at all is
//! `KeyboardType`, used solely for the `osk_layout()` conversion — nothing
//! this harness needs (it drives `osk::OskLayout` directly) — so pulling in
//! the *real* `PromptEntry` (and, transitively, its `config`/`ui::touch`
//! app-state ties) would just be dead weight here. This stub only
//! reproduces that one enum's shape.
//!
//! Not wired into the main `Cargo.toml`/workspace — like every other
//! `src/experiments/` tool, run it standalone: `cd src/experiments/osk-lab
//! && cargo run`.

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

const LAYOUTS: &[(&str, osk::OskLayout)] = &[
    ("Numeric", osk::OskLayout::Numeric),
    ("AlphaNumeric", osk::OskLayout::AlphaNumeric),
    ("Full", osk::OskLayout::Full),
];

fn rebuild(kb_holder: &gtk::Box, layout: osk::OskLayout) {
    while let Some(child) = kb_holder.first_child() {
        kb_holder.remove(&child);
    }
    kb_holder.append(&osk::build(layout));
}

fn main() -> glib::ExitCode {
    let app = adw::Application::builder().application_id("com.example.OskLab").build();

    app.connect_activate(|app| {
        let entry = gtk::Entry::builder()
            .hexpand(true)
            .placeholder_text("Type here…")
            .build();
        let entry2 = gtk::Entry::builder()
            .hexpand(true)
            .placeholder_text("...or here — no per-entry wiring needed")
            .build();

        let kb_holder = gtk::Box::builder().orientation(gtk::Orientation::Vertical).build();

        let mode_label = gtk::Label::builder().halign(gtk::Align::Start).build();
        let set_mode_label = {
            let mode_label = mode_label.clone();
            move |name: &str| mode_label.set_text(&format!("Mode: {name}"))
        };
        set_mode_label(LAYOUTS[0].0);

        let popover_box = gtk::Box::new(gtk::Orientation::Vertical, 4);
        let popover = gtk::Popover::builder().child(&popover_box).build();
        let menu_btn = gtk::MenuButton::builder().label("Mode ▾").popover(&popover).build();

        for &(name, layout) in LAYOUTS {
            let btn = gtk::Button::builder().label(name).build();
            btn.connect_clicked(glib::clone!(
                #[strong] kb_holder, #[strong] popover, #[strong] mode_label,
                move |_| {
                    rebuild(&kb_holder, layout);
                    mode_label.set_text(&format!("Mode: {name}"));
                    popover.popdown();
                }
            ));
            popover_box.append(&btn);
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
        content.append(&kb_holder);

        let toolbar = adw::ToolbarView::new();
        toolbar.add_top_bar(&header);
        toolbar.set_content(Some(&content));

        let window = adw::ApplicationWindow::builder()
            .application(app)
            .title("osk-lab")
            .content(&toolbar)
            .default_width(440)
            .default_height(420)
            .build();

        rebuild(&kb_holder, LAYOUTS[0].1);
        // Targets kb_holder (the stable container), not the keyboard
        // widget itself — rebuild() swaps that out on every mode change,
        // but the container it lives in never changes, so this only needs
        // wiring once.
        osk::wire_auto_show(&kb_holder, &window);
        window.present();
        entry.grab_focus();
    });

    app.run()
}
