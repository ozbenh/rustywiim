#![allow(dead_code)] // API surface used by future modules

use reqwest::Client;
use serde::Deserialize;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

pub static DEBUG: AtomicBool = AtomicBool::new(false);

fn debug(cmd: &str, resp: &str) {
    if DEBUG.load(Ordering::Relaxed) {
        println!("[API] {cmd} → {resp}");
    }
}

/// URL-encode a string for embedding as a WiiM API command argument.
pub fn urlencode(s: &str) -> String {
    let mut out = String::with_capacity(s.len() * 3);
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{:02X}", b)),
        }
    }
    out
}

// ── Response types ────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Deserialize, Default)]
pub struct PlayerStatus {
    #[serde(default)]
    pub status: String,
    #[serde(default)]
    pub vol: String,
    #[serde(default)]
    pub mute: String,
    #[serde(default)]
    pub curpos: String,
    #[serde(default)]
    pub totlen: String,
    #[serde(default, rename = "loop")]
    pub loop_mode: String,
    #[serde(default)]
    pub mode: String,
    #[serde(default)]
    pub vendor: String,
    #[serde(default)]
    pub plicount: String,
    #[serde(default)]
    pub plicurr: String,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct MetaData {
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub artist: String,
    #[serde(default)]
    pub album: String,
    /// API sometimes returns key with a trailing space
    #[serde(default, rename = "albumArtURI ")]
    pub album_art_uri_spaced: String,
    #[serde(default, rename = "albumArtURI")]
    pub album_art_uri: String,
    #[serde(default, rename = "sampleRate")]
    pub sample_rate: String,
    #[serde(default, rename = "bitDepth")]
    pub bit_depth: String,
    #[serde(default, rename = "bitRate")]
    pub bit_rate: String,
}

impl MetaData {
    pub fn art_uri(&self) -> &str {
        if !self.album_art_uri.is_empty() {
            &self.album_art_uri
        } else {
            &self.album_art_uri_spaced
        }
    }
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct MetaInfoResponse {
    #[serde(default, rename = "metaData")]
    pub meta_data: MetaData,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct DeviceInfo {
    #[serde(default, rename = "DeviceName")]
    pub device_name: String,
    #[serde(default)]
    pub ssid: String,
    #[serde(default)]
    pub firmware: String,
    #[serde(default)]
    pub uuid: String,
    #[serde(default)]
    pub project: String,
    #[serde(default)]
    pub hardware: String,
    #[serde(default)]
    pub eth0: String,
    #[serde(default)]
    pub apcli0: String,
    #[serde(default, rename = "Release")]
    pub release: String,
    /// Raw `plm_support` bitmap from `getStatusEx`.  May be decimal or
    /// `"0x…"` hex.  Use `plm_support_value()` to get the parsed integer.
    #[serde(default)]
    pub plm_support: String,
}

impl DeviceInfo {
    pub fn ip_addr(&self) -> &str {
        if !self.eth0.is_empty() && self.eth0 != "0.0.0.0" {
            &self.eth0
        } else {
            &self.apcli0
        }
    }

    /// Parse `plm_support` as a u64, handling both `"0x…"` hex and decimal.
    pub fn plm_support_value(&self) -> u64 {
        let s = self.plm_support.trim();
        if let Some(hex) = s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")) {
            u64::from_str_radix(hex, 16).unwrap_or(0)
        } else {
            s.parse::<u64>().unwrap_or(0)
        }
    }
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct AudioInputEntry {
    #[serde(default)]
    pub mode:   String,
    #[serde(default)]
    pub enable: u8,
}

impl AudioInputEntry {
    pub fn is_enabled(&self) -> bool { self.enable != 0 }
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct AudioOutputStatus {
    #[serde(default)]
    pub hardware: String,
    #[serde(default)]
    pub source: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Preset {
    pub number: u32,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub source: String,
    #[serde(default)]
    pub picurl: String,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct PresetResponse {
    #[serde(default)]
    pub preset_num: u32,
    #[serde(default)]
    pub preset_list: Vec<Preset>,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct RoutineStepPayload {
    #[serde(default)]
    pub input:  String,
    #[serde(default)]
    pub output: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct RoutineStep {
    #[serde(rename = "type", default)]
    pub step_type: String,
    #[serde(default)]
    pub payload:   RoutineStepPayload,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Routine {
    #[serde(default)]
    pub id:    String,
    #[serde(default)]
    pub name:  String,
    #[serde(default)]
    pub index: u32,
    #[serde(default)]
    pub steps: Vec<RoutineStep>,
}

impl Routine {
    /// Returns the `audioInput` input ID from steps, if present and non-empty.
    pub fn audio_input(&self) -> Option<&str> {
        self.steps.iter()
            .find(|s| s.step_type == "audioInput")
            .map(|s| s.payload.input.as_str())
            .filter(|s| !s.is_empty())
    }

    /// Returns the `audioOutput` output mode string from steps, if present and
    /// non-empty.  An empty string means "no output change" and is excluded.
    pub fn audio_output(&self) -> Option<&str> {
        self.steps.iter()
            .find(|s| s.step_type == "audioOutput")
            .map(|s| s.payload.output.as_str())
            .filter(|s| !s.is_empty())
    }
}

#[derive(Debug, Clone, Deserialize, Default)]
struct RoutinesResponse {
    #[serde(default)]
    routines: Vec<Routine>,
}

// ── Client ────────────────────────────────────────────────────────────────────

#[derive(Clone)]
pub struct WiimClient {
    http: Client,
    base: String,
    status_cmd: Arc<Mutex<Option<String>>>,
}

impl WiimClient {
    pub fn new(ip: &str) -> Self {
        let http = Client::builder()
            .danger_accept_invalid_certs(true)
            .timeout(Duration::from_secs(5))
            .build()
            .expect("http client");
        Self {
            http,
            base: format!("https://{ip}/httpapi.asp"),
            status_cmd: Arc::new(Mutex::new(None)),
        }
    }

    async fn cmd(&self, command: &str) -> anyhow::Result<String> {
        let url = format!("{}?command={}", self.base, command);
        let text = self.http.get(&url).send().await?.text().await?;
        debug(command, &text);
        Ok(text)
    }

    pub async fn get_status(&self) -> anyhow::Result<PlayerStatus> {
        let cached = self.status_cmd.lock().unwrap().clone();
        if let Some(cmd) = cached {
            let text = self.cmd(&cmd).await?;
            return Ok(serde_json::from_str(&text).unwrap_or_default());
        }
        for cmd in ["getPlayerStatusEx", "getPlayerStatus", "getStatusEx"] {
            if let Ok(text) = self.cmd(cmd).await {
                if let Ok(st) = serde_json::from_str::<PlayerStatus>(&text) {
                    if !st.status.is_empty() {
                        *self.status_cmd.lock().unwrap() = Some(cmd.to_string());
                        return Ok(st);
                    }
                }
            }
        }
        Ok(PlayerStatus::default())
    }

    pub async fn get_meta_info(&self) -> anyhow::Result<MetaData> {
        let text = self.cmd("getMetaInfo").await?;
        let resp: MetaInfoResponse = serde_json::from_str(&text).unwrap_or_default();
        Ok(resp.meta_data)
    }

    pub async fn get_device_info(&self) -> anyhow::Result<DeviceInfo> {
        let text = self.cmd("getStatusEx").await?;
        Ok(serde_json::from_str(&text).unwrap_or_default())
    }

    pub async fn get_presets(&self) -> anyhow::Result<PresetResponse> {
        let text = self.cmd("getPresetInfo").await?;
        Ok(serde_json::from_str(&text).unwrap_or_default())
    }

    pub async fn get_audio_output(&self) -> anyhow::Result<AudioOutputStatus> {
        let text = self.cmd("getNewAudioOutputHardwareMode").await?;
        Ok(serde_json::from_str(&text).unwrap_or_default())
    }

    /// Returns each input and whether it is enabled (1) or disabled (0).
    /// Returns an empty Vec if the device doesn't support the API.
    pub async fn get_audio_input_enable(&self) -> Vec<AudioInputEntry> {
        match self.cmd("getAudioInputEnable").await {
            Ok(text) => serde_json::from_str::<Vec<AudioInputEntry>>(&text).unwrap_or_default(),
            Err(_)   => Vec::new(),
        }
    }

    /// Returns user-assigned names keyed by input mode string.
    /// Returns an empty map if the device doesn't support the API or returns "Failed".
    pub async fn get_mode_rename(&self) -> std::collections::HashMap<String, String> {
        match self.cmd("getModeRename").await {
            Ok(text) if !text.trim().eq_ignore_ascii_case("failed") => {
                serde_json::from_str(&text).unwrap_or_default()
            }
            _ => std::collections::HashMap::new(),
        }
    }

    /// Returns the list of routines configured on the device.
    /// Returns an empty Vec if the device doesn't support the API.
    pub async fn get_all_routines(&self) -> Vec<Routine> {
        match self.cmd("getAllRoutines").await {
            Ok(text) => serde_json::from_str::<RoutinesResponse>(&text)
                .map(|r| r.routines)
                .unwrap_or_default(),
            Err(_) => Vec::new(),
        }
    }

    // ── Playback control ──────────────────────────────────────────────

    pub async fn play(&self) -> anyhow::Result<()> {
        self.cmd("setPlayerCmd:resume").await?;
        Ok(())
    }

    pub async fn pause(&self) -> anyhow::Result<()> {
        self.cmd("setPlayerCmd:pause").await?;
        Ok(())
    }

    pub async fn next(&self) -> anyhow::Result<()> {
        self.cmd("setPlayerCmd:next").await?;
        Ok(())
    }

    pub async fn prev(&self) -> anyhow::Result<()> {
        self.cmd("setPlayerCmd:prev").await?;
        Ok(())
    }

    pub async fn set_volume(&self, vol: u32) -> anyhow::Result<()> {
        self.cmd(&format!("setPlayerCmd:vol:{vol}")).await?;
        Ok(())
    }

    pub async fn set_mute(&self, mute: bool) -> anyhow::Result<()> {
        self.cmd(&format!("setPlayerCmd:mute:{}", mute as u8)).await?;
        Ok(())
    }

    pub async fn set_loop_mode(&self, mode: i32) -> anyhow::Result<()> {
        self.cmd(&format!("setPlayerCmd:loopmode:{mode}")).await?;
        Ok(())
    }

    pub async fn seek(&self, position_secs: u32) -> anyhow::Result<()> {
        self.cmd(&format!("setPlayerCmd:seek:{position_secs}")).await?;
        Ok(())
    }

    pub async fn play_preset(&self, number: u32) -> anyhow::Result<()> {
        self.cmd(&format!("MCUKeyShortClick:{number}")).await?;
        Ok(())
    }

    pub async fn switch_input(&self, source: &str) -> anyhow::Result<()> {
        self.cmd(&format!("setPlayerCmd:switchmode:{source}")).await?;
        Ok(())
    }

    pub async fn set_audio_output(&self, mode: u32) -> anyhow::Result<()> {
        self.cmd(&format!("setAudioOutputHardwareMode:{mode}")).await?;
        Ok(())
    }

    // ── Fetch helpers ─────────────────────────────────────────────────

    pub async fn fetch_bytes(&self, url: &str) -> anyhow::Result<Vec<u8>> {
        let bytes = self.http.get(url).send().await?.bytes().await?;
        Ok(bytes.to_vec())
    }
}
