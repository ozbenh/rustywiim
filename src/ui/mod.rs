#![allow(deprecated)] // glib clone! old-style @strong syntax

mod art_background;
pub mod devlist;
mod flip_cover;
mod icons;
pub(crate) mod menu;
mod scroll_fade_label;
mod device_window;
mod theme;
pub(crate) mod settings;
mod views;

use device_window::DeviceWindow;
pub(crate) use theme::{apply_accent_color, apply_theme, update_art_background_visibility};
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
        println!("[ui] {msg}");
    }
}

/// Set just before the quit action starts closing windows, so the
/// close-request/destroy handlers it triggers (DeviceWindowInner::cleanup())
/// know this isn't a user-initiated close. A window closed because the app
/// is quitting should still be reopened on next launch; a window the user
/// explicitly closed should not.
static QUITTING: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

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

// ── AppState ──────────────────────────────────────────────────────────────────
// Owns all top-level window state.  Every signal-handler closure captures
// either a strong Rc<AppState> or a Weak clone for the close-request handlers.

fn dbg_state(msg: &str) {
    if DEBUG_STATE.load(Ordering::Relaxed) {
        println!("[app] {msg}");
    }
}

pub(crate) struct AppState {
    app:            adw::Application,
    disc_mgr:       DiscoveryManager,
    device_manager: DeviceManager,
    registry:       RefCell<Vec<DeviceWindow>>,
    settings_reg:   RefCell<Vec<settings::SettingsWindow>>,
    disc_win:       RefCell<Option<devlist::DiscoveryWindow>>,
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
            let (access_override, mute_access_override) = config::with(|cfg| {
                let d = cfg.device(&uuid);
                (d.playback_access_override, d.mute_access_override)
            });
            dbg_state(&format!(
                "configure-device: {} ({uuid}) access_override={access_override:?} mute_access_override={mute_access_override:?}",
                ds.ip(),
            ));
            ds.set_playback_access_override(access_override);
            ds.set_mute_access_override(mute_access_override);
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
        let s = settings::SettingsWindow::new(ds, &self_rc.disc_mgr);
        let win_clone  = s.window_ref().clone();
        let weak_self  = Rc::downgrade(self_rc);
        let close_uuid = ds_uuid.clone();
        s.window_ref().connect_close_request(move |win| {
            dbg_state(&format!("settings: closed for {:?}", close_uuid));
            if let Some(state) = weak_self.upgrade() {
                state.settings_reg.borrow_mut().retain(|w| w.window_ref() != &win_clone);
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
            *dw = Some(devlist::DiscoveryWindow::new(
                &self_rc.app,
                &self_rc.disc_mgr,
                open_device_fn,
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

    /// Create a device window for `spec`, register it, and present it.
    fn open_device_spec(self_rc: &Rc<Self>, spec: DeviceSpec) {
        let log_uuid = spec.uuid.clone();
        let log_ip   = spec.ip.clone();
        dbg_state(&format!("device window: creating uuid={log_uuid} @ {log_ip}"));
        let show_fn = {
            let state = Rc::clone(self_rc);
            Rc::new(move || Self::show_devices(&state)) as Rc<dyn Fn()>
        };
        let open_settings_fn = {
            let state = Rc::clone(self_rc);
            Rc::new(move |ds| Self::open_settings(&state, ds)) as Rc<dyn Fn(Option<DeviceState>)>
        };
        let dw = DeviceWindow::new_for_device(
            &self_rc.app,
            self_rc.device_manager.clone(),
            show_fn,
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
            init_css(theme);
            init_icon_resource();
        }

        // Replace the app.quit action (set up in main.rs) with one that explicitly
        // destroys every device window first so connect_destroy fires (saving
        // config, cancelling timers). win.close() is a no-op on unrealized
        // windows (e.g. a window never shown when starting in mini mode), and app.quit()
        // on its own destroys windows after the main loop exits where cleanup is unreliable.
        {
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

        // `--connect` override: skip discovery/config-restored windows entirely
        // and open exactly one device window straight at the given address.
        // uuid is empty (unresolved until getStatusEx) — DeviceManager::get()
        // and DeviceWindow::new_inner() already handle that case (a brand new,
        // not-yet-deduplicated DeviceState), same as for a manually-added device.
        if let Some((ip, tls_mode)) = DIRECT_CONNECT.get() {
            dbg_state(&format!("activate: --connect direct to {ip} via {tls_mode:?}"));
            Self::open_device_spec(self_rc, DeviceSpec {
                ip: ip.clone(),
                uuid: String::new(),
                tls_mode: *tls_mode,
                try_connect: true,
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
        if discovery_open || !has_pending_windows {
            dbg_state("activate: showing device list");
            Self::show_devices(self_rc);
        }

        // Restore windows from config on startup.  initial-load fires once,
        // synchronously inside start(), so open_device() here is safe — no
        // risk of raising already-open windows on subsequent list changes.
        {
            let s = Rc::downgrade(self_rc);
            self_rc.disc_mgr.connect_initial_load(move |mgr| {
                let Some(self_rc) = s.upgrade() else { return };
                let entries = mgr.entries();
                let to_open: Vec<_> = config::with(|cfg| {
                    entries.into_iter()
                        .filter(|entry| !entry.uuid.is_empty()
                            && cfg.devices.get(&entry.uuid).map_or(false, |d| d.window_open))
                        .collect()
                });
                for entry in &to_open {
                    Self::open_device(&self_rc, entry);
                }
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
    }
}

