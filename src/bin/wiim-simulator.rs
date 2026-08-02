//! `wiim-simulator` — replays one or more `wiim-capture` JSON files as a
//! fleet of fake LinkPlay/WiiM HTTP(S) devices, so `rustywiim` (or
//! `wiim-capture` itself) can be pointed at something other than real
//! hardware for testing — including a multiroom group, which needs at least
//! two devices to exercise at all.
//!
//! Usage: `wiim-simulator <capture-file-or-dir>... [--http [N:]PORT]
//! [--https [N:]PORT] [--upnp-port [N:]PORT] [--base-ip IP]
//! [--standard-ports] [--no-upnp] [--no-stateful] [--global] [--keep-config]
//! [--group=N,N[:left|:right],...]`
//!
//! **`--group=1,2,3`** groups devices by their 1-based number, first is the
//! leader; repeat the flag for a second group. An optional per-member
//! `:left`/`:right` suffix sets that member's `channel` value. Static for
//! the whole run — the app itself has no way to form or dissolve a group
//! (only read one and relay volume/mute), so there is no join/leave to
//! simulate. Synthesized on both transports the app can read a group from:
//! the leader's `multiroom:getSlaveList` and every device's `GetInfoEx`
//! (`SlaveFlag`/`MasterUUID`/`SlaveList`) — necessary because WiiM/AudioCast
//! devices default to UPnP-polled access, where the HTTP slave list is
//! never even fetched.
//!
//! Once every device has finished binding, prints a summary of the fleet
//! (name/model/uuid/api/upnp per device) and a ready-to-paste `rustywiim`
//! command line with one `--connect` per device. The printed line includes
//! `--no-config` by default — every run mints fresh uuids, so without it a
//! user's real `config.json` would accumulate throwaway simulator devices
//! (and any group members `rustywiim` auto-adopts) run after run;
//! `--keep-config` drops it, for the runs where testing config persistence
//! itself is the point.
//!
//! **Every non-flag argument is a capture path** — one simulated device per
//! path, in the order given (a directory argument keeps its "newest `.json`
//! inside it" meaning, applied independently per path). Device *n* (1-based)
//! binds `127.0.0.{n+1}` by default (`--base-ip` to change the starting
//! address) — starting at `.2` keeps `127.0.0.1` (whatever the user already
//! runs there) out of the way. Distinct per-device loopback IPs are what let
//! every device serve UPnP on the real well-known port `49152` of its own
//! address simultaneously, so `rustywiim`'s `UpnpClient::discover()` finds
//! each one with no override needed.
//!
//! `--http`/`--https`/`--upnp-port` accept a bare `PORT` or an indexed
//! `N:PORT` (1-based device number). With exactly one capture path, a bare
//! `PORT` applies to that one device — today's meaning, and **cumulative**:
//! repeat the flag for more listeners on that device, all serving the same
//! capture/state. With more than one capture path, a bare `PORT` is a usage
//! error (which device is it for?) — use `N:PORT`, e.g. `--http 2:9090`.
//! Devices with no explicit port default to **random OS-assigned ports**
//! (`port: 0`) — the actually-assigned port is read back from each bound
//! socket and printed as a ready-to-use URL. `--standard-ports` opts every
//! device without an explicit port into `443`/`80` instead, for the day
//! privileges are arranged (`sysctl net.ipv4.ip_unprivileged_port_start=80`
//! or `setcap`) — since each device binds its own IP, several devices can all
//! use `443`/`80` at once with no collision. `--upnp-port` always defaults to
//! `49152` regardless of `--standard-ports` (already unprivileged).
//!
//! Each listener runs on its own OS thread; a bind failure on one listener is
//! logged and skipped rather than aborting the others. A device whose
//! listeners *all* fail to bind is reported and skipped entirely; the
//! process only exits non-zero if *no* device came up at all.
//!
//! **Loopback-only by default** (each device's own `127.0.0.N`) — a safer
//! default for a test tool than exposing it on the LAN. `--global` binds
//! every listener to `0.0.0.0` instead (including the UPnP listener, if any)
//! — restricted to a single capture path, since one host can only ever offer
//! one `0.0.0.0:49152`.
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

/// One simulated device — everything a request-handling thread needs for
/// *this* device, shared read-only (`index`, `upnp`) or behind a `Mutex`
/// (`state`) across every listener thread that serves it, including its own
/// UPnP one, when it exists. A fleet of devices never share any of this —
/// each gets its own `Device`, its own loopback address, and its own state.
struct Device {
    /// 0-based index into `Fleet::devices` — used for per-device log tags
    /// (printed 1-based) and to derive this device's host from `--base-ip`.
    n: usize,
    /// This device's own loopback address, e.g. `"127.0.0.3"` — always its
    /// real identity, even under `--global` (which only changes what address
    /// listeners actually *bind*, not what this device claims to be).
    host: String,
    /// `"Simulated {model} #{n+1}"` — needed because captures are anonymised
    /// (`DeviceName`/`GroupName` are scrubbed to `xxxx…`), so replaying one
    /// verbatim would give every simulated instance the same blank-looking
    /// identity. Patched into any JSON reply whose top-level `DeviceName`/
    /// `GroupName` key is present (`patch_identity_fields()`) and into
    /// `description.xml`'s `<friendlyName>` (`build_upnp_shared()`).
    name: String,
    /// This device's own primary bound API address, `"host:port"` (no
    /// scheme) — `None` only if every one of its listeners failed to bind.
    /// Read by another device's leader-role handling (`synth_slave_list()`)
    /// to fill a member's `ip` field, since a group's slave list is only
    /// ever produced by the *leader* thread reading a fellow `Device`'s
    /// fields, never its own.
    api_addr: Option<String>,
    index: HashMap<String, CommandCapture>,
    state: Mutex<SimState>,
    upnp: Option<UpnpShared>,
    /// This instance's own identity — see `generate_fresh_uuid()`'s doc
    /// comment for why every instance gets one instead of replaying the
    /// capture's frozen value. Patched into every JSON reply that carries a
    /// `uuid`/`upnp_uuid` field (`patch_identity_fields()`) and into the
    /// served `description.xml`'s `<uuid>`/`<UDN>` tags
    /// (`build_upnp_shared()`).
    fresh_uuid: FreshUuid,
}

/// The whole simulated fleet — one process, N devices. `stateful_http` is
/// process-wide (`--no-stateful` affects every device identically) rather
/// than per-device, since it's a testing-mode toggle, not part of any
/// device's simulated identity.
struct Fleet {
    devices: Vec<Arc<Device>>,
    /// Whether the main HTTP(S) command API applies `handle_mutation()`/
    /// `patch_player_status()` — today's default, `--no-stateful` turns it
    /// back off for pure verbatim replay. Doesn't affect the UPnP listener,
    /// which always reads/writes `state` regardless (see this file's module
    /// doc comment).
    stateful_http: bool,
    /// `--group`'s parsed, static topology — empty when no `--group` was
    /// given at all. Static because the app itself has no way to form or
    /// dissolve a group (only read one and relay volume/mute), so there is
    /// no join/leave to honour.
    groups: Vec<Group>,
}

/// One `--group` member: which device (0-based index into `Fleet::devices`)
/// and its channel role, if given (`--group=1,2:left,3:right`) — mapped
/// straight onto the wire `channel` value `device::group::ChannelRole`
/// decodes (`0` stereo, `1` left, `2` right), not a separate concept.
#[derive(Clone, Copy)]
struct GroupMemberSpec {
    dev: usize,
    channel: u8,
}

/// One `--group`'s static topology: `leader` and every `members` entry are
/// 0-based device indices, validated at parse time (see `parse_groups()`)
/// to be in range, in exactly one group, and not also a leader elsewhere.
struct Group {
    leader: usize,
    members: Vec<GroupMemberSpec>,
}

/// A device's role within `fleet.groups`, resolved fresh per request rather
/// than cached — the topology is static (no join/leave), so this is cheap
/// enough to just recompute; caching it would be a second place for it to
/// drift from `fleet.groups` itself.
enum SimRole {
    Standalone,
    /// Index into `fleet.groups`.
    Leader(usize),
    /// The leader's own device index.
    Follower(usize),
}

/// Resolves `dev_idx`'s role within `fleet.groups` — see `SimRole`'s doc
/// comment for why this isn't cached.
fn role_of(fleet: &Fleet, dev_idx: usize) -> SimRole {
    for (gi, g) in fleet.groups.iter().enumerate() {
        if g.leader == dev_idx {
            return SimRole::Leader(gi);
        }
        if g.members.iter().any(|m| m.dev == dev_idx) {
            return SimRole::Follower(g.leader);
        }
    }
    SimRole::Standalone
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

/// Advertises this simulated device's HTTP API address inside the UPnP
/// description, so `--connect <ip>` (a later step) can reach the API
/// without knowing its port — a client that already found the device over
/// UPnP reads this tag instead of guessing. **Only `wiim-simulator` ever
/// emits this** — real LinkPlay firmware serves its API on 80/443 and needs
/// no such advert; this exists purely because the simulator's API listeners
/// default to unprivileged, non-standard ports. Namespaced the same way
/// real firmware namespaces its own extensions (e.g. Tencent's
/// `qq:X_QPlay_SoftwareCapability`, present in every real WiiM capture).
const API_URL_TAG: &str = "X_RustyWiiM_ApiUrl";

/// Builds the UPnP-serving state from whatever `wiim-capture` recorded.
/// `None` when the capture has no `description.xml` at all — nothing to
/// serve, so no UPnP listener is started (see `main()`). `fresh_uuid`'s
/// `<uuid>`/`<UDN>`, `name`'s `<friendlyName>`, and `api_url`'s advert tag
/// are all patched in here, once, rather than per-request —
/// `description.xml` is served verbatim otherwise, so this is the one place
/// that needs to happen. `api_url` is a full `scheme://host:port` (not a
/// bare port) so the tag is self-describing when read by hand and so a
/// future consumer can resolve a `TlsMode` from it directly.
fn build_upnp_shared(capture: &CaptureFile, name: &str, fresh_uuid: &FreshUuid, api_url: &str) -> Option<UpnpShared> {
    let upnp = capture.upnp.as_ref()?;
    let description_xml = upnp.description.as_ref()?.body.as_str()?.to_string();
    let description_xml = patch_tag(&description_xml, "uuid", &fresh_uuid.plain);
    let description_xml = patch_tag(&description_xml, "UDN", &fresh_uuid.dashed);
    let description_xml = patch_tag(&description_xml, "friendlyName", name);
    let advert = format!(
        "<sim:{API_URL_TAG} xmlns:sim=\"https://github.com/rustywiim/simulator\">{api_url}</sim:{API_URL_TAG}>"
    );
    let description_xml = match description_xml.find("</device>") {
        Some(pos) => format!("{}{advert}{}", &description_xml[..pos], &description_xml[pos..]),
        None => description_xml,
    };
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

/// Like `patch_tag()`, but inserts `<tag>value</tag>` immediately before the
/// literal `before` marker when `tag` isn't already present in `xml`,
/// instead of returning it unchanged. Needed for `--group`'s UPnP tags
/// (`SlaveFlag`/`MasterUUID`/`SlaveList`): a capture from a device that
/// never reported one of these (most don't, absent a real group at capture
/// time) must still be able to act as a follower or leader.
fn patch_or_insert_tag(xml: &str, tag: &str, value: &str, before: &str) -> String {
    if xml.contains(&format!("<{tag}>")) {
        return patch_tag(xml, tag, value);
    }
    match xml.find(before) {
        Some(pos) => format!("{}<{tag}>{value}</{tag}>{}", &xml[..pos], &xml[pos..]),
        None => xml.to_string(),
    }
}

/// Escapes text for embedding inside XML element content (not an
/// attribute) — the mirror of `device/upnp.rs`'s own private
/// `unescape_xml_entities()`, used here to embed `SlaveList`'s JSON inside
/// `<SlaveList>...</SlaveList>`, matching how real firmware escapes the same
/// field. `&` first, so it doesn't double-escape the entities it just
/// produced.
fn escape_xml_text(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

/// The exact body a real follower (or standalone device) returns for
/// `multiroom:getSlaveList`, and the `<SlaveList>` UPnP tag value on every
/// non-leader — shared so both transports report an identical "not a
/// leader" shape.
const EMPTY_SLAVE_LIST_JSON: &str = r#"{"group_type":-1,"slaves":0,"surround":0,"wmrm_version":"4.3"}"#;

/// Answers the fixed set of UPnP SOAP actions this app itself makes (see
/// this file's module doc comment). `body` is the raw incoming SOAP request
/// XML (only actually read for `SetMute`/`SetQueueLoopMode`, the only two
/// that carry an argument). Returns `None` for anything outside this set,
/// so the caller replies with a visible error instead of a wrong guess.
fn handle_soap_action(service: &str, action: &str, body: &str, fleet: &Fleet, dev_idx: usize) -> Option<String> {
    let dev = &fleet.devices[dev_idx];
    let upnp = dev.upnp.as_ref()?;
    match action {
        "GetInfoEx" => {
            let template = upnp.info_ex_template.as_deref()?;
            // Own state lock is taken and dropped *before* any group
            // synthesis below might lock a fellow device's state — this
            // file's one hard rule against ever holding two `SimState`
            // locks at once (a crossing relay across two group members
            // would otherwise deadlock with no error message).
            let (vol, mute, loop_mode) = {
                let state = dev.state.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
                (state.vol, state.mute, state.loop_mode.clone())
            };
            let xml = patch_tag(template, "CurrentVolume", &vol.to_string());
            let xml = patch_tag(&xml, "CurrentMute", if mute { "1" } else { "0" });
            let xml = patch_tag(&xml, "LoopMode", &loop_mode);

            // `--group`'s UPnP side (the one that matters most: WiiM/
            // AudioCast devices default to UPnP-polled access, where the
            // HTTP slave list is never even fetched, so this is the *only*
            // place their group topology comes from). Every role, including
            // Standalone, gets these three tags overwritten — never only
            // "when grouped" — for the same stale-capture reason
            // `patch_group_status_fields()` does on the HTTP side.
            let (slave_flag, master_uuid, slave_list_json) = match role_of(fleet, dev_idx) {
                SimRole::Leader(gi) => {
                    ("0", String::new(), synth_slave_list(fleet, &fleet.groups[gi]).to_string())
                }
                SimRole::Follower(leader_idx) => {
                    ("1", fleet.devices[leader_idx].fresh_uuid.plain.clone(), EMPTY_SLAVE_LIST_JSON.to_string())
                }
                SimRole::Standalone => ("0", String::new(), EMPTY_SLAVE_LIST_JSON.to_string()),
            };
            let xml = patch_or_insert_tag(&xml, "SlaveFlag", slave_flag, "</u:GetInfoExResponse>");
            let xml = patch_or_insert_tag(&xml, "MasterUUID", &master_uuid, "</u:GetInfoExResponse>");
            let xml = patch_or_insert_tag(
                &xml,
                "SlaveList",
                &escape_xml_text(&slave_list_json),
                "</u:GetInfoExResponse>",
            );
            Some(xml)
        }
        "GetMute" => {
            let state = dev.state.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
            Some(soap_envelope(
                service,
                action,
                &format!("<CurrentMute>{}</CurrentMute>", if state.mute { "1" } else { "0" }),
            ))
        }
        "SetMute" => {
            let mut state = dev.state.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
            if let Some(v) = extract_tag(body, "DesiredMute") {
                state.mute = v.trim() == "1";
            }
            Some(soap_envelope(service, action, ""))
        }
        "GetQueueLoopMode" => {
            let state = dev.state.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
            Some(soap_envelope(service, action, &format!("<LoopMode>{}</LoopMode>", state.loop_mode)))
        }
        "SetQueueLoopMode" => {
            let mut state = dev.state.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
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
/// Takes the whole fleet plus this listener's device index (not just its
/// `Device` directly) purely so the log tag below can name the device — the
/// UPnP handling itself only ever touches `fleet.devices[dev_idx]`.
fn serve_upnp(server: tiny_http::Server, fleet: &Arc<Fleet>, dev_idx: usize) {
    let dev = &fleet.devices[dev_idx];
    let upnp = dev.upnp.as_ref().expect("caller only starts this listener when upnp is Some");
    let tag = format!("[wiim-simulator][{}]", dev.n + 1);
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
                    Some((service, action)) => match handle_soap_action(service, action, &body, fleet, dev_idx) {
                        Some(xml) => (200, xml, "text/xml"),
                        None => (500, format!("simulator: no response modeled for {action}"), "text/plain"),
                    },
                    None => (400, "missing/malformed SOAPACTION header".to_string(), "text/plain"),
                };
            }
            (404, String::new(), "text/plain")
        }));
        let (status, body, content_type) = result.unwrap_or_else(|_| {
            eprintln!("{tag} internal error handling UPnP request (see panic message above) -> 500");
            (500, "internal simulator error".to_string(), "text/plain")
        });
        eprintln!(
            "{tag} upnp {} {} -> {status}",
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

/// Loads a capture file, returning its resolved file name (just the final
/// path component — a directory argument resolves to whichever `.json`
/// inside it was actually picked) alongside the parsed `CaptureFile`, for
/// `main()`'s banner (step 3) to display without re-deriving the directory
/// resolution logic below.
fn load_capture(path: &std::path::Path) -> (String, CaptureFile) {
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
    let file_name = file_path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    (file_name, capture)
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

/// Rewrites a JSON body's top-level `uuid`/`upnp_uuid`/`DeviceName`/
/// `GroupName` fields to this device's own fresh identity, for whichever of
/// the four keys are actually present — never adds a field the real
/// response didn't have. Needed because captures are anonymised: `uuid`/
/// `upnp_uuid` are scrubbed to `xxxx…` (see `FreshUuid`'s doc comment), and
/// `DeviceName`/`GroupName` are often scrubbed too (see `Device::name`'s doc
/// comment) — replaying either verbatim would make every simulated instance
/// of the same capture look identical. A no-op for anything that isn't
/// valid JSON, or a JSON reply with none of these four keys (most captured
/// commands have none).
fn patch_identity_fields(body: &str, dev: &Device) -> String {
    let Ok(mut value) = serde_json::from_str::<serde_json::Value>(body) else { return body.to_string() };
    let Some(obj) = value.as_object_mut() else { return body.to_string() };
    let mut patched = false;
    if obj.contains_key("uuid") {
        obj.insert("uuid".into(), serde_json::Value::String(dev.fresh_uuid.plain.clone()));
        patched = true;
    }
    if obj.contains_key("upnp_uuid") {
        obj.insert("upnp_uuid".into(), serde_json::Value::String(dev.fresh_uuid.dashed.clone()));
        patched = true;
    }
    if obj.contains_key("DeviceName") {
        obj.insert("DeviceName".into(), serde_json::Value::String(dev.name.clone()));
        patched = true;
    }
    if obj.contains_key("GroupName") {
        obj.insert("GroupName".into(), serde_json::Value::String(dev.name.clone()));
        patched = true;
    }
    if patched { value.to_string() } else { body.to_string() }
}

/// Replays a captured entry faithfully: `outcome == Ok` serves the recorded
/// body at its recorded (or default 200) status; anything else (the command
/// itself failed at capture time) has no real body to replay, so it serves
/// the recorded status if there is one, else a generic 500 — still a
/// response, but visibly not a real one. JSON replies get their identity
/// fields patched to this device's own fresh identity (see
/// `patch_identity_fields()`) — unconditionally, regardless of
/// `--no-stateful`, since a fresh per-instance identity isn't "mini-device
/// simulation," it's basic test-tool correctness (two simulator instances
/// replaying the same capture file must not present as the same device).
fn handle_command(cap: &CommandCapture, dev: &Device) -> (u16, String) {
    if cap.outcome == Outcome::Ok {
        let body = render_body(cap);
        let body = if cap.format == Some(ResponseFormat::Json) {
            patch_identity_fields(&body, dev)
        } else {
            body
        };
        return (cap.http_status.unwrap_or(200), body);
    }
    (cap.http_status.unwrap_or(500), String::new())
}

/// Primary well-known LinkPlay UPnP port — matches `device::upnp`'s own
/// `DESCRIPTION_PORTS[0]`. See this file's module doc comment for why the
/// UPnP listener needs to be on this exact port (or `59152`, the other of
/// the two) to be discoverable by `rustywiim` at all.
const DEFAULT_UPNP_PORT: u16 = 49152;

/// `--base-ip`'s default — device *n* (1-based) binds `127.0.0.{n+1}`.
/// Starting at `.2` keeps `127.0.0.1` (whatever the user already runs there)
/// out of the way.
const DEFAULT_BASE_IP: std::net::Ipv4Addr = std::net::Ipv4Addr::new(127, 0, 0, 2);

/// One `--http`/`--https`/`--upnp-port` occurrence, parsed but not yet
/// resolved against how many devices were actually given — `None` means "no
/// `N:` prefix," valid only when there's exactly one capture path.
type PortSpec = (Option<usize>, u16);

struct Args {
    paths: Vec<String>,
    http: Vec<PortSpec>,
    https: Vec<PortSpec>,
    upnp_port: Vec<PortSpec>,
    base_ip: std::net::Ipv4Addr,
    standard_ports: bool,
    no_stateful: bool,
    no_upnp: bool,
    global: bool,
    keep_config: bool,
    /// Raw `--group=1,2,3` values (one per occurrence, repeatable for a
    /// second group), parsed by `parse_groups()` once `ndevices` is known.
    groups: Vec<String>,
}

const USAGE_LINE: &str = "Usage: wiim-simulator <capture-file-or-dir>... [OPTION]...";

/// One `--help` table entry: `flag` is the left column exactly as typed
/// (including its metavariable, e.g. `"--http [N:]PORT"`); `desc` is
/// plain prose, wrapped to fit the terminal by `print_help()` — no manual
/// line breaks or alignment inside `desc` itself.
struct OptHelp {
    flag: &'static str,
    desc: &'static str,
}

const OPTIONS: &[OptHelp] = &[
    OptHelp {
        flag: "--http [N:]PORT",
        desc: "HTTP listener port for device N (1-based). A bare PORT (no \"N:\") is only \
               valid with exactly one capture path. Cumulative per device — repeat for more \
               listeners on the same device. A device given neither --http nor --https \
               defaults to one of each, on a random OS-assigned port.",
    },
    OptHelp {
        flag: "--https [N:]PORT",
        desc: "HTTPS listener port for device N — same rules as --http.",
    },
    OptHelp {
        flag: "--upnp-port [N:]PORT",
        desc: "UPnP listener port for device N. Defaults to 49152 (the port rustywiim's own \
               UpnpClient looks for) regardless of --standard-ports, since it's already \
               unprivileged.",
    },
    OptHelp {
        flag: "--base-ip IP",
        desc: "Device 1's loopback address (default 127.0.0.2). Device n binds base_ip + \
               (n-1).",
    },
    OptHelp {
        flag: "--standard-ports",
        desc: "Use 443/80 instead of a random port for any device given no explicit --http/\
               --https.",
    },
    OptHelp {
        flag: "--no-upnp",
        desc: "Disable the UPnP listener entirely.",
    },
    OptHelp {
        flag: "--no-stateful",
        desc: "Disable the built-in mini-device simulation (on by default) in favor of pure \
               verbatim capture replay.",
    },
    OptHelp {
        flag: "--global",
        desc: "Listen on 0.0.0.0 instead of loopback. Single capture path only — one host can \
               only offer one 0.0.0.0:49152.",
    },
    OptHelp {
        flag: "--keep-config",
        desc: "Drop --no-config from the printed rustywiim command line. Omitted by default \
               because every run mints fresh uuids, which would otherwise accumulate in a \
               real config.json run after run.",
    },
    OptHelp {
        flag: "--group=N,N[:left|:right],...",
        desc: "Group devices into a multiroom fleet (1-based device numbers, first is the \
               leader). Repeat for a second group. An optional :left/:right suffix on a \
               member sets its channel.",
    },
    OptHelp {
        flag: "-h, --help",
        desc: "Show this help and exit.",
    },
];

/// Column where every description starts; a `flag` longer than this puts its
/// description on the following line instead of sharing the flag's own line
/// (same convention `ls --help` uses for its longer GNU-style options).
const HELP_COL: usize = 26;
/// Assumed terminal width — this tool has no isatty/COLUMNS probe, so it
/// just targets the conventional 80.
const TERM_WIDTH: usize = 80;

/// Greedy word-wrap: fills each line up to `width` without splitting words.
/// No hyphenation, no unicode-width awareness — plain ASCII option text only.
fn wrap_text(text: &str, width: usize) -> Vec<String> {
    let mut lines = Vec::new();
    let mut current = String::new();
    for word in text.split_whitespace() {
        if current.is_empty() {
            current.push_str(word);
        } else if current.len() + 1 + word.len() <= width {
            current.push(' ');
            current.push_str(word);
        } else {
            lines.push(std::mem::take(&mut current));
            current.push_str(word);
        }
    }
    if !current.is_empty() {
        lines.push(current);
    }
    lines
}

fn print_option(opt: &OptHelp) {
    let indent = " ".repeat(HELP_COL);
    let desc_width = TERM_WIDTH.saturating_sub(HELP_COL).max(20);
    let mut lines = wrap_text(opt.desc, desc_width).into_iter();
    let first = lines.next().unwrap_or_default();
    if opt.flag.len() + 2 <= HELP_COL {
        println!("  {:<width$}{first}", opt.flag, width = HELP_COL - 2);
    } else {
        println!("  {}", opt.flag);
        println!("{indent}{first}");
    }
    for line in lines {
        println!("{indent}{line}");
    }
}

/// `-h`/`--help`: full, formatted help on stdout, exit 0.
fn print_help() -> ! {
    println!("{USAGE_LINE}");
    println!();
    for line in wrap_text(
        "Replays one or more wiim-capture JSON files as a fleet of fake LinkPlay/WiiM HTTP(S) \
         devices — for pointing rustywiim (or wiim-capture itself) at something other than \
         real hardware, including a multiroom group.",
        TERM_WIDTH,
    ) {
        println!("{line}");
    }
    println!();
    for line in wrap_text(
        "Every non-flag argument is a capture path — one simulated device per path (a \
         directory picks the newest .json inside it).",
        TERM_WIDTH,
    ) {
        println!("{line}");
    }
    println!();
    println!("Options:");
    for opt in OPTIONS {
        print_option(opt);
    }
    std::process::exit(0);
}

/// A bad invocation: a short usage line + pointer to `--help`, on stderr,
/// exit 2 — `print_help()`'s full table would drown out `msg` otherwise.
fn usage_error(msg: &str) -> ! {
    eprintln!("wiim-simulator: {msg}");
    eprintln!("{USAGE_LINE}");
    eprintln!("Try 'wiim-simulator --help' for more information.");
    std::process::exit(2);
}

/// Parses one `--http`/`--https`/`--upnp-port` argument value: a bare `PORT`
/// or an indexed `N:PORT` (1-based device number, returned 0-based). Pure —
/// returns the usage-error message rather than exiting, so it's directly
/// unit-testable; `parse_port_spec()` below is the CLI-facing wrapper that
/// exits on `Err`.
fn try_parse_port_spec(flag: &str, spec: &str) -> Result<PortSpec, String> {
    if let Some((n, p)) = spec.split_once(':') {
        let idx: usize = n.parse().map_err(|_| format!("{flag} device index '{n}' is not a number"))?;
        if idx == 0 {
            return Err(format!("{flag} device indices are 1-based"));
        }
        let port: u16 = p.parse().map_err(|_| format!("{flag} port '{p}' is not a number"))?;
        Ok((Some(idx - 1), port))
    } else {
        let port: u16 = spec
            .parse()
            .map_err(|_| format!("{flag} requires a port number (or N:PORT)"))?;
        Ok((None, port))
    }
}

fn parse_port_spec(flag: &str, spec: &str) -> PortSpec {
    try_parse_port_spec(flag, spec).unwrap_or_else(|msg| usage_error(&msg))
}

fn parse_args() -> Args {
    let mut paths = Vec::new();
    let mut http = Vec::new();
    let mut https = Vec::new();
    let mut upnp_port = Vec::new();
    let mut base_ip = DEFAULT_BASE_IP;
    let mut standard_ports = false;
    let mut no_stateful = false;
    let mut no_upnp = false;
    let mut global = false;
    let mut keep_config = false;
    let mut groups = Vec::new();
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--http" | "--https" => {
                let spec = args.next()
                    .unwrap_or_else(|| usage_error(&format!("{arg} requires a port number (or N:PORT)")));
                let parsed = parse_port_spec(&arg, &spec);
                if arg == "--http" { http.push(parsed) } else { https.push(parsed) };
            }
            "--upnp-port" => {
                let spec = args.next()
                    .unwrap_or_else(|| usage_error("--upnp-port requires a port number (or N:PORT)"));
                upnp_port.push(parse_port_spec("--upnp-port", &spec));
            }
            "--base-ip" => {
                let spec = args.next()
                    .unwrap_or_else(|| usage_error("--base-ip requires an IPv4 address"));
                base_ip = spec.parse().unwrap_or_else(|_| {
                    usage_error(&format!("--base-ip '{spec}' is not a valid IPv4 address"))
                });
            }
            "--standard-ports" => standard_ports = true,
            "--no-upnp" => no_upnp = true,
            // Accepted (as a no-op) for anyone used to the old opt-in flag —
            // stateful is the default now, nothing left for it to enable.
            "--stateful" => {}
            "--no-stateful" => no_stateful = true,
            "--global" => global = true,
            "--keep-config" => keep_config = true,
            "-h" | "--help" => print_help(),
            other if other.starts_with("--group=") => groups.push(other["--group=".len()..].to_string()),
            other if !other.starts_with('-') => paths.push(other.to_string()),
            other => usage_error(&format!("unrecognized argument '{other}'")),
        }
    }
    if paths.is_empty() {
        usage_error("no capture path given");
    }
    if global && paths.len() > 1 {
        usage_error("--global only supports a single capture path (one host can only offer one 0.0.0.0:49152)");
    }
    Args {
        paths, http, https, upnp_port, base_ip, standard_ports,
        no_stateful, no_upnp, global, keep_config, groups,
    }
}

/// Parses every `--group=...` occurrence into validated, 0-based `Group`
/// topology. `--group=1,2,3` — 1-based device numbers, first is the leader;
/// repeatable for a second group. An optional per-member `:left`/`:right`
/// suffix (`--group=1,2:left,3:right`) sets that member's wire `channel`
/// value (see `GroupMemberSpec`'s doc comment). Pure — see
/// `try_parse_port_spec()`'s doc comment for why; `parse_groups()` below is
/// the CLI-facing wrapper.
fn try_parse_groups(raw: &[String], ndevices: usize) -> Result<Vec<Group>, String> {
    let mut groups = Vec::new();
    let mut assigned: std::collections::HashSet<usize> = std::collections::HashSet::new();
    for spec in raw {
        let mut entries: Vec<(usize, u8)> = Vec::new();
        for token in spec.split(',') {
            let (num_str, channel_str) = match token.split_once(':') {
                Some((n, c)) => (n, Some(c)),
                None => (token, None),
            };
            let n: usize = num_str
                .parse()
                .map_err(|_| format!("--group '{token}' is not a device number"))?;
            if n == 0 {
                return Err("--group device numbers are 1-based".to_string());
            }
            let dev = n - 1;
            if dev >= ndevices {
                return Err(format!(
                    "--group device {n} out of range (only {ndevices} capture path(s) given)"
                ));
            }
            let channel = match channel_str {
                None => 0,
                Some("left") => 1,
                Some("right") => 2,
                Some(other) => return Err(format!("--group channel '{other}' is not 'left' or 'right'")),
            };
            entries.push((dev, channel));
        }
        if entries.len() < 2 {
            return Err(format!("--group '{spec}' needs at least two devices (a leader and a follower)"));
        }
        for &(dev, _) in &entries {
            // Catches every overlap in one check: a device repeated within
            // this group, reused as a leader/member of an earlier one, or
            // both a leader in one group and a member in another — `assigned`
            // accumulates every device index across every group, leader
            // included.
            if !assigned.insert(dev) {
                return Err(format!("--group: device {} is in more than one group", dev + 1));
            }
        }
        let leader = entries[0].0;
        let members = entries[1..].iter().map(|&(dev, channel)| GroupMemberSpec { dev, channel }).collect();
        groups.push(Group { leader, members });
    }
    Ok(groups)
}

fn parse_groups(raw: Vec<String>, ndevices: usize) -> Vec<Group> {
    try_parse_groups(&raw, ndevices).unwrap_or_else(|msg| usage_error(&msg))
}

/// Groups parsed `--http`/`--https`/`--upnp-port` occurrences by device
/// index, validating each against `ndevices`. A bare (unindexed) spec is
/// only accepted when there's exactly one device, where it unambiguously
/// means that device — otherwise it's a usage error, never a guess. Pure —
/// see `try_parse_port_spec()`'s doc comment for why; `group_by_device()`
/// below is the CLI-facing wrapper.
fn try_group_by_device(specs: Vec<PortSpec>, ndevices: usize, flag: &str) -> Result<Vec<Vec<u16>>, String> {
    let mut out: Vec<Vec<u16>> = vec![Vec::new(); ndevices];
    for (idx, port) in specs {
        let idx = match idx {
            Some(i) => i,
            None if ndevices == 1 => 0,
            None => {
                return Err(format!("{flag} needs a device index with several captures, e.g. {flag} 2:9090"));
            }
        };
        if idx >= ndevices {
            return Err(format!(
                "{flag} device index {} out of range (only {ndevices} capture path(s) given)",
                idx + 1
            ));
        }
        out[idx].push(port);
    }
    Ok(out)
}

fn group_by_device(specs: Vec<PortSpec>, ndevices: usize, flag: &str) -> Vec<Vec<u16>> {
    try_group_by_device(specs, ndevices, flag).unwrap_or_else(|msg| {
        eprintln!("wiim-simulator: {msg}");
        std::process::exit(2);
    })
}

/// Device `i`'s (0-based) loopback address: `base_ip + i` in the last octet.
fn host_for(base_ip: std::net::Ipv4Addr, i: usize) -> String {
    let octets = base_ip.octets();
    let last = octets[3] as usize + i;
    format!("{}.{}.{}.{}", octets[0], octets[1], octets[2], last)
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

/// Per-device resolved port lists — `[]` for a scheme means "no listener of
/// this kind for this device," not "use a default"; defaults are only
/// filled in for a device that got *no* explicit `--http`/`--https` at all
/// (see the loop building this in `main()`), matching the original
/// single-device tool's "neither given" meaning generalized per device.
struct DevicePorts {
    http: Vec<u16>,
    https: Vec<u16>,
}

fn main() {
    let args = parse_args();
    let loaded: Vec<(String, CaptureFile)> =
        args.paths.iter().map(|p| load_capture(std::path::Path::new(p))).collect();
    let source_names: Vec<&str> = loaded.iter().map(|(name, _)| name.as_str()).collect();
    let captures: Vec<&CaptureFile> = loaded.iter().map(|(_, c)| c).collect();
    let ndevices = captures.len();
    let groups = parse_groups(args.groups, ndevices);

    let http_per_device = group_by_device(args.http, ndevices, "--http");
    let https_per_device = group_by_device(args.https, ndevices, "--https");
    let upnp_port_per_device = group_by_device(args.upnp_port, ndevices, "--upnp-port");

    let default_http_port = if args.standard_ports { 80 } else { 0 };
    let default_https_port = if args.standard_ports { 443 } else { 0 };
    let ports: Vec<DevicePorts> = (0..ndevices)
        .map(|i| {
            let mut http = http_per_device[i].clone();
            let mut https = https_per_device[i].clone();
            if http.is_empty() && https.is_empty() {
                http.push(default_http_port);
                https.push(default_https_port);
            }
            DevicePorts { http, https }
        })
        .collect();
    // UPnP always defaults to the well-known port regardless of
    // --standard-ports (it's already unprivileged); `None` means --no-upnp.
    let upnp_ports: Vec<Option<u16>> = (0..ndevices)
        .map(|i| (!args.no_upnp).then(|| upnp_port_per_device[i].last().copied().unwrap_or(DEFAULT_UPNP_PORT)))
        .collect();

    // One certificate, shared by every `--https` listener across the whole
    // fleet — generating it eagerly (only if actually needed) avoids paying
    // for it when every listener in the fleet is plain HTTP.
    let cert = ports.iter().any(|p| !p.https.is_empty()).then(generate_self_signed_cert);

    // Phase 1: bind every device's HTTP(S) API listeners *before* building
    // any `Device` — description.xml's API advert (built in phase 2) needs
    // to know the real bound address, which for an OS-assigned port (0)
    // only exists after the bind actually happens.
    struct BoundListener {
        https: bool,
        port: u16,
        server: tiny_http::Server,
    }
    let mut bound: Vec<Vec<BoundListener>> = Vec::with_capacity(ndevices);
    for i in 0..ndevices {
        let tag = format!("[wiim-simulator][{}]", i + 1);
        let host = host_for(args.base_ip, i);
        let bind_host = if args.global { "0.0.0.0".to_string() } else { host };
        let mut listeners = Vec::new();
        for &port in &ports[i].http {
            let addr = format!("{bind_host}:{port}");
            match tiny_http::Server::http(&addr) {
                Ok(server) => {
                    let bound_port = server.server_addr().to_ip().map(|a| a.port()).unwrap_or(port);
                    listeners.push(BoundListener { https: false, port: bound_port, server });
                }
                Err(e) => report_bind_failure(&tag, "http", &addr, &e, args.standard_ports),
            }
        }
        for &port in &ports[i].https {
            let addr = format!("{bind_host}:{port}");
            let cert = cert.as_ref().expect("cert generated above whenever an https listener exists");
            let ssl_config = tiny_http::SslConfig {
                certificate: cert.cert.pem().into_bytes(),
                private_key: cert.signing_key.serialize_pem().into_bytes(),
            };
            match tiny_http::Server::https(&addr, ssl_config) {
                Ok(server) => {
                    let bound_port = server.server_addr().to_ip().map(|a| a.port()).unwrap_or(port);
                    listeners.push(BoundListener { https: true, port: bound_port, server });
                }
                Err(e) => report_bind_failure(&tag, "https", &addr, &e, args.standard_ports),
            }
        }
        bound.push(listeners);
    }

    // Phase 2: build each `Device` now that its bound API address(es) are
    // known, so `build_upnp_shared()` can patch the real address into
    // description.xml's advert. Prefers an http listener over https for the
    // advert (fewer moving parts for a client resolving it).
    let mut devices = Vec::with_capacity(ndevices);
    // Collected alongside each `Device`, purely for the banner (below): every
    // bound API address ("api" line) and the one preferred as this device's
    // primary address — the same one patched into description.xml's advert
    // and the one the printed --connect command line uses.
    let mut all_api_urls: Vec<Vec<String>> = Vec::with_capacity(ndevices);
    let mut primary_api_url: Vec<Option<String>> = Vec::with_capacity(ndevices);
    for (i, capture) in captures.iter().enumerate() {
        let index = index_by_path(capture);
        let state = Mutex::new(init_state(capture));
        let fresh_uuid = FreshUuid::new();
        let host = host_for(args.base_ip, i);
        let name = format!("Simulated {} #{}", capture.model, i + 1);
        eprintln!(
            "[wiim-simulator][{}] identity: {name} uuid={} upnp_uuid={} host={host}",
            i + 1, fresh_uuid.plain, fresh_uuid.dashed,
        );
        all_api_urls.push(
            bound[i]
                .iter()
                .map(|l| format!("{}://{host}:{}", if l.https { "https" } else { "http" }, l.port))
                .collect(),
        );
        let api_listener = bound[i].iter().find(|l| !l.https).or_else(|| bound[i].first());
        let api_addr = api_listener.map(|l| format!("{host}:{}", l.port));
        let this_primary_url =
            api_listener.map(|l| format!("{}://{host}:{}", if l.https { "https" } else { "http" }, l.port));
        let upnp = match (upnp_ports[i].is_some(), &this_primary_url) {
            (true, Some(api_url)) => build_upnp_shared(capture, &name, &fresh_uuid, api_url),
            _ => None,
        };
        primary_api_url.push(this_primary_url);
        devices.push(Arc::new(Device { n: i, host, name, api_addr, index, state, upnp, fresh_uuid }));
    }
    let fleet = Arc::new(Fleet { devices, stateful_http: !args.no_stateful, groups });

    // Phase 3: spawn threads for the listeners already bound in phase 1
    // (now that `Fleet` exists to hand them), then bind+spawn each device's
    // UPnP listener, since that depends on `Device.upnp` from phase 2.
    let mut handles = Vec::new();
    let mut any_device_bound = false;
    for (i, listeners) in bound.into_iter().enumerate() {
        let dev = &fleet.devices[i];
        let tag = format!("[wiim-simulator][{}]", i + 1);
        let bind_host = if args.global { "0.0.0.0".to_string() } else { dev.host.clone() };
        let mut device_bound = false;

        for l in listeners {
            eprintln!(
                "{tag} serving {} on {}://{bind_host}:{}{}",
                captures[i].model,
                if l.https { "https" } else { "http" },
                l.port,
                if fleet.stateful_http { ", stateful mini-device on" } else { "" }
            );
            let fleet = Arc::clone(&fleet);
            let server = l.server;
            handles.push(std::thread::spawn(move || serve(server, &fleet, i)));
            device_bound = true;
        }

        match (upnp_ports[i], dev.upnp.is_some()) {
            (Some(port), true) => {
                let addr = format!("{bind_host}:{port}");
                match tiny_http::Server::http(&addr) {
                    Ok(server) => {
                        eprintln!("{tag} serving UPnP on http://{bind_host}:{port}");
                        let fleet = Arc::clone(&fleet);
                        handles.push(std::thread::spawn(move || serve_upnp(server, &fleet, i)));
                    }
                    Err(e) => {
                        eprintln!(
                            "{tag} failed to bind UPnP listener on {addr}: {e} — skipping it \
                             (GetInfoEx/GetMute/SetMute/GetQueueLoopMode/SetQueueLoopMode won't be reachable)"
                        );
                    }
                }
            }
            (Some(_), false) => eprintln!("{tag} no UPnP data in this capture — no UPnP listener started"),
            (None, _) => {}
        }

        if device_bound {
            any_device_bound = true;
        } else {
            eprintln!("{tag} no listener could bind for this device — skipping it entirely");
        }
    }

    if !any_device_bound {
        eprintln!("[wiim-simulator] no device came up, exiting");
        std::process::exit(1);
    }

    print_banner(&fleet, &captures, &source_names, &all_api_urls, &primary_api_url, &upnp_ports, args.keep_config);

    for handle in handles {
        let _ = handle.join();
    }
}

/// Prints a one-time summary of the fleet plus a paste-ready `rustywiim`
/// command line, once every device has finished binding. `--keep-config`
/// drops the emitted line's `--no-config`: every run mints fresh uuids, and
/// `--no-config` is what keeps those (and any group members `rustywiim`
/// adopts) out of the user's real `config.json` — the escape hatch exists
/// only for the runs where testing per-device config overrides is the
/// point.
#[allow(clippy::too_many_arguments)]
fn print_banner(
    fleet: &Fleet,
    captures: &[&CaptureFile],
    source_names: &[&str],
    all_api_urls: &[Vec<String>],
    primary_api_url: &[Option<String>],
    upnp_ports: &[Option<u16>],
    keep_config: bool,
) {
    eprintln!();
    for (i, dev) in fleet.devices.iter().enumerate() {
        eprintln!("[wiim-simulator] device {}  {}", i + 1, dev.name);
        eprintln!("                 model     {}  ({})", captures[i].model, source_names[i]);
        eprintln!("                 uuid      {}", dev.fresh_uuid.plain);
        if all_api_urls[i].is_empty() {
            eprintln!("                 api       (none — every listener failed to bind)");
        } else {
            eprintln!("                 api       {}", all_api_urls[i].join("   "));
        }
        if let (Some(port), true) = (upnp_ports[i], dev.upnp.is_some()) {
            eprintln!("                 upnp      http://{}:{port}", dev.host);
        }
    }
    for (gi, g) in fleet.groups.iter().enumerate() {
        let followers: Vec<String> = g.members.iter().map(|m| format!("#{}", m.dev + 1)).collect();
        eprintln!(
            "[wiim-simulator] group {}   leader #{}, followers {}",
            gi + 1, g.leader + 1, followers.join(", ")
        );
    }

    eprintln!();
    eprintln!("[wiim-simulator] run rustywiim against this fleet with:");
    eprintln!();
    // The bare-IP form: `rustywiim` resolves the API address itself from
    // this device's UPnP advert (`device::upnp::discover_api_address()`),
    // so nothing about the API's actual port needs to appear on the command
    // line at all. Only when this device has no UPnP data (nothing for that
    // resolution to read) does the banner fall back to the explicit
    // `scheme://host:port` form.
    let connect_args: Vec<String> = (0..fleet.devices.len())
        .filter_map(|i| {
            if fleet.devices[i].upnp.is_some() {
                Some(format!("--connect {}", fleet.devices[i].host))
            } else {
                primary_api_url[i].as_ref().map(|u| format!("--connect {u}"))
            }
        })
        .collect();
    let no_config = if keep_config { "" } else { "--no-config " };
    eprintln!("  target/debug/rustywiim {no_config}{}", connect_args.join(" "));
    eprintln!();
}

/// Logs one HTTP(S) listener bind failure, with an extra remedy hint when
/// it's plausibly `--standard-ports` hitting an unprivileged-port
/// restriction (`EACCES`) — the one failure mode worth telling the user how
/// to actually fix, rather than just "it didn't work."
fn report_bind_failure(
    tag: &str,
    scheme: &str,
    addr: &str,
    e: &Box<dyn std::error::Error + Send + Sync>,
    standard_ports: bool,
) {
    eprintln!("{tag} failed to bind {scheme} on {addr}: {e} — skipping this listener");
    let is_permission_denied = e
        .downcast_ref::<std::io::Error>()
        .is_some_and(|io_err| io_err.kind() == std::io::ErrorKind::PermissionDenied);
    if standard_ports && is_permission_denied {
        eprintln!("{tag} fix: sudo sysctl -w net.ipv4.ip_unprivileged_port_start=80");
        eprintln!("{tag} or drop --standard-ports to use OS-assigned ports instead");
    }
}

/// Resolves one request to a `(status, body)` reply for device `dev_idx` of
/// `fleet`. While `fleet.stateful_http` is set (the default) and the
/// request's `command=` value is a recognized mutator, `handle_mutation`
/// handles it entirely (under this device's own `state` `Mutex`, shared with
/// its own UPnP listener); `getPlayerStatusEx`/`getPlayerStatus` get the
/// current state patched in. Everything else (always, regardless of
/// `stateful_http`) falls through to plain replay from this device's own
/// `index`, keyed by the request's full path+query.
fn resolve_response(key: &str, command: Option<&str>, fleet: &Fleet, dev_idx: usize) -> (u16, String) {
    let dev = &fleet.devices[dev_idx];
    // `--group`'s HTTP side, ahead of everything else: explicit topology the
    // user asked for, so it applies regardless of `--no-stateful` (unlike
    // `handle_mutation()`'s mini-device simulation, this isn't optional
    // fidelity — a real device answers `multiroom:getSlaveList` the same way
    // whether or not it's also simulating playback state).
    if command == Some("multiroom:getSlaveList") {
        return (200, group_slave_list_body(fleet, dev_idx));
    }
    // Same reasoning as `multiroom:getSlaveList` above — a relay command is
    // explicit topology the user asked for with `--group`, not optional
    // mini-device fidelity, so it applies regardless of `--no-stateful` too.
    if let Some(command) = command {
        if let Some(result) = handle_group_mutation(command, fleet, dev_idx) {
            return result;
        }
    }
    let (status, body) = 'resolved: {
        if fleet.stateful_http {
            if let Some(command) = command {
                let mut state = dev.state.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
                if let Some(body) = handle_mutation(command, &mut state) {
                    break 'resolved (200, body);
                }
                if matches!(command, "getPlayerStatusEx" | "getPlayerStatus") {
                    break 'resolved match dev.index.get(key) {
                        Some(cap) if cap.outcome == Outcome::Ok => match cap.body.clone() {
                            Some(mut body) => {
                                patch_player_status(&mut body, &state);
                                (200, body.to_string())
                            }
                            None => handle_command(cap, dev),
                        },
                        Some(cap) => handle_command(cap, dev),
                        None => (404, "unknown command".to_string()),
                    };
                }
            }
        }
        match dev.index.get(key) {
            Some(cap) => handle_command(cap, dev),
            None => (404, "unknown command".to_string()),
        }
    };
    if status != 200 {
        return (status, body);
    }
    // Every role, including Standalone, gets its group/master_uuid/master_ip
    // fields overwritten — never only "when grouped" — since a capture can
    // carry stale group state (a real device recorded while it happened to
    // be in a group) that would otherwise leak into a simulated standalone
    // or differently-grouped run.
    if command == Some("getStatusEx") {
        return (status, patch_group_status_fields(&body, fleet, dev_idx));
    }
    // A real follower's own mode/status report reads mode 99 ("Follower" in
    // pywiim's SOURCE_CAPABILITIES table, `device/playback.rs`'s own
    // `loop_tier_http()` doc comment) rather than whatever source it was
    // last playing standalone — it's receiving relayed audio, not running
    // its own source. Applies regardless of `--no-stateful` for the same
    // reason the group fields above do: this is real firmware behavior for
    // any follower, not optional playback-mutation fidelity.
    if matches!(command, Some("getPlayerStatusEx") | Some("getPlayerStatus")) {
        return (status, patch_follower_mode_field(&body, fleet, dev_idx));
    }
    (status, body)
}

/// Overwrites `getPlayerStatusEx`/`getPlayerStatus`'s `mode` field to `"99"`
/// when this device is currently a group follower — see the call site's doc
/// comment for why. A no-op for every other role, and for anything that
/// isn't a JSON object.
fn patch_follower_mode_field(body: &str, fleet: &Fleet, dev_idx: usize) -> String {
    if !matches!(role_of(fleet, dev_idx), SimRole::Follower(_)) {
        return body.to_string();
    }
    let Ok(mut value) = serde_json::from_str::<serde_json::Value>(body) else { return body.to_string() };
    let Some(obj) = value.as_object_mut() else { return body.to_string() };
    obj.insert("mode".into(), serde_json::Value::String("99".to_string()));
    value.to_string()
}

/// The leader's `multiroom:getSlaveList` body, in the 4.3 schema real WiiM
/// firmware uses — read live from each member's own `SimState` (one lock at
/// a time, never two at once, per this file's group-synthesis rule) so a
/// relayed volume/mute change is visible here on the next poll. A
/// non-leader (follower or standalone) gets the exact body a real follower
/// returns: no slaves, nothing to relay through it.
fn group_slave_list_body(fleet: &Fleet, dev_idx: usize) -> String {
    match role_of(fleet, dev_idx) {
        SimRole::Leader(gi) => synth_slave_list(fleet, &fleet.groups[gi]).to_string(),
        SimRole::Follower(_) | SimRole::Standalone => {
            EMPTY_SLAVE_LIST_JSON.to_string()
        }
    }
}

/// Builds one `Group`'s slave-list JSON, matching the shape real WiiM
/// firmware's 4.3 `multiroom:getSlaveList` uses. `ip` carries the member's
/// own API `host:port` — that's what makes the member adoptable by a real
/// `rustywiim` leader (`DeviceManager::adopt_group_members()` creates a
/// `DeviceState` straight from it). `type`/`version` are plausible constants
/// (`device::group::decode_slave_list()` doesn't read either), not derived
/// from anything about the member.
fn synth_slave_list(fleet: &Fleet, group: &Group) -> serde_json::Value {
    let members: Vec<serde_json::Value> = group
        .members
        .iter()
        .map(|m| {
            let dev = &fleet.devices[m.dev];
            let (vol, mute) = {
                let state = dev.state.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
                (state.vol, state.mute)
            };
            serde_json::json!({
                "uuid": dev.fresh_uuid.plain,
                "name": dev.name,
                "ip": dev.api_addr.clone().unwrap_or_default(),
                "volume": vol,
                "mute": if mute { 1 } else { 0 },
                "channel": m.channel,
                "group_channel": -1,
                "type": "WiiMu-AmlogicA1",
                "version": "4.3",
                "battery_charging": 0,
                "battery_percent": 0,
            })
        })
        .collect();
    serde_json::json!({
        "group_type": 1,
        "slaves": members.len(),
        "surround": 0,
        "wmrm_version": "4.3",
        "slave_list": members,
    })
}

/// Overwrites `getStatusEx`'s `group`/`master_uuid`/`master_ip` fields to
/// match this device's role in `fleet.groups` — inserted even if the
/// captured body never had them (a follower must report them regardless of
/// what the source capture was), unlike every other identity field this
/// file patches, which only ever overwrites a key that's already present. A
/// no-op for anything that isn't a JSON object.
fn patch_group_status_fields(body: &str, fleet: &Fleet, dev_idx: usize) -> String {
    let Ok(mut value) = serde_json::from_str::<serde_json::Value>(body) else { return body.to_string() };
    let Some(obj) = value.as_object_mut() else { return body.to_string() };
    let (group, master_uuid, master_ip) = match role_of(fleet, dev_idx) {
        SimRole::Follower(leader_idx) => {
            let leader = &fleet.devices[leader_idx];
            ("1", leader.fresh_uuid.plain.clone(), leader.host.clone())
        }
        SimRole::Leader(_) | SimRole::Standalone => ("0", String::new(), String::new()),
    };
    obj.insert("group".into(), serde_json::Value::String(group.to_string()));
    obj.insert("master_uuid".into(), serde_json::Value::String(master_uuid));
    obj.insert("master_ip".into(), serde_json::Value::String(master_ip));
    value.to_string()
}

/// `multiroom:SlaveVolume:<ip>:<vol>` / `multiroom:SlaveMute:<ip>:<0|1>` —
/// what the app sends to a *leader* to control a *member*. `<ip>` is matched
/// against the target member's own bound address, tolerating both the bare
/// host and the full `host:port` form (the app sends back exactly what the
/// slave list gave it — `host:port` — but there's no reason to be fragile
/// about it). Only the target's own `SimState` lock is taken; the leader's
/// own state (this device's, `dev_idx`) is never touched, so this can't
/// cross-lock with anything `handle_soap_action()`'s `GetInfoEx` arm is
/// doing concurrently on the leader.
///
/// `None` when `command` isn't one of these two — the caller falls through
/// to normal replay. Anything else (sent to a non-leader, or naming a
/// device that isn't actually a member of *this* leader's group) is a 404
/// with a message naming the address that didn't resolve — visible absence,
/// never a silently-accepted no-op.
fn handle_group_mutation(command: &str, fleet: &Fleet, dev_idx: usize) -> Option<(u16, String)> {
    let (ip, value, is_mute) = if let Some(rest) = command.strip_prefix("multiroom:SlaveVolume:") {
        let (ip, vol) = rest.rsplit_once(':')?;
        (ip, vol, false)
    } else if let Some(rest) = command.strip_prefix("multiroom:SlaveMute:") {
        let (ip, mute) = rest.rsplit_once(':')?;
        (ip, mute, true)
    } else {
        return None;
    };

    let SimRole::Leader(gi) = role_of(fleet, dev_idx) else {
        return Some((404, format!("simulator: no such group member {ip}")));
    };
    let target = fleet.groups[gi]
        .members
        .iter()
        .map(|m| &fleet.devices[m.dev])
        .find(|d| d.api_addr.as_deref() == Some(ip) || d.host == ip);
    let Some(target) = target else {
        return Some((404, format!("simulator: no such group member {ip}")));
    };

    let mut state = target.state.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    if is_mute {
        state.mute = value.trim() == "1";
    } else if let Ok(vol) = value.parse::<u32>() {
        state.vol = vol.min(100);
    }
    Some((200, "OK".to_string()))
}

fn serve(server: tiny_http::Server, fleet: &Arc<Fleet>, dev_idx: usize) {
    let tag = format!("[wiim-simulator][{}]", fleet.devices[dev_idx].n + 1);
    for request in server.incoming_requests() {
        let key = request.url().to_string();
        let command = extract_command(&key);
        // Every request goes through catch_unwind: a panic handling one
        // request must not kill this listener thread (a real failure mode
        // found this way — see `percent_decode`'s doc comment). A panic here
        // becomes a 500, and the loop moves on to the next request instead.
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            resolve_response(&key, command.as_deref(), fleet, dev_idx)
        }));
        let (status, body) = result.unwrap_or_else(|_| {
            eprintln!(
                "{tag} internal error handling {} (see panic message above) -> 500",
                command.as_deref().unwrap_or(&key)
            );
            (500, "internal simulator error".to_string())
        });
        eprintln!(
            "{tag} {} -> {status}",
            command.as_deref().unwrap_or(&key)
        );
        let response = tiny_http::Response::from_string(body).with_status_code(status).with_header(
            tiny_http::Header::from_bytes(&b"Content-Type"[..], &b"application/json"[..])
                .expect("static header is valid"),
        );
        let _ = request.respond(response);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bare_port_parses_with_no_device_index() {
        assert_eq!(try_parse_port_spec("--http", "8080"), Ok((None, 8080)));
    }

    #[test]
    fn indexed_port_parses_one_based_to_zero_based() {
        assert_eq!(try_parse_port_spec("--http", "2:9090"), Ok((Some(1), 9090)));
    }

    #[test]
    fn indexed_port_rejects_zero_as_one_based() {
        assert!(try_parse_port_spec("--http", "0:9090").is_err());
    }

    #[test]
    fn port_spec_rejects_non_numeric_port() {
        assert!(try_parse_port_spec("--http", "abc").is_err());
        assert!(try_parse_port_spec("--http", "2:abc").is_err());
    }

    #[test]
    fn group_by_device_accepts_bare_port_with_one_device() {
        let grouped = try_group_by_device(vec![(None, 8080)], 1, "--http").unwrap();
        assert_eq!(grouped, vec![vec![8080]]);
    }

    #[test]
    fn group_by_device_rejects_bare_port_with_several_devices() {
        let err = try_group_by_device(vec![(None, 8080)], 2, "--http").unwrap_err();
        assert!(err.contains("needs a device index"), "{err}");
    }

    #[test]
    fn group_by_device_rejects_out_of_range_index() {
        let err = try_group_by_device(vec![(Some(5), 8080)], 2, "--http").unwrap_err();
        assert!(err.contains("out of range"), "{err}");
    }

    #[test]
    fn group_by_device_collects_cumulative_ports_for_the_same_device() {
        let grouped = try_group_by_device(vec![(Some(0), 8080), (Some(0), 8081)], 2, "--http").unwrap();
        assert_eq!(grouped, vec![vec![8080, 8081], vec![]]);
    }

    #[test]
    fn host_for_increments_the_last_octet_from_base_ip() {
        let base = std::net::Ipv4Addr::new(127, 0, 0, 2);
        assert_eq!(host_for(base, 0), "127.0.0.2");
        assert_eq!(host_for(base, 1), "127.0.0.3");
        assert_eq!(host_for(base, 5), "127.0.0.7");
    }

    fn test_device(n: usize, host: &str, name: &str, vol: u32, mute: bool) -> Arc<Device> {
        Arc::new(Device {
            n,
            host: host.to_string(),
            name: name.to_string(),
            api_addr: Some(format!("{host}:1234")),
            index: HashMap::new(),
            state: Mutex::new(SimState {
                vol,
                mute,
                status: "play".to_string(),
                curpos: 0,
                totlen: 0,
                loop_mode: "0".to_string(),
                mode: "0".to_string(),
            }),
            upnp: None,
            fresh_uuid: FreshUuid::new(),
        })
    }

    fn test_fleet_with_one_group() -> Fleet {
        let leader = test_device(0, "127.0.0.2", "Simulated Leader #1", 30, false);
        let member = test_device(1, "127.0.0.3", "Simulated Member #2", 55, true);
        Fleet {
            devices: vec![leader, member],
            stateful_http: true,
            groups: vec![Group { leader: 0, members: vec![GroupMemberSpec { dev: 1, channel: 0 }] }],
        }
    }

    #[test]
    fn synth_slave_list_round_trips_through_the_real_decoder() {
        let fleet = test_fleet_with_one_group();
        let member_uuid = fleet.devices[1].fresh_uuid.plain.clone();
        let raw = synth_slave_list(&fleet, &fleet.groups[0]).to_string();
        let list = rustywiim::device::group::decode_slave_list(&raw).unwrap();
        assert_eq!(list.kind, rustywiim::device::group::GroupKind::Multiroom);
        assert_eq!(list.members.len(), 1);
        assert_eq!(list.members[0].uuid, rustywiim::device::utils::normalize_uuid(&member_uuid));
        assert_eq!(list.members[0].volume, 55);
        assert!(list.members[0].muted);
    }

    #[test]
    fn role_of_finds_leader_and_follower_and_defaults_to_standalone() {
        let fleet = test_fleet_with_one_group();
        assert!(matches!(role_of(&fleet, 0), SimRole::Leader(0)));
        assert!(matches!(role_of(&fleet, 1), SimRole::Follower(0)));

        let standalone = Fleet {
            devices: vec![test_device(0, "127.0.0.2", "Solo", 30, false)],
            stateful_http: true,
            groups: vec![],
        };
        assert!(matches!(role_of(&standalone, 0), SimRole::Standalone));
    }

    #[test]
    fn detect_resolves_leader_and_follower_from_synthesized_inputs() {
        use rustywiim::device::group::{detect, GroupInputs, GroupRole};

        let fleet = test_fleet_with_one_group();
        let leader_uuid = fleet.devices[0].fresh_uuid.plain.clone();
        let raw = synth_slave_list(&fleet, &fleet.groups[0]).to_string();
        let slave_list = rustywiim::device::group::decode_slave_list(&raw);

        let leader_state = detect(&GroupInputs {
            self_uuid: leader_uuid.clone(),
            slave_list,
            ..Default::default()
        });
        assert_eq!(leader_state.role, GroupRole::Leader);

        let follower_state = detect(&GroupInputs {
            self_uuid: fleet.devices[1].fresh_uuid.plain.clone(),
            slave_flag: Some(true),
            master_uuid: Some(leader_uuid),
            ..Default::default()
        });
        assert_eq!(follower_state.role, GroupRole::Follower);
    }

    #[test]
    fn patch_or_insert_tag_inserts_when_absent_and_overwrites_when_present() {
        let xml = "<a>1</a></u:GetInfoExResponse>";
        let inserted = patch_or_insert_tag(xml, "b", "2", "</u:GetInfoExResponse>");
        assert_eq!(inserted, "<a>1</a><b>2</b></u:GetInfoExResponse>");
        let overwritten = patch_or_insert_tag(&inserted, "b", "3", "</u:GetInfoExResponse>");
        assert_eq!(overwritten, "<a>1</a><b>3</b></u:GetInfoExResponse>");
    }

    #[test]
    fn group_status_fields_are_inserted_even_when_absent_from_the_capture() {
        let fleet = test_fleet_with_one_group();
        let body = patch_group_status_fields("{}", &fleet, 1);
        let value: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(value["group"], "1");
        assert_eq!(value["master_uuid"], fleet.devices[0].fresh_uuid.plain);
        assert_eq!(value["master_ip"], fleet.devices[0].host);
    }

    #[test]
    fn slave_volume_relay_mutates_the_members_own_state_not_the_leaders() {
        let fleet = test_fleet_with_one_group();
        let member_addr = fleet.devices[1].api_addr.clone().unwrap();
        let (status, body) = handle_group_mutation(
            &format!("multiroom:SlaveVolume:{member_addr}:77"),
            &fleet,
            0,
        )
        .unwrap();
        assert_eq!(status, 200);
        assert_eq!(body, "OK");
        assert_eq!(fleet.devices[1].state.lock().unwrap().vol, 77);
        assert_eq!(fleet.devices[0].state.lock().unwrap().vol, 30, "leader's own volume must be untouched");
    }

    #[test]
    fn slave_mute_relay_accepts_the_bare_host_form_too() {
        let fleet = test_fleet_with_one_group();
        let (status, _) = handle_group_mutation("multiroom:SlaveMute:127.0.0.3:1", &fleet, 0).unwrap();
        assert_eq!(status, 200);
        assert!(fleet.devices[1].state.lock().unwrap().mute);
    }

    #[test]
    fn slave_volume_relay_rejects_a_non_leader() {
        let fleet = test_fleet_with_one_group();
        let member_addr = fleet.devices[1].api_addr.clone().unwrap();
        let (status, body) =
            handle_group_mutation(&format!("multiroom:SlaveVolume:{member_addr}:50"), &fleet, 1).unwrap();
        assert_eq!(status, 404);
        assert!(body.contains("no such group member"), "{body}");
    }

    #[test]
    fn slave_volume_relay_rejects_an_unknown_address() {
        let fleet = test_fleet_with_one_group();
        let (status, body) = handle_group_mutation("multiroom:SlaveVolume:10.0.0.99:50", &fleet, 0).unwrap();
        assert_eq!(status, 404);
        assert!(body.contains("no such group member"), "{body}");
    }

    #[test]
    fn follower_mode_field_is_forced_to_99_but_leader_and_standalone_are_untouched() {
        let fleet = test_fleet_with_one_group();
        let body = r#"{"mode":"10","vol":"30"}"#;
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&patch_follower_mode_field(body, &fleet, 1)).unwrap()["mode"],
            "99"
        );
        assert_eq!(patch_follower_mode_field(body, &fleet, 0), body, "leader must be untouched");

        let standalone = Fleet {
            devices: vec![test_device(0, "127.0.0.2", "Solo", 30, false)],
            stateful_http: true,
            groups: vec![],
        };
        assert_eq!(patch_follower_mode_field(body, &standalone, 0), body, "standalone must be untouched");
    }

    #[test]
    fn wrap_text_never_exceeds_the_requested_width() {
        let text = "HTTP listener port for device N (1-based). A bare PORT is only valid \
                    with exactly one capture path.";
        for line in wrap_text(text, 40) {
            assert!(line.len() <= 40, "{line:?} is {} chars", line.len());
        }
    }

    #[test]
    fn wrap_text_preserves_every_word_in_order() {
        let text = "one two three four five";
        let rejoined = wrap_text(text, 10).join(" ");
        assert_eq!(rejoined, text);
    }

    #[test]
    fn wrap_text_handles_empty_input() {
        assert!(wrap_text("", 40).is_empty());
    }

    #[test]
    fn wrap_text_keeps_a_too_long_word_on_its_own_line_rather_than_splitting_it() {
        let lines = wrap_text("a-very-long-unbreakable-word short", 10);
        assert_eq!(lines[0], "a-very-long-unbreakable-word");
        assert_eq!(lines[1], "short");
    }
}
