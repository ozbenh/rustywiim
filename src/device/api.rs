#![allow(dead_code)] // API surface used by future modules

use reqwest::Client;
use serde::Deserialize;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

pub static DEBUG: AtomicBool = AtomicBool::new(false);

fn debug(cmd: &str, resp: &str) {
    if DEBUG.load(Ordering::Relaxed) {
        println!("[API] {cmd} → {resp}");
    }
}

fn debug_info(msg: &str) {
    if DEBUG.load(Ordering::Relaxed) {
        println!("[API] {msg}");
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
/// CA/identity PEMs — ~250M instructions per call (see ANALYSIS.md). Every
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

/// Log a reqwest error for `context` (e.g. an API command or a discovery probe).
///
/// Always prints to stderr.  Walks the full `source()` chain so that the root
/// cause (e.g. the specific TLS or certificate failure) is visible.
pub fn log_request_error(context: &str, err: &reqwest::Error) {
    use std::error::Error as StdError;
    eprintln!("[API] {context}: {err}");
    let mut cause: Option<&dyn StdError> = err.source();
    while let Some(c) = cause {
        eprintln!("[API]   caused by: {c}");
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
        }
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
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct AudioInputEntry {
    #[serde(default)]
    pub mode:   String,
    #[serde(default)]
    pub enable: u8,
}

impl AudioInputEntry {
    pub fn is_enabled(&self) -> bool { self.enable != 0 }
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct AudioOutputStatus {
    #[serde(default)]
    pub hardware: String,
    #[serde(default)]
    pub source: String,
}

/// One entry in the device's supported-outputs list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutputEntry {
    /// Canonical internal name (e.g. `"line-out"`, `"usb-out"`).
    pub canon: &'static str,
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

/// A single resolved preset slot (1–12), ready for display.
#[derive(Debug, Clone)]
pub struct PresetEntry {
    pub slot:      usize,
    pub name:      String,
    pub kind:      PresetKind,
    /// Artwork bytes for `Media` presets; empty for all other kinds.
    pub art_bytes: Vec<u8>,
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
/// 2. `cardName == "AMLAUGESOUND"` → use `devName` verbatim (built-in SoC codec).
/// 3. Fallback → `output_display_name(canon)`.
fn soundcard_display_name(canon: &'static str, card_name: &str, dev_name: &str) -> String {
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
    if card_name == "AMLAUGESOUND" && !dev_name.is_empty() {
        return dev_name.to_string();
    }
    super::capabilities::output_display_name(canon).to_string()
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
                    debug(command, &text);
                    return Ok(text);
                }
                Err(e) => e,
            };
            // is_request() covers SendRequest errors (connection closed before
            // message completed).  These are transient; retry up to MAX_RETRIES.
            if !err.is_request() || attempt == MAX_RETRIES {
                log_request_error(command, &err);
                return Err(err.into());
            }
            eprintln!(
                "[API] {command}: transient send error (attempt {}/{}), retrying in 100ms: {err}",
                attempt + 1, MAX_RETRIES,
            );
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

    pub async fn get_presets(&self) -> anyhow::Result<PresetResponse> {
        let text = self.cmd("getPresetInfo").await?;
        Ok(serde_json::from_str(&text).unwrap_or_default())
    }

    pub async fn get_audio_output(&self) -> anyhow::Result<AudioOutputStatus> {
        let text = self.cmd("getNewAudioOutputHardwareMode").await?;
        Ok(serde_json::from_str(&text).unwrap_or_default())
    }

    /// Returns each input and whether it is enabled (1) or disabled (0), or
    /// `None` if the call failed or didn't parse (device doesn't support
    /// the API, or something else went wrong) — distinct from `Some(vec![])`,
    /// a real empty list. Real devices wrap the array
    /// (`{"audioInput": [...], "ver": "1.0"}`, confirmed via captures) —
    /// a previous version of this parsed it as a bare array, which silently
    /// failed every time against real hardware (see ANALYSIS.md item 19).
    pub async fn get_audio_input_enable(&self) -> Option<Vec<AudioInputEntry>> {
        #[derive(Deserialize)]
        struct Response {
            #[serde(default, rename = "audioInput")]
            audio_input: Vec<AudioInputEntry>,
        }
        let text = self.cmd("getAudioInputEnable").await.ok()?;
        serde_json::from_str::<Response>(&text).ok().map(|r| r.audio_input)
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

    pub async fn set_audio_output(&self, mode: u32) -> anyhow::Result<()> {
        self.cmd(&format!("setAudioOutputHardwareMode:{mode}")).await?;
        Ok(())
    }

    /// Query `getSoundCardModeSupportList`.
    ///
    /// Returns `Some(list)` with `OutputEntry` items for every output the device
    /// currently supports. "unknown" canonical names and duplicates are discarded.
    /// Returns `None` if the response is not a JSON array — the caller should
    /// treat this as "API not supported" and stop calling.
    pub async fn get_sound_card_mode_support_list(&self) -> Option<Vec<OutputEntry>> {
        let text = self.cmd("getSoundCardModeSupportList").await.ok()?;
        let arr: Vec<serde_json::Value> = serde_json::from_str(&text).ok()?;
        let mut seen = std::collections::HashSet::new();
        let outputs = arr.iter()
            .filter_map(|v| {
                let canon = super::capabilities::canon_new_output_name(
                    v["mode"].as_str()?
                );
                if canon == "unknown" || !seen.insert(canon) { return None; }
                let card_name = v["soundCard"]["cardName"].as_str().unwrap_or("");
                let dev_name  = v["soundCard"]["devName"].as_str().unwrap_or("");
                let name = soundcard_display_name(canon, card_name, dev_name);
                Some(OutputEntry { canon, name })
            })
            .collect();
        Some(outputs)
    }

    // ── Fetch helpers ─────────────────────────────────────────────────

    pub async fn fetch_bytes(&self, url: &str) -> anyhow::Result<Vec<u8>> {
        let bytes = self.http.get(url).send().await?.bytes().await?;
        Ok(bytes.to_vec())
    }

    /// Fetch and resolve the full preset list (metadata + artwork).
    ///
    /// Pass the fingerprint from the previous call as `old_fp`.  If the
    /// fingerprint is unchanged this returns `None` (expensive artwork
    /// downloads are skipped).  On a fresh connection pass an empty string.
    pub async fn fetch_presets(&self, old_fp: &str) -> Option<(String, Vec<PresetEntry>)> {
        use std::collections::{BTreeSet, HashMap, HashSet};

        let (presets_result, routines) =
            tokio::join!(self.get_presets(), self.get_all_routines());
        let presets      = presets_result.unwrap_or_default();
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

        // Skip artwork fetch if nothing changed.
        if fp == old_fp { return None; }

        // Build entries, fetching artwork only for media presets.
        let mut entries: Vec<PresetEntry> = Vec::new();

        for p in &presets.preset_list {
            let slot = p.number as usize;
            if !(1..=12).contains(&slot) { continue; }
            let art_bytes = if !p.picurl.is_empty() {
                self.fetch_bytes(&p.picurl).await.unwrap_or_default()
            } else {
                Vec::new()
            };
            entries.push(PresetEntry { slot, name: p.name.clone(), kind: PresetKind::Media, art_bytes });
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
            entries.push(PresetEntry { slot: n, name, kind, art_bytes: Vec::new() });
        }

        entries.sort_by_key(|e| e.slot);
        Some((fp, entries))
    }
}
