/// Device capability detection.
///
/// Vendor and model normalization follows pywiim's profiles.py / model_names.py.
/// Capability defaults follow pywiim's detect_device_capabilities() logic.
/// PEQ support cannot be determined statically and starts as `false`; it must
/// be confirmed via a runtime probe before being set to `true`.

use crate::api::DeviceInfo;

// ── Vendor ────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Vendor {
    WiiM,
    Arylic,
    AudioPro,
    LinkPlayGeneric,
}

impl Vendor {
    pub fn display_name(self) -> &'static str {
        match self {
            Vendor::WiiM            => "WiiM",
            Vendor::Arylic          => "Arylic",
            Vendor::AudioPro        => "Audio Pro",
            Vendor::LinkPlayGeneric => "LinkPlay",
        }
    }
}

// ── Capabilities ──────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct DeviceCapabilities {
    pub vendor:           Vendor,
    /// Marketing-friendly model name (e.g. "WiiM Pro Plus").
    pub model:            String,
    pub supports_presets: bool,
    pub supports_eq:      bool,
    /// Parametric EQ.  Cannot be determined statically; starts `false` and
    /// must be updated after a successful runtime probe.
    pub supports_peq:     bool,
}

impl DeviceCapabilities {
    pub fn from_device_info(info: &DeviceInfo) -> Self {
        let project_lc = normalize_project(&info.project);
        let name_lc    = info.device_name.to_lowercase();
        let fw_lc      = info.firmware.to_lowercase();

        let vendor = detect_vendor(&project_lc, &name_lc, &fw_lc);
        let model  = friendly_model_name(&project_lc, &info.device_name);
        let (supports_presets, supports_eq, supports_peq) =
            static_playback_caps(&vendor, &project_lc, &fw_lc);

        Self { vendor, model, supports_presets, supports_eq, supports_peq }
    }
}

// ── Internal helpers ──────────────────────────────────────────────────────────

/// Normalise the raw `project` field to lowercase with spaces → underscores.
fn normalize_project(project: &str) -> String {
    project.to_lowercase().replace([' ', '-'], "_")
}

/// Returns `true` for the known WiiM model alias set (from pywiim model_names.py).
fn is_known_wiim_model(project: &str) -> bool {
    matches!(
        project,
        "wiim_mini"
            | "wiim_pro"
            | "wiim_pro_plus"
            | "wiim_amp"
            | "wiim_amp_pro"
            | "wiim_ultra"
            | "wiim_pro_with_gc4a"
            | "wiim_amp_4layer"
            | "muzo_mini"
    ) || project.starts_with("wiim_")
}

/// Returns `true` for WiiM Ultra specifically (supports display-config API).
fn is_wiim_ultra(project: &str) -> bool {
    project == "wiim_ultra"
}

/// Determine the device vendor from the normalised project string, device name,
/// and firmware string.  Mirrors pywiim's `detect_vendor()` in profiles.py.
///
/// XXX This is missing all the normalization when falling back to device name
/// that is done by pywiim
fn detect_vendor(project: &str, name_lc: &str, fw_lc: &str) -> Vendor {
    
    // WiiM — known alias set, "wiim" substring in project or friendly name
    if is_known_wiim_model(project)
        || project.contains("wiim")
        || name_lc.contains("wiim")
    {
        return Vendor::WiiM;
    }

    // Arylic / Up2Stream
    if project.contains("arylic")
        || project.contains("up2stream")
        || project.contains("s10+")
        || project.contains("s10p")
        || name_lc.contains("arylic")
        || name_lc.contains("up2stream")
    {
        return Vendor::Arylic;
    }

    // Audio Pro — model substrings or firmware signature
    if project.contains("audio_pro")
        || project.contains("audio pro")
        || project.contains("addon")
        || matches!(project, "a10" | "a15" | "a28" | "c10")
        || name_lc.contains("audio pro")
        || name_lc.contains("addon")
        || fw_lc.contains("audiopro")
    {
        return Vendor::AudioPro;
    }

    Vendor::LinkPlayGeneric
}

/// Convert a normalised project string to a marketing-friendly model name.
/// Mirrors pywiim's `_FRIENDLY_MODEL_MAP` and `to_friendly_model_name()`.
fn friendly_model_name(project: &str, device_name: &str) -> String {
    let mapped = match project {
        "muzo_mini" | "wiim_mini"           => Some("WiiM Mini"),
        "wiim_pro" | "wiim_pro_with_gc4a"   => Some("WiiM Pro"),
        "wiim_pro_plus"                      => Some("WiiM Pro Plus"),
        "wiim_amp" | "wiim_amp_4layer"       => Some("WiiM Amp"),
        "wiim_amp_pro"                       => Some("WiiM Amp Pro"),
        "wiim_ultra"                         => Some("WiiM Ultra"),
        "up2stream"                          => Some("Arylic Up2Stream"),
        "s10+" | "s10_plus"                  => Some("Arylic S10+"),
        "addon_c10"                          => Some("Audio Pro Addon C10"),
        "a10"                                => Some("Audio Pro A10"),
        "a15"                                => Some("Audio Pro A15"),
        "a28"                                => Some("Audio Pro A28"),
        "c10"                                => Some("Audio Pro C10"),
        _                                    => None,
    };

    if let Some(name) = mapped {
        return name.to_string();
    }

    // Fallback: use the device's own friendly name if non-empty, otherwise
    // convert the project slug ("some_model_x") to title case ("Some Model X").
    if !device_name.is_empty() {
        return device_name.to_string();
    }

    project
        .split('_')
        .map(|word| {
            let mut chars = word.chars();
            match chars.next() {
                None    => String::new(),
                Some(c) => c.to_uppercase().to_string() + chars.as_str(),
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// Audio Pro generation, used to tune capability defaults.
/// Mirrors pywiim's `detect_audio_pro_generation()`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AudioProGen {
    MkII,
    WGeneration,
    Original,
}

fn detect_audio_pro_gen(project: &str, fw_lc: &str) -> AudioProGen {
    if project.contains("mkii")
        || project.contains("mk2")
        || project.contains("mk_ii")
        || project.contains("mark_ii")
        // Firmware 1.56–1.60 → MkII
        || fw_lc
            .split('.')
            .collect::<Vec<_>>()
            .windows(2)
            .any(|p| p[0] == "1" && matches!(p[1], "56"|"57"|"58"|"59"|"60"))
    {
        return AudioProGen::MkII;
    }

    if project.contains("w_")
        || project.contains("w_series")
        || project.contains("w_generation")
        // Firmware 2.x → W-generation
        || fw_lc.starts_with("2.")
    {
        return AudioProGen::WGeneration;
    }

    AudioProGen::Original
}

/// Static capability defaults for (supports_presets, supports_eq, supports_peq).
/// Matches pywiim's detect_device_capabilities() per-vendor branches.
/// PEQ is always `false` here — it requires a runtime probe.
fn static_playback_caps(vendor: &Vendor, project: &str, fw_lc: &str) -> (bool, bool, bool) {
    match vendor {
        Vendor::WiiM => {
            // All WiiM devices support presets and EQ.
            // Ultra supports display-config but that's tracked separately when needed.
            let _ = is_wiim_ultra(project); // reserved for future use
            (true, true, false)
        }

        Vendor::AudioPro => match detect_audio_pro_gen(project, fw_lc) {
            AudioProGen::MkII        => (false, false, false),
            AudioProGen::WGeneration => (true,  true,  false),
            AudioProGen::Original    => (true,  false, false),
        },

        // Arylic and generic LinkPlay: presets yes, EQ/PEQ unknown/no by default.
        Vendor::Arylic | Vendor::LinkPlayGeneric => (true, false, false),
    }
}

// ── Legacy helpers used by ui.rs ──────────────────────────────────────────────
// These operate directly on the raw `project` field and are retained for
// compatibility.  New code should prefer DeviceCapabilities.
//
// XXX This needs to be dramatically improved (see pywiim)

/// Detect available inputs from the device's `project` field (from getStatusEx).
pub fn detect_inputs(project: &str) -> &'static [&'static str] {
    let p = project.to_lowercase();
    if p.contains("ultra") {
        return &["wifi", "bluetooth", "line-in", "optical", "coaxial", "udisk", "HDMI", "phono"];
    }
    if p.contains("amp") {
        return &["wifi", "bluetooth", "line-in", "optical", "udisk", "HDMI"];
    }
    if p.contains("pro") {
        return &["wifi", "bluetooth", "line-in", "optical"];
    }
    if p.contains("mini") {
        return &["wifi", "bluetooth", "line-in", "optical"];
    }
    if p.contains("sound") {
        return &["wifi", "bluetooth", "line-in"];
    }
    &["wifi", "bluetooth", "line-in", "optical"]
}

/// Detect available outputs from the device's `project` field.
/// Returns canonical output names (see `output_canon_to_mode` and
/// `output_display_name` to convert to mode numbers and UI labels).
/// XXX bluetooth-out needs proper runtime detection; omitted for now.
pub fn detect_outputs(project: &str) -> Vec<&'static str> {
    let p = project.to_lowercase();

    if p.contains("amp ultra") || (p.contains("ultra") && p.contains("amp")) {
        return vec!["line-out", "usb-out", "hdmi-out"];
    } else if p.contains("ultra") {
        return vec!["line-out", "optical-out", "coax-out", "headphone-out", "usb-out"];
    } else if p.contains("amp pro") || (p.contains("amp") && p.contains("pro")) {
        return vec!["line-out", "usb-out"];
    } else if p.contains("amp") {
        return vec!["line-out", "usb-out"];
    } else if p.contains("mini") {
        return vec!["line-out", "optical-out"];
    } else if p.contains("pro") {
        return vec!["line-out", "optical-out", "coax-out"];
    }
    vec!["optical-out", "line-out"]
}

/// Convert a canonical output name to the API mode number used by
/// `setAudioOutput`.  Returns `None` for unknown names.
pub fn output_canon_to_mode(name: &str) -> Option<u32> {
    match name {
        "optical-out"   => Some(1),
        "line-out"      => Some(2),
        "coax-out"      => Some(3),
        "headphone-out" => Some(4),
        "bluetooth-out" => Some(4),
        "hdmi-out"      => Some(7),
        "usb-out"       => Some(8),
        _               => None,
    }
}

/// Human-readable display name for a canonical output name.
pub fn output_display_name(name: &str) -> &'static str {
    match name {
        "optical-out"   => "Optical Out",
        "line-out"      => "Line Out",
        "coax-out"      => "Coax Out",
        "headphone-out" => "Headphone Out",
        "hdmi-out"      => "HDMI Out",
        "usb-out"       => "USB Out",
        "bluetooth-out" => "Bluetooth Out",
        _               => "Unknown",
    }
}

/// Human-readable label for an input source ID.
pub fn input_display_name(id: &str) -> &str {
    match id {
        "wifi"      => "Network",
        "bluetooth" => "Bluetooth",
        "line-in"   => "Line-In",
        "optical"   => "Optical",
        "coaxial"   => "Coaxial",
        "udisk"     => "USB",
        "HDMI"      => "HDMI",
        "phono"     => "Phono",
        _           => id,
    }
}

/// Map a player mode number to the corresponding input source ID.
pub fn mode_to_input_source(mode: &str) -> &'static str {
    match mode {
        "40" | "44" | "60" => "line-in",
        "41"               => "bluetooth",
        "42" | "11" | "51" => "udisk",
        "43"               => "optical",
        "49"               => "HDMI",
        "54"               => "phono",
        _                  => "wifi",
    }
}

/// Translate a numerical output mode (as returned by the `getAudioOutputInfo`
/// `hardware` field) to a canonical output name.  Inverse of `output_canon_to_mode`.
pub fn canon_mode_output_name(mode: u32) -> &'static str {
    match mode {
        1 => "optical-out",
        2 => "line-out",
        3 => "coax-out",
        4 => "headphone-out",
        7 => "hdmi-out",
        8 => "usb-out",
        _ => "unknown",
    }
}

/// Translate a raw `getAllRoutines` output payload string to a canonical output
/// name.
/// XXX Incomplete — more payload strings to be mapped as they are discovered.
pub fn canon_routine_output_name(mode: &str) -> &'static str {
    match mode {
        "AUDIO_OUTPUT_COAX_MODE"  => "coax-out",
        "AUDIO_OUTPUT_SPDIF_MODE" => "optical-out",
        "AUDIO_OUTPUT_AUX_MODE"   => "line-out",
        _                         => "unknown",
    }
}
