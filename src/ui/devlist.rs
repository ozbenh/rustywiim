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

use crate::config::Config;
use crate::device::api::{TlsMode, WiimClient};
use crate::device::discovery::{DiscoveredDevice, DiscoveryService};

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
            SIGNALS.get_or_init(|| vec![Signal::builder("list-changed").build()])
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

        self.load_pinned_from_config();

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
            self.persist_pinned();
            let pruned = self.do_prune();
            if pruned {
                self.emit_by_name::<()>("list-changed", &[]);
            } else {
                // Emit anyway so the UI reflects the pin change.
                self.emit_by_name::<()>("list-changed", &[]);
            }
        }
    }

    /// Add a manually-discovered device (already confirmed alive by the caller).
    pub fn add_manual(&self, name: String, ip: String, uuid: String, tls_mode: TlsMode) {
        let key = device_key(&uuid, &ip);
        {
            let mut inner = self.imp().inner.borrow_mut();
            if inner.devices.contains_key(&key) { return; }
            let entry = ManagedEntry {
                uuid: uuid.clone(), name: name.clone(), ip: ip.clone(),
                tls_mode, pinned: true, presence: DevicePresence::Active,
            };
            inner.devices.insert(key, DeviceRecord {
                entry, in_discovery: false, client: WiimClient::new(&ip, tls_mode),
            });
        }
        self.persist_pinned();
        self.emit_by_name::<()>("list-changed", &[]);
    }

    pub fn connect_list_changed<F: Fn(&Self) + 'static>(&self, f: F) -> glib::SignalHandlerId {
        self.connect_local("list-changed", false, move |args| {
            f(&args[0].get::<Self>().unwrap());
            None
        })
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
        {
            let mut inner = self.imp().inner.borrow_mut();
            for rec in inner.devices.values_mut() {
                let k = device_key(&rec.entry.uuid, &rec.entry.ip);
                rec.in_discovery = disc_keys.contains(&k);
            }
            for dev in &discovered {
                let key = device_key(&dev.uuid, &dev.ip);
                if !inner.devices.contains_key(&key) {
                    let entry = ManagedEntry {
                        uuid:     dev.uuid.clone(),
                        name:     dev.name.clone(),
                        ip:       dev.ip.clone(),
                        tls_mode: dev.tls_mode,
                        pinned:   false,
                        presence: DevicePresence::Active,
                    };
                    inner.devices.insert(key, DeviceRecord {
                        entry, in_discovery: true,
                        client: WiimClient::new(&dev.ip, dev.tls_mode),
                    });
                    changed = true;
                }
            }
        }
        let pruned = self.do_prune();
        if changed || pruned {
            self.emit_by_name::<()>("list-changed", &[]);
        }
    }

    fn trigger_health_checks(&self) {
        let Some(tx) = self.imp().health_tx.borrow().clone() else { return };
        let rt = self.rt();
        let records: Vec<(String, WiimClient)> = self.imp().inner.borrow()
            .devices.iter()
            .map(|(k, r)| (k.clone(), r.client.clone()))
            .collect();
        for (key, client) in records {
            let tx = tx.clone();
            rt.spawn(async move {
                let alive = client.get_device_info().await.is_ok();
                let _ = tx.send(HealthResult { key, alive }).await;
            });
        }
    }

    fn on_health_result(&self, result: HealthResult) {
        let changed = {
            let mut inner = self.imp().inner.borrow_mut();
            if let Some(rec) = inner.devices.get_mut(&result.key) {
                let new_p = if result.alive {
                    DevicePresence::Active
                } else if rec.entry.pinned {
                    DevicePresence::Ghost
                } else {
                    DevicePresence::Dead
                };
                let was = rec.entry.presence;
                rec.entry.presence = new_p;
                was != new_p
            } else { false }
        };
        let pruned = self.do_prune();
        if changed || pruned {
            self.emit_by_name::<()>("list-changed", &[]);
        }
    }

    /// Remove entries that are Dead (not pinned, not responding) and no longer
    /// visible in the SSDP discovery list.  Returns true if anything was removed.
    fn do_prune(&self) -> bool {
        let mut inner = self.imp().inner.borrow_mut();
        let before = inner.devices.len();
        inner.devices.retain(|_, rec| {
            rec.entry.pinned
                || rec.entry.presence == DevicePresence::Active
                || rec.in_discovery
        });
        inner.devices.len() < before
    }

    fn load_pinned_from_config(&self) {
        let cfg = Config::load();
        let mut inner = self.imp().inner.borrow_mut();
        for (uuid, dev_cfg) in &cfg.devices {
            // Only load explicitly pinned devices.  Legacy entries (None) and
            // explicitly unpinned entries (Some(false)) are both skipped.
            if dev_cfg.pinned != Some(true) { continue; }
            let Some(ref ip) = dev_cfg.last_ip else { continue };
            if inner.devices.contains_key(uuid) { continue; }
            let tls    = TlsMode::HttpsWiiM;
            let name   = dev_cfg.name.clone().unwrap_or_else(|| format!("Device @ {ip}"));
            let pinned = dev_cfg.pinned == Some(true);
            let entry  = ManagedEntry {
                uuid: uuid.clone(), name, ip: ip.clone(),
                tls_mode: tls, pinned, presence: DevicePresence::Ghost,
            };
            inner.devices.insert(uuid.clone(), DeviceRecord {
                entry, in_discovery: false, client: WiimClient::new(ip, tls),
            });
        }
    }

    fn persist_pinned(&self) {
        let inner = self.imp().inner.borrow();
        let mut cfg = Config::load();
        for rec in inner.devices.values() {
            let dev     = cfg.device_mut(&rec.entry.uuid);
            dev.pinned  = Some(rec.entry.pinned); // Explicit Some(true/false) ends legacy treatment.
            dev.last_ip = Some(rec.entry.ip.clone());
            dev.name    = Some(rec.entry.name.clone());
        }
        cfg.save();
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
        app:         &adw::Application,
        manager:     &DiscoveryManager,
        open_device: Rc<dyn Fn(&ManagedEntry)>,
    ) -> Self {
        let saved_cfg = Config::load();
        let init_w = if saved_cfg.discovery_window_width  > 0 { saved_cfg.discovery_window_width  } else { 500 };
        let init_h = if saved_cfg.discovery_window_height > 0 { saved_cfg.discovery_window_height } else { 440 };
        let window = adw::ApplicationWindow::builder()
            .application(app)
            .title("Devices")
            .default_width(init_w)
            .default_height(init_h)
            .build();

        let header = adw::HeaderBar::new();

        let add_btn = gtk::Button::builder()
            .icon_name("list-add-symbolic")
            .tooltip_text("Add device by IP address…")
            .build();
        header.pack_end(&add_btn);

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

        manager.connect_list_changed(clone!(
            @strong list_box, @strong open_device
                => move |mgr| {
                    Self::rebuild_list(&list_box, &mgr.entries(), &open_device, mgr);
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
            let mut cfg = Config::load();
            cfg.discovery_open = false;
            let (ww, wh) = (w.width(), w.height());
            if ww > 0 { cfg.discovery_window_width  = ww; }
            if wh > 0 { cfg.discovery_window_height = wh; }
            cfg.save();

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
        let mut cfg = Config::load();
        cfg.discovery_open = true;
        cfg.save();
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
        let subtitle = match entry.presence {
            DevicePresence::Active => entry.ip.clone(),
            DevicePresence::Ghost  => format!("{} · offline (pinned)", entry.ip),
            DevicePresence::Dead   => format!("{} · offline", entry.ip),
        };

        let row = adw::ActionRow::builder()
            .title(&entry.name)
            .subtitle(&subtitle)
            .build();

        if entry.presence != DevicePresence::Active {
            row.add_css_class("dim-label");
        }

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

        // Double-click to open device window.
        let gesture = gtk::GestureClick::builder().button(1).build();
        let entry_clone  = entry.clone();
        let open_fn      = Rc::clone(open_device);
        gesture.connect_pressed(move |_, n_press, _, _| {
            if n_press >= 2 {
                open_fn(&entry_clone);
            }
        });
        row.add_controller(gesture);

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
