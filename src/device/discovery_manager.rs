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
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use glib::prelude::*;
use glib::subclass::prelude::*;
use gtk::glib;

use crate::device::api::TlsMode;
use crate::device::capabilities;
use crate::device::discovery::{DEBUG_DISCOVERY, DiscoveryService};
use crate::device::group;
use crate::device::manager::DeviceManager;
use crate::device::state::{ConnectionState, DeviceState};
use crate::device::utils;

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
    /// Where this entry sits in the multiroom topology, and therefore how
    /// it should be rendered. Set by `entries()`, which is the only place
    /// with a view of every device at once — a single `DeviceState` knows
    /// its own role but cannot resolve its leader to a row.
    pub group_role: EntryGroupRole,
}

/// How one row participates in a group, from the device list's point of
/// view.
///
/// A group replaces its members' individual rows with a `GroupHeader`
/// followed by one `Member` line each — including a line for the leader
/// itself, which is why a leader contributes two entries rather than one.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum EntryGroupRole {
    /// Not grouped; an ordinary row.
    #[default]
    Standalone,
    /// The group's own row. Backed by the leader's `DeviceState` — under
    /// the hood this still targets the leader, since the leader *is* the
    /// group's playback — so it keeps the full artwork/song treatment. Its
    /// volume is the group's, not the leader's own.
    GroupHeader { follower_count: usize },
    /// A compact line beneath a `GroupHeader`: name and volume only.
    Member {
        /// `device_key()` of the group this line belongs to, so the UI can
        /// tie a line back to its header without relying on adjacency.
        leader_key: String,
        /// False when this member is in the leader's slave list but is not
        /// a device we track — no `DeviceState`, so no volume control. Also
        /// true-but-unreachable members under WiFi-Direct grouping, whose
        /// reported address cannot be routed to from this host.
        tracked: bool,
    },
}

#[derive(Debug, Clone)]
pub struct NowPlaying {
    pub title:    String,
    /// Mirrors `PlaybackState::is_idle` — `title` is a real placeholder
    /// ("No music selected") rather than empty when idle, so row rendering
    /// (`subtitle_text_for()`) needs this to still prefer the device's
    /// model name over that placeholder, matching its own established
    /// "idle-but-connected still gets something sensible" behavior.
    pub is_idle:  bool,
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

    /// The device list, in render order, with multiroom topology resolved.
    ///
    /// Grouped devices do **not** appear as ordinary rows. Each group
    /// contributes a `GroupHeader` followed by one `Member` line per
    /// member, the leader included; ungrouped devices keep their normal
    /// row. See `resolve_topology()`, which holds the actual rules.
    pub fn entries(&self) -> Vec<ManagedEntry> {
        let inner = self.imp().inner.borrow();
        let song_info = inner.song_info;
        let rows: Vec<(String, ManagedEntry, group::GroupState)> = inner.devices.iter()
            .map(|(key, r)| (key.clone(), build_managed_entry(r, song_info), r.ds.group_state()))
            .collect();
        resolve_topology(rows)
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

    /// Tracks any member a leader reports that discovery has not reached.
    ///
    /// A grouped device can stop announcing itself over SSDP — confirmed on
    /// real hardware, where a follower was absent from discovery entirely
    /// while its leader listed it — which would otherwise leave it showing
    /// as a permanently greyed-out member line with no controls. The
    /// leader's slave list already carries the member's uuid *and* address,
    /// so there is enough to connect directly and nothing extra to fetch.
    ///
    /// Members on a WiFi-Direct leader's private subnet are deliberately
    /// skipped: those addresses are not routable from this host, so probing
    /// them only produces failures for devices that are fine.
    ///
    /// A member the leader reports with no uuid at all (`decode_member()`
    /// accepts ip-only entries) is skipped the same way — there is no key to
    /// track it under (see this module's `device_key()`/the codebase-wide
    /// "no key, no tracking" rule), and it still renders correctly via
    /// `resolve_topology()`'s existing "named by the leader but not tracked"
    /// fallback, the same path a member discovery hasn't reached yet already
    /// uses.
    fn adopt_group_members(&self, leader_key: &str, ds: &DeviceState) {
        let g = ds.group_state();
        if g.role != group::GroupRole::Leader {
            return;
        }
        // The leader's own TLS mode is the best available guess for a
        // member's: a group's devices are near-always the same generation,
        // and a wrong guess costs one failed probe that the normal
        // reconnect path already handles.
        let tls = self.imp().inner.borrow().devices.get(leader_key)
            .map(|r| r.entry.tls_mode)
            .unwrap_or(TlsMode::HttpsWiiM);
        let mut adopted = false;
        for m in g.members.iter() {
            if m.ip.is_empty() || utils::is_wifi_direct_address(&m.ip) {
                dbg(&format!("group: skipping unroutable member {} ({})", m.name, m.ip));
                continue;
            }
            let key = device_key(&m.uuid, &m.ip);
            if self.imp().inner.borrow().devices.contains_key(&key) {
                continue;
            }
            dbg(&format!("group: adopting member {} ({}) uuid={:?}", m.name, m.ip, m.uuid));
            self.track_device(
                &key, &m.uuid, &m.ip, tls, false,
                m.name.clone(), String::new(), String::new(), String::new(), false,
            );
            adopted = true;
        }
        if adopted {
            self.emit_list_changed();
        }
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
            // Resolved per-render by `entries()`, which is the only place
            // with a view of every device at once; the cached record never
            // carries a meaningful value.
            group_role: EntryGroupRole::Standalone,
        };
        let weak = self.downgrade();
        let key_owned = key.to_string();
        ds.connect_device_changed(move |ds| {
            let Some(mgr) = weak.upgrade() else { return };
            mgr.on_tracked_device_changed(&key_owned, ds);
        });
        // A topology change restructures the list itself — rows appear,
        // disappear and change kind — so unlike a now-playing update this
        // has to go through the full `list-changed` rebuild rather than the
        // per-row `song-info-changed` path.
        let weak_group = self.downgrade();
        let key_for_group = key.to_string();
        ds.connect_group_changed(move |ds| {
            let Some(mgr) = weak_group.upgrade() else { return };
            mgr.adopt_group_members(&key_for_group, ds);
            mgr.emit_list_changed();
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
            let key = device_key(&entry.uuid, ip);
            if self.imp().inner.borrow().devices.contains_key(&key) { continue; }
            let name     = entry.name.clone().unwrap_or_else(|| format!("Device @ {ip}"));
            let model    = entry.model.clone().unwrap_or_default();
            let project  = entry.project.clone().unwrap_or_default();
            let firmware = entry.firmware.clone().unwrap_or_default();
            dbg(&format!("seed: {name} ({ip}) uuid={} pinned={}", entry.uuid, entry.pinned));
            self.track_device(&key, &entry.uuid, ip, entry.tls_mode, entry.pinned, name, model, project, firmware, false);
        }
    }
}

/// `pub`, not private — `device/` and `ui/` are separate crates (see
/// `lib.rs`'s doc comment), so `pub(crate)` wouldn't reach `ui::devlist`'s
/// row-building code, which needs to compute the exact same key
/// independently (to index its own `RowWidgets` map alongside this
/// module's `Inner.devices`) — both sides must share this one algorithm
/// rather than risk drifting apart.
///
/// Normalises the uuid (`utils::normalize_uuid`) as a belt-and-braces
/// anchor: every uuid reaching here is already normalised at its entry
/// boundary (SSDP `USN` extraction, `getStatusEx` deserialisation, config
/// load), so this is idempotent — it exists so this key and any key an
/// independent caller (`ui::devlist`) computes from the same inputs cannot
/// drift apart, not because a raw value is expected to arrive.
pub fn device_key(uuid: &str, ip: &str) -> String {
    let uuid = utils::normalize_uuid(uuid);
    if !uuid.is_empty() { uuid } else { format!("ip:{ip}") }
}

/// Turns a flat set of tracked devices into the ordered, topology-annotated
/// list the device list renders.
///
/// Kept free of GObjects and of `DiscoveryManager` state so the rules below
/// can be tested directly — they are fiddly, and every one of them exists
/// because getting it wrong makes a device disappear from the list.
///
/// Rules:
/// - A **leader** becomes a `GroupHeader` carrying the group's name,
///   followed by a `Member` line for itself and one per follower. The
///   leader needs its own line because the header's volume is the group's,
///   not the leader's.
/// - A **follower whose leader we track** is absorbed into that group and
///   contributes no top-level row.
/// - A **follower whose leader we do not track** keeps an ordinary row.
///   Its leader may be offline or simply not discovered yet, and dropping
///   the row would make a reachable device vanish entirely.
/// - A member named in a leader's slave list but **not tracked at all**
///   still gets a line, built from what the leader reported, flagged
///   `tracked: false` so the UI can render it without controls.
/// - Ordering is by name across headers and standalone rows; a group's
///   member lines stay immediately after their header.
fn resolve_topology(rows: Vec<(String, ManagedEntry, group::GroupState)>) -> Vec<ManagedEntry> {
    // uuid -> key, since a follower knows its leader only by uuid.
    // `e.uuid` is already canonical (normalized at its entry boundary), so
    // this is a plain lookup, not a re-normalization.
    let by_uuid: HashMap<String, String> = rows.iter()
        .filter(|(_, e, _)| !e.uuid.is_empty())
        .map(|(key, e, _)| (e.uuid.clone(), key.clone()))
        .collect();
    let is_leader: HashSet<String> = rows.iter()
        .filter(|(_, _, g)| g.role == group::GroupRole::Leader)
        .map(|(key, _, _)| key.clone())
        .collect();

    let absorbed: HashSet<String> = rows.iter()
        .filter(|(_, _, g)| g.role == group::GroupRole::Follower)
        .filter(|(_, _, g)| g.leader_uuid.as_deref()
            .and_then(|u| by_uuid.get(u))
            .is_some_and(|lk| is_leader.contains(lk)))
        .map(|(key, _, _)| key.clone())
        .collect();

    let tracked: HashMap<&str, &ManagedEntry> =
        rows.iter().map(|(key, e, _)| (key.as_str(), e)).collect();

    let mut heads: Vec<&(String, ManagedEntry, group::GroupState)> = rows.iter()
        .filter(|(key, _, _)| !absorbed.contains(key))
        .collect();
    // Sort on the name as it will be displayed, which for a group is its
    // group name rather than the leader's device name.
    heads.sort_by_cached_key(|(_, e, g)| match g.role {
        group::GroupRole::Leader => group::auto_group_name(&e.name, g.follower_count()),
        _ => e.name.clone(),
    });

    let mut out = Vec::with_capacity(rows.len());
    for (key, entry, g) in heads {
        if g.role != group::GroupRole::Leader {
            out.push(entry.clone());
            continue;
        }

        let mut header = entry.clone();
        header.name = group::auto_group_name(&entry.name, g.follower_count());
        header.group_role = EntryGroupRole::GroupHeader { follower_count: g.follower_count() };
        out.push(header);

        let mut line = entry.clone();
        line.song_info_enabled = false;
        line.now_playing = None;
        line.group_role = EntryGroupRole::Member { leader_key: key.clone(), tracked: true };
        out.push(line);

        for m in g.members.iter() {
            let member_key = by_uuid.get(&m.uuid);
            let mut line = match member_key.and_then(|k| tracked.get(k.as_str())) {
                Some(e) => (*e).clone(),
                // Reported by the leader but not tracked. Presence is
                // `Dead` because nothing has ever reached it — the leader's
                // word is not a connection.
                None => ManagedEntry {
                    uuid:     m.uuid.clone(),
                    name:     m.name.clone(),
                    model:    String::new(),
                    project:  String::new(),
                    firmware: String::new(),
                    ip:       m.ip.clone(),
                    tls_mode: TlsMode::HttpsWiiM,
                    pinned:   false,
                    presence: DevicePresence::Dead,
                    song_info_enabled: false,
                    now_playing:       None,
                    group_role:        EntryGroupRole::Standalone,
                },
            };
            line.song_info_enabled = false;
            line.now_playing = None;
            line.group_role = EntryGroupRole::Member {
                leader_key: key.clone(),
                tracked:    member_key.is_some(),
            };
            out.push(line);
        }
    }
    out
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
        is_idle: ps.is_idle,
        artist:  ps.artist.to_string(),
        artwork: ps.artwork.clone(),
        art_url: ps.art_url.as_deref().map(|s| s.to_string()),
        icon_key,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::rc::Rc;

    /// `uuid` is normalized here for the same reason every real
    /// `ManagedEntry` holds a canonical one: it is only ever built from a
    /// source that normalized at its own entry boundary (`DeviceInfo`, SSDP,
    /// the config seed). Fixtures spell it raw so the tests can show the
    /// shapes the different sources actually produce.
    fn entry(uuid: &str, name: &str, ip: &str) -> ManagedEntry {
        ManagedEntry {
            uuid: utils::normalize_uuid(uuid), name: name.to_string(), model: String::new(),
            project: String::new(), firmware: String::new(), ip: ip.to_string(),
            tls_mode: TlsMode::HttpsWiiM, pinned: false, presence: DevicePresence::Active,
            song_info_enabled: true, now_playing: None,
            group_role: EntryGroupRole::Standalone,
        }
    }

    fn member(uuid: &str, name: &str, ip: &str) -> group::GroupMember {
        group::GroupMember {
            uuid: utils::normalize_uuid(uuid), name: name.to_string(), ip: ip.to_string(),
            volume: 30, muted: false, role: group::ChannelRole::Stereo, masked: false,
        }
    }

    fn leader_of(members: Vec<group::GroupMember>) -> group::GroupState {
        group::GroupState {
            role: group::GroupRole::Leader,
            members: Rc::new(members),
            ..Default::default()
        }
    }

    fn follower_of(leader_uuid: &str) -> group::GroupState {
        group::GroupState {
            role: group::GroupRole::Follower,
            leader_uuid: Some(utils::normalize_uuid(leader_uuid)),
            ..Default::default()
        }
    }

    fn row(key: &str, e: ManagedEntry, g: group::GroupState) -> (String, ManagedEntry, group::GroupState) {
        (key.to_string(), e, g)
    }

    fn shape(out: &[ManagedEntry]) -> Vec<(String, &'static str)> {
        out.iter().map(|e| (e.name.clone(), match e.group_role {
            EntryGroupRole::Standalone      => "standalone",
            EntryGroupRole::GroupHeader { .. } => "header",
            EntryGroupRole::Member { .. }   => "member",
        })).collect()
    }

    #[test]
    fn device_key_is_stable_across_the_uuid_shapes_that_reach_it() {
        // Every source spells the same device differently. Keying on the
        // raw string tracked one device under two keys and showed it twice
        // — and, worse, made its own identity check reject it.
        let canonical = device_key("FF280012BA8B2ABF53ECD9E4", "10.1.1.77");
        for raw in [
            "ff280012ba8b2abf53ecd9e4",                 // slave list (normalised)
            "uuid:FF280012BA8B2ABF53ECD9E4",            // SSDP UDN
            "uuid:ff280012-ba8b-2abf-53ec-d9e4",        // SSDP UDN, hyphenated
            " FF280012BA8B2ABF53ECD9E4 ",               // stray whitespace
        ] {
            assert_eq!(device_key(raw, "10.1.1.77"), canonical, "{raw}");
        }
        // No uuid at all still falls back to the address.
        assert_eq!(device_key("", "10.1.1.77"), "ip:10.1.1.77");
    }

    #[test]
    fn ungrouped_devices_render_as_plain_rows_sorted_by_name() {
        let out = resolve_topology(vec![
            row("b", entry("BBB", "Zulu", "1.1.1.2"), group::GroupState::default()),
            row("a", entry("AAA", "Alpha", "1.1.1.1"), group::GroupState::default()),
        ]);
        assert_eq!(shape(&out), vec![
            ("Alpha".into(), "standalone"),
            ("Zulu".into(), "standalone"),
        ]);
    }

    #[test]
    fn a_group_becomes_a_header_followed_by_a_line_per_member() {
        let out = resolve_topology(vec![
            row("L", entry("LEAD", "WiiM WorkBu", "1.1.1.1"),
                leader_of(vec![member("F1", "WiiM MiniBu", "1.1.1.2")])),
            row("F", entry("F1", "WiiM MiniBu", "1.1.1.2"), follower_of("LEAD")),
        ]);
        // The leader gets a line of its own as well as the header: the
        // header's volume is the group's, so the leader still needs
        // somewhere to expose its own.
        assert_eq!(shape(&out), vec![
            ("WiiM WorkBu + 1".into(), "header"),
            ("WiiM WorkBu".into(),     "member"),
            ("WiiM MiniBu".into(),     "member"),
        ]);
        // The follower contributes no top-level row of its own.
        assert!(!out.iter().any(|e| e.name == "WiiM MiniBu"
            && e.group_role == EntryGroupRole::Standalone));
    }

    #[test]
    fn member_lines_carry_their_leaders_key_and_drop_song_info() {
        let out = resolve_topology(vec![
            row("L", entry("LEAD", "Lead", "1.1.1.1"),
                leader_of(vec![member("F1", "Follower", "1.1.1.2")])),
            row("F", entry("F1", "Follower", "1.1.1.2"), follower_of("LEAD")),
        ]);
        for e in out.iter().filter(|e| matches!(e.group_role, EntryGroupRole::Member { .. })) {
            match &e.group_role {
                EntryGroupRole::Member { leader_key, tracked } => {
                    assert_eq!(leader_key, "L");
                    assert!(*tracked, "{} should be tracked", e.name);
                }
                _ => unreachable!(),
            }
            // Member lines are compact: no artwork, no song.
            assert!(!e.song_info_enabled, "{}", e.name);
            assert!(e.now_playing.is_none(), "{}", e.name);
        }
    }

    #[test]
    fn a_member_the_leader_names_but_we_do_not_track_still_gets_a_line() {
        // Until discovery reaches it, the leader's word is all we have —
        // omitting it would show a two-device group as one device.
        let out = resolve_topology(vec![
            row("L", entry("LEAD", "Lead", "1.1.1.1"),
                leader_of(vec![member("GHOST", "Unknown Speaker", "1.1.1.9")])),
        ]);
        assert_eq!(shape(&out), vec![
            ("Lead + 1".into(),        "header"),
            ("Lead".into(),            "member"),
            ("Unknown Speaker".into(), "member"),
        ]);
        let ghost = out.last().unwrap();
        assert_eq!(ghost.group_role, EntryGroupRole::Member { leader_key: "L".into(), tracked: false });
        assert_eq!(ghost.ip, "1.1.1.9");
        assert_eq!(ghost.presence, DevicePresence::Dead);
    }

    #[test]
    fn a_follower_whose_leader_is_not_tracked_keeps_its_own_row() {
        // Otherwise a device that is perfectly reachable disappears from
        // the list because its leader happens to be offline or
        // undiscovered.
        let out = resolve_topology(vec![
            row("F", entry("F1", "Orphan", "1.1.1.2"), follower_of("MISSING")),
        ]);
        assert_eq!(shape(&out), vec![("Orphan".into(), "standalone")]);
    }

    #[test]
    fn a_follower_pointing_at_a_device_that_is_not_a_leader_keeps_its_own_row() {
        // Stale state on one side of a group that has just been torn down:
        // absorbing it under a device that reports no members would produce
        // a header with nothing under it.
        let out = resolve_topology(vec![
            row("A", entry("AAA", "Alpha", "1.1.1.1"), group::GroupState::default()),
            row("B", entry("BBB", "Bravo", "1.1.1.2"), follower_of("AAA")),
        ]);
        assert_eq!(shape(&out), vec![
            ("Alpha".into(), "standalone"),
            ("Bravo".into(), "standalone"),
        ]);
    }

    #[test]
    fn uuid_punctuation_differences_still_resolve_a_follower_to_its_leader() {
        // The leader's slave list, SSDP and `getStatusEx` each punctuate the
        // same device differently. Resolution here is a plain lookup, so what
        // makes it line up is that all three normalized on the way in — this
        // pins that end-to-end, not a re-normalization inside
        // `resolve_topology`.
        let out = resolve_topology(vec![
            row("L", entry("FF98F7F4075B", "Lead", "1.1.1.1"),
                leader_of(vec![member("uuid:ff98-0002", "Follower", "1.1.1.2")])),
            row("F", entry("uuid:FF98-0002", "Follower", "1.1.1.2"),
                follower_of("uuid:ff98f7f4-075b")),
        ]);
        assert_eq!(shape(&out), vec![
            ("Lead + 1".into(), "header"),
            ("Lead".into(),     "member"),
            ("Follower".into(), "member"),
        ]);
        assert!(matches!(out[2].group_role, EntryGroupRole::Member { tracked: true, .. }));
    }

    #[test]
    fn groups_and_standalones_interleave_by_displayed_name() {
        // A group sorts under its *group* name, not the leader's device
        // name, so the list reads in the order it appears.
        let out = resolve_topology(vec![
            row("Z", entry("ZZZ", "Zulu", "1.1.1.3"), group::GroupState::default()),
            row("L", entry("LEAD", "Mike", "1.1.1.1"),
                leader_of(vec![member("F1", "Foxtrot", "1.1.1.2")])),
            row("A", entry("AAA", "Alpha", "1.1.1.4"), group::GroupState::default()),
            row("F", entry("F1", "Foxtrot", "1.1.1.2"), follower_of("LEAD")),
        ]);
        assert_eq!(shape(&out), vec![
            ("Alpha".into(),    "standalone"),
            ("Mike + 1".into(), "header"),
            ("Mike".into(),     "member"),
            ("Foxtrot".into(),  "member"),
            ("Zulu".into(),     "standalone"),
        ]);
    }
}
