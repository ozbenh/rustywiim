//! The per-device window: `DeviceWindow`/`DeviceWindowInner` and its
//! chrome, display, and geometry code. The playback/preset/input-output
//! content it hosts lives in `ui/views/` — this module is the *hosting*
//! side: window construction and lifecycle, full/mini mode switching,
//! geometry bookkeeping and persistence, and the window-level chrome
//! (header, bottom bar, mini top bar/resize).

mod chrome;
mod display;
mod geometry;

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::sync::atomic::Ordering;

use adw::prelude::*;
use glib::clone;
use gtk::gio;
use gtk::{Box as GtkBox, Orientation};

use crate::config;
use crate::device::manager::DeviceManager;
use crate::device::state::{DeviceState, FullModeGuard};
use crate::ui::{art_background, dbg_ui, icons, theme::update_art_background_visibility, views, wire_window_actions, DeviceSpec, ENTERING_KIOSK, QUITTING};

use chrome::*;

// ── DeviceWindowInner ─────────────────────────────────────────────────────────
// All "content" widget state for one device window, kept together so that every
// GTK signal closure only needs one `Rc::clone(&inner)` capture instead of
// capturing half a dozen independent `Rc<RefCell<...>>` values.

struct DeviceWindowInner {
    ds:               DeviceState,
    /// Acquired once, right after `ds` is obtained, for the lifetime of
    /// this window (main + mini share the same `ds`, so one guard covers
    /// both surfaces) — releases automatically when this struct drops
    /// (window close/last-ref-drop), reverting `ds` to `Simple` mode.
    /// `device::discovery_manager`'s own tracked devices never acquire
    /// `Full` themselves (they only need `Simple`'s liveness+identity
    /// polling), so a window closing is the only thing that ever drops
    /// this back down. See `DeviceState::acquire_full()`.
    _full_mode:       FullModeGuard,
    show_devices_fn:  Rc<dyn Fn()>,
    /// Enters Kiosk mode bound to this window's own device — see
    /// `win.kiosk`'s registration in `wire_window_lifecycle()`.
    enter_kiosk_fn:   Rc<dyn Fn()>,
    io:             views::io::InputOutputView,
    playback:       views::playback_full::PlaybackView,
    presets:        views::presets::PresetsView,
    status_bar:     views::status_bar::StatusBarView,
    /// Shown (spinning) only while `ConnectionState::Connecting` — see
    /// `reset_device_ui()`. Overlaid on the header bar, not packed into
    /// it — see `build_header()`'s doc comment for why.
    connecting_spinner: gtk::Spinner,
    /// When `connecting_spinner` was last shown — `None` while hidden.
    /// Lets `hide_connecting_spinner()` enforce `MIN_SPINNER_DISPLAY`
    /// (see that constant) instead of hiding it again so fast (a
    /// same-LAN reconnect can resolve in well under 100ms) that it never
    /// renders a single visible frame — exactly as unreadable/glitchy as
    /// the text flash it replaced.
    spinner_shown_at: std::cell::Cell<Option<std::time::Instant>>,
    /// Pending deferred hide from `hide_connecting_spinner()`, if any —
    /// cancelled in `cleanup()` like `settle_timer`/`config_save_timer`.
    spinner_hide_timer: RefCell<Option<glib::SourceId>>,
    // Window / panel state — kept here so device-change and close handlers
    // only need one Rc<Inner> capture.
    window:              adw::ApplicationWindow,
    /// The full window's content widget (art background + toolbar/header +
    /// paned + bottom bar) — kept as its own handle so
    /// `DeviceWindowInner::apply_window_chrome()` can swap `window`'s
    /// content back to this when leaving mini mode. `window`'s content is
    /// exactly one of this or `mini.root` at any given time; there is only
    /// ever one top-level GTK window per device now, not two.
    full_content:        gtk::Overlay,
    paned:               gtk::Paned,
    left_pane:           gtk::Box,
    sidebar_btn:         gtk::ToggleButton,
    saved_panel_width:   Rc<RefCell<i32>>,
    panel_collapsing:    Rc<RefCell<bool>>,
    /// In-flight sidebar-toggle slide animation, if any — cancelled/skipped
    /// on the next toggle so rapid clicks don't pile up overlapping animations.
    panel_anim:          RefCell<Option<adw::TimedAnimation>>,
    settle_timer:        Rc<RefCell<Option<glib::SourceId>>>,
    /// Deferred config-save timer: cancelled and rescheduled on every
    /// state change so only one disk write happens after a burst of events.
    config_save_timer:   Rc<RefCell<Option<glib::SourceId>>>,
    /// UUID this window is/was for — pre-seeded at construction with the
    /// *expected* UUID (from `config`, before any live connection) so
    /// `DeviceWindow::uuid()` can dedup windows even before the device has
    /// answered its first API call; updated to the live UUID once known.
    /// **Not** by itself a record of "has window state actually been
    /// applied yet" — see `window_state_loaded`, which exists precisely
    /// because for any already-known device this starts out equal to the
    /// live UUID it will later be compared against, so it can't answer
    /// that question on its own.
    applied_window_key: RefCell<String>,
    /// Whether `apply_device_window_state()`'s body has actually run yet
    /// for the current connection. Starts `false` regardless of what
    /// `applied_window_key` was pre-seeded with, specifically so the very
    /// first real call for an already-known device (where
    /// `applied_window_key` already equals the incoming UUID from
    /// construction) doesn't get mistaken for "already applied" and
    /// silently skip loading the saved window size/panel state/
    /// `playback_access_override` — a real bug this field fixes.
    window_state_loaded: Cell<bool>,
    /// Guards `cleanup()` so its body only actually runs once, even though
    /// it's invoked from both `close-request` and `connect_destroy` for a
    /// single user-initiated close. Nothing in `cleanup()` benefits from
    /// running twice, and computing "is this the last visible window"
    /// specifically needs to *not* re-run on the second call, since by then
    /// the window may be torn down enough for that to (wrongly) come out
    /// differently — a single guard on the whole function is simpler than
    /// caching that one value.
    cleaned_up: Cell<bool>,
    /// Last known friendly name — the window title's fallback while
    /// `device_info()` is `None` (still `Connecting`, or `Failed`/
    /// "Disconnected"): otherwise there'd be nothing to show but the
    /// generic "RustyWiiM". Seeded at construction from
    /// `config::DeviceConfig::name` (that field's own doc comment promises
    /// exactly this, "displayed while connecting / offline" — it just was
    /// never actually wired up to the window title before), then kept
    /// fresh by `apply_device_info()` every time the device actually
    /// answers — so a *later* disconnect falls back to the most recently
    /// confirmed live name, not a stale config-time one (e.g. the device
    /// having since been renamed in the WiiM app). Empty for a brand-new
    /// device config has never seen before and hasn't connected yet.
    cached_name: RefCell<String>,
    // ── Mini player ───────────────────────────────────────────────────────────
    mini:              MiniWidgets,
    /// Which of the two panels ("full" or "mini") the one shared `window` is
    /// currently showing. Not to be confused with GNOME/the desktop's own
    /// notion of a "maximized" window — that's an orthogonal, OS-level state
    /// a window can be in *while* showing our full panel; see the
    /// `full_mode_size`/`full_mode_maximized`/`mini_mode_width` group below
    /// for how the two interact.
    mini_mode:         RefCell<bool>,

    // Remembered geometry for whichever panel *isn't* currently showing, so
    // switching back to it restores the right size instead of leaving the
    // window at whatever size the other panel happened to need. Both
    // directions are needed because, unlike the old design (a genuinely
    // separate GTK window per panel, where the hidden one just kept
    // whatever size/maximized-state it already had — free, no bookkeeping
    // needed), the two panels now share one real window: switching panels
    // actually resizes it, so the size the panel you're leaving had is
    // about to be overwritten and has to be saved *first*.
    //
    // "Full mode" here means our own full-panel/mini-panel distinction
    // (`mini_mode` above) — a completely separate thing from the desktop's
    // own "maximized" window state, which is *also* only meaningful while
    // showing the full panel (mini mode is never maximized — see
    // `apply_window_chrome()`'s `resizable(false)` comment for why a
    // maximized mini panel isn't something we want the desktop offering in
    // the first place). A window can be "in full mode, maximized",
    // "in full mode, not maximized", or "in mini mode" — maximized mini
    // mode is simply not a state that exists.
    /// The full panel's windowed (non-maximized) size to restore on
    /// `exit_mini_mode()` — captured by `enter_mini_mode()` right before it
    /// shrinks the window down for mini content, but only while the window
    /// isn't currently maximized (while maximized, `width()`/`height()`
    /// report the full-screen size, not a real windowed size worth
    /// remembering — see `enter_mini_mode()`'s own comment). `exit_mini_mode()`
    /// applies this via `set_default_size()` unconditionally, even when also
    /// about to re-maximize — see that function's comment for why: it's not
    /// just the *visible* restored size, it's also what GTK/the compositor
    /// falls back to if the user later un-maximizes by hand, which needs
    /// resetting away from whatever mini panel width `enter_mini_mode()`
    /// last requested.
    full_mode_size:      RefCell<(i32, i32)>,
    /// Whether the window was OS-maximized right before `enter_mini_mode()`
    /// last shrank it for mini content — restored by `exit_mini_mode()`
    /// (which calls `maximize()` instead of relying on `full_mode_size`
    /// alone). Needed because entering mini mode has to un-maximize the
    /// window first (a maximized window can't also be the small floating
    /// mini panel), which would otherwise lose that fact for good.
    full_mode_maximized: Cell<bool>,
    /// Set immediately before `exit_mini_mode()` calls `window.maximize()`
    /// to restore a remembered maximized state, and consumed (read-and-clear)
    /// by the window's own `notify::maximized` handler — see that handler's
    /// comment (`new_inner()`) for why: it also opportunistically captures
    /// `full_mode_size` on every *genuine* transition into maximized (not
    /// just at `enter_mini_mode()` time), and this flag is what tells it
    /// "this particular transition is our own restore, not a fresh
    /// external one — don't recapture, the window is only this size because
    /// it was mini a moment ago."
    maximize_call_pending: Cell<bool>,
    /// The mini panel's width to restore on `enter_mini_mode()`, kept
    /// up to date while the full panel is showing (there's no live mini
    /// content to read a current width off in that state). Updated from
    /// config on device-info-apply and captured fresh by `exit_mini_mode()`
    /// (reading `window.width()` while it's still showing mini content,
    /// which reflects any live drag-resize too). Falls back to
    /// `widgets::MINI_WIDTH_DEFAULT` wherever it's read and still `0`
    /// (never set this session, nothing saved in config either).
    mini_mode_width:     Cell<i32>,

    mini_btn:          gtk::Button,
}

impl Drop for DeviceWindowInner {
    fn drop(&mut self) {
        dbg_ui(&format!(
            "DeviceWindowInner dropped (uuid={})",
            self.applied_window_key.borrow()
        ));
    }
}

impl DeviceWindowInner {
    fn cleanup(&self) {
        // Fires from both close-request and connect_destroy for a single
        // user-initiated close; nothing below benefits from running twice,
        // and computing "is this the last visible window" specifically
        // needs to run exactly once, while self.window is still guaranteed
        // fully alive/registered (true on the first call, not guaranteed by
        // the second) — so just don't let there be a second call.
        if self.cleaned_up.replace(true) { return; }

        let uuid = self.ds.device_info()
            .map(|di| di.uuid)
            .filter(|u| !u.is_empty())
            .unwrap_or_else(|| self.applied_window_key.borrow().clone());
        let settle_pending  = self.settle_timer.borrow().is_some();
        let cfgsave_pending = self.config_save_timer.borrow().is_some();
        dbg_ui(&format!(
            "DeviceWindow cleanup uuid={uuid} settle_pending={settle_pending} cfgsave_pending={cfgsave_pending}"
        ));
        if let Some(id) = self.settle_timer.borrow_mut().take() { id.remove(); }
        if let Some(id) = self.config_save_timer.borrow_mut().take() { id.remove(); }
        if let Some(id) = self.spinner_hide_timer.borrow_mut().take() { id.remove(); }
        self.save_config_now();

        // Skip saving the "window_open" flag to false if that was the last
        // window, since that will be "quit" operation, it's more intuitive
        // to come back to the same state on re-launch.
        //
        // Window geometry is still saved above either way — only the
        // window_open flag itself is skipped, in two cases: an explicit
        // app quit (QUITTING), and closing what turns out to be the last
        // visible window.
        //
        // Settings windows never register with the GtkApplication, so they
        // don't count as "another visible window" here — closing your last
        // device window while a Settings window happens to be open still
        // preserves it. There's only ever one top-level GTK window per
        // device now (full and mini are two contents of the same window,
        // not two windows), so unlike an older version of this check, no
        // second window needs excluding here.
        let last_window = self.window.application().is_some_and(|app| {
            !app.windows().iter().any(|w| {
                w.upcast_ref::<gtk::Widget>() != self.window.upcast_ref::<gtk::Widget>()
                    && w.is_visible()
            })
        });
        dbg_ui(&format!(
            "DeviceWindow cleanup uuid={uuid} last_window={last_window} quitting={} entering_kiosk={}",
            QUITTING.load(Ordering::Relaxed), ENTERING_KIOSK.load(Ordering::Relaxed)
        ));
        if !uuid.is_empty() && !QUITTING.load(Ordering::Relaxed)
            && !ENTERING_KIOSK.load(Ordering::Relaxed) && !last_window {
            dbg_ui(&format!("DeviceWindow cleanup uuid={uuid} persisting window_open=false"));
            config::update(|cfg| cfg.device_mut(&uuid).window_open = false);
        } else if !uuid.is_empty() {
            dbg_ui(&format!("DeviceWindow cleanup uuid={uuid} preserving window_open (last_window, quitting, or entering kiosk)"));
        }
    }
}

// ── DeviceWindow ──────────────────────────────────────────────────────────────

/// One device window.  Owns the GTK window and all content widgets.
#[derive(Clone)]
pub struct DeviceWindow {
    pub window: adw::ApplicationWindow,
    inner:      Rc<DeviceWindowInner>,
}

impl DeviceWindow {
    #[allow(dead_code)]
    pub fn ds(&self) -> &DeviceState { &self.inner.ds }

    /// UUID of the device this window was created for.  Returns the live UUID
    /// from the connected device if available, otherwise falls back to the UUID
    /// recorded at creation time (applied_window_key) so dedup works even before
    /// the device has responded to its first API call.
    pub fn uuid(&self) -> Option<String> {
        self.inner.ds.device_info()
            .map(|i| i.uuid)
            .filter(|u| !u.is_empty())
            .or_else(|| {
                let k = self.inner.applied_window_key.borrow().clone();
                if k.is_empty() { None } else { Some(k) }
            })
    }

    /// Build a device window connected to a specific device.
    pub fn new_for_device(
        app:            &adw::Application,
        device_manager: DeviceManager,
        show_devices_fn: Rc<dyn Fn()>,
        enter_kiosk_fn: Rc<dyn Fn()>,
        open_settings:  Rc<dyn Fn(Option<DeviceState>)>,
        spec:           DeviceSpec,
    ) -> Self {
        Self::new_inner(app, device_manager, show_devices_fn, enter_kiosk_fn, open_settings, Some(spec))
    }

    fn new_inner(
        app:             &adw::Application,
        device_manager:  DeviceManager,
        show_devices_fn: Rc<dyn Fn()>,
        enter_kiosk_fn:  Rc<dyn Fn()>,
        open_settings:   Rc<dyn Fn(Option<DeviceState>)>,
        device_spec:     Option<DeviceSpec>,
    ) -> Self {
        let icons = Rc::new(icons::IconSet::load());

        // Pick the device UUID to use for loading per-device window config.
        // The no-spec path no longer falls back to last_uuid (phased out).
        let cfg_uuid: String = device_spec.as_ref()
            .map(|s| s.uuid.clone())
            .filter(|u| !u.is_empty())
            .unwrap_or_default();

        let init_dev_cfg = config::with(|cfg| cfg.device(&cfg_uuid));

        let ds = match device_spec.as_ref() {
            Some(spec) => device_manager.get(
                &spec.uuid, &spec.ip, spec.tls_mode,
                init_dev_cfg.playback_access_override,
                init_dev_cfg.mute_access_override,
                init_dev_cfg.loop_mode_access_override,
                config::resolved_gena_enabled(&spec.uuid),
                spec.try_connect,
            ),
            None => {
                // No device spec: create a standalone state that isn't wired to
                // any device yet; polling still starts so the UI can be shown.
                let ds = DeviceState::new(device_manager.rt(), String::new());
                ds.start_polling();
                ds
            }
        };
        // A device window always wants Full mode — covers both branches
        // above (main + mini share this one `ds`, so one guard is enough
        // for both surfaces); released automatically when this window's
        // `DeviceWindowInner` drops. See `_full_mode`'s doc comment.
        let full_mode = ds.acquire_full();

        dbg_ui(&format!("DeviceWindow creating (uuid={})", cfg_uuid));

        let (header, sidebar_btn, mini_btn, connecting_spinner) = build_header(init_dev_cfg.panel_visible);
        let presets = views::presets::PresetsView::new(&ds, &icons);
        let io = views::io::InputOutputView::new(&ds, &icons);
        let left_pane = build_left_pane(&presets, &io);
        // Blurred-artwork background layer for the full window — built
        // before the playback view so the view can be handed a reference
        // (it feeds it artwork alongside its own FlipCover); attached to
        // the window overlay further down.
        let art_bg = art_background::ArtBackground::new();
        art_bg.set_hexpand(true);
        art_bg.set_vexpand(true);
        let playback = views::playback_full::PlaybackView::new(
            &ds, &icons, Some(&art_bg), views::playback_full::PlaybackLayout::Classic, None,
        );
        let (mini, _mini_root) = build_mini_window(&ds, &icons);

        // ── Paned split + sidebar logic ───────────────────────────────────────────
        let paned = gtk::Paned::new(Orientation::Horizontal);
        paned.set_start_child(Some(&left_pane));
        paned.set_end_child(Some(&playback));
        paned.set_shrink_start_child(true);
        paned.set_shrink_end_child(false);
        paned.set_resize_start_child(false);
        paned.set_resize_end_child(true);
        paned.set_margin_top(4);
        paned.set_margin_bottom(8);

        let panel_width = if init_dev_cfg.paned_position > 0 { init_dev_cfg.paned_position } else { 200 };
        paned.set_position(panel_width);
        left_pane.set_visible(init_dev_cfg.panel_visible);

        let saved_panel_width  = Rc::new(RefCell::new(panel_width));
        let panel_collapsing   = Rc::new(RefCell::new(false));
        let settle_timer:      Rc<RefCell<Option<glib::SourceId>>> = Rc::new(RefCell::new(None));
        let config_save_timer: Rc<RefCell<Option<glib::SourceId>>> = Rc::new(RefCell::new(None));

        let status_bar = views::status_bar::StatusBarView::new(&ds, &icons, false);

        let outer = GtkBox::new(Orientation::Vertical, 0);
        outer.append(&paned);
        outer.append(&gtk::Separator::new(Orientation::Horizontal));
        outer.append(&status_bar);

        let full_toolbar = adw::ToolbarView::new();
        full_toolbar.add_top_bar(&header);
        full_toolbar.set_content(Some(&outer));

        // `art_bg` (built above, before the playback view) sits behind the
        // toolbar. Always present; only visible when the active theme makes
        // the toolbar/window backgrounds transparent (RustyWiiM Modern —
        // see modern.css). Initial visibility is set once the window
        // exists, below — `update_art_background_visibility()`'s walk only
        // reaches whichever of this tree / `mini.root` is actually attached
        // as the window's content at the time it runs, so the mini tree's
        // own art_bg gets its first real pass from `apply_window_chrome()`
        // instead (called either by the mini-mode startup restore below, or
        // the first time `enter_mini_mode()` ever runs).
        let window_overlay = gtk::Overlay::new();
        window_overlay.set_child(Some(&art_bg));
        window_overlay.add_overlay(&full_toolbar);
        // Below the header row, not inside it — see `build_header()`'s doc
        // comment for why the header bar itself is the wrong place to
        // overlay this (collides with the native CSD window buttons).
        window_overlay.add_overlay(&connecting_spinner);

        let win_w = if init_dev_cfg.window_width  > 0 { init_dev_cfg.window_width  } else { 680 };
        let win_h = if init_dev_cfg.window_height > 0 { init_dev_cfg.window_height } else { 640 };
        let window = adw::ApplicationWindow::builder()
            .application(app).title("RustyWiiM").content(&window_overlay)
            .default_width(win_w).default_height(win_h)
            .build();
        window.add_css_class("player-window");
        if init_dev_cfg.window_maximized { window.maximize(); }

        // Diagnostic only (--debug=ui): logs every time GTK itself observes
        // a maximized/default-size change take effect, which can lag behind
        // (or simply not match) the synchronous call that requested it —
        // e.g. `set_default_size()` is a request, not a synchronous resize,
        // so there can be a gap between "we called it" and "GTK/the
        // compositor actually applied it." Added chasing a real bug: a
        // maximize → mini → back-to-full → un-maximize round trip restoring
        // to the mini panel's width instead of the full panel's saved size.
        window.connect_notify_local(Some("maximized"), |win, _| {
            dbg_ui(&format!(
                "window notify::maximized -> is_maximized={} width={} height={} default_size={:?}",
                win.is_maximized(), win.width(), win.height(), win.default_size(),
            ));
        });
        window.connect_notify_local(Some("default-width"), |win, _| {
            dbg_ui(&format!(
                "window notify::default-width -> default_size={:?} is_maximized={} width={} height={}",
                win.default_size(), win.is_maximized(), win.width(), win.height(),
            ));
        });
        window.connect_notify_local(Some("default-height"), |win, _| {
            dbg_ui(&format!(
                "window notify::default-height -> default_size={:?} is_maximized={} width={} height={}",
                win.default_size(), win.is_maximized(), win.width(), win.height(),
            ));
        });

        // apply_theme() only fires on explicit runtime switches, so a window
        // (main or mini, both now built) opened after the app already
        // started on some theme needs its initial art_bg visibility set
        // directly from the live config.
        update_art_background_visibility();

        let inner = Rc::new(DeviceWindowInner {
            ds: ds.clone(),
            _full_mode: full_mode,
            show_devices_fn,
            enter_kiosk_fn,
            io,
            playback,
            presets,
            status_bar,
            connecting_spinner,
            spinner_shown_at: std::cell::Cell::new(None),
            spinner_hide_timer: RefCell::new(None),
            window: window.clone(),
            full_content: window_overlay.clone(),
            paned:  paned.clone(),
            left_pane: left_pane.clone(),
            sidebar_btn: sidebar_btn.clone(),
            saved_panel_width,
            panel_collapsing,
            panel_anim: RefCell::new(None),
            settle_timer,
            config_save_timer,
            applied_window_key: RefCell::new(cfg_uuid.clone()),
            window_state_loaded: Cell::new(false),
            cleaned_up: Cell::new(false),
            cached_name: RefCell::new(init_dev_cfg.name.clone().unwrap_or_default()),
            mini,
            // Seeded correctly from the start (not left `false` and fixed
            // up later) so the upcoming `populate_all()` call — which only
            // refreshes whichever of main/mini is actually active — targets
            // the right one immediately for a device restoring straight
            // into mini mode. See `init_dev_cfg.mini_mode` block below for
            // the rest of that restore (things that need `inner`/already-
            // built widgets to exist, so can't be folded into this literal).
            mini_mode:         RefCell::new(init_dev_cfg.mini_mode),
            // Seeded from the same config-restored (or default) width/height
            // the window itself was just constructed with (win_w/win_h,
            // above) — not (0, 0). Left at (0, 0), a device that gets
            // maximized before ever entering mini mode even once this
            // session would have no real size to fall back on:
            // enter_mini_mode() only captures a fresh value while *not*
            // currently maximized (maximized width()/height() would be the
            // screen resolution, not a real windowed size — see that
            // function's comment), so if it's *always* been maximized so
            // far, this field is the only source of truth there is.
            // Confirmed live via --debug=ui: exactly this scenario left
            // exit_mini_mode() holding (0, 0), which its own w>0&&h>0 guard
            // correctly refused to apply — but with nothing to fall back to
            // either, `default_size` was simply never corrected back from
            // whatever enter_mini_mode() had last set it to (the mini
            // panel's width), so a later manual un-maximize snapped to that
            // instead of the real windowed size.
            full_mode_size:      RefCell::new((win_w, win_h)),
            full_mode_maximized: Cell::new(init_dev_cfg.window_maximized),
            maximize_call_pending: Cell::new(false),
            mini_mode_width:     Cell::new(init_dev_cfg.mini_window_width),
            mini_btn:          mini_btn.clone(),
        });

        wire_device_signals(&inner);

        // No playback-changed/input-changed dispatch here anymore — the
        // playback views subscribe themselves. This closes out the old
        // central-dispatcher shape ("one handler decides which panel
        // updates"), whose hidden-panel staleness and manual catch-up
        // calls were a recurring bug source.

        // Populate the window-level UI (title, bottom bar, chrome) from
        // whatever the DeviceState already has cached.
        inner.populate_all();

        // The left-pane views stay permanently active — each
        // self-subscribes to the DeviceState signals it needs, and the old
        // window-driven update paths refreshed them regardless of which
        // panel was showing anyway. The two playback views are
        // mode-following: exactly one is active at a time
        // (enter/exit_mini_mode flip them; activation runs the incoming
        // view's own full catch-up refresh).
        inner.presets.set_active(true);
        inner.io.set_active(true);
        inner.status_bar.set_active(true);
        inner.playback.set_active(!*inner.mini_mode.borrow());
        inner.mini.view.set_active(*inner.mini_mode.borrow());

        geometry::wire_maximize_tracking(&inner);

        display::wire_sidebar(&inner);

        display::wire_keyboard(&inner);

        wire_mini_chrome(&inner);

        wire_window_lifecycle(&inner, open_settings);

        if init_dev_cfg.mini_mode {
            // `mini_mode` itself is already correctly seeded above (before
            // `populate_all()` ran), so this is just the rest of the
            // restore that needs `inner`/already-built widgets to exist —
            // not calling the full `enter_mini_mode()` (which would treat
            // this as a live transition, e.g. trying to read `window`'s
            // current size as the full-mode size to restore later). Swaps
            // the window straight to its mini chrome/content before it's
            // ever presented, so `DeviceWindow::present()` shows it already
            // looking right. mini_btn itself needs no update — it's a plain,
            // stateless button (see its own doc comment in widgets.rs).
            // Seed full_mode_size from saved config so exit_mini_mode() can
            // restore the right full-panel size even before the mini panel
            // has ever been shown (full_mode_maximized is already seeded
            // the same way, straight in the struct literal above).
            *inner.full_mode_size.borrow_mut() = (win_w, win_h);
            // Mini mode is never maximized — see the matching unmaximize() in
            // enter_mini_mode(). `window.maximize()` above already ran
            // unconditionally off `init_dev_cfg.window_maximized` before
            // this block knew whether mini_mode was also set; undo it here
            // for the same reason enter_mini_mode() insists on it live.
            inner.window.unmaximize();
            inner.apply_window_chrome(true);
            // Same width-resolve + measured-height sizing as the live
            // `enter_mini_mode()` transition, shared so the two can't drift.
            inner.apply_mini_window_size();
        }

        Self { window, inner }
    }

    pub fn present(&self) {
        self.window.present();
    }
}

/// Connect the `DeviceState` signals the *window itself* consumes —
/// `device-changed` (title/window state via `populate_all()`). `network-
/// changed`/`remote-changed` are `StatusBarView`'s own subscriptions now;
/// the other views connect their own too.
fn wire_device_signals(inner: &Rc<DeviceWindowInner>) {
    let ds = inner.ds.clone();
    // Use Rc::downgrade so the closure doesn't keep DeviceWindowInner alive
    // after the window is closed — a broken upgrade() call becomes a no-op.
    ds.connect_device_changed({
        let i = Rc::downgrade(&inner);
        move |ds| {
            let Some(i) = i.upgrade() else { return };
            dbg_ui(&format!(
                "device-changed signal: device_info_present={}",
                ds.device_info().is_some(),
            ));
            i.populate_all();
        }
    });
}

/// The mini top bar's buttons and the double-click-to-restore gesture —
/// chrome around `MiniPlaybackView`, wired here because "restore to full
/// window"/"close" act on the window, not the view.
fn wire_mini_chrome(inner: &Rc<DeviceWindowInner>) {
    let window = inner.window.clone();
    // ── Mini player signals ───────────────────────────────────────────────────
    // A plain click, not a toggle — this button only ever lives in the
    // full panel's header, so clicking it can only ever mean "switch to
    // mini mode" (going the other way is restore_btn/double-click/M
    // below, none of which are this button).
    inner.mini_btn.connect_clicked({
        let i = Rc::downgrade(&inner);
        move |_btn| {
            let Some(i) = i.upgrade() else { return };
            i.enter_mini_mode();
            geometry::schedule_config_save(&i);
        }
    });

    inner.mini.restore_btn.connect_clicked({
        let i = Rc::downgrade(&inner);
        move |_| {
            let Some(i) = i.upgrade() else { return };
            i.exit_mini_mode();
            geometry::schedule_config_save(&i);
        }
    });

    // The mini panel's own close (X) button — same "close this device"
    // meaning as win.close below, just with no native titlebar button to
    // trigger it from (mini mode is undecorated).
    inner.mini.close_btn.connect_clicked({
        clone!(#[strong] window, move |_| {
            gtk::prelude::WidgetExt::realize(&window); // close() is a no-op on an unrealized window
            window.close();
        })
    });

    {
        let gesture = gtk::GestureClick::builder().button(1).build();
        gesture.connect_pressed({
            let i = Rc::downgrade(&inner);
            move |_, n_press, _, _| {
                let Some(i) = i.upgrade() else { return };
                if n_press >= 2 {
                    i.exit_mini_mode();
                    geometry::schedule_config_save(&i);
                }
            }
        });
        inner.mini.root.add_controller(gesture);
    }
}

/// The window-scoped actions (win.close/win.devices/win.about/
/// win.settings) and the close-request/destroy lifecycle handlers that
/// funnel into `DeviceWindowInner::cleanup()`.
fn wire_window_lifecycle(
    inner:         &Rc<DeviceWindowInner>,
    open_settings: Rc<dyn Fn(Option<DeviceState>)>,
) {
    let window = inner.window.clone();
    let ds = inner.ds.clone();
    // ── Window actions ────────────────────────────────────────────────────────
    // win.close (Ctrl-W), win.devices, win.about, win.settings — one
    // registration each now, on the one shared window (previously
    // duplicated onto a separate mini_win too).
    let close_action = gio::SimpleAction::new("close", None);
    {
        let win_for_close = window.clone();
        close_action.connect_activate(move |_, _| {
            gtk::prelude::WidgetExt::realize(&win_for_close); // close() is a no-op on an unrealized window
            win_for_close.close();
        });
    }
    window.add_action(&close_action);

    let devices_action = gio::SimpleAction::new("devices", None);
    {
        let i = Rc::downgrade(&inner);
        devices_action.connect_activate(move |_, _| {
            if let Some(i) = i.upgrade() { (i.show_devices_fn)(); }
        });
    }
    window.add_action(&devices_action);

    let kiosk_action = gio::SimpleAction::new("kiosk", None);
    {
        let i = Rc::downgrade(&inner);
        kiosk_action.connect_activate(move |_, _| {
            if let Some(i) = i.upgrade() { (i.enter_kiosk_fn)(); }
        });
    }
    window.add_action(&kiosk_action);

    wire_window_actions(&window, Some(ds.clone()), open_settings);

    // close-request fires on any close attempt: the native titlebar X
    // button (full mode only — mini mode is undecorated), win.close()
    // (Ctrl-W / the mini panel's close_btn), Alt+F4, a compositor-level
    // close, or the quit action closing every window on the way out.
    // Always means "close this device", regardless of which panel is
    // currently showing — mini mode has its own dedicated "go back to
    // full mode without closing" affordances (the restore button,
    // double-click, the header mini-toggle, the M shortcut); close-request
    // itself isn't one of them, in any mode. cleanup() is idempotent so
    // calling it here AND in connect_destroy is safe.
    window.connect_close_request({
        let i = Rc::downgrade(&inner);
        move |_win| {
            dbg_ui("window close-request");
            if let Some(i) = i.upgrade() { i.cleanup(); }
            glib::Propagation::Proceed
        }
    });

    // destroy: single place for all cleanup.  Fires on every destruction path:
    //   • user close  (close-request → Proceed → GTK destroys → destroy)
    //   • win.destroy() from quit action (skips close-request, fires destroy directly)
    //   • app.quit()   (GTK destroys all windows during shutdown → destroy)
    // A second connect_destroy added later in open_device_spec clears the registry
    // (fires after this one, in connection order), which drops the last Rc<Inner> → Drop.
    window.connect_destroy({
        let i = Rc::downgrade(&inner);
        move |_win| {
            if let Some(i) = i.upgrade() {
                i.cleanup();
            } else {
                dbg_ui("main window destroyed (inner already freed)");
            }
        }
    });
}
