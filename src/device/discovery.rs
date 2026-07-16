/// Persistent SSDP/UPnP discovery service.
///
/// Binds a multicast UDP socket on `0.0.0.0:1900` to receive UPnP NOTIFY
/// broadcasts and a separate ephemeral socket to send periodic M-SEARCH
/// queries.  Each new IP is probed asynchronously via the WiiM HTTP API;
/// device identity is confirmed with `DeviceId::detect()` from capabilities.
/// Two layers keep a non-WiiM device (e.g. a Samsung TV or Chromecast that
/// also answers `MediaRenderer:1`/`ssdp:all` SSDP searches) from being
/// probed forever: `is_likely_non_linkplay()` matches its `SERVER`/`ST`/`NT`/
/// `X-User-Agent` SSDP headers against a conservative, certain-negatives-only
/// denylist and
/// skips it before any network call at all; anything that slips through
/// (generic `SERVER: Linux` headers, common on Arylic/Audio Pro) still gets
/// a real API probe, but after `NON_API_FAIL_THRESHOLD` consecutive
/// failures it's treated as confirmed non-WiiM and skipped on further
/// re-announcements too. Both are session-only in-memory state, never
/// persisted, so a restart always retries.
///
/// Emits `discovery-updated` (on the GTK main thread) whenever the discovered
/// device list changes.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::net::Ipv4Addr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use glib::prelude::*;
use glib::subclass::prelude::*;
use gtk::glib;

use super::api::{DeviceInfo, TlsMode, api_base_url, build_reqwest_client};
use super::capabilities::DeviceId;

pub static DEBUG_DISCOVERY: AtomicBool = AtomicBool::new(false);

fn dbg(msg: &str) {
    if DEBUG_DISCOVERY.load(Ordering::Relaxed) {
        println!("{} [discovery] {msg}", super::timestamp());
    }
}

// ── DiscoveredDevice ──────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct DiscoveredDevice {
    pub ip:       String,
    pub name:     String,
    /// UUID from `getStatusEx`.  Used as the per-device config key.
    /// Empty if only the UPnP fallback path succeeded.
    pub uuid:     String,
    pub tls_mode: TlsMode,
}

// ── Inner state (GTK-thread only) ─────────────────────────────────────────────

struct Inner {
    /// Key: UUID when non-empty, otherwise `"ip:<ip>"`.
    devices: HashMap<String, DiscoveredDevice>,
    /// IPs currently being probed — prevents duplicate probe tasks.
    probing: HashSet<String>,
    /// Consecutive `identify_device()` failure count per IP. Once an IP
    /// reaches `NON_API_FAIL_THRESHOLD` it's treated as confirmed
    /// non-WiiM/LinkPlay and skipped on future SSDP re-announcements —
    /// session-only (never persisted to `config.json`), so an app restart
    /// or a fresh IP always gets a clean retry.
    failures: HashMap<String, u32>,
}

impl Default for Inner {
    fn default() -> Self {
        Self { devices: HashMap::new(), probing: HashSet::new(), failures: HashMap::new() }
    }
}

// ── SSDP constants ────────────────────────────────────────────────────────────

const SSDP_ADDR:        &str     = "239.255.255.250:1900";
const SSDP_IP:          Ipv4Addr = Ipv4Addr::new(239, 255, 255, 250);
const MSEARCH_INTERVAL: Duration = Duration::from_secs(60);
const PROBE_TIMEOUT:    Duration = Duration::from_secs(3);
/// Consecutive `identify_device()` failures (each of which already tries
/// every `PROBE_MODES` entry plus the description.xml fallback) before an IP
/// is treated as confirmed non-WiiM/LinkPlay and skipped on future SSDP
/// re-announcements for the rest of this run.
const NON_API_FAIL_THRESHOLD: u32 = 3;

const SEARCH_MSGS: &[&str] = &[
    "M-SEARCH * HTTP/1.1\r\n\
     HOST: 239.255.255.250:1900\r\n\
     MAN: \"ssdp:discover\"\r\n\
     MX: 3\r\n\
     ST: urn:schemas-upnp-org:device:MediaRenderer:1\r\n\
     \r\n",
    "M-SEARCH * HTTP/1.1\r\n\
     HOST: 239.255.255.250:1900\r\n\
     MAN: \"ssdp:discover\"\r\n\
     MX: 3\r\n\
     ST: ssdp:all\r\n\
     \r\n",
];

/// SSDP `SERVER` header substrings that certainly indicate a non-LinkPlay
/// device (ported from pywiim's `NON_LINKPLAY_SERVER_PATTERNS`) — matched
/// case-insensitively before any API probe is attempted, so a Samsung TV or
/// Chromecast answering `MediaRenderer:1`/`ssdp:all` never costs a network
/// round-trip at all. Deliberately conservative: only patterns confirmed to
/// appear in real non-LinkPlay SSDP responses. Devices with a generic
/// `SERVER` header (Arylic/Audio Pro often send plain "Linux") won't match
/// and fall through to the normal API probe, same as today.
const NON_LINKPLAY_SERVER_PATTERNS: &[&str] = &[
    "chromecast",
    "denon-heos",
    "mint-x",       // Sony devices
    "knos",         // Kodi/OSMC
    "sonos",
    "samsung",
    "sec_hhp",      // Samsung Electronics
    "smartthings",
];

/// SSDP `NT` (NOTIFY)/`ST` (M-SEARCH response) service-type substrings that
/// certainly indicate a non-LinkPlay device (ported from pywiim's
/// `NON_LINKPLAY_ST_PATTERNS`) — same conservative, certain-negatives-only
/// spirit as the `SERVER` list above.
const NON_LINKPLAY_ST_PATTERNS: &[&str] = &[
    "schemas-upnp-org:device:zoneplayer",       // Sonos
    "schemas-upnp-org:service:zonegrouptopology", // Sonos
    "schemas-upnp-org:service:grouprenderingcontrol", // Sonos
    "roku-com:device",
    "dial-multiscreen-org:device:dial",         // Chromecast et al.
    "samsung.com:device",
    "samsung.com:service",
];

/// SSDP `X-User-Agent` header substrings that certainly indicate a
/// non-LinkPlay device. Needed as a third signal, separate from `SERVER`/
/// `ST`, because a Samsung TV's Netflix "MDX" (Multi-Device Experience)
/// second-screen discovery service — a *different* SSDP announcement from
/// the TV's main DIAL/DLNA one, on its own port — reports a fully generic
/// `SERVER: Linux/x.y.z, UPnP/1.0, Portable SDK for UPnP devices` (no
/// vendor string at all) and a non-matching `ST`/`USN`
/// (`uuid:SSTVRMF1==...`), confirmed live (Ben, 2026-07-11) via a direct
/// unicast M-SEARCH to the TV — so it slipped through both existing
/// checks and got a full (failing) API probe. `X-User-Agent: NRDP MDX`
/// (Netflix's "Ready Device Platform") is what actually identifies it,
/// and is implemented by non-Samsung smart TVs too, not just this one.
const NON_LINKPLAY_USER_AGENT_PATTERNS: &[&str] = &[
    "nrdp", // Netflix "Ready Device Platform" / MDX second-screen discovery
];

/// True if the SSDP headers *certainly* identify a non-LinkPlay device —
/// never a false positive on a real WiiM/LinkPlay device, but also won't
/// catch every non-LinkPlay one (generic `SERVER: Linux` headers pass
/// through and rely on the API-probe failure counter instead).
fn is_likely_non_linkplay(server: &str, st: &str, x_user_agent: &str) -> bool {
    let server_lc = server.to_ascii_lowercase();
    let st_lc     = st.to_ascii_lowercase();
    let ua_lc     = x_user_agent.to_ascii_lowercase();
    NON_LINKPLAY_SERVER_PATTERNS.iter().any(|p| server_lc.contains(p))
        || NON_LINKPLAY_ST_PATTERNS.iter().any(|p| st_lc.contains(p))
        || NON_LINKPLAY_USER_AGENT_PATTERNS.iter().any(|p| ua_lc.contains(p))
}

const PROBE_MODES: &[TlsMode] = &[
    TlsMode::HttpsWiiM,
    TlsMode::HttpsAudioPro,
    TlsMode::Http,
];

// ── SSDP event (tokio → GTK thread) ──────────────────────────────────────────

enum SsdpEvent {
    Alive  { ip: String, uuid: String, location: String, server: String, st: String, x_user_agent: String },
    Byebye { uuid: String, ip: String },
}

// ── GObject implementation ────────────────────────────────────────────────────

mod imp {
    use super::*;
    use glib::subclass::Signal;
    use std::sync::OnceLock;

    pub struct DiscoveryService {
        pub(super) rt:    std::cell::OnceCell<Arc<tokio::runtime::Runtime>>,
        pub(super) inner: RefCell<Inner>,
    }

    impl Default for DiscoveryService {
        fn default() -> Self {
            Self {
                rt:    std::cell::OnceCell::new(),
                inner: RefCell::new(Inner::default()),
            }
        }
    }

    #[glib::object_subclass]
    impl ObjectSubclass for DiscoveryService {
        const NAME: &'static str = "RustyWiimDiscoveryService";
        type Type = super::DiscoveryService;
    }

    impl ObjectImpl for DiscoveryService {
        fn signals() -> &'static [Signal] {
            static SIGNALS: OnceLock<Vec<Signal>> = OnceLock::new();
            SIGNALS.get_or_init(|| {
                vec![Signal::builder("discovery-updated").build()]
            })
        }
    }
}

glib::wrapper! {
    pub struct DiscoveryService(ObjectSubclass<imp::DiscoveryService>);
}

// ── Public API ────────────────────────────────────────────────────────────────

impl DiscoveryService {
    pub fn new(rt: Arc<tokio::runtime::Runtime>) -> Self {
        let obj: Self = glib::Object::new();
        obj.imp().rt.set(rt).unwrap();
        obj
    }

    /// Start the SSDP listener and M-SEARCH loop.  Call once after `new()`.
    /// Must be called from the GTK main thread (uses `glib::spawn_future_local`).
    pub fn start(&self) {
        let (ssdp_tx, ssdp_rx) =
            async_channel::unbounded::<SsdpEvent>();
        let (probe_tx, probe_rx) =
            async_channel::unbounded::<(String, Option<DiscoveredDevice>)>();

        // SSDP listener runs in tokio — sends events to the GTK thread.
        self.rt().spawn(ssdp_listener(ssdp_tx));

        // Process SSDP events on the GTK thread.
        let svc_weak = self.downgrade();
        let probe_tx2 = probe_tx.clone();
        let rt2 = self.rt();
        glib::spawn_future_local(async move {
            while let Ok(event) = ssdp_rx.recv().await {
                let Some(svc) = svc_weak.upgrade() else { break };
                svc.handle_ssdp_event(event, &rt2, &probe_tx2);
            }
        });

        // Process probe results on the GTK thread.
        let svc_weak2 = self.downgrade();
        glib::spawn_future_local(async move {
            while let Ok((ip, result)) = probe_rx.recv().await {
                let Some(svc) = svc_weak2.upgrade() else { break };
                svc.handle_probe_result(ip, result);
            }
        });

        // After the initial M-SEARCH response window (MX=3 s + 1 s margin),
        // emit devices-changed even if the list is still empty so the UI can
        // show "No device" rather than remaining blank indefinitely.
        let svc_weak3 = self.downgrade();
        glib::timeout_add_local_once(
            std::time::Duration::from_secs(4),
            move || {
                let Some(svc) = svc_weak3.upgrade() else { return };
                if svc.imp().inner.borrow().devices.is_empty() {
                    dbg("initial timeout: no devices found, emitting devices-changed");
                    svc.emit_by_name::<()>("discovery-updated", &[]);
                }
            },
        );
    }

    /// Current snapshot of all confirmed devices, sorted by name.
    pub fn devices(&self) -> Vec<DiscoveredDevice> {
        let mut devs: Vec<DiscoveredDevice> =
            self.imp().inner.borrow().devices.values().cloned().collect();
        devs.sort_by(|a, b| a.name.cmp(&b.name));
        devs
    }

    /// Probe a single IP across all TLS modes.  Returns a `DiscoveredDevice`
    /// if the device responds and is identified as a WiiM/LinkPlay device.
    /// Intended for manually-added devices where there is no SSDP location URL.
    pub async fn probe_device(ip: &str) -> Option<DiscoveredDevice> {
        identify_device(ip, "").await
    }

    pub fn connect_discovery_updated<F: Fn(&Self) + 'static>(
        &self, f: F,
    ) -> glib::SignalHandlerId {
        self.connect_local("discovery-updated", false, move |args| {
            f(&args[0].get::<Self>().unwrap());
            None
        })
    }

    fn rt(&self) -> Arc<tokio::runtime::Runtime> {
        self.imp().rt.get().unwrap().clone()
    }

    fn handle_ssdp_event(
        &self,
        event: SsdpEvent,
        rt: &Arc<tokio::runtime::Runtime>,
        probe_tx: &async_channel::Sender<(String, Option<DiscoveredDevice>)>,
    ) {
        match event {
            SsdpEvent::Byebye { uuid, ip } => {
                let key     = device_key(&uuid, &ip);
                let removed = self.imp().inner.borrow_mut().devices.remove(&key).is_some();
                if removed {
                    dbg(&format!("byebye: removed {key}"));
                    self.emit_by_name::<()>("discovery-updated", &[]);
                }
            }
            SsdpEvent::Alive { ip, uuid, location, server, st, x_user_agent } => {
                // Cheapest check first: SSDP headers we already have in hand,
                // no network round-trip needed. Never a false positive on a
                // real WiiM/LinkPlay device, so this is safe to apply on
                // every re-announcement, not just the first.
                if is_likely_non_linkplay(&server, &st, &x_user_agent) {
                    dbg(&format!(
                        "alive: skipping {ip} (SSDP headers indicate non-LinkPlay device: \
                         SERVER={server:?} ST/NT={st:?} X-User-Agent={x_user_agent:?})"
                    ));
                    return;
                }

                let should_probe = {
                    let mut inner = self.imp().inner.borrow_mut();
                    // Already known by UUID or IP key?
                    let key = device_key(&uuid, &ip);
                    let already_known = inner.devices.contains_key(&key);
                    let confirmed_non_api = inner.failures.get(&ip)
                        .is_some_and(|&n| n >= NON_API_FAIL_THRESHOLD);
                    if already_known || inner.probing.contains(&ip) || confirmed_non_api {
                        if confirmed_non_api {
                            dbg(&format!("alive: skipping {ip} (confirmed non-API this run)"));
                        }
                        false
                    } else {
                        inner.probing.insert(ip.clone());
                        true
                    }
                };
                if should_probe {
                    dbg(&format!("alive: probing {ip} uuid={uuid:?}"));
                    let probe_tx = probe_tx.clone();
                    let ip2 = ip.clone();
                    rt.spawn(async move {
                        let result = identify_device(&ip2, &location).await;
                        let _ = probe_tx.send((ip2, result)).await;
                    });
                }
            }
        }
    }

    fn handle_probe_result(&self, ip: String, result: Option<DiscoveredDevice>) {
        let mut inner = self.imp().inner.borrow_mut();
        inner.probing.remove(&ip);
        if let Some(dev) = result {
            inner.failures.remove(&ip);
            dbg(&format!("probe ok: {} ({}) uuid={:?}", dev.name, dev.ip, dev.uuid));
            let key = device_key(&dev.uuid, &dev.ip);
            inner.devices.insert(key, dev);
            drop(inner);
            self.emit_by_name::<()>("discovery-updated", &[]);
        } else {
            let count = inner.failures.entry(ip.clone()).or_insert(0);
            *count += 1;
            if *count >= NON_API_FAIL_THRESHOLD {
                dbg(&format!(
                    "probe failed: {ip} ({count}/{NON_API_FAIL_THRESHOLD}) — giving up \
                     on this IP for the rest of this run"
                ));
            } else {
                dbg(&format!("probe failed: {ip} ({count}/{NON_API_FAIL_THRESHOLD})"));
            }
        }
    }
}

fn device_key(uuid: &str, ip: &str) -> String {
    if !uuid.is_empty() { uuid.to_string() } else { format!("ip:{ip}") }
}

// ── SSDP listener task ────────────────────────────────────────────────────────

async fn ssdp_listener(tx: async_channel::Sender<SsdpEvent>) {
    // Multicast socket: receives NOTIFY broadcasts from all devices on LAN.
    let notify_sock = match create_notify_socket() {
        Ok(s) => s,
        Err(e) => {
            eprintln!("{} [discovery] SSDP socket bind failed: {e}", super::timestamp());
            return;
        }
    };
    // Ephemeral socket: sends M-SEARCH and receives unicast responses.
    let Ok(search_sock) = tokio::net::UdpSocket::bind("0.0.0.0:0").await else {
        eprintln!("{} [discovery] M-SEARCH socket bind failed", super::timestamp());
        return;
    };

    let ssdp_addr: std::net::SocketAddr = SSDP_ADDR.parse().unwrap();
    // tokio::time::interval fires immediately on first tick, so the initial
    // M-SEARCH goes out as soon as the loop starts.
    let mut msearch_timer = tokio::time::interval(MSEARCH_INTERVAL);
    let mut nbuf = vec![0u8; 4096];
    let mut sbuf = vec![0u8; 4096];

    loop {
        tokio::select! {
            _ = msearch_timer.tick() => {
                for msg in SEARCH_MSGS {
                    let _ = search_sock.send_to(msg.as_bytes(), ssdp_addr).await;
                }
                dbg("M-SEARCH sent");
            }
            result = notify_sock.recv_from(&mut nbuf) => {
                if let Ok((len, src)) = result {
                    let pkt = String::from_utf8_lossy(&nbuf[..len]);
                    if let Some(ev) = parse_ssdp_packet(&pkt, &src.ip().to_string()) {
                        let _ = tx.send(ev).await;
                    }
                }
            }
            result = search_sock.recv_from(&mut sbuf) => {
                if let Ok((len, src)) = result {
                    let pkt = String::from_utf8_lossy(&sbuf[..len]);
                    if let Some(ev) = parse_ssdp_packet(&pkt, &src.ip().to_string()) {
                        let _ = tx.send(ev).await;
                    }
                }
            }
        }
    }
}

/// Create a UDP socket bound to `0.0.0.0:1900`, joined to the SSDP multicast
/// group, with `SO_REUSEADDR` + `SO_REUSEPORT` so other processes can coexist.
fn create_notify_socket() -> std::io::Result<tokio::net::UdpSocket> {
    use socket2::{Domain, Protocol, Socket, Type};
    let socket = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))?;
    socket.set_reuse_address(true)?;
    socket.bind(&"0.0.0.0:1900".parse::<std::net::SocketAddr>().unwrap().into())?;
    socket.join_multicast_v4(&SSDP_IP, &Ipv4Addr::UNSPECIFIED)?;
    socket.set_nonblocking(true)?;
    let std_sock: std::net::UdpSocket = socket.into();
    tokio::net::UdpSocket::from_std(std_sock)
}

// ── SSDP packet parsing ───────────────────────────────────────────────────────

fn parse_ssdp_packet(pkt: &str, src_ip: &str) -> Option<SsdpEvent> {
    let first      = pkt.lines().next().unwrap_or("").trim();
    let is_notify  = first.eq_ignore_ascii_case("NOTIFY * HTTP/1.1");
    let is_ok_resp = first.starts_with("HTTP/1.1 200");
    if !is_notify && !is_ok_resp { return None; }

    let nts      = extract_header(pkt, "NTS").unwrap_or_default();
    let location = extract_header(pkt, "LOCATION").unwrap_or_default();
    let usn      = extract_header(pkt, "USN").unwrap_or_default();
    let server   = extract_header(pkt, "SERVER").unwrap_or_default();
    let x_user_agent = extract_header(pkt, "X-USER-AGENT").unwrap_or_default();
    // NOTIFY packets carry the service/device type in `NT`; M-SEARCH 200 OK
    // responses carry it in `ST` — never both on the same packet, so either
    // one (whichever is present) is the "service type" signal for filtering.
    let st = extract_header(pkt, "ST")
        .or_else(|| extract_header(pkt, "NT"))
        .unwrap_or_default();

    let uuid = extract_uuid_from_usn(&usn);
    // Prefer the IP from the LOCATION header (authoritative) over src_ip.
    let ip = if !location.is_empty() {
        extract_ip_from_url(&location).unwrap_or_else(|| src_ip.to_string())
    } else {
        src_ip.to_string()
    };

    if nts == "ssdp:byebye" {
        Some(SsdpEvent::Byebye { uuid, ip })
    } else if is_ok_resp || nts == "ssdp:alive" || nts == "ssdp:update" {
        // Alive/response with no LOCATION is useless — skip.
        if location.is_empty() { return None; }
        Some(SsdpEvent::Alive { ip, uuid, location, server, st, x_user_agent })
    } else {
        None
    }
}

/// Extract the UUID from a USN header value.
/// USN format: `uuid:XXXX-XXXX::urn:...` or bare `uuid:XXXX-XXXX`.
fn extract_uuid_from_usn(usn: &str) -> String {
    if let Some(rest) = usn.strip_prefix("uuid:") {
        rest.split("::").next().unwrap_or(rest).to_string()
    } else {
        String::new()
    }
}

/// Extract the host IP from an `http://` or `https://` URL.
/// Returns `None` if the host portion is not a valid IP address.
fn extract_ip_from_url(url: &str) -> Option<String> {
    let after_scheme = url.strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))?;
    let hostport = after_scheme.split('/').next()?;
    let host = match hostport.find(':') {
        Some(i) => &hostport[..i],
        None    => hostport,
    };
    if host.is_empty() { return None; }
    host.parse::<std::net::IpAddr>().ok()?;
    Some(host.to_string())
}

// ── Device probing ────────────────────────────────────────────────────────────

/// Try each TLS mode in `PROBE_MODES` order; return the first `DiscoveredDevice`
/// whose API responds.  Falls back to the SSDP UPnP description URL if all
/// API probes fail.
async fn identify_device(ip: &str, location: &str) -> Option<DiscoveredDevice> {
    for &mode in PROBE_MODES {
        if let Some((name, uuid)) = probe_api(ip, mode).await {
            return Some(DiscoveredDevice { ip: ip.to_string(), name, uuid, tls_mode: mode });
        }
    }

    // API probes all failed.  Try the SSDP UPnP description URL as a last resort —
    // it at least confirms this is a WiiM/LinkPlay device so we can surface it in
    // the UI even if we don't yet know the right protocol.
    let client = build_reqwest_client(TlsMode::Http, PROBE_TIMEOUT);
    if let Ok(resp) = client.get(location).send().await {
        if let Ok(xml) = resp.text().await {
            let lower = xml.to_lowercase();
            if lower.contains("wiim") || lower.contains("linkplay") || lower.contains("wiimu") {
                let name = extract_xml_tag(&xml, "friendlyName")
                    .unwrap_or_else(|| format!("WiiM @ {ip}"));
                return Some(DiscoveredDevice {
                    ip: ip.to_string(),
                    name,
                    uuid: String::new(),
                    tls_mode: TlsMode::HttpsWiiM,
                });
            }
        }
    }
    None
}

/// Call `getStatusEx` on the device and parse the response as `DeviceInfo`.
/// Uses `DeviceId::detect()` from capabilities to confirm the device is a
/// supported LinkPlay/WiiM variant.  Returns `(name, uuid)` on success.
async fn probe_api(ip: &str, mode: TlsMode) -> Option<(String, String)> {
    let client = build_reqwest_client(mode, PROBE_TIMEOUT);
    let url    = format!("{}?command=getStatusEx", api_base_url(ip, mode));
    let text   = match client.get(&url).send().await {
        Ok(r)  => r.text().await.ok()?,
        Err(e) => {
            super::api::log_request_error(
                "API",
                &format!("probe {ip} [{}]", mode.description()),
                &e,
            );
            return None;
        }
    };

    // Parse into DeviceInfo and run capabilities detection to confirm this is
    // a recognised LinkPlay/WiiM device (any DeviceId is accepted — even
    // LinkPlayGeneric means a valid but unrecognised LinkPlay device).
    let info: DeviceInfo = serde_json::from_str(&text).ok()?;
    if info.uuid.is_empty() && info.device_name.is_empty() { return None; }

    let device_id = DeviceId::detect(&info.project, &info.firmware);
    dbg(&format!("probe ok: {ip} [{:?}] id={device_id:?}", mode));

    let name = if !info.device_name.is_empty() {
        info.device_name
    } else {
        format!("Device @ {ip}")
    };
    Some((name, info.uuid))
}

// ── Header / XML helpers ──────────────────────────────────────────────────────

fn extract_header(response: &str, header: &str) -> Option<String> {
    let upper = header.to_ascii_uppercase();
    for line in response.lines() {
        if let Some((key, rest)) = line.split_once(':') {
            if key.trim().to_ascii_uppercase() == upper {
                let val = rest.trim().to_string();
                if !val.is_empty() { return Some(val); }
            }
        }
    }
    None
}

fn extract_xml_tag(xml: &str, tag: &str) -> Option<String> {
    let open  = format!("<{tag}>");
    let close = format!("</{tag}>");
    let start = xml.find(&open)? + open.len();
    let end   = xml[start..].find(&close)? + start;
    let val   = xml[start..end].trim().to_string();
    if val.is_empty() { None } else { Some(val) }
}
