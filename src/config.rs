use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;

fn default_panel_visible() -> bool { true }

/// Per-device window state, keyed on the WiFi SSID the device is connected to.
/// The SSID is a stable hardware-level identifier that does not change when
/// the user renames the device.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeviceConfig {
    #[serde(default)]
    pub window_width: i32,
    #[serde(default)]
    pub window_height: i32,
    #[serde(default)]
    pub window_maximized: bool,
    #[serde(default)]
    pub paned_position: i32,
    #[serde(default = "default_panel_visible")]
    pub panel_visible: bool,
}

impl Default for DeviceConfig {
    fn default() -> Self {
        Self {
            window_width:    0,
            window_height:   0,
            window_maximized: false,
            paned_position:  0,
            panel_visible:   true,
        }
    }
}

/// Top-level application config.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Config {
    /// IP of the last connected device (for reconnect on startup).
    #[serde(default)]
    pub last_ip: String,
    /// SSID of the last connected device.  Used to look up the right
    /// DeviceConfig for initial window sizing before the device has reported
    /// its SSID to us.
    #[serde(default)]
    pub last_ssid: String,
    /// Per-device window/panel state, keyed on device WiFi SSID.
    #[serde(default)]
    pub devices: HashMap<String, DeviceConfig>,
}

fn config_path() -> PathBuf {
    let dir = dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from(".config"))
        .join("rustywiim");
    let _ = fs::create_dir_all(&dir);
    dir.join("config.json")
}

impl Config {
    pub fn load() -> Self {
        fs::read_to_string(config_path())
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    pub fn save(&self) {
        if let Ok(json) = serde_json::to_string_pretty(self) {
            let _ = fs::write(config_path(), json);
        }
    }

    /// Return the stored config for `ssid`, or a fresh default.
    pub fn device(&self, ssid: &str) -> DeviceConfig {
        self.devices.get(ssid).cloned().unwrap_or_default()
    }

    /// Upsert the per-device config for `ssid`.
    pub fn save_device(&mut self, ssid: impl Into<String>, dev_cfg: DeviceConfig) {
        self.devices.insert(ssid.into(), dev_cfg);
    }
}
