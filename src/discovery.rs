use std::collections::HashSet;
use std::net::SocketAddr;
use std::time::Duration;
use tokio::net::UdpSocket;
use tokio::time::timeout;

use crate::api::{TlsMode, api_base_url, build_reqwest_client};

#[derive(Debug, Clone)]
pub struct DiscoveredDevice {
    pub ip:       String,
    pub name:     String,
    /// UUID from `getStatusEx`.  Stable hardware identifier used as the
    /// per-device config key.  Empty for devices found only via the UPnP
    /// fallback path (where we can't reach the API).
    pub uuid:     String,
    /// The TLS mode to use for subsequent connections to this device.
    pub tls_mode: TlsMode,
}

const SSDP_ADDR: &str = "239.255.255.250:1900";

const SEARCH_MSGS: &[&str] = &[
    "M-SEARCH * HTTP/1.1\r\n\
     HOST: 239.255.255.250:1900\r\n\
     MAN: \"ssdp:discover\"\r\n\
     MX: 3\r\n\
     ST: urn:schemas-upnp-org:device:MediaRenderer:1\r\n\
     \r\n",
    "M-SEARCH * HTTP/1.1\r\n\
     HOST: 239.255.255.250:1900\r\n\
     MAN: \"ssdp:discover\"\r\n\
     MX: 3\r\n\
     ST: ssdp:all\r\n\
     \r\n",
];

const PROBE_MODES: &[TlsMode] = &[
    TlsMode::HttpsWiiM,
    TlsMode::HttpsAudioPro,
    TlsMode::Http,
];

pub async fn discover(duration: Duration) -> Vec<DiscoveredDevice> {
    let Ok(sock) = UdpSocket::bind("0.0.0.0:0").await else {
        return Vec::new();
    };

    let addr: SocketAddr = SSDP_ADDR.parse().unwrap();
    for msg in SEARCH_MSGS {
        let _ = sock.send_to(msg.as_bytes(), addr).await;
    }

    let mut locations: Vec<(String, String)> = Vec::new();
    let mut seen_ips: HashSet<String> = HashSet::new();
    let mut buf = [0u8; 4096];
    let deadline = tokio::time::Instant::now() + duration;

    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() { break; }
        match timeout(remaining, sock.recv_from(&mut buf)).await {
            Ok(Ok((len, src))) => {
                let ip = src.ip().to_string();
                let response = String::from_utf8_lossy(&buf[..len]);
                if let Some(loc) = extract_header(&response, "LOCATION") {
                    if seen_ips.insert(ip.clone()) {
                        locations.push((ip, loc));
                    }
                }
            }
            _ => break,
        }
    }

    let mut handles = Vec::new();
    for (ip, loc) in locations {
        handles.push(tokio::spawn(async move {
            identify_device(&ip, &loc).await
        }));
    }

    let mut devices = Vec::new();
    for h in handles {
        if let Ok(Some(dev)) = h.await {
            devices.push(dev);
        }
    }

    devices
}

/// Try each TLS mode in `PROBE_MODES` order; return the first `DiscoveredDevice`
/// whose API responds.  Falls back to the SSDP UPnP description if no API mode works.
async fn identify_device(ip: &str, location: &str) -> Option<DiscoveredDevice> {
    for &mode in PROBE_MODES {
        if let Some((name, uuid)) = probe_api(ip, mode).await {
            return Some(DiscoveredDevice { ip: ip.to_string(), name, uuid, tls_mode: mode });
        }
    }

    // API probes all failed.  Try the SSDP UPnP description URL as a last resort —
    // it at least confirms this is a WiiM/LinkPlay device so we can surface it in the
    // UI, even if we don't yet know the right protocol.
    let fallback_client = build_reqwest_client(TlsMode::Http, Duration::from_secs(2));
    if let Ok(resp) = fallback_client.get(location).send().await {
        if let Ok(xml) = resp.text().await {
            let lower = xml.to_lowercase();
            if lower.contains("wiim") || lower.contains("linkplay") || lower.contains("wiimu") {
                let name = extract_xml_tag(&xml, "friendlyName")
                    .unwrap_or_else(|| format!("WiiM @ {ip}"));
                return Some(DiscoveredDevice {
                    ip:       ip.to_string(),
                    name,
                    uuid:     String::new(),
                    tls_mode: TlsMode::HttpsWiiM,
                });
            }
        }
    }

    None
}

/// Try the WiiM API (`getStatusEx`) with a single TLS mode.
/// Returns `(name, uuid)` on success, or `None` on any error or non-WiiM response.
async fn probe_api(ip: &str, mode: TlsMode) -> Option<(String, String)> {
    let client = build_reqwest_client(mode, Duration::from_secs(2));
    let url = format!("{}?command=getStatusEx", api_base_url(ip, mode));
    let resp = match client.get(&url).send().await {
        Ok(r) => r,
        Err(e) => {
            crate::api::log_request_error(
                &format!("probe {ip} [{}]", mode.description()),
                &e,
            );
            return None;
        }
    };
    let text = resp.text().await.ok()?;
    if text.contains("uuid") && text.contains("DeviceName") {
        let val  = serde_json::from_str::<serde_json::Value>(&text).ok()?;
        let name = val["DeviceName"].as_str().map(String::from)
            .unwrap_or_else(|| format!("WiiM @ {ip}"));
        let uuid = val["uuid"].as_str().map(String::from).unwrap_or_default();
        Some((name, uuid))
    } else {
        None
    }
}

fn extract_xml_tag(xml: &str, tag: &str) -> Option<String> {
    let open  = format!("<{tag}>");
    let close = format!("</{tag}>");
    let start = xml.find(&open)? + open.len();
    let end   = xml[start..].find(&close)? + start;
    let val   = xml[start..end].trim().to_string();
    if val.is_empty() { None } else { Some(val) }
}

fn extract_header(response: &str, header: &str) -> Option<String> {
    let upper = header.to_ascii_uppercase();
    for line in response.lines() {
        if let Some((key, rest)) = line.split_once(':') {
            if key.trim().to_ascii_uppercase() == upper {
                let val = rest.trim().to_string();
                if !val.is_empty() { return Some(val); }
            }
        }
    }
    None
}
