//! Standalone A/B test for the "stash the window instead of destroying it
//! across Kiosk mode" idea being planned for rustywiim's real Kiosk
//! enter/exit path — no real Kiosk window or device connection needed.
//!
//! One "permanent" window simulates the always-present Kiosk surface and
//! is never touched by anything below except your own mouse — its buttons
//! drive two independent "test" windows, standing in for per-device
//! windows: Window A is driven via `minimize()`/`present()` (iconify),
//! Window B via `set_visible(false)`/`set_visible(true)` (hide). Move,
//! resize, and maximize each test window by hand first, then use the
//! permanent window's buttons to simulate a Kiosk-enter/exit cycle on
//! each, and judge purely by eye whether it comes back where/how it was —
//! GTK4/Wayland doesn't expose window position to clients at all, so
//! there's no coordinate to print; also worth trying across monitors/
//! workspaces if you have more than one.
//!
//! RETIRED: kept in src/experiments/ for posterity, no longer built
//! (nothing under src/experiments/ is a compile target). To run it again,
//! copy it back to src/bin/ first, then: cargo run --bin repro_kiosk_stash

use adw::prelude::*;
use gtk::glib;

fn build_test_window(app: &adw::Application, title: &str, tag: &'static str, n: i32) -> adw::ApplicationWindow {
    let win = adw::ApplicationWindow::builder()
        .application(app)
        .title(title)
        .default_width(500 + n * 80)
        .default_height(400 + n * 60)
        .build();

    win.connect_notify_local(Some("maximized"), move |w, _| {
        println!("[{tag}] notify::maximized -> is_maximized={} width={} height={}", w.is_maximized(), w.width(), w.height());
    });
    win.connect_notify_local(Some("default-width"), move |w, _| {
        println!("[{tag}] notify::default-width -> default_size={:?} is_maximized={}", w.default_size(), w.is_maximized());
    });

    let content = gtk::Label::new(Some(&format!(
        "{title}\n\nMove me, resize me, maybe maximize me — then use\nthe permanent window's buttons to drive me."
    )));
    content.set_margin_top(40);
    content.set_margin_bottom(40);
    content.set_margin_start(40);
    content.set_margin_end(40);

    // Explicit AdwHeaderBar rather than relying on whatever default
    // titlebar plain decorated(true) would otherwise produce — guarantees
    // a normal, visible, draggable bar on every window regardless of
    // theme/compositor defaults, so all these heavily-overlapping windows
    // can actually be dragged apart by hand.
    let toolbar_view = adw::ToolbarView::new();
    toolbar_view.add_top_bar(&adw::HeaderBar::new());
    toolbar_view.set_content(Some(&content));
    win.set_content(Some(&toolbar_view));

    win
}

fn main() -> glib::ExitCode {
    let app = adw::Application::builder()
        .application_id("io.github.ozbenh.repro-kiosk-stash")
        .build();

    app.connect_activate(|app| {
        let win_a = build_test_window(app, "Test Window A (driven via minimize/present)", "A", 0);
        let win_b = build_test_window(app, "Test Window B (driven via hide/show)", "B", 1);
        win_a.present();
        win_b.present();

        let permanent = adw::ApplicationWindow::builder()
            .application(app)
            .title("Permanent (fake Kiosk) window — never touched programmatically")
            .default_width(460)
            .default_height(260)
            .build();

        let vbox = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(8)
            .margin_top(20).margin_bottom(20).margin_start(20).margin_end(20).build();
        vbox.append(&gtk::Label::new(Some(
            "Move/resize/maximize the two test windows by hand first,\nthen use these buttons to drive them.",
        )));

        let btn_minimize_a = gtk::Button::with_label("Enter fake kiosk: minimize() Window A");
        let btn_present_a  = gtk::Button::with_label("Exit fake kiosk: present() Window A");
        let btn_hide_b     = gtk::Button::with_label("Enter fake kiosk: set_visible(false) Window B");
        let btn_show_b     = gtk::Button::with_label("Exit fake kiosk: set_visible(true) Window B");
        for b in [&btn_minimize_a, &btn_present_a, &btn_hide_b, &btn_show_b] {
            vbox.append(b);
        }
        let permanent_toolbar = adw::ToolbarView::new();
        permanent_toolbar.add_top_bar(&adw::HeaderBar::new());
        permanent_toolbar.set_content(Some(&vbox));
        permanent.set_content(Some(&permanent_toolbar));

        btn_minimize_a.connect_clicked(glib::clone!(#[weak] win_a, move |_| {
            println!("--- [A] minimize()");
            win_a.minimize();
        }));
        btn_present_a.connect_clicked(glib::clone!(#[weak] win_a, move |_| {
            println!("--- [A] present() (un-minimize)");
            win_a.present();
        }));
        btn_hide_b.connect_clicked(glib::clone!(#[weak] win_b, move |_| {
            println!("--- [B] set_visible(false)");
            win_b.set_visible(false);
        }));
        btn_show_b.connect_clicked(glib::clone!(#[weak] win_b, move |_| {
            println!("--- [B] set_visible(true)");
            win_b.set_visible(true);
        }));

        permanent.present();
    });

    app.run()
}
