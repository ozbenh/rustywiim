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
use std::collections::HashMap;
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
/// `trigger_poll()`'s one-shot follow-up poll is spaced at least this long
/// after whichever poll happened most recently — long enough that a real
/// device has almost certainly applied the command that prompted it.
const POLL_SETTLE_DELAY: Duration = Duration::from_millis(400);

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
    AudioOutputStatus, DeviceInfo, MetaData, OutputEntry, PlayerStatus,
    PresetEntry, TlsMode, WiimClient, TLS_MODE,
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

/// A fast-poll tick's result — HTTP and UPnP are mutually exclusive per
/// tick, never both: `dispatch_fast_poll()` decides which single backend to
/// hit *before* firing anything, based on `access`, rather than always
/// fetching HTTP as a baseline and optionally layering UPnP on top. No HTTP
/// fallback when `UpnpPolled` is selected but no `UpnpClient` has been
/// discovered yet — see `dispatch_fast_poll`'s doc comment.
enum PollData {
    Http { status: Option<PlayerStatus>, meta: Option<MetaData> },
    Upnp { info: Option<upnp::InfoEx> },
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
    /// `Some((fp, entries))` when the fingerprint changed; `None` when
    /// unchanged.
    Presets(Option<(String, Vec<PresetEntry>)>),
    /// `None` when the response wasn't a JSON array (API unsupported).
    Outputs(Option<Vec<OutputEntry>>),
    OutputStatus(Option<AudioOutputStatus>),
    DeviceInfo(Option<DeviceInfo>),
}

async fn run_slow_poll_phase(
    client:    WiimClient,
    phase:     SlowPollPhase,
    preset_fp: String,
) -> SlowPollResult {
    match phase {
        SlowPollPhase::Presets =>
            SlowPollResult::Presets(client.fetch_presets(&preset_fp).await),
        SlowPollPhase::Outputs =>
            SlowPollResult::Outputs(client.get_sound_card_mode_support_list().await),
        SlowPollPhase::OutputStatus =>
            SlowPollResult::OutputStatus(client.get_audio_output().await.ok()),
        SlowPollPhase::DeviceInfo =>
            SlowPollResult::DeviceInfo(client.get_device_info().await.ok()),
    }
}

/// The HTTP fast poll: `getPlayerStatusEx` + `getMetaInfo`. Only called by
/// `dispatch_fast_poll`/`trigger_poll` when `access == AccessMethod::Http`
/// — a `UpnpPolled` device never runs this (see `PollData`'s doc comment),
/// so `inner.player_status`/`inner.metadata` simply stop updating while
/// UPnP is selected.
async fn fetch_http_fast_poll(client: WiimClient) -> (Option<PlayerStatus>, Option<MetaData>) {
    let status = client.get_status().await.ok();
    let meta   = client.get_meta_info().await.ok();
    (status, meta)
}

/// The UPnP fast poll: `GetInfoEx` on an already-discovered `UpnpClient`.
/// The caller (`dispatch_fast_poll`/`trigger_poll`) only ever calls this
/// once it has confirmed a client exists — HTTP and UPnP are mutually
/// exclusive per tick, so there's no HTTP call running alongside this one
/// to worry about serializing against.
async fn fetch_upnp_fast_poll(upnp_client: UpnpClient) -> Option<upnp::InfoEx> {
    upnp_client.get_info_ex().await.ok()
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
    output_status:   Option<AudioOutputStatus>,
    mode_renames:    HashMap<String, String>,
    /// Raw wire `mode` value from the last poll; -1 = not known
    current_mode:    i32,
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
            access:          AccessMethod::Http,
            access_override: None,
            output_status:   None,
            mode_renames:    HashMap::new(),
            current_mode:    -1,
            netstat:          None,
            rssi:             None,
            remote:           RemoteInfo::default(),
            connection_state: ConnectionState::Disconnected,
            presets:          Vec::new(),
            preset_fp:        String::new(),
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
    /// `access_override` is established here, up front — not via a separate,
    /// later call to `set_playback_access_override()` that has to land
    /// before the first poll tick to matter. There's no window where this
    /// `DeviceState` exists with the wrong override, because there's no
    /// point at which it exists without one at all. Since this resets
    /// *everything* (`*inner = Inner::default()`), including whatever
    /// override an already-connected `DeviceState` had, a caller
    /// reconnecting an existing instance (`DeviceManager::update_ip()`)
    /// must read the current value first (`playback_access_override()`)
    /// and pass it back in, not just a fresh default.
    pub fn set_device(
        &self,
        ip: &str,
        tls: TlsMode,
        expected_uuid: Option<&str>,
        access_override: Option<AccessMethod>,
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
                inner.presets           = Vec::new();
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
    fn recompute_access(&self) {
        let wants_upnp = {
            let mut inner = self.imp().inner.borrow_mut();
            let base = inner.capabilities.as_ref()
                .map(|c| c.playback_access())
                .unwrap_or(AccessMethod::Http);
            inner.access = inner.access_override.unwrap_or(base);
            // Debug-only visibility aid for diagnosing a device where UPnP
            // discovery/`GetInfoEx` never succeeds (playback state silently
            // stays on whatever it last held, since the poll loop only
            // overwrites it when a `GetInfoEx` response actually arrives).
            if DEBUG_STATE.load(Ordering::Relaxed) && inner.access == AccessMethod::UpnpPolled {
                dbg("access config: set to UpnpPolled");
            }
            inner.access == AccessMethod::UpnpPolled
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

    // ── Polling ───────────────────────────────────────────────────────────────

    /// Start the unified 1-second timer plus background result processors.
    /// The timer handles fast polls every tick, one slow-poll phase per tick
    /// during a rotation started every SLOW_POLL_INTERVAL (see
    /// `SlowPollPhase`), pending volume commands, and reconnection attempts.
    /// Call once after `new()`.
    pub fn start_polling(&self) {
        let (poll_tx, poll_rx) = async_channel::unbounded::<PollData>();
        let (slow_tx, slow_rx) = async_channel::unbounded::<SlowPollResult>();
        let (art_tx,  art_rx)  = async_channel::unbounded::<Vec<u8>>();

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
        // Read straight off `capabilities` rather than a separate cached
        // bool in `Inner` — `supports_presets` was already purely
        // static/redundant with capabilities, and `probes_outputs` now
        // lives there too (set by `capabilities::detect_capabilities()`).
        let probe_outputs = inner.capabilities.as_ref().is_some_and(|c| c.probes_outputs);
        let probe_presets = inner.capabilities.as_ref().is_some_and(|c| c.supports_presets);
        let preset_fp     = inner.preset_fp.clone();
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

        self.dispatch_fast_poll(&client, poll_tx);
        self.dispatch_slow_poll(&client, slow_tx, dispatch_phase, probe_outputs, probe_presets, preset_fp);

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
    fn dispatch_fast_poll(&self, client: &WiimClient, poll_tx: &async_channel::Sender<PollData>) {
        let (wants_upnp, upnp_client) = {
            let inner = self.imp().inner.borrow();
            (inner.access == AccessMethod::UpnpPolled, inner.upnp_client.clone())
        };

        match (wants_upnp, upnp_client) {
            (true, None) => {
                // Selected but not ready yet — see doc comment above.
            }
            (true, Some(uc)) => {
                self.imp().inner.borrow_mut().last_poll = Some(Instant::now());
                let tx = poll_tx.clone();
                self.rt().spawn(async move {
                    let info = fetch_upnp_fast_poll(uc).await;
                    let _ = tx.send(PollData::Upnp { info }).await;
                });
            }
            (false, _) => {
                self.imp().inner.borrow_mut().last_poll = Some(Instant::now());
                let cp = client.clone();
                let tx = poll_tx.clone();
                self.rt().spawn(async move {
                    let (status, meta) = fetch_http_fast_poll(cp).await;
                    let _ = tx.send(PollData::Http { status, meta }).await;
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
        client:         &WiimClient,
        slow_tx:        &async_channel::Sender<SlowPollResult>,
        dispatch_phase: Option<SlowPollPhase>,
        probe_outputs:  bool,
        probe_presets:  bool,
        preset_fp:      String,
    ) {
        let Some(phase) = dispatch_phase else { return };
        let enabled = match phase {
            SlowPollPhase::Outputs => probe_outputs,
            SlowPollPhase::Presets => probe_presets,
            SlowPollPhase::OutputStatus | SlowPollPhase::DeviceInfo => true,
        };
        if !enabled {
            dbg(&format!("slow poll: phase {phase:?} skipped (not supported)"));
            return;
        }
        dbg(&format!("slow poll: phase {phase:?}"));
        let cp = client.clone();
        let tx = slow_tx.clone();
        self.rt().spawn(async move {
            let result = run_slow_poll_phase(cp, phase, preset_fp).await;
            let _ = tx.send(result).await;
        });
    }

    fn start_poll_processor(
        &self,
        poll_rx: async_channel::Receiver<PollData>,
        art_tx: async_channel::Sender<Vec<u8>>,
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
    fn start_art_loader(&self, art_rx: async_channel::Receiver<Vec<u8>>) {
        let ds = self.downgrade();
        glib::spawn_future_local(async move {
            while let Ok(bytes) = art_rx.recv().await {
                let Some(ds) = ds.upgrade() else { break };
                {
                    let mut inner = ds.imp().inner.borrow_mut();
                    Self::replace_artwork(&mut inner, None); // leak-check the outgoing value first
                    if bytes.is_empty() {
                        dbg("artwork fetch failed; clearing stale art");
                    } else {
                        dbg(&format!("artwork loaded: {} bytes", bytes.len()));
                        inner.playback.artwork = Some(Rc::new(bytes));
                    }
                }
                dbg("signal: playback-changed (artwork)");
                ds.emit_by_name::<()>("playback-changed", &[&playback_changed::ARTWORK]);
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

    /// Shared by `process_poll_http()`/`process_poll_upnp()`'s `mode_changed`
    /// handling — identical either way, only the raw mode value's source
    /// differs (HTTP `mode` vs. UPnP `PlayType`, confirmed byte-identical).
    /// Updates `current_mode`, clears stale artwork for the incoming track,
    /// and self-corrects a capability snapshot that claims this input is
    /// disabled despite demonstrably being in active use right now.
    fn apply_mode_change(inner: &mut Inner, new_mode: i32) {
        inner.current_mode = new_mode;
        Self::replace_artwork(inner, None);
        inner.playback.art_url = None;
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

    fn start_slow_poll_processor(&self, rx: async_channel::Receiver<SlowPollResult>) {
        let ds_weak = self.downgrade();
        glib::spawn_future_local(async move {
            while let Ok(result) = rx.recv().await {
                let Some(ds) = ds_weak.upgrade() else { break };
                match result {
                    SlowPollResult::Presets(presets)     => ds.handle_slow_poll_presets(presets),
                    SlowPollResult::Outputs(outputs)     => ds.handle_slow_poll_outputs(outputs),
                    SlowPollResult::OutputStatus(status) => ds.handle_slow_poll_output_status(status),
                    SlowPollResult::DeviceInfo(info)     => ds.handle_slow_poll_device_info(info),
                }
            }
        });
    }

    fn handle_slow_poll_presets(&self, presets: Option<(String, Vec<PresetEntry>)>) {
        let Some((new_fp, entries)) = presets else {
            dbg("slow poll: presets unchanged");
            return;
        };
        dbg(&format!("slow poll: presets updated: {} slots", entries.len()));
        {
            let mut inner = self.imp().inner.borrow_mut();
            inner.preset_fp = new_fp;
            inner.presets   = entries;
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
    fn process_poll(&self, data: PollData, art_tx: &async_channel::Sender<Vec<u8>>) {
        match data {
            PollData::Http { status, meta } => self.process_poll_http(status, meta, art_tx),
            PollData::Upnp { info } => self.process_poll_upnp(info, art_tx),
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
    fn process_poll_http(
        &self,
        status: Option<PlayerStatus>,
        meta:   Option<MetaData>,
        art_tx: &async_channel::Sender<Vec<u8>>,
    ) {
        let mut playback_mask: u32 = 0;

        if let Some(st) = status {
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
                // canonical state says Y" from here.
                let vol_changed = inner.target_volume < 0 && st.vol != inner.playback.volume;
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
            {
                let mut inner = self.imp().inner.borrow_mut();
                if mode_changed {
                    Self::apply_mode_change(&mut inner, st.mode);
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
                inner.player_status = Some(st);
            }

            // 3. Side effects, after the borrow is dropped.
            if mode_changed {
                dbg("signal: input-changed");
                self.emit_by_name::<()>("input-changed", &[]);
            }
        }

        if let Some(m) = meta {
            let art_url = m.art_uri().to_string();

            // 1. Borrow: diff against previous metadata, compute everything we
            //    need from `inner` before it's dropped.
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

            if title_changed  { playback_mask |= playback_changed::TITLE; }
            if artist_changed { playback_mask |= playback_changed::ARTIST; }
            if album_changed  { playback_mask |= playback_changed::ALBUM; }
            if other_changed  { playback_mask |= playback_changed::OTHER; }

            // 2. Borrow_mut: decode only what changed, straight into `playback`.
            {
                let mut inner = self.imp().inner.borrow_mut();
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
                inner.metadata = Some(m);
            }

            // 3. Side effects, after the borrow is dropped.
            if url_changed {
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
    fn process_poll_upnp(&self, info: Option<upnp::InfoEx>, art_tx: &async_channel::Sender<Vec<u8>>) {
        let Some(info) = info else { return };
        let mut playback_mask: u32 = 0;

        // 1. Borrow: diff each field group against the previous response.
        let (
            mode_changed, prev_mode,
            status_changed, time_changed, mute_changed, vol_changed,
            source_changed, title_changed, artist_changed, album_changed, quality_changed,
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
            let mute_changed = prev.map_or(true, |p| p.current_mute != info.current_mute);
            // Same self-heal reasoning as process_poll_http()'s vol_changed
            // — see its doc comment. `SetVolume` still goes over HTTP
            // regardless of poll backend, so the debounce/`target_volume`
            // state is shared between both paths.
            let vol_changed = inner.target_volume < 0 && info.current_volume != inner.playback.volume;
            let source_changed = prev.map_or(true, |p| {
                p.play_medium != info.play_medium || p.track_source != info.track_source
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
            )
        };

        if mute_changed || vol_changed { playback_mask |= playback_changed::VOLUME; }
        if time_changed                { playback_mask |= playback_changed::TIME; }
        if status_changed || source_changed || quality_changed { playback_mask |= playback_changed::OTHER; }
        if title_changed  { playback_mask |= playback_changed::TITLE; }
        if artist_changed { playback_mask |= playback_changed::ARTIST; }
        if album_changed  { playback_mask |= playback_changed::ALBUM; }

        if mode_changed {
            dbg(&format!("input changed (upnp): mode {prev_mode} → {}", info.play_type));
        }

        let mut art_url_for_fetch: Option<String> = None;
        let mut art_cleared = false;

        // 2. Borrow_mut: decode only what changed, straight into `playback`.
        {
            let mut inner = self.imp().inner.borrow_mut();
            if mode_changed {
                Self::apply_mode_change(&mut inner, info.play_type);
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
            if mute_changed { inner.playback.muted  = info.current_mute; }
            if source_changed {
                inner.playback.source_name =
                    playback::decode_source_name_upnp(&info.play_medium, &info.track_source);
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
            if art_url != cached {
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

            inner.upnp_info = Some(info);
        }

        // 3. Side effects, after the borrow is dropped.
        if mode_changed {
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

    fn fetch_art(&self, url: String, art_tx: &async_channel::Sender<Vec<u8>>) {
        let client = match self.imp().inner.borrow().client.clone() {
            Some(c) => c,
            None    => return,
        };
        let art_tx = art_tx.clone();
        self.rt().spawn(async move {
            // Always send, even on failure (as an empty Vec) — start_art_loader
            // treats that as "no artwork" and clears the stale texture instead
            // of the UI never hearing about the failure at all.
            let bytes = client.fetch_bytes(&url).await.unwrap_or_default();
            let _ = art_tx.send(bytes).await;
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
    /// `current_mode` is set to -1 during a switch. This will force a refresh and
    /// signal one the next poll.
    pub fn switch_input(&self, src: String) {
        let client = match self.imp().inner.borrow().client.clone() {
            Some(c) => c,
            None    => return,
        };
        self.imp().inner.borrow_mut().current_mode = -1;
        self.rt().spawn(async move { let _ = client.switch_input(&src).await; });
        // TODO: trigger_poll()
    }

    // ── Volume / mute commands ────────────────────────────────────────────────

    pub fn do_set_mute(&self, muted: bool) {
        let Some(client) = self.imp().inner.borrow().client.clone() else { return };
        self.rt().spawn(async move { let _ = client.set_mute(muted).await; });
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
        let Some(tx) = self.imp().poll_tx.borrow().clone() else { return };
        let ds = self.downgrade();
        let rt = self.rt();
        glib::timeout_add_local_once(delay, move || {
            let Some(ds) = ds.upgrade() else { return };
            let (wants_upnp, upnp_client, client) = {
                let inner = ds.imp().inner.borrow();
                (inner.access == AccessMethod::UpnpPolled, inner.upnp_client.clone(), inner.client.clone())
            };
            // Same either/or dispatch shape (and same "no HTTP fallback"
            // rule) as the main fast-poll in `dispatch_fast_poll` — see
            // that comment.
            match (wants_upnp, upnp_client) {
                (true, None) => {}
                (true, Some(uc)) => {
                    ds.imp().inner.borrow_mut().last_poll = Some(Instant::now());
                    rt.spawn(async move {
                        let info = fetch_upnp_fast_poll(uc).await;
                        let _ = tx.send(PollData::Upnp { info }).await;
                    });
                }
                (false, _) => {
                    let Some(c) = client else { return };
                    ds.imp().inner.borrow_mut().last_poll = Some(Instant::now());
                    rt.spawn(async move {
                        let (status, meta) = fetch_http_fast_poll(c).await;
                        let _ = tx.send(PollData::Http { status, meta }).await;
                    });
                }
            }
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

