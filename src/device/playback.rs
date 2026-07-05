/// Canonical playback state, decoupled from whichever backend populated it
/// (today: LinkPlay HTTP `getPlayerStatusEx`/`getMetaInfo`; later: UPnP).
/// See `/PLAYBACKSTATE.md` at the repo root for the full design rationale.
///
/// This module owns:
/// - The canonical `PlaybackState` struct + its component enums, built once
///   per device and updated in place by `state.rs` (never rebuilt/diffed
///   wholesale — see `state.rs::process_poll`).
/// - `AccessMethod`/`PlaybackAccessConfig`: per-field-group backend
///   selection, driven by device capability profiles and optionally
///   overridden per-device via Settings' Advanced panel.
/// - The `decode_*_http` functions: LinkPlay wire format -> canonical fields.
///   Presentation (turning canonical values into display strings/icons)
///   stays in `ui/playback.rs`.

use std::rc::Rc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use super::state::DEBUG_STATE;

fn dbg(msg: &str) {
    if DEBUG_STATE.load(Ordering::Relaxed) {
        println!("[playback] {msg}");
    }
}

// ── Canonical playback state ──────────────────────────────────────────────────

/// Canonical playback status, independent of which backend (HTTP polling,
/// UPnP polling, eventually UPnP eventing) populated it. Lives once per
/// device, in `Inner.playback` — updated in place, field by field, as
/// changes are detected by `state.rs::process_poll`, never rebuilt from
/// scratch and diffed wholesale every tick.
///
/// Every heap-allocated field is `Rc`-wrapped so that cloning the whole
/// struct (the pattern every `DeviceState` accessor uses to hand data out of
/// a `RefCell`) is a handful of refcount bumps, not a deep string/byte copy.
#[derive(Debug, Clone, PartialEq)]
pub struct PlaybackState {
    pub status:      PlaybackStatus,
    /// Decoded, display-ready source label ("AirPlay", "Spotify", "TuneIn"...).
    /// `None` when idle or not meaningful. NOT the raw mode code used to
    /// drive the input-selector dropdown — that stays a separate accessor
    /// (`DeviceState::current_mode()`), since it has to match capability-
    /// derived source IDs, not a display string.
    pub source_name: Option<Rc<str>>,
    pub title:       Rc<str>,
    pub artist:      Rc<str>,
    pub album:       Rc<str>,
    pub position:    Duration,
    pub duration:    Duration,
    pub volume:      u32,
    pub muted:       bool,
    pub shuffle:     bool,
    pub repeat:      RepeatMode,
    pub quality:     Option<AudioQuality>,
    /// Artwork URL — carried even though nothing reads it yet beyond driving
    /// the fetch pipeline in state.rs. Cheap to include, and doubles as a
    /// de-dupe key.
    pub art_url:     Option<Rc<str>>,
    /// Decoded image bytes; `None` until fetch resolves, or the track
    /// genuinely has none. `Rc`, not an owned `Vec<u8>` — produced once by
    /// the async fetch/decode pipeline (`DeviceState::start_art_loader`),
    /// shared from there via cheap `Rc::clone`, never copied byte-for-byte.
    pub artwork:     Option<Rc<Vec<u8>>>,
}

impl Default for PlaybackState {
    fn default() -> Self {
        Self {
            status:      PlaybackStatus::Stopped,
            source_name: None,
            title:       Rc::from(""),
            artist:      Rc::from(""),
            album:       Rc::from(""),
            position:    Duration::ZERO,
            duration:    Duration::ZERO,
            volume:      0,
            muted:       false,
            shuffle:     false,
            repeat:      RepeatMode::Off,
            quality:     None,
            art_url:     None,
            artwork:     None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RepeatMode {
    Off,
    All,
    One,
}

impl RepeatMode {
    /// Cycle Off -> All -> One -> Off, used by the repeat button's click handler.
    pub fn next(self) -> Self {
        match self {
            Self::Off => Self::All,
            Self::All => Self::One,
            Self::One => Self::Off,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum PlaybackStatus {
    Stopped,
    Playing,
    Paused,
    Loading,
    /// Unrecognized wire value, preserved verbatim rather than dropped —
    /// covers both HTTP's (play/pause/stop/loading) and UPnP's disjoint
    /// (PLAYING/PAUSED_PLAYBACK/STOPPED/TRANSITIONING/NO_MEDIA_PRESENT)
    /// vocabularies without either backend needing to know about the other.
    Unknown(String),
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct AudioQuality {
    pub bit_rate_kbps:   Option<f64>,
    pub sample_rate_khz: Option<f64>,
    pub bit_depth:       Option<u32>,
}

// ── Access method / per-field backend selection ───────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AccessMethod {
    /// `getPlayerStatusEx` (or whichever of `getPlayerStatusEx`/
    /// `getPlayerStatus`/`getStatusEx` `WiimClient::get_status()` resolves to
    /// — that fallback probing already lives in `api.rs` and is reused
    /// as-is, not duplicated here) — today's only source for
    /// status/timing/volume/mute/source.
    HttpPlayerStatusEx,
    /// `getMetaInfo` — today's only source for metadata/artwork.
    HttpMetaInfo,
    /// Not yet implemented — accepted by config/UI plumbing so the choice is
    /// persisted and visible, but `state.rs`'s poll loop has no UPnP fetch
    /// path yet and falls back to the HTTP default with a debug warning if
    /// selected. See `/PLAYBACKSTATE.md`'s "Non-goals".
    UpnpPolled,
}

/// Per-device-profile choice of which backend supplies each field group of
/// `PlaybackState`. Static — decided once from the device's capability
/// profile (optionally overridden per-device via Settings' Advanced panel),
/// not re-arbitrated live between two concurrently-running transports.
///
/// Grouped rather than one flag per struct field: `getPlayerStatusEx`
/// returns status/volume/mute/position/duration/mode/vendor in one call, and
/// no prior-art project was found splitting position from duration, or
/// volume from mute, across different sources — so those fold into `timing`
/// and `volume`. `source` stays independent rather than folded into `status`
/// or `metadata`: the natural bundling differs by backend (HTTP: rides with
/// `status`; UPnP: rides with `metadata`), and it's unverified whether
/// `getMetaInfo` might carry better source-identifying info than
/// `getPlayerStatusEx`'s `mode`/`vendor` — see `/PLAYBACKSTATE.md`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PlaybackAccessConfig {
    pub status:   AccessMethod,
    pub timing:   AccessMethod,
    pub volume:   AccessMethod,
    pub metadata: AccessMethod,
    pub artwork:  AccessMethod,
    pub source:   AccessMethod,
}

impl PlaybackAccessConfig {
    /// Today's actual behavior, with the two HTTP endpoints named explicitly
    /// instead of a single generic `Http`. `const fn` (not just `Default`)
    /// so `capabilities.rs`'s `static FamilyProfile` table entries can use
    /// it directly.
    pub const fn all_http() -> Self {
        Self {
            status:   AccessMethod::HttpPlayerStatusEx,
            timing:   AccessMethod::HttpPlayerStatusEx,
            volume:   AccessMethod::HttpPlayerStatusEx,
            metadata: AccessMethod::HttpMetaInfo,
            artwork:  AccessMethod::HttpMetaInfo,
            source:   AccessMethod::HttpPlayerStatusEx,
        }
    }
}

impl Default for PlaybackAccessConfig {
    fn default() -> Self {
        Self::all_http()
    }
}

impl PlaybackAccessConfig {
    /// Apply a per-device override (`None` entries mean "keep the profile
    /// default"), producing the effective config for this device. See
    /// `config::PlaybackAccessOverride` — deliberately never the other way
    /// around (never resolve an override's `None` into a concrete value
    /// before *saving* it back to config; that's a config.rs/settings.rs
    /// concern, not this function's).
    pub fn with_overrides(mut self, over: PlaybackAccessOverrideRef) -> Self {
        if let Some(v) = over.status   { self.status   = v; }
        if let Some(v) = over.timing   { self.timing   = v; }
        if let Some(v) = over.volume   { self.volume   = v; }
        if let Some(v) = over.metadata { self.metadata = v; }
        if let Some(v) = over.artwork  { self.artwork  = v; }
        if let Some(v) = over.source   { self.source   = v; }
        self
    }

    /// Debug-log (gated on `DEBUG_STATE`) any field group set to
    /// `AccessMethod::UpnpPolled` — not implemented yet, falls back silently
    /// to HTTP in the poll loop otherwise, which would be surprising without
    /// this note.
    pub fn warn_unimplemented(&self) {
        if !DEBUG_STATE.load(Ordering::Relaxed) {
            return;
        }
        let groups: &[(&str, AccessMethod)] = &[
            ("status", self.status), ("timing", self.timing),
            ("volume", self.volume), ("metadata", self.metadata),
            ("artwork", self.artwork), ("source", self.source),
        ];
        for (name, method) in groups {
            if *method == AccessMethod::UpnpPolled {
                dbg(&format!(
                    "access config: {name} set to UpnpPolled, which isn't \
                     implemented yet — falling back to the HTTP default"
                ));
            }
        }
    }
}

/// Plain-field mirror of `config::PlaybackAccessOverride`, used so this
/// module doesn't need to depend on `config` (which lives in the main
/// binary crate, not this library crate) just to apply an override.
/// `config.rs` builds one of these from its own `PlaybackAccessOverride`
/// when resolving the effective config for a device.
#[derive(Debug, Clone, Copy, Default)]
pub struct PlaybackAccessOverrideRef {
    pub status:   Option<AccessMethod>,
    pub timing:   Option<AccessMethod>,
    pub volume:   Option<AccessMethod>,
    pub metadata: Option<AccessMethod>,
    pub artwork:  Option<AccessMethod>,
    pub source:   Option<AccessMethod>,
}

// ── HTTP (LinkPlay getPlayerStatusEx / getMetaInfo) decoders ──────────────────
//
// Wire format -> canonical value. Assume the fixups already applied at the
// api.rs layer (e.g. `fixup_player_status`'s mode-10/vendor-"UDiskLocal"
// correction) have already run — these decoders operate on the corrected
// `PlayerStatus`/`MetaData` fields, same as every other consumer of `mode`.

pub fn decode_status_http(raw: &str) -> PlaybackStatus {
    match raw {
        "play"    => PlaybackStatus::Playing,
        "pause"   => PlaybackStatus::Paused,
        "stop"    => PlaybackStatus::Stopped,
        "loading" => PlaybackStatus::Loading,
        other     => PlaybackStatus::Unknown(other.to_string()),
    }
}

fn mode_source(mode: i32) -> &'static str {
    match mode {
        -1              => "Idle", // missing/unparseable sentinel — see de_i32_or_neg1
        0               => "Idle",
        1               => "AirPlay",
        2               => "DLNA",
        5               => "Chromecast",
        10 | 20         => "WiFi",
        11 | 42 | 51    => "USB",
        31              => "Spotify",
        32              => "TIDAL Connect",
        34              => "Lyrion",
        36              => "Qobuz",
        40 | 60         => "Line-In",
        41              => "Bluetooth",
        43              => "Optical",
        44              => "RCA",
        49              => "HDMI",
        54              => "Phono",
        99              => "Follower",
        _               => "",
    }
}

fn vendor_display(vendor: &str) -> &'static str {
    let v: String = vendor.to_lowercase().chars().filter(|c| !c.is_whitespace()).collect();
    match v.as_str() {
        "newtunein" | "tunein"              => "TuneIn",
        "iheartradio" | "iheart"            => "iHeartRadio",
        "spotify"                            => "Spotify",
        "tidal"                              => "TIDAL",
        "amazon" | "amazonmusic"             => "Amazon Music",
        "deezer"                             => "Deezer",
        "qobuz"                              => "Qobuz",
        "pandora"                            => "Pandora",
        "napster"                            => "Napster",
        "radioparadise"                      => "Radio Paradise",
        "vtuner"                             => "vTuner",
        "linkplayradio"                      => "Radio",
        "custompushurl"                      => "URL",
        "cast"                               => "Chromecast",
        _                                    => "",
    }
}

/// `None` when idle or the decoded label isn't meaningful (mirrors the
/// display-omission rule the old `format_status()` used to apply inline).
pub fn decode_source_name_http(mode: i32, vendor: &str) -> Option<Rc<str>> {
    let source_name = match mode {
        10 | 20 | 0 | 5 => {
            let vn = vendor_display(vendor);
            if !vn.is_empty() { vn } else { mode_source(mode) }
        }
        _ => mode_source(mode),
    };
    match source_name {
        "" | "Idle" => None,
        s           => Some(Rc::from(s)),
    }
}

/// `loop_mode` is always a stringified 0-5 integer on the wire; `-1` is the
/// missing/unparseable sentinel (`de_i32_or_neg1`), which falls through to
/// the same `(false, Off)` catch-all any other unrecognized value would —
/// reproducing exactly the "missing -> Off" behavior the old empty/garbage
/// string catch-all gave.
pub fn decode_loop_mode_http(loop_mode: i32) -> (bool, RepeatMode) {
    match loop_mode {
        4 => (false, RepeatMode::Off),
        0 => (false, RepeatMode::All),
        1 => (false, RepeatMode::One),
        3 => (true,  RepeatMode::Off),
        2 => (true,  RepeatMode::All),
        5 => (true,  RepeatMode::One),
        _ => (false, RepeatMode::Off),
    }
}

pub fn decode_quality_http(bit_rate: &str, sample_rate: &str, bit_depth: &str) -> Option<AudioQuality> {
    let br = bit_rate.trim();
    let sr = sample_rate.trim();
    let bd = bit_depth.trim();
    let has_br = !br.is_empty() && br != "0";
    let has_sr = !sr.is_empty() && sr != "0";
    if !has_br && !has_sr {
        return None;
    }
    let bit_rate_kbps   = if has_br { br.parse::<f64>().ok() } else { None };
    let sample_rate_khz = if has_sr { sr.parse::<f64>().ok().map(|v| v / 1000.0) } else { None };
    let bit_depth_val   = if !bd.is_empty() && bd != "0" { bd.parse::<u32>().ok() } else { None };
    Some(AudioQuality { bit_rate_kbps, sample_rate_khz, bit_depth: bit_depth_val })
}

/// Sources documented to always report `curpos`/`totlen` in milliseconds
/// (pywiim's `_MILLISECOND_TIME_SOURCES`, citing `mjcumming/wiim#75`: some
/// streaming services report microseconds instead, with no wire-level flag
/// to distinguish the two). Everything else falls through to the magnitude
/// heuristic in `decode_timing_http`.
const ALWAYS_MS_MODES: &[i32] = &[
    1,          // AirPlay
    2,          // DLNA
    10, 20,     // network/local URL, WiFi
    11, 42, 51, // USB
];

/// 10 hours in milliseconds — same threshold pywiim uses to decide whether a
/// value that doesn't match an `ALWAYS_MS_MODES` source is more plausibly
/// milliseconds or microseconds.
const MS_THRESHOLD: u64 = 36_000_000;

/// Whether this tick's `curpos`/`totlen` reading is trustworthy at all.
/// `curpos > totlen` (when `totlen` is actually known) is a documented
/// garbage-value case (linkplay-cli, pywiim) rather than a real position —
/// callers should skip updating position/duration for a tick that fails
/// this check rather than decode and display it.
pub fn timing_looks_valid(curpos: u64, totlen: u64) -> bool {
    totlen == 0 || curpos <= totlen
}

/// Converts raw `curpos`/`totlen` (assumed already validated via
/// `timing_looks_valid`) into `Duration`s, applying the ms-vs-µs heuristic.
pub fn decode_timing_http(curpos: u64, totlen: u64, mode: i32) -> (Duration, Duration) {
    let treat_as_ms = ALWAYS_MS_MODES.contains(&mode) || curpos < MS_THRESHOLD;
    let (cur_ms, tot_ms) = if treat_as_ms {
        (curpos, totlen)
    } else {
        (curpos / 1000, totlen / 1000)
    };
    (Duration::from_millis(cur_ms), Duration::from_millis(tot_ms))
}
