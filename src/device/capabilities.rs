/// Device capability detection.
///
/// Vendor and model normalization follows pywiim's profiles.py / model_names.py.
/// Capability defaults follow pywiim's detect_device_capabilities() logic.
/// PEQ support cannot be determined statically and starts as `false`; it must
/// be confirmed via a runtime probe before being set to `true`.

use std::sync::atomic::{AtomicBool, Ordering};

use super::api::{ApiOutcome, DeviceInfo, OutputEntry, WiimClient};
use super::playback::AccessMethod;

pub static DEBUG_DEVICE: AtomicBool = AtomicBool::new(false);

fn dbg(msg: &str) {
    if DEBUG_DEVICE.load(Ordering::Relaxed) {
        println!("{} [device] {msg}", super::timestamp());
    }
}

// ── Vendor ────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Vendor {
    WiiM,
    Arylic,
    AudioPro,
    IEast,
    LinkPlayGeneric,
}

impl Vendor {
    pub fn display_name(self) -> &'static str {
        match self {
            Vendor::WiiM            => "WiiM",
            Vendor::Arylic          => "Arylic",
            Vendor::AudioPro        => "Audio Pro",
            Vendor::IEast           => "iEAST",
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
    pub supports_eq:               bool,
    /// Some devices can read EQ but not write it (many Arylic).
    pub supports_eq_set:           bool,
    /// WiiM-only alarm scheduling endpoint.
    pub supports_alarms:           bool,
    /// WiiM-only sleep timer endpoint.
    pub supports_sleep_timer:      bool,
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
    pub playback_access:  AccessMethod,
    pub connection:       ConnectionConfig,
    pub endpoints:        EndpointConfig,
    pub grouping:         GroupingConfig,
}

// ── Static family profiles ────────────────────────────────────────────────────
//
// Every family defaults to `AccessMethod::UpnpPolled`: we suspect
// Arylic/Audio Pro/iEAST all run the same LinkPlay-licensed OEM software
// stack, which returns more complete data (artwork, metadata) over UPnP than
// over its HTTP API. WiiM defaults to `UpnpPolled` too now that it's proven
// out, leaving no family on `Http` by default — still overridable per-device
// via Settings.
//
// Note: There is evidence in pywiim that at least some AudioPro models might
// have something older/crappier/less compliant. We will deal with it when we
// have access or captures.

static FAMILY_WIIM: FamilyProfile = FamilyProfile {
    display_name:     "WiiM",
    playback_access: AccessMethod::UpnpPolled,
    connection: ConnectionConfig {
        requires_client_cert: false,
        // WiiM HTTPS:443 only; plain HTTP:80 is closed on WiiM hardware.
        preferred_ports:      &[443, 80],
        https_first:          true,
        response_timeout_ms:  5000,
        retry_count:          2,
    },
    endpoints: EndpointConfig {
        supports_eq:               true,
        supports_eq_set:           true,
        supports_alarms:           true,
        supports_sleep_timer:      true,
        reboot_command:            "reboot",
    },
    grouping: GroupingConfig { uses_wifi_direct: false },
};

/// Tested with Arylic S10+ (`DeviceId::ArylicS10Plus`, project "S10P_WIFI").
static FAMILY_ARYLIC: FamilyProfile = FamilyProfile {
    display_name:     "Arylic",
    // Arylic's own developer docs list coverart/playlist as UPnP-only and
    // never mention `getMetaInfo` — HTTP can't deliver artwork here at all.
    playback_access: AccessMethod::UpnpPolled,
    connection: ConnectionConfig {
        requires_client_cert: false,
        preferred_ports:      &[80, 443],
        https_first:          false,
        response_timeout_ms:  5000,
        retry_count:          2,
    },
    endpoints: EndpointConfig {
        supports_eq:               true,
        supports_eq_set:           false, // Many Arylic devices: read-only EQ
        supports_alarms:           false,
        supports_sleep_timer:      false,
        reboot_command:            "reboot",
    },
    grouping: GroupingConfig { uses_wifi_direct: false },
};

/// Audio Pro MkII: mTLS, restricted endpoints.
static FAMILY_AUDIO_PRO_MKII: FamilyProfile = FamilyProfile {
    display_name:     "Audio Pro MkII",
    // Same shared-stack gap as Arylic (no artwork), plus the same mTLS
    // requirement as iEAST AudioCast. Not itself confirmed that playback
    // control is broken over HTTP, just that artwork isn't available there.
    playback_access: AccessMethod::UpnpPolled,
    connection: ConnectionConfig {
        requires_client_cert: true,
        preferred_ports:      &[4443, 8443, 443],
        https_first:          true,
        response_timeout_ms:  6000,
        retry_count:          3,
    },
    endpoints: EndpointConfig {
        supports_eq:               false,
        supports_eq_set:           false,
        supports_alarms:           false,
        supports_sleep_timer:      false,
        reboot_command:            "StartRebootTime:0",
    },
    grouping: GroupingConfig { uses_wifi_direct: false },
};

/// Audio Pro W-Generation: HTTPS-first, modern endpoints, no client cert.
static FAMILY_AUDIO_PRO_WGEN: FamilyProfile = FamilyProfile {
    display_name:     "Audio Pro W-Generation",
    playback_access: AccessMethod::UpnpPolled,
    connection: ConnectionConfig {
        requires_client_cert: false,
        preferred_ports:      &[443, 8443, 80],
        https_first:          true,
        response_timeout_ms:  4000,
        retry_count:          2,
    },
    endpoints: EndpointConfig {
        supports_eq:               true,
        supports_eq_set:           true,
        supports_alarms:           false,
        supports_sleep_timer:      false,
        reboot_command:            "StartRebootTime:0",
    },
    grouping: GroupingConfig { uses_wifi_direct: false },
};

/// Audio Pro Original (Gen1): WiFi Direct grouping. Also covers the Addon
/// C5 (`DeviceId::AudioProAddonC5`, physically confirmed) via
/// `detect_audio_pro_family()`'s firmware-based generation detection.
/// `supports_eq`/`supports_alarms` are known wrong for that specific unit
/// (`EQGetBand`/`EQGetList`/`getAlarmClock` do respond) but harmless — EQ/
/// alarms aren't consumed by any real behavior yet at all. `getMetaInfo`
/// support (that unit's own `getMetaInfo` returns "unknown command") needs
/// no similar per-family caveat: it's runtime-detected per connection
/// (`DeviceCapabilities::probes_meta_info`), not declared statically.
static FAMILY_AUDIO_PRO_ORIGINAL: FamilyProfile = FamilyProfile {
    display_name:     "Audio Pro Original",
    playback_access: AccessMethod::UpnpPolled,
    connection: ConnectionConfig {
        requires_client_cert: false,
        preferred_ports:      &[80, 443],
        https_first:          false,
        response_timeout_ms:  5000,
        retry_count:          2,
    },
    endpoints: EndpointConfig {
        supports_eq:               false,
        supports_eq_set:           false,
        supports_alarms:           false,
        supports_sleep_timer:      false,
        reboot_command:            "StartRebootTime:0",
    },
    // Gen1 Audio Pro uses WiFi Direct peer-to-peer grouping.
    grouping: GroupingConfig { uses_wifi_direct: true },
};

/// Generic LinkPlay: conservative defaults for a device this code couldn't
/// identify at all, probe to confirm.
static FAMILY_LINKPLAY_GENERIC: FamilyProfile = FamilyProfile {
    display_name:     "LinkPlay Generic",
    // No direct evidence for this specific fallback (it's whatever wasn't
    // identifiable, by definition) — but every family we *have* identified
    // needs UPnP now, so an unidentified device is more likely than not to
    // as well.
    playback_access: AccessMethod::UpnpPolled,
    connection: ConnectionConfig {
        requires_client_cert: false,
        preferred_ports:      &[80, 443, 8080],
        https_first:          false,
        response_timeout_ms:  5000,
        retry_count:          2,
    },
    endpoints: EndpointConfig {
        supports_eq:               true,
        supports_eq_set:           true,
        supports_alarms:           false,
        supports_sleep_timer:      false,
        reboot_command:            "reboot",
    },
    grouping: GroupingConfig { uses_wifi_direct: false },
};

/// iEAST AudioCast (project "iEAST-02"): a bare network-audio adapter with no
/// physical inputs. No `getMetaInfo`, same as the rest of this shared OEM
/// stack — but also the one family where we directly observed the device
/// audibly stuttering while being polled over HTTP, not just missing
/// artwork. Requires an mTLS client cert too (plain `curl -k` fails
/// outright), same as Audio Pro MkII.
///
/// Also used by the newer `IEastAudioCastPro` (`DeviceId`/`DeviceProfile`
/// are still separate — different inputs/outputs — only the family-level
/// endpoint/connection profile is shared): its own capture confirmed the
/// same no-`getMetaInfo` gap, and re-checking *this* device's original
/// capture alongside it showed `supports_alarms: false` here was simply
/// wrong — `getAlarmClock:0/1/2` all returned real data on this device too,
/// just never asserted by an existing test. `requires_client_cert: true`
/// costs AudioCast Pro nothing even though its own capture connected fine
/// without one: nothing in this codebase's connection logic actually acts
/// on this field (`ConnectionConfig::requires_client_cert` is read only for
/// `--debug` logging, grep confirms) — offering a client cert an ordinary
/// TLS server never asked for is harmless.
static FAMILY_IEAST_AUDIOCAST: FamilyProfile = FamilyProfile {
    display_name:     "iEAST AudioCast",
    playback_access: AccessMethod::UpnpPolled,
    connection: ConnectionConfig {
        requires_client_cert: true,
        preferred_ports:      &[80, 443, 8080],
        https_first:          false,
        response_timeout_ms:  5000,
        retry_count:          2,
    },
    endpoints: EndpointConfig {
        supports_eq:               true,
        supports_eq_set:           true,
        // Was `false` — confirmed wrong (see this static's own doc
        // comment) while investigating AudioCast Pro's own capture.
        supports_alarms:           true,
        supports_sleep_timer:      false,
        reboot_command:            "reboot",
    },
    grouping: GroupingConfig { uses_wifi_direct: false },
};


// ── Device ID ─────────────────────────────────────────────────────────────────
//
// Discriminants are grouped by vendor with room to grow.  Each vendor block
// maps to its own profile array; DeviceId::profile() dispatches by range.
//   WiiM:            0–99
//   Arylic:        100–199
//   Audio Pro:     200–299
//   iEAST:         300–399
//   LinkPlay:     9999

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
    ArylicS10Plus      = 103,

    // Audio Pro — specific models
    AudioProLink2   = 200,
    AudioProA28     = 201,
    AudioProAddonC5 = 202,
    // Audio Pro — generation-based generics (for unrecognized models)
    AudioProMkII    = 203,
    AudioProWGen    = 204,
    AudioProOriginal = 205,

    // iEAST
    IEastAudioCast    = 300,
    IEastAudioCastPro = 301,

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
        // Confirmed via a real capture (`captures/test-devices/
        // Arylic_S10P_WIFI_20260720_005332.json`, project "S10P_WIFI") —
        // matches both that literal wire spelling and the "s10+"/"s10_plus"
        // aliases `model_name_fallback()` already had a name mapped for
        // (added ahead of any capture confirming detection for it, so this
        // is the detection rule that was still missing).
        if p.contains("s10p") || p.contains("s10+") {
            return Self::ArylicS10Plus;
        }
        if p.contains("arylic") || p.contains("up2stream") {
            return Self::ArylicGeneric;
        }

        // Audio Pro — specific models first, then generation-based generics
        if p.contains("link_2")   { return Self::AudioProLink2;   }
        if p.contains("a28")      { return Self::AudioProA28;     }
        // "addon_c5" is this model's *marketing* naming; a real unit
        // (project "AudioPro_C5I") uses "c5i" on the wire instead —
        // confirmed physically (Ben, 2026-07-13: "AddonC5" printed on the
        // back of the actual device) rather than guessed from the string
        // alone, so this maps straight to the same `AudioProAddonC5`
        // variant rather than falling through to the generic
        // generation-detection fallback.
        if p.contains("addon_c5") || p.contains("c5i") { return Self::AudioProAddonC5; }

        // "Known" Audio Pro model string — `audio_pro` (proper separator),
        // `addon`, or one of the bare model codes. Distinct from the loose
        // `audiopro`/firmware-only fallback just below — pywiim's own
        // `detect_audio_pro_generation()` defaults *differently* depending
        // on which of these got it here (known model → MkII; reached only
        // via the loose fallback → Original), and this port had collapsed
        // that distinction away — see `detect_audio_pro_gen()`'s doc
        // comment for why that mattered on real hardware. `a10`/`a15`/`c10`
        // are substring checks, matching pywiim's own `detect_vendor()`
        // (`any(pro in model_lower for pro in [..., "a10", "a15", "c10"])`)
        // exactly — an earlier version of this port used an exact-string
        // match instead, which would miss a real device reporting e.g.
        // `"A10_MkII"` or `"AudioPro-A10"` and silently fall through to the
        // loose fallback below, getting the wrong generation default.
        if p.contains("audio_pro") || p.contains("addon")
            || p.contains("a10") || p.contains("a15") || p.contains("c10")
        {
            return Self::detect_audio_pro_gen(&p, fw, true);
        }
        // `audiopro` (no separator) and the firmware-only fallback — a real
        // device ("AudioPro_C5I", confirmed live, 2026-07-13) reports
        // `project` with "Audio"/"Pro" concatenated as one word, only
        // underscored before the model suffix, so the separator-only check
        // above missed it entirely and this fell through to
        // `LinkPlayGeneric`. pywiim has the identical gap
        // (`profiles.py`'s `detect_vendor()` only checks
        // `"audio pro" in model_lower`, space-separated) — not something
        // this port regressed from upstream, a genuinely new case neither
        // project's *vendor* detection handled on its own.
        if p.contains("audiopro") || fw.contains("audiopro") {
            return Self::detect_audio_pro_gen(&p, fw, false);
        }

        // iEAST
        if p == "ieast_02" { return Self::IEastAudioCast; }
        // "AudioCast Pro" (confirmed via a real capture, project
        // "AudioCast_M30" — `ali_pid: "AudioCast Pro"` in its own
        // getStatusEx response) — a distinct, apparently newer/higher-end
        // model in the same product line as the bare `IEastAudioCast`
        // above, not the same device: different `plm_support`, and (unlike
        // that one) it actually answers the Bluetooth-status commands —
        // see `IEAST_PROFILES[1]`'s own doc comment.
        if p.contains("audiocast") { return Self::IEastAudioCastPro; }

        Self::LinkPlayGeneric
    }

    /// `known_model` — whether `project` matched one of the "known Audio
    /// Pro model string" checks above (`audio_pro`/`addon`/a10/a15/c10),
    /// as opposed to only the loose `audiopro`(-no-separator)/firmware
    /// fallback. Mirrors pywiim's `detect_audio_pro_generation()` exactly,
    /// which has the *same* two-branch structure with different defaults
    /// for each — this port had collapsed both into one function that
    /// always defaulted to MkII, which is wrong for the loose-fallback
    /// case: confirmed live (2026-07-13) against a real, very-old-firmware
    /// ("3.7.4830") Audio Pro C5 unit — `project` "AudioPro_C5I", no
    /// separator, so only the loose fallback matches at all, and the unit
    /// turned out to be Gen1 hardware (HTTP-only — 443/4443 refuse the TCP
    /// connection outright, not even a TLS/cert failure), not MkII (which
    /// requires HTTPS + a client cert — `FAMILY_AUDIO_PRO_MKII`'s
    /// `preferred_ports` doesn't even include plain HTTP). Defaulting a
    /// loose-fallback match to MkII, as before this fix, would have made a
    /// real device like this one *unreachable* over any port it actually
    /// supports.
    fn detect_audio_pro_gen(project: &str, fw: &str, known_model: bool) -> Self {
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
        if known_model {
            // Pywiim defaults to MkII for known modern Audio Pro models
            // that don't have explicit generation markers in the project
            // string.
            Self::AudioProMkII
        } else {
            // Pywiim defaults to "original" (Gen1) when it only got here
            // via the loose/non-standard-model-string fallback.
            Self::AudioProOriginal
        }
    }

    /// Vendor implied by this device ID.
    pub fn vendor(self) -> Vendor {
        match self {
            Self::WiimMini | Self::WiimPro | Self::WiimProPlus
            | Self::WiimAmp | Self::WiimAmpPro | Self::WiimAmpUltra
            | Self::WiimUltra | Self::WiimSound | Self::WiimGeneric => Vendor::WiiM,

            Self::ArylicUp2StreamAmp | Self::ArylicH50
            | Self::ArylicGeneric | Self::ArylicS10Plus => Vendor::Arylic,

            Self::AudioProLink2 | Self::AudioProA28 | Self::AudioProAddonC5
            | Self::AudioProMkII | Self::AudioProWGen
            | Self::AudioProOriginal              => Vendor::AudioPro,

            Self::IEastAudioCast | Self::IEastAudioCastPro => Vendor::IEast,

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
            300..=399 => &IEAST_PROFILES[id - 300],
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
            | Self::ArylicGeneric | Self::ArylicS10Plus => &FAMILY_ARYLIC,

            Self::AudioProMkII                     => &FAMILY_AUDIO_PRO_MKII,
            Self::AudioProWGen                     => &FAMILY_AUDIO_PRO_WGEN,
            // Specific model IDs: fall back to Original; from_device_info
            // overrides with firmware-based detection.
            Self::AudioProLink2 | Self::AudioProA28 | Self::AudioProAddonC5
            | Self::AudioProOriginal               => &FAMILY_AUDIO_PRO_ORIGINAL,

            // AudioCastPro shares the bare AudioCast's family profile
            // (own DeviceId/DeviceProfile — different inputs/outputs — but
            // the same endpoint/connection behavior) — see
            // FAMILY_IEAST_AUDIOCAST's own doc comment.
            Self::IEastAudioCast | Self::IEastAudioCastPro => &FAMILY_IEAST_AUDIOCAST,

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
    /// True for device for whose line-out is a 3.5mm jack and not RCA
    pub line_out_is_jack: bool,
    /// Per-output display-name overrides: `(canonical_id, label)` pairs.
    pub output_labels:    &'static [(&'static str, &'static str)],
    /// Per-input display-name overrides: `(canonical_id, label)` pairs,
    /// checked before `input_display_name()`'s generic table — for devices
    /// whose own physical silkscreen/manual labeling differs from the
    /// generic name (e.g. a front jack printed "AUX In" rather than the
    /// generic "Line-In"). Empty for every profile except ones that need
    /// it; order doesn't matter, only exact `canonical_id` matches.
    pub input_labels:    &'static [(&'static str, &'static str)],
    /// True for devices whose "line-in" input is a single 3.5mm jack
    /// rather than the paired-RCA connector the generic "line-in" icon
    /// (`ui/icons.rs`) depicts — see `icon_canon_for_input()`, the only
    /// consumer. Doesn't affect the canonical id/switchmode value itself
    /// (still `"line-in"` either way), only which icon key gets looked up.
    pub line_in_is_jack: bool,
}

// ── Per-vendor profile arrays ─────────────────────────────────────────────────
// Each array is indexed by (DeviceId as usize - vendor_base).
// DeviceId::profile() dispatches to the right array by numeric range.

static WIIM_PROFILES: [DeviceProfile; 9] = [
    /* 0 WiimMini */ DeviceProfile {
        model_name:      Some("WiiM Mini"),
        ignore_plm_bits: &[2, 5],     // USB-C power only; Coaxial not present
        extra_inputs:    &["bluetooth", "line-in"],
        outputs:         &["line-out", "optical-out"],
        line_out_is_speaker: false, line_out_is_jack: true,
        output_labels:    &[("line-out", "Aux Out"), ("optical-out", "SPDIF Out")],
        input_labels:    &[("line-in", "Aux In")],
        line_in_is_jack: true,
    },
    /* 1 WiimPro */ DeviceProfile {
        model_name:      Some("WiiM Pro"),
        ignore_plm_bits: &[2, 5],     // USB-C power only; Coaxial output only
        extra_inputs:    &["bluetooth", "line-in", "optical"],
        outputs:         &["line-out", "optical-out", "coax-out"],
        output_labels: &[],
        line_out_is_speaker: false, line_out_is_jack: false,
        input_labels: &[], line_in_is_jack: false,
    },
    /* 2 WiimProPlus */ DeviceProfile {
        model_name:      Some("WiiM Pro Plus"),
        ignore_plm_bits: &[2, 5],     // USB-C power only; Coaxial output only
        extra_inputs:    &["bluetooth", "line-in", "optical"],
        outputs:         &["line-out", "optical-out", "coax-out"],
        line_out_is_speaker: false, line_out_is_jack: false,
        output_labels: &[],
        input_labels: &[], line_in_is_jack: false,
    },
    /* 3 WiimAmp */ DeviceProfile {
        model_name:      Some("WiiM Amp"),
        ignore_plm_bits: &[],
        extra_inputs:    &["bluetooth", "line-in", "optical", "udisk", "HDMI"],
        outputs:         &["line-out", "usb-out"],
        line_out_is_speaker: true, line_out_is_jack: false,
        output_labels: &[("line-out", "Speaker Out")],
        input_labels: &[], line_in_is_jack: false,
    },
    /* 4 WiimAmpPro */ DeviceProfile {
        model_name:      Some("WiiM Amp Pro"),
        ignore_plm_bits: &[],
        extra_inputs:    &["bluetooth", "line-in", "optical", "udisk"],
        outputs:         &["line-out", "usb-out"],
        line_out_is_speaker: true, line_out_is_jack: false,
        output_labels: &[],
        input_labels: &[], line_in_is_jack: false,
    },
    /* 5 WiimAmpUltra */ DeviceProfile {
        model_name:      Some("WiiM Amp Ultra"),
        ignore_plm_bits: &[],
        extra_inputs:    &["bluetooth", "line-in", "optical", "udisk", "HDMI"],
        outputs:         &["speaker-out", "usb-out"],
        line_out_is_speaker: true, line_out_is_jack: false,
        output_labels: &[],
        input_labels: &[], line_in_is_jack: false,
    },
    /* 6 WiimUltra */ DeviceProfile {
        model_name:      Some("WiiM Ultra"),
        ignore_plm_bits: &[],
        extra_inputs:    &["bluetooth", "line-in", "optical", "coaxial", "udisk", "HDMI", "phono"],
        outputs:         &["line-out", "optical-out", "coax-out", "headphone-out", "usb-out"],
        line_out_is_speaker: false, line_out_is_jack: false,
        output_labels: &[],
        input_labels: &[], line_in_is_jack: false,
    },
    /* 7 WiimSound */ DeviceProfile {
        model_name:      Some("WiiM Sound"),
        ignore_plm_bits: &[2, 3, 5],  // No USB, Optical, or Coaxial
        extra_inputs:    &["bluetooth", "line-in"],
        outputs:         &[],          // Internal speakers only
        line_out_is_speaker: true, line_out_is_jack: false,
        output_labels: &[],
        input_labels: &[], line_in_is_jack: true,
    },
    /* 8 WiimGeneric */ DeviceProfile {
        model_name:      None,
        ignore_plm_bits: &[],
        extra_inputs:    &["bluetooth", "line-in", "optical"],
        outputs:         &["optical-out", "line-out"],
        line_out_is_speaker: false, line_out_is_jack: false,
        output_labels: &[],
        input_labels: &[], line_in_is_jack: false,
    },
];

static ARYLIC_PROFILES: [DeviceProfile; 4] = [
    /* 100 ArylicUp2StreamAmp */ DeviceProfile {
        model_name:      Some("Arylic Up2Stream Amp"),
        ignore_plm_bits: &[],
        extra_inputs:    &["bluetooth", "line-in", "optical", "udisk"],
        outputs:         &["line-out"],
        line_out_is_speaker: false, line_out_is_jack: false,
        output_labels: &[],
        input_labels: &[], line_in_is_jack: false,
    },
    /* 101 ArylicH50 */ DeviceProfile {
        model_name:      Some("Arylic H50"),
        ignore_plm_bits: &[],
        extra_inputs:    &["bluetooth", "line-in", "optical", "udisk", "phono", "HDMI"],
        outputs:         &["line-out", "optical-out"],
        line_out_is_speaker: false, line_out_is_jack: false,
        output_labels: &[],
        input_labels: &[], line_in_is_jack: false,
    },
    /* 102 ArylicGeneric */ DeviceProfile {
        model_name:      None,
        ignore_plm_bits: &[],
        extra_inputs:    &["bluetooth", "line-in", "optical"],
        outputs:         &["line-out", "optical-out"],
        line_out_is_speaker: false, line_out_is_jack: false,
        output_labels: &[],
        input_labels: &[], line_in_is_jack: false,
    },
    /* 103 ArylicS10Plus */ DeviceProfile {
        model_name:      Some("Arylic S10+"),
        // `plm_support` (confirmed live, 2026-07-20: `0x8006`) decodes to
        // bluetooth + udisk via PLM_BIT_TO_INPUT, plus bit 15 (unmapped —
        // not a known input bit, ignored) — both left as-is. "line-in" is
        // real too (confirmed directly, not from plm_support, which
        // doesn't assert it) — a 3.5mm jack, not paired RCA.
        ignore_plm_bits: &[],
        extra_inputs:    &["line-in"],
        // A single entry, not `&["line-out", "optical-out"]` (its previous
        // value): confirmed live, 2026-07-20, that both physical outputs (a
        // 3.5mm jack and optical) are hardwired to always run
        // simultaneously — there's no selection concept between them, and
        // Arylic's own app has no output control either. Represented as
        // one "line-out" entry (keeps the jack icon via `line_out_is_jack`
        // below — a dedicated combo icon may come later) labeled "Line &
        // Optical" (`output_labels`) rather than dropped to `&[]` entirely:
        // this device may grow *other*, genuinely independent outputs some
        // day (e.g. a DLNA/cast output), and the menu needs a slot for
        // "the physical pair" to coexist alongside those, not just a
        // binary "has outputs or doesn't" toggle.
        outputs:         &["line-out"],
        line_out_is_speaker: false, line_out_is_jack: true,
        output_labels: &[("line-out", "Line & Optical")],
        input_labels: &[], line_in_is_jack: true,
    },
];

static AUDIO_PRO_PROFILES: [DeviceProfile; 6] = [
    /* 200 AudioProLink2 */ DeviceProfile {
        model_name:      Some("Audio Pro Link 2"),
        ignore_plm_bits: &[],
        extra_inputs:    &["bluetooth", "optical", "coaxial", "line-in"],
        outputs:         &["optical-out", "coax-out", "line-out"],
        line_out_is_speaker: false, line_out_is_jack: false,
        output_labels: &[],
        input_labels: &[], line_in_is_jack: false,
    },
    /* 201 AudioProA28 */ DeviceProfile {
        model_name:      Some("Audio Pro A28"),
        ignore_plm_bits: &[],
        extra_inputs:    &["bluetooth", "optical", "line-in", "HDMI"],
        outputs:         &[],
        line_out_is_speaker: false, line_out_is_jack: false,
        output_labels: &[],
        input_labels: &[], line_in_is_jack: false,
    },
    /* 202 AudioProAddonC5 */ DeviceProfile {
        model_name:      Some("Audio Pro Addon C5"),
        // `plm_support` (confirmed live, 2026-07-13: `0x26`) decodes to
        // bluetooth+udisk+coaxial via `PLM_BIT_TO_INPUT` — bluetooth is
        // real, udisk/coaxial are not (this device has no USB or coaxial
        // input at all), and it's also missing both real physical inputs
        // entirely. Not a partial "ignore just the wrong bits" fix — the
        // whole bitmap is unreliable on this device, so every bit it could
        // ever assert is ignored outright and the real list comes purely
        // from `extra_inputs` below, same as the WiiM Mini precedent
        // (`WIIM_PROFILES[0]`'s doc comment) but total rather than partial.
        ignore_plm_bits: &[0, 1, 2, 3, 5, 7],
        // Confirmed live, 2026-07-13: front 3.5mm AUX jack ("line-in") and
        // a second, back-panel RCA input ("RCA" — see `input_display_name()`
        // for why this one id is uppercase). No optical/HDMI/phono/USB/
        // coaxial on this unit.
        extra_inputs:    &["bluetooth", "line-in", "RCA"],
        // Built-in amp/speakers only — confirmed live, 2026-07-13, single
        // "Speaker" output, no line/optical/coax/headphone/USB out.
        outputs:         &["speaker-out"],
        line_out_is_speaker: true,
        line_out_is_jack: false,
        output_labels: &[],
        // Device's own silkscreen/manual says "AUX In", not the generic
        // "Line-In" — confirmed live, 2026-07-13.
        input_labels:    &[("line-in", "AUX In")],
        // Front jack is a single 3.5mm connector, not paired RCA — see
        // `icon_canon_for_input()`.
        line_in_is_jack: true,
    },
    /* 203 AudioProMkII (generic — model unknown) */ DeviceProfile {
        model_name:      None,
        ignore_plm_bits: &[],
        extra_inputs:    &["bluetooth", "line-in", "optical"],
        outputs:         &[],
        output_labels: &[],
        line_out_is_speaker: false,
        line_out_is_jack: false,
        input_labels: &[], line_in_is_jack: false,
    },
    /* 204 AudioProWGen (generic — model unknown) */ DeviceProfile {
        model_name:      None,
        ignore_plm_bits: &[],
        extra_inputs:    &["bluetooth", "line-in", "optical"],
        outputs:         &[],
        output_labels: &[],
        line_out_is_speaker: false, line_out_is_jack: false,
        input_labels: &[], line_in_is_jack: false,
    },
    /* 205 AudioProOriginal (generic — model unknown) */ DeviceProfile {
        model_name:      None,
        ignore_plm_bits: &[],
        extra_inputs:    &["bluetooth", "line-in", "optical"],
        outputs:         &[],
        output_labels: &[],
        line_out_is_speaker: false, line_out_is_jack: false,
        input_labels: &[], line_in_is_jack: false,
    },
];

static IEAST_PROFILES: [DeviceProfile; 2] = [
    /* 300 IEastAudioCast */ DeviceProfile {
        model_name:      Some("AudioCast"),
        ignore_plm_bits: &[],
        // Confirmed via a real capture (`captures/test-devices/
        // AudioCast_iEAST-02_20260708_095957.json`, project "iEAST-02"): a
        // network-only audio adapter with no physical inputs and a single
        // line-out, `plm_support` genuinely `0x0`.
        extra_inputs:    &[],
        outputs:         &["line-out"],
        output_labels: &[],
        line_out_is_speaker: false, line_out_is_jack: true,
        input_labels: &[], line_in_is_jack: false,
    },
    /* 301 IEastAudioCastPro */ DeviceProfile {
        model_name:      Some("AudioCast Pro"),
        // `plm_support` (confirmed live, 2026-07-20: `0x4`) decodes to
        // udisk via PLM_BIT_TO_INPUT — confirmed wrong (no USB input on
        // this device at all), so that bit is suppressed. Bluetooth *is*
        // real despite plm_support not asserting it — confirmed both by
        // every Bluetooth-status command answering with real data
        // (getbtstatus/getbtpairstatus/getbthistory/getbtPairDevStat/
        // getbtdiscoveryresult) and directly.
        ignore_plm_bits: &[2],
        extra_inputs:    &["bluetooth"],
        // A single entry, not `&["line-out", "optical-out"]` (its previous
        // value) — same hardware reality as the Arylic S10+ (see
        // `ARYLIC_PROFILES[3]`'s doc comment): both physical outputs
        // (optical and a 3.5mm jack) always run simultaneously, confirmed
        // live 2026-07-20. Represented the same way: one "line-out" entry
        // (jack icon via `line_out_is_jack`), labeled "Stereo & Optical" —
        // leaves room for a genuinely independent future output (e.g. a
        // DLNA/cast or BT one) to sit alongside this combined pair, rather
	// than an all-or-nothing `&[]`. The device's own "Stereo Out"/
        // "Optical Out" naming (this profile's previous, never-actually-
        // matching `output_labels` entry — its first element didn't match
        // either canon id, so it silently never applied) is dropped in
        // favor of the same unified label the Arylic S10+ now uses, since
        // both outputs are one combined thing here, not two named ones.
        outputs:         &["line-out"],
        output_labels: &[("line-out", "Stereo & Optical")],
        line_out_is_speaker: false, line_out_is_jack: true,
        input_labels: &[], line_in_is_jack: false,
    },
];

static LINKPLAY_PROFILES: [DeviceProfile; 1] = [
    /* 9999 LinkPlayGeneric */ DeviceProfile {
        model_name:      None,
        ignore_plm_bits: &[],
        // No forced inputs/outputs here, unlike every other (identified)
        // profile in this file — this is the fallback for a device this
        // code couldn't identify at all, so unlike e.g. WiiM Pro (where
        // "this model has bluetooth/line-in/optical" is a confirmed real
        // fact), there's no actual basis to assert *any* specific
        // input/output exists. Previously asserted the same
        // "bluetooth, line-in, optical" / "optical-out, line-out" guess
        // every other WiiM/Arylic profile in this file uses — confirmed
        // wrong via a real capture of an unidentified LinkPlay device (see
        // `IEAST_PROFILES[0]`'s doc comment — that specific device now has
        // its own identified profile, but the same "no basis to assert
        // hardware that isn't confirmed" reasoning applies to whatever
        // still-unidentified device hits this fallback next): the forced
        // inputs showed inputs that don't exist, and the forced outputs
        // showed an output the device doesn't have. The plm_support bitmap
        // decode alone is all this profile trusts now — a specific,
        // *identified* model is the right place to assert confirmed extra
        // hardware, not the catch-all for unidentified ones.
        extra_inputs:    &[],
        outputs:         &["line-out"],
        output_labels: &[],
        line_out_is_speaker: false, line_out_is_jack: false,
        input_labels: &[], line_in_is_jack: false,
    },
];

// ── Capabilities ──────────────────────────────────────────────────────────────

/// Which source device presets actually come from — learned at runtime,
/// not a static per-vendor guess (unlike most other fields
/// `static_playback_caps()` computes), and persisted on `DeviceCapabilities`
/// for the connection's lifetime once determined, so a confirmed-
/// unsupported HTTP `getPresetInfo` doesn't get retried every single
/// slow-poll cycle forever. Every device starts at `Unknown` regardless of
/// vendor: trying costs one extra round trip on first connect, then
/// settles permanently — cheaper and more reliable than maintaining a
/// static per-family guess for something this binary and directly
/// discoverable at runtime (see `state.rs`'s `fetch_presets_with_fallback()`,
/// the one place that actually decides what to try based on this value and
/// reports the outcome back via `record_preset_source()`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PresetSource {
    /// Not yet determined for this connection — try HTTP `getPresetInfo` first.
    Unknown,
    /// HTTP `getPresetInfo` works; keep using it.
    Http,
    /// HTTP confirmed unsupported; UPnP `GetKeyMapping` works instead.
    Upnp,
    /// Neither HTTP nor UPnP worked (or no `UpnpClient` ever became
    /// available to try) — this device has no reachable preset source.
    /// `state.rs` stops dispatching the slow-poll Presets phase entirely
    /// once this is reached.
    Unavailable,
}

#[derive(Debug, Clone)]
pub struct DeviceCapabilities {
    pub device_id:          DeviceId,
    pub vendor:             Vendor,
    /// Marketing-friendly model name (e.g. "WiiM Pro Plus").
    pub model:              String,
    /// Family profile for protocol/endpoint/grouping behaviour.
    pub family:             &'static FamilyProfile,
    /// Effective WiFi Direct flag.  Normally `family.grouping.uses_wifi_direct`,
    /// but overridden to `true` for Gen1 devices detected via `wmrm_version`.
    pub uses_wifi_direct:   bool,
    /// Private — see `PresetSource`'s doc comment and `preset_source()`/
    /// `record_preset_source()`. The consecutive-network-failure retry
    /// counter behind this determination is *not* stored here — it's a
    /// short-lived, per-tick concern of `state.rs`'s
    /// `fetch_presets_with_fallback()`, not part of the device's identity,
    /// so it lives on `Inner` instead (`Inner::preset_probe_failures`).
    /// `DeviceCapabilities` only ever records the final, resolved source.
    preset_source:          PresetSource,
    pub supports_eq:        bool,
    /// Parametric EQ.  Cannot be determined statically; starts `false` and
    /// must be updated after a successful runtime probe.
    pub supports_peq:       bool,
    /// Resolved output list — from a live `getSoundCardModeSupportList`
    /// probe if the device supports it, else the static per-model fallback
    /// (`get_default_outputs()`). Empty/harmless until `detect_capabilities()`
    /// populates it (via `detect_outputs()`); that's the only thing that
    /// should set this for real.
    pub outputs:            Vec<OutputEntry>,
    /// Whether `getSoundCardModeSupportList` actually worked on this
    /// device. `state.rs` only reads this to decide whether to keep
    /// polling that endpoint on the slow-poll cycle — it doesn't need to
    /// know *why* the answer is what it is (static guess vs. live probe).
    /// Set directly by `state.rs` (a plain field, not behind a setter) once
    /// it decides to give up — the failure counter/threshold that decision
    /// is based on lives in `state.rs`'s `Inner::outputs_probe_failures`,
    /// not here: this struct is meant to hold a device's resolved
    /// capabilities (static per-family data, or the result of a one-shot
    /// connect-time probe), not ongoing per-tick retry bookkeeping.
    pub probes_outputs:     bool,
    /// Whether `getNewAudioOutputHardwareMode` actually works on this
    /// device — same shape as `probes_outputs` above, but for the separate
    /// "what's currently active" query rather than "what outputs exist."
    /// Confirmed unsupported on iEAST AudioCast (only has one output, so
    /// the query is meaningless there) — without this, a device where it
    /// always fails got asked again every slow-poll cycle forever, and
    /// each failure additionally fired a spurious `output-changed` signal
    /// (see `get_audio_output()`'s doc comment for that half of the bug).
    /// Set directly by `state.rs`, same reasoning as `probes_outputs`.
    pub probes_output_status: bool,
    /// Whether `getbtstatus` actually works on this device — same shape as
    /// `probes_outputs` above (a single confirmed `"unknown command"` is
    /// enough to flip this, no threshold, since `ApiOutcome::Unsupported`
    /// is already a definite answer — see `get_bt_status()`'s doc
    /// comment). Confirmed unsupported on the Audio Pro Addon C5 (real
    /// device, 2026-07-13: `curl .../getbtstatus` → literal `"unknown
    /// command"`) — without this, a device where it always fails got
    /// asked again every tick Bluetooth was the active input, and
    /// (`has_playable_content()`'s Bluetooth branch requiring a confirmed
    /// `connected: true`) permanently blanked playback content and
    /// disabled transport controls on hardware that simply never answers
    /// this call, regardless of whether a phone was actually connected.
    /// Set directly by `state.rs`, same reasoning as `probes_outputs`.
    pub probes_bt: bool,
    /// Whether `getMetaInfo` actually works on this device — same shape as
    /// `probes_bt` above, replacing what used to be a static per-family
    /// `EndpointConfig::supports_get_meta_info` guess: starts `true`
    /// (always tried at least once per connection), flipped to `false` by
    /// `state.rs`'s `resolve_meta_info()` the first time a call returns
    /// `ApiOutcome::Unsupported`, so the fast-poll loop stops spending a
    /// request on a call known to fail from the next tick on and
    /// synthesizes from `getPlayerStatusEx` instead
    /// (`MetaData::from_player_status()`).
    pub probes_meta_info: bool,
    /// Resolved audio input list. Seeded from `get_default_inputs()`'s static
    /// plm_support-based list (`enabled` defaulting `true`), but
    /// `detect_capabilities()` (via `detect_inputs()`) prefers the device's
    /// own authoritative `getAudioInputCapbility` list when that WiiM-app call
    /// is supported, replacing the plm guess entirely; either way it's then
    /// amended by a one-time `getAudioInputEnable` probe for the per-input
    /// `enabled` flags if that call succeeds and parses. `state.rs`
    /// additionally self-corrects this live: an entry marked `enabled:
    /// false` here gets forced back to `true` (with a warning) if the
    /// device's currently-polled playback mode maps to that same input —
    /// a capability snapshot can't be right calling something "disabled"
    /// while it's demonstrably in active use.
    pub inputs:             Vec<InputEntry>,
    /// Set when this device's *specific* family+firmware combination is
    /// known to have a real functionality gap (not a general "outdated
    /// firmware" nag) — a message meant to be shown to the user verbatim
    /// (e.g. a Settings/About banner), `None` otherwise. See
    /// `from_device_info()`'s computation for the one case that sets this
    /// so far (Audio Pro Addon C5, fw < 4.0).
    pub firmware_warning:   Option<&'static str>,
}

/// One resolved audio input: a canonical ID (from the device's authoritative
/// `getAudioInputCapbility` list when supported, else `get_default_inputs()`'s
/// fixed plm_support-derived set, plus anything `getAudioInputEnable` reports
/// that neither produced — appended rather than dropped) plus whether it's
/// currently enabled.
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

        // Nothing sets this today — mechanism (this field, the
        // warning-styled row in `ui/settings.rs`'s About panel) kept for a
        // real future case.
        let firmware_warning: Option<&'static str> = None;

        // Gen1 devices (wmrm_version "2.0" or very old firmware) use WiFi Direct.
        let gen1 = is_gen1(&info.wmrm_version, &info.firmware);
        let uses_wifi_direct = family.grouping.uses_wifi_direct || gen1;
        if gen1 && !family.grouping.uses_wifi_direct {
            dbg("wifi_direct: true (Gen1 override via wmrm_version/firmware)");
        }

        let model = device_id.profile().model_name
            .map(|s| s.to_string())
            .unwrap_or_else(|| model_name_fallback(&project_lc, &info.device_name));
        let (supports_eq, supports_peq) = static_playback_caps(device_id);

        // Base input list — static, from plm_support + per-model profile.
        // Just the starting point: `detect_capabilities()` prefers the
        // device's authoritative `getAudioInputCapbility` list when that call
        // is supported (replacing this guess entirely), and either way amends
        // `enabled` (and appends missed entries) via `getAudioInputEnable`.
        let inputs = get_default_inputs(device_id, info.plm_support_value())
            .into_iter()
            .map(|id| InputEntry { id: id.to_string(), enabled: true })
            .collect();

        let caps = Self {
            device_id, vendor, model, family,
            uses_wifi_direct,
            preset_source: PresetSource::Unknown, supports_eq, supports_peq,
            inputs, firmware_warning,
            // Harmless placeholders — only `detect_capabilities()` (the real
            // probing entry point) sets these to something meaningful.
            // Standalone callers of `from_device_info()` (e.g. `wiim-capture`,
            // which only wants the model name) never see these matter.
            outputs: Vec::new(), probes_outputs: true, probes_output_status: true, probes_bt: true,
            probes_meta_info: true,
        };

        if DEBUG_DEVICE.load(Ordering::Relaxed) {
            let pa = &caps.family.playback_access;
            let c  = &caps.family.connection;
            let ep = &caps.family.endpoints;

            dbg(&format!("model: {:?}  wifi_direct: {}",
                caps.model, caps.uses_wifi_direct));
            dbg(&format!("capabilities: preset_source={:?}  eq={}  peq={}",
                caps.preset_source, caps.supports_eq, caps.supports_peq));
            if let Some(w) = caps.firmware_warning {
                dbg(&format!("firmware_warning: {w:?}"));
            }
            dbg(&format!("playback_access: {pa:?}"));
            dbg(&format!(
                "connection: ports={:?}  https_first={}  timeout={}ms  retries={}  client_cert={}",
                c.preferred_ports, c.https_first, c.response_timeout_ms,
                c.retry_count, c.requires_client_cert,
            ));
            dbg(&format!(
                "endpoints: eq={}  eq_set={}  alarms={}  sleep_timer={}",
                ep.supports_eq, ep.supports_eq_set,
                ep.supports_alarms, ep.supports_sleep_timer,
            ));
            dbg(&format!("  reboot_command:  {:?}", ep.reboot_command));
        }

        caps
    }

    /// This device family's default `AccessMethod` — the starting point
    /// `DeviceState` applies any per-device `playback_access_override` on
    /// top of. Currently just the static per-family default
    /// (`self.family.playback_access`); once `detect_capabilities()` probes
    /// more than outputs, findings from that probing should feed into this
    /// too.
    pub fn playback_access(&self) -> AccessMethod {
        self.family.playback_access
    }

    /// Store a fresh `getSoundCardModeSupportList` result — called by
    /// `state.rs`'s `handle_slow_poll_outputs()` only on a confirmed
    /// success; the retry/give-up policy (failure counter, threshold,
    /// deciding when to flip `probes_outputs` to `false`) lives entirely in
    /// `state.rs`'s `Inner`, not here — see `probes_outputs`'s doc comment
    /// for why. Returns `true` if `self.outputs` actually changed (the
    /// caller emits `outputs-changed` when this is `true`).
    pub fn record_outputs(&mut self, mut list: Vec<OutputEntry>) -> bool {
        // The raw API call (`WiimClient::get_sound_card_mode_support_list()`)
        // has no notion of per-device profiles, so it always returns
        // `icon_canon == canon` — the `line_out_is_speaker` and
        // `line_out_is_jack` quirks must be (re)applied here on every probe,
        // not just the initial one in `detect_capabilities()`, or a corrected
        // icon from connect time gets clobbered back to the wrong one on the
        // very next slow poll.
        for e in &mut list {
            e.icon_canon = icon_canon_for_output(e.canon, self.device_id);
        }
        if list != self.outputs {
            self.outputs = list;
            true
        } else {
            false
        }
    }

    /// Current resolved preset-fetch source, or `Unknown` if not yet
    /// determined for this connection — see `PresetSource`'s doc comment.
    pub fn preset_source(&self) -> PresetSource {
        self.preset_source
    }

    /// Persist a newly-resolved preset source (e.g. after `state.rs` learns
    /// whether HTTP `getPresetInfo` or UPnP `GetKeyMapping` actually works
    /// for this device, or gives up on one after exhausting its own retry
    /// budget). Stays put for the life of the connection — see
    /// `PresetSource`'s doc comment for why a *confirmed* result must never
    /// be re-probed.
    pub fn record_preset_source(&mut self, source: PresetSource) {
        self.preset_source = source;
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

    detect_outputs(client, &mut caps).await;
    detect_inputs(client, &mut caps).await;

    Some((info, caps))
}

/// Resolve the real output list for a live connection, overwriting the
/// static default `from_device_info()` seeded into `caps`. Probes
/// `getSoundCardModeSupportList`; on success that list is authoritative, and
/// on failure/unsupported it falls back to the static per-model profile
/// (`get_default_outputs()`). A single attempt either way (no retry budget at
/// connect time — that's the ongoing slow poll's job, see `state.rs`'s
/// `outputs_probe_failures`), so `Unsupported` and `Failed` are treated
/// identically. Also records whether the live probe worked in
/// `caps.probes_outputs`.
async fn detect_outputs(client: &WiimClient, caps: &mut DeviceCapabilities) {
    match client.get_sound_card_mode_support_list().await {
        ApiOutcome::Ok(mut list) => {
            dbg(&format!("outputs from API: {:?}", list));
            for e in &mut list {
                e.icon_canon = icon_canon_for_output(e.canon, caps.device_id);
            }
            caps.outputs = list;
            caps.probes_outputs = true;
        }
        ApiOutcome::Unsupported | ApiOutcome::Failed => {
            dbg("getSoundCardModeSupportList not supported; using static profile");
            caps.outputs = get_default_outputs(caps.device_id)
                .iter()
                .map(|&canon| {
                    let icon_canon = icon_canon_for_output(canon, caps.device_id);
                    let name = output_display_name(canon, caps.device_id).to_string();
                    OutputEntry { canon, icon_canon, name }
                })
                .collect();
            caps.probes_outputs = false;
        }
    }
}

/// Resolve the real input list for a live connection, refining the static
/// default `from_device_info()` seeded into `caps`. Two live calls:
///
/// - `getAudioInputCapbility` (WiiM app command, WiiM-only in practice —
///   Audio Pro/iEAST/older devices reply "unknown command", which parses as
///   `None`) is *authoritative* for which physical inputs exist, so its list
///   replaces the `plm_support`-derived guess wholesale rather than merely
///   amending it — no plm bit decoding is trusted at all once we have this.
///   The reported `mode` strings are already canonical wire IDs (`"wifi"`,
///   `"line-in"`, `"HDMI"`, `"udisk"`, …), the same values `switch_input()`
///   sends, so they become `InputEntry.id`s verbatim with no translation.
///   When unsupported, the plm-derived list stands unchanged.
/// - `getAudioInputEnable` then corrects the per-input `enabled` flags. Never
///   authoritative for *existence* — only for `enabled`, and only for entries
///   it actually mentions (missing entries keep their default rather than
///   being dropped).
///
/// `udisk` (USB) is a deliberate exception to the enable pass: it's a
/// local-media *streaming* mode rather than a switchable physical input, so
/// `getAudioInputCapbility` always lists it (confirmed live on a WiiM Ultra:
/// present whether or not a stick is actually inserted) while
/// `getAudioInputEnable` never mentions it at all. It's force-kept enabled so
/// it can't be greyed out in the source menu.
async fn detect_inputs(client: &WiimClient, caps: &mut DeviceCapabilities) {
    match client.get_audio_input_capability().await {
        Some(ids) if !ids.is_empty() => {
            dbg(&format!("audio input capability (authoritative): {:?}", ids));
            caps.inputs = ids
                .into_iter()
                .map(|id| InputEntry { id, enabled: true })
                .collect();
        }
        _ => {
            dbg("getAudioInputCapbility not supported; keeping plm_support-derived input list");
        }
    }

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
                        "{} [device] getAudioInputEnable reported {:?}, which isn't in the \
                         detected input list for this device — adding it",
                        super::timestamp(), e.name,
                    );
                    caps.inputs.push(InputEntry { id: e.name.clone(), enabled: e.is_enabled() });
                }
            }
        }
        None => {
            dbg("getAudioInputEnable not supported or didn't parse; all inputs default enabled");
        }
    }

    // `udisk` is a streaming mode, not a switchable input — never enable-gated.
    // See this function's doc comment.
    if let Some(usb) = caps.inputs.iter_mut().find(|i| i.id.eq_ignore_ascii_case("udisk")) {
        usb.enabled = true;
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
        || project.contains("audiopro") // see DeviceId::detect()'s doc comment
        || project.contains("addon")
        || matches!(project, "a10" | "a15" | "a28" | "c10")
        || name_lc.contains("audio pro")
        || name_lc.contains("audiopro") // same no-separator case, in the device's own name
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
        // `detect_vendor_extended()` never actually returns `IEast` (iEAST
        // AudioCast is matched by exact project string in `DeviceId::detect()`
        // before falling through to this name/firmware-based fallback path
        // at all) — this arm only exists for match exhaustiveness.
        Vendor::IEast           => &FAMILY_IEAST_AUDIOCAST,
        Vendor::LinkPlayGeneric => &FAMILY_LINKPLAY_GENERIC,
    }
}

/// Select the Audio Pro family profile from firmware-based generation
/// detection. Only ever reached via `detect_vendor_extended()`'s own
/// loose/name-or-firmware-based fallback (never a "known model" project
/// string — `DeviceId::detect()` would already have matched that directly,
/// without ever falling through to `LinkPlayGeneric` and this path at
/// all), so always `known_model: false` — see `detect_audio_pro_gen()`'s
/// doc comment for why that default matters.
fn detect_audio_pro_family(project: &str, fw: &str) -> &'static FamilyProfile {
    match DeviceId::detect_audio_pro_gen(project, fw, false) {
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

/// Static capability defaults for (supports_eq, supports_peq).
/// Matches pywiim's detect_device_capabilities() per-vendor branches.
/// PEQ is always `false` here — it requires a runtime probe. Presets have no
/// static per-vendor guess any more — every device starts at
/// `PresetSource::Unknown` and self-determines HTTP vs. UPnP vs. unavailable
/// at runtime (see `PresetSource`'s doc comment).
fn static_playback_caps(device_id: DeviceId) -> (bool, bool) {
    match device_id.vendor() {
        Vendor::WiiM => (true, false),

        Vendor::AudioPro => match device_id {
            DeviceId::AudioProMkII => (false, false),
            DeviceId::AudioProWGen => (true,  false),
            _                      => (false, false),
        },

        Vendor::Arylic => (false, false),
        Vendor::IEast => (false, false),
        Vendor::LinkPlayGeneric => (false, false),
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

/// The *default* input list for a device — a best-effort guess from the
/// static per-model profile and the `plm_support` bitmap, used as the
/// starting point `DeviceCapabilities::from_device_info()` seeds `inputs`
/// (`InputEntry`) from. This does no live probing; it's superseded outright
/// by real detection (`getAudioInputCapbility`) in `detect_capabilities()`
/// whenever the device supports that call. Not `pub`: nothing outside this
/// module needs the raw guess on its own since `caps.inputs` is the one
/// place callers read resolved inputs from.
///
/// Algorithm:
/// 1. Decode `plm_support` bits using `PLM_BIT_TO_INPUT`.
/// 2. Remove inputs whose bit is in the device profile's `ignore_plm_bits`.
/// 3. Append any `extra_inputs` from the profile not already in the list.
/// 4. Prepend `"wifi"` (always available as a network streaming source).
fn get_default_inputs(device_id: DeviceId, plm_support: u64) -> Vec<&'static str> {
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

/// The *default* output list for a device — the canonical output names from
/// the static per-model profile, used as the fallback when live detection
/// (`getSoundCardModeSupportList`) isn't available. Does no probing itself;
/// superseded by the real list in `detect_capabilities()` whenever that call
/// succeeds.
/// XXX bluetooth-out needs proper runtime detection; omitted for now.
fn get_default_outputs(device_id: DeviceId) -> &'static [&'static str] {
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
pub fn output_display_name(id: &str, device_id: DeviceId) -> &str {
    if let Some((_, label)) = device_id.profile().output_labels.iter().find(|(i, _)| *i == id) {
        return label;
    }
    match id {
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

/// Human-readable label for an input source ID, optionally overridden by
/// the device's own profile (`DeviceProfile::input_labels` — e.g. a device
/// whose physical silkscreen says "AUX In" rather than the generic
/// "Line-In"). `device_id` is `None` when no device context is available
/// yet (e.g. before the first `getStatusEx` answers) — falls straight to
/// the generic table in that case, same as before this override existed.
pub fn input_display_name(device_id: Option<DeviceId>, id: &str) -> &str {
    if let Some(device_id) = device_id {
        if let Some((_, label)) = device_id.profile().input_labels.iter().find(|(i, _)| *i == id) {
            return label;
        }
    }
    match id {
        "wifi"      => "Network",
        "bluetooth" => "Bluetooth",
        "line-in"   => "Line-In",
        "line-in-2" => "Line-In 2",
        // Uppercase, unlike every sibling id here — matches this exact
        // wire string verbatim (both `PlayMedium` and the working
        // `setPlayerCmd:switchmode:RCA` argument, confirmed live,
        // 2026-07-13, Audio Pro Addon C5's back-panel RCA input) since
        // `switch_input()` sends `InputEntry.id` straight through with no
        // translation layer — unlike outputs, inputs have no separate
        // canon-to-wire mapping, so the id *is* the wire value.
        "RCA"       => "RCA",
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
/// Every canonical input ID here is lowercase *except* `"HDMI"` and `"RCA"`
/// — not a stylistic inconsistency, it matches the actual device wire
/// format exactly. `"HDMI"` confirmed both ways: `getAudioInputEnable`
/// reports it capitalized (real captures), and the authoritative `wiim`
/// SDK's own `InputMode` enum (`consts.py`) has `HDMI`'s wire command name
/// capitalized while every other entry's is lowercase — this is genuinely
/// how the device spells it, on both the read and write
/// (`setPlayerCmd:switchmode:{id}`) sides. Sending lowercase `"hdmi"` back
/// is silently rejected by real hardware. `"RCA"` (mode 44) confirmed the
/// same way, live, 2026-07-13, on an Audio Pro Addon C5 — a genuinely
/// distinct second line-level input (back-panel RCA jacks) from `40|60`'s
/// front 3.5mm AUX jack, not a firmware-numbering quirk for the *same*
/// jack (this bucket previously grouped `44` in with `40|60` as one
/// generic "line-in", which was already inconsistent with
/// `decode_source_name_http`'s own separate `44 => "RCA"` display label in
/// `playback.rs` — this device is what finally exposed the mismatch,
/// since selecting "RCA" from the source dropdown, once the device
/// actually reported mode 44, resolved back to `"line-in"` here and
/// silently reselected the AUX entry instead).
pub fn mode_to_input_source(mode: i32) -> &'static str {
    match mode {
        40 | 60      => "line-in",
        44           => "RCA",
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
    if canon == "line-out" {
        if device_id.profile().line_out_is_speaker {
            "speaker-out"
        } else if device_id.profile().line_out_is_jack {
            "jack-out"
        }
        else {
            canon
        }
    } else {
        canon
    }
}

/// Same idea as `icon_canon_for_output()`, for inputs: adjusts the icon
/// *lookup key* only, never the canonical id/switchmode value itself
/// (`"line-in"` either way — see `DeviceProfile::line_in_is_jack`'s doc
/// comment). `source_id` here is whatever `mode_to_input_source()`/
/// `InputEntry.id` already produced, not necessarily `'static` — this
/// still returns a borrow with that same lifetime (a `'static` literal
/// coerces fine when the override fires).
pub fn icon_canon_for_input<'a>(source_id: &'a str, device_id: DeviceId) -> &'a str {
    if source_id == "line-in" && device_id.profile().line_in_is_jack {
        "line-in-jack"
    } else {
        source_id
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capture::format::CaptureFile;

    fn load_capture(filename: &str) -> CaptureFile {
        let path = format!("{}/captures/test-devices/{filename}", env!("CARGO_MANIFEST_DIR"));
        let text = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("reading fixture {path}: {e}"));
        serde_json::from_str(&text)
            .unwrap_or_else(|e| panic!("parsing fixture {path}: {e}"))
    }

    /// Regression test for a real bug: mode 44 ("RCA") used to be bucketed
    /// together with 40/60 as plain "line-in", so selecting the "RCA"
    /// dropdown entry on a device that actually has both (Audio Pro Addon
    /// C5) would report back mode 44, resolve to `"line-in"` here, and
    /// silently reselect the AUX entry instead — reported live, 2026-07-13.
    #[test]
    fn mode_to_input_source_rca_is_distinct_from_line_in() {
        assert_eq!(mode_to_input_source(40), "line-in");
        assert_eq!(mode_to_input_source(60), "line-in");
        assert_eq!(mode_to_input_source(44), "RCA");
        assert_ne!(mode_to_input_source(44), mode_to_input_source(40));
    }

    /// Real WiiM Mini unit (project "Muzo_Mini", hardware "ALLWINNER-R328")
    /// confirmed to have only WiFi/Bluetooth/Line-In — no Optical, no USB
    /// (its USB-C port is power-only) — despite `plm_support` (0x300006)
    /// asserting bit 2 (USB) and the static profile previously force-adding
    /// "optical" regardless of what the device actually reports. See the
    /// `WIIM_PROFILES[0]` doc comment for the full investigation.
    #[test]
    fn wiim_mini_real_capture_has_no_optical_or_usb_input() {
        let cap = load_capture("WiiM_Mini_20260708_045125.json");
        let body = cap.commands.iter()
            .find(|c| c.command == "getStatusEx")
            .expect("capture has no getStatusEx")
            .body.clone()
            .expect("getStatusEx has no body");
        let info: DeviceInfo = serde_json::from_value(body).expect("parsing DeviceInfo");
        let caps = DeviceCapabilities::from_device_info(&info);
        assert_eq!(caps.model, "WiiM Mini");
        assert!(caps.inputs.iter().any(|i| i.id == "wifi"));
        assert!(caps.inputs.iter().any(|i| i.id == "bluetooth"));
        assert!(caps.inputs.iter().any(|i| i.id == "line-in"));
        assert!(!caps.inputs.iter().any(|i| i.id == "optical"), "real unit has no optical input");
        assert!(!caps.inputs.iter().any(|i| i.id == "udisk"), "real unit's USB-C is power-only");
    }

    /// Real Audio Pro Addon C5 unit (project "AudioPro_C5I", firmware
    /// 3.7.4830 — physically confirmed, "AddonC5" printed on the device
    /// itself). Resolves to `FAMILY_AUDIO_PRO_ORIGINAL` via
    /// `detect_audio_pro_family()`'s firmware-based generation detection —
    /// see that static's doc comment for the known-inaccurate fields.
    #[test]
    fn audio_pro_addon_c5_old_firmware_real_capture() {
        let cap = load_capture("Audio_Pro_Addon_C5_20260710_073433.FW3.7.x.json");
        let body = cap.commands.iter()
            .find(|c| c.command == "getStatusEx" && c.outcome == crate::capture::format::Outcome::Ok)
            .expect("capture has no successful getStatusEx")
            .body.clone()
            .expect("getStatusEx has no body");
        let info: DeviceInfo = serde_json::from_value(body).expect("parsing DeviceInfo");
        let caps = DeviceCapabilities::from_device_info(&info);
        assert_eq!(caps.device_id, DeviceId::AudioProAddonC5);
        assert_eq!(caps.vendor, Vendor::AudioPro);
        assert_eq!(caps.model, "Audio Pro Addon C5");
        assert_eq!(caps.family.display_name, "Audio Pro Original");
        assert_eq!(caps.family.playback_access, AccessMethod::UpnpPolled);
        assert!(!caps.family.connection.requires_client_cert);
        assert!(caps.family.connection.preferred_ports.contains(&80));
        assert!(caps.firmware_warning.is_none());
        // Real `plm_support` (0x26) decodes to bluetooth+udisk+coaxial via
        // the generic bitmap table — udisk/coaxial are spurious on this
        // device (confirmed live: no USB or coaxial input at all), and the
        // bitmap also misses both real inputs entirely (line-in, RCA).
        // `ignore_plm_bits` should suppress the whole bitmap, leaving
        // exactly `extra_inputs` (plus the always-present "wifi").
        let ids: Vec<&str> = caps.inputs.iter().map(|e| e.id.as_str()).collect();
        assert_eq!(ids, vec!["wifi", "bluetooth", "line-in", "RCA"]);
    }

    /// Real iEAST AudioCast unit (project "iEAST-02", a bare network-audio
    /// adapter) confirmed to have no physical inputs at all and a single
    /// line-out output. Was previously misidentified as the `LinkPlayGeneric`
    /// fallback, which force-added three inputs and an extra output this
    /// device doesn't have — it now gets its own identified profile
    /// (`IEAST_PROFILES[0]`) instead. This capture also has a real preset
    /// configured device-side (confirmed via its `PlayQueue`/`GetKeyMapping`
    /// data — not read by this test, which only exercises the HTTP-side
    /// capability detection), yet `getPresetInfo` still replies "unknown
    /// command" — a genuine firmware limitation (confirmed unsupported by
    /// `wiim-capture`'s own detection), not an artifact of no preset
    /// existing — but that determination is now made at runtime (see
    /// `PresetSource`'s doc comment), not guessed statically here, so
    /// `from_device_info()` alone reports `PresetSource::Unknown` regardless.
    #[test]
    fn ieast_audiocast_real_capture_has_no_forced_inputs_or_extra_outputs() {
        let cap = load_capture("AudioCast_iEAST-02_20260708_095957.json");
        let body = cap.commands.iter()
            .find(|c| c.command == "getStatusEx")
            .expect("capture has no getStatusEx")
            .body.clone()
            .expect("getStatusEx has no body");
        let info: DeviceInfo = serde_json::from_value(body).expect("parsing DeviceInfo");
        let caps = DeviceCapabilities::from_device_info(&info);
        assert_eq!(caps.device_id, DeviceId::IEastAudioCast);
        assert_eq!(caps.vendor, Vendor::IEast);
        assert_eq!(caps.model, "AudioCast");
        assert_eq!(caps.family.playback_access, AccessMethod::UpnpPolled);
        assert_eq!(caps.inputs.len(), 1, "expected only wifi: {:?}", caps.inputs);
        assert_eq!(caps.inputs[0].id, "wifi");
        assert_eq!(caps.device_id.profile().outputs, &["line-out"]);
        assert_eq!(caps.preset_source(), PresetSource::Unknown);

        let preset_cmd = cap.commands.iter()
            .find(|c| c.command == "getPresetInfo")
            .expect("capture has no getPresetInfo");
        assert!(preset_cmd.unsupported);
    }

    /// Real Arylic S10+ unit (project "S10P_WIFI") — see `ARYLIC_PROFILES[3]`'s
    /// own doc comment: the "line-in" entry in `extra_inputs` is confirmed
    /// directly against the hardware, not derived from this capture (every
    /// output/input-enumeration command in it was "unknown command").
    /// `outputs` is a single "line-out" entry, not the two independent
    /// ones its wire-level names might suggest — also confirmed directly:
    /// both physical outputs (a 3.5mm jack and optical) always run
    /// simultaneously with no selection concept between them, so they're
    /// represented as one combined output ("Line & Optical" via
    /// `output_labels`) rather than two or none.
    #[test]
    fn arylic_s10_plus_real_capture_detects_correctly() {
        let cap = load_capture("Arylic_S10P_WIFI_20260720_005332.json");
        let body = cap.commands.iter()
            .find(|c| c.command == "getStatusEx")
            .expect("capture has no getStatusEx")
            .body.clone()
            .expect("getStatusEx has no body");
        let info: DeviceInfo = serde_json::from_value(body).expect("parsing DeviceInfo");
        let caps = DeviceCapabilities::from_device_info(&info);
        assert_eq!(caps.device_id, DeviceId::ArylicS10Plus);
        assert_eq!(caps.vendor, Vendor::Arylic);
        assert_eq!(caps.model, "Arylic S10+");
        assert_eq!(caps.family.playback_access, AccessMethod::UpnpPolled);
        assert!(!caps.family.connection.requires_client_cert);
        // plm_support 0x8006 -> bluetooth (bit 1) + udisk (bit 2); bit 15
        // is unmapped/ignored. "line-in" comes from extra_inputs, confirmed
        // directly (a 3.5mm jack, not asserted by plm_support at all).
        let ids: Vec<&str> = caps.inputs.iter().map(|e| e.id.as_str()).collect();
        assert_eq!(ids, vec!["wifi", "bluetooth", "udisk", "line-in"]);
        assert_eq!(caps.device_id.profile().outputs, &["line-out"]);
        assert_eq!(output_display_name("line-out", caps.device_id), "Line & Optical");

        let meta_cmd = cap.commands.iter()
            .find(|c| c.command == "getMetaInfo")
            .expect("capture has no getMetaInfo");
        assert!(meta_cmd.unsupported);
    }

    /// Real iEAST "AudioCast Pro" unit (project "AudioCast_M30") — a
    /// different, newer device from the bare `IEastAudioCast` (iEAST-02)
    /// above with its own `DeviceId`/`DeviceProfile` (different inputs/
    /// outputs), but reusing that device's `FAMILY_IEAST_AUDIOCAST`
    /// (endpoint/connection behavior matched closely enough not to
    /// justify a second family — see that static's own doc comment,
    /// including why `requires_client_cert: true` there is harmless even
    /// though this specific capture didn't need one). See
    /// `IEAST_PROFILES[1]`'s own doc comment for why "bluetooth" is
    /// force-added despite `plm_support` not asserting it.
    #[test]
    fn ieast_audiocast_pro_real_capture_detects_correctly() {
        let cap = load_capture("AudioCast_M30_20260720_014924.json");
        let body = cap.commands.iter()
            .find(|c| c.command == "getStatusEx")
            .expect("capture has no getStatusEx")
            .body.clone()
            .expect("getStatusEx has no body");
        let info: DeviceInfo = serde_json::from_value(body).expect("parsing DeviceInfo");
        let caps = DeviceCapabilities::from_device_info(&info);
        assert_eq!(caps.device_id, DeviceId::IEastAudioCastPro);
        assert_eq!(caps.vendor, Vendor::IEast);
        assert_eq!(caps.model, "AudioCast Pro");
        assert_eq!(caps.family.playback_access, AccessMethod::UpnpPolled);
        assert!(caps.family.endpoints.supports_alarms);
        // plm_support 0x4 -> udisk (bit 2), but confirmed wrong (no USB
        // input on this device) and suppressed via ignore_plm_bits;
        // "bluetooth" comes from extra_inputs, confirmed live via the
        // getbt* commands below.
        let ids: Vec<&str> = caps.inputs.iter().map(|e| e.id.as_str()).collect();
        assert_eq!(ids, vec!["wifi", "bluetooth"]);
        // Same "one combined output, not two independent ones" shape as
        // the Arylic S10+ (`ARYLIC_PROFILES[3]`'s doc comment) — confirmed
        // to be the same underlying hardware under different branding.
        assert_eq!(caps.device_id.profile().outputs, &["line-out"]);
        assert_eq!(output_display_name("line-out", caps.device_id), "Stereo & Optical");

        let bt_cmd = cap.commands.iter()
            .find(|c| c.command == "getbtstatus")
            .expect("capture has no getbtstatus");
        assert!(!bt_cmd.unsupported);
    }
}
