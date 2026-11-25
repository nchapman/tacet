use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use oxicast::{
    CastApp, CastClient, Image, MediaInfo, MediaMetadata, PlayerState, QueueItem, RepeatMode,
    StreamType,
};
use tokio::sync::Mutex as AsyncMutex;
use tokio_util::sync::CancellationToken;

use crate::upnp::{
    Controller, MediaItem, QueueController, RendererInfo, Result, TransportStatus, UpnpError,
    local_ip_for_host,
};

const CAST_MEDIA_NAMESPACE: &str = "urn:x-cast:com.google.cast.media";
const DEFAULT_MEDIA_RECEIVER_APP_ID: &str = "CC1AD845";

/// Native audio formats supported by Google Cast Default Media Receiver (CC1AD845).
/// DSD is intentionally omitted so tacet auto-decodes 1-bit DSD to 24-bit/88.2kHz WAV.
const CAST_SINK_PROTOCOLS: &[&str] = &[
    "http-get:*:audio/flac:*",
    "http-get:*:audio/wav:*",
    "http-get:*:audio/mpeg:*",
    "http-get:*:audio/aac:*",
    "http-get:*:audio/mp4:*",
    "http-get:*:audio/ogg:*",
    "http-get:*:audio/webm:*",
];

#[derive(Debug, Clone)]
pub struct CastDeviceDesc {
    pub id: String,
    pub name: String,
    pub model: String,
    pub host: String,
    pub port: u16,
}

struct CastConnState {
    client: Option<CastClient>,
    transport_id: String,
    media_session_id: i32,
    repeat_mode: RepeatMode,
    queue_items: Vec<MediaItem>,
    cur_idx: i32,
    queue_dirty: bool,
}

impl CastConnState {
    fn reset_conn(&mut self) {
        self.client = None;
        self.transport_id.clear();
        self.media_session_id = 0;
    }
}

pub struct CastRenderer {
    desc: CastDeviceDesc,
    local_ip: String,
    state: AsyncMutex<CastConnState>,
}

impl CastRenderer {
    pub fn new(desc: CastDeviceDesc) -> Arc<Self> {
        let local_ip = local_ip_for_host(&desc.host);
        Arc::new(Self {
            desc,
            local_ip,
            state: AsyncMutex::new(CastConnState {
                client: None,
                transport_id: String::new(),
                media_session_id: 0,
                repeat_mode: RepeatMode::RepeatOff,
                queue_items: Vec::new(),
                cur_idx: -1,
                queue_dirty: false,
            }),
        })
    }

    async fn ensure_connected<'a>(&self, st: &'a mut CastConnState) -> Result<&'a CastClient> {
        if let Some(ref c) = st.client
            && c.is_connected()
        {
            return Ok(st.client.as_ref().unwrap());
        }
        let client = CastClient::connect(&self.desc.host, self.desc.port)
            .await
            .map_err(|e| {
                UpnpError::Other(format!(
                    "cast connect {}:{}: {e}",
                    self.desc.host, self.desc.port
                ))
            })?;
        st.client = Some(client);
        st.transport_id.clear();
        st.media_session_id = 0;
        Ok(st.client.as_ref().unwrap())
    }

    async fn ensure_media_app(&self, st: &mut CastConnState) -> Result<()> {
        let client = self.ensure_connected(st).await?;
        let app = client
            .launch_app(&CastApp::DefaultMediaReceiver)
            .await
            .map_err(|e| UpnpError::Other(format!("cast launch DefaultMediaReceiver: {e}")))?;
        st.transport_id = app.transport_id;
        Ok(())
    }

    pub async fn close(&self) {
        let mut st = self.state.lock().await;
        if let Some(c) = st.client.take() {
            let _ = c.close().await;
        }
        st.transport_id.clear();
        st.media_session_id = 0;
    }

    async fn replace_queue_locked(
        &self,
        st: &mut CastConnState,
        items: &[MediaItem],
        start_index: usize,
    ) -> Result<()> {
        if items.is_empty() {
            return Err(UpnpError::Other("cast replace_queue: empty items".into()));
        }
        let qitems: Vec<QueueItem> = items.iter().map(to_cast_queue_item).collect();
        let start = if start_index < qitems.len() {
            start_index as u32
        } else {
            0
        };
        let repeat = st.repeat_mode;

        let mut last_err = None;
        for attempt in 0..2 {
            if let Err(e) = self.ensure_media_app(st).await {
                st.reset_conn();
                last_err = Some(e);
                continue;
            }
            let client = st.client.as_ref().unwrap();
            match client.queue_load(&qitems, start, repeat).await {
                Ok(ms) => {
                    st.media_session_id = ms.media_session_id;
                    st.queue_items = items.to_vec();
                    st.cur_idx = start as i32;
                    st.queue_dirty = false;
                    return Ok(());
                }
                Err(e) => {
                    st.reset_conn();
                    last_err = Some(UpnpError::Other(format!("cast queue_load: {e}")));
                    if attempt == 1 {
                        break;
                    }
                }
            }
        }
        Err(last_err.unwrap_or_else(|| UpnpError::Other("cast queue_load failed".into())))
    }

    async fn queue_jump(&self, delta: i32) -> Result<()> {
        let mut st = self.state.lock().await;
        if st.queue_dirty {
            let target = st.cur_idx + delta;
            if target >= 0 && (target as usize) < st.queue_items.len() {
                let items = st.queue_items.clone();
                return self
                    .replace_queue_locked(&mut st, &items, target as usize)
                    .await;
            }
        }
        let dest = st.transport_id.clone();
        let msid = st.media_session_id;
        if dest.is_empty() || msid == 0 {
            return Err(UpnpError::Other("cast no active media session".into()));
        }
        let client = self.ensure_connected(&mut st).await?;
        client
            .send_raw(
                CAST_MEDIA_NAMESPACE,
                &dest,
                serde_json::json!({
                    "type": "QUEUE_UPDATE",
                    "mediaSessionId": msid,
                    "jump": delta,
                }),
            )
            .await
            .map_err(|e| UpnpError::Other(format!("cast queue_jump({delta}): {e}")))?;
        let next_idx = st.cur_idx + delta;
        if next_idx >= 0 && (next_idx as usize) < st.queue_items.len() {
            st.cur_idx = next_idx;
        }
        Ok(())
    }
}

/// Matches a Cast `contentId` against the tracked queue, preferring `cur_idx`,
/// adjacent items (`cur_idx ± 1`), wrap-around `0`, and finally a full scan.
fn match_queue_index(items: &[MediaItem], cur_idx: i32, content_id: &str) -> Option<usize> {
    if content_id.is_empty() || items.is_empty() {
        return None;
    }
    let candidates = [cur_idx, cur_idx + 1, cur_idx - 1, 0];
    for c in candidates {
        if c >= 0
            && let Some(it) = items.get(c as usize)
            && it.url == content_id
        {
            return Some(c as usize);
        }
    }
    items.iter().position(|it| it.url == content_id)
}

fn to_cast_media_info(item: &MediaItem) -> MediaInfo {
    let mime = if item.mime.is_empty() {
        "audio/flac".to_string()
    } else {
        item.mime.clone()
    };
    let mut images = Vec::new();
    if !item.album_art_url.is_empty() {
        images.push(Image {
            url: item.album_art_url.clone(),
            width: None,
            height: None,
        });
    }
    let meta = MediaMetadata::MusicTrack {
        title: (!item.title.trim().is_empty()).then(|| item.title.trim().to_string()),
        artist: (!item.artist.trim().is_empty()).then(|| item.artist.trim().to_string()),
        album_name: (!item.album.trim().is_empty()).then(|| item.album.trim().to_string()),
        composer: None,
        track_number: (item.track_number > 0).then_some(item.track_number as u32),
        disc_number: None,
        images,
    };
    let mut mi = MediaInfo::new(item.url.clone(), mime)
        .stream_type(StreamType::Buffered)
        .metadata(meta);
    if !item.duration.is_zero() {
        mi = mi.duration(item.duration.as_secs_f64());
    }
    mi
}

fn to_cast_queue_item(item: &MediaItem) -> QueueItem {
    QueueItem {
        media: to_cast_media_info(item),
        autoplay: true,
        start_time: 0.0,
    }
}

fn map_repeat_mode(repeat: &str, shuffle: bool) -> RepeatMode {
    match repeat.trim().to_ascii_lowercase().as_str() {
        "one" => RepeatMode::RepeatSingle,
        "all" if shuffle => RepeatMode::RepeatAllAndShuffle,
        "all" => RepeatMode::RepeatAll,
        _ => RepeatMode::RepeatOff,
    }
}

fn repeat_mode_str(m: RepeatMode) -> &'static str {
    match m {
        RepeatMode::RepeatAll => "REPEAT_ALL",
        RepeatMode::RepeatSingle => "REPEAT_SINGLE",
        RepeatMode::RepeatAllAndShuffle => "REPEAT_ALL_AND_SHUFFLE",
        _ => "REPEAT_OFF",
    }
}

#[async_trait]
impl Controller for CastRenderer {
    fn info(&self) -> RendererInfo {
        RendererInfo {
            id: self.desc.id.clone(),
            kind: "cast".into(),
            name: self.desc.name.clone(),
            manufacturer: "Google".into(),
            model: self.desc.model.clone(),
            location: format!("cast://{}:{}", self.desc.host, self.desc.port),
            host: self.desc.host.clone(),
            has_volume: true,
            supports_set_next: false,
            supports_seek: true,
            supports_events: false,
            supports_queue: true,
            sink_protocols: CAST_SINK_PROTOCOLS.iter().map(|s| (*s).into()).collect(),
        }
    }

    fn local_ip(&self) -> String {
        self.local_ip.clone()
    }

    async fn set_uri(&self, item: &MediaItem) -> Result<()> {
        let mut st = self.state.lock().await;
        let mi = to_cast_media_info(item);
        let mut last_err = None;
        for attempt in 0..2 {
            if let Err(e) = self.ensure_media_app(&mut st).await {
                st.reset_conn();
                last_err = Some(e);
                continue;
            }
            let client = st.client.as_ref().unwrap();
            match client.load_media(&mi, true, 0.0, None).await {
                Ok(ms) => {
                    st.media_session_id = ms.media_session_id;
                    st.queue_items = vec![item.clone()];
                    st.cur_idx = 0;
                    st.queue_dirty = false;
                    return Ok(());
                }
                Err(e) => {
                    st.reset_conn();
                    last_err = Some(UpnpError::Other(format!("cast load_media: {e}")));
                    if attempt == 1 {
                        break;
                    }
                }
            }
        }
        Err(last_err.unwrap_or_else(|| UpnpError::Other("cast load_media failed".into())))
    }

    async fn set_next_uri(&self, _item: &MediaItem) -> Result<()> {
        Err(UpnpError::ActionUnsupported)
    }

    async fn play(&self) -> Result<()> {
        let mut st = self.state.lock().await;
        let client = self.ensure_connected(&mut st).await?;
        let ms = client
            .play()
            .await
            .map_err(|e| UpnpError::Other(format!("cast play: {e}")))?;
        st.media_session_id = ms.media_session_id;
        Ok(())
    }

    async fn pause(&self) -> Result<()> {
        let mut st = self.state.lock().await;
        let Some(client) = st.client.as_ref() else {
            return Ok(());
        };
        let ms = client
            .pause()
            .await
            .map_err(|e| UpnpError::Other(format!("cast pause: {e}")))?;
        st.media_session_id = ms.media_session_id;
        Ok(())
    }

    async fn stop(&self) -> Result<()> {
        let mut st = self.state.lock().await;
        let Some(client) = st.client.as_ref() else {
            return Ok(());
        };
        let _ = client.stop_media().await;
        st.media_session_id = 0;
        Ok(())
    }

    async fn seek(&self, pos: Duration) -> Result<()> {
        let mut st = self.state.lock().await;
        let client = self.ensure_connected(&mut st).await?;
        client
            .seek(pos.as_secs_f64())
            .await
            .map_err(|e| UpnpError::Other(format!("cast seek: {e}")))?;
        Ok(())
    }

    async fn status(&self) -> Result<TransportStatus> {
        let mut st = self.state.lock().await;
        let mut ts = TransportStatus {
            state: "STOPPED".into(),
            has_volume: true,
            ..Default::default()
        };

        // Do not connect or wake idle Cast devices during periodic status polling.
        let Some(client) = st.client.as_ref() else {
            return Ok(ts);
        };

        let (rs_res, ms_res) = (client.receiver_status().await, client.media_status().await);

        // If both receiver and media status fail, tear down the stale Cast client so the next call reconnects.
        if rs_res.is_err() && ms_res.is_err() {
            st.client = None;
            st.transport_id.clear();
            st.media_session_id = 0;
            return Err(UpnpError::Other(
                "cast status failed on both receiver and media channels".into(),
            ));
        }

        let mut our_app_active = false;
        if let Ok(rs) = rs_res {
            ts.volume = (rs.volume.level.clamp(0.0, 1.0) * 100.0).round() as i32;
            ts.has_vol_value = true;
            ts.muted = rs.volume.muted;
            ts.has_mute_val = true;
            if let Some(app) = rs.applications.first()
                && app.app_id == DEFAULT_MEDIA_RECEIVER_APP_ID
            {
                st.transport_id = app.transport_id.clone();
                our_app_active = true;
            } else {
                st.transport_id.clear();
                st.media_session_id = 0;
            }
        }

        if our_app_active && matches!(ms_res, Ok(None)) {
            st.media_session_id = 0;
        }

        if our_app_active && let Ok(Some(ms)) = ms_res {
            st.media_session_id = ms.media_session_id;
            ts.state = match ms.player_state {
                PlayerState::Playing => "PLAYING",
                PlayerState::Paused => "PAUSED_PLAYBACK",
                PlayerState::Buffering => "TRANSITIONING",
                _ => "STOPPED",
            }
            .into();
            ts.position = Duration::from_secs_f64(ms.current_time.max(0.0));
            ts.has_position = true;
            if let Some(d) = ms.duration
                && d > 0.0
            {
                ts.duration = Duration::from_secs_f64(d);
            }
            if let Some(m) = ms.media {
                if let Some(idx) = match_queue_index(&st.queue_items, st.cur_idx, &m.content_id) {
                    st.cur_idx = idx as i32;
                    ts.queue_index = idx as i32;
                    ts.has_queue_index = true;
                }
                ts.track_uri = m.content_id;
            }
        }

        Ok(ts)
    }

    async fn set_volume(&self, vol: i32) -> Result<()> {
        let mut st = self.state.lock().await;
        let client = self.ensure_connected(&mut st).await?;
        let level = (vol.clamp(0, 100) as f32) / 100.0;
        client
            .set_volume(level)
            .await
            .map_err(|e| UpnpError::Other(format!("cast set_volume: {e}")))?;
        Ok(())
    }

    async fn set_mute(&self, muted: bool) -> Result<()> {
        let mut st = self.state.lock().await;
        let client = self.ensure_connected(&mut st).await?;
        client
            .set_muted(muted)
            .await
            .map_err(|e| UpnpError::Other(format!("cast set_muted: {e}")))?;
        Ok(())
    }

    fn as_queue_controller(&self) -> Option<&dyn QueueController> {
        Some(self)
    }

    async fn close_session(&self) {
        self.close().await;
    }
}

#[async_trait]
impl QueueController for CastRenderer {
    async fn replace_queue(&self, items: &[MediaItem], start_index: usize) -> Result<()> {
        let mut st = self.state.lock().await;
        self.replace_queue_locked(&mut st, items, start_index).await
    }

    async fn append_queue(&self, items: &[MediaItem]) -> Result<()> {
        if items.is_empty() {
            return Ok(());
        }
        let mut st = self.state.lock().await;
        st.queue_items.extend_from_slice(items);
        if st.queue_dirty || st.media_session_id == 0 || st.client.is_none() {
            st.queue_dirty = true;
            return Ok(());
        }
        let qitems: Vec<QueueItem> = items.iter().map(to_cast_queue_item).collect();
        let client = st.client.as_ref().unwrap();
        match client.queue_insert(&qitems, None).await {
            Ok(ms) => {
                st.media_session_id = ms.media_session_id;
                Ok(())
            }
            Err(_) => {
                st.queue_dirty = true;
                Ok(())
            }
        }
    }

    async fn remove_queue_range(&self, start: usize, end: usize) -> Result<()> {
        let mut st = self.state.lock().await;
        if end < start || start >= st.queue_items.len() {
            return Ok(());
        }
        let end = end.min(st.queue_items.len() - 1);
        st.queue_items.drain(start..=end);
        let removed = (end - start + 1) as i32;
        if st.cur_idx > end as i32 {
            st.cur_idx -= removed;
        } else if st.cur_idx >= start as i32 {
            if st.queue_items.is_empty() {
                st.cur_idx = -1;
            } else {
                st.cur_idx = (start as i32).min(st.queue_items.len() as i32 - 1);
            }
        }
        st.queue_dirty = true;
        Ok(())
    }

    async fn move_queue_item(&self, from: usize, to: usize) -> Result<()> {
        let mut st = self.state.lock().await;
        if from >= st.queue_items.len() || to >= st.queue_items.len() || from == to {
            return Ok(());
        }
        let moved = st.queue_items.remove(from);
        st.queue_items.insert(to, moved);
        let (f, t) = (from as i32, to as i32);
        if st.cur_idx == f {
            st.cur_idx = t;
        } else if f < st.cur_idx && t >= st.cur_idx {
            st.cur_idx -= 1;
        } else if f > st.cur_idx && t <= st.cur_idx {
            st.cur_idx += 1;
        }
        st.queue_dirty = true;
        Ok(())
    }

    async fn play_index(&self, index: usize) -> Result<()> {
        let mut st = self.state.lock().await;
        if index >= st.queue_items.len() {
            return Err(UpnpError::Other(format!(
                "cast play_index {index} out of range [0,{})",
                st.queue_items.len()
            )));
        }
        if !st.queue_dirty
            && !st.transport_id.is_empty()
            && st.media_session_id != 0
            && st.cur_idx >= 0
            && let Some(client) = st.client.as_ref()
        {
            let jump = index as i32 - st.cur_idx;
            if jump == 0 {
                return Ok(());
            }
            if client
                .send_raw(
                    CAST_MEDIA_NAMESPACE,
                    &st.transport_id,
                    serde_json::json!({
                        "type": "QUEUE_UPDATE",
                        "mediaSessionId": st.media_session_id,
                        "jump": jump,
                    }),
                )
                .await
                .is_ok()
            {
                st.cur_idx = index as i32;
                return Ok(());
            }
        }
        let items = st.queue_items.clone();
        self.replace_queue_locked(&mut st, &items, index).await
    }

    async fn next(&self) -> Result<()> {
        self.queue_jump(1).await
    }

    async fn previous(&self) -> Result<()> {
        self.queue_jump(-1).await
    }

    async fn set_loop_mode(&self, repeat: &str, shuffle: bool) -> Result<()> {
        let mut st = self.state.lock().await;
        let mode = map_repeat_mode(repeat, shuffle);
        let dest = st.transport_id.clone();
        let msid = st.media_session_id;
        if dest.is_empty() || msid == 0 {
            st.repeat_mode = mode;
            return Ok(());
        }
        let client = self.ensure_connected(&mut st).await?;
        client
            .send_raw(
                CAST_MEDIA_NAMESPACE,
                &dest,
                serde_json::json!({
                    "type": "QUEUE_UPDATE",
                    "mediaSessionId": msid,
                    "repeatMode": repeat_mode_str(mode),
                }),
            )
            .await
            .map_err(|e| UpnpError::Other(format!("cast set_loop_mode: {e}")))?;
        st.repeat_mode = mode;
        Ok(())
    }
}

/// Discovers Google Cast devices on the LAN via mDNS (`_googlecast._tcp.local.`).
pub async fn discover_cast(timeout: Duration) -> Vec<Arc<CastRenderer>> {
    let devices = match oxicast::discovery::discover_devices(timeout).await {
        Ok(d) => d,
        Err(e) => {
            tracing::debug!(error = %e, "cast mDNS discovery failed");
            return Vec::new();
        }
    };
    devices
        .into_iter()
        .filter(|d| d.port >= 1024 && crate::upnp::is_allowed_renderer_ip(d.ip))
        .map(|d| {
            let uuid = d.uuid.unwrap_or_else(|| format!("{}:{}", d.ip, d.port));
            CastRenderer::new(CastDeviceDesc {
                id: format!("cast:{uuid}"),
                name: d.name,
                model: d.model.unwrap_or_else(|| "Chromecast".into()),
                host: d.ip.to_string(),
                port: d.port,
            })
        })
        .collect()
}

pub type RemovedCallback = Arc<dyn Fn(&str, &str) + Send + Sync>;

pub struct CastMonitorCallbacks {
    pub list_cast: Arc<dyn Fn() -> Vec<RendererInfo> + Send + Sync>,
    pub list_upnp_hosts: Arc<dyn Fn() -> HashSet<String> + Send + Sync>,
    pub is_active: Arc<dyn Fn(&str) -> bool + Send + Sync>,
    pub on_found: Arc<dyn Fn(Arc<CastRenderer>) + Send + Sync>,
    pub on_removed: RemovedCallback,
}

pub struct CastMonitor {
    sweep_interval: Duration,
    miss_threshold: usize,
    cb: CastMonitorCallbacks,
    cancel: CancellationToken,
    started: Mutex<bool>,
}

impl CastMonitor {
    pub fn new(sweep_interval: Duration, cb: CastMonitorCallbacks) -> Arc<Self> {
        Arc::new(Self {
            sweep_interval: if sweep_interval.is_zero() {
                Duration::from_secs(60)
            } else {
                sweep_interval
            },
            miss_threshold: 3,
            cb,
            cancel: CancellationToken::new(),
            started: Mutex::new(false),
        })
    }

    pub fn start(self: &Arc<Self>) {
        {
            let mut g = self.started.lock().unwrap_or_else(|e| e.into_inner());
            if *g {
                return;
            }
            *g = true;
        }
        let this = Arc::clone(self);
        tokio::spawn(async move {
            // Brief startup delay so UPnP discovery registers dual-protocol devices first.
            tokio::select! {
                _ = this.cancel.cancelled() => return,
                _ = tokio::time::sleep(Duration::from_secs(3)) => {}
            }
            let mut missed: HashMap<String, usize> = HashMap::new();
            loop {
                let found = discover_cast(Duration::from_secs(3)).await;
                let upnp_hosts = (this.cb.list_upnp_hosts)();
                let mut seen: HashSet<String> = HashSet::with_capacity(found.len());
                for r in found {
                    let info = r.info();
                    if upnp_hosts.contains(&info.host) {
                        continue;
                    }
                    seen.insert(info.id.clone());
                    missed.remove(&info.id);
                    (this.cb.on_found)(r);
                }
                for info in (this.cb.list_cast)() {
                    if seen.contains(&info.id) || (this.cb.is_active)(&info.id) {
                        missed.remove(&info.id);
                        continue;
                    }
                    let count = missed.entry(info.id.clone()).or_insert(0);
                    *count += 1;
                    if *count >= this.miss_threshold {
                        missed.remove(&info.id);
                        (this.cb.on_removed)(&info.id, &info.name);
                    }
                }
                tokio::select! {
                    _ = this.cancel.cancelled() => break,
                    _ = tokio::time::sleep(this.sweep_interval) => {}
                }
            }
        });
    }

    pub fn stop(&self) {
        self.cancel.cancel();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cast_media_info_and_repeat_mode() {
        let item = MediaItem {
            id: "1".into(),
            url: "http://192.168.1.5:4320/media/local/1.flac".into(),
            title: "Time".into(),
            artist: "Pink Floyd".into(),
            album: "DSOTM".into(),
            track_number: 4,
            mime: "audio/flac".into(),
            duration: Duration::from_secs(420),
            ..Default::default()
        };
        let mi = to_cast_media_info(&item);
        assert_eq!(mi.content_type, "audio/flac");
        assert_eq!(mi.duration, Some(420.0));
        assert_eq!(map_repeat_mode("one", false), RepeatMode::RepeatSingle);
        assert_eq!(
            map_repeat_mode("all", true),
            RepeatMode::RepeatAllAndShuffle
        );
        assert_eq!(map_repeat_mode("off", false), RepeatMode::RepeatOff);
    }

    #[tokio::test]
    async fn cast_queue_tracking_remove_move_and_match_index() {
        let r = CastRenderer::new(CastDeviceDesc {
            id: "cast:test".into(),
            name: "Living Room TV".into(),
            model: "Chromecast".into(),
            host: "192.168.1.99".into(),
            port: 8009,
        });
        let mk = |u: &str| MediaItem {
            url: u.into(),
            ..Default::default()
        };
        {
            let mut st = r.state.lock().await;
            st.queue_items = vec![mk("u0"), mk("u1"), mk("u2"), mk("u3")];
            st.cur_idx = 1;
            st.queue_dirty = false;
        }

        // match_queue_index prefers cur_idx / adjacent / wrap / scan
        {
            let st = r.state.lock().await;
            assert_eq!(match_queue_index(&st.queue_items, 1, "u1"), Some(1));
            assert_eq!(match_queue_index(&st.queue_items, 1, "u2"), Some(2));
            assert_eq!(match_queue_index(&st.queue_items, 3, "u0"), Some(0));
            assert_eq!(match_queue_index(&st.queue_items, 0, "u3"), Some(3));
        }

        // Removing an upcoming track (index 2) keeps cur_idx=1 and marks dirty
        r.remove_queue_range(2, 2).await.unwrap();
        {
            let st = r.state.lock().await;
            assert_eq!(st.queue_items.len(), 3);
            assert_eq!(st.cur_idx, 1);
            assert!(st.queue_dirty);
        }

        // Moving index 2 before cur_idx=1 shifts cur_idx to 2
        r.move_queue_item(2, 0).await.unwrap();
        {
            let st = r.state.lock().await;
            assert_eq!(st.queue_items[0].url, "u3");
            assert_eq!(st.cur_idx, 2);
            assert_eq!(st.queue_items[st.cur_idx as usize].url, "u1");
        }
    }
}
