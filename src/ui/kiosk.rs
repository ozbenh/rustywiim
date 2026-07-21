//! Kiosk mode's single dedicated window: a fullscreen, undecorated surface
//! showing exactly one device at a time, meant for single-surface kiosk
//! compositors that can't juggle rustywiim's normal multi-window setup
//! (one `DeviceWindow` per open device plus a separate `DiscoveryWindow`).
//!
//! Not a GObject — a plain chrome struct like `DiscoveryWindow`, since
//! this owns window lifecycle/CSS/keyboard wiring, not a self-contained
//! bindable widget. Shows exactly one device's `PlaybackView`, plus a
//! collapsible side panel (presets/IO) split off it via `sidebar_paned` —
//! same shape as `DeviceWindow`'s own, just with its own floating toggle
//! button (top-left, symmetric to the device-name button) instead of a
//! header-bar one, since Kiosk mode has no header bar. Grouped in the same
//! floating cluster (a `top_left_group` `gtk::Box`, moving together as one
//! unit when the panel opens/closes): an exit-Kiosk button, next to the
//! panel toggle rather than its own separate corner, by request. A
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
    /// Same "rebuilt per bind" story as `_status_bar` — parented into
    /// `sidebar_paned`'s start slot by `finish_bind()`; kept here only so
    /// they're not otherwise unreferenced (nothing outside `finish_bind()`
    /// needs to reach them directly yet).
    _presets: crate::ui::views::presets::PresetsView,
    _io:      crate::ui::views::io::InputOutputView,
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
    /// Splits the side panel (presets/IO, start child) from the playback
    /// content (end child) — same shape as `DeviceWindow`'s own `paned`.
    /// Persistent across device switches (unlike its two children, which
    /// `finish_bind()` replaces each time): its own `position` is exactly
    /// the "is the panel open, and how wide" state, so nothing else needs
    /// to track that separately.
    sidebar_paned: gtk::Paned,
    /// In-flight sidebar-toggle slide animation, if any — mirrors
    /// `DeviceWindowInner::panel_anim`. Skipped (not just dropped) before
    /// starting a new one, same reason that file's own comment gives: a
    /// dropped `TimedAnimation` doesn't stop driving its callback target
    /// on its own.
    panel_anim:    RefCell<Option<adw::TimedAnimation>>,
    bound:         RefCell<Option<BoundDevice>>,
    /// Toggled by "L" (`toggle_layout()`) — persists across device
    /// switches within a session (not saved to config); seeded from
    /// `new()`'s `initial_layout` (`--kiosk:layout`, default `WideRight`)
    /// on a fresh launch.
    layout:        Cell<PlaybackLayout>,
}

/// Fallback width if the sidebar's start child is somehow unset when
/// opened (shouldn't happen in practice — `toggle_sidebar()` measures the
/// real content instead, see its own comment).
const SIDEBAR_OPEN_WIDTH: i32 = 280;
/// Extra room to the left of the sidebar's own content once opened, so it
/// doesn't sit flush against the divider.
const SIDEBAR_OPEN_MARGIN: i32 = 24;

impl KioskWindow {
    pub(crate) fn new(
        app:            &adw::Application,
        manager:        &DiscoveryManager,
        icons:          &Rc<IconSet>,
        exit_kiosk:     Rc<dyn Fn()>,
        initial_layout: PlaybackLayout,
        kiosk_only:     bool,
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

        // Splits the side panel from the playback content, same shape as
        // DeviceWindow's own paned — persistent across device switches
        // (see the struct field's own doc comment), unlike its two
        // children, which finish_bind() replaces each time. Starts closed
        // (position 0); shrink_start_child(true) is what lets it actually
        // reach 0 at all (a Paned otherwise won't shrink a child below its
        // own minimum size).
        let sidebar_paned = gtk::Paned::new(gtk::Orientation::Horizontal);
        sidebar_paned.set_hexpand(true);
        sidebar_paned.set_vexpand(true);
        sidebar_paned.set_resize_start_child(false);
        sidebar_paned.set_shrink_start_child(true);
        sidebar_paned.set_resize_end_child(true);
        sidebar_paned.set_shrink_end_child(false);
        sidebar_paned.set_position(0);
        content_holder.append(&sidebar_paned);

        // Everything that floats over the content, added after
        // content_holder so it always stacks on top (gtk::Overlay z-orders
        // purely by add order) — the device-name button and the sidebar
        // toggle, symmetric to it on the opposite corner.
        let (device_btn, sidebar_btn, exit_kiosk_btn) = Self::build_floating_buttons(&overlay);
        // `--kiosk:only`: no exit path at all, not even hidden-but-wired —
        // see `ui::kiosk_only()`'s own doc comment. The button itself just
        // disappears from the group rather than leaving an empty gap.
        if kiosk_only {
            exit_kiosk_btn.set_visible(false);
        }

        // Tracks the panel's own right edge while open, snapping back to
        // the base (CSS-set) margin while closed — see sidebar_btn's own
        // field doc comment. Moves `top_left_group` (sidebar_btn +
        // exit_kiosk_btn together), not sidebar_btn alone, so the exit
        // button stays glued to it as one unit. `.kiosk-sidebar-btn`
        // deliberately doesn't set margin-start itself, so this always wins
        // without a CSS priority fight (see that class's own comment in
        // dark.css/system.css).
        let top_left_group = sidebar_btn.parent().and_downcast::<gtk::Box>()
            .expect("sidebar_btn's parent is the top_left_group Box built alongside it");
        sidebar_paned.connect_notify_local(Some("position"), clone!(
            #[weak] top_left_group,
            move |paned, _| {
                let pos = paned.position();
                if pos > 8 { top_left_group.set_margin_start(pos + 12); }
                else       { top_left_group.set_margin_start(20); }
            }
        ));

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
            sidebar_paned,
            panel_anim: RefCell::new(None),
            bound: RefCell::new(None),
            layout: Cell::new(initial_layout),
        });

        device_btn.connect_clicked(clone!(#[weak] popover, move |_| {
            if popover.is_visible() { popover.popdown(); } else { popover.popup(); }
        }));
        sidebar_btn.connect_clicked({
            let weak = Rc::downgrade(&this);
            move |_| {
                let Some(this) = weak.upgrade() else { return };
                this.toggle_sidebar();
            }
        });
        if !kiosk_only {
            exit_kiosk_btn.connect_clicked({
                let exit_kiosk = Rc::clone(&exit_kiosk);
                move |_| exit_kiosk()
            });
        }
        device_list.connect_device_selected({
            let weak = Rc::downgrade(&this);
            move |_, key| {
                let Some(this) = weak.upgrade() else { return };
                this.bind_device(Some(key));
                this.popover.popdown();
            }
        });

        // Keyboard: "K" exits kiosk mode (unless `--kiosk:only` — see
        // `ui::kiosk_only()`'s own doc comment, same reasoning as the exit
        // button above: no wired escape hatch at all, not just a hidden
        // one), "L" swaps between the Classic and WideRight playback
        // layouts (neither shared with DeviceWindow's own controller — no
        // "M" here at all, kiosk has no mini mode). "T" (theme cycle) *is*
        // shared verbatim with DeviceWindow's own controller, via
        // `ui::theme::cycle_theme()`. Everything else delegates to the
        // shared transport-key helper against whatever's currently bound.
        let key_ctrl = gtk::EventControllerKey::new();
        key_ctrl.set_propagation_phase(gtk::PropagationPhase::Capture);
        key_ctrl.connect_key_pressed({
            let weak = Rc::downgrade(&this);
            move |_, keyval, _keycode, state| {
                let Some(this) = weak.upgrade() else { return glib::Propagation::Proceed };
                if state.intersects(gtk::gdk::ModifierType::CONTROL_MASK | gtk::gdk::ModifierType::ALT_MASK) {
                    return glib::Propagation::Proceed;
                }
                if !kiosk_only {
                    if let gtk::gdk::Key::k | gtk::gdk::Key::K = keyval {
                        exit_kiosk();
                        return glib::Propagation::Stop;
                    }
                }
                if let gtk::gdk::Key::l | gtk::gdk::Key::L = keyval {
                    this.toggle_layout();
                    return glib::Propagation::Stop;
                }
                if let gtk::gdk::Key::t | gtk::gdk::Key::T = keyval {
                    crate::ui::cycle_theme();
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
    /// `PlaybackView` is currently showing), all added from this one place
    /// rather than scattered through `new()`. Returns them specifically
    /// since `new()` still needs both to wire up their click handling.
    fn build_floating_buttons(overlay: &gtk::Overlay) -> (gtk::Button, gtk::Button, gtk::Button) {
        let device_btn = gtk::Button::builder()
            .label("Select device")
            .css_classes(["kiosk-device-btn"])
            .halign(gtk::Align::End)
            .valign(gtk::Align::Start)
            .build();
        overlay.add_overlay(&device_btn);

        // Symmetric to device_btn on the opposite corner. No margin-start
        // set directly on either button here — that's the wrapping
        // `top_left_group` Box's job below (matching `.kiosk-sidebar-btn`'s
        // own comment on deliberately not setting it itself), so it stays
        // fully Rust-controlled (see sidebar_paned's "notify::position"
        // handler in new(), which moves the whole group live once the
        // panel's open) and both buttons move together as one unit.
        let sidebar_btn = gtk::Button::builder()
            .icon_name("sidebar-show-symbolic")
            .tooltip_text("Toggle presets panel")
            .css_classes(["kiosk-sidebar-btn"])
            .build();
        // Exits Kiosk mode, next to `sidebar_btn` rather than its own
        // separate floating corner, by request.
        let exit_kiosk_btn = gtk::Button::builder()
            .icon_name("rustywiim-exit-kiosk")
            .tooltip_text("Exit Kiosk mode")
            .css_classes(["kiosk-sidebar-btn"])
            .build();
        let top_left_group = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal).spacing(8)
            .halign(gtk::Align::Start).valign(gtk::Align::Start)
            .margin_start(20)
            .build();
        top_left_group.append(&sidebar_btn);
        top_left_group.append(&exit_kiosk_btn);
        overlay.add_overlay(&top_left_group);

        (device_btn, sidebar_btn, exit_kiosk_btn)
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
        // Available width for the playback pane specifically, not the
        // whole window's — sidebar_paned.position() is exactly the side
        // panel's current width (0 when closed), same fix as
        // DeviceWindowInner::toggle_layout()'s own version of this closure,
        // and for the same reason: feeding the *whole* window's width into
        // this view's own size_request would let it force a minimum wide
        // enough to block the panel from ever opening past a small width.
        let size_source: Rc<dyn Fn() -> Option<(i32, i32)>> = {
            let window = self.window.clone();
            let sidebar_paned = self.sidebar_paned.clone();
            Rc::new(move || {
                let (win_w, win_h) = (window.width(), window.height());
                if win_w <= 0 || win_h <= 0 { return None; }
                const HANDLE_W: i32 = 12;
                let avail_w = (win_w - sidebar_paned.position() - HANDLE_W).max(1);
                Some((avail_w, win_h))
            })
        };
        let view = PlaybackView::new(&ds, &self.icons, Some(&self.art_bg), layout, size_source);
        // Fills sidebar_paned's end slot (art_bg is the main overlay child
        // driving the window's own size, per new()'s comment) — still
        // needs its own explicit expansion to fill that slot.
        view.set_hexpand(true);
        view.set_vexpand(true);
        // Views start inactive (views/mod.rs's shared contract) — this is
        // what actually performs the initial render.
        view.set_active(true);
        self.sidebar_paned.set_end_child(Some(&view));

        // Side panel (presets/IO) — same widget shapes DeviceWindow's own
        // left_pane uses, rebuilt fresh each bind exactly like view/
        // status_bar are. "panel-card" is Modern-theme-only styling (see
        // modern.css), inert everywhere else.
        let presets = views::presets::PresetsView::new(&ds, &self.icons);
        let io = views::io::InputOutputView::new(&ds, &self.icons);
        presets.set_active(true);
        io.set_active(true);
        // margin_top matches sidebar_btn's own (see .kiosk-sidebar-btn) so
        // the panel's top edge lines up with the toggle button's, instead
        // of starting right at the very top of the window.
        let left_pane = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .css_classes(["panel-card"])
            .margin_top(20)
            .build();
        left_pane.append(&presets);
        left_pane.append(&io);
        self.sidebar_paned.set_start_child(Some(&left_pane));

        // sidebar_paned itself is persistent (see its own field doc
        // comment) — content_holder was just cleared by the caller, so it
        // needs re-adding, but its `position` (open/closed + width) is a
        // plain property on the same long-lived object, untouched by
        // being briefly unparented, so there's nothing to restore here.
        self.content_holder.append(&self.sidebar_paned);

        // Status bar (network/BLE-remote/device info), same as
        // DeviceWindow's own — always shown for this first cut, no
        // Settings toggle exists yet to make it optional. Deliberately no
        // separator line above it here (unlike DeviceWindow's own bottom
        // bar) — Kiosk mode's version looks better without one.
        let status_bar = crate::ui::views::status_bar::StatusBarView::new(&ds, &self.icons, true);
        // `set_scale()` uses the same `compute_wide_right_art_side()`
        // reference regardless of which layout is actually showing above
        // it — it's just this bar's own screen-size-to-font/icon-size
        // ratio, not something tied to WideRight's own artwork, so Classic
        // (which has no proportional scaling of its own yet) still wants
        // this bar itself sized correctly. `set_edge_margin()` stays
        // WideRight-only: it lines this bar's edges up with PlaybackView's
        // own margin, which is only a fraction-of-artwork-size value in
        // that layout — Classic's own margins are small/fixed already,
        // close enough to this bar's own defaults that no adjustment is
        // needed there.
        match size_hint {
            Some((w, h)) => {
                let side = compute_wide_right_art_side(w, h);
                if layout == PlaybackLayout::WideRight {
                    status_bar.set_edge_margin(wide_right_margin_h(side));
                }
                status_bar.set_scale(side);
            }
            // Same cold-start gap PlaybackView itself guards against (see
            // its own tick-callback fallback's comment): on a slower/
            // different compositor the window may not have reported a
            // real size yet at this exact point, so size_hint comes back
            // None here and — unlike PlaybackView, which keeps retrying on
            // its own — this call would otherwise just be skipped forever,
            // leaving the mismatch permanently uncorrected. Confirmed
            // live: hit every time on a Raspberry Pi 5, never on a desktop
            // fast enough to already have a real window size by this point.
            None => {
                let weak_bar = status_bar.downgrade();
                let window = self.window.clone();
                self.window.add_tick_callback(move |_, _| {
                    let Some(bar) = weak_bar.upgrade() else { return glib::ControlFlow::Break };
                    let (w, h) = (window.width(), window.height());
                    if w <= 0 || h <= 0 { return glib::ControlFlow::Continue; }
                    let side = compute_wide_right_art_side(w, h);
                    if layout == PlaybackLayout::WideRight {
                        bar.set_edge_margin(wide_right_margin_h(side));
                    }
                    bar.set_scale(side);
                    glib::ControlFlow::Break
                });
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
            _presets: presets,
            _io: io,
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

    /// The side-panel toggle button. Reading `sidebar_paned.position()` as
    /// the source of truth for "is it open" (rather than a separate bool)
    /// means a direct drag of the divider (still possible — nothing
    /// disables it) and a button click both leave the state in exactly
    /// one place. Icon stays fixed (`sidebar-show-symbolic`) regardless of
    /// open/closed state for now — `sidebar-hide-symbolic` isn't in every
    /// icon theme and rendered as a broken/missing-icon glyph when tried.
    fn toggle_sidebar(self: &Rc<Self>) {
        let open = self.sidebar_paned.position() > 8;
        let target = if open {
            0
        } else {
            // The real content's own natural width (PresetsView/
            // InputOutputView, whatever `left_pane` holds right now) plus
            // a margin — not a fixed guess. A static width clipped the
            // panel's content on some screen/content combinations; this
            // guarantees it's fully visible regardless of screen size.
            let natural = self.sidebar_paned.start_child()
                .map(|w| w.measure(gtk::Orientation::Horizontal, -1).1)
                .unwrap_or(SIDEBAR_OPEN_WIDTH);
            natural + SIDEBAR_OPEN_MARGIN
        };

        // Same animation DeviceWindow's own animate_panel_to() uses —
        // skip (not just drop) any still-running one first, since a
        // dropped TimedAnimation doesn't stop driving its callback target
        // on its own.
        if let Some(a) = self.panel_anim.borrow_mut().take() { a.skip(); }

        let from = self.sidebar_paned.position();
        let animate = from != target
            && config::with(|cfg| cfg.animations)
            && gtk::Settings::default().is_some_and(|s| s.is_gtk_enable_animations());
        if !animate {
            self.sidebar_paned.set_position(target);
            return;
        }

        let paned = self.sidebar_paned.clone();
        let anim_target = adw::CallbackAnimationTarget::new(move |v| {
            paned.set_position(v.round() as i32);
        });
        let anim = adw::TimedAnimation::new(&self.sidebar_paned, from as f64, target as f64, 200, anim_target);
        anim.set_easing(adw::Easing::EaseInOutCubic);
        let weak = Rc::downgrade(self);
        anim.connect_done(move |_| {
            let Some(this) = weak.upgrade() else { return };
            *this.panel_anim.borrow_mut() = None;
        });
        anim.play();
        *self.panel_anim.borrow_mut() = Some(anim);
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

