//! `wiim-simulator` — replays a `wiim-capture` JSON file as a fake
//! LinkPlay/WiiM HTTP(S) device, so `rustywiim` (or `wiim-capture` itself)
//! can be pointed at something other than real hardware for testing.
//!
//! Usage: `wiim-simulator <capture-file-or-dir> [--http PORT] [--https PORT]
//! [--upnp-port PORT] [--no-upnp] [--no-stateful] [--global]`
//!
//! `--http`/`--https` are **cumulative** — each occurrence opens one more
//! listener (all serving the same capture file/state), not a single
//! toggle-plus-port pair. With **neither** given, defaults to one HTTP and
//! one HTTPS listener on **random OS-assigned ports** (`port: 0`, the
//! standard "pick an ephemeral port" meaning) — the actually-assigned port is
//! read back from each bound socket and printed as a ready-to-use URL, so
//! there's nothing to guess and no fixed port to collide with something else
//! already running locally. Each listener runs on its own OS thread; a bind
//! failure on one listener is logged and skipped rather than aborting the
//! others (the process only exits if *none* of them bind).
//!
//! **Loopback-only by default** (`127.0.0.1`) — a safer default for a test
//! tool than exposing it on the LAN. `--global` binds every listener to
//! `0.0.0.0` instead (including the UPnP listener, if any).
//!
//! **Pure replay by default**: every request is answered strictly from what's
//! actually in the capture file, keyed by request path + query (not just the
//! `command=` value) — this is what lets the `getsyslog:download` entry's
//! distinct URL (not a `httpapi.asp?command=...` call at all) replay
//! correctly through the exact same lookup, with no special-casing. A
//! request with no matching entry gets 404 — a visible "the capture has no
//! data for this," never a silently wrong response.
//!
//! **Stateful by default** (`--no-stateful` opts back out to pure verbatim
//! replay): a small in-memory mini-device (`SimState`: volume, mute, play
//! state, position, loop mode, source mode) seeded from the captured
//! `getPlayerStatusEx`/`getPlayerStatus` body, shared (behind a `Mutex`,
//! since multiple listener threads — including the UPnP one — can touch it
//! concurrently) across every listener. Only the small, fixed set of
//! playback-control commands `handle_mutation()` recognizes
//! (`setPlayerCmd:vol/mute/seek/loopmode/resume/play/pause/onepause/stop/
//! next/prev`, `switchmode`, `MCUKeyShortClick`, `setAudioOutputHardwareMode`
//! — the ones `wiim-capture` deliberately never sends to a real device) are
//! actually simulated: they update `SimState` and return a synthesized "OK".
//! `getPlayerStatusEx`/`getPlayerStatus` get patched with the current
//! in-memory state before replay so subsequent polls reflect it, instead of
//! showing the frozen captured snapshot forever. Everything else — even
//! while stateful — still replays from the capture file exactly as in the
//! `--no-stateful` case; this is deliberately a small, fixed set of
//! commands, not a general device model, with more coverage left for later.
//! `--no-stateful` only affects the main HTTP(S) API — the UPnP listener
//! (below) always reads/writes the same live `SimState`, since it never had
//! a "pure replay" mode to begin with.
//!
//! **UPnP**: whenever the capture has real UPnP data (`wiim-capture`'s basic
//! read-only UPnP probe — a `description.xml` plus a handful of standard
//! `AVTransport`/`RenderingControl` SOAP actions), one additional listener
//! serves it: `GET /description.xml` replays the captured description
//! verbatim (its `controlURL`s are relative paths, so they resolve correctly
//! against whatever host:port this listener actually binds), and SOAP `POST`
//! requests are dispatched by their `SOAPACTION` header (not by path, which
//! can vary by capture) to the exact set of actions `device/upnp.rs`'s
//! `UpnpClient` itself calls: `AVTransport.GetInfoEx` (read-only — no
//! `SetInfoEx` exists in the real protocol either; replays the captured
//! response with its live `CurrentVolume`/`CurrentMute`/`LoopMode` tags
//! patched from `SimState`, the same spirit as `patch_player_status()`),
//! `RenderingControl.GetMute`/`SetMute`, and `PlayQueue.GetQueueLoopMode`/
//! `SetQueueLoopMode` — both Get and Set, fully synthesized from `SimState`
//! rather than templated (real captures never include a `Set*` action,
//! `wiim-capture` being read-only by design, and these two are trivial
//! enough not to need a captured template anyway). Any other SOAP action is
//! outside this fixed set and gets a 500, same "visible absence, not a wrong
//! guess" rule as the main API.
//!
//! Binds **port 49152 by default** (`--upnp-port` to override), not a
//! random one like the main API listeners: `device::upnp::UpnpClient::
//! discover()` only ever checks the two well-known LinkPlay UPnP ports
//! (49152/59152) — it deliberately ignores any port embedded in the address
//! it's given, since on real hardware UPnP's port is independent of the
//! main API's — so this listener has to be on one of those two fixed ports
//! to be discoverable by `rustywiim` itself at all. That's also why only one
//! UPnP listener exists (not cumulative like `--http`/`--https`): a real
//! device only ever has one. `--no-upnp` disables it entirely.
//!
//! `--https` serves TLS with a self-signed certificate generated fresh at
//! startup (`rcgen`) — `rustywiim`'s own TLS client
//! (`danger_accept_invalid_certs`) never validates the server's certificate
//! against a CA, exactly like it doesn't for real WiiM hardware, so a
//! throwaway cert is sufficient; nothing to manage on disk. One certificate
//! is generated and shared by every `--https` listener. The UPnP listener is
//! always plain HTTP (matching real hardware, and `UpnpClient::discover()`'s
//! own probe order, which tries `http://` before `https://` at each port).

use rustywiim::capture::format::{CaptureFile, CommandCapture, Outcome, ResponseFormat};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

struct SimState {
    vol: u32,
    mute: bool,
    status: String,
    curpos: u64,
    totlen: u64,
    loop_mode: String,
    mode: String,
}

/// Everything needed to answer the UPnP SOAP actions this app itself makes
/// (see this file's module doc comment for exactly which ones). Built once
/// at startup (`build_upnp_shared()`) from whatever `wiim-capture` recorded;
/// `None` — no UPnP listener at all — when the capture has no real
/// `description.xml` to serve.
struct UpnpShared {
    /// Raw captured `description.xml` body, replayed verbatim — its
    /// `controlURL`s are relative paths (LinkPlay convention, confirmed
    /// against real captures), so they resolve correctly against whatever
    /// host:port this listener actually binds, no rewriting needed.
    description_xml: String,
    /// Real captured `GetInfoEx` response envelope, when the capture has one
    /// (`outcome == Ok`) — its live fields (`CurrentVolume`/`CurrentMute`/
    /// `LoopMode`) are patched from `SimState` before each reply, same
    /// spirit as `patch_player_status()`. `None` (capture never captured a
    /// successful `GetInfoEx`) means `GetInfoEx` requests get a 500 — same
    /// "visible absence, not an invented response" rule the main HTTP API
    /// replay already follows, rather than fabricating fake track metadata.
    info_ex_template: Option<String>,
}

/// Everything a request-handling thread needs, shared read-only (`index`,
/// `upnp`) or behind a `Mutex` (`state`) across every listener thread —
/// including the UPnP one, when it exists.
struct Shared {
    index: HashMap<String, CommandCapture>,
    state: Mutex<SimState>,
    /// Whether the main HTTP(S) command API applies `handle_mutation()`/
    /// `patch_player_status()` — today's default, `--no-stateful` turns it
    /// back off for pure verbatim replay. Doesn't affect the UPnP listener,
    /// which always reads/writes `state` regardless (see this file's module
    /// doc comment).
    stateful_http: bool,
    upnp: Option<UpnpShared>,
    /// This instance's own identity — see `generate_fresh_uuid()`'s doc
    /// comment for why every instance gets one instead of replaying the
    /// capture's frozen value. Patched into every JSON reply that carries a
    /// `uuid`/`upnp_uuid` field (`handle_command()`) and into the served
    /// `description.xml`'s `<uuid>`/`<UDN>` tags (`build_upnp_shared()`).
    fresh_uuid: FreshUuid,
}

/// One simulator instance's fresh identity: the plain 24-hex-char LinkPlay
/// UUID (`getStatusEx`'s `uuid` field) and its UPnP-dashed derivative
/// (`getStatusEx`'s own `upnp_uuid` field, and `description.xml`'s `<UDN>`)
/// — confirmed against a real WiiM Ultra (`10.1.1.73`, 2026-07-15) that the
/// latter is deterministically `plain + plain[0..8]`, dash-grouped 8-4-4-4-12
/// and prefixed `uuid:` (real example: plain `FF98F7F4075B5A90FA9572C3` →
/// `uuid:FF98F7F4-075B-5A90-FA95-72C3FF98F7F4` — note the last group,
/// `72C3FF98F7F4`, is the plain value's own tail followed by its own head
/// repeated) — not independently random, so this is computed once from
/// `plain`, never generated separately.
struct FreshUuid {
    plain: String,
    dashed: String,
}

impl FreshUuid {
    fn new() -> Self {
        let plain = generate_fresh_uuid();
        let dashed = derive_upnp_uuid(&plain);
        Self { plain, dashed }
    }
}

/// Generates a fresh, plausible-looking 24-hex-character LinkPlay-style
/// device UUID (matching the shape real devices use, e.g.
/// `"FF98F7F4075B5A90FA9572C3"` — uppercase hex, no dashes) — not
/// cryptographically random, just distinct per process run (mixes
/// wall-clock time, PID, and a stack address, which differs per run under
/// ASLR), so multiple simulator instances replaying the same capture file
/// present as genuinely different devices instead of the identical UUID
/// baked into the capture at record time — confirmed this matters live:
/// `DeviceManager` dedupes `DeviceState`s per UUID, discovery dedups by
/// UUID, and multiroom grouping is UUID-keyed, all of which would treat
/// two simulator instances from the same capture as one device otherwise.
fn generate_fresh_uuid() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let nanos = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_nanos() as u64;
    let pid = std::process::id() as u64;
    let stack_addr = &nanos as *const _ as u64;
    let mut seed = nanos ^ pid.rotate_left(32) ^ stack_addr.rotate_left(17) ^ 0x9E37_79B9_7F4A_7C15;
    let mut out = String::with_capacity(24);
    for _ in 0..24 {
        // xorshift64 — not cryptographic, just needs to look random and
        // differ run to run, which the seed mixing above already ensures.
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        out.push(std::char::from_digit((seed & 0xf) as u32, 16).unwrap().to_ascii_uppercase());
    }
    out
}

/// `uuid:XXXXXXXX-XXXX-XXXX-XXXX-XXXXXXXXXXXX` from a 24-hex-char plain
/// UUID — see `FreshUuid`'s doc comment for the real-device-confirmed
/// derivation (pad to the 32 hex digits a dashed UUID needs by repeating
/// the first 8, then group 8-4-4-4-12).
fn derive_upnp_uuid(plain: &str) -> String {
    let padded = format!("{plain}{}", &plain[..8.min(plain.len())]);
    format!(
        "uuid:{}-{}-{}-{}-{}",
        &padded[0..8], &padded[8..12], &padded[12..16], &padded[16..20], &padded[20..32],
    )
}

/// Seeds `SimState` from the first of `getPlayerStatusEx`/`getPlayerStatus`
/// present (in that order) and successfully captured — searched directly in
/// `capture.commands` (not the path-keyed `index`) since this is a one-time
/// startup lookup by command name, not a per-request hot path.
fn init_state(capture: &CaptureFile) -> SimState {
    let mut state = SimState {
        vol: 30,
        mute: false,
        status: "stop".to_string(),
        curpos: 0,
        totlen: 0,
        loop_mode: "0".to_string(),
        mode: "0".to_string(),
    };
    for cmd in ["getPlayerStatusEx", "getPlayerStatus"] {
        let Some(cap) = capture.commands.iter().find(|c| c.command == cmd) else { continue };
        if cap.outcome != Outcome::Ok {
            continue;
        }
        let Some(obj) = cap.body.as_ref().and_then(|b| b.as_object()) else { continue };
        let str_field = |k: &str| obj.get(k).and_then(|v| v.as_str());
        if let Some(v) = str_field("vol").and_then(|s| s.parse().ok()) {
            state.vol = v;
        }
        if let Some(v) = str_field("mute") {
            state.mute = v == "1";
        }
        if let Some(v) = str_field("status") {
            state.status = v.to_string();
        }
        if let Some(v) = str_field("curpos").and_then(|s| s.parse().ok()) {
            state.curpos = v;
        }
        if let Some(v) = str_field("totlen").and_then(|s| s.parse().ok()) {
            state.totlen = v;
        }
        if let Some(v) = str_field("loop") {
            state.loop_mode = v.to_string();
        }
        if let Some(v) = str_field("mode") {
            state.mode = v.to_string();
        }
        break;
    }
    state
}

/// Recognizes the playback-control commands `wiim-capture` never sends to a
/// real device, updates `state`, and returns a synthesized "OK". Returns
/// `None` for anything it doesn't recognize as a mutator, so the caller
/// falls through to replaying a captured response instead.
fn handle_mutation(command: &str, state: &mut SimState) -> Option<String> {
    if let Some(n) = command.strip_prefix("setPlayerCmd:vol:") {
        state.vol = n.parse::<u32>().unwrap_or(state.vol).min(100);
        return Some("OK".to_string());
    }
    if let Some(n) = command.strip_prefix("setPlayerCmd:mute:") {
        state.mute = n.trim() == "1";
        return Some("OK".to_string());
    }
    if let Some(n) = command.strip_prefix("setPlayerCmd:seek:") {
        state.curpos = n.parse().unwrap_or(state.curpos);
        return Some("OK".to_string());
    }
    if let Some(n) = command.strip_prefix("setPlayerCmd:loopmode:") {
        state.loop_mode = n.to_string();
        return Some("OK".to_string());
    }
    match command {
        "setPlayerCmd:resume" | "setPlayerCmd:play" => {
            state.status = "play".to_string();
            Some("OK".to_string())
        }
        "setPlayerCmd:pause" => {
            state.status = "pause".to_string();
            Some("OK".to_string())
        }
        "setPlayerCmd:onepause" => {
            state.status = if state.status == "play" { "pause" } else { "play" }.to_string();
            Some("OK".to_string())
        }
        "setPlayerCmd:stop" => {
            state.status = "stop".to_string();
            Some("OK".to_string())
        }
        "setPlayerCmd:next" | "setPlayerCmd:prev" => {
            state.curpos = 0;
            Some("OK".to_string())
        }
        _ if command.starts_with("setPlayerCmd:switchmode:")
            || command.starts_with("MCUKeyShortClick")
            || command.starts_with("setAudioOutputHardwareMode:") =>
        {
            Some("OK".to_string())
        }
        _ => None,
    }
}

fn patch_player_status(body: &mut serde_json::Value, state: &SimState) {
    let Some(obj) = body.as_object_mut() else { return };
    obj.insert("vol".into(), serde_json::Value::String(state.vol.to_string()));
    obj.insert(
        "mute".into(),
        serde_json::Value::String(if state.mute { "1" } else { "0" }.to_string()),
    );
    obj.insert("status".into(), serde_json::Value::String(state.status.clone()));
    obj.insert("curpos".into(), serde_json::Value::String(state.curpos.to_string()));
    obj.insert("totlen".into(), serde_json::Value::String(state.totlen.to_string()));
    obj.insert("loop".into(), serde_json::Value::String(state.loop_mode.clone()));
    obj.insert("mode".into(), serde_json::Value::String(state.mode.clone()));
}

// ── UPnP ──────────────────────────────────────────────────────────────────────

/// Builds the UPnP-serving state from whatever `wiim-capture` recorded.
/// `None` when the capture has no `description.xml` at all — nothing to
/// serve, so no UPnP listener is started (see `main()`). `fresh_uuid`'s
/// `<uuid>`/`<UDN>` are patched in here, once, rather than per-request —
/// `description.xml` is served verbatim otherwise, so this is the one
/// place that needs to happen.
fn build_upnp_shared(capture: &CaptureFile, fresh_uuid: &FreshUuid) -> Option<UpnpShared> {
    let upnp = capture.upnp.as_ref()?;
    let description_xml = upnp.description.as_ref()?.body.as_str()?.to_string();
    let description_xml = patch_tag(&description_xml, "uuid", &fresh_uuid.plain);
    let description_xml = patch_tag(&description_xml, "UDN", &fresh_uuid.dashed);
    let info_ex_template = upnp
        .actions
        .iter()
        .find(|a| a.action == "GetInfoEx" && a.outcome == Outcome::Ok)
        .and_then(|a| a.response.as_ref())
        .and_then(|r| r.body.as_str())
        .map(|s| s.to_string());
    Some(UpnpShared { description_xml, info_ex_template })
}

/// Wraps `args_xml` in a standard SOAP 1.1 response envelope for `action` on
/// `service` — the same shape `device/upnp.rs`'s `soap_call()` builds for
/// requests, mirrored for responses.
fn soap_envelope(service: &str, action: &str, args_xml: &str) -> String {
    format!(
        "<?xml version=\"1.0\"?>\r\n\
         <s:Envelope xmlns:s=\"http://schemas.xmlsoap.org/soap/envelope/\" \
         s:encodingStyle=\"http://schemas.xmlsoap.org/soap/encoding/\">\
         <s:Body><u:{action}Response xmlns:u=\"{service}\">{args_xml}</u:{action}Response></s:Body></s:Envelope>"
    )
}

/// Extracts `(service_type, action)` from a `SOAPACTION` header value —
/// `"<service_type>#<action>"`, the exact convention `device/upnp.rs`'s own
/// `soap_call()` sends (quoted; unquoted tolerated too, defensively).
fn parse_soap_action(header_value: &str) -> Option<(&str, &str)> {
    header_value.trim().trim_matches('"').split_once('#')
}

/// Finds the first `<tag>...</tag>` in `xml` and returns its content —
/// used to read `<DesiredMute>`/`<LoopMode>` out of an incoming `SetMute`/
/// `SetQueueLoopMode` request body. Mirrors `device/upnp.rs`'s own private
/// `extract_tag()` (not reused directly — that module keeps wire-parsing
/// internal to `device/`, per this codebase's own layering rule; this is a
/// test tool operating on the wire from the *other* side).
fn extract_tag<'a>(xml: &'a str, tag: &str) -> Option<&'a str> {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    let start = xml.find(&open)? + open.len();
    let end = start + xml[start..].find(&close)?;
    Some(&xml[start..end])
}

/// Replaces the *content* of the first `<tag>...</tag>` in `xml` with
/// `new_value`, leaving everything else untouched — a no-op (returns `xml`
/// unchanged) if the tag isn't present. The write-side counterpart to
/// `extract_tag()` above, used to patch `GetInfoEx`'s live fields into the
/// captured template.
fn patch_tag(xml: &str, tag: &str, new_value: &str) -> String {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    let Some(start) = xml.find(&open) else { return xml.to_string() };
    let content_start = start + open.len();
    let Some(close_rel) = xml[content_start..].find(&close) else { return xml.to_string() };
    let content_end = content_start + close_rel;
    format!("{}{}{}", &xml[..content_start], new_value, &xml[content_end..])
}

/// Answers the fixed set of UPnP SOAP actions this app itself makes (see
/// this file's module doc comment). `body` is the raw incoming SOAP request
/// XML (only actually read for `SetMute`/`SetQueueLoopMode`, the only two
/// that carry an argument). Returns `None` for anything outside this set,
/// so the caller replies with a visible error instead of a wrong guess.
fn handle_soap_action(
    service: &str,
    action: &str,
    body: &str,
    upnp: &UpnpShared,
    state: &Mutex<SimState>,
) -> Option<String> {
    let mut state = state.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    match action {
        "GetInfoEx" => {
            let template = upnp.info_ex_template.as_deref()?;
            let xml = patch_tag(template, "CurrentVolume", &state.vol.to_string());
            let xml = patch_tag(&xml, "CurrentMute", if state.mute { "1" } else { "0" });
            let xml = patch_tag(&xml, "LoopMode", &state.loop_mode);
            Some(xml)
        }
        "GetMute" => Some(soap_envelope(
            service,
            action,
            &format!("<CurrentMute>{}</CurrentMute>", if state.mute { "1" } else { "0" }),
        )),
        "SetMute" => {
            if let Some(v) = extract_tag(body, "DesiredMute") {
                state.mute = v.trim() == "1";
            }
            Some(soap_envelope(service, action, ""))
        }
        "GetQueueLoopMode" => {
            Some(soap_envelope(service, action, &format!("<LoopMode>{}</LoopMode>", state.loop_mode)))
        }
        "SetQueueLoopMode" => {
            if let Some(v) = extract_tag(body, "LoopMode") {
                state.loop_mode = v.trim().to_string();
            }
            Some(soap_envelope(service, action, ""))
        }
        _ => None,
    }
}

/// The UPnP listener's request loop — structurally parallel to `serve()`
/// below (same `catch_unwind` panic-safety rationale) but answering GET
/// `/description.xml` and SOAP `POST`s instead of the main command API.
fn serve_upnp(server: tiny_http::Server, upnp: &UpnpShared, state: &Mutex<SimState>) {
    for mut request in server.incoming_requests() {
        let is_description_get =
            *request.method() == tiny_http::Method::Get && request.url() == "/description.xml";
        let is_post = *request.method() == tiny_http::Method::Post;
        let soap_action_header = is_post.then(|| {
            request
                .headers()
                .iter()
                .find(|h| h.field.equiv("SOAPACTION"))
                .map(|h| h.value.as_str().to_string())
        }).flatten();

        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            if is_description_get {
                return (200, upnp.description_xml.clone(), "text/xml");
            }
            if is_post {
                let mut body = String::new();
                let _ = request.as_reader().read_to_string(&mut body);
                return match soap_action_header.as_deref().and_then(parse_soap_action) {
                    Some((service, action)) => match handle_soap_action(service, action, &body, upnp, state) {
                        Some(xml) => (200, xml, "text/xml"),
                        None => (500, format!("simulator: no response modeled for {action}"), "text/plain"),
                    },
                    None => (400, "missing/malformed SOAPACTION header".to_string(), "text/plain"),
                };
            }
            (404, String::new(), "text/plain")
        }));
        let (status, body, content_type) = result.unwrap_or_else(|_| {
            eprintln!("[wiim-simulator] internal error handling UPnP request (see panic message above) -> 500");
            (500, "internal simulator error".to_string(), "text/plain")
        });
        eprintln!(
            "[wiim-simulator] upnp {} {} -> {status}",
            if is_post { "POST" } else { "GET" },
            request.url(),
        );
        let response = tiny_http::Response::from_string(body).with_status_code(status).with_header(
            tiny_http::Header::from_bytes(&b"Content-Type"[..], content_type.as_bytes())
                .expect("static header is valid"),
        );
        let _ = request.respond(response);
    }
}

fn load_capture(path: &std::path::Path) -> CaptureFile {
    let file_path = if path.is_dir() {
        let mut candidates: Vec<std::path::PathBuf> = std::fs::read_dir(path)
            .unwrap_or_else(|e| {
                eprintln!("wiim-simulator: cannot read directory {}: {e}", path.display());
                std::process::exit(1);
            })
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| p.extension().and_then(|s| s.to_str()) == Some("json"))
            .collect();
        candidates.sort_by_key(|p| {
            std::fs::metadata(p)
                .and_then(|m| m.modified())
                .unwrap_or(std::time::SystemTime::UNIX_EPOCH)
        });
        match candidates.pop() {
            Some(p) => p,
            None => {
                eprintln!("wiim-simulator: no .json capture files found in {}", path.display());
                std::process::exit(1);
            }
        }
    } else {
        path.to_path_buf()
    };

    let raw = std::fs::read_to_string(&file_path).unwrap_or_else(|e| {
        eprintln!("wiim-simulator: failed to read {}: {e}", file_path.display());
        std::process::exit(1);
    });
    let capture: CaptureFile = serde_json::from_str(&raw).unwrap_or_else(|e| {
        eprintln!("wiim-simulator: {} is not a valid capture file: {e}", file_path.display());
        std::process::exit(1);
    });
    eprintln!("[wiim-simulator] loaded {} ({})", file_path.display(), capture.model);
    capture
}

/// Strips `scheme://host:port` off a captured URL, leaving the path+query
/// exactly as a server sees an incoming request-target — e.g.
/// `https://xxx.xxx.x.xx:443/httpapi.asp?command=getStatusEx` →
/// `/httpapi.asp?command=getStatusEx`, or
/// `https://xxx.xxx.x.xx:443/data/sys.log` → `/data/sys.log`. IP
/// anonymization (`anonymize_ip_in_url` in `wiim-capture.rs`) only ever
/// scrambles the host octets, never the path/query, so this works
/// identically on the anonymized URLs a capture file actually contains.
fn path_and_query(url: &str) -> String {
    let Some(scheme_end) = url.find("://") else { return url.to_string() };
    let after_scheme = &url[scheme_end + 3..];
    match after_scheme.find('/') {
        Some(slash) => after_scheme[slash..].to_string(),
        None => "/".to_string(),
    }
}

/// Indexes every captured command by its request path+query (see
/// `path_and_query`) — this is what lets *any* captured URL replay
/// correctly, not just `httpapi.asp?command=...` ones (e.g. `getsyslog`'s
/// separate download-link fetch). Clones each `CommandCapture` (cheap,
/// one-time, at startup) rather than borrowing, so the index can be shared
/// by value across listener threads via `Arc`.
fn index_by_path(capture: &CaptureFile) -> HashMap<String, CommandCapture> {
    // UPnP SOAP actions are handled by a dedicated listener/dispatcher
    // (`serve_upnp()`/`handle_soap_action()`, keyed by SOAPACTION rather
    // than path) — not indexed here, which stays GET-response replay of the
    // main HTTP(S) API's `commands` array only.
    capture.commands.iter().map(|c| (path_and_query(&c.url), c.clone())).collect()
}

/// Best-effort `command=` value from a request's query string, for logging
/// only — routing itself is by full path+query (`index_by_path`), not this.
fn extract_command(url: &str) -> Option<String> {
    let query = url.split('?').nth(1)?;
    query.split('&').find_map(|pair| pair.strip_prefix("command=").map(percent_decode))
}

/// Percent-decodes `s`. Operates on raw bytes throughout (never slices `s`
/// itself, only its `&[u8]` view) and reassembles via `from_utf8_lossy` at
/// the end — two bugs this fixes over an earlier version that indexed `s`
/// directly: (1) `s[i+1..i+3]` on a `&str` panics ("byte index is not a char
/// boundary") whenever those two bytes aren't themselves a char boundary,
/// which a stray `%` near non-ASCII bytes can trigger; (2) `push(byte as
/// char)` per decoded byte mangled any multi-byte percent-encoded UTF-8
/// sequence (e.g. `%C3%A9` for "é" became two separate mis-decoded
/// characters instead of one). `serve()`'s `catch_unwind` is a second,
/// independent guard against anything like (1) recurring elsewhere.
fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' if i + 2 < bytes.len() => {
                let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).ok();
                match hex.and_then(|h| u8::from_str_radix(h, 16).ok()) {
                    Some(byte) => {
                        out.push(byte);
                        i += 3;
                    }
                    None => {
                        out.push(b'%');
                        i += 1;
                    }
                }
            }
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn render_body(cap: &CommandCapture) -> String {
    match cap.format {
        Some(ResponseFormat::Json) => cap.body.as_ref().map(|v| v.to_string()).unwrap_or_default(),
        // Xml/Text are both already plain strings — served as-is, same as a
        // real device would send them over the wire (the format tag is a
        // capture-file-side distinction for readability, not a wire concept).
        Some(ResponseFormat::Xml) | Some(ResponseFormat::Text) => {
            cap.body.as_ref().and_then(|v| v.as_str()).unwrap_or("").to_string()
        }
        Some(ResponseFormat::Base64) => {
            let encoded = cap.body.as_ref().and_then(|v| v.as_str()).unwrap_or("");
            base64::Engine::decode(&base64::engine::general_purpose::STANDARD, encoded)
                .ok()
                .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
                .unwrap_or_default()
        }
        None => String::new(),
    }
}

/// Rewrites a JSON body's top-level `uuid`/`upnp_uuid` fields (`getStatusEx`'s
/// identity fields — confirmed present together on real hardware, see
/// `FreshUuid`'s doc comment) to this instance's own fresh identity, if
/// either key is present. A no-op for every other JSON reply (most captured
/// commands have neither key), and for anything that isn't valid JSON.
fn patch_uuid_fields(body: &str, fresh_uuid: &FreshUuid) -> String {
    let Ok(mut value) = serde_json::from_str::<serde_json::Value>(body) else { return body.to_string() };
    let Some(obj) = value.as_object_mut() else { return body.to_string() };
    let mut patched = false;
    if obj.contains_key("uuid") {
        obj.insert("uuid".into(), serde_json::Value::String(fresh_uuid.plain.clone()));
        patched = true;
    }
    if obj.contains_key("upnp_uuid") {
        obj.insert("upnp_uuid".into(), serde_json::Value::String(fresh_uuid.dashed.clone()));
        patched = true;
    }
    if patched { value.to_string() } else { body.to_string() }
}

/// Replays a captured entry faithfully: `outcome == Ok` serves the recorded
/// body at its recorded (or default 200) status; anything else (the command
/// itself failed at capture time) has no real body to replay, so it serves
/// the recorded status if there is one, else a generic 500 — still a
/// response, but visibly not a real one. JSON replies get their identity
/// fields patched to this instance's own fresh UUID (see
/// `patch_uuid_fields()`) — unconditionally, regardless of `--no-stateful`,
/// since a fresh per-instance identity isn't "mini-device simulation," it's
/// basic test-tool correctness (two simulator instances replaying the same
/// capture file must not present as the same device).
fn handle_command(cap: &CommandCapture, fresh_uuid: &FreshUuid) -> (u16, String) {
    if cap.outcome == Outcome::Ok {
        let body = render_body(cap);
        let body = if cap.format == Some(ResponseFormat::Json) {
            patch_uuid_fields(&body, fresh_uuid)
        } else {
            body
        };
        return (cap.http_status.unwrap_or(200), body);
    }
    (cap.http_status.unwrap_or(500), String::new())
}

#[derive(Clone, Copy, Debug)]
enum Scheme {
    Http,
    Https,
}

impl Scheme {
    fn as_str(self) -> &'static str {
        match self {
            Scheme::Http => "http",
            Scheme::Https => "https",
        }
    }
}

/// `port: 0` means "let the OS pick an ephemeral port" (the standard TCP/IP
/// meaning of binding to port 0) — used for the default two listeners so
/// they never collide with anything already running locally; the actually-
/// assigned port is read back from the bound socket and printed.
struct Listener {
    scheme: Scheme,
    port: u16,
}

/// Primary well-known LinkPlay UPnP port — matches `device::upnp`'s own
/// `DESCRIPTION_PORTS[0]`. See this file's module doc comment for why the
/// UPnP listener needs to be on this exact port (or `59152`, the other of
/// the two) to be discoverable by `rustywiim` at all.
const DEFAULT_UPNP_PORT: u16 = 49152;

struct Args {
    path: String,
    listeners: Vec<Listener>,
    no_stateful: bool,
    no_upnp: bool,
    upnp_port: u16,
    global: bool,
}

fn usage() -> ! {
    eprintln!(
        "usage: wiim-simulator <capture-file-or-dir> [--http PORT] [--https PORT] \
         [--upnp-port PORT] [--no-upnp] [--no-stateful] [--global]"
    );
    eprintln!("  (--http/--https are cumulative — repeat for more listeners; with neither");
    eprintln!("   given, defaults to one http + one https listener on random ports)");
    eprintln!("  (stateful mini-device simulation is on by default; --no-stateful disables it)");
    eprintln!("  (--upnp-port defaults to 49152, the port rustywiim's own UpnpClient looks for)");
    eprintln!("  (--global listens on 0.0.0.0 instead of the default 127.0.0.1-only)");
    std::process::exit(2);
}

fn parse_args() -> Args {
    let mut path = None;
    let mut listeners = Vec::new();
    let mut no_stateful = false;
    let mut no_upnp = false;
    let mut upnp_port = DEFAULT_UPNP_PORT;
    let mut global = false;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--http" | "--https" => {
                let scheme = if arg == "--http" { Scheme::Http } else { Scheme::Https };
                let port = args.next().and_then(|s| s.parse().ok()).unwrap_or_else(|| {
                    eprintln!("wiim-simulator: {arg} requires a port number");
                    std::process::exit(2);
                });
                listeners.push(Listener { scheme, port });
            }
            "--upnp-port" => {
                upnp_port = args.next().and_then(|s| s.parse().ok()).unwrap_or_else(|| {
                    eprintln!("wiim-simulator: --upnp-port requires a port number");
                    std::process::exit(2);
                });
            }
            "--no-upnp" => no_upnp = true,
            // Accepted (as a no-op) for anyone used to the old opt-in flag —
            // stateful is the default now, nothing left for it to enable.
            "--stateful" => {}
            "--no-stateful" => no_stateful = true,
            "--global" => global = true,
            "-h" | "--help" => usage(),
            other if path.is_none() && !other.starts_with('-') => path = Some(other.to_string()),
            other => {
                eprintln!("wiim-simulator: unrecognized argument '{other}'");
                usage();
            }
        }
    }
    let Some(path) = path else { usage() };
    if listeners.is_empty() {
        listeners.push(Listener { scheme: Scheme::Http, port: 0 });
        listeners.push(Listener { scheme: Scheme::Https, port: 0 });
    }
    Args { path, listeners, no_stateful, no_upnp, upnp_port, global }
}

/// Generates a throwaway self-signed cert (fresh every run, never written to
/// disk) — sufficient because `rustywiim`'s own HTTPS client
/// (`build_reqwest_client`/`TlsMode`) sets `danger_accept_invalid_certs`,
/// exactly like it does for real self-signed WiiM hardware, so nothing here
/// ever validates this certificate against a CA.
fn generate_self_signed_cert() -> rcgen::CertifiedKey<rcgen::KeyPair> {
    rcgen::generate_simple_self_signed(["wiim-simulator".to_string()]).unwrap_or_else(|e| {
        eprintln!("wiim-simulator: failed to generate a self-signed certificate: {e}");
        std::process::exit(1);
    })
}

fn main() {
    let args = parse_args();
    let capture = load_capture(std::path::Path::new(&args.path));
    let index = index_by_path(&capture);
    let state = Mutex::new(init_state(&capture));
    let fresh_uuid = FreshUuid::new();
    eprintln!(
        "[wiim-simulator] this instance's identity: uuid={} upnp_uuid={}",
        fresh_uuid.plain, fresh_uuid.dashed,
    );
    let upnp = (!args.no_upnp).then(|| build_upnp_shared(&capture, &fresh_uuid)).flatten();
    let shared = Arc::new(Shared { index, state, stateful_http: !args.no_stateful, upnp, fresh_uuid });

    // One certificate, shared by every `--https` listener — generating it
    // eagerly (only if actually needed) avoids paying for it when every
    // listener is plain HTTP.
    let cert = args
        .listeners
        .iter()
        .any(|l| matches!(l.scheme, Scheme::Https))
        .then(generate_self_signed_cert);

    // Default (no --global): loopback-only, matching "don't accidentally
    // expose this on the LAN" as the safer default for a test tool. --global
    // switches every listener (including UPnP) to 0.0.0.0.
    let bind_host = if args.global { "0.0.0.0" } else { "127.0.0.1" };

    let mut handles = Vec::new();
    for listener in &args.listeners {
        let addr = format!("{bind_host}:{}", listener.port);
        let server = match listener.scheme {
            Scheme::Http => tiny_http::Server::http(&addr),
            Scheme::Https => {
                let cert = cert.as_ref().expect("cert generated above whenever an https listener exists");
                let ssl_config = tiny_http::SslConfig {
                    certificate: cert.cert.pem().into_bytes(),
                    private_key: cert.signing_key.serialize_pem().into_bytes(),
                };
                tiny_http::Server::https(&addr, ssl_config)
            }
        };
        let server = match server {
            Ok(s) => s,
            Err(e) => {
                eprintln!(
                    "[wiim-simulator] failed to bind {} on {addr}: {e} — skipping this listener",
                    listener.scheme.as_str()
                );
                continue;
            }
        };
        // With `port: 0` (the default), the OS picked an ephemeral port —
        // read back what it actually bound to rather than printing "0".
        let bound_port = server.server_addr().to_ip().map(|a| a.port()).unwrap_or(listener.port);
        eprintln!(
            "[wiim-simulator] serving {} on {}://{bind_host}:{bound_port}{}",
            capture.model,
            listener.scheme.as_str(),
            if shared.stateful_http { ", stateful mini-device on" } else { "" }
        );
        let shared = Arc::clone(&shared);
        handles.push(std::thread::spawn(move || serve(server, &shared)));
    }

    if shared.upnp.is_some() {
        let addr = format!("{bind_host}:{}", args.upnp_port);
        match tiny_http::Server::http(&addr) {
            Ok(server) => {
                eprintln!("[wiim-simulator] serving UPnP on http://{bind_host}:{}", args.upnp_port);
                let shared = Arc::clone(&shared);
                handles.push(std::thread::spawn(move || {
                    serve_upnp(server, shared.upnp.as_ref().expect("checked above"), &shared.state)
                }));
            }
            Err(e) => {
                eprintln!(
                    "[wiim-simulator] failed to bind UPnP listener on {addr}: {e} — skipping it \
                     (GetInfoEx/GetMute/SetMute/GetQueueLoopMode/SetQueueLoopMode won't be reachable)"
                );
            }
        }
    } else if !args.no_upnp {
        eprintln!("[wiim-simulator] no UPnP data in this capture — no UPnP listener started");
    }

    if handles.is_empty() {
        eprintln!("[wiim-simulator] no listener could bind, exiting");
        std::process::exit(1);
    }
    for handle in handles {
        let _ = handle.join();
    }
}

/// Resolves one request to a `(status, body)` reply. While `shared.stateful_http`
/// is set (the default) and the request's `command=` value is a recognized
/// mutator, `handle_mutation` handles it entirely (under `state`'s `Mutex`,
/// shared with the UPnP listener); `getPlayerStatusEx`/`getPlayerStatus` get
/// the current state patched in. Everything else (always, regardless of
/// `stateful_http`) falls through to plain replay from `shared.index`, keyed
/// by the request's full path+query.
fn resolve_response(key: &str, command: Option<&str>, shared: &Shared) -> (u16, String) {
    if shared.stateful_http {
        if let Some(command) = command {
            let mut state = shared.state.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
            if let Some(body) = handle_mutation(command, &mut state) {
                return (200, body);
            }
            if matches!(command, "getPlayerStatusEx" | "getPlayerStatus") {
                return match shared.index.get(key) {
                    Some(cap) if cap.outcome == Outcome::Ok => match cap.body.clone() {
                        Some(mut body) => {
                            patch_player_status(&mut body, &state);
                            (200, body.to_string())
                        }
                        None => handle_command(cap, &shared.fresh_uuid),
                    },
                    Some(cap) => handle_command(cap, &shared.fresh_uuid),
                    None => (404, "unknown command".to_string()),
                };
            }
        }
    }
    match shared.index.get(key) {
        Some(cap) => handle_command(cap, &shared.fresh_uuid),
        None => (404, "unknown command".to_string()),
    }
}

fn serve(server: tiny_http::Server, shared: &Arc<Shared>) {
    for request in server.incoming_requests() {
        let key = request.url().to_string();
        let command = extract_command(&key);
        // Every request goes through catch_unwind: a panic handling one
        // request must not kill this listener thread (a real failure mode
        // found this way — see `percent_decode`'s doc comment). A panic here
        // becomes a 500, and the loop moves on to the next request instead.
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            resolve_response(&key, command.as_deref(), shared)
        }));
        let (status, body) = result.unwrap_or_else(|_| {
            eprintln!(
                "[wiim-simulator] internal error handling {} (see panic message above) -> 500",
                command.as_deref().unwrap_or(&key)
            );
            (500, "internal simulator error".to_string())
        });
        eprintln!(
            "[wiim-simulator] {} -> {status}",
            command.as_deref().unwrap_or(&key)
        );
        let response = tiny_http::Response::from_string(body).with_status_code(status).with_header(
            tiny_http::Header::from_bytes(&b"Content-Type"[..], &b"application/json"[..])
                .expect("static header is valid"),
        );
        let _ = request.respond(response);
    }
}
