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

use std::collections::HashSet;
use std::rc::Rc;
use std::sync::{LazyLock, Mutex};
use std::time::Duration;

use super::capabilities::{self, DeviceId};
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
    /// Whether the current input is a fixed physical passthrough (line-in/
    /// optical/coax/HDMI/phono/RCA) rather than an app/streaming source —
    /// see `is_physical_input_mode()`'s doc comment. Set from
    /// `apply_mode_change()`, the one place `current_mode` changes. `ui/`
    /// reads this instead of calling `is_physical_input_mode()` on a raw
    /// mode number itself — for a physical input, `source_name` already
    /// holds the input's own display name ("Optical In", "Bluetooth", ...)
    /// via `decode_source_name_http`/`decode_source_name_upnp`'s
    /// physical-input arms, since there's no separate "service" for it;
    /// this flag is what lets a view show that name as the title instead
    /// of as a service badge, since there's no real song info either way.
    pub is_physical_input: bool,
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
    /// Bluetooth A2DP sink connection state — only ever populated while
    /// Bluetooth is the active input (`state.rs`'s slow poll only fetches
    /// `getbtstatus` in that case); reset to `false`/`None` as soon as a
    /// different input becomes active, so switching back to Bluetooth
    /// later doesn't show a stale value from a previous session until the
    /// next slow-poll cycle. `bt_device_name` is only meaningful when
    /// `bt_connected` is `true`.
    pub bt_connected:    bool,
    pub bt_device_name:  Option<Rc<str>>,
    pub bt_pairing:      bool,
}

impl Default for PlaybackState {
    fn default() -> Self {
        Self {
            status:      PlaybackStatus::Stopped,
            source_name: None,
            is_physical_input: false,
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
            bt_connected:   false,
            bt_device_name: None,
            bt_pairing:     false,
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
/// `SourceCapability` flag set and `SOURCE_CAPABILITIES` table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SourceCapabilities {
    pub can_next:     bool,
    pub can_previous: bool,
    pub can_shuffle:  bool,
    pub can_repeat:   bool,
    pub can_seek:     bool,
    pub can_playpause: bool,
}

impl Default for SourceCapabilities {
    fn default() -> Self {
        Self {
            can_next: true, can_previous: true, can_shuffle: true, can_repeat: true, can_seek: true,
            can_playpause: true,
        }
    }
}

/// Coarse per-source shuffle/repeat/seek tier — modeled directly on
/// `pywiim`'s `SourceCapability` bundles (`FULL_CONTROL`/`TRACK_CONTROL`/
/// `NONE` in `player/source_capabilities.py`'s `SOURCE_CAPABILITIES`
/// table), since that project's reasoning generalizes cleanly: `TrackOnly`
/// is a source where the device is just forwarding transport commands
/// to/from an external app (AirPlay, Bluetooth, DLNA, Chromecast, a
/// multiroom follower routing to its master) — next/previous/seek still
/// make sense as forwarded commands, but shuffle/repeat don't, since the
/// device has no queue of its own to reorder. `None` is no queue and no
/// forwarding target either (radio, physical inputs). `Full` is
/// everything else — the device (or the connected streaming service)
/// genuinely owns a queue.
enum LoopTier {
    Full,
    TrackOnly,
    None,
}

impl LoopTier {
    fn shuffle_repeat_seek(self) -> (bool, bool, bool) {
        match self {
            Self::Full      => (true, true, true),
            Self::TrackOnly => (false, false, true),
            Self::None      => (false, false, false),
        }
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

fn mode_source(mode: i32, device_id: Option<DeviceId>) -> &'static str {
    // Physical inputs route through `input_display_name()` so a
    // per-device override (`DeviceProfile::input_labels` — e.g. a device's
    // own "AUX In" labeling instead of the generic "Line-In") shows up
    // consistently in status text and the source dropdown alike, rather
    // than the two drifting out of sync (as they did before this existed).
    let physical_id = match mode {
        40 | 60 => Some("line-in"),
        44      => Some("RCA"),
        43      => Some("optical"),
        49      => Some("HDMI"),
        54      => Some("phono"),
        41      => Some("bluetooth"),
        _       => None,
    };
    if let Some(id) = physical_id {
        return capabilities::input_display_name(device_id, id);
    }
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
        "amazon" | "amazonmusic" | "prime"   => "Amazon Music",
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
/// `device_id` — see `mode_source()`'s doc comment; `None` when no device
/// context is available yet (falls straight to the generic table).
pub fn decode_source_name_http(mode: i32, vendor: &str, device_id: Option<DeviceId>) -> Option<Rc<str>> {
    let source_name = match mode {
        10 | 20 | 0 | 5 => {
            let vn = vendor_display(vendor);
            if !vn.is_empty() { vn } else { mode_source(mode, device_id) }
        }
        _ => mode_source(mode, device_id),
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
    let (can_shuffle, can_repeat, can_seek) = loop_tier_http(mode, vendor).shuffle_repeat_seek();
    SourceCapabilities { can_next, can_previous, can_shuffle, can_repeat, can_seek, can_playpause: true }
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

/// `mode`/`play_type` values that are fixed physical audio-passthrough
/// inputs — no application/service behind them at all, so shuffle/repeat/
/// seek are meaningless by construction, and so is a "service name": there's
/// no song info, only whatever's physically plugged in. `state.rs` uses
/// this to drive `PlaybackState::is_physical_input`, the canonical signal
/// `ui/` reads instead of a raw mode number (see that field's doc comment).
pub(crate) fn is_physical_input_mode(mode: i32) -> bool {
    matches!(mode, 40 | 60 | 43 | 44 | 49 | 54)
}

/// Fabricates a substitute `mode`/`play_type` value from a `PlayMedium`-
/// shaped string — both `GetInfoEx`'s own `PlayMedium` (devices whose
/// `GetInfoEx` never carries `<PlayType>` at all — confirmed, 2026-07-13,
/// on an Audio Pro Addon C5, see `capabilities.rs`'s
/// `FAMILY_AUDIO_PRO_ADDON_C5` doc comment) and GENA's `AVTransport`
/// NOTIFY's `PlaybackStorageMedium` share this vocabulary (confirmed live:
/// `"PHONO"`/`"TIDAL_CONNECT"`/`"SONGLIST-NETWORK"`/`"QOBUZ_CONNECT"` all
/// match values already in this table or `decode_source_name_upnp()`'s own
/// table below).
/// `None` for anything not in the confirmed list — the caller decides what
/// to do with an unrecognized value (`mode_from_play_medium_fallback()`
/// below still wants *some* number regardless; `DeviceState`'s GENA NOTIFY
/// handling wants to know whether to trust it enough to skip triggering a
/// confirming poll).
///
/// The exact numeric value matters for `mode_to_input_source()`'s bucket
/// (line-in/optical/HDMI/phono/bluetooth/"everything else is wifi"),
/// `is_physical_input_mode()`'s bool, and `decode_transport_caps_http()`/
/// `decode_transport_caps_upnp()`'s own per-mode capability table — *not*
/// for distinguishing between different streaming services in general
/// (Spotify vs. TuneIn vs. Tidal), which `process_poll_upnp`'s separate
/// `play_medium`/`track_source` diffing already handles on its own — so
/// every streaming source *without* its own already-established HTTP
/// `mode` number shares the one generic placeholder (`10`). But a source
/// that *does* have an existing dedicated HTTP `mode` (`SPOTIFY`→`31`,
/// `TIDAL_CONNECT`→`32`, `QOBUZ_CONNECT`→`36`) must return that same
/// number, not `10` — a real bug (2026-07-20, see `TIDAL_CONNECT`'s own
/// comment below) came from `TIDAL_CONNECT` returning `10` here while
/// `inner.current_mode` was already `32` from HTTP/UPnP polling: this
/// function disagreeing with the poll's own numbering for a source that
/// hadn't actually changed made `DeviceState` see a false mode change on
/// every GENA `PlaybackStorageMedium` NOTIFY, blanking and immediately
/// re-populating the whole playback baseline (visible as `FlipCover`
/// flipping away and straight back to the same artwork). When adding a
/// new entry here, check `decode_source_name_http()`'s mode table first.
pub fn mode_from_play_medium(play_medium: &str) -> Option<i32> {
    match play_medium {
        "" | "NONE" | "UNKNOWN" => Some(0), // idle — matches `has_playable_content()`'s own `0` sentinel
        "BLUETOOTH"  => Some(41),
        "LINE-IN"    => Some(40),
        "RCA"        => Some(44),
        "OPTICAL"    => Some(43),
        "HDMI"       => Some(49),
        "PHONO"      => Some(54),
        "SPOTIFY"    => Some(31), // confirmed live, 2026-07-13 — matches HTTP's own `31 => "Spotify"`
        // Confirmed live, 2026-07-19 (`captures/test-sources/
        // WiiM_Ultra_20260719_111807.QobuzConnect.json`): `PlayType` and
        // this same session's `getPlayerStatusEx` `mode` both `36`, the
        // same value already in `decode_source_name_http`'s table (`36 =>
        // "Qobuz"`) and consumed by `decode_transport_caps_http()` — same
        // reasoning as `SPOTIFY` above for using its own specific value
        // instead of the generic `10` placeholder.
        "QOBUZ_CONNECT" => Some(36),
        // Re-confirmed live, 2026-07-20 (WiiM Ultra, real Tidal Connect
        // session): `GetInfoEx`'s own `PlayType` was `32` throughout,
        // matching `decode_source_name_http`'s `32 => "TIDAL Connect"` and
        // `decode_transport_caps_http`/`decode_transport_caps_upnp`'s own
        // specific handling of that value — same reasoning as `SPOTIFY`/
        // `QOBUZ_CONNECT` above. Previously shared the generic `10`
        // bucket with `SONGLIST-NETWORK`; that mismatch (this function
        // returning `10` for a GENA `PlaybackStorageMedium` NOTIFY while
        // `inner.current_mode` was already `32` from HTTP/UPnP polling)
        // caused a real, visible bug: `DeviceState` saw that as a genuine
        // mode change, ran `blank_playback_baseline()` (clearing title/
        // artist/album/art/caps), then immediately saw the *next* poll
        // report `32` again and repopulated everything — visible as
        // `FlipCover` flipping away to the fallback icon and straight back
        // to the exact same artwork, for no actual source change at all.
        "TIDAL_CONNECT" => Some(32),
        "SONGLIST-NETWORK" => Some(10),
        // Confirmed via three real captures (`WiiM_Amp_20260706_135152`,
        // `WiiM_Amp_20260707_173909`, `WiiM_Amp_Ultra_20260707_173928` —
        // all Pandora sessions): `GetInfoEx`'s own `PlayType` is `10`
        // every time `PlayMedium` is `STATION-NETWORK`, the same generic
        // bucket `SONGLIST-NETWORK` already uses — this was previously
        // missing from this table entirely (fell through to `None`,
        // "unrecognized"), unlike `SONGLIST-NETWORK`'s own on-demand
        // counterpart.
        "STATION-NETWORK" => Some(10),
        _ => None, // unrecognized — not a confirmed value, let the caller decide
    }
}

/// `mode_from_play_medium()`, but with the generic "some other streaming
/// source" bucket (`10`) substituted for anything unrecognized — for
/// callers that need *a* value no matter what, since there's no other
/// source of a mode number to fall back to (`fetch_upnp_fast_poll()`'s
/// `PlayType`-missing case).
pub fn mode_from_play_medium_fallback(play_medium: &str) -> i32 {
    mode_from_play_medium(play_medium).unwrap_or(10)
}

/// `pywiim`'s `SOURCE_CAPABILITIES` table, translated into HTTP's
/// `mode`/`vendor` vocabulary — see `LoopTier`'s doc comment for the
/// reasoning. `mode` 1/2/5/41/99 (AirPlay/DLNA/Chromecast/Bluetooth/
/// Follower) are `pywiim`'s `"airplay"/"dlna"/"cast"/"bluetooth"/
/// "multiroom"` entries, all `TRACK_CONTROL`. Spotify (`mode` 31)
/// deliberately follows `pywiim`'s static `"spotify": FULL_CONTROL`
/// as-is for now, same as everywhere else here — **known to be
/// questionable**: the real free-tier `guibehavior` capture already on
/// hand shows `loop`/`shuffle` both disabled there, contradicting this.
/// Not corrected yet since HTTP has no `guibehavior` equivalent to know
/// which tier a given session is in, and revisiting needs a free-account
/// retest that hasn't happened yet.
fn loop_tier_http(mode: i32, vendor: &str) -> LoopTier {
    if is_physical_input_mode(mode) {
        return LoopTier::None;
    }
    if matches!(mode, 1 | 2 | 5 | 41 | 99) {
        return LoopTier::TrackOnly;
    }
    if mode == 10 || mode == 20 {
        let normalized = normalize_vendor(vendor);
        if HTTP_RADIO_VENDORS.contains(&normalized.as_str()) {
            return LoopTier::None;
        }
        // `pywiim` singles this one out as pure radio (`SourceCapability::NONE`)
        // even though the `wiim` SDK's `TRACK_SOURCES_CTRL` (used for
        // next/previous above) treats it as skip-forward-capable — the two
        // heuristics disagree here and this function follows `pywiim`,
        // since shuffle/repeat/seek is its own independent classification.
        if normalized == "iheartradio" {
            return LoopTier::None;
        }
        return LoopTier::Full;
    }
    LoopTier::Full
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

/// The exact inverse of `decode_loop_mode_http()` — the wire encoding for
/// a (shuffle, repeat) pair, used by both write paths that accept this
/// same 0-5 integer: HTTP `setPlayerCmd:loopmode:N` and UPnP `PlayQueue`'s
/// `SetQueueLoopMode`'s `<LoopMode>` argument (confirmed identical
/// convention on both — this is the actual XML the WiiM phone app sends
/// for `SetQueueLoopMode`, and the numeric table matches pywiim's/
/// wiimplay's Arylic-scheme tables exactly, cross-checked against this
/// app's own `decode_loop_mode_http` table above). Lives here, not in
/// `ui/`, so `ui/` only ever passes canonical `(bool, RepeatMode)` values
/// to `DeviceState::do_set_loop_mode()` — never a raw wire number.
pub fn encode_loop_mode(shuffle: bool, repeat: RepeatMode) -> i32 {
    match (shuffle, repeat) {
        (false, RepeatMode::Off) => 4,
        (false, RepeatMode::All) => 0,
        (false, RepeatMode::One) => 1,
        (true,  RepeatMode::Off) => 3,
        (true,  RepeatMode::All) => 2,
        (true,  RepeatMode::One) => 5,
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
    // Occasionally reported as 32 — no WiiM hardware actually does more
    // than 24-bit, and the WiiM app itself caps its own display at 24;
    // clamp rather than show a bogus, unachievable value.
    let bit_depth_val = if !bd.is_empty() && bd != "0" {
        bd.parse::<u32>().ok().map(|d| d.min(24))
    } else {
        None
    };
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
/// same inputs, both routed through `input_display_name()` for any
/// per-device label override), `BLUETOOTH`, `SPOTIFY`
/// (`TrackSource` is a `spotify:user:...:collection` URI here, not a
/// plain vendor name, so this needs its own entry rather than relying on
/// `vendor_display()`), and `CAST` (Chromecast — resolves via
/// `vendor_display("CAST")` already, needs no dedicated entry). Other
/// `*_CONNECT`/`*-CONNECT` mediums are formatted the same way by pattern
/// rather than hardcoded (unconfirmed — no captures yet for Qobuz/Amazon
/// Connect). `track_source` values reuse `vendor_display()`, the same
/// table HTTP's `vendor` field already maps through (`"Tidal"`/
/// `"newTuneIn"`/`"CAST"` etc. match that table's keys after lowercasing).
/// `device_id` — see `mode_source()`'s doc comment; the physical-input
/// cases below (`LINE-IN`/`RCA`/`OPTICAL`/`HDMI`/`PHONO`/`BLUETOOTH`) route
/// through the same `input_display_name()` table `mode_source()` does, so
/// a per-device override applies identically whether this device is
/// polled over HTTP or UPnP.
pub fn decode_source_name_upnp(play_medium: &str, track_source: &str, device_id: Option<DeviceId>) -> Option<Rc<str>> {
    if play_medium.is_empty() {
        return None;
    }
    let label = match play_medium {
        "TIDAL_CONNECT"  => "TIDAL Connect".to_string(),
        "SONGLIST-LOCAL" => "USB".to_string(),
        "THIRD-DLNA"     => "DLNA".to_string(),
        "LINE-IN"        => capabilities::input_display_name(device_id, "line-in").to_string(),
        // Second physical line-level input (RCA jacks on the back, vs.
        // "LINE-IN"'s front 3.5mm AUX jack) — confirmed live, 2026-07-13,
        // Audio Pro Addon C5, both the `PlayMedium` string and (via
        // `getPlayerStatusEx`) the numeric `mode`: 44, matching HTTP's
        // already-existing `44 => "RCA"` case in `decode_source_name_http`
        // above — same mode, same label, not the separate `47`/
        // "line-in-2" id (`capabilities.rs`'s `mode_to_input_source()`)
        // that name might suggest.
        "RCA"            => capabilities::input_display_name(device_id, "RCA").to_string(),
        "OPTICAL"        => capabilities::input_display_name(device_id, "optical").to_string(),
        "HDMI"           => capabilities::input_display_name(device_id, "HDMI").to_string(),
        "PHONO"          => capabilities::input_display_name(device_id, "phono").to_string(),
        "BLUETOOTH"      => capabilities::input_display_name(device_id, "bluetooth").to_string(),
        "SPOTIFY"        => "Spotify".to_string(),
        "RADIO-NETWORK"  => {
            let vn = vendor_display(track_source);
            if vn.is_empty() { "Radio".to_string() } else { vn.to_string() }
        }
        // A network-pushed queue (e.g. DLNA/cast of local files from a
        // phone) whose `TrackSource` doesn't identify a recognized
        // service — confirmed live, 2026-07-13, playing local media on an
        // Audio Pro Addon C5: showed the raw wire string verbatim instead
        // of a real label. `"SONGLIST-LOCAL"` (this device's own USB/local
        // case) already has the same "generic fallback, recognized vendor
        // still wins" shape — mirrored here rather than invented fresh.
        "SONGLIST-NETWORK" => {
            let vn = vendor_display(track_source);
            if vn.is_empty() { "Streaming".to_string() } else { vn.to_string() }
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
const PLAY_MEDIUMS_CTRL: &[&str] = &["RADIO-NETWORK", "THIRD-DLNA", "LINE-IN", "RCA", "OPTICAL", "HDMI", "PHONO"];

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
/// `prev` is left to `gui_behavior` as normal when present — confirmed to
/// correctly track the free/premium distinction on WiiM hardware (every
/// WiiM-Ultra-plus-Spotify capture examined has `gui_behavior` present,
/// 5/5). When it's genuinely absent (confirmed live, 2026-07-13, on a
/// non-WiiM device — Audio Pro Addon C5, whose `GetInfoEx` never carries
/// `song:guibehavior` at all, for any source, regardless of account tier),
/// `prev` now defaults to `true` rather than the `false` this used to fall
/// back to — Spotify isn't a radio-style source (it has a real track
/// queue/history), so it shouldn't inherit `PLAY_MEDIUMS_CTRL`'s
/// no-track-concept heuristic below; erring toward enabled (same
/// "trust real behavior, don't assume broken" philosophy as the `next`
/// override above) beats permanently hiding a working control on any
/// device whose firmware just doesn't send this WiiM-authored DIDL
/// extension tag at all.
///
/// Base static heuristic (used for everything else) is the `wiim` Python
/// SDK's `async_get_transport_capabilities()`: `play_medium` in
/// `PLAY_MEDIUMS_CTRL` means there's no track to skip to/from at all —
/// this is the actual "radio" heuristic, and still applies with
/// `gui_behavior` absent exactly as before; failing that, `track_source`
/// in `TRACK_SOURCES_CTRL` means a station-style service that supports
/// skipping forward but not rewinding back into history (confirmed on
/// `"Pandora2"` via a real capture — see `TRACK_SOURCES_CTRL`'s doc
/// comment).
pub fn decode_transport_caps_upnp(
    play_medium: &str, track_source: &str, play_type: i32, gui_behavior: Option<GuiBehavior>,
) -> SourceCapabilities {
    let (can_next, can_previous) = decode_next_prev_upnp(play_medium, track_source, gui_behavior);
    let (can_shuffle, can_repeat, can_seek) = loop_tier_upnp(play_medium, track_source, play_type).shuffle_repeat_seek();
    SourceCapabilities { can_next, can_previous, can_shuffle, can_repeat, can_seek, can_playpause: true }
}

/// `pywiim`'s `SOURCE_CAPABILITIES` table, translated into UPnP's
/// `play_medium`/`track_source` vocabulary — see `LoopTier`'s doc
/// comment for the reasoning, and `loop_tier_http`'s doc comment for the
/// Spotify caveat (identical here: follows `pywiim`'s static
/// `"spotify": FULL_CONTROL` as-is, known-questionable given the
/// contradicting real `guibehavior` capture, not corrected yet).
fn loop_tier_upnp(play_medium: &str, track_source: &str, play_type: i32) -> LoopTier {
    if is_physical_input_mode(play_type) {
        return LoopTier::None;
    }
    // RADIO-NETWORK/THIRD-DLNA already disable next/previous entirely
    // (`PLAY_MEDIUMS_CTRL`) — no queue and no forwarding target either.
    if play_medium == "RADIO-NETWORK" || play_medium == "THIRD-DLNA" {
        return LoopTier::None;
    }
    if TRACK_CONTROL_ONLY_UPNP.contains(&play_medium) {
        return LoopTier::TrackOnly;
    }
    // Same `pywiim`-vs-`wiim`-SDK disagreement on iHeartRadio specifically
    // as `loop_tier_http` — see its doc comment.
    if normalize_vendor(track_source) == "iheartradio" {
        return LoopTier::None;
    }
    LoopTier::Full
}

/// `play_medium` values where the device is relaying commands to/from an
/// external app rather than owning a queue — `pywiim`'s `"bluetooth"`/
/// `"cast"` entries, both `TRACK_CONTROL`.
const TRACK_CONTROL_ONLY_UPNP: &[&str] = &["BLUETOOTH", "CAST"];

fn decode_next_prev_upnp(
    play_medium: &str, track_source: &str, gui_behavior: Option<GuiBehavior>,
) -> (bool, bool) {
    if play_medium == "SPOTIFY" {
        return (true, gui_behavior.map_or(true, |g| g.prev));
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

/// Already-warned-about raw `actualQuality` values (see
/// `translate_quality_badge()`'s doc comment) — printed once per distinct
/// value per process run, not once per poll tick.
static WARNED_UNKNOWN_QUALITY: LazyLock<Mutex<HashSet<String>>> =
    LazyLock::new(|| Mutex::new(HashSet::new()));

/// Translates a non-empty `song:actualQuality` into a display badge.
/// Takes `service_name` too (`PlaybackState::source_name`) even though no
/// current mapping actually branches on it — every value confirmed so far
/// happens to be unambiguous across services, so this is one flat match
/// rather than a per-service one, but a future conflict (two services
/// reusing the same raw code for different tiers) would need to match on
/// `service_name` too, so the parameter is already part of the signature.
///
/// Confirmed mappings, grouped by the service whose own `actualQuality`
/// vocabulary each value belongs to (the raw values themselves don't say
/// so — each service picked its own scheme independently):
/// - **TIDAL**: `LOSSLESS`/`HI_RES`/`HI_RES_LOSSLESS` are all TIDAL
///   concepts. → `"HIGH"`/`"MQA"`/`"FLAC"` respectively, matching the WiiM
///   app's own relabeling. `LOSSLESS`→`"HIGH"` and `HI_RES_LOSSLESS`→
///   `"FLAC"` are confirmed directly from real captures, not guessed: two
///   captures independently named for what the WiiM app itself displayed
///   at capture time (`WiiM_Ultra_20260706_075156.TidalConnect-HIGH.json`,
///   `..._110556.TidalBuiltIn-HIGH.json`) both carry the raw wire value
///   `LOSSLESS`, proving the app really does show "HIGH" for that value
///   (an earlier version of this function dropped this specific mapping,
///   mistaking it for an unconfirmed guess — it wasn't). `HI_RES`→`"MQA"`
///   is TIDAL's now-deprecated MQA tier name, not independently capture-
///   confirmed but a known, documented TIDAL API value. TIDAL's remaining
///   documented tier (`LOW`, and the raw `HIGH` wire value itself, its
///   lossy AAC tier — not to be confused with the *display* label "HIGH"
///   above) has no captures showing what the app displays for it, so it's
///   genuinely unconfirmed — left as verbatim passthrough.
/// - **Qobuz**: `"6"`/`"7"`/`"27"` are Qobuz's own numeric `actualQuality`
///   enum → `"CD"`/`"Hi-Res"`/`"Hi-Res"` (`"27"` is a second, equivalent
///   Hi-Res tier code, not a typo for `"7"`).
/// - **Amazon Music**: `"UHD"`/`"HD"` → `"Ultra HD"`/`"HD"` (`"HD"` maps to
///   itself — listed explicitly so it reads as a confirmed value, not an
///   arbitrary passthrough).
///
/// Any other non-numeric string is also shown verbatim — an unrecognized
/// value that's at least human-readable is more useful displayed than
/// hidden. A purely-numeric string that reaches here, though, is some
/// other service's *internal* tier code with no confirmed translation
/// (the way Qobuz's own `"6"`/`"7"`/`"27"` would look before being
/// mapped) — showing a bare, context-free digit to the user is worse than
/// showing nothing, so this returns `None` (no badge at all) and logs a
/// warning once per distinct value, so a real one can get its own mapping
/// added once seen.
pub fn translate_quality_badge(label: &str, service_name: Option<&str>) -> Option<Rc<str>> {
    match (service_name, label) {
        // Qobuz.
        (_, "6")  => Some(Rc::from("CD")),
        (_, "7")  => Some(Rc::from("Hi-Res")),
        (_, "27") => Some(Rc::from("Hi-Res")),
        // Amazon Music.
        (_, "UHD") => Some(Rc::from("Ultra HD")),
        (_, "HD")  => Some(Rc::from("HD")),
        // TIDAL — see this function's doc comment for the capture
        // evidence confirming `LOSSLESS`/`HI_RES_LOSSLESS`; `HI_RES` is a
        // known documented value, not independently capture-confirmed.
        (_, "LOSSLESS")        => Some(Rc::from("HIGH")),
        (_, "HI_RES")          => Some(Rc::from("MQA")),
        (_, "HI_RES_LOSSLESS") => Some(Rc::from("FLAC")),
        (_, other) => {
            if !other.is_empty() && other.chars().all(|c| c.is_ascii_digit()) {
                if WARNED_UNKNOWN_QUALITY.lock().unwrap().insert(other.to_string()) {
                    eprintln!(
                        "{} [playback] unrecognized numeric actualQuality {other:?} (service={service_name:?}) — no display mapping, dropping",
                        super::timestamp(),
                    );
                }
                None
            } else {
                Some(Rc::from(other))
            }
        }
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
///   translated display string (see `translate_quality_badge`).
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
/// LinkPlay firmware sometimes reports a literal placeholder string instead
/// of leaving the album-art field empty when there's no real artwork —
/// confirmed live on a WiiM Ultra's UPnP `GetInfoEx` (`AlbumArtURI` value
/// `"un_known"`, on an HDMI input with nothing playing). Treated the same
/// as "no artwork" by every `art_url` call site in `state.rs` (HTTP
/// `getMetaInfo`, UPnP `GetInfoEx`, and GENA `AVTransport` NOTIFY all share
/// this one check) rather than handed to `fetch_art()`, which would just
/// produce a guaranteed-failing HTTP request for a nonsense host name.
pub fn is_valid_art_url(url: &str) -> bool {
    url.starts_with("http://") || url.starts_with("https://")
}

pub fn decode_quality_upnp(
    actual_quality: Option<&str>,
    bitrate: &str,
    format_s: &str,
    rate_hz: &str,
    protocol_info: Option<&str>,
    play_medium: &str,
    service_name: Option<&str>,
) -> (Option<AudioQuality>, Option<Rc<str>>) {
    let quality = decode_quality_http(bitrate, rate_hz, format_s);
    let codec_label = match actual_quality {
        Some(q) if !q.is_empty() => translate_quality_badge(q, service_name),
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
    /// Same as `caps()`, for a `LoopTier::None` source (physical input or
    /// radio) — shuffle/repeat/seek are always disabled there.
    fn caps_none_tier(can_next: bool, can_previous: bool) -> SourceCapabilities {
        SourceCapabilities { can_next, can_previous, can_shuffle: false, can_repeat: false, can_seek: false, can_playpause: true }
    }

    /// `LoopTier::TrackOnly` — next/previous/seek stay whatever the
    /// next/previous heuristic already gave, shuffle/repeat always false.
    fn caps_track_only(can_next: bool, can_previous: bool) -> SourceCapabilities {
        SourceCapabilities { can_next, can_previous, can_shuffle: false, can_repeat: false, can_seek: true, can_playpause: true }
    }

    /// All confirmed live, 2026-07-13, against a real Audio Pro Addon C5
    /// (whose `GetInfoEx` never carries `<PlayType>` at all — see
    /// `capabilities.rs`'s `FAMILY_AUDIO_PRO_ADDON_C5` doc comment): each
    /// value here is what `getPlayerStatusEx`'s real `mode` field reported
    /// while the device's `GetInfoEx` simultaneously showed the given
    /// `PlayMedium`, checked one input at a time, not guessed.
    #[test]
    fn mode_from_play_medium_fallback_matches_real_device_values() {
        assert_eq!(mode_from_play_medium_fallback("NONE"), 0);
        assert_eq!(mode_from_play_medium_fallback("UNKNOWN"), 0);
        assert_eq!(mode_from_play_medium_fallback(""), 0);
        assert_eq!(mode_from_play_medium_fallback("BLUETOOTH"), 41);
        assert_eq!(mode_from_play_medium_fallback("LINE-IN"), 40);
        assert_eq!(mode_from_play_medium_fallback("RCA"), 44);
        assert_eq!(mode_from_play_medium_fallback("SPOTIFY"), 31);
        // Was `10` — corrected 2026-07-20 against a real WiiM Ultra Tidal
        // Connect session, where `GetInfoEx`'s own `PlayType` was `32`
        // throughout; see `mode_from_play_medium()`'s own doc comment for
        // the real bug the old value caused.
        assert_eq!(mode_from_play_medium_fallback("TIDAL_CONNECT"), 32);
        assert_eq!(mode_from_play_medium_fallback("SONGLIST-NETWORK"), 10);
        // Confirmed via three real Pandora captures — see
        // `mode_from_play_medium()`'s own comment for the specific files.
        assert_eq!(mode_from_play_medium_fallback("STATION-NETWORK"), 10);
        assert_eq!(mode_from_play_medium_fallback("SOME_FUTURE_SERVICE"), 10);
        assert_eq!(mode_from_play_medium("SOME_FUTURE_SERVICE"), None);
        assert_eq!(mode_from_play_medium("TIDAL_CONNECT"), Some(32));
    }

    /// `"un_known"` is a real firmware placeholder confirmed live on a WiiM
    /// Ultra's UPnP `GetInfoEx` — see `is_valid_art_url`'s doc comment.
    #[test]
    fn is_valid_art_url_rejects_firmware_placeholder_and_empty() {
        assert!(!is_valid_art_url(""));
        assert!(!is_valid_art_url("un_known"));
        assert!(is_valid_art_url("http://cdn-albums.tunein.com/gn/2PKPNZ7WW2g.jpg"));
        assert!(is_valid_art_url("https://i.scdn.co/image/ab67616d0000b273c8ef777dbf6677f99f96210d"));
    }

    #[test]
    fn quality_badge_rule_tidal_hi_res_lossless_becomes_flac_lossless_becomes_high() {
        // Both confirmed directly from real captures — see
        // `translate_quality_badge`'s doc comment for the evidence.
        let (quality, label) = decode_quality_upnp(
            Some("HI_RES_LOSSLESS"), "1571", "24", "48000", Some("http-get:*:*:*"), "TIDAL_CONNECT",
            Some("TIDAL Connect"),
        );
        assert_eq!(label.as_deref(), Some("FLAC"));
        let q = quality.unwrap();
        assert_eq!(q.bit_rate_kbps, Some(1571.0));
        assert_eq!(q.bit_depth, Some(24));
        assert_eq!(q.sample_rate_khz, Some(48.0));

        let (_, label) = decode_quality_upnp(
            Some("LOSSLESS"), "546", "16", "44100", Some("http-get:*:*:*"), "TIDAL_CONNECT",
            Some("TIDAL Connect"),
        );
        assert_eq!(label.as_deref(), Some("HIGH"));
    }

    /// TIDAL's remaining two documented tiers (`LOW`, and the raw `HIGH`
    /// wire value itself — its lossy AAC tier, not to be confused with the
    /// WiiM app's *display* label "HIGH" for `LOSSLESS` above) have no
    /// captures showing what the app displays for them — verbatim
    /// passthrough, not a guessed mapping.
    #[test]
    fn quality_badge_rule_tidal_unconfirmed_tiers_shown_verbatim() {
        assert_eq!(translate_quality_badge("LOW", Some("TIDAL Connect")).as_deref(), Some("LOW"));
        assert_eq!(translate_quality_badge("HIGH", Some("TIDAL Connect")).as_deref(), Some("HIGH"));
    }

    /// TIDAL's now-deprecated MQA tier — a known, documented TIDAL API
    /// value, not independently capture-confirmed like `LOSSLESS`/
    /// `HI_RES_LOSSLESS` above.
    #[test]
    fn quality_badge_rule_tidal_hi_res_becomes_mqa() {
        assert_eq!(translate_quality_badge("HI_RES", Some("TIDAL Connect")).as_deref(), Some("MQA"));
    }

    #[test]
    fn quality_badge_rule_qobuz_tiers_translated() {
        assert_eq!(translate_quality_badge("6", Some("Qobuz")).as_deref(), Some("CD"));
        assert_eq!(translate_quality_badge("7", Some("Qobuz")).as_deref(), Some("Hi-Res"));
        // "27" is a second, equivalent Hi-Res tier code, not a typo for "7".
        assert_eq!(translate_quality_badge("27", Some("Qobuz")).as_deref(), Some("Hi-Res"));
    }

    #[test]
    fn quality_badge_rule_amazon_music_tiers_translated() {
        assert_eq!(
            translate_quality_badge("UHD", Some("Amazon Music")).as_deref(),
            Some("Ultra HD"),
        );
        assert_eq!(
            translate_quality_badge("HD", Some("Amazon Music")).as_deref(),
            Some("HD"),
        );
    }

    /// An unrecognized *numeric* code (some other service's own internal
    /// tier value with no confirmed mapping — the same shape Qobuz's `"6"`/
    /// `"7"` would have before being mapped) has no meaningful display as a
    /// bare digit, so it's dropped (`None`), not shown verbatim like a
    /// human-readable unrecognized string would be.
    #[test]
    fn quality_badge_rule_unrecognized_numeric_code_is_dropped() {
        assert_eq!(translate_quality_badge("42", Some("SomeNewService")), None);
    }

    /// An unrecognized *non-numeric* string, in contrast, is shown as-is —
    /// still more useful than nothing.
    #[test]
    fn quality_badge_rule_unrecognized_text_shown_verbatim() {
        assert_eq!(
            translate_quality_badge("SOME_FUTURE_TIER", Some("SomeNewService")).as_deref(),
            Some("SOME_FUTURE_TIER"),
        );
    }

    #[test]
    fn quality_badge_rule_present_empty_falls_back_to_protocol_info_for_songlist_local() {
        let (_, label) = decode_quality_upnp(
            Some(""), "320", "16", "48000",
            Some("http-get:*:audio/mpeg:DLNA.ORG_PN=MP3;DLNA.ORG_OP=01;"),
            "SONGLIST-LOCAL", None,
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
            "RADIO-NETWORK", None,
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
            "RADIO-NETWORK", None,
        );
        assert_eq!(label, None);
    }

    /// Real-world glitch: bit depth occasionally reported as 32 even though
    /// no WiiM hardware does more than 24-bit (and the WiiM app itself caps
    /// its own display there) — must be clamped, not shown as a bogus,
    /// unachievable 32-bit value.
    #[test]
    fn decode_quality_http_clamps_bit_depth_to_24() {
        let q = decode_quality_http("1000", "44100", "32").unwrap();
        assert_eq!(q.bit_depth, Some(24));
    }

    #[test]
    fn source_name_tidal_connect() {
        assert_eq!(decode_source_name_upnp("TIDAL_CONNECT", "Tidal", None).as_deref(), Some("TIDAL Connect"));
    }

    #[test]
    fn source_name_radio_network_uses_vendor_table() {
        assert_eq!(decode_source_name_upnp("RADIO-NETWORK", "newTuneIn", None).as_deref(), Some("TuneIn"));
    }

    #[test]
    fn source_name_songlist_local() {
        assert_eq!(decode_source_name_upnp("SONGLIST-LOCAL", "UPnPServer", None).as_deref(), Some("USB"));
    }

    #[test]
    fn source_name_bluetooth() {
        assert_eq!(decode_source_name_upnp("BLUETOOTH", "", None).as_deref(), Some("Bluetooth"));
    }

    #[test]
    fn source_name_spotify() {
        // TrackSource is a spotify: URI here, not a plain vendor name —
        // must resolve via the dedicated PlayMedium match, not vendor_display.
        assert_eq!(
            decode_source_name_upnp("SPOTIFY", "spotify:user:1516emh5k43jthv55arsid1k6:collection", None).as_deref(),
            Some("Spotify"),
        );
    }

    #[test]
    fn source_name_chromecast_via_vendor_display() {
        assert_eq!(decode_source_name_upnp("CAST", "CAST", None).as_deref(), Some("Chromecast"));
    }

    #[test]
    fn source_name_songlist_network_builtin_tidal_via_vendor_display() {
        assert_eq!(decode_source_name_upnp("SONGLIST-NETWORK", "Tidal", None).as_deref(), Some("TIDAL"));
    }

    /// Regression test for a real bug: an unrecognized/empty `TrackSource`
    /// showed the raw wire string "SONGLIST-NETWORK" verbatim instead of a
    /// real label — reported live, 2026-07-13, playing local media (DLNA
    /// push, no identifiable vendor) on an Audio Pro Addon C5.
    #[test]
    fn source_name_songlist_network_unknown_vendor_shows_generic_label() {
        assert_eq!(decode_source_name_upnp("SONGLIST-NETWORK", "", None).as_deref(), Some("Streaming"));
    }

    #[test]
    fn source_name_third_dlna() {
        assert_eq!(decode_source_name_upnp("THIRD-DLNA", "", None).as_deref(), Some("DLNA"));
    }

    #[test]
    fn source_name_analog_digital_inputs() {
        assert_eq!(decode_source_name_upnp("LINE-IN", "", None).as_deref(), Some("Line-In"));
        assert_eq!(decode_source_name_upnp("OPTICAL", "", None).as_deref(), Some("Optical"));
        assert_eq!(decode_source_name_upnp("HDMI", "", None).as_deref(), Some("HDMI"));
        assert_eq!(decode_source_name_upnp("PHONO", "", None).as_deref(), Some("Phono"));
    }

    /// Regression test for a real bug: the status text ("Stopped Line-in")
    /// used to stay generic even when the source dropdown correctly showed
    /// a per-device override ("AUX In") for the exact same input, because
    /// `decode_source_name_upnp`/`decode_source_name_http` had their own
    /// separate hardcoded labels instead of routing through
    /// `input_display_name()` like the dropdown does — reported live,
    /// 2026-07-13, on the Audio Pro Addon C5 profile that defines the
    /// override.
    #[test]
    fn source_name_honors_per_device_input_label_override() {
        let dev = Some(DeviceId::AudioProAddonC5);
        assert_eq!(decode_source_name_upnp("LINE-IN", "", dev).as_deref(), Some("AUX In"));
        assert_eq!(decode_source_name_http(40, "", dev).as_deref(), Some("AUX In"));
        // Unaffected inputs on the same device still use the generic label.
        assert_eq!(decode_source_name_upnp("RCA", "", dev).as_deref(), Some("RCA"));
        assert_eq!(decode_source_name_http(44, "", dev).as_deref(), Some("RCA"));
        // No device context at all: falls back to the generic label, same
        // as before this override existed.
        assert_eq!(decode_source_name_upnp("LINE-IN", "", None).as_deref(), Some("Line-In"));
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
        assert_eq!(decode_transport_caps_upnp("RADIO-NETWORK", "newTuneIn", 10, None), caps_none_tier(false, false));
        assert_eq!(decode_transport_caps_upnp("LINE-IN", "", 40, None), caps_none_tier(false, false));
        assert_eq!(decode_transport_caps_upnp("HDMI", "", 49, None), caps_none_tier(false, false));
    }

    #[test]
    fn transport_caps_upnp_station_services_disable_previous_only() {
        assert_eq!(decode_transport_caps_upnp("STATION-NETWORK", "Pandora2", 10, None), caps(true, false));
    }

    #[test]
    fn transport_caps_upnp_spotify_falls_back_to_previous_true_without_guibehavior() {
        // `next` is always forced true for Spotify regardless of
        // `gui_behavior`. `prev` without a `gui_behavior` reading now also
        // defaults to `true` — real-device-confirmed (Audio Pro Addon C5,
        // 2026-07-13, Premium account, no `song:guibehavior` in its
        // `GetInfoEx` for any source) that `gui_behavior`'s absence isn't a
        // free-tier signal, just a trait of firmware that doesn't send
        // this WiiM-authored DIDL extension tag at all — every archived
        // WiiM-Ultra-plus-Spotify capture has it present (5/5), so there's
        // no confirmed case left of a *WiiM* device genuinely lacking it.
        assert_eq!(decode_transport_caps_upnp("SPOTIFY", "spotify:playlist:37i9dQZF1EIZAuCHB2O9dH", 31, None), caps(true, true));
    }

    #[test]
    fn transport_caps_upnp_spotify_next_forced_true_even_if_guibehavior_disagrees() {
        // Confirmed via a real free-tier capture: guibehavior claimed
        // next:false, but pressing the button actually skipped forward.
        let gb = GuiBehavior { next: false, prev: false };
        assert_eq!(decode_transport_caps_upnp("SPOTIFY", "spotify:playlist:x", 31, Some(gb)), caps(true, false));
    }

    #[test]
    fn transport_caps_upnp_spotify_previous_follows_guibehavior_premium_vs_free() {
        // Confirmed via two real captures on the same playlist mechanism,
        // differing only by account tier.
        let free = GuiBehavior { next: false, prev: false };
        assert_eq!(decode_transport_caps_upnp("SPOTIFY", "spotify:playlist:free", 31, Some(free)), caps(true, false));
        let premium = GuiBehavior { next: true, prev: true };
        assert_eq!(decode_transport_caps_upnp("SPOTIFY", "spotify:playlist:premium", 31, Some(premium)), caps(true, true));
    }

    #[test]
    fn transport_caps_upnp_guibehavior_trusted_over_static_heuristic_when_present() {
        // A source the static heuristic would otherwise default-enable,
        // but guibehavior says otherwise for this specific track — trust it.
        let gb = GuiBehavior { next: false, prev: true };
        assert_eq!(decode_transport_caps_upnp("TIDAL_CONNECT", "Tidal", 32, Some(gb)), caps(false, true));
    }

    #[test]
    fn transport_caps_upnp_unknown_medium_defaults_permissive() {
        assert_eq!(decode_transport_caps_upnp("TIDAL_CONNECT", "Tidal", 32, None), caps(true, true));
        assert_eq!(decode_transport_caps_upnp("SONGLIST-LOCAL", "UPnPServer", 11, None), caps(true, true));
    }

    #[test]
    fn transport_caps_upnp_bluetooth_and_cast_are_track_control_only() {
        assert_eq!(decode_transport_caps_upnp("BLUETOOTH", "", 41, None), caps_track_only(true, true));
        assert_eq!(decode_transport_caps_upnp("CAST", "", 5, None), caps_track_only(true, true));
    }

    #[test]
    fn transport_caps_upnp_iheartradio_track_source_is_pure_radio_for_loop() {
        let result = decode_transport_caps_upnp("STATION-NETWORK", "iHeartRadio", 10, None);
        assert_eq!((result.can_next, result.can_previous), (true, false));
        assert!(!result.can_shuffle && !result.can_repeat && !result.can_seek);
    }

    #[test]
    fn transport_caps_http_physical_inputs_and_idle_disable_both() {
        assert_eq!(decode_transport_caps_http(40, ""), caps_none_tier(false, false)); // Line-In
        assert_eq!(decode_transport_caps_http(49, ""), caps_none_tier(false, false)); // HDMI
        assert_eq!(decode_transport_caps_http(43, ""), caps_none_tier(false, false)); // Optical
        assert_eq!(decode_transport_caps_http(44, ""), caps_none_tier(false, false)); // RCA
        assert_eq!(decode_transport_caps_http(54, ""), caps_none_tier(false, false)); // Phono
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

        let non_physical = decode_transport_caps_upnp("TIDAL_CONNECT", "Tidal", 32, None);
        assert!(non_physical.can_shuffle && non_physical.can_repeat && non_physical.can_seek);
        let physical = decode_transport_caps_upnp("OPTICAL", "", 43, None);
        assert!(!physical.can_shuffle && !physical.can_repeat && !physical.can_seek);
    }

    #[test]
    fn transport_caps_upnp_physical_input_check_is_driven_by_play_type_not_play_medium() {
        // play_type (== HDMI's 49) decides this, not the play_medium string
        // — confirmed via real captures that play_type == HTTP's mode
        // byte-for-byte, so this is the same check decode_transport_caps_http
        // makes, not a separate PlayMedium-keyed table.
        let result = decode_transport_caps_upnp("SOME_UNRECOGNIZED_MEDIUM", "", 49, None);
        assert!(!result.can_shuffle && !result.can_repeat && !result.can_seek);
    }

    #[test]
    fn transport_caps_http_unconfirmed_network_services_default_enabled() {
        // No positive reason to believe these lack transport control —
        // err toward enabling rather than disabling until proven otherwise.
        // USB/TIDAL Connect/Lyrion/Qobuz genuinely own their own queue
        // (LoopTier::Full); AirPlay/DLNA/Chromecast/Bluetooth are relayed
        // to/from an external app (LoopTier::TrackOnly) — shuffle/repeat
        // don't apply there even though next/previous still do.
        assert_eq!(decode_transport_caps_http(11, ""), caps(true, true)); // USB
        assert_eq!(decode_transport_caps_http(1,  ""), caps_track_only(true, true)); // AirPlay
        assert_eq!(decode_transport_caps_http(2,  ""), caps_track_only(true, true)); // DLNA
        assert_eq!(decode_transport_caps_http(5,  ""), caps_track_only(true, true)); // Chromecast
        assert_eq!(decode_transport_caps_http(32, ""), caps(true, true)); // TIDAL Connect
        assert_eq!(decode_transport_caps_http(34, ""), caps(true, true)); // Lyrion
        assert_eq!(decode_transport_caps_http(36, ""), caps(true, true)); // Qobuz
        assert_eq!(decode_transport_caps_http(41, ""), caps_track_only(true, true)); // Bluetooth
    }

    #[test]
    fn transport_caps_http_wifi_bucket_uses_vendor() {
        assert_eq!(decode_transport_caps_http(10, "newTuneIn"), caps_none_tier(false, false));
        assert_eq!(decode_transport_caps_http(10, "WiiMRadio"), caps_none_tier(false, false));
        assert_eq!(decode_transport_caps_http(10, "Linkplay Radio"), caps_none_tier(false, false));
        assert_eq!(decode_transport_caps_http(10, "vTuner"), caps_none_tier(false, false));
        assert_eq!(decode_transport_caps_http(10, "RadioParadise"), caps_none_tier(false, false));
        // Pandora: next/previous per TRACK_SOURCES_CTRL (previous only
        // disabled), but shuffle/repeat/seek all enabled (LoopTier::Full,
        // per pywiim's "pandora" entry) since it isn't classified as pure
        // radio the way iHeartRadio specifically is.
        assert_eq!(decode_transport_caps_http(10, "Pandora2"), caps(true, false));
        assert_eq!(decode_transport_caps_http(10, "UDiskLocal"), caps(true, true));
        assert_eq!(decode_transport_caps_http(10, ""), caps(true, true));
    }

    #[test]
    fn transport_caps_http_iheartradio_is_pure_radio_for_loop_despite_track_sources_ctrl() {
        // TRACK_SOURCES_CTRL (next/previous) says "previous disabled,
        // next OK" for iHeartRadio, but pywiim's independent shuffle/
        // repeat/seek classification treats it as pure radio (None tier)
        // — the two heuristics deliberately disagree here.
        let result = decode_transport_caps_http(10, "iHeartRadio");
        assert_eq!((result.can_next, result.can_previous), (true, false));
        assert!(!result.can_shuffle && !result.can_repeat && !result.can_seek);
    }

    #[test]
    fn transport_caps_http_spotify_defaults_fully_enabled_tier_unknown() {
        // HTTP has no guibehavior-equivalent signal to distinguish free
        // from premium accounts, unlike UPnP — default permissive.
        assert_eq!(decode_transport_caps_http(31, ""), caps(true, true));
    }

    #[test]
    fn loop_mode_encode_decode_roundtrip() {
        // Every wire value 0-5 round-trips through encode/decode, and vice
        // versa for every (shuffle, repeat) pair — guards the exact table a
        // real WiiM-device bug report was about (mode 5 silently ignored
        // over HTTP on some WiiM units, motivating the UPnP write path).
        for mode in 0..=5 {
            let (shuffle, repeat) = decode_loop_mode_http(mode);
            assert_eq!(encode_loop_mode(shuffle, repeat), mode, "mode {mode} didn't round-trip");
        }
        for shuffle in [false, true] {
            for repeat in [RepeatMode::Off, RepeatMode::All, RepeatMode::One] {
                let mode = encode_loop_mode(shuffle, repeat);
                assert_eq!(decode_loop_mode_http(mode), (shuffle, repeat));
            }
        }
    }
}
