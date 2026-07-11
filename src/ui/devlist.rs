#![allow(deprecated)] // glib clone! @strong syntax

/// Device-list window and its backing DiscoveryManager.
///
/// `DiscoveryManager` (GObject) subscribes to `DiscoveryService` and tracks
/// every known device (SSDP-discovered, pinned-and-config-remembered, or
/// manually added by IP) as a real, `device::manager::DeviceManager`-owned
/// `DeviceState` — holding a strong reference to it for as long as the
/// device is "known" (see `do_prune()`), which is what keeps it alive and
/// polling (Simple mode, unless a device window also holds an
/// `acquire_full()` guard) even with no window open. There is no separate
/// health-check poll: Simple-mode polling's own `getStatusEx` *is* the
/// liveness check now, and presence for rendering
/// (`DevicePresence::compute()`) is read straight off each tracked
/// `DeviceState::connection_state()`, not tracked independently. Recovery
/// after a failure is `DeviceState`'s own job (`maybe_self_reconnect()`,
/// `device/state.rs`) — nothing external pokes it anymore.
///
/// `DiscoveryWindow` renders that list and lets the user pin/unpin entries,
/// open device windows by double-clicking, and add devices manually by IP.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::Arc;

use adw::prelude::*;
use glib::clone;
use glib::subclass::prelude::*;
use gtk::{glib, Orientation};

use crate::config;
use crate::device::api::TlsMode;
use crate::device::capabilities;
use crate::device::discovery::{DEBUG_DISCOVERY, DiscoveredDevice, DiscoveryService};
use crate::device::manager::DeviceManager;
use crate::device::state::{ConnectionState, DeviceState};
use crate::ui::icons::IconSet;
use super::flip_cover::FlipCover;
use super::playback::vol_icon;
use super::scroll_fade_label::ScrollFadeLabel;

/// `[disc-mgr]` — `DiscoveryManager`, the picker-list backend (this file's
/// `impl DiscoveryManager` block). Distinct from `device/discovery.rs`'s
/// `[discovery]` (the SSDP service itself) and from `DiscoveryWindow` (the
/// actual on-screen picker), which has no debug logging of its own.
fn dbg(msg: &str) {
    if DEBUG_DISCOVERY.load(std::sync::atomic::Ordering::Relaxed) {
        println!("[disc-mgr] {msg}");
    }
}

// ── Public types ──────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DevicePresence {
    Active, // ConnectionState::Connected
    Ghost,  // pinned, not Connected
    Dead,   // not pinned, not Connected
}

impl DevicePresence {
    fn compute(state: ConnectionState, pinned: bool) -> Self {
        if state == ConnectionState::Connected {
            Self::Active
        } else if pinned {
            Self::Ghost
        } else {
            Self::Dead
        }
    }
}

#[derive(Debug, Clone)]
pub struct ManagedEntry {
    pub uuid:     String,
    pub name:     String,
    pub model:    String,
    /// Internal `project`/`firmware` strings from `getStatusEx` — a
    /// different namespace from `model` (the marketing name), needed to
    /// resolve the device's profile default while offline (see
    /// `config::DeviceConfig::project`'s doc comment). Empty until the
    /// tracked `DeviceState` has connected at least once.
    pub project:  String,
    pub firmware: String,
    pub ip:       String,
    pub tls_mode: TlsMode,
    pub pinned:   bool,
    pub presence: DevicePresence,
    /// Mirrors `devlist_song_info` at the moment `entries()` was called —
    /// separate from `now_playing` below so a row can reserve its
    /// artwork/icon slot (fixed size, so the row's right-hand side never
    /// shifts as devices update) even when this particular device has
    /// nothing to show there yet (not `Active`, e.g.).
    pub song_info_enabled: bool,
    /// Live now-playing snapshot for row rendering — unlike the identity
    /// fields above (cached on `DeviceRecord.entry`, refreshed only on
    /// `device-changed`), this is computed fresh every `entries()` call
    /// straight from the tracked `DeviceState::playback_state()`, since
    /// title/artist change far more often than identity does. `None`
    /// unless `devlist_song_info` is on and the device is `Active` — *not*
    /// further gated on actually having a track loaded, so an idle-but-
    /// connected device still gets its input/mode icon rather than nothing.
    pub now_playing: Option<NowPlaying>,
}

#[derive(Debug, Clone)]
pub struct NowPlaying {
    pub title:    String,
    pub artist:   String,
    pub artwork:  Option<Rc<Vec<u8>>>,
    /// Doubles as `FlipCover::set_art()`'s de-dupe key (same as
    /// `ui/playback.rs`'s `update_artwork()` uses for the main window) —
    /// `apply_now_playing()` must never use a constant-per-device value
    /// (e.g. uuid) for that key, or every update after the first becomes a
    /// silent no-op once the row's `FlipCover` is a persistent widget
    /// (`RowWidgets`) rather than rebuilt fresh each time.
    pub art_url:  Option<String>,
    /// Icon key for the row's fallback icon when `artwork` is `None` —
    /// same `IconSet::source_paintable()` lookup key the main window's
    /// `update_artwork()` uses for its own no-art fallback, computed the
    /// same way (`capabilities::mode_to_input_source()` +
    /// `icon_canon_for_input()`) so a device's picker row shows the same
    /// icon its own window would.
    pub icon_key: String,
}

// ── Internal record ───────────────────────────────────────────────────────────

/// One tracked device: cached rendering identity (refreshed from
/// `ds.device_info()`/`ds.capabilities()` whenever `ds` connects — see
/// `refresh_identity_from_device()`) plus the strong `DeviceState` handle
/// that keeps it alive/polling for as long as this record exists. `ds` is
/// what makes "forgetting a device" plain refcounting: dropping this
/// record (`do_prune()`) drops the last reference `ui::devlist` holds, and
/// once no device window holds one either, the `DeviceState` itself goes
/// away.
struct DeviceRecord {
    entry: ManagedEntry,
    /// Whether this uuid/ip is currently visible in the live SSDP scan —
    /// exempts an otherwise-prunable entry, same as `has_open_window`
    /// below (see `do_prune()`).
    in_discovery: bool,
    /// Whether a device window is currently open for this uuid — set via
    /// `set_window_open()`, called by `ui::mod`'s `AppState` whenever a
    /// device window opens/closes. Exempts the entry from `do_prune()`: a
    /// device the user has an open, now-"Disconnected" window for
    /// shouldn't vanish from the picker list out from under them just
    /// because it's unpinned and offline.
    has_open_window: bool,
    /// The `device-changed` handler connected in `create_and_track()` is
    /// never explicitly disconnected — no `SignalHandlerId` kept for it —
    /// because `do_prune()` only ever drops a record while
    /// `has_open_window` is false, and `device::manager::DeviceManager`
    /// itself only ever keeps *weak* refs, so `ds` below is guaranteed to
    /// have no other strong holder at that point: dropping this record
    /// finalizes `ds` outright, connection included.
    ds: DeviceState,
}

struct Inner {
    devices: HashMap<String, DeviceRecord>,
    /// Mirrors `config::Config::devlist_song_info` — cached here rather
    /// than re-read from config on every device creation, refreshed once
    /// in `start()` and again on every `set_song_info()` call. See
    /// `set_song_info()`'s doc comment for the full fan-out story.
    song_info: bool,
}

impl Default for Inner {
    fn default() -> Self { Self { devices: HashMap::new(), song_info: false } }
}

// ── DiscoveryManager GObject ──────────────────────────────────────────────────

mod mgr_imp {
    use super::*;
    use glib::subclass::Signal;
    use std::sync::OnceLock;

    pub struct DiscoveryManager {
        pub(super) rt:             std::cell::OnceCell<Arc<tokio::runtime::Runtime>>,
        pub(super) discovery:      std::cell::OnceCell<DiscoveryService>,
        pub(super) device_manager: std::cell::OnceCell<DeviceManager>,
        pub(super) inner:          RefCell<Inner>,
    }

    impl Default for DiscoveryManager {
        fn default() -> Self {
            Self {
                rt:             std::cell::OnceCell::new(),
                discovery:      std::cell::OnceCell::new(),
                device_manager: std::cell::OnceCell::new(),
                inner:          RefCell::new(Inner::default()),
            }
        }
    }

    #[glib::object_subclass]
    impl ObjectSubclass for DiscoveryManager {
        const NAME: &'static str = "RustyWiimDiscoveryManager";
        type Type = super::DiscoveryManager;
    }

    impl ObjectImpl for DiscoveryManager {
        fn signals() -> &'static [Signal] {
            static SIGNALS: OnceLock<Vec<Signal>> = OnceLock::new();
            SIGNALS.get_or_init(|| vec![
                // Structural changes only: device added/removed/renamed/
                // pinned/moved, presence flips. Rebuilding every row is
                // acceptable here — these are comparatively rare.
                Signal::builder("list-changed").build(),
                // Fired once, synchronously in start(), after the initial config
                // load — before any async discovery results arrive.
                Signal::builder("initial-load").build(),
                // A single tracked device's now-playing content (title/
                // artist/artwork) or volume/mute changed — deliberately
                // *not* folded into `list-changed`. That would rebuild
                // every row's widgets from scratch on every track/volume
                // change (this fires far more often than anything
                // structural), which is both wasteful
                // and defeats FlipCover's flip-vs-fade logic: a freshly
                // reconstructed FlipCover never has "previous real art" on
                // the same widget instance to flip from. Param: the
                // tracked device's key (`device_key()`'s result — same
                // string `entries()`'s rows/`current_entries` are indexed
                // by), so a listener can update just that one row in place.
                Signal::builder("song-info-changed")
                    .param_types([String::static_type()])
                    .build(),
            ])
        }
    }
}

glib::wrapper! {
    pub struct DiscoveryManager(ObjectSubclass<mgr_imp::DiscoveryManager>);
}

impl DiscoveryManager {
    /// `device_manager` is a direct reference (not a hook/callback) —
    /// deliberate, now that this module owns the full picker-list
    /// backend rather than bridging to a separate one: `device/`'s
    /// `DeviceManager` is the registry every tracked `DeviceState` comes
    /// from, and there's no ownership-layering reason left to hide that
    /// behind indirection.
    pub fn new(rt: Arc<tokio::runtime::Runtime>, discovery: DiscoveryService, device_manager: DeviceManager) -> Self {
        let obj: Self = glib::Object::new();
        obj.imp().rt.set(rt).unwrap();
        obj.imp().discovery.set(discovery).unwrap();
        obj.imp().device_manager.set(device_manager).unwrap();
        obj
    }

    fn device_manager(&self) -> &DeviceManager {
        self.imp().device_manager.get().unwrap()
    }

    pub fn start(&self) {
        self.imp().inner.borrow_mut().song_info = config::with(|cfg| cfg.devlist_song_info);
        self.load_known_devices_from_config();
        // initial-load fires once synchronously so AppState can open any windows
        // that config says should be restored — before async discovery arrives.
        self.emit_by_name::<()>("initial-load", &[]);
        // list-changed lets already-connected handlers (e.g. last_ip tracking)
        // see the initial device set.
        self.emit_list_changed();

        let weak = self.downgrade();
        self.imp().discovery.get().unwrap()
            .connect_discovery_updated(move |svc| {
                let Some(mgr) = weak.upgrade() else { return };
                mgr.on_discovery_updated(svc);
            });
    }

    pub fn entries(&self) -> Vec<ManagedEntry> {
        let inner = self.imp().inner.borrow();
        let song_info = inner.song_info;
        let mut v: Vec<ManagedEntry> = inner.devices.values()
            .map(|r| build_managed_entry(r, song_info))
            .collect();
        v.sort_by(|a, b| a.name.cmp(&b.name));
        v
    }

    /// Single-entry counterpart to `entries()` — used by the
    /// `song-info-changed` handler to refresh just one row's content
    /// without recomputing (or rebuilding widgets for) every other tracked
    /// device. `key` is `device_key()`'s result, same as `entries()`'s
    /// rows are implicitly keyed by for row-widget lookup purposes.
    pub fn entry_for(&self, key: &str) -> Option<ManagedEntry> {
        let inner = self.imp().inner.borrow();
        let song_info = inner.song_info;
        inner.devices.get(key).map(|r| build_managed_entry(r, song_info))
    }

    /// The tracked `DeviceState` for `key` — cheap to clone (GObject
    /// refcount). Used for the picker row's volume/mute control, which
    /// talks to the device directly rather than going through `ManagedEntry`
    /// (volume isn't part of the rendered snapshot anywhere else).
    pub fn device_state_for(&self, key: &str) -> Option<DeviceState> {
        self.imp().inner.borrow().devices.get(key).map(|r| r.ds.clone())
    }

    /// The one place `list-changed` actually fires — dumps the full
    /// tracked-device table under `--debug=discovery` first, so testing
    /// doesn't have to piece the current state back together from
    /// scattered one-line event logs.
    fn emit_list_changed(&self) {
        if DEBUG_DISCOVERY.load(std::sync::atomic::Ordering::Relaxed) {
            self.dump_devices();
        }
        self.emit_by_name::<()>("list-changed", &[]);
    }

    /// Fired from `create_and_track()`'s `playback-changed` handler
    /// instead of `emit_list_changed()` — see `song-info-changed`'s own
    /// doc comment (`signals()`) for why the two are kept separate.
    fn emit_song_info_changed(&self, key: &str) {
        dbg(&format!("song info changed: {key}"));
        self.emit_by_name::<()>("song-info-changed", &[&key.to_string()]);
    }

    fn dump_devices(&self) {
        let inner = self.imp().inner.borrow();
        let mut recs: Vec<_> = inner.devices.iter().collect();
        recs.sort_by(|a, b| a.1.entry.name.cmp(&b.1.entry.name));
        dbg(&format!("── device list: {} tracked ──", recs.len()));
        if recs.is_empty() { dbg("  (none)"); }
        for (key, rec) in recs {
            let mut flags = Vec::new();
            if rec.entry.pinned    { flags.push("pinned"); }
            if rec.in_discovery    { flags.push("in-discovery"); }
            if rec.has_open_window { flags.push("window-open"); }
            let flags_str = if flags.is_empty() { String::new() } else { format!(" [{}]", flags.join(",")) };
            let presence = format!("{:?}", rec.entry.presence);
            dbg(&format!(
                "  {:<24} {:<17} {presence:<8}{flags_str} key={key:?}",
                rec.entry.name, rec.entry.ip,
            ));
        }
    }

    pub fn set_pinned(&self, uuid: &str, pinned: bool) {
        let changed = {
            let mut inner = self.imp().inner.borrow_mut();
            if let Some(rec) = inner.devices.get_mut(uuid) {
                let was = rec.entry.pinned;
                rec.entry.pinned = pinned;
                rec.entry.presence = DevicePresence::compute(rec.ds.connection_state(), pinned);
                was != pinned
            } else { false }
        };
        if changed {
            dbg(&format!("pin: {uuid} → {pinned}"));
            self.persist_pinned();
            self.do_prune();
            self.emit_list_changed();
        }
    }

    /// Toggles whether every tracked device additionally fetches title/
    /// artist/artwork (`config::Config::devlist_song_info`, surfaced as the
    /// "Song info in device list" switch in Settings' General page).
    /// Persists immediately, updates the cached `Inner.song_info` new
    /// devices read at creation (`create_and_track()`), and pushes the new
    /// value onto every currently-tracked `DeviceState` right away — so
    /// toggling takes effect immediately, not just for devices tracked
    /// afterward. No effect on a device already in `Full` mode (an open
    /// window already fetches this content regardless — see
    /// `DeviceState::configure_simple_mode()`'s doc comment).
    pub fn set_song_info(&self, want: bool) {
        {
            let inner = self.imp().inner.borrow_mut();
            for rec in inner.devices.values() {
                rec.ds.configure_simple_mode(want);
            }
        }
        self.imp().inner.borrow_mut().song_info = want;
        config::update(|cfg| cfg.devlist_song_info = want);
        self.emit_list_changed();
    }

    /// Records whether a device window is currently open for `uuid` — see
    /// `DeviceRecord::has_open_window`'s doc comment. Called by `ui::mod`'s
    /// `AppState` on window open/close. No-op if `uuid` is empty or
    /// unknown to devlist (a window with nothing here to mark — e.g. a
    /// first-ever manual connect whose uuid isn't resolved yet).
    pub fn set_window_open(&self, uuid: &str, open: bool) {
        if uuid.is_empty() { return; }
        let mut inner = self.imp().inner.borrow_mut();
        if let Some(rec) = inner.devices.get_mut(uuid) {
            rec.has_open_window = open;
        }
    }

    /// Add a manually-discovered device (already confirmed alive by the caller).
    pub fn add_manual(&self, name: String, ip: String, uuid: String, tls_mode: TlsMode) {
        let key = device_key(&uuid, &ip);
        if self.imp().inner.borrow().devices.contains_key(&key) {
            dbg(&format!("add manual: already known {name} ({ip}) uuid={uuid:?}"));
            return;
        }
        dbg(&format!("add manual: {name} ({ip}) uuid={uuid:?}"));
        self.track_device(&key, &uuid, &ip, tls_mode, true, name, String::new(), String::new(), String::new(), false);
        self.persist_pinned();
        self.emit_list_changed();
    }

    pub fn connect_list_changed<F: Fn(&Self) + 'static>(&self, f: F) -> glib::SignalHandlerId {
        self.connect_local("list-changed", false, move |args| {
            f(&args[0].get::<Self>().unwrap());
            None
        })
    }

    /// Fired whenever a single tracked device's now-playing content
    /// changes — see `song-info-changed`'s doc comment (`signals()`) for
    /// why this is separate from `list-changed`. The callback's `&str` is
    /// the device's key (`device_key()`'s result); use `entry_for(key)` to
    /// get its fresh content.
    pub fn connect_song_info_changed<F: Fn(&Self, &str) + 'static>(&self, f: F) -> glib::SignalHandlerId {
        self.connect_local("song-info-changed", false, move |args| {
            let obj = args[0].get::<Self>().unwrap();
            let key = args[1].get::<String>().unwrap();
            f(&obj, &key);
            None
        })
    }

    /// Fired once, synchronously inside `start()`, after loading devices from
    /// config — before any async discovery results arrive.
    /// Use this to restore windows from config; do NOT use `list-changed` for
    /// that, as it fires on every subsequent change (e.g. pin toggles).
    pub fn connect_initial_load<F: Fn(&Self) + 'static>(&self, f: F) -> glib::SignalHandlerId {
        self.connect_local("initial-load", false, move |args| {
            f(&args[0].get::<Self>().unwrap());
            None
        })
    }

    /// Fires once when the underlying SSDP scan cycle completes (or the 4-second
    /// initial timeout expires with no devices found).  Use this — not
    /// `connect_list_changed` — to clear a "Scanning…" indicator, because
    /// devices already tracked from config would clear it prematurely.
    pub fn connect_scan_complete<F: Fn() + 'static>(&self, f: F) {
        let weak = self.downgrade();
        self.imp().discovery.get().unwrap()
            .connect_discovery_updated(move |_| {
                if weak.upgrade().is_some() { f(); }
            });
    }

    pub fn rt(&self) -> Arc<tokio::runtime::Runtime> {
        self.imp().rt.get().unwrap().clone()
    }

    /// Creates (if not already tracked) or updates (`ip`/`tls_mode`, if
    /// moved) a `DeviceRecord` — the one path `on_discovery_updated()`/
    /// `load_known_devices_from_config()`/`add_manual()` all funnel
    /// through, so a record is always built/refreshed the same way
    /// regardless of what triggered it. `name`/`model`/`project`/
    /// `firmware` seed the entry's *rendering* fields for a record that
    /// doesn't exist yet (config-cached values, or whatever the SSDP/
    /// manual-add probe already had) — ignored if the record already
    /// exists, since `refresh_identity_from_device()` (wired via
    /// `device-changed`) is the one place identity fields get overwritten
    /// once `ds` has actually answered for real.
    #[allow(clippy::too_many_arguments)]
    fn track_device(
        &self,
        key: &str, uuid: &str, ip: &str, tls: TlsMode, pinned: bool,
        name: String, model: String, project: String, firmware: String,
        in_discovery: bool,
    ) {
        let moved = {
            let mut inner = self.imp().inner.borrow_mut();
            let Some(rec) = inner.devices.get_mut(key) else {
                drop(inner);
                self.create_and_track(key, uuid, ip, tls, pinned, name, model, project, firmware, in_discovery);
                return;
            };
            let moved = rec.entry.ip != ip || rec.entry.tls_mode != tls;
            if moved {
                dbg(&format!("track_device: {} moved {} → {ip}", rec.entry.name, rec.entry.ip));
                rec.entry.ip = ip.to_string();
                rec.entry.tls_mode = tls;
                // Covers any live DeviceState for this uuid, not just this
                // devlist entry — e.g. an already-open device window
                // reconnects to the corrected IP too.
                self.device_manager().update_ip(uuid, ip, tls);
            }
            rec.in_discovery = rec.in_discovery || in_discovery;
            moved
        };
        if moved { self.persist_pinned(); }
    }

    /// The actual creation half of `track_device()`, split out only so its
    /// `inner` borrow (above) can drop cleanly before this runs —
    /// `create_and_configure()` can re-enter this same `DiscoveryManager`
    /// synchronously via the `configure-device` signal's connected handler
    /// (`ui::AppState`'s, which doesn't touch devlist — but `device-changed`
    /// firing on the very first poll tick, before `create_and_configure()`
    /// even returns, is close enough to a real risk to just not hold the
    /// borrow across the call at all).
    #[allow(clippy::too_many_arguments)]
    fn create_and_track(
        &self,
        key: &str, uuid: &str, ip: &str, tls: TlsMode, pinned: bool,
        name: String, model: String, project: String, firmware: String,
        in_discovery: bool,
    ) {
        dbg(&format!("track_device: new {name} ({ip}) uuid={uuid:?} key={key:?}"));
        let ds = self.device_manager().create_and_configure(uuid, ip, tls);
        ds.configure_simple_mode(self.imp().inner.borrow().song_info);
        let entry = ManagedEntry {
            uuid: uuid.to_string(), name, model, project, firmware,
            ip: ip.to_string(), tls_mode: tls, pinned,
            presence: DevicePresence::compute(ds.connection_state(), pinned),
            song_info_enabled: self.imp().inner.borrow().song_info,
            now_playing: None,
        };
        let weak = self.downgrade();
        let key_owned = key.to_string();
        ds.connect_device_changed(move |ds| {
            let Some(mgr) = weak.upgrade() else { return };
            mgr.on_tracked_device_changed(&key_owned, ds);
        });
        // Updates just this row's content on an actual now-playing or
        // volume/mute change (not every poll tick — filtered to the
        // TITLE/ARTIST/ARTWORK/VOLUME bits) via the dedicated
        // `song-info-changed` signal, *not* `emit_list_changed()` — this
        // fires far more often than anything structural, and rebuilding
        // every row's widgets on every track/volume change is both
        // wasteful and defeats FlipCover's flip transition (see
        // `song-info-changed`'s doc comment in `signals()`). No-op,
        // cheaply, when `song_info` is off.
        let weak2 = self.downgrade();
        let key_for_song_info = key.to_string();
        ds.connect_playback_changed(move |_, mask| {
            if mask & (crate::device::state::playback_changed::TITLE
                | crate::device::state::playback_changed::ARTIST
                | crate::device::state::playback_changed::ARTWORK
                | crate::device::state::playback_changed::VOLUME) == 0
            {
                return;
            }
            let Some(mgr) = weak2.upgrade() else { return };
            if mgr.imp().inner.borrow().song_info {
                mgr.emit_song_info_changed(&key_for_song_info);
            }
        });
        self.imp().inner.borrow_mut().devices.insert(key.to_string(), DeviceRecord {
            entry, in_discovery, has_open_window: false, ds,
        });
    }

    /// Fired whenever a tracked device's `DeviceState` emits
    /// `device-changed` — i.e. it just connected, just failed, or its
    /// identity was otherwise confirmed/updated. Refreshes this record's
    /// rendering fields from the live `DeviceState` (never a redundant
    /// separate probe — `ds` already did the work), re-prunes (a
    /// transition can make an entry newly prunable, e.g. it just went
    /// offline and isn't pinned/open/in-discovery), persists if identity
    /// actually changed, and always re-renders.
    fn on_tracked_device_changed(&self, key: &str, ds: &DeviceState) {
        let mut needs_persist = false;
        {
            let mut inner = self.imp().inner.borrow_mut();
            let Some(rec) = inner.devices.get_mut(key) else {
                dbg(&format!("device-changed: {key} no longer tracked, ignoring"));
                return;
            };
            let new_presence = DevicePresence::compute(ds.connection_state(), rec.entry.pinned);
            if rec.entry.presence != new_presence {
                dbg(&format!("device-changed: {} {:?} → {new_presence:?}", rec.entry.name, rec.entry.presence));
                rec.entry.presence = new_presence;
            }
            if let Some(info) = ds.device_info() {
                if !info.device_name.is_empty() && rec.entry.name != info.device_name {
                    rec.entry.name = info.device_name.clone();
                    needs_persist = true;
                }
                if !info.project.is_empty() && rec.entry.project != info.project {
                    rec.entry.project = info.project.clone();
                    needs_persist = true;
                }
                if !info.firmware.is_empty() && rec.entry.firmware != info.firmware {
                    rec.entry.firmware = info.firmware.clone();
                    needs_persist = true;
                }
            }
            if let Some(caps) = ds.capabilities() {
                if !caps.model.is_empty() && rec.entry.model != caps.model {
                    rec.entry.model = caps.model.clone();
                    needs_persist = true;
                }
            }
        }
        self.do_prune();
        if needs_persist { self.persist_pinned(); }
        self.emit_list_changed();
    }

    fn on_discovery_updated(&self, svc: &DiscoveryService) {
        let discovered = svc.devices();
        let disc_keys: std::collections::HashSet<String> = discovered.iter()
            .map(|d| device_key(&d.uuid, &d.ip))
            .collect();

        {
            let mut inner = self.imp().inner.borrow_mut();
            for (key, rec) in inner.devices.iter_mut() {
                let was = rec.in_discovery;
                rec.in_discovery = disc_keys.contains(key);
                if was && !rec.in_discovery {
                    dbg(&format!("discovery: {} ({}) left SSDP scope", rec.entry.name, rec.entry.ip));
                }
            }
        }

        let known_devices = config::with(|c| c.devices.clone());
        for dev in &discovered {
            let key = device_key(&dev.uuid, &dev.ip);
            let cached = known_devices.get(&dev.uuid);
            let name  = cached.and_then(|c| c.name.clone())
                .filter(|n| !n.is_empty())
                .unwrap_or_else(|| dev.name.clone());
            let model    = cached.and_then(|c| c.model.clone()).unwrap_or_default();
            let project  = cached.and_then(|c| c.project.clone()).unwrap_or_default();
            let firmware = cached.and_then(|c| c.firmware.clone()).unwrap_or_default();
            let pinned = cached.map_or(false, |c| c.pinned == Some(true));
            self.track_device(&key, &dev.uuid, &dev.ip, dev.tls_mode, pinned, name, model, project, firmware, true);
        }

        let pruned = self.do_prune();
        self.emit_list_changed();
        let _ = pruned; // list-changed already covers both cases; kept named for clarity at call site
    }

    /// Remove entries that are `Dead` (not pinned, not `Connected`) and no
    /// longer visible in the SSDP discovery list, dropping devlist's
    /// strong `DeviceState` reference — the actual "forgetting" (see this
    /// module's own doc comment: a device goes away for good once no
    /// window holds a reference either). Returns true if anything was
    /// removed.
    fn do_prune(&self) -> bool {
        let mut inner = self.imp().inner.borrow_mut();
        let before = inner.devices.len();
        inner.devices.retain(|key, rec| {
            let keep = rec.entry.pinned
                || rec.entry.presence == DevicePresence::Active
                || rec.in_discovery
                || rec.has_open_window;
            // No explicit `ds.disconnect(device_changed_id)` needed here:
            // `has_open_window` being false (a precondition for `!keep`) means
            // no device window holds its own strong ref to `rec.ds` either
            // (`device::manager::DeviceManager` only ever keeps weak refs),
            // so dropping `rec` below drops the last strong reference and
            // finalizes the `DeviceState` — signal connection included —
            // outright.
            if !keep {
                dbg(&format!("prune: forgetting {} ({key})", rec.entry.name));
            }
            keep
        });
        inner.devices.len() < before
    }

    fn load_known_devices_from_config(&self) {
        let devices = config::with(|cfg| cfg.devices.clone());
        for (uuid, dev_cfg) in &devices {
            let pinned = dev_cfg.pinned == Some(true);
            // Load pinned devices always.  Also pre-load non-pinned devices whose
            // window should reopen: the list-changed handler will open the window,
            // and if SSDP later confirms the device it stays; if not, do_prune
            // removes it (the window stays open independently).
            if !pinned && !dev_cfg.window_open { continue; }
            let Some(ref ip) = dev_cfg.last_ip else { continue };
            if self.imp().inner.borrow().devices.contains_key(uuid) { continue; }
            let tls = dev_cfg.tls_mode
                .map(|n| TlsMode::from_usize(n as usize))
                .unwrap_or(TlsMode::HttpsWiiM);
            let name     = dev_cfg.name.clone().unwrap_or_else(|| format!("Device @ {ip}"));
            let model    = dev_cfg.model.clone().unwrap_or_default();
            let project  = dev_cfg.project.clone().unwrap_or_default();
            let firmware = dev_cfg.firmware.clone().unwrap_or_default();
            dbg(&format!("load from config: {name} ({ip}) uuid={uuid} pinned={pinned}"));
            self.track_device(uuid, uuid, ip, tls, pinned, name, model, project, firmware, false);
        }
    }

    fn persist_pinned(&self) {
        let inner = self.imp().inner.borrow();
        config::update(|cfg| {
            for rec in inner.devices.values() {
                if rec.entry.uuid.is_empty() { continue; }
                let dev     = cfg.device_mut(&rec.entry.uuid);
                dev.pinned  = Some(rec.entry.pinned); // Explicit Some(true/false) ends legacy treatment.
                dev.last_ip = Some(rec.entry.ip.clone());
                dev.tls_mode = Some(rec.entry.tls_mode as u8);
                dev.name    = Some(rec.entry.name.clone());
                if !rec.entry.model.is_empty() {
                    dev.model = Some(rec.entry.model.clone());
                }
                if !rec.entry.project.is_empty() {
                    dev.project = Some(rec.entry.project.clone());
                }
                if !rec.entry.firmware.is_empty() {
                    dev.firmware = Some(rec.entry.firmware.clone());
                }
            }
        });
    }
}

fn device_key(uuid: &str, ip: &str) -> String {
    if !uuid.is_empty() { uuid.to_string() } else { format!("ip:{ip}") }
}

/// Shared by `entries()`/`entry_for()` — one record's cached identity
/// fields plus a freshly-computed `now_playing` snapshot, gated on
/// `song_info` and the record's own presence.
fn build_managed_entry(r: &DeviceRecord, song_info: bool) -> ManagedEntry {
    let mut entry = r.entry.clone();
    entry.song_info_enabled = song_info;
    entry.now_playing = (song_info && entry.presence == DevicePresence::Active)
        .then(|| compute_now_playing(&r.ds));
    entry
}

fn compute_now_playing(ds: &DeviceState) -> NowPlaying {
    let ps = ds.playback_state();
    let source_id = capabilities::mode_to_input_source(ds.current_mode());
    let icon_key = match ds.capabilities() {
        Some(caps) => capabilities::icon_canon_for_input(source_id, caps.device_id).to_string(),
        None       => source_id.to_string(),
    };
    NowPlaying {
        title:   ps.title.to_string(),
        artist:  ps.artist.to_string(),
        artwork: ps.artwork.clone(),
        art_url: ps.art_url.as_deref().map(|s| s.to_string()),
        icon_key,
    }
}

/// "Title · Artist" (round-dot separator, same as the mini window's
/// artist/album line) when now-playing content is available; falls back
/// to the model name otherwise. Shared by `build_row()` (initial render)
/// and the `song-info-changed` handler (in-place update).
fn subtitle_text_for(entry: &ManagedEntry) -> String {
    match &entry.now_playing {
        Some(np) if !np.title.is_empty() && !np.artist.is_empty() => format!("{} \u{00b7} {}", np.title, np.artist),
        Some(np) if !np.title.is_empty()  => np.title.clone(),
        Some(np) if !np.artist.is_empty() => np.artist.clone(),
        _ => entry.model.clone(),
    }
}

/// Applies `entry.now_playing` to an existing `FlipCover` — real art when
/// available, the input/mode icon otherwise, cleared when there's no
/// now-playing content at all. Shared by `build_row()` (initial render,
/// where it never flips — a freshly constructed `FlipCover` has no
/// "previous real art" to flip from) and the `song-info-changed` handler
/// (in-place update on a persistent widget, where a real track-to-track
/// change *does* flip — see `flip_cover.rs`'s own `set_content()`).
fn apply_now_playing(flip: &FlipCover, icons: &IconSet, entry: &ManagedEntry) {
    match &entry.now_playing {
        Some(np) => {
            let tex = np.artwork.as_ref().and_then(|bytes| {
                let gbytes = glib::Bytes::from(bytes.as_ref().as_slice());
                gtk::gdk::Texture::from_bytes(&gbytes).ok()
            });
            // Keys must vary with the actual content (matching
            // `ui/playback.rs`'s `update_artwork()`), never a constant
            // per-device value like `entry.uuid` — `FlipCover` treats a
            // repeated key as "nothing changed" and no-ops, which on this
            // persistent-per-row widget would silently drop every update
            // after the first.
            match &tex {
                Some(t) => flip.set_art(Some(t), np.art_url.as_deref().unwrap_or("")),
                None    => flip.set_icon(icons.source_paintable(&np.icon_key), 32.0, &format!("icon:{}", np.icon_key)),
            }
        }
        None => flip.clear(),
    }
}

/// Volume button + popover slider + mute button, same shape as the mini
/// window's own (`widgets.rs`'s `build_mini_flip_cover()`'s sibling —
/// see `.devlist-vol-*` CSS) sized for a compact row rather than a
/// standalone window. Caller wires the actual click/drag/mute handlers.
fn build_devlist_vol_popover() -> (gtk::Button, gtk::Image, gtk::Label, gtk::Scale, gtk::Button, gtk::Popover) {
    let vol_icon_img = gtk::Image::builder()
        .icon_name("audio-volume-high-symbolic")
        .pixel_size(13) // ~20% bigger than the mini-window-derived 11px original
        .build();
    let vol_label = gtk::Label::builder()
        .label("—")
        .width_chars(3)
        .xalign(1.0)
        .css_classes(["devlist-vol-label"])
        .build();
    let btn_box = gtk::Box::builder()
        .orientation(Orientation::Horizontal)
        .spacing(1)
        .build();
    btn_box.append(&vol_icon_img);
    btn_box.append(&vol_label);
    let vol_btn = gtk::Button::builder()
        .css_classes(["devlist-vol-btn", "flat"])
        .tooltip_text("Volume")
        .valign(gtk::Align::Center)
        .build();
    vol_btn.set_child(Some(&btn_box));

    let vol_scale = gtk::Scale::with_range(Orientation::Vertical, 0.0, 100.0, 1.0);
    vol_scale.set_inverted(true);
    vol_scale.set_vexpand(true);
    vol_scale.set_height_request(120);
    vol_scale.set_draw_value(false);
    vol_scale.set_width_request(20);
    vol_scale.set_round_digits(0);
    vol_scale.add_css_class("devlist-vol-pop");
    vol_scale.set_increments(5.0, 20.0);

    let mute_btn = gtk::Button::builder()
        .icon_name("audio-volume-muted-symbolic")
        .css_classes(["flat"])
        .tooltip_text("Mute")
        .halign(gtk::Align::Center)
        .build();

    let vol_pop_box = gtk::Box::builder()
        .orientation(Orientation::Vertical)
        .margin_top(4).margin_bottom(4).margin_start(4).margin_end(4)
        .spacing(4)
        .build();
    vol_pop_box.append(&vol_scale);
    vol_pop_box.append(&mute_btn);
    let vol_popover = gtk::Popover::new();
    vol_popover.add_css_class("devlist-vol-popover");
    vol_popover.set_child(Some(&vol_pop_box));
    vol_popover.set_parent(&vol_btn);

    (vol_btn, vol_icon_img, vol_label, vol_scale, mute_btn, vol_popover)
}

/// Syncs a row's volume icon/label/slider from `ds`'s live state — called
/// both at row construction and from the `song-info-changed` handler.
/// Skips repositioning the slider while `drag_timer` is set (same
/// protection the main/mini windows use), so a poll update arriving
/// mid-drag doesn't fight the user's own gesture.
fn sync_devlist_vol_display(rw: &RowWidgets, ds: &DeviceState) {
    let vol = ds.get_vol() as f64;
    let muted = ds.muted();
    if rw.vol_drag_timer.borrow().is_none() {
        rw.vol_scale.set_value(vol);
    }
    rw.vol_icon_img.set_icon_name(Some(vol_icon(muted, vol)));
    rw.vol_label.set_label(&format!("{}", vol as u32));
    rw.mute_btn.set_icon_name(if muted { "audio-volume-muted-symbolic" } else { "audio-volume-high-symbolic" });
}

// ── DiscoveryWindow ───────────────────────────────────────────────────────────

/// The subset of a row's widgets `song-info-changed` needs to update in
/// place — keyed by `device_key()`'s result in a map alongside
/// `current_entries`, rebuilt (not incrementally patched) whenever
/// `rebuild_list()` runs. Only present for rows built while
/// `entry.song_info_enabled` was true (see `build_row()`).
struct RowWidgets {
    flip:         FlipCover,
    subtitle:     ScrollFadeLabel,
    vol_icon_img: gtk::Image,
    vol_label:    gtk::Label,
    vol_scale:    gtk::Scale,
    mute_btn:     gtk::Button,
    /// Same drag-protection pattern as the main/mini windows'
    /// `DeviceWindowInner::ui_state.drag_timer` — while set, a live poll
    /// update (`song-info-changed`) skips repositioning the slider so it
    /// doesn't fight an in-progress drag.
    vol_drag_timer: Rc<RefCell<Option<glib::SourceId>>>,
}

pub struct DiscoveryWindow {
    window: adw::ApplicationWindow,
}

impl DiscoveryWindow {
    pub fn new(
        app:           &adw::Application,
        manager:       &DiscoveryManager,
        open_device:   Rc<dyn Fn(&ManagedEntry)>,
        open_settings: Rc<dyn Fn(Option<DeviceState>)>,
    ) -> Self {
        let (init_w, init_h) = config::with(|cfg| (
            if cfg.discovery_window_width  > 0 { cfg.discovery_window_width  } else { 500 },
            if cfg.discovery_window_height > 0 { cfg.discovery_window_height } else { 440 },
        ));

        // One IconSet for the whole window, shared by every row — same
        // pattern as a device window's own `icons` field, not rebuilt per
        // row/rebuild.
        let icons = Rc::new(IconSet::load());
        let window = adw::ApplicationWindow::builder()
            .application(app)
            .title("RustyWiiM")
            .default_width(init_w)
            .default_height(init_h)
            .build();

        let header = adw::HeaderBar::new();

        // Custom title widget so the spinner can sit immediately to the right
        // of the "Scanning…" subtitle text.  adw::WindowTitle has no widget
        // slot in its subtitle row so we build the layout by hand.
        let title_label = gtk::Label::builder()
            .label("RustyWiiM")
            .css_classes(["title"])
            .build();

        let subtitle_label = gtk::Label::builder()
            .label("Scanning\u{2026}")
            .css_classes(["subtitle"])
            .build();

        let spinner = gtk::Spinner::builder()
            .spinning(true)
            .build();
        spinner.set_size_request(12, 12);

        let subtitle_row = gtk::Box::builder()
            .orientation(Orientation::Horizontal)
            .spacing(4)
            .halign(gtk::Align::Center)
            .build();
        subtitle_row.append(&subtitle_label);
        subtitle_row.append(&spinner);

        let title_box = gtk::Box::builder()
            .orientation(Orientation::Vertical)
            .valign(gtk::Align::Center)
            .build();
        title_box.append(&title_label);
        title_box.append(&subtitle_row);

        header.set_title_widget(Some(&title_box));

        let add_btn = gtk::Button::builder()
            .icon_name("list-add-symbolic")
            .tooltip_text("Add device by IP address…")
            .build();
        header.pack_end(&add_btn);
        header.pack_end(&super::menu::build_menu_button(false));
        super::wire_window_actions(&window, None, open_settings);

        // Device list
        let list_box = gtk::ListBox::builder()
            .selection_mode(gtk::SelectionMode::None)
            .css_classes(["boxed-list"])
            .margin_top(12).margin_bottom(12)
            .margin_start(12).margin_end(12)
            .build();

        let scroll = gtk::ScrolledWindow::builder()
            .hscrollbar_policy(gtk::PolicyType::Never)
            .vexpand(true)
            .build();
        scroll.set_child(Some(&list_box));

        let content = gtk::Box::builder()
            .orientation(Orientation::Vertical)
            .build();
        content.append(&scroll);

        let toolbar_view = adw::ToolbarView::new();
        toolbar_view.add_top_bar(&header);
        toolbar_view.set_content(Some(&content));
        window.set_content(Some(&toolbar_view));

        // The entries backing the currently-rendered rows, kept in the same
        // order they were appended in — `list_box.connect_row_activated()`
        // below looks a clicked/Enter-activated row up by index into this
        // rather than each row closing over its own entry, since GtkListBox
        // already fires `row-activated` for any child row type (not just
        // AdwActionRow) on click or keyboard activation, and reusing that
        // is simpler than reimplementing it per row.
        let current_entries: Rc<RefCell<Vec<ManagedEntry>>> = Rc::new(RefCell::new(Vec::new()));
        // Live-updatable widgets for rows currently showing song info —
        // see `song-info-changed`'s doc comment (`signals()`) for why
        // this exists instead of just rebuilding the list.
        let row_widgets: Rc<RefCell<HashMap<String, RowWidgets>>> = Rc::new(RefCell::new(HashMap::new()));

        // Populate list and subscribe to manager changes.
        Self::rebuild_list(&list_box, &manager.entries(), &current_entries, &row_widgets, manager, &icons);

        // List rebuild: structural changes only (device added/removed/
        // renamed/pinned/moved, presence flips) — see `signals()`.
        manager.connect_list_changed(clone!(
            @strong list_box, @strong current_entries, @strong row_widgets, @strong icons
                => move |mgr| {
                    Self::rebuild_list(&list_box, &mgr.entries(), &current_entries, &row_widgets, mgr, &icons);
                }
        ));

        // One device's now-playing content changed — update just its row
        // in place instead of rebuilding the whole list.
        manager.connect_song_info_changed(clone!(@strong row_widgets, @strong icons => move |mgr, key| {
            let Some(entry) = mgr.entry_for(key) else { return };
            let widgets = row_widgets.borrow();
            let Some(rw) = widgets.get(key) else { return };
            apply_now_playing(&rw.flip, &icons, &entry);
            rw.subtitle.set_text(&subtitle_text_for(&entry));
            if let Some(ds) = mgr.device_state_for(key) {
                sync_devlist_vol_display(rw, &ds);
            }
        }));

        list_box.connect_row_activated(clone!(@strong current_entries, @strong open_device => move |_, row| {
            let idx = row.index();
            if idx < 0 { return; }
            if let Some(entry) = current_entries.borrow().get(idx as usize) {
                open_device(entry);
            }
        }));

        // Scanning indicator: clear only when the SSDP scan cycle reports in.
        let scanning = Rc::new(std::cell::Cell::new(true));
        manager.connect_scan_complete(clone!(
            @strong subtitle_row, @strong scanning, @strong spinner
                => move || {
                    if scanning.replace(false) {
                        spinner.set_spinning(false);
                        subtitle_row.set_visible(false);
                    }
                }
        ));

        // "Add device" button.
        add_btn.connect_clicked(clone!(@strong window, @strong manager => move |_| {
            Self::show_add_dialog(&window, &manager);
        }));

        // win.close action — lets Ctrl-W (set app-wide) close this window.
        {
            let close_act = gtk::gio::SimpleAction::new("close", None);
            close_act.connect_activate(clone!(@strong window => move |_, _| { window.close(); }));
            window.add_action(&close_act);
        }

        // Hide when other windows are visible; quit (propagate) when last.
        window.connect_close_request(clone!(@strong window => move |w| {
            let (ww, wh) = (w.width(), w.height());
            config::update(|cfg| {
                cfg.discovery_open = false;
                if ww > 0 { cfg.discovery_window_width  = ww; }
                if wh > 0 { cfg.discovery_window_height = wh; }
            });

            let others_visible = w.application().map_or(false, |app| {
                app.windows().iter().any(|other| {
                    other.upcast_ref::<gtk::Widget>() != w.upcast_ref::<gtk::Widget>()
                        && other.is_visible()
                })
            });
            if others_visible {
                w.hide();
                glib::Propagation::Stop
            } else {
                glib::Propagation::Proceed
            }
        }));

        Self { window }
    }

    pub fn present(&self) {
        config::update(|cfg| cfg.discovery_open = true);
        self.window.present();
    }

    fn rebuild_list(
        list_box:        &gtk::ListBox,
        entries:         &[ManagedEntry],
        current_entries: &Rc<RefCell<Vec<ManagedEntry>>>,
        row_widgets:     &Rc<RefCell<HashMap<String, RowWidgets>>>,
        manager:         &DiscoveryManager,
        icons:           &Rc<IconSet>,
    ) {
        while let Some(child) = list_box.first_child() {
            list_box.remove(&child);
        }
        // Must match row append order exactly — `list_box.connect_row_activated`
        // looks a clicked row up by index into this.
        *current_entries.borrow_mut() = entries.to_vec();
        // Rebuilt from scratch alongside the rows themselves (structural
        // change — every widget is new) — `song-info-changed`'s handler
        // only ever updates a row that's still in here.
        row_widgets.borrow_mut().clear();
        if entries.is_empty() {
            let placeholder = adw::ActionRow::builder()
                .title("No devices found")
                .sensitive(false)
                .build();
            list_box.append(&placeholder);
            return;
        }
        for entry in entries {
            list_box.append(&Self::build_row(entry, row_widgets, manager, icons));
        }
    }

    fn build_row(
        entry:       &ManagedEntry,
        row_widgets: &Rc<RefCell<HashMap<String, RowWidgets>>>,
        manager:     &DiscoveryManager,
        icons:       &Rc<IconSet>,
    ) -> gtk::ListBoxRow {
        let key = device_key(&entry.uuid, &entry.ip);

        let hbox = gtk::Box::builder()
            .orientation(Orientation::Horizontal)
            .spacing(12)
            .margin_top(8).margin_bottom(8)
            .margin_start(12).margin_end(12)
            .build();

        // Artwork/icon slot — reserved at a fixed size (`.devlist-art`'s
        // CSS min-width/min-height) whenever song-info display is on
        // globally, regardless of whether *this* device currently has
        // anything to show there, so the row's right-hand side (volume
        // control, pin button) never shifts as devices update. Same
        // FlipCover widget (flip/crossfade between real art and the
        // fallback icon) the main and mini windows use, not a separate
        // plain-image path.
        let flip = entry.song_info_enabled.then(|| {
            let flip = FlipCover::new();
            flip.set_hexpand(false);
            flip.set_vexpand(false);
            flip.set_valign(gtk::Align::Center);
            flip.add_css_class("devlist-art");
            flip.set_overflow(gtk::Overflow::Hidden);
            apply_now_playing(&flip, icons, entry);
            hbox.append(&flip);
            flip
        });

        let text_box = gtk::Box::builder()
            .orientation(Orientation::Vertical)
            .valign(gtk::Align::Center)
            .hexpand(true)
            .build();

        let title_label = gtk::Label::builder()
            .label(&entry.name)
            .halign(gtk::Align::Start)
            .ellipsize(gtk::pango::EllipsizeMode::End)
            .build();
        text_box.append(&title_label);

        // "Title · Artist" (round-dot separator, same as the mini window's
        // artist/album line) when now-playing content is available; falls
        // back to the model name otherwise. A ScrollFadeLabel — not a
        // plain Label, and deliberately not AdwActionRow's own `subtitle`
        // property — for two reasons: it scrolls long text instead of
        // silently truncating, and it sets plain Pango text rather than
        // markup (`subtitle` interprets markup, which broke on a literal
        // "&" in a device or track name — moot here since ScrollFadeLabel
        // never parses its text as markup at all).
        let subtitle = ScrollFadeLabel::new(&subtitle_text_for(entry));
        subtitle.set_halign(gtk::Align::Start);
        subtitle.set_hexpand(true);
        subtitle.add_label_css_class("dim-label");
        subtitle.add_label_css_class("caption");
        text_box.append(&subtitle);

        hbox.append(&text_box);

        // IP address moved from a permanently-visible label to a hover
        // tooltip on the whole row — freeing that space for the volume
        // control below.
        let status_suffix = match entry.presence {
            DevicePresence::Active => String::new(),
            DevicePresence::Ghost | DevicePresence::Dead => " · offline".to_string(),
        };
        let row_tooltip = format!("{}{}", entry.ip, status_suffix);

        // Volume button + popover slider + mute, same widget shape as the
        // mini window's own. Reserved alongside the artwork slot (same
        // `song_info_enabled` gate — volume data is only kept fresh while
        // Simple-mode's fuller poll is active, same as title/artist), and
        // greyed out for a device that isn't `Active` right now rather
        // than hidden, so the row's layout doesn't shift either way. A
        // click on it doesn't open the device window — a `GtkButton`
        // child claims its own click before the row's own click-to-
        // activate gesture sees it, same as the pin button already relies
        // on (never special-cased, just how GTK widgets nest).
        let vol_widgets = entry.song_info_enabled.then(|| {
            let (vol_btn, vol_icon_img, vol_label, vol_scale, mute_btn, vol_popover) = build_devlist_vol_popover();
            vol_btn.set_sensitive(entry.presence == DevicePresence::Active);
            vol_btn.connect_clicked(clone!(@weak vol_popover => move |_| {
                if vol_popover.is_visible() { vol_popover.popdown(); } else { vol_popover.popup(); }
            }));
            hbox.append(&vol_btn);
            (vol_icon_img, vol_label, vol_scale, mute_btn)
        });

        if let (Some(flip), Some((vol_icon_img, vol_label, vol_scale, mute_btn))) = (flip, vol_widgets) {
            let vol_drag_timer: Rc<RefCell<Option<glib::SourceId>>> = Rc::new(RefCell::new(None));

            if let Some(ds) = manager.device_state_for(&key) {
                let rw_for_sync = RowWidgets {
                    flip: flip.clone(), subtitle: subtitle.clone(),
                    vol_icon_img: vol_icon_img.clone(), vol_label: vol_label.clone(),
                    vol_scale: vol_scale.clone(), mute_btn: mute_btn.clone(),
                    vol_drag_timer: vol_drag_timer.clone(),
                };
                sync_devlist_vol_display(&rw_for_sync, &ds);

                mute_btn.connect_clicked(clone!(@strong ds => move |_| {
                    ds.do_set_mute(!ds.muted());
                }));
                vol_scale.connect_change_value(clone!(
                    @strong ds, @strong vol_icon_img, @strong vol_label, @strong vol_drag_timer
                        => move |_, _, vol| {
                            let icon = vol_icon(ds.muted(), vol);
                            vol_icon_img.set_icon_name(Some(icon));
                            vol_label.set_label(&format!("{}", vol as u32));
                            ds.do_set_volume(vol as u32);
                            if let Some(id) = vol_drag_timer.borrow_mut().take() { id.remove(); }
                            let timer_cell = Rc::clone(&vol_drag_timer);
                            let id = glib::timeout_add_local_once(std::time::Duration::from_millis(500), move || {
                                timer_cell.borrow_mut().take();
                            });
                            *vol_drag_timer.borrow_mut() = Some(id);
                            glib::Propagation::Proceed
                        }
                ));
            }

            row_widgets.borrow_mut().insert(key, RowWidgets {
                flip, subtitle: subtitle.clone(),
                vol_icon_img, vol_label, vol_scale, mute_btn,
                vol_drag_timer,
            });
        }

        // Pin / unpin toggle button.
        let pin_btn = gtk::ToggleButton::builder()
            .icon_name(if entry.pinned { "starred-symbolic" } else { "non-starred-symbolic" })
            .tooltip_text(if entry.pinned { "Unpin device" } else { "Pin device" })
            .active(entry.pinned)
            .valign(gtk::Align::Center)
            .css_classes(["flat"])
            .build();
        if entry.pinned {
            pin_btn.add_css_class("accent");
        }
        let uuid_for_pin = entry.uuid.clone();
        pin_btn.connect_toggled(clone!(@strong manager => move |btn| {
            manager.set_pinned(&uuid_for_pin, btn.is_active());
        }));
        hbox.append(&pin_btn);

        let row = gtk::ListBoxRow::builder()
            .activatable(true)
            .child(&hbox)
            .tooltip_text(&row_tooltip)
            .build();
        if entry.presence != DevicePresence::Active {
            row.add_css_class("dim-label");
        }
        row
    }

    fn show_add_dialog(parent: &adw::ApplicationWindow, manager: &DiscoveryManager) {
        let ip_entry = gtk::Entry::builder()
            .placeholder_text("192.168.1.x")
            .activates_default(true)
            .build();

        let dialog = adw::AlertDialog::builder()
            .heading("Add Device")
            .body("Enter the IP address of a WiiM device:")
            .close_response("cancel")
            .build();
        dialog.add_response("cancel", "Cancel");
        dialog.add_response("add", "Add");
        dialog.set_response_appearance("add", adw::ResponseAppearance::Suggested);
        dialog.set_default_response(Some("add"));
        dialog.set_extra_child(Some(&ip_entry));

        dialog.connect_response(None, clone!(
            @strong manager, @strong ip_entry
                => move |_dlg, resp| {
                    if resp != "add" { return; }
                    let ip = ip_entry.text().to_string();
                    if ip.is_empty() { return; }

                    let rt = manager.rt();
                    let (tx, rx) = async_channel::bounded::<Option<DiscoveredDevice>>(1);
                    let ip2 = ip.clone();
                    rt.spawn(async move {
                        let result = crate::device::discovery::DiscoveryService::probe_device(&ip2).await;
                        let _ = tx.send(result).await;
                    });

                    glib::spawn_future_local(clone!(@strong manager => async move {
                        if let Ok(Some(dev)) = rx.recv().await {
                            manager.add_manual(dev.name, dev.ip, dev.uuid, dev.tls_mode);
                        } else {
                            eprintln!("[devlist-ui] Could not reach device at {ip}");
                        }
                    }));
                }
        ));

        dialog.present(Some(parent));
    }
}
