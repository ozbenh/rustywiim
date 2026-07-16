/// UPnP GENA (General Event Notification Architecture) eventing — `SUBSCRIBE`/
/// `UNSUBSCRIBE`/renewal plumbing plus the process-wide NOTIFY listener.
/// Structurally a sibling to `upnp.rs` the way `upnp.rs` is to `api.rs`:
/// `upnp.rs` stays request/response SOAP calls only, this module owns the
/// long-lived-subscription side of UPnP (a genuinely different shape —
/// persistent state, renewal timers, an inbound listener — not just more SOAP
/// actions). `state.rs` decides *when* a device should hold a GENA session;
/// this module owns *how*.
///
use std::collections::HashMap;
use std::net::UdpSocket;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use super::api::{build_reqwest_client, TlsMode};
use super::upnp::{discover_description, extract_tag, extract_url_for_service, unescape_xml_entities};

pub static DEBUG_GENA: AtomicBool = AtomicBool::new(false);

fn dbg(msg: &str) {
    if DEBUG_GENA.load(Ordering::Relaxed) {
        println!("{} [gena] {msg}", super::timestamp());
    }
}

const REQUEST_TIMEOUT: Duration = Duration::from_secs(5);
/// Requested subscription lifetime — the device may grant a shorter one;
/// always use what it actually returns, never assume this was honored
/// verbatim.
const REQUESTED_TIMEOUT_SECS: u32 = 1800;
/// Retry interval once a subscription is confirmed dead (renewal failed and
/// a fresh `SUBSCRIBE` also failed)
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

fn register_route(sid: &str, label: String, service: &'static str) -> anyhow::Result<u16> {
    let port = ensure_listener()?;
    if let Some(l) = LISTENER.get() {
        l.routes.lock().unwrap().insert(sid.to_string(), RouteEntry { label, service });
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
/// every other device sharing it. Phase 1: logs only, no field parsing into
/// `PlaybackState`.
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

            match sid.as_deref().and_then(|s| routes.lock().unwrap().get(s).map(|r| (r.label.clone(), r.service))) {
                Some((label, service)) => {
                    let last_change = extract_tag(&body, "LastChange").map(|s| unescape_xml_entities(&s));
                    match last_change {
                        Some(lc) => dbg(&format!("{label}: {service}: NOTIFY (seq={}) {lc}", seq.as_deref().unwrap_or("?"))),
                        None => dbg(&format!("{label}: {service}: NOTIFY (seq={}) with no LastChange property: {body}", seq.as_deref().unwrap_or("?"))),
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

// ── Per-device session ───────────────────────────────────────────────────────

struct HeldSub {
    event_sub_url: String,
    host_header: String,
    sid: String,
}

/// One device's live GENA subscriptions (whichever of the three services it
/// actually advertised). Owns the renewal tasks; `stop()` must be called
/// (from an async context, since it does real `UNSUBSCRIBE` network calls)
/// before dropping — plain `Drop` only aborts the renewal tasks as a safety
/// net, it can't do the network round-trip itself.
pub struct GenaSession {
    tasks: Vec<tokio::task::JoinHandle<()>>,
    subs: Arc<Mutex<HashMap<&'static str, HeldSub>>>,
}

impl Drop for GenaSession {
    fn drop(&mut self) {
        for task in &self.tasks {
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
    pub async fn start(ip: &str, label: String) -> Self {
        let subs: Arc<Mutex<HashMap<&'static str, HeldSub>>> = Arc::new(Mutex::new(HashMap::new()));
        let mut tasks = Vec::new();

        let found = match discover_event_sub_urls(ip).await {
            Ok(f) if !f.is_empty() => f,
            Ok(_) => {
                dbg(&format!("{label}: device advertises no GENA-eventable services"));
                return Self { tasks, subs };
            }
            Err(e) => {
                dbg(&format!("{label}: description.xml discovery failed: {e}"));
                return Self { tasks, subs };
            }
        };

        let local_ip = match find_local_ip(ip.split(':').next().unwrap_or(ip)) {
            Ok(addr) => addr,
            Err(e) => {
                dbg(&format!("{label}: could not determine local callback IP: {e}"));
                return Self { tasks, subs };
            }
        };
        let port = match ensure_listener() {
            Ok(p) => p,
            Err(e) => {
                dbg(&format!("{label}: could not start NOTIFY listener: {e}"));
                return Self { tasks, subs };
            }
        };
        let callback_url = format!("<http://{local_ip}:{port}/notify>");

        for (service, event_sub_url) in found {
            let host_header = host_port_of(&event_sub_url);
            let task = tokio::spawn(service_loop(
                service, event_sub_url, host_header, callback_url.clone(),
                label.clone(), Arc::clone(&subs),
            ));
            tasks.push(task);
        }

        Self { tasks, subs }
    }

    /// Aborts every service's task and sends `UNSUBSCRIBE` for whichever
    /// subscriptions are actually held at this moment (a service still
    /// stuck retrying its initial/recovery `SUBSCRIBE` simply has nothing to
    /// unsubscribe). Async because `UNSUBSCRIBE` is a real network call —
    /// callers must run this on `rt()`, not from a GTK signal handler
    /// directly (same rule as every other device network call in this app).
    pub async fn stop(mut self) {
        for task in self.tasks.drain(..) {
            task.abort();
        }
        let subs = std::mem::take(&mut *self.subs.lock().unwrap());
        for (service, sub) in subs {
            unregister_route(&sub.sid);
            unsubscribe(&sub.event_sub_url, &sub.host_header, &sub.sid, service).await;
        }
    }
}

/// One service's entire lifetime, for as long as the session lives: keep
/// retrying `SUBSCRIBE` every `RETRY_INTERVAL_SECS` **until it succeeds**,
/// then renew on schedule until a renewal fails, then go right back to
/// retrying `SUBSCRIBE` — never a one-shot give-up at any point.
/// This is also handles retries for a failed initial `SUBSCRIBE`, it runs
/// through the exact same retry loop a post-renewal recovery does, since
/// "recovery is the same as initial start of GENA".
/// Only ends when its `JoinHandle` is aborted externally
/// (`GenaSession::stop()`/`Drop`) — never exits on its own.
async fn service_loop(
    service: &'static str,
    event_sub_url: String,
    host_header: String,
    callback_url: String,
    label: String,
    subs: Arc<Mutex<HashMap<&'static str, HeldSub>>>,
) {
    loop {
        // Phase 1: (re)subscribe, retrying indefinitely on failure — this
        // covers both the very first attempt and post-renewal-failure
        // recovery identically.
        let (sid, mut timeout_secs) = loop {
            match subscribe(&event_sub_url, &host_header, &callback_url, &format!("{label}: {service}")).await {
                Ok(ok) => break ok,
                Err(_) => {
                    // subscribe() already logged the detailed error via
                    // log_request_error — this is just visibility that
                    // we're still trying, not giving up (there's no other
                    // log line distinguishing "still retrying" from
                    // "silently stuck" until Phase 3's GenaHealth state
                    // machine exists).
                    dbg(&format!("{label}: {service}: still trying, next SUBSCRIBE attempt in {RETRY_INTERVAL_SECS}s"));
                    tokio::time::sleep(Duration::from_secs(RETRY_INTERVAL_SECS)).await;
                }
            }
        };
        dbg(&format!("{label}: {service}: subscribed (sid={sid}, timeout={timeout_secs}s)"));
        let _ = register_route(&sid, label.clone(), service);
        subs.lock().unwrap().insert(service, HeldSub {
            event_sub_url: event_sub_url.clone(),
            host_header: host_header.clone(),
            sid: sid.clone(),
        });

        // Phase 2: renew on schedule until a renewal fails, then loop back
        // to phase 1. Checks `subs` on each wake since that's the one point
        // this task can notice `GenaSession::stop()` already ran (its task
        // will also be aborted around the same time, but this avoids
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
