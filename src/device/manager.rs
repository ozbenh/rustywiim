// Device-state manager — single source of truth for live DeviceState objects.
//
// `DeviceState` is a GObject, so it is already internally reference-counted.
// `DeviceManager` keeps a `WeakRef<DeviceState>` per UUID.  Callers that need
// a device state call `get(uuid, ip, tls)` and receive a strong (owned) ref.
// When every caller drops its ref the GObject is destroyed and the weak entry
// becomes stale; stale entries are pruned lazily on the next `get()` call.
//
// An empty UUID cannot be deduplicated (the device isn't identified yet); in
// that case `get()` returns a fresh, uncached `DeviceState` every time.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::Arc;

use glib::clone::Downgrade;
use gtk::glib;

use crate::device::api::TlsMode;
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
    ///   held by someone else, that same object is returned.  The `ip`/`tls`
    ///   arguments are ignored (the device is already connected).
    /// * **New entry**: a fresh `DeviceState` is created, connected to the
    ///   device, polling is started, and a weak reference is stored.
    /// * **Empty UUID**: the device is not yet identified; a standalone
    ///   `DeviceState` is created without caching it.
    pub fn get(&self, uuid: &str, ip: &str, tls: TlsMode) -> DeviceState {
        let mut states = self.0.states.borrow_mut();
        // Prune stale entries lazily so the map doesn't grow unboundedly.
        states.retain(|_, w| w.upgrade().is_some());

        if !uuid.is_empty() {
            if let Some(ds) = states.get(uuid).and_then(|w| w.upgrade()) {
                return ds;
            }
        }

        let ds = DeviceState::new(self.0.rt.clone());
        ds.set_device(ip, tls, None);
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
}
