// Device-state manager — single source of truth for live DeviceState objects.
//
// `DeviceManager` keeps a `WeakRef<DeviceState>` per UUID.  The DeviceState
// lives as long as at least one consumer (device window, settings window, …)
// holds a strong ref.  When the last consumer drops its ref the GObject is
// finalised, polling stops, and the weak entry here goes stale.
//
// On re-open, `get()` finds the stale entry, creates a fresh DeviceState, and
// the new window's `populate_all()` call handles the initial blank state
// (showing "Connecting…" until the first poll result arrives).
//
// An empty UUID cannot be deduplicated; `get()` returns a fresh uncached
// DeviceState every time for those.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::Arc;

use glib::clone::Downgrade;
use gtk::glib;

use crate::device::api::TlsMode;
use crate::device::playback::AccessMethod;
use crate::device::state::DeviceState;

struct Inner {
    rt:     Arc<tokio::runtime::Runtime>,
    states: RefCell<HashMap<String, glib::WeakRef<DeviceState>>>,
    /// Set once by the caller (`ui::AppState`, at startup) via
    /// `set_offline_hook()`. Wired onto every newly-created `DeviceState` in
    /// `get()` (see `DeviceState::set_offline_callback`'s doc comment) so a
    /// `DeviceState` noticing its own connection failure can tell the
    /// caller immediately, without this module needing to know anything
    /// about what the caller actually does with it (`ui::devlist`'s health
    /// check, in practice — kept out of this module since `device/` mustn't
    /// depend on `ui/`).
    offline_hook: RefCell<Option<Rc<dyn Fn(String)>>>,
}

/// Cheap-to-clone handle to the device-state registry.
#[derive(Clone)]
pub struct DeviceManager(Rc<Inner>);

impl DeviceManager {
    pub fn new(rt: Arc<tokio::runtime::Runtime>) -> Self {
        Self(Rc::new(Inner {
            rt,
            states: RefCell::new(HashMap::new()),
            offline_hook: RefCell::new(None),
        }))
    }

    /// Registers the hook invoked (with the device's uuid) whenever a
    /// `DeviceState` created by this manager notices its own connection
    /// failure — see `Inner::offline_hook`'s doc comment. Only ever set
    /// once, by `ui::AppState` at startup.
    pub fn set_offline_hook(&self, hook: impl Fn(String) + 'static) {
        *self.0.offline_hook.borrow_mut() = Some(Rc::new(hook));
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
    /// passes this based on devlist's current belief about the device: if
    /// devlist already thinks it's offline, there's no point immediately
    /// repeating a connection attempt that's already known to fail — the
    /// `DeviceState` sits configured-but-`Disconnected` until devlist's
    /// health check confirms otherwise (`mark_reachable()`). Ignored (always
    /// `true`) for an empty uuid — devlist has no presence to consult for a
    /// device it doesn't know about (`--connect`/a brand new manual add),
    /// so there's nothing to defer to.
    ///
    /// `access_override`/`mute_access_override` take the same
    /// `Option<AccessMethod>` shape `DeviceState::set_playback_access_override()`/
    /// `set_mute_access_override()` already use — already `config`-free on
    /// their own (this module can't depend on `config`, main-binary-crate
    /// only, kept out of the reusable device layer the CLI tools link
    /// against), so the caller (the only one there is: `ui/mod.rs`'s
    /// `DeviceWindow::new_for_device()`, which already has the per-device
    /// config in hand at this exact point) can pass
    /// `config::DeviceConfig::playback_access_override`/`mute_access_override`
    /// straight through with no conversion step.
    pub fn get(
        &self,
        uuid: &str,
        ip: &str,
        tls: TlsMode,
        access_override: Option<AccessMethod>,
        mute_access_override: Option<AccessMethod>,
        try_connect: bool,
    ) -> DeviceState {
        let mut states = self.0.states.borrow_mut();
        // Prune stale entries lazily so the map doesn't grow unboundedly.
        states.retain(|_, w| w.upgrade().is_some());

        if !uuid.is_empty() {
            if let Some(ds) = states.get(uuid).and_then(|w| w.upgrade()) {
                return ds;
            }
        }

        let ds = DeviceState::new(self.0.rt.clone(), uuid.to_string());
        ds.set_device(ip, tls, access_override, mute_access_override, try_connect || uuid.is_empty());
        ds.start_polling();

        if !uuid.is_empty() {
            // Only a uuid-keyed DeviceState can be looked back up by
            // mark_offline()/mark_reachable() at all, so only these get the
            // callback wired — an empty-uuid DeviceState (first-ever
            // connect, uuid not resolved until getStatusEx answers) has no
            // key the hook's caller could act on anyway.
            if let Some(hook) = self.0.offline_hook.borrow().clone() {
                let hook_uuid = uuid.to_string();
                ds.set_offline_callback(move || hook(hook_uuid.clone()));
            }
            states.insert(uuid.to_string(), ds.downgrade());
        }
        ds
    }

    /// Expose the tokio runtime for callers that need it directly.
    pub fn rt(&self) -> Arc<tokio::runtime::Runtime> {
        self.0.rt.clone()
    }

    /// Tell the live `DeviceState` for `uuid`, if any, that its
    /// reachability (as devlist understands it — the canonical source,
    /// per `ui::devlist`'s `DiscoveryManager`) just changed. No-op if
    /// there's no live `DeviceState` for this uuid (not open in any window
    /// right now). The single entry point for the devlist → `DeviceState`
    /// direction — see `DeviceState::mark_offline()`/`mark_reachable()`.
    pub fn sync_reachability(&self, uuid: &str, reachable: bool) {
        if uuid.is_empty() { return; }
        let ds = self.0.states.borrow().get(uuid).and_then(|w| w.upgrade());
        if let Some(ds) = ds {
            if reachable { ds.mark_reachable(); } else { ds.mark_offline(); }
        }
    }

    /// Push a possibly-new `ip`/`tls` to the live `DeviceState` for `uuid`,
    /// if one exists and it isn't already using this IP.
    ///
    /// `get()` only resolves `ip`/`tls` when creating a *new* `DeviceState`;
    /// an already-open device window keeps polling whatever IP it connected
    /// with, even after discovery learns the device moved (DHCP lease
    /// change). Call this whenever discovery reports a device's current
    /// address — e.g. from `DiscoveryManager`'s `list-changed` handler — so
    /// an open window reconnects to the right IP instead of retrying a dead
    /// one forever.
    pub fn update_ip(&self, uuid: &str, ip: &str, tls: TlsMode) {
        if uuid.is_empty() { return; }
        let ds = {
            let states = self.0.states.borrow();
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
}
