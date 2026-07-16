#![allow(dead_code)] // API surface used by future modules

use reqwest::Client;
use serde::Deserialize;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

pub static DEBUG: AtomicBool = AtomicBool::new(false);
/// `--debug=api:verbose` (or `all:verbose`): include the full response body
/// in `debug()`'s output. Without it, `debug()` logs just the base URL and
/// command — enough to see call traffic/timing without a wall of JSON.
/// `upnp.rs` has its own, independent `DEBUG_UPNP`/`DEBUG_UPNP_VERBOSE` pair
/// for SOAP tracing — the two used to share this flag/format, but that made
/// it impossible to turn one on without the other; `log_request_error()`
/// below is the one piece still actually shared (it's the same walk-the-
/// error-chain logic either way, just tagged per caller).
pub static DEBUG_VERBOSE: AtomicBool = AtomicBool::new(false);

pub(crate) fn debug(base: &str, cmd: &str, resp: &str) {
    if !DEBUG.load(Ordering::Relaxed) {
        return;
    }
    if DEBUG_VERBOSE.load(Ordering::Relaxed) {
        println!("{} [API] {base} {cmd} → {resp}", super::timestamp());
    } else {
        println!("{} [API] {base} {cmd}", super::timestamp());
    }
}

pub(crate) fn debug_info(msg: &str) {
    if DEBUG.load(Ordering::Relaxed) {
        println!("{} [API] {msg}", super::timestamp());
    }
}

// ── TLS / protocol mode ───────────────────────────────────────────────────────

/// Self-signed CA certificate used by WiiM/LinkPlay devices (issued by www.linkplay.com).
/// Used to verify the server certificate in `HttpsWiiM` mode.
static WIIM_CA_CERT: &[u8] = include_bytes!("../certs/wiim_ca.pem");

/// Private key for Audio Pro mTLS client authentication.
static AUDIO_PRO_KEY: &[u8] = include_bytes!("../certs/audio_pro_key.pem");

/// Active connection protocol override.  `0` = `Auto` (default): use the per-device
/// mode stored in config, falling back to `HttpsWiiM`.
pub static TLS_MODE: AtomicUsize = AtomicUsize::new(0);

/// Connection protocol and TLS certificate policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TlsMode {
    /// Automatic: use the per-device stored mode (from discovery), else `HttpsWiiM`.
    Auto          = 0,
    /// Plain HTTP, no TLS.  Port 80.
    Http          = 1,
    /// HTTPS, no server cert verification, WiiM CA cert loaded.  Port 443.
    /// Stored value 2 from old configs is treated identically to HttpsWiiM (3).
    HttpsAny      = 2,
    /// HTTPS, no server cert verification, WiiM CA cert loaded.  Port 443.
    HttpsWiiM     = 3,
    /// HTTPS with mutual TLS: AudioPro client cert+key + WiiM CA.  Port 4443.
    HttpsAudioPro = 4,
}

impl TlsMode {
    pub fn from_usize(n: usize) -> Self {
        match n {
            1 => Self::Http,
            2 => Self::HttpsAny,
            3 => Self::HttpsWiiM,
            4 => Self::HttpsAudioPro,
            _ => Self::Auto,
        }
    }

    pub fn description(self) -> &'static str {
        match self {
            Self::Auto          => "auto (per-device mode or https-wiim default)",
            Self::Http          => "http (plain HTTP, port 80)",
            Self::HttpsAny      => "https (no cert verification, WiiM CA loaded, port 443)",
            Self::HttpsWiiM     => "https-wiim (no cert verification, WiiM CA loaded, port 443)",
            Self::HttpsAudioPro => "https-audio-pro (mTLS AudioPro client cert, port 4443)",
        }
    }
}

/// Build (or reuse a cached) reqwest `Client` for the given TLS mode.
///
/// All HTTPS modes:
///   - Disable server certificate verification (`danger_accept_invalid_certs`).
///   - Load the static WiiM/LinkPlay CA certificate into the trust store.
///   - Present the WiiM CA certificate + key as a client certificate for mutual TLS.
///     Devices that do not request a client cert ignore it; devices that do (including
///     some regular WiiM units and all AudioPro units) will accept it.
///
/// The only difference between `HttpsWiiM` and `HttpsAudioPro` is the port used in
/// the URL (443 vs 4443); the TLS configuration is identical, so they (and
/// `HttpsAny`) share a single cached client.
///
/// Uses OpenSSL (native-tls) which accepts CA-as-end-entity server certificates
/// that rustls rejects with `CaUsedAsEndEntity`.
///
/// Building a `Client` loads and parses the system CA store plus the WiiM
/// CA/identity PEMs — ~250M instructions per call, measured via callgrind. Every
/// `WiimClient::new()`, health check, and discovery probe used to pay that
/// cost from scratch; a `reqwest::Client` is cheap to clone (internally
/// `Arc`-based) and every device shares identical TLS config, so it's cached
/// per (TLS family, timeout) pair and cloned on every call after the first.
pub fn build_reqwest_client(tls: TlsMode, timeout: Duration) -> Client {
    fn cache() -> &'static Mutex<HashMap<(u8, u64), Client>> {
        static CACHE: OnceLock<Mutex<HashMap<(u8, u64), Client>>> = OnceLock::new();
        CACHE.get_or_init(|| Mutex::new(HashMap::new()))
    }

    let family: u8 = match tls {
        TlsMode::Auto => panic!("build_reqwest_client: Auto must be resolved by the caller"),
        TlsMode::Http => 0,
        TlsMode::HttpsAny | TlsMode::HttpsWiiM | TlsMode::HttpsAudioPro => 1,
    };
    let key = (family, timeout.as_millis() as u64);
    if let Some(client) = cache().lock().unwrap().get(&key) {
        return client.clone();
    }

    let client = match tls {
        TlsMode::Auto => unreachable!("handled above"),

        TlsMode::Http => Client::builder()
            .timeout(timeout)
            .build()
            .expect("http client"),

        TlsMode::HttpsAny | TlsMode::HttpsWiiM | TlsMode::HttpsAudioPro => {
            let ca = reqwest::tls::Certificate::from_pem(WIIM_CA_CERT).expect("WiiM CA cert");
            let identity = reqwest::Identity::from_pkcs8_pem(WIIM_CA_CERT, AUDIO_PRO_KEY)
                .expect("WiiM mTLS identity");
            Client::builder()
                .danger_accept_invalid_certs(true)
                .add_root_certificate(ca)
                .identity(identity)
                .timeout(timeout)
                .build()
                .expect("https client")
        }
    };
    cache().lock().unwrap().insert(key, client.clone());
    client
}

/// Log a reqwest error for `context` (e.g. an API command or a discovery
/// probe), tagged `[{tag}]` — shared by `api.rs` itself (`"API"`) and
/// `upnp.rs`'s SOAP calls (`"upnp"`), since the walk-the-`source()`-chain
/// logic is identical either way, only the tag differs now that the two
/// modules have their own independent `--debug=api`/`--debug=upnp` flags.
///
/// Always prints to stderr, regardless of any `--debug` flag.  Walks the
/// full `source()` chain so that the root cause (e.g. the specific TLS or
/// certificate failure) is visible.
pub fn log_request_error(tag: &str, context: &str, err: &reqwest::Error) {
    use std::error::Error as StdError;
    let ts = super::timestamp();
    eprintln!("{ts} [{tag}] {context}: {err}");
    let mut cause: Option<&dyn StdError> = err.source();
    while let Some(c) = cause {
        eprintln!("{ts} [{tag}]   caused by: {c}");
        cause = c.source();
    }
}

/// Return the base `httpapi.asp` URL for the given IP and TLS mode.
///
/// Does not append any command query string — append `?command=…` yourself.
///
/// `ip` may already include a `:port` (e.g. `"127.0.0.1:8080"`, used by
/// `--connect`/`wiim-simulator` testing) — for `Http`/`HttpsAny`/`HttpsWiiM`
/// this just works, since those modes never append a port of their own and
/// let the URL's own default (80/443) apply otherwise. `HttpsAudioPro`
/// always uses port 4443 on real hardware, so its hardcoded `:4443` is only
/// skipped when `ip` already carries its own port, to avoid doubling up.
pub fn api_base_url(ip: &str, tls: TlsMode) -> String {
    match tls {
        TlsMode::Http                           => format!("http://{ip}/httpapi.asp"),
        TlsMode::HttpsAny | TlsMode::HttpsWiiM => format!("https://{ip}/httpapi.asp"),
        TlsMode::HttpsAudioPro if ip.contains(':') => format!("https://{ip}/httpapi.asp"),
        TlsMode::HttpsAudioPro                  => format!("https://{ip}:4443/httpapi.asp"),
        TlsMode::Auto => panic!("api_base_url: Auto must be resolved by the caller"),
    }
}

/// URL-encode a string for embedding as a WiiM API command argument.
pub fn urlencode(s: &str) -> String {
    let mut out = String::with_capacity(s.len() * 3);
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{:02X}", b)),
        }
    }
    out
}

// ── Response types ────────────────────────────────────────────────────────────

/// The WiiM/LinkPlay API reports numeric and boolean fields as JSON strings
/// (e.g. `"vol": "45"`, `"mute": "1"`). These helpers parse them at the
/// deserialization boundary so callers get real numbers/bools instead of
/// re-parsing the same string at every call site.
fn de_num_from_str<'de, D, T>(deserializer: D) -> Result<T, D::Error>
where
    D: serde::Deserializer<'de>,
    T: std::str::FromStr + Default,
{
    let s = String::deserialize(deserializer)?;
    Ok(s.parse().unwrap_or_default())
}

fn de_bool_from_01<'de, D>(deserializer: D) -> Result<bool, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let s = String::deserialize(deserializer)?;
    Ok(s == "1")
}

/// -1 sentinel for "missing or unparseable" — used for fields (`mode`,
/// `loop_mode`) where every value in the field's actual range (0.. for
/// `mode`, 0-5 for `loop_mode`) is already meaningful, so falling back to 0
/// on parse failure (as `de_num_from_str` does) would silently collide with
/// a real value. Confirmed via real captures, pywiim, Wiim-Dashboard, and
/// linkplay-cli that `mode` is always a non-negative stringified integer on
/// the wire; -1 itself is never observed on the wire, only used defensively
/// by linkplay-cli's own lookup table — same spirit here. `loop_mode`'s own
/// decode (`decode_loop_mode_http`) already has a catch-all defaulting to
/// `(false, RepeatMode::Off)` for any unrecognized value, so -1 lands there
/// and reproduces exactly the same "missing -> Off" behavior as today's
/// empty/unparseable-string catch-all, not a new default.
fn default_neg1() -> i32 { -1 }

fn de_i32_or_neg1<'de, D>(deserializer: D) -> Result<i32, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let s = String::deserialize(deserializer)?;
    Ok(s.parse().unwrap_or(-1))
}

#[derive(Debug, Clone, Deserialize)]
pub struct PlayerStatus {
    #[serde(default)]
    pub status: String,
    #[serde(default, deserialize_with = "de_num_from_str")]
    pub vol: u32,
    #[serde(default, deserialize_with = "de_bool_from_01")]
    pub mute: bool,
    #[serde(default, deserialize_with = "de_num_from_str")]
    pub curpos: u64,
    #[serde(default, deserialize_with = "de_num_from_str")]
    pub totlen: u64,
    /// LinkPlay's `loop` field, always a stringified 0-5 integer on the wire
    /// (see `decode_loop_mode_http` for the meaning of each value). -1 is a
    /// sentinel for missing/unparseable, not a wire value — see
    /// `de_i32_or_neg1`'s doc comment.
    #[serde(default = "default_neg1", rename = "loop", deserialize_with = "de_i32_or_neg1")]
    pub loop_mode: i32,
    /// Always a stringified non-negative integer on the wire (confirmed
    /// against real captures and every reference project this session
    /// researched) — -1 is a sentinel for missing/unparseable, never a real
    /// wire value. See `de_i32_or_neg1`'s doc comment.
    #[serde(default = "default_neg1", deserialize_with = "de_i32_or_neg1")]
    pub mode: i32,
    #[serde(default)]
    pub vendor: String,
    #[serde(default)]
    pub plicount: String,
    #[serde(default)]
    pub plicurr: String,
    /// Hex-encoded UTF-8 (same LinkPlay convention as `essid`/`ssid_decoded()`
    /// above) — present directly in `getPlayerStatusEx` on devices that don't
    /// support `getMetaInfo` at all (confirmed live, 2026-07-13, on an Audio
    /// Pro Addon C5 running old firmware: `getMetaInfo` returns "unknown
    /// command", but `getPlayerStatusEx` carries `Title`/`Artist`/`Album`
    /// directly). Use `title_decoded()`/`artist_decoded()`/`album_decoded()`,
    /// never these raw fields — matches every other hex field's own accessor
    /// pattern rather than leaving decode-or-not ambiguous at call sites.
    #[serde(default, rename = "Title")]
    pub title: String,
    #[serde(default, rename = "Artist")]
    pub artist: String,
    #[serde(default, rename = "Album")]
    pub album: String,
}

impl Default for PlayerStatus {
    fn default() -> Self {
        Self {
            status:    String::new(),
            vol:       0,
            mute:      false,
            curpos:    0,
            totlen:    0,
            loop_mode: -1,
            mode:      -1,
            vendor:    String::new(),
            plicount:  String::new(),
            plicurr:   String::new(),
            title:     String::new(),
            artist:    String::new(),
            album:     String::new(),
        }
    }
}

impl PlayerStatus {
    pub fn title_decoded(&self) -> String {
        hex_decode_utf8(&self.title).unwrap_or_else(|| self.title.clone())
    }
    pub fn artist_decoded(&self) -> String {
        hex_decode_utf8(&self.artist).unwrap_or_else(|| self.artist.clone())
    }
    pub fn album_decoded(&self) -> String {
        hex_decode_utf8(&self.album).unwrap_or_else(|| self.album.clone())
    }
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct MetaData {
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub artist: String,
    #[serde(default)]
    pub album: String,
    /// API sometimes returns key with a trailing space
    #[serde(default, rename = "albumArtURI ")]
    pub album_art_uri_spaced: String,
    #[serde(default, rename = "albumArtURI")]
    pub album_art_uri: String,
    #[serde(default, rename = "sampleRate")]
    pub sample_rate: String,
    #[serde(default, rename = "bitDepth")]
    pub bit_depth: String,
    #[serde(default, rename = "bitRate")]
    pub bit_rate: String,
}

impl MetaData {
    pub fn art_uri(&self) -> &str {
        if !self.album_art_uri.is_empty() {
            &self.album_art_uri
        } else {
            &self.album_art_uri_spaced
        }
    }

    /// Synthesizes a `MetaData` from a `getPlayerStatusEx` response's own
    /// `Title`/`Artist`/`Album`, for devices whose family profile has
    /// `endpoints.supports_get_meta_info: false` — see `fetch_http_fast_poll`
    /// in `state.rs`, the only caller. No artwork/quality fields: this
    /// endpoint doesn't carry them at all (confirmed on the Audio Pro Addon
    /// C5 capture this was built for — a real `getPlayerStatusEx` response
    /// while playing had no art-URL field of any kind), so `art_uri()`
    /// returns empty and `process_poll_http`'s quality decode falls back to
    /// its own already-existing "nothing to show" path, same as it does for
    /// any device with no quality data.
    pub fn from_player_status(st: &PlayerStatus) -> Self {
        Self {
            title:  st.title_decoded(),
            artist: st.artist_decoded(),
            album:  st.album_decoded(),
            ..Self::default()
        }
    }
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct MetaInfoResponse {
    #[serde(default, rename = "metaData")]
    pub meta_data: MetaData,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct DeviceInfo {
    #[serde(default, rename = "DeviceName")]
    pub device_name: String,
    #[serde(default)]
    pub ssid: String,
    /// Hex-encoded AP SSID (same LinkPlay convention as `getPlayerStatusEx`'s
    /// `Title`/`Artist`/`Album` — hex bytes, decoded as UTF-8 — used because
    /// an SSID can contain arbitrary/non-ASCII characters). Use
    /// `ssid_decoded()` rather than reading this directly.
    #[serde(default)]
    pub essid: String,
    #[serde(default)]
    pub firmware: String,
    #[serde(default)]
    pub uuid: String,
    #[serde(default)]
    pub project: String,
    #[serde(default)]
    pub hardware: String,
    #[serde(default)]
    pub eth0: String,
    #[serde(default)]
    pub apcli0: String,
    #[serde(default, rename = "Release")]
    pub release: String,
    /// Raw `plm_support` bitmap from `getStatusEx`.  May be decimal or
    /// `"0x…"` hex.  Use `plm_support_value()` to get the parsed integer.
    #[serde(default)]
    pub plm_support: String,
    /// Network connection type as a string: "0" = ethernet, "2" = wifi.
    #[serde(default)]
    pub netstat: String,
    /// Wifi RSSI in dBm as a string (e.g. "-61").  Empty on ethernet.
    #[serde(default, rename = "RSSI")]
    pub rssi: String,
    /// Multiroom protocol version.  "2.0" → Gen1 (WiFi Direct grouping).
    /// "4.2" → Gen2+.  Used for Gen1 detection in capability profiles.
    #[serde(default)]
    pub wmrm_version: String,
    /// Whether a BLE remote is currently paired/present: "1"/"0". Absent
    /// entirely on devices with no BLE remote hardware.
    #[serde(default, rename = "BleRemoteConnected")]
    pub ble_remote_connected: String,
    /// BLE remote battery level, percentage as a string (e.g. "100").
    #[serde(default, rename = "BleRemoteBatterylevel")]
    pub ble_remote_battery: String,
    /// BLE remote RSSI in dBm as a string (e.g. "-73") — same shape as the
    /// top-level `RSSI` field, just for the remote's own radio link.
    #[serde(default, rename = "BleRemoteRSSI")]
    pub ble_remote_rssi: String,
    /// Max number of hardware preset slots the device supports, as a decimal
    /// string (e.g. "6"). Not read anywhere yet — recovered so it's available
    /// once something needs it, rather than needing another capture/parsing
    /// round-trip later.
    #[serde(default)]
    pub preset_key: String,
}

impl DeviceInfo {
    pub fn ip_addr(&self) -> &str {
        if !self.eth0.is_empty() && self.eth0 != "0.0.0.0" {
            &self.eth0
        } else {
            &self.apcli0
        }
    }

    /// Parse `plm_support` as a u64, handling both `"0x…"` hex and decimal.
    pub fn plm_support_value(&self) -> u64 {
        let s = self.plm_support.trim();
        if let Some(hex) = s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")) {
            u64::from_str_radix(hex, 16).unwrap_or(0)
        } else {
            s.parse::<u64>().unwrap_or(0)
        }
    }

    /// Decoded AP SSID from `essid` (hex → UTF-8), falling back to the raw
    /// `essid` string if it doesn't decode cleanly (matches linkplay-cli's
    /// own `_decode_string` fallback behavior for this exact field).
    pub fn ssid_decoded(&self) -> String {
        hex_decode_utf8(&self.essid).unwrap_or_else(|| self.essid.clone())
    }
}

/// Decode a hex-encoded UTF-8 string — LinkPlay's convention for fields that
/// might contain non-ASCII characters (`getPlayerStatusEx`'s `Title`/
/// `Artist`/`Album`, `getStatusEx`'s `essid`). `None` if `s` isn't
/// even-length all-hex-digits, or doesn't decode to valid UTF-8.
fn hex_decode_utf8(s: &str) -> Option<String> {
    if s.is_empty() || s.len() % 2 != 0 || !s.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let bytes: Option<Vec<u8>> = (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).ok())
        .collect();
    bytes.and_then(|b| String::from_utf8(b).ok())
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct AudioInputEntry {
    /// Input source name (e.g. `"wifi"`, `"HDMI"`) — wire key is `"mode"`,
    /// but it's a name string, not a numeric mode like `PlayerStatus.mode`;
    /// renamed on this side to avoid confusing the two.
    #[serde(default, rename = "mode")]
    pub name:   String,
    #[serde(default)]
    pub enable: u8,
}

impl AudioInputEntry {
    pub fn is_enabled(&self) -> bool { self.enable != 0 }
}

/// Parse a `getAudioInputCapbility` response body into the list of canonical
/// input IDs it reports. `None` if the body isn't the expected wrapped-array
/// object (e.g. the literal `"unknown command"` string most non-WiiM devices
/// return) — distinct from `Some(vec![])`. Split out from
/// `WiimClient::get_audio_input_capability()` so it's unit-testable against a
/// real capture fixture without a live client.
fn parse_audio_input_capability(text: &str) -> Option<Vec<String>> {
    #[derive(Deserialize)]
    struct Item {
        #[serde(default)]
        mode: String,
    }
    #[derive(Deserialize)]
    struct Response {
        #[serde(default, rename = "audioInput")]
        audio_input: Vec<Item>,
    }
    let resp = serde_json::from_str::<Response>(text).ok()?;
    Some(
        resp.audio_input
            .into_iter()
            .map(|i| i.mode)
            .filter(|m| !m.is_empty())
            .collect(),
    )
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct AudioOutputStatus {
    #[serde(default)]
    pub hardware: String,
    #[serde(default)]
    pub source: String,
}

/// Decoded Bluetooth A2DP sink status obtained from `getbtstatus`'s
/// `{"a2dp_sink": {"link_state": "connected"|"disconnected",
/// "name": "...", "pairing": N}, ...}` .
/// `pairing` is whether the sink is currently discoverable/open for
/// pairing. I noticed some false negatives but those are harmless
/// and seem to happen with the WiiM app too (ie, it offers to restart
/// pairing while still visible to other devices too).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct BtStatus {
    pub connected:    bool,
    pub device_name:  String,
    pub pairing:      bool,
}

/// One entry in the device's supported-outputs list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutputEntry {
    /// Canonical internal name (e.g. `"line-out"`, `"usb-out"`) — drives
    /// mode-setting (`output_canon_to_mode`) and hardware-value matching;
    /// never adjusted for display/icon purposes, so it always resolves to
    /// the correct wire value.
    pub canon: &'static str,
    /// Canonical name to use for *icon lookup only* — equal to `canon`
    /// except where `DeviceProfile.line_out_is_speaker` applies (some
    /// Amp-family devices report their built-in speaker output through the
    /// generic `"line-out"` slot), in which case it's corrected to
    /// `"speaker-out"`. Set by `capabilities::detect_capabilities()`.
    pub icon_canon: &'static str,
    /// User-visible label derived from the `getSoundCardModeSupportList` response,
    /// falling back to `output_display_name()` when the API fields give nothing useful.
    pub name: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Preset {
    pub number: u32,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub source: String,
    #[serde(default)]
    pub picurl: String,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct PresetResponse {
    #[serde(default)]
    pub preset_num: u32,
    #[serde(default)]
    pub preset_list: Vec<Preset>,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct RoutineStepPayload {
    #[serde(default)]
    pub input:  String,
    #[serde(default)]
    pub output: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct RoutineStep {
    #[serde(rename = "type", default)]
    pub step_type: String,
    #[serde(default)]
    pub payload:   RoutineStepPayload,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Routine {
    #[serde(default)]
    pub id:    String,
    #[serde(default)]
    pub name:  String,
    #[serde(default)]
    pub index: u32,
    #[serde(default)]
    pub steps: Vec<RoutineStep>,
}

impl Routine {
    /// Returns the `audioInput` input ID from steps, if present and non-empty.
    pub fn audio_input(&self) -> Option<&str> {
        self.steps.iter()
            .find(|s| s.step_type == "audioInput")
            .map(|s| s.payload.input.as_str())
            .filter(|s| !s.is_empty())
    }

    /// Returns the `audioOutput` output mode string from steps, if present and
    /// non-empty.  An empty string means "no output change" and is excluded.
    pub fn audio_output(&self) -> Option<&str> {
        self.steps.iter()
            .find(|s| s.step_type == "audioOutput")
            .map(|s| s.payload.output.as_str())
            .filter(|s| !s.is_empty())
    }
}

#[derive(Debug, Clone, Deserialize, Default)]
struct RoutinesResponse {
    #[serde(default)]
    routines: Vec<Routine>,
}

// ── Preset display entries ────────────────────────────────────────────────────

/// What kind of action a preset slot performs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PresetKind {
    /// Regular media preset (internet radio, playlist, service favourite).
    Media,
    /// Routine that switches audio input; `input_id` is used for the icon.
    InputSwitch { input_id: String },
    /// Routine that switches audio output mode; `output_id` is used for the icon.
    OutputSwitch { output_id: String },
    /// Routine with an action type we don't recognise.
    OtherRoutine,
    /// Slot with neither a media preset nor a routine.
    Empty,
}

/// Outcome of one preset-list fetch attempt, from whichever source
/// (HTTP `getPresetInfo` or, for devices where that's confirmed
/// unsupported, UPnP `GetKeyMapping` — see `upnp.rs`'s
/// `get_key_mapping_presets()`). Both sources report through this same
/// type so `state.rs` can react identically regardless of which one is
/// active for a given device.
#[derive(Debug)]
pub enum PresetFetchOutcome {
    /// The device doesn't support this call at all — confirmed via its
    /// "unknown command"-style response (or, for UPnP, no `PlayQueue`
    /// service advertised at all). A *final* answer: never re-tried, no
    /// retry budget consulted. Distinct from `Failed` below, which is
    /// merely inconclusive.
    Unsupported,
    /// The call itself didn't complete — a network/transport failure (or
    /// a response that came back but didn't parse), not a device-reported
    /// "unsupported" signal. Says nothing about whether the device
    /// actually supports this call; `state.rs` retries a bounded number of
    /// times (`PRESET_PROBE_FAIL_THRESHOLD`) before treating this the same
    /// as a confirmed `Unsupported` — the same "don't give up on one
    /// flaky miss" reasoning `capabilities.rs`'s `record_outputs_probe()`
    /// already applies to `getSoundCardModeSupportList`.
    Failed,
    /// Fingerprint unchanged since the last call — nothing to rebuild.
    Unchanged,
    /// Fingerprint changed; here's the fresh list.
    Changed(String, Vec<PresetEntry>),
}

/// LinkPlay's "not supported" signal: a 200 OK whose body is literally one
/// of these strings (case-insensitive) rather than a real payload. Same
/// sentinel `wiim-capture`'s own `is_unsupported_text()` checks for.
fn is_unsupported_text(raw: &str) -> bool {
    matches!(raw.trim().to_lowercase().as_str(), "unknown command" | "failed" | "unknown")
}

/// `get_presets()`'s own result, one layer below `PresetFetchOutcome` —
/// distinguishes a confirmed-unsupported response (final) from the call
/// simply not completing/parsing (inconclusive: connection error, timeout,
/// or a 200 response that isn't valid `PresetResponse` JSON) so
/// `fetch_presets()` can map the latter to `PresetFetchOutcome::Failed`
/// rather than treating every non-response as "unsupported."
enum GetPresetsResult {
    Ok(PresetResponse),
    Unsupported,
    Failed,
}

/// Outcome of a probe for a command that might return LinkPlay's "not
/// supported" sentinel (`is_unsupported_text()`) instead of real data.
/// `Unsupported` is a *definite* answer from the device — it said so, in
/// plain text, not a transient hiccup — so callers must never retry it on
/// a timer the way they would `Failed` (a genuine transport/parse error,
/// worth a few retries before assuming the same thing). Shared by
/// `get_audio_output()`/`get_sound_card_mode_support_list()`; `get_presets()`
/// has its own richer `GetPresetsResult` (needs to additionally carry the
/// real "zero presets configured" case, which isn't just "no data").
pub enum ApiOutcome<T> {
    Ok(T),
    Unsupported,
    Failed,
}

/// A single resolved preset slot (1–12), ready for display.
#[derive(Debug, Clone)]
pub struct PresetEntry {
    pub slot:      usize,
    pub name:      String,
    pub kind:      PresetKind,
    /// Artwork bytes for `Media` presets; empty for all other kinds, and
    /// initially empty even for a `Media` preset until `state.rs`'s
    /// per-tick preset-art dispatch fetches it (or reuses it from the
    /// previous list, if `picurl` didn't change) — `fetch_presets()` below
    /// never fetches artwork itself.
    pub art_bytes: Vec<u8>,
    /// Source URL for `art_bytes`, empty for all non-`Media` kinds. Not
    /// meaningful for display — only used to know what to fetch and to
    /// detect when a slot's artwork actually needs re-fetching (URL
    /// changed) versus can be reused (URL unchanged).
    pub picurl:    String,
}

impl PresetEntry {
    pub fn label(&self) -> &str {
        match &self.kind {
            PresetKind::Empty => "",
            _                 => &self.name,
        }
    }

    pub fn tooltip(&self) -> String {
        match &self.kind {
            PresetKind::Empty               => format!("Preset {}", self.slot),
            PresetKind::OtherRoutine        => format!("{} (preset {})", self.name, self.slot),
            PresetKind::InputSwitch  { .. } => format!("{} (preset {} — input)", self.name, self.slot),
            PresetKind::OutputSwitch { .. } => format!("{} (preset {} — output)", self.name, self.slot),
            PresetKind::Media               => format!("{} (preset {})", self.name, self.slot),
        }
    }
}

// ── OutputEntry helpers ───────────────────────────────────────────────────────

/// Derive a user-visible label for one `getSoundCardModeSupportList` entry.
///
/// Rules (in priority order):
/// 1. USB outputs → `devName`, truncated at the first `" at usb"` (case-insensitive).
/// 2. otherwise use `devName` verbatim (built-in SoC codec).
fn soundcard_display_name(canon: &'static str, dev_name: &str) -> String {
    if canon == "usb-out" && !dev_name.is_empty() {
        let lower = dev_name.to_ascii_lowercase();
        let label = match lower.find(" at usb") {
            Some(pos) => dev_name[..pos].trim(),
            None      => dev_name.trim(),
        };
        if !label.is_empty() {
            return label.to_string();
        }
    }
    return dev_name.to_string();
}

// ── PlayerStatus fixups ───────────────────────────────────────────────────────

/// Apply firmware workarounds to a freshly-parsed `PlayerStatus`.
fn fixup_player_status(st: &mut PlayerStatus) {
    // Some devices report mode 10 (HTTP network stream) while actually playing
    // from a locally-attached USB storage device.  The "vendor" field is set to
    // "UDiskLocal" in that case, which lets us detect and correct it to mode 11
    // (USB local playback).
    if st.mode == 10 && st.vendor == "UDiskLocal" {
        st.mode = 11;
    }
}

// ── Client ────────────────────────────────────────────────────────────────────

#[derive(Clone)]
pub struct WiimClient {
    http: Client,
    base: String,
    status_cmd: Arc<Mutex<Option<String>>>,
}

impl WiimClient {
    /// Create a client for `ip` using the given resolved TLS mode.
    ///
    /// Panics if `tls` is `TlsMode::Auto` — the caller must resolve it first.
    pub fn new(ip: &str, tls: TlsMode) -> Self {
        debug_info(&format!("connecting to {ip}: {}", tls.description()));
        let http = build_reqwest_client(tls, Duration::from_secs(5));
        let base = api_base_url(ip, tls);
        Self { http, base, status_cmd: Arc::new(Mutex::new(None)) }
    }

    async fn cmd(&self, command: &str) -> anyhow::Result<String> {
        const MAX_RETRIES: u32 = 3;
        let url = format!("{}?command={}", self.base, command);
        for attempt in 0..=MAX_RETRIES {
            if attempt > 0 {
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            let err = match self.http.get(&url).send().await {
                Ok(resp) => {
                    let text = resp.text().await?;
                    debug(&self.base, command, &text);
                    return Ok(text);
                }
                Err(e) => e,
            };
            // is_request() covers SendRequest errors (connection closed before
            // message completed).  These are transient; retry up to MAX_RETRIES.
            if !err.is_request() || attempt == MAX_RETRIES {
                log_request_error("API", command, &err);
                return Err(err.into());
            }
            // Attempt 1's failure is the routine, self-healing case this
            // whole retry loop exists to paper over — only log it under
            // --debug=api. A first *retry* that also fails (attempt > 0)
            // is more likely a real problem, so that always logs.
            if attempt > 0 || DEBUG.load(Ordering::Relaxed) {
                eprintln!(
                    "{} [API] {command}: transient send error (attempt {}/{}), retrying in 100ms: {err}",
                    super::timestamp(), attempt + 1, MAX_RETRIES,
                );
            }
        }
        unreachable!()
    }

    pub async fn get_status(&self) -> anyhow::Result<PlayerStatus> {
        let cached = self.status_cmd.lock().unwrap().clone();
        if let Some(cmd) = cached {
            let text = self.cmd(&cmd).await?;
            let mut st: PlayerStatus = serde_json::from_str(&text).unwrap_or_default();
            fixup_player_status(&mut st);
            return Ok(st);
        }
        for cmd in ["getPlayerStatusEx", "getPlayerStatus", "getStatusEx"] {
            if let Ok(text) = self.cmd(cmd).await {
                if let Ok(mut st) = serde_json::from_str::<PlayerStatus>(&text) {
                    if !st.status.is_empty() {
                        fixup_player_status(&mut st);
                        *self.status_cmd.lock().unwrap() = Some(cmd.to_string());
                        return Ok(st);
                    }
                }
            }
        }
        Ok(PlayerStatus::default())
    }

    pub async fn get_meta_info(&self) -> anyhow::Result<MetaData> {
        let text = self.cmd("getMetaInfo").await?;
        let resp: MetaInfoResponse = serde_json::from_str(&text).unwrap_or_default();
        Ok(resp.meta_data)
    }

    pub async fn get_device_info(&self) -> anyhow::Result<DeviceInfo> {
        let text = self.cmd("getStatusEx").await?;
        Ok(serde_json::from_str(&text).unwrap_or_default())
    }

    /// Single unretried `getStatusEx` attempt that never logs on failure —
    /// bypasses `cmd()` entirely rather than just calling it once, since
    /// `cmd()`'s failure path always calls `log_request_error()`. For
    /// liveness probing of a device already believed offline (devlist's
    /// health check): at that point a failure is the expected, routine
    /// result on every probe until the device comes back, so `cmd()`'s
    /// retry/logging (tuned for a device believed reachable) would just
    /// waste time and spam stderr once per probe forever.
    pub async fn get_device_info_quiet(&self) -> anyhow::Result<DeviceInfo> {
        let url = format!("{}?command=getStatusEx", self.base);
        let text = self.http.get(&url).send().await?.text().await?;
        Ok(serde_json::from_str(&text).unwrap_or_default())
    }

    /// Fetches `getPresetInfo`, distinguishing a confirmed-unsupported
    /// response from a merely-failed/inconclusive one — see
    /// `GetPresetsResult`'s doc comment. `Ok(PresetResponse { preset_num: 0,
    /// .. })` (a real device report of zero configured presets) is its own
    /// distinct case from either.
    async fn get_presets(&self) -> GetPresetsResult {
        let text = match self.cmd("getPresetInfo").await {
            Ok(t) => t,
            Err(_) => return GetPresetsResult::Failed,
        };
        if is_unsupported_text(&text) { return GetPresetsResult::Unsupported; }
        match serde_json::from_str(&text) {
            Ok(p) => GetPresetsResult::Ok(p),
            Err(_) => GetPresetsResult::Failed,
        }
    }

    /// `Unsupported` when the device confirms it doesn't support this
    /// command at all (the `is_unsupported_text()` sentinel — confirmed on
    /// iEAST AudioCast, which only has one output, via real captures);
    /// `Failed` when the response otherwise doesn't parse as
    /// `AudioOutputStatus` (transient — worth retrying, unlike
    /// `Unsupported`). Previously this returned `anyhow::Result` and fell
    /// back to `unwrap_or_default()` on a parse failure, which turned
    /// *both* cases into `Ok(AudioOutputStatus { hardware: "", .. })` —
    /// `state.rs`'s `handle_slow_poll_output_status()` couldn't tell that
    /// apart from genuine (if oddly empty) data, so on a device where this
    /// always fails it fired a spurious `output-changed` signal every
    /// single slow-poll cycle forever, since `""` differs from whatever
    /// the last successful call (if any) had cached. Then, even once that
    /// was fixed to a plain `Err`, the caller couldn't distinguish
    /// `Unsupported` from `Failed` either, and kept retrying a confirmed
    /// "unknown command" on the same tolerant timer as a real transient
    /// failure — this `ApiOutcome` split is what actually lets `state.rs`
    /// give up immediately on the former while still tolerating a few
    /// misses of the latter.
    pub async fn get_audio_output(&self) -> ApiOutcome<AudioOutputStatus> {
        let text = match self.cmd("getNewAudioOutputHardwareMode").await {
            Ok(t)  => t,
            Err(_) => return ApiOutcome::Failed,
        };
        if is_unsupported_text(&text) {
            return ApiOutcome::Unsupported;
        }
        match serde_json::from_str(&text) {
            Ok(v)  => ApiOutcome::Ok(v),
            Err(_) => ApiOutcome::Failed,
        }
    }

    /// `getbtstatus` — the caller (`state.rs`'s fast poll) only ever calls
    /// this while Bluetooth is the active input in the first place.
    /// `ApiOutcome::Unsupported` for a confirmed `"unknown command"`
    /// response (some devices — e.g. Audio Pro Addon C5 — don't implement
    /// this endpoint at all) — same split as `get_audio_output()`, and for
    /// the same reason: `Unsupported` is a definite answer worth acting on
    /// immediately (`state.rs` stops calling this at all once seen, rather
    /// than retrying a call that will never succeed), `Failed` is a
    /// transient transport/parse error worth tolerating.
    pub async fn get_bt_status(&self) -> ApiOutcome<BtStatus> {
        #[derive(Deserialize, Default)]
        struct Response {
            #[serde(default)]
            a2dp_sink: A2dpSink,
        }
        #[derive(Deserialize, Default)]
        struct A2dpSink {
            #[serde(default)]
            link_state: String,
            #[serde(default)]
            name: String,
            #[serde(default)]
            pairing: u8,
        }
        let text = match self.cmd("getbtstatus").await {
            Ok(t)  => t,
            Err(_) => return ApiOutcome::Failed,
        };
        if is_unsupported_text(&text) {
            return ApiOutcome::Unsupported;
        }
        let Ok(resp) = serde_json::from_str::<Response>(&text) else {
            return ApiOutcome::Failed;
        };
        ApiOutcome::Ok(BtStatus {
            connected:   resp.a2dp_sink.link_state == "connected",
            device_name: resp.a2dp_sink.name,
            pairing:     resp.a2dp_sink.pairing != 0,
        })
    }

    /// Returns each input and whether it is enabled (1) or disabled (0), or
    /// `None` if the call failed or didn't parse (device doesn't support
    /// the API, or something else went wrong) — distinct from `Some(vec![])`,
    /// a real empty list. Real devices wrap the array
    /// (`{"audioInput": [...], "ver": "1.0"}`, confirmed via captures) —
    /// a previous version of this parsed it as a bare array, which silently
    /// failed every time against real hardware.
    pub async fn get_audio_input_enable(&self) -> Option<Vec<AudioInputEntry>> {
        #[derive(Deserialize)]
        struct Response {
            #[serde(default, rename = "audioInput")]
            audio_input: Vec<AudioInputEntry>,
        }
        let text = self.cmd("getAudioInputEnable").await.ok()?;
        serde_json::from_str::<Response>(&text).ok().map(|r| r.audio_input)
    }

    /// Authoritative list of audio input source IDs the device exposes, from
    /// the WiiM app's `getAudioInputCapbility` command (the name really is
    /// misspelled "Capbility" in the firmware). The returned `mode` strings
    /// are already in our canonical wire form (`"wifi"`, `"line-in"`,
    /// `"bluetooth"`, `"optical"`, `"phono"`, `"HDMI"`, `"udisk"`, …) — the
    /// same values `switchmode`/`PlayMedium` use — so callers can treat them
    /// as `InputEntry.id`s directly with no translation. Same wrapped shape
    /// as `getAudioInputEnable` (`{"audioInput": [...], "ver": "1.0"}`).
    ///
    /// `None` if the call failed or didn't parse — distinct from
    /// `Some(vec![])`, a real empty list. Most non-WiiM devices answer a
    /// literal `"unknown command"` string here (confirmed via captures:
    /// Audio Pro, iEAST AudioCast, WiiM Mini), which doesn't deserialize as
    /// the wrapped object and so correctly yields `None`.
    pub async fn get_audio_input_capability(&self) -> Option<Vec<String>> {
        let text = self.cmd("getAudioInputCapbility").await.ok()?;
        parse_audio_input_capability(&text)
    }

    /// Returns user-assigned names keyed by input mode string.
    /// Returns an empty map if the device doesn't support the API or returns "Failed".
    pub async fn get_mode_rename(&self) -> std::collections::HashMap<String, String> {
        match self.cmd("getModeRename").await {
            Ok(text) if !text.trim().eq_ignore_ascii_case("failed") => {
                serde_json::from_str(&text).unwrap_or_default()
            }
            _ => std::collections::HashMap::new(),
        }
    }

    /// Returns the list of routines configured on the device.
    /// Returns an empty Vec if the device doesn't support the API.
    pub async fn get_all_routines(&self) -> Vec<Routine> {
        match self.cmd("getAllRoutines").await {
            Ok(text) => serde_json::from_str::<RoutinesResponse>(&text)
                .map(|r| r.routines)
                .unwrap_or_default(),
            Err(_) => Vec::new(),
        }
    }

    // ── Playback control ──────────────────────────────────────────────

    pub async fn play(&self) -> anyhow::Result<()> {
        self.cmd("setPlayerCmd:resume").await?;
        Ok(())
    }

    pub async fn pause(&self) -> anyhow::Result<()> {
        self.cmd("setPlayerCmd:pause").await?;
        Ok(())
    }

    pub async fn next(&self) -> anyhow::Result<()> {
        self.cmd("setPlayerCmd:next").await?;
        Ok(())
    }

    pub async fn prev(&self) -> anyhow::Result<()> {
        self.cmd("setPlayerCmd:prev").await?;
        Ok(())
    }

    pub async fn set_volume(&self, vol: u32) -> anyhow::Result<()> {
        self.cmd(&format!("setPlayerCmd:vol:{vol}")).await?;
        Ok(())
    }

    pub async fn set_mute(&self, mute: bool) -> anyhow::Result<()> {
        self.cmd(&format!("setPlayerCmd:mute:{}", mute as u8)).await?;
        Ok(())
    }

    pub async fn set_loop_mode(&self, mode: i32) -> anyhow::Result<()> {
        self.cmd(&format!("setPlayerCmd:loopmode:{mode}")).await?;
        Ok(())
    }

    pub async fn seek(&self, position_secs: u32) -> anyhow::Result<()> {
        self.cmd(&format!("setPlayerCmd:seek:{position_secs}")).await?;
        Ok(())
    }

    pub async fn play_preset(&self, number: u32) -> anyhow::Result<()> {
        self.cmd(&format!("MCUKeyShortClick:{number}")).await?;
        Ok(())
    }

    pub async fn switch_input(&self, source: &str) -> anyhow::Result<()> {
        self.cmd(&format!("setPlayerCmd:switchmode:{source}")).await?;
        Ok(())
    }

    /// Puts the device's Bluetooth A2DP sink back into pairing mode —
    /// exposed in the UI as "Restart pairing", shown only while Bluetooth
    /// is the active input and nothing is currently connected.
    pub async fn bt_enter_pair(&self) -> anyhow::Result<()> {
        self.cmd("btavkenterpair").await?;
        Ok(())
    }

    pub async fn set_audio_output(&self, mode: u32) -> anyhow::Result<()> {
        self.cmd(&format!("setAudioOutputHardwareMode:{mode}")).await?;
        Ok(())
    }

    /// Query `getSoundCardModeSupportList`.
    ///
    /// `Ok(list)` has an `OutputEntry` for every output the device
    /// currently supports ("unknown" canonical names and duplicates
    /// discarded). `Unsupported` on the confirmed-not-supported sentinel,
    /// `Failed` on any other transport/parse failure — see
    /// `ApiOutcome`'s doc comment for why callers must treat those two
    /// differently (only `Failed` is worth retrying).
    pub async fn get_sound_card_mode_support_list(&self) -> ApiOutcome<Vec<OutputEntry>> {
        let text = match self.cmd("getSoundCardModeSupportList").await {
            Ok(t)  => t,
            Err(_) => return ApiOutcome::Failed,
        };
        if is_unsupported_text(&text) {
            return ApiOutcome::Unsupported;
        }
        let Ok(arr) = serde_json::from_str::<Vec<serde_json::Value>>(&text) else {
            return ApiOutcome::Failed;
        };
        let mut seen = std::collections::HashSet::new();
        let outputs = arr.iter()
            .filter_map(|v| {
                let canon = super::capabilities::canon_new_output_name(
                    v["mode"].as_str()?
                );
                if canon == "unknown" || !seen.insert(canon) { return None; }
                let dev_name  = v["soundCard"]["devName"].as_str().unwrap_or("");
                let name = soundcard_display_name(canon, dev_name);
                // `icon_canon` is filled in by `capabilities::detect_capabilities()`,
                // which knows the device's profile; this method only talks to
                // the wire format.
                Some(OutputEntry { canon, icon_canon: canon, name })
            })
            .collect();
        ApiOutcome::Ok(outputs)
    }

    // ── Fetch helpers ─────────────────────────────────────────────────

    pub async fn fetch_bytes(&self, url: &str) -> anyhow::Result<Vec<u8>> {
        let bytes = self.http.get(url).send().await?.bytes().await?;
        Ok(bytes.to_vec())
    }

    /// Fetch and resolve the preset list's metadata (name/kind/picurl per
    /// slot) — artwork bytes are always empty in the returned entries; see
    /// `PresetEntry::art_bytes`'s doc comment for where those actually get
    /// filled in.
    ///
    /// Pass the fingerprint from the previous call as `old_fp`. If the
    /// fingerprint is unchanged this returns `Unchanged` (no need to
    /// rebuild the list at all) *before* doing any of the more expensive
    /// entry-building work below — the fingerprint is computed straight
    /// from the raw `preset_list`/routines, not from already-built
    /// `PresetEntry` values. On a fresh connection pass an empty string.
    pub async fn fetch_presets(&self, old_fp: &str) -> PresetFetchOutcome {
        use std::collections::{BTreeSet, HashMap, HashSet};

        let (presets_result, routines) =
            tokio::join!(self.get_presets(), self.get_all_routines());
        let presets = match presets_result {
            GetPresetsResult::Ok(p)      => p,
            GetPresetsResult::Unsupported => return PresetFetchOutcome::Unsupported,
            GetPresetsResult::Failed      => return PresetFetchOutcome::Failed,
        };
        let preset_total = presets.preset_num as usize;

        let mut routine_map: HashMap<usize, Routine> = HashMap::new();
        for r in routines {
            let slot = r.index as usize + 1;
            if (1..=12).contains(&slot) { routine_map.insert(slot, r); }
        }

        let mut seen: HashSet<usize> = HashSet::new();
        for p in &presets.preset_list {
            let s = p.number as usize;
            if (1..=12).contains(&s) { seen.insert(s); }
        }

        let mut all_slots: BTreeSet<usize> = seen.iter().copied().collect();
        for n in 1..=preset_total.min(12) { all_slots.insert(n); }
        for &s in routine_map.keys()       { all_slots.insert(s); }

        // Build fingerprint from list metadata (no artwork needed for this).
        let fp = {
            let mut parts: Vec<String> = presets.preset_list.iter()
                .map(|p| format!("{}:{}:{}", p.number, p.name, p.picurl))
                .collect();
            for &n in &all_slots {
                if !seen.contains(&n) {
                    if let Some(r) = routine_map.get(&n) {
                        parts.push(format!("r{}:{}", n, r.name));
                    } else {
                        parts.push(format!("{n}:empty"));
                    }
                }
            }
            parts.sort();
            parts.join("|")
        };

        // Skip entirely if nothing changed — including artwork: `state.rs`
        // reuses whatever it already has for a slot whose `picurl` hasn't
        // changed, so there's nothing to redo here either way.
        if fp == old_fp { return PresetFetchOutcome::Unchanged; }

        // Build entries. Artwork is *not* fetched here — these URLs are on
        // external CDN hosts (Pandora, Spotify, etc.), not the embedded
        // device itself, so fetching them doesn't belong in this
        // device-API-call function at all: `state.rs` dispatches and
        // collects those fetches itself, on the 1-second fast-poll tick
        // rather than this (at-most-every-10-seconds) slow-poll phase, so a
        // slow/throttled CDN request never holds up noticing an actual
        // preset-list change on the device.
        let mut entries: Vec<PresetEntry> = Vec::new();
        for p in &presets.preset_list {
            let slot = p.number as usize;
            if !(1..=12).contains(&slot) { continue; }
            entries.push(PresetEntry {
                slot, name: p.name.clone(), kind: PresetKind::Media,
                art_bytes: Vec::new(), picurl: p.picurl.clone(),
            });
        }

        for &n in &all_slots {
            if seen.contains(&n) { continue; }
            let kind = if let Some(r) = routine_map.get(&n) {
                if let Some(id) = r.audio_input() {
                    PresetKind::InputSwitch { input_id: id.to_string() }
                } else if let Some(oid) = r.audio_output() {
                    PresetKind::OutputSwitch { output_id: oid.to_string() }
                } else {
                    PresetKind::OtherRoutine
                }
            } else {
                PresetKind::Empty
            };
            let name = routine_map.get(&n).map(|r| r.name.clone()).unwrap_or_default();
            entries.push(PresetEntry { slot: n, name, kind, art_bytes: Vec::new(), picurl: String::new() });
        }

        entries.sort_by_key(|e| e.slot);
        PresetFetchOutcome::Changed(fp, entries)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capture::format::CaptureFile;

    /// Real iEAST AudioCast unit, captured with one preset configured
    /// device-side. `getStatusEx`'s `preset_key` reports the device's max
    /// preset slot count regardless of whether `getPresetInfo` itself works
    /// (it's confirmed unsupported on this device — see
    /// `capabilities.rs`'s `ieast_audiocast_real_capture_has_no_forced_inputs_or_extra_outputs`).
    #[test]
    fn device_info_recovers_preset_key() {
        let path = format!(
            "{}/captures/test-devices/AudioCastBu_20260708_095957.json",
            env!("CARGO_MANIFEST_DIR"),
        );
        let text = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("reading fixture {path}: {e}"));
        let cap: CaptureFile = serde_json::from_str(&text)
            .unwrap_or_else(|e| panic!("parsing fixture {path}: {e}"));
        let body = cap.commands.iter()
            .find(|c| c.command == "getStatusEx")
            .expect("capture has no getStatusEx")
            .body.clone()
            .expect("getStatusEx has no body");
        let info: DeviceInfo = serde_json::from_value(body).expect("parsing DeviceInfo");
        assert_eq!(info.preset_key, "6");
    }

    /// Helper: pull one command's captured body back out as the raw response
    /// text the device would have sent, for feeding to a parser under test.
    fn capture_body_text(cap: &CaptureFile, command: &str) -> String {
        let body = cap.commands.iter()
            .find(|c| c.command == command)
            .unwrap_or_else(|| panic!("capture has no {command}"))
            .body.clone()
            .unwrap_or_else(|| panic!("{command} has no body"));
        match body {
            // "unknown command"/"Failed" etc. — captured as a bare JSON string;
            // the device sends it unquoted, so hand the parser the inner text.
            serde_json::Value::String(s) => s,
            other => serde_json::to_string(&other).expect("serializing body"),
        }
    }

    fn load_capture(filename: &str) -> CaptureFile {
        let path = format!("{}/captures/test-devices/{filename}", env!("CARGO_MANIFEST_DIR"));
        let text = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("reading fixture {path}: {e}"));
        serde_json::from_str(&text)
            .unwrap_or_else(|e| panic!("parsing fixture {path}: {e}"))
    }

    /// Real WiiM Ultra capture: `getAudioInputCapbility` returns the
    /// authoritative input list as canonical wire IDs (`wifi`, `line-in`,
    /// `HDMI`, `udisk`, …) that flow straight into `InputEntry.id`.
    #[test]
    fn audio_input_capability_parses_wiim_ultra() {
        let cap = load_capture("WiiM_Ultra_20260708_100034.json");
        let text = capture_body_text(&cap, "getAudioInputCapbility");
        let ids = parse_audio_input_capability(&text).expect("should parse");
        assert_eq!(
            ids,
            vec!["wifi", "line-in", "bluetooth", "optical", "phono", "HDMI", "udisk"],
        );
    }

    /// A device that doesn't support the call answers a literal
    /// "unknown command" string, which must parse to `None` (not an empty
    /// list) so the caller keeps its plm_support-derived input list.
    #[test]
    fn audio_input_capability_unknown_command_is_none() {
        let cap = load_capture("WiiM_Mini_20260708_045125.json");
        let text = capture_body_text(&cap, "getAudioInputCapbility");
        assert!(parse_audio_input_capability(&text).is_none());
    }
}
