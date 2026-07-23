mod art_background;
mod brand_icon;
pub mod devlist;
mod eq;
mod flip_cover;
mod icons;
mod kiosk;
pub(crate) mod menu;
mod prompt_entry;
mod scroll_fade_label;
mod device_window;
mod theme;
pub(crate) mod settings;
mod views;

use device_window::DeviceWindow;
pub(crate) use theme::{
    apply_accent_color, apply_theme, appearance_changed, broadcast_appearance_changed,
    current_tunables, cycle_theme, update_art_background_visibility,
};
use theme::{init_css, init_icon_resource};

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use adw::prelude::*;
use gtk::gio;

use crate::device::api::TlsMode;
use crate::config;
use crate::device::discovery::DiscoveryService;
use crate::device::discovery_manager::{DevicePresence, DiscoveryManager, ManagedEntry, SeedEntry};
use crate::device::manager::DeviceManager;
use crate::device::state::{ConnectionState, DeviceState, DEBUG_STATE};

/// GApplication ID / icon name / GResource base path / `.desktop` basename —
/// all the same string by freedesktop convention, kept in one place so
/// there's no risk of them drifting apart.
pub const APP_ID: &str = "io.github.ozbenh.rustywiim";

pub static DEBUG_UI: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

fn dbg_ui(msg: &str) {
    if DEBUG_UI.load(Ordering::Relaxed) {
        println!("{} [ui] {msg}", crate::timestamp());
    }
}

/// Set just before the quit action starts closing windows, so the
/// close-request/destroy handlers it triggers (DeviceWindowInner::cleanup())
/// know this isn't a user-initiated close. A window closed because the app
/// is quitting should still be reopened on next launch; a window the user
/// explicitly closed should not.
static QUITTING: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Set while `AppState::enter_kiosk()` is closing every device/discovery
/// window to make room for the single Kiosk window — same purpose as
/// `QUITTING` (see its doc comment), just for a different transition:
/// these windows are expected to reopen once Kiosk mode exits, so
/// `DeviceWindowInner::cleanup()` must not persist `window_open = false`
/// for them either.
static ENTERING_KIOSK: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

// ── Shared window actions ─────────────────────────────────────────────────────

/// Register `win.about` and `win.settings` on any ApplicationWindow.
/// Both the device window, discovery window, and mini window share these actions.
/// `ds` is `None` for the discovery window (settings window title has no device name).
pub(crate) fn wire_window_actions(
    window:        &impl glib::object::IsA<gtk::ApplicationWindow>,
    ds:            Option<DeviceState>,
    open_settings: Rc<dyn Fn(Option<DeviceState>)>,
) {
    let window = window.upcast_ref::<gtk::ApplicationWindow>().clone();
    let about_action = gio::SimpleAction::new("about", None);
    let win = window.clone();
    about_action.connect_activate(move |_, _| {
        adw::AboutDialog::builder()
            .application_name("RustyWiiM")
            .application_icon(APP_ID)
            .version(concat!(env!("CARGO_PKG_VERSION"), " (", env!("GIT_HASH"), ")"))
            .developer_name("Benjamin Herrenschmidt")
            .copyright("© 2026 Benjamin Herrenschmidt")
            .license_type(gtk::License::MitX11)
            .website("https://github.com/ozbenh/rustywiim")
            .build()
            .present(Some(&win));
    });
    window.add_action(&about_action);

    // Use a WeakRef so the closure does not keep the DeviceState alive after the
    // device window closes.  Upgrading on activation gives the same device (or
    // None if it has already been freed, which opens global settings — harmless).
    let ds_weak: Option<glib::WeakRef<DeviceState>> = ds.as_ref().map(|d| d.downgrade());
    let settings_action = gio::SimpleAction::new("settings", None);
    settings_action.connect_activate(move |_, _| {
        open_settings(ds_weak.as_ref().and_then(|w| w.upgrade()));
    });
    window.add_action(&settings_action);
}

// ── DeviceSpec ────────────────────────────────────────────────────────────────

/// Describes a specific device to connect to when creating a new device window.
pub struct DeviceSpec {
    pub ip:       String,
    pub uuid:     String,
    pub tls_mode: TlsMode,
    /// Whether to actually attempt a connection immediately
    /// (`DeviceManager::get()`'s `try_connect`) — `false` when devlist
    /// already believes this device offline, so opening its window
    /// doesn't repeat an already-known-to-fail attempt; see that
    /// function's doc comment.
    pub try_connect: bool,
}

/// `--connect <scheme://ip[:port]>` override: when set, `AppState::activate()`
/// skips discovery entirely and opens exactly one device window straight at
/// this address (uuid unknown until `getStatusEx` resolves it, same as any
/// freshly-added manual device) — for pointing the app directly at
/// `wiim-simulator` without it needing to be discoverable via SSDP. Must be
/// set (via `set_direct_connect`) before `activate()` runs — in practice,
/// during `main.rs`'s `connect_handle_local_options`.
static DIRECT_CONNECT: std::sync::OnceLock<(String, TlsMode)> = std::sync::OnceLock::new();

pub fn set_direct_connect(ip: String, tls_mode: TlsMode) {
    let _ = DIRECT_CONNECT.set((ip, tls_mode));
}

/// `--kiosk`: when set, `AppState::activate()` starts directly in Kiosk
/// mode, skipping the normal device-list-first-or-restore-per-device-
/// windows startup sequence entirely. Set (via `set_start_in_kiosk`)
/// before `activate()` runs, same as `DIRECT_CONNECT` — in practice,
/// during `main.rs`'s `connect_handle_local_options`. Combined with
/// `--connect`, Kiosk mode starts pre-bound to that device instead of
/// unbound (`activate()`'s own `DIRECT_CONNECT` branch handles this).
static START_IN_KIOSK: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

pub fn set_start_in_kiosk(v: bool) {
    START_IN_KIOSK.store(v, Ordering::Relaxed);
}

/// `--kiosk:only`: locks the session into Kiosk mode permanently — no
/// exit button, no "K" key. Implies `--kiosk` itself (same as
/// `--kiosk:layout`), set before `activate()` runs. `KioskWindow::new()`
/// reads this via `kiosk_only()` to skip wiring both exit paths entirely,
/// rather than merely hiding/disabling them — a locked-down kiosk
/// deployment shouldn't leave a technically-still-wired escape hatch
/// behind a hidden button or a stray keypress.
static KIOSK_ONLY: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

pub fn set_kiosk_only(v: bool) {
    KIOSK_ONLY.store(v, Ordering::Relaxed);
}

pub fn kiosk_only() -> bool {
    KIOSK_ONLY.load(Ordering::Relaxed)
}

/// `--kiosk:layout=1|2`: which playback layout Kiosk mode starts in
/// (still changeable at runtime with "L"). A lightweight mirror of
/// `views::playback_full::PlaybackLayout` rather than that type itself —
/// `views` is private to `ui`, so `main.rs` (a sibling of `ui`, not a
/// descendant) can't name it directly; `enter_kiosk_window()` converts
/// this into the real type right before constructing `KioskWindow`.
#[derive(Clone, Copy)]
pub enum KioskLayoutOverride { Classic, WideRight }

static KIOSK_LAYOUT_OVERRIDE: std::sync::OnceLock<KioskLayoutOverride> = std::sync::OnceLock::new();

pub fn set_kiosk_layout_override(v: KioskLayoutOverride) {
    let _ = KIOSK_LAYOUT_OVERRIDE.set(v);
}

// ── AppState ──────────────────────────────────────────────────────────────────
// Owns all top-level window state.  Every signal-handler closure captures
// either a strong Rc<AppState> or a Weak clone for the close-request handlers.

fn dbg_state(msg: &str) {
    if DEBUG_STATE.load(Ordering::Relaxed) {
        println!("{} [app] {msg}", crate::timestamp());
    }
}

pub(crate) struct AppState {
    app:            adw::Application,
    disc_mgr:       DiscoveryManager,
    device_manager: DeviceManager,
    registry:       RefCell<Vec<DeviceWindow>>,
    settings_reg:   RefCell<Vec<settings::SettingsWindow>>,
    disc_win:       RefCell<Option<devlist::DiscoveryWindow>>,
    kiosk_win:      RefCell<Option<Rc<kiosk::KioskWindow>>>,
}

impl AppState {
    // `disc_svc.start()` must run inside `connect_activate` so that
    // `glib::spawn_future_local` has an active main context.
    //
    // Skipped entirely under `--connect`: that mode exists to point the app
    // at an isolated target (e.g. `wiim-simulator`) without touching the
    // real network, so starting SSDP discovery in the background would
    // defeat the purpose (and send real traffic) even though `activate()`
    // never shows its results.
    pub(crate) fn new(app: &adw::Application, rt: Arc<tokio::runtime::Runtime>) -> Rc<Self> {
        let disc_svc = DiscoveryService::new(rt.clone());
        if DIRECT_CONNECT.get().is_none() {
            disc_svc.start();
        }
        let device_manager = DeviceManager::new(rt.clone());

        // `device_manager` construction is inert (no side effects) —
        // connecting `configure-device` this early, before anything else
        // touches `device_manager`, means there's no window where a
        // `DeviceState` could be created before this handler exists to
        // configure it. Resolves per-device config overrides (device/
        // can't read config itself) and pushes them onto the fresh
        // `DeviceState` before `create_and_configure()` lets it make first
        // contact.
        device_manager.connect_configure_device(|_, ds| {
            let uuid = ds.uuid();
            if uuid.is_empty() { return; }
            let (access_override, mute_access_override, loop_mode_access_override) = config::with(|cfg| {
                let d = cfg.device(&uuid);
                (d.playback_access_override, d.mute_access_override, d.loop_mode_access_override)
            });
            let gena_enabled = config::resolved_gena_enabled(&uuid);
            dbg_state(&format!(
                "configure-device: {} ({uuid}) access_override={access_override:?} mute_access_override={mute_access_override:?} loop_mode_access_override={loop_mode_access_override:?} gena_enabled={gena_enabled}",
                ds.ip(),
            ));
            ds.set_playback_access_override(access_override);
            ds.set_mute_access_override(mute_access_override);
            ds.set_loop_mode_access_override(loop_mode_access_override);
            ds.set_gena_enabled(gena_enabled);
        });

        // `disc_mgr` now owns the *entire* known-device registry (SSDP
        // consumption, pinned/config-remembered devices, presence — see
        // `device::discovery_manager`'s module doc comment) — it holds
        // `device_manager` directly rather than through a hook/callback
        // pair, since both live in `device/` now.
        let disc_mgr = DiscoveryManager::new(rt, disc_svc.clone(), device_manager.clone());

        Rc::new(Self {
            app:            app.clone(),
            disc_mgr,
            device_manager,
            registry:       RefCell::new(Vec::new()),
            settings_reg:   RefCell::new(Vec::new()),
            disc_win:       RefCell::new(None),
            kiosk_win:      RefCell::new(None),
        })
    }

    /// Open (or re-present) the settings window for `ds`, deduplicating by UUID.
    fn open_settings(self_rc: &Rc<Self>, ds: Option<DeviceState>) {
        let ds_uuid = ds.as_ref()
            .and_then(|d| d.device_info())
            .map(|i| i.uuid.clone())
            .filter(|u| !u.is_empty());
        {
            let reg = self_rc.settings_reg.borrow();
            for sw in reg.iter() {
                if sw.device_uuid() == ds_uuid {
                    dbg_state(&format!("settings: presenting existing for {:?}", ds_uuid));
                    sw.present();
                    return;
                }
            }
        }
        dbg_state(&format!("settings: opening new for {:?}", ds_uuid));
        let notify_kiosk_changed = {
            let state = Rc::clone(self_rc);
            Rc::new(move |mask: u32| {
                if let Some(kiosk) = state.kiosk_win.borrow().as_ref() {
                    kiosk.on_settings_changed(mask);
                }
            }) as Rc<dyn Fn(u32)>
        };
        let s = settings::SettingsWindow::new(ds, &self_rc.disc_mgr, notify_kiosk_changed);
        let win_clone  = s.window_ref().clone();
        let weak_self  = Rc::downgrade(self_rc);
        let close_uuid = ds_uuid.clone();
        // Kiosk's own screensaver/auto-hide idle timers used to keep
        // running the whole time Settings was open (still a plain
        // separate toplevel, not an overlay within Kiosk's own window),
        // so closing back to Kiosk could land on a black screensaver (or
        // hidden chrome) despite the user having been actively working
        // in Settings — see `KioskWindow::external_window_opened()`'s own
        // doc comment. Also hides Settings' own cursor to match, on a
        // touch screen where Kiosk already permanently hides its own.
        if let Some(kiosk) = self_rc.kiosk_win.borrow().as_ref() {
            kiosk.external_window_opened(s.window_ref());
            if kiosk.should_hide_cursor() {
                s.window_ref().set_cursor_from_name(Some("none"));
            }
        }
        s.window_ref().connect_close_request(move |win| {
            dbg_state(&format!("settings: closed for {:?}", close_uuid));
            if let Some(state) = weak_self.upgrade() {
                state.settings_reg.borrow_mut().retain(|w| w.window_ref() != &win_clone);
                if let Some(kiosk) = state.kiosk_win.borrow().as_ref() {
                    kiosk.external_window_closed(win);
                }
            }
            // Explicit, rather than relying on close()'s default handler to
            // do it — this is what actually frees the page widgets
            // (ComboRows etc.) and, with them, any strong refs their signal
            // closures hold (e.g. the Advanced page's access-method rows,
            // even after those were fixed to hold `ds` weakly — see
            // `wire_access_row()`'s doc comment). Without an explicit
            // destroy() here nothing actually confirmed the window's widget
            // tree itself was ever torn down, only that `settings_reg`
            // dropped its own reference to it.
            win.destroy();
            glib::Propagation::Proceed
        });
        s.present();
        self_rc.settings_reg.borrow_mut().push(s);
    }

    /// Show (or lazily create) the device-list window.
    fn show_devices(self_rc: &Rc<Self>) {
        let mut dw = self_rc.disc_win.borrow_mut();
        if dw.is_none() {
            dbg_state("device list: creating window");
            let open_device_fn = {
                let state = Rc::clone(self_rc);
                Rc::new(move |entry: &ManagedEntry| Self::open_device(&state, entry))
                    as Rc<dyn Fn(&ManagedEntry)>
            };
            let open_settings_fn = {
                let state = Rc::clone(self_rc);
                Rc::new(move |ds| Self::open_settings(&state, ds))
                    as Rc<dyn Fn(Option<DeviceState>)>
            };
            let enter_kiosk_fn = {
                let state = Rc::clone(self_rc);
                Rc::new(move || Self::enter_kiosk(&state, None)) as Rc<dyn Fn()>
            };
            *dw = Some(devlist::DiscoveryWindow::new(
                &self_rc.app,
                &self_rc.disc_mgr,
                open_device_fn,
                enter_kiosk_fn,
                open_settings_fn,
            ));
        }
        dbg_state("device list: presenting");
        dw.as_ref().unwrap().present();
    }

    /// Present the existing device window for `entry`, or open a new one.
    fn open_device(self_rc: &Rc<Self>, entry: &ManagedEntry) {
        {
            let reg = self_rc.registry.borrow();
            for w in reg.iter() {
                if w.uuid().map_or(false, |u| u == entry.uuid) {
                    dbg_state(&format!("device window: presenting existing for {} ({})", entry.name, entry.uuid));
                    w.present();
                    return;
                }
            }
        }
        dbg_state(&format!("device window: opening {} ({}) @ {}", entry.name, entry.uuid, entry.ip));
        if !entry.uuid.is_empty() {
            config::update(|cfg| cfg.device_mut(&entry.uuid).window_open = true);
        }
        Self::open_device_spec(self_rc, DeviceSpec {
            ip:          entry.ip.clone(),
            uuid:        entry.uuid.clone(),
            tls_mode:    entry.tls_mode,
            try_connect: entry.presence == DevicePresence::Active,
        });
    }

    /// Opens (or presents) a window for every entry in `entries` that
    /// `config.json` currently says should have one open. Shared by
    /// `activate()`'s startup restore (called once discovery's initial SSDP
    /// round completes, via `connect_initial_load`) and `exit_kiosk()`'s own
    /// restore (called immediately — discovery's long since settled by
    /// then, no need to wait for anything).
    fn open_windows_pending_in_config(self_rc: &Rc<Self>, entries: Vec<ManagedEntry>) {
        let to_open: Vec<_> = config::with(|cfg| {
            entries.into_iter()
                .filter(|entry| !entry.uuid.is_empty()
                    && cfg.devices.get(&entry.uuid).map_or(false, |d| d.window_open))
                .collect()
        });
        for entry in &to_open {
            Self::open_device(self_rc, entry);
        }
    }

    /// Same decision `activate()` makes at real startup (show the device
    /// list if it was open, or if nothing else was; reopen every device
    /// window `config.json` says was open), just synchronous — used by
    /// `exit_kiosk()` instead of replaying an in-session snapshot, so
    /// leaving Kiosk mode falls back to exactly what a fresh launch would
    /// show for the same `config.json`.
    fn restore_windows_from_config(self_rc: &Rc<Self>) {
        let (discovery_open, has_pending_windows) = config::with(|cfg| (
            cfg.discovery_open,
            cfg.devices.values().any(|d| d.window_open),
        ));
        if discovery_open || !has_pending_windows {
            Self::show_devices(self_rc);
        }
        Self::open_windows_pending_in_config(self_rc, self_rc.disc_mgr.entries());
    }

    /// Create a device window for `spec`, register it, and present it.
    fn open_device_spec(self_rc: &Rc<Self>, spec: DeviceSpec) {
        let log_uuid = spec.uuid.clone();
        let log_ip   = spec.ip.clone();
        dbg_state(&format!("device window: creating uuid={log_uuid} @ {log_ip}"));
        let show_fn = {
            let state = Rc::clone(self_rc);
            Rc::new(move || Self::show_devices(&state)) as Rc<dyn Fn()>
        };
        let enter_kiosk_fn = {
            let state = Rc::clone(self_rc);
            let uuid = log_uuid.clone();
            Rc::new(move || Self::enter_kiosk(&state, Some(uuid.clone()))) as Rc<dyn Fn()>
        };
        let open_settings_fn = {
            let state = Rc::clone(self_rc);
            Rc::new(move |ds| Self::open_settings(&state, ds)) as Rc<dyn Fn(Option<DeviceState>)>
        };
        let dw = DeviceWindow::new_for_device(
            &self_rc.app,
            self_rc.device_manager.clone(),
            show_fn,
            enter_kiosk_fn,
            open_settings_fn,
            spec,
        );
        let gtk_win   = dw.window.clone();
        dw.present();
        self_rc.registry.borrow_mut().push(dw);
        // Exempts this device from devlist's do_prune() for as long as this
        // window is open — see DeviceRecord::has_open_window's doc comment.
        // No-op if log_uuid is empty (uuid not resolved yet) or unknown to
        // devlist.
        self_rc.disc_mgr.set_window_open(&log_uuid, true);
        let win_key   = gtk_win.clone();
        let weak_self = Rc::downgrade(self_rc);
        gtk_win.connect_close_request({
            let log_uuid = log_uuid.clone();
            let win_key = win_key.clone();
            let weak_self = weak_self.clone();
            move |_| {
                dbg_state(&format!("device window: close-request uuid={log_uuid}"));
                if let Some(s) = weak_self.upgrade() {
                    let live_uuid = s.registry.borrow().iter()
                        .find(|w| w.window == win_key)
                        .and_then(|w| w.uuid());
                    s.registry.borrow_mut().retain(|w| w.window != win_key);
                    // Also close any Settings window open for this device.
                    // SettingsWindow holds a *strong* DeviceState clone
                    // (settings_reg, until the settings window itself
                    // closes) — without this, closing the device window
                    // leaves that strong clone alive, the DeviceState
                    // GObject never disposes, and polling keeps running
                    // indefinitely even though no window looks associated
                    // with the device anymore. Clone the window handle and
                    // drop the settings_reg borrow before calling close() —
                    // close() re-enters this same RefCell synchronously via
                    // its own close-request handler.
                    if let Some(uuid) = live_uuid.filter(|u| !u.is_empty()) {
                        let target = s.settings_reg.borrow().iter()
                            .find(|sw| sw.device_uuid().as_deref() == Some(uuid.as_str()))
                            .map(|sw| sw.window_ref().clone());
                        if let Some(win) = target {
                            win.close();
                        }
                    }
                }
                glib::Propagation::Proceed
            }
        });
        // Second connect_destroy: fires after new_inner's handler (connection order).
        // Removing from registry drops the last Rc<DeviceWindowInner>, triggering Drop.
        gtk_win.connect_destroy(move |_| {
            dbg_state(&format!("device window: destroyed uuid={log_uuid}"));
            if let Some(s) = weak_self.upgrade() {
                s.registry.borrow_mut().retain(|w| w.window != win_key);
                s.disc_mgr.set_window_open(&log_uuid, false);
            }
        });
    }

    /// Called once from `app.connect_activate`.
    pub(crate) fn activate(self_rc: &Rc<Self>) {
        {
            // update() only writes to disk if migrate() actually changed
            // something, so no need to check its return value here.
            config::update(|cfg| { cfg.migrate(); });
            let theme = config::with(|cfg| cfg.theme);
            // Must run before init_css(): it registers the embedded
            // GResource bundle, which is what makes the "resource:///..."
            // URIs the Wood theme's stylesheet references actually resolve.
            init_icon_resource();
            init_css(theme);
        }

        Self::install_quit_action(self_rc);

        // `--connect` override: skip discovery/config-restored windows entirely
        // and open exactly one device window straight at the given address.
        // uuid is empty (unresolved until getStatusEx) — DeviceManager::get()
        // and DeviceWindow::new_inner() already handle that case (a brand new,
        // not-yet-deduplicated DeviceState), same as for a manually-added device.
        //
        // `--connect --kiosk` together used to silently drop `--kiosk`: this
        // branch returned unconditionally, so the "if start_in_kiosk" check
        // further down never even ran. Building the DeviceState directly
        // (mirroring what `device_manager.get()` call `DeviceWindow::new_inner()`
        // itself makes from a `DeviceSpec`) and handing it to Kiosk mode via
        // `enter_kiosk_with_device()` instead of `open_device_spec()` is what
        // lets the two flags combine: Kiosk mode pre-bound to the `--connect`
        // target rather than a plain `DeviceWindow`.
        if let Some((ip, tls_mode)) = DIRECT_CONNECT.get() {
            let start_in_kiosk = START_IN_KIOSK.load(Ordering::Relaxed);
            dbg_state(&format!(
                "activate: --connect direct to {ip} via {tls_mode:?}{}",
                if start_in_kiosk { " (--kiosk)" } else { "" }
            ));
            if start_in_kiosk {
                let ds = self_rc.device_manager.get(
                    "", ip, *tls_mode, None, None, None, config::resolved_gena_enabled(""), true,
                );
                Self::enter_kiosk_with_device(self_rc, ds, ip.clone());
            } else {
                Self::open_device_spec(self_rc, DeviceSpec {
                    ip: ip.clone(),
                    uuid: String::new(),
                    tls_mode: *tls_mode,
                    try_connect: true,
                });
            }
            return;
        }

        // Reconnecting an already-open window to a corrected IP happens
        // directly inside `device::discovery_manager`'s own
        // `track_device()` the moment it detects a move (which then
        // triggers `list-changed`, persisting the correction via this
        // file's own listener above) — no separate `list-changed`-driven
        // pass needed here anymore (an earlier version of this
        // reconstructed "did the IP change" from a `list-changed` snapshot
        // diff, which is exactly the pattern that caused a real flapping
        // `Disconnected`/`Connecting…` bug for presence; not resurrecting
        // that shape for IP changes either).

        // `--kiosk`: skip showing the normal device-list/per-device windows
        // below entirely (Kiosk mode starts unbound regardless of what was
        // open last session) — but discovery itself still needs to run
        // (unlike `--connect`'s early return above), since Kiosk mode's own
        // device-list popover needs real tracked devices to show. Entering
        // Kiosk mode itself happens after `disc_mgr.start()` further down.
        let start_in_kiosk = START_IN_KIOSK.load(Ordering::Relaxed);

        // Show the device list (if it should appear at all) *before*
        // starting discovery/restoring per-device windows below, so it
        // ends up at the bottom of the window stack instead of on top of
        // (potentially hiding) smaller device windows that open right
        // after it — GTK/GNOME gives no direct stacking-order control,
        // but a newly-presented window consistently lands above ones
        // already presented, so ordering these calls is the only lever
        // available. Reading `discovery_open`/`has_pending_windows`
        // directly from config rather than via `disc_mgr` — neither
        // depends on `start()` having run yet.
        let (discovery_open, has_pending_windows) = config::with(|cfg| (
            cfg.discovery_open,
            cfg.devices.values().any(|d| d.window_open),
        ));
        if !start_in_kiosk && (discovery_open || !has_pending_windows) {
            dbg_state("activate: showing device list");
            Self::show_devices(self_rc);
        }

        // Restore windows from config on startup.  initial-load fires once,
        // synchronously inside start(), so open_device() here is safe — no
        // risk of raising already-open windows on subsequent list changes.
        // Skipped entirely under `--kiosk` — see above.
        if !start_in_kiosk {
            let s = Rc::downgrade(self_rc);
            self_rc.disc_mgr.connect_initial_load(move |mgr| {
                let Some(self_rc) = s.upgrade() else { return };
                Self::open_windows_pending_in_config(&self_rc, mgr.entries());
            });
        }

        // Seed the manager from config — it can't read config itself (same
        // rule `device::manager::DeviceManager` already follows). Must
        // happen before `start()`, which eagerly tracks the pinned/
        // window_open subset of this synchronously.
        let seed: Vec<SeedEntry> = config::with(|cfg| {
            cfg.devices.iter().map(|(uuid, d)| SeedEntry {
                uuid:        uuid.clone(),
                name:        d.name.clone(),
                model:       d.model.clone(),
                project:     d.project.clone(),
                firmware:    d.firmware.clone(),
                pinned:      d.pinned == Some(true),
                last_ip:     d.last_ip.clone(),
                tls_mode:    d.tls_mode.map(|n| TlsMode::from_usize(n as usize)).unwrap_or(TlsMode::HttpsWiiM),
                window_open: d.window_open,
            }).collect()
        });
        let devlist_song_info = config::with(|cfg| cfg.devlist_song_info);
        self_rc.disc_mgr.load_seed(seed, devlist_song_info);

        // `disc_mgr` can't persist to config itself either — this is the
        // "report out" half of the same rule, replacing what used to be an
        // internal `persist_pinned()` call scattered across several of its
        // own methods. Fires unconditionally on every `list-changed`
        // (pin toggle, identity update, presence flip, ...) rather than
        // being selectively triggered — cheap and safe since
        // `config::update()` already diffs the whole `Config` before
        // deciding whether to actually write to disk.
        self_rc.disc_mgr.connect_list_changed(|mgr| {
            let entries = mgr.entries();
            config::update(|cfg| {
                for e in &entries {
                    if e.uuid.is_empty() { continue; }
                    let dev = cfg.device_mut(&e.uuid);
                    dev.pinned = Some(e.pinned); // Explicit Some(true/false) ends legacy treatment.
                    dev.last_ip = Some(e.ip.clone());
                    dev.tls_mode = Some(e.tls_mode as u8);
                    dev.name = Some(e.name.clone());
                    if !e.model.is_empty()    { dev.model = Some(e.model.clone()); }
                    if !e.project.is_empty()  { dev.project = Some(e.project.clone()); }
                    if !e.firmware.is_empty() { dev.firmware = Some(e.firmware.clone()); }
                }
            });
        });

        self_rc.disc_mgr.start();

        if start_in_kiosk {
            dbg_state("activate: --kiosk, entering Kiosk mode unbound");
            Self::enter_kiosk(self_rc, None);
        }
    }

    /// Enters Kiosk mode, bound to `bind_uuid` if given (unbound —
    /// showing the device-list popover with nothing selected — when
    /// `None`, e.g. entered from the discovery window's own menu).
    ///
    /// Snapshots which device windows are currently open (uuids only —
    /// deliberately pure in-session runtime state, not persisted to
    /// `config.json`: `DeviceConfig::window_open`'s own
    /// quit/last-window-preservation logic in `DeviceWindowInner::cleanup()`
    /// is the wrong mechanism here, since it only reliably preserves
    /// whichever window happens to close *last* — an earlier-closed window
    /// in a multi-window session would have its flag cleared to `false`
    /// before this function finishes closing everything else) and whether
    /// the discovery window was open, to reopen both in `exit_kiosk()`.
    ///
    /// Presents the (possibly already-existing) `KioskWindow` *before*
    /// closing anything else — load-bearing ordering: `main.rs`'s
    /// `connect_window_removed` auto-quits the instant zero windows are
    /// visible, unconditionally, with no `QUITTING`/`ENTERING_KIOSK` guard
    /// of its own. Presenting first guarantees at least one window stays
    /// visible throughout this transition, so that auto-quit never fires.
    pub(crate) fn enter_kiosk(self_rc: &Rc<Self>, bind_uuid: Option<String>) {
        let kw = Self::enter_kiosk_window(self_rc);

        // An explicit bind_uuid (entered from a device window) always wins;
        // otherwise fall back to whatever device Kiosk mode last showed
        // (Config::kiosk_last_uuid, if it's already a currently-tracked
        // device), and failing that, the first Active device found.
        let bind_uuid = bind_uuid.or_else(|| Self::resolve_kiosk_default(&self_rc.disc_mgr));
        kw.bind_device(bind_uuid.as_deref());

        // If nothing resolved *yet*, keep watching rather than settling for
        // "nothing selected": discovery is asynchronous (SSDP responses
        // arrive well after `disc_mgr.start()` returns — confirmed live, a
        // fresh `--kiosk` launch reaches this point before any real device
        // has actually responded, so the immediate resolution above finds
        // nothing even for an already-known kiosk_last_uuid device that
        // isn't otherwise pinned/previously-open). The first time a device
        // becomes available — the persisted device reappearing, or failing
        // that any Active device — bind it, unless the user has already
        // picked something else by then (checked via `current_key()`).
        if kw.current_key().is_empty() {
            let weak_kw = Rc::downgrade(&kw);
            self_rc.disc_mgr.connect_list_changed(move |mgr| {
                let Some(kw) = weak_kw.upgrade() else { return };
                if !kw.current_key().is_empty() { return; }
                if let Some(uuid) = Self::resolve_kiosk_default(mgr) {
                    kw.bind_device(Some(&uuid));
                }
            });
        }
    }

    /// Same as `enter_kiosk`, but for `--connect`'s already-constructed
    /// `DeviceState` — `--connect` deliberately bypasses discovery/SSDP
    /// entirely (see `DIRECT_CONNECT`'s doc comment), so there's no
    /// `DiscoveryManager` entry/uuid for `KioskWindow::bind_device()` to
    /// resolve; `bind_direct()` skips that lookup and uses `ds` as-is.
    /// No fallback-watching needed either, since the device is already
    /// known synchronously — unlike the uuid path, nothing here depends on
    /// discovery ever completing.
    pub(crate) fn enter_kiosk_with_device(self_rc: &Rc<Self>, ds: DeviceState, label: String) {
        let kw = Self::enter_kiosk_window(self_rc);
        kw.bind_direct(ds, &label);
    }

    /// Shared by `enter_kiosk()`/`enter_kiosk_with_device()`: returns the
    /// existing `KioskWindow` if already in Kiosk mode (retargeting is the
    /// caller's job), otherwise builds and presents a fresh `KioskWindow`
    /// and closes everything else — all before either caller binds a
    /// device into it. No in-session snapshot of what was open — closing
    /// every other window under `ENTERING_KIOSK` leaves `config.json`
    /// describing exactly what was really open (each window's own close
    /// path already persists that, guarded not to clear it for this
    /// transition — see `DeviceWindowInner::cleanup()` and
    /// `DiscoveryWindow`'s own `close_request` handler), the same way a
    /// real quit does; `exit_kiosk()` just re-reads it, mirroring
    /// `activate()`'s own startup decision instead of replaying a
    /// remembered list.
    fn enter_kiosk_window(self_rc: &Rc<Self>) -> Rc<kiosk::KioskWindow> {
        if let Some(kw) = self_rc.kiosk_win.borrow().as_ref() {
            return Rc::clone(kw);
        }

        let icons = Rc::new(icons::IconSet::load());
        let exit_fn = {
            let state = Rc::clone(self_rc);
            Rc::new(move || Self::exit_kiosk(&state)) as Rc<dyn Fn()>
        };
        // Same shared, plain non-modal open_settings_fn every other window
        // uses (DeviceWindow/DiscoveryWindow).
        let open_settings_fn = {
            let state = Rc::clone(self_rc);
            Rc::new(move |ds| Self::open_settings(&state, ds)) as Rc<dyn Fn(Option<DeviceState>)>
        };
        let initial_layout = match KIOSK_LAYOUT_OVERRIDE.get() {
            Some(KioskLayoutOverride::Classic) => views::playback_full::PlaybackLayout::Classic,
            Some(KioskLayoutOverride::WideRight) | None => views::playback_full::PlaybackLayout::WideRight,
        };
        let kw = kiosk::KioskWindow::new(
            &self_rc.app, &self_rc.disc_mgr, &icons, exit_fn, open_settings_fn, initial_layout, kiosk_only(),
        );
        kw.present();
        *self_rc.kiosk_win.borrow_mut() = Some(Rc::clone(&kw));

        ENTERING_KIOSK.store(true, Ordering::Relaxed);
        // Collect first so connect_destroy (which mutates registry) doesn't
        // invalidate the iterator — same pattern install_quit_action() uses.
        let wins: Vec<_> = self_rc.registry.borrow().iter().map(|dw| dw.window.clone()).collect();
        for win in wins {
            gtk::prelude::WidgetExt::realize(&win);
            win.close();
        }
        // Hidden, not destroyed — DiscoveryWindow's own close-request
        // handler already does exactly that when another window (the
        // just-presented KioskWindow) is visible, so `disc_win` stays
        // populated and `exit_kiosk()` can just re-present the same cached
        // instance via `show_devices()`, same as any other re-present.
        if let Some(dw) = self_rc.disc_win.borrow().as_ref() {
            gtk::prelude::WidgetExt::realize(&dw.window);
            dw.window.close();
        }
        ENTERING_KIOSK.store(false, Ordering::Relaxed);

        kw
    }

    /// See `enter_kiosk()`'s fallback-selection comment. Among Active
    /// devices, prefers one that's actually playing right now over just
    /// any responding device.
    fn resolve_kiosk_default(mgr: &DiscoveryManager) -> Option<String> {
        let last = config::with(|cfg| cfg.kiosk_last_uuid.clone());
        if let Some(uuid) = last {
            if mgr.entry_for(&uuid).is_some() {
                return Some(uuid);
            }
        }
        let active: Vec<_> = mgr.entries().into_iter()
            .filter(|e| e.presence == DevicePresence::Active)
            .collect();
        active.iter()
            .find(|e| mgr.device_state_for(&e.uuid)
                .is_some_and(|ds| ds.playback_state().status == crate::device::playback::PlaybackStatus::Playing))
            .or_else(|| active.first())
            .map(|e| e.uuid.clone())
    }

    /// Exits Kiosk mode: re-derives what should be open straight from
    /// `config.json` (`restore_windows_from_config()` — the same decision
    /// `activate()` makes at real startup) rather than replaying an
    /// in-session snapshot, then closes the `KioskWindow` — nothing more
    /// (no special-casing for whatever device was actively bound *inside*
    /// Kiosk mode at the moment of exit). A `--kiosk`-launched process with
    /// nothing ever marked open in config naturally falls back to the
    /// device list (`restore_windows_from_config()`'s own
    /// `!has_pending_windows` branch), matching normal fresh-install
    /// behavior instead of quitting the way it used to.
    ///
    /// **Ordering is load-bearing here too, same as `enter_kiosk()`
    /// (gremlin 9): reopen everything else *before* closing `KioskWindow`**,
    /// not after — closing it first, even briefly, is exactly the "zero
    /// windows visible" moment `main.rs`'s unconditional
    /// `connect_window_removed` auto-quit fires on, killing the whole app
    /// instead of returning to normal mode (confirmed live: plain
    /// K-to-enter, K-to-exit from a normal desktop session quit the app
    /// before this was fixed). `restore_windows_from_config()` always
    /// presents at least one window (the device list, if nothing else
    /// qualifies), so that moment never arrives here either.
    pub(crate) fn exit_kiosk(self_rc: &Rc<Self>) {
        if self_rc.kiosk_win.borrow().is_none() { return; }

        Self::restore_windows_from_config(self_rc);

        if let Some(kw) = self_rc.kiosk_win.borrow_mut().take() {
            kw.close();
        }
    }

    /// Replace the app.quit action (set up in main.rs) with one that
    /// explicitly destroys every device window first so connect_destroy
    /// fires (saving config, cancelling timers). win.close() is a no-op on
    /// unrealized windows (e.g. a window never shown when starting in mini
    /// mode), and app.quit() on its own destroys windows after the main
    /// loop exits, where cleanup is unreliable.
    fn install_quit_action(self_rc: &Rc<Self>) {
        let s = Rc::downgrade(self_rc);
        let app = self_rc.app.clone();
        let quit_action = gio::SimpleAction::new("quit", None);
        quit_action.connect_activate(move |_, _| {
            dbg_ui("quit action fired");
            QUITTING.store(true, Ordering::Relaxed);
            if let Some(s) = s.upgrade() {
                // Collect first so connect_destroy (which mutates registry) doesn't
                // invalidate the iterator.
                let wins: Vec<_> = s.registry.borrow().iter()
                    .map(|dw| dw.window.clone())
                    .collect();
                dbg_ui(&format!("quit: closing {} window(s)", wins.len()));
                for win in wins {
                    // realize() first: close() is a no-op on unrealized windows
                    // (e.g. main window never shown when starting in mini mode).
                    gtk::prelude::WidgetExt::realize(&win);
                    win.close();
                }
            } else {
                dbg_ui("quit: AppState already freed");
            }
            app.quit();
        });
        self_rc.app.add_action(&quit_action);
    }
}

