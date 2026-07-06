/// Device capability detection.
///
/// Vendor and model normalization follows pywiim's profiles.py / model_names.py.
/// Capability defaults follow pywiim's detect_device_capabilities() logic.
/// PEQ support cannot be determined statically and starts as `false`; it must
/// be confirmed via a runtime probe before being set to `true`.

use std::sync::atomic::{AtomicBool, Ordering};

use super::api::{DeviceInfo, OutputEntry, WiimClient};
use super::playback::PlaybackAccessConfig;

pub static DEBUG_DEVICE: AtomicBool = AtomicBool::new(false);

fn dbg(msg: &str) {
    if DEBUG_DEVICE.load(Ordering::Relaxed) {
        println!("[device] {msg}");
    }
}

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

// ── Family profile ────────────────────────────────────────────────────────────
//
// A FamilyProfile captures device-family–level behaviour that is common across
// all individual models in that family: protocol preferences, UPnP vs HTTP state
// sources, which endpoints are available, and grouping topology.
//
// This mirrors pywiim's DeviceProfile dataclass (profiles.py).  Each DeviceId
// carries a reference to its family's static profile.  When a device cannot be
// identified (LinkPlayGeneric fallback), `detect_family_from_info()` runs the
// same vendor/generation logic pywiim uses and returns the matching family.

/// Which loop-mode integer scheme this family uses.
/// WiiM scheme and Arylic/LinkPlay scheme differ in their bit assignments.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoopModeScheme {
    WiiM,
    Arylic,
}

// `StateSourceConfig` (per-field HTTP-vs-UPnP preference booleans) used to
// live here as dead code — read only by a debug log line, never actually
// consulted by anything that fetches data. It's been replaced by
// `playback::PlaybackAccessConfig`, which gives that same intent a real
// consumer (`DeviceState`'s effective access-config resolution) and a
// Settings-UI-facing per-device override mechanism.
//
// Every family profile below defaults to `PlaybackAccessConfig::default()`
// (all-HTTP) rather than reinstating the old MkII UPnP-preference values,
// since `AccessMethod::UpnpPolled` has no real fetch implementation yet
// (see `device/upnp.rs`) — setting it as a *default* today would silently
// regress Audio Pro MkII rather than just leave it exactly as it behaves
// now. Revisit once UPnP fetching actually exists.

/// Connection and protocol settings.
#[derive(Debug)]
pub struct ConnectionConfig {
    /// Audio Pro MkII requires mTLS client certificate.
    pub requires_client_cert: bool,
    /// Ports to try, in order of preference.
    pub preferred_ports:      &'static [u16],
    /// Try HTTPS before HTTP.
    pub https_first:          bool,
    pub response_timeout_ms:  u32,
    pub retry_count:          u8,
}

/// Which API endpoints are available on this device family.
#[derive(Debug)]
pub struct EndpointConfig {
    /// `getPlayerStatusEx` available (Audio Pro MkII uses `getStatusEx` instead).
    pub supports_player_status_ex: bool,
    pub supports_get_meta_info:    bool,
    pub supports_eq:               bool,
    /// Some devices can read EQ but not write it (many Arylic).
    pub supports_eq_set:           bool,
    /// WiiM-only alarm scheduling endpoint.
    pub supports_alarms:           bool,
    /// WiiM-only sleep timer endpoint.
    pub supports_sleep_timer:      bool,
    /// Full path for the primary status poll (differs on Audio Pro MkII).
    pub status_endpoint:           &'static str,
    /// Command string for the reboot API call.
    pub reboot_command:            &'static str,
}

/// Multiroom grouping topology for this family.
#[derive(Debug)]
pub struct GroupingConfig {
    /// Gen1 devices use WiFi Direct peer-to-peer grouping instead of
    /// router-based multiroom.  Detected at runtime via `wmrm_version`.
    pub uses_wifi_direct: bool,
}

/// Complete behavioural profile for a device family.
#[derive(Debug)]
pub struct FamilyProfile {
    pub display_name:     &'static str,
    pub loop_mode_scheme: LoopModeScheme,
    pub playback_access:  PlaybackAccessConfig,
    pub connection:       ConnectionConfig,
    pub endpoints:        EndpointConfig,
    pub grouping:         GroupingConfig,
}

// ── Static family profiles ────────────────────────────────────────────────────

static FAMILY_WIIM: FamilyProfile = FamilyProfile {
    display_name:     "WiiM",
    loop_mode_scheme: LoopModeScheme::WiiM,
    playback_access: PlaybackAccessConfig::all_http(),
    connection: ConnectionConfig {
        requires_client_cert: false,
        // WiiM HTTPS:443 only; plain HTTP:80 is closed on WiiM hardware.
        preferred_ports:      &[443, 80],
        https_first:          true,
        response_timeout_ms:  5000,
        retry_count:          2,
    },
    endpoints: EndpointConfig {
        supports_player_status_ex: true,
        supports_get_meta_info:    true,
        supports_eq:               true,
        supports_eq_set:           true,
        supports_alarms:           true,
        supports_sleep_timer:      true,
        status_endpoint:           "/httpapi.asp?command=getPlayerStatusEx",
        reboot_command:            "reboot",
    },
    grouping: GroupingConfig { uses_wifi_direct: false },
};

static FAMILY_ARYLIC: FamilyProfile = FamilyProfile {
    display_name:     "Arylic",
    loop_mode_scheme: LoopModeScheme::Arylic,
    playback_access: PlaybackAccessConfig::all_http(),
    connection: ConnectionConfig {
        requires_client_cert: false,
        preferred_ports:      &[80, 443],
        https_first:          false,
        response_timeout_ms:  5000,
        retry_count:          2,
    },
    endpoints: EndpointConfig {
        supports_player_status_ex: true,
        supports_get_meta_info:    true,
        supports_eq:               true,
        supports_eq_set:           false, // Many Arylic devices: read-only EQ
        supports_alarms:           false,
        supports_sleep_timer:      false,
        status_endpoint:           "/httpapi.asp?command=getPlayerStatusEx",
        reboot_command:            "reboot",
    },
    grouping: GroupingConfig { uses_wifi_direct: false },
};

/// Audio Pro MkII: mTLS, UPnP-primary state, restricted endpoints.
static FAMILY_AUDIO_PRO_MKII: FamilyProfile = FamilyProfile {
    display_name:     "Audio Pro MkII",
    loop_mode_scheme: LoopModeScheme::Arylic,
    // NOTE: HTTP doesn't expose play state/volume/mute/position/duration/
    // source on MkII at all (the old dead `StateSourceConfig` flagged this
    // with `*_upnp: true` for exactly those fields) — but `AccessMethod::
    // UpnpPolled` has no real fetch implementation yet, so defaulting to it
    // here would make MkII strictly worse than today rather than neutral.
    // Left as the all-HTTP default for now; flip these to `UpnpPolled` once
    // `device/upnp.rs` actually implements fetching.
    playback_access: PlaybackAccessConfig::all_http(),
    connection: ConnectionConfig {
        requires_client_cert: true,
        preferred_ports:      &[4443, 8443, 443],
        https_first:          true,
        response_timeout_ms:  6000,
        retry_count:          3,
    },
    endpoints: EndpointConfig {
        supports_player_status_ex: false, // Uses getStatusEx instead
        supports_get_meta_info:    false,
        supports_eq:               false,
        supports_eq_set:           false,
        supports_alarms:           false,
        supports_sleep_timer:      false,
        status_endpoint:           "/httpapi.asp?command=getStatusEx",
        reboot_command:            "StartRebootTime:0",
    },
    grouping: GroupingConfig { uses_wifi_direct: false },
};

/// Audio Pro W-Generation: HTTPS-first, modern endpoints, no client cert.
static FAMILY_AUDIO_PRO_WGEN: FamilyProfile = FamilyProfile {
    display_name:     "Audio Pro W-Generation",
    loop_mode_scheme: LoopModeScheme::Arylic,
    playback_access: PlaybackAccessConfig::all_http(),
    connection: ConnectionConfig {
        requires_client_cert: false,
        preferred_ports:      &[443, 8443, 80],
        https_first:          true,
        response_timeout_ms:  4000,
        retry_count:          2,
    },
    endpoints: EndpointConfig {
        supports_player_status_ex: true,
        supports_get_meta_info:    true,
        supports_eq:               true,
        supports_eq_set:           true,
        supports_alarms:           false,
        supports_sleep_timer:      false,
        status_endpoint:           "/httpapi.asp?command=getPlayerStatusEx",
        reboot_command:            "StartRebootTime:0",
    },
    grouping: GroupingConfig { uses_wifi_direct: false },
};

/// Audio Pro Original (Gen1): HTTP-first, WiFi Direct grouping.
static FAMILY_AUDIO_PRO_ORIGINAL: FamilyProfile = FamilyProfile {
    display_name:     "Audio Pro Original",
    loop_mode_scheme: LoopModeScheme::Arylic,
    playback_access: PlaybackAccessConfig::all_http(),
    connection: ConnectionConfig {
        requires_client_cert: false,
        preferred_ports:      &[80, 443],
        https_first:          false,
        response_timeout_ms:  5000,
        retry_count:          2,
    },
    endpoints: EndpointConfig {
        supports_player_status_ex: true,
        supports_get_meta_info:    true,
        supports_eq:               false,
        supports_eq_set:           false,
        supports_alarms:           false,
        supports_sleep_timer:      false,
        status_endpoint:           "/httpapi.asp?command=getPlayerStatusEx",
        reboot_command:            "StartRebootTime:0",
    },
    // Gen1 Audio Pro uses WiFi Direct peer-to-peer grouping.
    grouping: GroupingConfig { uses_wifi_direct: true },
};

/// Generic LinkPlay: conservative HTTP-first defaults, probe to confirm.
static FAMILY_LINKPLAY_GENERIC: FamilyProfile = FamilyProfile {
    display_name:     "LinkPlay Generic",
    loop_mode_scheme: LoopModeScheme::Arylic,
    playback_access: PlaybackAccessConfig::all_http(),
    connection: ConnectionConfig {
        requires_client_cert: false,
        preferred_ports:      &[80, 443, 8080],
        https_first:          false,
        response_timeout_ms:  5000,
        retry_count:          2,
    },
    endpoints: EndpointConfig {
        supports_player_status_ex: true,
        supports_get_meta_info:    true,
        supports_eq:               true,
        supports_eq_set:           true,
        supports_alarms:           false,
        supports_sleep_timer:      false,
        status_endpoint:           "/httpapi.asp?command=getPlayerStatusEx",
        reboot_command:            "reboot",
    },
    grouping: GroupingConfig { uses_wifi_direct: false },
};

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
    WiimMini     = 0,
    WiimPro      = 1,
    WiimProPlus  = 2,
    WiimAmp      = 3,
    WiimAmpPro   = 4,
    WiimAmpUltra = 5,
    WiimUltra    = 6,
    WiimSound    = 7,
    WiimGeneric  = 8,

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
        if p.contains("wiim_ultra")     { return Self::WiimUltra;    }
        if p.contains("wiim_amp_ultra") { return Self::WiimAmpUltra; }
        if p.contains("wiim_amp_pro")   { return Self::WiimAmpPro;   }
        if p.contains("wiim_amp")       { return Self::WiimAmp;      }
        if p.contains("wiim_pro_plus")  { return Self::WiimProPlus;  }
        if p.contains("wiim_pro")       { return Self::WiimPro;      }
        if p.contains("wiim_mini") || p == "muzo_mini" {
            return Self::WiimMini;
        }
        if p.contains("wiim_sound")     { return Self::WiimSound;   }
        if p.contains("wiim")           { return Self::WiimGeneric; }

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
            || is_fw_audio_pro_mkii(fw)
        {
            return Self::AudioProMkII;
        }
        if project.contains("w_") || project.contains("w_series")
            || project.contains("w_generation") || project.contains("w_gen")
            || is_fw_audio_pro_wgen(fw)
        {
            return Self::AudioProWGen;
        }
        // Pywiim defaults to MkII for known modern Audio Pro models that don't
        // have explicit generation markers in the project string.
        Self::AudioProMkII
    }

    /// Vendor implied by this device ID.
    pub fn vendor(self) -> Vendor {
        match self {
            Self::WiimMini | Self::WiimPro | Self::WiimProPlus
            | Self::WiimAmp | Self::WiimAmpPro | Self::WiimAmpUltra
            | Self::WiimUltra | Self::WiimSound | Self::WiimGeneric => Vendor::WiiM,

            Self::ArylicUp2StreamAmp | Self::ArylicH50
            | Self::ArylicGeneric                 => Vendor::Arylic,

            Self::AudioProLink2 | Self::AudioProA28 | Self::AudioProAddonC5
            | Self::AudioProMkII | Self::AudioProWGen
            | Self::AudioProOriginal              => Vendor::AudioPro,

            Self::LinkPlayGeneric                 => Vendor::LinkPlayGeneric,
        }
    }

    /// Per-device capability profile (inputs, outputs, PLM mask).
    pub fn profile(self) -> &'static DeviceProfile {
        let id = self as usize;
        match id {
            0..=99   => &WIIM_PROFILES[id],
            100..=199 => &ARYLIC_PROFILES[id - 100],
            200..=299 => &AUDIO_PRO_PROFILES[id - 200],
            _         => &LINKPLAY_PROFILES[0],
        }
    }

    /// Family profile for this device ID.
    ///
    /// Audio Pro specific model IDs (Link2, A28, AddonC5) are not mapped here
    /// because their family (MkII vs W-Gen vs Original) depends on firmware
    /// version; `DeviceCapabilities::from_device_info` handles those via
    /// `detect_audio_pro_family()`.  `LinkPlayGeneric` is similarly handled
    /// via `detect_family_from_info()`.
    pub fn family_profile(self) -> &'static FamilyProfile {
        match self {
            Self::WiimMini | Self::WiimPro | Self::WiimProPlus | Self::WiimAmp
            | Self::WiimAmpPro | Self::WiimAmpUltra | Self::WiimUltra
            | Self::WiimSound | Self::WiimGeneric  => &FAMILY_WIIM,

            Self::ArylicUp2StreamAmp | Self::ArylicH50
            | Self::ArylicGeneric                  => &FAMILY_ARYLIC,

            Self::AudioProMkII                     => &FAMILY_AUDIO_PRO_MKII,
            Self::AudioProWGen                     => &FAMILY_AUDIO_PRO_WGEN,
            // Specific model IDs: fall back to Original; from_device_info
            // overrides with firmware-based detection.
            Self::AudioProLink2 | Self::AudioProA28
            | Self::AudioProAddonC5
            | Self::AudioProOriginal               => &FAMILY_AUDIO_PRO_ORIGINAL,

            Self::LinkPlayGeneric                  => &FAMILY_LINKPLAY_GENERIC,
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
    /// True for devices whose built-in speaker output is reported through
    /// the generic `AUDIO_OUTPUT_AUX_MODE`/`"line-out"` slot rather than a
    /// dedicated speaker enum value (confirmed via capture for WiiM Amp;
    /// `devName` already labels it "Speaker Out" correctly, so this only
    /// affects icon lookup and the static-profile fallback label — see
    /// `icon_canon_for_output()`). WiiM Amp Ultra's newer firmware instead
    /// reports `AUDIO_OUTPUT_SPEAKER_MODE` directly (already its own
    /// `"speaker-out"` canon, unaffected by this flag either way).
    pub line_out_is_speaker: bool,
}

// ── Per-vendor profile arrays ─────────────────────────────────────────────────
// Each array is indexed by (DeviceId as usize - vendor_base).
// DeviceId::profile() dispatches to the right array by numeric range.

static WIIM_PROFILES: [DeviceProfile; 9] = [
    /* 0 WiimMini */ DeviceProfile {
        model_name:      Some("WiiM Mini"),
        ignore_plm_bits: &[5],        // Coaxial not present
        extra_inputs:    &["bluetooth", "line-in", "optical"],
        outputs:         &["line-out", "optical-out"],
        line_out_is_speaker: false,
    },
    /* 1 WiimPro */ DeviceProfile {
        model_name:      Some("WiiM Pro"),
        ignore_plm_bits: &[2, 5],     // USB-C power only; Coaxial output only
        extra_inputs:    &["bluetooth", "line-in", "optical"],
        outputs:         &["line-out", "optical-out", "coax-out"],
        line_out_is_speaker: false,
    },
    /* 2 WiimProPlus */ DeviceProfile {
        model_name:      Some("WiiM Pro Plus"),
        ignore_plm_bits: &[2, 5],     // USB-C power only; Coaxial output only
        extra_inputs:    &["bluetooth", "line-in", "optical"],
        outputs:         &["line-out", "optical-out", "coax-out"],
        line_out_is_speaker: false,
    },
    /* 3 WiimAmp */ DeviceProfile {
        model_name:      Some("WiiM Amp"),
        ignore_plm_bits: &[],
        extra_inputs:    &["bluetooth", "line-in", "optical", "udisk", "HDMI"],
        outputs:         &["line-out", "usb-out"],
        line_out_is_speaker: true,
    },
    /* 4 WiimAmpPro */ DeviceProfile {
        model_name:      Some("WiiM Amp Pro"),
        ignore_plm_bits: &[],
        extra_inputs:    &["bluetooth", "line-in", "optical", "udisk"],
        outputs:         &["line-out", "usb-out"],
        line_out_is_speaker: true,
    },
    /* 5 WiimAmpUltra */ DeviceProfile {
        model_name:      Some("WiiM Amp Ultra"),
        ignore_plm_bits: &[],
        extra_inputs:    &["bluetooth", "line-in", "optical", "udisk", "HDMI"],
        outputs:         &["speaker-out"],
        // Real firmware already reports `AUDIO_OUTPUT_SPEAKER_MODE`
        // directly (canon `"speaker-out"`, not `"line-out"`), so this flag
        // is inert today — set for consistency/defense against firmware
        // variance.
        line_out_is_speaker: true,
    },
    /* 6 WiimUltra */ DeviceProfile {
        model_name:      Some("WiiM Ultra"),
        ignore_plm_bits: &[],
        extra_inputs:    &["bluetooth", "line-in", "optical", "coaxial", "udisk", "HDMI", "phono"],
        outputs:         &["line-out", "optical-out", "coax-out", "headphone-out", "usb-out"],
        line_out_is_speaker: false,
    },
    /* 7 WiimSound */ DeviceProfile {
        model_name:      Some("WiiM Sound"),
        ignore_plm_bits: &[2, 3, 5],  // No USB, Optical, or Coaxial
        extra_inputs:    &["bluetooth", "line-in"],
        outputs:         &[],          // Internal speakers only
        line_out_is_speaker: true,
    },
    /* 8 WiimGeneric */ DeviceProfile {
        model_name:      None,
        ignore_plm_bits: &[],
        extra_inputs:    &["bluetooth", "line-in", "optical"],
        outputs:         &["optical-out", "line-out"],
        line_out_is_speaker: false,
    },
];

static ARYLIC_PROFILES: [DeviceProfile; 3] = [
    /* 100 ArylicUp2StreamAmp */ DeviceProfile {
        model_name:      Some("Arylic Up2Stream Amp"),
        ignore_plm_bits: &[],
        extra_inputs:    &["bluetooth", "line-in", "optical", "udisk"],
        outputs:         &["line-out"],
        line_out_is_speaker: false,
    },
    /* 101 ArylicH50 */ DeviceProfile {
        model_name:      Some("Arylic H50"),
        ignore_plm_bits: &[],
        extra_inputs:    &["bluetooth", "line-in", "optical", "udisk", "phono", "HDMI"],
        outputs:         &["line-out", "optical-out"],
        line_out_is_speaker: false,
    },
    /* 102 ArylicGeneric */ DeviceProfile {
        model_name:      None,
        ignore_plm_bits: &[],
        extra_inputs:    &["bluetooth", "line-in", "optical"],
        outputs:         &["line-out", "optical-out"],
        line_out_is_speaker: false,
    },
];

static AUDIO_PRO_PROFILES: [DeviceProfile; 6] = [
    /* 200 AudioProLink2 */ DeviceProfile {
        model_name:      Some("Audio Pro Link 2"),
        ignore_plm_bits: &[],
        extra_inputs:    &["bluetooth", "optical", "coaxial", "line-in"],
        outputs:         &[],
        line_out_is_speaker: false,
    },
    /* 201 AudioProA28 */ DeviceProfile {
        model_name:      Some("Audio Pro A28"),
        ignore_plm_bits: &[],
        extra_inputs:    &["bluetooth", "optical", "line-in", "HDMI"],
        outputs:         &[],
        line_out_is_speaker: false,
    },
    /* 202 AudioProAddonC5 */ DeviceProfile {
        model_name:      Some("Audio Pro Addon C5"),
        ignore_plm_bits: &[],
        extra_inputs:    &["bluetooth", "line-in"],
        outputs:         &[],
        line_out_is_speaker: false,
    },
    /* 203 AudioProMkII (generic — model unknown) */ DeviceProfile {
        model_name:      None,
        ignore_plm_bits: &[],
        extra_inputs:    &["bluetooth", "line-in", "optical"],
        outputs:         &[],
        line_out_is_speaker: false,
    },
    /* 204 AudioProWGen (generic — model unknown) */ DeviceProfile {
        model_name:      None,
        ignore_plm_bits: &[],
        extra_inputs:    &["bluetooth", "line-in", "optical"],
        outputs:         &[],
        line_out_is_speaker: false,
    },
    /* 205 AudioProOriginal (generic — model unknown) */ DeviceProfile {
        model_name:      None,
        ignore_plm_bits: &[],
        extra_inputs:    &["bluetooth", "line-in", "optical"],
        outputs:         &[],
        line_out_is_speaker: false,
    },
];

static LINKPLAY_PROFILES: [DeviceProfile; 1] = [
    /* 9999 LinkPlayGeneric */ DeviceProfile {
        model_name:      None,
        ignore_plm_bits: &[],
        extra_inputs:    &["bluetooth", "line-in", "optical"],
        outputs:         &["optical-out", "line-out"],
        line_out_is_speaker: false,
    },
];

// ── Capabilities ──────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct DeviceCapabilities {
    pub device_id:          DeviceId,
    pub vendor:             Vendor,
    /// Marketing-friendly model name (e.g. "WiiM Pro Plus").
    pub model:              String,
    /// Family profile for protocol/endpoint/grouping behaviour.
    pub family:             &'static FamilyProfile,
    /// Effective loop mode scheme.  Normally `family.loop_mode_scheme`, but
    /// WiiM Ultra on firmware ≥ 5.2 switches to `Arylic` (pywiim issue #17).
    pub loop_mode_scheme:   LoopModeScheme,
    /// Effective WiFi Direct flag.  Normally `family.grouping.uses_wifi_direct`,
    /// but overridden to `true` for Gen1 devices detected via `wmrm_version`.
    pub uses_wifi_direct:   bool,
    pub supports_presets:   bool,
    pub supports_eq:        bool,
    /// Parametric EQ.  Cannot be determined statically; starts `false` and
    /// must be updated after a successful runtime probe.
    pub supports_peq:       bool,
    /// Resolved output list — from a live `getSoundCardModeSupportList`
    /// probe if the device supports it, else the static per-model fallback
    /// (`detect_outputs()`). Empty/harmless until `detect_capabilities()`
    /// populates it; that's the only thing that should set this for real.
    pub outputs:            Vec<OutputEntry>,
    /// Whether `getSoundCardModeSupportList` actually worked on this
    /// device. `state.rs` only reads this to decide whether to keep
    /// polling that endpoint on the slow-poll cycle — it doesn't need to
    /// know *why* the answer is what it is (static guess vs. live probe).
    pub probes_outputs:     bool,
    /// Consecutive `getSoundCardModeSupportList` failures during ongoing
    /// slow polling (not part of the initial `detect_capabilities()` probe,
    /// which is a single attempt). Private — `state.rs` reports results via
    /// `record_outputs_probe()` and never sees this counter directly; see
    /// `OUTPUTS_PROBE_FAIL_THRESHOLD`.
    outputs_probe_failures: u32,
    /// Resolved audio input list — `detect_inputs()`'s static plm_support-
    /// based list, `enabled` defaulting `true` for every entry, optionally
    /// amended by a one-time `getAudioInputEnable` probe in
    /// `detect_capabilities()` if that call succeeds and parses. `state.rs`
    /// additionally self-corrects this live: an entry marked `enabled:
    /// false` here gets forced back to `true` (with a warning) if the
    /// device's currently-polled playback mode maps to that same input —
    /// a capability snapshot can't be right calling something "disabled"
    /// while it's demonstrably in active use.
    pub inputs:             Vec<InputEntry>,
}

/// One resolved audio input: a canonical ID (from `detect_inputs()`'s fixed
/// set, or — if `getAudioInputEnable` reports something `detect_inputs()`
/// missed — the raw device-reported string, appended rather than dropped)
/// plus whether it's currently enabled.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InputEntry {
    pub id:      String,
    pub enabled: bool,
}

impl DeviceCapabilities {
    pub fn from_device_info(info: &DeviceInfo) -> Self {
        let project_lc = normalize_project(&info.project);
        let name_lc    = info.device_name.to_lowercase();
        let fw_lc      = info.firmware.to_lowercase();

        dbg(&format!(
            "raw  project={:?}  name={:?}  firmware={:?}  wmrm_version={:?}",
            info.project, info.device_name, info.firmware, info.wmrm_version,
        ));

        let device_id = DeviceId::detect(&project_lc, &fw_lc);
        // For unrecognized devices, fall back to name/firmware-based detection.
        let vendor = if device_id == DeviceId::LinkPlayGeneric {
            detect_vendor_extended(&project_lc, &name_lc, &fw_lc)
        } else {
            device_id.vendor()
        };

        dbg(&format!("DeviceId: {device_id:?}  vendor: {vendor:?}"));

        // Family profile: use the known device table's direct mapping when
        // the device was positively identified.  For Audio Pro specific model
        // IDs the family still depends on firmware (generation detection).
        // For completely unknown devices (LinkPlayGeneric) run pywiim-style
        // fallback detection from project/name/firmware.
        let (family, family_source): (&'static FamilyProfile, &'static str) = match device_id {
            DeviceId::LinkPlayGeneric => (
                detect_family_from_info(&project_lc, &name_lc, &fw_lc),
                "pywiim fallback (no table match)",
            ),
            DeviceId::AudioProLink2
            | DeviceId::AudioProA28
            | DeviceId::AudioProAddonC5 => (
                detect_audio_pro_family(&project_lc, &fw_lc),
                "known model, fw-based generation detection",
            ),
            _ => (device_id.family_profile(), "direct table lookup"),
        };

        dbg(&format!(
            "family: {:?}  (via {})",
            family.display_name, family_source,
        ));

        // WiiM Ultra on firmware ≥ 5.2 switches to Arylic loop mode (pywiim#17).
        let loop_mode_scheme = if device_id == DeviceId::WiimUltra
            && fw_ver_at_least(&info.firmware, 5, 2)
        {
            dbg("loop_mode_scheme: Arylic (WiiM Ultra fw ≥ 5.2 override)");
            LoopModeScheme::Arylic
        } else {
            family.loop_mode_scheme
        };

        // Gen1 devices (wmrm_version "2.0" or very old firmware) use WiFi Direct.
        let gen1 = is_gen1(&info.wmrm_version, &info.firmware);
        let uses_wifi_direct = family.grouping.uses_wifi_direct || gen1;
        if gen1 && !family.grouping.uses_wifi_direct {
            dbg("wifi_direct: true (Gen1 override via wmrm_version/firmware)");
        }

        let model = device_id.profile().model_name
            .map(|s| s.to_string())
            .unwrap_or_else(|| model_name_fallback(&project_lc, &info.device_name));
        let (supports_presets, supports_eq, supports_peq) =
            static_playback_caps(device_id);

        // Base input list — static, from plm_support + per-model profile.
        // `detect_capabilities()` may amend `enabled` (and append entries
        // this static detection missed) via a live `getAudioInputEnable`
        // probe; this is just the starting point.
        let inputs = detect_inputs(device_id, info.plm_support_value())
            .into_iter()
            .map(|id| InputEntry { id: id.to_string(), enabled: true })
            .collect();

        let caps = Self {
            device_id, vendor, model, family,
            loop_mode_scheme, uses_wifi_direct,
            supports_presets, supports_eq, supports_peq,
            inputs,
            // Harmless placeholders — only `detect_capabilities()` (the real
            // probing entry point) sets these to something meaningful.
            // Standalone callers of `from_device_info()` (e.g. `wiim-capture`,
            // which only wants the model name) never see these matter.
            outputs: Vec::new(), probes_outputs: true, outputs_probe_failures: 0,
        };

        if DEBUG_DEVICE.load(Ordering::Relaxed) {
            let pa = &caps.family.playback_access;
            let c  = &caps.family.connection;
            let ep = &caps.family.endpoints;

            dbg(&format!("model: {:?}  loop_mode: {:?}  wifi_direct: {}",
                caps.model, caps.loop_mode_scheme, caps.uses_wifi_direct));
            dbg(&format!("capabilities: presets={}  eq={}  peq={}",
                caps.supports_presets, caps.supports_eq, caps.supports_peq));
            dbg(&format!(
                "playback_access: status={:?}  timing={:?}  volume={:?}  metadata={:?}  artwork={:?}  source={:?}",
                pa.status, pa.timing, pa.volume, pa.metadata, pa.artwork, pa.source,
            ));
            dbg(&format!(
                "connection: ports={:?}  https_first={}  timeout={}ms  retries={}  client_cert={}",
                c.preferred_ports, c.https_first, c.response_timeout_ms,
                c.retry_count, c.requires_client_cert,
            ));
            dbg(&format!(
                "endpoints: player_status_ex={}  meta={}  eq={}  eq_set={}  alarms={}  sleep_timer={}",
                ep.supports_player_status_ex, ep.supports_get_meta_info,
                ep.supports_eq, ep.supports_eq_set,
                ep.supports_alarms, ep.supports_sleep_timer,
            ));
            dbg(&format!("  status_endpoint: {:?}", ep.status_endpoint));
            dbg(&format!("  reboot_command:  {:?}", ep.reboot_command));
        }

        caps
    }

    /// This device family's default `PlaybackAccessConfig` — the starting
    /// point `DeviceState` applies any per-device `PlaybackAccessOverride`
    /// on top of. Currently just the static per-family default
    /// (`self.family.playback_access`); once `detect_capabilities()` probes
    /// more than outputs, findings from that probing should feed into this
    /// too.
    pub fn playback_access(&self) -> PlaybackAccessConfig {
        self.family.playback_access
    }

    /// Consecutive `getSoundCardModeSupportList` failures tolerated during
    /// ongoing slow polling before giving up on it for this device
    /// (`probes_outputs` flips to `false`). Matches `state.rs`'s
    /// `SLOW_POLL_FAIL_THRESHOLD` — these embedded HTTP servers are flaky
    /// enough that a single miss shouldn't immediately be treated as
    /// "device doesn't support this."
    const OUTPUTS_PROBE_FAIL_THRESHOLD: u32 = 3;

    /// Report the result of one `getSoundCardModeSupportList` slow-poll
    /// attempt. `state.rs` just reports what happened here; this method —
    /// not the caller — decides whether/when to actually give up. Keeps the
    /// give-up policy (and the failure counter itself) entirely inside
    /// capabilities.rs: `state.rs` never sees the counter or the threshold,
    /// only the resulting `probes_outputs` flag and whatever it needs to
    /// know whether to emit `outputs-changed`.
    ///
    /// Returns `true` if `self.outputs` actually changed as a result (the
    /// caller emits `outputs-changed` when this is `true`).
    pub fn record_outputs_probe(&mut self, result: Option<Vec<OutputEntry>>) -> bool {
        match result {
            None => {
                self.outputs_probe_failures += 1;
                eprintln!(
                    "[device] getSoundCardModeSupportList failed ({}/{})",
                    self.outputs_probe_failures, Self::OUTPUTS_PROBE_FAIL_THRESHOLD,
                );
                if self.outputs_probe_failures >= Self::OUTPUTS_PROBE_FAIL_THRESHOLD {
                    eprintln!(
                        "[device] giving up on getSoundCardModeSupportList for this device \
                         after {} consecutive failures",
                        self.outputs_probe_failures,
                    );
                    self.probes_outputs = false;
                }
                false
            }
            Some(list) => {
                self.outputs_probe_failures = 0;
                if list != self.outputs {
                    self.outputs = list;
                    true
                } else {
                    false
                }
            }
        }
    }
}

/// Full capability detection for a live connection — the single place that
/// owns *both* the static classification (`from_device_info()`) and
/// whatever live probing is needed to resolve the rest of
/// `DeviceCapabilities`. Fetches `getStatusEx` (`WiimClient::
/// get_device_info()`) itself rather than requiring the caller to have
/// already fetched it, and currently probes `getSoundCardModeSupportList`
/// for output support — the same two calls `state.rs`'s `fetch_device_info`
/// used to make and interpret itself, now made once, here, and returned as
/// one flat, opaque result. `state.rs` never decides what to try or
/// interprets a failure; it only reads the result — the distinction between
/// "statically known" and "just probed" must never leak past this
/// function, so callers can't end up branching on *how* a fact was
/// determined instead of just what it is.
///
/// Returns `None` if `getStatusEx` itself fails — nothing else is worth
/// probing without basic device info.
pub async fn detect_capabilities(client: &WiimClient) -> Option<(DeviceInfo, DeviceCapabilities)> {
    let info = client.get_device_info().await.ok()?;
    let mut caps = DeviceCapabilities::from_device_info(&info);

    match client.get_sound_card_mode_support_list().await {
        Some(mut list) => {
            dbg(&format!("outputs from API: {:?}", list));
            for e in &mut list {
                e.icon_canon = icon_canon_for_output(e.canon, caps.device_id);
            }
            caps.outputs = list;
            caps.probes_outputs = true;
        }
        None => {
            dbg("getSoundCardModeSupportList not supported; using static profile");
            caps.outputs = detect_outputs(caps.device_id)
                .iter()
                .map(|&canon| {
                    let icon_canon = icon_canon_for_output(canon, caps.device_id);
                    let name = output_display_name(icon_canon).to_string();
                    OutputEntry { canon, icon_canon, name }
                })
                .collect();
            caps.probes_outputs = false;
        }
    }

    // Amend the static input list with a live enable/disable reading, if the
    // device supports the call and the response actually parses. Never
    // authoritative for *existence* — only for `enabled`, and only for
    // entries it actually mentions (missing entries keep their static
    // default rather than being dropped).
    match client.get_audio_input_enable().await {
        Some(entries) => {
            dbg(&format!("audio input enable: {:?}", entries));
            for e in &entries {
                // Case-insensitive match only — some devices report a mode
                // in different casing than our canonical ID between calls
                // or firmware versions. Never rewrite the *stored* ID to the
                // device's casing here: canonical IDs are sent verbatim as
                // the `setPlayerCmd:switchmode:{id}` wire value elsewhere
                // (`DeviceState::switch_input()`), and for `"HDMI"`
                // specifically that has to stay exactly the case the device
                // expects — comparing case-sensitively used to silently
                // append a second, differently-cased duplicate entry instead
                // of updating the existing one (visible as a duplicate input
                // in the UI, with a different icon since icon lookup is also
                // ID-keyed).
                if let Some(existing) = caps.inputs.iter_mut()
                    .find(|i| i.id.eq_ignore_ascii_case(&e.name))
                {
                    existing.enabled = e.is_enabled();
                } else {
                    eprintln!(
                        "[device] getAudioInputEnable reported {:?}, which isn't in the \
                         static input list for this device — adding it",
                        e.name,
                    );
                    caps.inputs.push(InputEntry { id: e.name.clone(), enabled: e.is_enabled() });
                }
            }
        }
        None => {
            dbg("getAudioInputEnable not supported or didn't parse; all inputs default enabled");
        }
    }

    Some((info, caps))
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

/// Fallback family detection for devices that hit the `LinkPlayGeneric`
/// DeviceId.  Mirrors pywiim's `get_device_profile()` logic: detect vendor
/// from project/name/firmware, then for Audio Pro also detect generation.
fn detect_family_from_info(
    project: &str,
    name_lc: &str,
    fw_lc:   &str,
) -> &'static FamilyProfile {
    let vendor = detect_vendor_extended(project, name_lc, fw_lc);
    match vendor {
        Vendor::WiiM            => &FAMILY_WIIM,
        Vendor::Arylic          => &FAMILY_ARYLIC,
        Vendor::AudioPro        => detect_audio_pro_family(project, fw_lc),
        Vendor::LinkPlayGeneric => &FAMILY_LINKPLAY_GENERIC,
    }
}

/// Select the Audio Pro family profile from firmware-based generation detection.
fn detect_audio_pro_family(project: &str, fw: &str) -> &'static FamilyProfile {
    match DeviceId::detect_audio_pro_gen(project, fw) {
        DeviceId::AudioProMkII => &FAMILY_AUDIO_PRO_MKII,
        DeviceId::AudioProWGen => &FAMILY_AUDIO_PRO_WGEN,
        _                      => &FAMILY_AUDIO_PRO_ORIGINAL,
    }
}

/// Firmware version 1.56–1.60 indicates Audio Pro MkII generation.
/// Mirrors pywiim `_MKII_FIRMWARE_RE = r"(?<!\d)1\.5[6-9](?!\d)|(?<!\d)1\.60(?!\d)"`.
fn is_fw_audio_pro_mkii(fw: &str) -> bool {
    let parts: Vec<&str> = fw.splitn(3, '.').collect();
    if parts.len() < 2 || parts[0] != "1" { return false; }
    parts[1].parse::<u32>().map_or(false, |n| (56..=60).contains(&n))
}

/// Firmware version 2.0–2.3 indicates Audio Pro W-Generation.
/// Mirrors pywiim `_W_GEN_FIRMWARE_RE = r"(?<!\d)2\.[0-3](?!\d)"`.
fn is_fw_audio_pro_wgen(fw: &str) -> bool {
    let parts: Vec<&str> = fw.splitn(3, '.').collect();
    parts.len() >= 2
        && parts[0] == "2"
        && parts[1].parse::<u32>().map_or(false, |n| n <= 3)
}

/// Returns `true` when firmware version is at least `major.minor`.
/// Used for WiiM Ultra FW ≥ 5.2 loop mode detection (pywiim issue #17).
fn fw_ver_at_least(fw: &str, major: u32, minor: u32) -> bool {
    let mut parts = fw.splitn(3, '.');
    let fmaj = parts.next().and_then(|s| s.parse::<u32>().ok()).unwrap_or(0);
    let fmin = parts.next().and_then(|s| s.parse::<u32>().ok()).unwrap_or(0);
    (fmaj, fmin) >= (major, minor)
}

/// Returns `true` for Gen1 devices that use WiFi Direct grouping.
/// `wmrm_version == "2.0"` is the primary signal; very old firmware
/// (< 4.2.8020) is the fallback when wmrm_version is absent.
fn is_gen1(wmrm_version: &str, fw: &str) -> bool {
    if wmrm_version == "2.0" { return true;  }
    if wmrm_version == "4.2" { return false; }
    if fw.is_empty()          { return false; }
    // Numeric comparison: parse up to three components and compare tuples.
    let parts: Vec<u32> = fw.splitn(4, '.')
        .take(3)
        .map(|s| s.parse().unwrap_or(0))
        .collect();
    match parts.as_slice() {
        [maj, min, patch] => (*maj, *min, *patch) < (4, 2, 8020),
        [maj, min]        => (*maj, *min) < (4, 2),
        [maj]             => *maj < 4,
        _                 => false,
    }
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

/// Detect available inputs for a device — the static base list
/// `DeviceCapabilities::from_device_info()` seeds `inputs` (`InputEntry`)
/// from. No longer `pub`: nothing outside this module needs the raw list
/// on its own since `caps.inputs` is now the one place callers read
/// resolved inputs from.
///
/// Algorithm:
/// 1. Decode `plm_support` bits using `PLM_BIT_TO_INPUT`.
/// 2. Remove inputs whose bit is in the device profile's `ignore_plm_bits`.
/// 3. Append any `extra_inputs` from the profile not already in the list.
/// 4. Prepend `"wifi"` (always available as a network streaming source).
fn detect_inputs(device_id: DeviceId, plm_support: u64) -> Vec<&'static str> {
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
        "speaker-out"   => Some(7),
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
        "speaker-out"   => "Speaker Out",
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
        "HDMI"      => "HDMI",
        "phono"     => "Phono",
        _           => id,
    }
}

/// Map a player mode number to the corresponding input source ID.
/// `mode` is the raw wire `PlayerStatus.mode` value (`DeviceState::
/// current_mode()`, now a plain `i32` — see that method's doc comment for
/// why the older overloading with a canonical source-ID string was removed
/// rather than converted).
///
/// Every canonical input ID here is lowercase *except* `"HDMI"` — not a
/// stylistic inconsistency, it matches the actual device wire format
/// exactly. Confirmed both ways: `getAudioInputEnable` reports it capitalized
/// (real captures), and the authoritative `wiim` SDK's own `InputMode` enum
/// (`consts.py`) has `HDMI`'s wire command name capitalized while every
/// other entry's is lowercase — this is genuinely how the device spells it,
/// on both the read and write (`setPlayerCmd:switchmode:{id}`) sides.
/// Sending lowercase `"hdmi"` back is silently rejected by real hardware.
pub fn mode_to_input_source(mode: i32) -> &'static str {
    match mode {
        40 | 44 | 60 => "line-in",
        47           => "line-in-2",
        41           => "bluetooth",
        42 | 11 | 51 => "udisk",
        43           => "optical",
        49           => "HDMI",
        54           => "phono",
        _            => "wifi",
    }
}

/// Translate a numerical output mode (from `getAudioOutputInfo` `hardware`
/// field) to a canonical output name.  Inverse of `output_canon_to_mode`.
/// This numbering matches the authoritative LinkPlay-maintained `wiim` SDK's
/// `AudioOutputHwMode` enum (`cmd` values), confirmed against a real WiiM Amp
/// Ultra capture reporting `hardware: "7"` while its only output — the
/// built-in amp speakers — was selected.
pub fn canon_mode_output_name(mode: u32) -> &'static str {
    match mode {
        1 => "optical-out",
        2 => "line-out",
        3 => "coax-out",
        4 => "headphone-out",
        7 => "speaker-out",
        8 => "usb-out",
        _ => "unknown",
    }
}

/// Translate the "new" output names as returned by `getAllRoutines` and
/// `getSoundCardModeSupportList` to our canonical output names.
/// XXX Incomplete — more payload strings to be mapped as they are discovered.
pub fn canon_new_output_name(mode: &str) -> &'static str {
    match mode {
        "AUDIO_OUTPUT_COAX_MODE"       => "coax-out",
        "AUDIO_OUTPUT_SPDIF_MODE"      => "optical-out",
        "AUDIO_OUTPUT_AUX_MODE"        => "line-out",
        "AUDIO_OUTPUT_PHONE_JACK_MODE" => "headphone-out",
        "AUDIO_OUTPUT_UAC_CARD_MODE"   => "usb-out",
        "AUDIO_OUTPUT_SPEAKER_MODE"    => "speaker-out",
        _                              => "unknown",
    }
}

/// Icon-lookup name for an output's canonical name, applying the
/// `DeviceProfile.line_out_is_speaker` quirk. Equal to `canon` except where
/// the quirk applies — never adjusts `canon` itself, which must keep
/// resolving to the correct wire value/hardware match.
pub fn icon_canon_for_output(canon: &'static str, device_id: DeviceId) -> &'static str {
    if canon == "line-out" && device_id.profile().line_out_is_speaker {
        "speaker-out"
    } else {
        canon
    }
}
