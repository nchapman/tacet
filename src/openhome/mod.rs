pub mod monitor;

use std::collections::HashSet;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use futures_util::stream::{self, StreamExt};
use openhome_client::{
    ClientError, HttpClient, InfoClient, OpenHomeDevice, PlaylistClient, ProductClient, TimeClient,
    VolumeClient,
};
use openhome_core::{DeviceDescription, SourceType, TransportState};
use url::Url;

use crate::upnp::discovery::msearch_locations;
use crate::upnp::{
    Controller, MediaItem, QueueController, RendererInfo, Result, TransportStatus, UpnpError,
    build_didl, local_ip_for_host, validate_renderer_url,
};

pub use monitor::{OpenHomeMonitor, OpenHomeMonitorCallbacks};

const OPENHOME_HTTP_TIMEOUT: Duration = Duration::from_secs(5);
const OPENHOME_PROBE_TIMEOUT: Duration = Duration::from_secs(3);
const DEFAULT_TRACKS_MAX: usize = 1000;
const SOAP_FAULT_ID_NOT_FOUND: u16 = 800;

const OPENHOME_SEARCH_TARGETS: &[&str] = &[
    "urn:av-openhome-org:service:Product:1",
    "urn:av-openhome-org:device:Source:1",
];

#[derive(Default)]
struct PlaylistCache {
    ids: Vec<u32>,
    token: u32,
}

#[derive(Default)]
struct LastTrackCache {
    id: u32,
    uri: String,
}

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
    tracks_max: usize,
    playlist_source_index: Option<u32>,
    /// Serializes `QueueController` mutations against `status()` ID-array refreshes.
    playlist_mu: tokio::sync::Mutex<()>,
    /// Ordered OpenHome `Playlist` track IDs + `IdArray` token mirroring queue indices.
    playlist_cache: Mutex<PlaylistCache>,
    /// Last resolved `(track_id, uri)` from `Info::Track()` to avoid redundant RPCs.
    last_track: Mutex<LastTrackCache>,
}

struct DeviceProbe {
    name: String,
    playlist_source_index: Option<u32>,
    sink_protocols: Vec<String>,
    volume_enabled: bool,
    volume_max: u32,
    tracks_max: usize,
    initial_cache: PlaylistCache,
}

async fn probe_device(
    product: Option<&ProductClient>,
    playlist: &PlaylistClient,
    volume: Option<&VolumeClient>,
    friendly_name: &str,
    host: &str,
) -> DeviceProbe {
    let (product_meta, sources_res, proto_res, vol_char_res, vol_limit_res, tracks_max_res, id_arr) = tokio::join!(
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
            match volume {
                Some(v) => tokio::time::timeout(OPENHOME_PROBE_TIMEOUT, v.volume_limit())
                    .await
                    .ok()
                    .and_then(|r| r.ok()),
                None => None,
            }
        },
        async {
            tokio::time::timeout(OPENHOME_PROBE_TIMEOUT, playlist.tracks_max())
                .await
                .ok()
                .and_then(|r| r.ok())
        },
        async {
            tokio::time::timeout(OPENHOME_PROBE_TIMEOUT, playlist.id_array())
                .await
                .ok()
                .and_then(|r| r.ok())
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
            .position(|s| s.visible && s.source_type == SourceType::Playlist)
            .or_else(|| {
                sources
                    .iter()
                    .position(|s| s.source_type == SourceType::Playlist)
            })
            .map(|i| i as u32)
    });
    let sink_protocols = proto_res
        .map(|s| parse_protocol_info(&s))
        .unwrap_or_default();

    let (volume_enabled, volume_max) = resolve_volume_bounds(
        volume.is_some(),
        vol_char_res.map(|c| c.volume_max),
        vol_limit_res,
    );

    let tracks_max = tracks_max_res
        .map(|m| m as usize)
        .filter(|&m| m > 0)
        .unwrap_or(DEFAULT_TRACKS_MAX);

    let initial_cache = match id_arr {
        Some((token, ids)) => PlaylistCache { ids, token },
        None => PlaylistCache::default(),
    };

    DeviceProbe {
        name,
        playlist_source_index,
        sink_protocols,
        volume_enabled,
        volume_max,
        tracks_max,
        initial_cache,
    }
}

fn resolve_volume_bounds(
    has_volume_svc: bool,
    char_max: Option<u32>,
    vol_limit: Option<u32>,
) -> (bool, u32) {
    if !has_volume_svc {
        return (false, 100);
    }
    // Fixed-output / preamp-bypass renderers advertise VolumeMax == 0.
    if char_max == Some(0) {
        return (false, 0);
    }
    let hw_max = char_max.unwrap_or(100);
    let effective = match vol_limit {
        Some(lim) if lim > 0 => hw_max.min(lim),
        _ => hw_max,
    };
    (true, effective.max(1))
}

impl OpenHomeRenderer {
    /// Connect to an OpenHome device's `description.xml` URL after validating
    /// it against the SSRF/LAN guard (pinning the resolved IP to prevent DNS rebinding)
    /// and verifying `<URLBase>` and all service control URLs stay on the same origin.
    pub async fn connect(location: &str) -> Result<Arc<Self>> {
        let pinned = validate_renderer_url(location).await?;
        if pinned.scheme() != "http" {
            return Err(UpnpError::ActionUnsupported);
        }
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
        let raw_volume = device.volume().ok();

        let location = device.location().to_string();
        let host = extract_host(&location);
        let local_ip = local_ip_for_host(&host);
        let desc = device.description();
        let udn = normalize_udn(&desc.udn, &location);
        let probe = probe_device(
            product.as_ref(),
            &playlist,
            raw_volume.as_ref(),
            &desc.friendly_name,
            &host,
        )
        .await;

        let volume = if probe.volume_enabled {
            raw_volume
        } else {
            None
        };

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
            tracks_max: probe.tracks_max,
            playlist_source_index: probe.playlist_source_index,
            playlist_mu: tokio::sync::Mutex::new(()),
            playlist_cache: Mutex::new(probe.initial_cache),
            last_track: Mutex::new(LastTrackCache::default()),
        }))
    }

    /// Refresh local `playlist_cache` from the device's `Playlist::IdArray`.
    async fn resync_playlist_cache(&self) {
        if let Ok((token, ids)) = self.playlist.id_array().await {
            *self
                .playlist_cache
                .lock()
                .unwrap_or_else(|e| e.into_inner()) = PlaylistCache { ids, token };
        }
    }

    async fn ensure_playlist_active(&self) {
        let Some(ref prod) = self.product else {
            return;
        };
        if let Ok(true) = prod.standby().await {
            let _ = prod.set_standby(false).await;
        }
        if let Some(target_idx) = self.playlist_source_index {
            match prod.source_index().await {
                Ok(cur) if cur == target_idx => {}
                _ => {
                    let _ = prod.set_source_index(target_idx).await;
                }
            }
        }
    }

    async fn resolve_queue_index(&self, current_id: u32) -> Option<usize> {
        if current_id == 0 {
            return None;
        }
        let _guard = self.playlist_mu.lock().await;
        let (cached_pos, cached_token) = {
            let cache = self
                .playlist_cache
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            (
                cache.ids.iter().position(|&id| id == current_id),
                cache.token,
            )
        };

        // Detect external control point mutations (Kazoo / Linn App) via IdArrayChanged.
        let needs_refresh = match cached_pos {
            None => true,
            Some(_) => matches!(self.playlist.id_array_changed(cached_token).await, Ok(true)),
        };
        if !needs_refresh {
            return cached_pos;
        }

        if let Ok((token, ids)) = self.playlist.id_array().await {
            let pos = ids.iter().position(|&id| id == current_id);
            *self
                .playlist_cache
                .lock()
                .unwrap_or_else(|e| e.into_inner()) = PlaylistCache { ids, token };
            return pos;
        }
        cached_pos
    }

    async fn fetch_track_uri_cached(&self, cur_id: u32) -> String {
        if cur_id == 0 {
            return String::new();
        }
        {
            let cached = self.last_track.lock().unwrap_or_else(|e| e.into_inner());
            if cached.id == cur_id && !cached.uri.is_empty() {
                return cached.uri.clone();
            }
        }
        let Some(ref info_svc) = self.info_svc else {
            return String::new();
        };
        if let Ok((uri, _meta)) = info_svc.track().await {
            *self.last_track.lock().unwrap_or_else(|e| e.into_inner()) = LastTrackCache {
                id: cur_id,
                uri: uri.clone(),
            };
            return uri;
        }
        String::new()
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

    fn qobuz_delivery(&self) -> &str {
        // Force Tacet HTTP relay so pre-loaded native playlist URLs never hit
        // Qobuz Direct ~1h signed-URL expiry or foreign-URI false-stops.
        "force"
    }

    async fn set_uri(&self, item: &MediaItem) -> Result<()> {
        let _guard = self.playlist_mu.lock().await;
        self.ensure_playlist_active().await;
        let _ = self.playlist.set_shuffle(false).await;
        self.playlist.delete_all().await.map_err(map_client_err)?;
        let didl = build_didl(item, &self.info.sink_protocols);
        let id = self
            .playlist
            .insert(0, &item.url, &didl)
            .await
            .map_err(map_client_err)?;
        self.resync_playlist_cache().await;
        self.playlist.seek_id(id).await.map_err(map_client_err)?;
        Ok(())
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
        let secs = pos.as_secs().min(u64::from(u32::MAX)) as u32;
        self.playlist
            .seek_second_absolute(secs)
            .await
            .map_err(map_client_err)
    }

    async fn status(&self) -> Result<TransportStatus> {
        // Stage 1 (max 2 concurrent sockets): transport state + active track ID.
        let (ts_res, id_res) = tokio::join!(self.playlist.transport_state(), self.playlist.id());
        let transport_state = ts_res.map_err(map_client_err)?;
        let is_stopped = transport_state == TransportState::Stopped;

        let mut st = TransportStatus {
            state: map_transport_state(transport_state).into(),
            has_volume: self.volume.is_some(),
            ..Default::default()
        };

        let cur_id = id_res.unwrap_or(0);
        if cur_id != 0
            && let Some(idx) = self.resolve_queue_index(cur_id).await
        {
            st.queue_index = idx as i32;
            st.has_queue_index = true;
        }

        // Stage 2 (max 2 concurrent sockets): time (only when active) + cached track URI.
        if !is_stopped {
            let (time_res, track_uri) = tokio::join!(
                async {
                    match &self.time {
                        Some(t) => Some(t.time().await),
                        None => None,
                    }
                },
                self.fetch_track_uri_cached(cur_id),
            );
            if let Some(Ok(t)) = time_res {
                st.position = Duration::from_secs(u64::from(t.seconds));
                st.duration = Duration::from_secs(u64::from(t.duration));
                st.has_position = true;
            }
            st.track_uri = track_uri;
        }

        // Stage 3 (max 2 concurrent sockets): volume + mute.
        if let Some(ref v) = self.volume {
            let (vol_res, mute_res) = tokio::join!(v.volume(), v.mute());
            if let Ok(raw_vol) = vol_res {
                st.volume = volume_from_device(raw_vol, self.volume_max);
                st.has_vol_value = true;
            }
            if let Ok(muted) = mute_res {
                st.muted = muted;
                st.has_mute_val = true;
            }
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
        let _guard = self.playlist_mu.lock().await;
        self.ensure_playlist_active().await;
        let _ = self.playlist.set_shuffle(false).await;
        self.playlist.delete_all().await.map_err(map_client_err)?;

        let capped_len = items.len().min(self.tracks_max);
        let items = &items[..capped_len];
        let start_idx = start_index.min(items.len().saturating_sub(1));

        let mut new_ids = Vec::with_capacity(items.len());
        let mut after_id = 0u32;

        // Insert up to start_index first and begin playback immediately for low-latency start.
        let head_end = if items.is_empty() { 0 } else { start_idx + 1 };
        for item in &items[..head_end] {
            let didl = build_didl(item, &self.info.sink_protocols);
            match self.playlist.insert(after_id, &item.url, &didl).await {
                Ok(id) => {
                    new_ids.push(id);
                    after_id = id;
                }
                Err(e) => {
                    self.resync_playlist_cache().await;
                    return Err(map_client_err(e));
                }
            }
        }

        if let Some(&target_id) = new_ids.get(start_idx) {
            self.playlist
                .seek_id(target_id)
                .await
                .map_err(map_client_err)?;
            // Per OpenHome Playlist:1 spec, SeekId already begins playback;
            // keep a best-effort Play() for non-compliant renderers without failing on buffering.
            let _ = self.playlist.play().await;
        }

        // Insert remaining tail tracks after playback has already started.
        for item in &items[head_end..] {
            let didl = build_didl(item, &self.info.sink_protocols);
            match self.playlist.insert(after_id, &item.url, &didl).await {
                Ok(id) => {
                    new_ids.push(id);
                    after_id = id;
                }
                Err(e) => {
                    self.resync_playlist_cache().await;
                    return Err(map_client_err(e));
                }
            }
        }

        self.resync_playlist_cache().await;
        Ok(())
    }

    async fn append_queue(&self, items: &[MediaItem]) -> Result<()> {
        if items.is_empty() {
            return Ok(());
        }
        let _guard = self.playlist_mu.lock().await;
        let (mut after_id, existing_len) = {
            let cache = self
                .playlist_cache
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            (cache.ids.last().copied().unwrap_or(0), cache.ids.len())
        };
        let headroom = self.tracks_max.saturating_sub(existing_len);
        if headroom == 0 {
            return Ok(());
        }
        for item in items.iter().take(headroom) {
            let didl = build_didl(item, &self.info.sink_protocols);
            match self.playlist.insert(after_id, &item.url, &didl).await {
                Ok(id) => after_id = id,
                Err(e) => {
                    self.resync_playlist_cache().await;
                    return Err(map_client_err(e));
                }
            }
        }
        self.resync_playlist_cache().await;
        Ok(())
    }

    async fn remove_queue_range(&self, start: usize, end: usize) -> Result<()> {
        let _guard = self.playlist_mu.lock().await;
        let (is_full_clear, to_delete) = {
            let cache = self
                .playlist_cache
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            if cache.ids.is_empty() || start > end || start >= cache.ids.len() {
                return Ok(());
            }
            let clamped_end = end.min(cache.ids.len() - 1);
            let full = start == 0 && clamped_end == cache.ids.len() - 1;
            (full, cache.ids[start..=clamped_end].to_vec())
        };

        if is_full_clear {
            self.playlist.delete_all().await.map_err(map_client_err)?;
            self.resync_playlist_cache().await;
            return Ok(());
        }

        for id in &to_delete {
            match self.playlist.delete_id(*id).await {
                Ok(()) => {}
                // Fault 800 = ID already removed by an external control point; continue cleanly.
                Err(ClientError::Fault(ref f)) if f.code == SOAP_FAULT_ID_NOT_FOUND => {}
                Err(e) => {
                    self.resync_playlist_cache().await;
                    return Err(map_client_err(e));
                }
            }
        }
        self.resync_playlist_cache().await;
        Ok(())
    }

    async fn move_queue_item(&self, from: usize, to: usize) -> Result<()> {
        if from == to {
            return Ok(());
        }
        let _guard = self.playlist_mu.lock().await;
        let (moving_id, prev_id, after_id) = {
            let cache = self
                .playlist_cache
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            if from >= cache.ids.len() || to >= cache.ids.len() {
                return Ok(());
            }
            let moving_id = cache.ids[from];
            let prev_id = if from == 0 { 0 } else { cache.ids[from - 1] };
            let mut simulated = cache.ids.clone();
            simulated.remove(from);
            let after_id = if to == 0 { 0 } else { simulated[to - 1] };
            (moving_id, prev_id, after_id)
        };

        // If moving the currently active track, capture playback position to restore after re-insert.
        let restore_pos = if self.playlist.id().await.ok() == Some(moving_id) {
            let playing = matches!(
                self.playlist.transport_state().await.ok(),
                Some(TransportState::Playing | TransportState::Buffering)
            );
            let secs = match &self.time {
                Some(t) => t.time().await.ok().map(|v| v.seconds).unwrap_or(0),
                None => 0,
            };
            Some((playing, secs))
        } else {
            None
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
                let _ = self
                    .playlist
                    .insert(prev_id, &entry.uri, &entry.metadata)
                    .await;
                self.resync_playlist_cache().await;
                return Err(map_client_err(e));
            }
        };

        if let Some((was_playing, secs)) = restore_pos {
            let _ = self.playlist.seek_id(new_id).await;
            if secs > 0 {
                let _ = self.playlist.seek_second_absolute(secs).await;
            }
            if !was_playing {
                let _ = self.playlist.pause().await;
            }
        }

        self.resync_playlist_cache().await;
        Ok(())
    }

    async fn play_index(&self, index: usize) -> Result<()> {
        let _guard = self.playlist_mu.lock().await;
        self.ensure_playlist_active().await;
        let maybe_id = self
            .playlist_cache
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .ids
            .get(index)
            .copied();
        match maybe_id {
            Some(id) => self.playlist.seek_id(id).await.map_err(map_client_err)?,
            None => self
                .playlist
                .seek_index(index as u32)
                .await
                .map_err(map_client_err)?,
        }
        let _ = self.playlist.play().await;
        Ok(())
    }

    async fn next(&self) -> Result<()> {
        self.playlist.next().await.map_err(map_client_err)
    }

    async fn previous(&self) -> Result<()> {
        self.playlist.previous().await.map_err(map_client_err)
    }

    async fn set_loop_mode(&self, repeat: &str, shuffle: bool) -> Result<()> {
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

    fn supports_repeat_one(&self) -> bool {
        false
    }
}

/// Connect to an OpenHome renderer by `description.xml` URL.
pub async fn connect_by_url(location: &str) -> Result<Arc<OpenHomeRenderer>> {
    OpenHomeRenderer::connect(location).await
}

/// Try connecting to a renderer URL as OpenHome.
/// Returns `Ok(None)` when the device is reachable but not OpenHome (or uses HTTPS /
/// lenient XML that only Tacet's UPnP parser supports), while propagating SSRF,
/// cross-origin `<URLBase>`, timeout, and connection errors directly.
pub async fn try_connect_by_url(location: &str) -> Result<Option<Arc<OpenHomeRenderer>>> {
    let pinned = validate_renderer_url(location).await?;
    if pinned.scheme() != "http" {
        return Ok(None);
    }
    let http = HttpClient::new(OPENHOME_HTTP_TIMEOUT);
    let device = match OpenHomeDevice::connect_with_http(pinned.as_str(), http).await {
        Ok(dev) => dev,
        // Lenient-XML UPnP devices (Latin-1, undeclared prefixes) should fall back to UPnP.
        Err(ClientError::Core(openhome_core::Error::Xml(_))) => return Ok(None),
        Err(e) => return Err(map_client_err(e)),
    };
    validate_service_control_urls(&pinned, device.description())?;
    if !device.is_openhome_device() || device.playlist().is_err() {
        return Ok(None);
    }
    Ok(Some(OpenHomeRenderer::from_device(device).await?))
}

/// Ensure `<URLBase>` (if present) and every service `controlURL` declared in
/// `description.xml` resolve to the exact same scheme, host, and port as the
/// validated/pinned description URL.
pub(crate) fn validate_service_control_urls(pinned: &Url, desc: &DeviceDescription) -> Result<()> {
    let effective_base = match desc.url_base.as_deref().map(str::trim) {
        Some(raw) if !raw.is_empty() => {
            let parsed = Url::parse(raw)
                .map_err(|e| UpnpError::Other(format!("invalid URLBase in description: {e}")))?;
            if !same_origin(&parsed, pinned) {
                return Err(UpnpError::Other(format!(
                    "refusing cross-origin URLBase: {raw}"
                )));
            }
            parsed
        }
        _ => pinned.clone(),
    };

    for svc in &desc.services {
        let resolved = effective_base
            .join(&svc.control_url)
            .map_err(|e| UpnpError::Other(format!("invalid service control URL: {e}")))?;
        if !same_origin(&resolved, pinned) {
            return Err(UpnpError::Other(format!(
                "refusing cross-origin service control URL: {}",
                svc.control_url
            )));
        }
    }
    Ok(())
}

fn same_origin(a: &Url, b: &Url) -> bool {
    a.scheme() == b.scheme()
        && a.host() == b.host()
        && a.port_or_known_default() == b.port_or_known_default()
}

/// Discover raw OpenHome SSDP `LOCATION` URLs on all active LAN interfaces
/// without performing any HTTP fetches.
pub(crate) async fn discover_openhome_locations(timeout: Duration) -> Vec<String> {
    msearch_locations(OPENHOME_SEARCH_TARGETS, timeout)
        .await
        .into_iter()
        .map(|(loc, _)| loc)
        .collect()
}

/// Perform a targeted SSDP discovery sweep for OpenHome Media renderers on the LAN.
/// Every `LOCATION` is SSRF-validated/pinned BEFORE any HTTP fetch, and connected once.
pub async fn discover_openhome(timeout: Duration) -> Vec<Arc<OpenHomeRenderer>> {
    let locs = discover_openhome_locations(timeout).await;
    connect_locations(locs).await
}

pub(crate) async fn connect_locations(locs: Vec<String>) -> Vec<Arc<OpenHomeRenderer>> {
    let mut seen_ids = HashSet::new();
    let mut out = Vec::new();
    let mut futs = stream::iter(locs)
        .map(|loc| async move { OpenHomeRenderer::connect(&loc).await.ok() })
        .buffer_unordered(8);

    while let Some(opt_r) = futs.next().await {
        if let Some(r) = opt_r
            && seen_ids.insert(r.info.id.clone())
        {
            out.push(r);
        }
    }
    out
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

fn volume_to_device(vol: i32, max: u32) -> u32 {
    let clamped = vol.clamp(0, 100) as u64;
    let max = u64::from(max.max(1));
    ((clamped * max + 50) / 100).min(max) as u32
}

fn volume_from_device(raw: u32, max: u32) -> i32 {
    let max = u64::from(max.max(1));
    let raw = u64::from(raw).min(max);
    ((raw * 100 + max / 2) / max).min(100) as i32
}

fn extract_host(location: &str) -> String {
    Url::parse(location)
        .ok()
        .and_then(|u| u.host_str().map(str::to_string))
        .unwrap_or_default()
}

fn normalize_udn(udn: &str, location: &str) -> String {
    let trimmed = udn.trim();
    if !trimmed.is_empty() {
        return trimmed
            .strip_prefix("uuid:")
            .or_else(|| trimmed.strip_prefix("UUID:"))
            .unwrap_or(trimmed)
            .to_string();
    }
    location.to_string()
}

fn choose_display_name(
    room: Option<&str>,
    product_name: Option<&str>,
    friendly: &str,
    host: &str,
) -> String {
    let room = room.map(str::trim).filter(|s| !s.is_empty());
    let prod = product_name.map(str::trim).filter(|s| !s.is_empty());
    match (room, prod) {
        (Some(r), Some(p)) if !r.eq_ignore_ascii_case(p) => format!("{r} ({p})"),
        (Some(r), _) => r.to_string(),
        (None, Some(p)) => p.to_string(),
        (None, None) => {
            let f = friendly.trim();
            if !f.is_empty() {
                f.to_string()
            } else if !host.is_empty() {
                format!("OpenHome ({host})")
            } else {
                "OpenHome Renderer".to_string()
            }
        }
    }
}

fn parse_protocol_info(raw: &str) -> Vec<String> {
    raw.split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use openhome_core::ServiceDescription;
    use openhome_server::{MemoryRenderer, OpenHomeServer, SsdpConfig};
    use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};

    #[test]
    fn volume_scaling_and_bounds_handle_limits_and_fixed_output() {
        assert_eq!(volume_to_device(0, 80), 0);
        assert_eq!(volume_to_device(50, 80), 40);
        assert_eq!(volume_to_device(100, 80), 80);
        assert_eq!(volume_to_device(150, 80), 80);
        assert_eq!(volume_from_device(40, 80), 50);
        assert_eq!(volume_from_device(80, 80), 100);
        assert_eq!(volume_from_device(99, 80), 100);

        // Hardware VolumeLimit clamps effective max below VolumeMax.
        assert_eq!(resolve_volume_bounds(true, Some(100), Some(70)), (true, 70));
        // Fixed-output device with VolumeMax == 0 disables volume control.
        assert_eq!(resolve_volume_bounds(true, Some(0), Some(100)), (false, 0));
        assert_eq!(resolve_volume_bounds(false, None, None), (false, 100));
    }

    #[test]
    fn validate_service_control_urls_blocks_cross_origin_url_base_and_control_url() {
        let pinned = Url::parse("http://192.168.1.50:49152/description.xml").unwrap();
        let mk_desc = |url_base: Option<&str>, control_url: &str| DeviceDescription {
            url_base: url_base.map(str::to_string),
            device_type: "urn:av-openhome-org:device:Source:1".into(),
            friendly_name: "Test".into(),
            manufacturer: "Tacet".into(),
            manufacturer_url: None,
            model_description: None,
            model_name: "Mock".into(),
            model_number: None,
            model_url: None,
            serial_number: None,
            udn: "uuid:test".into(),
            presentation_url: None,
            services: vec![ServiceDescription {
                service_type: "urn:av-openhome-org:service:Playlist:1".into(),
                service_id: "urn:av-openhome-org:serviceId:Playlist".into(),
                scpd_url: "/Playlist/scpd.xml".into(),
                control_url: control_url.into(),
                event_sub_url: "/Playlist/event".into(),
            }],
        };

        assert!(
            validate_service_control_urls(
                &pinned,
                &mk_desc(Some("http://192.168.1.50:49152/"), "/Playlist/control"),
            )
            .is_ok()
        );
        assert!(
            validate_service_control_urls(
                &pinned,
                &mk_desc(Some("http://127.0.0.1:4320/"), "/Playlist/control"),
            )
            .is_err()
        );
        assert!(
            validate_service_control_urls(
                &pinned,
                &mk_desc(None, "http://169.254.169.254/latest/meta-data/"),
            )
            .is_err()
        );
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
    async fn try_connect_by_url_returns_none_for_https_lan_urls() {
        let res = try_connect_by_url("https://192.168.1.99:49152/description.xml")
            .await
            .expect("https LAN URL should cleanly fall back to UPnP");
        assert!(res.is_none());
    }

    #[tokio::test]
    async fn openhome_renderer_end_to_end_and_concurrent_status_safety() {
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
        assert_eq!(renderer.qobuz_delivery(), "force");

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
        assert!(!qc.supports_repeat_one());
        qc.replace_queue(&[mk("t1", "So What"), mk("t2", "Freddie Freeloader")], 0)
            .await
            .expect("replace_queue");

        let st = renderer.status().await.expect("status");
        assert_eq!(st.state, "PLAYING");
        assert!(st.has_queue_index);
        assert_eq!(st.queue_index, 0);

        // Concurrent status() polling during append_queue must not corrupt playlist_cache.
        let extra = [mk("t3", "Blue in Green")];
        let (append_res, status_res) = tokio::join!(qc.append_queue(&extra), renderer.status());
        append_res.expect("concurrent append_queue");
        status_res.expect("concurrent status");
        assert_eq!(renderer.playlist_cache.lock().unwrap().ids.len(), 3);

        qc.play_index(2).await.expect("play_index");
        let st = renderer.status().await.expect("status after play_index");
        assert_eq!(st.queue_index, 2);

        qc.move_queue_item(2, 0).await.expect("move_queue_item");
        qc.remove_queue_range(1, 1)
            .await
            .expect("remove_queue_range");
        assert_eq!(renderer.playlist_cache.lock().unwrap().ids.len(), 2);

        // Regression: radio-tail sync passes end == len; must clamp to len - 1.
        qc.remove_queue_range(1, 2)
            .await
            .expect("remove_queue_range clamps end == len");
        assert_eq!(renderer.playlist_cache.lock().unwrap().ids.len(), 1);

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

        // Full-range clear uses DeleteAll cleanly.
        qc.remove_queue_range(0, 0).await.expect("full-range clear");
        assert!(renderer.playlist_cache.lock().unwrap().ids.is_empty());

        server.shutdown().await;
    }
}
