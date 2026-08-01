//! Shared device-identity helpers, and the home of one codebase-wide rule.
//!
//! # UUID normalisation: normalise once, at the edge
//!
//! **A device uuid is normalised exactly where it enters the app from the
//! outside world. Everywhere inside, uuids are already canonical — compare
//! them with `==` and key maps on them directly.**
//!
//! Re-normalising an already-canonical value is harmless (the function is
//! idempotent), but it burns CPU on per-poll and per-lookup paths and, worse,
//! obscures where the real boundary is: if every comparison site normalises
//! defensively, nobody can tell which one is actually load-bearing, and a
//! genuinely missing boundary hides indefinitely.
//!
//! ## The entry boundaries
//!
//! These are the only places a raw uuid crosses in. Each normalises, so
//! nothing downstream ever sees an unnormalised form:
//!
//! 1. **Config file** — `Config::normalize_uuids()`, called from `load()`:
//!    every `devices` key, `last_uuid`, `kiosk_last_uuid`. Keys that collide
//!    once normalised are merged (first wins); the empty key is dropped.
//! 2. **Device HTTP API** — `deserialize_normalized` on `DeviceInfo`'s `uuid`
//!    and `master_uuid` (`getStatusEx`/`getStatus`).
//! 3. **SSDP / UPnP discovery** — `extract_uuid_from_usn()`. SSDP `UDN` is the
//!    most punctuated shape there is: `uuid:` prefix, hyphens, arbitrary case.
//! 4. **UPnP `GetInfoEx`** — `parse_info_ex_response()` normalises `MasterUUID`
//!    at the parse site.
//! 5. **A leader's slave list** — `group::decode_member()` normalises each
//!    member uuid.
//!
//! Users only ever type IP addresses, never uuids, so manual "Add Device" is
//! not a separate boundary — it picks its uuid up from #2.
//!
//! ## Adding code that handles uuids
//!
//! - **New wire or config field carrying a uuid?** That is a new boundary.
//!   Put `#[serde(deserialize_with = "…deserialize_normalized")]` on it, or
//!   call `normalize_uuid` at the parse site if it is not serde-driven. GENA
//!   (`gena.rs`) carries no uuid fields yet; when it does, it is next on the
//!   list above.
//! - **Consuming a uuid from anywhere inside** (`DeviceState::uuid()`,
//!   `ManagedEntry.uuid`, a config `devices` key, `DeviceInfo.uuid`,
//!   `GroupState::leader_uuid`)? It is already canonical. Do not re-normalise.
//!   Reach for [`same_device`] only where a side may legitimately be empty, or
//!   where tolerance is wanted on purpose.
//! - Prefer keying maps on `discovery_manager::device_key()` (uuid, else
//!   `ip:<addr>`) for anything that must line up with the existing device maps.
//!
//! ## The deliberate exceptions
//!
//! Three interior sites re-normalise even though their inputs are provably
//! canonical. All three are one-shot or near-enough, and each guards an
//! identity invariant, so the cost is nil and they mark a seam worth marking:
//! `discovery_manager::device_key()` (computed independently by `device/` and
//! by `ui/`, so drift between the two would be invisible), `DeviceState::new()`
//! (makes `uuid()` canonical by construction for the identity check against
//! `getStatusEx`), and `DeviceManager::get_state()` (the lookup deciding
//! whether a group member is driven directly or relayed through its leader).
//! Anything beyond these three is redundant — strip it.

// ── UUID normalisation ────────────────────────────────────────────────────────

/// Normalises the several shapes a device uuid arrives in so they can be
/// compared: an SSDP `UDN` carries a `uuid:` prefix, slave-list entries
/// sometimes do, and hyphenation and case both vary by source.
///
/// Returns an empty string for empty input, which callers treat as
/// "unknown" rather than as a match — comparing two unknowns must never
/// succeed.
pub fn normalize_uuid(raw: &str) -> String {
    let trimmed = raw.trim();
    // Case-insensitively: the prefix is conventionally lower-case in an
    // SSDP `UDN`, but a config file or a device is free to spell it
    // otherwise, and a missed prefix would survive the filter below as four
    // extra characters rather than being stripped.
    let without_prefix = trimmed
        .get(..5)
        .filter(|p| p.eq_ignore_ascii_case("uuid:"))
        .map_or(trimmed, |_| &trimmed[5..]);
    without_prefix
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .map(|c| c.to_ascii_lowercase())
        .collect()
}

/// True for an address a leader reports that this host cannot route to.
///
/// A leader that groups over WiFi-Direct raises its own access point and
/// moves its followers onto `10.10.10.x`, so the addresses it reports for
/// them are on a network we are not on. Probing those is guaranteed to
/// fail, and doing it repeatedly would light up the failure and retry
/// paths for devices that are working perfectly well — they are simply
/// unreachable by design for as long as the group exists.
pub fn is_wifi_direct_address(ip: &str) -> bool {
    ip.trim().starts_with("10.10.10.")
}

/// `serde` field attribute for a uuid arriving from a device or from
/// config: normalises it as it is deserialised, so nothing downstream ever
/// sees the raw form.
///
/// Use this on every uuid field of a wire or config struct. Normalising at
/// the boundary is what lets the rest of the app compare and key on uuids
/// with plain `==`; doing it at each comparison site instead is how one
/// device ends up tracked under two keys.
pub fn deserialize_normalized<'de, D>(d: D) -> Result<String, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde::Deserialize;
    let raw = String::deserialize(d)?;
    Ok(normalize_uuid(&raw))
}

/// True when both uuids are non-empty and refer to the same device.
pub fn same_device(a: &str, b: &str) -> bool {
    let (a, b) = (normalize_uuid(a), normalize_uuid(b));
    !a.is_empty() && a == b
}

