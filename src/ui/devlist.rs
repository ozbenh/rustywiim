#![allow(deprecated)] // glib clone! @strong syntax

/// Device-list window and its backing DiscoveryManager.
///
/// `DiscoveryManager` (GObject) subscribes to `DiscoveryService` and tracks
/// every known device (SSDP-discovered, pinned-and-config-remembered, or
/// manually added by IP) as a real, `device::manager::DeviceManager`-owned
/// `DeviceState` — holding a strong reference to it for as long as the
/// device is "known" (see `do_prune()`), which is what keeps it alive and
/// polling (Simple mode, unless a device window also holds an
/// `acquire_full()` guard) even with no window open. There is no separate
/// health-check poll: Simple-mode polling's own `getStatusEx` *is* the
/// liveness check now, and presence for rendering
/// (`DevicePresence::compute()`) is read straight off each tracked
/// `DeviceState::connection_state()`, not tracked independently. Recovery
/// after a failure is `DeviceState`'s own job (`maybe_self_reconnect()`,
/// `device/state.rs`) — nothing external pokes it anymore.
///
/// `DiscoveryWindow` renders that list and lets the user pin/unpin entries,
/// open device windows by double-clicking, and add devices manually by IP.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::Arc;

use adw::prelude::*;
use glib::clone;
use glib::subclass::prelude::*;
use gtk::{glib, Orientation};

use crate::config;
use crate::device::api::TlsMode;
use crate::device::discovery::{DEBUG_DISCOVERY, DiscoveredDevice, DiscoveryService};
use crate::device::manager::DeviceManager;
use crate::device::state::{ConnectionState, DeviceState};

/// `[disc-mgr]` — `DiscoveryManager`, the picker-list backend (this file's
/// `impl DiscoveryManager` block). Distinct from `device/discovery.rs`'s
/// `[discovery]` (the SSDP service itself) and from `DiscoveryWindow` (the
/// actual on-screen picker), which has no debug logging of its own.
fn dbg(msg: &str) {
    if DEBUG_DISCOVERY.load(std::sync::atomic::Ordering::Relaxed) {
        println!("[disc-mgr] {msg}");
    }
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
    /// resolve the device's profile default while offline (see
    /// `config::DeviceConfig::project`'s doc comment). Empty until the
    /// tracked `DeviceState` has connected at least once.
    pub project:  String,
    pub firmware: String,
    pub ip:       String,
    pub tls_mode: TlsMode,
    pub pinned:   bool,
    pub presence: DevicePresence,
}

// ── Internal record ───────────────────────────────────────────────────────────

/// One tracked device: cached rendering identity (refreshed from
/// `ds.device_info()`/`ds.capabilities()` whenever `ds` connects — see
/// `refresh_identity_from_device()`) plus the strong `DeviceState` handle
/// that keeps it alive/polling for as long as this record exists. `ds` is
/// what makes "forgetting a device" plain refcounting: dropping this
/// record (`do_prune()`) drops the last reference `ui::devlist` holds, and
/// once no device window holds one either, the `DeviceState` itself goes
/// away.
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
    /// The `device-changed` handler connected in `create_and_track()` is
    /// never explicitly disconnected — no `SignalHandlerId` kept for it —
    /// because `do_prune()` only ever drops a record while
    /// `has_open_window` is false, and `device::manager::DeviceManager`
    /// itself only ever keeps *weak* refs, so `ds` below is guaranteed to
    /// have no other strong holder at that point: dropping this record
    /// finalizes `ds` outright, connection included.
    ds: DeviceState,
}

struct Inner {
    devices: HashMap<String, DeviceRecord>,
}

impl Default for Inner {
    fn default() -> Self { Self { devices: HashMap::new() } }
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
                Signal::builder("list-changed").build(),
                // Fired once, synchronously in start(), after the initial config
                // load — before any async discovery results arrive.
                Signal::builder("initial-load").build(),
            ])
        }
    }
}

glib::wrapper! {
    pub struct DiscoveryManager(ObjectSubclass<mgr_imp::DiscoveryManager>);
}

impl DiscoveryManager {
    /// `device_manager` is a direct reference (not a hook/callback) —
    /// deliberate, now that this module owns the full picker-list
    /// backend rather than bridging to a separate one: `device/`'s
    /// `DeviceManager` is the registry every tracked `DeviceState` comes
    /// from, and there's no ownership-layering reason left to hide that
    /// behind indirection.
    pub fn new(rt: Arc<tokio::runtime::Runtime>, discovery: DiscoveryService, device_manager: DeviceManager) -> Self {
        let obj: Self = glib::Object::new();
        obj.imp().rt.set(rt).unwrap();
        obj.imp().discovery.set(discovery).unwrap();
        obj.imp().device_manager.set(device_manager).unwrap();
        obj
    }

    fn device_manager(&self) -> &DeviceManager {
        self.imp().device_manager.get().unwrap()
    }

    pub fn start(&self) {
        self.load_known_devices_from_config();
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
        let mut v: Vec<ManagedEntry> = self.imp().inner.borrow()
            .devices.values().map(|r| r.entry.clone()).collect();
        v.sort_by(|a, b| a.name.cmp(&b.name));
        v
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
            self.persist_pinned();
            self.do_prune();
            self.emit_list_changed();
        }
    }

    /// Records whether a device window is currently open for `uuid` — see
    /// `DeviceRecord::has_open_window`'s doc comment. Called by `ui::mod`'s
    /// `AppState` on window open/close. No-op if `uuid` is empty or
    /// unknown to devlist (a window with nothing here to mark — e.g. a
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
        self.persist_pinned();
        self.emit_list_changed();
    }

    pub fn connect_list_changed<F: Fn(&Self) + 'static>(&self, f: F) -> glib::SignalHandlerId {
        self.connect_local("list-changed", false, move |args| {
            f(&args[0].get::<Self>().unwrap());
            None
        })
    }

    /// Fired once, synchronously inside `start()`, after loading devices from
    /// config — before any async discovery results arrive.
    /// Use this to restore windows from config; do NOT use `list-changed` for
    /// that, as it fires on every subsequent change (e.g. pin toggles).
    pub fn connect_initial_load<F: Fn(&Self) + 'static>(&self, f: F) -> glib::SignalHandlerId {
        self.connect_local("initial-load", false, move |args| {
            f(&args[0].get::<Self>().unwrap());
            None
        })
    }

    /// Fires once when the underlying SSDP scan cycle completes (or the 4-second
    /// initial timeout expires with no devices found).  Use this — not
    /// `connect_list_changed` — to clear a "Scanning…" indicator, because
    /// devices already tracked from config would clear it prematurely.
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
    /// `load_known_devices_from_config()`/`add_manual()` all funnel
    /// through, so a record is always built/refreshed the same way
    /// regardless of what triggered it. `name`/`model`/`project`/
    /// `firmware` seed the entry's *rendering* fields for a record that
    /// doesn't exist yet (config-cached values, or whatever the SSDP/
    /// manual-add probe already had) — ignored if the record already
    /// exists, since `refresh_identity_from_device()` (wired via
    /// `device-changed`) is the one place identity fields get overwritten
    /// once `ds` has actually answered for real.
    #[allow(clippy::too_many_arguments)]
    fn track_device(
        &self,
        key: &str, uuid: &str, ip: &str, tls: TlsMode, pinned: bool,
        name: String, model: String, project: String, firmware: String,
        in_discovery: bool,
    ) {
        let moved = {
            let mut inner = self.imp().inner.borrow_mut();
            let Some(rec) = inner.devices.get_mut(key) else {
                drop(inner);
                self.create_and_track(key, uuid, ip, tls, pinned, name, model, project, firmware, in_discovery);
                return;
            };
            let moved = rec.entry.ip != ip || rec.entry.tls_mode != tls;
            if moved {
                dbg(&format!("track_device: {} moved {} → {ip}", rec.entry.name, rec.entry.ip));
                rec.entry.ip = ip.to_string();
                rec.entry.tls_mode = tls;
                // Covers any live DeviceState for this uuid, not just this
                // devlist entry — e.g. an already-open device window
                // reconnects to the corrected IP too.
                self.device_manager().update_ip(uuid, ip, tls);
            }
            rec.in_discovery = rec.in_discovery || in_discovery;
            moved
        };
        if moved { self.persist_pinned(); }
    }

    /// The actual creation half of `track_device()`, split out only so its
    /// `inner` borrow (above) can drop cleanly before this runs —
    /// `create_and_configure()` can re-enter this same `DiscoveryManager`
    /// synchronously via the `configure-device` signal's connected handler
    /// (`ui::AppState`'s, which doesn't touch devlist — but `device-changed`
    /// firing on the very first poll tick, before `create_and_configure()`
    /// even returns, is close enough to a real risk to just not hold the
    /// borrow across the call at all).
    #[allow(clippy::too_many_arguments)]
    fn create_and_track(
        &self,
        key: &str, uuid: &str, ip: &str, tls: TlsMode, pinned: bool,
        name: String, model: String, project: String, firmware: String,
        in_discovery: bool,
    ) {
        dbg(&format!("track_device: new {name} ({ip}) uuid={uuid:?} key={key:?}"));
        let ds = self.device_manager().create_and_configure(uuid, ip, tls);
        let entry = ManagedEntry {
            uuid: uuid.to_string(), name, model, project, firmware,
            ip: ip.to_string(), tls_mode: tls, pinned,
            presence: DevicePresence::compute(ds.connection_state(), pinned),
        };
        let weak = self.downgrade();
        let key_owned = key.to_string();
        ds.connect_device_changed(move |ds| {
            let Some(mgr) = weak.upgrade() else { return };
            mgr.on_tracked_device_changed(&key_owned, ds);
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
    /// offline and isn't pinned/open/in-discovery), persists if identity
    /// actually changed, and always re-renders.
    fn on_tracked_device_changed(&self, key: &str, ds: &DeviceState) {
        let mut needs_persist = false;
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
                if !info.device_name.is_empty() && rec.entry.name != info.device_name {
                    rec.entry.name = info.device_name.clone();
                    needs_persist = true;
                }
                if !info.project.is_empty() && rec.entry.project != info.project {
                    rec.entry.project = info.project.clone();
                    needs_persist = true;
                }
                if !info.firmware.is_empty() && rec.entry.firmware != info.firmware {
                    rec.entry.firmware = info.firmware.clone();
                    needs_persist = true;
                }
            }
            if let Some(caps) = ds.capabilities() {
                if !caps.model.is_empty() && rec.entry.model != caps.model {
                    rec.entry.model = caps.model.clone();
                    needs_persist = true;
                }
            }
        }
        self.do_prune();
        if needs_persist { self.persist_pinned(); }
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

        let known_devices = config::with(|c| c.devices.clone());
        for dev in &discovered {
            let key = device_key(&dev.uuid, &dev.ip);
            let cached = known_devices.get(&dev.uuid);
            let name  = cached.and_then(|c| c.name.clone())
                .filter(|n| !n.is_empty())
                .unwrap_or_else(|| dev.name.clone());
            let model    = cached.and_then(|c| c.model.clone()).unwrap_or_default();
            let project  = cached.and_then(|c| c.project.clone()).unwrap_or_default();
            let firmware = cached.and_then(|c| c.firmware.clone()).unwrap_or_default();
            let pinned = cached.map_or(false, |c| c.pinned == Some(true));
            self.track_device(&key, &dev.uuid, &dev.ip, dev.tls_mode, pinned, name, model, project, firmware, true);
        }

        let pruned = self.do_prune();
        self.emit_list_changed();
        let _ = pruned; // list-changed already covers both cases; kept named for clarity at call site
    }

    /// Remove entries that are `Dead` (not pinned, not `Connected`) and no
    /// longer visible in the SSDP discovery list, dropping devlist's
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

    fn load_known_devices_from_config(&self) {
        let devices = config::with(|cfg| cfg.devices.clone());
        for (uuid, dev_cfg) in &devices {
            let pinned = dev_cfg.pinned == Some(true);
            // Load pinned devices always.  Also pre-load non-pinned devices whose
            // window should reopen: the list-changed handler will open the window,
            // and if SSDP later confirms the device it stays; if not, do_prune
            // removes it (the window stays open independently).
            if !pinned && !dev_cfg.window_open { continue; }
            let Some(ref ip) = dev_cfg.last_ip else { continue };
            if self.imp().inner.borrow().devices.contains_key(uuid) { continue; }
            let tls = dev_cfg.tls_mode
                .map(|n| TlsMode::from_usize(n as usize))
                .unwrap_or(TlsMode::HttpsWiiM);
            let name     = dev_cfg.name.clone().unwrap_or_else(|| format!("Device @ {ip}"));
            let model    = dev_cfg.model.clone().unwrap_or_default();
            let project  = dev_cfg.project.clone().unwrap_or_default();
            let firmware = dev_cfg.firmware.clone().unwrap_or_default();
            dbg(&format!("load from config: {name} ({ip}) uuid={uuid} pinned={pinned}"));
            self.track_device(uuid, uuid, ip, tls, pinned, name, model, project, firmware, false);
        }
    }

    fn persist_pinned(&self) {
        let inner = self.imp().inner.borrow();
        config::update(|cfg| {
            for rec in inner.devices.values() {
                if rec.entry.uuid.is_empty() { continue; }
                let dev     = cfg.device_mut(&rec.entry.uuid);
                dev.pinned  = Some(rec.entry.pinned); // Explicit Some(true/false) ends legacy treatment.
                dev.last_ip = Some(rec.entry.ip.clone());
                dev.tls_mode = Some(rec.entry.tls_mode as u8);
                dev.name    = Some(rec.entry.name.clone());
                if !rec.entry.model.is_empty() {
                    dev.model = Some(rec.entry.model.clone());
                }
                if !rec.entry.project.is_empty() {
                    dev.project = Some(rec.entry.project.clone());
                }
                if !rec.entry.firmware.is_empty() {
                    dev.firmware = Some(rec.entry.firmware.clone());
                }
            }
        });
    }
}

fn device_key(uuid: &str, ip: &str) -> String {
    if !uuid.is_empty() { uuid.to_string() } else { format!("ip:{ip}") }
}

// ── DiscoveryWindow ───────────────────────────────────────────────────────────

pub struct DiscoveryWindow {
    window: adw::ApplicationWindow,
}

impl DiscoveryWindow {
    pub fn new(
        app:           &adw::Application,
        manager:       &DiscoveryManager,
        open_device:   Rc<dyn Fn(&ManagedEntry)>,
        open_settings: Rc<dyn Fn(Option<DeviceState>)>,
    ) -> Self {
        let (init_w, init_h) = config::with(|cfg| (
            if cfg.discovery_window_width  > 0 { cfg.discovery_window_width  } else { 500 },
            if cfg.discovery_window_height > 0 { cfg.discovery_window_height } else { 440 },
        ));
        let window = adw::ApplicationWindow::builder()
            .application(app)
            .title("RustyWiiM")
            .default_width(init_w)
            .default_height(init_h)
            .build();

        let header = adw::HeaderBar::new();

        // Custom title widget so the spinner can sit immediately to the right
        // of the "Scanning…" subtitle text.  adw::WindowTitle has no widget
        // slot in its subtitle row so we build the layout by hand.
        let title_label = gtk::Label::builder()
            .label("RustyWiiM")
            .css_classes(["title"])
            .build();

        let subtitle_label = gtk::Label::builder()
            .label("Scanning\u{2026}")
            .css_classes(["subtitle"])
            .build();

        let spinner = gtk::Spinner::builder()
            .spinning(true)
            .build();
        spinner.set_size_request(12, 12);

        let subtitle_row = gtk::Box::builder()
            .orientation(Orientation::Horizontal)
            .spacing(4)
            .halign(gtk::Align::Center)
            .build();
        subtitle_row.append(&subtitle_label);
        subtitle_row.append(&spinner);

        let title_box = gtk::Box::builder()
            .orientation(Orientation::Vertical)
            .valign(gtk::Align::Center)
            .build();
        title_box.append(&title_label);
        title_box.append(&subtitle_row);

        header.set_title_widget(Some(&title_box));

        let add_btn = gtk::Button::builder()
            .icon_name("list-add-symbolic")
            .tooltip_text("Add device by IP address…")
            .build();
        header.pack_end(&add_btn);
        header.pack_end(&super::menu::build_menu_button(false));
        super::wire_window_actions(&window, None, open_settings);

        // Device list
        let list_box = gtk::ListBox::builder()
            .selection_mode(gtk::SelectionMode::None)
            .css_classes(["boxed-list"])
            .margin_top(12).margin_bottom(12)
            .margin_start(12).margin_end(12)
            .build();

        let scroll = gtk::ScrolledWindow::builder()
            .hscrollbar_policy(gtk::PolicyType::Never)
            .vexpand(true)
            .build();
        scroll.set_child(Some(&list_box));

        let content = gtk::Box::builder()
            .orientation(Orientation::Vertical)
            .build();
        content.append(&scroll);

        let toolbar_view = adw::ToolbarView::new();
        toolbar_view.add_top_bar(&header);
        toolbar_view.set_content(Some(&content));
        window.set_content(Some(&toolbar_view));

        // Populate list and subscribe to manager changes.
        Self::rebuild_list(&list_box, &manager.entries(), &open_device, manager);

        // List rebuild: fires on every discovery event or tracked-device change.
        manager.connect_list_changed(clone!(
            @strong list_box, @strong open_device
                => move |mgr| {
                    Self::rebuild_list(&list_box, &mgr.entries(), &open_device, mgr);
                }
        ));

        // Scanning indicator: clear only when the SSDP scan cycle reports in.
        let scanning = Rc::new(std::cell::Cell::new(true));
        manager.connect_scan_complete(clone!(
            @strong subtitle_row, @strong scanning, @strong spinner
                => move || {
                    if scanning.replace(false) {
                        spinner.set_spinning(false);
                        subtitle_row.set_visible(false);
                    }
                }
        ));

        // "Add device" button.
        add_btn.connect_clicked(clone!(@strong window, @strong manager => move |_| {
            Self::show_add_dialog(&window, &manager);
        }));

        // win.close action — lets Ctrl-W (set app-wide) close this window.
        {
            let close_act = gtk::gio::SimpleAction::new("close", None);
            close_act.connect_activate(clone!(@strong window => move |_, _| { window.close(); }));
            window.add_action(&close_act);
        }

        // Hide when other windows are visible; quit (propagate) when last.
        window.connect_close_request(clone!(@strong window => move |w| {
            let (ww, wh) = (w.width(), w.height());
            config::update(|cfg| {
                cfg.discovery_open = false;
                if ww > 0 { cfg.discovery_window_width  = ww; }
                if wh > 0 { cfg.discovery_window_height = wh; }
            });

            let others_visible = w.application().map_or(false, |app| {
                app.windows().iter().any(|other| {
                    other.upcast_ref::<gtk::Widget>() != w.upcast_ref::<gtk::Widget>()
                        && other.is_visible()
                })
            });
            if others_visible {
                w.hide();
                glib::Propagation::Stop
            } else {
                glib::Propagation::Proceed
            }
        }));

        Self { window }
    }

    pub fn present(&self) {
        config::update(|cfg| cfg.discovery_open = true);
        self.window.present();
    }

    fn rebuild_list(
        list_box:    &gtk::ListBox,
        entries:     &[ManagedEntry],
        open_device: &Rc<dyn Fn(&ManagedEntry)>,
        manager:     &DiscoveryManager,
    ) {
        while let Some(child) = list_box.first_child() {
            list_box.remove(&child);
        }
        if entries.is_empty() {
            let placeholder = adw::ActionRow::builder()
                .title("No devices found")
                .sensitive(false)
                .build();
            list_box.append(&placeholder);
            return;
        }
        for entry in entries {
            list_box.append(&Self::build_row(entry, open_device, manager));
        }
    }

    fn build_row(
        entry:       &ManagedEntry,
        open_device: &Rc<dyn Fn(&ManagedEntry)>,
        manager:     &DiscoveryManager,
    ) -> adw::ActionRow {
        let subtitle = if entry.model.is_empty() { String::new() } else { entry.model.clone() };

        let status_suffix = match entry.presence {
            DevicePresence::Active => String::new(),
            DevicePresence::Ghost | DevicePresence::Dead => " · offline".to_string(),
        };
        let ip_label_text = format!("{}{}", entry.ip, status_suffix);

        let row = adw::ActionRow::builder()
            .title(&entry.name)
            .subtitle(&subtitle)
            .activatable(true)
            .build();

        if entry.presence != DevicePresence::Active {
            row.add_css_class("dim-label");
        }

        let ip_label = gtk::Label::builder()
            .label(&ip_label_text)
            .valign(gtk::Align::Center)
            .css_classes(["dim-label", "caption"])
            .build();
        row.add_suffix(&ip_label);

        // Pin / unpin toggle button.
        let pin_btn = gtk::ToggleButton::builder()
            .icon_name(if entry.pinned { "starred-symbolic" } else { "non-starred-symbolic" })
            .tooltip_text(if entry.pinned { "Unpin device" } else { "Pin device" })
            .active(entry.pinned)
            .valign(gtk::Align::Center)
            .css_classes(["flat"])
            .build();
        if entry.pinned {
            pin_btn.add_css_class("accent");
        }

        let uuid_for_pin = entry.uuid.clone();
        pin_btn.connect_toggled(clone!(@strong manager => move |btn| {
            manager.set_pinned(&uuid_for_pin, btn.is_active());
        }));
        row.add_suffix(&pin_btn);

        // Single click or Enter key opens the device window.
        let entry_clone = entry.clone();
        let open_fn     = Rc::clone(open_device);
        row.connect_activated(move |_| {
            open_fn(&entry_clone);
        });

        row
    }

    fn show_add_dialog(parent: &adw::ApplicationWindow, manager: &DiscoveryManager) {
        let ip_entry = gtk::Entry::builder()
            .placeholder_text("192.168.1.x")
            .activates_default(true)
            .build();

        let dialog = adw::AlertDialog::builder()
            .heading("Add Device")
            .body("Enter the IP address of a WiiM device:")
            .close_response("cancel")
            .build();
        dialog.add_response("cancel", "Cancel");
        dialog.add_response("add", "Add");
        dialog.set_response_appearance("add", adw::ResponseAppearance::Suggested);
        dialog.set_default_response(Some("add"));
        dialog.set_extra_child(Some(&ip_entry));

        dialog.connect_response(None, clone!(
            @strong manager, @strong ip_entry
                => move |_dlg, resp| {
                    if resp != "add" { return; }
                    let ip = ip_entry.text().to_string();
                    if ip.is_empty() { return; }

                    let rt = manager.rt();
                    let (tx, rx) = async_channel::bounded::<Option<DiscoveredDevice>>(1);
                    let ip2 = ip.clone();
                    rt.spawn(async move {
                        let result = crate::device::discovery::DiscoveryService::probe_device(&ip2).await;
                        let _ = tx.send(result).await;
                    });

                    glib::spawn_future_local(clone!(@strong manager => async move {
                        if let Ok(Some(dev)) = rx.recv().await {
                            manager.add_manual(dev.name, dev.ip, dev.uuid, dev.tls_mode);
                        } else {
                            eprintln!("[devlist-ui] Could not reach device at {ip}");
                        }
                    }));
                }
        ));

        dialog.present(Some(parent));
    }
}
