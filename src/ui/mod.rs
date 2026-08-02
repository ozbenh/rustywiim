mod art_background;
mod brand_icon;
pub mod devlist;
mod engraved_label;
mod eq;
mod flip_cover;
mod icons;
mod kiosk;
pub(crate) mod menu;
mod prompt_entry;
mod osk;
mod scroll_fade_label;
mod device_window;
mod theme;
pub(crate) mod settings;
mod touch;
mod vfd_scanline;
mod vfd_scanline_overlay;
mod views;

use device_window::DeviceWindow;
pub(crate) use theme::{
    apply_accent_color, apply_theme, appearance_changed, broadcast_appearance_changed,
    current_tunables, cycle_theme, update_art_background_visibility,
};
#[cfg(target_os = "macos")]
pub(crate) use device_window::update_mini_floating_state;
use theme::{init_css, init_icon_resource};

use std::cell::{Cell, RefCell};
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

pub(crate) fn dbg_ui(msg: &str) {
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

/// Register `win.about` and `win.preferences` on any ApplicationWindow.
/// The device window, discovery window, and mini window share these
/// actions. `win.device-settings` is *not* registered here — only a device
/// window needs it, and only that window has the group-role live wiring
/// (leader greying) and the `DeviceState` a device-scoped action needs; see
/// `device_window::wire_window_lifecycle()`, which builds its own alongside
/// this call rather than this function taking an `Option<DeviceState>` for
/// a case only one caller ever has.
pub(crate) fn wire_window_actions(
    window:           &impl glib::object::IsA<gtk::ApplicationWindow>,
    open_preferences: Rc<dyn Fn()>,
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

    let preferences_action = gio::SimpleAction::new("preferences", None);
    preferences_action.connect_activate(move |_, _| {
        open_preferences();
    });
    window.add_action(&preferences_action);
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
/// skips discovery entirely, resolves this address's uuid with one direct
/// `DiscoveryService::probe_known_scheme()` call, and opens exactly one
/// device window straight at it — for pointing the app directly at
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

/// Confirms a device-list trashcan click before actually forgetting the
/// device — shared by `DiscoveryWindow` and Kiosk's own device popover
/// (both wire `DeviceListView::connect_device_forget` to this rather than
/// calling `forget` directly), since removal also drops all of that
/// device's per-device settings (access overrides, GENA toggle, ...), not
/// just its list entry.
pub(crate) fn confirm_forget_device(
    parent: &adw::ApplicationWindow,
    name: &str,
    uuid: String,
    forget: Rc<dyn Fn(&str)>,
) {
    let dialog = adw::AlertDialog::builder()
        .heading("Remove Device?")
        .body(format!(
            "This will remove device \u{201c}{name}\u{201d} from the list and forget all of this application's settings for this device."
        ))
        .close_response("cancel")
        .build();
    dialog.add_response("cancel", "Cancel");
    dialog.add_response("ok", "OK");
    dialog.set_response_appearance("ok", adw::ResponseAppearance::Destructive);
    dialog.set_default_response(Some("cancel"));
    dialog.connect_response(None, move |_, response| {
        if response == "ok" {
            forget(&uuid);
        }
    });
    dialog.present(Some(parent));
}

pub(crate) struct AppState {
    app:            adw::Application,
    disc_mgr:       DiscoveryManager,
    device_manager: DeviceManager,
    registry:       RefCell<Vec<DeviceWindow>>,
    /// Exactly one instance process-wide — see `PreferencesWindow`'s own
    /// doc comment. `None` when not currently open; `open_preferences()`
    /// lazily creates it and clears this back to `None` on close (the
    /// window is fully destroyed on close, unlike `disc_win` below, which
    /// hides instead — so re-presenting a stale, already-destroyed handle
    /// isn't an option here).
    preferences_win: RefCell<Option<settings::PreferencesWindow>>,
    /// One entry per currently-open `DeviceSettingsWindow`, deduplicated by
    /// uuid in `open_device_settings()`. Unlike `preferences_win` this
    /// isn't a singleton — a different device's settings can be open at
    /// the same time.
    device_settings_reg: RefCell<Vec<settings::DeviceSettingsWindow>>,
    disc_win:       RefCell<Option<devlist::DiscoveryWindow>>,
    kiosk_win:      RefCell<Option<Rc<kiosk::KioskWindow>>>,
    /// Set while `exit_kiosk()` is waiting for the Kiosk window to finish
    /// leaving fullscreen, so a second exit request during that window
    /// (the "K" key is still live until the window actually closes) can't
    /// start a second teardown.
    kiosk_exiting:  Cell<bool>,
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
        // consumption, config-remembered devices, presence — see
        // `device::discovery_manager`'s module doc comment) — it holds
        // `device_manager` directly rather than through a hook/callback
        // pair, since both live in `device/` now.
        let disc_mgr = DiscoveryManager::new(rt, disc_svc.clone(), device_manager.clone());

        Rc::new(Self {
            app:            app.clone(),
            disc_mgr,
            device_manager,
            registry:       RefCell::new(Vec::new()),
            preferences_win: RefCell::new(None),
            device_settings_reg: RefCell::new(Vec::new()),
            disc_win:       RefCell::new(None),
            kiosk_win:      RefCell::new(None),
            kiosk_exiting:  Cell::new(false),
        })
    }

    /// Builds the `notify_kiosk_changed` callback `PreferencesWindow` needs
    /// (its Kiosk page live-pushes certain settings into a currently-open
    /// `KioskWindow` — a no-op when none is open). Shared by
    /// `open_preferences()`; its own small function only so that one isn't
    /// cluttered with this closure's construction.
    fn kiosk_notify_fn(self_rc: &Rc<Self>) -> Rc<dyn Fn(u32)> {
        let state = Rc::clone(self_rc);
        Rc::new(move |mask: u32| {
            if let Some(kiosk) = state.kiosk_win.borrow().as_ref() {
                kiosk.on_settings_changed(mask);
            }
        })
    }

    /// Registers the Kiosk-idle-timer/cursor interplay and the
    /// destroy-on-close teardown a settings-family window (Preferences or
    /// Device Settings) needs — the part that's identical between the two
    /// regardless of what's actually in the window. `on_closed` runs first,
    /// inside the close-request handler, so the caller can drop its own
    /// registry entry before the generic Kiosk/destroy handling below runs.
    fn wire_settings_window_close(
        self_rc:   &Rc<Self>,
        window:    &adw::Window,
        on_closed: impl Fn(&Rc<Self>) + 'static,
    ) {
        // Kiosk's own screensaver/auto-hide idle timers used to keep
        // running the whole time a settings window was open (still a plain
        // separate toplevel, not an overlay within Kiosk's own window), so
        // closing back to Kiosk could land on a black screensaver (or
        // hidden chrome) despite the user having been actively working in
        // it — see `KioskWindow::external_window_opened()`'s own doc
        // comment. Also hides the settings window's own cursor to match,
        // on a touch screen where Kiosk already permanently hides its own.
        if let Some(kiosk) = self_rc.kiosk_win.borrow().as_ref() {
            kiosk.external_window_opened(window);
            if kiosk.should_hide_cursor() {
                window.set_cursor_from_name(Some("none"));
            }
        }
        let weak_self = Rc::downgrade(self_rc);
        window.connect_close_request(move |win| {
            if let Some(state) = weak_self.upgrade() {
                on_closed(&state);
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
            // tree itself was ever torn down, only that the registry
            // dropped its own reference to it.
            win.destroy();
            glib::Propagation::Proceed
        });
    }

    /// Open (or re-present) the one app-wide Preferences window.
    fn open_preferences(self_rc: &Rc<Self>) {
        if let Some(pw) = self_rc.preferences_win.borrow().as_ref() {
            dbg_state("preferences: presenting existing");
            pw.present();
            return;
        }
        dbg_state("preferences: opening new");
        let notify_kiosk_changed = Self::kiosk_notify_fn(self_rc);
        let pw = settings::PreferencesWindow::new(&self_rc.disc_mgr, notify_kiosk_changed);
        Self::wire_settings_window_close(self_rc, pw.window_ref(), |state| {
            dbg_state("preferences: closed");
            *state.preferences_win.borrow_mut() = None;
        });
        pw.present();
        *self_rc.preferences_win.borrow_mut() = Some(pw);
    }

    /// Open (or re-present) the Device Settings window for `ds`,
    /// deduplicating by uuid.
    fn open_device_settings(self_rc: &Rc<Self>, ds: DeviceState) {
        let uuid = ds.uuid();
        {
            let reg = self_rc.device_settings_reg.borrow();
            for sw in reg.iter() {
                if sw.device_uuid().as_deref() == Some(uuid.as_str()) {
                    dbg_state(&format!("device settings: presenting existing for {uuid}"));
                    sw.present();
                    return;
                }
            }
        }
        dbg_state(&format!("device settings: opening new for {uuid}"));
        let s = settings::DeviceSettingsWindow::new(ds);
        let win_clone = s.window_ref().clone();
        Self::wire_settings_window_close(self_rc, s.window_ref(), move |state| {
            dbg_state(&format!("device settings: closed for {uuid}"));
            state.device_settings_reg.borrow_mut().retain(|w| w.window_ref() != &win_clone);
        });
        s.present();
        self_rc.device_settings_reg.borrow_mut().push(s);
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
            let open_preferences_fn = {
                let state = Rc::clone(self_rc);
                Rc::new(move || Self::open_preferences(&state)) as Rc<dyn Fn()>
            };
            let open_device_settings_fn = {
                let state = Rc::clone(self_rc);
                Rc::new(move |ds| Self::open_device_settings(&state, ds))
                    as Rc<dyn Fn(DeviceState)>
            };
            let enter_kiosk_fn = {
                let state = Rc::clone(self_rc);
                Rc::new(move || Self::enter_kiosk(&state, None)) as Rc<dyn Fn()>
            };
            let forget_device_fn = {
                let state = Rc::clone(self_rc);
                Rc::new(move |uuid: &str| Self::forget_device(&state, uuid)) as Rc<dyn Fn(&str)>
            };
            *dw = Some(devlist::DiscoveryWindow::new(
                &self_rc.app,
                &self_rc.disc_mgr,
                open_device_fn,
                enter_kiosk_fn,
                open_preferences_fn,
                open_device_settings_fn,
                forget_device_fn,
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
        let open_preferences_fn = {
            let state = Rc::clone(self_rc);
            Rc::new(move || Self::open_preferences(&state)) as Rc<dyn Fn()>
        };
        let open_device_settings_fn = {
            let state = Rc::clone(self_rc);
            Rc::new(move |ds| Self::open_device_settings(&state, ds)) as Rc<dyn Fn(DeviceState)>
        };
        let dw = DeviceWindow::new_for_device(
            &self_rc.app,
            self_rc.device_manager.clone(),
            show_fn,
            enter_kiosk_fn,
            open_preferences_fn,
            open_device_settings_fn,
            spec,
        );
        let gtk_win   = dw.window.clone();
        dw.present();
        self_rc.registry.borrow_mut().push(dw);
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
                    // Also close any Device Settings window open for this
                    // device. DeviceSettingsWindow holds a *strong*
                    // DeviceState clone (device_settings_reg, until the
                    // window itself closes) — without this, closing the
                    // device window leaves that strong clone alive, the
                    // DeviceState GObject never disposes, and polling keeps
                    // running indefinitely even though no window looks
                    // associated with the device anymore. Clone the window
                    // handle and drop the device_settings_reg borrow before
                    // calling close() — close() re-enters this same
                    // RefCell synchronously via its own close-request
                    // handler.
                    if let Some(uuid) = live_uuid.filter(|u| !u.is_empty()) {
                        Self::close_settings_window_for(&s, &uuid);
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
            }
        });
    }

    /// Closes the Device Settings window open for `uuid`, if any. Shared by
    /// `open_device_spec()`'s own close-request cascade (a device window
    /// closing shouldn't leave its Device Settings window behind, holding
    /// a strong `DeviceState` clone that keeps polling with nothing
    /// visibly attached to it anymore) and `forget_device()` below, which
    /// may need to close one with no device window behind it at all —
    /// opened straight from the device list's cog.
    fn close_settings_window_for(self_rc: &Rc<Self>, uuid: &str) {
        let target = self_rc.device_settings_reg.borrow().iter()
            .find(|sw| sw.device_uuid().as_deref() == Some(uuid))
            .map(|sw| sw.window_ref().clone());
        if let Some(win) = target {
            win.close();
        }
    }

    /// User-initiated device removal — the device list's offline-only
    /// trashcan button (see `views::devlist::build_device_content()`).
    ///
    /// Force-closes any open device window for `uuid` first (its own
    /// close-request handler already cascades into closing a Settings
    /// window behind it — see `close_settings_window_for()`'s doc
    /// comment); if there's no device window but a Settings window is
    /// still open on its own (opened straight from the device list's cog),
    /// closes that directly. Leaving either open would keep it driving a
    /// `DeviceState` the registry no longer tracks via a strong ref of its
    /// own.
    ///
    /// Rebinds Kiosk mode away from the forgotten device if it's the one
    /// currently shown there, rather than leaving it displaying a device
    /// that's no longer in the list.
    ///
    /// `disc_mgr.forget()` alone would only be half the job — the config
    /// entry is what would otherwise re-seed this exact device the next
    /// time `load_seed()`/`start()` runs (known-by-default retention — see
    /// `DiscoveryManager`'s own doc comment), so deleting it is what makes
    /// this removal actually stick.
    fn forget_device(self_rc: &Rc<Self>, uuid: &str) {
        if uuid.is_empty() { return; }
        dbg_state(&format!("forget device: {uuid}"));

        let win = self_rc.registry.borrow().iter()
            .find(|w| w.uuid().as_deref() == Some(uuid))
            .map(|w| w.window.clone());
        if let Some(win) = win {
            win.close();
        } else {
            Self::close_settings_window_for(self_rc, uuid);
        }

        if let Some(kw) = self_rc.kiosk_win.borrow().as_ref() {
            if kw.current_key() == uuid {
                kw.bind_device(None);
            }
        }

        self_rc.disc_mgr.forget(uuid);
        config::update(|cfg| { cfg.devices.remove(uuid); });
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
        // and resolve exactly one device straight at the given address, then
        // open a window on it (nothing without a resolved uuid may ever be
        // tracked — see `device::discovery::ProbeFailure`'s doc comment). The
        // flag already names the scheme, so this probes that one TLS mode
        // directly rather than walking `PROBE_MODES` the way a plain
        // manual-add-by-IP does.
        //
        // `--connect --kiosk` together hands the resolved `DeviceState` to
        // Kiosk mode via `enter_kiosk_with_device()` instead of
        // `open_device_spec()`, which is what lets the two flags combine:
        // Kiosk mode pre-bound to the `--connect` target rather than a plain
        // `DeviceWindow`.
        if let Some((ip, tls_mode)) = DIRECT_CONNECT.get() {
            let start_in_kiosk = START_IN_KIOSK.load(Ordering::Relaxed);
            dbg_state(&format!(
                "activate: --connect direct to {ip} via {tls_mode:?}{}",
                if start_in_kiosk { " (--kiosk)" } else { "" }
            ));
            let self_rc  = Rc::clone(self_rc);
            let ip       = ip.clone();
            let tls_mode = *tls_mode;
            let rt = self_rc.device_manager.rt();
            let (tx, rx) = async_channel::bounded::<Result<crate::device::discovery::DiscoveredDevice, crate::device::discovery::ProbeFailure>>(1);
            let probe_ip = ip.clone();
            rt.spawn(async move {
                let result = DiscoveryService::probe_known_scheme(&probe_ip, tls_mode).await;
                let _ = tx.send(result).await;
            });
            glib::spawn_future_local(async move {
                match rx.recv().await {
                    Ok(Ok(dev)) => {
                        if start_in_kiosk {
                            let ds = self_rc.device_manager.create_and_configure(&dev.uuid, &dev.ip, dev.tls_mode);
                            Self::enter_kiosk_with_device(&self_rc, ds, dev.name);
                        } else {
                            Self::open_device_spec(&self_rc, DeviceSpec {
                                ip:          dev.ip,
                                uuid:        dev.uuid,
                                tls_mode:    dev.tls_mode,
                                try_connect: true,
                            });
                        }
                    }
                    // A testing flag aimed at one known target: report why
                    // and exit non-zero rather than leaving a window sitting
                    // at "Connecting…" forever.
                    Ok(Err(failure)) => {
                        eprintln!("{}: {}", ip, failure.describe(&ip));
                        std::process::exit(1);
                    }
                    // Sender dropped — nothing left to report to.
                    Err(_) => {}
                }
            });
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
        // happen before `start()`, which eagerly tracks every entry in it
        // that already has an address (known-by-default — see
        // `DiscoveryManager`'s own doc comment).
        let seed: Vec<SeedEntry> = config::with(|cfg| {
            cfg.devices.iter().map(|(uuid, d)| SeedEntry {
                uuid:        uuid.clone(),
                name:        d.name.clone(),
                model:       d.model.clone(),
                project:     d.project.clone(),
                firmware:    d.firmware.clone(),
                last_ip:     d.last_ip.clone(),
                tls_mode:    d.tls_mode.map(|n| TlsMode::from_usize(n as usize)).unwrap_or(TlsMode::HttpsWiiM),
                window_open: d.window_open,
            }).collect()
        });
        let devlist_song_info = config::with(|cfg| cfg.devlist_song_info);
        self_rc.disc_mgr.load_seed(seed, devlist_song_info);

        // `disc_mgr` can't persist to config itself either — this is the
        // "report out" half of the same rule. Fires unconditionally on
        // every `list-changed` (identity update, presence flip, ...)
        // rather than being selectively triggered — cheap and safe since
        // `config::update()` already diffs the whole `Config` before
        // deciding whether to actually write to disk. Deleting a config
        // entry outright is `forget_device()`'s job, not this — this loop
        // only ever updates fields for a still-tracked device.
        self_rc.disc_mgr.connect_list_changed(|mgr| {
            let entries = mgr.entries();
            config::update(|cfg| {
                for e in &entries {
                    if e.uuid.is_empty() { continue; }
                    let dev = cfg.device_mut(&e.uuid);
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
        // nothing even for an already-known kiosk_last_uuid device seeded
        // with no `last_ip` yet). The first time a device
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
        // Same shared, plain non-modal open_preferences_fn/open_device_settings_fn
        // every other window uses (DeviceWindow/DiscoveryWindow).
        let open_preferences_fn = {
            let state = Rc::clone(self_rc);
            Rc::new(move || Self::open_preferences(&state)) as Rc<dyn Fn()>
        };
        let open_device_settings_fn = {
            let state = Rc::clone(self_rc);
            Rc::new(move |ds| Self::open_device_settings(&state, ds)) as Rc<dyn Fn(DeviceState)>
        };
        let forget_device_fn = {
            let state = Rc::clone(self_rc);
            Rc::new(move |uuid: &str| Self::forget_device(&state, uuid)) as Rc<dyn Fn(&str)>
        };
        let initial_layout = match KIOSK_LAYOUT_OVERRIDE.get() {
            Some(KioskLayoutOverride::Classic) => views::playback_full::PlaybackLayout::Classic,
            Some(KioskLayoutOverride::WideRight) | None => views::playback_full::PlaybackLayout::WideRight,
        };
        let kw = kiosk::KioskWindow::new(
            &self_rc.app, &self_rc.disc_mgr, &icons, exit_fn,
            open_preferences_fn, open_device_settings_fn, forget_device_fn,
            initial_layout, kiosk_only(),
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
    ///
    /// Both of those steps wait for the Kiosk window to finish *leaving*
    /// fullscreen first. On macOS a fullscreen window owns its own Space,
    /// and collapsing one is an animated, asynchronous transition:
    /// destroying the window mid-transition leaves the macOS GDK backend
    /// delivering `windowDidExitFullScreen:` callbacks to an
    /// already-finalized surface (a burst of `GDK_IS_MACOS_SURFACE` /
    /// `frame_clock` assertion failures). Three things go wrong as a
    /// result: the Space-collapse configure event is lost, so restored
    /// windows keep the fullscreen geometry and land partly offscreen;
    /// windows created while the Space is still up are born into it at its
    /// size; and the half-destroyed window can stay registered with the
    /// `GtkApplication`, whose per-window use count then keeps the process
    /// alive after every visible window is gone. Waiting for the Space to
    /// actually collapse avoids all three, and costs nothing on backends
    /// that leave fullscreen synchronously.
    pub(crate) fn exit_kiosk(self_rc: &Rc<Self>) {
        let Some(kw) = self_rc.kiosk_win.borrow().as_ref().map(Rc::clone) else { return };

        // The exit button and "K" both stay live until the window is
        // actually gone, so a second request can arrive mid-wait.
        if self_rc.kiosk_exiting.get() { return; }
        self_rc.kiosk_exiting.set(true);

        let weak = Rc::downgrade(self_rc);
        kw.unfullscreen_then(move || {
            let Some(self_rc) = weak.upgrade() else { return };

            Self::restore_windows_from_config(&self_rc);

            // Taken out of the RefCell before the `if let`, so the borrow
            // guard isn't still alive across `close()`.
            let kw = self_rc.kiosk_win.borrow_mut().take();
            if let Some(kw) = kw {
                kw.close();
            }

            // Cleared last, not on entry to this callback: until the window
            // is actually taken out of `kiosk_win`, a re-entrant exit would
            // still find one and restore every window a second time.
            self_rc.kiosk_exiting.set(false);
        });
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

