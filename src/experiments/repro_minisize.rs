//! Minimal standalone repro for a live-observed bug: after toggling a
//! window between "mini" (resizable(false), size_request(-1,-1)) and
//! "full" (resizable(true), size_request(360,200)) chrome — mirroring
//! rustywiim's own DeviceWindowInner::apply_window_chrome() — then
//! restoring default_size() to the real full-panel size, default_size
//! silently resets to some *other* size later on, confirmed live to
//! coincide with clicking on a different window (a focus-change event).
//! Reproduced only on Ubuntu 24.04 so far, not Fedora 44.
//!
//! RETIRED: kept in src/experiments/ for the findings below, no longer
//! built (nothing under src/experiments/ is a compile target). To run it
//! again, copy it back to src/bin/ first, then: cargo run --bin repro_minisize
//! Then, on Window A: click "Enter mini", then one of the "Exit mini"
//! variants (A-E), then click Window B (or anywhere outside Window A) to
//! shift focus, and watch the console for a notify::default-width/height
//! line appearing with no button click in between. Window F is a separate,
//! self-contained test of a different fix (see below) — its own
//! Enter/Exit-mini buttons don't touch Window A at all.
//!
//! Findings so far, across variants A-E (all on Window A):
//! - A (current app behavior: size_request set to a smaller fixed floor
//!   (360,200), default_size set to the real size right after — the two
//!   left out of sync): reproduces the bug.
//! - B (size_request kept equal to default_size, never changed again):
//!   does NOT reproduce it — but permanently pins size_request at the
//!   restored size, so the window can never be dragged smaller afterward
//!   until the next mini toggle. Rejected as unusable.
//! - C (toggle resizable false->true around the resize, a workaround
//!   reported in the GTK community for a related bug class): reproduces it.
//! - D (never set any size_request floor at all, just size_request(-1,-1)
//!   throughout): reproduces it too — disproving the theory that GTK
//!   reconciles default_size toward whatever size_request holds. Instead it
//!   snapped to whatever the window's *actual* transitional size happened
//!   to be at the moment default_size was called.
//! - E (B, but relax size_request back down once the actual size has
//!   genuinely caught up to the target): still reproduces it, snapping to a
//!   *third*, seemingly arbitrary transitional value — neither the floor
//!   nor the target. This shows the exact value of size_request isn't what
//!   matters; a *second* set_size_request() call on the same window, ever,
//!   seems to be what reopens the hazard, regardless of timing or value —
//!   looks like GTK/Mutter caching and later replaying a stale intermediate
//!   configure from the resize animation itself.
//!
//! Window F tests the resulting theory directly: never call
//! set_size_request() more than once on a given window for its entire
//! life. A single compromise floor (COMPROMISE_MIN_SIZE) is set once at
//! construction; F's own Enter/Exit-mini buttons only ever touch
//! resizable/default_size afterward, never size_request again.

use std::cell::RefCell;
use std::rc::Rc;

use adw::prelude::*;
use gtk::glib;

const FULL_SIZE: (i32, i32) = (900, 700);
const MINI_SIZE: (i32, i32) = (380, 128);
const FULL_MIN_SIZE: (i32, i32) = (360, 200);
// A single compromise floor low enough not to hurt MINI_SIZE's height (128),
// set ONCE at Window F's construction and never touched again — see F's
// comment for why "never call set_size_request a second time, ever" is the
// theory this specifically tests, after every attempt to relax/replace a
// floor mid-session (variants A/C/D/E) reproduced the bug.
const COMPROMISE_MIN_SIZE: (i32, i32) = (200, 100);

fn main() -> glib::ExitCode {
    let app = adw::Application::builder()
        .application_id("io.github.ozbenh.repro-minisize")
        .build();

    app.connect_activate(|app| {
        let win_a = adw::ApplicationWindow::builder()
            .application(app)
            .title("Window A (the one we're testing)")
            .default_width(FULL_SIZE.0)
            .default_height(FULL_SIZE.1)
            .build();

        win_a.connect_notify_local(Some("default-width"), |w, _| {
            println!(
                "{:?} [A] notify::default-width -> default_size={:?} is_maximized={} width={} height={}",
                std::time::Instant::now(), w.default_size(), w.is_maximized(), w.width(), w.height(),
            );
        });
        win_a.connect_notify_local(Some("default-height"), |w, _| {
            println!(
                "{:?} [A] notify::default-height -> default_size={:?} is_maximized={} width={} height={}",
                std::time::Instant::now(), w.default_size(), w.is_maximized(), w.width(), w.height(),
            );
        });

        let vbox = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(8)
            .margin_top(20).margin_bottom(20).margin_start(20).margin_end(20).build();
        let btn_mini    = gtk::Button::with_label("Enter mini");
        let btn_full_a  = gtk::Button::with_label("Exit mini (A: current behavior — fixed 360x200 floor)");
        let btn_full_b  = gtk::Button::with_label("Exit mini (B: sync size_request to match default_size)");
        let btn_full_c  = gtk::Button::with_label("Exit mini (C: toggle resizable around the resize)");
        let btn_full_d  = gtk::Button::with_label("Exit mini (D: no floor at all — size_request(-1,-1))");
        let btn_full_e  = gtk::Button::with_label("Exit mini (E: sync size_request, then relax once actual size catches up)");
        vbox.append(&btn_mini);
        vbox.append(&btn_full_a);
        vbox.append(&btn_full_b);
        vbox.append(&btn_full_c);
        vbox.append(&btn_full_d);
        vbox.append(&btn_full_e);
        win_a.set_content(Some(&vbox));

        // decorated is deliberately left alone here (always true) — the
        // real bug is about size_request/default_size reconciliation, not
        // decoration, and keeping the title bar means both windows stay
        // independently movable/clickable regardless of stacking order.
        btn_mini.connect_clicked(glib::clone!(#[weak] win_a, move |_| {
            println!("--- Enter mini: resizable(false), size_request(-1,-1), default_size({},{})",
                MINI_SIZE.0, MINI_SIZE.1);
            win_a.set_resizable(false);
            win_a.set_size_request(-1, -1);
            win_a.set_default_size(MINI_SIZE.0, MINI_SIZE.1);
        }));
        // A: reproduces rustywiim's exact current apply_window_chrome(false)
        // + exit_mini_mode() sequence — size_request set to a *smaller*
        // fixed floor, default_size set to the real (larger) size right
        // after. This is the baseline that's confirmed buggy live.
        btn_full_a.connect_clicked(glib::clone!(#[weak] win_a, move |_| {
            println!("--- Exit mini (A): resizable(true), size_request({},{}), default_size({},{})",
                FULL_MIN_SIZE.0, FULL_MIN_SIZE.1, FULL_SIZE.0, FULL_SIZE.1);
            win_a.set_resizable(true);
            win_a.set_size_request(FULL_MIN_SIZE.0, FULL_MIN_SIZE.1);
            win_a.set_default_size(FULL_SIZE.0, FULL_SIZE.1);
            win_a.unmaximize();
        }));
        // B: keep size_request in sync with default_size (matching, not a
        // smaller floor) — testing the theory that GTK reconciles toward
        // size_request on some later re-layout (e.g. a focus change), so
        // keeping them equal should leave nothing to "snap back" to.
        btn_full_b.connect_clicked(glib::clone!(#[weak] win_a, move |_| {
            println!("--- Exit mini (B): resizable(true), size_request({},{}) [=default_size], default_size({},{})",
                FULL_SIZE.0, FULL_SIZE.1, FULL_SIZE.0, FULL_SIZE.1);
            win_a.set_resizable(true);
            win_a.set_size_request(FULL_SIZE.0, FULL_SIZE.1);
            win_a.set_default_size(FULL_SIZE.0, FULL_SIZE.1);
            win_a.unmaximize();
        }));
        // C: a workaround reported in the GTK community for a related
        // "window manager doesn't acknowledge the new size" class of bug —
        // toggle resizable false -> resize -> true around the resize, to
        // force the WM to re-negotiate rather than trust a cached size.
        btn_full_c.connect_clicked(glib::clone!(#[weak] win_a, move |_| {
            println!("--- Exit mini (C): size_request({},{}), resizable(false) -> default_size({},{}) -> resizable(true)",
                FULL_MIN_SIZE.0, FULL_MIN_SIZE.1, FULL_SIZE.0, FULL_SIZE.1);
            win_a.set_size_request(FULL_MIN_SIZE.0, FULL_MIN_SIZE.1);
            win_a.set_resizable(false);
            win_a.set_default_size(FULL_SIZE.0, FULL_SIZE.1);
            win_a.set_resizable(true);
            win_a.unmaximize();
        }));
        // D: unlike B, never re-establish *any* size_request floor — if
        // there's no size_request at all, there's nothing for GTK to
        // reconcile default_size back toward. Also tests whether B's
        // downside (size_request pinned to the restored size blocks
        // shrinking smaller afterward) is actually necessary to dodge the
        // bug, or whether the mere presence of *some* stale size_request
        // value (any value, even one equal to a stale default_size) is
        // what matters, in which case having none at all should be safe too.
        btn_full_d.connect_clicked(glib::clone!(#[weak] win_a, move |_| {
            println!("--- Exit mini (D): resizable(true), size_request(-1,-1), default_size({},{})",
                FULL_SIZE.0, FULL_SIZE.1);
            win_a.set_resizable(true);
            win_a.set_size_request(-1, -1);
            win_a.set_default_size(FULL_SIZE.0, FULL_SIZE.1);
            win_a.unmaximize();
        }));
        // E: same as B (size_request synced to the target, forcing the
        // compositor to actually honor the resize — D showed a bare
        // default_size() isn't enough on its own), but not permanently:
        // once the window's *actual* width()/height() catches up to the
        // target, relax size_request back down to a small floor so normal
        // shrink-resizing isn't blocked afterward. Polls at 50ms rather
        // than guessing a fixed delay, since the settle time is confirmed
        // to vary run to run.
        btn_full_e.connect_clicked(glib::clone!(#[weak] win_a, move |_| {
            println!("--- Exit mini (E): resizable(true), size_request({},{}) [=default_size], default_size({},{}), will relax to {:?} once actual size matches",
                FULL_SIZE.0, FULL_SIZE.1, FULL_SIZE.0, FULL_SIZE.1, FULL_MIN_SIZE);
            win_a.set_resizable(true);
            win_a.set_size_request(FULL_SIZE.0, FULL_SIZE.1);
            win_a.set_default_size(FULL_SIZE.0, FULL_SIZE.1);
            win_a.unmaximize();
            glib::timeout_add_local(std::time::Duration::from_millis(50), glib::clone!(#[weak] win_a, #[upgrade_or] glib::ControlFlow::Break, move || {
                if win_a.width() == FULL_SIZE.0 && win_a.height() == FULL_SIZE.1 {
                    println!("--- Exit mini (E): actual size caught up, relaxing size_request to {:?}", FULL_MIN_SIZE);
                    win_a.set_size_request(FULL_MIN_SIZE.0, FULL_MIN_SIZE.1);
                    glib::ControlFlow::Break
                } else {
                    glib::ControlFlow::Continue
                }
            }));
        }));

        win_a.present();

        // A second, unrelated window — click on it (or just click its
        // titlebar/content) to shift focus away from Window A, which is
        // the action confirmed live to precede the spurious reset.
        let win_b = adw::ApplicationWindow::builder()
            .application(app)
            .title("Window B (click this to shift focus)")
            .default_width(400)
            .default_height(300)
            .build();
        win_b.set_content(Some(&gtk::Label::new(Some("Click me to shift focus to Window B"))));
        win_b.present();

        // Window F: F showed size_request being touched a *second* time,
        // ever, on the same window is what seems to actually matter — not
        // its value, not the timing, not whether it matches default_size
        // (E relaxed it, at exactly the moment the resize had genuinely
        // finished, and it still broke, snapping to a transitional
        // mid-animation size instead of either the floor or the target).
        // So F never touches size_request more than once, period: set to a
        // single compromise floor at construction, low enough not to
        // squash MINI_SIZE's height, and every Enter/Exit toggle below
        // only ever touches resizable/default_size, never size_request
        // again.
        let win_f = adw::ApplicationWindow::builder()
            .application(app)
            .title("Window F (size_request set ONCE at construction, never again)")
            .default_width(FULL_SIZE.0)
            .default_height(FULL_SIZE.1)
            .build();
        win_f.set_size_request(COMPROMISE_MIN_SIZE.0, COMPROMISE_MIN_SIZE.1);

        win_f.connect_notify_local(Some("default-width"), |w, _| {
            println!(
                "{:?} [F] notify::default-width -> default_size={:?} is_maximized={} width={} height={}",
                std::time::Instant::now(), w.default_size(), w.is_maximized(), w.width(), w.height(),
            );
        });
        win_f.connect_notify_local(Some("default-height"), |w, _| {
            println!(
                "{:?} [F] notify::default-height -> default_size={:?} is_maximized={} width={} height={}",
                std::time::Instant::now(), w.default_size(), w.is_maximized(), w.width(), w.height(),
            );
        });

        let vbox_f = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(8)
            .margin_top(20).margin_bottom(20).margin_start(20).margin_end(20).build();
        let btn_mini_f = gtk::Button::with_label("Enter mini (F)");
        let btn_full_f = gtk::Button::with_label("Exit mini (F)");
        vbox_f.append(&btn_mini_f);
        vbox_f.append(&btn_full_f);
        win_f.set_content(Some(&vbox_f));

        btn_mini_f.connect_clicked(glib::clone!(#[weak] win_f, move |_| {
            println!("--- [F] Enter mini: resizable(false), default_size({},{}) [size_request left untouched at {:?}]",
                MINI_SIZE.0, MINI_SIZE.1, COMPROMISE_MIN_SIZE);
            win_f.set_resizable(false);
            win_f.set_default_size(MINI_SIZE.0, MINI_SIZE.1);
        }));
        btn_full_f.connect_clicked(glib::clone!(#[weak] win_f, move |_| {
            println!("--- [F] Exit mini: resizable(true), default_size({},{}) [size_request left untouched at {:?}]",
                FULL_SIZE.0, FULL_SIZE.1, COMPROMISE_MIN_SIZE);
            win_f.set_resizable(true);
            win_f.set_default_size(FULL_SIZE.0, FULL_SIZE.1);
            win_f.unmaximize();
        }));

        win_f.present();

        // Window G: F disproved "never touch size_request twice" (it
        // reverted to the *previous* default_size, 380x128, with no
        // size_request call involved at all) — so instead of touching
        // size_request only at the mini/full transition, G keeps it
        // permanently chasing the window's own *actual* live width/height,
        // via a plain notify::width/height handler wired once at
        // construction. Enter/Exit-mini here only ever set
        // resizable/default_size, same as F — size_request is entirely
        // driven by this live tracker, in both directions, so it should
        // never fall behind (blocking shrink) or fall out of sync (opening
        // the reset hazard).
        let win_g = adw::ApplicationWindow::builder()
            .application(app)
            .title("Window G (size_request continuously synced to actual live size)")
            .default_width(FULL_SIZE.0)
            .default_height(FULL_SIZE.1)
            .build();

        // GtkWidget's width()/height() aren't real bindable GObject
        // properties (unlike default-width/default-height) — a
        // notify::width/notify::height hook never fires at all. Poll
        // instead, same spirit as this codebase's own established
        // add_tick_callback()-based frame polling elsewhere.
        glib::timeout_add_local(std::time::Duration::from_millis(16), glib::clone!(#[weak] win_g, #[upgrade_or] glib::ControlFlow::Break, move || {
            let (cw, ch) = (win_g.width(), win_g.height());
            if cw > 0 && ch > 0 && win_g.size_request() != (cw, ch) {
                win_g.set_size_request(cw, ch);
            }
            glib::ControlFlow::Continue
        }));

        win_g.connect_notify_local(Some("default-width"), |w, _| {
            println!(
                "{:?} [G] notify::default-width -> default_size={:?} is_maximized={} width={} height={} size_request={:?}",
                std::time::Instant::now(), w.default_size(), w.is_maximized(), w.width(), w.height(), w.size_request(),
            );
        });
        win_g.connect_notify_local(Some("default-height"), |w, _| {
            println!(
                "{:?} [G] notify::default-height -> default_size={:?} is_maximized={} width={} height={} size_request={:?}",
                std::time::Instant::now(), w.default_size(), w.is_maximized(), w.width(), w.height(), w.size_request(),
            );
        });

        let vbox_g = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(8)
            .margin_top(20).margin_bottom(20).margin_start(20).margin_end(20).build();
        let btn_mini_g = gtk::Button::with_label("Enter mini (G)");
        let btn_full_g = gtk::Button::with_label("Exit mini (G)");
        vbox_g.append(&btn_mini_g);
        vbox_g.append(&btn_full_g);
        win_g.set_content(Some(&vbox_g));

        btn_mini_g.connect_clicked(glib::clone!(#[weak] win_g, move |_| {
            println!("--- [G] Enter mini: resizable(false), default_size({},{})", MINI_SIZE.0, MINI_SIZE.1);
            win_g.set_resizable(false);
            win_g.set_default_size(MINI_SIZE.0, MINI_SIZE.1);
        }));
        btn_full_g.connect_clicked(glib::clone!(#[weak] win_g, move |_| {
            println!("--- [G] Exit mini: resizable(true), default_size({},{})", FULL_SIZE.0, FULL_SIZE.1);
            win_g.set_resizable(true);
            win_g.set_default_size(FULL_SIZE.0, FULL_SIZE.1);
            win_g.unmaximize();
        }));

        win_g.present();

        // Window H: instead of trying to prevent the transition-time reset
        // (every attempt at that has failed or had an unacceptable cost),
        // keep size_request permanently synced to the *current* size
        // (like B, known-safe against the reset bug) but transiently drop
        // it to (-1,-1) right when a resize is about to start — both our
        // own programmatic Enter/Exit-mini calls, and (the actual point of
        // this variant) a real user-driven edge-drag, detected via a
        // Capture-phase click watcher on the window so it sees the press
        // before GTK's own internal CSD border-drag handling does — same
        // technique this codebase already uses for
        // device_window/display.rs's handle_transport_key(). The watcher
        // doesn't claim the sequence, so the normal drag still proceeds
        // unblocked by whatever the floor currently is. Once nothing has
        // changed default-width/height for SETTLE_MS, raise size_request
        // back up to match the now-settled actual size — same debounce
        // shape as this codebase's own schedule_config_save().
        const SETTLE_MS: u64 = 400;
        let win_h = adw::ApplicationWindow::builder()
            .application(app)
            .title("Window H (floor dropped on press, re-synced after settle)")
            .default_width(FULL_SIZE.0)
            .default_height(FULL_SIZE.1)
            .build();
        win_h.set_size_request(FULL_SIZE.0, FULL_SIZE.1);

        let settle_timer: Rc<RefCell<Option<glib::SourceId>>> = Rc::new(RefCell::new(None));
        let schedule_settle = {
            let win_h = win_h.downgrade();
            let settle_timer = Rc::clone(&settle_timer);
            move || {
                if let Some(id) = settle_timer.borrow_mut().take() { id.remove(); }
                let win_h2 = win_h.clone();
                let settle_timer2 = Rc::clone(&settle_timer);
                let id = glib::timeout_add_local_once(std::time::Duration::from_millis(SETTLE_MS), move || {
                    *settle_timer2.borrow_mut() = None;
                    let Some(win_h) = win_h2.upgrade() else { return };
                    let (w, h) = (win_h.width(), win_h.height());
                    if w > 0 && h > 0 {
                        println!("--- [H] settled at {w}x{h}, syncing size_request to match");
                        win_h.set_size_request(w, h);
                    }
                });
                *settle_timer.borrow_mut() = Some(id);
            }
        };

        win_h.connect_notify_local(Some("default-width"), {
            let schedule_settle = schedule_settle.clone();
            move |w, _| {
                println!(
                    "{:?} [H] notify::default-width -> default_size={:?} is_maximized={} width={} height={} size_request={:?}",
                    std::time::Instant::now(), w.default_size(), w.is_maximized(), w.width(), w.height(), w.size_request(),
                );
                schedule_settle();
            }
        });
        win_h.connect_notify_local(Some("default-height"), {
            let schedule_settle = schedule_settle.clone();
            move |w, _| {
                println!(
                    "{:?} [H] notify::default-height -> default_size={:?} is_maximized={} width={} height={} size_request={:?}",
                    std::time::Instant::now(), w.default_size(), w.is_maximized(), w.width(), w.height(), w.size_request(),
                );
                schedule_settle();
            }
        });

        // Capture phase so this sees the press before GTK's own internal
        // CSD border-drag detection does; not claimed, so that detection
        // (and the resulting begin_resize()) still proceeds normally.
        let press_watcher = gtk::GestureClick::new();
        press_watcher.set_propagation_phase(gtk::PropagationPhase::Capture);
        press_watcher.set_button(0); // any button
        press_watcher.connect_pressed(glib::clone!(#[weak] win_h, move |_, _, _, _| {
            println!("--- [H] press detected, dropping size_request to (-1,-1) to unblock any incoming resize drag");
            win_h.set_size_request(-1, -1);
        }));
        win_h.add_controller(press_watcher);

        let vbox_h = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(8)
            .margin_top(20).margin_bottom(20).margin_start(20).margin_end(20).build();
        let btn_mini_h = gtk::Button::with_label("Enter mini (H)");
        let btn_full_h = gtk::Button::with_label("Exit mini (H)");
        vbox_h.append(&btn_mini_h);
        vbox_h.append(&btn_full_h);
        win_h.set_content(Some(&vbox_h));

        btn_mini_h.connect_clicked(glib::clone!(#[weak] win_h, #[strong] schedule_settle, move |_| {
            println!("--- [H] Enter mini: size_request(-1,-1), resizable(false), default_size({},{})", MINI_SIZE.0, MINI_SIZE.1);
            win_h.set_size_request(-1, -1);
            win_h.set_resizable(false);
            win_h.set_default_size(MINI_SIZE.0, MINI_SIZE.1);
            schedule_settle();
        }));
        btn_full_h.connect_clicked(glib::clone!(#[weak] win_h, #[strong] schedule_settle, move |_| {
            println!("--- [H] Exit mini: size_request(-1,-1), resizable(true), default_size({},{})", FULL_SIZE.0, FULL_SIZE.1);
            win_h.set_size_request(-1, -1);
            win_h.set_resizable(true);
            win_h.set_default_size(FULL_SIZE.0, FULL_SIZE.1);
            win_h.unmaximize();
            schedule_settle();
        }));

        win_h.present();
    });

    app.run()
}
