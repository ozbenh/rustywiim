/// Low-level plumbing for the raw **TCP** UART pass-through protocol
/// (port 8899) — the tcpuart analogue of `api.rs`/`upnp.rs`. Named
/// `tcpuart` throughout to distinguish it from the separate *physical*
/// UART interface some of these same WiFi modules also expose (a literal
/// serial port, not this TCP-wrapped pass-through).
///
/// Skeleton only, like `upnp.rs`: packet framing plus a curated GET-only
/// command list, currently only exercised by `wiim-capture`'s `--tcpuart`
/// flag (and rendered by `wiim-capdump`) for exploration/capture, not
/// anything `state.rs` calls yet. Lives here rather than under
/// `src/capture/` in preparation for `rustywiim` itself eventually using
/// this transport for real — Audio Pro treble/bass control isn't
/// reachable over HTTP at all, only over this pass-through — `device/`
/// is meant to own every wire-protocol detail, not just the ones already
/// wired into `state.rs`.
///
/// Packet layout: `header(4) = 18 96 18 20` + `length(4, LE u32)` +
/// `checksum(4, LE u32) = sum of every payload byte` + `reserved(8) = 0x00×8`
/// + `payload(N bytes, ASCII)`. Confirmed by hand against Arylic's own
/// published TCP API spec (<https://developer.arylic.com/tcpapi/>) and
/// its sample packet for `MCU+VOL+050` (11 payload bytes summing to
/// `0x2c1`, matching that packet's checksum field exactly) — not a guess.
pub const TCPUART_PORT: u16 = 8899;

const HEADER: [u8; 4] = [0x18, 0x96, 0x18, 0x20];
const HEADER_LEN: usize = 20; // header(4) + length(4) + checksum(4) + reserved(8)

/// Wrap `payload` (a plain ASCII command like `"MCU+VOL+GET"`) in the full
/// binary packet ready to write to the socket.
pub fn build_packet(payload: &str) -> Vec<u8> {
    let bytes = payload.as_bytes();
    let checksum: u32 = bytes.iter().map(|&b| b as u32).sum();
    let mut packet = Vec::with_capacity(HEADER_LEN + bytes.len());
    packet.extend_from_slice(&HEADER);
    packet.extend_from_slice(&(bytes.len() as u32).to_le_bytes());
    packet.extend_from_slice(&checksum.to_le_bytes());
    packet.extend_from_slice(&[0u8; 8]);
    packet.extend_from_slice(bytes);
    packet
}

/// One packet parsed out of a raw byte stream, for display/validation —
/// not used for sending, only for `wiim-capdump`'s hexdump rendering.
pub struct ParsedPacket<'a> {
    pub header_ok: bool,
    pub declared_length: u32,
    pub declared_checksum: u32,
    /// Sum of the payload bytes actually present in `payload` — compare
    /// against `declared_checksum` to sanity-check the capture (a mismatch
    /// means either a transcription bug here, a genuinely different
    /// checksum algorithm on the device that sent this, or a truncated/
    /// multi-packet capture where `payload` isn't really one whole packet).
    pub computed_checksum: u32,
    pub payload: &'a [u8],
}

/// Parse one packet from the front of `raw`. Returns `None` if `raw` is
/// shorter than a bare header (20 bytes) — anything received is still
/// worth showing even if this fails, callers should fall back to a plain
/// hexdump of the whole buffer in that case rather than dropping it.
pub fn parse_packet(raw: &[u8]) -> Option<ParsedPacket<'_>> {
    if raw.len() < HEADER_LEN {
        return None;
    }
    let header_ok = raw[0..4] == HEADER;
    let declared_length = u32::from_le_bytes(raw[4..8].try_into().ok()?);
    let declared_checksum = u32::from_le_bytes(raw[8..12].try_into().ok()?);
    // reserved: raw[12..20], not checked — always zero in every sample seen.
    let available = raw.len() - HEADER_LEN;
    let take = (declared_length as usize).min(available);
    let payload = &raw[HEADER_LEN..HEADER_LEN + take];
    let computed_checksum: u32 = payload.iter().map(|&b| b as u32).sum();
    Some(ParsedPacket { header_ok, declared_length, declared_checksum, computed_checksum, payload })
}

/// GET-only commands to probe the protocol with. Every entry is read-only
/// by construction: no `Set`-style command is included, and there is no
/// `--destructive`-style escape hatch for this list at all, unlike the
/// main HTTP `commands.yaml` capture — there is nothing gated behind one.
pub const GET_COMMANDS: &[&str] = &[
    "MCU+DEV+GET",
    "MCU+INF+GET",
    "MCU+WWW+GET",
    "MCU+USB+GET",
    // Note the '?' terminator, not '&' like every other command here —
    // confirmed intentional (not a transcription typo, both were
    // observed on the wire as shown).
    "MCU+MMC+GET?",
    "MCU+VOL+GET",
    "MCU+MUT+GET",
    "MCU+PLP+GET",
    "MCU+PLM+GET",
    "MCU+SONGGET",
    "MCU+MEA+GET",
    "MCU+PINFGET",
    // Not in Arylic's own published TCP API docs — a capture finding
    // confirmed to reach real tone-control (bass/treble) state on Audio
    // Pro hardware (silent on iEAST AudioCast).
    "MCU+PAS+GET&",
    // Arylic's own documented EQ query — a different command from the
    // bare GET above, and answered differently on real Audio Pro
    // hardware (a single combined bass+treble message vs. the bare
    // GET's two separate ones, and a different centre value) — not
    // implemented at all there (silent), so both are worth sending.
    "MCU+PAS+EQGet&",
    // AP8064-platform passthrough identifier (`Rakoit:`, mixed case) —
    // per Arylic's docs, only relevant to PRO v1/2, AMP v1/v2, some
    // A50/S50. Harmless on a device that doesn't recognize it — expect
    // silence or an unrelated reply, not a crash.
    "MCU+PAS+Rakoit:GetBoard&",
    "MCU+PAS+Rakoit:GetCommit&",
    "MCU+PAS+Rakoit:GetPrompt&",
    "MCU+PAS+Rakoit:GetAPIVer&",
    "MCU+PAS+Rakoit:MaxVolume:Get&",
    "MCU+PAS+Rakoit:VB:Get&",
    // BP10XX-platform passthrough identifier (`RAKOIT:`, all caps — a
    // different identifier from AP8064's `Rakoit:` above, per Arylic's
    // own docs) — MINI V3, PRO v3, AMP v3 and newer.
    "MCU+PAS+RAKOIT:VOL&",
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_packet_checksum_matches_arylic_sample() {
        // Arylic's own published worked example for "MCU+VOL+050".
        let packet = build_packet("MCU+VOL+050");
        assert_eq!(&packet[0..4], &HEADER);
        assert_eq!(&packet[4..8], &11u32.to_le_bytes()); // length = 11 bytes
        assert_eq!(&packet[8..12], &0x2c1u32.to_le_bytes()); // checksum = 0x2c1
        assert_eq!(&packet[12..20], &[0u8; 8]);
        assert_eq!(&packet[20..], b"MCU+VOL+050");
    }

    #[test]
    fn parse_packet_round_trips_build_packet() {
        let packet = build_packet("MCU+VOL+GET");
        let parsed = parse_packet(&packet).expect("parses");
        assert!(parsed.header_ok);
        assert_eq!(parsed.declared_length, 11);
        assert_eq!(parsed.declared_checksum, parsed.computed_checksum);
        assert_eq!(parsed.payload, b"MCU+VOL+GET");
    }

    #[test]
    fn parse_packet_none_for_short_buffer() {
        assert!(parse_packet(&[0u8; 10]).is_none());
    }
}
