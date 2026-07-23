//! The newer per-source-aware LV2 command family (`EQv2*`/`EQGetLV2*Ex`/
//! `EQChangeSourceFX`) — command builders, JSON encoding, and decode/
//! encode for the wire shapes this generation uses. Pure functions only;
//! no network I/O (the caller sends the built command strings via
//! `WiimClient::cmd()` and hands the raw response text back in).

use std::collections::HashMap;

use super::{ChannelBands, EqState, EqTarget, GraphicBand, ParametricBand, PeqBandMode, TargetOverview};
use crate::device::api::ApiOutcome;
use crate::device::capabilities::EqKind;

pub(crate) const PLUGIN_URI_GRAPHIC: &str = "http://moddevices.com/plugins/caps/Eq10HP";
pub(crate) const PLUGIN_URI_PARAMETRIC: &str = "http://moddevices.com/plugins/caps/EqNp";
pub(crate) const PRESET_SETTLE: std::time::Duration = std::time::Duration::from_millis(300);

pub(crate) fn plugin_uri(kind: EqKind) -> &'static str {
    match kind {
        EqKind::Graphic => PLUGIN_URI_GRAPHIC,
        EqKind::Parametric => PLUGIN_URI_PARAMETRIC,
        EqKind::ToneControl => unreachable!("ToneControl has no LV2 pluginURI"),
    }
}

/// Percent-encode compact JSON for embedding in a `command=` query value —
/// same rule `pywiim`'s `quote(safe="")` uses. Relies on `serde_json`'s
/// default key ordering (`Value::Object` is a `BTreeMap`, alphabetical)
/// matching the real captured URLs (`pluginURI` before `source_name`) —
/// confirmed byte-for-byte against a real capture in this module's tests.
pub(crate) fn encode_json_arg(v: &serde_json::Value) -> String {
    let compact = v.to_string();
    let mut out = String::with_capacity(compact.len() * 3);
    for b in compact.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'_' | b'.' | b'~' | b'-' => out.push(b as char),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

// ── Command builders ──────────────────────────────────────────────────────────

pub(crate) fn cmd_get_source_modes() -> String {
    "EQGetSourceModes".to_string()
}

pub(crate) fn cmd_get_source_band(kind: EqKind, source: &str) -> String {
    let v = serde_json::json!({ "pluginURI": plugin_uri(kind), "source_name": source });
    format!("EQGetLV2SourceBandEx:{}", encode_json_arg(&v))
}

pub(crate) fn cmd_get_room_corr() -> String {
    "RoomCorrGet".to_string()
}

pub(crate) fn cmd_set_source_band(kind: EqKind, source: &str, eq_band: serde_json::Value) -> String {
    let v = serde_json::json!({
        "source_name": source,
        "pluginURI": plugin_uri(kind),
        "channelMode": "Stereo",
        "EQBand": eq_band,
    });
    format!("EQSetLV2SourceBand:{}", encode_json_arg(&v))
}

/// L/R write — whether a single-channel partial write is accepted is
/// unconfirmed against real hardware, so this always builds both
/// channels' full arrays instead, never a delta.
pub(crate) fn cmd_set_source_band_lr(
    kind: EqKind, source: &str,
    left: serde_json::Value, right: serde_json::Value,
) -> String {
    let v = serde_json::json!({
        "source_name": source,
        "pluginURI": plugin_uri(kind),
        "channelMode": "L/R",
        "EQBandL": left,
        "EQBandR": right,
    });
    format!("EQSetLV2SourceBand:{}", encode_json_arg(&v))
}

pub(crate) fn cmd_set_channel_mode(kind: EqKind, source: &str, lr: bool) -> String {
    let v = serde_json::json!({
        "source_name": source,
        "pluginURI": plugin_uri(kind),
        "channelMode": if lr { "L/R" } else { "Stereo" },
    });
    format!("EQSetChannelMode:{}", encode_json_arg(&v))
}

pub(crate) fn cmd_change_source_fx(kind: EqKind, source: &str) -> String {
    let v = serde_json::json!({ "source_name": source, "pluginURI": plugin_uri(kind) });
    format!("EQChangeSourceFX:{}", encode_json_arg(&v))
}

pub(crate) fn cmd_source_off(kind: EqKind, source: &str) -> String {
    let v = serde_json::json!({ "source_name": source, "pluginURI": plugin_uri(kind) });
    format!("EQSourceOff:{}", encode_json_arg(&v))
}

/// Colon form, unencoded — confirmed working against real hardware,
/// unlike `EQGetLV2SourceBand`'s (non-`Ex`) colon form.
pub(crate) fn cmd_list(kind: EqKind) -> String {
    format!("EQv2GetList:{}", plugin_uri(kind))
}

pub(crate) fn cmd_source_load(kind: EqKind, source: &str, name: &str) -> String {
    let v = serde_json::json!({ "source_name": source, "pluginURI": plugin_uri(kind), "Name": name });
    format!("EQv2SourceLoad:{}", encode_json_arg(&v))
}

pub(crate) fn cmd_source_save(kind: EqKind, source: &str, name: &str) -> String {
    let v = serde_json::json!({ "source_name": source, "pluginURI": plugin_uri(kind), "Name": name });
    format!("EQSourceSave:{}", encode_json_arg(&v))
}

pub(crate) fn cmd_delete(kind: EqKind, name: &str) -> String {
    let v = serde_json::json!({ "pluginURI": plugin_uri(kind), "Name": name });
    format!("EQv2Delete:{}", encode_json_arg(&v))
}

pub(crate) fn cmd_rename(kind: EqKind, old: &str, new: &str) -> String {
    let v = serde_json::json!({ "pluginURI": plugin_uri(kind), "Name": old, "newName": new });
    format!("EQv2Rename:{}", encode_json_arg(&v))
}

// ── Response classification ───────────────────────────────────────────────────

/// Applied to every raw reply. `"unknown command"`/`"not support"`
/// (substring, case-insensitive — some replies wrap it in other text) →
/// `Unsupported`; parses as JSON with `"status":"Failed"` → `Failed`
/// (the shape `EQGetStat` and a stray `EQGetLV2SourceBand` (non-`Ex`)
/// call both returned on a real Ultra); otherwise `Ok`.
pub(crate) fn classify(body: &str) -> ApiOutcome<&str> {
    let lower = body.to_lowercase();
    if lower.contains("unknown command") || lower.contains("not support") {
        return ApiOutcome::Unsupported;
    }
    if let Ok(v) = serde_json::from_str::<serde_json::Value>(body) {
        if v.get("status").and_then(|s| s.as_str()) == Some("Failed") {
            return ApiOutcome::Failed;
        }
    }
    ApiOutcome::Ok(body)
}

// ── Decode ─────────────────────────────────────────────────────────────────────

fn band_param_map(arr: &[serde_json::Value]) -> HashMap<String, f64> {
    let mut map = HashMap::new();
    for entry in arr {
        if let (Some(name), Some(value)) = (
            entry.get("param_name").and_then(|v| v.as_str()),
            entry.get("value").and_then(|v| v.as_f64()),
        ) {
            map.insert(name.to_string(), value);
        }
    }
    map
}

/// Confirmed live (2026-07-23, real WiiM Ultra, one custom preset with a
/// distinct filter per band): `Off=-1, LowShelf=0, Peak=1, HighShelf=2,
/// LowPass=3, HighPass=5` — `4` is unused by any of the six
/// `PEQ.Filters` entries.
fn mode_from_wire(value: f64) -> PeqBandMode {
    match value.round() as i64 {
        -1 => PeqBandMode::Off,
        0  => PeqBandMode::LowShelf,
        1  => PeqBandMode::Peak,
        2  => PeqBandMode::HighShelf,
        3  => PeqBandMode::LowPass,
        5  => PeqBandMode::HighPass,
        _  => PeqBandMode::Other(value.to_string()),
    }
}

/// Reverse of `mode_from_wire()`. Errors on `Other` — never send an
/// unrecognized/guessed numeric value back to the device.
fn mode_to_wire(mode: &PeqBandMode) -> anyhow::Result<f64> {
    Ok(match mode {
        PeqBandMode::Off       => -1.0,
        PeqBandMode::LowShelf  =>  0.0,
        PeqBandMode::Peak      =>  1.0,
        PeqBandMode::HighShelf =>  2.0,
        PeqBandMode::LowPass   =>  3.0,
        PeqBandMode::HighPass  =>  5.0,
        PeqBandMode::Other(s)  => anyhow::bail!("cannot encode unrecognized filter mode {s:?}"),
    })
}

/// Decode a raw `EQBand`/`EQBandL`/`EQBandR` array into parametric bands,
/// letters `a..` in order, capped at `bands_cap` — **the one place WiiM
/// PEQ's k/l safety rule is enforced on the read side**: this simply
/// stops once `bands_cap` bands have been produced, so a caller can never
/// receive band `k`/`l` no matter how many the wire array actually
/// contains.
pub(crate) fn parse_parametric_bands(arr: &[serde_json::Value], bands_cap: u8) -> Vec<ParametricBand> {
    let map = band_param_map(arr);
    let mut out = Vec::new();
    for i in 0..26u8 {
        if out.len() >= bands_cap as usize { break; }
        let letter = (b'a' + i) as char;
        let Some(&mode_v) = map.get(&format!("{letter}_mode")) else { break };
        let freq = map.get(&format!("{letter}_freq")).copied().unwrap_or(1000.0);
        let q    = map.get(&format!("{letter}_q")).copied().unwrap_or(0.7);
        let gain = map.get(&format!("{letter}_gain")).copied().unwrap_or(0.0);
        out.push(ParametricBand { mode: mode_from_wire(mode_v), freq_hz: freq, q, gain_db: gain });
    }
    out
}

/// Decode a raw `EQBand`/`EQBandL`/`EQBandR` array into graphic bands, in
/// `index` order, capped at `bands_cap` (no-op cap for GEQ today — 10
/// wire, 10 shown, no gap — but still enforced here rather than assumed).
pub(crate) fn parse_graphic_bands(arr: &[serde_json::Value], bands_cap: u8) -> Vec<GraphicBand> {
    let mut entries: Vec<&serde_json::Value> = arr.iter().collect();
    entries.sort_by_key(|e| e.get("index").and_then(|v| v.as_i64()).unwrap_or(0));
    let mut out = Vec::new();
    for entry in entries {
        if out.len() >= bands_cap as usize { break; }
        let Some(name) = entry.get("param_name").and_then(|v| v.as_str()) else { continue };
        // "band31hz" -> "31", "band1khz" -> "1k".
        let Some(freq_label) = name.strip_prefix("band").and_then(|s| s.strip_suffix("hz")) else { continue };
        let Some(gain) = entry.get("value").and_then(|v| v.as_f64()) else { continue };
        out.push(GraphicBand { param_name: name.to_string(), freq_label: freq_label.to_string(), gain_db: gain });
    }
    out
}

/// Full-state decode for one target+mechanism. Per the evidence section:
/// band data is present-and-valid even while `EQStat` is `"Off"` (except
/// the known `EQGetLV2BandEx`-without-source bug, which this guards
/// against defensively) — so `enabled` is deliberately **not** derived
/// here; the host panel gets that from `TargetOverview` instead. Only a
/// JSON parse failure, or a missing/malformed band array, falls back to
/// `EqState::Off`.
pub(crate) fn parse_state(kind: EqKind, bands_cap: u8, body: &str) -> EqState {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(body) else {
        return EqState::Off;
    };
    let active_preset = v.get("Name").and_then(|n| n.as_str())
        .filter(|s| !s.is_empty()).map(|s| s.to_string());
    let is_lr = v.get("channelMode").and_then(|c| c.as_str()) == Some("L/R");

    macro_rules! decode_channel_bands {
        ($parse_fn:ident) => {{
            if is_lr {
                let left  = v.get("EQBandL").and_then(|b| b.as_array());
                let right = v.get("EQBandR").and_then(|b| b.as_array());
                let (Some(left), Some(right)) = (left, right) else { return EqState::Off };
                ChannelBands::LeftRight {
                    left:  $parse_fn(left, bands_cap),
                    right: $parse_fn(right, bands_cap),
                }
            } else {
                let Some(arr) = v.get("EQBand").and_then(|b| b.as_array()) else { return EqState::Off };
                ChannelBands::Stereo($parse_fn(arr, bands_cap))
            }
        }};
    }

    match kind {
        EqKind::Graphic => EqState::Graphic {
            bands: decode_channel_bands!(parse_graphic_bands),
            active_preset,
        },
        EqKind::Parametric => EqState::Parametric {
            bands: decode_channel_bands!(parse_parametric_bands),
            active_preset,
        },
        EqKind::ToneControl => unreachable!("ToneControl has no LV2 shape"),
    }
}

/// `EQGetSourceModes` — one entry per source, the anchor call for
/// `EqSession::get_overview()` on a `PerSource` layer (see the evidence
/// section: one call covers every source, confirmed on both a real WiiM
/// Ultra and Mini). Unrecognized `pluginURI`s are skipped defensively
/// rather than erroring the whole list.
pub(crate) fn parse_source_modes(body: &str) -> anyhow::Result<Vec<TargetOverview>> {
    let arr: Vec<serde_json::Value> = serde_json::from_str(body)?;
    let mut out = Vec::new();
    for entry in arr {
        let Some(source) = entry.get("source_name").and_then(|v| v.as_str()) else { continue };
        let Some(uri) = entry.get("pluginURI").and_then(|v| v.as_str()) else { continue };
        let kind = if uri == PLUGIN_URI_GRAPHIC {
            EqKind::Graphic
        } else if uri == PLUGIN_URI_PARAMETRIC {
            EqKind::Parametric
        } else {
            continue;
        };
        let enabled = entry.get("EQStat").and_then(|v| v.as_str()) == Some("On");
        let preset = entry.get("Name").and_then(|v| v.as_str())
            .filter(|s| !s.is_empty()).map(String::from);
        let lr = entry.get("channelMode").and_then(|v| v.as_str()) == Some("L/R");
        out.push(TargetOverview { target: EqTarget::Source(source.to_string()), kind, enabled, preset, lr });
    }
    Ok(out)
}

// ── Encode (writes) ────────────────────────────────────────────────────────────

/// Stereo-mode partial write: only the touched bands' 4 params each.
/// `debug_assert`s every index against `bands_cap` — the k/l rule
/// enforced at the encode boundary too, not just decode.
pub(crate) fn encode_parametric_bands(
    bands: &[(usize, ParametricBand)], bands_cap: u8,
) -> anyhow::Result<serde_json::Value> {
    let mut arr = Vec::new();
    for (idx, band) in bands {
        debug_assert!(*idx < bands_cap as usize,
            "band index {idx} exceeds cap {bands_cap} — k/l safety rule violated");
        let letter = (b'a' + *idx as u8) as char;
        let mode_v = mode_to_wire(&band.mode)?;
        arr.push(serde_json::json!({"param_name": format!("{letter}_mode"),  "value": mode_v}));
        arr.push(serde_json::json!({"param_name": format!("{letter}_freq"), "value": band.freq_hz}));
        arr.push(serde_json::json!({"param_name": format!("{letter}_q"),    "value": band.q}));
        arr.push(serde_json::json!({"param_name": format!("{letter}_gain"), "value": band.gain_db}));
    }
    Ok(serde_json::Value::Array(arr))
}

/// Graphic write: one `{param_name, value}` per band, echoing the
/// device-reported `param_name` back verbatim (no label reconstruction).
pub(crate) fn encode_graphic_bands(bands: &[(usize, GraphicBand)], bands_cap: u8) -> serde_json::Value {
    let arr: Vec<_> = bands.iter().map(|(idx, band)| {
        debug_assert!(*idx < bands_cap as usize,
            "band index {idx} exceeds cap {bands_cap} — k/l safety rule violated");
        serde_json::json!({"param_name": band.param_name, "value": band.gain_db})
    }).collect();
    serde_json::Value::Array(arr)
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

    fn command_body(cap: &CaptureFile, command: &str) -> String {
        cap.commands.iter()
            .find(|c| c.command == command)
            .unwrap_or_else(|| panic!("capture has no command {command:?}"))
            .body.clone()
            .unwrap_or_else(|| panic!("command {command:?} has no body"))
            .to_string()
    }

    /// Confirms the hand-rolled percent-encoder produces exactly the same
    /// query string `wiim-capture` recorded from a real request — not
    /// just "some valid encoding," byte-for-byte identical, including key
    /// order (`pluginURI` before `source_name`).
    #[test]
    fn encode_json_arg_matches_real_captured_url() {
        let expected = "EQGetLV2SourceBandEx:%7B%22pluginURI%22%3A%22http%3A%2F%2Fmoddevices.com%2Fplugins%2Fcaps%2FEqNp%22%2C%22source_name%22%3A%22wifi%22%7D";
        assert_eq!(cmd_get_source_band(EqKind::Parametric, "wifi"), expected);
    }

    /// The known device bug (`EQGetLV2BandEx` without a source, `EQStat:
    /// "Off"`): `EQBand`'s value is missing entirely, invalid JSON. Must
    /// decode to `EqState::Off` gracefully, never panic.
    #[test]
    fn parse_state_handles_malformed_eqband_gracefully() {
        use base64::Engine;
        let cap = load_capture("WiiM_Ultra_20260708_100034.json");
        let entry = cap.commands.iter()
            .find(|c| c.command.starts_with("EQGetLV2BandEx") && c.command.contains("EqNp"))
            .expect("capture has no EQGetLV2BandEx/EqNp entry");
        assert_eq!(entry.format, Some(crate::capture::format::ResponseFormat::Base64),
            "expected this malformed entry to be stored as base64 (JSON parse failed at capture time)");
        let raw = entry.body.as_ref().and_then(|b| b.as_str())
            .expect("base64 body should be a JSON string");
        let decoded_bytes = base64::engine::general_purpose::STANDARD.decode(raw)
            .expect("valid base64");
        let decoded = String::from_utf8(decoded_bytes)
            .expect("decoded bytes should be UTF-8 text, just not valid JSON");
        assert!(decoded.ends_with("\"EQBand\":}"), "capture shape changed: {decoded:?}");
        assert_eq!(parse_state(EqKind::Parametric, 10, &decoded), EqState::Off);
    }

    /// Real PEQ response is wire-12-band; capped at 10 (WiiM's own
    /// app-shown band count — the last two, k/l, are never touched) must
    /// produce exactly 10, never touching bands k/l.
    #[test]
    fn parse_parametric_bands_truncates_12_wire_bands_to_10() {
        let cap = load_capture("WiiM_Ultra_20260708_100034.json");
        let body = command_body(&cap, "EQGetLV2SourceBandEx:%7B%22pluginURI%22%3A%22http%3A%2F%2Fmoddevices.com%2Fplugins%2Fcaps%2FEqNp%22%2C%22source_name%22%3A%22wifi%22%7D");
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        let arr = v["EQBand"].as_array().expect("EQBand array");
        assert_eq!(arr.len(), 48, "expected 12 bands x 4 params in the raw wire array");
        let bands = parse_parametric_bands(arr, 10);
        assert_eq!(bands.len(), 10, "must cap at 10, never expose k/l");
    }

    /// Full round-trip through `parse_state()` for the same 12-band wire
    /// response, confirming the cap applies at that level too.
    #[test]
    fn parse_state_caps_parametric_bands_at_10() {
        let cap = load_capture("WiiM_Ultra_20260708_100034.json");
        let body = command_body(&cap, "EQGetLV2SourceBandEx:%7B%22pluginURI%22%3A%22http%3A%2F%2Fmoddevices.com%2Fplugins%2Fcaps%2FEqNp%22%2C%22source_name%22%3A%22wifi%22%7D");
        let EqState::Parametric { bands: ChannelBands::Stereo(bands), .. } =
            parse_state(EqKind::Parametric, 10, &body)
        else { panic!("expected Parametric/Stereo state") };
        assert_eq!(bands.len(), 10);
    }

    /// Live-confirmed filter-mode table (2026-07-23, real WiiM Ultra): a
    /// "Test" preset with bands a-f set to PK/LS/HS/LP/HP/OFF respectively.
    #[test]
    fn parse_parametric_bands_confirmed_filter_mode_table() {
        let cap = load_capture("WiiM_Ultra_20260723_020535.EQTest.json");
        let body = command_body(&cap, "EQGetLV2SourceBandEx:%7B%22pluginURI%22%3A%22http%3A%2F%2Fmoddevices.com%2Fplugins%2Fcaps%2FEqNp%22%2C%22source_name%22%3A%22wifi%22%7D");
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        let arr = v["EQBand"].as_array().expect("EQBand array");
        let bands = parse_parametric_bands(arr, 10);
        assert_eq!(bands[0].mode, PeqBandMode::Peak,      "band a");
        assert_eq!(bands[1].mode, PeqBandMode::LowShelf,  "band b");
        assert_eq!(bands[2].mode, PeqBandMode::HighShelf, "band c");
        assert_eq!(bands[3].mode, PeqBandMode::LowPass,   "band d");
        assert_eq!(bands[4].mode, PeqBandMode::HighPass,  "band e");
        assert_eq!(bands[5].mode, PeqBandMode::Off,       "band f");
    }

    #[test]
    fn mode_to_wire_round_trips_known_modes() {
        for (mode, value) in [
            (PeqBandMode::Off, -1.0), (PeqBandMode::LowShelf, 0.0),
            (PeqBandMode::Peak, 1.0), (PeqBandMode::HighShelf, 2.0),
            (PeqBandMode::LowPass, 3.0), (PeqBandMode::HighPass, 5.0),
        ] {
            assert_eq!(mode_to_wire(&mode).unwrap(), value);
            assert_eq!(mode_from_wire(value), mode);
        }
    }

    #[test]
    fn mode_to_wire_rejects_unrecognized_mode() {
        assert!(mode_to_wire(&PeqBandMode::Other("4".to_string())).is_err());
    }

    #[test]
    fn parse_graphic_bands_decodes_frequency_labels() {
        let cap = load_capture("WiiM_Ultra_20260708_100034.json");
        let body = command_body(&cap, "EQGetLV2SourceBandEx:%7B%22pluginURI%22%3A%22http%3A%2F%2Fmoddevices.com%2Fplugins%2Fcaps%2FEq10HP%22%2C%22source_name%22%3A%22wifi%22%7D");
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        let arr = v["EQBand"].as_array().expect("EQBand array");
        let bands = parse_graphic_bands(arr, 10);
        assert_eq!(bands.len(), 10);
        assert_eq!(bands[0].freq_label, "31");
        assert_eq!(bands[0].param_name, "band31hz");
        assert_eq!(bands[5].freq_label, "1k");
        assert_eq!(bands[5].param_name, "band1khz");
        assert_eq!(bands[9].freq_label, "16k");
    }

    #[test]
    fn parse_source_modes_decodes_real_capture() {
        let cap = load_capture("WiiM_Mini_20260723_020506.EQTest.json");
        let body = command_body(&cap, "EQGetSourceModes");
        let overview = parse_source_modes(&body).expect("parsing EQGetSourceModes");
        let wifi = overview.iter().find(|t| t.target == EqTarget::Source("wifi".to_string()))
            .expect("wifi entry");
        assert_eq!(wifi.kind, EqKind::Parametric);
        assert!(wifi.enabled, "EQStat was On in this capture (the Test preset, actively engaged)");
        assert_eq!(wifi.preset.as_deref(), Some("Test"));
        let line_in = overview.iter().find(|t| t.target == EqTarget::Source("line-in".to_string()))
            .expect("line-in entry");
        assert_eq!(line_in.kind, EqKind::Graphic);
        assert!(!line_in.enabled, "line-in's EQ is off in this capture");
        assert_eq!(line_in.preset, None);
    }

    #[test]
    fn encode_parametric_bands_never_produces_kl_params() {
        let bands = [(0usize, ParametricBand {
            mode: PeqBandMode::Peak, freq_hz: 1000.0, q: 1.0, gain_db: 3.0,
        })];
        let encoded = encode_parametric_bands(&bands, 10).unwrap();
        let text = encoded.to_string();
        assert!(!text.contains("\"k_"));
        assert!(!text.contains("\"l_"));
        assert!(text.contains("a_mode"));
    }

    #[test]
    fn encode_parametric_bands_rejects_other_mode() {
        let bands = [(0usize, ParametricBand {
            mode: PeqBandMode::Other("4".to_string()), freq_hz: 1000.0, q: 1.0, gain_db: 0.0,
        })];
        assert!(encode_parametric_bands(&bands, 10).is_err());
    }
}
