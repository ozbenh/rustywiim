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
        println!("{} [config] {msg}", crate::timestamp());
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
/// Both the app-wide (`Config::gena_enabled`) and per-device
/// (`DeviceConfig::gena_enabled`) GENA toggles default on — GENA only ever
/// starts a session when *both* are true (see `resolved_gena_enabled()`).
fn default_gena_enabled() -> bool { true }
/// Per-theme default accent color, used whenever `Config::accent_color` is
/// `None` (the user hasn't explicitly overridden it via Settings' "Override
/// accent color" switch). `RustyWiiM`/`RustyWiiMModern` keep the teal that
/// was hardcoded in dark.css before the accent became configurable at all,
/// so existing users see no visual change by default. `RustyWiiMWood` uses
/// a bright orange, `#ff8000` (Ben's pick, 2026-07-24) — a teal accent
/// doesn't suit a warm walnut theme. gthibo/Wiim-Dashboard's own "Rust"
/// token (`--primary: #B3441E`, see THEMING.md) was the first default
/// tried here, sourced from the reference for traceability, but Ben
/// preferred a brighter, more saturated orange in practice.
/// System/SystemLight/SystemDark never read this (Settings greys the
/// accent controls out for them; those themes use Adwaita's own accent
/// instead), so they're folded into the catch-all rather than listed.
pub fn default_accent_for_theme(theme: ThemeMode) -> &'static str {
    match theme {
        ThemeMode::RustyWiiMWood => "#ff8000",
        _                        => "#4ecdc4",
    }
}
fn default_devlist_song_info() -> bool { true }
/// Matches `ScrollFadeLabel::SPEED_DEFAULT`.
fn default_scroll_speed() -> f64 { 0.6 }
fn default_kiosk_auto_hide_controls() -> bool { true }
fn default_kiosk_auto_hide_all_controls() -> bool { true }
fn default_kiosk_screensaver_enable() -> bool { true }
/// Settings' own slider range is 10s-600s (10 minutes).
fn default_kiosk_screensaver_timeout_secs() -> u32 { 30 }
fn default_kiosk_screensaver_include_phys_inputs() -> bool { true }
fn default_kiosk_hide_cursor_on_touch() -> bool { true }

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
    /// RustyWiiM Wood: a CSS-only "hi-fi hardware" reskin experiment (walnut
    /// wood-grain background, beveled gradient/box-shadow controls in place
    /// of flat buttons) — see THEMING.md for the design write-up and the
    /// reference material it's modeled on.
    #[serde(rename = "rusty_wiim_wood")]
    RustyWiiMWood,
}

/// Kiosk mode's "Inhibit System Screensaver" setting — see
/// `ui::kiosk`'s doc comment on its own inhibit-cookie handling for the
/// rationale behind offering all three instead of a plain on/off.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum InhibitSystemScreensaver {
    Never,
    #[default]
    WhenPlaying,
    Always,
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
    /// Not a `TlsMode` field directly — that type isn't `Serialize`/`Deserialize`
    /// — a plain integer avoids needing to derive that just for this one
    /// persisted field.
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
    /// Same override mechanism as `playback_access_override`, but for the
    /// loop-mode (shuffle/repeat) write path specifically. Separate field
    /// because HTTP `setPlayerCmd:loopmode:5` (shuffle + repeat-one) is
    /// confirmed silently ignored on at least the WiiM Mini (works fine on
    /// WiiM Ultra and the Audio Pro Addon C5) — the global default is
    /// `UpnpPolled` (`PlayQueue.SetQueueLoopMode`, the same path the WiiM
    /// phone app itself uses), with this override available for a device
    /// where UPnP turns out to be the broken one instead.
    #[serde(default, skip_serializing_if = "Option::is_none", deserialize_with = "deserialize_lenient_access_override")]
    pub loop_mode_access_override: Option<AccessMethod>,
    /// Per-device GENA (UPnP eventing) on/off switch, editable via Settings'
    /// "Device -> Advanced" panel. GENA only ever starts a session for this
    /// device when this **and** the app-wide `Config::gena_enabled` are both
    /// true — see `resolved_gena_enabled()`. Defaults on.
    #[serde(default = "default_gena_enabled")]
    pub gena_enabled: bool,
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
            loop_mode_access_override: None,
            gena_enabled: default_gena_enabled(),
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
    /// Highlight/accent colour override (hex, e.g. "#4ecdc4") used for song
    /// progress, playback status, the play/pause button, and the side-panel
    /// toggle. `None` (the default — Settings' "Override accent color"
    /// switch off) means "use the active theme's own default", resolved via
    /// `default_accent_for_theme()` rather than a single fixed value, so
    /// each RustyWiiM-family theme can suit its own palette until the user
    /// explicitly picks one. Only meaningful for the RustyWiiM-family themes
    /// at all; the Settings control is greyed out for System/Light/Dark,
    /// which use Adwaita's own accent colour instead.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub accent_color: Option<String>,
    /// Marquee scroll speed for title/artist/album text (`ScrollFadeLabel`'s
    /// `speed` property, pixels/tick). User-adjustable in Settings'
    /// Appearance page.
    #[serde(default = "default_scroll_speed")]
    pub scroll_speed: f64,
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
    /// App-wide GENA (UPnP eventing) on/off switch. GENA only ever starts a
    /// session for a device when this **and** that device's own
    /// `DeviceConfig::gena_enabled` are both true — see
    /// `resolved_gena_enabled()`. Defaults on.
    #[serde(default = "default_gena_enabled")]
    pub gena_enabled: bool,
    /// The device Kiosk mode was last bound to — updated every time
    /// `KioskWindow::bind_device()` successfully binds a real device.
    /// Consulted when entering Kiosk mode unbound (from the discovery
    /// window's menu, or a fresh `--kiosk` launch) so it can restore this
    /// device instead of starting with nothing selected.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kiosk_last_uuid: Option<String>,
    /// Fade the floating chrome (device-name/sidebar/exit buttons) out
    /// after a few seconds of no mouse/touch activity, back in on any
    /// activity. Defaults on.
    #[serde(default = "default_kiosk_auto_hide_controls")]
    pub kiosk_auto_hide_controls: bool,
    /// When `kiosk_auto_hide_controls` is also on, additionally fades the
    /// bound device's own playback transport buttons and volume control
    /// out along with the floating chrome, instead of just the latter.
    /// Defaults on.
    #[serde(default = "default_kiosk_auto_hide_all_controls")]
    pub kiosk_auto_hide_all_controls: bool,
    /// Whether to inhibit the *system's* idle/screensaver/DPMS mechanism,
    /// and under what condition — see `InhibitSystemScreensaver`'s own doc
    /// comment.
    #[serde(default)]
    pub kiosk_inhibit_screensaver: InhibitSystemScreensaver,
    /// Whether Kiosk mode fades to black after `kiosk_screensaver_timeout_secs`
    /// of the bound device not `Playing`. Defaults on.
    #[serde(default = "default_kiosk_screensaver_enable")]
    pub kiosk_screensaver_enable: bool,
    /// Seconds of not-`Playing` before the screensaver triggers. Settings'
    /// own slider range is 10-600.
    #[serde(default = "default_kiosk_screensaver_timeout_secs")]
    pub kiosk_screensaver_timeout_secs: u32,
    /// Whether a physical input (line-in/optical/RCA/HDMI/phono/Bluetooth)
    /// counts as "not playing" for the screensaver's own idle clock, even
    /// while its `PlaybackStatus` reports `Playing` — a device parked on
    /// one can report `Playing` with nothing audible actually happening
    /// (nothing plugged in, a silent source), so without this the
    /// screensaver would never trigger for that class of input at all.
    /// Defaults on; only meaningful when `kiosk_screensaver_enable` is.
    #[serde(default = "default_kiosk_screensaver_include_phys_inputs")]
    pub kiosk_screensaver_include_phys_inputs: bool,
    /// Permanently hides the mouse cursor in Kiosk mode when a touch
    /// screen is detected (`kiosk::has_touchscreen()`), rather than only
    /// while idle the way `kiosk_auto_hide_controls` does — a touch
    /// screen's cursor has no real position to show (it only ever appears
    /// at whatever point was last touched, or the corner on startup), so
    /// unlike a mouse-driven setup there's no "not idle yet" state where
    /// showing it is actually useful. Defaults on; a no-op on a
    /// non-touch display (`has_touchscreen()` returns false).
    #[serde(default = "default_kiosk_hide_cursor_on_touch")]
    pub kiosk_hide_cursor_on_touch: bool,
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
            animations: default_animations(),
            mini_modern: default_mini_modern(),
            accent_color: None,
            scroll_speed: default_scroll_speed(),
            mini_stale_pixel_workaround: false,
            devlist_song_info: default_devlist_song_info(),
            gena_enabled: default_gena_enabled(),
            kiosk_last_uuid: None,
            kiosk_auto_hide_controls: default_kiosk_auto_hide_controls(),
            kiosk_auto_hide_all_controls: default_kiosk_auto_hide_all_controls(),
            kiosk_inhibit_screensaver: InhibitSystemScreensaver::default(),
            kiosk_screensaver_enable: default_kiosk_screensaver_enable(),
            kiosk_screensaver_timeout_secs: default_kiosk_screensaver_timeout_secs(),
            kiosk_screensaver_include_phys_inputs: default_kiosk_screensaver_include_phys_inputs(),
            kiosk_hide_cursor_on_touch: default_kiosk_hide_cursor_on_touch(),
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
                    eprintln!("{} [config] failed to read {}: {e}", crate::timestamp(), path.display());
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
                eprintln!("{} [config] failed to parse {}: {e}", crate::timestamp(), path.display());
                eprintln!("{} [config] file contents:\n{text}", crate::timestamp());
                eprintln!("{} [config] using defaults (discovery window will open)", crate::timestamp());
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
            eprintln!("{} [config] dropping bogus empty-uuid device entry before saving", crate::timestamp());
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

/// Combines the app-wide and per-device GENA toggles into the one resolved
/// bool `device::state::DeviceState::set_gena_enabled()` actually takes —
/// `device/` has no concept of "two separate switches" (it can't read
/// config at all), so callers pushing this into a `DeviceState` always
/// resolve it here first, never pass either flag through on its own.
pub fn resolved_gena_enabled(uuid: &str) -> bool {
    with(|cfg| cfg.gena_enabled && cfg.device(uuid).gena_enabled)
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
/// (theme, mini-window Modern, animations, accent colour, scroll speed) to
/// its default, in one `update()` call. Callers still need to push the new
/// values into their widgets afterwards — this only touches the persisted
/// config.
pub fn reset_ui_settings() {
    update(|cfg| {
        // `ThemeMode::default()` (its `#[default]` variant), not a
        // hardcoded value here — this drifted out of sync with the real
        // default once before (was `ThemeMode::RustyWiiM`, a full theme
        // switch away from the actual `RustyWiiMModern` default), so
        // deriving it keeps the two from silently diverging again.
        cfg.theme = ThemeMode::default();
        cfg.mini_modern = default_mini_modern();
        cfg.animations = default_animations();
        cfg.accent_color = None;
        cfg.scroll_speed = default_scroll_speed();
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
