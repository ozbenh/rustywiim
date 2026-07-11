// Device-state manager — single source of truth for live DeviceState objects.
//
// `DeviceManager` keeps a `WeakRef<DeviceState>` per UUID.  The DeviceState
// lives as long as at least one consumer (device window, settings window,
// `ui::devlist`'s picker-list tracking, …) holds a strong ref.  When the
// last consumer drops its ref the GObject is finalised, polling stops, and
// the weak entry here goes stale.
//
// On re-open, `get()` finds the stale entry, creates a fresh DeviceState, and
// the new window's `populate_all()` call handles the initial blank state
// (showing "Connecting…" until the first poll result arrives).
//
// An empty UUID cannot be deduplicated; `get()` returns a fresh uncached
// DeviceState every time for those.
//
// `configure-device` (param: the freshly-created `DeviceState`) fires
// synchronously, before first contact, for every `DeviceState` this manager
// creates via `create_and_configure()`/`add_known_device()` (not `get()`,
// whose callers already resolve config before calling in) — `ui/`'s only
// listener resolves per-device config (TLS/access overrides; `device/` can't
// read config itself) and pushes it onto the fresh instance. SSDP
// consumption, presence computation, and config persistence for
// picker-list rendering all live in `ui::devlist` — this module only owns
// the `DeviceState` registry itself.

use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::Arc;

use glib::prelude::*;
use glib::subclass::prelude::*;
use gtk::glib;

use crate::device::api::TlsMode;
use crate::device::playback::AccessMethod;
use crate::device::state::DeviceState;

mod imp {
    use super::*;
    use glib::subclass::Signal;
    use std::sync::OnceLock;

    pub struct DeviceManager {
        pub(super) rt:     std::cell::OnceCell<Arc<tokio::runtime::Runtime>>,
        pub(super) states: RefCell<HashMap<String, glib::WeakRef<DeviceState>>>,
    }

    impl Default for DeviceManager {
        fn default() -> Self {
            Self {
                rt:     std::cell::OnceCell::new(),
                states: RefCell::new(HashMap::new()),
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
                ]
            })
        }
    }
}

glib::wrapper! {
    pub struct DeviceManager(ObjectSubclass<imp::DeviceManager>);
}

impl DeviceManager {
    pub fn new(rt: Arc<tokio::runtime::Runtime>) -> Self {
        let obj: Self = glib::Object::new();
        obj.imp().rt.set(rt).unwrap();
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
    /// * **Existing entry**: if a live `DeviceState` for this UUID is already
    ///   held by a consumer, that same object is returned.  The `ip`/`tls`/
    ///   `access_override`/`try_connect` arguments are ignored (the device
    ///   is already connected and configured).
    /// * **New / stale entry**: a fresh `DeviceState` is created, given
    ///   `access_override` up front (before polling starts, so the very
    ///   first poll tick already uses it, not just ones after some later
    ///   caller happens to push it in), configured (`ip`/`tls`/client, and
    ///   an actual connection attempt too if `try_connect`), polling is
    ///   started, and a weak reference is stored.
    /// * **Empty UUID**: creates an uncached standalone `DeviceState`
    ///   (always with `try_connect` effectively forced true — see below).
    ///
    /// `try_connect` — whether to actually attempt a connection now
    /// (`DeviceState::set_device`'s `connect_now`). The caller (`ui/mod.rs`)
    /// passes this based on devlist's current belief about the device
    /// (`ManagedEntry::presence`, computed from its own tracked
    /// `DeviceState::connection_state()`): if devlist already believes it's
    /// offline, there's no point immediately repeating a connection attempt
    /// that's already known to fail — the fresh `DeviceState` sits
    /// configured-but-`Disconnected` until its own `maybe_self_reconnect()`
    /// (or an external `mark_reachable()` call, for a caller that wants to
    /// drive this itself) brings it back. Ignored (always `true`) for an
    /// empty uuid — devlist has no presence to consult for a device it
    /// doesn't know about (`--connect`/a brand new manual add), so there's
    /// nothing to defer to.
    ///
    /// `access_override`/`mute_access_override` take the same
    /// `Option<AccessMethod>` shape `DeviceState::set_playback_access_override()`/
    /// `set_mute_access_override()` already use — already `config`-free on
    /// their own (this module can't depend on `config`, main-binary-crate
    /// only, kept out of the reusable device layer the CLI tools link
    /// against), so the caller (currently `ui/mod.rs`'s
    /// `DeviceWindow::new_for_device()`, which already has the per-device
    /// config in hand at this exact point) can pass
    /// `config::DeviceConfig::playback_access_override`/`mute_access_override`
    /// straight through with no conversion step. Doesn't go through
    /// `configure-device` at all — this is the older, still-valid pattern
    /// of the caller resolving config *before* ever calling in, which
    /// `add_known_device()`/`create_and_configure()` below can't use since
    /// they're triggered without a synchronous config-aware caller in the
    /// loop.
    pub fn get(
        &self,
        uuid: &str,
        ip: &str,
        tls: TlsMode,
        access_override: Option<AccessMethod>,
        mute_access_override: Option<AccessMethod>,
        try_connect: bool,
    ) -> DeviceState {
        if let Some(ds) = self.lookup_and_prune(uuid) {
            return ds;
        }

        let ds = DeviceState::new(self.rt(), uuid.to_string());
        ds.set_device(ip, tls, access_override, mute_access_override, try_connect || uuid.is_empty());
        ds.start_polling();

        if !uuid.is_empty() {
            self.wire_and_insert(&ds, uuid);
        }
        ds
    }

    /// Create (if not already tracked) a `DeviceState` purely from identity
    /// — no config-derived parameters at all, deliberately: TLS mode
    /// defaults to `TlsMode::HttpsWiiM` here since this convenience
    /// wrapper has no way to know a device's actual remembered mode
    /// (playback/mute access overrides come via `configure-device`
    /// instead). `ui::devlist`, which *does* know the real per-device
    /// `TlsMode` from config, calls `create_and_configure()` directly with
    /// it rather than going through this wrapper. For pinned devices
    /// seeded from config at `ui::AppState`'s startup.
    pub fn add_known_device(&self, uuid: &str, ip: &str) -> DeviceState {
        self.create_and_configure(uuid, ip, TlsMode::HttpsWiiM)
    }

    /// Shared by `add_known_device()` and `ui::devlist`'s SSDP-driven/
    /// manual-add creation (which know the real probed `TlsMode`, unlike
    /// `add_known_device()`'s hardcoded default) — one creation+configure
    /// path, not two, so overrides are resolved identically regardless of
    /// what triggered creation. `pub` (not `add_known_device`-only) so
    /// `ui::devlist` can pass its own resolved `tls`. Fires
    /// `configure-device` synchronously, then reads back whatever the
    /// connected handler set via `set_playback_access_override()`/
    /// `set_mute_access_override()` before making first contact
    /// (`set_device(..., connect_now: true)`).
    pub fn create_and_configure(&self, uuid: &str, ip: &str, tls: TlsMode) -> DeviceState {
        if let Some(ds) = self.lookup_and_prune(uuid) {
            return ds;
        }

        let ds = DeviceState::new(self.rt(), uuid.to_string());
        self.emit_by_name::<()>("configure-device", &[&ds]);
        let access_override      = ds.playback_access_override();
        let mute_access_override = ds.mute_access_override();
        ds.set_device(ip, tls, access_override, mute_access_override, true);
        ds.start_polling();

        if !uuid.is_empty() {
            self.wire_and_insert(&ds, uuid);
        }
        ds
    }

    /// Look up an already-tracked `DeviceState` by uuid — doesn't create
    /// one. `None` means this manager doesn't know this uuid at all yet.
    /// Callers wanting `Full` mode call `.acquire_full()` on the result
    /// themselves (see `DeviceState::acquire_full()`) — not baked into a
    /// `mode` parameter here, since acquiring is inherently a "hold this
    /// guard for a while" operation the caller (a device window) owns the
    /// lifetime of, not something `get_state()` itself could sensibly do
    /// on the caller's behalf.
    pub fn get_state(&self, uuid: &str) -> Option<DeviceState> {
        if uuid.is_empty() { return None; }
        self.imp().states.borrow().get(uuid).and_then(|w| w.upgrade())
    }

    /// Push a possibly-new `ip`/`tls` to the live `DeviceState` for `uuid`,
    /// if one exists and it isn't already using this IP.
    ///
    /// `get()`/`create_and_configure()` only resolve `ip`/`tls` when
    /// creating a *new* `DeviceState`; an already-open device window keeps
    /// polling whatever IP it connected with, even after discovery learns
    /// the device moved (DHCP lease change). Call this whenever discovery
    /// reports a device's current address — e.g. from `DiscoveryManager`'s
    /// `list-changed` handler — so an open window reconnects to the right
    /// IP instead of retrying a dead one forever.
    pub fn update_ip(&self, uuid: &str, ip: &str, tls: TlsMode) {
        if uuid.is_empty() { return; }
        let ds = {
            let states = self.imp().states.borrow();
            states.get(uuid).and_then(|w| w.upgrade())
        };
        if let Some(ds) = ds {
            if ds.ip() != ip {
                // Preserve the current overrides across the reconnect —
                // set_device() resets everything else too, and a device
                // simply moving to a new IP shouldn't lose them.
                let access_override = ds.playback_access_override();
                let mute_access_override = ds.mute_access_override();
                // Identity verification no longer needs an explicit
                // `expected_uuid` opt-in — `ds` was looked up by `uuid`, so
                // its own fixed `uuid()` already equals it, and
                // `fetch_device_info()` checks that unconditionally now.
                // Always connect_now: discovery just confirmed a moved IP
                // for an already-live DeviceState, not a device devlist
                // merely still believes offline.
                ds.set_device(ip, tls, access_override, mute_access_override, true);
            }
        }
    }

    /// Shared prune-and-look-up prefix for `get()`/`create_and_configure()`
    /// — prunes stale (weak-ref-only, GC'd) entries lazily so the map
    /// doesn't grow unboundedly, then returns the existing entry for
    /// `uuid` if there is one. Empty `uuid` never matches (can't be
    /// deduplicated at all — see this module's own doc comment).
    fn lookup_and_prune(&self, uuid: &str) -> Option<DeviceState> {
        let mut states = self.imp().states.borrow_mut();
        states.retain(|_, w| w.upgrade().is_some());
        if uuid.is_empty() { return None; }
        states.get(uuid).and_then(|w| w.upgrade())
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
    /// behavior once `ui::devlist` stopped independently health-checking,
    /// not a regression.
    fn wire_and_insert(&self, ds: &DeviceState, uuid: &str) {
        self.imp().states.borrow_mut().insert(uuid.to_string(), ds.downgrade());
    }
}
