// Device-state manager — single source of truth for live DeviceState objects.
//
// `DeviceManager` holds a strong ref to each known DeviceState so polling
// continues and state is preserved regardless of how many windows are open.
// Multiple consumers (device windows, settings, device list) can all hold
// their own clone of the same DeviceState GObject.
//
// An empty UUID cannot be deduplicated; `get()` returns a fresh uncached
// DeviceState every time for those.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::Arc;

use crate::device::api::TlsMode;
use crate::device::state::DeviceState;

struct Inner {
    rt:     Arc<tokio::runtime::Runtime>,
    // Strong refs: DeviceState stays alive (and keeps polling) as long as the
    // DeviceManager exists, regardless of how many windows are open.  This
    // lets multiple consumers (device windows, settings, future device list)
    // share the same live state without any one of them being the sole owner.
    states: RefCell<HashMap<String, DeviceState>>,
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

    /// Return a `DeviceState` for `uuid` + `ip` + `tls`.
    ///
    /// * **Existing entry**: returns the already-running `DeviceState` for
    ///   this UUID.  The `ip`/`tls` arguments are ignored.
    /// * **New entry**: creates a fresh `DeviceState`, starts polling, and
    ///   stores a strong reference so the state outlives any single window.
    /// * **Empty UUID**: creates an uncached standalone `DeviceState`.
    pub fn get(&self, uuid: &str, ip: &str, tls: TlsMode) -> DeviceState {
        let mut states = self.0.states.borrow_mut();

        if !uuid.is_empty() {
            if let Some(ds) = states.get(uuid) {
                return ds.clone();
            }
        }

        let ds = DeviceState::new(self.0.rt.clone());
        ds.set_device(ip, tls, None);
        ds.start_polling();

        if !uuid.is_empty() {
            states.insert(uuid.to_string(), ds.clone());
        }
        ds
    }

    /// Expose the tokio runtime for callers that need it directly.
    pub fn rt(&self) -> Arc<tokio::runtime::Runtime> {
        self.0.rt.clone()
    }
}
