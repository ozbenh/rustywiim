use std::collections::HashSet;
use std::net::SocketAddr;
use std::time::Duration;
use tokio::net::UdpSocket;
use tokio::time::timeout;

#[derive(Debug, Clone)]
pub struct DiscoveredDevice {
    pub ip: String,
    pub name: String,
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
        if remaining.is_zero() {
            break;
        }
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

    let http = reqwest::Client::builder()
        .danger_accept_invalid_certs(true)
        .timeout(Duration::from_secs(3))
        .build()
        .unwrap();

    let mut handles = Vec::new();
    for (ip, loc) in locations {
        let http = http.clone();
        handles.push(tokio::spawn(async move {
            identify_device(&http, &ip, &loc).await
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

async fn identify_device(
    http: &reqwest::Client,
    ip: &str,
    location: &str,
) -> Option<DiscoveredDevice> {
    if let Ok(resp) = http.get(location).send().await {
        if let Ok(xml) = resp.text().await {
            let lower = xml.to_lowercase();
            if lower.contains("wiim") || lower.contains("linkplay") || lower.contains("wiimu") {
                let name = extract_xml_tag(&xml, "friendlyName")
                    .unwrap_or_else(|| format!("WiiM @ {ip}"));
                return Some(DiscoveredDevice { ip: ip.to_string(), name });
            }
        }
    }

    let api_url = format!("https://{ip}/httpapi.asp?command=getStatusEx");
    if let Ok(resp) = http.get(&api_url).send().await {
        if let Ok(text) = resp.text().await {
            if text.contains("uuid") && text.contains("DeviceName") {
                let name = serde_json::from_str::<serde_json::Value>(&text)
                    .ok()
                    .and_then(|v| v["DeviceName"].as_str().map(String::from))
                    .unwrap_or_else(|| format!("WiiM @ {ip}"));
                return Some(DiscoveredDevice { ip: ip.to_string(), name });
            }
        }
    }

    None
}

fn extract_xml_tag(xml: &str, tag: &str) -> Option<String> {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    let start = xml.find(&open)? + open.len();
    let end = xml[start..].find(&close)? + start;
    let val = xml[start..end].trim().to_string();
    if val.is_empty() { None } else { Some(val) }
}

fn extract_header(response: &str, header: &str) -> Option<String> {
    let upper = header.to_ascii_uppercase();
    for line in response.lines() {
        if let Some((key, rest)) = line.split_once(':') {
            if key.trim().to_ascii_uppercase() == upper {
                let val = rest.trim().to_string();
                if !val.is_empty() {
                    return Some(val);
                }
            }
        }
    }
    None
}
