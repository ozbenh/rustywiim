// Device-state registry — single source of truth for live DeviceState
// objects, and the sole strong holder of each.
// `DeviceManager` keeps a `DeviceState` per UUID for as long as it's
// known — there is no independent lifetime tracking beyond that anymore;
// `track()`'s callers (device windows, settings windows,
// `device::discovery_manager`'s picker-list tracking, …) borrow it, they
// don't own it. The only way an entry leaves is `forget()`, an explicit
// removal — "known by default, forgetting is the explicit act" applies here
// exactly as it does to `discovery_manager`'s own tracked-device set (see
// that module's doc comment), since the two are converging into one registr
//
// Every caller resolves a real uuid before reaching this registry — nothing
// without one may ever be tracked (see `device::discovery::ProbeFailure`'s
// doc comment and `device::discovery_manager::adopt_group_members()`'s
// no-uuid skip) — so `get()`/`create_and_configure()` need no empty-uuid
// special case. `DeviceState::detached()` (Kiosk's "no device bound"
// placeholder) is the one legitimate keyless `DeviceState` in the app, and it
// bypasses this registry entirely rather than being an exception here.
//
// `configure-device` (param: the freshly-created `DeviceState`) fires
// synchronously, before first contact, for every `DeviceState` this manager
// creates via `create_and_configure()`/`add_known_device()` (not `get()`,
// whose callers already resolve config before calling in) — `ui/`'s only
// listener resolves per-device config (TLS/access overrides; `device/` can't
// read config itself) and pushes it onto the fresh instance. SSDP
// consumption, presence computation, and the picker-list tracking itself
// live in `device::discovery_manager` — this module only owns the
// `DeviceState` registry.

use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::Arc;

use glib::prelude::*;
use glib::subclass::prelude::*;

use crate::device::api::TlsMode;
use crate::device::playback::AccessMethod;
use crate::device::state::DeviceState;

mod imp {
    use super::*;
    use glib::subclass::Signal;
    use std::sync::OnceLock;

    pub struct DeviceManager {
        pub(super) rt:     std::cell::OnceCell<Arc<tokio::runtime::Runtime>>,
        pub(super) states: RefCell<HashMap<String, DeviceState>>,
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
    /// passes this based on devlist's current belief about the device
    /// (`ManagedEntry::presence`, computed from its own tracked
    /// `DeviceState::connection_state()`): if devlist already believes it's
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
    /// instead). `device::discovery_manager`, which *does* know the real
    /// per-device `TlsMode` (from its config-derived seed), calls
    /// `create_and_configure()` directly with it rather than going through
    /// this wrapper — currently unused as a result, kept in case a future
    /// caller wants the identity-only convenience path.
    pub fn add_known_device(&self, uuid: &str, ip: &str) -> DeviceState {
        self.create_and_configure(uuid, ip, TlsMode::HttpsWiiM)
    }

    /// Shared by `add_known_device()` and `device::discovery_manager`'s
    /// SSDP-driven/manual-add creation (which know the real probed
    /// `TlsMode`, unlike `add_known_device()`'s hardcoded default) — one
    /// creation+configure path, not two, so overrides are resolved
    /// identically regardless of what triggered creation. `pub` (not
    /// `add_known_device`-only) so `device::discovery_manager` can pass
    /// its own resolved `tls`. Fires
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
    /// one. `None` means this manager doesn't know this uuid at all yet.
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
    /// reports a device's current address — e.g. from `DiscoveryManager`'s
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
                // for an already-live DeviceState, not a device devlist
                // merely still believes offline.
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

    /// Drop this registry's strong reference to `uuid`, removing it from
    /// the known set — the **only** way an entry leaves (see this module's
    /// own doc comment). No-op if `uuid` isn't tracked.
    ///
    /// This alone does not guarantee the underlying `DeviceState` actually
    /// finalises — a device window or settings window may still hold its
    /// own strong clone. Callers that need the device to actually stop
    /// polling (the device-list trashcan's "forget this device," not a
    /// device window's ordinary close) must close every such window
    /// *before* calling this — see `ui/mod.rs`'s `forget_device()`, the one
    /// place that ordering is enforced.
    pub fn forget(&self, uuid: &str) {
        self.imp().states.borrow_mut().remove(uuid);
    }
}
