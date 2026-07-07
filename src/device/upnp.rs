/// Low-level UPnP SOAP request/response plumbing — the UPnP analogue of
/// `api.rs`. Owns wire-shaped (not canonical) response types and the SOAP
/// calls that produce them; `state.rs` decides *when* to call it, exactly
/// like it already decides when to call `WiimClient` methods.
/// `device/playback.rs` owns turning these into canonical `PlaybackState`
/// fields, not this module.
///
/// Real WiiM firmware's `GetInfoEx` (`AVTransport`) bundles transport state,
/// timing, volume+mute, loop mode, source, and the full DIDL-Lite track
/// metadata in one action — confirmed against 5 real device captures
/// (`captures/test-sources/WiiM_Ultra_20260706_*.json`). That's why this module only
/// ever calls `GetInfoEx`, not the separate `GetTransportInfo`/
/// `GetPositionInfo`/`GetVolume`/`GetMute` a naive per-UPnP-service reading
/// of the spec would suggest.

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use super::api::{build_reqwest_client, TlsMode};

pub static DEBUG_UPNP: AtomicBool = AtomicBool::new(false);

/// Higher-level tracing specific to this module (discovery, control-URL
/// resolution) — gated on `--debug=upnp`, distinct from the raw
/// request/response wire tracing `soap_call()` does via `api::debug()`
/// under `--debug=api` (see that call site's comment for why).
fn dbg(msg: &str) {
    if DEBUG_UPNP.load(Ordering::Relaxed) {
        println!("[upnp] {msg}");
    }
}

const REQUEST_TIMEOUT: Duration = Duration::from_secs(5);
const AV_TRANSPORT_SERVICE: &str = "urn:schemas-upnp-org:service:AVTransport:1";
/// Candidate `description.xml` ports, same well-known LinkPlay UPnP ports
/// `wiim-capture.rs`'s `fetch_description()` already probes.
const DESCRIPTION_PORTS: &[u16] = &[49152, 59152];

/// Everything `GetInfoEx` returns, wire-shaped (not canonical — see
/// `device/playback.rs`'s `decode_*_upnp` functions for the canonical
/// translation).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct InfoEx {
    pub transport_state: String,
    /// `"HH:MM:SS"` wire format, decoded by `playback::decode_hms_duration`.
    pub rel_time:        String,
    /// `"HH:MM:SS"` wire format, decoded by `playback::decode_hms_duration`.
    pub track_duration:  String,
    pub current_volume:  u32,
    pub current_mute:    bool,
    pub loop_mode:       i32,
    /// Confirmed byte-for-byte identical to HTTP `getPlayerStatusEx`'s
    /// `mode` across 18 real captures spanning 3 device families, zero
    /// exceptions — the same raw, uncorrected wire value, not something
    /// independently derived. Defaults to `-1` (never a real device value)
    /// when the tag is absent, matching `Inner::current_mode`'s own
    /// "never seen a mode yet" sentinel.
    pub play_type:       i32,
    pub play_medium:     String,
    pub track_source:    String,
    pub title:           String,
    pub artist:          String,
    pub album:           String,
    pub album_art_uri:   Option<String>,
    /// `None` when `<song:actualQuality>` is absent from the DIDL-Lite
    /// entirely; `Some("")` when present but empty. This distinction is the
    /// entire codec-badge rule (see `playback::decode_quality_upnp`) — don't
    /// collapse it to a plain `String`/`.unwrap_or_default()`.
    pub actual_quality:  Option<String>,
    pub bitrate:         String,
    /// Bit depth (DIDL-Lite's `song:format_s`).
    pub format_s:        String,
    pub rate_hz:         String,
    /// The `<res protocolInfo="...">` attribute, e.g.
    /// `"http-get:*:audio/mpeg:DLNA.ORG_PN=MP3;DLNA.ORG_OP=01;"`.
    pub protocol_info:   Option<String>,
}

/// UPnP control-point client for one device's `AVTransport` service.
/// Discovered once via `description.xml` (`UpnpClient::discover`) and reused
/// for every subsequent `get_info_ex()` poll.
#[derive(Debug, Clone)]
pub struct UpnpClient {
    control_url: String,
}

impl UpnpClient {
    /// Probe `description.xml` at the well-known LinkPlay UPnP ports/schemes,
    /// parse the `AVTransport` service block's `controlURL`, and resolve it
    /// against whichever candidate actually answered. `ip` may already
    /// include an embedded `:port` (the `--connect`/simulator testing
    /// convention used elsewhere) — only the host part is used here since
    /// UPnP's own port is independent of the main HTTP API's (real hardware:
    /// HTTPS on 443 for the API, plain HTTP on 49152 for UPnP).
    pub async fn discover(ip: &str) -> anyhow::Result<Self> {
        let host = ip.split(':').next().unwrap_or(ip);
        let mut last_err: Option<anyhow::Error> = None;
        for scheme in ["http", "https"] {
            for &port in DESCRIPTION_PORTS {
                let url = format!("{scheme}://{host}:{port}/description.xml");
                dbg(&format!("trying description.xml at {url}"));
                let client = build_reqwest_client(tls_for_scheme(scheme), REQUEST_TIMEOUT);
                let resp = match client.get(&url).send().await {
                    Ok(r) => r,
                    Err(e) => { last_err = Some(e.into()); continue; }
                };
                if !resp.status().is_success() {
                    last_err = Some(anyhow::anyhow!("{url}: HTTP {}", resp.status()));
                    continue;
                }
                let body = match resp.text().await {
                    Ok(b) => b,
                    Err(e) => { last_err = Some(e.into()); continue; }
                };
                match extract_av_transport_control_url(&body, &url) {
                    Some(control_url) => {
                        dbg(&format!("AVTransport control URL: {control_url}"));
                        return Ok(Self { control_url });
                    }
                    None => {
                        last_err = Some(anyhow::anyhow!(
                            "description.xml at {url} has no AVTransport service"
                        ));
                    }
                }
            }
        }
        Err(last_err.unwrap_or_else(|| anyhow::anyhow!("no description.xml candidate answered")))
    }

    pub async fn get_info_ex(&self) -> anyhow::Result<InfoEx> {
        let body = soap_call(&self.control_url, AV_TRANSPORT_SERVICE, "GetInfoEx", "<InstanceID>0</InstanceID>").await?;
        parse_info_ex_response(&body)
    }
}

fn tls_for_scheme(scheme: &str) -> TlsMode {
    if scheme == "https" { TlsMode::HttpsAny } else { TlsMode::Http }
}

async fn soap_call(control_url: &str, service_type: &str, action: &str, args_xml: &str) -> anyhow::Result<String> {
    let scheme = control_url.split(':').next().unwrap_or("http");
    let client = build_reqwest_client(tls_for_scheme(scheme), REQUEST_TIMEOUT);
    let body = format!(
        "<?xml version=\"1.0\" encoding=\"utf-8\"?>\r\n\
         <s:Envelope xmlns:s=\"http://schemas.xmlsoap.org/soap/envelope/\" \
         s:encodingStyle=\"http://schemas.xmlsoap.org/soap/encoding/\">\
         <s:Body><u:{action} xmlns:u=\"{service_type}\">{args_xml}</u:{action}></s:Body></s:Envelope>"
    );
    let soap_action_header = format!("\"{service_type}#{action}\"");
    // Same `[API]`/`--debug=api` request/response tracing as api.rs's
    // `cmd()` — reusing its `debug()`/`log_request_error()` directly
    // rather than a second, upnp-specific log format, since this is still
    // fundamentally an API call, just over SOAP instead of a plain GET.
    let resp = match client
        .post(control_url)
        .header("Content-Type", "text/xml; charset=\"utf-8\"")
        .header("SOAPACTION", soap_action_header)
        .body(body)
        .send()
        .await
    {
        Ok(r) => r,
        Err(e) => {
            super::api::log_request_error(action, &e);
            return Err(e.into());
        }
    };
    if !resp.status().is_success() {
        anyhow::bail!("{action}: HTTP {}", resp.status());
    }
    let text = resp.text().await?;
    super::api::debug(action, &text);
    Ok(text)
}

// ── description.xml / SOAP response parsing ──────────────────────────────────
//
// Minimal, non-namespace-aware `<tag>...</tag>`/attribute extraction —
// sufficient for the well-known tags LinkPlay's UPnP responses actually use.
// Ported from (not shared with) `wiim-capture.rs`'s identical helpers: that's
// a separate diagnostic binary crate that can't depend on this library
// crate's internals, and this module is the intended runtime-plumbing home
// for the same logic.

/// Extracts `<tag>...</tag>`'s content. `None` means the tag is entirely
/// absent — a present-but-empty tag (`<tag></tag>`) still returns `Some("")`.
/// Callers that care about that distinction (`actual_quality`) must not
/// collapse it with `.unwrap_or_default()`.
fn extract_tag(xml: &str, tag: &str) -> Option<String> {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    let start = xml.find(&open)? + open.len();
    let end = xml[start..].find(&close)? + start;
    Some(xml[start..end].trim().to_string())
}

fn extract_attr(tag_text: &str, attr: &str) -> Option<String> {
    let needle = format!("{attr}=\"");
    let start = tag_text.find(&needle)? + needle.len();
    let end = tag_text[start..].find('"')? + start;
    Some(tag_text[start..end].to_string())
}

/// Extracts the `<res protocolInfo="...">` attribute from a DIDL-Lite item.
fn extract_res_protocol_info(didl: &str) -> Option<String> {
    let start = didl.find("<res ")?;
    let tag_end = didl[start..].find('>').map(|i| start + i)?;
    extract_attr(&didl[start..tag_end], "protocolInfo")
}

fn extract_service_blocks(xml: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut rest = xml;
    while let Some(start) = rest.find("<service>") {
        let after = &rest[start + "<service>".len()..];
        match after.find("</service>") {
            Some(end) => {
                out.push(after[..end].to_string());
                rest = &after[end + "</service>".len()..];
            }
            None => break,
        }
    }
    out
}

/// Resolves a (possibly relative) `controlURL` against the origin
/// (`scheme://host:port`) of the `description.xml` URL it came from.
fn resolve_url(description_url: &str, maybe_relative: &str) -> String {
    if maybe_relative.starts_with("http://") || maybe_relative.starts_with("https://") {
        return maybe_relative.to_string();
    }
    let Some(scheme_end) = description_url.find("://") else {
        return maybe_relative.to_string();
    };
    let after_scheme = &description_url[scheme_end + 3..];
    let origin_end = after_scheme.find('/').map(|i| scheme_end + 3 + i).unwrap_or(description_url.len());
    let origin = &description_url[..origin_end];
    if maybe_relative.starts_with('/') {
        format!("{origin}{maybe_relative}")
    } else {
        format!("{origin}/{maybe_relative}")
    }
}

fn extract_av_transport_control_url(description_xml: &str, description_url: &str) -> Option<String> {
    for block in extract_service_blocks(description_xml) {
        let Some(service_type) = extract_tag(&block, "serviceType") else { continue };
        if !service_type.contains(":service:AVTransport:") { continue; }
        let control_url_raw = extract_tag(&block, "controlURL")?;
        return Some(resolve_url(description_url, &control_url_raw));
    }
    None
}

/// Unescapes the handful of entities LinkPlay's XML actually uses.
/// `&amp;` is replaced last so a literal `&amp;lt;` in the source (an
/// escaped ampersand followed by plain text "lt;") doesn't get
/// double-unescaped into `<`.
fn unescape_xml_entities(s: &str) -> String {
    s.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&amp;", "&")
}

/// Parses a `GetInfoExResponse` SOAP envelope. `TrackMetaData` is XML text
/// escaped *twice* on the wire: once by DIDL-Lite's own XML serialization
/// (a literal `&` in a title becomes `&amp;`), then again because the whole
/// DIDL-Lite document is embedded as escaped text inside the outer SOAP XML
/// (`<` becomes `&lt;`, and the already-escaped `&amp;` becomes `&amp;amp;`).
/// One `unescape_xml_entities` pass recovers real DIDL-Lite XML (tag
/// boundaries become real `<tag>`s); a second pass on the extracted leaf
/// text fields (title/artist/album) recovers real characters from any
/// DIDL-level entity still present in their content.
fn parse_info_ex_response(envelope: &str) -> anyhow::Result<InfoEx> {
    let transport_state = extract_tag(envelope, "CurrentTransportState").unwrap_or_default();
    let rel_time        = extract_tag(envelope, "RelTime").unwrap_or_default();
    let track_duration  = extract_tag(envelope, "TrackDuration").unwrap_or_default();
    let current_volume  = extract_tag(envelope, "CurrentVolume").and_then(|s| s.parse().ok()).unwrap_or(0);
    let current_mute    = extract_tag(envelope, "CurrentMute").is_some_and(|s| s == "1");
    let loop_mode        = extract_tag(envelope, "LoopMode").and_then(|s| s.parse().ok()).unwrap_or(-1);
    let play_type        = extract_tag(envelope, "PlayType").and_then(|s| s.parse().ok()).unwrap_or(-1);
    let play_medium      = extract_tag(envelope, "PlayMedium").unwrap_or_default();
    let track_source     = extract_tag(envelope, "TrackSource").unwrap_or_default();

    let track_metadata_raw = extract_tag(envelope, "TrackMetaData").unwrap_or_default();
    let didl = unescape_xml_entities(&track_metadata_raw);

    let title  = extract_tag(&didl, "dc:title").map(|s| unescape_xml_entities(&s)).unwrap_or_default();
    let artist = extract_tag(&didl, "upnp:artist").map(|s| unescape_xml_entities(&s)).unwrap_or_default();
    let album  = extract_tag(&didl, "upnp:album").map(|s| unescape_xml_entities(&s)).unwrap_or_default();
    let album_art_uri  = extract_tag(&didl, "upnp:albumArtURI");
    // `None` = tag absent, `Some("")` = present-but-empty — see `InfoEx::actual_quality`'s doc comment.
    let actual_quality = extract_tag(&didl, "song:actualQuality");
    let bitrate        = extract_tag(&didl, "song:bitrate").unwrap_or_default();
    let format_s       = extract_tag(&didl, "song:format_s").unwrap_or_default();
    let rate_hz        = extract_tag(&didl, "song:rate_hz").unwrap_or_default();
    let protocol_info  = extract_res_protocol_info(&didl);

    Ok(InfoEx {
        transport_state, rel_time, track_duration, current_volume, current_mute,
        loop_mode, play_type, play_medium, track_source, title, artist, album, album_art_uri,
        actual_quality, bitrate, format_s, rate_hz, protocol_info,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capture::format::CaptureFile;

    // These tests parse real `wiim-capture` capture files
    // (`captures/test-sources/WiiM_Ultra_20260706_*.json` — same-device
    // captures that vary by active source) rather than embedding SOAP
    // bodies as string literals in the source — same file format
    // `wiim-capdump` already reads (`CaptureFile`/`UpnpCapture`/
    // `UpnpActionCapture`/`Blob` in `src/capture/format.rs`), so no bespoke
    // parsing here. This is what proved the codec-badge rule that's
    // driving `decode_quality_upnp` — these tests are now that
    // investigation's regression coverage.

    fn load_capture(filename: &str) -> CaptureFile {
        let path = format!("{}/captures/test-sources/{filename}", env!("CARGO_MANIFEST_DIR"));
        let text = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("reading fixture {path}: {e}"));
        serde_json::from_str(&text)
            .unwrap_or_else(|e| panic!("parsing fixture {path}: {e}"))
    }

    fn blob_text(blob: &crate::capture::format::Blob) -> &str {
        blob.body.as_str().expect("blob body is not a string")
    }

    fn get_info_ex_body(cap: &CaptureFile) -> String {
        let upnp = cap.upnp.as_ref().expect("capture has no upnp section");
        let action = upnp.actions.iter().find(|a| a.action == "GetInfoEx")
            .expect("capture has no GetInfoEx action");
        blob_text(action.response.as_ref().expect("GetInfoEx has no response")).to_string()
    }

    #[test]
    fn control_url_discovery_from_real_description_xml() {
        let cap = load_capture("WiiM_Ultra_20260706_075156.TidalConnect-FLAC.json");
        let upnp = cap.upnp.as_ref().unwrap();
        let description_url = upnp.description_url.as_deref().unwrap();
        let description_xml = blob_text(upnp.description.as_ref().unwrap());
        let url = extract_av_transport_control_url(description_xml, description_url).unwrap();
        assert_eq!(url, "http://xx.x.x.xx:49152/upnp/control/rendertransport1");
    }

    #[test]
    fn flac_case_has_hi_res_lossless_actual_quality() {
        let cap = load_capture("WiiM_Ultra_20260706_075156.TidalConnect-FLAC.json");
        let info = parse_info_ex_response(&get_info_ex_body(&cap)).unwrap();
        assert_eq!(info.actual_quality.as_deref(), Some("HI_RES_LOSSLESS"));
        assert_eq!(info.title, "Superstar");
        assert_eq!(info.artist, "Diana Krall");
        assert_eq!(info.album, "Wallflower (Deluxe Edition)");
        assert_eq!(info.bitrate, "1571");
        assert_eq!(info.format_s, "24");
        assert_eq!(info.rate_hz, "48000");
        assert_eq!(info.play_medium, "TIDAL_CONNECT");
        assert_eq!(info.current_volume, 17);
        assert!(!info.current_mute);
        assert_eq!(info.rel_time, "00:00:45");
        assert_eq!(info.track_duration, "00:04:17");
    }

    #[test]
    fn high_case_has_lossless_actual_quality_and_unescapes_album_ampersand() {
        let cap = load_capture("WiiM_Ultra_20260706_075156.TidalConnect-HIGH.json");
        let info = parse_info_ex_response(&get_info_ex_body(&cap)).unwrap();
        assert_eq!(info.actual_quality.as_deref(), Some("LOSSLESS"));
        // Real album title has a literal "&", double-escaped on the wire
        // (`&amp;amp;`) — one unescape recovers real DIDL XML (`&amp;`
        // inside the tag content), a second recovers the literal "&".
        assert_eq!(info.album, "The Art Tatum & Ben Webster Quartet (Bonus Track Version)");
    }

    #[test]
    fn mp3_case_has_present_but_empty_actual_quality() {
        let cap = load_capture("WiiM_Ultra_20260706_075341.usb-mp3.json");
        let info = parse_info_ex_response(&get_info_ex_body(&cap)).unwrap();
        // Present-but-empty, not absent — this is the crux of the codec
        // badge rule (falls back to `protocol_info`, not to "no badge").
        assert_eq!(info.actual_quality.as_deref(), Some(""));
        assert_eq!(info.protocol_info.as_deref(), Some("http-get:*:audio/mpeg:DLNA.ORG_PN=MP3;DLNA.ORG_OP=01;"));
        assert_eq!(info.play_medium, "SONGLIST-LOCAL");
        assert_eq!(info.track_source, "UPnPServer");
    }

    #[test]
    fn tunein_case_has_actual_quality_tag_entirely_absent() {
        let cap = load_capture("WiiM_Ultra_20260706_075502.tunein.json");
        let info = parse_info_ex_response(&get_info_ex_body(&cap)).unwrap();
        // Absent, not empty — same `protocol_info` as the mp3 case
        // (`DLNA.ORG_PN=MP3` in both), which is exactly why the WiiM app's
        // "show a badge or not" decision can't be based on protocol_info
        // alone; only presence-vs-absence of this tag distinguishes them.
        assert_eq!(info.actual_quality, None);
        assert_eq!(info.protocol_info.as_deref(), Some("http-get:*:audio/mpeg:DLNA.ORG_PN=MP3;DLNA.ORG_OP=01;"));
        assert_eq!(info.play_medium, "RADIO-NETWORK");
        assert_eq!(info.track_source, "newTuneIn");
        assert_eq!(info.title, "Foolin’ Myself");
    }

    #[test]
    fn dlna_case_also_has_actual_quality_tag_entirely_absent() {
        // Third-party DLNA push (a "Music Assistant"-controlled cast, per
        // `dc:description`), not the device's own local `SONGLIST-LOCAL`
        // playback. Notably, `res protocolInfo` here genuinely says
        // `audio/flac` (unlike the mp3/tunein cases, which both said
        // `audio/mpeg` regardless of the tag's presence) — yet
        // `actual_quality` is still absent entirely, and per the confirmed
        // rule that alone means no codec badge, even though the real
        // content genuinely is FLAC and protocolInfo says so. Confirms the
        // rule is driven solely by tag presence, not by inspecting
        // protocolInfo's mime type as a smarter universal fallback.
        let cap = load_capture("WiiM_Ultra_20260706_084809.DLNA.json");
        let info = parse_info_ex_response(&get_info_ex_body(&cap)).unwrap();
        assert_eq!(info.actual_quality, None);
        assert!(info.protocol_info.as_deref().unwrap().contains("audio/flac"));
        assert_eq!(info.play_medium, "THIRD-DLNA");
        assert_eq!(info.track_source, "");
        assert_eq!(info.rate_hz, "192000");
        assert_eq!(info.format_s, "24");
        assert_eq!(info.title, "Doralice");
    }

    #[test]
    fn unescape_handles_double_escaped_ampersand() {
        assert_eq!(unescape_xml_entities("&amp;amp;"), "&amp;");
        assert_eq!(unescape_xml_entities("&lt;tag&gt;"), "<tag>");
    }
}
