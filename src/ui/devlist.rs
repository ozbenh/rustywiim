#![allow(deprecated)] // glib clone! @strong syntax

/// Device-list window and its backing DiscoveryManager.
///
/// `DiscoveryManager` (GObject) subscribes to `DiscoveryService`, maintains a
/// persistent list of known devices with per-device health checks, and honours
/// "pinned" status so devices survive SSDP disappearance.
///
/// `DiscoveryWindow` renders that list and lets the user pin/unpin entries,
/// open device windows by double-clicking, and add devices manually by IP.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use adw::prelude::*;
use glib::clone;
use glib::subclass::prelude::*;
use gtk::{glib, Orientation};

use crate::config;
use crate::device::api::{TlsMode, WiimClient};
use crate::device::capabilities::DeviceCapabilities;
use crate::device::discovery::{DEBUG_DISCOVERY, DiscoveredDevice, DiscoveryService};
use crate::device::state::DeviceState;

fn dbg(msg: &str) {
    if DEBUG_DISCOVERY.load(std::sync::atomic::Ordering::Relaxed) {
        println!("[devlist] {msg}");
    }
}

// ── Public types ──────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DevicePresence {
    Active, // responds to getStatusEx
    Ghost,  // pinned, not responding
    Dead,   // not pinned, not responding
}

#[derive(Debug, Clone)]
pub struct ManagedEntry {
    pub uuid:     String,
    pub name:     String,
    pub model:    String,
    pub ip:       String,
    pub tls_mode: TlsMode,
    pub pinned:   bool,
    pub presence: DevicePresence,
}

// ── Internal record ───────────────────────────────────────────────────────────

struct DeviceRecord {
    entry:        ManagedEntry,
    in_discovery: bool,
    client:       WiimClient,
}

struct HealthResult {
    key:   String, // uuid, or "ip:<ip>" when uuid is unknown
    alive: bool,
    /// Device name and model from a successful `getStatusEx` call; None when offline.
    name:  Option<String>,
    model: Option<String>,
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
        pub(super) rt:        std::cell::OnceCell<Arc<tokio::runtime::Runtime>>,
        pub(super) discovery: std::cell::OnceCell<DiscoveryService>,
        pub(super) inner:     RefCell<Inner>,
        pub(super) health_tx: RefCell<Option<async_channel::Sender<HealthResult>>>,
    }

    impl Default for DiscoveryManager {
        fn default() -> Self {
            Self {
                rt:        std::cell::OnceCell::new(),
                discovery: std::cell::OnceCell::new(),
                inner:     RefCell::new(Inner::default()),
                health_tx: RefCell::new(None),
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
                // load — before any async discovery or health-check results arrive.
                Signal::builder("initial-load").build(),
            ])
        }
    }
}

glib::wrapper! {
    pub struct DiscoveryManager(ObjectSubclass<mgr_imp::DiscoveryManager>);
}

impl DiscoveryManager {
    pub fn new(rt: Arc<tokio::runtime::Runtime>, discovery: DiscoveryService) -> Self {
        let obj: Self = glib::Object::new();
        obj.imp().rt.set(rt).unwrap();
        obj.imp().discovery.set(discovery).unwrap();
        obj
    }

    pub fn start(&self) {
        let (health_tx, health_rx) = async_channel::unbounded::<HealthResult>();
        *self.imp().health_tx.borrow_mut() = Some(health_tx);

        let weak = self.downgrade();
        glib::spawn_future_local(async move {
            while let Ok(result) = health_rx.recv().await {
                let Some(mgr) = weak.upgrade() else { break };
                mgr.on_health_result(result);
            }
        });

        self.load_known_devices_from_config();
        // initial-load fires once synchronously so AppState can open any windows
        // that config says should be restored — before async discovery arrives.
        self.emit_by_name::<()>("initial-load", &[]);
        // list-changed lets already-connected handlers (e.g. last_ip tracking)
        // see the initial device set.
        self.emit_by_name::<()>("list-changed", &[]);

        let weak2 = self.downgrade();
        self.imp().discovery.get().unwrap()
            .connect_discovery_updated(move |svc| {
                let Some(mgr) = weak2.upgrade() else { return };
                mgr.on_discovery_updated(svc);
            });

        // Health-check all known devices every 30 seconds.
        let weak3 = self.downgrade();
        glib::timeout_add_local(Duration::from_secs(30), move || {
            let Some(mgr) = weak3.upgrade() else { return glib::ControlFlow::Break };
            mgr.trigger_health_checks();
            glib::ControlFlow::Continue
        });
        self.trigger_health_checks();
    }

    pub fn entries(&self) -> Vec<ManagedEntry> {
        let mut v: Vec<ManagedEntry> = self.imp().inner.borrow()
            .devices.values().map(|r| r.entry.clone()).collect();
        v.sort_by(|a, b| a.name.cmp(&b.name));
        v
    }

    pub fn set_pinned(&self, uuid: &str, pinned: bool) {
        let changed = {
            let mut inner = self.imp().inner.borrow_mut();
            if let Some(rec) = inner.devices.get_mut(uuid) {
                let was = rec.entry.pinned;
                rec.entry.pinned = pinned;
                was != pinned
            } else { false }
        };
        if changed {
            dbg(&format!("pin: {uuid} → {pinned}"));
            self.persist_pinned();
            self.do_prune();
            self.emit_by_name::<()>("list-changed", &[]);
        }
    }

    /// Add a manually-discovered device (already confirmed alive by the caller).
    pub fn add_manual(&self, name: String, ip: String, uuid: String, tls_mode: TlsMode) {
        let key = device_key(&uuid, &ip);
        {
            let mut inner = self.imp().inner.borrow_mut();
            if inner.devices.contains_key(&key) {
                dbg(&format!("add manual: already known {name} ({ip}) uuid={uuid:?}"));
                return;
            }
            let entry = ManagedEntry {
                uuid: uuid.clone(), name: name.clone(), model: String::new(),
                ip: ip.clone(), tls_mode, pinned: true, presence: DevicePresence::Active,
            };
            inner.devices.insert(key.clone(), DeviceRecord {
                entry, in_discovery: false, client: WiimClient::new(&ip, tls_mode),
            });
        }
        dbg(&format!("add manual: {name} ({ip}) uuid={uuid:?}"));
        self.persist_pinned();
        self.emit_by_name::<()>("list-changed", &[]);
        // Fetch model (and confirm liveness) immediately rather than waiting for
        // the next 30-second health-check cycle.
        self.trigger_health_check_for(&key);
    }

    pub fn connect_list_changed<F: Fn(&Self) + 'static>(&self, f: F) -> glib::SignalHandlerId {
        self.connect_local("list-changed", false, move |args| {
            f(&args[0].get::<Self>().unwrap());
            None
        })
    }

    /// Fired once, synchronously inside `start()`, after loading devices from
    /// config — before any async discovery or health-check results arrive.
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
    /// health-check results arrive much earlier and would clear it prematurely.
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

    fn on_discovery_updated(&self, svc: &DiscoveryService) {
        let discovered = svc.devices();
        let disc_keys: std::collections::HashSet<String> = discovered.iter()
            .map(|d| device_key(&d.uuid, &d.ip))
            .collect();

        let mut changed = false;
        let mut new_keys: Vec<String> = Vec::new();
        {
            let mut inner = self.imp().inner.borrow_mut();
            for rec in inner.devices.values_mut() {
                let k = device_key(&rec.entry.uuid, &rec.entry.ip);
                let was = rec.in_discovery;
                rec.in_discovery = disc_keys.contains(&k);
                if was && !rec.in_discovery {
                    dbg(&format!("discovery: {} ({}) left SSDP scope", rec.entry.name, rec.entry.ip));
                }
            }
            let known_devices = config::with(|c| c.devices.clone());
            for dev in &discovered {
                let key = device_key(&dev.uuid, &dev.ip);
                match inner.devices.get_mut(&key) {
                    None => {
                        // Use cached name/model from config so the list shows
                        // correct values immediately, before the health check returns.
                        let cached = known_devices.get(&dev.uuid);
                        let name  = cached.and_then(|c| c.name.clone())
                            .filter(|n| !n.is_empty())
                            .unwrap_or_else(|| dev.name.clone());
                        let model = cached.and_then(|c| c.model.clone())
                            .unwrap_or_default();
                        dbg(&format!("discovery: new device {} ({}) uuid={:?}", name, dev.ip, dev.uuid));
                        let entry = ManagedEntry {
                            uuid:     dev.uuid.clone(),
                            name,
                            model,
                            ip:       dev.ip.clone(),
                            tls_mode: dev.tls_mode,
                            pinned:   false,
                            presence: DevicePresence::Active,
                        };
                        inner.devices.insert(key.clone(), DeviceRecord {
                            entry, in_discovery: true,
                            client: WiimClient::new(&dev.ip, dev.tls_mode),
                        });
                        new_keys.push(key);
                        changed = true;
                    }
                    // Known UUID reappeared at a different IP (e.g. DHCP lease
                    // renewal) or TLS mode.  Refresh the entry and rebuild the
                    // client so health checks target the live endpoint instead
                    // of pinging the old, now-dead IP forever — this is what
                    // lets a pinned "Ghost" device recover automatically.
                    Some(rec) if rec.entry.ip != dev.ip || rec.entry.tls_mode != dev.tls_mode => {
                        dbg(&format!(
                            "discovery: {} moved {} → {} uuid={:?}",
                            rec.entry.name, rec.entry.ip, dev.ip, dev.uuid,
                        ));
                        rec.entry.ip       = dev.ip.clone();
                        rec.entry.tls_mode = dev.tls_mode;
                        rec.client         = WiimClient::new(&dev.ip, dev.tls_mode);
                        new_keys.push(key);
                        changed = true;
                    }
                    Some(_) => {}
                }
            }
        }
        let pruned = self.do_prune();
        if changed || pruned {
            self.emit_by_name::<()>("list-changed", &[]);
        }
        // Immediately health-check newly discovered or IP-changed devices
        // rather than waiting for the 30-second health-check cycle.
        for key in new_keys {
            self.trigger_health_check_for(&key);
        }
    }

    fn trigger_health_checks(&self) {
        let keys: Vec<(String, WiimClient)> = self.imp().inner.borrow()
            .devices.iter()
            .map(|(k, r)| (k.clone(), r.client.clone()))
            .collect();
        dbg(&format!("health check: cycle starting for {} device(s)", keys.len()));
        for (key, _) in keys {
            self.trigger_health_check_for(&key);
        }
    }

    fn trigger_health_check_for(&self, key: &str) {
        let Some(tx) = self.imp().health_tx.borrow().clone() else { return };
        let Some((client, name)) = self.imp().inner.borrow()
            .devices.get(key).map(|r| (r.client.clone(), r.entry.name.clone()))
            else { return };
        dbg(&format!("health check: pinging {name} ({key})"));
        let key = key.to_string();
        self.rt().spawn(async move {
            let (alive, name, model) = match client.get_device_info().await {
                Ok(info) => (true,
                             Some(info.device_name.clone()),
                             Some(DeviceCapabilities::from_device_info(&info).model)),
                Err(_)   => (false, None, None),
            };
            let _ = tx.send(HealthResult { key, alive, name, model }).await;
        });
    }

    fn on_health_result(&self, result: HealthResult) {
        let mut needs_persist = false;
        {
            let mut inner = self.imp().inner.borrow_mut();
            if let Some(rec) = inner.devices.get_mut(&result.key) {
                let new_presence = if result.alive {
                    DevicePresence::Active
                } else if rec.entry.pinned {
                    DevicePresence::Ghost
                } else {
                    DevicePresence::Dead
                };
                if new_presence != rec.entry.presence {
                    dbg(&format!("health result: {} ({}) {:?} → {:?}",
                        rec.entry.name, rec.entry.ip, rec.entry.presence, new_presence));
                } else {
                    dbg(&format!("health result: {} ({}) {:?} (unchanged)",
                        rec.entry.name, rec.entry.ip, new_presence));
                }
                rec.entry.presence = new_presence;
                if let Some(name) = result.name {
                    if !name.is_empty() && rec.entry.name != name {
                        dbg(&format!("health result: {} name → {:?}", rec.entry.ip, name));
                        rec.entry.name = name;
                        needs_persist = true;
                    }
                }
                if let Some(model) = result.model {
                    if !model.is_empty() && rec.entry.model != model {
                        dbg(&format!("health result: {} model = {:?}", rec.entry.name, model));
                        rec.entry.model = model;
                        needs_persist = true;
                    }
                }
            } else {
                dbg(&format!("health result: unknown key {}", result.key));
            }
        }
        self.do_prune();
        if needs_persist { self.persist_pinned(); }
        // Always emit so the scanning indicator clears even when presence is unchanged.
        self.emit_by_name::<()>("list-changed", &[]);
    }

    /// Remove entries that are Dead (not pinned, not responding) and no longer
    /// visible in the SSDP discovery list.  Returns true if anything was removed.
    fn do_prune(&self) -> bool {
        let mut inner = self.imp().inner.borrow_mut();
        let before = inner.devices.len();
        inner.devices.retain(|key, rec| {
            let keep = rec.entry.pinned
                || rec.entry.presence == DevicePresence::Active
                || rec.in_discovery;
            if !keep {
                dbg(&format!("prune: removing {} ({key})", rec.entry.name));
            }
            keep
        });
        inner.devices.len() < before
    }

    fn load_known_devices_from_config(&self) {
        config::with(|cfg| {
            let mut inner = self.imp().inner.borrow_mut();
            for (uuid, dev_cfg) in &cfg.devices {
                let pinned = dev_cfg.pinned == Some(true);
                // Load pinned devices always.  Also pre-load non-pinned devices whose
                // window should reopen: the list-changed handler will open the window,
                // and if SSDP later confirms the device it stays; if not, do_prune
                // removes it (the window stays open independently).
                if !pinned && !dev_cfg.window_open { continue; }
                let Some(ref ip) = dev_cfg.last_ip else { continue };
                if inner.devices.contains_key(uuid) { continue; }
                let tls   = TlsMode::HttpsWiiM;
                let name  = dev_cfg.name.clone().unwrap_or_else(|| format!("Device @ {ip}"));
                let model = dev_cfg.model.clone().unwrap_or_default();
                dbg(&format!("load from config: {name} ({ip}) uuid={uuid} pinned={pinned}"));
                let entry = ManagedEntry {
                    uuid: uuid.clone(), name, model, ip: ip.clone(),
                    // Start Active (optimistic); health check will demote to Ghost if offline.
                    tls_mode: tls, pinned, presence: DevicePresence::Active,
                };
                inner.devices.insert(uuid.clone(), DeviceRecord {
                    entry, in_discovery: false, client: WiimClient::new(ip, tls),
                });
            }
        });
    }

    fn persist_pinned(&self) {
        let inner = self.imp().inner.borrow();
        config::update(|cfg| {
            for rec in inner.devices.values() {
                let dev     = cfg.device_mut(&rec.entry.uuid);
                dev.pinned  = Some(rec.entry.pinned); // Explicit Some(true/false) ends legacy treatment.
                dev.last_ip = Some(rec.entry.ip.clone());
                dev.name    = Some(rec.entry.name.clone());
                if !rec.entry.model.is_empty() {
                    dev.model = Some(rec.entry.model.clone());
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

        // List rebuild: fires on every health-check cycle or discovery event.
        manager.connect_list_changed(clone!(
            @strong list_box, @strong open_device
                => move |mgr| {
                    Self::rebuild_list(&list_box, &mgr.entries(), &open_device, mgr);
                }
        ));

        // Scanning indicator: clear only when the SSDP scan cycle reports in.
        // health-check results (list-changed) arrive much faster and would
        // dismiss the indicator before the first frame is even rendered.
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
            DevicePresence::Ghost  => " · offline (pinned)".to_string(),
            DevicePresence::Dead   => " · offline".to_string(),
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
                            eprintln!("[devlist] Could not reach device at {ip}");
                        }
                    }));
                }
        ));

        dialog.present(Some(parent));
    }
}
