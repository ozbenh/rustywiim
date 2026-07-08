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

use super::upnp::GuiBehavior;

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
    /// Which transport controls make sense for the current source — see
    /// `SourceCapabilities`'s own doc comment.
    pub caps:        SourceCapabilities,
    pub quality:     Option<AudioQuality>,
    /// WiiM-app-style codec/quality badge text ("FLAC"/"HIGH"/"mp3"/...),
    /// only ever populated by the UPnP backend (`decode_quality_upnp`) —
    /// there's no equivalent field anywhere in the HTTP API. `None` when
    /// `access` is `Http`, or when the source track's UPnP metadata carries
    /// no quality signal at all (see that function's doc comment for the
    /// confirmed present/empty/absent rule).
    pub codec_label: Option<Rc<str>>,
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
            caps:        SourceCapabilities::default(),
            quality:     None,
            codec_label: None,
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

/// Which transport controls make sense for the *current source* — dynamic,
/// changes with what's playing, unlike `capabilities::DeviceCapabilities`
/// (static, probed once per device). Modeled after `pywiim`'s
/// `SourceCapability` flag set and `SOURCE_CAPABILITIES` table
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SourceCapabilities {
    pub can_next:     bool,
    pub can_previous: bool,
    pub can_shuffle:  bool,
    pub can_repeat:   bool,
    pub can_seek:     bool,
}

impl Default for SourceCapabilities {
    fn default() -> Self {
        Self { can_next: true, can_previous: true, can_shuffle: true, can_repeat: true, can_seek: true }
    }
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

/// Case/whitespace-insensitive form of a raw `vendor`/`TrackSource` string
/// — real captures have shown both `"Linkplay Radio"` (with a space) and
/// presumably other casing variants of the same handful of services, so
/// every vendor-string classification in this module normalizes through
/// this instead of matching the raw string directly.
fn normalize_vendor(vendor: &str) -> String {
    vendor.to_lowercase().chars().filter(|c| !c.is_whitespace()).collect()
}

fn vendor_display(vendor: &str) -> &'static str {
    let v = normalize_vendor(vendor);
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

/// List of station-style services that support skipping forward but
/// not rewinding back into history based on the info from
/// `TrackSource` (UPnP)/`vendor` (HTTP) values (confirmed to be the
/// same identifier space from our captures).
///
/// List ported from the `wiim` Python SDK's `TRACK_SOURCES_CTRL`;
/// only `"Pandora2"` is confirmed via a real capture so far (WiiM Amp,
/// 2026-07-06) — Add/remove entries here as real reports/captures
/// confirm or contradict them, same as any other vendor-string table in
/// this file.
const TRACK_SOURCES_CTRL: &[&str] = &["Pandora2", "SoundMachine", "Soundtrack", "iHeartRadio"];

/// Decode can_prev/can_next for HTTP protocol.
/// The protocol has no equivalent of UPnP's `PlayMedium`, only
/// `mode`/`vendor`, so this is a heuristic — but the default leans
/// permissive, mirroring `decode_transport_caps_upnp`'s own default: a
/// mode only gets disabled here for a *positive* reason to believe it
/// has no track/skip concept at all, not merely because it hasn't been
/// individually confirmed yet. Wrongly enabling a skip button that turns
/// out to be a no-op is a much smaller problem than wrongly disabling one
/// that actually works (e.g. TIDAL Connect, Lyrion, Qobuz, AirPlay,
/// Chromecast, USB — none of these have any real reason to lack transport
/// control, and `PLAY_MEDIUMS_CTRL` itself doesn't list most of them).
///
/// Disabled outright: the physical analog/digital audio-passthrough
/// inputs (Line-In, Optical, RCA, HDMI, Phono) — there's no application
/// layer behind these at all, just a relayed signal, so "skip" is
/// meaningless by construction, not merely unconfirmed (HDMI specifically
/// might turn out to support transport control via HDMI-CEC on some
/// TVs/sources — unconfirmed either way, kept disabled by default until
/// tested). Idle (`mode` 0/-1) — nothing loaded to skip to/from.
/// Everything else with an actual application/service behind it —
/// AirPlay, DLNA, Chromecast, Bluetooth (real sinks commonly support
/// AVRCP transport commands back to the source phone), USB, Spotify,
/// TIDAL Connect, Lyrion, Qobuz — defaults enabled: no positive reason to
/// believe any of them lack transport control.
///
/// `mode` 10/20 (the generic "WiFi"/network-playback bucket —
/// `mode_source()`) is the one genuinely ambiguous case (covers
/// USB-local/internet-radio/built-in-Tidal all at once) and keeps its
/// `vendor` sub-classification, same as `decode_source_name_http` needs:
/// a confirmed radio vendor disables both, a `TRACK_SOURCES_CTRL` vendor
/// disables just previous (mirroring `decode_transport_caps_upnp`), and
/// anything else (including no vendor at all, e.g. local USB/queue
/// playback) defaults to fully enabled.
///
/// `mode` 31 (Spotify) defaults fully enabled — unlike UPnP (see
/// `decode_transport_caps_upnp`), HTTP has no `song:guibehavior`
/// equivalent, so there's no way to tell a free account (confirmed via a
/// real capture to disable `previous`) from a premium one (confirmed to
/// allow it) apart. Defaulting to enabled matches this function's general
/// "err toward enabling" policy rather than assuming the more restrictive
/// tier.
pub fn decode_transport_caps_http(mode: i32, vendor: &str) -> SourceCapabilities {
    let (can_next, can_previous) = decode_next_prev_http(mode, vendor);
    // For now disable shuffle/repeat/seek on physical inputs only
    let physical = is_physical_input_http(mode);
    SourceCapabilities {
        can_next, can_previous,
        can_shuffle: !physical,
        can_repeat:  !physical,
        can_seek:    !physical,
    }
}

fn decode_next_prev_http(mode: i32, vendor: &str) -> (bool, bool) {
    match mode {
        31                          => return (true, true),   // Spotify
        -1 | 0                      => return (false, false), // Idle
        40 | 60 | 43 | 44 | 49 | 54 => return (false, false), // Line-In/Optical/RCA/HDMI/Phono
        _ => {}
    }
    if mode != 10 && mode != 20 {
        return (true, true);
    }
    let normalized = normalize_vendor(vendor);
    if HTTP_RADIO_VENDORS.contains(&normalized.as_str()) {
        return (false, false);
    }
    if TRACK_SOURCES_CTRL.contains(&vendor) {
        return (true, false);
    }
    (true, true)
}

/// `mode` values that are fixed physical audio-passthrough inputs — no
/// application/service behind them at all, so shuffle/repeat/seek are
/// meaningless by construction. Same set `decode_next_prev_http` uses for
/// its own physical-input case.
fn is_physical_input_http(mode: i32) -> bool {
    matches!(mode, 40 | 60 | 43 | 44 | 49 | 54)
}

/// Vendor strings (normalized via `normalize_vendor`) confirmed to mean
/// "internet radio" for the `mode` 10/20 bucket — the same station
/// services `vendor_display()` recognizes for display purposes
/// (`newtunein`/`tunein`, `linkplayradio`, `vtuner`, `radioparadise`),
/// plus `wiimradio` (WiiM's own built-in radio app, confirmed via a real
/// capture — `vendor_display()` itself has no display-name entry for it
/// at all, a separate pre-existing gap not addressed here).
const HTTP_RADIO_VENDORS: &[&str] = &["newtunein", "tunein", "linkplayradio", "vtuner", "radioparadise", "wiimradio"];

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

// ── UPnP (AVTransport GetInfoEx) decoders ─────────────────────────────────────
//
// Wire format -> canonical value, mirroring the `Http` decoders above.
// Confirmed against real device captures
// (`captures/test-sources/WiiM_Ultra_20260706_*.json`) — see
// `device/upnp.rs`'s module doc comment for the investigation that
// produced these.

/// `CurrentTransportState`'s vocabulary is completely disjoint from HTTP's
/// `play`/`pause`/`stop`/`loading` — `PlaybackStatus::Unknown` covers
/// anything neither side recognizes.
pub fn decode_status_upnp(raw: &str) -> PlaybackStatus {
    match raw {
        "PLAYING"          => PlaybackStatus::Playing,
        "PAUSED_PLAYBACK"  => PlaybackStatus::Paused,
        "STOPPED"          => PlaybackStatus::Stopped,
        "TRANSITIONING"    => PlaybackStatus::Loading,
        "NO_MEDIA_PRESENT" => PlaybackStatus::Stopped,
        other              => PlaybackStatus::Unknown(other.to_string()),
    }
}

/// Parses UPnP's `"HH:MM:SS"` `RelTime`/`TrackDuration` wire format.
/// Malformed input (missing device, `NOT_IMPLEMENTED`, etc.) decodes to zero
/// rather than erroring — same "don't display nonsense" spirit as
/// `timing_looks_valid`, just simpler since there's no garbage-vs-valid
/// ambiguity to detect here (unlike HTTP's ms-vs-µs heuristic, UPnP's format
/// is unambiguous when it parses at all).
pub fn decode_hms_duration(s: &str) -> Duration {
    let parts: Vec<&str> = s.split(':').collect();
    let [h, m, sec] = match parts.as_slice() {
        [h, m, sec] => [*h, *m, *sec],
        _ => return Duration::ZERO,
    };
    let (Ok(h), Ok(m), Ok(sec)) = (h.parse::<u64>(), m.parse::<u64>(), sec.parse::<u64>()) else {
        return Duration::ZERO;
    };
    Duration::from_secs(h * 3600 + m * 60 + sec)
}

/// `PlayMedium`/`TrackSource` → display name. Confirmed from real captures
/// (15, covering Tidal Connect, USB/local, two internet-radio backends,
/// third-party DLNA push, Bluetooth, HDMI, Line-In, Optical, Phono, the
/// built-in Tidal app, Spotify, and Chromecast/YouTube):
/// `TIDAL_CONNECT`, `SONGLIST-LOCAL`, `SONGLIST-NETWORK` (the *built-in*
/// Tidal app playing directly, as opposed to `TIDAL_CONNECT`'s cast-from-
/// phone — `TrackSource` is `"Tidal"` for both, so this falls through to
/// the same `vendor_display()`-driven label via the `other` arm, no
/// dedicated match needed), `RADIO-NETWORK`, `THIRD-DLNA` (a third-party
/// DLNA control point pushing to the device — e.g. Music Assistant — as
/// opposed to `SONGLIST-LOCAL`'s own local/USB playback; `TrackSource` is
/// empty for this one, no vendor to look up), `LINE-IN`, `OPTICAL`,
/// `HDMI`, `PHONO` (matches HTTP's `mode_source()` display strings for the
/// same inputs — note `decode_source_name_upnp`'s result never drives the
/// actual input selector regardless: see `state.rs`'s UPnP `process_poll`
/// block, `current_mode` is only ever set from HTTP's `mode` field,
/// unconditionally, independent of `access` — UPnP polling can only affect
/// this display label, never input detection), `BLUETOOTH`, `SPOTIFY`
/// (`TrackSource` is a `spotify:user:...:collection` URI here, not a
/// plain vendor name, so this needs its own entry rather than relying on
/// `vendor_display()`), and `CAST` (Chromecast — resolves via
/// `vendor_display("CAST")` already, needs no dedicated entry). Other
/// `*_CONNECT`/`*-CONNECT` mediums are formatted the same way by pattern
/// rather than hardcoded (unconfirmed — no captures yet for Qobuz/Amazon
/// Connect). `track_source` values reuse `vendor_display()`, the same
/// table HTTP's `vendor` field already maps through (`"Tidal"`/
/// `"newTuneIn"`/`"CAST"` etc. match that table's keys after lowercasing).
pub fn decode_source_name_upnp(play_medium: &str, track_source: &str) -> Option<Rc<str>> {
    if play_medium.is_empty() {
        return None;
    }
    let label = match play_medium {
        "TIDAL_CONNECT"  => "TIDAL Connect".to_string(),
        "SONGLIST-LOCAL" => "USB".to_string(),
        "THIRD-DLNA"     => "DLNA".to_string(),
        "LINE-IN"        => "Line-In".to_string(),
        "OPTICAL"        => "Optical".to_string(),
        "HDMI"           => "HDMI".to_string(),
        "PHONO"          => "Phono".to_string(),
        "BLUETOOTH"      => "Bluetooth".to_string(),
        "SPOTIFY"        => "Spotify".to_string(),
        "RADIO-NETWORK"  => {
            let vn = vendor_display(track_source);
            if vn.is_empty() { "Radio".to_string() } else { vn.to_string() }
        }
        other => {
            let vn = vendor_display(track_source);
            if !vn.is_empty() {
                vn.to_string()
            } else if let Some(prefix) = other.strip_suffix("_CONNECT").or_else(|| other.strip_suffix("-CONNECT")) {
                format!("{prefix} Connect")
            } else {
                other.to_string()
            }
        }
    };
    if label.is_empty() { None } else { Some(Rc::from(label.as_str())) }
}

/// `PlayMedium` values with no next/previous concept at all — an internet
/// radio stream, a third-party DLNA push, or a physical input. Ported
/// verbatim from the `wiim` Python SDK's `PLAY_MEDIUMS_CTRL`
/// (`consts.py`) — real-hardware-validated, not a novel guess.
const PLAY_MEDIUMS_CTRL: &[&str] = &["RADIO-NETWORK", "THIRD-DLNA", "LINE-IN", "OPTICAL", "HDMI", "PHONO"];

/// `gui_behavior` (from `upnp::InfoEx`, `song:guibehavior`'s parsed
/// `next`/`prev` flags — see its doc comment) is trusted directly
/// whenever the DIDL-Lite item actually carries one, no per-service
/// allowlist: every non-Spotify case checked against it so far (Pandora2,
/// WiiM's own radio app) matched the static heuristic below exactly, and
/// it's the *only* signal that can distinguish something a static
/// `play_medium`/`track_source` rule fundamentally can't — e.g. a Spotify
/// free vs. premium account, confirmed via two otherwise-identical real
/// captures (`next`/`prev`/`loop`/`seek`/`shuffle` all `false` on free,
/// all `true` except `queue` on premium). It's still not present on every
/// track, though — confirmed absent even for some genuinely non-skippable
/// sources (TuneIn/BBC Radio) — so the static fallback below still
/// matters for tag-absent cases, not just services that never got a tag
/// at all. Two other candidate device-reported signals were checked and
/// rejected: the standard `GetCurrentTransportActions` UPnP action (only
/// ever used by `pywiim`, which wraps it, from a diagnostics-only
/// snapshot method, never its real state model — and confirmed to report
/// a stale-looking, `Next`-omitting action list for a session where
/// `Next` demonstrably worked), and trusting `guibehavior`'s `next` for
/// Spotify specifically (see the special case below — it's right about
/// `prev`'s tier-dependence but wrong about `next`).
///
/// **`play_medium == "SPOTIFY"` gets one further override on top of
/// `gui_behavior`**: `next` is forced `true` unconditionally, since
/// real-device testing (WiiM Ultra, Spotify Connect, transport buttons
/// themselves — not the WiiM app's own display, and not `gui_behavior`,
/// both of which are sometimes wrong here) confirmed it always works
/// regardless of what `gui_behavior` claims (it reported `next: false`
/// on the free-tier capture even though pressing the button worked).
/// `prev` is left to `gui_behavior` as normal — confirmed to correctly
/// track the free/premium distinction — falling back to `false`
/// (conservative — matches the free-tier default) only in the
/// unobserved case of a Spotify session with no `gui_behavior` at all.
///
/// Base static heuristic (used for everything else, and for Spotify's
/// `prev` when `gui_behavior` is absent) is the `wiim` Python SDK's
/// `async_get_transport_capabilities()`: `play_medium` in
/// `PLAY_MEDIUMS_CTRL` means there's no track to skip to/from at all;
/// failing that, `track_source` in `TRACK_SOURCES_CTRL` means a
/// station-style service that supports skipping forward but not
/// rewinding back into history (confirmed on `"Pandora2"` via a real
/// capture — see `TRACK_SOURCES_CTRL`'s doc comment).
pub fn decode_transport_caps_upnp(
    play_medium: &str, track_source: &str, gui_behavior: Option<GuiBehavior>,
) -> SourceCapabilities {
    let (can_next, can_previous) = decode_next_prev_upnp(play_medium, track_source, gui_behavior);
    // For now disable shuffle/repeat/seek on physical inputs only
    let physical = PHYSICAL_INPUTS_UPNP.contains(&play_medium);
    SourceCapabilities {
        can_next, can_previous,
        can_shuffle: !physical,
        can_repeat:  !physical,
        can_seek:    !physical,
    }
}

fn decode_next_prev_upnp(
    play_medium: &str, track_source: &str, gui_behavior: Option<GuiBehavior>,
) -> (bool, bool) {
    if play_medium == "SPOTIFY" {
        return (true, gui_behavior.map_or(false, |g| g.prev));
    }
    if let Some(g) = gui_behavior {
        return (g.next, g.prev);
    }
    if PLAY_MEDIUMS_CTRL.contains(&play_medium) {
        return (false, false);
    }
    if TRACK_SOURCES_CTRL.contains(&track_source) {
        return (true, false);
    }
    (true, true)
}

/// `PlayMedium` values that are fixed physical audio-passthrough inputs —
/// the subset of `PLAY_MEDIUMS_CTRL` that's an actual physical jack, not
/// e.g. `RADIO-NETWORK`/`THIRD-DLNA` (which have no shuffle/repeat/seek
/// concept for a different reason — a live/externally-pushed stream, not
/// a lack of any application layer at all).
const PHYSICAL_INPUTS_UPNP: &[&str] = &["LINE-IN", "OPTICAL", "HDMI", "PHONO"];

/// Translates a non-empty `song:actualQuality` into the WiiM app's own
/// display vocabulary. Only two mappings are confirmed from real captures —
/// `HI_RES_LOSSLESS` → "FLAC", `LOSSLESS` → "HIGH" (both are real TIDAL
/// `audioQuality` enum values; the app's choice to relabel rather than show
/// them verbatim is an observed fact, not one we understand the reasoning
/// for). Anything else (TIDAL's own `HIGH`/`LOW` lossy tiers, or any other
/// service's vocabulary — unconfirmed, no captures yet) is shown verbatim
/// rather than guessed at.
fn translate_actual_quality(q: &str) -> Rc<str> {
    match q {
        "HI_RES_LOSSLESS" => Rc::from("FLAC"),
        "LOSSLESS"        => Rc::from("HIGH"),
        other             => Rc::from(other),
    }
}

/// Falls back to a literal container-format name parsed from `res
/// protocolInfo` — only called by `decode_quality_upnp` for
/// `play_medium == "SONGLIST-LOCAL"`, since that's the only source type
/// confirmed to report a real (not placeholder) value here. Prefers the
/// DLNA profile name (`DLNA.ORG_PN=MP3` → `"mp3"`); if that attribute is
/// absent, falls back to the protocolInfo's mime-type subtype
/// (`audio/mpeg` → `"mpeg"`).
fn codec_label_from_protocol_info(pi: &str) -> Option<Rc<str>> {
    if let Some(pos) = pi.find("DLNA.ORG_PN=") {
        let rest = &pi[pos + "DLNA.ORG_PN=".len()..];
        let end = rest.find(';').unwrap_or(rest.len());
        let pn = &rest[..end];
        if !pn.is_empty() {
            return Some(Rc::from(pn.to_lowercase().as_str()));
        }
    }
    let mime = pi.split(':').nth(2)?;
    let subtype = mime.split('/').nth(1)?;
    if subtype.is_empty() || subtype == "*" {
        return None;
    }
    Some(Rc::from(subtype.to_lowercase().as_str()))
}

/// Implements the confirmed codec-badge rule:
/// - `actual_quality` present and non-empty → `codec_label` is the
///   translated display string (see `translate_actual_quality`).
/// - Present but empty (`Some("")`) → `codec_label` falls back to a literal
///   format name parsed from `protocol_info`, **but only for
///   `play_medium == "SONGLIST-LOCAL"`** (local/USB playback) — see below.
/// - Absent entirely (`None`) → `codec_label` is `None` (no badge at all).
///
/// **`res protocolInfo` is a static placeholder for most sources, not a
/// real per-track signal** — confirmed by comparing 15 real captures
/// covering very different source types (Bluetooth/HDMI/Line-In/Optical/
/// Phono/Spotify/Chromecast/two different internet-radio backends): all of
/// them report the exact same `"http-get:*:audio/mpeg:DLNA.ORG_PN=MP3;
/// DLNA.ORG_OP=01;"` string regardless of what's actually playing (Line-In
/// and Phono obviously aren't literally MP3 files). It only varies for
/// genuine local-file-serving cases — `SONGLIST-LOCAL` (this device's own
/// USB/local playback) and `THIRD-DLNA` (a third-party DLNA push, which
/// reports a real, matching `audio/flac` when that's genuinely what's
/// playing) — but `THIRD-DLNA`'s one confirmed capture has
/// `actual_quality` *absent*, not present-empty, so it never reaches this
/// fallback anyway. Restricting the fallback to `SONGLIST-LOCAL`
/// specifically (an allowlist, not a `RADIO-NETWORK` denylist) is the
/// conservative choice: two internet-radio captures (`WiimRadio`/
/// `BBCRadio`) were found with `actual_quality` present-but-empty *and*
/// the same generic MP3 placeholder — without this restriction, both
/// would show a bogus "mp3" badge for a live stream neither the badge
/// concept nor the placeholder value has anything meaningful to say about.
///
/// `AudioQuality` itself is built the same way `decode_quality_http` does,
/// from `bitrate`/`rate_hz`/`format_s` (bit depth).
pub fn decode_quality_upnp(
    actual_quality: Option<&str>,
    bitrate: &str,
    format_s: &str,
    rate_hz: &str,
    protocol_info: Option<&str>,
    play_medium: &str,
) -> (Option<AudioQuality>, Option<Rc<str>>) {
    let quality = decode_quality_http(bitrate, rate_hz, format_s);
    let codec_label = match actual_quality {
        Some(q) if !q.is_empty() => Some(translate_actual_quality(q)),
        Some(_) if play_medium == "SONGLIST-LOCAL" => {
            protocol_info.and_then(codec_label_from_protocol_info)
        }
        Some(_) | None => None,
    };
    (quality, codec_label)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn caps(can_next: bool, can_previous: bool) -> SourceCapabilities {
        SourceCapabilities { can_next, can_previous, ..Default::default() }
    }

    /// Same as `caps()`, for a fixed physical input — shuffle/repeat/seek
    /// are always disabled there, unlike every other case `caps()` covers.
    fn caps_physical(can_next: bool, can_previous: bool) -> SourceCapabilities {
        SourceCapabilities { can_next, can_previous, can_shuffle: false, can_repeat: false, can_seek: false }
    }

    #[test]
    fn quality_badge_rule_hi_res_lossless_becomes_flac() {
        let (quality, label) = decode_quality_upnp(
            Some("HI_RES_LOSSLESS"), "1571", "24", "48000", Some("http-get:*:*:*"), "TIDAL_CONNECT",
        );
        assert_eq!(label.as_deref(), Some("FLAC"));
        let q = quality.unwrap();
        assert_eq!(q.bit_rate_kbps, Some(1571.0));
        assert_eq!(q.bit_depth, Some(24));
        assert_eq!(q.sample_rate_khz, Some(48.0));
    }

    #[test]
    fn quality_badge_rule_lossless_becomes_high() {
        let (_, label) = decode_quality_upnp(
            Some("LOSSLESS"), "546", "16", "44100", Some("http-get:*:*:*"), "TIDAL_CONNECT",
        );
        assert_eq!(label.as_deref(), Some("HIGH"));
    }

    #[test]
    fn quality_badge_rule_present_empty_falls_back_to_protocol_info_for_songlist_local() {
        let (_, label) = decode_quality_upnp(
            Some(""), "320", "16", "48000",
            Some("http-get:*:audio/mpeg:DLNA.ORG_PN=MP3;DLNA.ORG_OP=01;"),
            "SONGLIST-LOCAL",
        );
        assert_eq!(label.as_deref(), Some("mp3"));
    }

    #[test]
    fn quality_badge_rule_present_empty_but_not_songlist_local_means_no_badge() {
        // Real regression found from `WiimRadio`/`BBCRadio` captures: both
        // report `actual_quality` present-but-empty *and* the exact same
        // generic MP3 `protocolInfo` placeholder every other non-local
        // source reports too (Bluetooth/HDMI/Line-In/Optical/Phono/
        // Spotify/Chromecast all show it verbatim) — not a real signal for
        // a live radio stream. Only `SONGLIST-LOCAL` is confirmed to make
        // this fallback meaningful.
        let (_, label) = decode_quality_upnp(
            Some(""), "0", "16", "44100",
            Some("http-get:*:audio/mpeg:DLNA.ORG_PN=MP3;DLNA.ORG_OP=01;"),
            "RADIO-NETWORK",
        );
        assert_eq!(label, None);
    }

    #[test]
    fn quality_badge_rule_absent_tag_means_no_badge() {
        // Byte-identical protocol_info to the present-empty case above —
        // the absence of the tag itself is what suppresses the badge, not
        // the underlying format.
        let (_, label) = decode_quality_upnp(
            None, "0", "32", "44100",
            Some("http-get:*:audio/mpeg:DLNA.ORG_PN=MP3;DLNA.ORG_OP=01;"),
            "RADIO-NETWORK",
        );
        assert_eq!(label, None);
    }

    #[test]
    fn source_name_tidal_connect() {
        assert_eq!(decode_source_name_upnp("TIDAL_CONNECT", "Tidal").as_deref(), Some("TIDAL Connect"));
    }

    #[test]
    fn source_name_radio_network_uses_vendor_table() {
        assert_eq!(decode_source_name_upnp("RADIO-NETWORK", "newTuneIn").as_deref(), Some("TuneIn"));
    }

    #[test]
    fn source_name_songlist_local() {
        assert_eq!(decode_source_name_upnp("SONGLIST-LOCAL", "UPnPServer").as_deref(), Some("USB"));
    }

    #[test]
    fn source_name_bluetooth() {
        assert_eq!(decode_source_name_upnp("BLUETOOTH", "").as_deref(), Some("Bluetooth"));
    }

    #[test]
    fn source_name_spotify() {
        // TrackSource is a spotify: URI here, not a plain vendor name —
        // must resolve via the dedicated PlayMedium match, not vendor_display.
        assert_eq!(
            decode_source_name_upnp("SPOTIFY", "spotify:user:1516emh5k43jthv55arsid1k6:collection").as_deref(),
            Some("Spotify"),
        );
    }

    #[test]
    fn source_name_chromecast_via_vendor_display() {
        assert_eq!(decode_source_name_upnp("CAST", "CAST").as_deref(), Some("Chromecast"));
    }

    #[test]
    fn source_name_songlist_network_builtin_tidal_via_vendor_display() {
        assert_eq!(decode_source_name_upnp("SONGLIST-NETWORK", "Tidal").as_deref(), Some("TIDAL"));
    }

    #[test]
    fn source_name_third_dlna() {
        assert_eq!(decode_source_name_upnp("THIRD-DLNA", "").as_deref(), Some("DLNA"));
    }

    #[test]
    fn source_name_analog_digital_inputs() {
        assert_eq!(decode_source_name_upnp("LINE-IN", "").as_deref(), Some("Line-In"));
        assert_eq!(decode_source_name_upnp("OPTICAL", "").as_deref(), Some("Optical"));
        assert_eq!(decode_source_name_upnp("HDMI", "").as_deref(), Some("HDMI"));
        assert_eq!(decode_source_name_upnp("PHONO", "").as_deref(), Some("Phono"));
    }

    #[test]
    fn hms_duration_parses() {
        assert_eq!(decode_hms_duration("00:04:17"), Duration::from_secs(4 * 60 + 17));
        assert_eq!(decode_hms_duration("NOT_IMPLEMENTED"), Duration::ZERO);
    }

    #[test]
    fn status_upnp_vocabulary() {
        assert_eq!(decode_status_upnp("PLAYING"), PlaybackStatus::Playing);
        assert_eq!(decode_status_upnp("PAUSED_PLAYBACK"), PlaybackStatus::Paused);
        assert_eq!(decode_status_upnp("weird"), PlaybackStatus::Unknown("weird".to_string()));
    }

    #[test]
    fn transport_caps_upnp_no_skip_mediums_disable_both() {
        assert_eq!(decode_transport_caps_upnp("RADIO-NETWORK", "newTuneIn", None), caps(false, false));
        assert_eq!(decode_transport_caps_upnp("LINE-IN", "", None), caps_physical(false, false));
        assert_eq!(decode_transport_caps_upnp("HDMI", "", None), caps_physical(false, false));
    }

    #[test]
    fn transport_caps_upnp_station_services_disable_previous_only() {
        assert_eq!(decode_transport_caps_upnp("STATION-NETWORK", "Pandora2", None), caps(true, false));
    }

    #[test]
    fn transport_caps_upnp_spotify_falls_back_to_previous_false_without_guibehavior() {
        // Real-device-tested (transport buttons, not just guibehavior/app
        // display, both of which wrongly claim neither works) — `next` is
        // always forced true for Spotify; `prev` without a `gui_behavior`
        // reading defaults to the conservative (free-tier-like) `false`.
        assert_eq!(decode_transport_caps_upnp("SPOTIFY", "spotify:playlist:37i9dQZF1EIZAuCHB2O9dH", None), caps(true, false));
    }

    #[test]
    fn transport_caps_upnp_spotify_next_forced_true_even_if_guibehavior_disagrees() {
        // Confirmed via a real free-tier capture: guibehavior claimed
        // next:false, but pressing the button actually skipped forward.
        let gb = GuiBehavior { next: false, prev: false };
        assert_eq!(decode_transport_caps_upnp("SPOTIFY", "spotify:playlist:x", Some(gb)), caps(true, false));
    }

    #[test]
    fn transport_caps_upnp_spotify_previous_follows_guibehavior_premium_vs_free() {
        // Confirmed via two real captures on the same playlist mechanism,
        // differing only by account tier.
        let free = GuiBehavior { next: false, prev: false };
        assert_eq!(decode_transport_caps_upnp("SPOTIFY", "spotify:playlist:free", Some(free)), caps(true, false));
        let premium = GuiBehavior { next: true, prev: true };
        assert_eq!(decode_transport_caps_upnp("SPOTIFY", "spotify:playlist:premium", Some(premium)), caps(true, true));
    }

    #[test]
    fn transport_caps_upnp_guibehavior_trusted_over_static_heuristic_when_present() {
        // A source the static heuristic would otherwise default-enable,
        // but guibehavior says otherwise for this specific track — trust it.
        let gb = GuiBehavior { next: false, prev: true };
        assert_eq!(decode_transport_caps_upnp("TIDAL_CONNECT", "Tidal", Some(gb)), caps(false, true));
    }

    #[test]
    fn transport_caps_upnp_unknown_medium_defaults_permissive() {
        assert_eq!(decode_transport_caps_upnp("TIDAL_CONNECT", "Tidal", None), caps(true, true));
        assert_eq!(decode_transport_caps_upnp("SONGLIST-LOCAL", "UPnPServer", None), caps(true, true));
    }

    #[test]
    fn transport_caps_http_physical_inputs_and_idle_disable_both() {
        assert_eq!(decode_transport_caps_http(40, ""), caps_physical(false, false)); // Line-In
        assert_eq!(decode_transport_caps_http(49, ""), caps_physical(false, false)); // HDMI
        assert_eq!(decode_transport_caps_http(43, ""), caps_physical(false, false)); // Optical
        assert_eq!(decode_transport_caps_http(44, ""), caps_physical(false, false)); // RCA
        assert_eq!(decode_transport_caps_http(54, ""), caps_physical(false, false)); // Phono
        // Idle isn't a "fixed physical input" — shuffle/repeat/seek aren't
        // narrowed for it (yet), only next/previous.
        assert_eq!(decode_transport_caps_http(0,  ""), caps(false, false)); // Idle
        assert_eq!(decode_transport_caps_http(-1, ""), caps(false, false)); // Idle (sentinel)
    }

    #[test]
    fn transport_caps_shuffle_repeat_seek_disabled_only_for_physical_inputs() {
        let non_physical = decode_transport_caps_http(11, ""); // USB
        assert!(non_physical.can_shuffle && non_physical.can_repeat && non_physical.can_seek);
        let physical = decode_transport_caps_http(54, ""); // Phono
        assert!(!physical.can_shuffle && !physical.can_repeat && !physical.can_seek);

        let non_physical = decode_transport_caps_upnp("TIDAL_CONNECT", "Tidal", None);
        assert!(non_physical.can_shuffle && non_physical.can_repeat && non_physical.can_seek);
        let physical = decode_transport_caps_upnp("OPTICAL", "", None);
        assert!(!physical.can_shuffle && !physical.can_repeat && !physical.can_seek);
    }

    #[test]
    fn transport_caps_http_unconfirmed_network_services_default_enabled() {
        // No positive reason to believe these lack transport control —
        // err toward enabling rather than disabling until proven otherwise.
        assert_eq!(decode_transport_caps_http(11, ""), caps(true, true)); // USB
        assert_eq!(decode_transport_caps_http(1,  ""), caps(true, true)); // AirPlay
        assert_eq!(decode_transport_caps_http(2,  ""), caps(true, true)); // DLNA
        assert_eq!(decode_transport_caps_http(5,  ""), caps(true, true)); // Chromecast
        assert_eq!(decode_transport_caps_http(32, ""), caps(true, true)); // TIDAL Connect
        assert_eq!(decode_transport_caps_http(34, ""), caps(true, true)); // Lyrion
        assert_eq!(decode_transport_caps_http(36, ""), caps(true, true)); // Qobuz
        assert_eq!(decode_transport_caps_http(41, ""), caps(true, true)); // Bluetooth
    }

    #[test]
    fn transport_caps_http_wifi_bucket_uses_vendor() {
        assert_eq!(decode_transport_caps_http(10, "newTuneIn"), caps(false, false));
        assert_eq!(decode_transport_caps_http(10, "WiiMRadio"), caps(false, false));
        assert_eq!(decode_transport_caps_http(10, "Linkplay Radio"), caps(false, false));
        assert_eq!(decode_transport_caps_http(10, "vTuner"), caps(false, false));
        assert_eq!(decode_transport_caps_http(10, "RadioParadise"), caps(false, false));
        assert_eq!(decode_transport_caps_http(10, "Pandora2"), caps(true, false));
        assert_eq!(decode_transport_caps_http(10, "UDiskLocal"), caps(true, true));
        assert_eq!(decode_transport_caps_http(10, ""), caps(true, true));
    }

    #[test]
    fn transport_caps_http_spotify_defaults_fully_enabled_tier_unknown() {
        // HTTP has no guibehavior-equivalent signal to distinguish free
        // from premium accounts, unlike UPnP — default permissive.
        assert_eq!(decode_transport_caps_http(31, ""), caps(true, true));
    }
}
