// Device registry — single source of truth for live `DeviceState` objects,
// and also the picker-list backend (previously a separate GObject type,
// `device::discovery_manager`, since folded in here). The two were never two
// different *kinds* of thing, only two different *policies* layered on the
// same one: `states` (low-level, every `DeviceState` this process has ever
// created, deduped by uuid) and the picker-list metadata below it
// (`Inner.devices`, the known-by-default tracked set the device list
// actually renders).
//
// **Why two maps, not one.** `states` is a strict superset: `--connect`'s
// device gets a `DeviceState` (so `get()`/`create_and_configure()` dedupe
// correctly against it) without ever being picker-tracked (so it doesn't
// show up in the device list, and never reaches config either — see
// `Config::device_mut()`'s own doc comment). Fusing the two maps would mean
// either showing `--connect`'s device in the picker (wrong) or bolting a
// "shown in picker" flag onto every entry, which is just the two-map split
// again with extra steps.
//
// Retention for the picker-tracked set is known-by-default: a device that
// has ever been successfully identified (a real, resolved uuid — see the
// codebase-wide "no key, no tracking" rule in `device::utils`) stays tracked
// forever, with no presence-based eviction. There is no pinning, no "in
// discovery scope" exemption, no "has an open window" exemption — none of
// that machinery is needed once staying known no longer depends on a
// heuristic. The only way a picker-tracked device stops being tracked is
// `forget()`, an explicit user action (the device list's offline-only
// trashcan button) — see that method's doc comment for what it does and
// doesn't handle. `states` alone (a device this registry created a
// `DeviceState` for but never picker-tracked) has no equivalent removal —
// nothing needs one yet; see `forget()`'s own doc comment.
//
// There is no separate health-check poll: Simple-mode polling's own
// `getStatusEx` *is* the liveness check, and presence for rendering
// (`DevicePresence::compute()`) is read straight off each tracked
// `DeviceState::connection_state()`, not tracked independently. Recovery
// after a failure is `DeviceState`'s own job (`maybe_self_reconnect()`,
// `state.rs`) — nothing external pokes it anymore.
//
// This module cannot depend on `config` — `device/` is meant to be a
// self-sufficient hardware abstraction with no implicit knowledge of the
// UI/config layer, forkable into its own crate with no dependency on the
// main binary. Instead: `ui/`
// calls `load_seed()` once at startup with a config-derived snapshot
// (`SeedEntry`), and listens to the `list-changed` signal to persist
// whatever this module learns back to config — see `load_seed()`'s doc
// comment for the full seed-in/report-out story. `forget()` additionally
// needs `ui/` to delete that uuid's config entry itself, or the device would
// simply be re-seeded on the next launch — see `forget()`'s doc comment.
// `configure-device` is the same shape of inversion of control, one level
// down: it fires per-`DeviceState` rather than for the tracked set as a
// whole, so `ui/` can push per-device behaviour overrides in before first
// contact.
//
// `ui::devlist::DiscoveryWindow` is the actual on-screen picker — it renders
// `entries()` and calls back into this module (`forget()`, `add_manual()`,
// etc.) but owns no tracking state of its own.
//
// Every caller resolves a real uuid before reaching this registry — nothing
// without one may ever be tracked (see `device::discovery::ProbeFailure`'s
// doc comment and `adopt_group_members()`'s no-uuid skip below) — so
// `get()`/`create_and_configure()` need no empty-uuid special case.
// `DeviceState::detached()` (Kiosk's "no device bound" placeholder) is the
// one legitimate keyless `DeviceState` in the app, and it bypasses this
// registry entirely rather than being an exception here.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use glib::prelude::*;
use glib::subclass::prelude::*;

use crate::device::api::TlsMode;
use crate::device::capabilities;
use crate::device::discovery::{DEBUG_DISCOVERY, DiscoveryService};
use crate::device::group;
use crate::device::playback::AccessMethod;
use crate::device::state::{ConnectionState, DeviceState};

/// `[disc-mgr]` — this module's own tracking/presence/persistence-signal
/// logic. Distinct from `device/discovery.rs`'s `[discovery]` (the SSDP
/// service itself) and from `ui::devlist`'s `[devlist-ui]` (the actual
/// on-screen picker window, which has no debug logging of its own beyond
/// that one line). Kept as `[disc-mgr]`, not renamed to match this file's
/// own name, on purpose — every existing `--debug=discovery` log line a
/// user or a saved bug report already references still matches.
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
    Active,  // ConnectionState::Connected
    Offline, // anything else — known-by-default means no "pinned vs not" split
}

impl DevicePresence {
    fn compute(state: ConnectionState) -> Self {
        if state == ConnectionState::Connected { Self::Active } else { Self::Offline }
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
        /// Uuid of the group (leader) this line belongs to, so the UI can
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
    pub last_ip:      Option<String>,
    pub tls_mode:     TlsMode,
    pub window_open:  bool,
}

// ── Internal records ──────────────────────────────────────────────────────────

/// One picker-tracked device: cached rendering identity, refreshed from
/// `ds.device_info()`/`ds.capabilities()` whenever `ds` connects (see
/// `on_tracked_device_changed()`). No `DeviceState` handle of its own —
/// `states` (below) is the sole strong holder; reached via
/// `get_state(key)` wherever this module needs the live object.
/// `forget()`'s job is correspondingly split: dropping this record removes
/// the *metadata*; the underlying `DeviceState` only actually finalises
/// once `forget()` also drops `states`' own strong reference (which it does,
/// right after) **and** nothing else — a device or settings window — still
/// holds a clone of its own.
///
/// The `device-changed`/`playback-changed`/`group-changed` handlers
/// connected in `create_and_track()` are never explicitly disconnected —
/// no `SignalHandlerId` kept for them anywhere. Each closure captures this
/// registry only weakly, so a handler simply becomes a no-op the moment
/// every strong reference to *this object itself* is gone.
struct DeviceRecord {
    entry: ManagedEntry,
}

#[derive(Default)]
struct Inner {
    devices: HashMap<String, DeviceRecord>,
    /// Config-derived cache of every known device's identity, handed in
    /// once via `load_seed()`. Consulted (never mutated) by
    /// `on_discovery_updated()` to enrich a freshly-SSDP-seen device that
    /// `load_seed()`/`start()` didn't already eagerly track — the one case
    /// that still happens with known-by-default retention is a config entry
    /// with no `last_ip` yet (nothing to connect to until discovery supplies
    /// one) — see `load_seed()`'s doc comment for why a boot-time-only
    /// snapshot is safe here.
    seed: HashMap<String, SeedEntry>,
}

mod imp {
    use super::*;
    use glib::subclass::Signal;
    use std::sync::OnceLock;

    pub struct DeviceManager {
        pub(super) rt:        std::cell::OnceCell<Arc<tokio::runtime::Runtime>>,
        pub(super) discovery: std::cell::OnceCell<DiscoveryService>,
        pub(super) states:    RefCell<HashMap<String, DeviceState>>,
        /// Picker-list metadata — `Inner.devices`/`Inner.seed`, kept in its
        /// own `RefCell` separate from `states` above (not merged into one
        /// structure) exactly as it was split across two GObjects before
        /// this module merge: the two are borrowed independently and at
        /// different times (e.g. `entries()` borrows `inner` and looks
        /// `states` up per-row without ever needing a mutable borrow of
        /// `states` itself), and keeping the split avoids introducing any
        /// new double-borrow risk while restructuring.
        pub(super) inner:     RefCell<Inner>,
    }

    impl Default for DeviceManager {
        fn default() -> Self {
            Self {
                rt:        std::cell::OnceCell::new(),
                discovery: std::cell::OnceCell::new(),
                states:    RefCell::new(HashMap::new()),
                inner:     RefCell::new(Inner::default()),
            }
        }
    }

    #[glib::object_subclass]
    impl ObjectSubclass for DeviceManager {
        const NAME: &'static str = "RustyWiimDeviceManager";
        type Type = super::DeviceManager;
    }

    impl ObjectImpl for DeviceManager {
        fn signals() -> &'static [Signal] {
            static SIGNALS: OnceLock<Vec<Signal>> = OnceLock::new();
            SIGNALS.get_or_init(|| {
                vec![
                    // Fired synchronously exactly once per `DeviceState`,
                    // right after construction and *before* it's allowed to
                    // make first contact (`set_device(..., connect_now:
                    // true)` — see `create_and_configure()`) — never on
                    // `get()`'s path, which already receives overrides as
                    // caller-supplied params instead (`ui/`'s existing,
                    // older pattern of resolving config before ever calling
                    // in). A real GObject signal rather than a `Rc<dyn
                    // Fn(..)>` hook deliberately, for the long-term "fork
                    // `device/` into its own crate, possibly with a C API
                    // on top" goal.
                    //
                    // The connected handler resolves config for this
                    // device's uuid (`DeviceState::uuid()`, already fixed
                    // at construction) and calls back
                    // `set_playback_access_override()`/
                    // `set_mute_access_override()` on the passed
                    // `DeviceState` before returning — `device/` can't read
                    // config itself, this is the one place `ui/` gets a
                    // synchronous chance to push it in before polling
                    // starts.
                    Signal::builder("configure-device")
                        .param_types([DeviceState::static_type()])
                        .build(),
                    // Fired on every tracked-device-list change (new/moved/
                    // pruned/forgotten device, presence flip, identity
                    // update). `ui/`'s own listener reads `entries()` off
                    // this and persists the relevant subset back to config
                    // — this module never writes config itself (see this
                    // module's own doc comment). Deliberately structural
                    // only — a single tracked device's now-playing content
                    // or volume/mute change goes through `song-info-changed`
                    // instead (see below), not this.
                    Signal::builder("list-changed").build(),
                    // Fired once, synchronously in start(), after the seed
                    // (handed in via load_seed()) has been eagerly tracked —
                    // before any async discovery results arrive.
                    Signal::builder("initial-load").build(),
                    // A single tracked device's now-playing content (title/
                    // artist/artwork) or volume/mute changed — deliberately
                    // *not* folded into `list-changed`. That would make
                    // `ui/` rebuild every row's widgets from scratch on
                    // every track/volume change (this fires far more often
                    // than anything structural), which is both wasteful and
                    // defeats FlipCover's flip-vs-fade logic there: a
                    // freshly reconstructed FlipCover never has "previous
                    // real art" on the same widget instance to flip from.
                    // Params: the tracked device's uuid (the same string
                    // `entries()`'s rows are indexed by) and the raw
                    // `playback_changed` bitmask. The mask matters, not just
                    // the key — a handler that reran on *every* firing
                    // regardless of which bits changed would catch the gap
                    // where title/artist land before the async art fetch
                    // resolves, and flash the fallback icon before the real
                    // flip.
                    Signal::builder("song-info-changed")
                        .param_types([String::static_type(), u32::static_type()])
                        .build(),
                ]
            })
        }
    }
}

glib::wrapper! {
    pub struct DeviceManager(ObjectSubclass<imp::DeviceManager>);
}

impl DeviceManager {
    /// `discovery` is a direct reference (not a hook/callback) — this
    /// registry is the only consumer of SSDP results, and there's no
    /// ownership-layering reason to hide that behind indirection.
    pub fn new(rt: Arc<tokio::runtime::Runtime>, discovery: DiscoveryService) -> Self {
        let obj: Self = glib::Object::new();
        obj.imp().rt.set(rt).unwrap();
        obj.imp().discovery.set(discovery).unwrap();
        obj
    }

    /// Connect to `configure-device` — see `imp::DeviceManager::signals()`'s
    /// doc comment for the full contract (fires synchronously, before first
    /// contact; connect this immediately after `new()`, before anything
    /// else runs).
    pub fn connect_configure_device<F: Fn(&Self, &DeviceState) + 'static>(
        &self,
        f: F,
    ) -> glib::SignalHandlerId {
        self.connect_local("configure-device", false, move |values| {
            let obj = values[0].get::<Self>().expect("configure-device arg 0: DeviceManager");
            let ds  = values[1].get::<DeviceState>().expect("configure-device arg 1: DeviceState");
            f(&obj, &ds);
            None
        })
    }

    /// Expose the tokio runtime for callers that need it directly.
    pub fn rt(&self) -> Arc<tokio::runtime::Runtime> {
        self.imp().rt.get().expect("DeviceManager::rt() called before new()").clone()
    }

    /// Return a live `DeviceState` for `uuid` + `ip` + `tls`.
    ///
    /// * **Existing entry**: if this uuid is already tracked, that same
    ///   object is returned. The `ip`/`tls`/`access_override`/`try_connect`
    ///   arguments are ignored (the device is already connected and
    ///   configured).
    /// * **New entry**: a fresh `DeviceState` is created, given
    ///   `access_override` up front (before polling starts, so the very
    ///   first poll tick already uses it, not just ones after some later
    ///   caller happens to push it in), configured (`ip`/`tls`/client, and
    ///   an actual connection attempt too if `try_connect`), polling is
    ///   started, and it's stored — for as long as this uuid stays known,
    ///   not just for as long as some caller keeps its own clone (see this
    ///   module's own doc comment).
    ///
    /// `uuid` must be non-empty — every caller resolves a real uuid before
    /// reaching this registry (see this module's own doc comment).
    ///
    /// `try_connect` — whether to actually attempt a connection now
    /// (`DeviceState::set_device`'s `connect_now`). The caller (`ui/mod.rs`)
    /// passes this based on the device list's current belief about the
    /// device (`ManagedEntry::presence`, computed from its own tracked
    /// `DeviceState::connection_state()`): if it already believes it's
    /// offline, there's no point immediately repeating a connection attempt
    /// that's already known to fail — the fresh `DeviceState` sits
    /// configured-but-`Disconnected` until its own `maybe_self_reconnect()`
    /// (or an external `mark_reachable()` call, for a caller that wants to
    /// drive this itself) brings it back.
    ///
    /// `access_override`/`mute_access_override`/`loop_mode_access_override`
    /// take the same `Option<AccessMethod>` shape
    /// `DeviceState::set_playback_access_override()`/`set_mute_access_override()`/
    /// `set_loop_mode_access_override()` already use — already `config`-free
    /// on their own (this module can't depend on `config`, main-binary-crate
    /// only, kept out of the reusable device layer the CLI tools link
    /// against), so the caller (currently `ui/mod.rs`'s
    /// `DeviceWindow::new_for_device()`, which already has the per-device
    /// config in hand at this exact point) can pass
    /// `config::DeviceConfig::playback_access_override`/`mute_access_override`/
    /// `loop_mode_access_override` straight through with no conversion step.
    /// `gena_enabled` mirrors this exactly, except it's already the fully
    /// resolved bool (`config::resolved_gena_enabled()`) rather than an
    /// override to combine with a profile default — see
    /// `DeviceState::set_device()`'s doc comment.
    /// Doesn't go through `configure-device` at all — this is the older,
    /// still-valid pattern of the caller resolving config *before* ever
    /// calling in, which `add_known_device()`/`create_and_configure()` below
    /// can't use since they're triggered without a synchronous config-aware
    /// caller in the loop.
    pub fn get(
        &self,
        uuid: &str,
        ip: &str,
        tls: TlsMode,
        access_override: Option<AccessMethod>,
        mute_access_override: Option<AccessMethod>,
        loop_mode_access_override: Option<AccessMethod>,
        gena_enabled: bool,
        try_connect: bool,
    ) -> DeviceState {
        if let Some(ds) = self.lookup(uuid) {
            return ds;
        }

        let ds = DeviceState::new(self.rt(), uuid.to_string());
        ds.set_manager(self);
        ds.set_device(ip, tls, access_override, mute_access_override, loop_mode_access_override, gena_enabled, try_connect);
        ds.start_polling();

        self.wire_and_insert(&ds, uuid);
        ds
    }

    /// Create (if not already tracked) a `DeviceState` purely from identity
    /// — no config-derived parameters at all, deliberately: TLS mode
    /// defaults to `TlsMode::HttpsWiiM` here since this convenience
    /// wrapper has no way to know a device's actual remembered mode
    /// (playback/mute access overrides come via `configure-device`
    /// instead). The picker-list tracking below, which *does* know the real
    /// per-device `TlsMode` (from its config-derived seed), calls
    /// `create_and_configure()` directly with it rather than going through
    /// this wrapper — currently unused as a result, kept in case a future
    /// caller wants the identity-only convenience path.
    pub fn add_known_device(&self, uuid: &str, ip: &str) -> DeviceState {
        self.create_and_configure(uuid, ip, TlsMode::HttpsWiiM)
    }

    /// Shared by `add_known_device()` and this module's own SSDP-driven/
    /// manual-add creation below (which know the real probed `TlsMode`,
    /// unlike `add_known_device()`'s hardcoded default) — one
    /// creation+configure path, not two, so overrides are resolved
    /// identically regardless of what triggered creation. Fires
    /// `configure-device` synchronously, then reads back whatever the
    /// connected handler set via `set_playback_access_override()`/
    /// `set_mute_access_override()`/`set_loop_mode_access_override()`/
    /// `set_gena_enabled()` before making first contact
    /// (`set_device(..., connect_now: true)`).
    pub fn create_and_configure(&self, uuid: &str, ip: &str, tls: TlsMode) -> DeviceState {
        if let Some(ds) = self.lookup(uuid) {
            return ds;
        }

        let ds = DeviceState::new(self.rt(), uuid.to_string());
        ds.set_manager(self);
        self.emit_by_name::<()>("configure-device", &[&ds]);
        let access_override           = ds.playback_access_override();
        let mute_access_override      = ds.mute_access_override();
        let loop_mode_access_override = ds.loop_mode_access_override();
        let gena_enabled              = ds.gena_enabled();
        ds.set_device(ip, tls, access_override, mute_access_override, loop_mode_access_override, gena_enabled, true);
        ds.start_polling();

        self.wire_and_insert(&ds, uuid);
        ds
    }

    /// Look up an already-tracked `DeviceState` by uuid — doesn't create
    /// one. `None` means this registry doesn't know this uuid at all yet.
    /// Callers wanting `Full` mode call `.acquire_full()` on the result
    /// themselves (see `DeviceState::acquire_full()`) — not baked into a
    /// `mode` parameter here, since acquiring is inherently a "hold this
    /// guard for a while" operation the caller (a device window) owns the
    /// lifetime of, not something `get_state()` itself could sensibly do
    /// on the caller's behalf.
    pub fn get_state(&self, uuid: &str) -> Option<DeviceState> {
        let uuid = crate::device::utils::normalize_uuid(uuid);
        self.imp().states.borrow().get(&uuid).cloned()
    }

    /// Look up an already-tracked `DeviceState` by address.
    ///
    /// The fallback for resolving a group member: a leader's slave list
    /// reports each member's uuid *and* address, and the two do not always
    /// agree with what the member itself reports — a member found this way
    /// can be driven through its own `DeviceState` rather than relayed
    /// through the leader, which is both faster and rate-limit-free.
    ///
    /// Address is a weaker key than uuid (it moves on a DHCP lease change),
    /// so this is deliberately only a fallback, never the primary lookup.
    pub fn get_state_by_ip(&self, ip: &str) -> Option<DeviceState> {
        if ip.is_empty() { return None; }
        self.imp().states.borrow().values()
            .find(|ds| ds.ip() == ip)
            .cloned()
    }

    /// Calls `f` for every currently-tracked `DeviceState`. Used to re-push
    /// an app-wide setting change (the GENA on/off switch) to every known
    /// device at once, rather than only the one `DeviceState` a Settings
    /// window happens to be scoped to.
    pub fn for_each_live(&self, f: impl Fn(&DeviceState)) {
        for ds in self.imp().states.borrow().values() {
            f(ds);
        }
    }

    /// Push a possibly-new `ip`/`tls` to the live `DeviceState` for `uuid`,
    /// if one exists and it isn't already using this IP.
    ///
    /// `get()`/`create_and_configure()` only resolve `ip`/`tls` when
    /// creating a *new* `DeviceState`; an already-open device window keeps
    /// polling whatever IP it connected with, even after discovery learns
    /// the device moved (DHCP lease change). Call this whenever discovery
    /// reports a device's current address — e.g. from this registry's own
    /// `list-changed` handler — so an open window reconnects to the right
    /// IP instead of retrying a dead one forever.
    pub fn update_ip(&self, uuid: &str, ip: &str, tls: TlsMode) {
        let ds = {
            let states = self.imp().states.borrow();
            states.get(uuid).cloned()
        };
        if let Some(ds) = ds {
            if ds.ip() != ip {
                // Preserve the current overrides across the reconnect —
                // set_device() resets everything else too, and a device
                // simply moving to a new IP shouldn't lose them.
                let access_override           = ds.playback_access_override();
                let mute_access_override      = ds.mute_access_override();
                let loop_mode_access_override = ds.loop_mode_access_override();
                let gena_enabled              = ds.gena_enabled();
                // Identity verification no longer needs an explicit
                // `expected_uuid` opt-in — `ds` was looked up by `uuid`, so
                // its own fixed `uuid()` already equals it, and
                // `fetch_device_info()` checks that unconditionally now.
                // Always connect_now: discovery just confirmed a moved IP
                // for an already-live DeviceState, not a device the device
                // list merely still believes offline.
                ds.set_device(ip, tls, access_override, mute_access_override, loop_mode_access_override, gena_enabled, true);
            }
        }
    }

    /// Shared look-up prefix for `get()`/`create_and_configure()` — returns
    /// the existing entry for `uuid`, if there is one. No pruning needed:
    /// with strong storage, an entry only ever leaves via `forget()`.
    fn lookup(&self, uuid: &str) -> Option<DeviceState> {
        self.imp().states.borrow().get(uuid).cloned()
    }

    /// Shared map-insertion tail, used by `get()` and
    /// `create_and_configure()` alike. Caller must already have checked
    /// `!uuid.is_empty()`. No `offline_cb` wiring here (deliberately, as of
    /// the devlist merge — `DeviceState::set_offline_callback()` still
    /// exists for `--connect`/testing-mode standalone use, but nothing in
    /// the normal app path registers one anymore): with no external
    /// watcher, `DeviceState::report_failure()` falls through to mutating
    /// `connection_state` locally, and `maybe_self_reconnect()` (see its
    /// own doc comment) is the fallback that brings it back — the intended
    /// behavior once the picker-list backend stopped independently
    /// health-checking, not a regression.
    fn wire_and_insert(&self, ds: &DeviceState, uuid: &str) {
        self.imp().states.borrow_mut().insert(uuid.to_string(), ds.clone());
    }

    /// Drop `states`' own strong reference to `uuid` alone, without
    /// touching picker-list metadata — the low-level half `forget()` below
    /// builds on. Not `pub`: nothing outside this module has a reason to
    /// drop just the `DeviceState` while leaving stale picker metadata
    /// behind (and nothing today needs to drop a `states`-only entry, e.g.
    /// `--connect`'s, at all — see this module's own doc comment).
    fn drop_state(&self, uuid: &str) {
        self.imp().states.borrow_mut().remove(uuid);
    }

    // ── Picker-list tracking ────────────────────────────────────────────

    /// Hand in a config-derived snapshot — this module's only view of
    /// config, since it can't read config itself. Must be called exactly
    /// once, before `start()`. Stores the full `seed` map (keyed by uuid) in
    /// `Inner`; `start()` is what actually eagerly tracks every entry in it
    /// (known-by-default — see this module's own doc comment) that already
    /// has an address to connect to.
    ///
    /// A boot-time-only snapshot (never refreshed after this call) is
    /// safe, not just convenient: the app is the sole config writer while
    /// running, and `seed` is only ever consulted for a uuid *not yet*
    /// tracked (`on_discovery_updated()`) — once a device becomes tracked
    /// it's kept live via `on_tracked_device_changed()` instead, never
    /// falling back to `seed` again.
    pub fn load_seed(&self, seed: Vec<SeedEntry>) {
        let mut inner = self.imp().inner.borrow_mut();
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
    ///
    /// `display_name` resolves a uuid to what should be shown for it — this
    /// module cannot read `config` itself (same rule as everywhere else in
    /// `device/`), so it asks rather than reads; `ui::name_resolver_for()`
    /// builds the resolver every real caller passes
    /// (`display_name_for()` — a device's own reported name, falling back to
    /// what config remembers). Every returned `ManagedEntry.name` — standalone,
    /// group header (via `group::auto_group_name()`), or a tracked member's own
    /// line — is resolved through this, never read from a cached field.
    pub fn entries(&self, display_name: &dyn Fn(&str) -> String) -> Vec<ManagedEntry> {
        let inner = self.imp().inner.borrow();
        // `filter_map`, not `map`: a tracked record with no matching
        // `states` entry shouldn't happen (this module always creates
        // through `create_and_configure()` and always `forget()`s both
        // together), but skipping it rather than unwrapping keeps a
        // violated invariant from becoming a panic — the row just silently
        // doesn't render that tick.
        let inputs: Vec<TopoInput> = inner.devices.iter()
            .filter_map(|(key, r)| {
                let ds = self.get_state(key)?;
                Some(TopoInput {
                    key: key.clone(), uuid: r.entry.uuid.clone(),
                    name: display_name(&r.entry.uuid), ip: r.entry.ip.clone(),
                    group: ds.group_state(),
                })
            })
            .collect();
        resolve_topology(inputs).into_iter()
            .map(|row| build_entry_for_row(&inner.devices, self, row))
            .collect()
    }

    /// Single-entry counterpart to `entries()` — used by the
    /// `song-info-changed` handler to refresh just one row's content
    /// without recomputing (or rebuilding widgets for) every other tracked
    /// device. `key` is the device's uuid, same as `entries()`'s rows are
    /// implicitly keyed by for row-widget lookup purposes.
    pub fn entry_for(&self, key: &str) -> Option<ManagedEntry> {
        let inner = self.imp().inner.borrow();
        let r = inner.devices.get(key)?;
        let ds = self.get_state(key)?;
        Some(build_managed_entry(r, &ds))
    }

    /// The tracked `DeviceState` for `key` — cheap to clone (GObject
    /// refcount). Used for a picker row's volume/mute control, which talks
    /// to the device directly rather than going through `ManagedEntry`
    /// (volume isn't part of the rendered snapshot anywhere else).
    ///
    /// Existence-checked against the picker-tracked set first, not just
    /// handed straight to `get_state()` — `states` can hold devices the
    /// picker list never tracked at all (e.g. `--connect`'s), and this
    /// accessor answers "is the picker list showing this," not "does the
    /// registry know this uuid."
    pub fn device_state_for(&self, key: &str) -> Option<DeviceState> {
        if !self.imp().inner.borrow().devices.contains_key(key) {
            return None;
        }
        self.get_state(key)
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
            let presence = format!("{:?}", rec.entry.presence);
            dbg(&format!(
                "  {:<24} {:<17} {presence:<8} key={key:?}",
                rec.entry.name, rec.entry.ip,
            ));
        }
    }

    /// Explicitly forgets a device — the only way a picker-tracked device
    /// leaves the registry now that retention is known-by-default (see
    /// this module's own doc comment). No-op if `uuid` isn't picker-tracked
    /// (in particular, this cannot forget a `states`-only entry like
    /// `--connect`'s — see `drop_state()`'s own doc comment for why nothing
    /// needs that today).
    ///
    /// Drops the picker metadata record *and* `states`' own strong
    /// reference — the only one left, since this registry retains strongly.
    /// That still doesn't guarantee the underlying `DeviceState` actually
    /// finalises — a device or settings window may hold a clone of its own
    /// — and this method knows nothing about windows or config either way.
    /// The caller (`ui/mod.rs`, which owns both) must close any such window
    /// *before* calling this — see that module's own `forget_device()`, the
    /// one place every part of removal happens together in the right order
    /// — and delete the uuid's config entry afterward, or the device simply
    /// reappears the next time `load_seed()`/`start()` runs.
    pub fn forget(&self, uuid: &str) {
        let removed = self.imp().inner.borrow_mut().devices.remove(uuid).is_some();
        if removed {
            self.drop_state(uuid);
            dbg(&format!("forget: {uuid}"));
            self.emit_list_changed();
        }
    }

    /// Add a manually-discovered device (already confirmed alive by the caller).
    pub fn add_manual(&self, name: String, ip: String, uuid: String, tls_mode: TlsMode) {
        if self.imp().inner.borrow().devices.contains_key(&uuid) {
            dbg(&format!("add manual: already known {name} ({ip}) uuid={uuid:?}"));
            return;
        }
        dbg(&format!("add manual: {name} ({ip}) uuid={uuid:?}"));
        self.track_device(&uuid, &uuid, &ip, tls_mode, name, String::new(), String::new(), String::new());
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
    /// track it under (see the codebase-wide "no key, no tracking" rule),
    /// and it still renders correctly via
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
            if m.uuid.is_empty() {
                dbg(&format!("group: skipping member with no uuid {} ({})", m.name, m.ip));
                continue;
            }
            if !group::member_is_directly_reachable(m) {
                // Not a dead end for the member — it stays in the group's
                // topology and is still controllable by relay through its
                // leader (see `group::member_is_relayable()`); it just
                // can't have a `DeviceState` of its own, since nothing
                // here can open a connection to it.
                dbg(&format!("group: not adopting unroutable member {} ({}) — relay only", m.name, m.ip));
                continue;
            }
            if self.imp().inner.borrow().devices.contains_key(&m.uuid) {
                continue;
            }
            dbg(&format!("group: adopting member {} ({}) uuid={:?}", m.name, m.ip, m.uuid));
            self.track_device(
                &m.uuid, &m.uuid, &m.ip, tls,
                m.name.clone(), String::new(), String::new(), String::new(),
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
    /// the device's uuid; use `entry_for(key)` to get its fresh content.
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
    /// for that, as it fires on every subsequent change.
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
        key: &str, uuid: &str, ip: &str, tls: TlsMode,
        name: String, model: String, project: String, firmware: String,
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
            self.create_and_track(key, uuid, ip, tls, name, model, project, firmware);
            return;
        };
        if rec.entry.ip != ip || rec.entry.tls_mode != tls {
            dbg(&format!("track_device: {} moved {} → {ip}", rec.entry.name, rec.entry.ip));
            rec.entry.ip = ip.to_string();
            rec.entry.tls_mode = tls;
            // Covers any live DeviceState for this uuid, not just this
            // entry — e.g. an already-open device window reconnects to
            // the corrected IP too.
            self.update_ip(uuid, ip, tls);
        }
    }

    /// The actual creation half of `track_device()`, split out only so its
    /// `inner` borrow (above) can drop cleanly before this runs —
    /// `create_and_configure()` can re-enter this same registry
    /// synchronously via the `configure-device` signal's connected handler
    /// (`ui::AppState`'s — but `device-changed` firing on the very first
    /// poll tick, before `create_and_configure()` even returns, is close
    /// enough to a real risk to just not hold the borrow across the call at
    /// all).
    #[allow(clippy::too_many_arguments)]
    fn create_and_track(
        &self,
        key: &str, uuid: &str, ip: &str, tls: TlsMode,
        name: String, model: String, project: String, firmware: String,
    ) {
        dbg(&format!("track_device: new {name} ({ip}) uuid={uuid:?} key={key:?}"));
        let ds = self.create_and_configure(uuid, ip, tls);
        ds.configure_simple_mode(true);
        let entry = ManagedEntry {
            uuid: uuid.to_string(), name, model, project, firmware,
            ip: ip.to_string(), tls_mode: tls,
            presence: DevicePresence::compute(ds.connection_state()),
            song_info_enabled: true,
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
        // `song-info-changed`'s doc comment in `signals()`).
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
            mgr.emit_song_info_changed(&key_for_song_info, mask);
        });
        self.imp().inner.borrow_mut().devices.insert(key.to_string(), DeviceRecord { entry });
    }

    /// Fired whenever a tracked device's `DeviceState` emits
    /// `device-changed` — i.e. it just connected, just failed, or its
    /// identity was otherwise confirmed/updated. Refreshes this record's
    /// rendering fields from the live `DeviceState` (never a redundant
    /// separate probe — `ds` already did the work) and always re-renders
    /// (`ui/`'s `list-changed` listener persists identity changes back to
    /// config unconditionally — this module doesn't gate that itself, since
    /// it can't touch config at all).
    fn on_tracked_device_changed(&self, key: &str, ds: &DeviceState) {
        {
            let mut inner = self.imp().inner.borrow_mut();
            let Some(rec) = inner.devices.get_mut(key) else {
                dbg(&format!("device-changed: {key} no longer tracked, ignoring"));
                return;
            };
            let new_presence = DevicePresence::compute(ds.connection_state());
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
        self.emit_list_changed();
    }

    fn on_discovery_updated(&self, svc: &DiscoveryService) {
        let discovered = svc.devices();

        // Snapshot, not a live borrow held across `track_device()` below
        // (which re-borrows `inner` itself) — see `load_seed()`'s doc
        // comment for why a boot-time-only seed is safe to keep consulting
        // like this for the lifetime of the app.
        let seed = self.imp().inner.borrow().seed.clone();
        for dev in &discovered {
            let cached = seed.get(&dev.uuid);
            let name  = cached.and_then(|c| c.name.clone())
                .filter(|n| !n.is_empty())
                .unwrap_or_else(|| dev.name.clone());
            let model    = cached.and_then(|c| c.model.clone()).unwrap_or_default();
            let project  = cached.and_then(|c| c.project.clone()).unwrap_or_default();
            let firmware = cached.and_then(|c| c.firmware.clone()).unwrap_or_default();
            self.track_device(&dev.uuid, &dev.uuid, &dev.ip, dev.tls_mode, name, model, project, firmware);
        }

        self.emit_list_changed();
    }

    /// Eagerly track every seeded device that already has an address to
    /// connect to — known-by-default means the whole config-derived seed,
    /// not just a `pinned || window_open` subset (see this module's own doc
    /// comment). A seed entry with no `last_ip` yet is skipped here and
    /// left for `on_discovery_updated()`'s enrichment path to pick up once
    /// SSDP actually finds it. Called once from `start()`, after
    /// `load_seed()` has already populated `Inner.seed`.
    fn track_seeded_devices(&self) {
        let seed = self.imp().inner.borrow().seed.clone();
        for entry in seed.values() {
            let Some(ref ip) = entry.last_ip else { continue };
            if self.imp().inner.borrow().devices.contains_key(&entry.uuid) { continue; }
            let name     = entry.name.clone().unwrap_or_else(|| format!("Device @ {ip}"));
            let model    = entry.model.clone().unwrap_or_default();
            let project  = entry.project.clone().unwrap_or_default();
            let firmware = entry.firmware.clone().unwrap_or_default();
            dbg(&format!("seed: {name} ({ip}) uuid={}", entry.uuid));
            self.track_device(&entry.uuid, &entry.uuid, ip, entry.tls_mode, name, model, project, firmware);
        }
    }
}

/// One tracked device, as topology resolution sees it — identity, address,
/// display name and group role, and nothing else. Deliberately not
/// `ManagedEntry` (a full render snapshot dragging model/project/firmware/
/// tls_mode/presence/song_info_enabled/now_playing along for no reason the
/// algorithm below needs) and not `DeviceState` (a GObject the unit tests
/// would have to construct one of just to exercise pure data-shuffling).
/// `entries()` builds these from `Inner.devices`; the tests build them by
/// hand via `row()`.
struct TopoInput {
    key:   String,
    uuid:  String,
    name:  String,
    ip:    String,
    group: group::GroupState,
}

/// One resolved row — what `resolve_topology()` decided this device's place
/// in the list is. `entries()` maps each of these back to a full
/// `ManagedEntry` by looking `key` up in `Inner.devices` (or, for an
/// untracked member, building the minimal fallback the `None` case
/// describes below) — see `build_entry_for_row()`.
struct TopoRow {
    key:  String,
    uuid: String,
    name: String,
    ip:   String,
    role: EntryGroupRole,
}

/// Turns a flat set of tracked devices into the ordered, topology-annotated
/// list the device list renders.
///
/// Kept free of GObjects and of registry state so the rules below can be
/// tested directly — they are fiddly, and every one of them exists because
/// getting it wrong makes a device disappear from the list.
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
fn resolve_topology(rows: Vec<TopoInput>) -> Vec<TopoRow> {
    // uuid -> key, since a follower knows its leader only by uuid.
    // `r.uuid` is already canonical (normalized at its entry boundary), so
    // this is a plain lookup, not a re-normalization.
    let by_uuid: HashMap<String, String> = rows.iter()
        .filter(|r| !r.uuid.is_empty())
        .map(|r| (r.uuid.clone(), r.key.clone()))
        .collect();
    let is_leader: HashSet<String> = rows.iter()
        .filter(|r| r.group.role == group::GroupRole::Leader)
        .map(|r| r.key.clone())
        .collect();

    let absorbed: HashSet<String> = rows.iter()
        .filter(|r| r.group.role == group::GroupRole::Follower)
        .filter(|r| r.group.leader_uuid.as_deref()
            .and_then(|u| by_uuid.get(u))
            .is_some_and(|lk| is_leader.contains(lk)))
        .map(|r| r.key.clone())
        .collect();

    let tracked: HashMap<&str, &TopoInput> =
        rows.iter().map(|r| (r.key.as_str(), r)).collect();

    let mut heads: Vec<&TopoInput> = rows.iter()
        .filter(|r| !absorbed.contains(&r.key))
        .collect();
    // Sort on the name as it will be displayed, which for a group is its
    // group name rather than the leader's device name.
    heads.sort_by_cached_key(|r| match r.group.role {
        group::GroupRole::Leader => group::auto_group_name(&r.name, r.group.follower_count()),
        _ => r.name.clone(),
    });

    let mut out = Vec::with_capacity(rows.len());
    for input in heads {
        if input.group.role != group::GroupRole::Leader {
            out.push(TopoRow {
                key: input.key.clone(), uuid: input.uuid.clone(),
                name: input.name.clone(), ip: input.ip.clone(),
                role: EntryGroupRole::Standalone,
            });
            continue;
        }

        out.push(TopoRow {
            key: input.key.clone(), uuid: input.uuid.clone(),
            name: group::auto_group_name(&input.name, input.group.follower_count()),
            ip: input.ip.clone(),
            role: EntryGroupRole::GroupHeader { follower_count: input.group.follower_count() },
        });

        out.push(TopoRow {
            key: input.key.clone(), uuid: input.uuid.clone(),
            name: input.name.clone(), ip: input.ip.clone(),
            role: EntryGroupRole::Member { leader_key: input.key.clone(), tracked: true },
        });

        for m in input.group.members.iter() {
            let member_key = by_uuid.get(&m.uuid);
            let (key, uuid, name, ip) = match member_key.and_then(|k| tracked.get(k.as_str())) {
                Some(e) => (e.key.clone(), e.uuid.clone(), e.name.clone(), e.ip.clone()),
                // Reported by the leader but not tracked — nothing in
                // `Inner.devices` to look up, so `build_entry_for_row()`
                // falls back to a minimal offline entry built straight from
                // what the leader reported (see that function's doc
                // comment).
                None => (m.uuid.clone(), m.uuid.clone(), m.name.clone(), m.ip.clone()),
            };
            out.push(TopoRow {
                key, uuid, name, ip,
                role: EntryGroupRole::Member {
                    leader_key: input.key.clone(),
                    tracked:    member_key.is_some(),
                },
            });
        }
    }
    out
}

/// Resolves one `TopoRow` back into a full `ManagedEntry` — the counterpart
/// to `resolve_topology()` taking a `TopoInput` seam instead of a full
/// `ManagedEntry`. `devices` is `Inner.devices`; called once per row from
/// `entries()`.
///
/// A tracked row (`devices` has `row.key`) gets its real identity/presence/
/// now-playing content from `build_managed_entry()`, with `name`/`group_role`
/// overridden from the resolved row (a group header's name is the group's,
/// not the leader's own device name). A **member** row additionally drops
/// song info — member lines are compact by design (no artwork, no song),
/// regardless of what the underlying device would otherwise show — which is
/// why this is a plain function of `role`, not something `resolve_topology()`
/// itself needs to carry.
///
/// An **untracked** member row (`devices` has no entry for `row.key`, or —
/// shouldn't happen, see `entries()`'s own comment — its `DeviceState` has
/// gone missing from the registry) builds the minimal fallback entirely
/// from what the leader reported: `Offline` presence, because nothing has
/// ever actually reached it — the leader's word is not a connection.
fn build_entry_for_row(
    devices: &HashMap<String, DeviceRecord>,
    manager: &DeviceManager,
    row: TopoRow,
) -> ManagedEntry {
    let is_member = matches!(row.role, EntryGroupRole::Member { .. });
    let tracked = devices.get(&row.key).zip(manager.get_state(&row.key));
    match tracked {
        Some((rec, ds)) => {
            let mut entry = build_managed_entry(rec, &ds);
            entry.name = row.name;
            entry.group_role = row.role;
            if is_member {
                entry.song_info_enabled = false;
                entry.now_playing = None;
            }
            entry
        }
        None => ManagedEntry {
            uuid: row.uuid, name: row.name, model: String::new(),
            project: String::new(), firmware: String::new(), ip: row.ip,
            tls_mode: TlsMode::HttpsWiiM, presence: DevicePresence::Offline,
            song_info_enabled: false, now_playing: None,
            group_role: row.role,
        },
    }
}

/// Shared by `entries()`/`entry_for()` — one record's cached identity
/// fields plus a freshly-computed `now_playing` snapshot, gated on
/// the record's own presence.
fn build_managed_entry(r: &DeviceRecord, ds: &DeviceState) -> ManagedEntry {
    let mut entry = r.entry.clone();
    entry.song_info_enabled = true;
    entry.now_playing = (entry.presence == DevicePresence::Active)
        .then(|| compute_now_playing(ds));
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
    use crate::device::utils;

    /// `uuid` is normalized here for the same reason every real `TopoInput`
    /// holds a canonical one: it is only ever built from a source that
    /// normalized at its own entry boundary (`DeviceInfo`, SSDP, the config
    /// seed). Fixtures spell it raw so the tests can show the shapes the
    /// different sources actually produce.
    fn row(key: &str, uuid: &str, name: &str, ip: &str, group: group::GroupState) -> TopoInput {
        TopoInput {
            key: key.to_string(), uuid: utils::normalize_uuid(uuid),
            name: name.to_string(), ip: ip.to_string(), group,
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

    fn shape(out: &[TopoRow]) -> Vec<(String, &'static str)> {
        out.iter().map(|r| (r.name.clone(), match r.role {
            EntryGroupRole::Standalone      => "standalone",
            EntryGroupRole::GroupHeader { .. } => "header",
            EntryGroupRole::Member { .. }   => "member",
        })).collect()
    }

    #[test]
    fn ungrouped_devices_render_as_plain_rows_sorted_by_name() {
        let out = resolve_topology(vec![
            row("b", "BBB", "Zulu", "1.1.1.2", group::GroupState::default()),
            row("a", "AAA", "Alpha", "1.1.1.1", group::GroupState::default()),
        ]);
        assert_eq!(shape(&out), vec![
            ("Alpha".into(), "standalone"),
            ("Zulu".into(), "standalone"),
        ]);
    }

    #[test]
    fn a_group_becomes_a_header_followed_by_a_line_per_member() {
        let out = resolve_topology(vec![
            row("L", "LEAD", "WiiM WorkBu", "1.1.1.1",
                leader_of(vec![member("F1", "WiiM MiniBu", "1.1.1.2")])),
            row("F", "F1", "WiiM MiniBu", "1.1.1.2", follower_of("LEAD")),
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
        assert!(!out.iter().any(|r| r.name == "WiiM MiniBu"
            && r.role == EntryGroupRole::Standalone));
    }

    #[test]
    fn member_lines_carry_their_leaders_key_and_are_flagged_tracked() {
        // "Drop song info" used to be asserted here too, back when this
        // test operated on full `ManagedEntry`s — that's now a two-line,
        // purely-a-function-of-`role` decision in `build_entry_for_row()`
        // (not something `resolve_topology()` itself computes anymore), so
        // it isn't `resolve_topology()`'s own behaviour to pin here. What
        // *is* still this function's job — and still fiddly enough to be
        // worth a dedicated assertion — is that every member line correctly
        // carries its leader's key and is flagged tracked.
        let out = resolve_topology(vec![
            row("L", "LEAD", "Lead", "1.1.1.1",
                leader_of(vec![member("F1", "Follower", "1.1.1.2")])),
            row("F", "F1", "Follower", "1.1.1.2", follower_of("LEAD")),
        ]);
        for r in out.iter().filter(|r| matches!(r.role, EntryGroupRole::Member { .. })) {
            match &r.role {
                EntryGroupRole::Member { leader_key, tracked } => {
                    assert_eq!(leader_key, "L");
                    assert!(*tracked, "{} should be tracked", r.name);
                }
                _ => unreachable!(),
            }
        }
    }

    #[test]
    fn a_member_the_leader_names_but_we_do_not_track_still_gets_a_line() {
        // Until discovery reaches it, the leader's word is all we have —
        // omitting it would show a two-device group as one device.
        let out = resolve_topology(vec![
            row("L", "LEAD", "Lead", "1.1.1.1",
                leader_of(vec![member("GHOST", "Unknown Speaker", "1.1.1.9")])),
        ]);
        assert_eq!(shape(&out), vec![
            ("Lead + 1".into(),        "header"),
            ("Lead".into(),            "member"),
            ("Unknown Speaker".into(), "member"),
        ]);
        let ghost = out.last().unwrap();
        assert_eq!(ghost.role, EntryGroupRole::Member { leader_key: "L".into(), tracked: false });
        assert_eq!(ghost.uuid, utils::normalize_uuid("GHOST"));
        assert_eq!(ghost.ip, "1.1.1.9");
    }

    #[test]
    fn a_follower_whose_leader_is_not_tracked_keeps_its_own_row() {
        // Otherwise a device that is perfectly reachable disappears from
        // the list because its leader happens to be offline or
        // undiscovered.
        let out = resolve_topology(vec![
            row("F", "F1", "Orphan", "1.1.1.2", follower_of("MISSING")),
        ]);
        assert_eq!(shape(&out), vec![("Orphan".into(), "standalone")]);
    }

    #[test]
    fn a_follower_pointing_at_a_device_that_is_not_a_leader_keeps_its_own_row() {
        // Stale state on one side of a group that has just been torn down:
        // absorbing it under a device that reports no members would produce
        // a header with nothing under it.
        let out = resolve_topology(vec![
            row("A", "AAA", "Alpha", "1.1.1.1", group::GroupState::default()),
            row("B", "BBB", "Bravo", "1.1.1.2", follower_of("AAA")),
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
            row("L", "FF98F7F4075B", "Lead", "1.1.1.1",
                leader_of(vec![member("uuid:ff98-0002", "Follower", "1.1.1.2")])),
            row("F", "uuid:FF98-0002", "Follower", "1.1.1.2",
                follower_of("uuid:ff98f7f4-075b")),
        ]);
        assert_eq!(shape(&out), vec![
            ("Lead + 1".into(), "header"),
            ("Lead".into(),     "member"),
            ("Follower".into(), "member"),
        ]);
        assert!(matches!(out[2].role, EntryGroupRole::Member { tracked: true, .. }));
    }

    #[test]
    fn groups_and_standalones_interleave_by_displayed_name() {
        // A group sorts under its *group* name, not the leader's device
        // name, so the list reads in the order it appears.
        let out = resolve_topology(vec![
            row("Z", "ZZZ", "Zulu", "1.1.1.3", group::GroupState::default()),
            row("L", "LEAD", "Mike", "1.1.1.1",
                leader_of(vec![member("F1", "Foxtrot", "1.1.1.2")])),
            row("A", "AAA", "Alpha", "1.1.1.4", group::GroupState::default()),
            row("F", "F1", "Foxtrot", "1.1.1.2", follower_of("LEAD")),
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
