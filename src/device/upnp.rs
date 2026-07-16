/// Low-level UPnP SOAP request/response plumbing — the UPnP analogue of
/// `api.rs`. Owns wire-shaped (not canonical) response types and the SOAP
/// calls that produce them; `state.rs` decides *when* to call it, exactly
/// like it already decides when to call `WiimClient` methods.
/// `device/playback.rs` owns turning these into canonical `PlaybackState`
/// fields, not this module.
///
/// Real WiiM firmware's `GetInfoEx` (`AVTransport`) bundles transport state,
/// timing, volume+mute, loop mode, source, and the full DIDL-Lite track
/// metadata in one action — confirmed against 5 real device captures
/// (`captures/test-sources/WiiM_Ultra_20260706_*.json`). That's why this module only
/// ever calls `GetInfoEx` for polling, not the separate `GetTransportInfo`/
/// `GetPositionInfo`/`GetVolume` a naive per-UPnP-service reading of the
/// spec would suggest.
///
/// One confirmed exception: `GetInfoEx` doesn't carry `CurrentMute` at all
/// on iEAST AudioCast (two real captures, muted and unmuted, both entirely
/// missing the tag — see `InfoEx::current_mute`'s doc comment), so
/// `RenderingControl.GetMute`/`SetMute` exist alongside `GetInfoEx` as a
/// per-poll supplementary read (`fetch_upnp_fast_poll()` in `state.rs`)
/// and the real write path for mute, rather than folding mute into the
/// "just call GetInfoEx" rule this module otherwise follows.

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use super::api::{build_reqwest_client, PresetEntry, PresetFetchOutcome, PresetKind, TlsMode};

pub static DEBUG_UPNP: AtomicBool = AtomicBool::new(false);
/// `--debug=upnp:verbose` (or `all:verbose`): include the full SOAP
/// envelope body in `soap_debug()`'s output. Without it, a SOAP call logs
/// just the control URL and action — enough to see call traffic/timing
/// without a wall of XML. Independent of `api.rs`'s own `DEBUG_VERBOSE` —
/// the two used to share one flag/format (`api::debug()`), but that made it
/// impossible to trace SOAP calls without also tracing every plain HTTP
/// `httpapi.asp` command, or vice versa.
pub static DEBUG_UPNP_VERBOSE: AtomicBool = AtomicBool::new(false);

/// Higher-level tracing specific to this module (discovery, control-URL
/// resolution) — gated on `--debug=upnp`, same flag `soap_debug()` below
/// uses for the raw SOAP request/response wire tracing, just always at
/// summary detail (these messages are already single-line, nothing to
/// strip for non-verbose mode).
fn dbg(msg: &str) {
    if DEBUG_UPNP.load(Ordering::Relaxed) {
        println!("{} [upnp] {msg}", super::timestamp());
    }
}

/// Raw SOAP request/response tracing for `soap_call()` — this module's own
/// counterpart to `api.rs`'s `debug()`, not a reuse of it (see
/// `DEBUG_UPNP_VERBOSE`'s doc comment for why they're independent now).
fn soap_debug(control_url: &str, action: &str, resp: &str) {
    if !DEBUG_UPNP.load(Ordering::Relaxed) {
        return;
    }
    if DEBUG_UPNP_VERBOSE.load(Ordering::Relaxed) {
        println!("{} [upnp] {control_url} {action} → {resp}", super::timestamp());
    } else {
        println!("{} [upnp] {control_url} {action}", super::timestamp());
    }
}

const REQUEST_TIMEOUT: Duration = Duration::from_secs(5);
const AV_TRANSPORT_SERVICE: &str = "urn:schemas-upnp-org:service:AVTransport:1";
/// LinkPlay-proprietary service backing `GetKeyMapping` (this module's one
/// use for it — see `get_key_mapping_presets()`) — the same service
/// `wiim-capture`'s `capture_playqueue()` already discovered independently.
const PLAY_QUEUE_SERVICE: &str = "urn:schemas-wiimu-com:service:PlayQueue:1";
/// Standard UPnP AV service backing `GetMute`/`SetMute` — see this module's
/// top doc comment for why it's used at all despite `GetInfoEx` otherwise
/// covering everything.
const RENDERING_CONTROL_SERVICE: &str = "urn:schemas-upnp-org:service:RenderingControl:1";
/// Candidate `description.xml` ports, same well-known LinkPlay UPnP ports
/// `wiim-capture.rs`'s `fetch_description()` already probes.
const DESCRIPTION_PORTS: &[u16] = &[49152, 59152];

/// Testing-only UPnP-address override (`--connect`'s optional second,
/// comma-separated UPnP URL — see `main.rs`'s `--connect` handling and
/// `wiim-simulator`, which binds a fixed, printed UPnP port rather than a
/// random one specifically so this can point at it). When set,
/// `UpnpClient::discover()` tries *only* this host:port instead of probing
/// the real device's two hardcoded ports — `--connect` already means
/// "point at exactly one known test target," not "prefer this, but also
/// try the real thing," so replacing normal probing entirely (rather than
/// trying this first with a fallback) is the right behavior here. Set
/// once, before `activate()` runs, same lifecycle as `api::TLS_MODE`/
/// `ui::DIRECT_CONNECT` — there is exactly one directly-connected device
/// per `--connect` invocation, so a single process-global override is
/// sufficient; no need to thread it through `DeviceState`/`DeviceManager`.
static UPNP_DISCOVER_OVERRIDE: std::sync::OnceLock<String> = std::sync::OnceLock::new();

pub fn set_discover_override(host_port: String) {
    let _ = UPNP_DISCOVER_OVERRIDE.set(host_port);
}

/// Splits `"host:port"` into its two parts — `port` defaults to `49152`
/// (the primary well-known UPnP port) if missing or unparseable, which
/// only matters for a malformed override value (a real caller always
/// supplies one, since `wiim-simulator`'s UPnP listener always has an
/// explicit port).
fn split_host_port(addr: &str) -> (&str, u16) {
    match addr.rsplit_once(':') {
        Some((host, port_str)) => (host, port_str.parse().unwrap_or(DESCRIPTION_PORTS[0])),
        None => (addr, DESCRIPTION_PORTS[0]),
    }
}

/// Everything `GetInfoEx` returns, wire-shaped (not canonical — see
/// `device/playback.rs`'s `decode_*_upnp` functions for the canonical
/// translation).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct InfoEx {
    pub transport_state: String,
    /// `"HH:MM:SS"` wire format, decoded by `playback::decode_hms_duration`.
    pub rel_time:        String,
    /// `"HH:MM:SS"` wire format, decoded by `playback::decode_hms_duration`.
    pub track_duration:  String,
    pub current_volume:  u32,
    /// `None` when `<CurrentMute>` is absent from the response entirely —
    /// confirmed on iEAST AudioCast (two real captures, muted and unmuted,
    /// both entirely missing the tag), not merely "false". Callers (see
    /// `state.rs`'s `fetch_upnp_fast_poll()`) treat `None` as "ask
    /// `RenderingControl.GetMute` instead", not as "unmuted".
    pub current_mute:    Option<bool>,
    pub loop_mode:       i32,
    /// Confirmed byte-for-byte identical to HTTP `getPlayerStatusEx`'s
    /// `mode` across 18 real captures spanning 3 device families, zero
    /// exceptions — the same raw, uncorrected wire value, not something
    /// independently derived. Defaults to `-1` (never a real device value)
    /// when the tag is absent, matching `Inner::current_mode`'s own
    /// "never seen a mode yet" sentinel.
    pub play_type:       i32,
    pub play_medium:     String,
    pub track_source:    String,
    pub title:           String,
    pub artist:          String,
    pub album:           String,
    pub album_art_uri:   Option<String>,
    /// `None` when `<song:actualQuality>` is absent from the DIDL-Lite
    /// entirely; `Some("")` when present but empty. This distinction is the
    /// entire codec-badge rule (see `playback::decode_quality_upnp`) — don't
    /// collapse it to a plain `String`/`.unwrap_or_default()`.
    pub actual_quality:  Option<String>,
    pub bitrate:         String,
    /// Bit depth (DIDL-Lite's `song:format_s`).
    pub format_s:        String,
    pub rate_hz:         String,
    /// The `<res protocolInfo="...">` attribute, e.g.
    /// `"http-get:*:audio/mpeg:DLNA.ORG_PN=MP3;DLNA.ORG_OP=01;"`.
    pub protocol_info:   Option<String>,
    /// Parsed `song:guibehavior` JSON blob, when the DIDL-Lite item carries
    /// one — per-action `"enabled"` flags the device reports directly for
    /// the *current track*, confirmed (Spotify Connect, free vs. premium
    /// account, otherwise-identical session) to react to things no static
    /// `play_medium`/`track_source` rule ever could. `None` when the tag
    /// is absent entirely, which is common — confirmed absent even for
    /// some genuinely non-skippable sources (TuneIn/BBC Radio) — so
    /// `playback::decode_transport_caps_upnp` still needs its static
    /// fallback for that case, not just for services never granted a tag
    /// at all.
    pub gui_behavior:    Option<GuiBehavior>,
}

/// One `song:guibehavior` JSON blob's `next`/`prev` flags, already
/// resolved to a definite bool — a key genuinely missing from the JSON
/// (confirmed on a real Pandora2 capture, which omits `"next"` entirely
/// while explicitly listing `prev`/`loop`/`queue`) means "not restricted"
/// (`true`), not "unknown"; that resolution happens here, at parse time,
/// rather than leaking `Option<bool>` per-action ambiguity out to callers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GuiBehavior {
    pub next: bool,
    pub prev: bool,
}

/// UPnP control-point client for one device's `AVTransport` (and, when
/// advertised, `PlayQueue`/`RenderingControl`) service. Discovered once via
/// `description.xml` (`UpnpClient::discover`) and reused for every
/// subsequent `get_info_ex()`/`get_key_mapping_presets()`/`get_mute()`/
/// `set_mute()` poll.
#[derive(Debug, Clone)]
pub struct UpnpClient {
    control_url: String,
    /// `PlayQueue` service's control URL, when the device advertises it —
    /// `None` for devices that don't, in which case
    /// `get_key_mapping_presets()` always reports `PresetFetchOutcome::Unsupported`.
    play_queue_control_url: Option<String>,
    /// `RenderingControl` service's control URL, when the device advertises
    /// it — `None` for devices that don't, in which case `get_mute()`/
    /// `set_mute()` both report an error rather than guessing.
    rendering_control_url: Option<String>,
}

impl UpnpClient {
    /// Probe `description.xml` at the well-known LinkPlay UPnP ports/schemes,
    /// parse the `AVTransport` service block's `controlURL`, and resolve it
    /// against whichever candidate actually answered. `ip` may already
    /// include an embedded `:port` (the `--connect`/simulator testing
    /// convention used elsewhere) — only the host part is used here since
    /// UPnP's own port is independent of the main HTTP API's (real hardware:
    /// HTTPS on 443 for the API, plain HTTP on 49152 for UPnP). Unless
    /// `set_discover_override()` has been called (`--connect`'s optional
    /// second, comma-separated UPnP URL), in which case `ip` is ignored
    /// entirely and only the override host:port is tried — see that
    /// function's doc comment for why this replaces normal probing rather
    /// than being tried first with a fallback.
    pub async fn discover(ip: &str) -> anyhow::Result<Self> {
        let (body, url) = discover_description(ip).await?;
        match extract_url_for_service(&body, &url, ":service:AVTransport:", "controlURL") {
            Some(control_url) => {
                dbg(&format!("AVTransport control URL: {control_url}"));
                let play_queue_control_url =
                    extract_url_for_service(&body, &url, "wiimu-com:service:PlayQueue", "controlURL");
                dbg(&format!("PlayQueue control URL: {play_queue_control_url:?}"));
                let rendering_control_url =
                    extract_url_for_service(&body, &url, ":service:RenderingControl:", "controlURL");
                dbg(&format!("RenderingControl control URL: {rendering_control_url:?}"));
                Ok(Self { control_url, play_queue_control_url, rendering_control_url })
            }
            None => Err(anyhow::anyhow!("description.xml at {url} has no AVTransport service")),
        }
    }

    pub async fn get_info_ex(&self) -> anyhow::Result<InfoEx> {
        let body = soap_call(&self.control_url, AV_TRANSPORT_SERVICE, "GetInfoEx", "<InstanceID>0</InstanceID>").await?;
        parse_info_ex_response(&body)
    }

    /// Fetches `GetKeyMapping` (no arguments — confirmed against Arylic's
    /// own `upnp_hack` reference scripts) and resolves it into the same
    /// `PresetFetchOutcome` shape `WiimClient::fetch_presets()` uses, so
    /// `state.rs` can treat this and the HTTP path identically. Reports
    /// `Unsupported` if the device never advertised a `PlayQueue` service
    /// at all (`discover()` found no control URL for it, a confirmed/final
    /// fact about this device, not a transient one) — matching how a
    /// genuine "unknown command" HTTP response is reported. A SOAP-call
    /// failure (connection error, timeout) is `Failed` instead, not
    /// `Unsupported` — it says nothing about whether the device actually
    /// supports this action, so `state.rs` retries it a bounded number of
    /// times before giving up the same way.
    pub async fn get_key_mapping_presets(&self, old_fp: &str) -> PresetFetchOutcome {
        let Some(control_url) = &self.play_queue_control_url else {
            return PresetFetchOutcome::Unsupported;
        };
        match soap_call(control_url, PLAY_QUEUE_SERVICE, "GetKeyMapping", "").await {
            Ok(body) => parse_key_mapping_presets(&body, old_fp),
            Err(_) => PresetFetchOutcome::Failed,
        }
    }

    /// `RenderingControl.GetMute`, `Channel="Master"` (matches the
    /// LinkPlay-maintained `wiim` SDK's own argument shape). Errors rather
    /// than guessing if the device never advertised `RenderingControl` at
    /// all, or if a response somehow lacks `<CurrentMute>` — callers decide
    /// what "unknown" means (e.g. `fetch_upnp_fast_poll()` just leaves
    /// `InfoEx::current_mute` as `None` on either failure).
    pub async fn get_mute(&self) -> anyhow::Result<bool> {
        let Some(control_url) = &self.rendering_control_url else {
            anyhow::bail!("device has no RenderingControl service");
        };
        let body = soap_call(
            control_url, RENDERING_CONTROL_SERVICE, "GetMute",
            "<InstanceID>0</InstanceID><Channel>Master</Channel>",
        ).await?;
        extract_tag(&body, "CurrentMute")
            .map(|s| s == "1")
            .ok_or_else(|| anyhow::anyhow!("GetMute response has no CurrentMute tag"))
    }

    /// `RenderingControl.SetMute`, `Channel="Master", DesiredMute=...` —
    /// same argument shape the `wiim` SDK uses. The real write path for
    /// mute on devices where `AccessMethod` resolves mute to `UpnpPolled`
    /// (see `DeviceState::do_set_mute()`); HTTP `setPlayerCmd:mute:n`
    /// remains the only path for volume, matching that same SDK precedent
    /// of not moving every write to UPnP uniformly.
    pub async fn set_mute(&self, mute: bool) -> anyhow::Result<()> {
        let Some(control_url) = &self.rendering_control_url else {
            anyhow::bail!("device has no RenderingControl service");
        };
        let desired = if mute { "1" } else { "0" };
        soap_call(
            control_url, RENDERING_CONTROL_SERVICE, "SetMute",
            &format!("<InstanceID>0</InstanceID><Channel>Master</Channel><DesiredMute>{desired}</DesiredMute>"),
        ).await?;
        Ok(())
    }

    /// `PlayQueue.SetQueueLoopMode`, single `<LoopMode>` argument — same 0-5
    /// wire encoding as HTTP `setPlayerCmd:loopmode:N`
    /// (`playback::encode_loop_mode`/`decode_loop_mode_http`), confirmed by
    /// capturing the real WiiM phone app's own SOAP request. The real write
    /// path for loop mode on devices where `AccessMethod` resolves
    /// `loop_mode_access` to `UpnpPolled` (see `DeviceState::do_set_loop_mode()`)
    /// — added because HTTP's `setPlayerCmd:loopmode:5` (shuffle + repeat
    /// one) is silently ignored on at least the WiiM Mini, confirmed working
    /// on WiiM Ultra and the Audio Pro Addon C5, so not a universal WiiM-HTTP
    /// bug, but real enough on real hardware that the UPnP path (which the
    /// vendor's own app already relies on) is the safer default.
    pub async fn set_queue_loop_mode(&self, mode: i32) -> anyhow::Result<()> {
        let Some(control_url) = &self.play_queue_control_url else {
            anyhow::bail!("device has no PlayQueue service");
        };
        soap_call(
            control_url, PLAY_QUEUE_SERVICE, "SetQueueLoopMode",
            &format!("<LoopMode>{mode}</LoopMode>"),
        ).await?;
        Ok(())
    }
}

/// Probes `description.xml` at the well-known LinkPlay UPnP ports/schemes
/// (or the `--connect` override, same as `UpnpClient::discover()` uses),
/// returning the raw body and the URL that answered. `pub(crate)` so
/// `device/gena.rs` can reuse the exact same probing this module's own
/// `UpnpClient::discover()` is built on, rather than duplicating it, since
/// GENA needs `eventSubURL`s from the same document `discover()` already
/// fetches for `controlURL`s.
pub(crate) async fn discover_description(ip: &str) -> anyhow::Result<(String, String)> {
    let (host, ports): (&str, Vec<u16>) = match UPNP_DISCOVER_OVERRIDE.get() {
        Some(addr) => {
            let (host, port) = split_host_port(addr);
            (host, vec![port])
        }
        None => (ip.split(':').next().unwrap_or(ip), DESCRIPTION_PORTS.to_vec()),
    };
    let mut last_err: Option<anyhow::Error> = None;
    for scheme in ["http", "https"] {
        for &port in &ports {
            let url = format!("{scheme}://{host}:{port}/description.xml");
            dbg(&format!("trying description.xml at {url}"));
            let client = build_reqwest_client(tls_for_scheme(scheme), REQUEST_TIMEOUT);
            let resp = match client.get(&url).send().await {
                Ok(r) => r,
                Err(e) => { last_err = Some(e.into()); continue; }
            };
            if !resp.status().is_success() {
                last_err = Some(anyhow::anyhow!("{url}: HTTP {}", resp.status()));
                continue;
            }
            match resp.text().await {
                Ok(body) => return Ok((body, url)),
                Err(e) => last_err = Some(e.into()),
            }
        }
    }
    Err(last_err.unwrap_or_else(|| anyhow::anyhow!("no description.xml candidate answered")))
}

fn tls_for_scheme(scheme: &str) -> TlsMode {
    if scheme == "https" { TlsMode::HttpsAny } else { TlsMode::Http }
}

/// `[upnp]`/`--debug=upnp` request/response tracing via this module's own
/// `soap_debug()` — used to reuse `api.rs`'s `debug()`/`--debug=api`
/// directly (still fundamentally an API call, just over SOAP instead of a
/// plain GET), but that made the two flags impossible to use
/// independently; `log_request_error()` is the one piece still actually
/// shared with `api.rs` (same logic, tagged `"upnp"` here).
///
/// Retry loop mirrors `cmd()`'s exactly (`api.rs:711-738`): up to
/// `MAX_RETRIES` retries with a 100ms backoff, only for
/// `reqwest::Error::is_request()` failures ("connection closed before
/// message completed" — a known pooled-keep-alive-connection race, not a
/// real fault). Logging follows the
/// same noise rule `cmd()` uses too: the first attempt's transient
/// failure only logs under `--debug=upnp` (routine, self-healing — that's
/// the entire point of retrying), but a first *retry* that also fails
/// logs unconditionally (more likely a real problem).
async fn soap_call(control_url: &str, service_type: &str, action: &str, args_xml: &str) -> anyhow::Result<String> {
    const MAX_RETRIES: u32 = 3;
    let scheme = control_url.split(':').next().unwrap_or("http");
    let client = build_reqwest_client(tls_for_scheme(scheme), REQUEST_TIMEOUT);
    let body = format!(
        "<?xml version=\"1.0\" encoding=\"utf-8\"?>\r\n\
         <s:Envelope xmlns:s=\"http://schemas.xmlsoap.org/soap/envelope/\" \
         s:encodingStyle=\"http://schemas.xmlsoap.org/soap/encoding/\">\
         <s:Body><u:{action} xmlns:u=\"{service_type}\">{args_xml}</u:{action}></s:Body></s:Envelope>"
    );
    let soap_action_header = format!("\"{service_type}#{action}\"");

    for attempt in 0..=MAX_RETRIES {
        if attempt > 0 {
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        let err = match client
            .post(control_url)
            .header("Content-Type", "text/xml; charset=\"utf-8\"")
            .header("SOAPACTION", &soap_action_header)
            .body(body.clone())
            .send()
            .await
        {
            Ok(resp) => {
                if !resp.status().is_success() {
                    anyhow::bail!("{action}: HTTP {}", resp.status());
                }
                let text = resp.text().await?;
                soap_debug(control_url, action, &text);
                return Ok(text);
            }
            Err(e) => e,
        };
        if !err.is_request() || attempt == MAX_RETRIES {
            super::api::log_request_error("upnp", action, &err);
            return Err(err.into());
        }
        if attempt > 0 || DEBUG_UPNP.load(Ordering::Relaxed) {
            eprintln!(
                "{} [upnp] {action}: transient send error (attempt {}/{}), retrying in 100ms: {err}",
                super::timestamp(), attempt + 1, MAX_RETRIES,
            );
        }
    }
    unreachable!()
}

// ── description.xml / SOAP response parsing ──────────────────────────────────
//
// Minimal, non-namespace-aware `<tag>...</tag>`/attribute extraction —
// sufficient for the well-known tags LinkPlay's UPnP responses actually use.
// Ported from (not shared with) `wiim-capture.rs`'s identical helpers: that's
// a separate diagnostic binary crate that can't depend on this library
// crate's internals, and this module is the intended runtime-plumbing home
// for the same logic.

/// Extracts `<tag>...</tag>`'s content. `None` means the tag is entirely
/// absent — a present-but-empty tag (`<tag></tag>`) still returns `Some("")`.
/// Callers that care about that distinction (`actual_quality`) must not
/// collapse it with `.unwrap_or_default()`.
pub(crate) fn extract_tag(xml: &str, tag: &str) -> Option<String> {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    let start = xml.find(&open)? + open.len();
    let end = xml[start..].find(&close)? + start;
    Some(xml[start..end].trim().to_string())
}

pub(crate) fn extract_attr(tag_text: &str, attr: &str) -> Option<String> {
    let needle = format!("{attr}=\"");
    let start = tag_text.find(&needle)? + needle.len();
    let end = tag_text[start..].find('"')? + start;
    Some(tag_text[start..end].to_string())
}

/// Extracts the `<res protocolInfo="...">` attribute from a DIDL-Lite item.
fn extract_res_protocol_info(didl: &str) -> Option<String> {
    let start = didl.find("<res ")?;
    let tag_end = didl[start..].find('>').map(|i| start + i)?;
    extract_attr(&didl[start..tag_end], "protocolInfo")
}

pub(crate) fn extract_service_blocks(xml: &str) -> Vec<String> {
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
/// (`scheme://host:port`) of the `description.xml` URL it came from.
pub(crate) fn resolve_url(description_url: &str, maybe_relative: &str) -> String {
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

/// Finds `url_tag`'s value (`controlURL` or `eventSubURL`) of the first
/// advertised service whose `serviceType` contains `service_type_substr`,
/// resolved against `description_url`'s origin. `pub(crate)` so
/// `device/gena.rs` can reuse this for `eventSubURL` discovery rather than
/// hand-rolling a third copy of this parsing (`wiim-capture.rs`'s own copy
/// is a separate diagnostic binary crate that can't depend on this library
/// crate's internals, so that one stays independent). Shared here by
/// AVTransport (required — `discover()` fails outright if absent) and
/// PlayQueue (optional — presets simply aren't available via UPnP if a
/// device doesn't advertise it).
pub(crate) fn extract_url_for_service(description_xml: &str, description_url: &str, service_type_substr: &str, url_tag: &str) -> Option<String> {
    for block in extract_service_blocks(description_xml) {
        let Some(service_type) = extract_tag(&block, "serviceType") else { continue };
        if !service_type.contains(service_type_substr) { continue; }
        let url_raw = extract_tag(&block, url_tag)?;
        return Some(resolve_url(description_url, &url_raw));
    }
    None
}

/// Unescapes the handful of entities LinkPlay's XML actually uses.
/// `&amp;` is replaced last so a literal `&amp;lt;` in the source (an
/// escaped ampersand followed by plain text "lt;") doesn't get
/// double-unescaped into `<`.
pub(crate) fn unescape_xml_entities(s: &str) -> String {
    s.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&amp;", "&")
}

/// Strips LinkPlay's internal `_#~<timestamp>` uniquifying suffix off a
/// `GetKeyMapping`/`BrowseQueue` `<Name>` value (e.g. `"My Mix 1_#~2026-
/// 07-08 16:49:59"` → `"My Mix 1"`) — confirmed present on every named
/// entry in two real captures from different device families
/// (`AudioCastBu_20260708_095957.json`'s `GetKeyMapping`/`BrowseQueue`,
/// `WiiM_Ultra_20260708_100034.json`'s equivalents), so this is the
/// device's own internal disambiguator (likely so re-saving a preset under
/// an unchanged display name still gets a distinct identity), not something
/// meant to be shown. `"_#~"` itself never legitimately appears in a real
/// display name in any capture seen so far, so a plain substring split is
/// enough — no need to anchor it to a stricter timestamp-shaped pattern.
fn strip_wiimu_name_suffix(name: &str) -> String {
    match name.find("_#~") {
        Some(idx) => name[..idx].to_string(),
        None => name.to_string(),
    }
}

/// Parses a `GetKeyMappingResponse` SOAP envelope into the same
/// `PresetFetchOutcome` shape the HTTP `getPresetInfo` path uses. The real
/// preset data sits inside `<QueueContext>`, escaped once (LinkPlay's own
/// XML nested inside the outer SOAP XML) — confirmed against real
/// `GetKeyMapping` captures (`captures/test-devices/AudioCastBu_20260708_095957.json`,
/// `WiiM_Ultra_20260708_100034.json`): `<KeyList><Key1><Name>.../Name>
/// <Source>...</Source><PicUrl>...</PicUrl></Key1>...</KeyList>`. `Key0` is
/// always present-but-empty in both captures and never has a meaning here
/// (WiiM has no concept of a "preset 0"), which the `1..=12` range already
/// excludes without special-casing. A genuinely unconfigured slot has an
/// entirely empty `<KeyN></KeyN>` (confirmed on a real AudioCast unit with
/// only 2 of its slots configured — every other `KeyN` tag is empty, not
/// just missing a `<Name>`) — that's treated the same as `getPresetInfo`
/// simply not reporting a nonexistent slot: dropped from `entries`
/// entirely, not turned into a `PresetKind::Empty` placeholder (which would
/// otherwise show as a visible button with a generic icon and no name for
/// every unused slot). Capped at slots 1–12 (same range `PresetEntry`/the
/// UI's fixed preset panel already use for HTTP presets) even though
/// `MaxNumber` can be much larger (33 in both captures) — there's no UI
/// concept for more than 12 yet, and no evidence `KeyN` numbering beyond
/// that even corresponds to a playable slot at all — how a
/// `GetKeyMapping`-sourced preset beyond slot 12 would actually be
/// triggered is still an open question; this only covers *listing* them.
/// `Url`/`Metadata` (present
/// for direct-URL presets like a TuneIn station, absent for a
/// streaming-service Mix reference) aren't decoded here for the same
/// reason — nothing yet needs them.
///
/// Fingerprint computed from the raw extracted `(slot, name, picurl)`
/// triples *before* building any `PresetEntry` (and before filtering out
/// empty slots), same discipline `WiimClient::fetch_presets()` uses — an
/// unchanged list is detected without ever constructing the entries it
/// would've produced.
fn parse_key_mapping_presets(envelope: &str, old_fp: &str) -> PresetFetchOutcome {
    let Some(queue_context) = extract_tag(envelope, "QueueContext") else {
        return PresetFetchOutcome::Unsupported;
    };
    let key_list = unescape_xml_entities(&queue_context);

    // `content` is `<KeyN>`'s full inner text, kept around (not just `name`)
    // so an entirely-empty tag (`<Key3></Key3>`) can be told apart from one
    // that has other fields but happens to lack `<Name>`.
    let slots: Vec<(usize, String, String, String)> = (1..=12usize)
        .map(|n| {
            let content = extract_tag(&key_list, &format!("Key{n}")).unwrap_or_default();
            let name = extract_tag(&content, "Name")
                .filter(|s| !s.is_empty())
                .map(|s| strip_wiimu_name_suffix(&s))
                .unwrap_or_default();
            let pic_url = extract_tag(&content, "PicUrl").unwrap_or_default();
            (n, content, name, pic_url)
        })
        .collect();

    let fp = {
        let mut parts: Vec<String> = slots.iter()
            .map(|(slot, _content, name, pic_url)| format!("{slot}:{name}:{pic_url}"))
            .collect();
        parts.sort();
        parts.join("|")
    };
    if fp == old_fp { return PresetFetchOutcome::Unchanged; }

    let entries = slots.into_iter()
        .filter(|(_, content, _, _)| !content.trim().is_empty())
        .map(|(slot, _content, name, picurl)| {
            let kind = if name.is_empty() { PresetKind::Empty } else { PresetKind::Media };
            PresetEntry { slot, name, kind, art_bytes: Vec::new(), picurl }
        })
        .collect();
    PresetFetchOutcome::Changed(fp, entries)
}

/// One DIDL-Lite `<item>`'s fields, parsed the same way regardless of which
/// wire wrapper embedded it — `GetInfoEx`'s `TrackMetaData` element text
/// (`parse_info_ex_response`, below) and GENA `AVTransport` NOTIFY's
/// `CurrentTrackMetaData` attribute value (`gena::parse_av_transport_event`)
/// carry the exact same DIDL-Lite item, confirmed live on both a WiiM Ultra
/// and an Audio Pro Addon C5 (title/artist/album/`upnp:albumArtURI`/
/// `song:bitrate`/`song:format_s`/`song:rate_hz` all present in a real
/// NOTIFY's `CurrentTrackMetaData`, identical tag names to `GetInfoEx`'s).
/// `didl` must already be real DIDL-Lite XML (tag boundaries are real
/// `<tag>`s) — the caller's own unescape pass(es) to get there differ by
/// embedding mechanism, but the leaf-field extraction from that point on is
/// identical.
pub(crate) struct DidlItem {
    pub title:         String,
    pub artist:        String,
    pub album:         String,
    pub album_art_uri: Option<String>,
    /// `None` = tag absent, `Some("")` = present-but-empty — see
    /// `InfoEx::actual_quality`'s doc comment.
    pub actual_quality: Option<String>,
    pub bitrate:        String,
    pub format_s:       String,
    pub rate_hz:        String,
    pub protocol_info:  Option<String>,
    pub gui_behavior:   Option<GuiBehavior>,
}

pub(crate) fn parse_didl_item(didl: &str) -> DidlItem {
    DidlItem {
        title:  extract_tag(didl, "dc:title").map(|s| unescape_xml_entities(&s)).unwrap_or_default(),
        artist: extract_tag(didl, "upnp:artist").map(|s| unescape_xml_entities(&s)).unwrap_or_default(),
        album:  extract_tag(didl, "upnp:album").map(|s| unescape_xml_entities(&s)).unwrap_or_default(),
        album_art_uri:  extract_tag(didl, "upnp:albumArtURI"),
        actual_quality: extract_tag(didl, "song:actualQuality"),
        bitrate:        extract_tag(didl, "song:bitrate").unwrap_or_default(),
        format_s:       extract_tag(didl, "song:format_s").unwrap_or_default(),
        rate_hz:        extract_tag(didl, "song:rate_hz").unwrap_or_default(),
        protocol_info:  extract_res_protocol_info(didl),
        gui_behavior: extract_tag(didl, "song:guibehavior")
            .map(|s| unescape_xml_entities(&s)) // double-escaped, same as title/artist/album above
            .and_then(|s| parse_gui_behavior(&s)),
    }
}

/// Parses a `GetInfoExResponse` SOAP envelope. `TrackMetaData` is XML text
/// escaped *twice* on the wire: once by DIDL-Lite's own XML serialization
/// (a literal `&` in a title becomes `&amp;`), then again because the whole
/// DIDL-Lite document is embedded as escaped text inside the outer SOAP XML
/// (`<` becomes `&lt;`, and the already-escaped `&amp;` becomes `&amp;amp;`).
/// One `unescape_xml_entities` pass recovers real DIDL-Lite XML, handed to
/// `parse_didl_item` for the rest.
fn parse_info_ex_response(envelope: &str) -> anyhow::Result<InfoEx> {
    let transport_state = extract_tag(envelope, "CurrentTransportState").unwrap_or_default();
    let rel_time        = extract_tag(envelope, "RelTime").unwrap_or_default();
    let track_duration  = extract_tag(envelope, "TrackDuration").unwrap_or_default();
    let current_volume  = extract_tag(envelope, "CurrentVolume").and_then(|s| s.parse().ok()).unwrap_or(0);
    let current_mute    = extract_tag(envelope, "CurrentMute").map(|s| s == "1");
    let loop_mode        = extract_tag(envelope, "LoopMode").and_then(|s| s.parse().ok()).unwrap_or(-1);
    let play_type        = extract_tag(envelope, "PlayType").and_then(|s| s.parse().ok()).unwrap_or(-1);
    let play_medium      = extract_tag(envelope, "PlayMedium").unwrap_or_default();
    let track_source     = extract_tag(envelope, "TrackSource").unwrap_or_default();

    let track_metadata_raw = extract_tag(envelope, "TrackMetaData").unwrap_or_default();
    let didl = unescape_xml_entities(&track_metadata_raw);
    let item = parse_didl_item(&didl);

    Ok(InfoEx {
        transport_state, rel_time, track_duration, current_volume, current_mute,
        loop_mode, play_type, play_medium, track_source,
        title: item.title, artist: item.artist, album: item.album, album_art_uri: item.album_art_uri,
        actual_quality: item.actual_quality, bitrate: item.bitrate, format_s: item.format_s,
        rate_hz: item.rate_hz, protocol_info: item.protocol_info, gui_behavior: item.gui_behavior,
    })
}

/// A key present in the JSON but with `"enabled": false` means restricted;
/// a key missing from the JSON entirely means not restricted — see
/// `GuiBehavior`'s doc comment. Malformed/unparseable JSON (a tag present
/// but not actually valid JSON, or a bare object without the expected
/// shape) degrades to `None`, same as the tag being absent — this is a
/// best-effort supplementary signal, not something worth erroring over.
fn parse_gui_behavior(s: &str) -> Option<GuiBehavior> {
    let v: serde_json::Value = serde_json::from_str(s).ok()?;
    let enabled = |action: &str| v.get(action)?.get("enabled")?.as_bool();
    Some(GuiBehavior {
        next: enabled("next").unwrap_or(true),
        prev: enabled("prev").unwrap_or(true),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capture::format::CaptureFile;

    // These tests parse real `wiim-capture` capture files
    // (`captures/test-sources/WiiM_Ultra_20260706_*.json` — same-device
    // captures that vary by active source) rather than embedding SOAP
    // bodies as string literals in the source — same file format
    // `wiim-capdump` already reads (`CaptureFile`/`UpnpCapture`/
    // `UpnpActionCapture`/`Blob` in `src/capture/format.rs`), so no bespoke
    // parsing here. This is what proved the codec-badge rule that's
    // driving `decode_quality_upnp` — these tests are now that
    // investigation's regression coverage.

    fn load_capture(filename: &str) -> CaptureFile {
        let path = format!("{}/captures/test-sources/{filename}", env!("CARGO_MANIFEST_DIR"));
        let text = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("reading fixture {path}: {e}"));
        serde_json::from_str(&text)
            .unwrap_or_else(|e| panic!("parsing fixture {path}: {e}"))
    }

    fn blob_text(blob: &crate::capture::format::Blob) -> &str {
        blob.body.as_str().expect("blob body is not a string")
    }

    fn get_info_ex_body(cap: &CaptureFile) -> String {
        let upnp = cap.upnp.as_ref().expect("capture has no upnp section");
        let action = upnp.actions.iter().find(|a| a.action == "GetInfoEx")
            .expect("capture has no GetInfoEx action");
        blob_text(action.response.as_ref().expect("GetInfoEx has no response")).to_string()
    }

    /// `captures/test-devices/*` fixtures, unlike `load_capture()`'s
    /// `captures/test-sources/*` — same split `capabilities.rs`'s own tests
    /// use.
    fn load_device_capture(filename: &str) -> CaptureFile {
        let path = format!("{}/captures/test-devices/{filename}", env!("CARGO_MANIFEST_DIR"));
        let text = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("reading fixture {path}: {e}"));
        serde_json::from_str(&text)
            .unwrap_or_else(|e| panic!("parsing fixture {path}: {e}"))
    }

    fn get_key_mapping_body(cap: &CaptureFile) -> String {
        let upnp = cap.upnp.as_ref().expect("capture has no upnp section");
        let action = upnp.actions.iter().find(|a| a.action == "GetKeyMapping")
            .expect("capture has no GetKeyMapping action");
        blob_text(action.response.as_ref().expect("GetKeyMapping has no response")).to_string()
    }

    #[test]
    fn control_url_discovery_from_real_description_xml() {
        let cap = load_capture("WiiM_Ultra_20260706_075156.TidalConnect-FLAC.json");
        let upnp = cap.upnp.as_ref().unwrap();
        let description_url = upnp.description_url.as_deref().unwrap();
        let description_xml = blob_text(upnp.description.as_ref().unwrap());
        let url = extract_url_for_service(description_xml, description_url, ":service:AVTransport:", "controlURL").unwrap();
        assert_eq!(url, "http://xx.x.x.xx:49152/upnp/control/rendertransport1");
    }

    /// Real Audio Pro Addon C5 unit (old firmware, `state.rs`'s
    /// `process_poll_upnp()` has a `CurrentTransportState` fallback for
    /// exactly this device) — confirms `<PlayType>` is genuinely absent
    /// from a real `GetInfoEx` response (`play_type` decodes to `-1`, the
    /// "tag missing" sentinel — see `InfoEx::play_type`'s doc comment)
    /// while a full, correct DIDL-Lite (title/artist/album/art) is present
    /// regardless. This is the capture that showed HTTP's `getPlayerStatusEx`
    /// has none of this for Spotify Connect (`Title`/`Artist`/`Album` are
    /// all just `"Unknown"` there) — UPnP is the only usable source on this
    /// device, once the `PlayType`-absence is worked around.
    #[test]
    fn audio_pro_addon_c5_spotify_capture_has_no_play_type_but_full_metadata_and_art() {
        let cap = load_device_capture("AudioPro_C5I_20260710_102122.spotify.json");
        let info = parse_info_ex_response(&get_info_ex_body(&cap)).unwrap();
        assert_eq!(info.play_type, -1, "PlayType tag confirmed absent on this device");
        assert_eq!(info.transport_state, "PLAYING");
        assert_eq!(info.title, "Take Five");
        assert_eq!(info.artist, "The Dave Brubeck Quartet");
        assert_eq!(info.album, "Time Out");
        assert_eq!(
            info.album_art_uri.as_deref(),
            Some("http://i.scdn.co/image/ab67616d0000b273b6bd44cf06bf8f4d5ce1e080"),
        );
        assert_eq!(info.play_medium, "SPOTIFY");
    }

    #[test]
    fn flac_case_has_hi_res_lossless_actual_quality() {
        let cap = load_capture("WiiM_Ultra_20260706_075156.TidalConnect-FLAC.json");
        let info = parse_info_ex_response(&get_info_ex_body(&cap)).unwrap();
        assert_eq!(info.actual_quality.as_deref(), Some("HI_RES_LOSSLESS"));
        assert_eq!(info.title, "Superstar");
        assert_eq!(info.artist, "Diana Krall");
        assert_eq!(info.album, "Wallflower (Deluxe Edition)");
        assert_eq!(info.bitrate, "1571");
        assert_eq!(info.format_s, "24");
        assert_eq!(info.rate_hz, "48000");
        assert_eq!(info.play_medium, "TIDAL_CONNECT");
        assert_eq!(info.current_volume, 17);
        assert_eq!(info.current_mute, Some(false));
        assert_eq!(info.rel_time, "00:00:45");
        assert_eq!(info.track_duration, "00:04:17");
    }

    fn get_action_body(cap: &CaptureFile, action: &str) -> String {
        let upnp = cap.upnp.as_ref().expect("capture has no upnp section");
        let a = upnp.actions.iter().find(|a| a.action == action)
            .unwrap_or_else(|| panic!("capture has no {action} action"));
        blob_text(a.response.as_ref().unwrap_or_else(|| panic!("{action} has no response"))).to_string()
    }

    /// Two real captures from the same iEAST AudioCast unit (one muted, one
    /// not, same song) — confirms `GetInfoEx` never carries `CurrentMute`
    /// on this device at all, in either state, not just that it's wrong.
    /// This is the gap `fetch_upnp_fast_poll()`'s supplementary
    /// `RenderingControl.GetMute` call (tested below) exists to fill.
    #[test]
    fn audiocast_get_info_ex_never_has_current_mute() {
        for f in [
            "iEAST_AudioCast_20260709_053917.unmuted.json",
            "iEAST_AudioCast_20260709_054053.muted.json",
        ] {
            let cap = load_capture(f);
            let info = parse_info_ex_response(&get_info_ex_body(&cap)).unwrap();
            assert_eq!(info.current_mute, None, "{f}: expected CurrentMute tag to be absent from GetInfoEx");
        }
    }

    /// Same two captures — `RenderingControl.GetMute` correctly reports
    /// `CurrentMute` on this device even though `GetInfoEx` doesn't.
    #[test]
    fn audiocast_rendering_control_get_mute_reports_correctly() {
        let unmuted = load_capture("iEAST_AudioCast_20260709_053917.unmuted.json");
        let body = get_action_body(&unmuted, "GetMute");
        assert_eq!(extract_tag(&body, "CurrentMute").as_deref(), Some("0"));

        let muted = load_capture("iEAST_AudioCast_20260709_054053.muted.json");
        let body = get_action_body(&muted, "GetMute");
        assert_eq!(extract_tag(&body, "CurrentMute").as_deref(), Some("1"));
    }

    #[test]
    fn high_case_has_lossless_actual_quality_and_unescapes_album_ampersand() {
        let cap = load_capture("WiiM_Ultra_20260706_075156.TidalConnect-HIGH.json");
        let info = parse_info_ex_response(&get_info_ex_body(&cap)).unwrap();
        assert_eq!(info.actual_quality.as_deref(), Some("LOSSLESS"));
        // Real album title has a literal "&", double-escaped on the wire
        // (`&amp;amp;`) — one unescape recovers real DIDL XML (`&amp;`
        // inside the tag content), a second recovers the literal "&".
        assert_eq!(info.album, "The Art Tatum & Ben Webster Quartet (Bonus Track Version)");
    }

    #[test]
    fn mp3_case_has_present_but_empty_actual_quality() {
        let cap = load_capture("WiiM_Ultra_20260706_075341.usb-mp3.json");
        let info = parse_info_ex_response(&get_info_ex_body(&cap)).unwrap();
        // Present-but-empty, not absent — this is the crux of the codec
        // badge rule (falls back to `protocol_info`, not to "no badge").
        assert_eq!(info.actual_quality.as_deref(), Some(""));
        assert_eq!(info.protocol_info.as_deref(), Some("http-get:*:audio/mpeg:DLNA.ORG_PN=MP3;DLNA.ORG_OP=01;"));
        assert_eq!(info.play_medium, "SONGLIST-LOCAL");
        assert_eq!(info.track_source, "UPnPServer");
    }

    #[test]
    fn tunein_case_has_actual_quality_tag_entirely_absent() {
        let cap = load_capture("WiiM_Ultra_20260706_075502.tunein.json");
        let info = parse_info_ex_response(&get_info_ex_body(&cap)).unwrap();
        // Absent, not empty — same `protocol_info` as the mp3 case
        // (`DLNA.ORG_PN=MP3` in both), which is exactly why the WiiM app's
        // "show a badge or not" decision can't be based on protocol_info
        // alone; only presence-vs-absence of this tag distinguishes them.
        assert_eq!(info.actual_quality, None);
        assert_eq!(info.protocol_info.as_deref(), Some("http-get:*:audio/mpeg:DLNA.ORG_PN=MP3;DLNA.ORG_OP=01;"));
        assert_eq!(info.play_medium, "RADIO-NETWORK");
        assert_eq!(info.track_source, "newTuneIn");
        assert_eq!(info.title, "Foolin’ Myself");
    }

    #[test]
    fn dlna_case_also_has_actual_quality_tag_entirely_absent() {
        // Third-party DLNA push (a "Music Assistant"-controlled cast, per
        // `dc:description`), not the device's own local `SONGLIST-LOCAL`
        // playback. Notably, `res protocolInfo` here genuinely says
        // `audio/flac` (unlike the mp3/tunein cases, which both said
        // `audio/mpeg` regardless of the tag's presence) — yet
        // `actual_quality` is still absent entirely, and per the confirmed
        // rule that alone means no codec badge, even though the real
        // content genuinely is FLAC and protocolInfo says so. Confirms the
        // rule is driven solely by tag presence, not by inspecting
        // protocolInfo's mime type as a smarter universal fallback.
        let cap = load_capture("WiiM_Ultra_20260706_084809.DLNA.json");
        let info = parse_info_ex_response(&get_info_ex_body(&cap)).unwrap();
        assert_eq!(info.actual_quality, None);
        assert!(info.protocol_info.as_deref().unwrap().contains("audio/flac"));
        assert_eq!(info.play_medium, "THIRD-DLNA");
        assert_eq!(info.track_source, "");
        assert_eq!(info.rate_hz, "192000");
        assert_eq!(info.format_s, "24");
        assert_eq!(info.title, "Doralice");
    }

    #[test]
    fn unescape_handles_double_escaped_ampersand() {
        assert_eq!(unescape_xml_entities("&amp;amp;"), "&amp;");
        assert_eq!(unescape_xml_entities("&lt;tag&gt;"), "<tag>");
    }

    #[test]
    fn gui_behavior_omitted_action_key_means_enabled() {
        // Real Pandora2 shape: "next" is omitted entirely while prev/loop/
        // queue are explicitly listed — confirmed via a real capture that
        // next genuinely works there (matches TRACK_SOURCES_CTRL's own
        // "previous restricted, next not" rule for this service).
        let gb = parse_gui_behavior(
            r#"{"loop": {"enabled": false},"prev": {"enabled": false},"queue": {"enabled": false},"seek": {"enabled": true}}"#
        ).unwrap();
        assert_eq!(gb, GuiBehavior { next: true, prev: false });
    }

    #[test]
    fn gui_behavior_absent_tag_and_malformed_json_both_give_none() {
        assert_eq!(parse_gui_behavior("not json"), None);
        assert_eq!(parse_gui_behavior("{}"), Some(GuiBehavior { next: true, prev: true }));
    }

    #[test]
    fn wiim_radio_case_has_guibehavior_all_disabled() {
        let cap = load_capture("WiiM_Ultra_20260706_110856.WiimRadio.json");
        let info = parse_info_ex_response(&get_info_ex_body(&cap)).unwrap();
        assert_eq!(info.play_medium, "RADIO-NETWORK");
        assert_eq!(info.gui_behavior, Some(GuiBehavior { next: false, prev: false }));
    }

    #[test]
    fn spotify_free_tier_case_has_guibehavior_all_disabled() {
        let cap = load_capture("WiiM_Ultra_20260708_012808.Spotify.json");
        let info = parse_info_ex_response(&get_info_ex_body(&cap)).unwrap();
        assert_eq!(info.play_medium, "SPOTIFY");
        assert_eq!(info.gui_behavior, Some(GuiBehavior { next: false, prev: false }));
    }

    #[test]
    fn spotify_premium_tier_case_has_guibehavior_all_enabled() {
        // Otherwise-identical Spotify Connect session, differing only by
        // account tier — confirms `gui_behavior` (not any static
        // play_medium/track_source rule) is what actually tracks this.
        let cap = load_capture("WiiM_Ultra_20260708_013737.Spotify-WithPrev.json");
        let info = parse_info_ex_response(&get_info_ex_body(&cap)).unwrap();
        assert_eq!(info.play_medium, "SPOTIFY");
        assert_eq!(info.gui_behavior, Some(GuiBehavior { next: true, prev: true }));
    }

    /// Real iEAST AudioCast unit with only 2 of its slots actually
    /// configured (`Key1`/`Key2`) — every other `KeyN` (including `Key0`
    /// and everything from `Key3` through `Key33`) is a completely empty
    /// tag, `<KeyN></KeyN>`, not just missing a `<Name>`. Confirms those
    /// slots are dropped from `entries` entirely rather than turned into
    /// `PresetKind::Empty` placeholders — otherwise every one of this
    /// device's unused slots would show up as a visible preset button with
    /// a generic icon and no name.
    #[test]
    fn key_mapping_empty_slots_are_dropped_not_placeholder_entries() {
        let cap = load_device_capture("AudioCastBu_20260708_095957.json");
        let body = get_key_mapping_body(&cap);
        match parse_key_mapping_presets(&body, "") {
            PresetFetchOutcome::Changed(_, entries) => {
                assert_eq!(entries.len(), 2, "expected only the 2 configured slots: {entries:?}");
                assert_eq!(entries[0].slot, 1);
                // Also confirms the `_#~<timestamp>` internal-uniquifier
                // suffix (`"My Mix 1_#~2026-07-08 16:49:59"` on the wire) is
                // stripped for display, not shown verbatim.
                assert_eq!(entries[0].name, "My Mix 1");
                assert_eq!(entries[1].slot, 2);
                assert_eq!(entries[1].name, "Radio National Sydney");
                for e in &entries {
                    assert_eq!(e.kind, PresetKind::Media);
                }
            }
            other => panic!("expected Changed, got {other:?}"),
        }
    }

    #[test]
    fn strip_wiimu_name_suffix_removes_hash_tilde_timestamp() {
        assert_eq!(strip_wiimu_name_suffix("My Mix 1_#~2026-07-08 16:49:59"), "My Mix 1");
        assert_eq!(strip_wiimu_name_suffix("Radio National Sydney"), "Radio National Sydney");
        assert_eq!(strip_wiimu_name_suffix(""), "");
    }
}
