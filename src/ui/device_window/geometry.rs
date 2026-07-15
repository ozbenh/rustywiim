//! Window-mode and geometry handling for `DeviceWindowInner`: the
//! full/mini panel switch on the one shared window (chrome swap, sizing,
//! the maximize interplay), and per-device window-state persistence.
//! The bookkeeping here is deliberately verbose in comments — most of it
//! exists because of live-confirmed compositor behaviors; see also
//! `chrome::wire_mini_resize()`'s doc comment for the same spirit.

use std::rc::Rc;

use adw::prelude::*;

use crate::config;
use super::*;

impl DeviceWindowInner {
    /// Apply per-device window/panel state (size, maximized, panel
    /// visibility/width, mini-window width) for the device identified by
    /// `uuid`. Guarded by `window_state_loaded`/`applied_window_key` so
    /// repeated device-changed fires for the same device don't override the
    /// user's manual resizes. Deliberately *not* just `applied_window_key ==
    /// uuid`: that field is pre-seeded at construction with the *expected*
    /// UUID (for `DeviceWindow::uuid()`'s dedup, before any live connection
    /// exists), so for an already-known device it already equals `uuid` on
    /// the very first real call here — checking it alone would mistake
    /// "this is the device we expected to connect to" for "we already
    /// loaded this device's window state," and silently skip loading the
    /// saved window size/panel state at launch.
    ///
    /// Also (re-)establishes `playback_access_override`/`mute_access_override`
    /// for the resolved `uuid` — normally already done at connection time
    /// (`DeviceManager::get()` passes them straight to
    /// `DeviceState::set_device()`), but a manually-connected device
    /// (`--connect`, or any freshly-added device the same way) has no real
    /// uuid yet at that point (`getStatusEx` hasn't answered), so
    /// `DeviceManager::get()` is called with an empty uuid and looks up
    /// nothing. This is the first point after construction where the real
    /// uuid is known, so it's also the first point config for that uuid can
    /// actually be found — this call is what makes a saved access-method
    /// override for a manually-connected device take effect at all, rather
    /// than silently staying on the HTTP default forever. A no-op re-push
    /// for an already-known device (same uuid, same config, already applied
    /// at construction).
    pub(super) fn apply_device_window_state(&self, uuid: &str) {
        if uuid.is_empty() { return; }
        let already_loaded = self.window_state_loaded.get();
        let prev_uuid = self.applied_window_key.borrow().clone();
        if already_loaded && prev_uuid == uuid { return; }
        self.window_state_loaded.set(true);

        // Save the previous device's window state before overwriting the layout.
        // We use prev_uuid directly rather than ds.device_info() because by the
        // time this is called from apply_device_info, device_info() already points
        // to the new device. Only if a device's state was actually loaded before
        // (not just pre-seeded at construction) — otherwise there's nothing real
        // to save yet.
        if already_loaded && !prev_uuid.is_empty() {
            // Same in_mini-aware read `save_config_now()` uses just below —
            // needed here too now that `self.window` is the *one* shared
            // window (showing whichever content is currently active)
            // rather than a dedicated always-full-size window that simply
            // sat hidden while mini content showed elsewhere.
            let in_mini = *self.mini_mode.borrow();
            let maximized = if in_mini { self.full_mode_maximized.get() } else { self.window.is_maximized() };
            let (w, h) = if in_mini {
                *self.full_mode_size.borrow()
            } else {
                (self.window.width(), self.window.height())
            };
            config::update(|cfg| {
                let dev = cfg.device_mut(&prev_uuid);
                dev.window_maximized = maximized;
                dev.window_width     = if maximized { 0 } else { w };
                dev.window_height    = if maximized { 0 } else { h };
                dev.panel_visible    = self.sidebar_btn.is_active();
                dev.paned_position   = *self.saved_panel_width.borrow();
                dev.mini_mode        = in_mini;
                // Only overwrite if the mini panel has actually been shown
                // this session (`mini_mode_width` starts at 0 same as a
                // never-realized window's width() used to) — otherwise this
                // would clobber a previously saved good value with 0 every
                // time a session never happens to enter mini mode.
                let mw = if in_mini { self.window.width() } else { self.mini_mode_width.get() };
                if mw > 0 { dev.mini_window_width = mw; }
            });
        }

        *self.applied_window_key.borrow_mut() = uuid.to_string();

        let dev_cfg = config::with(|cfg| cfg.device(uuid));
        crate::ui::dbg_ui(&format!(
            "apply_device_window_state: uuid={uuid:?} playback_access_override={:?} mute_access_override={:?}",
            dev_cfg.playback_access_override, dev_cfg.mute_access_override,
        ));
        self.ds.set_playback_access_override(dev_cfg.playback_access_override);
        self.ds.set_mute_access_override(dev_cfg.mute_access_override);

        let panel_width = if dev_cfg.paned_position > 0 { dev_cfg.paned_position } else { 200 };
        *self.saved_panel_width.borrow_mut() = panel_width;

        // Guard with panel_collapsing to avoid triggering the sidebar toggle handler.
        *self.panel_collapsing.borrow_mut() = true;
        if dev_cfg.panel_visible {
            self.left_pane.set_visible(true);
            self.paned.set_position(panel_width);
            self.sidebar_btn.set_active(true);
        } else {
            self.left_pane.set_visible(false);
            self.sidebar_btn.set_active(false);
        }
        *self.panel_collapsing.borrow_mut() = false;

        if *self.mini_mode.borrow() {
            // Mini content is showing right now — `self.window` isn't the
            // full-panel window at this moment, so record what to restore on
            // exit_mini_mode() instead of touching it directly.
            *self.full_mode_size.borrow_mut() = (dev_cfg.window_width, dev_cfg.window_height);
            self.full_mode_maximized.set(dev_cfg.window_maximized);
        } else if dev_cfg.window_maximized {
            self.window.maximize();
        } else {
            // set_default_size must come before unmaximize so the compositor
            // uses the stored size when restoring from maximized state.
            if dev_cfg.window_width > 0 && dev_cfg.window_height > 0 {
                self.window.set_default_size(dev_cfg.window_width, dev_cfg.window_height);
            }
            self.window.unmaximize();
        }

        if dev_cfg.mini_window_width > 0 {
            self.mini_mode_width.set(dev_cfg.mini_window_width);
            if *self.mini_mode.borrow() {
                self.window.set_default_width(dev_cfg.mini_window_width);
            }
        }
    }

    /// Immediately persist the current device's window/panel state.
    /// Loads the full config, updates only the current device's entry, and
    /// saves so no other device's entry is overwritten.
    pub(super) fn save_config_now(&self) {
        let uuid = match self.ds.device_info() {
            Some(di) if !di.uuid.is_empty() => di.uuid,
            _ => return,
        };
        // In mini mode, use the remembered full-panel size/maximized state
        // rather than reading them off the window, which is showing mini
        // content right now, not the full panel.
        let in_mini = *self.mini_mode.borrow();
        let maximized = if in_mini { self.full_mode_maximized.get() } else { self.window.is_maximized() };
        let (w, h) = if in_mini {
            *self.full_mode_size.borrow()
        } else {
            (self.window.width(), self.window.height())
        };
        config::update(|cfg| {
            cfg.last_uuid = uuid.clone();
            // Update only the window-related fields; preserve pinned / window_open / etc.
            let dev = cfg.device_mut(&uuid);
            dev.window_maximized = maximized;
            dev.window_width     = if maximized { 0 } else { w };
            dev.window_height    = if maximized { 0 } else { h };
            dev.panel_visible    = self.sidebar_btn.is_active();
            dev.paned_position   = *self.saved_panel_width.borrow();
            dev.mini_mode        = in_mini;
            // See the matching guard in apply_device_window_state(): only
            // overwrite once the mini panel has actually been shown this
            // session (in which case reading it live off the window, which
            // is currently showing it, is accurate — mid-drag too).
            let mw = if in_mini { self.window.width() } else { self.mini_mode_width.get() };
            if mw > 0 { dev.mini_window_width = mw; }
        });
    }

    /// Swap `window`'s content/chrome between the full and mini looks —
    /// shared by `enter_mini_mode()`/`exit_mini_mode()` and the mini-mode
    /// startup restore in `new_inner()` (which needs the same swap applied
    /// before the window is ever presented, without going through a live
    /// "transition" that would misread the window's not-yet-realized size).
    ///
    /// Toggling `decorated`/swapping which content is packed at runtime is a
    /// technique with no prior art elsewhere in this codebase, unlike the
    /// resize-on-switch technique used just below (`set_default_size()`,
    /// already proven live by `chrome::wire_mini_resize()`'s drag-resize).
    ///
    /// Sizing is the caller's job, not this function's — `enter_mini_mode()`/
    /// `exit_mini_mode()`/the startup restore each call `set_default_size()`
    /// themselves right after, since they disagree on what size to apply
    /// (mini's remembered width vs. the full window's saved/pre-mini size).
    pub(super) fn apply_window_chrome(&self, mini: bool) {
        if mini {
            self.window.remove_css_class("player-window");
            self.window.add_css_class("mini-window");
            self.window.set_content(Some(&self.mini.root));
            self.window.set_decorated(false);
            self.window.set_resizable(false);
            // AdwWindow/AdwApplicationWindow hardcodes a 360x200 minimum via
            // `gtk_widget_set_size_request(self, 360, 200)` in its own
            // constructor (adw-window.c) — NOT a CSS rule, so no stylesheet
            // override touches it. On a `resizable(false)` window GTK sizes to
            // max(content, size_request), so that floor pins the mini window at
            // 200px tall even though the mini content only wants ~118px, packing
            // it into the top half (the "twice as tall" bug seen on the newer
            // libadwaita in GTK 4.22 / Fedora 44 — the 4.14 AdwWindow had no such
            // request). Clear it while mini content is shown; restored below when
            // returning to full mode. This works because it's the *shared* window
            // now — the old design's mini panel was a plain gtk::ApplicationWindow,
            // which never had this request in the first place.
            self.window.set_size_request(-1, -1);
        } else {
            self.window.remove_css_class("mini-window");
            self.window.remove_css_class("mini-window-modern");
            self.window.add_css_class("player-window");
            self.window.set_content(Some(&self.full_content));
            self.window.set_decorated(true);
            self.window.set_resizable(true);
            // Restore AdwWindow's own default minimum, cleared for mini mode
            // above — see that comment.
            self.window.set_size_request(360, 200);
        }
        // Re-derives ArtBackground visibility (+ mini-window-modern +
        // ScrollFadeLabel drop-shadow) for whichever content subtree is now
        // actually attached to the window — the walk in
        // `update_art_background_visibility()` only reaches attached
        // widgets, so the *other* subtree (now detached) simply keeps
        // whatever it last had; harmless while it's not shown, and this
        // same call self-heals it the next time it's reattached.
        crate::ui::update_art_background_visibility();
    }

    /// The mini window's target size for `set_default_size()`: the requested
    /// width plus the mini content's *measured* natural height at that width.
    ///
    /// Passes a concrete measured height rather than `-1`. On an already-built
    /// window `-1` doesn't mean "shrink to content" — it means "leave the
    /// height default unchanged", so the window would keep the full-mode
    /// height (~640) it was constructed with unless something re-negotiates it
    /// down. Requesting the measured natural height leaves nothing ambiguous
    /// and behaves the same across GTK versions. (Note: this is complementary
    /// to, not the cure for, the "twice as tall" bug — that was AdwWindow's
    /// hardcoded 360x200 `size_request` floor, cleared in `apply_window_chrome()`.)
    pub(super) fn mini_target_size(&self, mini_w: i32) -> (i32, i32) {
        let (_, nat_h, _, _) = self.mini.root.measure(gtk::Orientation::Vertical, mini_w);
        (mini_w, nat_h.max(1))
    }

    /// One-line window-geometry snapshot for the `--debug=ui` diagnostics
    /// sprinkled through `enter_mini_mode()`/`exit_mini_mode()` — kept as a
    /// helper so every call site logs the same fields in the same format.
    /// Always includes what `full_mode_size`/`mini_mode_width` currently
    /// hold (`saved[full:W,H mini:W]`) right alongside the window's actual
    /// live geometry, specifically so the two are easy to eyeball against
    /// each other in a `--debug=ui` log — that comparison (does "actual"
    /// match "saved" yet?) is exactly what tracking down the maximize/mini
    /// resize bugs needed.
    fn window_geom(&self) -> String {
        let (fw, fh) = *self.full_mode_size.borrow();
        let mw = self.mini_mode_width.get();
        format!(
            "is_maximized={} width={} height={} default_size={:?} saved[full:{fw},{fh} mini:{mw}]",
            self.window.is_maximized(), self.window.width(), self.window.height(),
            self.window.default_size(),
        )
    }

    /// Resolve the mini window's width (the per-device saved resize width, or
    /// `MINI_WIDTH_DEFAULT` if never resized) and request it plus the mini
    /// content's measured natural height via `set_default_size()`. Shared by
    /// the live `enter_mini_mode()` transition and the start-in-mini restore
    /// in `new_inner()` (mod.rs) so the two can't drift on how the mini window
    /// is sized — this is the logic the "twice as tall" investigation churned
    /// through, kept in one place deliberately. Caller handles chrome swap,
    /// bookkeeping, and present().
    pub(super) fn apply_mini_window_size(&self) {
        let mini_w = if self.mini_mode_width.get() > 0 {
            self.mini_mode_width.get()
        } else {
            super::chrome::MINI_WIDTH_DEFAULT
        };
        let (mini_w, mini_h) = self.mini_target_size(mini_w);
        crate::ui::dbg_ui(&format!("apply mini window size: requesting set_default_size({mini_w}, {mini_h})"));
        self.window.set_default_size(mini_w, mini_h);
    }

    pub(super) fn enter_mini_mode(&self) {
        if *self.mini_mode.borrow() { return; }
        crate::ui::dbg_ui(&format!(
            "enter mini mode (uuid={}) before: {}",
            self.applied_window_key.borrow(), self.window_geom(),
        ));
        let was_maximized = self.window.is_maximized();
        // Remember the full panel's current size/maximized state — about to
        // be overwritten below — so exit_mini_mode() can put it back later.
        // See the doc comment on this field group in mod.rs for the full
        // picture of why this bookkeeping exists at all now.
        //
        // Only capture width()/height() while *not* currently maximized —
        // while maximized they report the full-screen size, not the
        // windowed size to actually restore to, and full_mode_size already
        // holds that correctly from whenever the window last *was*
        // windowed (construction, a config restore, or an earlier
        // non-maximized enter_mini_mode() call) — no reason to clobber a
        // good value with a useless one.
        if !was_maximized {
            let captured = (self.window.width(), self.window.height());
            *self.full_mode_size.borrow_mut() = captured;
            crate::ui::dbg_ui(&format!(
                "enter mini mode: window not maximized -> storing current size into full_mode_size: full:{},{}",
                captured.0, captured.1,
            ));
        } else {
            let (fw, fh) = *self.full_mode_size.borrow();
            crate::ui::dbg_ui(&format!(
                "enter mini mode: window already maximized -> NOT touching full_mode_size (keeping full:{fw},{fh})",
            ));
        }
        self.full_mode_maximized.set(was_maximized);
        *self.mini_mode.borrow_mut() = true;
        // Activation runs the incoming view's own full catch-up refresh
        // (live or offline); the outgoing one stops reacting to signals
        // while hidden.
        self.playback.set_active(false);
        self.mini.view.set_active(true);

        // Mini mode is never maximized (resizable(false) below relies on
        // it, same reasoning `chrome::build_mini_window()`'s doc comment
        // gives for why an always-resizable undecorated window risks
        // GNOME's edge-tiling/snap-to-maximize gesture) — un-maximize
        // first, now that whether it *was* maximized is safely remembered
        // above for exit_mini_mode() to restore.
        self.window.unmaximize();
        crate::ui::dbg_ui(&format!("enter mini mode: after unmaximize(): {}", self.window_geom()));
        self.apply_window_chrome(true);
        // `apply_mini_window_size()` also overwrites GTK/the compositor's own
        // notion of "the size to restore to when un-maximized" — see
        // exit_mini_mode()'s matching set_default_size() call, which resets it
        // back before maximize()/unmaximize() runs there to undo that side effect.
        self.apply_mini_window_size();
        crate::ui::dbg_ui(&format!("enter mini mode: after set_default_size(): {}", self.window_geom()));
        self.window.present();
    }

    pub(super) fn exit_mini_mode(&self) {
        if !*self.mini_mode.borrow() { return; }
        crate::ui::dbg_ui(&format!(
            "exit mini mode (uuid={}) before: {}",
            self.applied_window_key.borrow(), self.window_geom(),
        ));
        // Capture the mini panel's final width (post any drag-resize) before
        // swapping away from it — see `mini_mode_width`'s doc comment.
        let captured_mini_w = self.window.width();
        self.mini_mode_width.set(captured_mini_w);
        crate::ui::dbg_ui(&format!(
            "exit mini mode: storing current width into mini_mode_width: mini:{captured_mini_w}",
        ));
        *self.mini_mode.borrow_mut() = false;
        self.mini.view.set_active(false);

        self.apply_window_chrome(false);
        crate::ui::dbg_ui(&format!("exit mini mode: after apply_window_chrome(false): {}", self.window_geom()));
        // Request the full panel's remembered size in both branches below —
        // for the non-maximized branch this is the actual restored size;
        // for the maximized branch it's only a best-effort (see the big
        // comment right below for why the real guarantee lives elsewhere).
        let (w, h) = *self.full_mode_size.borrow();
        // Ensure we have a sane size
        let (w, h) = if w > 0 && h > 0 { (w, h) } else { (680, 640) };
        crate::ui::dbg_ui(&format!(
            "exit mini mode: restoring full_mode_size (full:{w},{h}), full_mode_maximized={}",
            self.full_mode_maximized.get(),
        ));
        if self.full_mode_maximized.get() {
            // Tells the window's own notify::maximized handler (mod.rs) not
            // to treat the resulting transition as a fresh, genuine maximize
            // worth (re-)capturing into full_mode_size — see that handler's
            // comment and this flag's own doc comment.
            self.maximize_call_pending.set(true);
            // maximize() immediately, no waiting — a maximized window fills
            // the screen regardless of whatever Mutter's own internal
            // "restore to" snapshot ends up being, so this is always
            // visually correct right away: one clean mini-to-maximized
            // zoom, no intermediate windowed-size frame to glitch on.
            //
            // set_default_size() here is a harmless best-effort, not the
            // actual guarantee of correctness — confirmed live, twice now,
            // that Mutter doesn't reliably treat it as authoritative for
            // "what to restore to on a later un-maximize" (it snapshots the
            // window's *actual* surface size at the moment maximize() is
            // processed instead, which is still the mini panel's size here
            // no matter what we've just requested). The real guarantee is
            // deferred to the moment it's actually needed: the window's own
            // notify::maximized handler (mod.rs) corrects the size, off the
            // main loop's idle queue rather than synchronously, the next
            // time this window genuinely becomes un-maximized (a real user
            // action, not this call) — see that handler's own comment for
            // why synchronous-in-the-handler wasn't enough on its own, and
            // why it's still unverified whether deferring is.
            self.window.maximize();
            self.window.set_default_size(w, h);
            crate::ui::dbg_ui(&format!("exit mini mode: after maximize() + set_default_size({w}, {h}): {}", self.window_geom()));
        } else {
            self.window.set_default_size(w, h);
            self.window.unmaximize();
            crate::ui::dbg_ui(&format!("exit mini mode: after set_default_size({w}, {h}) + unmaximize(): {}", self.window_geom()));
        }
        self.window.present();
        // Activation runs the incoming view's own full catch-up refresh
        // (live or offline).
        self.playback.set_active(true);
    }
} // impl DeviceWindowInner

/// Schedule a deferred config save for `inner`, debounced at 500 ms.
/// Cancels any previously scheduled save so only one write happens per burst.
pub(super) fn schedule_config_save(i: &Rc<DeviceWindowInner>) {
    if let Some(id) = i.config_save_timer.borrow_mut().take() { id.remove(); }
    let i2 = Rc::clone(i);
    let id = glib::timeout_add_local_once(
        std::time::Duration::from_millis(500),
        move || {
            *i2.config_save_timer.borrow_mut() = None;
            i2.save_config_now();
        },
    );
    *i.config_save_timer.borrow_mut() = Some(id);
}
