/// Low-level UPnP SOAP request/response plumbing — the UPnP analogue of
/// `api.rs`. Owns wire-shaped (not canonical) response types and the SOAP
/// calls that produce them; `state.rs` decides *when* to call it, exactly
/// like it already decides when to call `WiimClient` methods.
/// `device/playback.rs` owns turning these into canonical `PlaybackState`
/// fields, not this module.
///
/// Skeleton only for now — see `/PLAYBACKSTATE.md`'s "New module:
/// src/device/upnp.rs" section. Nothing in `state.rs` calls this yet;
/// `AccessMethod::UpnpPolled` is accepted by the config/UI plumbing but has
/// no real fetch path until this module is built out against a concrete
/// need (the artwork-source experiment is the most likely first consumer).

#[derive(Debug, Clone, Default)]
pub struct TransportInfo {
    pub current_transport_state: String,
}

#[derive(Debug, Clone, Default)]
pub struct PositionInfo {
    pub rel_time:         String,
    pub track_duration:   String,
}

#[derive(Debug, Clone, Default)]
pub struct InfoEx {
    pub track_metadata: String, // raw DIDL-Lite XML, not yet parsed
    pub track_source:   String,
    pub playback_storage_medium: String,
}

/// UPnP control-point client for one device's `AVTransport`/
/// `RenderingControl` services. Control URLs would come from the device's
/// UPnP description XML (`discovery.rs` already fetches SSDP location; it
/// does not yet fetch/parse `description.xml` for control URLs — that's
/// part of building this out for real).
#[derive(Debug, Clone)]
pub struct UpnpClient {
    #[allow(dead_code)]
    av_transport_control_url: String,
    #[allow(dead_code)]
    rendering_control_url: String,
}

impl UpnpClient {
    pub fn new(av_transport_control_url: String, rendering_control_url: String) -> Self {
        Self { av_transport_control_url, rendering_control_url }
    }

    pub async fn get_transport_info(&self) -> anyhow::Result<TransportInfo> {
        anyhow::bail!("UpnpClient::get_transport_info not implemented yet")
    }

    pub async fn get_position_info(&self) -> anyhow::Result<PositionInfo> {
        anyhow::bail!("UpnpClient::get_position_info not implemented yet")
    }

    pub async fn get_info_ex(&self) -> anyhow::Result<InfoEx> {
        anyhow::bail!("UpnpClient::get_info_ex not implemented yet")
    }
}
