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
    AudioInputEntry, AudioOutputStatus, DeviceInfo, MetaData, PlayerStatus, TlsMode,
    WiimClient, TLS_MODE,
};
use crate::capabilities::DeviceCapabilities;

// ── Poll payload ──────────────────────────────────────────────────────────────

struct PollData {
    status: Option<PlayerStatus>,
    meta:   Option<MetaData>,
    output: Option<AudioOutputStatus>,
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
        }
    }
}

// ── GObject implementation ────────────────────────────────────────────────────

mod imp {
    use super::*;
    use glib::subclass::Signal;
    use std::sync::OnceLock;

    pub struct DeviceState {
        pub(super) inner: RefCell<Inner>,
        pub(super) rt:    std::cell::OnceCell<Arc<tokio::runtime::Runtime>>,
    }

    impl Default for DeviceState {
        fn default() -> Self {
            Self {
                inner: RefCell::new(Inner::default()),
                rt:    std::cell::OnceCell::new(),
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
    pub fn set_device(&self, ip: &str, tls: TlsMode) {
        // Apply --tls CLI override if set; otherwise use the caller-supplied mode.
        let tls = {
            let global = TlsMode::from_usize(TLS_MODE.load(Ordering::Relaxed));
            if global != TlsMode::Auto { global } else { tls }
        };
        dbg(&format!("set_device: connecting to {ip} ({})", tls.description()));
        {
            let mut inner = self.imp().inner.borrow_mut();
            *inner = Inner::default();
            inner.client = Some(WiimClient::new(ip, tls));
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
        let (tx, rx) = async_channel::bounded::<(
            DeviceInfo,
            Option<AudioOutputStatus>,
            Vec<AudioInputEntry>,
            HashMap<String, String>,
        )>(1);

        rt.spawn(async move {
            if let Ok(info) = client.get_device_info().await {
                let output    = client.get_audio_output().await.ok();
                let in_enable = client.get_audio_input_enable().await;
                let renames   = client.get_mode_rename().await;
                let _ = tx.send((info, output, in_enable, renames)).await;
            }
        });

        let ds = self.downgrade();
        glib::spawn_future_local(async move {
            let Ok((info, output, in_enable, renames)) = rx.recv().await else { return };
            let Some(ds) = ds.upgrade() else { return };
            let caps = DeviceCapabilities::from_device_info(&info);
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
                inner.capabilities  = Some(caps);
                inner.device_info   = Some(info);
                inner.output_status = output;
                inner.audio_inputs  = in_enable;
                inner.mode_renames  = renames;
            }
            dbg("signal: device-changed (ready)");
            ds.emit_by_name::<()>("device-changed", &[]);
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
    }

    fn start_poll_timer(&self, poll_tx: async_channel::Sender<PollData>) {
        let ds = self.downgrade();
        let rt = self.rt();
        glib::timeout_add_local(Duration::from_secs(1), move || {
            let Some(ds) = ds.upgrade() else {
                return glib::ControlFlow::Break;
            };
            let client = ds.imp().inner.borrow().client.clone();
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
}

