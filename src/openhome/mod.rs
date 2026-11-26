pub mod monitor;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use openhome_client::{
    ClientError, DiscoveryOptions, HttpClient, InfoClient, OpenHomeDevice, PlaylistClient,
    ProductClient, TimeClient, VolumeClient, discover,
};
use openhome_core::{SourceType, TransportState};
use url::Url;

use crate::upnp::{
    Controller, MediaItem, QueueController, RendererInfo, Result, TransportStatus, UpnpError,
    build_didl, local_ip_for_host, validate_renderer_url,
};

pub use monitor::{OpenHomeMonitor, OpenHomeMonitorCallbacks};

const OPENHOME_HTTP_TIMEOUT: Duration = Duration::from_secs(5);
const OPENHOME_PROBE_TIMEOUT: Duration = Duration::from_secs(3);

/// Native OpenHome Media renderer (`urn:av-openhome-org:device:Source:1`)
/// backed by `openhome-client`. Implements both `Controller` and
/// `QueueController` via the on-device `Playlist` service.
pub struct OpenHomeRenderer {
    info: RendererInfo,
    local_ip: String,
    product: Option<ProductClient>,
    playlist: PlaylistClient,
    info_svc: Option<InfoClient>,
    time: Option<TimeClient>,
    volume: Option<VolumeClient>,
    volume_max: u32,
    playlist_source_index: Option<u32>,
    /// Ordered OpenHome `Playlist` track IDs mirroring our queue indices.
    track_ids: Mutex<Vec<u32>>,
}

struct DeviceProbe {
    name: String,
    playlist_source_index: Option<u32>,
    sink_protocols: Vec<String>,
    volume_max: u32,
    initial_ids: Vec<u32>,
}

async fn probe_device(
    product: Option<&ProductClient>,
    playlist: &PlaylistClient,
    volume: Option<&VolumeClient>,
    friendly_name: &str,
    host: &str,
) -> DeviceProbe {
    let (product_meta, sources_res, proto_res, vol_char_res, initial_ids) = tokio::join!(
        async {
            match product {
                Some(p) => tokio::time::timeout(OPENHOME_PROBE_TIMEOUT, p.product())
                    .await
                    .ok()
                    .and_then(|r| r.ok()),
                None => None,
            }
        },
        async {
            match product {
                Some(p) => tokio::time::timeout(OPENHOME_PROBE_TIMEOUT, p.source_xml())
                    .await
                    .ok()
                    .and_then(|r| r.ok()),
                None => None,
            }
        },
        async {
            tokio::time::timeout(OPENHOME_PROBE_TIMEOUT, playlist.protocol_info())
                .await
                .ok()
                .and_then(|r| r.ok())
        },
        async {
            match volume {
                Some(v) => tokio::time::timeout(OPENHOME_PROBE_TIMEOUT, v.characteristics())
                    .await
                    .ok()
                    .and_then(|r| r.ok()),
                None => None,
            }
        },
        async {
            tokio::time::timeout(OPENHOME_PROBE_TIMEOUT, playlist.id_array())
                .await
                .ok()
                .and_then(|r| r.ok())
                .map(|(_, ids)| ids)
                .unwrap_or_default()
        },
    );

    let name = choose_display_name(
        product_meta.as_ref().map(|p| p.room.as_str()),
        product_meta.as_ref().map(|p| p.name.as_str()),
        friendly_name,
        host,
    );
    let playlist_source_index = sources_res.as_ref().and_then(|sources| {
        sources
            .iter()
            .position(|s| s.source_type == SourceType::Playlist)
            .map(|i| i as u32)
    });
    let sink_protocols = proto_res
        .map(|raw| parse_protocol_info(&raw))
        .unwrap_or_default();
    let volume_max = vol_char_res
        .map(|c| c.volume_max)
        .filter(|&m| m > 0)
        .unwrap_or(100);

    DeviceProbe {
        name,
        playlist_source_index,
        sink_protocols,
        volume_max,
        initial_ids,
    }
}

impl OpenHomeRenderer {
    /// Connect to an OpenHome device's `description.xml` URL after validating
    /// it against the SSRF/LAN guard (pinning the resolved IP to prevent DNS rebinding)
    /// and verifying all service control URLs stay on the same origin.
    pub async fn connect(location: &str) -> Result<Arc<Self>> {
        let pinned = validate_renderer_url(location).await?;
        let http = HttpClient::new(OPENHOME_HTTP_TIMEOUT);
        let device = OpenHomeDevice::connect_with_http(pinned.as_str(), http)
            .await
            .map_err(map_client_err)?;
        validate_service_control_urls(&pinned, device.description())?;
        Self::from_device(device).await
    }

    /// Connect without SSRF IP filtering (used by localhost unit tests).
    #[cfg(test)]
    pub(crate) async fn connect_unchecked(location: &str) -> Result<Arc<Self>> {
        let http = HttpClient::new(OPENHOME_HTTP_TIMEOUT);
        let device = OpenHomeDevice::connect_with_http(location, http)
            .await
            .map_err(map_client_err)?;
        Self::from_device(device).await
    }

    /// Build an `OpenHomeRenderer` from an already-fetched `OpenHomeDevice`.
    pub async fn from_device(device: OpenHomeDevice) -> Result<Arc<Self>> {
        if !device.is_openhome_device() {
            return Err(UpnpError::ActionUnsupported);
        }

        let playlist = device.playlist().map_err(map_client_err)?;
        let product = device.product().ok();
        let info_svc = device.info().ok();
        let time = device.time().ok();
        let volume = device.volume().ok();

        let location = device.location().to_string();
        let host = extract_host(&location);
        let local_ip = local_ip_for_host(&host);
        let desc = device.description();
        let udn = normalize_udn(&desc.udn, &location);
        let probe = probe_device(
            product.as_ref(),
            &playlist,
            volume.as_ref(),
            &desc.friendly_name,
            &host,
        )
        .await;

        let info = RendererInfo {
            id: format!("openhome:{udn}"),
            kind: "openhome".into(),
            name: probe.name,
            manufacturer: desc.manufacturer.clone(),
            model: desc.model_name.clone(),
            location,
            host,
            has_volume: volume.is_some(),
            supports_set_next: false,
            supports_seek: true,
            supports_events: false,
            supports_queue: true,
            sink_protocols: probe.sink_protocols,
        };

        Ok(Arc::new(Self {
            info,
            local_ip,
            product,
            playlist,
            info_svc,
            time,
            volume,
            volume_max: probe.volume_max,
            playlist_source_index: probe.playlist_source_index,
            track_ids: Mutex::new(probe.initial_ids),
        }))
    }

    /// Wake the renderer from standby (if needed) and ensure the `Playlist`
    /// source is selected before starting playback.
    /// Refresh local `track_ids` cache from the device's `Playlist::IdArray`.
    async fn resync_track_ids(&self) {
        if let Ok((_, ids)) = self.playlist.id_array().await {
            *self.track_ids.lock().unwrap_or_else(|e| e.into_inner()) = ids;
        }
    }

    async fn ensure_playlist_active(&self) {
        let Some(ref product) = self.product else {
            return;
        };
        if let Ok(true) = product.standby().await {
            let _ = product.set_standby(false).await;
        }
        if let Some(target_idx) = self.playlist_source_index
            && product
                .source_index()
                .await
                .map(|cur| cur != target_idx)
                .unwrap_or(true)
        {
            let _ = product.set_source_index(target_idx).await;
        }
    }

    async fn resolve_queue_index(&self, current_id: u32) -> Option<i32> {
        if current_id == 0 {
            return None;
        }
        if let Some(pos) = self
            .track_ids
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .position(|&id| id == current_id)
        {
            return Some(pos as i32);
        }
        // Cache miss (e.g., external OpenHome control point modified the playlist):
        // refresh the ID array once from the device.
        if let Ok((_, ids)) = self.playlist.id_array().await {
            let pos = ids
                .iter()
                .position(|&id| id == current_id)
                .map(|p| p as i32);
            *self.track_ids.lock().unwrap_or_else(|e| e.into_inner()) = ids;
            return pos;
        }
        None
    }
}

#[async_trait]
impl Controller for OpenHomeRenderer {
    fn info(&self) -> RendererInfo {
        self.info.clone()
    }

    fn local_ip(&self) -> String {
        self.local_ip.clone()
    }

    async fn set_uri(&self, item: &MediaItem) -> Result<()> {
        self.replace_queue(std::slice::from_ref(item), 0).await
    }

    async fn set_next_uri(&self, _item: &MediaItem) -> Result<()> {
        Err(UpnpError::ActionUnsupported)
    }

    async fn play(&self) -> Result<()> {
        self.ensure_playlist_active().await;
        self.playlist.play().await.map_err(map_client_err)
    }

    async fn pause(&self) -> Result<()> {
        self.playlist.pause().await.map_err(map_client_err)
    }

    async fn stop(&self) -> Result<()> {
        self.playlist.stop().await.map_err(map_client_err)
    }

    async fn seek(&self, pos: Duration) -> Result<()> {
        let secs = pos.as_secs().min(u32::MAX as u64) as u32;
        self.playlist
            .seek_second_absolute(secs)
            .await
            .map_err(map_client_err)
    }

    async fn status(&self) -> Result<TransportStatus> {
        let (ts_res, id_res, time_res, track_res, vol_res, mute_res) = tokio::join!(
            self.playlist.transport_state(),
            self.playlist.id(),
            async {
                match &self.time {
                    Some(t) => Some(t.time().await),
                    None => None,
                }
            },
            async {
                match &self.info_svc {
                    Some(i) => Some(i.track().await),
                    None => None,
                }
            },
            async {
                match &self.volume {
                    Some(v) => Some(v.volume().await),
                    None => None,
                }
            },
            async {
                match &self.volume {
                    Some(v) => Some(v.mute().await),
                    None => None,
                }
            },
        );

        let any_ok = ts_res.is_ok()
            || id_res.is_ok()
            || matches!(time_res, Some(Ok(_)))
            || matches!(track_res, Some(Ok(_)))
            || matches!(vol_res, Some(Ok(_)))
            || matches!(mute_res, Some(Ok(_)));
        if !any_ok {
            return Err(map_client_err(ts_res.unwrap_err()));
        }

        let state = match ts_res {
            Ok(ts) => map_transport_state(ts),
            Err(_) => "UNKNOWN",
        };
        let mut st = TransportStatus {
            state: state.into(),
            has_volume: self.volume.is_some(),
            ..Default::default()
        };

        if let Ok(cur_id) = id_res
            && let Some(idx) = self.resolve_queue_index(cur_id).await
        {
            st.queue_index = idx;
            st.has_queue_index = true;
        }

        if let Some(Ok(t)) = time_res {
            st.position = Duration::from_secs(u64::from(t.seconds));
            st.duration = Duration::from_secs(u64::from(t.duration));
            st.has_position = true;
        }

        if let Some(Ok((uri, _meta))) = track_res {
            st.track_uri = uri;
        }

        if let Some(Ok(raw_vol)) = vol_res {
            st.volume = volume_from_device(raw_vol, self.volume_max);
            st.has_vol_value = true;
        }

        if let Some(Ok(muted)) = mute_res {
            st.muted = muted;
            st.has_mute_val = true;
        }

        Ok(st)
    }

    async fn set_volume(&self, vol: i32) -> Result<()> {
        let Some(ref v) = self.volume else {
            return Err(UpnpError::ActionUnsupported);
        };
        let scaled = volume_to_device(vol, self.volume_max);
        v.set_volume(scaled).await.map_err(map_client_err)
    }

    async fn set_mute(&self, muted: bool) -> Result<()> {
        let Some(ref v) = self.volume else {
            return Err(UpnpError::ActionUnsupported);
        };
        v.set_mute(muted).await.map_err(map_client_err)
    }

    fn as_queue_controller(&self) -> Option<&dyn QueueController> {
        Some(self)
    }
}

#[async_trait]
impl QueueController for OpenHomeRenderer {
    async fn replace_queue(&self, items: &[MediaItem], start_index: usize) -> Result<()> {
        self.ensure_playlist_active().await;
        self.playlist.delete_all().await.map_err(map_client_err)?;

        let mut new_ids = Vec::with_capacity(items.len());
        let mut after_id = 0u32;
        for item in items {
            let didl = build_didl(item, &self.info.sink_protocols);
            match self.playlist.insert(after_id, &item.url, &didl).await {
                Ok(id) => {
                    new_ids.push(id);
                    after_id = id;
                }
                Err(e) => {
                    self.resync_track_ids().await;
                    return Err(map_client_err(e));
                }
            }
        }

        let target_id = new_ids
            .get(start_index)
            .or_else(|| new_ids.first())
            .copied();
        *self.track_ids.lock().unwrap_or_else(|e| e.into_inner()) = new_ids;

        if let Some(tid) = target_id {
            self.playlist.seek_id(tid).await.map_err(map_client_err)?;
            self.playlist.play().await.map_err(map_client_err)?;
        }
        Ok(())
    }

    async fn append_queue(&self, items: &[MediaItem]) -> Result<()> {
        if items.is_empty() {
            return Ok(());
        }
        let mut after_id = self
            .track_ids
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .last()
            .copied()
            .unwrap_or(0);
        let mut added = Vec::with_capacity(items.len());
        for item in items {
            let didl = build_didl(item, &self.info.sink_protocols);
            match self.playlist.insert(after_id, &item.url, &didl).await {
                Ok(id) => {
                    added.push(id);
                    after_id = id;
                }
                Err(e) => {
                    self.resync_track_ids().await;
                    return Err(map_client_err(e));
                }
            }
        }
        self.track_ids
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .extend(added);
        Ok(())
    }

    async fn remove_queue_range(&self, start: usize, end: usize) -> Result<()> {
        let to_delete: Vec<u32> = {
            let ids = self.track_ids.lock().unwrap_or_else(|e| e.into_inner());
            if ids.is_empty() || start > end || start >= ids.len() {
                return Ok(());
            }
            let clamped_end = end.min(ids.len() - 1);
            ids[start..=clamped_end].to_vec()
        };
        for id in &to_delete {
            if let Err(e) = self.playlist.delete_id(*id).await {
                self.resync_track_ids().await;
                return Err(map_client_err(e));
            }
        }
        let mut ids = self.track_ids.lock().unwrap_or_else(|e| e.into_inner());
        ids.retain(|id| !to_delete.contains(id));
        Ok(())
    }

    async fn move_queue_item(&self, from: usize, to: usize) -> Result<()> {
        if from == to {
            return Ok(());
        }
        let (moving_id, prev_id, after_id) = {
            let ids = self.track_ids.lock().unwrap_or_else(|e| e.into_inner());
            if from >= ids.len() || to >= ids.len() {
                return Ok(());
            }
            let moving_id = ids[from];
            let prev_id = if from == 0 { 0 } else { ids[from - 1] };
            let mut simulated = ids.clone();
            simulated.remove(from);
            let after_id = if to == 0 { 0 } else { simulated[to - 1] };
            (moving_id, prev_id, after_id)
        };

        let entry = self
            .playlist
            .read(moving_id)
            .await
            .map_err(map_client_err)?;
        self.playlist
            .delete_id(moving_id)
            .await
            .map_err(map_client_err)?;
        let new_id = match self
            .playlist
            .insert(after_id, &entry.uri, &entry.metadata)
            .await
        {
            Ok(id) => id,
            Err(e) => {
                // Best-effort rollback so the moved track isn't silently lost.
                let _ = self
                    .playlist
                    .insert(prev_id, &entry.uri, &entry.metadata)
                    .await;
                self.resync_track_ids().await;
                return Err(map_client_err(e));
            }
        };

        let mut ids = self.track_ids.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(pos) = ids.iter().position(|&id| id == moving_id) {
            ids.remove(pos);
        }
        let insert_at = to.min(ids.len());
        ids.insert(insert_at, new_id);
        Ok(())
    }

    async fn play_index(&self, index: usize) -> Result<()> {
        self.ensure_playlist_active().await;
        let maybe_id = self
            .track_ids
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(index)
            .copied();
        if let Some(tid) = maybe_id {
            self.playlist.seek_id(tid).await.map_err(map_client_err)?;
        } else {
            self.playlist
                .seek_index(index as u32)
                .await
                .map_err(map_client_err)?;
        }
        self.playlist.play().await.map_err(map_client_err)
    }

    async fn next(&self) -> Result<()> {
        self.playlist.next().await.map_err(map_client_err)
    }

    async fn previous(&self) -> Result<()> {
        self.playlist.previous().await.map_err(map_client_err)
    }

    async fn set_loop_mode(&self, repeat: &str, shuffle: bool) -> Result<()> {
        // OpenHome Playlist:1 supports boolean Repeat (queue-level repeat) and Shuffle.
        // Repeat-one is handled by Tacet's queue manager, so only "all" enables device repeat.
        let repeat_all = repeat.eq_ignore_ascii_case("all");
        self.playlist
            .set_repeat(repeat_all)
            .await
            .map_err(map_client_err)?;
        self.playlist
            .set_shuffle(shuffle)
            .await
            .map_err(map_client_err)?;
        Ok(())
    }
}

/// Connect to an OpenHome renderer by `description.xml` URL.
pub async fn connect_by_url(location: &str) -> Result<Arc<OpenHomeRenderer>> {
    OpenHomeRenderer::connect(location).await
}

/// Try connecting to a renderer URL as OpenHome.
/// Returns `Ok(None)` only when the device is reachable and valid UPnP XML
/// but does not expose OpenHome Product + Playlist services (so callers can
/// cleanly fall back to plain UPnP without masking SSRF/network/auth errors).
pub async fn try_connect_by_url(location: &str) -> Result<Option<Arc<OpenHomeRenderer>>> {
    let pinned = validate_renderer_url(location).await?;
    let http = HttpClient::new(OPENHOME_HTTP_TIMEOUT);
    let device = OpenHomeDevice::connect_with_http(pinned.as_str(), http)
        .await
        .map_err(map_client_err)?;
    validate_service_control_urls(&pinned, device.description())?;
    if !device.is_openhome_device() || device.playlist().is_err() {
        return Ok(None);
    }
    Ok(Some(OpenHomeRenderer::from_device(device).await?))
}

/// Ensure every service control URL declared in `description.xml` resolves to
/// the same scheme, host, and port as the validated/pinned description URL.
fn validate_service_control_urls(
    base: &url::Url,
    desc: &openhome_core::DeviceDescription,
) -> Result<()> {
    for svc in &desc.services {
        let resolved = base
            .join(&svc.control_url)
            .map_err(|e| UpnpError::Other(format!("invalid service control URL: {e}")))?;
        if resolved.scheme() != base.scheme()
            || resolved.host() != base.host()
            || resolved.port_or_known_default() != base.port_or_known_default()
        {
            return Err(UpnpError::Other(format!(
                "refusing cross-origin service control URL: {}",
                svc.control_url
            )));
        }
    }
    Ok(())
}

/// Perform an SSDP discovery sweep for OpenHome Media renderers on the LAN.
pub async fn discover_openhome(timeout: Duration) -> Vec<Arc<OpenHomeRenderer>> {
    let opts = DiscoveryOptions {
        timeout,
        openhome_only: true,
        http: HttpClient::new(OPENHOME_HTTP_TIMEOUT),
        ..Default::default()
    };
    let Ok(discovered) = discover(opts).await else {
        return Vec::new();
    };

    let mut out = Vec::new();
    for dev in discovered {
        if let Some(renderer) = connect_discovered(dev).await {
            out.push(renderer);
        }
    }
    out
}

async fn connect_discovered(
    dev: openhome_client::DiscoveredDevice,
) -> Option<Arc<OpenHomeRenderer>> {
    OpenHomeRenderer::connect(&dev.location).await.ok()
}

fn map_client_err(err: ClientError) -> UpnpError {
    match err {
        ClientError::MissingService(_) => UpnpError::ActionUnsupported,
        ClientError::Fault(fault) => {
            if fault.code == 401 || fault.code == 602 {
                UpnpError::ActionUnsupported
            } else {
                UpnpError::SoapFault {
                    code: fault.code as i32,
                    desc: fault.description,
                }
            }
        }
        other => UpnpError::Other(other.to_string()),
    }
}

fn map_transport_state(state: TransportState) -> &'static str {
    match state {
        TransportState::Playing => "PLAYING",
        TransportState::Paused => "PAUSED_PLAYBACK",
        TransportState::Buffering => "TRANSITIONING",
        TransportState::Stopped => "STOPPED",
    }
}

fn volume_to_device(percent: i32, volume_max: u32) -> u32 {
    let clamped = percent.clamp(0, 100) as f64;
    let max = f64::from(volume_max.max(1));
    ((clamped / 100.0) * max).round() as u32
}

fn volume_from_device(raw: u32, volume_max: u32) -> i32 {
    let max = f64::from(volume_max.max(1));
    let pct = (f64::from(raw.min(volume_max)) / max) * 100.0;
    (pct.round() as i32).clamp(0, 100)
}

fn extract_host(location: &str) -> String {
    Url::parse(location)
        .ok()
        .and_then(|u| u.host_str().map(ToString::to_string))
        .unwrap_or_default()
}

fn normalize_udn(udn: &str, fallback: &str) -> String {
    let trimmed = udn.trim();
    let stripped = trimmed.strip_prefix("uuid:").unwrap_or(trimmed);
    if !stripped.is_empty() {
        stripped.to_string()
    } else {
        fallback.trim().to_string()
    }
}

fn choose_display_name(
    room: Option<&str>,
    product_name: Option<&str>,
    friendly_name: &str,
    host: &str,
) -> String {
    let room = room.map(str::trim).unwrap_or("");
    let product = product_name.map(str::trim).unwrap_or("");
    if !room.is_empty() && !product.is_empty() && !room.eq_ignore_ascii_case(product) {
        return format!("{room} ({product})");
    }
    if !room.is_empty() {
        return room.to_string();
    }
    if !friendly_name.trim().is_empty() {
        return friendly_name.trim().to_string();
    }
    if !product.is_empty() {
        return product.to_string();
    }
    host.to_string()
}

fn parse_protocol_info(raw: &str) -> Vec<String> {
    raw.split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(ToString::to_string)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use openhome_server::{MemoryRenderer, OpenHomeServer, SsdpConfig};
    use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};

    #[test]
    fn volume_scaling_roundtrips_cleanly() {
        assert_eq!(volume_to_device(0, 80), 0);
        assert_eq!(volume_to_device(50, 80), 40);
        assert_eq!(volume_to_device(100, 80), 80);
        assert_eq!(volume_to_device(150, 80), 80);
        assert_eq!(volume_from_device(40, 80), 50);
        assert_eq!(volume_from_device(80, 80), 100);
        assert_eq!(volume_from_device(99, 80), 100);
    }

    #[test]
    fn display_name_prefers_room_and_product() {
        assert_eq!(
            choose_display_name(Some("Living Room"), Some("Linn DSM"), "Fallback", "1.2.3.4"),
            "Living Room (Linn DSM)"
        );
        assert_eq!(
            choose_display_name(Some("Office"), Some("Office"), "Fallback", "1.2.3.4"),
            "Office"
        );
        assert_eq!(
            choose_display_name(None, None, "Friendly", "1.2.3.4"),
            "Friendly"
        );
    }

    #[tokio::test]
    async fn openhome_renderer_end_to_end_against_memory_server() {
        let mem = Arc::new(MemoryRenderer::new("Studio", "TacetMock"));
        let server = OpenHomeServer::builder(Arc::clone(&mem) as _)
            .udn("uuid:11111111-2222-3333-4444-555555555555")
            .friendly_name("Test OpenHome")
            .playlist(Arc::clone(&mem) as _)
            .info(Arc::clone(&mem) as _)
            .time(Arc::clone(&mem) as _)
            .volume(Arc::clone(&mem) as _)
            .bind_addr(SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0)))
            .ssdp(SsdpConfig::disabled())
            .start()
            .await
            .expect("start in-process OpenHomeServer");

        let loc = server.description_url().to_string();
        let renderer = OpenHomeRenderer::connect_unchecked(&loc)
            .await
            .expect("connect OpenHomeRenderer");

        let info = renderer.info();
        assert_eq!(info.kind, "openhome");
        assert_eq!(info.id, "openhome:11111111-2222-3333-4444-555555555555");
        assert_eq!(info.name, "Studio (TacetMock)");
        assert!(info.supports_queue);
        assert!(info.has_volume);

        let mk = |id: &str, title: &str| MediaItem {
            id: id.into(),
            url: format!("http://127.0.0.1:4320/media/local/{id}.flac"),
            title: title.into(),
            artist: "Miles Davis".into(),
            album: "Kind of Blue".into(),
            mime: "audio/flac".into(),
            duration: Duration::from_secs(180),
            seekable: true,
            ..Default::default()
        };

        let qc = renderer
            .as_queue_controller()
            .expect("exposes QueueController");
        qc.replace_queue(&[mk("t1", "So What"), mk("t2", "Freddie Freeloader")], 0)
            .await
            .expect("replace_queue");

        let st = renderer.status().await.expect("status");
        assert_eq!(st.state, "PLAYING");
        assert!(st.has_queue_index);
        assert_eq!(st.queue_index, 0);

        qc.append_queue(&[mk("t3", "Blue in Green")])
            .await
            .expect("append_queue");
        qc.play_index(2).await.expect("play_index");
        let st = renderer.status().await.expect("status after play_index");
        assert_eq!(st.queue_index, 2);

        qc.move_queue_item(2, 0).await.expect("move_queue_item");
        qc.remove_queue_range(1, 1)
            .await
            .expect("remove_queue_range");
        assert_eq!(renderer.track_ids.lock().unwrap().len(), 2);

        // Regression: callers like radio-tail sync pass end == len; must clamp to len - 1.
        qc.remove_queue_range(1, 2)
            .await
            .expect("remove_queue_range clamps end == len");
        assert_eq!(renderer.track_ids.lock().unwrap().len(), 1);

        renderer.set_volume(42).await.expect("set_volume");
        renderer.set_mute(true).await.expect("set_mute");
        let st = renderer.status().await.expect("status after volume");
        assert_eq!(st.volume, 42);
        assert!(st.muted);

        qc.set_loop_mode("all", true).await.expect("set_loop_mode");
        renderer.pause().await.expect("pause");
        assert_eq!(renderer.status().await.unwrap().state, "PAUSED_PLAYBACK");
        renderer.stop().await.expect("stop");
        assert_eq!(renderer.status().await.unwrap().state, "STOPPED");

        server.shutdown().await;
    }
}
