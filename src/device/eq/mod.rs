//! EQ abstraction — canonical types plus `EqSession`, the dispatch layer
//! that picks the right transport/generation per mechanism and talks to
//! the device.
//!
//! Deliberately **not** shaped like `state.rs`'s `DeviceState`: no poller,
//! no `glib::timeout_add_local`, no GObject subclass, no signals. EQ is
//! read once when a host panel opens and edited from there — it does not
//! watch for or reconcile changes made by another controller (the WiiM
//! app, a second window) while open. Every type here must be `Send`
//! (`String`, never `Rc`) — decoding happens on the tokio thread and
//! results cross the async_channel bridge to the GTK thread, the
//! opposite direction from `PlaybackState`'s `Rc<str>` fields, which are
//! decoded on the GTK thread and never leave it.
//!
//! `ui/` only ever sees the types in this module (plus
//! `capabilities::EqProfile`/`EqMechanism`/etc.) — never a raw command
//! string, plugin URI, generation, or transport.

pub(crate) mod legacy;
pub(crate) mod lv2;

use std::sync::atomic::{AtomicBool, Ordering};

pub static DEBUG_EQ: AtomicBool = AtomicBool::new(false);

/// `pub` (not `pub(crate)`, unlike most per-module debug helpers) since
/// `ui/eq/panel.rs` — a different crate (the `main.rs` bin, not this
/// library) — logs its own higher-level flow (mechanism switches, resync)
/// under this same flag rather than a separate `ui`-side atomic, to keep
/// "what is the EQ code doing" all behind one `--debug=eq` token.
///
/// Takes the device identifier (`WiimClient::ip()`/`DeviceState::ip()`)
/// as its own parameter, not baked into `msg` — with more than one
/// device window open, a bare unattributed `[eq] ...` line gives no way
/// to tell which connection it came from (same reasoning as `state.rs`'s
/// own `dbg(ds, msg)`).
pub fn dbg(ip: &str, msg: &str) {
    if DEBUG_EQ.load(Ordering::Relaxed) {
        println!("{} [eq] {ip}: {msg}", super::timestamp());
    }
}

/// Whether `name` is safe to send as a preset name — ASCII letters,
/// digits, and underscores only, non-empty. Matches the official app's
/// own restriction, and sidesteps ever finding out the hard way whether
/// this device family's firmware parses `EQSourceSave`/`EQv2Rename`'s
/// `Name`/`newName` fields robustly against a stray quote/brace/angle
/// bracket — this app's own JSON encoding (`serde_json::json!`) already
/// escapes those correctly on the wire, but a constrained embedded
/// parser on the *receiving* end reportedly isn't as trustworthy, per the
/// same reasoning the official app's own input restriction implies.
pub fn is_valid_preset_name(name: &str) -> bool {
    !name.is_empty() && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// A parametric band's filter type. `GetAcousticCapability`'s
/// `PEQ.Filters` shows this vocabulary is device-reported and larger
/// than the four modes assumed by every other WiiM client checked
/// (`pywiim`, Wiim-Dashboard): `["OFF","LS","PK","HS","LP","HP"]` on a
/// real WiiM Ultra. Wire values confirmed live (2026-07-23, real Ultra —
/// one custom preset with a distinct filter per band, band→filter
/// assignment confirmed directly, not guessed): `Off = -1`, `LowShelf =
/// 0`, `Peak = 1`, `HighShelf = 2`, `LowPass = 3`, `HighPass = 5` — note
/// `4` is unused by any of the six `PEQ.Filters` entries (plausibly a
/// reserved/internal mode the app doesn't expose). Devices without
/// `GetAcousticCapability` (confirmed live on a WiiM Mini) only ever
/// offer the first four. `Other` is the escape hatch for a filter-type
/// token/value this codebase doesn't have a named variant for — the
/// editor widget only ever offers the modes actually present in
/// `capabilities::EqMechanism::filters`, never a hardcoded fixed set.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PeqBandMode {
    Off,       // -1
    LowShelf,  //  0
    Peak,      //  1
    HighShelf, //  2
    LowPass,   //  3
    HighPass,  //  5 — note the gap: 4 is unused
    Other(String),
}

/// One graphic-EQ band. `param_name` is the device-reported wire
/// parameter name (e.g. `"band31hz"`), kept through decode so the encode
/// path just echoes it back rather than reconstructing it from
/// `freq_label`.
#[derive(Debug, Clone, PartialEq)]
pub struct GraphicBand {
    pub param_name: String,
    /// Display form: "31", "63", ... "1k", "16k" — device-reported, not
    /// assumed to always be the same 10 values.
    pub freq_label: String,
    pub gain_db: f64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ParametricBand {
    pub mode:    PeqBandMode,
    pub freq_hz: f64,
    pub q:       f64,
    pub gain_db: f64,
}

/// Default center frequency for PEQ band `index` (0-based, a=0..j=9) —
/// used by the editor's "Reset" action. One-octave spacing from 62.5Hz
/// up matches both `Wiim-Dashboard`'s hardcoded `PEQ_DEFAULT_FREQ`
/// (`src/lib/wiim/eq-constants.ts`, which uses 31.25 for band `a`) *and*
/// a real WiiM Ultra's own factory-shipped, never-touched band-`a` value
/// (confirmed via `EQGetLV2SourceBandEx` against an untouched HDMI
/// source: `a_freq: 31.250`) — except band `a` specifically, which is
/// `30.0` here, not `31.25`: a live MITM capture of the official app's
/// own "Reset" button (2026-07-23, real Ultra) showed it writing
/// `a_freq: 30.0` while every other band matched the 31.25-based
/// octave series exactly. Deliberately matches the *app's* observed
/// Reset behavior over the device's own factory value, since this
/// function's whole purpose is reproducing what "Reset" does, not
/// reporting a factory default — no endpoint on hand exposes these
/// values directly either way. Past the 10th band (never expected in
/// practice, since PEQ is capped at 10 exposed bands everywhere else in
/// this codebase), falls back to doubling the last known value rather
/// than panicking or truncating.
pub fn default_peq_freq_hz(index: usize) -> f64 {
    const DEFAULTS: [f64; 10] = [30.0, 62.5, 125.0, 250.0, 500.0, 1000.0, 2000.0, 4000.0, 8000.0, 16000.0];
    match DEFAULTS.get(index) {
        Some(&hz) => hz,
        None => DEFAULTS[9] * 2f64.powi((index - 9) as i32),
    }
}

/// Stereo (shared bands) vs. independent left/right — real, app-observed
/// shape (Wiim-Dashboard: `channelMode: "L/R"` returns `EQBandL`/
/// `EQBandR`, not a flat `EQBand`), not yet seen in a capture on hand,
/// but modeled from the start rather than retrofitted later.
#[derive(Debug, Clone, PartialEq)]
pub enum ChannelBands<Band> {
    Stereo(Vec<Band>),
    LeftRight { left: Vec<Band>, right: Vec<Band> },
}

/// One EQ target's full state — a per-source slot, or the room-correction
/// layer. Mutually exclusive per the WiiM Ultra's own model ("off, GEQ,
/// or PEQ, not both") — an enum makes that invariant structural instead
/// of two nullable fields that could both be `Some` by a decode bug.
/// Generation is deliberately absent from this type — by the time `ui/`
/// sees an `EqState`, the dispatch layer has already resolved which
/// generation produced it.
#[derive(Debug, Clone, PartialEq)]
pub enum EqState {
    Off,
    Graphic { bands: ChannelBands<GraphicBand>, active_preset: Option<String> },
    Parametric { bands: ChannelBands<ParametricBand>, active_preset: Option<String> },
}

/// Which slot an `EqState` belongs to — the "data set" identifier a host
/// panel passes to this module's entry points. Identical shape whether
/// it's a per-source slot or the room-correction layer, including a
/// future/unrecognized one.
///
/// `Source`'s `String` is the device-reported `source_name` token
/// **verbatim**, exactly as it appeared in `EQGetSourceModes` (e.g.
/// `"HDMI"`, capitalized on a real Ultra) — *not* the lowercase canonical
/// input id (`capabilities::InputEntry::id`, e.g. `"hdmi"`). Every
/// outgoing command echoes this string back unchanged; never send a
/// canonical input id as `source_name`. Mapping to canonical ids (for
/// display labels only) is a UI-side, case-insensitive concern.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EqTarget {
    Source(String),
    RoomCorrection,
    /// Believed to always be a singleton (no per-source headphone EQ),
    /// unconfirmed — see `capabilities::EqLayerKind::HeadphoneEq`.
    HeadphoneEq,
    Other(String),
}

/// Device-reported preset names only — never matched against a
/// locally-compiled name table (`pywiim`'s ADR-017: firmware adds preset
/// labels over time; a hardcoded map breaks on names it doesn't know).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct EqPresetList {
    pub hardwired: Vec<String>,
    pub custom:    Vec<String>,
}

/// Per-target mechanism/preset/channel-mode summary — what a host panel
/// needs to render its selector rows before fetching any band data.
/// `kind`/`enabled` are two separate fields, not one `Option<EqKind>`,
/// because a source remembers its last-active plugin and preset name
/// even while its EQ is off (confirmed live, WiiM Mini: `EQGetSourceModes`
/// reported `pluginURI` for `EqNp` and `Name: "Test"` on the `wifi` entry
/// while that entry's `EQStat` was `"Off"`) — losing that would force the
/// panel to guess which curve to show for a currently-off target instead
/// of the device simply saying so.
#[derive(Debug, Clone, PartialEq)]
pub struct TargetOverview {
    pub target:  EqTarget,
    pub kind:    super::capabilities::EqKind,
    pub enabled: bool,
    pub preset:  Option<String>,
    /// `channelMode == "L/R"`.
    pub lr:      bool,
}

// ── EqSession ──────────────────────────────────────────────────────────────────

use std::sync::Arc;

use anyhow::{anyhow, bail};

use super::api::WiimClient;
use super::capabilities::{EqGeneration, EqKind, EqLayerKind, EqMechanism, EqProfile, EqTransport};
use super::api::ApiOutcome;

/// Opaque device handle for one EQ editing session — the only way `ui/`
/// reaches EQ I/O (mirrors `WiimClient` itself: `ui/` never touches that
/// directly either). Cheap to clone (an `Arc` + whatever `WiimClient`
/// itself is, already cheap to clone elsewhere in this codebase) — the
/// clone moved into each `rt.spawn`ed future.
#[derive(Clone)]
pub struct EqSession {
    client:  WiimClient,
    profile: Arc<EqProfile>,
}

impl EqSession {
    pub(crate) fn new(client: WiimClient, profile: Arc<EqProfile>) -> Self {
        Self { client, profile }
    }

    pub fn profile(&self) -> &EqProfile {
        &self.profile
    }

    fn layer_kind_for(target: &EqTarget) -> EqLayerKind {
        match target {
            EqTarget::Source(_)      => EqLayerKind::Source,
            EqTarget::RoomCorrection => EqLayerKind::RoomCorrection,
            EqTarget::HeadphoneEq    => EqLayerKind::HeadphoneEq,
            EqTarget::Other(s)       => EqLayerKind::Other(s.clone()),
        }
    }

    fn mechanism(&self, target: &EqTarget, kind: EqKind) -> anyhow::Result<&EqMechanism> {
        let layer_kind = Self::layer_kind_for(target);
        self.profile.layers.iter()
            .find(|l| l.kind == layer_kind)
            .ok_or_else(|| anyhow!("no {layer_kind:?} layer on this device"))?
            .mechanisms.iter().find(|m| m.kind == kind)
            .ok_or_else(|| anyhow!("no {kind:?} mechanism for {layer_kind:?}"))
    }

    fn require_lv2(mech: &EqMechanism) -> anyhow::Result<()> {
        match mech.transport {
            EqTransport::Http(EqGeneration::Lv2) => Ok(()),
            other => bail!("not implemented for this device (transport: {other:?})"),
        }
    }

    fn require_source(target: &EqTarget) -> anyhow::Result<&str> {
        match target {
            EqTarget::Source(s) => Ok(s.as_str()),
            other => bail!("target {other:?} has no source_name"),
        }
    }

    fn ok_body<'a>(text: &'a str, what: &str) -> anyhow::Result<&'a str> {
        match lv2::classify(text) {
            ApiOutcome::Ok(body)  => Ok(body),
            ApiOutcome::Unsupported => bail!("{what}: unsupported"),
            ApiOutcome::Failed      => bail!("{what}: failed"),
        }
    }

    /// One call covers every source on a `PerSource` layer (see the
    /// evidence section — confirmed on both a real Ultra and Mini).
    pub async fn get_overview(&self) -> anyhow::Result<Vec<TargetOverview>> {
        dbg(self.client.ip(), "get_overview");
        let text = self.client.cmd(&lv2::cmd_get_source_modes()).await?;
        let body = Self::ok_body(&text, "get_overview")?;
        lv2::parse_source_modes(body)
    }

    pub async fn get_eq_state(&self, target: &EqTarget, kind: EqKind) -> anyhow::Result<EqState> {
        dbg(self.client.ip(), &format!("get_eq_state target={target:?} kind={kind:?}"));
        let mech = self.mechanism(target, kind)?;
        Self::require_lv2(mech)?;
        let cmd = match target {
            EqTarget::Source(source) => lv2::cmd_get_source_band(kind, source),
            EqTarget::RoomCorrection => lv2::cmd_get_room_corr(),
            other => bail!("get_eq_state: unsupported target {other:?}"),
        };
        let text = self.client.cmd(&cmd).await?;
        let body = Self::ok_body(&text, "get_eq_state")?;
        Ok(lv2::parse_state(kind, mech.bands, body))
    }

    /// Stereo-mode partial write: `(band_index, band)` pairs, only those
    /// params sent. Errors on a target currently in L/R mode — this pass
    /// deliberately never sends a `channelMode: "Stereo"` payload to a
    /// target that's actually split (see "Editor widget contract"'s
    /// `ChannelBands::LeftRight` note); the host panel is responsible for
    /// knowing which write method applies before calling either.
    pub async fn set_graphic_bands(&self, target: &EqTarget,
        bands: &[(usize, GraphicBand)]) -> anyhow::Result<()> {
        dbg(self.client.ip(), &format!("set_graphic_bands target={target:?} {} band(s)", bands.len()));
        let mech = self.mechanism(target, EqKind::Graphic)?;
        Self::require_lv2(mech)?;
        let source = Self::require_source(target)?;
        let payload = lv2::encode_graphic_bands(bands, mech.bands);
        let cmd = lv2::cmd_set_source_band(EqKind::Graphic, source, payload);
        let text = self.client.cmd(&cmd).await?;
        Self::ok_body(&text, "set_graphic_bands")?;
        Ok(())
    }

    pub async fn set_parametric_bands(&self, target: &EqTarget,
        bands: &[(usize, ParametricBand)]) -> anyhow::Result<()> {
        dbg(self.client.ip(), &format!("set_parametric_bands target={target:?} {} band(s)", bands.len()));
        let mech = self.mechanism(target, EqKind::Parametric)?;
        Self::require_lv2(mech)?;
        let source = Self::require_source(target)?;
        let payload = lv2::encode_parametric_bands(bands, mech.bands)?;
        let cmd = lv2::cmd_set_source_band(EqKind::Parametric, source, payload);
        let text = self.client.cmd(&cmd).await?;
        Self::ok_body(&text, "set_parametric_bands")?;
        Ok(())
    }

    /// L/R-mode write: **full** band lists for both channels — see the
    /// "Editor widget contract" section's open question on why (partial
    /// single-channel writes are unconfirmed against a real device).
    pub async fn set_graphic_bands_lr(&self, target: &EqTarget,
        left: &[GraphicBand], right: &[GraphicBand]) -> anyhow::Result<()> {
        dbg(self.client.ip(), &format!("set_graphic_bands_lr target={target:?}"));
        let mech = self.mechanism(target, EqKind::Graphic)?;
        Self::require_lv2(mech)?;
        let source = Self::require_source(target)?;
        let left_idx: Vec<(usize, GraphicBand)>  = left.iter().cloned().enumerate().collect();
        let right_idx: Vec<(usize, GraphicBand)> = right.iter().cloned().enumerate().collect();
        let left_payload  = lv2::encode_graphic_bands(&left_idx, mech.bands);
        let right_payload = lv2::encode_graphic_bands(&right_idx, mech.bands);
        let cmd = lv2::cmd_set_source_band_lr(EqKind::Graphic, source, left_payload, right_payload);
        let text = self.client.cmd(&cmd).await?;
        Self::ok_body(&text, "set_graphic_bands_lr")?;
        Ok(())
    }

    pub async fn set_parametric_bands_lr(&self, target: &EqTarget,
        left: &[ParametricBand], right: &[ParametricBand]) -> anyhow::Result<()> {
        dbg(self.client.ip(), &format!("set_parametric_bands_lr target={target:?}"));
        let mech = self.mechanism(target, EqKind::Parametric)?;
        Self::require_lv2(mech)?;
        let source = Self::require_source(target)?;
        let left_idx: Vec<(usize, ParametricBand)>  = left.iter().cloned().enumerate().collect();
        let right_idx: Vec<(usize, ParametricBand)> = right.iter().cloned().enumerate().collect();
        let left_payload  = lv2::encode_parametric_bands(&left_idx, mech.bands)?;
        let right_payload = lv2::encode_parametric_bands(&right_idx, mech.bands)?;
        let cmd = lv2::cmd_set_source_band_lr(EqKind::Parametric, source, left_payload, right_payload);
        let text = self.client.cmd(&cmd).await?;
        Self::ok_body(&text, "set_parametric_bands_lr")?;
        Ok(())
    }

    /// Stereo ⟷ L/R (`EQSetChannelMode`) — a real device write, not a
    /// display-only toggle; changes the shape of the *next*
    /// `get_eq_state()` read (one band list vs. two).
    pub async fn set_channel_mode(&self, target: &EqTarget, kind: EqKind, lr: bool) -> anyhow::Result<()> {
        dbg(self.client.ip(), &format!("set_channel_mode target={target:?} kind={kind:?} lr={lr}"));
        let mech = self.mechanism(target, kind)?;
        Self::require_lv2(mech)?;
        if !mech.supports_lr_channels {
            bail!("this mechanism doesn't support L/R channels");
        }
        let source = Self::require_source(target)?;
        let cmd = lv2::cmd_set_channel_mode(kind, source, lr);
        let text = self.client.cmd(&cmd).await?;
        Self::ok_body(&text, "set_channel_mode")?;
        Ok(())
    }

    /// Enable/switch the target to `kind`'s plugin (`EQChangeSourceFX`).
    pub async fn enable_mechanism(&self, target: &EqTarget, kind: EqKind) -> anyhow::Result<()> {
        dbg(self.client.ip(), &format!("enable_mechanism target={target:?} kind={kind:?}"));
        let mech = self.mechanism(target, kind)?;
        Self::require_lv2(mech)?;
        let source = Self::require_source(target)?;
        let cmd = lv2::cmd_change_source_fx(kind, source);
        let text = self.client.cmd(&cmd).await?;
        Self::ok_body(&text, "enable_mechanism")?;
        Ok(())
    }

    /// Turn the target's EQ off. `current` = the mechanism that is (or
    /// was last) active — required because the wire command
    /// (`EQSourceOff`) takes a `pluginURI`; the panel always knows it
    /// from its `TargetOverview`. There is no plugin-less "off" command
    /// in the LV2 family.
    pub async fn disable(&self, target: &EqTarget, current: EqKind) -> anyhow::Result<()> {
        dbg(self.client.ip(), &format!("disable target={target:?} current={current:?}"));
        let mech = self.mechanism(target, current)?;
        Self::require_lv2(mech)?;
        let source = Self::require_source(target)?;
        let cmd = lv2::cmd_source_off(current, source);
        let text = self.client.cmd(&cmd).await?;
        Self::ok_body(&text, "disable")?;
        Ok(())
    }

    /// Per-plugin, not per-target — GEQ and PEQ each have one device-wide
    /// preset namespace (see the evidence section), so this doesn't take
    /// an `EqTarget` at all.
    pub async fn list_presets(&self, kind: EqKind) -> anyhow::Result<EqPresetList> {
        dbg(self.client.ip(), &format!("list_presets kind={kind:?}"));
        let text = self.client.cmd(&lv2::cmd_list(kind)).await?;
        let body = Self::ok_body(&text, "list_presets")?;
        let v: serde_json::Value = serde_json::from_str(body)?;
        let strings = |key: &str| -> Vec<String> {
            v.get(key).and_then(|a| a.as_array())
                .map(|a| a.iter().filter_map(|e| e.as_str().map(String::from)).collect())
                .unwrap_or_default()
        };
        Ok(EqPresetList { hardwired: strings("preset"), custom: strings("custom") })
    }

    /// Sends the load, sleeps `lv2::PRESET_SETTLE`, then re-reads — the
    /// one place this codebase deliberately re-fetches shortly after a
    /// write (see `pywiim`'s reversion-diagnostic caution in "Reference
    /// material": the device is handing back values the user hasn't seen
    /// yet, not an external actor).
    pub async fn load_preset(&self, target: &EqTarget, kind: EqKind, name: &str) -> anyhow::Result<EqState> {
        dbg(self.client.ip(), &format!("load_preset target={target:?} kind={kind:?} name={name:?}"));
        let mech = self.mechanism(target, kind)?;
        Self::require_lv2(mech)?;
        let source = Self::require_source(target)?;
        let cmd = lv2::cmd_source_load(kind, source, name);
        let text = self.client.cmd(&cmd).await?;
        Self::ok_body(&text, "load_preset")?;
        tokio::time::sleep(lv2::PRESET_SETTLE).await;
        self.get_eq_state(target, kind).await
    }

    pub async fn save_preset(&self, target: &EqTarget, kind: EqKind, name: &str) -> anyhow::Result<()> {
        dbg(self.client.ip(), &format!("save_preset target={target:?} kind={kind:?} name={name:?}"));
        if !is_valid_preset_name(name) {
            anyhow::bail!("preset name must contain only letters, numbers, and underscores: {name:?}");
        }
        let mech = self.mechanism(target, kind)?;
        Self::require_lv2(mech)?;
        let source = Self::require_source(target)?;
        let cmd = lv2::cmd_source_save(kind, source, name);
        let text = self.client.cmd(&cmd).await?;
        Self::ok_body(&text, "save_preset")?;
        Ok(())
    }

    pub async fn delete_preset(&self, kind: EqKind, name: &str) -> anyhow::Result<()> {
        dbg(self.client.ip(), &format!("delete_preset kind={kind:?} name={name:?}"));
        let cmd = lv2::cmd_delete(kind, name);
        let text = self.client.cmd(&cmd).await?;
        Self::ok_body(&text, "delete_preset")?;
        Ok(())
    }

    pub async fn rename_preset(&self, kind: EqKind, old: &str, new: &str) -> anyhow::Result<()> {
        dbg(self.client.ip(), &format!("rename_preset kind={kind:?} old={old:?} new={new:?}"));
        if !is_valid_preset_name(new) {
            anyhow::bail!("preset name must contain only letters, numbers, and underscores: {new:?}");
        }
        let cmd = lv2::cmd_rename(kind, old, new);
        let text = self.client.cmd(&cmd).await?;
        Self::ok_body(&text, "rename_preset")?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_peq_freq_hz_matches_app_reset_behavior() {
        // Band `a` is 30.0, not Wiim-Dashboard's 31.25 — see this
        // function's own doc comment for the real MITM evidence behind
        // that deliberate deviation.
        let expected = [30.0, 62.5, 125.0, 250.0, 500.0, 1000.0, 2000.0, 4000.0, 8000.0, 16000.0];
        for (i, &hz) in expected.iter().enumerate() {
            assert_eq!(default_peq_freq_hz(i), hz, "band index {i}");
        }
    }

    #[test]
    fn default_peq_freq_hz_extrapolates_past_10_bands() {
        // Never expected in practice (PEQ is capped at 10 exposed bands
        // everywhere else in this codebase) — just confirms this doesn't
        // panic/truncate if it ever were.
        assert_eq!(default_peq_freq_hz(10), 32000.0);
        assert_eq!(default_peq_freq_hz(11), 64000.0);
    }

    #[test]
    fn preset_name_validation_allows_only_ascii_alnum_and_underscore() {
        for ok in ["Rock", "bass_boost", "Preset_2", "ABC123", "_leading", "1"] {
            assert!(is_valid_preset_name(ok), "{ok:?} should be valid");
        }
        for bad in [
            "", "has space", "quote\"", "brace{", "angle<script>", "semi;colon",
            "emoji😀", "café", "new\nline", "a/b",
        ] {
            assert!(!is_valid_preset_name(bad), "{bad:?} should be rejected");
        }
    }
}
