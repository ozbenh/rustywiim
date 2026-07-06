//! `wiim-capture` — probes a real WiiM/LinkPlay device in the field and
//! records its responses to a curated command list, plus a basic UPnP
//! capture, into one JSON file for later use by `wiim-simulator`.
//!
//! Usage: `wiim-capture [--destructive] <ip>`
//!
//! By default no `Set` command is ever sent, however reversible-looking a
//! `commands.yaml` entry's `safe: true` marks it — only `--destructive`
//! unlocks those. A real run without this gate was once observed to leave
//! a device's touch controls disabled, its LED off, its volume changed,
//! and its EQ preset overwritten.
//!
//! `wiim-capture --one <target> <ip>` — a lightweight escape hatch for
//! quick protocol probing/testing, distinct from the curated
//! `commands.yaml`-driven capture above: sends exactly the one given
//! command/action verbatim and prints only the raw response body to
//! stdout — no JSON wrapping, no capture file written, no
//! `commands.yaml`/safety-gating involved at all (it's whatever the caller
//! directly typed, not something this tool inferred was safe). `<target>`
//! is either:
//! - a plain `httpapi.asp` command string, e.g. `getPlayerStatusEx` (sent
//!   via the same probed/detected scheme+port as the normal flow), or
//! - `upnp:<Action>` / `upnp:<Service>:<Action>` for a UPnP SOAP action,
//!   e.g. `upnp:GetInfoEx` or `upnp:RenderingControl:GetVolume` — the
//!   service is auto-detected for the handful of actions this tool already
//!   knows about (see `known_upnp_action_service`), otherwise spell it out.
//! Pairs with `wiim-capdump -`, which reads that raw body from stdin and
//! pretty-prints it the same way it would a capture file's command body.

use base64::Engine;
use rustywiim::capture::commands::{self, ExpandedCommand};
use rustywiim::capture::format::{
    Blob, CaptureFile, CommandCapture, Method, Outcome, ResponseFormat, UpnpActionCapture, UpnpCapture,
};
use rustywiim::device::api::{build_reqwest_client, DeviceInfo, TlsMode};
use rustywiim::device::capabilities::DeviceCapabilities;
use std::time::Duration;

const MAX_RETRIES: u32 = 3;
const INTER_COMMAND_DELAY: Duration = Duration::from_millis(100);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(5);

/// HTTPS-first, matching pywiim's `wiim-diagnostics` probe order.
const PROBE_COMBOS: &[(&str, u16)] = &[
    ("https", 443),
    ("https", 4443),
    ("https", 8443),
    ("http", 80),
    ("http", 8080),
];

// ── Low-level request/retry ─────────────────────────────────────────────────

/// Result of sending one fully-substituted command (after any retries).
struct Attempt {
    outcome: Outcome,
    http_status: Option<u16>,
    error: Option<String>,
    /// Raw response text; only `Some` when `outcome == Ok`.
    body: Option<String>,
    attempts: u32,
}

/// Send a request built by `build` (re-invoked fresh on every attempt, since
/// a `reqwest::RequestBuilder` can't just be resent), retrying up to
/// `MAX_RETRIES` times, but only on a connection failure — an HTTP error
/// status or any other protocol-level error is recorded as-is with no
/// retry.
async fn send_request<F>(build: F) -> Attempt
where
    F: Fn() -> reqwest::RequestBuilder,
{
    let mut attempts = 0u32;
    loop {
        attempts += 1;
        match build().send().await {
            Ok(resp) => {
                let status = resp.status();
                if status.is_success() {
                    let body = resp.text().await.unwrap_or_default();
                    return Attempt {
                        outcome: Outcome::Ok,
                        http_status: Some(status.as_u16()),
                        error: None,
                        body: Some(body),
                        attempts,
                    };
                }
                return Attempt {
                    outcome: Outcome::HttpError,
                    http_status: Some(status.as_u16()),
                    error: None,
                    body: None,
                    attempts,
                };
            }
            Err(e) => {
                let is_connection_failure = e.is_connect() || e.is_timeout();
                if is_connection_failure && attempts <= MAX_RETRIES {
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    continue;
                }
                return Attempt {
                    outcome: if is_connection_failure {
                        Outcome::ConnectionError
                    } else {
                        Outcome::ProtocolError
                    },
                    http_status: None,
                    error: Some(e.to_string()),
                    body: None,
                    attempts,
                };
            }
        }
    }
}

// ── Blob encoding / anonymization / decoding ────────────────────────────────

/// True if `s` looks like an XML document/fragment — checked before the
/// plain-text tier below, since real XML (e.g. UPnP's description.xml/SOAP
/// responses) commonly contains `"` inside attributes, which would otherwise
/// push it straight to the base64 tier and make it opaque.
fn looks_like_xml(s: &str) -> bool {
    let t = s.trim();
    t.starts_with("<?xml") || (t.starts_with('<') && t.ends_with('>'))
}

/// Blob encoding rule: JSON if it parses, else XML if it looks like XML,
/// else plain text if printable-ASCII-no-quotes, else base64.
fn encode_blob(raw: &str) -> (ResponseFormat, serde_json::Value) {
    if let Ok(v) = serde_json::from_str::<serde_json::Value>(raw) {
        return (ResponseFormat::Json, v);
    }
    if looks_like_xml(raw) {
        return (ResponseFormat::Xml, serde_json::Value::String(raw.to_string()));
    }
    let is_plain_text = raw
        .bytes()
        .all(|b| (0x20..=0x7E).contains(&b) || matches!(b, b'\n' | b'\r' | b'\t'))
        && !raw.contains('"');
    if is_plain_text {
        (ResponseFormat::Text, serde_json::Value::String(raw.to_string()))
    } else {
        let encoded = base64::engine::general_purpose::STANDARD.encode(raw.as_bytes());
        (ResponseFormat::Base64, serde_json::Value::String(encoded))
    }
}

/// Key-name substrings (case-insensitive) that trigger anonymization of a
/// JSON string value, regardless of the value's own shape. Broadened over
/// time beyond the original mac/uuid/ssid scope.
/// `eth0`/`apcli0`/`eth2`/`ra0` are LinkPlay's actual LAN-interface
/// field names (confirmed against a real WiiM Ultra capture) — plain `"ip"`
/// as a substring was dropped: it never matched any of those real field
/// names in the first place (none of them contain the literal substring
/// "ip"), while still false-matching unrelated fields elsewhere (e.g.
/// `project_build_name` isn't personal data, it just contains "name").
/// `looks_like_ipv4()` below is the actual fix for the LAN-IP fields — value
/// *shape*, not key name, so it also catches an IP address sitting in a
/// field this list doesn't happen to name. This is the JSON-key list only —
/// `XML_ANON_KEY_SUBSTRINGS` below (used by `anonymize_xml`) is a separate,
/// stricter list; see its doc comment for why they've diverged (`"udn"` only
/// matters there — no JSON field is named that — and `"ad"` had to be
/// dropped from the XML side only).
const ANON_KEY_SUBSTRINGS: &[&str] =
    &["mac", "uuid", "ssid", "bssid", "ad", "name", "eth0", "eth2", "apcli0", "ra0"];

fn anonymize_string(s: &str) -> String {
    s.chars().map(|c| if c.is_alphanumeric() { 'x' } else { c }).collect()
}

/// Replaces every literal occurrence of the real target IP inside a request
/// URL with its anonymized (shape-preserving) form, for the `url` field
/// written into the capture file. The real, unmodified `url` is still what's
/// actually used to perform the request — this only affects what gets
/// recorded afterward, same as `anonymize_json` does for response bodies.
fn anonymize_ip_in_url(url: &str, ip: &str) -> String {
    url.replace(ip, &anonymize_string(ip))
}

/// True if `s` is shaped like an IPv4 address (four dot-separated 0-255
/// octets) — checked on every string value regardless of its key, so a LAN
/// IP gets scrubbed even from a field this module doesn't know the name of
/// (confirmed necessary: a real WiiM Ultra capture had its LAN IP sitting
/// unanonymized in `apcli0`/`ra0`, neither of which contains "ip").
fn looks_like_ipv4(s: &str) -> bool {
    let parts: Vec<&str> = s.split('.').collect();
    parts.len() == 4
        && parts.iter().all(|p| {
            !p.is_empty()
                && p.len() <= 3
                && p.chars().all(|c| c.is_ascii_digit())
                && p.parse::<u16>().is_ok_and(|n| n <= 255)
        })
}

/// Anonymizes matching string values in a (shallow, LinkPlay responses are
/// flat) top-level JSON object, in place. A value is anonymized if its *key*
/// matches `ANON_KEY_SUBSTRINGS`, or if the *value itself* is IP-shaped —
/// see `looks_like_ipv4()`.
fn anonymize_json(v: &mut serde_json::Value) {
    if let serde_json::Value::Object(map) = v {
        for (k, val) in map.iter_mut() {
            let kl = k.to_lowercase();
            let key_matches = ANON_KEY_SUBSTRINGS.iter().any(|needle| kl.contains(needle));
            if let serde_json::Value::String(s) = val {
                if key_matches || looks_like_ipv4(s) {
                    *s = anonymize_string(s);
                }
            }
        }
    }
}

/// Tag-name substrings considered identifying for XML anonymization — a
/// *stricter* subset of `ANON_KEY_SUBSTRINGS` than the JSON-key matcher uses.
/// `"ad"` is deliberately excluded here: it was added to the JSON list for
/// Bluetooth's flat `ad` field (an exact key name in a flat object, low
/// collision risk), but as a 2-letter substring it collides with ordinary
/// HTML/XML tag names — confirmed against a real device: `getsyslog`'s HTML
/// response has a `<head>` tag, and "head" contains "ad", which made
/// `anonymize_xml` treat the *entire* `<head>...</head>` span as matched
/// "content" and character-scrub it (`anonymize_string` doesn't know about
/// markup, so this corrupted the real `<meta charset=...>` tag inside into
/// garbage). No other list entry has shown this problem in practice.
const XML_ANON_KEY_SUBSTRINGS: &[&str] =
    &["mac", "uuid", "ssid", "bssid", "name", "eth0", "eth2", "apcli0", "ra0", "udn"];

/// Anonymizes the text content of XML leaf elements whose *tag name* matches
/// `XML_ANON_KEY_SUBSTRINGS` — e.g. `<UDN>uuid:...</UDN>` in UPnP
/// description.xml. Non-namespace-aware, assumes the matched tag has no
/// nested elements (true for every LinkPlay/UPnP tag this matches in
/// practice) — sufficient for the known shape, not a general XML anonymizer,
/// same spirit as `extract_tag`.
fn anonymize_xml(xml: &str) -> String {
    let mut out = String::with_capacity(xml.len());
    let mut i = 0;
    while i < xml.len() {
        let Some(tag_start) = xml[i..].find('<').map(|p| i + p) else {
            out.push_str(&xml[i..]);
            break;
        };
        out.push_str(&xml[i..tag_start]);

        let Some(tag_end) = xml[tag_start..].find('>').map(|p| tag_start + p + 1) else {
            out.push_str(&xml[tag_start..]);
            break;
        };
        let tag = &xml[tag_start..tag_end];
        out.push_str(tag);
        i = tag_end;

        let is_opening = !tag.starts_with("</") && !tag.ends_with("/>") && !tag.starts_with("<?") && !tag.starts_with("<!");
        if !is_opening {
            continue;
        }
        let name = tag[1..tag.len() - 1].split_whitespace().next().unwrap_or("").to_lowercase();
        // friendlyName/modelName would otherwise match ANON_KEY_SUBSTRINGS's
        // "name" substring, but per explicit request these two are not
        // scrubbed — a room name and a marketing model string are useful to
        // see in a capture and not treated as sensitive here (unlike UDN,
        // MAC, SSID, etc., which "name" is still broad enough to catch
        // elsewhere, e.g. Bluetooth device names).
        if matches!(name.as_str(), "friendlyname" | "modelname") {
            continue;
        }
        if !XML_ANON_KEY_SUBSTRINGS.iter().any(|needle| name.contains(needle)) {
            continue;
        }
        let Some(close_start) = xml[i..].find("</").map(|p| i + p) else {
            continue;
        };
        out.push_str(&anonymize_string(&xml[i..close_start]));
        i = close_start;
    }
    out
}

/// LinkPlay's "not supported" signal: a 200 OK whose body is literally one
/// of these strings (case-insensitive) rather than a real payload.
fn is_unsupported_text(raw: &str) -> bool {
    matches!(raw.trim().to_lowercase().as_str(), "unknown command" | "failed" | "unknown")
}

fn hex_decode_utf8(s: &str) -> Option<String> {
    if s.is_empty() || s.len() % 2 != 0 || !s.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let chars: Vec<char> = s.chars().collect();
    let mut bytes = Vec::with_capacity(chars.len() / 2);
    for pair in chars.chunks(2) {
        let byte_str: String = pair.iter().collect();
        bytes.push(u8::from_str_radix(&byte_str, 16).ok()?);
    }
    while bytes.last() == Some(&0) {
        bytes.pop();
    }
    String::from_utf8(bytes).ok()
}

fn html_unescape(s: &str) -> String {
    s.replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&#39;", "'")
}

/// Decoded companion for `getPlayerStatus`/`getPlayerStatusEx`'s hex+HTML-
/// entity-encoded Title/Artist/Album fields. Checked: no existing decoder
/// for this in `src/device/api.rs`/`state.rs` — this app gets track
/// metadata from `getMetaInfo` instead, which is already plain UTF-8, so it
/// never needed one.
fn decode_player_status_fields(command: &str, body: &serde_json::Value) -> Option<serde_json::Value> {
    if !matches!(command, "getPlayerStatus" | "getPlayerStatusEx") {
        return None;
    }
    let obj = body.as_object()?;
    let mut decoded = serde_json::Map::new();
    for field in ["Title", "Artist", "Album"] {
        if let Some(serde_json::Value::String(raw)) = obj.get(field) {
            if let Some(hex_decoded) = hex_decode_utf8(raw) {
                decoded.insert(field.to_string(), serde_json::Value::String(html_unescape(&hex_decoded)));
            }
        }
    }
    if decoded.is_empty() {
        None
    } else {
        Some(serde_json::Value::Object(decoded))
    }
}

fn build_command_capture(command: &str, url: &str, ip: &str, attempt: Attempt, meta: Option<&ExpandedCommand>) -> CommandCapture {
    let mut format = None;
    let mut body = None;
    let mut unsupported = false;
    let mut decoded = None;

    if attempt.outcome == Outcome::Ok {
        if let Some(raw) = &attempt.body {
            unsupported = is_unsupported_text(raw);
            let (fmt, mut val) = encode_blob(raw);
            if fmt == ResponseFormat::Json {
                anonymize_json(&mut val);
                decoded = decode_player_status_fields(command, &val);
            } else if fmt == ResponseFormat::Xml {
                if let serde_json::Value::String(s) = &val {
                    val = serde_json::Value::String(anonymize_xml(s));
                }
            }
            format = Some(fmt);
            body = Some(val);
        }
    }

    CommandCapture {
        command: command.to_string(),
        url: anonymize_ip_in_url(url, ip),
        attempts: attempt.attempts,
        outcome: attempt.outcome,
        http_status: attempt.http_status,
        error: attempt.error,
        format,
        body,
        unsupported,
        decoded,
        summary: meta.and_then(|m| m.summary.clone()),
        tag: meta.and_then(|m| m.tag.clone()),
        operation_id: meta.and_then(|m| m.operation_id.clone()),
    }
}

// ── Port/TLS probing + model detection ──────────────────────────────────────

struct Winner {
    scheme: &'static str,
    port: u16,
    client: reqwest::Client,
    cmd_used: String,
    body: String,
}

fn tls_for_scheme(scheme: &str) -> TlsMode {
    if scheme == "https" {
        TlsMode::HttpsWiiM
    } else {
        TlsMode::Http
    }
}

/// LinkPlay endpoint sanity check from pywiim's `wiim-diagnostics`: a 2xx
/// response is only trusted as "the real endpoint" if the body is literally
/// "OK" or looks like a JSON object — cheap insurance against an unrelated
/// 200-OK page (a router admin UI on 8080, say).
fn looks_like_linkplay_response(body: &str) -> bool {
    let t = body.trim();
    t == "OK" || t.starts_with('{')
}

/// Probes getStatusEx, then getStatus, across all 5 scheme/port combos each,
/// stopping at the first response that looks like a real LinkPlay endpoint.
/// Every individual probe attempt is recorded (not just the winner), for
/// maximum diagnostic value when nothing responds at all.
async fn probe_and_detect(ip: &str) -> (Vec<CommandCapture>, Option<Winner>) {
    let mut records = Vec::new();

    for cmd in ["getStatusEx", "getStatus"] {
        for &(scheme, port) in PROBE_COMBOS {
            let client = build_reqwest_client(tls_for_scheme(scheme), REQUEST_TIMEOUT);
            let url = format!("{scheme}://{ip}:{port}/httpapi.asp?command={cmd}");
            eprintln!("[wiim-capture] probing {url}");
            let attempt = send_request(|| client.get(&url)).await;

            let is_winner = attempt.outcome == Outcome::Ok
                && attempt.body.as_deref().map(looks_like_linkplay_response).unwrap_or(false);
            let winner_body = if is_winner { attempt.body.clone() } else { None };

            records.push(build_command_capture(cmd, &url, ip, attempt, None));
            tokio::time::sleep(INTER_COMMAND_DELAY).await;

            if let Some(body) = winner_body {
                return (
                    records,
                    Some(Winner { scheme, port, client, cmd_used: cmd.to_string(), body }),
                );
            }
        }
    }

    (records, None)
}

// ── getsyslog special-case (two-step retrieval) ─────────────────────────────

/// Longer timeout for the whole getsyslog flow — confirmed by hand (real
/// device, real network) to work but take noticeably longer than
/// `REQUEST_TIMEOUT` covers, for both the initial call and its follow-up
/// download-link fetch.
const SYSLOG_TIMEOUT: Duration = Duration::from_secs(90);

/// Result of a raw-bytes fetch. `getsyslog`'s download link isn't text — it's
/// RC4-encrypted, possibly gzip/tar, binary — so this can't reuse `Attempt`'s
/// `.text()`-based body.
struct ByteAttempt {
    outcome: Outcome,
    http_status: Option<u16>,
    error: Option<String>,
    body: Option<Vec<u8>>,
    attempts: u32,
}

/// Byte-returning sibling of `send_request`, same retry semantics (retry
/// only on connection failure, up to `MAX_RETRIES`).
async fn fetch_bytes(client: &reqwest::Client, url: &str) -> ByteAttempt {
    let mut attempts = 0u32;
    loop {
        attempts += 1;
        match client.get(url).send().await {
            Ok(resp) => {
                let status = resp.status();
                if status.is_success() {
                    let body = resp.bytes().await.map(|b| b.to_vec()).unwrap_or_default();
                    return ByteAttempt {
                        outcome: Outcome::Ok,
                        http_status: Some(status.as_u16()),
                        error: None,
                        body: Some(body),
                        attempts,
                    };
                }
                return ByteAttempt {
                    outcome: Outcome::HttpError,
                    http_status: Some(status.as_u16()),
                    error: None,
                    body: None,
                    attempts,
                };
            }
            Err(e) => {
                let is_connection_failure = e.is_connect() || e.is_timeout();
                if is_connection_failure && attempts <= MAX_RETRIES {
                    tokio::time::sleep(INTER_COMMAND_DELAY).await;
                    continue;
                }
                return ByteAttempt {
                    outcome: if is_connection_failure { Outcome::ConnectionError } else { Outcome::ProtocolError },
                    http_status: None,
                    error: Some(e.to_string()),
                    body: None,
                    attempts,
                };
            }
        }
    }
}

/// Extracts the `href` attribute of the first `<a>` tag in a small HTML
/// fragment. LinkPlay's `getsyslog` response is an HTML page whose only
/// useful content is a download link, not the log itself — see
/// linkplay-cli's `cli.py::getsyslog`, which extracts the same link with
/// BeautifulSoup; this is the minimal hand-rolled equivalent for this one
/// known shape.
fn extract_href(html: &str) -> Option<String> {
    let idx = html.find("href=")?;
    let after = &html[idx + "href=".len()..];
    let quote = after.chars().next()?;
    if quote == '"' || quote == '\'' {
        let rest = &after[quote.len_utf8()..];
        let end = rest.find(quote)?;
        return Some(rest[..end].to_string());
    }
    // Unquoted HTML attribute value — valid HTML5, and what a real device
    // was confirmed to send (`<a href=data/sys.log>download</a>`, no
    // quotes at all). Ends at the first whitespace or `>`.
    let end = after.find(|c: char| c.is_whitespace() || c == '>').unwrap_or(after.len());
    Some(after[..end].to_string())
}

/// Joins a (possibly relative) path onto a `scheme://ip:port` origin.
fn join_url(scheme: &str, ip: &str, port: u16, path: &str) -> String {
    let origin = format!("{scheme}://{ip}:{port}");
    if path.starts_with('/') {
        format!("{origin}{path}")
    } else {
        format!("{origin}/{path}")
    }
}

/// Builds the `CommandCapture` for the raw-bytes download-link fetch: always
/// `format: base64` (the payload is RC4-encrypted, so it's never valid
/// JSON/text/XML) and left completely undecoded — `wiim-capdump` does the
/// RC4-decrypt/gunzip/untar, keeping this tool a pure recorder.
fn build_bytes_command_capture(command: &str, url: &str, ip: &str, attempt: ByteAttempt) -> CommandCapture {
    let (format, body) = match &attempt.body {
        Some(bytes) => (
            Some(ResponseFormat::Base64),
            Some(serde_json::Value::String(base64::engine::general_purpose::STANDARD.encode(bytes))),
        ),
        None => (None, None),
    };
    CommandCapture {
        command: command.to_string(),
        url: anonymize_ip_in_url(url, ip),
        attempts: attempt.attempts,
        outcome: attempt.outcome,
        http_status: attempt.http_status,
        error: attempt.error,
        format,
        body,
        unsupported: false,
        decoded: None,
        summary: Some("Raw encrypted (+ possibly gzip/tar) syslog bytes — decode with wiim-capdump".to_string()),
        tag: None,
        operation_id: None,
    }
}

/// `getsyslog` doesn't return the log itself — it returns a small HTML page
/// containing a download link (see linkplay-cli's `cli.py::getsyslog`, which
/// this mirrors). Captures both the initial HTML response and the raw bytes
/// behind that link, completely undecoded; the RC4-decrypt/gunzip/untar
/// happens in `wiim-capdump`, not here, so this tool stays a pure recorder.
async fn capture_syslog(scheme: &str, ip: &str, port: u16, meta: Option<&ExpandedCommand>) -> Vec<CommandCapture> {
    let mut records = Vec::new();
    let client = build_reqwest_client(tls_for_scheme(scheme), SYSLOG_TIMEOUT);

    let url = format!("{scheme}://{ip}:{port}/httpapi.asp?command=getsyslog");
    eprintln!("[wiim-capture] getsyslog (may be slow)");
    let attempt = send_request(|| client.get(&url)).await;
    let html = attempt.body.clone();
    records.push(build_command_capture("getsyslog", &url, ip, attempt, meta));

    let Some(html) = html else { return records };
    let Some(href) = extract_href(&html) else {
        eprintln!("[wiim-capture] getsyslog: no <a href> found in the response, can't fetch the actual log");
        return records;
    };

    let download_url = join_url(scheme, ip, port, &href);
    eprintln!("[wiim-capture] getsyslog: fetching download link (may be slow)");
    let byte_attempt = fetch_bytes(&client, &download_url).await;
    records.push(build_bytes_command_capture("getsyslog:download", &download_url, ip, byte_attempt));
    records
}

// ── UPnP capture ─────────────────────────────────────────────────────────────

async fn ssdp_probe(ip: &str) -> (Option<String>, Option<String>) {
    let sock = match tokio::net::UdpSocket::bind("0.0.0.0:0").await {
        Ok(s) => s,
        Err(e) => return (None, Some(e.to_string())),
    };
    let target = format!("{ip}:1900");
    let msg = "M-SEARCH * HTTP/1.1\r\n\
        HOST: 239.255.255.250:1900\r\n\
        MAN: \"ssdp:discover\"\r\n\
        MX: 3\r\n\
        ST: urn:schemas-upnp-org:device:MediaRenderer:1\r\n\
        \r\n";
    if let Err(e) = sock.send_to(msg.as_bytes(), &target).await {
        return (None, Some(e.to_string()));
    }
    let mut buf = [0u8; 8192];
    match tokio::time::timeout(Duration::from_secs(3), sock.recv_from(&mut buf)).await {
        Ok(Ok((n, _))) => (Some(String::from_utf8_lossy(&buf[..n]).to_string()), None),
        Ok(Err(e)) => (None, Some(e.to_string())),
        Err(_) => (None, Some("timed out waiting for an SSDP response".to_string())),
    }
}

fn extract_header(text: &str, name: &str) -> Option<String> {
    text.lines().find_map(|line| {
        let (key, value) = line.split_once(':')?;
        key.trim().eq_ignore_ascii_case(name).then(|| value.trim().to_string())
    })
}

/// Minimal, non-namespace-aware `<tag>...</tag>` extractor — sufficient for
/// the well-known tags LinkPlay's device-description XML actually uses; a
/// full XML parser would be overkill for this basic, read-only UPnP capture.
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

/// Resolves a (possibly relative) `controlURL` against the origin
/// (`scheme://host:port`) of the description.xml URL it came from.
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

async fn fetch_description(ip: &str, location: Option<&str>) -> Option<(String, Attempt)> {
    let mut candidates: Vec<String> = Vec::new();
    if let Some(loc) = location {
        candidates.push(loc.to_string());
    }
    for scheme in ["http", "https"] {
        for port in [49152u16, 59152] {
            candidates.push(format!("{scheme}://{ip}:{port}/description.xml"));
        }
    }
    for url in candidates {
        let client = build_reqwest_client(tls_for_scheme(url.split(':').next().unwrap_or("http")), REQUEST_TIMEOUT);
        eprintln!("[wiim-capture] upnp: trying description.xml at {url}");
        let attempt = send_request(|| client.get(&url)).await;
        if attempt.outcome == Outcome::Ok {
            return Some((url, attempt));
        }
    }
    None
}

/// Sends one SOAP action, returning the raw (unanonymized) `Attempt` —
/// shared by `soap_call()` (full capture: wraps + anonymizes the result for
/// the JSON file) and `send_one_upnp()` (the `--one upnp:...` escape hatch,
/// which wants the bare response body, nothing wrapped or scrubbed).
async fn soap_call_raw(control_url: &str, service_type: &str, action: &str, args_xml: &str) -> Attempt {
    let client = build_reqwest_client(
        tls_for_scheme(control_url.split(':').next().unwrap_or("http")),
        REQUEST_TIMEOUT,
    );
    let body = format!(
        "<?xml version=\"1.0\" encoding=\"utf-8\"?>\r\n\
         <s:Envelope xmlns:s=\"http://schemas.xmlsoap.org/soap/envelope/\" \
         s:encodingStyle=\"http://schemas.xmlsoap.org/soap/encoding/\">\
         <s:Body><u:{action} xmlns:u=\"{service_type}\">{args_xml}</u:{action}></s:Body></s:Envelope>"
    );
    let soap_action_header = format!("\"{service_type}#{action}\"");
    send_request(|| {
        client
            .post(control_url)
            .header("Content-Type", "text/xml; charset=\"utf-8\"")
            .header("SOAPACTION", soap_action_header.clone())
            .body(body.clone())
    })
    .await
}

async fn soap_call(control_url: &str, service_type: &str, action: &str, args_xml: &str, ip: &str) -> UpnpActionCapture {
    let attempt = soap_call_raw(control_url, service_type, action, args_xml).await;

    let response = attempt.body.as_ref().map(|raw| {
        let (format, mut body) = encode_blob(raw);
        if format == ResponseFormat::Xml {
            if let serde_json::Value::String(s) = &body {
                body = serde_json::Value::String(anonymize_xml(s));
            }
        }
        Blob { format, body }
    });

    UpnpActionCapture {
        service: service_type.to_string(),
        action: action.to_string(),
        control_url: anonymize_ip_in_url(control_url, ip),
        outcome: attempt.outcome,
        http_status: attempt.http_status,
        error: attempt.error,
        response,
    }
}

// ── `--one upnp:...` support ─────────────────────────────────────────────────

const AVT_SERVICE_TYPE: &str = "urn:schemas-upnp-org:service:AVTransport:1";
const RC_SERVICE_TYPE: &str = "urn:schemas-upnp-org:service:RenderingControl:1";

/// Known action -> owning short service name, so `--one upnp:<Action>`
/// doesn't require spelling out the service too. Mirrors the action lists
/// `capture_upnp()` already probes.
fn known_upnp_action_service(action: &str) -> Option<&'static str> {
    match action {
        "GetTransportInfo" | "GetPositionInfo" | "GetMediaInfo" | "GetInfoEx" => Some("AVTransport"),
        "GetVolume" | "GetMute" => Some("RenderingControl"),
        _ => None,
    }
}

fn upnp_service_type(short: &str) -> Option<&'static str> {
    match short {
        "AVTransport" => Some(AVT_SERVICE_TYPE),
        "RenderingControl" => Some(RC_SERVICE_TYPE),
        _ => None,
    }
}

/// Every action this tool sends only ever needs `InstanceID`, plus
/// `Channel` for `RenderingControl` — matches `capture_upnp()`'s existing
/// argument choices for the actions it already knows about. An action
/// requiring something richer (a real `Seek`/`SetVolume` argument, say)
/// isn't supported by this read-only escape hatch.
fn default_upnp_args(short_service: &str) -> &'static str {
    if short_service == "RenderingControl" {
        "<InstanceID>0</InstanceID><Channel>Master</Channel>"
    } else {
        "<InstanceID>0</InstanceID>"
    }
}

/// Discovers the control URL for `short_service` ("AVTransport" or
/// "RenderingControl") via SSDP + `description.xml`'s service list — the
/// same discovery `capture_upnp()` does inline, factored out here for reuse
/// by `--one upnp:...` (which only wants one service's control URL, not a
/// full UPnP capture).
async fn discover_control_url(ip: &str, short_service: &str) -> Option<String> {
    let (ssdp_text, _) = ssdp_probe(ip).await;
    let location = ssdp_text.as_deref().and_then(|t| extract_header(t, "LOCATION"));
    let (description_url, attempt) = fetch_description(ip, location.as_deref()).await?;
    let raw = attempt.body?;
    for block in extract_service_blocks(&raw) {
        let Some(service_type) = extract_tag(&block, "serviceType") else { continue };
        if service_type.contains(&format!(":service:{short_service}:")) {
            let control_url_raw = extract_tag(&block, "controlURL")?;
            return Some(resolve_url(&description_url, &control_url_raw));
        }
    }
    None
}

/// Runs `--one upnp:...`: resolves the service (explicit, or looked up for
/// a known action), discovers its control URL, sends the action, and prints
/// the raw response body to stdout. Exits the process on any failure.
async fn send_one_upnp(ip: &str, service: Option<String>, action: String) -> ! {
    let short_service = match service {
        Some(s) => s,
        None => match known_upnp_action_service(&action) {
            Some(s) => s.to_string(),
            None => {
                eprintln!(
                    "[wiim-capture] don't know which UPnP service '{action}' belongs to; \
                     specify it explicitly, e.g. --one upnp:AVTransport:{action}"
                );
                std::process::exit(2);
            }
        },
    };
    let Some(service_type) = upnp_service_type(&short_service) else {
        eprintln!("[wiim-capture] unknown UPnP service '{short_service}' (expected AVTransport or RenderingControl)");
        std::process::exit(2);
    };
    eprintln!("[wiim-capture] discovering {short_service} control URL on {ip}...");
    let Some(control_url) = discover_control_url(ip, &short_service).await else {
        eprintln!("[wiim-capture] couldn't discover a {short_service} control URL for {ip}");
        std::process::exit(1);
    };
    eprintln!("[wiim-capture] {short_service}.{action} on {control_url}");
    let args_xml = default_upnp_args(&short_service);
    let attempt = soap_call_raw(&control_url, service_type, &action, args_xml).await;
    match attempt.body {
        Some(b) if attempt.outcome == Outcome::Ok => {
            println!("{b}");
            std::process::exit(0);
        }
        _ => {
            eprintln!(
                "[wiim-capture] {short_service}.{action} failed: {}",
                attempt.error.as_deref().unwrap_or("no response body"),
            );
            std::process::exit(1);
        }
    }
}

async fn capture_upnp(ip: &str) -> UpnpCapture {
    let mut upnp = UpnpCapture::default();

    let (ssdp_text, ssdp_err) = ssdp_probe(ip).await;
    upnp.ssdp_response = ssdp_text.clone();
    upnp.ssdp_error = ssdp_err;
    let location = ssdp_text.as_deref().and_then(|t| extract_header(t, "LOCATION"));
    upnp.location = location.as_deref().map(|l| anonymize_ip_in_url(l, ip));

    let Some((description_url, attempt)) = fetch_description(ip, location.as_deref()).await else {
        return upnp;
    };
    upnp.description_url = Some(anonymize_ip_in_url(&description_url, ip));
    let Some(raw) = attempt.body else { return upnp };

    let (format, mut body) = encode_blob(&raw);
    // Anonymize unconditionally, not gated on `format == Xml` — `encode_blob`'s
    // `looks_like_xml()` is a coarse heuristic (trimmed start/end characters)
    // that can misclassify a real device's response (e.g. a leading BOM byte
    // — not stripped by `.trim()` — would push it to the text/base64 tier
    // instead), which previously meant `anonymized_raw` silently fell back to
    // the *unanonymized* `raw` whenever that happened, leaking the real
    // friendlyName/UDN into the capture file's UPnP header even though the
    // `description` blob itself got scrubbed. `anonymize_xml` only ever acts
    // on `<tag>...</tag>` shapes it actually finds, so running it
    // unconditionally is harmless even on a body that isn't XML at all.
    let anonymized_raw = anonymize_xml(&raw);
    if format == ResponseFormat::Xml {
        body = serde_json::Value::String(anonymized_raw.clone());
    }
    upnp.description = Some(Blob { format, body });
    upnp.friendly_name = extract_tag(&anonymized_raw, "friendlyName");
    upnp.model_name = extract_tag(&anonymized_raw, "modelName");
    upnp.udn = extract_tag(&anonymized_raw, "UDN");

    // Service list/controlURL resolution still needs the real, unanonymized
    // structural content (serviceType/controlURL aren't scrubbed and must
    // stay intact for `resolve_url()` to work).
    let services = extract_service_blocks(&raw);
    for block in &services {
        if let Some(st) = extract_tag(block, "serviceType") {
            if st.contains("wiimu-com:service:PlayQueue") {
                upnp.has_playqueue = true;
            }
            if st.contains("tencent-com:service:QPlay") {
                upnp.has_qplay = true;
            }
            if st.contains("upnp-org:service:ContentDirectory") {
                upnp.has_content_directory = true;
            }
            upnp.service_types.push(st);
        }
    }

    for block in &services {
        let (Some(service_type), Some(control_url_raw)) =
            (extract_tag(block, "serviceType"), extract_tag(block, "controlURL"))
        else {
            continue;
        };
        let control_url = resolve_url(&description_url, &control_url_raw);
        if service_type.contains(":service:AVTransport:") {
            for action in ["GetTransportInfo", "GetPositionInfo", "GetMediaInfo", "GetInfoEx"] {
                eprintln!("[wiim-capture] upnp: {action} on {service_type}");
                upnp.actions.push(soap_call(&control_url, &service_type, action, "<InstanceID>0</InstanceID>", ip).await);
                tokio::time::sleep(INTER_COMMAND_DELAY).await;
            }
        } else if service_type.contains(":service:RenderingControl:") {
            for action in ["GetVolume", "GetMute"] {
                eprintln!("[wiim-capture] upnp: {action} on {service_type}");
                let args = "<InstanceID>0</InstanceID><Channel>Master</Channel>";
                upnp.actions.push(soap_call(&control_url, &service_type, action, args, ip).await);
                tokio::time::sleep(INTER_COMMAND_DELAY).await;
            }
        }
    }

    upnp
}

// ── Output ───────────────────────────────────────────────────────────────────

fn sanitize_filename_component(s: &str) -> String {
    let replaced: String = s.chars().map(|c| if c == ' ' || c == '/' || c == '\\' { '_' } else { c }).collect();
    replaced.chars().map(|c| if c.is_alphanumeric() || c == '_' || c == '-' { c } else { '_' }).collect()
}

fn write_output(capture: &CaptureFile) {
    let ts = chrono::Utc::now().format("%Y%m%d_%H%M%S");
    let model_part = sanitize_filename_component(&capture.model);
    let filename = format!("{model_part}_{ts}.json");
    let json = serde_json::to_string_pretty(capture).expect("CaptureFile must serialize");
    match std::fs::write(&filename, json) {
        Ok(()) => eprintln!("[wiim-capture] wrote {filename}"),
        Err(e) => eprintln!("[wiim-capture] failed to write {filename}: {e}"),
    }
}

// ── CLI arguments ────────────────────────────────────────────────────────────

struct Args {
    ip: String,
    /// Unlocks `method: set, safe: true` commands in `commands.yaml`. Without
    /// this, `wiim-capture` never sends a `Set` command at all — see the
    /// module doc comment.
    destructive: bool,
    /// `--one <command>` — see the module doc comment. `None` for the
    /// normal full-capture flow.
    one: Option<String>,
}

fn usage() -> ! {
    eprintln!("usage: wiim-capture [--destructive] <ip>");
    eprintln!("       wiim-capture --one <command> <ip>");
    eprintln!("       wiim-capture --one upnp:<Action> <ip>");
    eprintln!("       wiim-capture --one upnp:<Service>:<Action> <ip>");
    eprintln!("           <command>  a plain httpapi.asp command, e.g. getPlayerStatusEx");
    eprintln!("           <Action>   a UPnP action, e.g. GetInfoEx — service auto-detected");
    eprintln!("                      for known actions (GetTransportInfo/GetPositionInfo/");
    eprintln!("                      GetMediaInfo/GetInfoEx -> AVTransport; GetVolume/GetMute");
    eprintln!("                      -> RenderingControl), otherwise specify <Service>:");
    eprintln!("           <Service>  AVTransport or RenderingControl");
    std::process::exit(2);
}

/// A `--one` target: either a plain HTTP `httpapi.asp` command, or a UPnP
/// SOAP action — see `usage()` for the `upnp:<Action>`/`upnp:<Service>:<Action>`
/// syntax.
enum OneTarget {
    Http(String),
    Upnp { service: Option<String>, action: String },
}

fn parse_one_target(raw: &str) -> OneTarget {
    match raw.strip_prefix("upnp:") {
        Some(rest) => match rest.split_once(':') {
            Some((service, action)) => {
                OneTarget::Upnp { service: Some(service.to_string()), action: action.to_string() }
            }
            None => OneTarget::Upnp { service: None, action: rest.to_string() },
        },
        None => OneTarget::Http(raw.to_string()),
    }
}

fn parse_args() -> Args {
    let mut ip = None;
    let mut destructive = false;
    let mut one = None;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--destructive" => destructive = true,
            "--one" => {
                let Some(cmd) = args.next() else {
                    eprintln!("wiim-capture: --one requires a command argument");
                    usage();
                };
                one = Some(cmd);
            }
            "-h" | "--help" => usage(),
            other if ip.is_none() && !other.starts_with('-') => ip = Some(other.to_string()),
            other => {
                eprintln!("wiim-capture: unrecognized argument '{other}'");
                usage();
            }
        }
    }
    let Some(ip) = ip else { usage() };
    Args { ip, destructive, one }
}

// ── Main ─────────────────────────────────────────────────────────────────────

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let args = parse_args();
    let ip = args.ip;
    let one = args.one.as_deref().map(parse_one_target);

    // UPnP one-shot doesn't touch httpapi.asp at all (SSDP + description.xml
    // discovery instead), so it's handled before — not after —
    // `probe_and_detect()`, which would otherwise waste 1-2 unrelated HTTP
    // probes for no reason.
    if let Some(OneTarget::Upnp { service, action }) = one {
        send_one_upnp(&ip, service, action).await;
    }

    if args.destructive {
        eprintln!(
            "[wiim-capture] --destructive: safe:true Set commands WILL be sent and will change device state"
        );
    }
    eprintln!("[wiim-capture] target {ip}");
    let (mut commands, winner) = probe_and_detect(&ip).await;

    if let Some(OneTarget::Http(command)) = one {
        let Some(winner) = winner else {
            eprintln!("[wiim-capture] gave up: neither getStatusEx nor getStatus responded on any probed port");
            std::process::exit(1);
        };
        // Endpoint detection already happened to send `winner.cmd_used` —
        // reuse that response instead of an extra round trip if it's the
        // exact command asked for.
        let body = if command == winner.cmd_used {
            winner.body
        } else {
            eprintln!("[wiim-capture] {command}");
            let url = format!("{}://{}:{}/httpapi.asp?command={}", winner.scheme, ip, winner.port, command);
            let attempt = send_request(|| winner.client.get(&url)).await;
            match attempt.body {
                Some(b) if attempt.outcome == Outcome::Ok => b,
                _ => {
                    eprintln!(
                        "[wiim-capture] {command} failed: {}",
                        attempt.error.as_deref().unwrap_or("no response body"),
                    );
                    std::process::exit(1);
                }
            }
        };
        println!("{body}");
        return;
    }

    let captured_at = chrono::Utc::now().to_rfc3339();

    let Some(winner) = winner else {
        eprintln!("[wiim-capture] gave up: neither getStatusEx nor getStatus responded on any probed port");
        let capture = CaptureFile {
            captured_at,
            gave_up: true,
            model: "unknown".to_string(),
            model_source: None,
            firmware: None,
            hardware: None,
            project: None,
            tls_scheme: None,
            tls_port: None,
            commands,
            skipped_unsafe: Vec::new(),
            skipped_not_destructive: Vec::new(),
            upnp: None,
        };
        write_output(&capture);
        return;
    };

    eprintln!("[wiim-capture] endpoint found: {}:{} via {}", winner.scheme, winner.port, winner.cmd_used);

    let (model, firmware, hardware, project) = match serde_json::from_str::<DeviceInfo>(&winner.body) {
        Ok(info) => {
            let caps = DeviceCapabilities::from_device_info(&info);
            (
                caps.model,
                (!info.firmware.is_empty()).then_some(info.firmware),
                (!info.hardware.is_empty()).then_some(info.hardware),
                (!info.project.is_empty()).then_some(info.project),
            )
        }
        Err(_) => ("unknown".to_string(), None, None, None),
    };

    let specs = commands::load_command_specs();
    let (expanded, skipped_unsafe, skipped_not_destructive) = commands::expand_commands(&specs, args.destructive);
    if !skipped_not_destructive.is_empty() {
        eprintln!(
            "[wiim-capture] skipping {} safe Set command(s) (pass --destructive to send them): {}",
            skipped_not_destructive.len(),
            skipped_not_destructive.join(", ")
        );
    }

    for exp in &expanded {
        if exp.command == winner.cmd_used {
            continue; // already captured during model detection above, don't re-fetch
        }
        if exp.method == Method::Getsyslog {
            commands.extend(capture_syslog(winner.scheme, &ip, winner.port, Some(exp)).await);
            continue;
        }
        let url = format!("{}://{}:{}/httpapi.asp?command={}", winner.scheme, ip, winner.port, exp.command);
        eprintln!("[wiim-capture] {}", exp.command);
        let attempt = send_request(|| winner.client.get(&url)).await;
        commands.push(build_command_capture(&exp.command, &url, &ip, attempt, Some(exp)));
        tokio::time::sleep(INTER_COMMAND_DELAY).await;
    }

    eprintln!("[wiim-capture] capturing UPnP (SSDP + description.xml + basic SOAP actions)");
    let upnp = capture_upnp(&ip).await;

    let capture = CaptureFile {
        captured_at,
        gave_up: false,
        model,
        model_source: Some(winner.cmd_used),
        firmware,
        hardware,
        project,
        tls_scheme: Some(winner.scheme.to_string()),
        tls_port: Some(winner.port),
        commands,
        skipped_unsafe,
        skipped_not_destructive,
        upnp: Some(upnp),
    };
    write_output(&capture);
}
