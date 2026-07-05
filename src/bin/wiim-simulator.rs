//! `wiim-simulator` — replays a `wiim-capture` JSON file as a fake
//! LinkPlay/WiiM HTTP(S) device, so `rustywiim` (or `wiim-capture` itself)
//! can be pointed at something other than real hardware for testing. See
//! TESTING.md's "Part 2 — simulator server" for the design.
//!
//! Usage: `wiim-simulator <capture-file-or-dir> [--http PORT] [--https PORT] [--stateful] [--global]`
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
//! `0.0.0.0` instead.
//!
//! **Pure replay by default**: every request is answered strictly from what's
//! actually in the capture file, keyed by request path + query (not just the
//! `command=` value) — this is what lets the `getsyslog:download` entry's
//! distinct URL (not a `httpapi.asp?command=...` call at all) replay
//! correctly through the exact same lookup, with no special-casing. A
//! request with no matching entry gets 404 — a visible "the capture has no
//! data for this," never a silently wrong response.
//!
//! **`--stateful`** turns on a small in-memory mini-device (`SimState`:
//! volume, mute, play state, position, loop mode, source mode) seeded from
//! the captured `getPlayerStatusEx`/`getPlayerStatus` body, shared (behind a
//! `Mutex`, since multiple listener threads can now touch it concurrently —
//! e.g. one poll arriving over HTTP while a control command arrives over
//! HTTPS) across every listener. Only the small, fixed set of
//! playback-control commands `handle_mutation()` recognizes
//! (`setPlayerCmd:vol/mute/seek/loopmode/resume/play/pause/onepause/stop/
//! next/prev`, `switchmode`, `MCUKeyShortClick`, `setAudioOutputHardwareMode`
//! — the ones `wiim-capture` deliberately never sends to a real device) are
//! actually simulated: they update `SimState` and return a synthesized "OK".
//! `getPlayerStatusEx`/`getPlayerStatus` get patched with the current
//! in-memory state before replay so subsequent polls reflect it, instead of
//! showing the frozen captured snapshot forever. Everything else — even with
//! `--stateful` on — still replays from the capture file exactly as in the
//! default case; this is deliberately a small, fixed set of commands, not a
//! general device model, with more coverage left for later.
//!
//! `--https` serves TLS with a self-signed certificate generated fresh at
//! startup (`rcgen`) — `rustywiim`'s own TLS client
//! (`danger_accept_invalid_certs`) never validates the server's certificate
//! against a CA, exactly like it doesn't for real WiiM hardware, so a
//! throwaway cert is sufficient; nothing to manage on disk. One certificate
//! is generated and shared by every `--https` listener.

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

/// Everything a request-handling thread needs, shared read-only (`index`) or
/// behind a `Mutex` (`state`) across every listener thread.
struct Shared {
    index: HashMap<String, CommandCapture>,
    state: Option<Mutex<SimState>>,
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
    // UPnP SOAP actions (POST + XML body + SOAPAction header, matched by
    // more than just the path) are deliberately not indexed here — this is
    // GET-response replay of the `commands` array only, for now.
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

/// Replays a captured entry faithfully: `outcome == Ok` serves the recorded
/// body at its recorded (or default 200) status; anything else (the command
/// itself failed at capture time) has no real body to replay, so it serves
/// the recorded status if there is one, else a generic 500 — still a
/// response, but visibly not a real one.
fn handle_command(cap: &CommandCapture) -> (u16, String) {
    if cap.outcome == Outcome::Ok {
        return (cap.http_status.unwrap_or(200), render_body(cap));
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

struct Args {
    path: String,
    listeners: Vec<Listener>,
    stateful: bool,
    global: bool,
}

fn usage() -> ! {
    eprintln!("usage: wiim-simulator <capture-file-or-dir> [--http PORT] [--https PORT] [--stateful] [--global]");
    eprintln!("  (--http/--https are cumulative — repeat for more listeners; with neither");
    eprintln!("   given, defaults to one http + one https listener on random ports)");
    eprintln!("  (--global listens on 0.0.0.0 instead of the default 127.0.0.1-only)");
    std::process::exit(2);
}

fn parse_args() -> Args {
    let mut path = None;
    let mut listeners = Vec::new();
    let mut stateful = false;
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
            "--stateful" => stateful = true,
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
    Args { path, listeners, stateful, global }
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
    let state = args.stateful.then(|| Mutex::new(init_state(&capture)));
    let shared = Arc::new(Shared { index, state });

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
    // switches every listener to 0.0.0.0.
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
            if args.stateful { ", stateful mini-device on" } else { "" }
        );
        let shared = Arc::clone(&shared);
        handles.push(std::thread::spawn(move || serve(server, &shared)));
    }

    if handles.is_empty() {
        eprintln!("[wiim-simulator] no listener could bind, exiting");
        std::process::exit(1);
    }
    for handle in handles {
        let _ = handle.join();
    }
}

/// Resolves one request to a `(status, body)` reply. When `shared.state` is
/// `Some` (`--stateful`) and the request's `command=` value is a recognized
/// mutator, `handle_mutation` handles it entirely (under the state's
/// `Mutex`, since multiple listener threads share it); `getPlayerStatusEx`/
/// `getPlayerStatus` get the current state patched in. Everything else
/// (always, regardless of `--stateful`) falls through to plain replay from
/// `shared.index`, keyed by the request's full path+query.
fn resolve_response(key: &str, command: Option<&str>, shared: &Shared) -> (u16, String) {
    if let (Some(state_mutex), Some(command)) = (&shared.state, command) {
        let mut state = state_mutex.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
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
                    None => handle_command(cap),
                },
                Some(cap) => handle_command(cap),
                None => (404, "unknown command".to_string()),
            };
        }
    }
    match shared.index.get(key) {
        Some(cap) => handle_command(cap),
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
