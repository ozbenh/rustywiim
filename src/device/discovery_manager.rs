// Picker-list backend: tracks every known device (SSDP-discovered, pinned/
// remembered from a config-derived seed, or manually added by IP) as a real
// `device::manager::DeviceManager`-owned `DeviceState` — holding a strong
// reference to it for as long as the device is "known" (see `do_prune()`),
// which is what keeps it alive and polling (Simple mode, unless a device
// window also holds an `acquire_full()` guard) even with no window open.
// There is no separate health-check poll: Simple-mode polling's own
// `getStatusEx` *is* the liveness check, and presence for rendering
// (`DevicePresence::compute()`) is read straight off each tracked
// `DeviceState::connection_state()`, not tracked independently. Recovery
// after a failure is `DeviceState`'s own job (`maybe_self_reconnect()`,
// `state.rs`) — nothing external pokes it anymore.
//
// This module cannot depend on `config` (same rule `device::manager`
// already follows — `device/` is meant to be a self-sufficient hardware
// abstraction with no implicit knowledge of the UI/config layer). Instead:
// `ui/` calls `load_seed()` once at startup with a config-derived snapshot
// (`SeedEntry`), and listens to the existing `list-changed` signal to persist
// whatever this module learns back to config — see `load_seed()`'s and
// `set_song_info()`'s doc comments for the full seed-in/report-out story.
//
// `ui::devlist::DiscoveryWindow` is the actual on-screen picker — it renders
// `entries()` and calls back into this module (`set_pinned()`, `add_manual()`,
// etc.) but owns no tracking state of its own.

use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::Arc;

use glib::prelude::*;
use glib::subclass::prelude::*;
use gtk::glib;

use crate::device::api::TlsMode;
use crate::device::capabilities;
use crate::device::discovery::{DEBUG_DISCOVERY, DiscoveryService};
use crate::device::manager::DeviceManager;
use crate::device::state::{ConnectionState, DeviceState};

/// `[disc-mgr]` — this module's own tracking/presence/persistence-signal
/// logic. Distinct from `device/discovery.rs`'s `[discovery]` (the SSDP
/// service itself) and from `ui::devlist`'s `[devlist-ui]` (the actual
/// on-screen picker window, which has no debug logging of its own beyond
/// that one line).
fn dbg(msg: &str) {
    if DEBUG_DISCOVERY.load(std::sync::atomic::Ordering::Relaxed) {
        println!("{} [disc-mgr] {msg}", super::timestamp());
    }
}

/// Human-readable form of a `device::state::playback_changed` bitmask, for
/// `--debug=discovery`'s `song-info-changed` line — lets a live session
/// show exactly which bits triggered a given row update instead of just
/// the raw hex value.
fn describe_playback_mask(mask: u32) -> String {
    use crate::device::state::playback_changed as PC;
    let names: &[(u32, &str)] = &[
        (PC::ARTWORK, "ARTWORK"), (PC::TITLE, "TITLE"), (PC::ARTIST, "ARTIST"),
        (PC::ALBUM, "ALBUM"), (PC::TIME, "TIME"), (PC::VOLUME, "VOLUME"), (PC::OTHER, "OTHER"),
    ];
    let bits: Vec<&str> = names.iter().filter(|(bit, _)| mask & bit != 0).map(|(_, name)| *name).collect();
    if bits.is_empty() { "none".to_string() } else { bits.join("|") }
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
    /// resolve the device's profile default while offline. Empty until the
    /// tracked `DeviceState` has connected at least once.
    pub project:  String,
    pub firmware: String,
    pub ip:       String,
    pub tls_mode: TlsMode,
    pub pinned:   bool,
    pub presence: DevicePresence,
    /// Mirrors song-info display's on/off state at the moment `entries()`
    /// was called — separate from `now_playing` below so `ui/`'s row
    /// rendering can reserve its artwork/icon slot (fixed size, so the
    /// row's right-hand side never shifts as devices update) even when
    /// this particular device has nothing to show there yet (not
    /// `Active`, e.g.).
    pub song_info_enabled: bool,
    /// Live now-playing snapshot for row rendering — unlike the identity
    /// fields above (cached on `DeviceRecord.entry`, refreshed only on
    /// `device-changed`), this is computed fresh every `entries()` call
    /// straight from the tracked `DeviceState::playback_state()`, since
    /// title/artist change far more often than identity does. `None`
    /// unless song-info display is on and the device is `Active` — *not*
    /// further gated on actually having a track loaded, so an idle-but-
    /// connected device still gets its input/mode icon rather than nothing.
    pub now_playing: Option<NowPlaying>,
}

#[derive(Debug, Clone)]
pub struct NowPlaying {
    pub title:    String,
    pub artist:   String,
    pub artwork:  Option<std::rc::Rc<Vec<u8>>>,
    /// Doubles as `ui/`'s `FlipCover::set_art()` de-dupe key (same as the
    /// main window's own `update_artwork()` uses) — never a constant-per-
    /// device value (e.g. uuid), or every update after the first becomes a
    /// silent no-op once a row's `FlipCover` is a persistent widget rather
    /// than rebuilt fresh each time.
    pub art_url:  Option<String>,
    /// Icon key for the row's fallback icon when `artwork` is `None` — the
    /// same `icons::IconSet::source_paintable()` lookup key the main
    /// window's own no-art fallback uses, computed the same way
    /// (`mode_to_input_source()` + `icon_canon_for_input()`) so a device's
    /// picker row shows the same icon its own window would. `ui/` owns
    /// actually resolving this into a paintable — this module just
    /// supplies the key.
    pub icon_key: String,
}

/// Config-derived seed for one uuid, handed in once via `load_seed()` —
/// this module's only view of `config::DeviceConfig`'s relevant fields,
/// since it can't read config itself. Mirrors the subset `ui/` already
/// persists back via `list-changed`.
#[derive(Debug, Clone)]
pub struct SeedEntry {
    pub uuid:        String,
    pub name:        Option<String>,
    pub model:        Option<String>,
    pub project:      Option<String>,
    pub firmware:     Option<String>,
    pub pinned:       bool,
    pub last_ip:      Option<String>,
    pub tls_mode:     TlsMode,
    pub window_open:  bool,
}

// ── Internal record ───────────────────────────────────────────────────────────

/// One tracked device: cached rendering identity (refreshed from
/// `ds.device_info()`/`ds.capabilities()` whenever `ds` connects — see
/// `on_tracked_device_changed()`) plus the strong `DeviceState` handle that
/// keeps it alive/polling for as long as this record exists. `ds` is what
/// makes "forgetting a device" plain refcounting: dropping this record
/// (`do_prune()`) drops the last reference this module holds, and once no
/// device window holds one either, the `DeviceState` itself goes away.
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
    /// The `device-changed`/`playback-changed` handlers connected in
    /// `create_and_track()` are never explicitly disconnected — no
    /// `SignalHandlerId` kept for them — because `do_prune()` only ever
    /// drops a record while `has_open_window` is false, and
    /// `device::manager::DeviceManager` itself only ever keeps *weak*
    /// refs, so `ds` below is guaranteed to have no other strong holder at
    /// that point: dropping this record finalizes `ds` outright,
    /// connection included.
    ds: DeviceState,
}

struct Inner {
    devices: HashMap<String, DeviceRecord>,
    /// Whether every tracked device additionally fetches title/artist/
    /// artwork — mirrors `config::Config::devlist_song_info`, pushed in
    /// once via `load_seed()` and again on every `set_song_info()` call.
    /// Cached here rather than re-read from config (this module can't) on
    /// every device creation.
    song_info: bool,
    /// Config-derived cache of every known device's identity, handed in
    /// once via `load_seed()`. Consulted (never mutated) by
    /// `on_discovery_updated()` to enrich a freshly-SSDP-seen device that
    /// `load_seed()`/`start()` didn't already eagerly track (a known but
    /// unpinned device seen again this session) — see `load_seed()`'s doc
    /// comment for why a boot-time-only snapshot is safe here.
    seed: HashMap<String, SeedEntry>,
}

impl Default for Inner {
    fn default() -> Self {
        Self { devices: HashMap::new(), song_info: false, seed: HashMap::new() }
    }
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
                // Fired on every tracked-device-list change (new/moved/
                // pruned device, presence flip, identity update, pin
                // toggle, song-info toggle). `ui/`'s own listener reads
                // `entries()` off this and persists the relevant subset
                // back to config — see `set_song_info()`'s doc comment for
                // why this module never writes config itself. Deliberately
                // structural only — a single tracked device's now-playing
                // content or volume/mute change goes through
                // `song-info-changed` instead (see below), not this.
                Signal::builder("list-changed").build(),
                // Fired once, synchronously in start(), after the seed
                // (handed in via load_seed()) has been eagerly tracked —
                // before any async discovery results arrive.
                Signal::builder("initial-load").build(),
                // A single tracked device's now-playing content (title/
                // artist/artwork) or volume/mute changed — deliberately
                // *not* folded into `list-changed`. That would make `ui/`
                // rebuild every row's widgets from scratch on every track/
                // volume change (this fires far more often than anything
                // structural), which is both wasteful and defeats
                // FlipCover's flip-vs-fade logic there: a freshly
                // reconstructed FlipCover never has "previous real art" on
                // the same widget instance to flip from. Params: the
                // tracked device's key (`device_key()`'s result — same
                // string `entries()`'s rows/`current_entries` are indexed
                // by) and the raw `playback_changed` bitmask. The mask
                // matters, not just the key — a handler that reran on
                // *every* firing regardless of which bits changed would
                // catch the gap where title/artist land before the async
                // art fetch resolves, and flash the fallback icon before
                // the real flip.
                Signal::builder("song-info-changed")
                    .param_types([String::static_type(), u32::static_type()])
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
    /// `device/`'s `DeviceManager` is the registry every tracked
    /// `DeviceState` comes from, and there's no ownership-layering reason
    /// to hide that behind indirection now that both live in `device/`.
    pub fn new(rt: Arc<tokio::runtime::Runtime>, discovery: DiscoveryService, device_manager: DeviceManager) -> Self {
        let obj: Self = glib::Object::new();
        obj.imp().rt.set(rt).unwrap();
        obj.imp().discovery.set(discovery).unwrap();
        obj.imp().device_manager.set(device_manager).unwrap();
        obj
    }

    /// `pub` (not just an internal helper) so `ui/settings.rs`'s general
    /// preferences page can reach `DeviceManager::for_each_live()` to
    /// re-push the app-wide GENA toggle to every open device — see that
    /// page's build function.
    pub fn device_manager(&self) -> &DeviceManager {
        self.imp().device_manager.get().unwrap()
    }

    /// Hand in a config-derived snapshot — this module's only view of
    /// config, since it can't read config itself. Must be called exactly
    /// once, before `start()`. Stores `song_info` and the full `seed` map
    /// (keyed by uuid) in `Inner`; `start()` is what actually eagerly
    /// tracks the `pinned || window_open` subset of it.
    ///
    /// A boot-time-only snapshot (never refreshed after this call) is
    /// safe, not just convenient: the app is the sole config writer while
    /// running, and `seed` is only ever consulted for a uuid *not yet*
    /// tracked (`on_discovery_updated()`) — once a device becomes tracked
    /// it's kept live via `on_tracked_device_changed()`/`set_pinned()`
    /// instead, never falling back to `seed` again.
    pub fn load_seed(&self, seed: Vec<SeedEntry>, song_info: bool) {
        let mut inner = self.imp().inner.borrow_mut();
        inner.song_info = song_info;
        inner.seed = seed.into_iter().map(|e| (e.uuid.clone(), e)).collect();
    }

    pub fn start(&self) {
        self.track_seeded_devices();
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
    /// refcount). Used for a picker row's volume/mute control, which talks
    /// to the device directly rather than going through `ManagedEntry`
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
    fn emit_song_info_changed(&self, key: &str, mask: u32) {
        dbg(&format!("song info changed: {key} mask={mask:#04x} ({})", describe_playback_mask(mask)));
        self.emit_by_name::<()>("song-info-changed", &[&key.to_string(), &mask]);
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
            self.do_prune();
            self.emit_list_changed();
        }
    }

    /// Toggles whether every tracked device additionally fetches title/
    /// artist/artwork (`ui/`'s Settings "General" page). Updates the
    /// cached `Inner.song_info` new devices read at creation
    /// (`create_and_track()`) and pushes the new value onto every
    /// currently-tracked `DeviceState` right away — so toggling takes
    /// effect immediately, not just for devices tracked afterward. No
    /// effect on a device already in `Full` mode (an open window already
    /// fetches this content regardless — see
    /// `DeviceState::configure_simple_mode()`'s doc comment).
    ///
    /// Deliberately does **not** persist to config itself — this module
    /// can't. The caller (`ui/settings.rs`'s switch handler, which already
    /// has config access) does the `config::update()` write; this just
    /// does the fan-out.
    pub fn set_song_info(&self, want: bool) {
        {
            let inner = self.imp().inner.borrow_mut();
            for rec in inner.devices.values() {
                rec.ds.configure_simple_mode(want);
            }
        }
        self.imp().inner.borrow_mut().song_info = want;
        self.emit_list_changed();
    }

    /// Records whether a device window is currently open for `uuid` — see
    /// `DeviceRecord::has_open_window`'s doc comment. Called by `ui::mod`'s
    /// `AppState` on window open/close. No-op if `uuid` is empty or
    /// unknown to this module (a window with nothing here to mark — e.g. a
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
    pub fn connect_song_info_changed<F: Fn(&Self, &str, u32) + 'static>(&self, f: F) -> glib::SignalHandlerId {
        self.connect_local("song-info-changed", false, move |args| {
            let obj  = args[0].get::<Self>().unwrap();
            let key  = args[1].get::<String>().unwrap();
            let mask = args[2].get::<u32>().unwrap();
            f(&obj, &key, mask);
            None
        })
    }

    /// Fired once, synchronously inside `start()`, after eagerly tracking
    /// the seeded devices — before any async discovery results arrive.
    /// Use this to restore windows from config; do NOT use `list-changed`
    /// for that, as it fires on every subsequent change (e.g. pin toggles).
    pub fn connect_initial_load<F: Fn(&Self) + 'static>(&self, f: F) -> glib::SignalHandlerId {
        self.connect_local("initial-load", false, move |args| {
            f(&args[0].get::<Self>().unwrap());
            None
        })
    }

    /// Fires once when the underlying SSDP scan cycle completes (or the 4-second
    /// initial timeout expires with no devices found).  Use this — not
    /// `connect_list_changed` — to clear a "Scanning…" indicator, because
    /// devices already tracked from the seed would clear it prematurely.
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
    /// `track_seeded_devices()`/`add_manual()` all funnel through, so a
    /// record is always built/refreshed the same way regardless of what
    /// triggered it. `name`/`model`/`project`/`firmware` seed the entry's
    /// *rendering* fields for a record that doesn't exist yet
    /// (seed-cached values, or whatever the SSDP/manual-add probe already
    /// had) — ignored if the record already exists, since
    /// `on_tracked_device_changed()` (wired via `device-changed`) is the
    /// one place identity fields get overwritten once `ds` has actually
    /// answered for real.
    #[allow(clippy::too_many_arguments)]
    fn track_device(
        &self,
        key: &str, uuid: &str, ip: &str, tls: TlsMode, pinned: bool,
        name: String, model: String, project: String, firmware: String,
        in_discovery: bool,
    ) {
        // No explicit persist/emit here on the "moved" path — every caller
        // of `track_device()` (`on_discovery_updated()`, `add_manual()`,
        // `track_seeded_devices()` via `start()`) already calls
        // `emit_list_changed()` itself afterward, which is also what
        // `ui/`'s listener persists off; a second emission here would just
        // be redundant.
        let mut inner = self.imp().inner.borrow_mut();
        let Some(rec) = inner.devices.get_mut(key) else {
            drop(inner);
            self.create_and_track(key, uuid, ip, tls, pinned, name, model, project, firmware, in_discovery);
            return;
        };
        if rec.entry.ip != ip || rec.entry.tls_mode != tls {
            dbg(&format!("track_device: {} moved {} → {ip}", rec.entry.name, rec.entry.ip));
            rec.entry.ip = ip.to_string();
            rec.entry.tls_mode = tls;
            // Covers any live DeviceState for this uuid, not just this
            // entry — e.g. an already-open device window reconnects to
            // the corrected IP too.
            self.device_manager().update_ip(uuid, ip, tls);
        }
        rec.in_discovery = rec.in_discovery || in_discovery;
    }

    /// The actual creation half of `track_device()`, split out only so its
    /// `inner` borrow (above) can drop cleanly before this runs —
    /// `create_and_configure()` can re-enter this same `DiscoveryManager`
    /// synchronously via the `configure-device` signal's connected handler
    /// (`ui::AppState`'s, which doesn't touch this module — but
    /// `device-changed` firing on the very first poll tick, before
    /// `create_and_configure()` even returns, is close enough to a real
    /// risk to just not hold the borrow across the call at all).
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
                mgr.emit_song_info_changed(&key_for_song_info, mask);
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
    /// offline and isn't pinned/open/in-discovery), and always re-renders
    /// (`ui/`'s `list-changed` listener persists identity changes back to
    /// config unconditionally — see `set_song_info()`'s doc comment for
    /// why this module doesn't gate that itself).
    fn on_tracked_device_changed(&self, key: &str, ds: &DeviceState) {
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
                if !info.device_name.is_empty() { rec.entry.name = info.device_name.clone(); }
                if !info.project.is_empty()     { rec.entry.project = info.project.clone(); }
                if !info.firmware.is_empty()    { rec.entry.firmware = info.firmware.clone(); }
            }
            if let Some(caps) = ds.capabilities() {
                if !caps.model.is_empty() { rec.entry.model = caps.model.clone(); }
            }
        }
        self.do_prune();
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

        // Snapshot, not a live borrow held across `track_device()` below
        // (which re-borrows `inner` itself) — see `load_seed()`'s doc
        // comment for why a boot-time-only seed is safe to keep consulting
        // like this for the lifetime of the app.
        let seed = self.imp().inner.borrow().seed.clone();
        for dev in &discovered {
            let key = device_key(&dev.uuid, &dev.ip);
            let cached = seed.get(&dev.uuid);
            let name  = cached.and_then(|c| c.name.clone())
                .filter(|n| !n.is_empty())
                .unwrap_or_else(|| dev.name.clone());
            let model    = cached.and_then(|c| c.model.clone()).unwrap_or_default();
            let project  = cached.and_then(|c| c.project.clone()).unwrap_or_default();
            let firmware = cached.and_then(|c| c.firmware.clone()).unwrap_or_default();
            let pinned = cached.map_or(false, |c| c.pinned);
            self.track_device(&key, &dev.uuid, &dev.ip, dev.tls_mode, pinned, name, model, project, firmware, true);
        }

        let pruned = self.do_prune();
        self.emit_list_changed();
        let _ = pruned; // list-changed already covers both cases; kept named for clarity at call site
    }

    /// Remove entries that are `Dead` (not pinned, not `Connected`) and no
    /// longer visible in the SSDP discovery list, dropping this module's
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

    /// Eagerly track every seeded device with `pinned || window_open` set
    /// (same condition `ui/devlist.rs`'s old `load_known_devices_from_config()`
    /// used) — the rest of `seed` stays around purely as
    /// `on_discovery_updated()`'s enrichment cache. Called once from
    /// `start()`, after `load_seed()` has already populated `Inner.seed`.
    fn track_seeded_devices(&self) {
        let seed = self.imp().inner.borrow().seed.clone();
        for entry in seed.values() {
            if !entry.pinned && !entry.window_open { continue; }
            let Some(ref ip) = entry.last_ip else { continue };
            if self.imp().inner.borrow().devices.contains_key(&entry.uuid) { continue; }
            let name     = entry.name.clone().unwrap_or_else(|| format!("Device @ {ip}"));
            let model    = entry.model.clone().unwrap_or_default();
            let project  = entry.project.clone().unwrap_or_default();
            let firmware = entry.firmware.clone().unwrap_or_default();
            dbg(&format!("seed: {name} ({ip}) uuid={} pinned={}", entry.uuid, entry.pinned));
            self.track_device(&entry.uuid, &entry.uuid, ip, entry.tls_mode, entry.pinned, name, model, project, firmware, false);
        }
    }
}

/// `pub`, not private — `device/` and `ui/` are separate crates (see
/// `lib.rs`'s doc comment), so `pub(crate)` wouldn't reach `ui::devlist`'s
/// row-building code, which needs to compute the exact same key
/// independently (to index its own `RowWidgets` map alongside this
/// module's `Inner.devices`) — both sides must share this one algorithm
/// rather than risk drifting apart.
pub fn device_key(uuid: &str, ip: &str) -> String {
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
