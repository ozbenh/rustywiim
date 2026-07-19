//! Kiosk mode's single dedicated window: a fullscreen, undecorated surface
//! showing exactly one device at a time, meant for single-surface kiosk
//! compositors that can't juggle rustywiim's normal multi-window setup
//! (one `DeviceWindow` per open device plus a separate `DiscoveryWindow`).
//!
//! Not a GObject — a plain chrome struct like `DiscoveryWindow`, since
//! this owns window lifecycle/CSS/keyboard wiring, not a self-contained
//! bindable widget. Shows exactly one device's basic `PlaybackView` at a
//! time, deliberately minimal for a first cut (no side panel); a
//! transparent top-right button showing the bound device's name opens a
//! popover containing a `DeviceListView` to switch devices.
//!
//! Keyboard shortcuts are owned entirely by this window, not shared with
//! `DeviceWindow`'s own controller — "K" exits kiosk mode here; there is
//! deliberately no "M" (kiosk has no mini mode). The common transport/
//! volume keys delegate to `views::common::handle_transport_key()`, the
//! same helper `DeviceWindow` uses, rather than being reimplemented here.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use adw::prelude::*;
use glib::clone;

use crate::config;
use crate::device::discovery_manager::DiscoveryManager;
use crate::device::state::{DeviceState, FullModeGuard};
use crate::ui::art_background::ArtBackground;
use crate::ui::icons::IconSet;
use crate::ui::update_art_background_visibility;
use crate::ui::views;
use crate::ui::views::devlist::DeviceListView;
use crate::ui::views::playback_full::{
    compute_wide_right_art_side, wide_right_margin_h, PlaybackLayout, PlaybackView,
};

/// The currently-shown device's view plus the `FullModeGuard` keeping its
/// polling at full fidelity for as long as Kiosk mode is looking at it —
/// dropping this (on unbind/rebind, or the whole window closing) releases
/// it, same as a `DeviceWindow` does for its own `DeviceState`. `key` is
/// empty when nothing is really selected (see `bind_device()`'s "no
/// device" branch) — `DiscoveryManager::set_window_open()` no-ops on an
/// empty key, so that case needs no special-casing elsewhere.
struct BoundDevice {
    key:        String,
    ds:         DeviceState,
    view:       PlaybackView,
    /// Rebuilt alongside `view` on every `bind_device()` call rather than
    /// rebound in place — `StatusBarView` follows the same "bound to one
    /// `DeviceState` at construction, never rebound" contract every view
    /// does (`views/mod.rs`). Shown unconditionally for this first cut
    /// (no Settings toggle exists yet to make it optional).
    _status_bar: crate::ui::views::status_bar::StatusBarView,
    _full_mode: FullModeGuard,
}

pub(crate) struct KioskWindow {
    window:        adw::ApplicationWindow,
    manager:       DiscoveryManager,
    icons:         Rc<IconSet>,
    art_bg:        ArtBackground,
    /// Holds the current `PlaybackView` (real or stub), swapped in place by
    /// `bind_device()`. A stable overlay child added *before* `device_btn`
    /// (see `new()`), rather than adding/removing the view directly on the
    /// overlay each time — that would make each new view the *last*-added
    /// overlay child and so stack on top of `device_btn`, covering it,
    /// since `gtk::Overlay` z-orders purely by add order.
    content_holder: gtk::Box,
    device_btn:    gtk::Button,
    popover:       gtk::Popover,
    bound:         RefCell<Option<BoundDevice>>,
    /// Toggled by "L" (`toggle_layout()`) — persists across device
    /// switches within a session (not saved to config); seeded from
    /// `new()`'s `initial_layout` (`--kiosk:layout`, default `WideRight`)
    /// on a fresh launch.
    layout:        Cell<PlaybackLayout>,
}

impl KioskWindow {
    pub(crate) fn new(
        app:            &adw::Application,
        manager:        &DiscoveryManager,
        icons:          &Rc<IconSet>,
        exit_kiosk:     Rc<dyn Fn()>,
        initial_layout: PlaybackLayout,
    ) -> Rc<Self> {
        // resizable(false) is deliberately *not* set here, unlike the mini
        // window — that flag exists there specifically to keep GNOME/Mutter
        // from offering its edge-tiling/snap-to-maximize gesture on an
        // undecorated panel; a fullscreen window has no edges to tile from
        // in the first place, and resizable(false) risks the compositor
        // refusing the resize a fullscreen request inherently requires.
        let window = adw::ApplicationWindow::builder()
            .application(app)
            .title("RustyWiiM")
            .decorated(false)
            .css_classes(["kiosk-window"])
            .build();

        // ArtBackground is the overlay's main (measured) child — same
        // shape as DeviceWindow's own window_overlay — with hexpand/vexpand
        // set explicitly, since it's what actually drives the whole
        // window's size; PlaybackView (added as an overlay child below,
        // swapped by bind_device()) doesn't request expansion as a bare
        // widget on its own the way DeviceWindow's ArtBackground does.
        // Starts visible (its own default) rather than explicitly hidden —
        // unlike the mini window's own ArtBackground, this one is also the
        // overlay's main/measured child, and an invisible widget reports
        // zero size, which would undo the fullscreen-sizing fix above.
        // `update_art_background_visibility()` below corrects the initial
        // visibility for the current theme, same as DeviceWindow's own.
        let art_bg = ArtBackground::new();
        art_bg.set_hexpand(true);
        art_bg.set_vexpand(true);

        let overlay = gtk::Overlay::new();
        overlay.set_child(Some(&art_bg));

        // Stable overlay child (never removed/re-added) holding whichever
        // PlaybackView is current — see the struct field's doc comment for
        // why this indirection exists instead of swapping directly on the
        // overlay.
        let content_holder = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .hexpand(true)
            .vexpand(true)
            .build();
        overlay.add_overlay(&content_holder);

        // Everything that floats over the content, added after
        // content_holder so it always stacks on top (gtk::Overlay z-orders
        // purely by add order) — just the device-name button for now, more
        // to come here later (e.g. a settings icon).
        let device_btn = Self::build_floating_buttons(&overlay);

        let device_list = DeviceListView::new(manager, icons);
        // A gtk::Popover sizes itself to its content's *natural* size, not
        // "whatever space is available" the way a window's content does —
        // DeviceListView's internal ScrolledWindow relies on the latter
        // (fine inside DiscoveryWindow, which has its own explicit default
        // size), so without an explicit size here the popover collapses to
        // a barely-visible sliver.
        device_list.set_size_request(360, 480);
        let popover = gtk::Popover::new();
        popover.add_css_class("kiosk-devlist-popover");
        popover.set_child(Some(&device_list));
        popover.set_parent(&device_btn);

        window.set_content(Some(&overlay));
        // Sets art_bg's initial visibility for the current theme — this
        // window is now a real toplevel of the GtkApplication (via
        // .application(app) above), so the generic list_toplevels() walk
        // reaches it like any other window.
        update_art_background_visibility();

        let this = Rc::new(Self {
            window: window.clone(),
            manager: manager.clone(),
            icons: Rc::clone(icons),
            art_bg,
            content_holder,
            device_btn: device_btn.clone(),
            popover: popover.clone(),
            bound: RefCell::new(None),
            layout: Cell::new(initial_layout),
        });

        device_btn.connect_clicked(clone!(#[weak] popover, move |_| {
            if popover.is_visible() { popover.popdown(); } else { popover.popup(); }
        }));
        device_list.connect_device_selected({
            let weak = Rc::downgrade(&this);
            move |_, key| {
                let Some(this) = weak.upgrade() else { return };
                this.bind_device(Some(key));
                this.popover.popdown();
            }
        });

        // Keyboard: "K" exits kiosk mode, "L" swaps between the Classic and
        // WideRight playback layouts (neither shared with DeviceWindow's
        // own controller — no "M" here at all, kiosk has no mini mode).
        // Everything else delegates to the shared transport-key helper
        // against whatever's currently bound.
        let key_ctrl = gtk::EventControllerKey::new();
        key_ctrl.set_propagation_phase(gtk::PropagationPhase::Capture);
        key_ctrl.connect_key_pressed({
            let weak = Rc::downgrade(&this);
            move |_, keyval, _keycode, state| {
                let Some(this) = weak.upgrade() else { return glib::Propagation::Proceed };
                if state.intersects(gtk::gdk::ModifierType::CONTROL_MASK | gtk::gdk::ModifierType::ALT_MASK) {
                    return glib::Propagation::Proceed;
                }
                if let gtk::gdk::Key::k | gtk::gdk::Key::K = keyval {
                    exit_kiosk();
                    return glib::Propagation::Stop;
                }
                if let gtk::gdk::Key::l | gtk::gdk::Key::L = keyval {
                    this.toggle_layout();
                    return glib::Propagation::Stop;
                }
                let Some((ds, view)) = this.bound.borrow().as_ref().map(|b| (b.ds.clone(), b.view.clone())) else {
                    return glib::Propagation::Proceed;
                };
                let (prev, play, next) = view.transport_buttons();
                views::common::handle_transport_key(&ds, &view.volume(), &prev, &next, &play, keyval)
            }
        });
        window.add_controller(key_ctrl);

        this
    }

    /// Builds the buttons that float over the content (added to `overlay`
    /// after `content_holder`, so they always stack on top of whichever
    /// `PlaybackView` is currently showing) — just the device-name button
    /// for this first cut; more are expected here later (e.g. a settings
    /// icon), all added from this one place rather than scattered through
    /// `new()`. Returns the device button specifically, since `new()` still
    /// needs it to wire up the popover/click handling.
    fn build_floating_buttons(overlay: &gtk::Overlay) -> gtk::Button {
        let device_btn = gtk::Button::builder()
            .label("Select device")
            .css_classes(["kiosk-device-btn"])
            .halign(gtk::Align::End)
            .valign(gtk::Align::Start)
            .build();
        overlay.add_overlay(&device_btn);
        device_btn
    }

    /// Resolves `key` (a `device_key()` result — see `DiscoveryManager`)
    /// through `manager.device_state_for()`, tears down whatever was
    /// previously shown (dropping its `PlaybackView` and `FullModeGuard`
    /// together — `views/*`'s `dispose()` handles the view's own handler
    /// cleanup), and builds a fresh `PlaybackView` for the new one.
    ///
    /// `None` (or a `key` that no longer resolves to a tracked device)
    /// builds a `PlaybackView` against a standalone, never-connecting
    /// `DeviceState` instead of a bespoke "no device" placeholder — the
    /// same "no device spec" pattern `DeviceWindow::new_inner()` already
    /// uses, which naturally renders the disconnected/greyed-out state
    /// `PlaybackView` already supports, rather than adding a distinct
    /// "no device at all" mode to it.
    pub(crate) fn bind_device(&self, key: Option<&str>) {
        // Release whichever device was shown before, regardless of what
        // (if anything) replaces it — mirrors DeviceWindow's own
        // set_window_open bookkeeping so DiscoveryManager's prune logic
        // doesn't think a stale device is still "open" here. No-ops for
        // the empty key the "no device" branch below uses.
        if let Some(old) = self.bound.borrow_mut().take() {
            self.manager.set_window_open(&old.key, false);
        }
        // Clear generically rather than removing old.view/old.status_bar
        // individually, so any future addition here doesn't need its own
        // matching removal line.
        while let Some(child) = self.content_holder.first_child() {
            self.content_holder.remove(&child);
        }

        let resolved = key.and_then(|k| self.manager.device_state_for(k).map(|ds| (k.to_string(), ds)));
        let (key, ds, label) = match resolved {
            Some((key, ds)) => {
                self.manager.set_window_open(&key, true);
                let label = self.manager.entry_for(&key).map(|e| e.name).unwrap_or_else(|| key.clone());
                // Remembered so a future unbound entry (discovery window's
                // menu, or a fresh `--kiosk` launch) can restore this
                // device instead of starting with nothing selected — see
                // `AppState::enter_kiosk()`.
                config::update(|cfg| cfg.kiosk_last_uuid = Some(key.clone()));
                (key, ds, label)
            }
            None => {
                let ds = DeviceState::new(self.manager.rt(), String::new());
                ds.start_polling();
                (String::new(), ds, "Select device".to_string())
            }
        };
        self.finish_bind(key, ds, label);
    }

    /// Binds directly to an already-constructed `DeviceState`, skipping
    /// `manager.device_state_for()`'s `DiscoveryManager` lookup entirely —
    /// for `--connect --kiosk` together, where the device comes from
    /// `--connect`'s own direct-connection path (see `DIRECT_CONNECT`'s
    /// doc comment: it deliberately bypasses discovery/SSDP, so there's no
    /// tracked entry/uuid for `bind_device()`'s normal resolution to find).
    /// Not persisted as `kiosk_last_uuid` — the uuid isn't known yet
    /// either (unresolved until `getStatusEx` answers), and re-selecting
    /// it from the popover later isn't meaningful the way a discovered
    /// device's is.
    pub(crate) fn bind_direct(&self, ds: DeviceState, label: &str) {
        if let Some(old) = self.bound.borrow_mut().take() {
            self.manager.set_window_open(&old.key, false);
        }
        while let Some(child) = self.content_holder.first_child() {
            self.content_holder.remove(&child);
        }
        self.finish_bind(String::new(), ds, label.to_string());
    }

    /// Shared tail of `bind_device()`/`bind_direct()`: builds the fresh
    /// `PlaybackView`/`StatusBarView` for `ds` and installs the new
    /// `BoundDevice`. Caller has already released the old binding and
    /// cleared `content_holder`.
    fn finish_bind(&self, key: String, ds: DeviceState, label: String) {
        self.device_btn.set_label(&label);

        // Known synchronously for every device switch (the window's
        // already fullscreen and stable by then) — only the very first
        // bind at startup might still see 0 here, if the window hasn't
        // finished its initial fullscreen negotiation yet. See
        // PlaybackView::new()'s own doc comment for what this avoids.
        let (win_w, win_h) = (self.window.width(), self.window.height());
        let size_hint = if win_w > 0 && win_h > 0 { Some((win_w, win_h)) } else { None };
        let layout = self.layout.get();
        let view = PlaybackView::new(&ds, &self.icons, Some(&self.art_bg), layout, size_hint);
        // Fills content_holder (art_bg is the main overlay child driving
        // the window's own size, per new()'s comment) — still needs its
        // own explicit expansion to fill that stable holder.
        view.set_hexpand(true);
        view.set_vexpand(true);
        // Views start inactive (views/mod.rs's shared contract) — this is
        // what actually performs the initial render.
        view.set_active(true);
        self.content_holder.append(&view);

        // Status bar (network/BLE-remote/device info), same as
        // DeviceWindow's own — always shown for this first cut, no
        // Settings toggle exists yet to make it optional. Deliberately no
        // separator line above it here (unlike DeviceWindow's own bottom
        // bar) — Kiosk mode's version looks better without one.
        let status_bar = crate::ui::views::status_bar::StatusBarView::new(&ds, &self.icons, true);
        // Lines this bar's left/right content up with PlaybackView's own
        // edges above it — there's no separator between them to visually
        // excuse a mismatch anymore, and PlaybackView's own margin is
        // itself a fraction of the artwork's size (see wide_right_margin_h()),
        // not a fixed value, so this has to be (re)computed the same way
        // rather than hardcoded to match it. Classic's own margins are
        // small/fixed already, close enough to this bar's own defaults
        // that no adjustment is needed there.
        if layout == PlaybackLayout::WideRight {
            match size_hint {
                Some((w, h)) => {
                    status_bar.set_edge_margin(wide_right_margin_h(compute_wide_right_art_side(w, h)));
                }
                // Same cold-start gap PlaybackView itself guards against
                // (see its own tick-callback fallback's comment): on a
                // slower/different compositor the window may not have
                // reported a real size yet at this exact point, so
                // size_hint comes back None here and — unlike
                // PlaybackView, which keeps retrying on its own — this
                // call would otherwise just be skipped forever, leaving
                // the mismatch permanently uncorrected. Confirmed live: hit
                // every time on a Raspberry Pi 5, never on a desktop fast
                // enough to already have a real window size by this point.
                None => {
                    let weak_bar = status_bar.downgrade();
                    let window = self.window.clone();
                    self.window.add_tick_callback(move |_, _| {
                        let Some(bar) = weak_bar.upgrade() else { return glib::ControlFlow::Break };
                        let (w, h) = (window.width(), window.height());
                        if w <= 0 || h <= 0 { return glib::ControlFlow::Continue; }
                        bar.set_edge_margin(wide_right_margin_h(compute_wide_right_art_side(w, h)));
                        glib::ControlFlow::Break
                    });
                }
            }
        }
        status_bar.set_active(true);
        self.content_holder.append(&status_bar);

        *self.bound.borrow_mut() = Some(BoundDevice {
            key,
            _full_mode: ds.acquire_full(),
            ds,
            view,
            _status_bar: status_bar,
        });
    }

    /// "L" — swaps between the Classic and WideRight playback layouts and
    /// rebuilds the view for whatever's currently bound (a no-op on the
    /// binding itself if nothing is: the new layout still takes effect on
    /// the next `bind_device()`/`bind_direct()` either way, since both
    /// read `self.layout` fresh). Reuses the same `key`/`ds` `finish_bind()`
    /// already has, rather than going through `bind_device()`'s
    /// `DiscoveryManager` resolution again — this isn't a device switch.
    pub(crate) fn toggle_layout(&self) {
        self.layout.set(match self.layout.get() {
            PlaybackLayout::Classic => PlaybackLayout::WideRight,
            PlaybackLayout::WideRight => PlaybackLayout::Classic,
        });
        let Some((key, ds)) = self.bound.borrow().as_ref().map(|b| (b.key.clone(), b.ds.clone())) else {
            return;
        };
        let label = self.device_btn.label().map(|s| s.to_string()).unwrap_or_default();
        while let Some(child) = self.content_holder.first_child() {
            self.content_holder.remove(&child);
        }
        self.finish_bind(key, ds, label);
    }

    pub(crate) fn present(&self) {
        // fullscreen() before present(), not after: requesting it before
        // the window is first mapped lets it be negotiated as part of the
        // initial surface configure, avoiding the same class of GTK/
        // Wayland async-property-timing race already hit once before in
        // this codebase (see CLAUDE.md's `begin_resize()`/`resizable(true)`
        // gotcha) — calling it after present() risked the window briefly
        // (or indefinitely) staying at its small unfullscreened default size.
        self.window.fullscreen();
        self.window.present();
    }

    pub(crate) fn close(&self) {
        if let Some(old) = self.bound.borrow_mut().take() {
            self.manager.set_window_open(&old.key, false);
        }
        self.window.close();
    }

    /// The currently-shown device's key, or empty if nothing real is
    /// selected (the "no device" stub) — see `AppState::enter_kiosk()`'s
    /// reactive auto-pick, which uses this to avoid overriding a device
    /// the user has already picked by the time one becomes available.
    pub(crate) fn current_key(&self) -> String {
        self.bound.borrow().as_ref().map(|b| b.key.clone()).unwrap_or_default()
    }
}

