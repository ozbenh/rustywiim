use serde::{Deserialize, Serialize};
use std::fs;
use std::path::PathBuf;

fn default_panel_visible() -> bool { true }

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub last_ip: String,
    #[serde(default)]
    pub window_width: i32,
    #[serde(default)]
    pub window_height: i32,
    #[serde(default)]
    pub window_maximized: bool,
    /// Last non-zero width of the left panel.
    #[serde(default)]
    pub paned_position: i32,
    /// Whether the left panel is shown (default true for first run).
    #[serde(default = "default_panel_visible")]
    pub panel_visible: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            last_ip: String::new(),
            window_width: 0,
            window_height: 0,
            window_maximized: false,
            paned_position: 0,
            panel_visible: true,
        }
    }
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
}
