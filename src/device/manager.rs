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
    ///   held by a consumer, that same object is returned.  The `ip`/`tls`
    ///   arguments are ignored (the device is already connected).
    /// * **New / stale entry**: a fresh `DeviceState` is created, connected,
    ///   polling is started, and a weak reference is stored.
    /// * **Empty UUID**: creates an uncached standalone `DeviceState`.
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
