/// UPnP GENA (General Event Notification Architecture) eventing — `SUBSCRIBE`/
/// `UNSUBSCRIBE`/renewal plumbing, the process-wide NOTIFY listener, NOTIFY
/// body parsing, and the per-service `GenaHealth` state machine. Structurally
/// a sibling to `upnp.rs` the way `upnp.rs` is to `api.rs`: `upnp.rs` stays
/// request/response SOAP calls only, this module owns the long-lived-
/// subscription side of UPnP (a genuinely different shape — persistent
/// state, renewal timers, an inbound listener — not just more SOAP actions).
/// `state.rs` decides *when* a device should hold a GENA session, applies
/// parsed NOTIFY events into canonical `PlaybackState`, and drives
/// `GenaHealth` from its poll path's own comparisons; this module owns *how*
/// the subscription itself works and the pure health-transition/parsing
/// logic. `service_loop()` retries `SUBSCRIBE` indefinitely (never a
/// one-shot give-up) — a permanently blocked callback path costs nothing
/// beyond one attempt per retry interval while regular polling keeps
/// covering the device fully in the meantime.
use std::collections::HashMap;
use std::net::UdpSocket;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use super::api::{build_reqwest_client, TlsMode};
use super::upnp::{discover_description, extract_attr, extract_tag, extract_url_for_service, unescape_xml_entities};

pub static DEBUG_GENA: AtomicBool = AtomicBool::new(false);
/// `--debug=gena:verbose` (or `all:verbose`): include a NOTIFY's full
/// `LastChange` content in `dbg_notify()`'s output. Without it, a NOTIFY
/// logs just the service and sequence number — lifecycle messages
/// (subscribe/renew/unsubscribe, all always via plain `dbg()`) are already
/// single-line, so only NOTIFY bodies have a "full content" dimension to
/// strip in summary mode.
pub static DEBUG_GENA_VERBOSE: AtomicBool = AtomicBool::new(false);

fn dbg(msg: &str) {
    if DEBUG_GENA.load(Ordering::Relaxed) {
        println!("{} [gena] {msg}", super::timestamp());
    }
}

/// NOTIFY-specific logging: `summary` (service + seq, always shown) plus
/// `full` (the `LastChange` content, `None` when absent) only in verbose
/// mode.
fn dbg_notify(summary: &str, full: Option<&str>) {
    if !DEBUG_GENA.load(Ordering::Relaxed) {
        return;
    }
    if !DEBUG_GENA_VERBOSE.load(Ordering::Relaxed) {
        println!("{} [gena] {summary}", super::timestamp());
        return;
    }
    match full {
        Some(f) => println!("{} [gena] {summary} {f}", super::timestamp()),
        None => println!("{} [gena] {summary} (no LastChange content)", super::timestamp()),
    }
}

const REQUEST_TIMEOUT: Duration = Duration::from_secs(5);
/// Requested subscription lifetime — the device may grant a shorter one;
/// always use what it actually returns, never assume this was honored
/// verbatim.
const REQUESTED_TIMEOUT_SECS: u32 = 1800;
/// Retry interval once a subscription is confirmed dead (renewal failed and
/// a fresh `SUBSCRIBE` also failed) — retries forever at this cadence
/// rather than giving up once, since a permanently
/// blocked callback path (e.g. a corporate firewall) costs nothing beyond
/// one `SUBSCRIBE` attempt per interval while full-rate polling already
/// covers the device fully in the meantime.
const RETRY_INTERVAL_SECS: u64 = 30;

struct WantedService {
    name: &'static str,
    service_type_substr: &'static str,
}

const WANTED_SERVICES: &[WantedService] = &[
    WantedService { name: "AVTransport", service_type_substr: ":service:AVTransport:" },
    WantedService { name: "RenderingControl", service_type_substr: ":service:RenderingControl:" },
    WantedService { name: "PlayQueue", service_type_substr: "wiimu-com:service:PlayQueue" },
];

fn tls_for_scheme(scheme: &str) -> TlsMode {
    if scheme == "https" { TlsMode::HttpsAny } else { TlsMode::Http }
}

/// `"scheme://host:port/path"` -> `"host:port"`, for the `HOST` header GENA
/// requires on every `SUBSCRIBE`/renewal/`UNSUBSCRIBE`.
fn host_port_of(url: &str) -> String {
    let after_scheme = url.split_once("://").map(|(_, rest)| rest).unwrap_or(url);
    after_scheme.split('/').next().unwrap_or(after_scheme).to_string()
}

/// The OS's own routing table already knows which local interface would be
/// used to reach `device_host` — connecting a UDP socket to it (no packet
/// actually sent) is the standard, dependency-free way to read that address
/// back, which is exactly the address the device can reach us on.
fn find_local_ip(device_host: &str) -> anyhow::Result<std::net::IpAddr> {
    let sock = UdpSocket::bind("0.0.0.0:0")?;
    sock.connect((device_host, 1900))?;
    Ok(sock.local_addr()?.ip())
}

async fn discover_event_sub_urls(ip: &str) -> anyhow::Result<Vec<(&'static str, String)>> {
    let (body, url) = discover_description(ip).await?;
    let mut found = Vec::new();
    for wanted in WANTED_SERVICES {
        if let Some(event_sub_url) = extract_url_for_service(&body, &url, wanted.service_type_substr, "eventSubURL") {
            found.push((wanted.name, event_sub_url));
        }
    }
    Ok(found)
}

fn parse_timeout_header(raw: Option<&str>) -> u32 {
    raw.and_then(|s| s.strip_prefix("Second-"))
        .and_then(|s| s.parse().ok())
        .unwrap_or(REQUESTED_TIMEOUT_SECS)
}

/// Sends whatever `request` builds, retrying `reqwest::Error::is_request()`
/// failures ("connection closed before message completed" — a known
/// pooled-keep-alive-connection race, not a real fault, confirmed live on a
/// real `SUBSCRIBE`) up to `MAX_RETRIES` times with a 100ms backoff — the
/// exact same pattern `api.rs`'s `cmd()`/`upnp.rs`'s `soap_call()` already
/// use for this error class, just shared here across `subscribe()`/
/// `renew()`/`unsubscribe()` instead of tripled. `request` is called fresh
/// on every attempt (a `RequestBuilder` isn't reusable once sent). Any other
/// error, or exhausting the retries, logs via `log_request_error()` (tagged
/// `"gena"`, `context` identifying the call — e.g. `"10.1.1.73: AVTransport:
/// SUBSCRIBE"`) and returns it — a plain `{e}` `Display` on a
/// `reqwest::Error` is often as unhelpfully generic as "error sending
/// request for url (...)", hiding the actual cause that only walking
/// `.source()` reveals.
async fn send_with_retry(
    request: impl Fn() -> reqwest::RequestBuilder,
    context: &str,
) -> Result<reqwest::Response, reqwest::Error> {
    const MAX_RETRIES: u32 = 3;
    for attempt in 0..=MAX_RETRIES {
        if attempt > 0 {
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        match request().send().await {
            Ok(resp) => return Ok(resp),
            Err(e) => {
                if !e.is_request() || attempt == MAX_RETRIES {
                    super::api::log_request_error("gena", context, &e);
                    return Err(e);
                }
                // Same noise rule as api.rs's cmd(): the first attempt's
                // transient failure only logs under --debug=gena (routine,
                // self-healing), but a first *retry* that also fails logs
                // unconditionally (more likely a real problem).
                if attempt > 0 || DEBUG_GENA.load(Ordering::Relaxed) {
                    eprintln!(
                        "{} [gena] {context}: transient send error (attempt {}/{}), retrying in 100ms: {e}",
                        super::timestamp(), attempt + 1, MAX_RETRIES,
                    );
                }
            }
        }
    }
    unreachable!()
}

async fn subscribe(event_sub_url: &str, host_header: &str, callback_url: &str, context: &str) -> anyhow::Result<(String, u32)> {
    let scheme = event_sub_url.split(':').next().unwrap_or("http");
    let client = build_reqwest_client(tls_for_scheme(scheme), REQUEST_TIMEOUT);
    let method = reqwest::Method::from_bytes(b"SUBSCRIBE").expect("SUBSCRIBE is a valid method token");
    let resp = send_with_retry(
        || client
            .request(method.clone(), event_sub_url)
            .header("HOST", host_header)
            .header("CALLBACK", callback_url)
            .header("NT", "upnp:event")
            .header("TIMEOUT", format!("Second-{REQUESTED_TIMEOUT_SECS}")),
        &format!("{context}: SUBSCRIBE"),
    ).await?;
    if !resp.status().is_success() {
        anyhow::bail!("SUBSCRIBE {event_sub_url}: HTTP {}", resp.status());
    }
    let sid = resp
        .headers()
        .get("SID")
        .and_then(|v| v.to_str().ok())
        .ok_or_else(|| anyhow::anyhow!("SUBSCRIBE response has no SID header"))?
        .to_string();
    let timeout_secs = parse_timeout_header(resp.headers().get("TIMEOUT").and_then(|v| v.to_str().ok()));
    Ok((sid, timeout_secs))
}

async fn renew(event_sub_url: &str, host_header: &str, sid: &str, context: &str) -> anyhow::Result<u32> {
    let scheme = event_sub_url.split(':').next().unwrap_or("http");
    let client = build_reqwest_client(tls_for_scheme(scheme), REQUEST_TIMEOUT);
    let method = reqwest::Method::from_bytes(b"SUBSCRIBE").expect("SUBSCRIBE is a valid method token");
    let resp = send_with_retry(
        || client
            .request(method.clone(), event_sub_url)
            .header("HOST", host_header)
            .header("SID", sid),
        &format!("{context}: renew SUBSCRIBE"),
    ).await?;
    if !resp.status().is_success() {
        anyhow::bail!("renew SUBSCRIBE {event_sub_url}: HTTP {}", resp.status());
    }
    Ok(parse_timeout_header(resp.headers().get("TIMEOUT").and_then(|v| v.to_str().ok())))
}

async fn unsubscribe(event_sub_url: &str, host_header: &str, sid: &str, context: &str) {
    let scheme = event_sub_url.split(':').next().unwrap_or("http");
    let client = build_reqwest_client(tls_for_scheme(scheme), REQUEST_TIMEOUT);
    let method = reqwest::Method::from_bytes(b"UNSUBSCRIBE").expect("UNSUBSCRIBE is a valid method token");
    match send_with_retry(
        || client
            .request(method.clone(), event_sub_url)
            .header("HOST", host_header)
            .header("SID", sid),
        &format!("{context}: UNSUBSCRIBE {sid}"),
    ).await {
        Ok(resp) if resp.status().is_success() => dbg(&format!("{context}: UNSUBSCRIBE {sid}: ok")),
        Ok(resp) => dbg(&format!("{context}: UNSUBSCRIBE {sid}: HTTP {}", resp.status())),
        Err(_) => {} // already logged by send_with_retry
    }
}

// ── NOTIFY body parsing ───────────────────────────────────────────────────────
//
// `LastChange`'s inner content is real XML, but shaped as self-closing
// `<Tag val="...">` attributes (`<InstanceID val="0"><TransportState
// val="PLAYING"/>...`, or `<QueueID><LoopMode val="4"/></QueueID>` for
// `PlayQueue`) — a different envelope from `GetInfoEx`'s nested
// `<Tag>value</Tag>` elements (`upnp.rs`'s `parse_info_ex_response`), even
// though several of the same tag names and wire encodings apply. None of
// these tags repeat/nest per NOTIFY, so a flat search for `<TagName ...>`
// works regardless of which wrapper element (`InstanceID` vs `QueueID`)
// happens to contain it — no need to scope the search to a specific
// wrapper block first.

/// One payload delivered from the shared NOTIFY listener thread to the
/// owning `DeviceState`, over a per-session channel — just enough for
/// `state.rs` to decode and compare, parsing itself stays here.
pub struct NotifyPayload {
    pub service: &'static str,
    pub last_change: String,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct AvTransportEvent {
    pub transport_state: Option<String>,
    pub title: Option<String>,
    pub artist: Option<String>,
    pub album: Option<String>,
    /// Shares its vocabulary with `GetInfoEx`'s `PlayMedium` (confirmed
    /// live: `PHONO`/`TIDAL_CONNECT`/`SONGLIST-NETWORK`/`SPOTIFY` all seen
    /// on both) — `playback::mode_from_play_medium()` maps it to a `mode`/
    /// `play_type`-equivalent number the same way `fetch_upnp_fast_poll()`
    /// already does for `GetInfoEx` when `PlayType` is absent.
    pub playback_storage_medium: Option<String>,
    /// Shares its vocabulary with `GetInfoEx`'s `TrackSource` — not acted
    /// on yet, kept for future use alongside `playback_storage_medium`.
    pub track_source: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct RenderingControlEvent {
    pub volume: Option<u32>,
    pub mute: Option<bool>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct PlayQueueEvent {
    pub loop_mode: Option<i32>,
}

/// Extracts `<tag ... val="X" .../>`'s `val` attribute, unescaping XML
/// entities in it — a present tag's value is always at least single-escaped
/// (it's an XML attribute), matching `extract_attr`'s own raw-string return.
fn extract_val_attr(xml: &str, tag: &str) -> Option<String> {
    let needle = format!("<{tag} ");
    let start = xml.find(&needle)?;
    let rest = &xml[start..];
    let tag_end = rest.find('>')?;
    extract_attr(&rest[..tag_end], "val").map(|s| unescape_xml_entities(&s))
}

/// `CurrentTrackMetaData`'s `val` attribute holds a whole DIDL-Lite XML
/// document, escaped to fit inside an XML attribute — the same double-
/// escaping depth `parse_info_ex_response`'s nested-element
/// `TrackMetaData` has, just via a different embedding mechanism (attribute
/// vs. element text). `extract_val_attr` above already did the first
/// unescape pass (recovering real DIDL-Lite tags); a second pass on each
/// extracted leaf value recovers real characters from any DIDL-level entity
/// still present in their content, exactly like that function's title/
/// artist/album handling.
pub fn parse_av_transport_event(last_change: &str) -> AvTransportEvent {
    let transport_state = extract_val_attr(last_change, "TransportState");
    let (title, artist, album) = match extract_val_attr(last_change, "CurrentTrackMetaData") {
        Some(didl) => (
            extract_tag(&didl, "dc:title").map(|s| unescape_xml_entities(&s)),
            extract_tag(&didl, "upnp:artist").map(|s| unescape_xml_entities(&s)),
            extract_tag(&didl, "upnp:album").map(|s| unescape_xml_entities(&s)),
        ),
        None => (None, None, None),
    };
    let playback_storage_medium = extract_val_attr(last_change, "PlaybackStorageMedium");
    let track_source = extract_val_attr(last_change, "TrackSource");
    AvTransportEvent { transport_state, title, artist, album, playback_storage_medium, track_source }
}

pub fn parse_rendering_control_event(last_change: &str) -> RenderingControlEvent {
    RenderingControlEvent {
        volume: extract_val_attr(last_change, "Volume").and_then(|s| s.parse().ok()),
        mute: extract_val_attr(last_change, "Mute").map(|s| s == "1"),
    }
}

/// Checks both `LoopMode` and `LoopMpde` — a confirmed real misspelling on
/// the wire (the `wiim` SDK's own comments), same defensive spirit
/// `decode_loop_mode_http`'s catch-all already has for its own inputs.
pub fn parse_play_queue_event(last_change: &str) -> PlayQueueEvent {
    let loop_mode = extract_val_attr(last_change, "LoopMode")
        .or_else(|| extract_val_attr(last_change, "LoopMpde"))
        .and_then(|s| s.parse().ok());
    PlayQueueEvent { loop_mode }
}

// ── Process-wide NOTIFY listener ─────────────────────────────────────────────
//
// One `tiny_http` listener for the whole process (not one per device, since a
// real device only ever NOTIFYs a URL we chose) on its own dedicated OS
// thread — never the shared tokio `current_thread` runtime, which would be
// starved by a blocking accept loop. Routes incoming NOTIFYs to whichever
// label/service a SID was registered under; an unrecognized SID (a device
// retrying faster than an old UNSUBSCRIBE landed, or in principle a rogue LAN
// host) is logged and dropped, never misrouted.

struct RouteEntry {
    label: String,
    service: &'static str,
    notify_tx: async_channel::Sender<NotifyPayload>,
}

struct Listener {
    port: u16,
    routes: Arc<Mutex<HashMap<String, RouteEntry>>>,
}

static LISTENER: OnceLock<Listener> = OnceLock::new();

/// Starts the listener on first use (0.0.0.0, OS-assigned port — bound
/// broadly rather than to one specific interface, since different devices
/// may be reached via different local interfaces on a multi-homed machine;
/// only the per-device `CALLBACK` host, not the listening interface, needs
/// to be device-specific). Safe to call redundantly — every call after the
/// first just returns the already-running listener's port. Not called from
/// an async context requiring a yield point, so no risk of two callers
/// racing to initialize it twice on this app's single-OS-thread tokio
/// runtime.
fn ensure_listener() -> anyhow::Result<u16> {
    if let Some(l) = LISTENER.get() {
        return Ok(l.port);
    }
    let server = tiny_http::Server::http("0.0.0.0:0")
        .map_err(|e| anyhow::anyhow!("binding GENA NOTIFY listener: {e}"))?;
    let port = server
        .server_addr()
        .to_ip()
        .map(|a| a.port())
        .ok_or_else(|| anyhow::anyhow!("GENA NOTIFY listener has no IP address"))?;
    let routes: Arc<Mutex<HashMap<String, RouteEntry>>> = Arc::new(Mutex::new(HashMap::new()));
    {
        let routes = Arc::clone(&routes);
        std::thread::Builder::new()
            .name("gena-notify".into())
            .spawn(move || serve_notify(server, &routes))
            .expect("spawning GENA NOTIFY listener thread");
    }
    dbg(&format!("NOTIFY listener started on 0.0.0.0:{port}/notify"));
    let _ = LISTENER.set(Listener { port, routes });
    Ok(LISTENER.get().expect("just set above").port)
}

fn register_route(sid: &str, label: String, service: &'static str, notify_tx: async_channel::Sender<NotifyPayload>) -> anyhow::Result<u16> {
    let port = ensure_listener()?;
    if let Some(l) = LISTENER.get() {
        l.routes.lock().unwrap().insert(sid.to_string(), RouteEntry { label, service, notify_tx });
    }
    Ok(port)
}

fn unregister_route(sid: &str) {
    if let Some(l) = LISTENER.get() {
        l.routes.lock().unwrap().remove(sid);
    }
}

/// Request-handling loop for the shared NOTIFY listener thread. Every
/// request goes through `catch_unwind` — same discipline `wiim-simulator.rs`
/// established — so a panic handling one NOTIFY can't kill the listener for
/// every other device sharing it. Logs via `dbg_notify()` (summary always,
/// full `LastChange` content only under `--debug=gena:verbose`), and
/// forwards it (unparsed — parsing happens on the receiving end, in
/// `state.rs`, which is where the comparison target lives) to the owning
/// session's channel via `try_send`, never `send_blocking`: this thread
/// must never block on a momentarily-full channel — losing an occasional
/// sample under backpressure is fine, applying it a tick late isn't worth
/// stalling every other device's NOTIFY delivery over.
fn serve_notify(server: tiny_http::Server, routes: &Arc<Mutex<HashMap<String, RouteEntry>>>) {
    for mut request in server.incoming_requests() {
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let sid = request
                .headers()
                .iter()
                .find(|h| h.field.equiv("SID"))
                .map(|h| h.value.as_str().to_string());
            let seq = request
                .headers()
                .iter()
                .find(|h| h.field.equiv("SEQ"))
                .map(|h| h.value.as_str().to_string());
            let mut body = String::new();
            let _ = request.as_reader().read_to_string(&mut body);

            let route = sid.as_deref().and_then(|s| {
                let routes = routes.lock().unwrap();
                routes.get(s).map(|r| (r.label.clone(), r.service, r.notify_tx.clone()))
            });
            match route {
                Some((label, service, notify_tx)) => {
                    let last_change = extract_tag(&body, "LastChange").map(|s| unescape_xml_entities(&s));
                    let summary = format!("{label}: {service}: NOTIFY (seq={})", seq.as_deref().unwrap_or("?"));
                    match last_change {
                        Some(lc) => {
                            dbg_notify(&summary, Some(&lc));
                            let _ = notify_tx.try_send(NotifyPayload { service, last_change: lc });
                        }
                        None => dbg_notify(&format!("{summary} with no LastChange property"), Some(&body)),
                    }
                }
                None => {
                    dbg(&format!("NOTIFY for unrecognized SID {:?} — dropped", sid.as_deref().unwrap_or("?")));
                }
            }
        }));
        if result.is_err() {
            eprintln!("{} [gena] internal error handling a NOTIFY (see panic message above)", super::timestamp());
        }
        let response = tiny_http::Response::from_string("").with_status_code(200);
        let _ = request.respond(response);
    }
}

// ── Health tracking ───────────────────────────────────────────────────────────
//
// Per-service health state machine — one independent instance per service
// (`AVTransport`/`RenderingControl`/`PlayQueue`), not one shared per-device
// blob: a device can be `Healthy` on two of these and stuck on the third at
// the same time (confirmed real — a device that accepts a
// `RenderingControl` `SUBSCRIBE` but never actually fires a NOTIFY for a
// volume/mute change, while `AVTransport`/`PlayQueue` eventing works fine on
// the same device at the same time).

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum GenaHealth {
    /// No GENA session for this service right now (GENA disabled, session
    /// not yet started, or the device doesn't advertise this service).
    #[default]
    Off,
    /// A `SUBSCRIBE` is held (or being retried) but no real NOTIFY has been
    /// confirmed consistent with polling yet since this attempt — covers
    /// both the very first subscribe and post-recovery re-subscribe
    /// identically (Ben: "recovery is the same as initial start of GENA").
    Subscribing,
    /// At least one real NOTIFY has been confirmed consistent with polling
    /// for this service.
    Healthy,
    /// A single poll-vs-NOTIFY mismatch was seen (poll request sent after
    /// the last NOTIFY, but still disagreed) — not yet acted on, could be a
    /// race with a clarifying NOTIFY still in flight. A second consecutive
    /// mismatch confirms unhealthy, which isn't its own variant — it's
    /// acted on immediately (forced `UNSUBSCRIBE`+`SUBSCRIBE`), landing
    /// straight back on `Subscribing`.
    MaybeUnhealthy,
}

/// Per-service GENA state — deliberately *just* the health, nothing else.
/// An earlier version of this also kept a shadow snapshot of the last
/// NOTIFY event to re-compare against each poll, but that's redundant once
/// a NOTIFY's fields are applied directly into `PlaybackState`
/// (`DeviceState::apply_gena_notify()`): `playback` itself already *is*
/// "what GENA last told us," so the poll side doesn't need its own copy to
/// compare against — it already does its own change-detection against the
/// previous raw response (`process_poll_http()`/`process_poll_upnp()`'s
/// existing `*_changed` diffing), so this just rides on that instead of
/// re-implementing a parallel comparison.
#[derive(Debug, Clone, Copy, Default)]
pub struct GenaServiceState {
    pub health: GenaHealth,
}

impl GenaServiceState {
    /// A real NOTIFY was just applied for this service — always (re)confirms
    /// `Healthy`, self-healing from `MaybeUnhealthy` or advancing from
    /// `Subscribing`: a NOTIFY arriving at all is evidence the subscription
    /// is alive and delivering, regardless of what it happened to carry.
    /// Returns the previous health, so the caller can log an actual
    /// transition rather than a no-op re-confirmation.
    pub fn notify_received(&mut self) -> GenaHealth {
        let old = self.health;
        self.health = GenaHealth::Healthy;
        old
    }

    /// The poll's own existing change-detection (`process_poll_http()`/
    /// `process_poll_upnp()`'s `*_changed` diffing) independently found a
    /// new value for something this service should already have delivered
    /// via NOTIFY, but didn't (or delivered differently). Degrades one
    /// step: `Healthy` -> `MaybeUnhealthy` (could be a harmless race — Ben:
    /// "if it's stale, we'll get another one"), `MaybeUnhealthy` ->
    /// `Subscribing` (a *second* miss confirms unhealthy — the caller must
    /// force a real `UNSUBSCRIBE`+`SUBSCRIBE`), `Subscribing`/`Off`
    /// unchanged (nothing to degrade from — no session, or no confirmed
    /// health yet to lose). Returns `true` exactly on the confirmed-
    /// unhealthy transition.
    pub fn poll_mismatch(&mut self) -> bool {
        let old = self.health;
        self.health = match old {
            GenaHealth::Healthy => GenaHealth::MaybeUnhealthy,
            GenaHealth::MaybeUnhealthy => GenaHealth::Subscribing,
            other => other,
        };
        old == GenaHealth::MaybeUnhealthy
    }
}

// ── Per-device session ───────────────────────────────────────────────────────

struct HeldSub {
    event_sub_url: String,
    host_header: String,
    sid: String,
}

/// Per-service context needed to spawn a fresh `service_loop` task on
/// demand — captured once at `GenaSession::start()` time (right alongside
/// the initial spawn) so `GenaSessionHandle::force_resubscribe()` can
/// restart one service later without re-running discovery for the other
/// two.
struct ServiceContext {
    event_sub_url: String,
    host_header: String,
}

/// Cheaply-`Clone`able handle to a live `GenaSession`'s mutable state, so
/// `state.rs`'s health-check code can force one service's
/// `UNSUBSCRIBE`+`SUBSCRIBE` cycle (`force_resubscribe()`) from an
/// `rt().spawn()`'d async block without holding the owning `DeviceState`'s
/// `RefCell` borrow across an `.await` — same reason `stop_gena_session()`
/// already `take()`s the whole session out first, just finer-grained here
/// since the other services must keep running regardless of one service's
/// health outcome.
#[derive(Clone)]
pub struct GenaSessionHandle {
    tasks: Arc<Mutex<HashMap<&'static str, tokio::task::JoinHandle<()>>>>,
    subs: Arc<Mutex<HashMap<&'static str, HeldSub>>>,
    contexts: Arc<HashMap<&'static str, ServiceContext>>,
    callback_url: Arc<str>,
    label: Arc<str>,
    notify_tx: async_channel::Sender<NotifyPayload>,
}

impl GenaSessionHandle {
    /// Aborts `service`'s current task, best-effort `UNSUBSCRIBE`s its held
    /// subscription (if any — a service still stuck retrying its initial
    /// `SUBSCRIBE` has nothing to unsubscribe), then spawns a fresh
    /// `service_loop` for it, re-entering the same "(re)subscribe, retrying
    /// indefinitely" entry point a real renewal failure already goes
    /// through. No-op if `service` isn't one this session actually
    /// holds a context for (the device never advertised it — nothing to
    /// restart).
    pub async fn force_resubscribe(&self, service: &'static str) {
        if let Some(task) = self.tasks.lock().unwrap().remove(service) {
            task.abort();
        }
        let sub = self.subs.lock().unwrap().remove(service);
        if let Some(sub) = sub {
            unregister_route(&sub.sid);
            unsubscribe(
                &sub.event_sub_url, &sub.host_header, &sub.sid,
                &format!("{}: {service} (forced resubscribe)", self.label),
            ).await;
        }
        let Some(ctx) = self.contexts.get(service) else { return };
        let task = tokio::spawn(service_loop(
            service, ctx.event_sub_url.clone(), ctx.host_header.clone(),
            self.callback_url.to_string(), self.label.to_string(),
            Arc::clone(&self.subs), self.notify_tx.clone(),
        ));
        self.tasks.lock().unwrap().insert(service, task);
    }
}

/// One device's live GENA subscriptions (whichever of the three services it
/// actually advertised). Owns the renewal tasks; `stop()` must be called
/// (from an async context, since it does real `UNSUBSCRIBE` network calls)
/// before dropping — plain `Drop` only aborts the renewal tasks as a safety
/// net, it can't do the network round-trip itself.
pub struct GenaSession {
    handle: GenaSessionHandle,
}

impl Drop for GenaSession {
    fn drop(&mut self) {
        for task in self.handle.tasks.lock().unwrap().values() {
            task.abort();
        }
    }
}

impl GenaSession {
    /// Discovers whichever GENA-eventable services `ip` advertises and
    /// spawns one persistent `service_loop` task per service — each keeps
    /// retrying `SUBSCRIBE` forever (starting immediately, not just after a
    /// later failure — a failed *initial* attempt is retried exactly like a
    /// post-renewal recovery would be, see `service_loop`'s doc comment) —
    /// logging as it goes under `--debug=gena`. Only the up-front steps that
    /// apply to every service at once (discovery, callback IP detection,
    /// listener startup) can fail the whole session outright; an individual
    /// service's `SUBSCRIBE` failing never does, exactly like a real device
    /// that only advertises some of the three would look from here.
    /// `notify_tx` is where parsed-ready-to-compare NOTIFY payloads for this
    /// device end up — the caller (`state.rs`) owns the receiving end and
    /// the actual comparison logic.
    pub async fn start(ip: &str, label: String, notify_tx: async_channel::Sender<NotifyPayload>) -> Self {
        let tasks: Arc<Mutex<HashMap<&'static str, tokio::task::JoinHandle<()>>>> = Arc::new(Mutex::new(HashMap::new()));
        let subs: Arc<Mutex<HashMap<&'static str, HeldSub>>> = Arc::new(Mutex::new(HashMap::new()));
        let label_arc: Arc<str> = Arc::from(label.as_str());

        let found = match discover_event_sub_urls(ip).await {
            Ok(f) if !f.is_empty() => f,
            Ok(_) => {
                dbg(&format!("{label}: device advertises no GENA-eventable services"));
                return Self::empty(tasks, subs, label_arc, notify_tx);
            }
            Err(e) => {
                dbg(&format!("{label}: description.xml discovery failed: {e}"));
                return Self::empty(tasks, subs, label_arc, notify_tx);
            }
        };

        let local_ip = match find_local_ip(ip.split(':').next().unwrap_or(ip)) {
            Ok(addr) => addr,
            Err(e) => {
                dbg(&format!("{label}: could not determine local callback IP: {e}"));
                return Self::empty(tasks, subs, label_arc, notify_tx);
            }
        };
        let port = match ensure_listener() {
            Ok(p) => p,
            Err(e) => {
                dbg(&format!("{label}: could not start NOTIFY listener: {e}"));
                return Self::empty(tasks, subs, label_arc, notify_tx);
            }
        };
        let callback_url = format!("<http://{local_ip}:{port}/notify>");
        let callback_url_arc: Arc<str> = Arc::from(callback_url.as_str());

        let mut contexts = HashMap::new();
        for (service, event_sub_url) in found {
            let host_header = host_port_of(&event_sub_url);
            contexts.insert(service, ServiceContext {
                event_sub_url: event_sub_url.clone(),
                host_header: host_header.clone(),
            });
            let task = tokio::spawn(service_loop(
                service, event_sub_url, host_header, callback_url.clone(),
                label.clone(), Arc::clone(&subs), notify_tx.clone(),
            ));
            tasks.lock().unwrap().insert(service, task);
        }

        Self {
            handle: GenaSessionHandle {
                tasks, subs, contexts: Arc::new(contexts),
                callback_url: callback_url_arc, label: label_arc, notify_tx,
            },
        }
    }

    /// Shared tail for every early-return path in `start()` above — a
    /// session that never got as far as finding anything to subscribe to
    /// still needs a valid (empty) handle, since `state.rs` unconditionally
    /// stores whatever `start()` returns.
    fn empty(
        tasks: Arc<Mutex<HashMap<&'static str, tokio::task::JoinHandle<()>>>>,
        subs: Arc<Mutex<HashMap<&'static str, HeldSub>>>,
        label: Arc<str>,
        notify_tx: async_channel::Sender<NotifyPayload>,
    ) -> Self {
        Self {
            handle: GenaSessionHandle {
                tasks, subs, contexts: Arc::new(HashMap::new()),
                callback_url: Arc::from(""), label, notify_tx,
            },
        }
    }

    /// Cheap `Clone` of this session's mutable-state handle — see
    /// `GenaSessionHandle`'s doc comment for why `state.rs`'s health-check
    /// code needs this instead of a `&GenaSession` reference.
    pub fn handle(&self) -> GenaSessionHandle {
        self.handle.clone()
    }

    /// Aborts every service's task and sends `UNSUBSCRIBE` for whichever
    /// subscriptions are actually held at this moment (a service still
    /// stuck retrying its initial/recovery `SUBSCRIBE` simply has nothing to
    /// unsubscribe). Async because `UNSUBSCRIBE` is a real network call —
    /// callers must run this on `rt()`, not from a GTK signal handler
    /// directly (same rule as every other device network call in this app).
    pub async fn stop(self) {
        for (_, task) in self.handle.tasks.lock().unwrap().drain() {
            task.abort();
        }
        let subs = std::mem::take(&mut *self.handle.subs.lock().unwrap());
        for (service, sub) in subs {
            unregister_route(&sub.sid);
            unsubscribe(&sub.event_sub_url, &sub.host_header, &sub.sid, service).await;
        }
    }
}

/// One service's entire lifetime, for as long as the session lives: keep
/// retrying `SUBSCRIBE` every `RETRY_INTERVAL_SECS` **until it succeeds**,
/// then renew on schedule until a renewal fails, then go right back to
/// retrying `SUBSCRIBE` — never a one-shot give-up at any point. This is
/// also the fix for "does a failed *initial* `SUBSCRIBE` ever get
/// retried": it runs through the exact same retry loop a post-renewal
/// recovery does, since recovery genuinely is the same thing as an initial
/// start. Only ends when its `JoinHandle` is aborted externally
/// (`GenaSession::stop()`/`Drop`) — never exits on its own.
async fn service_loop(
    service: &'static str,
    event_sub_url: String,
    host_header: String,
    callback_url: String,
    label: String,
    subs: Arc<Mutex<HashMap<&'static str, HeldSub>>>,
    notify_tx: async_channel::Sender<NotifyPayload>,
) {
    loop {
        // (Re)subscribe, retrying indefinitely on failure — this covers
        // both the very first attempt and post-renewal-failure recovery
        // identically.
        let (sid, mut timeout_secs) = loop {
            match subscribe(&event_sub_url, &host_header, &callback_url, &format!("{label}: {service}")).await {
                Ok(ok) => break ok,
                Err(_) => {
                    // subscribe() already logged the detailed error via
                    // log_request_error — this is just visibility that
                    // we're still trying, not giving up.
                    dbg(&format!("{label}: {service}: still trying, next SUBSCRIBE attempt in {RETRY_INTERVAL_SECS}s"));
                    tokio::time::sleep(Duration::from_secs(RETRY_INTERVAL_SECS)).await;
                }
            }
        };
        dbg(&format!("{label}: {service}: subscribed (sid={sid}, timeout={timeout_secs}s)"));
        let _ = register_route(&sid, label.clone(), service, notify_tx.clone());
        subs.lock().unwrap().insert(service, HeldSub {
            event_sub_url: event_sub_url.clone(),
            host_header: host_header.clone(),
            sid: sid.clone(),
        });

        // Renew on schedule until a renewal fails, then loop back to
        // (re)subscribing. Checks `subs` on each wake since that's the one
        // point this task can notice `GenaSession::stop()` already ran (its
        // task will also be aborted around the same time, but this avoids
        // firing one last unnecessary renewal request in the meantime).
        loop {
            let delay = timeout_secs.saturating_sub(60).max(30);
            tokio::time::sleep(Duration::from_secs(delay as u64)).await;
            if !subs.lock().unwrap().contains_key(service) {
                return;
            }
            match renew(&event_sub_url, &host_header, &sid, &format!("{label}: {service}")).await {
                Ok(new_timeout) => {
                    timeout_secs = new_timeout;
                    dbg(&format!("{label}: {service}: renewed (sid={sid}, timeout={timeout_secs}s)"));
                }
                Err(_) => {
                    dbg(&format!("{label}: {service}: renewal failed, will resubscribe"));
                    unregister_route(&sid);
                    subs.lock().unwrap().remove(service);
                    break;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── GenaServiceState::{notify_received,poll_mismatch}() ───────────────

    #[test]
    fn notify_received_confirms_healthy_from_subscribing() {
        let mut s = GenaServiceState { health: GenaHealth::Subscribing };
        let old = s.notify_received();
        assert_eq!(old, GenaHealth::Subscribing);
        assert_eq!(s.health, GenaHealth::Healthy);
    }

    #[test]
    fn notify_received_self_heals_from_maybe_unhealthy() {
        let mut s = GenaServiceState { health: GenaHealth::MaybeUnhealthy };
        s.notify_received();
        assert_eq!(s.health, GenaHealth::Healthy);
    }

    #[test]
    fn notify_received_is_a_no_op_transition_while_already_healthy() {
        let mut s = GenaServiceState { health: GenaHealth::Healthy };
        let old = s.notify_received();
        assert_eq!(old, GenaHealth::Healthy);
        assert_eq!(s.health, GenaHealth::Healthy);
    }

    #[test]
    fn poll_mismatch_degrades_healthy_to_maybe_unhealthy() {
        let mut s = GenaServiceState { health: GenaHealth::Healthy };
        let confirmed_unhealthy = s.poll_mismatch();
        assert!(!confirmed_unhealthy);
        assert_eq!(s.health, GenaHealth::MaybeUnhealthy);
    }

    #[test]
    fn poll_mismatch_confirms_unhealthy_on_second_miss() {
        let mut s = GenaServiceState { health: GenaHealth::MaybeUnhealthy };
        let confirmed_unhealthy = s.poll_mismatch();
        assert!(confirmed_unhealthy);
        assert_eq!(s.health, GenaHealth::Subscribing);
    }

    #[test]
    fn poll_mismatch_is_a_no_op_with_no_session_or_no_confirmed_health_yet() {
        let mut off = GenaServiceState { health: GenaHealth::Off };
        assert!(!off.poll_mismatch());
        assert_eq!(off.health, GenaHealth::Off);

        let mut subscribing = GenaServiceState { health: GenaHealth::Subscribing };
        assert!(!subscribing.poll_mismatch());
        assert_eq!(subscribing.health, GenaHealth::Subscribing);
    }

    // Shapes taken directly from real captured NOTIFY bodies (confirmed
    // from the `wiim` SDK's own parsing code), not invented — same
    // discipline the rest of this codebase's wire-format tests follow.

    #[test]
    fn av_transport_event_parses_transport_state_and_title() {
        let last_change = r#"<Event xmlns="urn:schemas-upnp-org:metadata-1-0/AVT/">
  <InstanceID val="0">
    <TransportState val="PLAYING"/>
    <CurrentTrackMetaData val="&lt;DIDL-Lite&gt;&lt;item&gt;&lt;dc:title&gt;Foo &amp;amp; Bar&lt;/dc:title&gt;&lt;upnp:artist&gt;Some Artist&lt;/upnp:artist&gt;&lt;upnp:album&gt;Some Album&lt;/upnp:album&gt;&lt;/item&gt;&lt;/DIDL-Lite&gt;"/>
  </InstanceID>
</Event>"#;
        let ev = parse_av_transport_event(last_change);
        assert_eq!(ev.transport_state.as_deref(), Some("PLAYING"));
        // Double-escaped (DIDL-Lite's own serialization escaping a literal
        // "&", then the whole DIDL-Lite document escaped again to fit
        // inside the outer `val="..."` attribute) — same depth as
        // `parse_info_ex_response`'s nested-element `TrackMetaData` case,
        // just via a different embedding mechanism.
        assert_eq!(ev.title.as_deref(), Some("Foo & Bar"));
        assert_eq!(ev.artist.as_deref(), Some("Some Artist"));
        assert_eq!(ev.album.as_deref(), Some("Some Album"));
    }

    /// Real captured shapes: an Audio Pro C5 (phono input) and an Audio Pro
    /// with built-in TIDAL, respectively.
    #[test]
    fn av_transport_event_parses_playback_storage_medium_and_track_source() {
        let last_change = r#"<Event xmlns="urn:schemas-upnp-org:metadata-1-0/AVT/">
  <InstanceID val="0">
    <TransportState val="PLAYING"/>
    <PlaybackStorageMedium val="PHONO"/>
  </InstanceID>
</Event>"#;
        let ev = parse_av_transport_event(last_change);
        assert_eq!(ev.playback_storage_medium.as_deref(), Some("PHONO"));
        assert_eq!(ev.track_source, None);

        let last_change_tidal = r#"<Event xmlns="urn:schemas-upnp-org:metadata-1-0/AVT/">
  <InstanceID val="0">
    <TransportState val="PLAYING"/>
    <PlaybackStorageMedium val="SONGLIST-NETWORK"/>
    <TrackSource val="Tidal"/>
  </InstanceID>
</Event>"#;
        let ev = parse_av_transport_event(last_change_tidal);
        assert_eq!(ev.playback_storage_medium.as_deref(), Some("SONGLIST-NETWORK"));
        assert_eq!(ev.track_source.as_deref(), Some("Tidal"));
    }

    #[test]
    fn av_transport_event_missing_metadata_gives_none_fields() {
        let last_change = r#"<Event xmlns="urn:schemas-upnp-org:metadata-1-0/AVT/">
  <InstanceID val="0">
    <TransportState val="PAUSED_PLAYBACK"/>
  </InstanceID>
</Event>"#;
        let ev = parse_av_transport_event(last_change);
        assert_eq!(ev.transport_state.as_deref(), Some("PAUSED_PLAYBACK"));
        assert_eq!(ev.title, None);
        assert_eq!(ev.artist, None);
        assert_eq!(ev.album, None);
    }

    #[test]
    fn rendering_control_event_parses_volume_and_mute() {
        let last_change = r#"<Event xmlns="urn:schemas-upnp-org:metadata-1-0/RCS/">
  <InstanceID val="0">
    <Volume channel="Master" val="50"/>
    <Mute channel="Master" val="0"/>
  </InstanceID>
</Event>"#;
        let ev = parse_rendering_control_event(last_change);
        assert_eq!(ev.volume, Some(50));
        assert_eq!(ev.mute, Some(false));
    }

    #[test]
    fn play_queue_event_parses_loop_mode() {
        let last_change = r#"<Event xmlns="urn:schemas-wiimu-com:metadata-1-0/PlayQueue/">
  <QueueID>
    <LoopMode val="4"/>
  </QueueID>
</Event>"#;
        let ev = parse_play_queue_event(last_change);
        assert_eq!(ev.loop_mode, Some(4));
    }

    /// Confirmed real wire quirk (the `wiim` SDK's own comments): `LoopMode`
    /// sometimes arrives misspelled `LoopMpde`.
    #[test]
    fn play_queue_event_handles_loopmpde_misspelling() {
        let last_change = r#"<Event xmlns="urn:schemas-wiimu-com:metadata-1-0/PlayQueue/">
  <QueueID>
    <LoopMpde val="2"/>
  </QueueID>
</Event>"#;
        let ev = parse_play_queue_event(last_change);
        assert_eq!(ev.loop_mode, Some(2));
    }

    #[test]
    fn extract_val_attr_returns_none_for_absent_tag() {
        assert_eq!(extract_val_attr("<Event></Event>", "TransportState"), None);
    }
}
