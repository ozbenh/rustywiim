/// Device state GObject — owns the WiiM client, caches polled state, and
/// emits GTK signals when state changes.  All methods run on the GTK main
/// thread; API calls are dispatched to a tokio runtime and results are
/// returned via `async_channel`.
///
/// Signals
/// -------
/// * `device-changed`   — device info (re)loaded or cleared (UI should rebuild)
/// * `playback-changed` — player status / metadata / artwork updated
/// * `input-changed`    — current input mode changed
/// * `output-changed`   — audio output selection changed
/// * `outputs-changed`  — supported output list updated (rebuild menu)
/// * `network-changed`  — netstat or RSSI changed
/// * `presets-changed`  — preset list (re)loaded; UI should re-read `presets()`

use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use glib::prelude::*;
use glib::subclass::prelude::*;
use gtk::glib;

pub static DEBUG_STATE: AtomicBool = AtomicBool::new(false);

fn dbg(msg: &str) {
    if DEBUG_STATE.load(Ordering::Relaxed) {
        println!("[state] {msg}");
    }
}

use crate::api::{
    AudioInputEntry, AudioOutputStatus, DeviceInfo, MetaData, OutputEntry, PlayerStatus,
    PresetEntry, TlsMode, WiimClient, TLS_MODE,
};
use crate::capabilities::{DeviceCapabilities, detect_outputs, output_display_name};

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

struct PollData {
    status: Option<PlayerStatus>,
    meta:   Option<MetaData>,
    output: Option<AudioOutputStatus>,
}

// ── Slow poll ─────────────────────────────────────────────────────────────────

struct SlowPollResult {
    /// `Some(result)` when probe was attempted; `None` when skipped (probe_outputs=false).
    outputs:     Option<Option<Vec<OutputEntry>>>,
    device_info: Option<DeviceInfo>,
    /// `Some((fp, entries))` when the fingerprint changed; `None` when
    /// unchanged or when preset probing is disabled.
    presets:     Option<(String, Vec<PresetEntry>)>,
}

async fn run_slow_poll(
    client:        WiimClient,
    probe_outputs: bool,
    probe_presets: bool,
    preset_fp:     String,
) -> SlowPollResult {
    let outputs = if probe_outputs {
        Some(client.get_sound_card_mode_support_list().await)
    } else {
        None
    };
    let device_info = client.get_device_info().await.ok();
    let presets = if probe_presets {
        client.fetch_presets(&preset_fp).await
    } else {
        None
    };
    SlowPollResult { outputs, device_info, presets }
}

// ── Cached device state ───────────────────────────────────────────────────────

struct Inner {
    client:          Option<WiimClient>,
    device_info:     Option<DeviceInfo>,
    capabilities:    Option<DeviceCapabilities>,
    player_status:   Option<PlayerStatus>,
    metadata:        Option<MetaData>,
    output_status:   Option<AudioOutputStatus>,
    audio_inputs:    Vec<AudioInputEntry>,
    mode_renames:    HashMap<String, String>,
    current_mode:    String,
    current_art_url: String,
    art_bytes:       Option<Vec<u8>>,
    /// Outputs currently supported by the device (canonical name + display label).
    /// Initialised from the static capability profile; replaced by
    /// `getSoundCardModeSupportList` results when the device supports that API.
    outputs:         Vec<OutputEntry>,
    /// `true` while `getSoundCardModeSupportList` should be polled.
    /// Set to `false` on the first call that returns a non-array response.
    probe_outputs:    bool,
    /// `true` while preset polling is active.
    /// Set to `false` when capabilities indicate no preset support.
    probe_presets:    bool,
    connection_state: ConnectionState,
    /// Last known network connection type (0=ethernet, 2=wifi).
    /// `None` until first `getStatusEx` result arrives.
    netstat:          Option<u32>,
    /// Last known wifi RSSI in dBm.  `None` until first `getStatusEx` result.
    rssi:             Option<i32>,
    /// Resolved preset slots (1–12), cached from the last successful fetch.
    presets:          Vec<PresetEntry>,
    /// Fingerprint of the last fetched preset list (used to skip re-fetches).
    preset_fp:        String,
    /// Expected WiFi SSID for the current startup reconnect attempt.
    /// `None` means accept any SSID (user-initiated connect or already verified).
    expected_ssid:    Option<String>,
}

impl Default for Inner {
    fn default() -> Self {
        Self {
            client:          None,
            device_info:     None,
            capabilities:    None,
            player_status:   None,
            metadata:        None,
            output_status:   None,
            audio_inputs:    Vec::new(),
            mode_renames:    HashMap::new(),
            current_mode:    String::new(),
            current_art_url: String::new(),
            art_bytes:       None,
            outputs:          Vec::new(),
            probe_outputs:    true,
            probe_presets:    true,
            netstat:          None,
            rssi:             None,
            connection_state: ConnectionState::Disconnected,
            presets:          Vec::new(),
            preset_fp:        String::new(),
            expected_ssid:    None,
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
        fn signals() -> &'static [Signal] {
            static SIGNALS: OnceLock<Vec<Signal>> = OnceLock::new();
            SIGNALS.get_or_init(|| {
                vec![
                    Signal::builder("device-changed").build(),
                    Signal::builder("playback-changed").build(),
                    Signal::builder("input-changed").build(),
                    Signal::builder("output-changed").build(),
                    Signal::builder("outputs-changed").build(),
                    Signal::builder("network-changed").build(),
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

    /// Switch to a new device IP.  Clears all cached state, saves config,
    /// emits `device-changed` immediately (with cleared state so the UI can
    /// show "Connecting…"), then fetches device info asynchronously and emits
    /// `device-changed` again when the data arrives.
    ///
    /// `expected_ssid` — when `Some`, the SSID reported by the device must
    /// match; on mismatch the connection is aborted and state reverts to
    /// `Disconnected` so the caller can try a different IP.  Pass `None` for
    /// user-initiated connects where the right device is already known.
    pub fn set_device(&self, ip: &str, tls: TlsMode, expected_ssid: Option<&str>) {
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
            inner.connection_state = ConnectionState::Connecting;
            inner.expected_ssid    = expected_ssid.map(String::from);
        }
        {
            use crate::config::Config;
            let mut cfg = Config::load();
            cfg.last_ip = ip.to_string();
            cfg.save();
        }
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
            info:      DeviceInfo,
            output:    Option<AudioOutputStatus>,
            in_enable: Vec<AudioInputEntry>,
            renames:   HashMap<String, String>,
            sc_list:   Option<Vec<OutputEntry>>,
        }
        let (tx, rx) = async_channel::bounded::<Option<FetchOk>>(1);

        rt.spawn(async move {
            let payload = match client.get_device_info().await {
                Ok(info) => {
                    let output    = client.get_audio_output().await.ok();
                    let in_enable = client.get_audio_input_enable().await;
                    let renames   = client.get_mode_rename().await;
                    let sc_list   = client.get_sound_card_mode_support_list().await;
                    Some(FetchOk { info, output, in_enable, renames, sc_list })
                }
                Err(e) => {
                    eprintln!("[state] fetch_device_info failed: {e}");
                    None
                }
            };
            let _ = tx.send(payload).await;
        });

        let ds = self.downgrade();
        glib::spawn_future_local(async move {
            let payload = rx.recv().await.ok().flatten();
            let Some(ds) = ds.upgrade() else { return };

            let Some(FetchOk { info, output, in_enable, renames, sc_list }) = payload else {
                ds.imp().inner.borrow_mut().connection_state = ConnectionState::Failed;
                dbg("signal: device-changed (failed)");
                ds.emit_by_name::<()>("device-changed", &[]);
                return;
            };
            // If we were given an expected SSID (startup reconnect), verify it
            // before accepting the connection.  On mismatch the device at this
            // IP is not ours; drop back to Disconnected so discovery can
            // reconnect to the right IP by SSID.
            let expected_ssid = ds.imp().inner.borrow().expected_ssid.clone();
            if let Some(expected) = expected_ssid {
                if info.ssid != expected {
                    dbg(&format!(
                        "SSID mismatch: expected {:?}, got {:?}; aborting connection",
                        expected, info.ssid,
                    ));
                    *ds.imp().inner.borrow_mut() = Inner::default();
                    ds.emit_by_name::<()>("device-changed", &[]);
                    return;
                }
            }
            let caps = DeviceCapabilities::from_device_info(&info);
            // Initialise outputs: prefer the live API list; fall back to the static profile.
            let (probe_outputs, outputs) = match sc_list {
                Some(list) => {
                    dbg(&format!("outputs from API: {:?}", list));
                    (true, list)
                }
                None => {
                    dbg("getSoundCardModeSupportList not supported; using static profile");
                    let fallback = detect_outputs(caps.device_id)
                        .iter()
                        .map(|&canon| OutputEntry {
                            canon,
                            name: output_display_name(canon).to_string(),
                        })
                        .collect();
                    (false, fallback)
                }
            };
            dbg(&format!(
                "device info: model=\"{}\" vendor={} fw={} project={} inputs={} outputs={}",
                caps.model,
                caps.vendor.display_name(),
                info.firmware,
                info.project,
                in_enable.len(),
                output.as_ref().map_or("none", |o| &o.hardware),
            ));
            {
                let mut inner = ds.imp().inner.borrow_mut();
                inner.netstat           = info.netstat.parse().ok();
                inner.rssi              = info.rssi.parse().ok();
                let probe_presets = caps.supports_presets;
                inner.capabilities      = Some(caps);
                inner.device_info       = Some(info);
                inner.output_status     = output;
                inner.audio_inputs      = in_enable;
                inner.mode_renames      = renames;
                inner.outputs           = outputs;
                inner.probe_outputs     = probe_outputs;
                // Disable preset polling if capabilities explicitly report no support.
                if !probe_presets {
                    inner.probe_presets = false;
                }
                // Reset preset data so the first slow-poll cycle re-fetches from scratch.
                inner.preset_fp         = String::new();
                inner.presets           = Vec::new();
                inner.connection_state  = ConnectionState::Connected;
            }
            dbg("signal: device-changed (ready)");
            ds.emit_by_name::<()>("device-changed", &[]);
            // Immediately run one slow poll so presets and network status appear
            // right after connection rather than waiting for the 5-second timer.
            ds.fire_slow_poll();
        });
    }

    /// Enqueue one slow-poll cycle (outputs + device_info + presets) for
    /// immediate execution.  Safe to call from the GTK thread at any time;
    /// does nothing when the slow-poll channel isn't set up yet.
    fn fire_slow_poll(&self) {
        let inner = self.imp().inner.borrow();
        let Some(client)  = inner.client.clone()   else { return };
        let probe_outputs = inner.probe_outputs;
        let probe_presets = inner.probe_presets;
        let preset_fp     = inner.preset_fp.clone();
        drop(inner);
        let Some(tx) = self.imp().slow_poll_tx.borrow().clone() else { return };
        self.rt().spawn(async move {
            let result = run_slow_poll(client, probe_outputs, probe_presets, preset_fp).await;
            let _ = tx.send(result).await;
        });
    }

    // ── Polling ───────────────────────────────────────────────────────────────

    /// Start the 1-second poll timer and background result processors.
    /// Call once after `new()`.
    pub fn start_polling(&self) {
        let (poll_tx, poll_rx) = async_channel::unbounded::<PollData>();
        let (art_tx,  art_rx)  = async_channel::unbounded::<Vec<u8>>();

        self.start_poll_timer(poll_tx);
        self.start_poll_processor(poll_rx, art_tx);
        self.start_art_loader(art_rx);
        self.start_slow_pollers();
    }

    fn start_poll_timer(&self, poll_tx: async_channel::Sender<PollData>) {
        *self.imp().poll_tx.borrow_mut() = Some(poll_tx.clone());
        let ds = self.downgrade();
        let rt = self.rt();
        glib::timeout_add_local(Duration::from_secs(1), move || {
            let Some(ds) = ds.upgrade() else { return glib::ControlFlow::Break };
            let inner = ds.imp().inner.borrow();
            // Only poll when fully connected; skip during connecting / failed / disconnected.
            if inner.connection_state != ConnectionState::Connected {
                return glib::ControlFlow::Continue;
            }
            let client = inner.client.clone();
            drop(inner);
            if let Some(c) = client {
                let tx = poll_tx.clone();
                rt.spawn(async move {
                    let status = c.get_status().await.ok();
                    let meta   = c.get_meta_info().await.ok();
                    let output = c.get_audio_output().await.ok();
                    let _ = tx.send(PollData { status, meta, output }).await;
                });
            }
            glib::ControlFlow::Continue
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

    fn start_art_loader(&self, art_rx: async_channel::Receiver<Vec<u8>>) {
        let ds = self.downgrade();
        glib::spawn_future_local(async move {
            while let Ok(bytes) = art_rx.recv().await {
                let Some(ds) = ds.upgrade() else { break };
                dbg(&format!("artwork loaded: {} bytes", bytes.len()));
                ds.imp().inner.borrow_mut().art_bytes = Some(bytes);
                dbg("signal: playback-changed (artwork)");
                ds.emit_by_name::<()>("playback-changed", &[]);
            }
        });
    }

    fn start_slow_pollers(&self) {
        let (tx, rx) = async_channel::unbounded::<SlowPollResult>();

        // Store the sender so fire_slow_poll() can enqueue an immediate cycle.
        *self.imp().slow_poll_tx.borrow_mut() = Some(tx.clone());

        // Timer: fires every 5 seconds.
        //  - Connected → run normal slow polls (output list + getStatusEx + presets).
        //  - Failed    → attempt to reconnect via fetch_device_info().
        //  - Connecting / Disconnected → skip.
        let ds_weak = self.downgrade();
        let rt = self.rt();
        glib::timeout_add_local(Duration::from_secs(5), move || {
            let Some(ds) = ds_weak.upgrade() else { return glib::ControlFlow::Break };
            let inner = ds.imp().inner.borrow();
            let state         = inner.connection_state;
            let probe_outputs = inner.probe_outputs;
            let probe_presets = inner.probe_presets;
            let preset_fp     = inner.preset_fp.clone();
            let client        = inner.client.clone();
            drop(inner);

            match state {
                ConnectionState::Failed => {
                    if client.is_some() {
                        dbg("reconnect attempt: transitioning Connecting");
                        ds.imp().inner.borrow_mut().connection_state = ConnectionState::Connecting;
                        ds.emit_by_name::<()>("device-changed", &[]);
                        ds.fetch_device_info();
                    }
                }
                ConnectionState::Connected => {
                    let Some(client) = client else { return glib::ControlFlow::Continue };
                    let tx = tx.clone();
                    rt.spawn(async move {
                        let result = run_slow_poll(client, probe_outputs, probe_presets, preset_fp).await;
                        let _ = tx.send(result).await;
                    });
                }
                _ => {} // Connecting or Disconnected — skip
            }
            glib::ControlFlow::Continue
        });

        // Processor: handles results from the Connected slow polls.
        let ds_weak = self.downgrade();
        glib::spawn_future_local(async move {
            while let Ok(result) = rx.recv().await {
                let Some(ds) = ds_weak.upgrade() else { break };

                // ── output list ───────────────────────────────────────────────
                match result.outputs {
                    Some(None) => {
                        dbg("getSoundCardModeSupportList returned non-array; disabling");
                        ds.imp().inner.borrow_mut().probe_outputs = false;
                    }
                    Some(Some(list)) => {
                        let prev = ds.imp().inner.borrow().outputs.clone();
                        if list != prev {
                            dbg(&format!("outputs updated by poll: {:?}", list));
                            ds.imp().inner.borrow_mut().outputs = list;
                            ds.emit_by_name::<()>("outputs-changed", &[]);
                        }
                    }
                    None => {} // probe skipped
                }

                // ── device info (getStatusEx) ─────────────────────────────────
                let Some(new_info) = result.device_info else {
                    // getStatusEx failed while we were Connected → declare failure.
                    if ds.imp().inner.borrow().connection_state == ConnectionState::Connected {
                        dbg("slow poll: getStatusEx failed; transitioning to Failed");
                        {
                            let mut inner = ds.imp().inner.borrow_mut();
                            inner.connection_state = ConnectionState::Failed;
                            inner.device_info      = None;
                        }
                        ds.emit_by_name::<()>("device-changed", &[]);
                    }
                    continue;
                };

                let (prev_fw, prev_ssid, prev_name, prev_netstat, prev_rssi) = {
                    let inner = ds.imp().inner.borrow();
                    let di = inner.device_info.as_ref();
                    (
                        di.map(|i| i.firmware.clone()).unwrap_or_default(),
                        di.map(|i| i.ssid.clone()).unwrap_or_default(),
                        di.map(|i| i.device_name.clone()).unwrap_or_default(),
                        inner.netstat,
                        inner.rssi,
                    )
                };

                // SSID change means the underlying device has been replaced on the
                // same IP.  Do a full re-init rather than a partial identity update.
                if !prev_ssid.is_empty() && new_info.ssid != prev_ssid {
                    dbg(&format!(
                        "slow poll: SSID changed ({} → {}); resetting connection",
                        prev_ssid, new_info.ssid,
                    ));
                    let client = ds.imp().inner.borrow().client.clone();
                    {
                        let mut inner = ds.imp().inner.borrow_mut();
                        *inner = Inner::default();
                        inner.client           = client;
                        inner.connection_state = ConnectionState::Connecting;
                    }
                    ds.emit_by_name::<()>("device-changed", &[]);
                    ds.fetch_device_info();
                    continue;
                }

                let identity_changed =
                    new_info.firmware    != prev_fw   ||
                    new_info.device_name != prev_name;

                let new_netstat: Option<u32> = new_info.netstat.parse().ok();
                let new_rssi:    Option<i32> = new_info.rssi.parse().ok();

                let network_changed =
                    new_netstat != prev_netstat ||
                    new_rssi    != prev_rssi;

                {
                    let mut inner = ds.imp().inner.borrow_mut();
                    inner.netstat = new_netstat;
                    inner.rssi    = new_rssi;
                    if identity_changed {
                        dbg(&format!(
                            "device identity changed: fw={} ssid={} name={}",
                            new_info.firmware, new_info.ssid, new_info.device_name,
                        ));
                        inner.device_info = Some(new_info);
                    }
                }

                if identity_changed {
                    ds.emit_by_name::<()>("device-changed", &[]);
                }
                if network_changed {
                    dbg(&format!(
                        "signal: network changed: netstat={} rssi={}",
                        ds.imp().inner.borrow().netstat.unwrap_or(0),
                        ds.imp().inner.borrow().rssi.unwrap_or(0),
                    ));
                    ds.emit_by_name::<()>("network-changed", &[]);
                }

                // ── presets ───────────────────────────────────────────────────
                if let Some((new_fp, entries)) = result.presets {
                    dbg(&format!("signal: presets updated: {} slots", entries.len()));
                    {
                        let mut inner = ds.imp().inner.borrow_mut();
                        inner.preset_fp = new_fp;
                        inner.presets   = entries;
                    }
                    ds.emit_by_name::<()>("presets-changed", &[]);
                }
            }
        });
    }

    fn process_poll(&self, data: PollData, art_tx: &async_channel::Sender<Vec<u8>>) {
        let PollData { status, meta, output } = data;
        let mut emit_playback = false;

        if let Some(st) = status {
            let prev_mode = self.imp().inner.borrow().current_mode.clone();
            let mode_changed = st.mode != prev_mode;
            if mode_changed {
                dbg(&format!(
                    "input changed: mode {} → {} (status={})",
                    prev_mode, st.mode, st.status,
                ));
                let mut inner = self.imp().inner.borrow_mut();
                inner.current_mode    = st.mode.clone();
                inner.current_art_url.clear();
                inner.art_bytes       = None;
            }
            self.imp().inner.borrow_mut().player_status = Some(st);
            if mode_changed {
                dbg("signal: input-changed");
                self.emit_by_name::<()>("input-changed", &[]);
            }
            emit_playback = true;
        }

        if let Some(out) = output {
            let prev_hw = self.imp().inner.borrow()
                .output_status.as_ref().map(|o| o.hardware.clone());
            let changed = prev_hw.as_deref() != Some(&out.hardware);
            if changed {
                dbg(&format!(
                    "output changed: {} → {}",
                    prev_hw.as_deref().unwrap_or("none"),
                    out.hardware,
                ));
            }
            self.imp().inner.borrow_mut().output_status = Some(out);
            if changed {
                dbg("signal: output-changed");
                self.emit_by_name::<()>("output-changed", &[]);
            }
        }

        if let Some(m) = meta {
            let art_url = m.art_uri().to_string();
            let url_changed = art_url != self.imp().inner.borrow().current_art_url;
            self.imp().inner.borrow_mut().metadata = Some(m);
            if !art_url.is_empty() && url_changed {
                dbg(&format!("art url changed: {art_url}"));
                self.imp().inner.borrow_mut().current_art_url = art_url.clone();
                self.imp().inner.borrow_mut().art_bytes = None;
                self.fetch_art(art_url, art_tx);
            }
            emit_playback = true;
        }

        if emit_playback {
            dbg("signal: playback-changed");
            self.emit_by_name::<()>("playback-changed", &[]);
        }
    }

    fn fetch_art(&self, url: String, art_tx: &async_channel::Sender<Vec<u8>>) {
        let client = match self.imp().inner.borrow().client.clone() {
            Some(c) => c,
            None    => return,
        };
        let art_tx = art_tx.clone();
        self.rt().spawn(async move {
            if let Ok(bytes) = client.fetch_bytes(&url).await {
                let _ = art_tx.send(bytes).await;
            }
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
    /// The cached `current_mode` is updated optimistically.  The regular poll
    /// detects any mismatch and emits `input-changed` to correct the dropdown.
    pub fn switch_input(&self, src: String) {
        let client = match self.imp().inner.borrow().client.clone() {
            Some(c) => c,
            None    => return,
        };
        self.imp().inner.borrow_mut().current_mode = src.clone();
        self.rt().spawn(async move { let _ = client.switch_input(&src).await; });
    }

    // ── Volume / mute commands ────────────────────────────────────────────────

    pub fn do_set_mute(&self, muted: bool) {
        let Some(client) = self.imp().inner.borrow().client.clone() else { return };
        if let Some(ref mut st) = self.imp().inner.borrow_mut().player_status {
            st.mute = if muted { "1" } else { "0" }.to_string();
        }
        self.emit_by_name::<()>("playback-changed", &[]);
        self.rt().spawn(async move { let _ = client.set_mute(muted).await; });
    }

    pub fn do_set_volume(&self, vol: u32) {
        let Some(client) = self.imp().inner.borrow().client.clone() else { return };
        if let Some(ref mut st) = self.imp().inner.borrow_mut().player_status {
            st.vol = vol.to_string();
        }
        self.rt().spawn(async move { let _ = client.set_volume(vol).await; });
    }

    // ── Transport commands ────────────────────────────────────────────────────

    // Optimistic "play or pause based on current cached state" — use this
    // instead of calling client().play()/pause() directly so the decision
    // is made from the same source of truth as the poll.
    pub fn do_play_pause(&self) {
        let inner = self.imp().inner.borrow();
        let Some(client) = inner.client.clone() else { return };
        let playing = inner.player_status.as_ref().map(|s| s.status == "play").unwrap_or(false);
        drop(inner);
        self.rt().spawn(async move {
            if playing { let _ = client.pause().await; } else { let _ = client.play().await; }
        });
        self.trigger_poll_after(400);
    }

    pub fn do_prev(&self) {
        let Some(client) = self.imp().inner.borrow().client.clone() else { return };
        self.rt().spawn(async move { let _ = client.prev().await; });
        self.trigger_poll_after(400);
    }

    pub fn do_next(&self) {
        let Some(client) = self.imp().inner.borrow().client.clone() else { return };
        self.rt().spawn(async move { let _ = client.next().await; });
        self.trigger_poll_after(400);
    }

    fn trigger_poll_after(&self, delay_ms: u64) {
        let Some(tx) = self.imp().poll_tx.borrow().clone() else { return };
        let ds = self.downgrade();
        let rt = self.rt();
        glib::timeout_add_local_once(Duration::from_millis(delay_ms), move || {
            let Some(ds) = ds.upgrade() else { return };
            let client = ds.imp().inner.borrow().client.clone();
            if let Some(c) = client {
                rt.spawn(async move {
                    let status = c.get_status().await.ok();
                    let meta   = c.get_meta_info().await.ok();
                    let _ = tx.send(PollData { status, meta, output: None }).await;
                });
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

    pub fn device_info(&self) -> Option<DeviceInfo> {
        self.imp().inner.borrow().device_info.clone()
    }

    pub fn capabilities(&self) -> Option<DeviceCapabilities> {
        self.imp().inner.borrow().capabilities.clone()
    }

    pub fn player_status(&self) -> Option<PlayerStatus> {
        self.imp().inner.borrow().player_status.clone()
    }

    pub fn muted(&self) -> bool {
        self.imp().inner.borrow().player_status.as_ref().map(|s| s.mute == "1").unwrap_or(false)
    }

    pub fn metadata(&self) -> Option<MetaData> {
        self.imp().inner.borrow().metadata.clone()
    }

    pub fn output_status(&self) -> Option<AudioOutputStatus> {
        self.imp().inner.borrow().output_status.clone()
    }

    pub fn audio_inputs(&self) -> Vec<AudioInputEntry> {
        self.imp().inner.borrow().audio_inputs.clone()
    }

    pub fn mode_renames(&self) -> HashMap<String, String> {
        self.imp().inner.borrow().mode_renames.clone()
    }

    pub fn current_mode(&self) -> String {
        self.imp().inner.borrow().current_mode.clone()
    }

    pub fn art_bytes(&self) -> Option<Vec<u8>> {
        self.imp().inner.borrow().art_bytes.clone()
    }

    // ── Typed signal connectors ───────────────────────────────────────────────

    pub fn connect_device_changed<F: Fn(&Self) + 'static>(&self, f: F) -> glib::SignalHandlerId {
        self.connect_local("device-changed", false, move |args| {
            f(&args[0].get::<Self>().unwrap());
            None
        })
    }

    pub fn connect_playback_changed<F: Fn(&Self) + 'static>(&self, f: F) -> glib::SignalHandlerId {
        self.connect_local("playback-changed", false, move |args| {
            f(&args[0].get::<Self>().unwrap());
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

    pub fn outputs(&self) -> Vec<OutputEntry> {
        self.imp().inner.borrow().outputs.clone()
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

