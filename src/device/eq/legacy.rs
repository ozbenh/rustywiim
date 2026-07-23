//! The older global-only `EQ*` command family
//! (`EQGetList`/`EQLoad`/`EQOn`/`EQOff`/`EQGetBand` — note `EQGetStat` is
//! known-unreliable, per both a real Ultra capture and `pywiim`'s
//! cross-confirming comment, so nothing here should ever call it).
//!
//! Only reachable for a mechanism whose transport resolved to
//! `Http(Legacy)` — plausibly the Arylic S10+'s 8-band GEQ (per its "old
//! school EQ APIs" framing), still untraced, and possibly non-WiiM
//! LinkPlay/Audio Pro families too, not yet confirmed against a real
//! non-WiiM HTTP-EQ capture. Deliberately minimal this pass — just enough
//! to detect *whether* a device has this generation at all
//! (`resolve_eq_profile()`'s fallback when LV2 doesn't answer); real
//! decode/encode for it waits for a real Legacy-generation device.

use super::super::api::WiimClient;

/// Whether `EQGetList` answers with a JSON array at all — the one signal
/// `resolve_eq_profile()` needs to know this generation exists, without
/// yet being able to do anything with it.
pub(crate) async fn probe(client: &WiimClient) -> bool {
    let Ok(text) = client.cmd("EQGetList").await else { return false };
    serde_json::from_str::<Vec<serde_json::Value>>(&text).is_ok()
}
