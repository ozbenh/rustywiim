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
/// Consecutive getStatusEx failures tolerated during a slow poll before the
/// connection is declared Failed. These embedded HTTP servers are flaky
/// enough that a single miss shouldn't reset the whole UI.
const SLOW_POLL_FAIL_THRESHOLD: u32 = 3;
/// How soon to retry after a getStatusEx failure, instead of waiting out the
/// full SLOW_POLL_INTERVAL.
const SLOW_POLL_FAIL_RETRY: Duration = Duration::from_secs(1);
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

fn dbg(msg: &str) {
    if DEBUG_STATE.load(Ordering::Relaxed) {
        println!("[state] {msg}");
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
    AudioOutputStatus, BtStatus, DeviceInfo, MetaData, OutputEntry, PlayerStatus,
    PresetEntry, PresetFetchOutcome, TlsMode, WiimClient, TLS_MODE,
};
use super::capabilities::{self, DeviceCapabilities};
use super::playback;
use super::playback::{AccessMethod, PlaybackState};
use super::upnp::{self, UpnpClient};

// ── Connection state ──────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ConnectionState {
    #[default]
    Disconnected,
    Connecting,
    Connected,
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
    Http { status: Option<PlayerStatus>, meta: Option<MetaData>, bt_status: Option<BtStatus> },
    Upnp { info: Option<upnp::InfoEx>, bt_status: Option<BtStatus> },
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
    /// `None` when the response wasn't a JSON array (API unsupported).
    Outputs(Option<Vec<OutputEntry>>),
    OutputStatus(Option<AudioOutputStatus>),
    DeviceInfo(Option<DeviceInfo>),
}

/// Consecutive `PresetFetchOutcome::Failed` results (network/transport
/// failure, not a confirmed-unsupported response) tolerated for whichever
/// backend is currently being attempted before giving up on it exactly as
/// a confirmed-unsupported response would. Same reasoning/value as
/// `capabilities.rs`'s `OUTPUTS_PROBE_FAIL_THRESHOLD` — these embedded
/// HTTP/UPnP servers are flaky enough that a single miss shouldn't
/// immediately be treated as "device doesn't support this."
const PRESET_PROBE_FAIL_THRESHOLD: u32 = 3;

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
            SlowPollResult::OutputStatus(client.get_audio_output().await.ok()),
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
async fn fetch_http_fast_poll(
    client: WiimClient, want_bt: bool,
) -> (Option<PlayerStatus>, Option<MetaData>, Option<BtStatus>) {
    let bt_status = if want_bt { client.get_bt_status().await } else { None };
    let skip_meta = bt_status.as_ref().is_some_and(|b| !b.connected);
    let status = client.get_status().await.ok();
    let meta = if skip_meta { None } else { client.get_meta_info().await.ok() };
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
    upnp_client: UpnpClient, client: WiimClient, want_bt: bool,
) -> (Option<upnp::InfoEx>, Option<BtStatus>) {
    let bt_status = if want_bt { client.get_bt_status().await } else { None };
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
    /// IP the current `client` was built for.  Used to detect when a fresh
    /// IP (e.g. from a DHCP lease change) actually differs from the one
    /// already connected, so `DeviceManager::update_ip()` can skip a no-op
    /// reconnect.
    ip:              String,
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
    /// Expected UUID for the current startup reconnect attempt.
    /// `None` means accept any device (user-initiated connect or already verified).
    expected_uuid:    Option<String>,
    /// Pending volume level to send on the next 1s tick (-1 = none pending).
    target_volume:    i32,
    /// When the last volume API command was sent (None = never).
    last_volume_cmd:  Option<Instant>,
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
    /// Consecutive getStatusEx failures during a slow poll while Connected.
    /// Reset to 0 on any successful getStatusEx. See SLOW_POLL_FAIL_THRESHOLD.
    slow_poll_failures: u32,
}

impl Default for Inner {
    fn default() -> Self {
        Self {
            client:          None,
            ip:              String::new(),
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
            output_status:   None,
            mode_renames:    HashMap::new(),
            current_mode:    -1,
            input_changing:      false,
            input_change_time:   None,
            netstat:          None,
            rssi:             None,
            remote:           RemoteInfo::default(),
            connection_state: ConnectionState::Disconnected,
            presets:          Vec::new(),
            preset_fp:        String::new(),
            preset_probe_failures: 0,
            pending_preset_art:  HashMap::new(),
            preset_art_inflight: HashSet::new(),
            expected_uuid:    None,
            target_volume:    -1,
            last_volume_cmd:  None,
            last_slow_poll:   None,
            last_poll:        None,
            slow_poll_active: false,
            slow_poll_phase:  SlowPollPhase::FIRST,
            slow_poll_failures: 0,
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
        pub(super) rt:            std::cell::OnceCell<Arc<tokio::runtime::Runtime>>,
        pub(super) slow_poll_tx:  RefCell<Option<async_channel::Sender<SlowPollResult>>>,
        pub(super) poll_tx:       RefCell<Option<async_channel::Sender<PollData>>>,
    }

    impl Default for DeviceState {
        fn default() -> Self {
            Self {
                inner:         RefCell::new(Inner::default()),
                rt:            std::cell::OnceCell::new(),
                slow_poll_tx:  RefCell::new(None),
                poll_tx:       RefCell::new(None),
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
            let inner = self.inner.borrow();
            let id = inner.device_info.as_ref()
                .map(|d| format!("{} ({})", d.device_name, d.ip_addr()))
                .unwrap_or_else(|| "unknown".to_string());
            dbg(&format!("DeviceState dropped: {}", id));
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

// ── Public API ────────────────────────────────────────────────────────────────

impl DeviceState {
    pub fn new(rt: Arc<tokio::runtime::Runtime>) -> Self {
        let obj: Self = glib::Object::new();
        obj.imp().rt.set(rt).unwrap();
        obj
    }

    // ── Connection ────────────────────────────────────────────────────────────

    /// Switch to a new device IP.  Clears all cached state, emits
    /// `device-changed` immediately (with cleared state so the UI can show
    /// "Connecting…"), then fetches device info asynchronously and emits
    /// `device-changed` again when the data arrives.
    ///
    /// `expected_uuid` — when `Some`, the UUID reported by the device must
    /// match; on mismatch the connection is aborted and state reverts to
    /// `Disconnected` so the caller can try a different IP.  Pass `None` for
    /// user-initiated connects where the right device is already known.
    ///
    /// `access_override`/`mute_access_override` are established here, up
    /// front — not via a separate, later call to
    /// `set_playback_access_override()`/`set_mute_access_override()` that
    /// has to land before the first poll tick to matter. There's no window
    /// where this `DeviceState` exists with the wrong override, because
    /// there's no point at which it exists without one at all. Since this
    /// resets *everything* (`*inner = Inner::default()`), including
    /// whatever overrides an already-connected `DeviceState` had, a caller
    /// reconnecting an existing instance (`DeviceManager::update_ip()`)
    /// must read the current values first (`playback_access_override()`/
    /// `mute_access_override()`) and pass them back in, not just fresh
    /// defaults.
    pub fn set_device(
        &self,
        ip: &str,
        tls: TlsMode,
        expected_uuid: Option<&str>,
        access_override: Option<AccessMethod>,
        mute_access_override: Option<AccessMethod>,
    ) {
        // Apply --tls CLI override if set; otherwise use the caller-supplied mode.
        let tls = {
            let global = TlsMode::from_usize(TLS_MODE.load(Ordering::Relaxed));
            if global != TlsMode::Auto { global } else { tls }
        };
        dbg(&format!("set_device: connecting to {ip} ({})", tls.description()));
        {
            let mut inner = self.imp().inner.borrow_mut();
            *inner = Inner::default();
            inner.client           = Some(WiimClient::new(ip, tls));
            inner.ip                = ip.to_string();
            inner.connection_state = ConnectionState::Connecting;
            inner.expected_uuid    = expected_uuid.map(String::from);
            inner.access_override  = access_override;
            inner.mute_access_override = mute_access_override;
        }
        self.recompute_access();
        dbg("signal: device-changed (connecting)");
        self.emit_by_name::<()>("device-changed", &[]);
        self.fetch_device_info();
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

            let Some(FetchOk { info, caps, renames }) = payload else {
                ds.imp().inner.borrow_mut().connection_state = ConnectionState::Failed;
                dbg("signal: device-changed (failed)");
                ds.emit_by_name::<()>("device-changed", &[]);
                return;
            };
            // If we were given an expected UUID (startup reconnect), verify it
            // before accepting the connection.  On mismatch the device at this
            // IP is not ours; drop back to Disconnected so discovery can
            // reconnect to the right IP by UUID.
            let expected_uuid = ds.imp().inner.borrow().expected_uuid.clone();
            if let Some(expected) = expected_uuid {
                if info.uuid != expected {
                    dbg(&format!(
                        "UUID mismatch: expected {:?}, got {:?}; aborting connection",
                        expected, info.uuid,
                    ));
                    *ds.imp().inner.borrow_mut() = Inner::default();
                    ds.emit_by_name::<()>("device-changed", &[]);
                    return;
                }
            }
            dbg(&format!(
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
                inner.presets           = Vec::new();
                inner.pending_preset_art.clear();
                inner.preset_art_inflight.clear();
                inner.connection_state  = ConnectionState::Connected;
                inner.slow_poll_failures = 0;
            }
            ds.recompute_access();
            dbg("signal: device-changed (ready)");
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
    /// Also recomputes `mute_access` alongside `access`. Unlike `access`,
    /// `mute_access`'s base isn't sourced from a per-`FamilyProfile` field —
    /// only one device family has ever needed a different mute backend
    /// (iEAST AudioCast), and the per-device Settings override already
    /// covers that exception, so the base here is just the global
    /// `AccessMethod::UpnpPolled` default rather than a second capability
    /// axis.
    fn recompute_access(&self) {
        let wants_upnp = {
            let mut inner = self.imp().inner.borrow_mut();
            let base = inner.capabilities.as_ref()
                .map(|c| c.playback_access())
                .unwrap_or(AccessMethod::Http);
            inner.access = inner.access_override.unwrap_or(base);
            inner.mute_access = inner.mute_access_override.unwrap_or(AccessMethod::UpnpPolled);
            // Debug-only visibility aid for diagnosing a device where UPnP
            // discovery/`GetInfoEx` never succeeds (playback state silently
            // stays on whatever it last held, since the poll loop only
            // overwrites it when a `GetInfoEx` response actually arrives).
            if DEBUG_STATE.load(Ordering::Relaxed) && inner.access == AccessMethod::UpnpPolled {
                dbg("access config: set to UpnpPolled");
            }
            inner.access == AccessMethod::UpnpPolled || inner.mute_access == AccessMethod::UpnpPolled
        };
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
        let ip = {
            let inner = self.imp().inner.borrow();
            if inner.upnp_client.is_some() || inner.upnp_discovery_in_flight {
                return;
            }
            inner.ip.clone()
        };
        if ip.is_empty() {
            return;
        }
        self.imp().inner.borrow_mut().upnp_discovery_in_flight = true;
        dbg("upnp: starting control-URL discovery");

        let (tx, rx) = async_channel::bounded(1);
        self.rt().spawn(async move {
            let result = UpnpClient::discover(&ip).await;
            let _ = tx.send(result).await;
        });

        let ds = self.downgrade();
        glib::spawn_future_local(async move {
            let Ok(result) = rx.recv().await else { return };
            let Some(ds) = ds.upgrade() else { return };
            let mut inner = ds.imp().inner.borrow_mut();
            inner.upnp_discovery_in_flight = false;
            match result {
                Ok(client) => {
                    dbg("upnp: discovery succeeded");
                    inner.upnp_client = Some(client);
                }
                Err(e) => dbg(&format!("upnp: discovery failed: {e}")),
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

        *self.imp().slow_poll_tx.borrow_mut() = Some(slow_tx.clone());

        self.start_unified_timer(poll_tx, slow_tx);
        self.start_poll_processor(poll_rx, art_tx);
        self.start_art_loader(art_rx);
        self.start_slow_poll_processor(slow_rx);
    }

    fn start_unified_timer(
        &self,
        poll_tx: async_channel::Sender<PollData>,
        slow_tx: async_channel::Sender<SlowPollResult>,
    ) {
        *self.imp().poll_tx.borrow_mut() = Some(poll_tx.clone());
        let ds_weak = self.downgrade();
        glib::timeout_add_local(Duration::from_secs(1), move || {
            let Some(ds) = ds_weak.upgrade() else { return glib::ControlFlow::Break };
            ds.do_poll(&poll_tx, &slow_tx)
        });
    }

    /// Fires once per second while Connected or Failed (a no-op tick while
    /// Disconnected/Connecting). Reads everything this tick needs to decide
    /// from `Inner` in one borrow — several interrelated pieces of state
    /// that don't split cleanly without just moving the borrow-juggling
    /// into another function's parameter list — then, once the borrow is
    /// dropped, hands off to a focused helper per action: reconnecting, the
    /// fast poll, one slow-poll phase.
    fn do_poll(
        &self,
        poll_tx: &async_channel::Sender<PollData>,
        slow_tx: &async_channel::Sender<SlowPollResult>,
    ) -> glib::ControlFlow {
        let mut inner = self.imp().inner.borrow_mut();
        let state = inner.connection_state;

        // Nothing to do while not yet connected or deliberately disconnected.
        if matches!(state, ConnectionState::Disconnected | ConnectionState::Connecting) {
            return glib::ControlFlow::Continue;
        }

        let now = Instant::now();

        // Is it time to start a new slow-poll cycle / reconnect attempt?
        let cycle_due = inner.last_slow_poll
            .map_or(true, |t| now.duration_since(t) >= SLOW_POLL_INTERVAL);

        // Flush any pending volume command if the debounce window has elapsed.
        let pending_vol = if state == ConnectionState::Connected
            && inner.target_volume >= 0
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

        let do_reconnect   = cycle_due && state == ConnectionState::Failed;
        let dispatch_phase = self.advance_slow_poll_rotation(&mut inner, state, cycle_due, now);

        let client        = inner.client.clone();
        // `probes_outputs`/`preset_source` are read straight off
        // `capabilities` (set by `capabilities::detect_capabilities()`/
        // persisted there for the connection's lifetime); `preset_probe_failures`
        // is a short-lived retry counter that isn't part of the device's
        // identity, so it lives directly on `Inner` instead (see its doc
        // comment) — `capabilities.rs` only ever records `preset_source`.
        let probe_outputs = inner.capabilities.as_ref().is_some_and(|c| c.probes_outputs);
        let preset_source = inner.capabilities.as_ref()
            .map(|c| c.preset_source())
            .unwrap_or(capabilities::PresetSource::Unknown);
        let preset_probe_failures = inner.preset_probe_failures;
        let preset_fp     = inner.preset_fp.clone();
        let upnp_client   = inner.upnp_client.clone();
        drop(inner);

        // Reconnect when Failed and the interval has elapsed.
        if do_reconnect {
            self.try_reconnect(client);
            return glib::ControlFlow::Continue;
        }

        let Some(client) = client else { return glib::ControlFlow::Continue };

        // Send any deferred volume command first.
        if let Some(vol) = pending_vol {
            let cv = client.clone();
            self.rt().spawn(async move { let _ = cv.set_volume(vol).await; });
        }

        self.dispatch_fast_poll();
        self.dispatch_slow_poll(
            &client, slow_tx, dispatch_phase, probe_outputs,
            preset_source, preset_probe_failures, preset_fp, upnp_client,
        );
        self.dispatch_pending_preset_art(&client, poll_tx);

        glib::ControlFlow::Continue
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
            dbg(&format!(
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

    /// Begin a reconnect attempt: transition to Connecting and re-run
    /// `fetch_device_info()` against the same client/IP. No-op if there's
    /// no client at all (shouldn't normally happen while Failed).
    fn try_reconnect(&self, client: Option<WiimClient>) {
        if client.is_some() {
            dbg("reconnect attempt: transitioning Connecting");
            self.imp().inner.borrow_mut().connection_state = ConnectionState::Connecting;
            self.emit_by_name::<()>("device-changed", &[]);
            self.fetch_device_info();
        }
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
        let (wants_upnp, upnp_client, want_bt, client) = {
            let inner = self.imp().inner.borrow();
            let want_bt = capabilities::mode_to_input_source(inner.current_mode) == "bluetooth";
            (inner.access == AccessMethod::UpnpPolled, inner.upnp_client.clone(), want_bt, inner.client.clone())
        };
        let Some(client) = client else { return };

        match (wants_upnp, upnp_client) {
            (true, None) => {
                // Selected but not ready yet — see doc comment above.
            }
            (true, Some(uc)) => {
                self.imp().inner.borrow_mut().last_poll = Some(Instant::now());
                self.rt().spawn(async move {
                    let (info, bt_status) = fetch_upnp_fast_poll(uc, client, want_bt).await;
                    let _ = poll_tx.send(PollData::Upnp { info, bt_status }).await;
                });
            }
            (false, _) => {
                self.imp().inner.borrow_mut().last_poll = Some(Instant::now());
                self.rt().spawn(async move {
                    let (status, meta, bt_status) = fetch_http_fast_poll(client, want_bt).await;
                    let _ = poll_tx.send(PollData::Http { status, meta, bt_status }).await;
                });
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
        preset_source:         capabilities::PresetSource,
        preset_probe_failures: u32,
        preset_fp:             String,
        upnp_client:           Option<UpnpClient>,
    ) {
        let Some(phase) = dispatch_phase else { return };
        let enabled = match phase {
            SlowPollPhase::Outputs => probe_outputs,
            SlowPollPhase::Presets => preset_source != capabilities::PresetSource::Unavailable,
            SlowPollPhase::OutputStatus | SlowPollPhase::DeviceInfo => true,
        };
        if !enabled {
            dbg(&format!("slow poll: phase {phase:?} skipped (not supported)"));
            return;
        }
        dbg(&format!("slow poll: phase {phase:?}"));
        let cp = client.clone();
        let tx = slow_tx.clone();
        let dispatched_at = Instant::now();
        self.rt().spawn(async move {
            let result = run_slow_poll_phase(
                cp, phase, preset_fp, upnp_client, preset_source, preset_probe_failures,
            ).await;
            // Every phase here is one or two calls straight to the device
            // itself, so this should always be fast — logged (round-trip,
            // not just "dispatched") so a phase that's unexpectedly slow
            // shows up rather than just being an unexplained delay.
            let elapsed = dispatched_at.elapsed();
            if elapsed > Duration::from_secs(1) {
                dbg(&format!("slow poll: phase {phase:?} took {elapsed:?} (slower than usual)"));
            }
            let _ = tx.send(result).await;
        });
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
            dbg(&format!("preset art: fetching slot {slot} ({url})"));
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
                        Self::replace_artwork(&mut inner, None); // leak-check the outgoing value first
                        if bytes.is_empty() {
                            dbg("artwork fetch failed; clearing stale art");
                        } else {
                            dbg(&format!("artwork loaded: {} bytes", bytes.len()));
                            inner.playback.artwork = Some(Rc::new(bytes));
                        }
                        true
                    }
                };
                if applied {
                    dbg("signal: playback-changed (artwork)");
                    ds.emit_by_name::<()>("playback-changed", &[&playback_changed::ARTWORK]);
                } else {
                    dbg(&format!("artwork fetch result for stale/superseded url ignored: {url}"));
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
    fn replace_artwork(inner: &mut Inner, new: Option<Rc<Vec<u8>>>) {
        if let Some(old) = inner.playback.artwork.take() {
            let refs = Rc::strong_count(&old);
            if refs > 1 {
                dbg(&format!(
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
    fn blank_playback_baseline(inner: &mut Inner) -> bool {
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
            Self::replace_artwork(inner, None);
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
    fn apply_mode_change(inner: &mut Inner, new_mode: i32) {
        inner.current_mode = new_mode;
        Self::blank_playback_baseline(inner);
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
                }
            }
        }
    }

    /// Shared by `process_poll_http()`/`process_poll_upnp()`'s mode-change
    /// handling. Returns whether the caller should emit `input-changed` —
    /// true either for a real, confirmed mode change, or for a timed-out
    /// switch that needs reverting.
    fn handle_input_mode_poll(inner: &mut Inner, mode_changed: bool, new_mode: i32) -> bool {
        if mode_changed {
            Self::apply_mode_change(inner, new_mode);
        } else {
            let Some(sent) = inner.input_change_time else { return false };
            if !inner.input_changing || sent.elapsed() < INPUT_CHANGE_TIMEOUT {
                return false;
            }
            eprintln!("[state] timeout changing input");
        }
        inner.input_changing = false;
        true
    }

    fn start_slow_poll_processor(&self, rx: async_channel::Receiver<SlowPollResult>) {
        let ds_weak = self.downgrade();
        glib::spawn_future_local(async move {
            while let Ok(result) = rx.recv().await {
                let Some(ds) = ds_weak.upgrade() else { break };
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
            dbg("slow poll: presets unchanged");
            return;
        };
        dbg(&format!("slow poll: presets updated: {} slots", entries.len()));
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
        dbg("signal: presets-changed");
        self.emit_by_name::<()>("presets-changed", &[]);
    }

    /// Reports one slow-poll `getSoundCardModeSupportList` result to
    /// `capabilities::DeviceCapabilities::record_outputs_probe()`, which
    /// owns the actual give-up/failure-counting policy — this is just the
    /// thin reporting + signal-emitting wrapper. `state.rs` never sees a
    /// failure counter or threshold.
    fn handle_slow_poll_outputs(&self, outputs: Option<Vec<OutputEntry>>) {
        let mut inner = self.imp().inner.borrow_mut();
        let Some(caps) = inner.capabilities.as_mut() else { return };
        let changed = caps.record_outputs_probe(outputs);
        drop(inner);
        if changed {
            dbg("signal: outputs-changed");
            self.emit_by_name::<()>("outputs-changed", &[]);
        }
    }

    fn handle_slow_poll_output_status(&self, status: Option<AudioOutputStatus>) {
        let Some(out) = status else {
            dbg("slow poll: getNewAudioOutputHardwareMode failed");
            return;
        };
        let (changed, prev_hw) = {
            let inner = self.imp().inner.borrow();
            let prev_hw = inner.output_status.as_ref().map(|o| o.hardware.clone());
            let changed = prev_hw.as_deref() != Some(out.hardware.as_str());
            (changed, prev_hw)
        };
        if changed {
            dbg(&format!(
                "slow poll: output changed: {} → {}",
                prev_hw.as_deref().unwrap_or("none"), out.hardware,
            ));
        } else {
            dbg(&format!("slow poll: output status unchanged: {}", out.hardware));
        }
        self.imp().inner.borrow_mut().output_status = Some(out);
        if changed {
            dbg("signal: output-changed");
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
        // getStatusEx failed. Tolerate a few consecutive misses (these
        // embedded HTTP servers are flaky) before declaring the connection
        // Failed — clearing device_info on every transient blip needlessly
        // resets the whole UI (e.g. the output selector, see the bug this
        // was fixing) for something that usually self-heals a second later.
        let Some(new_info) = info else {
            if self.imp().inner.borrow().connection_state == ConnectionState::Connected {
                let declared_failed = {
                    let mut inner = self.imp().inner.borrow_mut();
                    inner.slow_poll_failures += 1;
                    if inner.slow_poll_failures >= SLOW_POLL_FAIL_THRESHOLD {
                        dbg(&format!(
                            "slow poll: getStatusEx failed {} times in a row; transitioning to Failed",
                            inner.slow_poll_failures,
                        ));
                        inner.connection_state = ConnectionState::Failed;
                        inner.device_info      = None;
                        true
                    } else {
                        dbg(&format!(
                            "slow poll: getStatusEx failed ({}/{SLOW_POLL_FAIL_THRESHOLD}); retrying in {}s",
                            inner.slow_poll_failures, SLOW_POLL_FAIL_RETRY.as_secs(),
                        ));
                        // Rewind last_slow_poll so the next 1s tick retries
                        // immediately instead of waiting out the full
                        // SLOW_POLL_INTERVAL.
                        inner.last_slow_poll =
                            Instant::now().checked_sub(SLOW_POLL_INTERVAL - SLOW_POLL_FAIL_RETRY);
                        false
                    }
                };
                if declared_failed {
                    self.emit_by_name::<()>("device-changed", &[]);
                }
            }
            return;
        };
        dbg("slow poll: getStatusEx ok");

        let (prev_fw, prev_uuid, prev_name, prev_netstat, prev_rssi, prev_remote) = {
            let inner = self.imp().inner.borrow();
            let di = inner.device_info.as_ref();
            (
                di.map(|i| i.firmware.clone()).unwrap_or_default(),
                di.map(|i| i.uuid.clone()).unwrap_or_default(),
                di.map(|i| i.device_name.clone()).unwrap_or_default(),
                inner.netstat,
                inner.rssi,
                inner.remote,
            )
        };

        // UUID change means the underlying device has been replaced on the
        // same IP.  Do a full re-init rather than a partial identity update.
        if !prev_uuid.is_empty() && new_info.uuid != prev_uuid {
            dbg(&format!(
                "slow poll: UUID changed ({} → {}); resetting connection",
                prev_uuid, new_info.uuid,
            ));
            let (client, ip) = {
                let inner = self.imp().inner.borrow();
                (inner.client.clone(), inner.ip.clone())
            };
            {
                let mut inner = self.imp().inner.borrow_mut();
                *inner = Inner::default();
                inner.client           = client;
                inner.ip                = ip;
                inner.connection_state = ConnectionState::Connecting;
            }
            self.emit_by_name::<()>("device-changed", &[]);
            self.fetch_device_info();
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
            inner.slow_poll_failures = 0;
            if identity_changed {
                dbg(&format!(
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
            dbg(&format!(
                "signal: network changed: netstat={} rssi={}",
                self.imp().inner.borrow().netstat.unwrap_or(0),
                self.imp().inner.borrow().rssi.unwrap_or(0),
            ));
            self.emit_by_name::<()>("network-changed", &[]);
        }
        if remote_changed {
            dbg(&format!("signal: remote changed: {:?}", self.imp().inner.borrow().remote));
            self.emit_by_name::<()>("remote-changed", &[]);
        }
    }

    /// Dispatch to whichever backend actually produced this tick's data —
    /// `PollData::Http`/`PollData::Upnp` are mutually exclusive (see
    /// `PollData`'s doc comment), so exactly one of these runs per tick,
    /// never both.
    fn process_poll(&self, data: PollData, art_tx: &async_channel::Sender<(String, Vec<u8>)>) {
        match data {
            PollData::Http { status, meta, bt_status } => {
                self.process_poll_http(status, meta, bt_status, art_tx);
            }
            PollData::Upnp { info, bt_status } => {
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
                dbg(&format!("preset art: slot {slot} loaded ({url})"));
                dbg("signal: presets-changed");
                self.emit_by_name::<()>("presets-changed", &[]);
            }
            None => {
                let attempts = attempts + 1;
                if attempts >= PRESET_ART_MAX_ATTEMPTS {
                    dbg(&format!("preset art: slot {slot} failed {attempts} times, giving up ({url})"));
                    inner.pending_preset_art.remove(&slot);
                } else {
                    dbg(&format!("preset art: slot {slot} failed (attempt {attempts}/{PRESET_ART_MAX_ATTEMPTS}), will retry ({url})"));
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
                let mute_changed = prev.map_or(true, |p| p.mute != st.mute);
                // Volume is the one field with an optimistic write
                // (`do_set_volume`, for slider responsiveness while
                // dragging), so a plain diff against the *previous* raw
                // response isn't enough: if a `SetVolume` command silently
                // failed to stick device-side, the device's own answer
                // never changes between polls, so that diff would never
                // fire and `playback.volume` would stay wrong forever.
                // Instead, resync straight against the device's answer
                // whenever nothing we sent is still in flight
                // (`target_volume < 0`) — this self-heals a rejected/
                // clamped command exactly the same way it picks up a
                // genuine remote change (physical remote, another app,
                // slave-speaker sync): both look like "device says X,
                // canonical state says Y" from here. Also gated on
                // `VOLUME_POLL_SETTLE` — a real device can keep reporting
                // its pre-command volume for a moment after accepting a
                // `SetVolume`, so `target_volume < 0` (command sent) alone
                // isn't enough; see that constant's doc comment.
                let vol_settled = inner.last_volume_cmd
                    .map_or(true, |t| Instant::now().duration_since(t) >= VOLUME_POLL_SETTLE);
                let vol_changed = inner.target_volume < 0 && vol_settled && st.vol != inner.playback.volume;
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
                dbg(&format!(
                    "input changed: mode {} → {} (status={})",
                    prev_mode, st.mode, st.status,
                ));
            }
            if !timing_valid {
                dbg(&format!(
                    "timing: ignoring garbage reading (curpos={} > totlen={})",
                    st.curpos, st.totlen,
                ));
            }

            // 2. Borrow_mut: decode only what changed, straight into `playback`.
            let emit_input_changed;
            {
                let mut inner = self.imp().inner.borrow_mut();
                emit_input_changed = Self::handle_input_mode_poll(&mut inner, mode_changed, st.mode);
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
                    inner.playback.source_name = playback::decode_source_name_http(st.mode, &st.vendor);
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
                        dbg(&format!(
                            "transport caps (http): mode={} vendor={:?} -> {caps:?}",
                            st.mode, st.vendor,
                        ));
                        inner.playback.caps = caps;
                        playback_mask |= playback_changed::OTHER;
                    }
                } else if Self::blank_playback_baseline(&mut inner) {
                    playback_mask |= playback_changed::ALL;
                }
                inner.has_content = has_content;

                inner.player_status = Some(st);
            }

            // 3. Side effects, after the borrow is dropped.
            if emit_input_changed {
                dbg("signal: input-changed");
                self.emit_by_name::<()>("input-changed", &[]);
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
                        Self::replace_artwork(&mut inner, None);
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
                    dbg("art url cleared: current track has no artwork");
                    playback_mask |= playback_changed::ARTWORK;
                } else {
                    dbg(&format!("art url changed: {art_url}"));
                    // No immediate ARTWORK signal here: artwork is already
                    // cleared, but we hold off telling the UI until fetch_art()
                    // resolves (success or failure — see start_art_loader) so a
                    // fast reload doesn't flash the fallback icon in between.
                    self.fetch_art(art_url, art_tx);
                }
            }
        }

        if playback_mask != 0 {
            dbg(&format!("signal: playback-changed mask={:#x}", playback_mask));
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
            // as a change, and never written below.
            let mute_changed = info.current_mute.is_some()
                && prev.map_or(true, |p| p.current_mute != info.current_mute);
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
            dbg(&format!("input changed (upnp): mode {prev_mode} → {}", info.play_type));
        }

        let mut art_url_for_fetch: Option<String> = None;
        let mut art_cleared = false;

        // 2. Borrow_mut: decode only what changed, straight into `playback`.
        let emit_input_changed;
        {
            let mut inner = self.imp().inner.borrow_mut();
            emit_input_changed = Self::handle_input_mode_poll(&mut inner, mode_changed, info.play_type);

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
                inner.playback.source_name =
                    playback::decode_source_name_upnp(&info.play_medium, &info.track_source);
            }
            if has_content {
                if source_changed || (has_content != had_content) {
                    let caps = playback::decode_transport_caps_upnp(
                        &info.play_medium, &info.track_source, info.play_type, info.gui_behavior,
                    );
                    dbg(&format!(
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
                    Self::replace_artwork(&mut inner, None);
                    if art_url.is_empty() {
                        dbg("upnp art url cleared: current track has no artwork");
                        art_cleared = true;
                    } else {
                        art_url_for_fetch = Some(art_url);
                    }
                }
            } else if Self::blank_playback_baseline(&mut inner) {
                playback_mask |= playback_changed::ALL;
            }
            inner.has_content = has_content;

            inner.upnp_info = Some(info);
        }

        // 3. Side effects, after the borrow is dropped.
        if emit_input_changed {
            dbg("signal: input-changed");
            self.emit_by_name::<()>("input-changed", &[]);
        }
        if art_cleared { playback_mask |= playback_changed::ARTWORK; }
        if let Some(url) = art_url_for_fetch {
            dbg(&format!("upnp art url changed: {url}"));
            self.fetch_art(url, art_tx);
        }
        if playback_mask != 0 {
            dbg(&format!("signal: playback-changed mask={:#x}", playback_mask));
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
            let inner = self.imp().inner.borrow();
            (inner.mute_access, inner.client.clone(), inner.upnp_client.clone())
        };
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
        let mut inner = self.imp().inner.borrow_mut();
        // Optimistic update of playback.volume to avoid slider glitches
        inner.playback.volume = vol;
        let now = Instant::now();
        let since_last = inner.last_volume_cmd
            .map_or(VOLUME_DEBOUNCE, |t| now.duration_since(t));
        if since_last < VOLUME_DEBOUNCE {
            // Within the debounce window — save as pending; the 1s timer will flush it.
            inner.target_volume = vol as i32;
            return;
        }
        // Debounce window has elapsed — send immediately.
        inner.target_volume   = -1;
        inner.last_volume_cmd = Some(now);
        let Some(client) = inner.client.clone() else { return };
        drop(inner);
        self.rt().spawn(async move { let _ = client.set_volume(vol).await; });
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

    pub fn do_set_loop_mode(&self, mode: i32) {
        let Some(client) = self.imp().inner.borrow().client.clone() else { return };
        self.rt().spawn(async move { let _ = client.set_loop_mode(mode).await; });
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
        self.imp().inner.borrow().ip.clone()
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

