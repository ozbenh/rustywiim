#![allow(deprecated)] // glib clone! old-style @strong syntax

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use adw::prelude::*;
use gtk::{Align, Box as GtkBox, Button, Label, Orientation};

use crate::ui::art_background;
use crate::device::state::DeviceState;

// ── Widget bundles ────────────────────────────────────────────────────────────
// Grouping related widgets + associated state into structs keeps signal-handler
// signatures short and the closures easy to read.

/// The mini window's *chrome*: everything around the actual playback
/// display (which is `view`, a self-contained `MiniPlaybackView`). The
/// top-bar controls stay chrome because they presuppose the two-panel
/// window pair ("restore to full window" means nothing to e.g. a future
/// devlist-row host), and the blurred `ArtBackground` because it's
/// visually the chrome's background — the view just feeds it artwork.
pub(crate) struct MiniWidgets {
    pub root:          gtk::WindowHandle,
    pub device_label:  Label,
    #[allow(dead_code)] // owned for lifetime; the widget is parented to the top bar
    pub menu_btn:      gtk::MenuButton,
    pub restore_btn:   Button,
    pub close_btn:     Button,
    pub view:          crate::ui::views::playback_mini::MiniPlaybackView,
}

// ── Build functions ───────────────────────────────────────────────────────────

/// Returns the header-bar widget to actually add as the toolbar's top bar,
/// the two existing toggle buttons, and a small spinner shown while
/// `ConnectionState::Connecting` — see `reset_device_ui()`. The spinner is
/// **not** attached anywhere in here — `adw::HeaderBar` reserves its own
/// far-right corner for the native CSD window buttons
/// (`show-end-title-buttons`, on by default), so overlaying the header
/// itself puts the spinner right on top of/behind those, effectively
/// invisible. Instead the caller overlays it on the window's *content*
/// area (`window_overlay` in `mod.rs`), below the header row entirely —
/// still an overlay child, not packed, so it never shifts any of the
/// header's own buttons even briefly, it just floats on top of whatever's
/// already in that corner of the content instead.
pub(in crate::ui) fn build_header(
    init_panel_visible: bool,
) -> (adw::HeaderBar, gtk::ToggleButton, gtk::Button, gtk::Spinner) {
    let header = adw::HeaderBar::new();

    let sidebar_btn = gtk::ToggleButton::builder()
        .icon_name("sidebar-show-symbolic")
        .active(init_panel_visible)
        .tooltip_text("Toggle presets panel")
        .build();
    sidebar_btn.add_css_class("sidebar-toggle");
    header.pack_start(&sidebar_btn);

    header.pack_end(&crate::ui::menu::build_menu_button(true));

    // Plain Button, not ToggleButton: clicking it only ever means "switch to
    // mini mode" — a one-shot action, not a persistent on/off state. It also
    // only ever lives in the full panel's header (invisible whenever mini
    // mode is actually active), so there's no "pressed" state for it to
    // meaningfully show even if it had one.
    let mini_btn = gtk::Button::builder()
        .icon_name("view-restore-symbolic")
        .tooltip_text("Mini player")
        .build();
    header.pack_end(&mini_btn);

    // margin_top clears the header bar's own height (it's overlaid on the
    // window's whole content area, below the header row — see the doc
    // comment above) so it lands in open content space, not on top of the
    // header row itself.
    let connecting_spinner = gtk::Spinner::builder()
        .halign(Align::End)
        .valign(Align::Start)
        .margin_end(12)
        .margin_top(56)
        .visible(false)
        .build();
    connecting_spinner.set_size_request(20, 20);
    connecting_spinner.add_css_class("connecting-spinner");

    (header, sidebar_btn, mini_btn, connecting_spinner)
}

pub(in crate::ui) fn build_left_pane(
    presets: &crate::ui::views::presets::PresetsView,
    io:      &crate::ui::views::io::InputOutputView,
) -> gtk::Box {
    // "panel-card" is only ever styled under the RustyWiiM Modern theme
    // (see modern.css) — inert everywhere else, so no theme branching here.
    let left_pane = GtkBox::builder()
        .orientation(Orientation::Vertical)
        .css_classes(["panel-card"])
        .build();
    left_pane.append(presets);
    left_pane.append(io);
    left_pane
}

fn build_mini_top_bar() -> (Label, gtk::MenuButton, Button, Button, GtkBox) {
    let mini_device_label = Label::builder()
        .label("").css_classes(["mini-device-label"])
        .halign(Align::Start).hexpand(true)
        .ellipsize(gtk::pango::EllipsizeMode::End)
        .build();
    let mini_restore_btn = Button::builder()
        .icon_name("view-fullscreen-symbolic")
        .css_classes(["mini-restore-btn"])
        .tooltip_text("Restore to full window")
        .build();
    let mini_menu_btn = crate::ui::menu::build_menu_button(true);
    mini_menu_btn.add_css_class("mini-restore-btn");
    mini_menu_btn.add_css_class("flat");
    let mini_close_btn = Button::builder()
        .icon_name("window-close-symbolic")
        .css_classes(["mini-restore-btn"])
        .tooltip_text("Close")
        .build();
    let mini_top_bar = GtkBox::builder()
        .orientation(Orientation::Horizontal).spacing(4)
        .margin_start(14).margin_end(12).margin_top(10).margin_bottom(4)
        .css_classes(["mini-top-bar"])
        .build();
    mini_top_bar.append(&mini_device_label);
    mini_top_bar.append(&mini_restore_btn);
    mini_top_bar.append(&mini_menu_btn);
    mini_top_bar.append(&mini_close_btn);
    (mini_device_label, mini_menu_btn, mini_restore_btn, mini_close_btn, mini_top_bar)
}

/// Narrowest/widest the mini window can be dragged to via `build_mini_resize_handle()`.
const MINI_WIDTH_MIN: i32 = 260;
const MINI_WIDTH_MAX: i32 = 900;

/// Default mini-panel width the first time a device ever shows it (no
/// drag-resize yet this session, no saved `mini_window_width` in config) —
/// used by `ui/mod.rs`'s `DeviceWindowInner::apply_window_chrome()` and the
/// mini-mode startup restore in `new_inner()`. Was previously just the
/// dedicated mini window's own `default_width(380)` builder call.
pub(in crate::ui) const MINI_WIDTH_DEFAULT: i32 = 380;

/// Hit-test width (px) for the right-edge resize drag, measured inward from
/// `stable`'s own right edge in `wire_mini_resize()` — a bit wider than the
/// visible cursor strip below, for an easier grab target.
const MINI_RESIZE_EDGE_PX: f64 = 10.0;

/// A thin, invisible, full-height strip along the window's right edge.
/// Purely a cursor hint (`ew-resize` on hover) — the actual resize gesture
/// is wired onto a *different*, stable-origin widget by `wire_mini_resize()`
/// (see its doc comment for why this strip can't carry the gesture itself).
fn build_mini_resize_handle() -> GtkBox {
    let handle = GtkBox::builder()
        .width_request(6)
        .hexpand(false).vexpand(true)
        .halign(Align::End)
        .build();
    handle.set_cursor_from_name(Some("ew-resize"));
    handle
}

/// Wires a right-edge resize drag onto `stable`, driven entirely by hand
/// (`gtk::GestureDrag` + `gtk::Window::set_default_width()`) rather than the
/// compositor-mediated `gdk::Toplevel::begin_resize()` a GTK CSD
/// border-drag would normally use. `begin_resize()` was tried and abandoned:
/// it hands the pointer grab to the compositor with no completion event to
/// react to, and was observed to silently do nothing in one real case —
/// flipping `resizable(true)` immediately before calling it raced GTK/
/// Wayland's asynchronous application of that property to the compositor,
/// which still believed the window was fixed-size and dropped the request.
///
/// `stable` must be a widget whose own on-screen *origin* (top-left) never
/// moves as a side effect of the resize itself — `mini_outer` in
/// `build_mini_window()`, which only ever grows rightward and keeps a fixed
/// top-left, qualifies; the resize-cursor strip from
/// `build_mini_resize_handle()` does not, because it's right-aligned and so
/// its own origin necessarily shifts right as the window grows. The first
/// attempt attached the gesture to that strip directly: `GtkGestureDrag`'s
/// offset is relative to whatever widget it's attached to, so each resize
/// we applied moved the reference frame for the *next* reading, creating a
/// feedback loop (`new_width` computed from an offset that itself shrank by
/// however much we'd already grown the window). Symptoms were exactly what
/// that predicts: rapid oscillation between two sizes while the pointer
/// briefly stopped moving (each resize is itself a synthetic "the pointer's
/// local position just changed" event, triggering another, opposite
/// correction), and systematic undershoot while dragging continuously. A
/// widget anchored at a fixed origin doesn't have this problem — its
/// reported offset is a clean read of actual pointer movement.
///
/// Right-edge-only, not left+right: GTK4/Wayland gives a client no way to
/// reposition its own top-level window, so growing from a fixed top-left
/// anchor (i.e. rightward) is the only direction that can be made to track
/// the cursor correctly.
fn wire_mini_resize(stable: &gtk::Overlay) {
    let stable = stable.clone();
    let gesture = gtk::GestureDrag::new();
    gesture.set_button(1); // primary button only
    let start_width:   Rc<Cell<i32>>                            = Rc::new(Cell::new(0));
    // Latest computed width from drag-update, applied at most once per
    // rendered frame by the tick callback below rather than immediately —
    // calling set_default_width() straight from drag-update fired a
    // resize/layout pass on every raw pointer-motion event, faster than the
    // compositor could redraw, and briefly showed a "shadow" of the
    // previous size superimposed while the drag was still in progress.
    let pending_width: Rc<Cell<Option<i32>>>                    = Rc::new(Cell::new(None));
    let tick_id:       Rc<RefCell<Option<gtk::TickCallbackId>>> = Rc::new(RefCell::new(None));

    gesture.connect_drag_begin(glib::clone!(
        @strong stable, @strong start_width, @strong pending_width, @strong tick_id
        => move |gesture, x, _y| {
            // `stable` spans the whole window, so this fires for a press
            // anywhere in it — only actually arm a resize near its right edge.
            if x < stable.width() as f64 - MINI_RESIZE_EDGE_PX {
                return;
            }
            // Claim the sequence: mini_root (an ancestor, gtk::WindowHandle)
            // has its own built-in click-and-drag-to-move gesture on the
            // same pointer sequence. Without an explicit claim here, that
            // ancestor gesture is free to also recognize the drag and wins
            // it — the cursor still showed the resize shape (that's just
            // CSS on hover), but the drag itself moved the window.
            gesture.set_state(gtk::EventSequenceState::Claimed);
            let Some(win) = stable.native().and_then(|n| n.downcast::<gtk::Window>().ok()) else { return };
            start_width.set(win.width());
            pending_width.set(None);
            let id = stable.add_tick_callback(glib::clone!(@strong win, @strong pending_width => move |_, _| {
                if let Some(w) = pending_width.take() {
                    win.set_default_width(w);
                }
                glib::ControlFlow::Continue
            }));
            *tick_id.borrow_mut() = Some(id);
        }
    ));
    gesture.connect_drag_update(glib::clone!(
        @strong start_width, @strong pending_width, @strong tick_id => move |_, offset_x, _offset_y| {
            if tick_id.borrow().is_none() { return; } // press wasn't near the edge
            let new_width = (start_width.get() + offset_x.round() as i32).clamp(MINI_WIDTH_MIN, MINI_WIDTH_MAX);
            pending_width.set(Some(new_width));
        }
    ));
    gesture.connect_drag_end(glib::clone!(
        @strong tick_id, @strong pending_width => move |_, _, _| {
            let Some(id) = tick_id.borrow_mut().take() else { return }; // press wasn't near the edge
            id.remove();
            pending_width.set(None);
        }
    ));
    stable.add_controller(gesture);
}

pub(in crate::ui) fn build_mini_window(
    ds:    &DeviceState,
    icons: &Rc<crate::ui::icons::IconSet>,
) -> (MiniWidgets, gtk::WindowHandle) {
    let (mini_device_label, mini_menu_btn, mini_restore_btn, mini_close_btn, mini_top_bar) = build_mini_top_bar();

    // ArtBackground sits *inside* mini-outer (not wrapping the whole
    // window) so mini-outer's own overflow(Hidden) + border-radius clips
    // both the background layer and the foreground content to the same
    // rounded shape — wrapping the whole window instead would let the
    // (rectangular) blur peek out past the rounded corners, where the
    // window itself is otherwise fully transparent to the real desktop.
    // Built before the view: the view is handed a reference so it can feed
    // it artwork (the view has the data; the chrome owns the surface).
    let mini_art_bg = art_background::ArtBackground::new();
    mini_art_bg.set_hexpand(true);
    mini_art_bg.set_vexpand(true);
    mini_art_bg.set_visible(false); // gated live — see update_art_background_visibility()

    let view = crate::ui::views::playback_mini::MiniPlaybackView::new(ds, icons, Some(&mini_art_bg));

    let mini_content = GtkBox::builder()
        .orientation(Orientation::Vertical).spacing(0)
        .build();
    mini_content.append(&mini_top_bar);
    mini_content.append(&view);

    let mini_outer = gtk::Overlay::new();
    mini_outer.set_child(Some(&mini_art_bg));
    mini_outer.add_overlay(&mini_content);
    // ArtBackground (the main/measured child) reports no intrinsic size — it's
    // meant to be sized by whatever allocates it — so without this the Overlay
    // sizes itself off a 0×0 child instead of mini_content, and the window's
    // actual height (there is no explicit default_height, only default_width)
    // ends up wrong. mini_content is the widget that should drive sizing here.
    mini_outer.set_measure_overlay(&mini_content, true);
    mini_outer.add_css_class("mini-outer");
    mini_outer.set_overflow(gtk::Overflow::Hidden);

    // An undecorated window (decorated(false) below) has no server-side
    // titlebar/border providing the usual edge hit-testing, so there's no UI
    // to resize it at all without this: a thin invisible strip along the
    // right edge, added as the topmost overlay child so it receives the
    // press before mini_content underneath (cursor hint only — see
    // wire_mini_resize()'s doc comment for why the actual gesture is wired
    // onto mini_outer itself instead of this strip).
    mini_outer.add_overlay(&build_mini_resize_handle());
    wire_mini_resize(&mini_outer);

    let mini_root = gtk::WindowHandle::new();
    mini_root.set_child(Some(&mini_outer));

    // Unlike an older version of this function, there is no dedicated
    // `gtk::ApplicationWindow` built here anymore — `mini_root` is packed
    // as the *shared* device window's content whenever mini mode is active
    // (see `ui/mod.rs`'s `DeviceWindowInner::apply_window_chrome()`), which
    // is also where `decorated(false)`/`resizable(false)`/the "mini-window"
    // CSS class are applied, live, only while mini content is showing.
    // `resizable(false)` specifically still matters there for the same
    // reason it did when this was its own window: GNOME/Mutter only offers
    // its edge-tiling/snap-to-maximize gesture to windows advertised as
    // resizable, so an always-resizable undecorated mini panel would be
    // eligible for it to silently full-screen. `wire_mini_resize()`'s
    // `set_default_width()` calls still work regardless of `resizable`:
    // unlike `gdk::Toplevel::begin_resize()` (a compositor-side interactive
    // resize — abandoned for this drag entirely, see that function's doc
    // comment), it's a pure client-side size *request*, not something that
    // needs the compositor to agree the window is resizable first.

    let mini = MiniWidgets {
        root:          mini_root.clone(),
        device_label:  mini_device_label,
        menu_btn:      mini_menu_btn,
        restore_btn:   mini_restore_btn,
        close_btn:     mini_close_btn,
        view,
    };

    (mini, mini_root)
}
