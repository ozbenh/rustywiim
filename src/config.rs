use serde::{Deserialize, Serialize};
use std::cell::RefCell;
use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::OnceLock;

use rustywiim::device::playback::AccessMethod;

/// `--no-config`: skip disk I/O entirely (no read at startup, no write ever)
/// — every run behaves like a fresh install with no persisted state at all.
/// Must be set (via `set_no_config`) before anything touches the config —
/// in practice, during `main.rs`'s `connect_handle_local_options`, which runs
/// before `connect_activate` and thus before the `CONFIG` thread_local below
/// is ever first accessed.
static NO_CONFIG: AtomicBool = AtomicBool::new(false);

/// `--config-file <path>`: use this path instead of the default
/// `dirs::config_dir()/rustywiim/config.json` — for testing against a
/// specific config state without touching the real one. Same set-before-use
/// requirement as `NO_CONFIG` above.
static CONFIG_PATH_OVERRIDE: OnceLock<PathBuf> = OnceLock::new();

pub static DEBUG_CONFIG: AtomicBool = AtomicBool::new(false);

fn dbg(msg: &str) {
    if DEBUG_CONFIG.load(Ordering::Relaxed) {
        println!("[config] {msg}");
    }
}

pub fn set_no_config(v: bool) {
    NO_CONFIG.store(v, Ordering::Relaxed);
}

pub fn set_config_path_override(path: PathBuf) {
    let _ = CONFIG_PATH_OVERRIDE.set(path);
}

fn default_panel_visible() -> bool { true }
fn default_animations() -> bool { true }
fn default_mini_modern() -> bool { true }
/// Matches the accent colour hardcoded in dark.css before it became
/// user-configurable, so existing users see no visual change by default.
fn default_accent_color() -> String { "#4ecdc4".to_string() }
fn default_devlist_song_info() -> bool { true }

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ThemeMode {
    /// Adwaita, follows the OS light/dark preference.
    System,
    /// Force Adwaita light mode.
    SystemLight,
    /// Force Adwaita dark mode.
    SystemDark,
    /// RustyWiiM custom dark theme.
    // "custom" is the old serialised name kept for backwards compatibility.
    #[serde(rename = "rusty_wiim", alias = "custom")]
    RustyWiiM,
    /// RustyWiiM Modern: blurred-artwork background with floating
    /// semi-transparent panels. Main window only for now — the mini window
    /// keeps the classic RustyWiiM styling regardless of this setting.
    #[serde(rename = "rusty_wiim_modern")]
    #[default]
    RustyWiiMModern,
}

/// Per-device window state, keyed on the device UUID from `getStatusEx`.
/// The UUID is a stable hardware-level identifier that does not change when
/// the device is renamed or moved to a different network.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
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
    /// Width of the mini player window, in pixels. 0 means "not saved yet,
    /// use the built-in default" — the mini window has no maximized state
    /// and its height is content-driven (no equivalent field needed), only
    /// its width is user-resizable.
    #[serde(default)]
    pub mini_window_width: i32,
    /// Keep in the device list even when not seen on the network.
    /// `None` means the device predates the pinning feature (legacy entry);
    /// it is treated as a ghost candidate until the user explicitly pins or
    /// unpins it, which writes `Some(true)` / `Some(false)` and ends legacy treatment.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pinned: Option<bool>,
    /// Whether the device's main window was open when the app last exited.
    #[serde(default)]
    pub window_open: bool,
    /// Last known IP — used to reconnect pinned ghosts on startup.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_ip: Option<String>,
    /// Last known friendly name — displayed while connecting / offline.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Last known marketing model name (e.g. "WiiM Pro Plus").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Last known internal `project` string from `getStatusEx` (e.g.
    /// `"Muzo_Mini"`) — a different namespace from the marketing `model`
    /// name above; `capabilities::DeviceId::detect()` is keyed on this
    /// (plus `firmware` below), not on `model`. Cached alongside `firmware`
    /// so Settings' "Device -> Advanced" panel can resolve the device's
    /// actual profile default while offline instead of guessing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project: Option<String>,
    /// Last known firmware string from `getStatusEx`, paired with `project`
    /// above for the same offline-default-resolution purpose.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub firmware: Option<String>,
    /// Last known working `TlsMode`, as its raw discriminant
    /// (`TlsMode::Http` = 1, `HttpsWiiM` = 3, etc. — see `device::api::TlsMode`).
    /// Confirmed bug (Ben, 2026-07-13): without this, `ui/devlist.rs`'s
    /// `load_known_devices_from_config()` always hardcoded `HttpsWiiM` when
    /// reloading a pinned device at startup — fine for most devices, but a
    /// device that was manually added specifically because it only answers
    /// on plain HTTP (confirmed no TLS listener at all, not a cert issue —
    /// an old-firmware Audio Pro C5 unit) would work for that one session
    /// (the manual-add flow probes and remembers the right mode in memory)
    /// and then silently go back to only trying HTTPS on next launch,
    /// failing outright. Not a `TlsMode` field directly — that type isn't
    /// `Serialize`/`Deserialize` — a plain integer avoids needing to derive
    /// that just for this one persisted field.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tls_mode: Option<u8>,
    /// Field-diagnostics override of the device profile's default
    /// `AccessMethod`, editable via Settings' "Device -> Advanced" panel.
    /// `None` means "use the device profile's default".
    ///
    /// **Selecting "Default" in the UI must write `None`, so it remains
    /// the default even if what "default" means changes.
    ///
    /// Deserialized leniently (`deserialize_lenient_access_override`): older
    /// pre-release builds stored this under different shapes (a nested
    /// per-field-group object, then a `{"player_status": ...}` wrapper)
    /// before it settled on a bare `Option<AccessMethod>`. A value left over
    /// from one of those no longer matches this enum's variants at all, and
    /// treating that as a hard parse error would nuke the *entire* config
    /// file (every device, window position, pin) over one stale field in
    /// one device entry — so an unrecognized value here is just discarded
    /// back to `None` ("use the device profile's default") instead.
    #[serde(default, skip_serializing_if = "Option::is_none", deserialize_with = "deserialize_lenient_access_override")]
    pub playback_access_override: Option<AccessMethod>,
    /// Same override mechanism as `playback_access_override`, but for the
    /// mute read/write path specifically. Separate field because the two
    /// can genuinely need different answers on the same device: iEAST
    /// AudioCast defaults `playback_access` to UPnP for everything *except*
    /// mute (`AVTransport.GetInfoEx` never carries `CurrentMute` on that
    /// family — confirmed via real capture — so mute reads instead fall
    /// back to a supplementary `RenderingControl.GetMute` poll, and writes
    /// go through `RenderingControl.SetMute`).
    #[serde(default, skip_serializing_if = "Option::is_none", deserialize_with = "deserialize_lenient_access_override")]
    pub mute_access_override: Option<AccessMethod>,
}

fn deserialize_lenient_access_override<'de, D>(deserializer: D) -> Result<Option<AccessMethod>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value: Option<serde_json::Value> = Option::deserialize(deserializer)?;
    Ok(value.and_then(|v| serde_json::from_value::<AccessMethod>(v).ok()))
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
            mini_window_width: 0,
            pinned:          None,
            window_open:     false,
            last_ip:         None,
            name:            None,
            model:           None,
            project:         None,
            firmware:        None,
            tls_mode:        None,
            playback_access_override: None,
            mute_access_override: None,
        }
    }
}

/// Top-level application config.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Config {
    /// IP of the last connected device.  Legacy field: read from old configs
    /// but not written back once cleared.  Per-device `DeviceConfig::last_ip`
    /// is the canonical source of truth going forward.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub last_ip: String,
    /// UUID of the last connected device.  Legacy field: read from old configs
    /// but not written back once cleared.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub last_uuid: String,
    /// Per-device window/panel state, keyed on device UUID.
    #[serde(default)]
    pub devices: HashMap<String, DeviceConfig>,
    /// Application-wide color scheme.
    #[serde(default)]
    pub theme: ThemeMode,
    /// Whether the device-list window was open when the app last exited.
    #[serde(default)]
    pub discovery_open: bool,
    /// Last known size of the device-list window.
    #[serde(default)]
    pub discovery_window_width: i32,
    #[serde(default)]
    pub discovery_window_height: i32,
    /// Enable UI animations: title/artist/album slide transitions and the
    /// artwork flip/fade. Defaults on; users can turn it off in Settings.
    #[serde(default = "default_animations")]
    pub animations: bool,
    /// Also apply RustyWiiM Modern's blurred-art background to the mini
    /// window (only meaningful when `theme == RustyWiiMModern`; the Settings
    /// toggle is greyed out otherwise). Defaults on.
    #[serde(default = "default_mini_modern")]
    pub mini_modern: bool,
    /// Highlight/accent colour (hex, e.g. "#4ecdc4") used for song progress,
    /// playback status, the play/pause button, and the side-panel toggle.
    /// Only meaningful for the two RustyWiiM themes (classic and Modern);
    /// the Settings control is greyed out for System/Light/Dark, which use
    /// Adwaita's own accent colour instead.
    #[serde(default = "default_accent_color")]
    pub accent_color: String,
    /// Hidden/debug-only: paint an explicit background behind the mini
    /// window's artwork+info row, working around a stale-GPU-pixel
    /// rendering glitch some users have seen there (NGL renderer). Off by
    /// default — the glitch hasn't been reliably reproduced since
    /// ScrollFadeLabel's rewrite, so this isn't worth another Settings row;
    /// flip it by hand-editing config.json (`"mini_stale_pixel_workaround":
    /// true`) if it turns up again.
    #[serde(default)]
    pub mini_stale_pixel_workaround: bool,
    /// Whether the device-picker list additionally fetches and shows
    /// title/artist/artwork for every tracked device, not just ones with an
    /// open window. On by default (Ben, 2026-07-11) — every known device is
    /// already polled continuously for liveness (Simple mode) regardless of
    /// this setting, and showing what's playing is the more useful default
    /// experience; the extra background HTTP/UPnP traffic it costs per
    /// device is the accepted trade-off, not something to hide behind an
    /// opt-in.
    #[serde(default = "default_devlist_song_info")]
    pub devlist_song_info: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            last_ip: String::new(),
            last_uuid: String::new(),
            devices: HashMap::new(),
            theme: ThemeMode::default(),
            discovery_open: false,
            discovery_window_width: 0,
            discovery_window_height: 0,
            animations: true,
            mini_modern: default_mini_modern(),
            accent_color: default_accent_color(),
            mini_stale_pixel_workaround: false,
            devlist_song_info: default_devlist_song_info(),
        }
    }
}

/// Remove trailing commas before `}` or `]` so VS Code / hand-edited configs
/// don't blow up the parser.  Handles string literals correctly (ignores
/// commas inside quoted values).
fn strip_trailing_commas(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut in_string = false;
    let mut escapes: u32 = 0;
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        if in_string {
            out.push(b);
            if b == b'\\' { escapes += 1; } else {
                if b == b'"' && escapes % 2 == 0 { in_string = false; }
                escapes = 0;
            }
        } else if b == b'"' {
            in_string = true;
            out.push(b);
        } else if b == b',' {
            // Peek past whitespace; if next token is } or ] this is a trailing comma.
            let mut j = i + 1;
            while j < bytes.len() && matches!(bytes[j], b' ' | b'\t' | b'\n' | b'\r') { j += 1; }
            if j < bytes.len() && matches!(bytes[j], b'}' | b']') {
                // skip the comma
            } else {
                out.push(b);
            }
        } else {
            out.push(b);
        }
        i += 1;
    }
    String::from_utf8(out).unwrap_or_else(|_| s.to_owned())
}

fn config_path() -> PathBuf {
    if let Some(p) = CONFIG_PATH_OVERRIDE.get() {
        if let Some(parent) = p.parent() {
            let _ = fs::create_dir_all(parent);
        }
        return p.clone();
    }
    let dir = dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from(".config"))
        .join("rustywiim");
    let _ = fs::create_dir_all(&dir);
    dir.join("config.json")
}

impl Config {
    fn load_from_disk() -> Self {
        if NO_CONFIG.load(Ordering::Relaxed) {
            return Self::default();
        }
        let path = config_path();
        dbg(&format!("loading from {}", path.display()));
        let text = match fs::read_to_string(&path) {
            Ok(s) => s,
            Err(e) => {
                if e.kind() != std::io::ErrorKind::NotFound {
                    eprintln!("[config] failed to read {}: {e}", path.display());
                }
                return Self::default();
            }
        };
        let cleaned = strip_trailing_commas(&text);
        match serde_json::from_str::<Self>(&cleaned) {
            Ok(cfg) => {
                dbg(&format!("loaded {} device(s): {:?}", cfg.devices.len(), cfg.devices.keys().collect::<Vec<_>>()));
                cfg
            }
            Err(e) => {
                eprintln!("[config] failed to parse {}: {e}", path.display());
                eprintln!("[config] file contents:\n{text}");
                eprintln!("[config] using defaults (discovery window will open)");
                Self::default()
            }
        }
    }

    fn write_to_disk(&self) {
        if NO_CONFIG.load(Ordering::Relaxed) {
            return;
        }
        // Never persist a bogus empty-uuid device entry — a safety net
        // against any call site that captures a device's uuid before it's
        // actually known (e.g. `device_mut("")` reached while a window is
        // still `Connecting`/offline) rather than guarding every such call
        // site individually. An empty key can never legitimately refer to
        // a real device, so this can never discard real data.
        if self.devices.contains_key("") {
            eprintln!("[config] dropping bogus empty-uuid device entry before saving");
            let mut sanitized = self.clone();
            sanitized.devices.remove("");
            if let Ok(json) = serde_json::to_string_pretty(&sanitized) {
                let _ = fs::write(config_path(), json);
            }
            return;
        }
        if let Ok(json) = serde_json::to_string_pretty(self) {
            let _ = fs::write(config_path(), json);
        }
    }

    /// Return the stored config for `uuid`, or a fresh default.
    pub fn device(&self, uuid: &str) -> DeviceConfig {
        self.devices.get(uuid).cloned().unwrap_or_default()
    }

    /// Return a mutable reference to the per-device config for `uuid`,
    /// inserting a default entry if none exists yet.
    pub fn device_mut(&mut self, uuid: &str) -> &mut DeviceConfig {
        self.devices.entry(uuid.to_string()).or_default()
    }

    /// Migrate legacy `last_ip` / `last_uuid` fields into the matching per-device
    /// entry and then clear them.  Should be called once at startup before any
    /// component reads `DeviceConfig::last_ip`.  Returns `true` if anything changed.
    pub fn migrate(&mut self) -> bool {
        if cfg!(debug_assertions) {
            // Just to avoid the name clash with the Rust `cfg!` macro below.
        }
        if self.last_ip.is_empty() && self.last_uuid.is_empty() {
            return false;
        }
        let uuid = std::mem::take(&mut self.last_uuid);
        let ip   = std::mem::take(&mut self.last_ip);
        if !uuid.is_empty() && !ip.is_empty() && self.devices.contains_key(&uuid) {
            let dev = self.device_mut(&uuid);
            if dev.last_ip.is_none() {
                dev.last_ip = Some(ip);
            }
            // Pin the device so it is visible in the discovery window after migration.
            if dev.pinned.is_none() {
                dev.pinned = Some(true);
            }
        }
        true
    }
}

// ── Live singleton ────────────────────────────────────────────────────────────
//
// All call sites (signal handlers, timer callbacks) run on the single GTK
// main-loop thread, so a thread_local is sufficient without needing Sync.
// The config is read from disk once, on first access, and kept live in
// memory after that — `with`/`update` read/mutate it directly instead of
// every call site doing its own load-mutate-save dance. This does mean
// hand-edits to config.json made while the app is already running are no
// longer picked up until restart, which matches the app's role as the sole
// writer once started.
thread_local! {
    static CONFIG: RefCell<Config> = RefCell::new(Config::load_from_disk());
}

/// Read-only access to the live config.
pub fn with<R>(f: impl FnOnce(&Config) -> R) -> R {
    CONFIG.with(|c| f(&c.borrow()))
}

/// Mutate the live config via `f`, then persist to disk — but only if `f`
/// actually changed something. Comparison is whole-`Config` equality rather
/// than tracking individual field writes, so a closure that touches several
/// fields (or several devices' entries) at once still costs exactly one
/// comparison and, if needed, one save — not one per field.
pub fn update<R>(f: impl FnOnce(&mut Config) -> R) -> R {
    let (result, changed) = CONFIG.with(|c| {
        let before = c.borrow().clone();
        let result = f(&mut c.borrow_mut());
        (result, *c.borrow() != before)
    });
    if changed { save(); }
    result
}

/// Reset every field the Settings window's Appearance page controls
/// (theme, mini-window Modern, animations, accent colour) to its default, in
/// one `update()` call. Callers still need to push the new values into their
/// widgets afterwards — this only touches the persisted config.
pub fn reset_ui_settings() {
    update(|cfg| {
        cfg.theme = ThemeMode::RustyWiiM;
        cfg.mini_modern = default_mini_modern();
        cfg.animations = default_animations();
        cfg.accent_color = default_accent_color();
    });
}

/// Write the current in-memory config to disk. `update()` already calls this
/// for you — exposed separately in case a caller ever wants to decouple
/// mutation from persistence (e.g. to batch or delay saves).
pub fn save() {
    CONFIG.with(|c| c.borrow().write_to_disk());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stale_playback_access_override_value_is_discarded_not_fatal() {
        let json = r#"{
            "devices": {
                "some-uuid": {
                    "name": "Living Room",
                    "playback_access_override": "player_status"
                }
            }
        }"#;
        let cfg: Config = serde_json::from_str(json).expect("stale AccessMethod value must not fail the whole document");
        let dev = cfg.devices.get("some-uuid").unwrap();
        assert_eq!(dev.name.as_deref(), Some("Living Room"));
        assert_eq!(dev.playback_access_override, None);
    }

    #[test]
    fn current_playback_access_override_values_still_parse() {
        let json = r#"{"devices": {"u": {"playback_access_override": "upnp_polled"}}}"#;
        let cfg: Config = serde_json::from_str(json).unwrap();
        assert_eq!(cfg.devices.get("u").unwrap().playback_access_override, Some(AccessMethod::UpnpPolled));
    }
}
