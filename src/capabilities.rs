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

// ── Device ID ─────────────────────────────────────────────────────────────────
//
// Discriminants are grouped by vendor with room to grow.  Each vendor block
// maps to its own profile array; DeviceId::profile() dispatches by range.
//   WiiM:          0–99
//   Arylic:      100–199
//   Audio Pro:   200–299
//   LinkPlay:   9999

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(usize)]
pub enum DeviceId {
    // WiiM
    WiimMini    = 0,
    WiimPro     = 1,
    WiimProPlus = 2,
    WiimAmp     = 3,
    WiimAmpPro  = 4,
    WiimUltra   = 5,
    WiimSound   = 6,
    WiimGeneric = 7,

    // Arylic / Up2Stream
    ArylicUp2StreamAmp = 100,
    ArylicH50          = 101,
    ArylicGeneric      = 102,

    // Audio Pro — specific models
    AudioProLink2   = 200,
    AudioProA28     = 201,
    AudioProAddonC5 = 202,
    // Audio Pro — generation-based generics (for unrecognized models)
    AudioProMkII    = 203,
    AudioProWGen    = 204,
    AudioProOriginal = 205,

    // Generic LinkPlay fallback
    LinkPlayGeneric = 9999,
}

impl DeviceId {
    /// Identify the device from the raw `project` and `firmware` fields
    /// (both from `getStatusEx`).  More-specific names are checked before
    /// less-specific substrings.  Firmware is used only for Audio Pro
    /// generation detection.
    pub fn detect(project: &str, fw: &str) -> Self {
        let p = normalize_project(project);

        // WiiM — compound names before simple substrings
        if p.contains("wiim_ultra")    { return Self::WiimUltra;   }
        if p.contains("wiim_amp_pro")  { return Self::WiimAmpPro;  }
        if p.contains("wiim_amp")      { return Self::WiimAmp;     }
        if p.contains("wiim_pro_plus") { return Self::WiimProPlus; }
        if p.contains("wiim_pro")      { return Self::WiimPro;     }
        if p.contains("wiim_mini") || p == "muzo_mini" {
            return Self::WiimMini;
        }
        if p.contains("wiim_sound")    { return Self::WiimSound;   }
        if p.contains("wiim")          { return Self::WiimGeneric; }

        // Arylic / Up2Stream — compound before simple
        if p.contains("up2stream_amp") { return Self::ArylicUp2StreamAmp; }
        if p.contains("arylic") && p.contains("h50") {
            return Self::ArylicH50;
        }
        if p.contains("arylic") || p.contains("up2stream") {
            return Self::ArylicGeneric;
        }

        // Audio Pro — specific models first, then generation-based generics
        if p.contains("link_2")   { return Self::AudioProLink2;   }
        if p.contains("a28")      { return Self::AudioProA28;     }
        if p.contains("addon_c5") { return Self::AudioProAddonC5; }

        if p.contains("audio_pro") || p.contains("addon")
            || matches!(p.as_str(), "a10" | "a15" | "c10")
            || fw.contains("audiopro")
        {
            return Self::detect_audio_pro_gen(&p, fw);
        }

        Self::LinkPlayGeneric
    }

    fn detect_audio_pro_gen(project: &str, fw: &str) -> Self {
        if project.contains("mkii") || project.contains("mk2")
            || project.contains("mk_ii") || project.contains("mark_ii")
            // Firmware 1.56–1.60 → MkII
            || fw.split('.').collect::<Vec<_>>().windows(2)
                .any(|s| s[0] == "1" && matches!(s[1], "56"|"57"|"58"|"59"|"60"))
        {
            return Self::AudioProMkII;
        }
        if project.contains("w_") || project.contains("w_series")
            || project.contains("w_generation")
            // Firmware 2.x → W-generation
            || fw.starts_with("2.")
        {
            return Self::AudioProWGen;
        }
        Self::AudioProOriginal
    }

    /// Vendor implied by this device ID.
    pub fn vendor(self) -> Vendor {
        match self {
            Self::WiimMini | Self::WiimPro | Self::WiimProPlus
            | Self::WiimAmp | Self::WiimAmpPro | Self::WiimUltra
            | Self::WiimSound | Self::WiimGeneric => Vendor::WiiM,

            Self::ArylicUp2StreamAmp | Self::ArylicH50
            | Self::ArylicGeneric                 => Vendor::Arylic,

            Self::AudioProLink2 | Self::AudioProA28 | Self::AudioProAddonC5
            | Self::AudioProMkII | Self::AudioProWGen
            | Self::AudioProOriginal              => Vendor::AudioPro,

            Self::LinkPlayGeneric                 => Vendor::LinkPlayGeneric,
        }
    }

    /// Per-device capability profile.
    pub fn profile(self) -> &'static DeviceProfile {
        let id = self as usize;
        match id {
            0..=99   => &WIIM_PROFILES[id],
            100..=199 => &ARYLIC_PROFILES[id - 100],
            200..=299 => &AUDIO_PRO_PROFILES[id - 200],
            _         => &LINKPLAY_PROFILES[0],
        }
    }
}

// ── Device profile ────────────────────────────────────────────────────────────

pub struct DeviceProfile {
    /// Marketing-friendly model name.  `None` for generic catch-all entries;
    /// those fall back to `model_name_fallback()` at runtime.
    pub model_name:      Option<&'static str>,
    /// plm_support bit positions to suppress (device incorrectly asserts
    /// bits for hardware it does not have).
    pub ignore_plm_bits: &'static [u8],
    /// Inputs guaranteed to be present on this device; added to the result
    /// from plm bitmap parsing if not already there.
    pub extra_inputs:    &'static [&'static str],
    /// Canonical output names available on this device.
    pub outputs:         &'static [&'static str],
}

// ── Per-vendor profile arrays ─────────────────────────────────────────────────
// Each array is indexed by (DeviceId as usize - vendor_base).
// DeviceId::profile() dispatches to the right array by numeric range.

static WIIM_PROFILES: [DeviceProfile; 8] = [
    /* 0 WiimMini */ DeviceProfile {
        model_name:      Some("WiiM Mini"),
        ignore_plm_bits: &[5],        // Coaxial not present
        extra_inputs:    &["bluetooth", "line-in", "optical"],
        outputs:         &["line-out", "optical-out"],
    },
    /* 1 WiimPro */ DeviceProfile {
        model_name:      Some("WiiM Pro"),
        ignore_plm_bits: &[2, 5],     // USB-C power only; Coaxial output only
        extra_inputs:    &["bluetooth", "line-in", "optical"],
        outputs:         &["line-out", "optical-out", "coax-out"],
    },
    /* 2 WiimProPlus */ DeviceProfile {
        model_name:      Some("WiiM Pro Plus"),
        ignore_plm_bits: &[2, 5],     // USB-C power only; Coaxial output only
        extra_inputs:    &["bluetooth", "line-in", "optical"],
        outputs:         &["line-out", "optical-out", "coax-out"],
    },
    /* 3 WiimAmp */ DeviceProfile {
        model_name:      Some("WiiM Amp"),
        ignore_plm_bits: &[],
        extra_inputs:    &["bluetooth", "line-in", "optical", "udisk", "hdmi"],
        outputs:         &["line-out", "usb-out"],
    },
    /* 4 WiimAmpPro */ DeviceProfile {
        model_name:      Some("WiiM Amp Pro"),
        ignore_plm_bits: &[],
        extra_inputs:    &["bluetooth", "line-in", "optical", "udisk"],
        outputs:         &["line-out", "usb-out"],
    },
    /* 5 WiimUltra */ DeviceProfile {
        model_name:      Some("WiiM Ultra"),
        ignore_plm_bits: &[],
        extra_inputs:    &["bluetooth", "line-in", "optical", "coaxial", "udisk", "hdmi", "phono"],
        outputs:         &["line-out", "optical-out", "coax-out", "headphone-out", "usb-out"],
    },
    /* 6 WiimSound */ DeviceProfile {
        model_name:      Some("WiiM Sound"),
        ignore_plm_bits: &[2, 3, 5],  // No USB, Optical, or Coaxial
        extra_inputs:    &["bluetooth", "line-in"],
        outputs:         &[],          // Internal speakers only
    },
    /* 7 WiimGeneric */ DeviceProfile {
        model_name:      None,
        ignore_plm_bits: &[],
        extra_inputs:    &["bluetooth", "line-in", "optical"],
        outputs:         &["optical-out", "line-out"],
    },
];

static ARYLIC_PROFILES: [DeviceProfile; 3] = [
    /* 100 ArylicUp2StreamAmp */ DeviceProfile {
        model_name:      Some("Arylic Up2Stream Amp"),
        ignore_plm_bits: &[],
        extra_inputs:    &["bluetooth", "line-in", "optical", "udisk"],
        outputs:         &["line-out"],
    },
    /* 101 ArylicH50 */ DeviceProfile {
        model_name:      Some("Arylic H50"),
        ignore_plm_bits: &[],
        extra_inputs:    &["bluetooth", "line-in", "optical", "udisk", "phono", "hdmi"],
        outputs:         &["line-out", "optical-out"],
    },
    /* 102 ArylicGeneric */ DeviceProfile {
        model_name:      None,
        ignore_plm_bits: &[],
        extra_inputs:    &["bluetooth", "line-in", "optical"],
        outputs:         &["line-out", "optical-out"],
    },
];

static AUDIO_PRO_PROFILES: [DeviceProfile; 6] = [
    /* 200 AudioProLink2 */ DeviceProfile {
        model_name:      Some("Audio Pro Link 2"),
        ignore_plm_bits: &[],
        extra_inputs:    &["bluetooth", "optical", "coaxial", "line-in"],
        outputs:         &[],
    },
    /* 201 AudioProA28 */ DeviceProfile {
        model_name:      Some("Audio Pro A28"),
        ignore_plm_bits: &[],
        extra_inputs:    &["bluetooth", "optical", "line-in", "hdmi"],
        outputs:         &[],
    },
    /* 202 AudioProAddonC5 */ DeviceProfile {
        model_name:      Some("Audio Pro Addon C5"),
        ignore_plm_bits: &[],
        extra_inputs:    &["bluetooth", "line-in"],
        outputs:         &[],
    },
    /* 203 AudioProMkII (generic — model unknown) */ DeviceProfile {
        model_name:      None,
        ignore_plm_bits: &[],
        extra_inputs:    &["bluetooth", "line-in", "optical"],
        outputs:         &[],
    },
    /* 204 AudioProWGen (generic — model unknown) */ DeviceProfile {
        model_name:      None,
        ignore_plm_bits: &[],
        extra_inputs:    &["bluetooth", "line-in", "optical"],
        outputs:         &[],
    },
    /* 205 AudioProOriginal (generic — model unknown) */ DeviceProfile {
        model_name:      None,
        ignore_plm_bits: &[],
        extra_inputs:    &["bluetooth", "line-in", "optical"],
        outputs:         &[],
    },
];

static LINKPLAY_PROFILES: [DeviceProfile; 1] = [
    /* 9999 LinkPlayGeneric */ DeviceProfile {
        model_name:      None,
        ignore_plm_bits: &[],
        extra_inputs:    &["bluetooth", "line-in", "optical"],
        outputs:         &["optical-out", "line-out"],
    },
];

// ── Capabilities ──────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct DeviceCapabilities {
    pub device_id:        DeviceId,
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

        let device_id = DeviceId::detect(&project_lc, &fw_lc);
        // For unrecognized devices, fall back to name/firmware-based detection
        // which can identify Arylic or Audio Pro devices by other signals.
        let vendor = if device_id == DeviceId::LinkPlayGeneric {
            detect_vendor_extended(&project_lc, &name_lc, &fw_lc)
        } else {
            device_id.vendor()
        };

        let model  = device_id.profile().model_name
            .map(|s| s.to_string())
            .unwrap_or_else(|| model_name_fallback(&project_lc, &info.device_name));
        let (supports_presets, supports_eq, supports_peq) =
            static_playback_caps(device_id);

        Self { device_id, vendor, model, supports_presets, supports_eq, supports_peq }
    }
}

// ── Internal helpers ──────────────────────────────────────────────────────────

/// Normalise the raw `project` field to lowercase with spaces/hyphens → underscores.
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

/// Extended vendor detection using device name and firmware in addition to
/// the project string.  Used only for `LinkPlayGeneric` fallback devices where
/// the project string alone is not enough.
fn detect_vendor_extended(project: &str, name_lc: &str, fw_lc: &str) -> Vendor {
    if is_known_wiim_model(project)
        || project.contains("wiim")
        || name_lc.contains("wiim")
    {
        return Vendor::WiiM;
    }
    if project.contains("arylic")
        || project.contains("up2stream")
        || project.contains("s10+")
        || project.contains("s10p")
        || name_lc.contains("arylic")
        || name_lc.contains("up2stream")
    {
        return Vendor::Arylic;
    }
    if project.contains("audio_pro")
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

/// Fallback model name for devices that hit a generic catch-all profile
/// (i.e. those where `DeviceProfile::model_name` is `None`).
/// Handles known project aliases not covered by any specific profile, then
/// uses the device's own advertised name, then title-cases the project slug.
fn model_name_fallback(project: &str, device_name: &str) -> String {
    let mapped = match project {
        "up2stream"         => Some("Arylic Up2Stream"),
        "s10+" | "s10_plus" => Some("Arylic S10+"),
        "addon_c10"         => Some("Audio Pro Addon C10"),
        "a10"               => Some("Audio Pro A10"),
        "a15"               => Some("Audio Pro A15"),
        "c10"               => Some("Audio Pro C10"),
        _                   => None,
    };

    if let Some(name) = mapped { return name.to_string(); }
    if !device_name.is_empty() { return device_name.to_string(); }

    // Title-case the project slug as last resort.
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

/// Static capability defaults for (supports_presets, supports_eq, supports_peq).
/// Matches pywiim's detect_device_capabilities() per-vendor branches.
/// PEQ is always `false` here — it requires a runtime probe.
fn static_playback_caps(device_id: DeviceId) -> (bool, bool, bool) {
    match device_id.vendor() {
        Vendor::WiiM => (true, true, false),

        Vendor::AudioPro => match device_id {
            DeviceId::AudioProMkII => (false, false, false),
            DeviceId::AudioProWGen => (true,  true,  false),
            _                      => (true,  false, false),
        },

        Vendor::Arylic | Vendor::LinkPlayGeneric => (true, false, false),
    }
}

// ── Input detection ───────────────────────────────────────────────────────────

/// plm_support bit index → canonical input source ID.
/// Bit meanings from pywiim's filter_plm_inputs / Arylic documentation.
static PLM_BIT_TO_INPUT: &[(u8, &str)] = &[
    (0, "line-in"),
    (1, "bluetooth"),
    (2, "udisk"),
    (3, "optical"),
    (5, "coaxial"),
    (7, "line-in-2"),
];

/// Detect available inputs for a device.
///
/// Algorithm:
/// 1. Decode `plm_support` bits using `PLM_BIT_TO_INPUT`.
/// 2. Remove inputs whose bit is in the device profile's `ignore_plm_bits`.
/// 3. Append any `extra_inputs` from the profile not already in the list.
/// 4. Prepend `"wifi"` (always available as a network streaming source).
pub fn detect_inputs(device_id: DeviceId, plm_support: u64) -> Vec<&'static str> {
    let profile = device_id.profile();

    // Step 1 — decode bitmap.
    let mut inputs: Vec<&'static str> = PLM_BIT_TO_INPUT.iter()
        .filter(|(bit, _)| plm_support & (1u64 << bit) != 0)
        .map(|(_, name)| *name)
        .collect();

    // Step 2 — drop bits the profile says are spurious.
    if !profile.ignore_plm_bits.is_empty() {
        inputs.retain(|&name| {
            let bit = PLM_BIT_TO_INPUT.iter()
                .find(|(_, n)| *n == name)
                .map(|(b, _)| *b);
            bit.map_or(true, |b| !profile.ignore_plm_bits.contains(&b))
        });
    }

    // Step 3 — add inputs guaranteed by the profile but absent from bitmap.
    for &extra in profile.extra_inputs {
        if !inputs.contains(&extra) {
            inputs.push(extra);
        }
    }

    // Step 4 — wifi is always first.
    inputs.retain(|&s| s != "wifi");
    inputs.insert(0, "wifi");

    inputs
}

/// Return the canonical output names available on a device.
/// XXX bluetooth-out needs proper runtime detection; omitted for now.
pub fn detect_outputs(device_id: DeviceId) -> &'static [&'static str] {
    device_id.profile().outputs
}

// ── Mode / name conversion helpers ───────────────────────────────────────────

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
        "line-in-2" => "Line-In 2",
        "optical"   => "Optical",
        "coaxial"   => "Coaxial",
        "udisk"     => "USB",
        "hdmi"      => "HDMI",
        "phono"     => "Phono",
        _           => id,
    }
}

/// Map a player mode number to the corresponding input source ID.
pub fn mode_to_input_source(mode: &str) -> &'static str {
    match mode {
        "40" | "44" | "60" => "line-in",
        "47"               => "line-in-2",
        "41"               => "bluetooth",
        "42" | "11" | "51" => "udisk",
        "43"               => "optical",
        "49"               => "hdmi",
        "54"               => "phono",
        _                  => "wifi",
    }
}

/// Translate a numerical output mode (from `getAudioOutputInfo` `hardware`
/// field) to a canonical output name.  Inverse of `output_canon_to_mode`.
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

/// Translate a raw `getAllRoutines` output payload string to a canonical output name.
/// XXX Incomplete — more payload strings to be mapped as they are discovered.
pub fn canon_routine_output_name(mode: &str) -> &'static str {
    match mode {
        "AUDIO_OUTPUT_COAX_MODE"  => "coax-out",
        "AUDIO_OUTPUT_SPDIF_MODE" => "optical-out",
        "AUDIO_OUTPUT_AUX_MODE"   => "line-out",
        _                         => "unknown",
    }
}
