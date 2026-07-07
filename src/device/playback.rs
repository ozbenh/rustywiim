/// Canonical playback state, decoupled from whichever backend populated it
/// (today: LinkPlay HTTP `getPlayerStatusEx`/`getMetaInfo`; later: UPnP).
///
/// This module owns:
/// - The canonical `PlaybackState` struct + its component enums, built once
///   per device and updated in place by `state.rs` (never rebuilt/diffed
///   wholesale — see `state.rs::process_poll`).
/// - `AccessMethod`: which backend supplies playback state for a device,
///   driven by device capability profiles and optionally overridden
///   per-device via Settings' Advanced panel.
/// - The `decode_*_http` functions: LinkPlay wire format -> canonical fields.
///   Presentation (turning canonical values into display strings/icons)
///   stays in `ui/playback.rs`.

use std::rc::Rc;
use std::time::Duration;

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

// ── Access method ─────────────────────────────────────────────────────────────

/// The AccessMethod indicates whether the player status is obtained from
/// the HTTP(S) LinkPlay API or via UPnP `GetInfoEx`.
///
/// In the current implementation, it's all one or the other. If we find
/// devices that really need some kind of mix & match, we'll add specific
/// variant to this enumeration (there are hints that some AudioPro devices
/// might but I don't have access to one nor have API captures yet).
///
/// We might complement this with UPnP GENA events in the future.
///
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AccessMethod {
    /// `getPlayerStatusEx` (or whichever of `getPlayerStatusEx`/
    /// `getPlayerStatus`/`getStatusEx` `WiimClient::get_status()` resolves to
    /// — that fallback probing already lives in `api.rs` and is reused
    /// as-is, not duplicated here) plus `getMetaInfo` — today's only source
    /// for all of playback state.
    Http,
    /// UPnP `GetInfoEx` — a single fat action that, on WiiM hardware, covers
    /// everything the two HTTP calls above cover combined.
    UpnpPolled,
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
