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

