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
    Blob, CaptureFile, CommandCapture, Method, Outcome, ResponseFormat, TcpUartCapture, TcpUartCommandCapture,
    TcpUartOutcome, UpnpActionCapture, UpnpCapture,
};
use rustywiim::device::api::{build_reqwest_client, DeviceInfo, TlsMode};
use rustywiim::device::capabilities::DeviceCapabilities;
use rustywiim::device::tcpuart;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

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

/// Same idea as `ANON_KEY_SUBSTRINGS`, but for keys found *nested* below a
/// JSON value's own top level (`anonymize_json`'s `depth > 0`) — deliberately
/// missing `"name"`, the one entry that's genuinely ambiguous once nested.
/// Confirmed necessary against a real capture: `getPresetInfo`'s
/// `preset_list[].name` holds real, wanted content ("QuickMix", "Yo-Yo Ma
/// Radio" — the entire point of capturing presets), not an identifying
/// device name, whereas `DeviceName`/`GroupName` at the *top* level of the
/// same kind of response genuinely are. `"ad"` stays in this list, unlike
/// `"name"` — confirmed against a real capture that it's needed one level
/// deep too: a paired Bluetooth device's MAC sits in `list[].ad`
/// (`getbtdiscoveryresult`/similar), and the only nested false-positive it
/// also catches (`mainSubExtraDelay`, an unrelated numeric delay setting
/// string, matched via "extrADelay") is a harmless, inconsequential loss
/// compared to leaking a real MAC. `"mac"`/`"uuid"`/`"ssid"`/`"bssid"`/the
/// network-interface names have no such ambiguity at all — they're never a
/// legitimate content-name field regardless of nesting depth — so those
/// stay in the nested list unchanged (e.g. still catches
/// `slave_list[].ssid`/`.uuid`).
const NESTED_ANON_KEY_SUBSTRINGS: &[&str] =
    &["mac", "uuid", "ssid", "bssid", "ad", "eth0", "eth2", "apcli0", "ra0"];

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

/// Length of a MAC-address shape (`XX:XX:XX:XX:XX:XX`, six colon-separated
/// hex octets) starting at `bytes[i]`, if any — always exactly 17 bytes.
fn mac_len_at(bytes: &[u8], i: usize) -> Option<usize> {
    if i + 17 > bytes.len() {
        return None;
    }
    let matches = (0..6).all(|group| {
        let base = i + group * 3;
        bytes[base].is_ascii_hexdigit()
            && bytes[base + 1].is_ascii_hexdigit()
            && (group == 5 || bytes[base + 2] == b':')
    });
    matches.then_some(17)
}

/// Length of a UUID shape (`8-4-4-4-12` hex hyphen-separated groups, e.g.
/// `FF98F359-7E21-BAE3-8D6E-1163FF98F359`) starting at `bytes[i]`, if any.
fn uuid_len_at(bytes: &[u8], i: usize) -> Option<usize> {
    const GROUPS: [usize; 5] = [8, 4, 4, 4, 12];
    let mut pos = i;
    for (gi, &len) in GROUPS.iter().enumerate() {
        if pos + len > bytes.len() || !bytes[pos..pos + len].iter().all(u8::is_ascii_hexdigit) {
            return None;
        }
        pos += len;
        if gi < GROUPS.len() - 1 {
            if bytes.get(pos) != Some(&b'-') {
                return None;
            }
            pos += 1;
        }
    }
    Some(pos - i)
}

/// Length of an IPv4-address shape (four dot-separated 0-255 octets)
/// starting at `bytes[i]`, if any — the embedded-in-larger-text analogue of
/// `looks_like_ipv4()`, which only checks whether an *entire* value is that
/// shape.
fn ipv4_len_at(bytes: &[u8], i: usize) -> Option<usize> {
    let mut pos = i;
    for octet in 0..4 {
        let start = pos;
        while pos < bytes.len() && bytes[pos].is_ascii_digit() && pos - start < 3 {
            pos += 1;
        }
        if pos == start {
            return None;
        }
        let val: u16 = std::str::from_utf8(&bytes[start..pos]).ok()?.parse().ok()?;
        if val > 255 {
            return None;
        }
        if octet < 3 {
            if bytes.get(pos) != Some(&b'.') {
                return None;
            }
            pos += 1;
        }
    }
    Some(pos - i)
}

/// Scans `s` for every occurrence of a shape (`shape_len_at` returns the
/// match length starting at a given byte index, if any) and scrubs each one
/// found — the embedded-in-free-text analogue of a whole-value shape check
/// like `looks_like_ipv4()`. Shared by MAC/UUID/IPv4 scrubbing (see
/// `mac_len_at()`/`uuid_len_at()`/`ipv4_len_at()`) — all three are needed:
/// confirmed against real captures that a MAC can show up embedded inside
/// an unrelated free-text field (`getNetworkHealth`'s `lastDisconnectedMsg`,
/// a diagnostic log line), a UUID inside a raw SSDP response's `USN:`
/// header (never anonymized at all before this — it isn't JSON or XML, just
/// raw multicast-response text), and an IP inside a URL-valued JSON field
/// (`metaData.albumArtURI`, e.g. `"http://10.1.1.10:8097/imageproxy/..."` —
/// neither IP-shaped as a *whole* value nor under a key this module
/// recognizes). ASCII-only: every shape this matches is itself pure ASCII,
/// so a non-ASCII string can't contain one — returned unchanged rather than
/// risk slicing on a non-UTF-8 boundary while scanning byte-by-byte.
fn scrub_embedded(s: &str, shape_len_at: impl Fn(&[u8], usize) -> Option<usize>) -> String {
    let bytes = s.as_bytes();
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    while i < bytes.len() {
        match shape_len_at(bytes, i) {
            Some(len) => {
                out.push_str(&anonymize_string(&s[i..i + len]));
                i += len;
            }
            None => {
                // Advance by one full UTF-8 character, not necessarily one
                // byte. `shape_len_at` only ever matches pure-ASCII spans
                // (MAC/UUID/IPv4 are all ASCII-only shapes) and safely
                // returns `None` at a multi-byte character's lead byte —
                // its own byte-level checks (`is_ascii_hexdigit()` etc.)
                // never match a byte ≥ 0x80 — but the *surrounding* text
                // can be arbitrary UTF-8 (e.g. a track artist like "João
                // Gilberto" sharing a `TrackMetaData` blob with a real,
                // scrubbable LAN IP elsewhere in the same string). An
                // earlier whole-string `is_ascii()` guard bailed out of
                // scrubbing *anything* whenever such a string showed up
                // anywhere in the value — confirmed via a real capture that
                // this silently let an IP right next to non-ASCII artist
                // text through untouched.
                let ch_len = s[i..].chars().next().map(|c| c.len_utf8()).unwrap_or(1);
                out.push_str(&s[i..i + ch_len]);
                i += ch_len;
            }
        }
    }
    out
}

fn scrub_embedded_macs(s: &str) -> String {
    scrub_embedded(s, mac_len_at)
}

fn scrub_embedded_uuids(s: &str) -> String {
    scrub_embedded(s, uuid_len_at)
}

fn scrub_embedded_ips(s: &str) -> String {
    scrub_embedded(s, ipv4_len_at)
}

/// Applies all three embedded-shape scrubs (MAC/UUID/IPv4) in sequence —
/// the one place that combination is needed, so callers don't have to
/// remember to chain all three themselves.
fn scrub_all_embedded(s: &str) -> String {
    scrub_embedded_ips(&scrub_embedded_uuids(&scrub_embedded_macs(s)))
}

/// True for a JSON key or XML tag name that holds an artwork image URL
/// (`albumArtURI`, `PicUrl`/`picurl`, ...) — see `anonymize_art_url()` for
/// why these get different treatment than every other URL/IP-bearing field.
fn is_art_url_field(name: &str) -> bool {
    let nl = name.to_lowercase();
    nl.contains("arturi") || nl.contains("picurl") || nl.contains("pic_url")
}

/// Artwork-image-URL fields (`albumArtURI`, `PicUrl`, ...) are deliberately
/// exempt from the usual IP/MAC/UUID scrubbing that applies to every other
/// field — confirmed necessary: art is sometimes served by a *separate* LAN
/// device (a DLNA/Music-Assistant server, say) whose address isn't the
/// target device's own and isn't otherwise identifying, and capture files
/// are meant to stay usable for testing real artwork downloads (this app's
/// own preset-art fetch path, for one) — scrubbing every such URL on
/// principle would make that impossible. The one exception: when the URL is
/// served *by the device itself* (confirmed real case: a USB stick's cover
/// art, served from the device's own embedded web server at its own IP),
/// that IS the device's real address like everywhere else, so it still gets
/// scrubbed. `device_ip` is `Some` only when the caller can identify that
/// address — the just-connected-to `ip` at live capture time; recovered
/// from the raw SSDP `LOCATION:` header when reprocessing an
/// already-written file (see `device_ip_from_ssdp()`), since every other
/// field that once held it has usually already been scrubbed into an
/// ambiguous placeholder by an earlier pass by the time `--reanonymize`
/// runs.
fn anonymize_art_url(url: &str, device_ip: Option<&str>) -> String {
    match device_ip {
        Some(ip) if url.contains(ip) => anonymize_ip_in_url(url, ip),
        _ => url.to_string(),
    }
}

/// Recovers the device's own LAN IP from its raw SSDP `LOCATION:` header
/// (`"http://10.1.1.10:49152/description.xml"` -> `"10.1.1.10"`) — the only
/// way `--reanonymize` can identify "the device's own address" for
/// `anonymize_art_url()`'s one exception. Must be called before
/// `ssdp_response` itself gets scrubbed.
fn device_ip_from_ssdp(ssdp_response: &str) -> Option<String> {
    let location = extract_header(ssdp_response, "LOCATION")?;
    let after_scheme = location.split("://").nth(1)?;
    let host = after_scheme.split(['/', ':']).next()?;
    looks_like_ipv4(host).then(|| host.to_string())
}

/// Anonymizes matching string values in a JSON value, in place. Public
/// entry point — always starts at depth 0 (the value's own top level),
/// which is where every existing call site's own real top-level fields
/// (`DeviceName`, `SSID`, etc.) live. See `anonymize_json_at()` for why
/// recursion below that uses a narrower key list. `device_ip`, when known,
/// is the one context `anonymize_art_url()` needs — see its doc comment.
fn anonymize_json(v: &mut serde_json::Value, device_ip: Option<&str>) {
    anonymize_json_at(v, 0, device_ip);
}

/// Recursion depth 0 uses the full `ANON_KEY_SUBSTRINGS` (including the
/// broad, ambiguous `"name"` entry) — safe there because a JSON blob's own
/// top level is always "device/session info" shaped in every LinkPlay
/// response this tool captures. Depth > 0 uses the narrower
/// `NESTED_ANON_KEY_SUBSTRINGS` instead (see its own doc comment for why
/// `"name"` specifically is dropped there but `"ad"` isn't): nested
/// structures (`slave_list`, `preset_list`, routine lists, key-mapping
/// lists, ...) commonly reuse `name` for actual *content* names (a preset's
/// name, a routine's name), not a device identity — confirmed via a real
/// capture that a naive "recurse with the same broad list at every depth"
/// fix would have scrubbed `preset_list[].name` ("QuickMix", "Yo-Yo Ma
/// Radio"), i.e. exactly the data these captures exist to show.
/// `looks_like_ipv4()` isn't depth-gated at all — an IP-shaped value is
/// unambiguous regardless of nesting (also how
/// `multiroom:getSlaveList`/UPnP `GetInfoEx`'s embedded `SlaveList` JSON's
/// `slave_list[].ip`, one level below the top object, get caught — a real
/// leak this recursion was added to fix in the first place). Every string
/// value not already fully scrubbed also gets `scrub_all_embedded()`
/// applied — a MAC/UUID/IP can show up *embedded* inside an otherwise-
/// unrelated field (`getNetworkHealth`'s `lastDisconnectedMsg`, a free-text
/// diagnostic log line — confirmed via a real capture), not just as a
/// field's entire value. Artwork-URL-shaped keys (`is_art_url_field()`) are
/// checked first and handled entirely separately via `anonymize_art_url()`.
fn anonymize_json_at(v: &mut serde_json::Value, depth: usize, device_ip: Option<&str>) {
    let key_substrings = if depth == 0 { ANON_KEY_SUBSTRINGS } else { NESTED_ANON_KEY_SUBSTRINGS };
    match v {
        serde_json::Value::Object(map) => {
            for (k, val) in map.iter_mut() {
                if let serde_json::Value::String(s) = val {
                    if is_art_url_field(k) {
                        *s = anonymize_art_url(s, device_ip);
                    } else {
                        let kl = k.to_lowercase();
                        let key_matches = key_substrings.iter().any(|needle| kl.contains(needle));
                        if key_matches || looks_like_ipv4(s) {
                            *s = anonymize_string(s);
                        } else {
                            *s = scrub_all_embedded(s);
                        }
                    }
                }
                anonymize_json_at(val, depth + 1, device_ip);
            }
        }
        serde_json::Value::Array(items) => {
            for val in items.iter_mut() {
                anonymize_json_at(val, depth + 1, device_ip);
            }
        }
        _ => {}
    }
}

/// Tag-name substrings considered identifying for XML anonymization — a
/// *stricter* subset of `ANON_KEY_SUBSTRINGS` than the JSON-key matcher
/// uses, in two ways.
///
/// `"ad"` is deliberately excluded: it was added to the JSON list for
/// Bluetooth's flat `ad` field (an exact key name in a flat object, low
/// collision risk), but as a 2-letter substring it collides with ordinary
/// HTML/XML tag names — confirmed against a real device: `getsyslog`'s HTML
/// response has a `<head>` tag, and "head" contains "ad", which made
/// `anonymize_xml` treat the *entire* `<head>...</head>` span as matched
/// "content" and character-scrub it (`anonymize_string` doesn't know about
/// markup, so this corrupted the real `<meta charset=...>` tag inside into
/// garbage). No other list entry has shown this problem in practice.
///
/// `"name"` is also excluded, unlike the JSON side (which still uses it at
/// depth 0 — see `ANON_KEY_SUBSTRINGS`'s doc comment). Every *XML tag* named
/// with "name" ever seen across real captures is either a device-identity
/// field already covered another way (`friendlyName`/`modelName`, which get
/// their own explicit skip below regardless) or, confirmed via the
/// `PlayQueue` service's `GetKeyMapping`/`BrowseQueue` responses, a genuine
/// *content* name (`<Name>`/`<ListName>`/`<PresetName>`/`<ShowName>` — a
/// preset's or playlist's own name) that must not be scrubbed — the same
/// "QuickMix"/"Yo-Yo Ma Radio" problem `NESTED_ANON_KEY_SUBSTRINGS` exists
/// to avoid for JSON, just discovered later for the XML side once
/// `wiim-capture` started actually calling those actions. Unlike the JSON
/// case there's no depth signal to fall back on here, so this is an
/// unconditional drop rather than a depth-gated one — accepted since no XML
/// tag has ever needed it.
const XML_ANON_KEY_SUBSTRINGS: &[&str] =
    &["mac", "uuid", "ssid", "bssid", "eth0", "eth2", "apcli0", "ra0", "udn"];

/// Anonymizes the text content of XML leaf elements whose *tag name* matches
/// `XML_ANON_KEY_SUBSTRINGS` — e.g. `<UDN>uuid:...</UDN>` in UPnP
/// description.xml. Non-namespace-aware, assumes the matched tag has no
/// nested elements (true for every LinkPlay/UPnP tag this matches in
/// practice) — sufficient for the known shape, not a general XML anonymizer,
/// same spirit as `extract_tag`. `device_ip`, when known, is the one
/// context `anonymize_art_url()` needs for `upnp:albumArtURI` — see its doc
/// comment.
fn anonymize_xml(xml: &str, device_ip: Option<&str>) -> String {
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
        // Scrub embedded MAC/UUID/IP shapes inside the tag's own *attribute
        // values*, not just its text content further down — confirmed real
        // leak: DIDL-Lite `<item parentID="wiim_uuid:FF98F7F4-...">`, the
        // device's own UUID sitting in an attribute value, invisible to
        // every content-based check below since this whole tag string gets
        // emitted here, before any of them run. Safe to apply
        // unconditionally to the whole `<...>` span: none of the shapes
        // scanned for contain `<`/`>`/`=`/`"`, so tag structure and
        // attribute names are never at risk, only an embedded value itself.
        out.push_str(&scrub_all_embedded(tag));
        i = tag_end;

        let is_opening = !tag.starts_with("</") && !tag.ends_with("/>") && !tag.starts_with("<?") && !tag.starts_with("<!");
        if !is_opening {
            continue;
        }
        let name = tag[1..tag.len() - 1].split_whitespace().next().unwrap_or("").to_lowercase();
        // Explicit early skip for these two, even though `XML_ANON_KEY_
        // SUBSTRINGS` no longer has a "name" entry that would otherwise
        // match them anyway (see its doc comment) — a room name and a
        // marketing model string are useful to see in a capture and were
        // never meant to be treated as sensitive here. Kept as its own
        // named case rather than relying on the general fallback further
        // down to also leave them alone, so the "these two are
        // deliberately exempt" intent stays explicit at the point they're
        // encountered, not just an emergent property of what else happens
        // to be in the substring list.
        if matches!(name.as_str(), "friendlyname" | "modelname") {
            continue;
        }
        if is_art_url_field(&name) {
            let Some(close_start) = xml[i..].find("</").map(|p| i + p) else {
                continue;
            };
            out.push_str(&anonymize_art_url(&xml[i..close_start], device_ip));
            i = close_start;
            continue;
        }
        if !XML_ANON_KEY_SUBSTRINGS.iter().any(|needle| name.contains(needle)) {
            // Not a known identifying tag name — but its content might
            // still embed identifying fields regardless, in one of two
            // shapes:
            //   - A JSON blob as escaped text content. LinkPlay bundles a
            //     full JSON status/slave-list this way in several UPnP SOAP
            //     responses (`GetControlDeviceInfo`'s `<Status>{"apcli0":
            //     "10.1.1.76",...}</Status>`/`<SlaveList>{...}</SlaveList>`,
            //     `GetInfoEx`'s `<SlaveList>` likewise) — confirmed via a
            //     real capture that leaked exactly this, since neither this
            //     tag-name check nor `anonymize_json` (which only ever runs
            //     on responses that are JSON *format*, not XML) ever looked
            //     at it. Handled by content *shape*, not tag name — same
            //     principle `looks_like_ipv4()` already applies to JSON
            //     values regardless of key name.
            //   - Escaped *XML* as text content (`TrackMetaData`'s
            //     DIDL-Lite, itself escaped once more inside the outer SOAP
            //     envelope) — not JSON at all, so the above never touches
            //     it; confirmed via a real capture that `GetPositionInfo`/
            //     `GetMediaInfo`/`GetInfoEx` all leak a real LAN IP this way
            //     (`upnp:albumArtURI`, when it *is* the device's own art —
            //     see `anonymize_nested_xml_in_xml_text()`, which recurses
            //     into this content with the same tag-name-aware logic,
            //     rather than treating the whole blob as one opaque string
            //     — that would apply the artwork-URL exemption too
            //     bluntly, to content that isn't actually an artwork URL).
            let Some(close_start) = xml[i..].find("</").map(|p| i + p) else {
                continue;
            };
            let content = &xml[i..close_start];
            // A real *container* tag with actual child elements (e.g.
            // `<DIDL-Lite>` once `anonymize_nested_xml_in_xml_text()` has
            // recursed into it) has a literal, unescaped `<` in its content
            // before that content's own end — the true end-of-content search
            // above just found its *first child's* closing tag, not its
            // own. Every genuinely leaf/opaque tag this fallback is meant
            // for (`Status`/`SlaveList`/`TrackMetaData`'s JSON or
            // once-escaped-XML text) never contains a literal `<` in its
            // content at all. So: a literal `<` here means "don't touch
            // this tag's content at all, let the main loop keep walking
            // tag-by-tag" — anything else risks cutting the wrong tag's
            // content short and corrupting real markup (confirmed: this is
            // exactly what happened before this check existed, corrupting
            // `<dc:title>` content nested inside a recursed-into
            // `<DIDL-Lite>`).
            if !content.contains('<') {
                let replacement = anonymize_json_in_xml_text(content, device_ip)
                    .or_else(|| anonymize_nested_xml_in_xml_text(content, device_ip))
                    .unwrap_or_else(|| scrub_all_embedded(content));
                out.push_str(&replacement);
                i = close_start;
            }
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

/// Un-escapes the handful of XML entities LinkPlay's JSON-in-XML-tag-content
/// actually uses. Order matters: `&amp;` must be un-escaped last, or an
/// already-escaped ampersand followed by literal text (`&amp;lt;`) would
/// incorrectly turn into `<` instead of the literal `&lt;` it represents.
fn xml_unescape(s: &str) -> String {
    s.replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&amp;", "&")
}

/// Re-escapes a JSON string for safe placement back inside XML tag text
/// content — the exact reverse of `xml_unescape()`, `&` first this time so
/// it doesn't double-escape the entities the later replacements produce.
fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// If `content` (raw XML tag text, still entity-escaped) unescapes to valid
/// JSON, anonymizes it (recursing via `anonymize_json()`) and re-escapes the
/// result for XML. Returns `None` if it doesn't parse as JSON at all — most
/// tag content doesn't (plain text, timestamps, nested XML-in-XML like
/// `TrackMetaData`'s DIDL-Lite), so the caller falls back to
/// `anonymize_nested_xml_in_xml_text()`/a plain content-shape scrub instead.
fn anonymize_json_in_xml_text(content: &str, device_ip: Option<&str>) -> Option<String> {
    let unescaped = xml_unescape(content);
    let mut value: serde_json::Value = serde_json::from_str(unescaped.trim()).ok()?;
    if !value.is_object() && !value.is_array() {
        return None;
    }
    anonymize_json(&mut value, device_ip);
    Some(xml_escape(&value.to_string()))
}

/// If `content` (raw XML tag text, still entity-escaped) unescapes to
/// something that looks like nested XML (`TrackMetaData`'s embedded
/// DIDL-Lite, etc. — starts with `<` once unescaped), recursively
/// anonymizes it with the exact same tag-name-aware logic (so e.g. an inner
/// `<upnp:albumArtURI>` gets the *same* artwork-URL exemption at any
/// nesting depth, not just at the top level of a SOAP response) and
/// re-escapes the result. Returns `None` if the unescaped content doesn't
/// look like XML at all, so the caller falls back to a plain content-shape
/// scrub instead.
fn anonymize_nested_xml_in_xml_text(content: &str, device_ip: Option<&str>) -> Option<String> {
    let unescaped = xml_unescape(content);
    if !unescaped.trim_start().starts_with('<') {
        return None;
    }
    Some(xml_escape(&anonymize_xml(&unescaped, device_ip)))
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
                anonymize_json(&mut val, Some(ip));
                decoded = decode_player_status_fields(command, &val);
            } else if fmt == ResponseFormat::Xml {
                if let serde_json::Value::String(s) = &val {
                    val = serde_json::Value::String(anonymize_xml(s, Some(ip)));
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

/// Extracts every non-overlapping, top-level `<tag>...</tag>` block's inner
/// content — used for `<service>` (device description) and `<action>`/
/// `<argument>` (SCPD action lists), all of which repeat as flat siblings
/// with no same-named nesting in real LinkPlay XML.
fn extract_blocks(xml: &str, tag: &str) -> Vec<String> {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    let mut out = Vec::new();
    let mut rest = xml;
    while let Some(start) = rest.find(&open) {
        let after = &rest[start + open.len()..];
        match after.find(&close) {
            Some(end) => {
                out.push(after[..end].to_string());
                rest = &after[end + close.len()..];
            }
            None => break,
        }
    }
    out
}

fn extract_service_blocks(xml: &str) -> Vec<String> {
    extract_blocks(xml, "service")
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

/// Fetches a service's SCPD (Service Control Point Definition) XML — the
/// UPnP-standard self-description of a service's actions and arguments.
/// `url` is already fully resolved (via `resolve_url()`), unlike
/// `fetch_description()` which has to guess candidate URLs.
async fn fetch_scpd(url: &str) -> Attempt {
    let client = build_reqwest_client(tls_for_scheme(url.split(':').next().unwrap_or("http")), REQUEST_TIMEOUT);
    send_request(|| client.get(url)).await
}

/// True for action names that look like pure accessors by UPnP/LinkPlay
/// naming convention ("Get"/"Browse" prefix) — mirrors `commands.yaml`'s own
/// "broad on Get, sparse on Set" philosophy for the HTTP command list. Only
/// these get auto-invoked when probing a newly-discovered service like
/// `PlayQueue`; anything else (Add/Remove/Delete/Insert/Clear/Set/Play/...)
/// is left alone since we have no per-action review of this vendor-specific
/// service the way `capture_upnp()`'s hardcoded AVTransport/RenderingControl
/// action lists already got.
fn looks_read_only_upnp_action(name: &str) -> bool {
    name.starts_with("Get") || name.starts_with("Browse")
}

/// Best-effort argument value for a `PlayQueue` SOAP argument name. Confirmed
/// against Arylic's own `upnp_hack` shell scripts (a real, working reference
/// implementation of this exact vendor service — not a guess derived from
/// unrelated standard-UPnP conventions): `BrowseQueue`'s `QueueName` defaults
/// to `"TotalQueue"` (list everything) with `SkipQueue` `"0"`; `GetKeyMapping`
/// takes no arguments at all. Anything not covered here falls back to an
/// empty value — a wrong guess just means the call fails and gets recorded
/// as such, which is itself useful diagnostic information.
fn guess_upnp_arg_value(name: &str) -> &'static str {
    match name {
        "InstanceID" => "0",
        "QueueName" => "TotalQueue",
        "SkipQueue" => "0",
        _ => "",
    }
}

/// Parses an SCPD's `<actionList>` into `(action name, "in" argument names)`
/// pairs, for whatever actions look read-only (see
/// `looks_read_only_upnp_action()`). Non-namespace-aware `<tag>` block
/// extraction, same as `extract_blocks()` elsewhere in this file — SCPD XML
/// from real LinkPlay devices doesn't need more than that.
fn parse_scpd_readonly_actions(scpd_xml: &str) -> Vec<(String, Vec<String>)> {
    extract_blocks(scpd_xml, "action")
        .into_iter()
        .filter_map(|action_block| {
            let name = extract_tag(&action_block, "name")?;
            if !looks_read_only_upnp_action(&name) {
                return None;
            }
            let in_args: Vec<String> = extract_blocks(&action_block, "argument")
                .into_iter()
                .filter(|arg| extract_tag(arg, "direction").as_deref() == Some("in"))
                .filter_map(|arg| extract_tag(&arg, "name"))
                .collect();
            Some((name, in_args))
        })
        .collect()
}

/// Fetches `PlayQueueSCPD.xml` (if the SCPDURL was found in the device
/// description) and, for each declared action that looks read-only, calls it
/// with best-effort guessed arguments against `control_url` (the same
/// per-service `controlURL` `capture_upnp()` already extracted from the
/// device description for its AVTransport/RenderingControl branches).
/// Stores the raw SCPD in `upnp.play_queue_scpd` regardless of whether any
/// action call succeeds — it's the authoritative record of what this
/// service actually declares, for a human to read directly rather than
/// trusting our argument guesses.
async fn capture_playqueue(
    upnp: &mut UpnpCapture,
    description_url: &str,
    scpd_url_raw: &str,
    control_url: &str,
    service_type: &str,
    ip: &str,
) {
    let scpd_url = resolve_url(description_url, scpd_url_raw);
    eprintln!("[wiim-capture] upnp: fetching PlayQueue SCPD at {scpd_url}");
    let attempt = fetch_scpd(&scpd_url).await;
    let Some(raw) = attempt.body else {
        eprintln!("[wiim-capture] upnp: PlayQueue SCPD fetch failed: {}", attempt.error.as_deref().unwrap_or("no response body"));
        return;
    };

    let (format, mut body) = encode_blob(&raw);
    if format == ResponseFormat::Xml {
        body = serde_json::Value::String(anonymize_xml(&raw, Some(ip)));
    }
    upnp.play_queue_scpd = Some(Blob { format, body });

    for (action, in_args) in parse_scpd_readonly_actions(&raw) {
        let args_xml: String = in_args.iter()
            .map(|a| format!("<{a}>{}</{a}>", guess_upnp_arg_value(a)))
            .collect();
        eprintln!("[wiim-capture] upnp: {action} on PlayQueue (args: {in_args:?})");
        upnp.actions.push(soap_call(control_url, service_type, &action, &args_xml, ip).await);
        tokio::time::sleep(INTER_COMMAND_DELAY).await;
    }
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
                body = serde_json::Value::String(anonymize_xml(s, Some(ip)));
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
        "GetTransportInfo" | "GetPositionInfo" | "GetMediaInfo" | "GetInfoEx"
            | "GetCurrentTransportActions" => Some("AVTransport"),
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
    // Raw multicast-response text, not JSON/XML — never anonymized at all
    // before this (`anonymize_json`/`anonymize_xml` only ever ran on
    // command/action response bodies), despite routinely carrying a real
    // LAN IP in its `LOCATION:` header and a real device UUID in `USN:`
    // (confirmed via a real capture leaking both).
    upnp.ssdp_response = ssdp_text.as_deref().map(scrub_all_embedded);
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
    let anonymized_raw = anonymize_xml(&raw, Some(ip));
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
            for action in [
                "GetTransportInfo", "GetPositionInfo", "GetMediaInfo", "GetInfoEx",
                // Standard AVTransport action reporting which transport
                // commands are currently valid (e.g. "Play,Stop,Pause,Seek,
                // Next,Previous"). Not consumed by anything yet — added
                // purely to capture what real WiiM hardware actually
                // returns for it, since neither reference project checked
                // (pywiim wraps it but only for a diagnostics-only
                // snapshot, never its real state model) trusts it enough
                // to rely on.
                "GetCurrentTransportActions",
            ] {
                eprintln!("[wiim-capture] upnp: {action} on {service_type}");
                upnp.actions.push(soap_call(&control_url, &service_type, action, "<InstanceID>0</InstanceID>", ip).await);
                tokio::time::sleep(INTER_COMMAND_DELAY).await;
            }
        } else if service_type.contains(":service:RenderingControl:") {
            for (action, args) in [
                ("GetVolume", "<InstanceID>0</InstanceID><Channel>Master</Channel>"),
                ("GetMute", "<InstanceID>0</InstanceID><Channel>Master</Channel>"),
                // LinkPlay/Arylic-specific extension (confirmed via Arylic's
                // own `upnp_hack` reference scripts): bundles volume/mute/
                // channel/slave-list plus the device's full `getStatusEx`-
                // equivalent JSON blob (`Status`) — a second, UPnP-only path
                // to the same device info HTTP's `getStatusEx` provides,
                // useful for devices where that HTTP call is unreliable.
                ("GetControlDeviceInfo", "<InstanceID>0</InstanceID>"),
            ] {
                eprintln!("[wiim-capture] upnp: {action} on {service_type}");
                upnp.actions.push(soap_call(&control_url, &service_type, action, args, ip).await);
                tokio::time::sleep(INTER_COMMAND_DELAY).await;
            }
        } else if service_type.contains("wiimu-com:service:PlayQueue") {
            if let Some(scpd_url_raw) = extract_tag(block, "SCPDURL") {
                capture_playqueue(&mut upnp, &description_url, &scpd_url_raw, &control_url, &service_type, ip).await;
            }
        }
    }

    upnp
}

// ── tcpuart capture ──────────────────────────────────────────────────────────

/// Connect timeout for the tcpuart probe itself — separate from the read
/// timeouts below, since plenty of devices simply won't have this port open
/// at all and that shouldn't hang the whole capture run.
const TCPUART_CONNECT_TIMEOUT: Duration = Duration::from_secs(3);
/// How long to wait for the *first* byte of a reply before concluding the
/// device didn't answer this particular command at all — a real, expected
/// outcome for several commands (e.g. Arylic-specific passthrough probes
/// against non-Arylic hardware), not a failure.
const TCPUART_FIRST_BYTE_TIMEOUT: Duration = Duration::from_millis(1500);
/// Once any data has arrived, how long to wait for more before deciding the
/// device is done sending — short, since a reply is normally one packet.
const TCPUART_QUIET_GAP_TIMEOUT: Duration = Duration::from_millis(400);
/// Arylic's own doc recommends at least 200ms between commands on this
/// protocol; used with a little headroom.
const TCPUART_INTER_COMMAND_DELAY: Duration = Duration::from_millis(250);

/// Reads from `stream` using a "read until quiet" pattern: wait up to
/// `TCPUART_FIRST_BYTE_TIMEOUT` for the first byte (a full timeout with
/// nothing read at all is reported as `Ok(Vec::new())`, an expected outcome,
/// not an error), then keep reading with a `TCPUART_QUIET_GAP_TIMEOUT`
/// quiet-gap timeout between chunks, stopping as soon as that gap elapses or
/// the peer closes the connection. Returns `Err` only for a real socket
/// error partway through, not for a plain timeout.
async fn read_until_quiet(stream: &mut tokio::net::TcpStream) -> std::io::Result<Vec<u8>> {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];

    match tokio::time::timeout(TCPUART_FIRST_BYTE_TIMEOUT, stream.read(&mut chunk)).await {
        Ok(Ok(0)) => return Ok(buf), // peer closed before sending anything
        Ok(Ok(n)) => buf.extend_from_slice(&chunk[..n]),
        Ok(Err(e)) => return Err(e),
        Err(_) => return Ok(buf), // no first byte within the window — NoResponse
    }

    loop {
        match tokio::time::timeout(TCPUART_QUIET_GAP_TIMEOUT, stream.read(&mut chunk)).await {
            Ok(Ok(0)) => break, // peer closed
            Ok(Ok(n)) => buf.extend_from_slice(&chunk[..n]),
            Ok(Err(e)) => return Err(e),
            Err(_) => break, // quiet gap elapsed, treat as end of this reply
        }
    }
    Ok(buf)
}

/// Best-effort tcpuart probe: connects once, then sends every command in
/// `tcpuart::GET_COMMANDS` in turn, recording whatever comes back (or
/// doesn't — `NoResponse` is a real, expected outcome for several commands,
/// not a failure). Never sends anything but the curated GET-only list — see
/// that constant's own doc comment for why there's no destructive-flag gate
/// here at all. A connect failure (many devices simply don't expose this
/// port) is recorded as `connect_error` and the whole probe is skipped, not
/// treated as fatal to the rest of the capture.
async fn capture_tcpuart(ip: &str) -> TcpUartCapture {
    let addr = format!("{ip}:{}", tcpuart::TCPUART_PORT);
    let mut stream = match tokio::time::timeout(TCPUART_CONNECT_TIMEOUT, tokio::net::TcpStream::connect(&addr)).await {
        Ok(Ok(s)) => s,
        Ok(Err(e)) => return TcpUartCapture { connect_error: Some(e.to_string()), commands: Vec::new() },
        Err(_) => return TcpUartCapture { connect_error: Some("connect timed out".to_string()), commands: Vec::new() },
    };

    let mut commands = Vec::new();
    for &cmd in tcpuart::GET_COMMANDS {
        eprintln!("[wiim-capture] tcpuart: {cmd}");
        let packet = tcpuart::build_packet(cmd);
        if let Err(e) = stream.write_all(&packet).await {
            commands.push(TcpUartCommandCapture {
                command: cmd.to_string(),
                outcome: TcpUartOutcome::ConnectionError,
                error: Some(e.to_string()),
                response_base64: None,
            });
            break; // connection's dead, no point trying further commands on it
        }

        let (outcome, error, response_base64) = match read_until_quiet(&mut stream).await {
            Ok(bytes) if bytes.is_empty() => (TcpUartOutcome::NoResponse, None, None),
            Ok(bytes) => (TcpUartOutcome::Ok, None, Some(base64::engine::general_purpose::STANDARD.encode(&bytes))),
            Err(e) => (TcpUartOutcome::ConnectionError, Some(e.to_string()), None),
        };
        let is_connection_error = outcome == TcpUartOutcome::ConnectionError;
        commands.push(TcpUartCommandCapture { command: cmd.to_string(), outcome, error, response_base64 });
        if is_connection_error {
            break;
        }

        tokio::time::sleep(TCPUART_INTER_COMMAND_DELAY).await;
    }

    TcpUartCapture { connect_error: None, commands }
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

// ── Re-anonymization of an already-written capture file ─────────────────────
//
// `--reanonymize <file>` reprocesses a capture file already on disk with the
// current anonymization logic in place — for fixing files written by an
// older, less complete pass (this tool's own anonymization has grown several
// real-capture-driven fixes over time; a file written before all of them
// exist can't retroactively benefit without this) without needing to redo
// the live capture. Overwrites the file in place. `friendly_name`/
// `model_name` are deliberately left untouched, same exemption
// `anonymize_xml` already applies at capture time. The device's own IP
// (needed for `anonymize_art_url()`'s one exception — see its doc comment)
// is recovered from the file's own raw `ssdp_response` via
// `device_ip_from_ssdp()`, since a live capture's `ip` argument obviously
// isn't available when just reprocessing a file after the fact.

fn reanonymize_blob_value(body: &mut serde_json::Value, fmt: ResponseFormat, device_ip: Option<&str>) {
    match fmt {
        ResponseFormat::Json => anonymize_json(body, device_ip),
        ResponseFormat::Xml => {
            if let serde_json::Value::String(s) = body {
                *s = anonymize_xml(s, device_ip);
            }
        }
        ResponseFormat::Text => {
            if let serde_json::Value::String(s) = body {
                *s = scrub_all_embedded(s);
            }
        }
        // Binary blob — scrubbing characters would corrupt the encoding.
        ResponseFormat::Base64 => {}
    }
}

fn reanonymize_blob(blob: &mut Blob, device_ip: Option<&str>) {
    reanonymize_blob_value(&mut blob.body, blob.format, device_ip);
}

fn reanonymize_command(c: &mut CommandCapture, device_ip: Option<&str>) {
    c.url = scrub_all_embedded(&c.url);
    if let Some(err) = &mut c.error {
        *err = scrub_all_embedded(err);
    }
    if let (Some(body), Some(fmt)) = (&mut c.body, c.format) {
        reanonymize_blob_value(body, fmt, device_ip);
    }
    if let Some(decoded) = &mut c.decoded {
        anonymize_json(decoded, device_ip);
    }
}

fn reanonymize_action(a: &mut UpnpActionCapture, device_ip: Option<&str>) {
    a.control_url = scrub_all_embedded(&a.control_url);
    if let Some(err) = &mut a.error {
        *err = scrub_all_embedded(err);
    }
    if let Some(resp) = &mut a.response {
        reanonymize_blob(resp, device_ip);
    }
}

fn reanonymize_capture(cap: &mut CaptureFile) {
    // Must be recovered before `ssdp_response` itself gets scrubbed below —
    // see `device_ip_from_ssdp()`'s doc comment for why this is the only
    // place `--reanonymize` can still identify the device's own address.
    let device_ip = cap.upnp.as_ref()
        .and_then(|u| u.ssdp_response.as_deref())
        .and_then(device_ip_from_ssdp);
    let device_ip = device_ip.as_deref();

    for c in &mut cap.commands {
        reanonymize_command(c, device_ip);
    }
    let Some(upnp) = &mut cap.upnp else { return };
    for s in [&mut upnp.ssdp_response, &mut upnp.ssdp_error, &mut upnp.location, &mut upnp.description_url] {
        if let Some(s) = s {
            *s = scrub_all_embedded(s);
        }
    }
    if let Some(b) = &mut upnp.description {
        reanonymize_blob(b, device_ip);
    }
    if let Some(b) = &mut upnp.play_queue_scpd {
        reanonymize_blob(b, device_ip);
    }
    for a in &mut upnp.actions {
        reanonymize_action(a, device_ip);
    }
}

fn run_reanonymize(path: &str) -> ! {
    let text = std::fs::read_to_string(path).unwrap_or_else(|e| {
        eprintln!("[wiim-capture] reading {path}: {e}");
        std::process::exit(1);
    });
    let mut cap: CaptureFile = serde_json::from_str(&text).unwrap_or_else(|e| {
        eprintln!("[wiim-capture] parsing {path}: {e}");
        std::process::exit(1);
    });
    reanonymize_capture(&mut cap);
    let json = serde_json::to_string_pretty(&cap).expect("CaptureFile must serialize");
    match std::fs::write(path, json) {
        Ok(()) => {
            eprintln!("[wiim-capture] re-anonymized {path}");
            std::process::exit(0);
        }
        Err(e) => {
            eprintln!("[wiim-capture] writing {path}: {e}");
            std::process::exit(1);
        }
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
    /// `--tcpuart` — skip the normal HTTP/UPnP capture entirely and only
    /// probe the raw TCP UART pass-through protocol (port 8899, see
    /// `device::tcpuart`) with its curated GET-only command list. A
    /// *quick-iteration* mode, not an additive one: the normal (no-flag)
    /// capture flow always attempts tcpuart too (see `main()`) — this
    /// flag is for when only the tcpuart side is wanted, without waiting
    /// through the full HTTP scheme/port probe and UPnP discovery first.
    tcpuart: bool,
}

fn usage() -> ! {
    eprintln!("usage: wiim-capture [--destructive] <ip>");
    eprintln!("       wiim-capture --tcpuart <ip>");
    eprintln!("       wiim-capture --one <command> <ip>");
    eprintln!("       wiim-capture --one upnp:<Action> <ip>");
    eprintln!("       wiim-capture --one upnp:<Service>:<Action> <ip>");
    eprintln!("       wiim-capture --reanonymize <file>");
    eprintln!("           <command>  a plain httpapi.asp command, e.g. getPlayerStatusEx");
    eprintln!("           <Action>   a UPnP action, e.g. GetInfoEx — service auto-detected");
    eprintln!("                      for known actions (GetTransportInfo/GetPositionInfo/");
    eprintln!("                      GetMediaInfo/GetInfoEx -> AVTransport; GetVolume/GetMute");
    eprintln!("                      -> RenderingControl), otherwise specify <Service>:");
    eprintln!("           <Service>  AVTransport or RenderingControl");
    eprintln!("           <file>     reprocesses an already-written capture file in place");
    eprintln!("                      with the current anonymization logic — no device contacted");
    eprintln!("       --tcpuart alone skips the HTTP/UPnP capture entirely and only probes the");
    eprintln!("       raw TCP UART pass-through protocol (port 8899) — for quick iteration.");
    eprintln!("       A normal (no-flag) run always attempts tcpuart too, alongside HTTP/UPnP.");
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
    let mut tcpuart = false;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--destructive" => destructive = true,
            "--tcpuart" => tcpuart = true,
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
    Args { ip, destructive, one, tcpuart }
}

// ── Main ─────────────────────────────────────────────────────────────────────

#[tokio::main(flavor = "current_thread")]
async fn main() {
    // Handled before `parse_args()` (which requires an <ip>) since this mode
    // never contacts a device at all.
    let mut raw_args = std::env::args().skip(1);
    if let Some(flag) = raw_args.next() {
        if flag == "--reanonymize" {
            let Some(path) = raw_args.next() else {
                eprintln!("wiim-capture: --reanonymize requires a file argument");
                usage();
            };
            run_reanonymize(&path);
        }
    }

    let args = parse_args();
    let ip = args.ip;
    let one = args.one.as_deref().map(parse_one_target);

    // `--tcpuart` alone is a quick-iteration mode: skip HTTP scheme/port
    // detection and UPnP discovery entirely (tcpuart doesn't depend on
    // either) and only probe the tcpuart transport, still writing a
    // normal (mostly-empty) capture file so `wiim-capdump` renders it
    // the same way as the tcpuart section of a full capture.
    if args.tcpuart {
        eprintln!("[wiim-capture] tcpuart-only: {ip}");
        let tcpuart = capture_tcpuart(&ip).await;
        let capture = CaptureFile {
            captured_at: chrono::Utc::now().to_rfc3339(),
            gave_up: false,
            model: "unknown".to_string(),
            model_source: None,
            firmware: None,
            hardware: None,
            project: None,
            tls_scheme: None,
            tls_port: None,
            commands: Vec::new(),
            skipped_unsafe: Vec::new(),
            skipped_not_destructive: Vec::new(),
            upnp: None,
            tcpuart: Some(tcpuart),
        };
        write_output(&capture);
        return;
    }

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
        // HTTP detection failing entirely doesn't mean tcpuart won't
        // answer — it's an independent transport — so still worth a try
        // rather than automatically recording `tcpuart: None`.
        eprintln!("[wiim-capture] capturing tcpuart (raw TCP pass-through protocol, port {})", tcpuart::TCPUART_PORT);
        let tcpuart = capture_tcpuart(&ip).await;
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
            tcpuart: Some(tcpuart),
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

    // Always attempted, not gated behind a flag — `--tcpuart` on its own
    // means "tcpuart *only*" (see the early-return branch above), not
    // "additionally capture tcpuart"; a normal run already always tries.
    eprintln!("[wiim-capture] capturing tcpuart (raw TCP pass-through protocol, port {})", tcpuart::TCPUART_PORT);
    let tcpuart = Some(capture_tcpuart(&ip).await);

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
        tcpuart,
    };
    write_output(&capture);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Real leaked value from a live `WiiM_Ultra` capture:
    /// `getNetworkHealth`'s `lastDisconnectedMsg` is a free-text diagnostic
    /// log line with a real MAC embedded mid-string — the key
    /// (`lastDisconnectedMsg`) doesn't match any key-substring list, and the
    /// whole value isn't MAC/IP-shaped on its own, so only
    /// `scrub_embedded_macs()` (applied to every otherwise-unscrubbed string
    /// value) catches this. The rest of the diagnostic message must survive.
    #[test]
    fn anonymize_json_scrubs_mac_embedded_in_free_text_field() {
        let mut v: serde_json::Value = serde_json::from_str(
            r#"{"lastDisconnectedMsg":"2026-07-08 06:22:49 <3>CTRL-EVENT-DISCONNECTED bssid=bc:fc:e7:2b:9e:4e reason=2"}"#,
        ).unwrap();
        anonymize_json(&mut v, None);
        let s = v.to_string();
        assert!(!s.contains("bc:fc:e7:2b:9e:4e"), "embedded MAC should be scrubbed: {s}");
        assert!(s.contains("CTRL-EVENT-DISCONNECTED"), "rest of message should survive: {s}");
        assert!(s.contains("reason=2"), "rest of message should survive: {s}");
    }

    /// `getPresetInfo`'s real response shape — `preset_list[].name`, one
    /// level below the top object, same nesting depth as `slave_list[].ip`.
    /// A real committed capture (`WiiM_Amp_20260707_173909.json`) shows
    /// these names ("QuickMix", "Yo-Yo Ma Radio") intact, unscrubbed — only
    /// because the *old* shallow `anonymize_json` never reached one level
    /// deep, the same limitation that let the `slave_list[].ip` leak
    /// through. Naively making `anonymize_json` recursive would now reach
    /// this `name` key too (it matches `ANON_KEY_SUBSTRINGS`) and scrub
    /// preset names — which is exactly the data these captures exist to
    /// show, not identifying information. This must keep passing.
    #[test]
    fn anonymize_json_does_not_scrub_preset_list_names() {
        let mut v: serde_json::Value = serde_json::from_str(
            r#"{"preset_list":[{"name":"QuickMix","number":1,"source":"Pandora2","url":"unknow"}],"preset_num":1}"#,
        ).unwrap();
        anonymize_json(&mut v, None);
        let s = v.to_string();
        assert!(s.contains("QuickMix"), "preset name must survive: {s}");
    }

    /// A paired Bluetooth device's MAC, one level below the top object —
    /// same nesting depth as `preset_list[].name` above, but `"ad"` (unlike
    /// `"name"`) is *not* ambiguous once nested, so it must still be caught.
    /// Real leaked value from a live capture (`getbtdiscoveryresult`-shaped
    /// response).
    #[test]
    fn anonymize_json_scrubs_nested_bluetooth_mac() {
        let mut v: serde_json::Value = serde_json::from_str(
            r#"{"list":[{"ad":"44:73:70:61:02:fa","rssi":-60}]}"#,
        ).unwrap();
        anonymize_json(&mut v, None);
        let s = v.to_string();
        assert!(!s.contains("44:73:70:61:02:fa"), "nested BT MAC should be scrubbed: {s}");
    }

    /// A URL field that isn't an artwork URL (`searchUrl`, real TuneIn
    /// DIDL-Lite shape) — not a bare IP (fails `looks_like_ipv4`) under a
    /// key that doesn't match any key-substring list — only
    /// `scrub_all_embedded()`'s IP scan catches the LAN IP embedded partway
    /// through it. The rest of the URL must survive.
    #[test]
    fn anonymize_json_scrubs_ip_embedded_in_non_art_url_value() {
        let mut v: serde_json::Value = serde_json::from_str(
            r#"{"metaData":{"searchUrl":"http://10.1.1.10:8097/imageproxy/abc123?size=512"}}"#,
        ).unwrap();
        anonymize_json(&mut v, None);
        let s = v.to_string();
        assert!(!s.contains("10.1.1.10"), "embedded IP should be scrubbed: {s}");
        assert!(s.contains("imageproxy/abc123"), "rest of URL should survive: {s}");
    }

    /// Per explicit request, artwork-URL fields (`albumArtURI`, `PicUrl`)
    /// are never scrubbed unless they match a known `device_ip` — same
    /// reasoning as the XML-side
    /// `anonymize_xml_leaves_third_party_art_url_untouched`/
    /// `anonymize_xml_scrubs_art_url_matching_device_ip`, for the JSON path
    /// (`getPlayerStatusEx`'s `MetaData.albumArtURI`, `getPresetInfo`'s
    /// `preset_list[].picurl`).
    #[test]
    fn anonymize_json_art_url_exemption_respects_device_ip() {
        let mut untouched: serde_json::Value = serde_json::from_str(
            r#"{"metaData":{"albumArtURI":"http://10.1.1.10:8097/imageproxy/abc123?size=512"}}"#,
        ).unwrap();
        let original = untouched.clone();
        anonymize_json(&mut untouched, Some("10.1.1.73"));
        assert_eq!(untouched, original, "third-party art URL must survive unchanged");

        let mut scrubbed: serde_json::Value = serde_json::from_str(
            r#"{"preset_list":[{"picurl":"https://10.1.1.73/data/lmp_cover_abc.jpeg"}]}"#,
        ).unwrap();
        anonymize_json(&mut scrubbed, Some("10.1.1.73"));
        let s = scrubbed.to_string();
        assert!(!s.contains("10.1.1.73"), "device's own IP should be scrubbed: {s}");
        assert!(s.contains("lmp_cover_abc.jpeg"), "rest of URL should survive: {s}");
    }

    /// Real leaked shape from a live capture: `ssdp_response` is raw SSDP
    /// multicast-response text (never JSON or XML), so it was never passed
    /// through any anonymization at all before `scrub_all_embedded()` was
    /// applied to it directly at capture time — confirmed to carry a real
    /// LAN IP in `LOCATION:` and a real device UUID in `USN:`.
    #[test]
    fn scrub_all_embedded_handles_raw_ssdp_response_text() {
        let ssdp = "HTTP/1.1 200 OK\r\n\
            LOCATION: http://192.168.1.184:49152/description.xml\r\n\
            USN: uuid:FF98F359-7E21-BAE3-8D6E-1163FF98F359::urn:schemas-upnp-org:device:MediaRenderer:1\r\n";
        let out = scrub_all_embedded(ssdp);
        assert!(!out.contains("192.168.1.184"), "LOCATION IP should be scrubbed: {out}");
        assert!(!out.contains("FF98F359-7E21-BAE3-8D6E-1163FF98F359"), "USN UUID should be scrubbed: {out}");
        assert!(out.contains("MediaRenderer"), "rest of USN should survive: {out}");
    }

    /// Nested LAN IP inside `slave_list[].ip`, one level below the flat
    /// top-level object `anonymize_json` used to stop at — the exact shape
    /// that leaked a real IP in `multiroom:getSlaveList`'s JSON body before
    /// `anonymize_json` recursed into arrays/nested objects.
    #[test]
    fn anonymize_json_scrubs_nested_slave_list_ip() {
        // Real shape (Arylic `upnp_hack` reference): a slave entry also
        // carries its own `ssid`/`uuid` — genuinely identifying regardless
        // of nesting, unlike `name` (see the test right below this one).
        let mut v: serde_json::Value = serde_json::from_str(
            r#"{"slaves":1,"slave_list":[{"name":"SoundSystem_05A4","ssid":"SoundSystem_05A4","ip":"10.10.10.92","uuid":"uuid:FF31F012-E0F9-174F-40A0-0FF5FF31F012"}]}"#,
        ).unwrap();
        anonymize_json(&mut v, None);
        let s = v.to_string();
        assert!(!s.contains("10.10.10.92"), "IP should be scrubbed: {s}");
        assert!(!s.contains("uuid:FF31F012"), "UUID should be scrubbed: {s}");
        // Both fields hold the same literal string ("SoundSystem_05A4") —
        // `ssid` must still be scrubbed (matches "ssid"), `name` must not
        // (see `anonymize_json_does_not_scrub_preset_list_names` — a nested
        // `name` is a content name, not a device identity, by the same
        // precedent already established for `friendlyName`/`modelName`).
        assert_eq!(
            v["slave_list"][0]["name"], "SoundSystem_05A4",
            "nested name is a room/device label, not scrubbed here (see friendlyName precedent): {s}"
        );
    }

    /// `GetControlDeviceInfo`'s real (anonymized-for-privacy) response shape
    /// — a JSON blob as the escaped text content of an ordinary-looking
    /// `<Status>` tag, which `anonymize_xml`'s tag-*name* check alone never
    /// touches (`"status"` isn't in `XML_ANON_KEY_SUBSTRINGS`) and which
    /// isn't a JSON-*format* response either, so `anonymize_json` alone
    /// never got a chance to run on it — this leaked two real LAN IPs and a
    /// MAC address in a real capture before the content-shape check was
    /// added.
    #[test]
    fn anonymize_xml_scrubs_json_embedded_in_untagged_element() {
        let xml = "<Status>{ &quot;apcli0&quot;: &quot;10.1.1.76&quot;, \
                   &quot;ra0&quot;: &quot;10.10.10.254&quot;, \
                   &quot;MAC&quot;: &quot;00:22:6C:3C:EB:7E&quot; }</Status>";
        let out = anonymize_xml(xml, None);
        assert!(!out.contains("10.1.1.76"), "apcli0 IP should be scrubbed: {out}");
        assert!(!out.contains("10.10.10.254"), "ra0 IP should be scrubbed: {out}");
        assert!(!out.contains("00:22:6C:3C:EB:7E"), "MAC should be scrubbed: {out}");
    }

    /// Plain leaf content that happens to parse as a bare JSON scalar (not
    /// an object/array) must be left completely untouched, not rewritten —
    /// `anonymize_json_in_xml_text` only acts on object/array content.
    #[test]
    fn anonymize_xml_leaves_plain_numeric_content_untouched() {
        let xml = "<CurrentVolume>60</CurrentVolume>";
        assert_eq!(anonymize_xml(xml, None), xml);
    }

    /// Nested XML-in-XML (DIDL-Lite inside `TrackMetaData`) isn't JSON at
    /// all once unescaped — must fall through *content* unchanged when
    /// there's nothing to scrub, not get mangled by a failed JSON-parse
    /// attempt.
    #[test]
    fn anonymize_xml_leaves_nested_xml_content_untouched() {
        let xml = "<TrackMetaData>&lt;DIDL-Lite&gt;&lt;dc:title&gt;Foo&lt;/dc:title&gt;&lt;/DIDL-Lite&gt;</TrackMetaData>";
        assert_eq!(anonymize_xml(xml, None), xml);
    }

    /// Real leaked value from a live capture: a DIDL-Lite `<item>`'s
    /// `parentID` *attribute* embeds the device's own UUID
    /// (`wiim_uuid:FF98F7F4-...`) — invisible to every content-based check,
    /// since the whole opening tag (attributes included) is normally
    /// emitted verbatim before any of them run. The tag structure and
    /// non-identifying `id`/`restricted` attributes must survive exactly.
    #[test]
    fn anonymize_xml_scrubs_uuid_embedded_in_tag_attribute() {
        let xml = "<TrackMetaData>&lt;item id=&quot;542af8a17b9748e3b1b89cda0da4eaff&quot; \
                   restricted=&quot;true&quot; parentID=&quot;wiim_uuid:FF98F7F4-075B-5A90-FA95-72C3FF98F7F4&quot;&gt;\
                   &lt;dc:title&gt;Doralice&lt;/dc:title&gt;&lt;/item&gt;</TrackMetaData>";
        let out = anonymize_xml(xml, None);
        assert!(!out.contains("FF98F7F4-075B-5A90-FA95-72C3FF98F7F4"), "attribute UUID should be scrubbed: {out}");
        assert!(out.contains("542af8a17b9748e3b1b89cda0da4eaff"), "non-identifying id attribute should survive: {out}");
        assert!(out.contains("Doralice"), "track title should survive: {out}");
    }

    /// Real regression from a live `GetKeyMapping` capture (against an
    /// AudioCast with a real Tidal Mix preset configured): its `<Name>` tag
    /// holds the preset's own name ("My Mix 1_#~2026-07-08 16:49:59"), not a
    /// device identity — `XML_ANON_KEY_SUBSTRINGS` previously still had a
    /// bare `"name"` entry (unlike the JSON side, which already special-
    /// cased this for `preset_list[].name`), so this got fully character-
    /// scrubbed into "xx xxx x_#~xxxx-xx-xx xx:xx:xx". `ListName`/
    /// `PresetName`/`ShowName` (also seen in real `PlayQueue` responses) are
    /// the same story. Non-identifying content (`Source`) must also survive.
    #[test]
    fn anonymize_xml_does_not_scrub_playqueue_preset_name() {
        let xml = "<QueueContext>&lt;KeyList&gt;&lt;Key1&gt;\
                   &lt;Name&gt;My Mix 1_#~2026-07-08 16:49:59&lt;/Name&gt;\
                   &lt;Source&gt;Tidal&lt;/Source&gt;\
                   &lt;/Key1&gt;&lt;/KeyList&gt;</QueueContext>";
        let out = anonymize_xml(xml, None);
        assert!(out.contains("My Mix 1_#~2026-07-08 16:49:59"), "preset name must survive: {out}");
        assert!(out.contains("Tidal"), "source must survive: {out}");
    }

    /// Real shape from a live `GetPositionInfo`/`GetMediaInfo`/`GetInfoEx`
    /// capture: `TrackMetaData`'s escaped DIDL-Lite contains an
    /// `upnp:albumArtURI` pointing at a *separate* LAN device (a DLNA/Music-
    /// Assistant server, not the target device itself) — per explicit
    /// request, artwork URLs are deliberately never scrubbed unless they're
    /// the device's own address (see `anonymize_art_url()`), so this must
    /// survive untouched: capture files need to stay usable for testing
    /// real artwork downloads. `device_ip` absent (`None`) means "unknown,"
    /// which must behave the same as "known but different" — never scrub
    /// without positive confirmation it's the device's own IP.
    #[test]
    fn anonymize_xml_leaves_third_party_art_url_untouched() {
        let xml = "<TrackMetaData>&lt;DIDL-Lite&gt;&lt;dc:title&gt;Foo&lt;/dc:title&gt;\
                   &lt;upnp:albumArtURI&gt;http://10.1.1.10:8097/imageproxy/abc?size=512\
                   &lt;/upnp:albumArtURI&gt;&lt;/DIDL-Lite&gt;</TrackMetaData>";
        assert_eq!(anonymize_xml(xml, None), xml);
        assert_eq!(anonymize_xml(xml, Some("10.1.1.73")), xml);
    }

    /// Real case (a USB stick's cover art, served from the device's *own*
    /// embedded web server): when the artwork URL's IP matches the known
    /// `device_ip`, it's the device's real address like everywhere else, so
    /// it still gets scrubbed — the one exception to
    /// `anonymize_xml_leaves_third_party_art_url_untouched` above.
    #[test]
    fn anonymize_xml_scrubs_art_url_matching_device_ip() {
        let xml = "<TrackMetaData>&lt;DIDL-Lite&gt;&lt;dc:title&gt;Foo&lt;/dc:title&gt;\
                   &lt;upnp:albumArtURI&gt;https://10.1.1.73/data/lmp_cover_abc.jpeg\
                   &lt;/upnp:albumArtURI&gt;&lt;/DIDL-Lite&gt;</TrackMetaData>";
        let out = anonymize_xml(xml, Some("10.1.1.73"));
        assert!(!out.contains("10.1.1.73"), "device's own IP should be scrubbed: {out}");
        assert!(out.contains("Foo"), "non-identifying content should survive: {out}");
        assert!(out.contains("lmp_cover_abc.jpeg"), "rest of URL should survive: {out}");
    }
}
