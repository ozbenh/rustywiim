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
}
