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

/// Floor for the side panel's *open* width — DeviceWindow-only (Kiosk
/// mode's own sidebar in `ui/kiosk.rs` computes its open width completely
/// independently, from a live natural-size measurement, and never reads
/// this or any other DeviceWindow state — see that module's own comments).
/// Previously there was no real floor beyond `wire_sidebar()`'s
/// `SNAP_PX = 30` open-vs-closed threshold, so a user could drag the panel
/// open to anything from 30px up — uncomfortably narrow well before
/// reaching a size any of `PresetsView`/`InputOutputView`'s real content
/// needs. Applied both when reading a saved/default width back
/// (construction, `apply_device_window_state()`) and when capturing a
/// freshly-dragged one (`wire_sidebar()`'s settle timer and button-release
/// handler), so an old too-small persisted value from before this floor
/// existed gets corrected the next time it's read, not just newly-dragged
/// ones going forward.
pub(super) const MIN_PANEL_WIDTH: i32 = 260;

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
    /// Also (re-)establishes `playback_access_override`/`mute_access_override`/
    /// `loop_mode_access_override` for the resolved `uuid` — normally already
    /// done at connection time
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
            config::update(|cfg| self.write_window_state(cfg.device_mut(&prev_uuid)));
        }

        *self.applied_window_key.borrow_mut() = uuid.to_string();

        let dev_cfg = config::with(|cfg| cfg.device(uuid));
        crate::ui::dbg_ui(&format!(
            "apply_device_window_state: uuid={uuid:?} playback_access_override={:?} mute_access_override={:?} loop_mode_access_override={:?}",
            dev_cfg.playback_access_override, dev_cfg.mute_access_override, dev_cfg.loop_mode_access_override,
        ));
        self.ds.set_playback_access_override(dev_cfg.playback_access_override);
        self.ds.set_mute_access_override(dev_cfg.mute_access_override);
        self.ds.set_loop_mode_access_override(dev_cfg.loop_mode_access_override);

        let panel_width = (if dev_cfg.paned_position > 0 { dev_cfg.paned_position } else { 200 })
            .max(MIN_PANEL_WIDTH);
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

    /// Write the current window/panel state into `dev` — only the
    /// window-related fields; pinned / window_open / etc. are preserved.
    /// Mini-mode aware: while the mini panel is showing, the full panel's
    /// size/maximized state come from the remembered `full_mode_*` fields
    /// (the one shared window is showing mini content right now, so
    /// reading it live would record the wrong panel), and the mini width
    /// reads live off the window (accurate mid-drag too); in full mode
    /// it's exactly the reverse.
    fn write_window_state(&self, dev: &mut config::DeviceConfig) {
        let in_mini = *self.mini_mode.borrow();
        let maximized = if in_mini { self.full_mode_maximized.get() } else { self.window.is_maximized() };
        let (w, h) = if in_mini {
            *self.full_mode_size.borrow()
        } else {
            (self.window.width(), self.window.height())
        };
        dev.window_maximized = maximized;
        dev.window_width     = if maximized { 0 } else { w };
        dev.window_height    = if maximized { 0 } else { h };
        dev.panel_visible    = self.sidebar_btn.is_active();
        dev.paned_position   = *self.saved_panel_width.borrow();
        dev.mini_mode        = in_mini;
        // Only overwrite once the mini panel has actually been shown this
        // session (`mini_mode_width` starts at 0 same as a never-realized
        // window's width() used to) — otherwise this would clobber a
        // previously saved good value with 0 every time a session never
        // happens to enter mini mode.
        let mw = if in_mini { self.window.width() } else { self.mini_mode_width.get() };
        if mw > 0 { dev.mini_window_width = mw; }
    }

    /// Immediately persist the current device's window/panel state.
    /// Loads the full config, updates only the current device's entry, and
    /// saves so no other device's entry is overwritten.
    pub(super) fn save_config_now(&self) {
        let uuid = match self.ds.device_info() {
            Some(di) if !di.uuid.is_empty() => di.uuid,
            _ => return,
        };
        config::update(|cfg| {
            cfg.last_uuid = uuid.clone();
            self.write_window_state(cfg.device_mut(&uuid));
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
        self.playback.borrow().set_active(false);
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
        self.playback.borrow().set_active(true);
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

/// Track OS-level maximize transitions on the shared window — capturing
/// `full_mode_size` on genuine maximizes and correcting the default size
/// after un-maximizes. See the comments inside for the live-confirmed
/// compositor behaviors this encodes.
pub(super) fn wire_maximize_tracking(inner: &Rc<DeviceWindowInner>) {
    let window = inner.window.clone();
    // Opportunistically keeps `full_mode_size` fresh from *any* genuine
    // maximize, not just the one `enter_mini_mode()` captures on its own
    // way in — covers a device that gets manually resized and then
    // maximized directly, with no intervening un-maximize, which
    // `enter_mini_mode()` alone can never see (by the time it runs,
    // width()/height() would already report the maximized size, not the
    // windowed one — see its own comment).
    //
    // The moment `is_maximized` flips to `true` is exactly the window
    // to catch this in: confirmed live (--debug=ui) that width()/
    // height() still report the *old*, pre-maximize windowed size at
    // that exact instant, for a brief window before GTK/the compositor
    // catches the surface up to the new maximized geometry — after
    // that, they're useless (screen resolution, not a real size to
    // remember). `maximize_call_pending` (see its own doc comment)
    // is what keeps this from misfiring on `exit_mini_mode()`'s own
    // restore-a-remembered-maximize call, which would otherwise
    // recapture the mini panel's own small size as if it were a real
    // windowed size the instant before maximizing.
    window.connect_notify_local(Some("maximized"), {
        let i = Rc::downgrade(&inner);
        move |win, _| {
            let Some(i) = i.upgrade() else { return };
            if !win.is_maximized() {
                // Not a "genuine maximize" transition — nothing to
                // capture. Still worth clearing defensively: a pending
                // flag that somehow never got consumed by a "true"
                // notify (e.g. the WM coalesced/dropped one) shouldn't
                // silently swallow a later, unrelated one.
                i.maximize_call_pending.set(false);

                // The actual guarantee behind the maximize/mini/
                // un-maximize fix (see exit_mini_mode()'s comment for
                // the full reasoning): whenever the window becomes
                // un-maximized while the full panel is the one showing
                // (never while entering mini mode itself — that's about
                // to shrink to mini dimensions right after this, forcing
                // full_mode_size here would just fight that), force it
                // back to full_mode_size if it isn't already there.
                //
                // Deferred via idle_add_local_once rather than applied
                // synchronously right here: an earlier version called
                // set_default_size() directly inside this same handler
                // and it didn't stick — confirmed live, a compositor
                // configure event for this *same* un-maximize transition
                // arrived a moment later and silently overwrote it back
                // to the wrong size. That call was racing (and losing
                // to) the compositor's own in-flight negotiation for
                // this transition, unlike enter_mini_mode()'s own
                // set_default_size() call after unmaximize() — that one
                // works because it runs on an already-*settled* window
                // (this same transition has fully finished by the time
                // any code the user's own actions trigger next runs),
                // making it a plain resize, not a race. `Priority::DEFAULT_IDLE`
                // (what idle_add_local_once uses) runs after any
                // already-queued/in-flight Wayland protocol messages —
                // i.e. after this transition's own remaining configure
                // events, if any — hopefully letting our correction go
                // last instead of first.
                if !*i.mini_mode.borrow() {
                    let (fw, fh) = *i.full_mode_size.borrow();
                    if fw > 0 && fh > 0 {
                        let win = win.clone();
                        glib::idle_add_local_once(move || {
                            if win.width() != fw || win.height() != fh {
                                dbg_ui(&format!(
                                    "un-maximize fixup (deferred): size is {}x{}, correcting to full_mode_size full:{fw},{fh}",
                                    win.width(), win.height(),
                                ));
                                win.set_default_size(fw, fh);
                            } else {
                                dbg_ui(&format!(
                                    "un-maximize fixup (deferred): size already correct (full:{fw},{fh}), nothing to do",
                                ));
                            }
                        });
                    }
                }
                return;
            }
            if i.maximize_call_pending.replace(false) {
                dbg_ui(&format!(
                    "maximize notify: our own exit_mini_mode() restore skipping state saving"
                ));
                return; // our own restore, not a fresh external maximize
            }
            // Real bug, confirmed live: the mini window is `resizable(false)`
            // and has no CSD of its own, but some external trigger (seen on
            // a user's system, cause unconfirmed — possibly a compositor/
            // decoration-negotiation quirk, not something this app did) can
            // still flip its `maximized` WM state hint while it's the one
            // showing. Without this guard, that got treated as a genuine
            // *full*-mode maximize and stored the mini panel's own tiny
            // dimensions (reported live: 360×200) into `full_mode_size` —
            // silently corrupting the size `exit_mini_mode()` restores to,
            // so leaving mini mode afterward reopened the full window at
            // that same tiny size instead of the user's real one. The mini
            // window's own geometry is never a meaningful "full mode" size
            // under any circumstance, genuine external maximize or not.
            if *i.mini_mode.borrow() {
                dbg_ui("maximize notify: mini mode active, not storing into full_mode_size");
                return;
            }
            let (w, h) = (win.width(), win.height());
            if w > 0 && h > 0 {
                let (old_fw, old_fh) = *i.full_mode_size.borrow();
                *i.full_mode_size.borrow_mut() = (w, h);
                dbg_ui(&format!(
                    "maximize notify: external maximize, storing size into full_mode_size: full:{w},{h} (was full:{old_fw},{old_fh})",
                ));
            }
        }
    });
}
