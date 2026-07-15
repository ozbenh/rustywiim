/// `wiim-gena-test <ip>` — Phase 0 of `GENA.md`'s plan: a standalone,
/// permanent diagnostic tool that discovers a real device's `AVTransport`/
/// `RenderingControl`/`PlayQueue` `eventSubURL`s from its `description.xml`,
/// sends `SUBSCRIBE` for each, runs a `NOTIFY` listener on its own thread,
/// prints every notification received, renews each subscription at
/// `max(30, timeout - 60)` seconds (the `wiim` SDK's own rule), and cleanly
/// `UNSUBSCRIBE`s everything on Ctrl-C. Deliberately self-contained rather
/// than reusing `device/upnp.rs` — this is meant for ad hoc real-hardware
/// probing (per `GENA.md`'s Phase 0 gate), not production plumbing, so it
/// duplicates the small amount of `description.xml`/tag-extraction logic
/// `upnp.rs` and `wiim-capture.rs` each already have their own copy of,
/// rather than depending on either.
use rustywiim::device::api::{build_reqwest_client, TlsMode};
use std::collections::HashMap;
use std::net::UdpSocket;
use std::sync::{Arc, Mutex};
use std::time::Duration;

const REQUEST_TIMEOUT: Duration = Duration::from_secs(5);
const DESCRIPTION_PORTS: &[u16] = &[49152, 59152];
/// Requested subscription lifetime — the device may grant a shorter one;
/// callers must use the `TIMEOUT` it actually returns, never assume this
/// was honored verbatim.
const REQUESTED_TIMEOUT_SECS: u32 = 1800;

struct WantedService {
    name: &'static str,
    service_type_substr: &'static str,
}

const WANTED_SERVICES: &[WantedService] = &[
    WantedService { name: "AVTransport", service_type_substr: ":service:AVTransport:" },
    WantedService { name: "RenderingControl", service_type_substr: ":service:RenderingControl:" },
    WantedService { name: "PlayQueue", service_type_substr: "wiimu-com:service:PlayQueue" },
];

fn usage() -> ! {
    eprintln!("usage: wiim-gena-test <ip>");
    std::process::exit(1);
}

fn tls_for_scheme(scheme: &str) -> TlsMode {
    if scheme == "https" { TlsMode::HttpsAny } else { TlsMode::Http }
}

// ── description.xml parsing — same minimal, non-namespace-aware tag
// extraction `device/upnp.rs` and `wiim-capture.rs` each keep their own copy
// of (see this file's top doc comment for why this one isn't shared either).

fn extract_tag(xml: &str, tag: &str) -> Option<String> {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    let start = xml.find(&open)? + open.len();
    let end = xml[start..].find(&close)? + start;
    Some(xml[start..end].trim().to_string())
}

fn extract_service_blocks(xml: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut rest = xml;
    while let Some(start) = rest.find("<service>") {
        let after = &rest[start + "<service>".len()..];
        match after.find("</service>") {
            Some(end) => {
                out.push(after[..end].to_string());
                rest = &after[end + "</service>".len()..];
            }
            None => break,
        }
    }
    out
}

fn resolve_url(description_url: &str, maybe_relative: &str) -> String {
    if maybe_relative.starts_with("http://") || maybe_relative.starts_with("https://") {
        return maybe_relative.to_string();
    }
    let Some(scheme_end) = description_url.find("://") else {
        return maybe_relative.to_string();
    };
    let after_scheme = &description_url[scheme_end + 3..];
    let origin_end = after_scheme.find('/').map(|i| scheme_end + 3 + i).unwrap_or(description_url.len());
    let origin = &description_url[..origin_end];
    if maybe_relative.starts_with('/') {
        format!("{origin}{maybe_relative}")
    } else {
        format!("{origin}/{maybe_relative}")
    }
}

fn extract_event_sub_url(description_xml: &str, description_url: &str, service_type_substr: &str) -> Option<String> {
    for block in extract_service_blocks(description_xml) {
        let Some(service_type) = extract_tag(&block, "serviceType") else { continue };
        if !service_type.contains(service_type_substr) { continue; }
        let raw = extract_tag(&block, "eventSubURL")?;
        return Some(resolve_url(description_url, &raw));
    }
    None
}

/// `"scheme://host:port/path"` -> `"host:port"`, for the `HOST` header GENA
/// requires on every SUBSCRIBE/renewal/UNSUBSCRIBE.
fn host_port_of(url: &str) -> String {
    let after_scheme = url.split_once("://").map(|(_, rest)| rest).unwrap_or(url);
    after_scheme.split('/').next().unwrap_or(after_scheme).to_string()
}

async fn discover_description(ip: &str) -> anyhow::Result<(String, String)> {
    let mut last_err: Option<anyhow::Error> = None;
    for scheme in ["http", "https"] {
        for &port in DESCRIPTION_PORTS {
            let url = format!("{scheme}://{ip}:{port}/description.xml");
            eprintln!("[wiim-gena-test] trying description.xml at {url}");
            let client = build_reqwest_client(tls_for_scheme(scheme), REQUEST_TIMEOUT);
            match client.get(&url).send().await {
                Ok(resp) if resp.status().is_success() => match resp.text().await {
                    Ok(body) => return Ok((body, url)),
                    Err(e) => last_err = Some(e.into()),
                },
                Ok(resp) => last_err = Some(anyhow::anyhow!("{url}: HTTP {}", resp.status())),
                Err(e) => last_err = Some(e.into()),
            }
        }
    }
    Err(last_err.unwrap_or_else(|| anyhow::anyhow!("no description.xml candidate answered")))
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

struct Subscribed {
    event_sub_url: String,
    host_header: String,
    sid: String,
    timeout_secs: u32,
}

async fn subscribe(
    client: &reqwest::Client,
    event_sub_url: &str,
    host_header: &str,
    callback_url: &str,
) -> anyhow::Result<(String, u32)> {
    let method = reqwest::Method::from_bytes(b"SUBSCRIBE").expect("SUBSCRIBE is a valid method token");
    let resp = client
        .request(method, event_sub_url)
        .header("HOST", host_header)
        .header("CALLBACK", callback_url)
        .header("NT", "upnp:event")
        .header("TIMEOUT", format!("Second-{REQUESTED_TIMEOUT_SECS}"))
        .send()
        .await?;
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

fn parse_timeout_header(raw: Option<&str>) -> u32 {
    raw.and_then(|s| s.strip_prefix("Second-"))
        .and_then(|s| s.parse().ok())
        .unwrap_or(REQUESTED_TIMEOUT_SECS)
}

/// Renews an existing SID (`HOST` + `SID` headers only, no `CALLBACK`/`NT`).
async fn renew(client: &reqwest::Client, event_sub_url: &str, host_header: &str, sid: &str) -> anyhow::Result<u32> {
    let method = reqwest::Method::from_bytes(b"SUBSCRIBE").expect("SUBSCRIBE is a valid method token");
    let resp = client
        .request(method, event_sub_url)
        .header("HOST", host_header)
        .header("SID", sid)
        .send()
        .await?;
    if !resp.status().is_success() {
        anyhow::bail!("renew SUBSCRIBE {event_sub_url}: HTTP {}", resp.status());
    }
    Ok(parse_timeout_header(resp.headers().get("TIMEOUT").and_then(|v| v.to_str().ok())))
}

async fn unsubscribe(client: &reqwest::Client, event_sub_url: &str, host_header: &str, sid: &str) {
    let method = reqwest::Method::from_bytes(b"UNSUBSCRIBE").expect("UNSUBSCRIBE is a valid method token");
    match client
        .request(method, event_sub_url)
        .header("HOST", host_header)
        .header("SID", sid)
        .send()
        .await
    {
        Ok(resp) if resp.status().is_success() => {
            eprintln!("[wiim-gena-test] UNSUBSCRIBE {sid} ok");
        }
        Ok(resp) => eprintln!("[wiim-gena-test] UNSUBSCRIBE {sid}: HTTP {}", resp.status()),
        Err(e) => eprintln!("[wiim-gena-test] UNSUBSCRIBE {sid}: {e}"),
    }
}

/// Renewal loop for one held subscription — runs for the lifetime of the
/// process (cancelled by dropping its `JoinHandle` on shutdown, same as
/// every other spawned task here). On a renewal failure it tries one fresh
/// SUBSCRIBE (new SID) before giving up on this service for the rest of the
/// run; either way, the current SID (or its absence) is kept in `held` so
/// the shutdown path knows what, if anything, still needs UNSUBSCRIBE.
async fn renewal_loop(
    client: reqwest::Client,
    name: &'static str,
    event_sub_url: String,
    host_header: String,
    callback_url: String,
    sid: String,
    timeout_secs: u32,
    sid_names: Arc<Mutex<HashMap<String, &'static str>>>,
    held: Arc<Mutex<HashMap<&'static str, Subscribed>>>,
) {
    let mut sid = sid;
    let mut timeout_secs = timeout_secs;
    loop {
        let delay = (timeout_secs.saturating_sub(60)).max(30);
        tokio::time::sleep(Duration::from_secs(delay as u64)).await;
        match renew(&client, &event_sub_url, &host_header, &sid).await {
            Ok(new_timeout) => {
                timeout_secs = new_timeout;
                eprintln!("[wiim-gena-test] {name}: renewed (sid={sid}, timeout={timeout_secs}s)");
            }
            Err(e) => {
                eprintln!("[wiim-gena-test] {name}: renewal failed ({e}), trying a fresh SUBSCRIBE");
                sid_names.lock().unwrap().remove(&sid);
                match subscribe(&client, &event_sub_url, &host_header, &callback_url).await {
                    Ok((new_sid, new_timeout)) => {
                        eprintln!("[wiim-gena-test] {name}: re-subscribed (sid={new_sid}, timeout={new_timeout}s)");
                        sid = new_sid;
                        timeout_secs = new_timeout;
                    }
                    Err(e2) => {
                        eprintln!("[wiim-gena-test] {name}: re-subscribe also failed ({e2}) — giving up on this service");
                        held.lock().unwrap().remove(name);
                        return;
                    }
                }
            }
        }
        sid_names.lock().unwrap().insert(sid.clone(), name);
        if let Some(entry) = held.lock().unwrap().get_mut(name) {
            entry.sid = sid.clone();
            entry.timeout_secs = timeout_secs;
        }
    }
}

/// Extracts `LastChange`'s doubly-nested content for display: the outer
/// `<e:propertyset>` body is real XML, but `LastChange`'s own content is
/// itself XML-escaped once more (see `GENA.md`'s NOTIFY body shape) — one
/// unescape pass turns it back into readable tags for the printed log line.
fn unescape_xml_entities(s: &str) -> String {
    s.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&amp;", "&")
}

fn serve_notify(server: tiny_http::Server, sid_names: &Arc<Mutex<HashMap<String, &'static str>>>) {
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

            let name = sid
                .as_deref()
                .and_then(|s| sid_names.lock().unwrap().get(s).copied())
                .unwrap_or("<unknown SID>");
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0);
            println!("── NOTIFY [{now}] {name} sid={} seq={} ──", sid.as_deref().unwrap_or("?"), seq.as_deref().unwrap_or("?"));
            if let Some(last_change) = extract_tag(&body, "LastChange") {
                println!("{}", unescape_xml_entities(&last_change));
            } else {
                println!("(no LastChange property — raw body follows)\n{body}");
            }
        }));
        if result.is_err() {
            eprintln!("[wiim-gena-test] internal error handling a NOTIFY (see panic message above)");
        }
        let response = tiny_http::Response::from_string("").with_status_code(200);
        let _ = request.respond(response);
    }
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> anyhow::Result<()> {
    let ip = std::env::args().nth(1).unwrap_or_else(|| usage());
    if ip.starts_with('-') || ip.is_empty() {
        usage();
    }

    let (description_xml, description_url) = discover_description(&ip).await?;
    let device_host = host_port_of(&description_url)
        .split(':')
        .next()
        .unwrap_or(&ip)
        .to_string();

    let mut found: Vec<(&'static str, String)> = Vec::new();
    for wanted in WANTED_SERVICES {
        match extract_event_sub_url(&description_xml, &description_url, wanted.service_type_substr) {
            Some(url) => {
                eprintln!("[wiim-gena-test] {}: eventSubURL = {url}", wanted.name);
                found.push((wanted.name, url));
            }
            None => eprintln!("[wiim-gena-test] {}: not advertised by this device, skipping", wanted.name),
        }
    }
    if found.is_empty() {
        anyhow::bail!("device advertises none of AVTransport/RenderingControl/PlayQueue — nothing to subscribe to");
    }

    let local_ip = find_local_ip(&device_host)?;
    let sid_names: Arc<Mutex<HashMap<String, &'static str>>> = Arc::new(Mutex::new(HashMap::new()));
    let held: Arc<Mutex<HashMap<&'static str, Subscribed>>> = Arc::new(Mutex::new(HashMap::new()));

    let server = tiny_http::Server::http(format!("{local_ip}:0"))
        .map_err(|e| anyhow::anyhow!("binding NOTIFY listener on {local_ip}: {e}"))?;
    let notify_port = server
        .server_addr()
        .to_ip()
        .map(|a| a.port())
        .ok_or_else(|| anyhow::anyhow!("NOTIFY listener has no IP address"))?;
    eprintln!("[wiim-gena-test] NOTIFY listener on http://{local_ip}:{notify_port}/notify");
    {
        let sid_names = Arc::clone(&sid_names);
        std::thread::Builder::new()
            .name("gena-notify".into())
            .spawn(move || serve_notify(server, &sid_names))
            .expect("spawning NOTIFY listener thread");
    }
    let callback_url = format!("<http://{local_ip}:{notify_port}/notify>");

    let client = build_reqwest_client(tls_for_scheme(description_url.split(':').next().unwrap_or("http")), REQUEST_TIMEOUT);
    let mut tasks = Vec::new();
    for (name, event_sub_url) in found {
        let host_header = host_port_of(&event_sub_url);
        match subscribe(&client, &event_sub_url, &host_header, &callback_url).await {
            Ok((sid, timeout_secs)) => {
                eprintln!("[wiim-gena-test] {name}: subscribed (sid={sid}, timeout={timeout_secs}s)");
                sid_names.lock().unwrap().insert(sid.clone(), name);
                held.lock().unwrap().insert(name, Subscribed {
                    event_sub_url: event_sub_url.clone(), host_header: host_header.clone(),
                    sid: sid.clone(), timeout_secs,
                });
                let task = tokio::spawn(renewal_loop(
                    client.clone(), name, event_sub_url, host_header, callback_url.clone(),
                    sid, timeout_secs, Arc::clone(&sid_names), Arc::clone(&held),
                ));
                tasks.push(task);
            }
            Err(e) => eprintln!("[wiim-gena-test] {name}: SUBSCRIBE failed: {e}"),
        }
    }
    if held.lock().unwrap().is_empty() {
        anyhow::bail!("every SUBSCRIBE attempt failed — nothing held, exiting");
    }

    println!("Subscribed — waiting for NOTIFYs, Ctrl-C to unsubscribe and quit.");
    let _ = tokio::signal::ctrl_c().await;
    println!("\n[wiim-gena-test] shutting down, unsubscribing...");
    for task in tasks {
        task.abort();
    }
    let held = std::mem::take(&mut *held.lock().unwrap());
    for (_, sub) in held {
        unsubscribe(&client, &sub.event_sub_url, &sub.host_header, &sub.sid).await;
    }
    Ok(())
}
