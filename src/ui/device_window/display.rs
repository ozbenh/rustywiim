//! Window-level display updates for `DeviceWindowInner`: the
//! connecting spinner, the offline/"Disconnected" chrome state, the
//! device-changed populate path (title, bottom bar, per-device window
//! state), network/BLE-remote indicators, keyboard transport shortcuts,
//! and the sidebar slide animation. The playback/preset/input-output
//! content itself is `ui/views/`' business, not this file's.

use std::rc::Rc;

use adw::prelude::*;

use crate::config;
use crate::ui::*;
use super::*;
use crate::ui::views::common::flash_button;
use super::geometry::schedule_config_save;

// ── impl DeviceWindowInner ────────────────────────────────────────────────────

/// Minimum time `connecting_spinner` stays visible once shown, so a
/// same-LAN reconnect that resolves in well under this doesn't hide it
/// again before it ever renders a single visible frame — see
/// `DeviceWindowInner::show_connecting_spinner()`/`hide_connecting_spinner()`.
const MIN_SPINNER_DISPLAY: std::time::Duration = std::time::Duration::from_secs(1);

impl DeviceWindowInner {
    // ── Connecting spinner ───────────────────────────────────────────────────

    /// Shows+starts `connecting_spinner`, recording when it was first shown
    /// (unless already showing) so `hide_connecting_spinner()` can enforce
    /// `MIN_SPINNER_DISPLAY`. Also cancels any pending deferred hide — a
    /// `Failed`/`Disconnected` → `Connecting` flip inside the debounce
    /// window must not let a stale hide fire after this call.
    fn show_connecting_spinner(self: &Rc<Self>) {
        if let Some(id) = self.spinner_hide_timer.borrow_mut().take() { id.remove(); }
        if self.spinner_shown_at.get().is_none() {
            self.spinner_shown_at.set(Some(std::time::Instant::now()));
        }
        self.connecting_spinner.set_visible(true);
        self.connecting_spinner.set_spinning(true);
    }

    /// Hides+stops `connecting_spinner`, deferring the actual hide if it
    /// hasn't been visible for `MIN_SPINNER_DISPLAY` yet. A no-op (besides
    /// making sure the widget is actually hidden) if the spinner was never
    /// shown in the first place.
    fn hide_connecting_spinner(self: &Rc<Self>) {
        let Some(shown_at) = self.spinner_shown_at.get() else {
            self.connecting_spinner.set_visible(false);
            self.connecting_spinner.set_spinning(false);
            return;
        };
        let elapsed = shown_at.elapsed();
        if elapsed >= MIN_SPINNER_DISPLAY {
            self.spinner_shown_at.set(None);
            self.connecting_spinner.set_visible(false);
            self.connecting_spinner.set_spinning(false);
            return;
        }
        if self.spinner_hide_timer.borrow().is_some() {
            return; // Hide already scheduled — let it run.
        }
        let i2 = Rc::clone(self);
        let id = glib::timeout_add_local_once(MIN_SPINNER_DISPLAY - elapsed, move || {
            *i2.spinner_hide_timer.borrow_mut() = None;
            i2.spinner_shown_at.set(None);
            i2.connecting_spinner.set_visible(false);
            i2.connecting_spinner.set_spinning(false);
        });
        *self.spinner_hide_timer.borrow_mut() = Some(id);
    }

    // ── Reset ─────────────────────────────────────────────────────────────────

    /// Shows the "no live `device_info`" state in the window chrome:
    /// title (cached-name fallback), connecting spinner, and bottom-bar
    /// labels. The playback panels render their own offline state
    /// (`render_offline()` in each playback view, keyed off
    /// `connection_state()` the same way) on their own `device-changed`
    /// subscriptions or on activation, whichever comes first.
    ///
    /// `Connecting` shows the corner spinner rather than any text — it's
    /// normally brief (a few hundred ms on a real LAN), and text that
    /// fast just reads as an unreadable flash/glitch. `apply_device_info()`
    /// is the other place that needs to hide the spinner again — it's the
    /// code path taken on the opposite transition (`Connecting` →
    /// `Connected`), which never runs through here.
    pub(super) fn reset_device_ui(self: &Rc<Self>, state: ConnectionState) {
        // Fall back to the cached name (see `cached_name`'s doc comment)
        // rather than the bare generic title, while there's no live
        // `device_info` yet to give a definitive one.
        let cached_name = self.cached_name.borrow();
        let win_title = if cached_name.is_empty() {
            "RustyWiiM".to_string()
        } else {
            format!("RustyWiiM ({cached_name})")
        };
        drop(cached_name);
        self.window.set_title(Some(&win_title));

        if state == ConnectionState::Connecting {
            self.show_connecting_spinner();
        } else {
            self.hide_connecting_spinner();
        }

        crate::ui::dbg_ui(&format!("reset_device_ui: state={state:?}"));
        self.dev_info_label.set_label("");
        self.ip_label.set_visible(false);
    }

    /// Populate the window-level UI (title, chrome, bottom bar) from
    /// whatever the DeviceState currently has cached. Called on initial
    /// window creation and on every `device-changed` signal. Safe to call
    /// redundantly — all underlying setters are idempotent. The playback
    /// displays, input/output dropdowns, presets, and volume clusters
    /// need nothing here — each view subscribes to `device-changed`
    /// itself.
    pub(super) fn populate_all(self: &Rc<Self>) {
        self.update_network_icon();
        self.update_remote_display();
        if self.ds.device_info().is_some() {
            self.apply_device_info();
        } else {
            // A window can sit genuinely `Disconnected` for a good while
            // (`set_device(..., connect_now: false)` means devlist already
            // believed the device offline and no connect was attempted).
            let state = self.ds.connection_state();
            crate::ui::dbg_ui(&format!("populate_all: no device_info, connection_state={state:?}"));
            self.reset_device_ui(state);
        }
    }

    // ── Network ───────────────────────────────────────────────────────────────

    pub(super) fn update_network_icon(&self) {
        match self.ds.netstat() {
            Some(0) => {
                self.net_icon.set_icon_name(Some("network-wired-symbolic"));
                self.net_icon.set_tooltip_text(None);
                self.net_icon.set_visible(true);
            }
            Some(2) => {
                let rssi = self.ds.rssi().unwrap_or(0);
                self.net_icon.set_icon_name(Some(wifi_icon_for_rssi(rssi)));
                let ssid = self.ds.device_info().map(|i| i.ssid_decoded()).unwrap_or_default();
                let tooltip = if ssid.is_empty() {
                    format!("Signal: {rssi} dBm")
                } else {
                    format!("Network: {ssid}\nSignal: {rssi} dBm")
                };
                self.net_icon.set_tooltip_text(Some(&tooltip));
                self.net_icon.set_visible(true);
            }
            _ => { self.net_icon.set_visible(false); }
        }
    }

    /// BLE remote presence/battery, bottom-left of the main window. Visible
    /// whenever `getStatusEx` has ever answered the question at all
    /// (`remote_info().connected.is_some()`) — including "known but
    /// currently disconnected" — and hidden only when we truly don't know
    /// (field absent from every response so far, e.g. no BLE remote
    /// hardware exists on this model). Hovering shows battery/signal detail,
    /// or "disconnected" when not currently connected.
    pub(super) fn update_remote_display(&self) {
        let info = self.ds.remote_info();
        let Some(connected) = info.connected else {
            self.remote_icon.set_visible(false);
            self.remote_label.set_visible(false);
            return;
        };

        let battery_text = if connected {
            info.battery.map(|pct| format!("{pct}%")).unwrap_or_default()
        } else {
            String::new()
        };
        let tooltip = if connected {
            format!(
                "Battery: {}\nSignal: {}",
                info.battery.map(|pct| format!("{pct}%")).unwrap_or_else(|| "unknown".to_string()),
                info.rssi.map(|r| format!("{r} dBm")).unwrap_or_else(|| "unknown".to_string()),
            )
        } else {
            "disconnected".to_string()
        };

        self.remote_label.set_label(&battery_text);
        self.remote_icon.set_tooltip_text(Some(&tooltip));
        self.remote_label.set_tooltip_text(Some(&tooltip));

        self.remote_icon.set_visible(true);
        self.remote_icon.queue_resize();
        self.remote_label.set_visible(!battery_text.is_empty());
        self.remote_label.queue_resize();
    }

    pub(super) fn apply_device_info(self: &Rc<Self>) {
        let info = match self.ds.device_info() { Some(i) => i, None => return };
        let caps = match self.ds.capabilities() { Some(c) => c, None => return };

        crate::ui::dbg_ui(&format!(
            "apply_device_info: showing real state for {:?}", info.device_name,
        ));
        // The only other place the corner spinner is touched is
        // `reset_device_ui()` (the `Connecting` case) — this is the
        // opposite transition (`Connecting` → `Connected`, a live
        // `device_info` just arrived) and never runs through there.
        self.hide_connecting_spinner();
        self.window.set_title(Some(&format!("RustyWiiM ({})", info.device_name)));
        // The mini top bar's device-name label is chrome (not part of
        // MiniPlaybackView), so it's kept fresh here alongside the window
        // title rather than by the view.
        self.mini.device_label.set_label(&info.device_name);
        // Refresh the disconnected-fallback title too (see `cached_name`'s
        // doc comment) — this device just answered, so its name is at
        // least as fresh as whatever config had at window-open time, and a
        // later disconnect should fall back to this, not that stale value
        // (e.g. the device having since been renamed in the WiiM app).
        *self.cached_name.borrow_mut() = info.device_name.clone();

        self.dev_info_label.set_label(&format!(
            "{} · {} · FW {}",
            caps.vendor.display_name(), caps.model, info.firmware,
        ));

        // Unlike dev_info_label (always visible, only its text ever
        // changes), ip_label starts invisible and is shown/hidden here on
        // every device-changed. queue_resize() forces a full fresh layout
        // pass on the reveal rather than risking a stale allocation/clip
        // from before the label was visible — belt-and-suspenders against
        // the top-row clipping seen on this label but not on dev_info_label.
        let ip = info.ip_addr();
        if !ip.is_empty() {
            self.ip_label.set_label(ip);
            self.ip_label.set_visible(true);
            self.ip_label.queue_resize();
        } else {
            self.ip_label.set_visible(false);
        }

        self.apply_device_window_state(&info.uuid);
    }

    // ── Volume helpers ────────────────────────────────────────────────────────

    /// The volume cluster belonging to whichever panel is currently
    /// showing — for the keyboard Up/Down shortcuts, so the flashy part
    /// (the level readout changing) happens where the user is looking.
    /// Returns a clone (a GObject refcount bump) since the mini one lives
    /// inside `MiniPlaybackView` rather than as a field here.
    pub(super) fn active_volume(&self) -> crate::ui::views::volume::VolumeControl {
        if *self.mini_mode.borrow() { self.mini.view.volume() } else { self.playback.volume() }
    }

} // impl DeviceWindowInner

/// Global playback/volume/window-mode keyboard shortcuts, shared by the main
/// and mini windows via the `EventControllerKey`s wired in `mod.rs`.
/// `prev_btn`/`next_btn`/`play_btn` are whichever window's transport buttons
/// received the key, so the flash appears on the window the user is
/// actually looking at.
pub(super) fn handle_transport_key(
    i:        &Rc<DeviceWindowInner>,
    keyval:   gtk::gdk::Key,
    state:    gtk::gdk::ModifierType,
    prev_btn: &gtk::Button,
    next_btn: &gtk::Button,
    play_btn: &gtk::Button,
) -> glib::Propagation {
    // Ignore Ctrl/Alt combinations so this doesn't shadow other accelerators
    // (Ctrl-W, Ctrl-Q, Alt-based window-manager bindings, etc.).
    if state.intersects(gtk::gdk::ModifierType::CONTROL_MASK | gtk::gdk::ModifierType::ALT_MASK) {
        return glib::Propagation::Proceed;
    }
    // The transport shortcuts follow their button's sensitivity (kept
    // current from `ps.caps.can_*` by the active playback view) — a
    // disabled action shouldn't fire just because it came in via keyboard.
    // `Proceed`, not `Stop`: with no action to perform, behave as if the
    // shortcut didn't exist rather than swallowing the key.
    match keyval {
        gtk::gdk::Key::Left if prev_btn.is_sensitive() => {
            i.ds.do_prev();
            flash_button(prev_btn);
            glib::Propagation::Stop
        }
        gtk::gdk::Key::Right if next_btn.is_sensitive() => {
            i.ds.do_next();
            flash_button(next_btn);
            glib::Propagation::Stop
        }
        gtk::gdk::Key::space if play_btn.is_sensitive() => {
            i.ds.do_play_pause();
            flash_button(play_btn);
            glib::Propagation::Stop
        }
        gtk::gdk::Key::Up => {
            i.active_volume().step(5);
            glib::Propagation::Stop
        }
        gtk::gdk::Key::Down => {
            i.active_volume().step(-5);
            glib::Propagation::Stop
        }
        gtk::gdk::Key::m | gtk::gdk::Key::M => {
            if *i.mini_mode.borrow() { i.exit_mini_mode(); } else { i.enter_mini_mode(); }
            schedule_config_save(i);
            glib::Propagation::Stop
        }
        _ => glib::Propagation::Proceed,
    }
}

/// Slide the paned's divider to `target_pos` (0 = fully closed) instead of
/// jumping instantly, so opening/closing the side panel reads as one motion.
/// Falls back to an instant set when animations are off (config.animations,
/// or GTK's reduce-motion). `panel_collapsing` is held for the animation's
/// duration so `connect_position_notify`'s drag-detection logic ignores the
/// frames this drives — same guard the instant path already relied on.
pub(super) fn animate_panel_to(i: &Rc<DeviceWindowInner>, target_pos: i32) {
    // Two statements, not `if let Some(a) = i.panel_anim.borrow_mut().take() { a.skip(); }`:
    // the RefMut temporary from borrow_mut() stays alive for the whole if-let
    // block (Rust's temporary lifetime rule for if-let scrutinees), so
    // panel_anim would still be borrowed while skip() runs below — and
    // skip() synchronously fires connect_done, which borrows panel_anim
    // again and panics. (Same bug as FlipCover's set_content/dispose/clear.)
    let old_anim = i.panel_anim.borrow_mut().take();
    if let Some(a) = old_anim { a.skip(); }

    if target_pos > 0 {
        // Visible immediately so it's revealed as the panel slides open,
        // rather than popping in once the animation finishes.
        i.left_pane.set_visible(true);
    }

    let from = i.paned.position();
    let animate = from != target_pos
        && config::with(|cfg| cfg.animations)
        && gtk::Settings::default().is_some_and(|s| s.is_gtk_enable_animations());

    if !animate {
        *i.panel_collapsing.borrow_mut() = true;
        i.paned.set_position(target_pos);
        *i.panel_collapsing.borrow_mut() = false;
        if target_pos <= 0 { i.left_pane.set_visible(false); }
        schedule_config_save(i);
        return;
    }

    *i.panel_collapsing.borrow_mut() = true;

    let weak  = Rc::downgrade(i);
    let paned = i.paned.clone();
    let anim_target = adw::CallbackAnimationTarget::new(move |v| {
        paned.set_position(v.round() as i32);
    });
    let anim = adw::TimedAnimation::new(&i.paned, from as f64, target_pos as f64, 200, anim_target);
    anim.set_easing(adw::Easing::EaseInOutCubic);
    anim.connect_done(move |_| {
        let Some(i) = weak.upgrade() else { return };
        *i.panel_collapsing.borrow_mut() = false;
        if target_pos <= 0 { i.left_pane.set_visible(false); }
        *i.panel_anim.borrow_mut() = None;
        schedule_config_save(&i);
    });
    anim.play();
    *i.panel_anim.borrow_mut() = Some(anim);
}

pub(super) fn wifi_icon_for_rssi(rssi: i32) -> &'static str {
    match rssi {
        i32::MIN..=-85 | 0 => "network-wireless-offline-symbolic",
        -84..=-75           => "network-wireless-signal-weak-symbolic",
        -74..=-65           => "network-wireless-signal-ok-symbolic",
        -64..=-55           => "network-wireless-signal-good-symbolic",
        _                   => "network-wireless-signal-excellent-symbolic",
    }
}
