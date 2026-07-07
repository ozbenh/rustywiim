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
}

/// Cheap-to-clone handle to the device-state registry.
#[derive(Clone)]
pub struct DeviceManager(Rc<Inner>);

impl DeviceManager {
    pub fn new(rt: Arc<tokio::runtime::Runtime>) -> Self {
        Self(Rc::new(Inner {
            rt,
            states: RefCell::new(HashMap::new()),
        }))
    }

    /// Return a live `DeviceState` for `uuid` + `ip` + `tls`.
    ///
    /// * **Existing entry**: if a live `DeviceState` for this UUID is already
    ///   held by a consumer, that same object is returned.  The `ip`/`tls`/
    ///   `access_override` arguments are ignored (the device is already
    ///   connected and configured).
    /// * **New / stale entry**: a fresh `DeviceState` is created, given
    ///   `access_override` up front (before polling starts, so the very
    ///   first poll tick already uses it, not just ones after some later
    ///   caller happens to push it in), connected, polling is started, and
    ///   a weak reference is stored.
    /// * **Empty UUID**: creates an uncached standalone `DeviceState`.
    ///
    /// `access_override` takes the same `Option<AccessMethod>` shape
    /// `DeviceState::set_playback_access_override()` already uses — already
    /// `config`-free on its own (this module can't depend on `config`,
    /// main-binary-crate only, kept out of the reusable device layer the
    /// CLI tools link against), so the caller (the only one there is:
    /// `ui/mod.rs`'s `DeviceWindow::new_for_device()`, which already has
    /// the per-device config in hand at this exact point) can pass
    /// `config::DeviceConfig::playback_access_override` straight through
    /// with no conversion step.
    pub fn get(&self, uuid: &str, ip: &str, tls: TlsMode, access_override: Option<AccessMethod>) -> DeviceState {
        let mut states = self.0.states.borrow_mut();
        // Prune stale entries lazily so the map doesn't grow unboundedly.
        states.retain(|_, w| w.upgrade().is_some());

        if !uuid.is_empty() {
            if let Some(ds) = states.get(uuid).and_then(|w| w.upgrade()) {
                return ds;
            }
        }

        let ds = DeviceState::new(self.0.rt.clone());
        ds.set_device(ip, tls, None, access_override);
        ds.start_polling();

        if !uuid.is_empty() {
            states.insert(uuid.to_string(), ds.downgrade());
        }
        ds
    }

    /// Expose the tokio runtime for callers that need it directly.
    pub fn rt(&self) -> Arc<tokio::runtime::Runtime> {
        self.0.rt.clone()
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
                // Preserve the current override across the reconnect —
                // set_device() resets everything else too, and a device
                // simply moving to a new IP shouldn't lose it.
                let access_override = ds.playback_access_override();
                ds.set_device(ip, tls, Some(uuid), access_override);
            }
        }
    }
}
