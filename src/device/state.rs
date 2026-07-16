/// Device state GObject — owns the WiiM client, caches polled state, and
/// emits GTK signals when state changes.  All methods run on the GTK main
/// thread; API calls are dispatched to a tokio runtime and results are
/// returned via `async_channel`.
///
/// Signals
/// -------
/// * `device-changed`   — device info (re)loaded or cleared (UI should rebuild)
/// * `playback-changed(u32)` — player status / metadata / artwork updated;
///                             the `u32` is a `PlaybackChanged` bitmask
/// * `input-changed`    — current input mode changed
/// * `inputs-changed`   — available input list / per-input enabled flags
///                        changed (rebuild source menu)
/// * `output-changed`   — audio output selection changed
/// * `outputs-changed`  — supported output list updated (rebuild menu)
/// * `network-changed`  — netstat or RSSI changed
/// * `remote-changed`   — BLE remote presence/battery/RSSI changed — kept
///                        separate from `network-changed` since it's a
///                        different physical thing (a battery-powered
///                        accessory, not the device's own network link)
/// * `presets-changed`  — preset list (re)loaded; UI should re-read `presets()`

/// Bitmask values for the `playback-changed` signal parameter.
pub mod playback_changed {
    pub const ARTWORK: u32 = 0x01;
    pub const TITLE:   u32 = 0x02;
    pub const ARTIST:  u32 = 0x04;
    pub const ALBUM:   u32 = 0x08;
    pub const TIME:    u32 = 0x10; // curpos + totlen
    pub const VOLUME:  u32 = 0x20; // vol + mute
    pub const OTHER:   u32 = 0x40; // status, loop mode, quality, etc.
    pub const ALL:     u32 = 0x7F;
}

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

/// Slow poll (device info, outputs, presets) runs at most this often.
const SLOW_POLL_INTERVAL: Duration = Duration::from_secs(10);
/// `Simple`-mode poll cadence — deliberately a separate constant from
/// `SLOW_POLL_INTERVAL` even though it starts at the same value, so the two
/// can be tuned independently later without hunting down every place that
/// assumed they were the same.
const SIMPLE_POLL_INTERVAL: Duration = Duration::from_secs(5);
/// Volume commands are rate-limited: at most one per this interval.
const VOLUME_DEBOUNCE: Duration = Duration::from_millis(500);
/// After sending a volume command, poll-reported volume is distrusted for
/// this long — a real device (confirmed on AudioCast) can keep reporting
/// its *pre-command* volume for a moment after accepting a `SetVolume`, so
/// the self-heal resync (`vol_changed` in `process_poll_http`/
/// `process_poll_upnp`) would otherwise briefly snap the slider back to
/// the old value before the following poll corrects it forward again.
/// Distinct from `VOLUME_DEBOUNCE`, which rate-limits *outgoing* commands
/// — this instead limits how soon an *incoming* poll reading is trusted
/// after the last outgoing one.
const VOLUME_POLL_SETTLE: Duration = Duration::from_secs(1);
/// `trigger_poll()`'s one-shot follow-up poll is spaced at least this long
/// after whichever poll happened most recently — long enough that a real
/// device has almost certainly applied the command that prompted it.
const POLL_SETTLE_DELAY: Duration = Duration::from_millis(400);
/// How long to wait for a `switch_input()` to actually take effect (a poll
/// reporting the new mode) before giving up and reverting the UI to
/// whatever the device is still really on. Input switches can take real
/// device-side time (e.g. an HDMI handshake/EDID negotiation), so this is
/// longer than `POLL_SETTLE_DELAY`.
const INPUT_CHANGE_TIMEOUT: Duration = Duration::from_secs(2);

use glib::prelude::*;
use glib::subclass::prelude::*;
use gtk::glib;

pub static DEBUG_STATE: AtomicBool = AtomicBool::new(false);

/// Takes the `DeviceState` itself (not a bare IP string) so the identifying
/// prefix can change later (e.g. to a device name) without touching every
/// call site — with several devices' windows open at once, a bare
/// `[state] ...` line gives no way to tell which one it belongs to.
fn dbg(ds: &DeviceState, msg: &str) {
    if DEBUG_STATE.load(Ordering::Relaxed) {
        println!("[state] {}: {msg}", ds.ip());
    }
}

/// Parse `getStatusEx`'s `BleRemoteConnected` ("1"/"0") into a tri-state:
/// `None` when the field is empty (device has no BLE remote hardware at
/// all, or the response didn't include it), `Some(true)`/`Some(false)`
/// otherwise.
fn parse_remote_connected(raw: &str) -> Option<bool> {
    if raw.is_empty() { None } else { Some(raw == "1") }
}

use super::api::{
    ApiOutcome, AudioOutputStatus, BtStatus, DeviceInfo, MetaData, OutputEntry, PlayerStatus,
    PresetEntry, PresetFetchOutcome, TlsMode, WiimClient, TLS_MODE,
};
use super::capabilities::{self, DeviceCapabilities};
use super::playback;
use super::playback::{AccessMethod, PlaybackState, RepeatMode};
use super::upnp::{self, UpnpClient};

// ── Connection state ──────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ConnectionState {
    #[default]
    Disconnected,
    Connecting,
    Connected,
    /// Believed offline (displayed "Disconnected") — fast/slow polling is
    /// fully stopped (see `do_poll()`'s early return). Recovery is
    /// `maybe_self_reconnect()` (its own doc comment) retrying on
    /// `SLOW_POLL_INTERVAL`, unless something has registered an offline
    /// callback (`set_offline_callback` — nothing in the normal app path
    /// does this anymore; `device::discovery_manager` reads
    /// `connection_state()` and lets `DeviceState` manage its own
    /// recovery), in which case that
    /// callback owns recovery instead (via `mark_reachable()`) and
    /// `maybe_self_reconnect()` steps aside. `mark_offline()`/
    /// `mark_reachable()` remain public for any external caller that wants
    /// to drive a `DeviceState`'s connectivity directly.
    Failed,
}

// ── Poll payload ──────────────────────────────────────────────────────────────

/// A fast-poll tick's result. `Http`/`Upnp` are mutually exclusive per
/// tick, never both: `dispatch_fast_poll()` decides which single backend to
/// hit *before* firing anything, based on `access`, rather than always
/// fetching HTTP as a baseline and optionally layering UPnP on top. No HTTP
/// fallback when `UpnpPolled` is selected but no `UpnpClient` has been
/// discovered yet — see `dispatch_fast_poll`'s doc comment.
///
/// `PresetArt` isn't part of that either/or choice at all — it's a preset
/// slot's artwork download (an external CDN fetch, not a WiiM API call)
/// completing. It rides the same channel/processor as the fast poll simply
/// because that's the existing per-tick pipeline already available, not
/// because it's genuinely a fast-poll backend result; see
/// `dispatch_pending_preset_art()`.
enum PollData {
    Http { status: Option<PlayerStatus>, meta: Option<MetaData>, bt_status: Option<ApiOutcome<BtStatus>> },
    Upnp { info: Option<upnp::InfoEx>, bt_status: Option<ApiOutcome<BtStatus>> },
    PresetArt { slot: usize, url: String, bytes: Option<Vec<u8>> },
}

// ── Slow poll ─────────────────────────────────────────────────────────────────
//
// The slow poll used to fire 3-4 sequential HTTP calls back to back every
// SLOW_POLL_INTERVAL. These embedded HTTP servers handle concurrent/rapid-fire
// connections poorly, so instead it's a rotation that dispatches exactly one
// call per 1-second tick, spread across the first ~4 seconds of every ~10s
// cycle, then idles until the next cycle starts. See start_unified_timer.

/// One phase of the slow-poll rotation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SlowPollPhase {
    Presets,
    Outputs,
    OutputStatus,
    DeviceInfo,
}

impl SlowPollPhase {
    const FIRST: Self = Self::Presets;

    /// The phase after this one; wraps back to `FIRST` after `DeviceInfo`,
    /// which the caller uses as the "rotation complete" signal.
    fn next(self) -> Self {
        match self {
            Self::Presets      => Self::Outputs,
            Self::Outputs      => Self::OutputStatus,
            Self::OutputStatus => Self::DeviceInfo,
            Self::DeviceInfo   => Self::FIRST,
        }
    }
}

enum SlowPollResult {
    /// `source`/`probe_failures` are the resolved `PresetSource` and
    /// consecutive-network-failure count to persist on `DeviceCapabilities`
    /// (see `fetch_presets_with_fallback()`); `entries` is `Some((fp,
    /// entries))` when the fingerprint changed, `None` when unchanged/still
    /// retrying/unavailable.
    Presets {
        source:         capabilities::PresetSource,
        probe_failures: u32,
        entries:        Option<(String, Vec<PresetEntry>)>,
    },
    /// `ApiOutcome::Unsupported` is a *confirmed* answer (device said so in
    /// plain text) — `handle_slow_poll_outputs()` gives up on it
    /// immediately, no retry budget, unlike `Failed` (transient).
    Outputs(ApiOutcome<Vec<OutputEntry>>),
    OutputStatus(ApiOutcome<AudioOutputStatus>),
    DeviceInfo(Option<DeviceInfo>),
}

/// Consecutive `PresetFetchOutcome::Failed` results (network/transport
/// failure, not a confirmed-unsupported response) tolerated for whichever
/// backend is currently being attempted before giving up on it exactly as
/// a confirmed-unsupported response would. Same reasoning/value as
/// `OUTPUTS_PROBE_FAIL_THRESHOLD`/`OUTPUT_STATUS_PROBE_FAIL_THRESHOLD`
/// below — these embedded HTTP/UPnP servers are flaky enough that a single
/// miss shouldn't immediately be treated as "device doesn't support this."
const PRESET_PROBE_FAIL_THRESHOLD: u32 = 3;

/// Consecutive `ApiOutcome::Failed` (transient — network/parse failure, not
/// a confirmed-unsupported response) results tolerated for
/// `getSoundCardModeSupportList`/`getNewAudioOutputHardwareMode` before
/// giving up on them for this connection (`probes_outputs`/
/// `probes_output_status` flip to `false`). `ApiOutcome::Unsupported` — the
/// device explicitly saying "unknown command" — skips this budget entirely
/// and gives up immediately; retrying a *definite* answer on a timer would
/// just be wrong, not merely wasteful. These failure counters live here,
/// on `Inner`, rather than on `DeviceCapabilities` — same reasoning as
/// `preset_probe_failures` above: they're short-lived per-tick retry
/// bookkeeping, not part of the device's resolved capability set.
const OUTPUTS_PROBE_FAIL_THRESHOLD: u32 = 3;
const OUTPUT_STATUS_PROBE_FAIL_THRESHOLD: u32 = 3;

/// Increments `*failures` for one `ApiOutcome::Failed` result and reports
/// whether the retry budget (`threshold`) is now exhausted — shared by
/// `handle_slow_poll_outputs()`/`handle_slow_poll_output_status()`, which
/// only ever call this for `Failed` (transient); `ApiOutcome::Unsupported`
/// skips the budget entirely and gives up on the spot, in the caller.
fn record_probe_failure(failures: &mut u32, threshold: u32, command: &str) -> bool {
    *failures += 1;
    eprintln!("[device] {command} failed ({failures}/{threshold})", failures = *failures);
    let gave_up = *failures >= threshold;
    if gave_up {
        eprintln!(
            "[device] giving up on {command} for this device after {failures} consecutive failures",
            failures = *failures,
        );
    }
    gave_up
}

/// Outcome of one single-backend preset-fetch attempt, already folding in
/// the retry-budget decision (see `PRESET_PROBE_FAIL_THRESHOLD`).
enum PresetProbeStep {
    /// The call worked — `Some((fp, entries))` on a changed list, `None`
    /// on an unchanged one.
    Ok(Option<(String, Vec<PresetEntry>)>),
    /// Confirmed unsupported, or `Failed` enough consecutive times to give
    /// up as if it were — final either way, try the next fallback (or
    /// `Unavailable` if there is none).
    GaveUp,
    /// Still within the retry budget (or no `UpnpClient` discovered yet
    /// this tick) — try the same backend again next cycle with this
    /// updated failure count.
    Retry(u32),
}

/// Interprets one raw `PresetFetchOutcome` against the current retry
/// budget — pure/sync so it's testable without a real network call. Shared
/// by `probe_http`/`probe_upnp` so the threshold policy exists in exactly
/// one place for both backends.
fn resolve_preset_probe_step(outcome: PresetFetchOutcome, probe_failures: u32) -> PresetProbeStep {
    match outcome {
        PresetFetchOutcome::Unchanged            => PresetProbeStep::Ok(None),
        PresetFetchOutcome::Changed(fp, entries) => PresetProbeStep::Ok(Some((fp, entries))),
        PresetFetchOutcome::Unsupported          => PresetProbeStep::GaveUp,
        PresetFetchOutcome::Failed => {
            let failures = probe_failures + 1;
            if failures >= PRESET_PROBE_FAIL_THRESHOLD { PresetProbeStep::GaveUp }
            else { PresetProbeStep::Retry(failures) }
        }
    }
}

async fn probe_http(client: &WiimClient, old_fp: &str, probe_failures: u32) -> PresetProbeStep {
    resolve_preset_probe_step(client.fetch_presets(old_fp).await, probe_failures)
}

async fn probe_upnp(upnp_client: Option<UpnpClient>, old_fp: &str, probe_failures: u32) -> PresetProbeStep {
    let Some(uc) = upnp_client else {
        // Not discovered yet this tick — neither a strike nor progress;
        // try again later once discovery succeeds, same budget untouched.
        return PresetProbeStep::Retry(probe_failures);
    };
    resolve_preset_probe_step(uc.get_key_mapping_presets(old_fp).await, probe_failures)
}

type PresetProbeResolution = (capabilities::PresetSource, u32, Option<(String, Vec<PresetEntry>)>);

/// Turns one backend's `PresetProbeStep` into the `(source, probe_failures,
/// entries)` triple to persist — pure/sync, shared by every
/// `fetch_presets_via_*`/`fetch_presets_resolving_unknown` below so the
/// same three-way mapping (success / still-retrying / gave-up) isn't
/// repeated per backend. `retry_source` is what to persist while still
/// within the retry budget (normally just `source`, the axis currently
/// being tried); `ok_source` is what to persist on a genuine success.
/// Returns `None` on `GaveUp` so the caller decides what happens next (a
/// further fallback, or `Unavailable`) — that decision differs per caller,
/// so it isn't folded into this function.
fn resolve_preset_step(
    step:         PresetProbeStep,
    retry_source: capabilities::PresetSource,
    ok_source:    capabilities::PresetSource,
) -> Option<PresetProbeResolution> {
    match step {
        PresetProbeStep::Ok(entries)     => Some((ok_source, 0, entries)),
        PresetProbeStep::Retry(failures) => Some((retry_source, failures, None)),
        PresetProbeStep::GaveUp          => None,
    }
}

/// `source == Http`: HTTP is the whole story — giving up (confirmed
/// unsupported, or exhausted retries) goes straight to `Unavailable`,
/// there's no further fallback once HTTP itself was the chosen backend.
async fn fetch_presets_via_http(
    client: &WiimClient, old_fp: &str, probe_failures: u32,
) -> PresetProbeResolution {
    use capabilities::PresetSource;
    let step = probe_http(client, old_fp, probe_failures).await;
    resolve_preset_step(step, PresetSource::Http, PresetSource::Http)
        .unwrap_or((PresetSource::Unavailable, 0, None))
}

/// `source == Upnp`: same shape as `fetch_presets_via_http`, just the
/// other backend — giving up here also means `Unavailable`.
async fn fetch_presets_via_upnp(
    upnp_client: Option<UpnpClient>, old_fp: &str, probe_failures: u32,
) -> PresetProbeResolution {
    use capabilities::PresetSource;
    let step = probe_upnp(upnp_client, old_fp, probe_failures).await;
    resolve_preset_step(step, PresetSource::Upnp, PresetSource::Upnp)
        .unwrap_or((PresetSource::Unavailable, 0, None))
}

/// `source == Unknown`: try HTTP first (retry budget tracked against
/// `Unknown` itself, so a later tick still lands back here rather than
/// prematurely committing to `Http` while only mid-retry). Only once HTTP
/// is confirmed unsupported or has exhausted its own retries does this
/// fall through to trying UPnP — same tick, with a fresh retry budget of
/// its own (a different backend, no strikes carried over), so a device
/// that needs UPnP doesn't sit idle for a whole extra slow-poll cycle just
/// to notice HTTP doesn't work.
async fn fetch_presets_resolving_unknown(
    client:         &WiimClient,
    upnp_client:    Option<UpnpClient>,
    old_fp:         &str,
    probe_failures: u32,
) -> PresetProbeResolution {
    use capabilities::PresetSource;
    let step = probe_http(client, old_fp, probe_failures).await;
    match resolve_preset_step(step, PresetSource::Unknown, PresetSource::Http) {
        Some(resolved) => resolved,
        None => fetch_presets_via_upnp(upnp_client, old_fp, 0).await,
    }
}

/// Resolves one preset-list fetch, dispatching to whichever backend
/// `source` (the capability's last-persisted `PresetSource`, `Unknown` the
/// first time) currently calls for. See `fetch_presets_via_http`/
/// `fetch_presets_via_upnp`/`fetch_presets_resolving_unknown` for what
/// each source actually does; all three retry a transient network failure
/// up to `PRESET_PROBE_FAIL_THRESHOLD` times (`resolve_preset_step`) before
/// treating it the same as a confirmed-unsupported response — a genuine
/// "unknown command" is still immediate/final, never retried. Reports back
/// the resolved `PresetSource` and failure count so the caller can persist
/// them via `DeviceCapabilities::record_preset_probe()`.
async fn fetch_presets_with_fallback(
    client:         &WiimClient,
    upnp_client:    Option<UpnpClient>,
    source:         capabilities::PresetSource,
    old_fp:         &str,
    probe_failures: u32,
) -> PresetProbeResolution {
    use capabilities::PresetSource;
    match source {
        PresetSource::Http =>
            fetch_presets_via_http(client, old_fp, probe_failures).await,
        PresetSource::Upnp =>
            fetch_presets_via_upnp(upnp_client, old_fp, probe_failures).await,
        PresetSource::Unknown =>
            fetch_presets_resolving_unknown(client, upnp_client, old_fp, probe_failures).await,
        PresetSource::Unavailable => (PresetSource::Unavailable, 0, None),
    }
}

async fn run_slow_poll_phase(
    client:         WiimClient,
    phase:          SlowPollPhase,
    preset_fp:      String,
    upnp_client:    Option<UpnpClient>,
    preset_source:  capabilities::PresetSource,
    preset_probe_failures: u32,
) -> SlowPollResult {
    match phase {
        SlowPollPhase::Presets => {
            let (source, probe_failures, entries) = fetch_presets_with_fallback(
                &client, upnp_client, preset_source, &preset_fp, preset_probe_failures,
            ).await;
            SlowPollResult::Presets { source, probe_failures, entries }
        }
        SlowPollPhase::Outputs =>
            SlowPollResult::Outputs(client.get_sound_card_mode_support_list().await),
        SlowPollPhase::OutputStatus =>
            SlowPollResult::OutputStatus(client.get_audio_output().await),
        SlowPollPhase::DeviceInfo =>
            SlowPollResult::DeviceInfo(client.get_device_info().await.ok()),
    }
}

/// The HTTP fast poll: `getbtstatus` (only if `want_bt`) + `getPlayerStatusEx`
/// + `getMetaInfo` (only if not skipped — see below), sequential, never
/// concurrent. Only called by `dispatch_fast_poll`/`trigger_poll` when
/// `access == AccessMethod::Http` — a `UpnpPolled` device never runs this
/// (see `PollData`'s doc comment), so `inner.player_status`/`inner.metadata`
/// simply stop updating while UPnP is selected.
///
/// `want_bt` (computed by the caller from `current_mode`, necessarily a
/// snapshot from *before* this tick's own `getPlayerStatusEx` answers —
/// there's no way to know this tick's real mode any earlier) gates whether
/// `getbtstatus` is called at all; it's fetched *first*, ahead of
/// `getPlayerStatusEx`, specifically so its fresh `connected` value (not
/// `inner.playback.bt_connected`, which could be up to one tick stale) is
/// what decides whether to skip `getMetaInfo` this same tick — nothing is
/// casting while Bluetooth is connected-to-nothing, so there's no metadata
/// to fetch, and `process_poll_http()` force-blanks the cached song data
/// in that case anyway (`blank_playback_baseline`) rather than
/// trusting whatever's still sitting in `meta`. `getPlayerStatusEx` itself
/// is always fetched regardless of any of this — status/mode/volume
/// polling keeps running unconditionally, since that's how a later input
/// change or a volume command's result is noticed at all.
///
/// `probe_bt` — the capability flag `probes_bt` (see `DeviceCapabilities`).
/// When `false` (confirmed `"unknown command"` at least once already),
/// `getbtstatus` is never called at all — `skip_meta` below only fires on
/// a *confirmed* `connected: false`, never on "didn't ask"/"asked and it
/// failed," so a device that's given up probing keeps fetching metadata
/// normally rather than getting stuck treating Bluetooth as permanently
/// silent.
async fn fetch_http_fast_poll(
    client: WiimClient, want_bt: bool, probe_bt: bool, has_meta_info: bool,
) -> (Option<PlayerStatus>, Option<MetaData>, Option<ApiOutcome<BtStatus>>) {
    let bt_status = if want_bt && probe_bt { Some(client.get_bt_status().await) } else { None };
    let skip_meta = matches!(&bt_status, Some(ApiOutcome::Ok(b)) if !b.connected);
    let status = client.get_status().await.ok();
    let meta = if skip_meta {
        None
    } else if has_meta_info {
        client.get_meta_info().await.ok()
    } else {
        // Family capability flag says `getMetaInfo` isn't supported at all
        // (confirmed: returns "unknown command" — e.g. Audio Pro Addon C5
        // on old firmware) — synthesize from this tick's own `status`
        // instead of spending a request on a call known to fail. See
        // `MetaData::from_player_status`'s doc comment for what's lost
        // (no artwork/quality this way — a real trade-off, not a bug).
        status.as_ref().map(MetaData::from_player_status)
    };
    (status, meta, bt_status)
}

/// The UPnP fast poll: `getbtstatus` (only if `want_bt`, HTTP — there's no
/// UPnP equivalent implemented) followed by `GetInfoEx` on an
/// already-discovered `UpnpClient`. The caller (`dispatch_fast_poll`/
/// `trigger_poll`) only ever calls this once it has confirmed a client
/// exists. `want_bt` and the call ordering mirror `fetch_http_fast_poll`'s
/// own doc comment exactly (bt status fetched first, ahead of the main
/// call) — the only difference here is there's no `skip_meta` decision to
/// make from it, since `GetInfoEx` always bundles metadata into the same
/// single call regardless.
///
/// Also follows `GetInfoEx` up with a supplementary `RenderingControl.GetMute`
/// call, but *only* when this response's `current_mute` came back `None`
/// (tag absent — confirmed on iEAST AudioCast; see `InfoEx::current_mute`'s
/// doc comment). A failed supplementary `GetMute` call just leaves
/// `current_mute` as `None` for this tick; `process_poll_upnp` treats that
/// as "no new information," not "unmuted."
///
/// This is the ideal stopgap to poll mute with, but `RenderingControl`
/// GENA eventing (a separate, already-tracked idea) would eventually
/// remove the need to poll for it at all.
async fn fetch_upnp_fast_poll(
    upnp_client: UpnpClient, client: WiimClient, want_bt: bool, probe_bt: bool,
) -> (Option<upnp::InfoEx>, Option<ApiOutcome<BtStatus>>) {
    let bt_status = if want_bt && probe_bt { Some(client.get_bt_status().await) } else { None };
    let Some(mut info) = upnp_client.get_info_ex().await.ok() else { return (None, bt_status) };
    if info.current_mute.is_none() {
        // AudioCast (and maybe other similarly slow devices) gets a
        // connection error on almost second attempt at this, give it
        // some breathing room and delay the GetMute by 100ms
        tokio::time::sleep(Duration::from_millis(100)).await;
        if let Ok(muted) = upnp_client.get_mute().await {
            info.current_mute = Some(muted);
        }
    }
    if info.play_type == -1 {
        // `<PlayType>` confirmed permanently absent on some devices (Audio
        // Pro Addon C5) — fabricate a substitute from `PlayMedium` instead
        // of leaving input-source tracking stuck at "unknown" forever. See
        // `mode_from_play_medium_fallback()`'s doc comment for why this is
        // preferred over a supplementary HTTP call (an earlier version of
        // this fix): zero extra network cost, same data already in hand.
        info.play_type = playback::mode_from_play_medium_fallback(&info.play_medium);
    }
    (Some(info), bt_status)
}

// ── Cached device state ───────────────────────────────────────────────────────

/// BLE remote presence/battery/RSSI, from `getStatusEx`'s `BleRemote*`
/// fields. All-`Copy` and read-only to the outside world, so unlike
/// `PlayerStatus`/`PlaybackState` (which hold owned `String`/`Rc` data and
/// need `.clone()`/`Rc` treatment to hand out cheaply) this is just returned
/// by value straight out of `Inner` — no cloning ceremony needed.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct RemoteInfo {
    /// `None` until the first `getStatusEx` result, or if the device has no
    /// BLE remote hardware at all (field absent from the response).
    pub connected: Option<bool>,
    /// Battery percentage.  `None` until first `getStatusEx`, or if no
    /// remote is connected.
    pub battery:   Option<u32>,
    /// Remote's own radio RSSI in dBm.  `None` until first `getStatusEx`, or
    /// if no remote is connected.
    pub rssi:      Option<i32>,
}

struct Inner {
    client:          Option<WiimClient>,
    device_info:     Option<DeviceInfo>,
    capabilities:    Option<DeviceCapabilities>,
    /// Raw wire-shaped responses, cached purely as a diffing baseline for
    /// next tick — the UI never reads these directly; see `playback` below.
    /// `player_status` in particular must stay read-only outside
    /// `process_poll()`'s `inner.player_status = Some(st)` assignment: an
    /// optimistic command write here would make the next real poll's diff
    /// see no change and silently skip updating `playback` to match.
    player_status:   Option<PlayerStatus>,
    metadata:        Option<MetaData>,
    /// UPnP `AVTransport` client, lazily discovered once any field group in
    /// `access` resolves to `AccessMethod::UpnpPolled` (see
    /// `ensure_upnp_client`/`recompute_access`). `None` until discovery
    /// succeeds; re-attempted on every `recompute_access()` call while still
    /// wanted and not yet obtained (no backoff/retry-limit — this is an
    /// opt-in diagnostic path, not the default connect flow).
    upnp_client:      Option<UpnpClient>,
    /// True while a `UpnpClient::discover()` attempt is in flight, so
    /// `ensure_upnp_client` doesn't fire a second concurrent discovery.
    upnp_discovery_in_flight: bool,
    /// Raw UPnP `GetInfoEx` response, cached purely as a diffing baseline
    /// for next tick — parallel to `player_status`/`metadata` above.
    upnp_info:        Option<upnp::InfoEx>,
    /// Canonical, backend-independent playback state — updated in place,
    /// field by field, by `process_poll()` rather than rebuilt and diffed
    /// wholesale every tick.
    playback:        PlaybackState,
    /// `false` on any tick where `process_poll_http()`/`process_poll_upnp()`
    /// skipped real content decode because `has_playable_content()` said
    /// no (idle, or Bluetooth not confirmed connected) — set once a
    /// tick successfully decodes real content again. Exists to force a
    /// full re-decode the instant `has_playable_content()` flips back to
    /// `false`, even if the underlying wire response happens not to have
    /// changed since the last real decode (e.g. `play_medium` stayed
    /// `"BLUETOOTH"` across a whole disconnect→reconnect cycle) — without
    /// this, the plain per-field diff against the raw response cache
    /// wouldn't detect anything to re-decode, and real values would never
    /// repopulate.
    has_content: bool,
    /// Backend selection for this device: capability-profile default with
    /// `access_override` applied on top, if set. Recomputed by
    /// `recompute_access()`.
    access:          AccessMethod,
    /// Access method override pushed in via `set_playback_access_override()`
    /// (from Settings' Advanced panel) — kept so `recompute_access()` can
    /// re-derive `access` when capabilities change without the caller
    /// needing to resupply it. `None` means "use the device profile's
    /// default".
    access_override: Option<AccessMethod>,
    /// Mute-specific counterpart to `access`/`access_override` — resolved
    /// and overridden the same way, but independently, since a device's
    /// best mute backend can differ from its best playback-state backend
    /// (iEAST AudioCast: UPnP for everything else, but `GetInfoEx` never
    /// carries `CurrentMute` on that family, so mute reads/writes go
    /// through `RenderingControl` specifically). Recomputed by
    /// `recompute_access()` alongside `access`.
    mute_access:      AccessMethod,
    /// Override pushed in via `set_mute_access_override()`, mirroring
    /// `access_override` exactly.
    mute_access_override: Option<AccessMethod>,
    /// Loop-mode (shuffle/repeat) write-path counterpart to `access`/
    /// `mute_access` — independent because HTTP `setPlayerCmd:loopmode:5`
    /// (shuffle + repeat-one) is confirmed silently ignored on at least the
    /// WiiM Mini (works fine on WiiM Ultra and the Audio Pro Addon C5), so
    /// this isn't a `playback_access`-style per-family axis — same
    /// "one global UpnpPolled default, per-device Settings override for
    /// the exception" shape as `mute_access`. Recomputed by
    /// `recompute_access()` alongside `access`/`mute_access`.
    loop_mode_access:      AccessMethod,
    /// Override pushed in via `set_loop_mode_access_override()`, mirroring
    /// `mute_access_override` exactly.
    loop_mode_access_override: Option<AccessMethod>,
    output_status:   Option<AudioOutputStatus>,
    mode_renames:    HashMap<String, String>,
    /// Raw wire `mode` value from the last poll; -1 = not known
    current_mode:    i32,
    /// `true` from the moment `switch_input()` fires until a poll either
    /// confirms the new mode (cleared normally, see `apply_mode_change()`'s
    /// caller) or `INPUT_CHANGE_TIMEOUT` elapses with no confirmation
    /// (cleared by the timeout path, which also reverts the UI to whatever
    /// `current_mode` still actually is — deliberately left untouched by
    /// `switch_input()` itself, see its doc comment for why).
    input_changing:      bool,
    /// When the in-flight `switch_input()` request was sent. `None` when
    /// `input_changing` is `false`.
    input_change_time:   Option<Instant>,
    connection_state: ConnectionState,
    /// A `maybe_self_reconnect()` probe is currently in flight. That path
    /// deliberately stays `Failed` while it probes (see its doc comment),
    /// so — unlike an externally-driven reconnect, which parks the state
    /// in `Connecting` — the state alone can't stop `do_poll()`'s `Failed`
    /// branch from dispatching a second probe on a later tick while the
    /// first is still waiting on its timeout. Cleared unconditionally when
    /// any `fetch_device_info()` completion runs.
    reconnect_in_flight: bool,
    /// The device state manager has two mode: `Full` and `Simple`. `Simple`
    /// is when just the device-list is displayed, `Full` is when the device
    /// window or setting window (or both) is/are displayed. We count the
    /// number of "full" clients to decide when to switch mode.
    full_clients: u32,
    /// Whether Simple-mode polling additionally fetches title/artist/
    /// artwork content, on top of the bare `getStatusEx` liveness/identity
    /// check it always does. Has no effect in `Full` mode (which always
    /// fetches everything regardless). Set via `configure_simple_mode()`,
    /// pushed explicitly whenever the "Song info in device list" setting
    /// changes rather than read lazily, so a toggle takes effect
    /// immediately on every already-tracked device.
    simple_mode_song_info: bool,
    /// Last known network connection type (0=ethernet, 2=wifi).
    /// `None` until first `getStatusEx` result arrives.
    netstat:          Option<u32>,
    /// Last known wifi RSSI in dBm.  `None` until first `getStatusEx` result.
    rssi:             Option<i32>,
    /// BLE remote presence/battery/RSSI, from the last `getStatusEx` result.
    remote:           RemoteInfo,
    /// Resolved preset slots (1–12), cached from the last successful fetch.
    presets:          Vec<PresetEntry>,
    /// Fingerprint of the last fetched preset list (used to skip re-fetches).
    preset_fp:        String,
    /// Consecutive `PresetFetchOutcome::Failed` (network/transport
    /// failure, not a confirmed-unsupported response) results for
    /// whichever backend `capabilities.preset_source()` currently names.
    /// Reset to 0 on any success/confirmed-unsupported/give-up — purely a
    /// short-lived retry counter, not part of the device's identity, so
    /// unlike `preset_source` it lives here rather than on
    /// `DeviceCapabilities` (see `PRESET_PROBE_FAIL_THRESHOLD`).
    preset_probe_failures: u32,
    /// Consecutive `ApiOutcome::Failed` results for `getSoundCardModeSupportList`/
    /// `getNewAudioOutputHardwareMode` respectively — same shape/reasoning
    /// as `preset_probe_failures` (see `OUTPUTS_PROBE_FAIL_THRESHOLD`/
    /// `OUTPUT_STATUS_PROBE_FAIL_THRESHOLD`). Reset to 0 on reconnect
    /// alongside `preset_probe_failures`.
    outputs_probe_failures:       u32,
    output_status_probe_failures: u32,
    /// Preset slots whose artwork still needs fetching (or re-fetching),
    /// keyed by slot rather than URL since display addresses slots, not
    /// URLs — `(url, attempts so far)`. Populated by
    /// `handle_slow_poll_presets()` for any slot whose `picurl` isn't
    /// already sitting in `presets` from the previous list; drained by
    /// `dispatch_pending_preset_art()` as fetches succeed or exhaust
    /// `PRESET_ART_MAX_ATTEMPTS`.
    pending_preset_art:  HashMap<usize, (String, u32)>,
    /// Slots with a fetch currently in flight, so a slow/throttled CDN
    /// request doesn't get redispatched again on every subsequent tick
    /// before it resolves.
    preset_art_inflight: HashSet<usize>,
    /// Pending volume level to send on the next 1s tick (-1 = none pending).
    target_volume:    i32,
    /// When the last volume API command was sent (None = never).
    last_volume_cmd:  Option<Instant>,
    /// When the last `SetMute` command was sent (None = never) — same
    /// settle-window role as `last_volume_cmd`, see `do_set_mute()`.
    last_mute_cmd:    Option<Instant>,
    /// When the current/most recent slow-poll cycle started (None = never;
    /// triggers a new cycle immediately).
    last_slow_poll:   Option<Instant>,
    /// When the last fast (status/metadata) poll was dispatched — either a
    /// regular 1s-tick poll or a `trigger_poll()` one-shot. Used to space
    /// `trigger_poll()`'s one-shot polls at least `POLL_SETTLE_DELAY` after
    /// whichever poll happened most recently, rather than always waiting a
    /// fixed delay from "now".
    last_poll:        Option<Instant>,
    /// `true` while a slow-poll cycle is actively rotating through phases
    /// (one per tick); `false` while idle between cycles.
    slow_poll_active: bool,
    /// The next phase to dispatch, while `slow_poll_active`.
    slow_poll_phase:  SlowPollPhase,
    /// `Some` from the moment `dispatch_fast_poll()` actually spawns a call
    /// until its result comes back through `process_poll()` (which clears
    /// it to `None`). Checked by `dispatch_fast_poll()` to skip dispatching
    /// a new one while the previous is still outstanding — without this,
    /// the once-a-second tick kept firing a fresh call every tick
    /// regardless of whether the last one had resolved, so a real unplug (a
    /// slow *timeout*, not an instant "connection refused") let several
    /// ticks' worth of calls pile up in flight before the first one even
    /// failed and triggered offline detection; all of those stragglers then
    /// had to individually time out afterward, looking like polling never
    /// actually stopped. Also lets `apply_disconnected()` `.abort()` a call
    /// that's still genuinely in flight the moment `Failed` is reached,
    /// instead of leaving it to time out on its own — see that function.
    /// Same reasoning `dispatch_slow_poll()`/`slow_poll_handle` follows for
    /// the other poll.
    fast_poll_handle: Option<tokio::task::JoinHandle<()>>,
    /// Slow-poll counterpart of `fast_poll_handle` — same reasoning,
    /// replaced when the next phase dispatches, cleared when that phase's
    /// `SlowPollResult` arrives in `start_slow_poll_processor()`.
    slow_poll_handle: Option<tokio::task::JoinHandle<()>>,
    /// `Simple`-mode counterpart of `last_slow_poll` — when the last
    /// `Simple`-mode poll was dispatched. Deliberately a separate field
    /// rather than reusing `last_slow_poll`/`slow_poll_active`/
    /// `slow_poll_phase`, which are all `Full`-mode's own rotation state
    /// and don't apply while in `Simple` mode at all.
    last_simple_poll: Option<Instant>,
    /// `Simple`-mode counterpart of `slow_poll_handle` — same in-flight-
    /// tracking reasoning, own field so a `Full`⇄`Simple` mode transition
    /// can't confuse the two.
    simple_poll_handle: Option<tokio::task::JoinHandle<()>>,
}

impl Default for Inner {
    fn default() -> Self {
        Self {
            client:          None,
            device_info:     None,
            capabilities:    None,
            player_status:   None,
            metadata:        None,
            upnp_client:      None,
            upnp_discovery_in_flight: false,
            upnp_info:        None,
            playback:        PlaybackState::default(),
            has_content:     false,
            access:          AccessMethod::Http,
            access_override: None,
            mute_access:      AccessMethod::UpnpPolled,
            mute_access_override: None,
            loop_mode_access:      AccessMethod::UpnpPolled,
            loop_mode_access_override: None,
            output_status:   None,
            mode_renames:    HashMap::new(),
            current_mode:    -1,
            input_changing:      false,
            input_change_time:   None,
            netstat:          None,
            rssi:             None,
            remote:           RemoteInfo::default(),
            connection_state: ConnectionState::Disconnected,
            reconnect_in_flight: false,
            full_clients:     0,
            simple_mode_song_info: false,
            presets:          Vec::new(),
            preset_fp:        String::new(),
            preset_probe_failures: 0,
            outputs_probe_failures:       0,
            output_status_probe_failures: 0,
            pending_preset_art:  HashMap::new(),
            preset_art_inflight: HashSet::new(),
            target_volume:    -1,
            last_volume_cmd:  None,
            last_mute_cmd:    None,
            last_slow_poll:   None,
            last_poll:        None,
            slow_poll_active: false,
            slow_poll_phase:  SlowPollPhase::FIRST,
            fast_poll_handle: None,
            slow_poll_handle: None,
            last_simple_poll: None,
            simple_poll_handle: None,
        }
    }
}

// ── GObject implementation ────────────────────────────────────────────────────

mod imp {
    use super::*;
    use glib::subclass::Signal;
    use std::sync::OnceLock;

    pub struct DeviceState {
        pub(super) inner:         RefCell<Inner>,
        /// IP the current `client` was built for — kept in its own `RefCell`
        /// rather than a field on `Inner` specifically so reading it (`ip()`,
        /// and every `dbg()` call, which prefixes its output with it) never
        /// conflicts with an already-active `inner.borrow()`/`borrow_mut()`
        /// elsewhere on the call stack; a same-RefCell double-borrow would
        /// panic, but a *different* RefCell never contends with `inner` no
        /// matter what's currently borrowed there. Used to detect when a
        /// fresh IP (e.g. from a DHCP lease change) actually differs from
        /// the one already connected, so `DeviceManager::update_ip()` can
        /// skip a no-op reconnect.
        pub(super) ip:            RefCell<String>,
        pub(super) rt:            std::cell::OnceCell<Arc<tokio::runtime::Runtime>>,
        pub(super) slow_poll_tx:  RefCell<Option<async_channel::Sender<SlowPollResult>>>,
        pub(super) poll_tx:       RefCell<Option<async_channel::Sender<PollData>>>,
        /// Set via `set_offline_callback()` by any external caller that
        /// wants to own connectivity recovery for this `DeviceState`
        /// itself, rather than letting `maybe_self_reconnect()` handle it —
        /// nothing in the normal app path registers one today
        /// (`device::discovery_manager` just reads `connection_state()`),
        /// so this is normally `None` and `report_failure()` falls through
        /// to mutating state locally.
        /// Kept outside `Inner` since a UUID-change reset (`*inner =
        /// Inner::default()`) shouldn't drop it.
        pub(super) offline_cb:    RefCell<Option<Rc<dyn Fn()>>>,
        /// This device's uuid — fixed at construction (`new()`) and never
        /// changed for the rest of this `DeviceState`'s life, full stop.
        /// **There is no such thing as "the uuid changed"**: if
        /// `getStatusEx` ever reports a different uuid than this one at
        /// the same IP, that's a *different device* now sitting at that
        /// address — this `DeviceState` must not attach itself to it (some
        /// other `DeviceState` may already own that uuid in
        /// `DeviceManager`'s registry). It just declares itself `Failed`
        /// (`fetch_device_info()`'s success handler) and stops, exactly
        /// like any other disconnect — `device::discovery_manager`'s own
        /// tracked `DeviceState` for *this* uuid, and `DeviceManager::update_ip()`
        /// for whichever `DeviceState` actually owns the uuid that showed
        /// up, handle the rest with existing machinery. `OnceCell` (not
        /// `RefCell`)
        /// specifically to make "never changes" a compile-time property,
        /// not just a convention — mirrors `rt`'s Default-construct-then-
        /// `new()`-sets-it-once pattern. Empty only for a `DeviceState`
        /// nothing has ever known the identity of at all (`--connect`/a
        /// fresh manual add by IP, where the uuid is genuinely unknown
        /// until the first successful connect — `DeviceManager` already
        /// treats these as second-class/undeduplicated, see its doc
        /// comment, so a permanently-empty stable uuid here is consistent
        /// with that, not a new gap). Lives outside `Inner` since a
        /// `Failed`/reconnect cycle must never touch it (unlike
        /// `Inner`, which a UUID-*mismatch* — see above — never even
        /// gets to reset, since that path returns before touching `Inner`
        /// at all now). Exists so callers that need a stable identity
        /// even while disconnected — `ui::settings`'s Advanced panel,
        /// notably — aren't stuck reading `device_info().uuid`, which is
        /// `None` for as long as the device is offline (see `pub fn
        /// uuid()`'s doc comment).
        pub(super) uuid: std::cell::OnceCell<String>,
    }

    impl Default for DeviceState {
        fn default() -> Self {
            Self {
                inner:         RefCell::new(Inner::default()),
                ip:            RefCell::new(String::new()),
                rt:            std::cell::OnceCell::new(),
                slow_poll_tx:  RefCell::new(None),
                poll_tx:       RefCell::new(None),
                uuid:          std::cell::OnceCell::new(),
                offline_cb:    RefCell::new(None),
            }
        }
    }

    #[glib::object_subclass]
    impl ObjectSubclass for DeviceState {
        const NAME: &'static str = "RustyWiimDeviceState";
        type Type = super::DeviceState;
    }

    impl ObjectImpl for DeviceState {
        fn dispose(&self) {
            let mut inner = self.inner.borrow_mut();
            let id = inner.device_info.as_ref()
                .map(|d| format!("{} ({})", d.device_name, d.ip_addr()))
                .unwrap_or_else(|| "unknown".to_string());
            // Nothing else will ever poll_tx/slow_tx.recv() this result
            // once we're being dropped — let any still-in-flight request
            // stop immediately instead of running to completion for no
            // reason (same reasoning as `apply_disconnected()`'s abort).
            if let Some(h) = inner.fast_poll_handle.take() { h.abort(); }
            if let Some(h) = inner.slow_poll_handle.take() { h.abort(); }
            if let Some(h) = inner.simple_poll_handle.take() { h.abort(); }
            drop(inner);
            dbg(&self.obj(), &format!("DeviceState dropped: {}", id));
        }

        fn signals() -> &'static [Signal] {
            static SIGNALS: OnceLock<Vec<Signal>> = OnceLock::new();
            SIGNALS.get_or_init(|| {
                vec![
                    Signal::builder("device-changed").build(),
                    Signal::builder("playback-changed")
                        .param_types([u32::static_type()])
                        .build(),
                    Signal::builder("input-changed").build(),
                    Signal::builder("inputs-changed").build(),
                    Signal::builder("output-changed").build(),
                    Signal::builder("outputs-changed").build(),
                    Signal::builder("network-changed").build(),
                    Signal::builder("remote-changed").build(),
                    Signal::builder("presets-changed").build(),
                ]
            })
        }
    }
}

glib::wrapper! {
    pub struct DeviceState(ObjectSubclass<imp::DeviceState>);
}

/// RAII handle for `Full`-mode polling, from `DeviceState::acquire_full()`.
/// Releases automatically on drop — hold one for as long as something (e.g.
/// an open device window) wants full detail.
pub struct FullModeGuard {
    ds: DeviceState,
}

impl Drop for FullModeGuard {
    fn drop(&mut self) {
        self.ds.release_full();
    }
}

// ── Public API ────────────────────────────────────────────────────────────────

impl DeviceState {
    /// `uuid` — this `DeviceState`'s identity for the rest of its life,
    /// fixed here and never changed afterward; empty only when genuinely
    /// unknown (a fresh `--connect`/manual add by IP). See
    /// `imp::DeviceState::uuid`'s doc comment for the full reasoning.
    pub fn new(rt: Arc<tokio::runtime::Runtime>, uuid: String) -> Self {
        let obj: Self = glib::Object::new();
        obj.imp().rt.set(rt).unwrap();
        obj.imp().uuid.set(uuid).unwrap();
        obj
    }

    // ── Connection ────────────────────────────────────────────────────────────

    /// Switch to a new device IP.  Clears all cached state, emits
    /// `device-changed` immediately (with cleared state so the UI can show
    /// "Connecting…"), then fetches device info asynchronously and emits
    /// `device-changed` again when the data arrives.
    ///
    /// No `expected_uuid` parameter anymore — identity verification is now
    /// unconditional and uses `uuid()` (fixed at construction) instead of a
    /// per-call opt-in: `fetch_device_info()`'s success handler checks
    /// `info.uuid` against it whenever `uuid()` is non-empty. See
    /// `imp::DeviceState::uuid`'s doc comment for why a mismatch there
    /// means "different device, don't attach" rather than "update our
    /// identity."
    ///
    /// `access_override`/`mute_access_override`/`loop_mode_access_override`
    /// are established here, up front — not via a separate, later call to
    /// `set_playback_access_override()`/`set_mute_access_override()`/
    /// `set_loop_mode_access_override()` that has to land before the first
    /// poll tick to matter. There's no window where this `DeviceState`
    /// exists with the wrong override, because there's no point at which it
    /// exists without one at all. Since this resets *everything*
    /// (`*inner = Inner::default()`), including whatever overrides an
    /// already-connected `DeviceState` had, a caller reconnecting an
    /// existing instance (`DeviceManager::update_ip()`) must read the
    /// current values first (`playback_access_override()`/
    /// `mute_access_override()`/`loop_mode_access_override()`) and pass them
    /// back in, not just fresh defaults.
    ///
    /// `connect_now` — when `false`, everything above still happens
    /// (`client`/`ip`/overrides configured, `device-changed` emitted) but
    /// `connection_state` is left `Disconnected` and no `fetch_device_info()`
    /// attempt is made. For `DeviceManager::get()` opening a window on a
    /// device devlist already believes is offline — attempting (and
    /// failing) a connection immediately, every single time, for a device
    /// already known to be unreachable is exactly the noisy behavior this
    /// whole connection-handling redesign exists to avoid; instead this
    /// `DeviceState` just sits configured-but-idle until
    /// `maybe_self_reconnect()` (or an external `mark_reachable()` call)
    /// actually attempts one.
    pub fn set_device(
        &self,
        ip: &str,
        tls: TlsMode,
        access_override: Option<AccessMethod>,
        mute_access_override: Option<AccessMethod>,
        loop_mode_access_override: Option<AccessMethod>,
        connect_now: bool,
    ) {
        // Apply --tls CLI override if set; otherwise use the caller-supplied mode.
        let tls = {
            let global = TlsMode::from_usize(TLS_MODE.load(Ordering::Relaxed));
            if global != TlsMode::Auto { global } else { tls }
        };
        *self.imp().ip.borrow_mut() = ip.to_string();
        {
            let mut inner = self.imp().inner.borrow_mut();
            *inner = Inner::default();
            inner.client           = Some(WiimClient::new(ip, tls));
            if connect_now { inner.connection_state = ConnectionState::Connecting; }
            inner.access_override  = access_override;
            inner.mute_access_override = mute_access_override;
            inner.loop_mode_access_override = loop_mode_access_override;
        }
        // Now that `self.imp().ip` above is actually set to the new value,
        // dbg()'s own ds.ip() prefix reflects it correctly (it's a separate
        // RefCell from `inner`, so setting it isn't ordering-sensitive
        // relative to the block above — it's set first here only to match
        // reading order).
        dbg(self, &format!(
            "set_device: configuring {ip} ({}), connect_now={connect_now}",
            tls.description(),
        ));
        self.recompute_access();
        dbg(self, &format!("signal: device-changed ({})", if connect_now { "connecting" } else { "configured" }));
        self.emit_by_name::<()>("device-changed", &[]);
        if connect_now {
            self.fetch_device_info();
        }
    }

    fn fetch_device_info(&self) {
        let client = match self.imp().inner.borrow().client.clone() {
            Some(c) => c,
            None    => return,
        };
        let rt = self.rt();
        struct FetchOk {
            info:    DeviceInfo,
            caps:    DeviceCapabilities,
            renames: HashMap<String, String>,
        }
        let (tx, rx) = async_channel::bounded::<Option<FetchOk>>(1);

        rt.spawn(async move {
            // `capabilities::detect_capabilities()` owns fetching getStatusEx
            // *and* whatever probing is needed to resolve the rest of
            // `DeviceCapabilities` (currently: getSoundCardModeSupportList
            // for outputs, getAudioInputEnable for inputs) — this function
            // doesn't try anything itself or interpret a failure; it only
            // reads the result.
            //
            // Deliberately NOT calling get_audio_output() here — that used
            // to duplicate the slow-poll's own OutputStatus phase, which
            // fires within the first few ticks anyway (fire_slow_poll()
            // starts the rotation immediately on connect). inner.output_status
            // just stays None (Inner::default()'s value) until that first
            // slow-poll tick arrives; populate_output() grays out the output
            // dropdown while it's None rather than showing a guess.
            let payload = match capabilities::detect_capabilities(&client).await {
                Some((info, caps)) => {
                    let renames = client.get_mode_rename().await;
                    Some(FetchOk { info, caps, renames })
                }
                None => {
                    eprintln!("[state] fetch_device_info failed: getStatusEx unreachable");
                    None
                }
            };
            let _ = tx.send(payload).await;
        });

        let ds = self.downgrade();
        glib::spawn_future_local(async move {
            let payload = rx.recv().await.ok().flatten();
            let Some(ds) = ds.upgrade() else { return };
            // Whatever kicked this attempt off, it has now resolved — clear
            // unconditionally (a no-op for attempts that weren't
            // maybe_self_reconnect()'s) before any of the outcome handling.
            ds.imp().inner.borrow_mut().reconnect_in_flight = false;

            let Some(FetchOk { info, caps, renames }) = payload else {
                ds.report_failure("fetch_device_info: getStatusEx unreachable");
                return;
            };
            // Identity check against our fixed uuid() (see
            // `imp::DeviceState::uuid`'s doc comment) — skipped only when
            // it's genuinely unknown (a fresh `--connect`/manual add,
            // where anything answering is accepted as-is). A mismatch
            // means a *different* device now sits at this IP; treat it
            // exactly like any other disconnect — this `DeviceState` must
            // not attach itself to it (something else may already own
            // that uuid). Recovering the actual device this `DeviceState`
            // is for, if it reappears elsewhere, is `DeviceManager::
            // update_ip()`'s job (driven by `device::discovery_manager`
            // noticing a moved IP via SSDP) — every tracked `DeviceState`
            // already gets this same identity check on its own connection
            // attempts, so there's no separate identity-mismatch handling
            // the picker-list backend itself needs to duplicate anymore.
            let known_uuid = ds.uuid();
            if !known_uuid.is_empty() && info.uuid != known_uuid {
                ds.report_failure(&format!(
                    "device identity mismatch at this IP (expected {known_uuid}, got {})",
                    info.uuid,
                ));
                return;
            }
            dbg(&ds, &format!(
                "device info: model=\"{}\" vendor={} fw={} project={} inputs={}",
                caps.model,
                caps.vendor.display_name(),
                info.firmware,
                info.project,
                caps.inputs.len(),
            ));
            {
                let mut inner = ds.imp().inner.borrow_mut();
                inner.netstat           = info.netstat.parse().ok();
                inner.rssi              = info.rssi.parse().ok();
                inner.remote = RemoteInfo {
                    connected: parse_remote_connected(&info.ble_remote_connected),
                    battery:   info.ble_remote_battery.parse().ok(),
                    rssi:      info.ble_remote_rssi.parse().ok(),
                };
                inner.capabilities      = Some(caps);
                inner.device_info       = Some(info);
                // output_status is left None (Inner::default()) — the
                // dropdown starts greyed out and the first slow-poll
                // OutputStatus tick fills it in; see the comment above.
                inner.mode_renames      = renames;
                // Reset preset data so the first slow-poll cycle re-fetches from scratch.
                inner.preset_fp         = String::new();
                inner.preset_probe_failures = 0;
                inner.outputs_probe_failures       = 0;
                inner.output_status_probe_failures = 0;
                inner.presets           = Vec::new();
                inner.pending_preset_art.clear();
                inner.preset_art_inflight.clear();
                inner.connection_state  = ConnectionState::Connected;
            }
            ds.recompute_access();
            dbg(&ds, "signal: device-changed (ready)");
            ds.emit_by_name::<()>("device-changed", &[]);
            // Kick off the slow-poll rotation on the very next 1s tick,
            // instead of waiting a full SLOW_POLL_INTERVAL, so presets/
            // outputs/network status appear promptly after connecting.
            ds.fire_slow_poll();
        });
    }

    /// Prime the slow-poll rotation (see `SlowPollPhase`) to start on the
    /// very next 1-second tick, instead of waiting for `SLOW_POLL_INTERVAL`
    /// to elapse. Only sets state; the unified timer does the actual
    /// dispatching, one phase per tick, same as any other cycle.
    fn fire_slow_poll(&self) {
        let mut inner = self.imp().inner.borrow_mut();
        if inner.client.is_none() { return; }
        inner.slow_poll_active = true;
        inner.slow_poll_phase  = SlowPollPhase::FIRST;
        inner.last_slow_poll   = Some(Instant::now());
    }

    // ── Playback access-method configuration ─────────────────────────────────

    /// Recompute the effective `AccessMethod` from this device's capability
    /// profile plus whatever override is currently stored (see
    /// `access_override`). Called whenever either input changes: after
    /// capabilities are (re)detected, and from `set_playback_access_override`.
    ///
    /// Also recomputes `mute_access`/`loop_mode_access` alongside `access`.
    /// Unlike `access`, neither of those is sourced from a per-`FamilyProfile`
    /// field — each is a real-hardware-confirmed exception on top of one
    /// global `AccessMethod::UpnpPolled` default, with the per-device
    /// Settings override covering it, rather than a family-level capability
    /// axis (mute: iEAST AudioCast's `GetInfoEx` never carries `CurrentMute`;
    /// loop mode: HTTP `setPlayerCmd:loopmode:5` confirmed silently ignored
    /// on at least the WiiM Mini).
    fn recompute_access(&self) {
        // `became_upnp` is read back and logged *after* the borrow below
        // ends — dbg() does its own fresh borrow of this same RefCell (to
        // read the ip for its prefix), which would panic if called while
        // still inside the `borrow_mut()` scope here.
        let (wants_upnp, became_upnp) = {
            let mut inner = self.imp().inner.borrow_mut();
            let base = inner.capabilities.as_ref()
                .map(|c| c.playback_access())
                .unwrap_or(AccessMethod::Http);
            let prev_access = inner.access;
            inner.access = inner.access_override.unwrap_or(base);
            inner.mute_access = inner.mute_access_override.unwrap_or(AccessMethod::UpnpPolled);
            inner.loop_mode_access = inner.loop_mode_access_override.unwrap_or(AccessMethod::UpnpPolled);
            // Debug-only visibility aid for diagnosing a device where UPnP
            // discovery/`GetInfoEx` never succeeds (playback state silently
            // stays on whatever it last held, since the poll loop only
            // overwrites it when a `GetInfoEx` response actually arrives).
            // Only logged on an actual transition — this fn runs twice per
            // connect (before and after capabilities are known), and would
            // otherwise print the same line twice whenever both resolve to
            // the same access method.
            let became_upnp = inner.access == AccessMethod::UpnpPolled && prev_access != AccessMethod::UpnpPolled;
            let wants_upnp = inner.access == AccessMethod::UpnpPolled
                || inner.mute_access == AccessMethod::UpnpPolled
                || inner.loop_mode_access == AccessMethod::UpnpPolled;
            (wants_upnp, became_upnp)
        };
        if became_upnp {
            dbg(self, "access config: set to UpnpPolled");
        }
        if wants_upnp {
            self.ensure_upnp_client();
        }
    }

    /// Kick off `UpnpClient::discover()` if `access` currently wants
    /// `AccessMethod::UpnpPolled` and we don't have a client yet (and one
    /// isn't already in flight). Fire-and-forget, using the same
    /// spawn-on-`rt()`-then-channel-back-to-GTK-thread bridge as every
    /// other async operation in this file (see `start_art_loader` for the
    /// closest parallel) — a fresh one-shot channel per attempt, since
    /// discovery is rare enough not to need a long-lived processor task.
    fn ensure_upnp_client(&self) {
        {
            let inner = self.imp().inner.borrow();
            if inner.upnp_client.is_some() || inner.upnp_discovery_in_flight {
                return;
            }
        }
        let ip = self.ip();
        if ip.is_empty() {
            return;
        }
        self.imp().inner.borrow_mut().upnp_discovery_in_flight = true;
        dbg(self, "upnp: starting control-URL discovery");

        let (tx, rx) = async_channel::bounded(1);
        self.rt().spawn(async move {
            let result = UpnpClient::discover(&ip).await;
            let _ = tx.send(result).await;
        });

        let ds = self.downgrade();
        glib::spawn_future_local(async move {
            let Ok(result) = rx.recv().await else { return };
            let Some(ds) = ds.upgrade() else { return };
            // `outcome` read back and logged *after* the borrow ends — see
            // `recompute_access()`'s comment for why.
            let outcome = {
                let mut inner = ds.imp().inner.borrow_mut();
                inner.upnp_discovery_in_flight = false;
                match result {
                    Ok(client) => { inner.upnp_client = Some(client); Ok(()) }
                    Err(e) => Err(e),
                }
            };
            match outcome {
                Ok(()) => dbg(&ds, "upnp: discovery succeeded"),
                Err(e) => dbg(&ds, &format!("upnp: discovery failed: {e}")),
            }
        });
    }

    /// Push a field-diagnostics override (from Settings' "Device -> Advanced"
    /// panel, sourced from `config::DeviceConfig::playback_access_override`)
    /// in and recompute the effective access config immediately, so a change
    /// takes effect on the next poll tick without reconnecting. For the
    /// *initial* value, prefer passing it to `set_device()` directly instead
    /// — this method remains for live changes to an already-connected device.
    pub fn set_playback_access_override(&self, over: Option<AccessMethod>) {
        self.imp().inner.borrow_mut().access_override = over;
        self.recompute_access();
    }

    /// Current access-method override, as last established by `set_device()`
    /// or `set_playback_access_override()`. Read by
    /// `DeviceManager::update_ip()` so reconnecting to a new IP (device
    /// moved) doesn't lose it — `set_device()`'s full state reset would
    /// otherwise wipe it back to `None`.
    pub fn playback_access_override(&self) -> Option<AccessMethod> {
        self.imp().inner.borrow().access_override
    }

    /// Mute-specific counterpart to `set_playback_access_override()` — same
    /// semantics, independent field. See `Inner::mute_access`'s doc comment
    /// for why this exists as a second override rather than folding into
    /// the playback one.
    pub fn set_mute_access_override(&self, over: Option<AccessMethod>) {
        self.imp().inner.borrow_mut().mute_access_override = over;
        self.recompute_access();
    }

    /// Current mute-access override, as last established by `set_device()`
    /// or `set_mute_access_override()`. Read by `DeviceManager::update_ip()`
    /// so reconnecting to a new IP doesn't lose it, mirroring
    /// `playback_access_override()`.
    pub fn mute_access_override(&self) -> Option<AccessMethod> {
        self.imp().inner.borrow().mute_access_override
    }

    /// Loop-mode-specific counterpart to `set_playback_access_override()` —
    /// same semantics, independent field. See `Inner::loop_mode_access`'s
    /// doc comment for why this exists as its own override.
    pub fn set_loop_mode_access_override(&self, over: Option<AccessMethod>) {
        self.imp().inner.borrow_mut().loop_mode_access_override = over;
        self.recompute_access();
    }

    /// Current loop-mode-access override, as last established by
    /// `set_device()` or `set_loop_mode_access_override()`. Read by
    /// `DeviceManager::update_ip()` so reconnecting to a new IP doesn't lose
    /// it, mirroring `playback_access_override()`/`mute_access_override()`.
    pub fn loop_mode_access_override(&self) -> Option<AccessMethod> {
        self.imp().inner.borrow().loop_mode_access_override
    }

    // ── Polling ───────────────────────────────────────────────────────────────

    /// Start the unified 1-second timer plus background result processors.
    /// The timer handles fast polls every tick, one slow-poll phase per tick
    /// during a rotation started every SLOW_POLL_INTERVAL (see
    /// `SlowPollPhase`), pending volume commands, and reconnection attempts.
    /// Call once after `new()`.
    pub fn start_polling(&self) {
        let (poll_tx, poll_rx) = async_channel::unbounded::<PollData>();
        let (slow_tx, slow_rx) = async_channel::unbounded::<SlowPollResult>();
        let (art_tx,  art_rx)  = async_channel::unbounded::<(String, Vec<u8>)>();
        let (simple_tx, simple_rx) = async_channel::unbounded::<Option<DeviceInfo>>();

        *self.imp().slow_poll_tx.borrow_mut() = Some(slow_tx.clone());

        self.start_unified_timer(poll_tx, slow_tx, simple_tx);
        self.start_poll_processor(poll_rx, art_tx);
        self.start_art_loader(art_rx);
        self.start_slow_poll_processor(slow_rx);
        self.start_simple_poll_processor(simple_rx);
    }

    fn start_unified_timer(
        &self,
        poll_tx: async_channel::Sender<PollData>,
        slow_tx: async_channel::Sender<SlowPollResult>,
        simple_tx: async_channel::Sender<Option<DeviceInfo>>,
    ) {
        *self.imp().poll_tx.borrow_mut() = Some(poll_tx.clone());
        let ds_weak = self.downgrade();
        glib::timeout_add_local(Duration::from_secs(1), move || {
            let Some(ds) = ds_weak.upgrade() else { return glib::ControlFlow::Break };
            ds.do_poll(&poll_tx, &slow_tx, &simple_tx)
        });
    }

    /// Fires once per second, but a genuine no-op tick unless Connected —
    /// see the early return below and `ConnectionState::Failed`'s doc
    /// comment for why `Failed` doesn't poll either. Reads everything this
    /// tick needs to decide from `Inner` in one borrow — several
    /// interrelated pieces of state that don't split cleanly without just
    /// moving the borrow-juggling into another function's parameter list —
    /// then, once the borrow is dropped, hands off to a focused helper per
    /// action: the fast poll, one slow-poll phase.
    ///
    /// `Simple` mode (`full_clients == 0`) branches off before any of
    /// `Full` mode's fast/slow-poll dispatch logic below runs — its own
    /// `do_simple_poll()` is a separate, much smaller poll loop, not a
    /// variation of this one.
    fn do_poll(
        &self,
        poll_tx: &async_channel::Sender<PollData>,
        slow_tx: &async_channel::Sender<SlowPollResult>,
        simple_tx: &async_channel::Sender<Option<DeviceInfo>>,
    ) -> glib::ControlFlow {
        let mut inner = self.imp().inner.borrow_mut();
        let state = inner.connection_state;

        // Only an actually-Connected device polls at all. `Disconnected`/
        // `Connecting` never had a live connection to poll yet; `Failed`
        // (displayed "Disconnected") deliberately stops polling too — see
        // `ConnectionState::Failed`'s doc comment. Reconnection there is
        // always externally driven (`mark_reachable()`) *when something is
        // actually watching* — `maybe_self_reconnect()` is the fallback for
        // when nothing is (see its doc comment).
        if state != ConnectionState::Connected {
            if state == ConnectionState::Failed {
                drop(inner);
                self.maybe_self_reconnect();
            }
            return glib::ControlFlow::Continue;
        }

        let now = Instant::now();

        if inner.full_clients == 0 {
            let dispatched = self.do_simple_poll(&mut inner, now, simple_tx);
            // Only piggyback the song-info fetch onto the *same* tick
            // `do_simple_poll()` actually dispatched on — not every tick —
            // or this would run at 1s cadence instead of
            // `SIMPLE_POLL_INTERVAL`, defeating the point of `Simple` mode
            // being cheaper. Reuses `dispatch_fast_poll()` (and its
            // existing `fast_poll_handle` in-flight guard/`process_poll()`
            // decode pipeline) unchanged rather than a second
            // implementation — same content `Full` mode's fast poll
            // fetches, just gated to a slower cadence here.
            let want_song_info = dispatched && inner.simple_mode_song_info;
            drop(inner);
            if want_song_info {
                self.dispatch_fast_poll();
            }
            return glib::ControlFlow::Continue;
        }

        // Is it time to start a new slow-poll cycle?
        let cycle_due = inner.last_slow_poll
            .map_or(true, |t| now.duration_since(t) >= SLOW_POLL_INTERVAL);

        // Flush any pending volume command if the debounce window has elapsed.
        let pending_vol = if inner.target_volume >= 0
            && inner.last_volume_cmd
                .map_or(true, |t| now.duration_since(t) >= VOLUME_DEBOUNCE)
        {
            let v = inner.target_volume as u32;
            inner.target_volume   = -1;
            inner.last_volume_cmd = Some(now);
            Some(v)
        } else {
            None
        };

        // Same "don't pile on top of a still-outstanding call" reasoning as
        // `fast_poll_handle` (see its doc comment) — don't even advance
        // the rotation while the previous phase's call hasn't resolved yet,
        // so it retries the same phase once clear rather than skipping it.
        let dispatch_phase = if inner.slow_poll_handle.is_some() {
            None
        } else {
            self.advance_slow_poll_rotation(&mut inner, state, cycle_due, now)
        };

        let client        = inner.client.clone();
        // `probes_outputs`/`preset_source` are read straight off
        // `capabilities` (set by `capabilities::detect_capabilities()`/
        // persisted there for the connection's lifetime); `preset_probe_failures`
        // is a short-lived retry counter that isn't part of the device's
        // identity, so it lives directly on `Inner` instead (see its doc
        // comment) — `capabilities.rs` only ever records `preset_source`.
        let probe_outputs = inner.capabilities.as_ref().is_some_and(|c| c.probes_outputs);
        let probe_output_status = inner.capabilities.as_ref().is_some_and(|c| c.probes_output_status);
        let preset_source = inner.capabilities.as_ref()
            .map(|c| c.preset_source())
            .unwrap_or(capabilities::PresetSource::Unknown);
        let preset_probe_failures = inner.preset_probe_failures;
        let preset_fp     = inner.preset_fp.clone();
        let upnp_client   = inner.upnp_client.clone();
        drop(inner);

        let Some(client) = client else { return glib::ControlFlow::Continue };

        // Send any deferred volume command first.
        if let Some(vol) = pending_vol {
            let cv = client.clone();
            self.rt().spawn(async move { let _ = cv.set_volume(vol).await; });
        }

        self.dispatch_fast_poll();
        self.dispatch_slow_poll(
            &client, slow_tx, dispatch_phase, probe_outputs, probe_output_status,
            preset_source, preset_probe_failures, preset_fp, upnp_client,
        );
        self.dispatch_pending_preset_art(&client, poll_tx);

        glib::ControlFlow::Continue
    }

    /// `Simple`-mode poll dispatch — one reduced-cadence loop
    /// (`SIMPLE_POLL_INTERVAL`) instead of `Full` mode's separate fast +
    /// slow timers, and no volume/preset-art dispatch at all (nothing to
    /// drive those without an open window). Always fetches `getStatusEx` —
    /// that alone *is* this device's liveness/identity check now, handled
    /// by the same `handle_slow_poll_device_info()` `Full` mode's
    /// `SlowPollPhase::DeviceInfo` already uses (see
    /// `start_simple_poll_processor()`), not a separate implementation.
    ///
    /// Returns whether this tick actually dispatched (vs. skipped: not due
    /// yet, or a previous call still in flight) — `do_poll()`'s caller uses
    /// this to piggyback the optional song-info fetch
    /// (`Inner::simple_mode_song_info`) onto the exact same cadence, rather
    /// than every 1s tick.
    fn do_simple_poll(
        &self,
        inner: &mut Inner,
        now: Instant,
        simple_tx: &async_channel::Sender<Option<DeviceInfo>>,
    ) -> bool {
        let due = inner.last_simple_poll
            .map_or(true, |t| now.duration_since(t) >= SIMPLE_POLL_INTERVAL);
        // Same "don't pile on top of a still-outstanding call" reasoning as
        // `fast_poll_handle`/`slow_poll_handle` (see their doc comments).
        if !due || inner.simple_poll_handle.is_some() {
            return false;
        }
        let Some(client) = inner.client.clone() else { return false };
        inner.last_simple_poll = Some(now);
        let tx = simple_tx.clone();
        let handle = self.rt().spawn(async move {
            let info = client.get_device_info().await.ok();
            let _ = tx.send(info).await;
        });
        inner.simple_poll_handle = Some(handle);
        true
    }

    /// Advances the slow-poll rotation (see `SlowPollPhase`), starting a
    /// new cycle if one is due, and returns this tick's phase to run, if
    /// any. Takes `&mut Inner` directly rather than re-borrowing —
    /// `do_poll()` already holds the borrow this needs to read/mutate
    /// (`slow_poll_active`/`slow_poll_phase`/`last_slow_poll`).
    fn advance_slow_poll_rotation(
        &self,
        inner:     &mut Inner,
        state:     ConnectionState,
        cycle_due: bool,
        now:       Instant,
    ) -> Option<SlowPollPhase> {
        if state != ConnectionState::Connected {
            return None;
        }
        if !inner.slow_poll_active && cycle_due {
            inner.slow_poll_active = true;
            inner.slow_poll_phase  = SlowPollPhase::FIRST;
            inner.last_slow_poll   = Some(now);
            let device_id = inner.device_info.as_ref()
                .map(|d| format!("{} ({})", d.device_name, d.ip_addr()))
                .unwrap_or_else(|| "unknown".to_string());
            dbg(self, &format!(
                "slow poll: starting new cycle (refcount={} device={device_id})",
                self.ref_count(),
            ));
        }
        if inner.slow_poll_active {
            let phase = inner.slow_poll_phase;
            let next  = phase.next();
            inner.slow_poll_phase = next;
            if next == SlowPollPhase::FIRST {
                // Rotation complete; go idle until the next cycle_due.
                inner.slow_poll_active = false;
            }
            Some(phase)
        } else {
            None
        }
    }

    // ── External connectivity control ─────────────────────────────────────────
    //
    // `DeviceState` normally manages its own connectivity end to end:
    // `apply_disconnected()` is the *only* place `connection_state` is ever
    // locally set to `Failed` (reached via `report_failure()`, for a poll
    // failure this `DeviceState` notices itself), and `maybe_self_reconnect()`
    // is what brings it back. `mark_offline()`/`mark_reachable()` plus
    // `set_offline_callback()` below exist for a caller that wants to *own*
    // that lifecycle externally instead — nothing in the normal app path
    // does this today (`device::discovery_manager` just reads
    // `connection_state()`), but the mechanism stays available: a
    // registered offline callback makes
    // `report_failure()` call it (round-tripping back into `mark_offline()`,
    // same call stack, no lag) instead of mutating state locally, and once
    // one's registered `maybe_self_reconnect()` steps aside entirely in
    // favor of that caller eventually calling `mark_reachable()`.

    /// Registers the callback `report_failure()` invokes when *this*
    /// `DeviceState` notices a poll failure itself (as opposed to being
    /// told about a failure externally via `mark_offline()`, which does not
    /// invoke it — seeing this callback fire always means an external
    /// owner finding out about a failure it didn't already know about).
    /// Takes no uuid/identity — the caller already knows which
    /// `DeviceState` this is and closes over whatever identity its own
    /// callback needs. Overwrites any previously-registered callback; only
    /// ever set once per `DeviceState` in practice, by a caller that wants
    /// to own this device's connectivity lifecycle externally (see this
    /// section's own doc comment — nothing in the normal app path does
    /// this today).
    pub fn set_offline_callback(&self, cb: impl Fn() + 'static) {
        *self.imp().offline_cb.borrow_mut() = Some(Rc::new(cb));
    }

    /// Told externally that this device is offline — for a caller that's
    /// registered an offline callback (`set_offline_callback()`) and wants
    /// to own recovery itself; this is the round-trip tail end of this same
    /// `DeviceState`'s own `report_failure()` call reaching that caller and
    /// bouncing straight back. Nothing in the normal app path calls this
    /// externally anymore (`device::discovery_manager` just reads
    /// `connection_state()` and lets `maybe_self_reconnect()` handle
    /// recovery) — kept as public API for a caller that wants this
    /// control. No-op unless currently
    /// `Connected` — deliberately *not* `Connecting` too: a fresh reconnect
    /// attempt (e.g. a window just (re)opened) gets to run to completion on
    /// its own merits rather than being preempted by stale presence from
    /// before the attempt even started — if it really is still down,
    /// `fetch_device_info()` will fail and reach `Failed` on its own within
    /// one round trip anyway.
    pub fn mark_offline(&self) {
        let connected = self.imp().inner.borrow().connection_state == ConnectionState::Connected;
        if !connected { return; }
        self.apply_disconnected("told externally");
    }

    /// Told externally that a plain `getStatusEx` against this device just
    /// succeeded again. No-op unless currently `Failed` or `Disconnected`
    /// — reconnecting from any other state doesn't make sense (already
    /// connected, or a first connection attempt already in flight).
    /// `Disconnected` is included alongside `Failed` for
    /// `set_device(..., connect_now: false)`'s case: a window opened on a
    /// device already believed offline sits configured-but-`Disconnected`
    /// (never having attempted a connect at all) until this fires. Re-runs
    /// the full `fetch_device_info()` path (capability detection, not just
    /// a bare liveness check) — same as any other (re)connect.
    pub fn mark_reachable(&self) {
        let can_reconnect = matches!(
            self.imp().inner.borrow().connection_state,
            ConnectionState::Failed | ConnectionState::Disconnected,
        );
        if !can_reconnect { return; }
        dbg(self, "connection: told reachable externally (devlist health check); reconnecting");
        self.imp().inner.borrow_mut().connection_state = ConnectionState::Connecting;
        self.emit_by_name::<()>("device-changed", &[]);
        self.fetch_device_info();
    }

    /// Self-driven periodic retry for a `Failed` `DeviceState` — the normal
    /// path today, since nothing in the normal app flow registers an
    /// offline callback anymore (`device::discovery_manager` reads
    /// `connection_state()` directly rather than owning recovery
    /// externally). No-op when a
    /// callback *is* registered (some external caller has opted into
    /// owning recovery itself instead — see `set_offline_callback()`'s doc
    /// comment) — that caller is expected to call `mark_reachable()` when
    /// it decides to retry. Same `SLOW_POLL_INTERVAL` cadence as ordinary
    /// slow polling, reusing `last_slow_poll` for it (untouched while
    /// `Failed`, so safe to repurpose) — no-op, cheaply, on every other tick.
    ///
    /// Deliberately **silent**: stays `Failed` and emits nothing while the
    /// probe runs. An earlier version flipped to `Connecting` (+
    /// `device-changed`) per attempt, which made an offline device's window
    /// oscillate "Disconnected" → spinner → "Disconnected" every 10s,
    /// indefinitely. `Connecting` should only ever show for an attempt with
    /// some sign of life behind it (a first/explicit connect, or
    /// `mark_reachable()` — devlist actually got an answer); a blind
    /// background retry isn't news until it *succeeds*, at which point
    /// `fetch_device_info()`'s completion emits `device-changed` with the
    /// state jumping straight to `Connected`. `reconnect_in_flight` (see
    /// its doc comment) is what now prevents a second dispatch while a
    /// probe is still waiting on its timeout — previously the `Connecting`
    /// state itself did that as a side effect, via `do_poll()`'s
    /// not-`Failed` check.
    fn maybe_self_reconnect(&self) {
        if self.imp().offline_cb.borrow().is_some() {
            return; // An external caller owns recovery for this one.
        }
        let (due, client) = {
            let mut inner = self.imp().inner.borrow_mut();
            if inner.reconnect_in_flight { return; }
            let due = inner.last_slow_poll
                .map_or(true, |t| Instant::now().duration_since(t) >= SLOW_POLL_INTERVAL);
            if due { inner.last_slow_poll = Some(Instant::now()); }
            (due, inner.client.clone())
        };
        if due && client.is_some() {
            dbg(self, "connection: no external health check registered; probing (silently, staying Failed)");
            self.imp().inner.borrow_mut().reconnect_in_flight = true;
            self.fetch_device_info();
        }
    }

    /// This `DeviceState` noticed a poll failure itself (`fetch_device_info()`
    /// unreachable/identity-mismatch, a fast-poll tick, or a slow-poll
    /// `getStatusEx`) — no threshold/counter anymore (a failure reaching
    /// this point already survived `cmd()`/`soap_call()`'s own internal
    /// retry, so a single one is already a strong signal; stacking more
    /// tolerance on top just delayed detection and multiplied log volume
    /// for a genuine disconnect, without actually improving flakiness
    /// tolerance — that's `cmd()`'s job already).
    ///
    /// Mutates local state directly (`apply_disconnected()`) unless an
    /// external caller has registered an offline callback (`offline_cb`
    /// — see "External connectivity control" above), in which case it
    /// calls that instead and lets the resulting round trip (that caller
    /// marking itself offline, then calling straight back into
    /// `mark_offline()`) perform the actual transition.
    fn report_failure(&self, reason: &str) {
        if let Some(cb) = self.imp().offline_cb.borrow().clone() {
            cb();
        } else {
            self.apply_disconnected(reason);
        }
    }

    /// The *only* place `connection_state` is ever locally set to `Failed`
    /// — reached either directly from `mark_offline()` (devlist told us) or
    /// from `report_failure()`'s no-one-watching fallback. No-op if already
    /// `Failed`/`Disconnected`.
    fn apply_disconnected(&self, reason: &str) {
        let transitioned = {
            let mut inner = self.imp().inner.borrow_mut();
            if matches!(inner.connection_state, ConnectionState::Failed | ConnectionState::Disconnected) {
                false
            } else {
                dbg(self, &format!("connection: {:?} → Failed ({reason})", inner.connection_state));
                inner.connection_state = ConnectionState::Failed;
                inner.device_info      = None;
                // Whichever poll triggered this transition already
                // resolved (process_poll()/start_slow_poll_processor()
                // clear the corresponding handle before this ever runs) —
                // but the *other* one may still be genuinely in flight
                // right now (e.g. slow poll detected the failure while an
                // independent fast-poll call was still waiting on its own
                // timeout). Cut it short immediately instead of letting it
                // run out its full timeout/retry chain for no reason.
                if let Some(h) = inner.fast_poll_handle.take() { h.abort(); }
                if let Some(h) = inner.slow_poll_handle.take() { h.abort(); }
                if let Some(h) = inner.simple_poll_handle.take() { h.abort(); }
                true
            }
        };
        if transitioned {
            dbg(self, "signal: device-changed (failed)");
            self.emit_by_name::<()>("device-changed", &[]);
        }
    }

    /// Fast-poll counterpart of `handle_slow_poll_device_info()`'s failure
    /// handling — factored out so `process_poll_http`/`process_poll_upnp`
    /// share the one policy. A no-op outside `Connected` (nothing to
    /// escalate from) — `dispatch_fast_poll()` doesn't fire outside
    /// `Connected` anyway, but this stays defensive rather than assuming
    /// that.
    fn note_fast_poll_result(&self, failed: bool) {
        if !failed { return; }
        if self.imp().inner.borrow().connection_state != ConnectionState::Connected {
            return;
        }
        self.report_failure("fast poll: getPlayerStatusEx/GetInfoEx failed");
    }

    /// Fast poll — exactly one of HTTP (`getPlayerStatusEx`+`getMetaInfo`)
    /// or UPnP (`GetInfoEx`) per tick, decided by `access`, never both: a
    /// device on `AccessMethod::Http` only ever hits HTTP; a device on
    /// `UpnpPolled` only ever hits UPnP, once a `UpnpClient` has actually
    /// been discovered. **Deliberately no HTTP fallback** when `UpnpPolled`
    /// is selected but discovery hasn't succeeded yet — this tick is
    /// skipped entirely (playback state stays stale until a client shows
    /// up) rather than silently substituting HTTP, which would contradict
    /// the point of the choice. (An HTTP-fallback mode was considered and
    /// deferred as unnecessary complexity for what's currently an opt-in
    /// diagnostic path, not the default.)
    ///
    /// Takes no parameters — fetches its own `client`/`poll_tx` (a couple
    /// of cheap `Option` clones off two `RefCell`s) rather than requiring
    /// the caller to already have them in hand, so both regular call sites
    /// can share this one function outright: `do_poll()`'s every-tick call
    /// (which happens to have `client` in scope anyway, for the slow-poll/
    /// preset-art dispatchers running the same tick, but doesn't need to
    /// pass it here too) and `trigger_poll()`'s delayed one-shot after a
    /// command (which doesn't have either in scope at all, running from
    /// its own `glib::timeout_add_local_once` closure).
    fn dispatch_fast_poll(&self) {
        let Some(poll_tx) = self.imp().poll_tx.borrow().clone() else { return };
        let (wants_upnp, upnp_client, want_bt, client, inflight, has_meta_info, probe_bt) = {
            let inner = self.imp().inner.borrow();
            let want_bt = capabilities::mode_to_input_source(inner.current_mode) == "bluetooth";
            let has_meta_info = inner.capabilities.as_ref()
                .map_or(true, |c| c.family.endpoints.supports_get_meta_info);
            let probe_bt = inner.capabilities.as_ref().map_or(true, |c| c.probes_bt);
            (inner.access == AccessMethod::UpnpPolled, inner.upnp_client.clone(), want_bt,
             inner.client.clone(), inner.fast_poll_handle.is_some(), has_meta_info, probe_bt)
        };
        let Some(client) = client else { return };
        // A poll not completing within a tick is itself a signal something
        // is wrong (a slow/hanging request, or the device having just gone
        // silent) — pile-driving more requests at it on top doesn't help,
        // and on a real disconnect (a timeout, not an instant "connection
        // refused") it produced a real backlog of straggling in-flight
        // calls that kept logging failures long after the device was
        // already correctly marked offline. Just skip this tick.
        if inflight { return; }

        match (wants_upnp, upnp_client) {
            (true, None) => {
                // Selected but not ready yet — see doc comment above.
            }
            (true, Some(uc)) => {
                let handle = self.rt().spawn(async move {
                    let (info, bt_status) = fetch_upnp_fast_poll(uc, client, want_bt, probe_bt).await;
                    let _ = poll_tx.send(PollData::Upnp { info, bt_status }).await;
                });
                let mut inner = self.imp().inner.borrow_mut();
                inner.last_poll = Some(Instant::now());
                inner.fast_poll_handle = Some(handle);
            }
            (false, _) => {
                let handle = self.rt().spawn(async move {
                    let (status, meta, bt_status) = fetch_http_fast_poll(client, want_bt, probe_bt, has_meta_info).await;
                    let _ = poll_tx.send(PollData::Http { status, meta, bt_status }).await;
                });
                let mut inner = self.imp().inner.borrow_mut();
                inner.last_poll = Some(Instant::now());
                inner.fast_poll_handle = Some(handle);
            }
        }
    }

    /// Slow poll — this tick's phase, if the rotation is active
    /// (`dispatch_phase`, from `advance_slow_poll_rotation()`). Skips (with
    /// a debug log) rather than fetching when the relevant capability flag
    /// says this device doesn't support the phase's endpoint.
    fn dispatch_slow_poll(
        &self,
        client:                &WiimClient,
        slow_tx:               &async_channel::Sender<SlowPollResult>,
        dispatch_phase:        Option<SlowPollPhase>,
        probe_outputs:         bool,
        probe_output_status:   bool,
        preset_source:         capabilities::PresetSource,
        preset_probe_failures: u32,
        preset_fp:             String,
        upnp_client:           Option<UpnpClient>,
    ) {
        let Some(phase) = dispatch_phase else { return };
        let enabled = match phase {
            SlowPollPhase::Outputs => probe_outputs,
            SlowPollPhase::OutputStatus => probe_output_status,
            SlowPollPhase::Presets => preset_source != capabilities::PresetSource::Unavailable,
            SlowPollPhase::DeviceInfo => true,
        };
        if !enabled {
            dbg(self, &format!("slow poll: phase {phase:?} skipped (not supported)"));
            return;
        }
        dbg(self, &format!("slow poll: phase {phase:?}"));
        let cp = client.clone();
        let tx = slow_tx.clone();
        let dispatched_at = Instant::now();
        // Captured by value (not `self`/`ds`) since this runs inside
        // `rt().spawn()` on the shared tokio thread — that future must be
        // `Send`, and `DeviceState` (a GObject) isn't, so `dbg()` (which
        // needs `&DeviceState`) can't be called from in here at all. A
        // plain ip string is cheap and Send-safe to capture instead.
        let ip = self.ip();
        let handle = self.rt().spawn(async move {
            let result = run_slow_poll_phase(
                cp, phase, preset_fp, upnp_client, preset_source, preset_probe_failures,
            ).await;
            // Every phase here is one or two calls straight to the device
            // itself, so this should always be fast — logged (round-trip,
            // not just "dispatched") so a phase that's unexpectedly slow
            // shows up rather than just being an unexplained delay.
            let elapsed = dispatched_at.elapsed();
            if elapsed > Duration::from_secs(1) && DEBUG_STATE.load(Ordering::Relaxed) {
                println!("[state] {ip}: slow poll: phase {phase:?} took {elapsed:?} (slower than usual)");
            }
            let _ = tx.send(result).await;
        });
        self.imp().inner.borrow_mut().slow_poll_handle = Some(handle);
    }

    /// Dispatch a fetch for every preset slot in `pending_preset_art` not
    /// already in flight. Called every fast-poll tick (`do_poll()`) rather
    /// than gated behind the slow-poll rotation — these are external CDN
    /// requests, not WiiM API calls, so they don't need to follow the
    /// device's one-call-per-tick discipline, and shouldn't wait for a
    /// `Presets` slow-poll phase to come around again either. Results ride
    /// the fast-poll's own channel/processor (`PollData::PresetArt`) — see
    /// that variant's doc comment for why.
    fn dispatch_pending_preset_art(&self, client: &WiimClient, poll_tx: &async_channel::Sender<PollData>) {
        let to_fetch: Vec<(usize, String)> = {
            let mut inner = self.imp().inner.borrow_mut();
            let out: Vec<(usize, String)> = inner.pending_preset_art.iter()
                .filter(|(slot, _)| !inner.preset_art_inflight.contains(slot))
                .map(|(&slot, (url, _))| (slot, url.clone()))
                .collect();
            for (slot, _) in &out {
                inner.preset_art_inflight.insert(*slot);
            }
            out
        };
        for (slot, url) in to_fetch {
            dbg(self, &format!("preset art: fetching slot {slot} ({url})"));
            let cp = client.clone();
            let tx = poll_tx.clone();
            self.rt().spawn(async move {
                let bytes = cp.fetch_bytes(&url).await.ok();
                let _ = tx.send(PollData::PresetArt { slot, url, bytes }).await;
            });
        }
    }

    fn start_poll_processor(
        &self,
        poll_rx: async_channel::Receiver<PollData>,
        art_tx: async_channel::Sender<(String, Vec<u8>)>,
    ) {
        let ds = self.downgrade();
        glib::spawn_future_local(async move {
            while let Ok(data) = poll_rx.recv().await {
                let Some(ds) = ds.upgrade() else { break };
                ds.process_poll(data, &art_tx);
            }
        });
    }

    /// `bytes` is empty when `fetch_art()` failed (or the URL truly returned
    /// nothing) — treated as "no artwork" rather than dropped silently, so a
    /// failed download still clears the previous track's stale art instead of
    /// leaving it on screen forever.
    ///
    /// `url` is the URL this fetch was *for*, tagged on by `fetch_art()` —
    /// checked against `inner.playback.art_url` before applying anything.
    /// A fetch that was in flight when the input changed (or a newer fetch
    /// superseded it) can land after `art_url` has already moved on to
    /// something else (or been cleared to `None` by
    /// `blank_playback_baseline()`); applying it anyway would paint the
    /// wrong track's artwork over whatever's actually current now. Mirrors
    /// `process_preset_art_result()`'s identical stale-result guard for
    /// presets.
    fn start_art_loader(&self, art_rx: async_channel::Receiver<(String, Vec<u8>)>) {
        let ds = self.downgrade();
        glib::spawn_future_local(async move {
            while let Ok((url, bytes)) = art_rx.recv().await {
                let Some(ds) = ds.upgrade() else { break };
                let applied = {
                    let mut inner = ds.imp().inner.borrow_mut();
                    if inner.playback.art_url.as_deref() != Some(url.as_str()) {
                        false
                    } else {
                        Self::replace_artwork(&ds, &mut inner, None); // leak-check the outgoing value first
                        if bytes.is_empty() {
                            dbg(&ds, "artwork fetch failed; clearing stale art");
                        } else {
                            dbg(&ds, &format!("artwork loaded: {} bytes", bytes.len()));
                            inner.playback.artwork = Some(Rc::new(bytes));
                        }
                        true
                    }
                };
                if applied {
                    dbg(&ds, "signal: playback-changed (artwork)");
                    ds.emit_by_name::<()>("playback-changed", &[&playback_changed::ARTWORK]);
                } else {
                    dbg(&ds, &format!("artwork fetch result for stale/superseded url ignored: {url}"));
                }
            }
        });
    }

    /// Replace `inner.playback.artwork` with `new`, first logging (gated on
    /// `DEBUG_STATE`) if the outgoing value's `Rc` still has more than one
    /// strong reference — that would mean something outside `DeviceState`
    /// (a stale widget, a leftover clone) is holding artwork alive longer
    /// than the track it belongs to, which should never happen since every
    /// consumer is expected to re-fetch via `playback_state()` rather than
    /// cache the `Rc` itself.
    fn replace_artwork(ds: &DeviceState, inner: &mut Inner, new: Option<Rc<Vec<u8>>>) {
        if let Some(old) = inner.playback.artwork.take() {
            let refs = Rc::strong_count(&old);
            if refs > 1 {
                dbg(ds, &format!(
                    "artwork Rc still has {refs} strong refs at replacement — possible leak"
                ));
            }
        }
        inner.playback.artwork = new;
    }

    /// Whether there's currently anything playable at all. Currently
    /// takes into account the bluetooth sink status when the input
    /// is set to BT and the mode (0 = nothing).
    fn has_playable_content(mode: i32, bt_status: &Option<BtStatus>) -> bool {
        if matches!(mode, -1 | 0) { return false; } // idle / not yet known
        if capabilities::mode_to_input_source(mode) == "bluetooth" {
            return bt_status.as_ref().is_some_and(|s| s.connected);
        }
        true
    }

    /// Forces every song-metadata field (title/artist/album/artwork/
    /// quality/codec_label) to a blank baseline and every transport
    /// capability (including `can_playpause`) to disabled — the shared
    /// "nothing playable right now" state, used whenever
    /// `has_playable_content()` says so (idle mode, or Bluetooth not
    /// confirmed connected). Diffed against the *current* `playback` state
    /// (not either backend's own raw response cache), so it's a cheap
    /// no-op once already blank — returns whether it actually changed
    /// anything, for the caller to decide whether a
    /// `playback_changed::ALL` refresh is warranted (see the "`blank_mask`
    /// is overkill" note this replaced — a precise per-field bitmask isn't
    /// worth the bookkeeping for a reset this coarse).
    fn blank_playback_baseline(ds: &DeviceState, inner: &mut Inner) -> bool {
        let mut changed = false;
        if !inner.playback.title.is_empty()  { inner.playback.title  = Rc::from(""); changed = true; }
        if !inner.playback.artist.is_empty() { inner.playback.artist = Rc::from(""); changed = true; }
        if !inner.playback.album.is_empty()  { inner.playback.album  = Rc::from(""); changed = true; }
        if inner.playback.quality.is_some() || inner.playback.codec_label.is_some() {
            inner.playback.quality     = None;
            inner.playback.codec_label = None;
            changed = true;
        }
        if inner.playback.art_url.is_some() || inner.playback.artwork.is_some() {
            inner.playback.art_url = None;
            Self::replace_artwork(ds, inner, None);
            changed = true;
        }
        let disabled = playback::SourceCapabilities {
            can_next: false, can_previous: false, can_shuffle: false,
            can_repeat: false, can_seek: false, can_playpause: false,
        };
        if inner.playback.caps != disabled {
            inner.playback.caps = disabled;
            changed = true;
        }
        changed
    }

    /// The input/mode has changed: resets the whole playback baseline via
    /// `blank_playback_baseline()` unconditionally (so a stale title/caps
    /// left over from whatever was previously selected never leaks into
    /// the new source's display, even for one tick — and, the same fix in
    /// the other direction, switching *away* from a Bluetooth-disconnected
    /// source doesn't leave everything stuck disabled just because the old
    /// override happened to still be asserted a moment earlier) and deals
    /// with inputs incorrectly marked disabled in case of firmware bug.
    /// This runs before any of this tick's own per-field decode logic
    /// (same borrow, right after), which is what actually repopulates real
    /// values for the new source, same tick — no staleness window.
    ///
    /// Returns `true` if it flipped a wrongly-`disabled` input back to
    /// `enabled` (an input demonstrably in active use can't really be
    /// disabled) — the caller emits `inputs-changed` so the source menu drops
    /// the stale greyed-out styling on that entry.
    fn apply_mode_change(ds: &DeviceState, inner: &mut Inner, new_mode: i32) -> bool {
        inner.current_mode = new_mode;
        Self::blank_playback_baseline(ds, inner);
        // Not (still) Bluetooth: nothing left to track for it either.
        if capabilities::mode_to_input_source(new_mode) != "bluetooth" {
            inner.playback.bt_connected   = false;
            inner.playback.bt_device_name = None;
            inner.playback.bt_pairing     = false;
        }
        let active_id = capabilities::mode_to_input_source(new_mode);
        if let Some(caps) = inner.capabilities.as_mut() {
            if let Some(entry) = caps.inputs.iter_mut().find(|i| i.id == active_id) {
                if !entry.enabled {
                    eprintln!(
                        "[state] input {active_id:?} reported disabled but is \
                         actively in use; marking enabled",
                    );
                    entry.enabled = true;
                    return true;
                }
            }
        }
        false
    }

    /// Shared by `process_poll_http()`/`process_poll_upnp()`'s mode-change
    /// handling. Returns `(emit_input_changed, emit_inputs_changed)`:
    /// `input-changed` for a real/confirmed mode change or a timed-out switch
    /// that needs reverting; `inputs-changed` only when `apply_mode_change()`
    /// had to force a wrongly-disabled active input back to enabled.
    fn handle_input_mode_poll(ds: &DeviceState, inner: &mut Inner, mode_changed: bool, new_mode: i32) -> (bool, bool) {
        let mut inputs_changed = false;
        if mode_changed {
            inputs_changed = Self::apply_mode_change(ds, inner, new_mode);
        } else {
            let Some(sent) = inner.input_change_time else { return (false, false) };
            if !inner.input_changing || sent.elapsed() < INPUT_CHANGE_TIMEOUT {
                return (false, false);
            }
            eprintln!("[state] timeout changing input");
        }
        inner.input_changing = false;
        (true, inputs_changed)
    }

    fn start_slow_poll_processor(&self, rx: async_channel::Receiver<SlowPollResult>) {
        let ds_weak = self.downgrade();
        glib::spawn_future_local(async move {
            while let Ok(result) = rx.recv().await {
                let Some(ds) = ds_weak.upgrade() else { break };
                ds.imp().inner.borrow_mut().slow_poll_handle = None;
                match result {
                    SlowPollResult::Presets { source, probe_failures, entries } => {
                        ds.handle_slow_poll_preset_source(source, probe_failures);
                        ds.handle_slow_poll_presets(entries);
                    }
                    SlowPollResult::Outputs(outputs)     => ds.handle_slow_poll_outputs(outputs),
                    SlowPollResult::OutputStatus(status) => ds.handle_slow_poll_output_status(status),
                    SlowPollResult::DeviceInfo(info)     => ds.handle_slow_poll_device_info(info),
                }
            }
        });
    }

    /// `Simple`-mode counterpart of `start_slow_poll_processor()` — its own
    /// channel/loop rather than sharing `slow_tx`/`slow_rx`, specifically so
    /// this can't accidentally clear `slow_poll_handle` (a `Full`-mode-only
    /// field) for a result that has nothing to do with `Full` mode's
    /// rotation. Reuses `handle_slow_poll_device_info()` unchanged — same
    /// liveness/identity-check logic as `Full` mode's `getStatusEx` phase,
    /// not a second implementation of it.
    fn start_simple_poll_processor(&self, rx: async_channel::Receiver<Option<DeviceInfo>>) {
        let ds_weak = self.downgrade();
        glib::spawn_future_local(async move {
            while let Ok(info) = rx.recv().await {
                let Some(ds) = ds_weak.upgrade() else { break };
                ds.imp().inner.borrow_mut().simple_poll_handle = None;
                ds.handle_slow_poll_device_info(info);
            }
        });
    }

    /// Persist the resolved `PresetSource`/failure count from this tick's
    /// fetch. `source` is the device's (effectively) one-way-door
    /// capability record — `DeviceCapabilities::record_preset_source()` —
    /// mirroring `handle_slow_poll_outputs()`'s `record_outputs_probe()`
    /// pattern; `fetch_presets_with_fallback()` only ever resolves it away
    /// from `Unknown`/back-and-forth between `Http`/`Upnp`/`Unavailable`
    /// once it's either succeeded, been confirmed unsupported, or
    /// exhausted its retry budget. `probe_failures` is a short-lived retry
    /// counter, not part of the device's identity, so it's written
    /// straight to `Inner` instead of through `capabilities` — see
    /// `Inner::preset_probe_failures`'s doc comment. Neither write emits a
    /// signal (unlike outputs, this isn't itself UI-visible) — just
    /// updating what the next tick's `dispatch_slow_poll` reads.
    fn handle_slow_poll_preset_source(&self, source: capabilities::PresetSource, probe_failures: u32) {
        let mut inner = self.imp().inner.borrow_mut();
        inner.preset_probe_failures = probe_failures;
        let Some(caps) = inner.capabilities.as_mut() else { return };
        caps.record_preset_source(source);
    }

    /// `fetch_presets()` never fetches artwork itself (see its doc
    /// comment) — this reuses whatever's already sitting in the *previous*
    /// `presets` list for a slot whose `picurl` hasn't changed, and queues
    /// the rest into `pending_preset_art` for `dispatch_pending_preset_art()`
    /// to pick up on a later fast-poll tick. The list itself (names/kinds,
    /// with placeholder/reused art) is applied and signalled immediately —
    /// artwork fills in progressively afterward, each arrival its own
    /// `presets-changed` emission (see `process_preset_art_result()`).
    fn handle_slow_poll_presets(&self, presets: Option<(String, Vec<PresetEntry>)>) {
        let Some((new_fp, mut entries)) = presets else {
            dbg(self, "slow poll: presets unchanged");
            return;
        };
        dbg(self, &format!("slow poll: presets updated: {} slots", entries.len()));
        {
            let mut inner = self.imp().inner.borrow_mut();
            let mut needs_fetch: Vec<(usize, String)> = Vec::new();
            for entry in entries.iter_mut() {
                if entry.picurl.is_empty() { continue; }
                if let Some(prev) = inner.presets.iter().find(|p| p.slot == entry.slot && p.picurl == entry.picurl) {
                    entry.art_bytes = prev.art_bytes.clone();
                }
                if entry.art_bytes.is_empty() {
                    needs_fetch.push((entry.slot, entry.picurl.clone()));
                }
            }
            inner.preset_fp = new_fp;
            inner.presets   = entries;
            // Drop tracking for slots that no longer need a fetch (art was
            // reused, or the slot no longer exists/isn't Media anymore).
            let needed_slots: HashSet<usize> = needs_fetch.iter().map(|(slot, _)| *slot).collect();
            inner.pending_preset_art.retain(|slot, _| needed_slots.contains(slot));
            for (slot, url) in needs_fetch {
                // Keep the existing attempt count if this slot was already
                // pending the *same* URL (so a bounced-but-unrelated list
                // refresh doesn't reset its retry budget); reset to 0 for a
                // genuinely new/changed URL.
                match inner.pending_preset_art.get(&slot) {
                    Some((existing_url, _)) if *existing_url == url => {}
                    _ => { inner.pending_preset_art.insert(slot, (url, 0)); }
                }
            }
        }
        dbg(self, "signal: presets-changed");
        self.emit_by_name::<()>("presets-changed", &[]);
    }

    /// Owns the give-up/retry-budget decision for `getSoundCardModeSupportList`
    /// itself (`outputs_probe_failures`, this `Inner`) — `capabilities.rs`'s
    /// `record_outputs()` only ever sees a confirmed success, storing the
    /// resolved list; it has no notion of failures or thresholds at all.
    /// `ApiOutcome::Unsupported` (the device explicitly said "unknown
    /// command") gives up immediately, no retry budget spent on it —
    /// that's a *definite* answer, not a transient miss.
    fn handle_slow_poll_outputs(&self, outputs: ApiOutcome<Vec<OutputEntry>>) {
        let mut inner = self.imp().inner.borrow_mut();
        let changed = match outputs {
            ApiOutcome::Ok(list) => {
                inner.outputs_probe_failures = 0;
                match inner.capabilities.as_mut() {
                    Some(caps) => caps.record_outputs(list),
                    None => false,
                }
            }
            ApiOutcome::Unsupported => {
                dbg(self, "slow poll: getSoundCardModeSupportList confirmed unsupported, giving up");
                if let Some(caps) = inner.capabilities.as_mut() { caps.probes_outputs = false; }
                false
            }
            ApiOutcome::Failed => {
                let gave_up = record_probe_failure(
                    &mut inner.outputs_probe_failures, OUTPUTS_PROBE_FAIL_THRESHOLD,
                    "getSoundCardModeSupportList",
                );
                if gave_up {
                    if let Some(caps) = inner.capabilities.as_mut() { caps.probes_outputs = false; }
                }
                false
            }
        };
        drop(inner);
        if changed {
            dbg(self, "signal: outputs-changed");
            self.emit_by_name::<()>("outputs-changed", &[]);
        }
    }

    /// Same give-up/retry-budget shape as `handle_slow_poll_outputs()`, for
    /// `getNewAudioOutputHardwareMode` — see that function's doc comment.
    fn handle_slow_poll_output_status(&self, status: ApiOutcome<AudioOutputStatus>) {
        let out = {
            let mut inner = self.imp().inner.borrow_mut();
            match status {
                ApiOutcome::Ok(out) => {
                    inner.output_status_probe_failures = 0;
                    Some(out)
                }
                ApiOutcome::Unsupported => {
                    dbg(self, "slow poll: getNewAudioOutputHardwareMode confirmed unsupported, giving up");
                    if let Some(caps) = inner.capabilities.as_mut() { caps.probes_output_status = false; }
                    None
                }
                ApiOutcome::Failed => {
                    let gave_up = record_probe_failure(
                        &mut inner.output_status_probe_failures, OUTPUT_STATUS_PROBE_FAIL_THRESHOLD,
                        "getNewAudioOutputHardwareMode",
                    );
                    if gave_up {
                        if let Some(caps) = inner.capabilities.as_mut() { caps.probes_output_status = false; }
                    }
                    None
                }
            }
        };
        let Some(out) = out else { return };
        let (changed, prev_hw) = {
            let inner = self.imp().inner.borrow();
            let prev_hw = inner.output_status.as_ref().map(|o| o.hardware.clone());
            let changed = prev_hw.as_deref() != Some(out.hardware.as_str());
            (changed, prev_hw)
        };
        if changed {
            dbg(self, &format!(
                "slow poll: output changed: {} → {}",
                prev_hw.as_deref().unwrap_or("none"), out.hardware,
            ));
        } else {
            dbg(self, &format!("slow poll: output status unchanged: {}", out.hardware));
        }
        self.imp().inner.borrow_mut().output_status = Some(out);
        if changed {
            dbg(self, "signal: output-changed");
            self.emit_by_name::<()>("output-changed", &[]);
        }
    }

    /// Writes one fast-poll `getbtstatus` result straight into
    /// `inner.playback` — called from *within*
    /// `process_poll_http`/`process_poll_upnp`'s own borrow, using the
    /// exact same `bt_status` value `has_playable_content()` just decided
    /// with, so there's no way for "what we decided" and "what we
    /// recorded" to diverge (the previous design applied this in a
    /// separate pass, reading `inner.current_mode` as it stood *after*
    /// this same tick's mode update but comparing against
    /// `inner.playback.bt_connected` from *before* it — exactly the
    /// staleness class of bug this whole redesign exists to close).
    /// Resetting back to disconnected/no-name when Bluetooth *isn't* (or
    /// stopped being) the active input is `apply_mode_change()`'s job, not
    /// this function's — this one only ever runs with a `status` already
    /// known to be fresh and relevant. Returns whether it actually changed
    /// anything, mirroring `blank_playback_baseline()`'s shape so both can
    /// feed the same "OR into `playback_changed::ALL`" pattern.
    fn apply_bt_status(inner: &mut Inner, status: &BtStatus) -> bool {
        let name: Option<Rc<str>> = if status.connected && !status.device_name.is_empty() {
            Some(Rc::from(status.device_name.as_str()))
        } else {
            None
        };
        let changed = inner.playback.bt_connected != status.connected
            || inner.playback.bt_device_name.as_deref() != name.as_deref()
            || inner.playback.bt_pairing != status.pairing;
        inner.playback.bt_connected   = status.connected;
        inner.playback.bt_device_name = name;
        inner.playback.bt_pairing     = status.pairing;
        changed
    }

    fn handle_slow_poll_device_info(&self, info: Option<DeviceInfo>) {
        // getStatusEx failed — no threshold/counter (see report_failure()'s
        // doc comment: cmd()'s own internal retry already absorbed a
        // transient blip before this ever got here).
        let Some(new_info) = info else {
            if self.imp().inner.borrow().connection_state == ConnectionState::Connected {
                self.report_failure("slow poll: getStatusEx failed");
            }
            return;
        };
        dbg(self, "slow poll: getStatusEx ok");

        let (prev_fw, prev_name, prev_netstat, prev_rssi, prev_remote) = {
            let inner = self.imp().inner.borrow();
            let di = inner.device_info.as_ref();
            (
                di.map(|i| i.firmware.clone()).unwrap_or_default(),
                di.map(|i| i.device_name.clone()).unwrap_or_default(),
                inner.netstat,
                inner.rssi,
                inner.remote,
            )
        };

        // A different device now answers this IP's getStatusEx than the
        // one this `DeviceState` is for — same "don't attach, just
        // disconnect" handling as `fetch_device_info()`'s success handler
        // (see `imp::DeviceState::uuid`'s doc comment for the full
        // reasoning; this is the DHCP-lease-reassignment/device-swapped-
        // at-this-IP case, not a "this device renamed its uuid" case —
        // there is no such thing).
        let known_uuid = self.uuid();
        if !known_uuid.is_empty() && new_info.uuid != known_uuid {
            self.report_failure(&format!(
                "device identity mismatch at this IP (expected {known_uuid}, got {})",
                new_info.uuid,
            ));
            return;
        }

        let identity_changed =
            new_info.firmware    != prev_fw   ||
            new_info.device_name != prev_name;

        let new_netstat: Option<u32> = new_info.netstat.parse().ok();
        let new_rssi:    Option<i32> = new_info.rssi.parse().ok();
        let new_remote = RemoteInfo {
            connected: parse_remote_connected(&new_info.ble_remote_connected),
            battery:   new_info.ble_remote_battery.parse().ok(),
            rssi:      new_info.ble_remote_rssi.parse().ok(),
        };

        let network_changed =
            new_netstat != prev_netstat ||
            new_rssi    != prev_rssi;

        let remote_changed = new_remote != prev_remote;

        {
            let mut inner = self.imp().inner.borrow_mut();
            inner.netstat = new_netstat;
            inner.rssi    = new_rssi;
            inner.remote  = new_remote;
            if identity_changed {
                dbg(self, &format!(
                    "device identity changed: fw={} uuid={} name={}",
                    new_info.firmware, new_info.uuid, new_info.device_name,
                ));
                inner.device_info = Some(new_info);
            }
        }

        if identity_changed {
            self.emit_by_name::<()>("device-changed", &[]);
        }
        if network_changed {
            dbg(self, &format!(
                "signal: network changed: netstat={} rssi={}",
                self.imp().inner.borrow().netstat.unwrap_or(0),
                self.imp().inner.borrow().rssi.unwrap_or(0),
            ));
            self.emit_by_name::<()>("network-changed", &[]);
        }
        if remote_changed {
            dbg(self, &format!("signal: remote changed: {:?}", self.imp().inner.borrow().remote));
            self.emit_by_name::<()>("remote-changed", &[]);
        }
    }

    /// Resolves this tick's raw `getbtstatus` outcome into the plain
    /// `Option<BtStatus>` `process_poll_http()`/`process_poll_upnp()`
    /// already expect, folding in the "confirmed unsupported" self-
    /// correction: `ApiOutcome::Unsupported` (a *definite* "unknown
    /// command" answer, not a transient hiccup) flips `probes_bt` to
    /// `false` so `dispatch_fast_poll()` stops calling this at all from
    /// the next tick on, and — both on the tick that just learned this
    /// and on every subsequent tick that skipped calling it for that
    /// reason — synthesizes `connected: true` rather than leaving
    /// Bluetooth looking permanently disconnected. `ApiOutcome::Failed`
    /// (transient) and `None` while Bluetooth isn't even the active input
    /// both still resolve to `None` — "no new information," same as
    /// before this existed.
    fn resolve_bt_status(&self, raw: Option<ApiOutcome<BtStatus>>) -> Option<BtStatus> {
        let assume_connected = || Some(BtStatus {
            connected: true, device_name: String::new(), pairing: false,
        });
        match raw {
            Some(ApiOutcome::Ok(status)) => Some(status),
            Some(ApiOutcome::Unsupported) => {
                let mut inner = self.imp().inner.borrow_mut();
                if let Some(caps) = inner.capabilities.as_mut() {
                    if caps.probes_bt {
                        dbg(self, "getbtstatus: confirmed unsupported, won't ask again");
                        caps.probes_bt = false;
                    }
                }
                assume_connected()
            }
            Some(ApiOutcome::Failed) => None,
            None => {
                let confirmed_unsupported =
                    self.imp().inner.borrow().capabilities.as_ref().is_some_and(|c| !c.probes_bt);
                if confirmed_unsupported { assume_connected() } else { None }
            }
        }
    }

    /// Dispatch to whichever backend actually produced this tick's data —
    /// `PollData::Http`/`PollData::Upnp` are mutually exclusive (see
    /// `PollData`'s doc comment), so exactly one of these runs per tick,
    /// never both.
    fn process_poll(&self, data: PollData, art_tx: &async_channel::Sender<(String, Vec<u8>)>) {
        match data {
            PollData::Http { status, meta, bt_status } => {
                let bt_status = self.resolve_bt_status(bt_status);
                self.imp().inner.borrow_mut().fast_poll_handle = None;
                self.process_poll_http(status, meta, bt_status, art_tx);
            }
            PollData::Upnp { info, bt_status } => {
                let bt_status = self.resolve_bt_status(bt_status);
                self.imp().inner.borrow_mut().fast_poll_handle = None;
                self.process_poll_upnp(info, bt_status, art_tx);
            }
            PollData::PresetArt { slot, url, bytes } => self.process_preset_art_result(slot, url, bytes),
        }
    }

    /// Applies one preset slot's artwork fetch result (`dispatch_pending_preset_art`).
    /// Up to `PRESET_ART_MAX_ATTEMPTS` failures are retried — one attempt
    /// per fast-poll tick a slot remains pending, not an inline retry loop —
    /// before giving up and leaving that slot on its placeholder (empty
    /// `art_bytes`, which the UI already renders as a fallback icon).
    fn process_preset_art_result(&self, slot: usize, url: String, bytes: Option<Vec<u8>>) {
        const PRESET_ART_MAX_ATTEMPTS: u32 = 3;
        let mut inner = self.imp().inner.borrow_mut();
        inner.preset_art_inflight.remove(&slot);

        // Stale result: the preset list moved on (different URL for this
        // slot, or the slot no longer needs a fetch at all) while this one
        // was in flight — discard rather than misapplying it.
        let Some(&(ref tracked_url, attempts)) = inner.pending_preset_art.get(&slot) else { return };
        if *tracked_url != url { return; }

        match bytes {
            Some(bytes) => {
                inner.pending_preset_art.remove(&slot);
                if let Some(entry) = inner.presets.iter_mut().find(|p| p.slot == slot) {
                    entry.art_bytes = bytes;
                }
                drop(inner);
                dbg(self, &format!("preset art: slot {slot} loaded ({url})"));
                dbg(self, "signal: presets-changed");
                self.emit_by_name::<()>("presets-changed", &[]);
            }
            None => {
                let attempts = attempts + 1;
                if attempts >= PRESET_ART_MAX_ATTEMPTS {
                    dbg(self, &format!("preset art: slot {slot} failed {attempts} times, giving up ({url})"));
                    inner.pending_preset_art.remove(&slot);
                } else {
                    dbg(self, &format!("preset art: slot {slot} failed (attempt {attempts}/{PRESET_ART_MAX_ATTEMPTS}), will retry ({url})"));
                    inner.pending_preset_art.insert(slot, (url, attempts));
                }
            }
        }
    }

    /// Diffs the raw HTTP responses against the cached baseline *before* any
    /// decoding happens (plain field/value comparisons — this is also the
    /// `playback_changed` bitmask computation), then decodes only the field
    /// groups whose bit came out set, writing straight into `inner.playback`
    /// in place. An unchanged `title` never gets re-run through metadata
    /// decoding, an unchanged `mode`/`vendor` pair never re-runs the
    /// source-name lookup, an unchanged `curpos`/`totlen` never re-runs the
    /// ms/µs heuristic — decode cost is paid only when the raw diff already
    /// told us something changed.
    ///
    /// `bt_status` is this exact tick's `getbtstatus` reading (already
    /// fetched by `fetch_http_fast_poll()`, ahead of `getPlayerStatusEx`) —
    /// applied here, in the same borrow that also decides
    /// `has_playable_content()`, rather than in a separate post-hoc pass
    /// (see `apply_bt_status()`'s doc comment for why that separation used
    /// to cause staleness bugs). Caps and metadata content are gated by
    /// `has_playable_content()`, freshly recomputed every tick from this
    /// tick's own `st.mode`/`bt_status` — never from a cross-tick cached
    /// value — so entering/leaving "nothing playable" (idle, or Bluetooth
    /// disconnected) takes effect the instant it's known, in either
    /// direction, with no lag and no dependency on write ordering.
    fn process_poll_http(
        &self,
        status: Option<PlayerStatus>,
        meta:   Option<MetaData>,
        bt_status: Option<BtStatus>,
        art_tx: &async_channel::Sender<(String, Vec<u8>)>,
    ) {
        self.note_fast_poll_result(status.is_none());

        let mut playback_mask: u32 = 0;

        // Fallback for the (very unlikely) case `status` itself failed this
        // tick but `meta` somehow still arrived — uses last tick's mode.
        // Overwritten with this tick's real, fresh `st.mode` below the
        // instant `status` is available.
        let (mut has_content, had_content) = {
            let inner = self.imp().inner.borrow();
            (Self::has_playable_content(inner.current_mode, &bt_status), inner.has_content)
        };

        if let Some(st) = status {
            has_content = Self::has_playable_content(st.mode, &bt_status);

            // 1. Borrow: diff against previous status, compute everything we
            //    need from `inner` before it's dropped.
            let (mode_changed, prev_mode, mute_changed, vol_changed, timing_valid, time_changed, other_changed) = {
                let inner = self.imp().inner.borrow();
                let prev = inner.player_status.as_ref();
                // Mute and volume both have an optimistic write
                // (`do_set_mute()`/`do_set_volume()`, for instant UI
                // feedback), so a plain diff against the *previous* raw
                // response isn't enough for either: if a `SetMute`/
                // `SetVolume` command silently failed to stick device-side,
                // the device's own answer never changes between polls, so
                // that diff would never fire and `playback.muted`/`.volume`
                // would stay wrong forever. Instead, resync straight
                // against the device's answer whenever nothing we sent is
                // still in flight/settling — this self-heals a rejected/
                // clamped command exactly the same way it picks up a
                // genuine remote change (physical remote, another app,
                // slave-speaker sync): both look like "device says X,
                // canonical state says Y" from here. Also gated on
                // `VOLUME_POLL_SETTLE` — a real device can keep reporting
                // its pre-command value for a moment after accepting a
                // command, so "nothing in flight" alone
                // isn't enough; see that constant's doc comment.
                let vol_settled = inner.last_volume_cmd
                    .map_or(true, |t| Instant::now().duration_since(t) >= VOLUME_POLL_SETTLE);
                let vol_changed = inner.target_volume < 0 && vol_settled && st.vol != inner.playback.volume;
                let mute_settled = inner.last_mute_cmd
                    .map_or(true, |t| Instant::now().duration_since(t) >= VOLUME_POLL_SETTLE);
                let mute_changed = mute_settled && st.mute != inner.playback.muted;
                let timing_valid = playback::timing_looks_valid(st.curpos, st.totlen);
                let time_changed = timing_valid
                    && prev.map_or(true, |p| p.curpos != st.curpos || p.totlen != st.totlen);
                let other_changed = prev.map_or(true, |p| {
                    p.status != st.status || p.loop_mode != st.loop_mode || p.vendor != st.vendor
                });
                let prev_mode = inner.current_mode;
                (st.mode != prev_mode, prev_mode, mute_changed, vol_changed, timing_valid, time_changed, other_changed)
            };

            if mute_changed || vol_changed { playback_mask |= playback_changed::VOLUME; }
            if time_changed                { playback_mask |= playback_changed::TIME; }
            if other_changed               { playback_mask |= playback_changed::OTHER; }

            if mode_changed {
                dbg(self, &format!(
                    "input changed: mode {} → {} (status={})",
                    prev_mode, st.mode, st.status,
                ));
            }
            if !timing_valid {
                dbg(self, &format!(
                    "timing: ignoring garbage reading (curpos={} > totlen={})",
                    st.curpos, st.totlen,
                ));
            }

            // 2. Borrow_mut: decode only what changed, straight into `playback`.
            let emit_input_changed;
            let emit_inputs_changed;
            {
                let mut inner = self.imp().inner.borrow_mut();
                (emit_input_changed, emit_inputs_changed) =
                    Self::handle_input_mode_poll(self, &mut inner, mode_changed, st.mode);
                if mode_changed { playback_mask |= playback_changed::ALL; }

                if let Some(bts) = &bt_status {
                    if Self::apply_bt_status(&mut inner, bts) { playback_mask |= playback_changed::ALL; }
                }

                if mute_changed { inner.playback.muted  = st.mute; }
                if vol_changed  { inner.playback.volume = st.vol;  }
                if time_changed {
                    let (pos, dur) = playback::decode_timing_http(st.curpos, st.totlen, st.mode);
                    inner.playback.position = pos;
                    inner.playback.duration = dur;
                }
                if other_changed {
                    inner.playback.status      = playback::decode_status_http(&st.status);
                    let dev_id = inner.capabilities.as_ref().map(|c| c.device_id);
                    inner.playback.source_name = playback::decode_source_name_http(st.mode, &st.vendor, dev_id);
                    let (shuffle, repeat) = playback::decode_loop_mode_http(st.loop_mode);
                    inner.playback.shuffle = shuffle;
                    inner.playback.repeat  = repeat;
                }
                if has_content {
                    // `had_content` false forces a redecode even without a raw
                    // diff — see `Inner::has_content`'s doc comment:
                    // the wire fields may genuinely not have changed across
                    // a disconnect→reconnect cycle.
                    if other_changed || !had_content {
                        let caps = playback::decode_transport_caps_http(st.mode, &st.vendor);
                        dbg(self, &format!(
                            "transport caps (http): mode={} vendor={:?} -> {caps:?}",
                            st.mode, st.vendor,
                        ));
                        inner.playback.caps = caps;
                        playback_mask |= playback_changed::OTHER;
                    }
                } else if Self::blank_playback_baseline(self, &mut inner) {
                    playback_mask |= playback_changed::ALL;
                }
                inner.has_content = has_content;

                inner.player_status = Some(st);
            }

            // 3. Side effects, after the borrow is dropped.
            if emit_input_changed {
                dbg(self, "signal: input-changed");
                self.emit_by_name::<()>("input-changed", &[]);
            }
            if emit_inputs_changed {
                dbg(self, "signal: inputs-changed");
                self.emit_by_name::<()>("inputs-changed", &[]);
            }
        }

        if let Some(m) = meta {
            let art_url = m.art_uri().to_string();

            // 1. Borrow: diff against previous metadata, compute everything we
            //    need from `inner` before it's dropped. Diffed regardless of
            //    `has_content` — the raw cache below always tracks the
            //    latest response so a future tick's diff stays accurate,
            //    even while nothing's being applied to `playback` right now.
            let (url_changed, title_changed, artist_changed, album_changed, other_changed) = {
                let inner = self.imp().inner.borrow();
                let prev = inner.metadata.as_ref();
                let title_changed  = prev.map_or(true, |p| p.title != m.title);
                let artist_changed = prev.map_or(true, |p| p.artist != m.artist);
                let album_changed  = prev.map_or(true, |p| p.album != m.album);
                let other_changed  = prev.map_or(true, |p| {
                    p.bit_rate != m.bit_rate || p.sample_rate != m.sample_rate || p.bit_depth != m.bit_depth
                });
                let cached_url = inner.playback.art_url.as_deref().unwrap_or("");
                (art_url != cached_url, title_changed, artist_changed, album_changed, other_changed)
            };
            if has_content != had_content { playback_mask |= playback_changed::ALL; }
            else if has_content {
                if title_changed  { playback_mask |= playback_changed::TITLE; }
                if artist_changed { playback_mask |= playback_changed::ARTIST; }
                if album_changed  { playback_mask |= playback_changed::ALBUM; }
                if url_changed    { playback_mask |= playback_changed::OTHER; }
            }

            // 2. Borrow_mut: decode only what changed, straight into `playback`.
            {
                let mut inner = self.imp().inner.borrow_mut();
                if has_content {
                    if title_changed  { inner.playback.title  = Rc::from(m.title.as_str()); }
                    if artist_changed { inner.playback.artist = Rc::from(m.artist.as_str()); }
                    if album_changed  { inner.playback.album  = Rc::from(m.album.as_str()); }
                    if other_changed {
                        inner.playback.quality =
                            playback::decode_quality_http(&m.bit_rate, &m.sample_rate, &m.bit_depth);
                        // HTTP has no codec-badge equivalent at all — always clear
                        // here so switching `metadata`'s access method back to
                        // HTTP (from a Settings override) doesn't leave a stale
                        // UPnP-sourced badge on screen forever. If `metadata` is
                        // actually still `UpnpPolled` and this tick also carries a
                        // fresh `GetInfoEx` result, the UPnP block below runs
                        // right after this and sets it again.
                        inner.playback.codec_label = None;
                    }
                    if url_changed {
                        inner.playback.art_url =
                            if art_url.is_empty() { None } else { Some(Rc::from(art_url.as_str())) };
                        Self::replace_artwork(self, &mut inner, None);
                    }
                }
                inner.metadata = Some(m);
            }

            // 3. Side effects, after the borrow is dropped.
            if has_content && url_changed {
                if art_url.is_empty() {
                    // Current track has no artwork at all (was non-empty before,
                    // or this is the first metadata) — clear immediately rather
                    // than leaving the previous track's art on screen forever.
                    dbg(self, "art url cleared: current track has no artwork");
                    playback_mask |= playback_changed::ARTWORK;
                } else {
                    dbg(self, &format!("art url changed: {art_url}"));
                    // No immediate ARTWORK signal here: artwork is already
                    // cleared, but we hold off telling the UI until fetch_art()
                    // resolves (success or failure — see start_art_loader) so a
                    // fast reload doesn't flash the fallback icon in between.
                    self.fetch_art(art_url, art_tx);
                }
            }
        }

        if playback_mask != 0 {
            dbg(self, &format!("signal: playback-changed mask={:#x}", playback_mask));
            self.emit_by_name::<()>("playback-changed", &[&playback_mask]);
        }
    }

    /// UPnP counterpart to `process_poll_http()` — decodes a `GetInfoEx`
    /// response straight into `inner.playback`, unconditionally (the
    /// mutually-exclusive dispatch in `dispatch_fast_poll()`/`trigger_poll()`
    /// already guarantees this is only ever called for a device actually
    /// configured for `AccessMethod::UpnpPolled`). Ported from the HTTP path
    /// rather than left as "whatever GetInfoEx happens to cover":
    /// - **Mode/input-change detection** (`info.play_type`, confirmed
    ///   byte-identical to HTTP `mode` — see `InfoEx::play_type`'s doc
    ///   comment) drives the same art-clear + capability self-correction +
    ///   `input-changed` signal `process_poll_http()`'s `mode_changed` block
    ///   does, since nothing else runs on a tick that only fetched UPnP.
    /// - **Volume self-heal**: `SetVolume` still goes over HTTP regardless
    ///   of which backend supplies reads (see `do_set_volume`), so the same
    ///   "don't clobber an in-flight optimistic write" guard
    ///   (`target_volume < 0`) applies here too.
    /// - **Per-field diffing**, not a coarse "did the whole response change
    ///   at all" check: `GetInfoEx` includes `RelTime`, which changes every
    ///   second regardless of anything the user cares about, so a coarse
    ///   check would be true almost every tick and flood the UI with
    ///   redundant redraws.
    ///
    /// `bt_status` — see `process_poll_http()`'s identical doc comment on
    /// its own `bt_status` parameter; the same "apply in the same borrow
    /// that decides `has_playable_content()`" reasoning applies here.
    /// Unlike HTTP, `GetInfoEx` always bundles metadata into the one call
    /// regardless (no fetch to skip), so `has_playable_content()` only
    /// gates the *decode*, not a separate fetch.
    fn process_poll_upnp(
        &self, info: Option<upnp::InfoEx>, bt_status: Option<BtStatus>,
        art_tx: &async_channel::Sender<(String, Vec<u8>)>,
    ) {
        self.note_fast_poll_result(info.is_none());
        let Some(info) = info else { return };
        let mut playback_mask: u32 = 0;

        // 1. Borrow: diff each field group against the previous response.
        let (
            mode_changed, prev_mode,
            status_changed, time_changed, mute_changed, vol_changed,
            source_changed, title_changed, artist_changed, album_changed, quality_changed,
            had_content,
        ) = {
            let inner = self.imp().inner.borrow();
            let prev = inner.upnp_info.as_ref();
            let prev_mode = inner.current_mode;
            let status_changed = prev.map_or(true, |p| {
                p.transport_state != info.transport_state || p.loop_mode != info.loop_mode
            });
            let time_changed = prev.map_or(true, |p| {
                p.rel_time != info.rel_time || p.track_duration != info.track_duration
            });
            // `None` (still-unresolved mute, even after `fetch_upnp_fast_poll`'s
            // supplementary call) means "no new information" — never treated
            // as a change, and never written below. Self-heals against
            // `playback.muted` (not the raw previous response), same
            // reasoning as `process_poll_http()`'s `mute_changed` — see its
            // doc comment. `SetMute`/`SetVolume` still go over HTTP or UPnP
            // per `mute_access`/`access`, but `last_mute_cmd`/
            // `last_volume_cmd` themselves are shared between both poll
            // backends regardless of which one actually sent the command.
            let mute_settled = inner.last_mute_cmd
                .map_or(true, |t| Instant::now().duration_since(t) >= VOLUME_POLL_SETTLE);
            let mute_changed = info.current_mute.is_some_and(|m| m != inner.playback.muted)
                && mute_settled;
            // Same self-heal reasoning as process_poll_http()'s vol_changed
            // — see its doc comment (including `VOLUME_POLL_SETTLE`).
            // `SetVolume` still goes over HTTP regardless of poll backend,
            // so the debounce/`target_volume`/`last_volume_cmd` state is
            // shared between both paths.
            let vol_settled = inner.last_volume_cmd
                .map_or(true, |t| Instant::now().duration_since(t) >= VOLUME_POLL_SETTLE);
            let vol_changed = inner.target_volume < 0 && vol_settled && info.current_volume != inner.playback.volume;
            let source_changed = prev.map_or(true, |p| {
                p.play_medium != info.play_medium || p.track_source != info.track_source
                    || p.gui_behavior != info.gui_behavior
            });
            let title_changed  = prev.map_or(true, |p| p.title != info.title);
            let artist_changed = prev.map_or(true, |p| p.artist != info.artist);
            let album_changed  = prev.map_or(true, |p| p.album != info.album);
            let quality_changed = prev.map_or(true, |p| {
                p.actual_quality != info.actual_quality || p.bitrate != info.bitrate
                    || p.format_s != info.format_s || p.rate_hz != info.rate_hz
                    || p.protocol_info != info.protocol_info
            });
            (
                info.play_type != prev_mode, prev_mode,
                status_changed, time_changed, mute_changed, vol_changed,
                source_changed, title_changed, artist_changed, album_changed, quality_changed,
                inner.has_content,
            )
        };

        // `info.play_type` is never the `-1` "tag absent" sentinel by this
        // point — `fetch_upnp_fast_poll()` already substitutes a real value
        // from `PlayMedium` when `GetInfoEx` doesn't carry `<PlayType>` at
        // all (confirmed permanent on some devices, e.g. Audio Pro Addon
        // C5), so this needs no special-casing here the way it briefly did.
        let has_content = Self::has_playable_content(info.play_type, &bt_status);
        if (has_content != had_content) || mode_changed {
            playback_mask |= playback_changed::ALL
        } else {
            if mute_changed || vol_changed { playback_mask |= playback_changed::VOLUME; }
            if time_changed                { playback_mask |= playback_changed::TIME; }
            if status_changed              { playback_mask |= playback_changed::OTHER; }
            if has_content  {
                if title_changed  { playback_mask |= playback_changed::TITLE; }
                if artist_changed { playback_mask |= playback_changed::ARTIST; }
                if album_changed  { playback_mask |= playback_changed::ALBUM; }
                if source_changed || quality_changed { playback_mask |= playback_changed::OTHER; }
            }
        }

        if mode_changed {
            dbg(self, &format!("input changed (upnp): mode {prev_mode} → {}", info.play_type));
        }

        let mut art_url_for_fetch: Option<String> = None;
        let mut art_cleared = false;

        // 2. Borrow_mut: decode only what changed, straight into `playback`.
        let emit_input_changed;
        let emit_inputs_changed;
        {
            let mut inner = self.imp().inner.borrow_mut();
            (emit_input_changed, emit_inputs_changed) =
                Self::handle_input_mode_poll(self, &mut inner, mode_changed, info.play_type);

            if let Some(bts) = &bt_status {
                if Self::apply_bt_status(&mut inner, bts) { playback_mask |= playback_changed::ALL; }
            }

            if status_changed {
                inner.playback.status = playback::decode_status_upnp(&info.transport_state);
                let (shuffle, repeat) = playback::decode_loop_mode_http(info.loop_mode);
                inner.playback.shuffle = shuffle;
                inner.playback.repeat  = repeat;
            }
            if time_changed {
                inner.playback.position = playback::decode_hms_duration(&info.rel_time);
                inner.playback.duration = playback::decode_hms_duration(&info.track_duration);
            }
            // See doc comment above: don't clobber a pending optimistic write.
            if vol_changed  { inner.playback.volume = info.current_volume; }
            // Safe: `mute_changed` only true when `info.current_mute.is_some()`.
            if mute_changed { inner.playback.muted  = info.current_mute.unwrap(); }

            // `source_name` stays unconditional (cheap, always correct, and
            // the Bluetooth status line needs it current immediately) —
            // only the transport-capability decode is gated.
            if source_changed || (has_content && !had_content) {
                let dev_id = inner.capabilities.as_ref().map(|c| c.device_id);
                inner.playback.source_name =
                    playback::decode_source_name_upnp(&info.play_medium, &info.track_source, dev_id);
            }
            if has_content {
                if source_changed || (has_content != had_content) {
                    let caps = playback::decode_transport_caps_upnp(
                        &info.play_medium, &info.track_source, info.play_type, info.gui_behavior,
                    );
                    dbg(self, &format!(
                        "transport caps (upnp): play_medium={:?} track_source={:?} gui_behavior={:?} -> {caps:?}",
                        info.play_medium, info.track_source, info.gui_behavior,
                    ));
                    inner.playback.caps = caps;
                }
                if title_changed  { inner.playback.title  = Rc::from(info.title.as_str()); }
                if artist_changed { inner.playback.artist = Rc::from(info.artist.as_str()); }
                if album_changed  { inner.playback.album  = Rc::from(info.album.as_str()); }
                if quality_changed {
                    let (quality, codec_label) = playback::decode_quality_upnp(
                        info.actual_quality.as_deref(),
                        &info.bitrate, &info.format_s, &info.rate_hz,
                        info.protocol_info.as_deref(),
                        &info.play_medium,
                    );
                    inner.playback.quality     = quality;
                    inner.playback.codec_label = codec_label;
                }

                let art_url = info.album_art_uri.clone().unwrap_or_default();
                let cached = inner.playback.art_url.as_deref().unwrap_or("");
                if art_url != cached || !had_content {
                    inner.playback.art_url = if art_url.is_empty() {
                        None
                    } else {
                        Some(Rc::from(art_url.as_str()))
                    };
                    Self::replace_artwork(self, &mut inner, None);
                    if art_url.is_empty() {
                        dbg(self, "upnp art url cleared: current track has no artwork");
                        art_cleared = true;
                    } else {
                        art_url_for_fetch = Some(art_url);
                    }
                }
            } else if Self::blank_playback_baseline(self, &mut inner) {
                playback_mask |= playback_changed::ALL;
            }
            inner.has_content = has_content;

            inner.upnp_info = Some(info);
        }

        // 3. Side effects, after the borrow is dropped.
        if emit_input_changed {
            dbg(self, "signal: input-changed");
            self.emit_by_name::<()>("input-changed", &[]);
        }
        if emit_inputs_changed {
            dbg(self, "signal: inputs-changed");
            self.emit_by_name::<()>("inputs-changed", &[]);
        }
        if art_cleared { playback_mask |= playback_changed::ARTWORK; }
        if let Some(url) = art_url_for_fetch {
            dbg(self, &format!("upnp art url changed: {url}"));
            self.fetch_art(url, art_tx);
        }
        if playback_mask != 0 {
            dbg(self, &format!("signal: playback-changed mask={:#x}", playback_mask));
            self.emit_by_name::<()>("playback-changed", &[&playback_mask]);
        }
    }

    fn fetch_art(&self, url: String, art_tx: &async_channel::Sender<(String, Vec<u8>)>) {
        let client = match self.imp().inner.borrow().client.clone() {
            Some(c) => c,
            None    => return,
        };
        let art_tx = art_tx.clone();
        self.rt().spawn(async move {
            // Always send, even on failure (as an empty Vec) — start_art_loader
            // treats that as "no artwork" and clears the stale texture instead
            // of the UI never hearing about the failure at all. Tagged with
            // `url` so the loader can tell whether this result is still
            // relevant by the time it lands — see its own doc comment.
            let bytes = client.fetch_bytes(&url).await.unwrap_or_default();
            let _ = art_tx.send((url, bytes)).await;
        });
    }

    // ── Input / output commands ───────────────────────────────────────────────

    /// Request an audio output hardware mode change.
    ///
    /// The cached `output_status.hardware` is updated optimistically so the UI
    /// reflects the requested state immediately.  The regular 1-second poll will
    /// detect any mismatch (e.g. USB DAC not connected) and emit `output-changed`
    /// to correct the dropdown.
    pub fn set_audio_output(&self, mode: u32) {
        let client = match self.imp().inner.borrow().client.clone() {
            Some(c) => c,
            None    => return,
        };
        if let Some(ref mut os) = self.imp().inner.borrow_mut().output_status {
            os.hardware = mode.to_string();
        }
        self.rt().spawn(async move { let _ = client.set_audio_output(mode).await; });
    }

    /// Request an input source switch.
    ///
    /// Deliberately does *not* touch `current_mode`. Instead we set a flag
    /// indicating we are changing mode and a timestamp. If the mode does
    /// change the poller will detect it and signal the UI to update. If
    /// the change times out, the poller will signal the UI to update as
    /// well causing the menu to revert.
    pub fn switch_input(&self, src: String) {
        let client = match self.imp().inner.borrow().client.clone() {
            Some(c) => c,
            None    => return,
        };
        {
            let mut inner = self.imp().inner.borrow_mut();
            inner.input_changing    = true;
            inner.input_change_time = Some(Instant::now());
        }
        self.rt().spawn(async move { let _ = client.switch_input(&src).await; });
        // TODO: trigger_poll()
    }

    /// Puts the device's Bluetooth A2DP sink back into pairing mode — the
    /// "Restart pairing" button, shown only while Bluetooth is the active
    /// input and nothing is currently connected. Fire-and-forget, same as
    /// `switch_input()`; the next `getbtstatus` slow-poll tick picks up the
    /// result once a phone/laptop actually re-pairs. UI-facing so this
    /// method exists at all — `ui/` never talks to `WiimClient` directly.
    pub fn bt_enter_pairing(&self) {
        let Some(client) = self.imp().inner.borrow().client.clone() else { return };
        self.rt().spawn(async move { let _ = client.bt_enter_pair().await; });
    }

    // ── Volume / mute commands ────────────────────────────────────────────────

    /// Branches on `mute_access`: UPnP `RenderingControl.SetMute` when the
    /// resolved backend is `UpnpPolled` (the `wiim` SDK's own precedent —
    /// see `upnp.rs`'s module doc comment), otherwise the HTTP `setMute`
    /// command. No HTTP fallback when UPnP is wanted but no client has been
    /// discovered yet — same "don't silently use the other backend"
    /// precedent `access`/`do_set_volume` already follow.
    pub fn do_set_mute(&self, muted: bool) {
        let (mute_access, client, upnp_client) = {
            // Optimistic update of the *canonical* playback.muted, for
            // instant UI feedback (a picker row, for instance, has no other
            // way to know a mute click landed before the next poll) —
            // deliberately not touching `player_status`/`GetInfoEx` cache,
            // which must stay a read-only diffing baseline written only by
            // process_poll() itself (see that field's doc comment — an
            // in-place command write there once caused the next real poll's
            // diff to silently see "no change" and skip ever correcting
            // `playback.muted` again). `last_mute_cmd` starts the settle
            // window `process_poll_http()`/`process_poll_upnp()`'s
            // self-healing `mute_changed` comparison waits out — same
            // reasoning as `do_set_volume()`'s `last_volume_cmd`/
            // `VOLUME_POLL_SETTLE`: a poll already in flight when this was
            // called can still report the pre-command value for a moment.
            let mut inner = self.imp().inner.borrow_mut();
            inner.playback.muted = muted;
            inner.last_mute_cmd = Some(Instant::now());
            (inner.mute_access, inner.client.clone(), inner.upnp_client.clone())
        };
        self.emit_by_name::<()>("playback-changed", &[&playback_changed::VOLUME]);
        match mute_access {
            AccessMethod::UpnpPolled => {
                let Some(upnp_client) = upnp_client else { return };
                self.rt().spawn(async move { let _ = upnp_client.set_mute(muted).await; });
            }
            AccessMethod::Http => {
                let Some(client) = client else { return };
                self.rt().spawn(async move { let _ = client.set_mute(muted).await; });
            }
        }
        self.trigger_poll();
    }

    pub fn do_set_volume(&self, vol: u32) {
        let (send_now, client) = {
            let mut inner = self.imp().inner.borrow_mut();
            // Optimistic update of playback.volume to avoid slider glitches
            inner.playback.volume = vol;
            let now = Instant::now();
            let since_last = inner.last_volume_cmd
                .map_or(VOLUME_DEBOUNCE, |t| now.duration_since(t));
            if since_last < VOLUME_DEBOUNCE {
                // Within the debounce window — save as pending; the 1s timer will flush it.
                inner.target_volume = vol as i32;
                (false, None)
            } else {
                // Debounce window has elapsed — send immediately.
                inner.target_volume   = -1;
                inner.last_volume_cmd = Some(now);
                (true, inner.client.clone())
            }
        };
        // Same synchronous emit `do_set_mute()` already does, and for the
        // same reason: the optimistic write above already lands in
        // canonical `playback.volume`, so the next real poll's self-heal
        // diff sees "no change" and never emits on its own — without this,
        // any listener *other* than the widget that made this call (e.g.
        // the devlist row's volume control while a device window's own
        // slider is what moved) never finds out at all.
        self.emit_by_name::<()>("playback-changed", &[&playback_changed::VOLUME]);
        if send_now {
            let Some(client) = client else { return };
            self.rt().spawn(async move { let _ = client.set_volume(vol).await; });
        }
    }

    // ── Transport commands ────────────────────────────────────────────────────

    // Optimistic "play or pause based on current cached state" — use this
    // instead of calling client().play()/pause() directly so the decision
    // is made from the same source of truth as the poll.
    pub fn do_play_pause(&self) {
        let inner = self.imp().inner.borrow();
        let Some(client) = inner.client.clone() else { return };
        // Canonical `playback.status`, not the raw HTTP `player_status`
        // cache — the latter never updates on a tick that only polled UPnP,
        // which would make this always send `play` on a UpnpPolled device.
        let playing = inner.playback.status == playback::PlaybackStatus::Playing;
        drop(inner);
        self.rt().spawn(async move {
            if playing { let _ = client.pause().await; } else { let _ = client.play().await; }
        });
        self.trigger_poll();
    }

    pub fn do_prev(&self) {
        let Some(client) = self.imp().inner.borrow().client.clone() else { return };
        self.rt().spawn(async move { let _ = client.prev().await; });
        self.trigger_poll();
    }

    pub fn do_next(&self) {
        let Some(client) = self.imp().inner.borrow().client.clone() else { return };
        self.rt().spawn(async move { let _ = client.next().await; });
        self.trigger_poll();
    }

    /// Takes canonical `(shuffle, repeat)` — `ui/` never passes a raw wire
    /// number; the encoding lives in `playback::encode_loop_mode()`, the
    /// exact inverse of the decoder every poll path already uses. Branches
    /// on `loop_mode_access`: UPnP `PlayQueue.SetQueueLoopMode` when the
    /// resolved backend is `UpnpPolled` (the default — see
    /// `Inner::loop_mode_access`'s doc comment for why), otherwise HTTP
    /// `setPlayerCmd:loopmode:N`. No HTTP fallback when UPnP is wanted but
    /// no client has been discovered yet — same "don't silently use the
    /// other backend" precedent `do_set_mute()`/`access` already follow. No
    /// optimistic UI update (unlike volume) — shuffle/repeat aren't dragged
    /// interactively, so waiting for the next poll's confirmation is fine.
    pub fn do_set_loop_mode(&self, shuffle: bool, repeat: RepeatMode) {
        let mode = playback::encode_loop_mode(shuffle, repeat);
        let (loop_mode_access, client, upnp_client) = {
            let inner = self.imp().inner.borrow();
            (inner.loop_mode_access, inner.client.clone(), inner.upnp_client.clone())
        };
        match loop_mode_access {
            AccessMethod::UpnpPolled => {
                let Some(upnp_client) = upnp_client else { return };
                self.rt().spawn(async move { let _ = upnp_client.set_queue_loop_mode(mode).await; });
            }
            AccessMethod::Http => {
                let Some(client) = client else { return };
                self.rt().spawn(async move { let _ = client.set_loop_mode(mode).await; });
            }
        }
        self.trigger_poll();
    }

    /// Trigger a one-shot status/metadata poll after issuing a device
    /// command, instead of waiting for the next regular ~1s tick. Spaced at
    /// least `POLL_SETTLE_DELAY` after whichever poll happened most
    /// recently (regular tick or a previous `trigger_poll()`) — e.g. if the
    /// last poll was 200ms ago, this fires in 200ms, not a full
    /// `POLL_SETTLE_DELAY` from now; if it's already been longer than that,
    /// this fires on the next main-loop iteration.
    fn trigger_poll(&self) {
        let now   = Instant::now();
        let delay = match self.imp().inner.borrow().last_poll {
            Some(t) => POLL_SETTLE_DELAY.saturating_sub(now.duration_since(t)),
            None    => Duration::ZERO,
        };
        let ds = self.downgrade();
        glib::timeout_add_local_once(delay, move || {
            let Some(ds) = ds.upgrade() else { return };
            ds.dispatch_fast_poll();
        });
    }

    // ── Accessors ─────────────────────────────────────────────────────────────

    pub fn rt(&self) -> Arc<tokio::runtime::Runtime> {
        self.imp().rt.get().unwrap().clone()
    }

    pub fn client(&self) -> Option<WiimClient> {
        self.imp().inner.borrow().client.clone()
    }

    /// IP the current connection is using (empty if never connected).
    pub fn ip(&self) -> String {
        self.imp().ip.borrow().clone()
    }

    /// This device's uuid, fixed at construction — unlike
    /// `device_info().map(|i| i.uuid)`, stays populated across a
    /// disconnect (see `imp::DeviceState::uuid`'s doc comment). Empty only
    /// for a `DeviceState` constructed without knowing it (a fresh
    /// `--connect`/manual add by IP).
    pub fn uuid(&self) -> String {
        self.imp().uuid.get().cloned().unwrap_or_default()
    }

    pub fn device_info(&self) -> Option<DeviceInfo> {
        self.imp().inner.borrow().device_info.clone()
    }

    pub fn capabilities(&self) -> Option<DeviceCapabilities> {
        self.imp().inner.borrow().capabilities.clone()
    }

    /// Canonical playback state, independent of which backend populated it.
    /// Cheap to clone — every heap-allocated field is `Rc`-wrapped, so this
    /// is refcount bumps only, not a deep copy.
    pub fn playback_state(&self) -> PlaybackState {
        self.imp().inner.borrow().playback.clone()
    }

    pub fn muted(&self) -> bool {
        self.imp().inner.borrow().playback.muted
    }

    pub fn get_vol(&self) -> u32 {
        self.imp().inner.borrow().playback.volume
    }

    pub fn output_status(&self) -> Option<AudioOutputStatus> {
        self.imp().inner.borrow().output_status.clone()
    }

    pub fn mode_renames(&self) -> HashMap<String, String> {
        self.imp().inner.borrow().mode_renames.clone()
    }

    /// Raw wire `mode` value from the last poll (-1 = not yet known).
    pub fn current_mode(&self) -> i32 {
        self.imp().inner.borrow().current_mode
    }

    // ── Typed signal connectors ───────────────────────────────────────────────

    pub fn connect_device_changed<F: Fn(&Self) + 'static>(&self, f: F) -> glib::SignalHandlerId {
        self.connect_local("device-changed", false, move |args| {
            f(&args[0].get::<Self>().unwrap());
            None
        })
    }

    pub fn connect_playback_changed<F: Fn(&Self, u32) + 'static>(&self, f: F) -> glib::SignalHandlerId {
        self.connect_local("playback-changed", false, move |args| {
            let ds   = args[0].get::<Self>().unwrap();
            let mask = args[1].get::<u32>().unwrap();
            f(&ds, mask);
            None
        })
    }

    pub fn connect_input_changed<F: Fn(&Self) + 'static>(&self, f: F) -> glib::SignalHandlerId {
        self.connect_local("input-changed", false, move |args| {
            f(&args[0].get::<Self>().unwrap());
            None
        })
    }

    pub fn connect_inputs_changed<F: Fn(&Self) + 'static>(&self, f: F) -> glib::SignalHandlerId {
        self.connect_local("inputs-changed", false, move |args| {
            f(&args[0].get::<Self>().unwrap());
            None
        })
    }

    pub fn connect_output_changed<F: Fn(&Self) + 'static>(&self, f: F) -> glib::SignalHandlerId {
        self.connect_local("output-changed", false, move |args| {
            f(&args[0].get::<Self>().unwrap());
            None
        })
    }

    pub fn connect_outputs_changed<F: Fn(&Self) + 'static>(&self, f: F) -> glib::SignalHandlerId {
        self.connect_local("outputs-changed", false, move |args| {
            f(&args[0].get::<Self>().unwrap());
            None
        })
    }

    pub fn connect_network_changed<F: Fn(&Self) + 'static>(&self, f: F) -> glib::SignalHandlerId {
        self.connect_local("network-changed", false, move |args| {
            f(&args[0].get::<Self>().unwrap());
            None
        })
    }

    pub fn connect_remote_changed<F: Fn(&Self) + 'static>(&self, f: F) -> glib::SignalHandlerId {
        self.connect_local("remote-changed", false, move |args| {
            f(&args[0].get::<Self>().unwrap());
            None
        })
    }

    pub fn outputs(&self) -> Vec<OutputEntry> {
        self.imp().inner.borrow().capabilities.as_ref()
            .map(|c| c.outputs.clone())
            .unwrap_or_default()
    }

    pub fn connection_state(&self) -> ConnectionState {
        self.imp().inner.borrow().connection_state
    }

    // ── Simple/Full polling mode ─────────────────────────────────────────────
    // `Simple` is the default for any `DeviceState`
    // `device::manager::DeviceManager` creates — `Full` is an opt-in,
    // refcounted upgrade an open device window acquires for as long as it
    // stays open. `device::discovery_manager`'s own tracked devices
    // deliberately never acquire `Full`, since `Simple`'s liveness+identity
    // polling is all the picker-list rendering needs.

    /// Acquire a `Full`-mode handle. Bumps the refcount immediately; `Full`
    /// mode is in effect for as long as *any* `FullModeGuard` for this
    /// device is alive, and reverts to `Simple` the moment the last one
    /// drops. Cheap and safe to call redundantly — multiple independent
    /// acquirers (e.g. two windows) just each get their own guard.
    pub fn acquire_full(&self) -> FullModeGuard {
        let n = {
            let mut inner = self.imp().inner.borrow_mut();
            inner.full_clients += 1;
            inner.full_clients
        };
        if n == 1 {
            dbg(self, &format!("Simple → Full mode (full_clients={n})"));
        }
        FullModeGuard { ds: self.clone() }
    }

    fn release_full(&self) {
        let n = {
            let mut inner = self.imp().inner.borrow_mut();
            debug_assert!(inner.full_clients > 0, "release_full() with no outstanding FullModeGuard");
            inner.full_clients = inner.full_clients.saturating_sub(1);
            inner.full_clients
        };
        if n == 0 {
            dbg(self, &format!("Full → Simple mode (full_clients={n})"));
        }
    }

    /// Whether this device is currently in `Full` mode (at least one
    /// `FullModeGuard` alive) as opposed to `Simple`.
    pub fn is_full_mode(&self) -> bool {
        self.imp().inner.borrow().full_clients > 0
    }

    /// Configure whether `Simple`-mode polling additionally fetches title/
    /// artist/artwork content, on top of the bare `getStatusEx` liveness/
    /// identity check it always does — see `Inner::simple_mode_song_info`'s
    /// doc comment. No effect while in `Full` mode. Pushed explicitly
    /// (rather than read lazily) so toggling the underlying setting takes
    /// effect immediately on an already-tracked device, not just ones
    /// created afterward.
    pub fn configure_simple_mode(&self, want_song_info: bool) {
        self.imp().inner.borrow_mut().simple_mode_song_info = want_song_info;
    }

    pub fn netstat(&self) -> Option<u32> {
        self.imp().inner.borrow().netstat
    }

    pub fn rssi(&self) -> Option<i32> {
        self.imp().inner.borrow().rssi
    }

    pub fn remote_info(&self) -> RemoteInfo {
        self.imp().inner.borrow().remote
    }

    pub fn presets(&self) -> Vec<PresetEntry> {
        self.imp().inner.borrow().presets.clone()
    }

    pub fn connect_presets_changed<F: Fn(&Self) + 'static>(&self, f: F) -> glib::SignalHandlerId {
        self.connect_local("presets-changed", false, move |args| {
            f(&args[0].get::<Self>().unwrap());
            None
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn is_ok_none(step: &PresetProbeStep) -> bool {
        matches!(step, PresetProbeStep::Ok(None))
    }
    fn is_gave_up(step: &PresetProbeStep) -> bool {
        matches!(step, PresetProbeStep::GaveUp)
    }
    fn retry_count(step: &PresetProbeStep) -> Option<u32> {
        match step {
            PresetProbeStep::Retry(n) => Some(*n),
            _ => None,
        }
    }

    #[test]
    fn preset_probe_unsupported_gives_up_immediately_regardless_of_failure_count() {
        // A confirmed "unknown command" is final on the very first attempt
        // — no retry budget consulted at all, unlike `Failed`.
        assert!(is_gave_up(&resolve_preset_probe_step(PresetFetchOutcome::Unsupported, 0)));
        assert!(is_gave_up(&resolve_preset_probe_step(PresetFetchOutcome::Unsupported, PRESET_PROBE_FAIL_THRESHOLD - 1)));
    }

    #[test]
    fn preset_probe_success_resets_regardless_of_prior_failures() {
        assert!(is_ok_none(&resolve_preset_probe_step(PresetFetchOutcome::Unchanged, 2)));
        match resolve_preset_probe_step(PresetFetchOutcome::Changed("fp".into(), Vec::new()), 2) {
            PresetProbeStep::Ok(Some((fp, entries))) => {
                assert_eq!(fp, "fp");
                assert!(entries.is_empty());
            }
            _ => panic!("expected Ok(Some(..))"),
        }
    }

    #[test]
    fn preset_probe_failed_retries_below_threshold_then_gives_up_at_threshold() {
        // Network/transient failures accumulate a strike count and are
        // only treated as final once the threshold is reached — never on
        // the first miss, unlike `Unsupported`.
        let mut failures = 0;
        for expected_next in 1..PRESET_PROBE_FAIL_THRESHOLD {
            let step = resolve_preset_probe_step(PresetFetchOutcome::Failed, failures);
            assert_eq!(retry_count(&step), Some(expected_next), "attempt {expected_next}");
            failures = expected_next;
        }
        // One more failure hits the threshold — now final, same as a
        // confirmed-unsupported response.
        assert!(is_gave_up(&resolve_preset_probe_step(PresetFetchOutcome::Failed, failures)));
    }

    #[test]
    fn resolve_preset_step_uses_retry_source_while_retrying_and_ok_source_on_success() {
        use capabilities::PresetSource;

        // Still-retrying: persist `retry_source`, not `ok_source` — this is
        // what keeps a device resolving from `Unknown` from prematurely
        // committing to `Http` mid-retry.
        match resolve_preset_step(PresetProbeStep::Retry(1), PresetSource::Unknown, PresetSource::Http) {
            Some((source, failures, None)) => {
                assert_eq!(source, PresetSource::Unknown);
                assert_eq!(failures, 1);
            }
            other => panic!("expected Some((Unknown, 1, None)), got a different shape: {}", other.is_some()),
        }
        // Success: persist `ok_source`, failure count resets to 0.
        match resolve_preset_step(PresetProbeStep::Ok(None), PresetSource::Unknown, PresetSource::Http) {
            Some((source, failures, None)) => {
                assert_eq!(source, PresetSource::Http);
                assert_eq!(failures, 0);
            }
            other => panic!("expected Some((Http, 0, None)), got a different shape: {}", other.is_some()),
        }
        // Gave up: `None` — the caller (not this function) decides what
        // happens next.
        assert!(resolve_preset_step(PresetProbeStep::GaveUp, PresetSource::Unknown, PresetSource::Http).is_none());
    }
}

