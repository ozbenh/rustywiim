use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;

fn default_panel_visible() -> bool { true }

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ThemeMode {
    /// Adwaita, follows the OS light/dark preference.
    #[default]
    System,
    /// Force Adwaita light mode.
    SystemLight,
    /// Force Adwaita dark mode.
    SystemDark,
    /// RustyWiiM custom dark theme.
    // "custom" is the old serialised name kept for backwards compatibility.
    #[serde(rename = "rusty_wiim", alias = "custom")]
    RustyWiiM,
}

/// Per-device window state, keyed on the device UUID from `getStatusEx`.
/// The UUID is a stable hardware-level identifier that does not change when
/// the device is renamed or moved to a different network.
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
    #[serde(default)]
    pub mini_mode: bool,
}

impl Default for DeviceConfig {
    fn default() -> Self {
        Self {
            window_width:    0,
            window_height:   0,
            window_maximized: false,
            paned_position:  0,
            panel_visible:   true,
            mini_mode:       false,
        }
    }
}

/// Top-level application config.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Config {
    /// IP of the last connected device (for reconnect on startup).
    #[serde(default)]
    pub last_ip: String,
    /// UUID of the last connected device.  Used to look up the right
    /// DeviceConfig for initial window sizing before the device has reported
    /// its UUID to us.
    #[serde(default)]
    pub last_uuid: String,
    /// Per-device window/panel state, keyed on device UUID.
    #[serde(default)]
    pub devices: HashMap<String, DeviceConfig>,
    /// Application-wide color scheme.
    #[serde(default)]
    pub theme: ThemeMode,
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

    /// Return the stored config for `uuid`, or a fresh default.
    pub fn device(&self, uuid: &str) -> DeviceConfig {
        self.devices.get(uuid).cloned().unwrap_or_default()
    }

    /// Upsert the per-device config for `uuid`.
    pub fn save_device(&mut self, uuid: impl Into<String>, dev_cfg: DeviceConfig) {
        self.devices.insert(uuid.into(), dev_cfg);
    }
}
