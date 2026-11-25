use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use rand::seq::SliceRandom;
use serde::Serialize;
use tokio::sync::{Mutex as AsyncMutex, broadcast};
use tokio_util::sync::CancellationToken;

use crate::library::{Catalog, SOURCE_LOCAL, SOURCE_QOBUZ, Track};
use crate::media::detect_transcoder;
use crate::qobuz::Client as QobuzClient;
use crate::upnp::{Controller, DeviceEvent, GenaClient, MediaItem, RendererInfo, TransportStatus};

use super::types::{
    AudioQuality, Event, Item, ItemsEvent, PlaybackEvent, PlaybackState, QueueError, RepeatMode,
    Result, StreamMode, VolumeEvent, ZoneErrorEvent, ZoneErrorPayload, ZoneSnapshot,
    resolve_track_quality,
};

struct ZoneInner {
    ctrl: Arc<dyn Controller>,
    queue: Vec<Item>,
    original_queue: Vec<Item>,
    current_index: i32,
    shuffle: bool,
    repeat: RepeatMode,
    revision: u64,
    status: TransportStatus,
    position_updated_at: DateTime<Utc>,
    current_uri: String,
    next_uri: String,
    next_index: i32,
    audio_quality: AudioQuality,
    active_quality: Option<AudioQuality>,
    next_preloaded: bool,
    set_next_unsupported: bool,
    last_played_item_id: String,
    last_command_at: Instant,
}

struct Zone {
    op_mu: AsyncMutex<()>,
    inner: Mutex<ZoneInner>,
}

pub struct Manager {
    catalog: Arc<Catalog>,
    qobuz: Option<Arc<QobuzClient>>,
    server_port: u16,
    public_url: RwLock<String>,
    stream_mode: RwLock<StreamMode>,
    zones: RwLock<HashMap<String, Arc<Zone>>>,
    gena: RwLock<Option<Arc<GenaClient>>>,
    item_seq: AtomicU64,
    event_seq: AtomicU64,
    err_seq: AtomicU64,
    events_tx: broadcast::Sender<Event>,
    stop: CancellationToken,
}

impl Manager {
    pub fn new(
        catalog: Arc<Catalog>,
        qobuz: Option<Arc<QobuzClient>>,
        server_port: u16,
        stream_mode: StreamMode,
    ) -> Arc<Self> {
        let (events_tx, _) = broadcast::channel(256);
        let mgr = Arc::new(Self {
            catalog,
            qobuz,
            server_port,
            public_url: RwLock::new(String::new()),
            stream_mode: RwLock::new(stream_mode),
            zones: RwLock::new(HashMap::new()),
            gena: RwLock::new(None),
            item_seq: AtomicU64::new(0),
            event_seq: AtomicU64::new(0),
            err_seq: AtomicU64::new(0),
            events_tx,
            stop: CancellationToken::new(),
        });
        mgr.start_poll_loop();
        mgr
    }

    pub fn set_gena(&self, gena: Arc<GenaClient>) {
        *self.gena.write().unwrap_or_else(|e| e.into_inner()) = Some(gena);
    }

    pub fn set_public_url(&self, url: impl Into<String>) {
        let u = url.into().trim().trim_end_matches('/').to_string();
        *self.public_url.write().unwrap_or_else(|e| e.into_inner()) = u;
    }

    fn base_url_for(&self, ctrl: &dyn Controller) -> String {
        let explicit = self
            .public_url
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        if !explicit.is_empty() {
            return explicit;
        }
        format!("http://{}:{}", ctrl.local_ip(), self.server_port)
    }

    pub fn close(&self) {
        self.stop.cancel();
    }

    pub fn subscribe(&self) -> broadcast::Receiver<Event> {
        self.events_tx.subscribe()
    }

    pub fn broadcast<T: Serialize>(&self, event_type: &str, data: &T) {
        self.emit(event_type, "", data);
    }

    fn emit<T: Serialize>(&self, event_type: &str, zone_id: &str, data: &T) {
        let Ok(val) = serde_json::to_value(data) else {
            return;
        };
        let id = self.event_seq.fetch_add(1, Ordering::Relaxed) + 1;
        let _ = self.events_tx.send(Event {
            id,
            event_type: event_type.into(),
            zone_id: zone_id.into(),
            data: val,
        });
    }

    pub fn stream_mode(&self) -> StreamMode {
        *self.stream_mode.read().unwrap_or_else(|e| e.into_inner())
    }

    pub fn set_stream_mode(&self, mode: StreamMode) {
        *self.stream_mode.write().unwrap_or_else(|e| e.into_inner()) = mode;
    }

    pub fn register_renderer(&self, ctrl: Arc<dyn Controller>) {
        let info = ctrl.info();
        let zone_id = info.id.clone();

        if let Some((av_ev, rc_ev)) = ctrl.event_urls()
            && let Some(gena) = self.gena.read().unwrap_or_else(|e| e.into_inner()).clone()
        {
            let cb_base = self.base_url_for(ctrl.as_ref());
            gena.watch_renderer(&zone_id, &cb_base, &av_ev, &rc_ev);
        }

        let is_new = {
            let mut g = self.zones.write().unwrap_or_else(|e| e.into_inner());
            if let Some(existing) = g.get(&zone_id) {
                let mut zi = existing.inner.lock().unwrap_or_else(|e| e.into_inner());
                zi.ctrl = ctrl;
                false
            } else {
                let z = Arc::new(Zone {
                    op_mu: AsyncMutex::new(()),
                    inner: Mutex::new(ZoneInner {
                        ctrl,
                        queue: Vec::new(),
                        original_queue: Vec::new(),
                        current_index: -1,
                        shuffle: false,
                        repeat: RepeatMode::Off,
                        revision: 1,
                        status: TransportStatus {
                            state: "STOPPED".into(),
                            volume: 100,
                            has_volume: info.has_volume,
                            ..Default::default()
                        },
                        position_updated_at: Utc::now(),
                        current_uri: String::new(),
                        next_uri: String::new(),
                        next_index: -1,
                        audio_quality: AudioQuality::Original,
                        active_quality: None,
                        next_preloaded: false,
                        set_next_unsupported: !info.supports_set_next,
                        last_played_item_id: String::new(),
                        last_command_at: Instant::now() - Duration::from_secs(60),
                    }),
                });
                g.insert(zone_id.clone(), z);
                true
            }
        };

        if is_new {
            let zones = self.list_zones();
            self.emit("zones.updated", "", &serde_json::json!({ "zones": zones }));
        }
    }

    pub fn remove_renderer(&self, zone_id: &str) {
        if let Some(gena) = self.gena.read().unwrap_or_else(|e| e.into_inner()).clone() {
            gena.unwatch_renderer(zone_id);
        }
        let removed = self
            .zones
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .remove(zone_id)
            .is_some();
        if removed {
            let zones = self.list_zones();
            self.emit("zones.updated", "", &serde_json::json!({ "zones": zones }));
        }
    }

    pub fn list_renderers(&self) -> Vec<RendererInfo> {
        let g = self.zones.read().unwrap_or_else(|e| e.into_inner());
        let mut out: Vec<RendererInfo> = g
            .values()
            .map(|z| {
                z.inner
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .ctrl
                    .info()
            })
            .collect();
        out.sort_by_key(|a| a.name.to_lowercase());
        out
    }

    pub fn list_renderers_by_kind(&self, kind: &str) -> Vec<RendererInfo> {
        self.list_renderers()
            .into_iter()
            .filter(|r| r.kind == kind)
            .collect()
    }

    pub fn is_zone_active(&self, zone_id: &str) -> bool {
        let Some(z) = self.get_zone(zone_id) else {
            return false;
        };
        let zi = z.inner.lock().unwrap_or_else(|e| e.into_inner());
        matches!(
            PlaybackState::normalize(&zi.status.state),
            PlaybackState::Playing | PlaybackState::Buffering
        )
    }

    pub fn list_zones(&self) -> Vec<ZoneSnapshot> {
        let zones: Vec<(String, Arc<Zone>)> = {
            let g = self.zones.read().unwrap_or_else(|e| e.into_inner());
            g.iter().map(|(k, v)| (k.clone(), Arc::clone(v))).collect()
        };
        let mut out: Vec<ZoneSnapshot> = zones
            .into_iter()
            .map(|(id, z)| self.snapshot_zone(&id, &z))
            .collect();
        out.sort_by(|a, b| {
            a.renderer
                .name
                .to_lowercase()
                .cmp(&b.renderer.name.to_lowercase())
        });
        out
    }

    pub fn zone_snapshot(&self, zone_id: &str) -> Result<ZoneSnapshot> {
        let z = self
            .get_zone(zone_id)
            .ok_or_else(|| QueueError::ZoneNotFound(zone_id.into()))?;
        Ok(self.snapshot_zone(zone_id, &z))
    }

    fn get_zone(&self, zone_id: &str) -> Option<Arc<Zone>> {
        self.zones
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .get(zone_id)
            .cloned()
    }

    fn events_active(&self, zone_id: &str, ctrl: &Arc<dyn Controller>) -> bool {
        if ctrl.push_connected() {
            return true;
        }
        self.gena
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .is_some_and(|g| g.is_active(zone_id))
    }

    fn snapshot_zone(&self, zone_id: &str, z: &Zone) -> ZoneSnapshot {
        let zi = z.inner.lock().unwrap_or_else(|e| e.into_inner());
        let info = zi.ctrl.info();
        let (cur_item_id, cur_track) = current_item_pair(&zi);
        let dur = if !zi.status.duration.is_zero() {
            zi.status.duration.as_secs_f64().round() as i32
        } else {
            cur_track.as_ref().map(|t| t.duration_sec).unwrap_or(0)
        };
        ZoneSnapshot {
            zone_id: zone_id.into(),
            renderer: info,
            revision: zi.revision,
            state: PlaybackState::normalize(&zi.status.state),
            position_sec: zi.status.position.as_secs_f64().round() as i32,
            position_updated_at: zi.position_updated_at,
            duration_sec: dur,
            volume: zi.status.volume,
            muted: zi.status.muted,
            queue: zi.queue.clone(),
            current_index: zi.current_index,
            current_item_id: cur_item_id,
            current_track: cur_track,
            audio_quality: zi.audio_quality,
            active_quality: zi.active_quality,
            shuffle: zi.shuffle,
            repeat: zi.repeat,
            gapless_armed: zi.next_preloaded,
            events_active: self.events_active(zone_id, &zi.ctrl),
        }
    }

    fn make_playback_event(&self, zone_id: &str, zi: &ZoneInner) -> PlaybackEvent {
        let (cur_item_id, cur_track) = current_item_pair(zi);
        let dur = if !zi.status.duration.is_zero() {
            zi.status.duration.as_secs_f64().round() as i32
        } else {
            cur_track.as_ref().map(|t| t.duration_sec).unwrap_or(0)
        };
        PlaybackEvent {
            zone_id: zone_id.into(),
            revision: zi.revision,
            state: PlaybackState::normalize(&zi.status.state),
            position_sec: zi.status.position.as_secs_f64().round() as i32,
            position_updated_at: zi.position_updated_at,
            server_time: Utc::now(),
            duration_sec: dur,
            current_index: zi.current_index,
            current_item_id: cur_item_id,
            current_track: cur_track,
            audio_quality: zi.audio_quality,
            active_quality: zi.active_quality,
            shuffle: zi.shuffle,
            repeat: zi.repeat,
            gapless_armed: zi.next_preloaded,
            events_active: self.events_active(zone_id, &zi.ctrl),
        }
    }

    fn make_items_event(zone_id: &str, zi: &ZoneInner) -> ItemsEvent {
        let (cur_item_id, cur_track) = current_item_pair(zi);
        ItemsEvent {
            zone_id: zone_id.into(),
            revision: zi.revision,
            current_index: zi.current_index,
            current_item_id: cur_item_id,
            current_track: cur_track,
            items: zi.queue.clone(),
        }
    }

    fn emit_zone_error(&self, zone_id: &str, code: &str, message: String, retryable: bool) {
        let id = format!("err-{}", self.err_seq.fetch_add(1, Ordering::Relaxed) + 1);
        self.emit(
            "zone.error",
            zone_id,
            &ZoneErrorEvent {
                id,
                zone_id: zone_id.into(),
                error: ZoneErrorPayload {
                    code: code.into(),
                    message,
                    retryable,
                },
                occurred_at: Utc::now(),
            },
        );
    }

    fn wrap_tracks(&self, tracks: Vec<Track>) -> Vec<Item> {
        tracks
            .into_iter()
            .map(|track| Item {
                item_id: format!("qi-{}", self.item_seq.fetch_add(1, Ordering::Relaxed) + 1),
                track,
            })
            .collect()
    }

    // --- Media URL & Quality resolution ---

    async fn build_media_item(
        &self,
        track: &Track,
        ctrl: &Arc<dyn Controller>,
        requested_quality: AudioQuality,
    ) -> Result<(MediaItem, AudioQuality)> {
        let info = ctrl.info();
        let tc = detect_transcoder();
        let (eff_q, container) =
            resolve_track_quality(requested_quality, track, &info, tc.available());
        let rel = ctrl.prefer_relative_media();
        let base = if rel {
            String::new()
        } else {
            self.base_url_for(ctrl.as_ref())
        };

        let artwork_url = if track.artwork_url.starts_with('/') {
            format!("{base}{}", track.artwork_url)
        } else {
            track.artwork_url.clone()
        };

        let mut url_str;
        let mut mime = track.mime.clone();
        let mut seekable = true;
        let mut transcoded = false;

        if track.source == SOURCE_LOCAL {
            let ext = if track.ext.is_empty() {
                ".flac"
            } else {
                &track.ext
            };
            url_str = format!("{base}/media/local/{}{ext}", track.id);
            if eff_q.needs_transcode() {
                url_str.push_str(&format!("?q={}", eff_q.as_str()));
                if !container.is_empty() {
                    url_str.push_str(&format!("&c={container}"));
                }
                mime = transcoded_mime(eff_q, container).into();
                seekable = false;
                transcoded = true;
            }
        } else if track.source == SOURCE_QOBUZ {
            let qid = if track.qobuz_track_id > 0 {
                track.qobuz_track_id
            } else {
                track
                    .external_id
                    .parse::<i64>()
                    .or_else(|_| {
                        track
                            .id
                            .strip_prefix("qobuz:")
                            .unwrap_or(&track.id)
                            .parse::<i64>()
                    })
                    .unwrap_or(0)
            };
            let delivery = ctrl.qobuz_delivery();
            let use_relay = eff_q.needs_transcode()
                || delivery == "force"
                || (delivery != "direct" && self.stream_mode() == StreamMode::Relay);

            if use_relay {
                url_str = format!("{base}/media/qobuz/{qid}.flac");
                if eff_q.needs_transcode() {
                    url_str.push_str(&format!("?q={}", eff_q.as_str()));
                    if !container.is_empty() {
                        url_str.push_str(&format!("&c={container}"));
                    }
                    mime = transcoded_mime(eff_q, container).into();
                    seekable = false;
                    transcoded = true;
                }
            } else {
                let qb = self
                    .qobuz
                    .as_ref()
                    .ok_or_else(|| QueueError::Renderer("qobuz not configured".into()))?;
                let si = qb
                    .stream_url(qid)
                    .await
                    .map_err(|e| QueueError::Renderer(e.to_string()))?;
                url_str = si.url;
                mime = si.mime;
            }
        } else {
            return Err(QueueError::Renderer(format!(
                "unsupported track source {:?}",
                track.source
            )));
        }

        if mime.is_empty() || mime == "audio/x-flac" {
            mime = "audio/flac".into();
        }

        Ok((
            MediaItem {
                id: track.id.clone(),
                url: url_str,
                title: track.title.clone(),
                artist: track.artist.clone(),
                album: track.album.clone(),
                album_art_url: artwork_url,
                track_number: track.track_number,
                mime,
                duration: Duration::from_secs(track.duration_sec.max(0) as u64),
                size: if transcoded { 0 } else { track.size },
                sample_rate: track.sample_rate,
                bit_depth: track.bit_depth,
                channels: if track.channels > 0 {
                    track.channels
                } else {
                    2
                },
                seekable,
                transcoded,
            },
            eff_q,
        ))
    }

    // --- Queue Mutations ---

    pub async fn set_queue(
        &self,
        zone_id: &str,
        tracks: Vec<Track>,
        start_index: i32,
        autoplay: bool,
    ) -> Result<ZoneSnapshot> {
        let z = self
            .get_zone(zone_id)
            .ok_or_else(|| QueueError::ZoneNotFound(zone_id.into()))?;
        let _op = z.op_mu.lock().await;

        let items = self.wrap_tracks(tracks);
        let start = if items.is_empty() {
            -1
        } else {
            start_index.clamp(0, items.len() as i32 - 1)
        };

        let (ctrl, q_ev) = {
            let mut zi = z.inner.lock().unwrap_or_else(|e| e.into_inner());
            zi.original_queue = items.clone();
            zi.queue = items;
            zi.current_index = start;
            zi.next_preloaded = false;
            zi.next_uri.clear();
            zi.next_index = -1;
            zi.revision += 1;
            let q_ev = Self::make_items_event(zone_id, &zi);
            (Arc::clone(&zi.ctrl), q_ev)
        };
        self.emit("zone.queue", zone_id, &q_ev);

        if start >= 0 && autoplay {
            self.start_current_locked(zone_id, &z, &ctrl, true).await?;
        } else if start < 0 {
            let _ = ctrl.stop().await;
            let pb = {
                let mut zi = z.inner.lock().unwrap_or_else(|e| e.into_inner());
                zi.status.state = "STOPPED".into();
                zi.status.position = Duration::ZERO;
                zi.position_updated_at = Utc::now();
                zi.revision += 1;
                self.make_playback_event(zone_id, &zi)
            };
            self.emit("zone.playback", zone_id, &pb);
        }

        Ok(self.snapshot_zone(zone_id, &z))
    }

    pub async fn enqueue(&self, zone_id: &str, tracks: Vec<Track>) -> Result<ZoneSnapshot> {
        let z = self
            .get_zone(zone_id)
            .ok_or_else(|| QueueError::ZoneNotFound(zone_id.into()))?;
        let _op = z.op_mu.lock().await;

        let new_items = self.wrap_tracks(tracks);
        if new_items.is_empty() {
            return Ok(self.snapshot_zone(zone_id, &z));
        }

        let (ctrl, was_empty, q_ev, requested_q) = {
            let mut zi = z.inner.lock().unwrap_or_else(|e| e.into_inner());
            let was_empty = zi.queue.is_empty();
            zi.original_queue.extend(new_items.clone());
            zi.queue.extend(new_items.clone());
            if was_empty {
                zi.current_index = 0;
            }
            zi.next_preloaded = false;
            zi.revision += 1;
            let q_ev = Self::make_items_event(zone_id, &zi);
            (Arc::clone(&zi.ctrl), was_empty, q_ev, zi.audio_quality)
        };
        self.emit("zone.queue", zone_id, &q_ev);

        if was_empty {
            self.start_current_locked(zone_id, &z, &ctrl, true).await?;
        } else if let Some(qc) = ctrl.as_queue_controller() {
            let mut media_items = Vec::with_capacity(new_items.len());
            for it in &new_items {
                if let Ok((mi, _)) = self.build_media_item(&it.track, &ctrl, requested_q).await {
                    media_items.push(mi);
                }
            }
            if !media_items.is_empty() {
                let _ = qc.append_queue(&media_items).await;
            }
        }

        Ok(self.snapshot_zone(zone_id, &z))
    }

    pub async fn play_next(&self, zone_id: &str, tracks: Vec<Track>) -> Result<ZoneSnapshot> {
        let z = self
            .get_zone(zone_id)
            .ok_or_else(|| QueueError::ZoneNotFound(zone_id.into()))?;
        let _op = z.op_mu.lock().await;

        let new_items = self.wrap_tracks(tracks);
        if new_items.is_empty() {
            return Ok(self.snapshot_zone(zone_id, &z));
        }

        let (ctrl, was_empty, q_ev) = {
            let mut zi = z.inner.lock().unwrap_or_else(|e| e.into_inner());
            let was_empty = zi.queue.is_empty();
            if was_empty {
                zi.queue = new_items.clone();
                zi.original_queue = new_items;
                zi.current_index = 0;
            } else {
                let ins = ((zi.current_index + 1).max(0) as usize).min(zi.queue.len());
                zi.queue.splice(ins..ins, new_items.clone());
                zi.original_queue.extend(new_items);
            }
            zi.next_preloaded = false;
            zi.revision += 1;
            let q_ev = Self::make_items_event(zone_id, &zi);
            (Arc::clone(&zi.ctrl), was_empty, q_ev)
        };
        self.emit("zone.queue", zone_id, &q_ev);

        if was_empty {
            self.start_current_locked(zone_id, &z, &ctrl, true).await?;
        }

        Ok(self.snapshot_zone(zone_id, &z))
    }

    pub async fn remove_item(&self, zone_id: &str, index: usize) -> Result<ZoneSnapshot> {
        let z = self
            .get_zone(zone_id)
            .ok_or_else(|| QueueError::ZoneNotFound(zone_id.into()))?;
        let _op = z.op_mu.lock().await;

        let (ctrl, removed_current, q_ev) = {
            let mut zi = z.inner.lock().unwrap_or_else(|e| e.into_inner());
            if index >= zi.queue.len() {
                return Err(QueueError::InvalidIndex);
            }
            let removed = zi.queue.remove(index);
            zi.original_queue.retain(|it| it.item_id != removed.item_id);
            let idx_i32 = index as i32;
            let removed_current = idx_i32 == zi.current_index;
            if zi.queue.is_empty() {
                zi.current_index = -1;
            } else if idx_i32 < zi.current_index {
                zi.current_index -= 1;
            } else if removed_current && zi.current_index >= zi.queue.len() as i32 {
                zi.current_index = zi.queue.len() as i32 - 1;
            }
            zi.next_preloaded = false;
            zi.revision += 1;
            let q_ev = Self::make_items_event(zone_id, &zi);
            (Arc::clone(&zi.ctrl), removed_current, q_ev)
        };
        self.emit("zone.queue", zone_id, &q_ev);

        if let Some(qc) = ctrl.as_queue_controller() {
            let _ = qc.remove_queue_range(index, index).await;
        }
        if removed_current {
            let has_items = !z
                .inner
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .queue
                .is_empty();
            if has_items {
                let _ = self.start_current_locked(zone_id, &z, &ctrl, true).await;
            } else {
                let _ = ctrl.stop().await;
            }
        }

        Ok(self.snapshot_zone(zone_id, &z))
    }

    pub async fn move_item(&self, zone_id: &str, from: usize, to: usize) -> Result<ZoneSnapshot> {
        let z = self
            .get_zone(zone_id)
            .ok_or_else(|| QueueError::ZoneNotFound(zone_id.into()))?;
        let _op = z.op_mu.lock().await;

        let (ctrl, q_ev) = {
            let mut zi = z.inner.lock().unwrap_or_else(|e| e.into_inner());
            if from >= zi.queue.len() || to >= zi.queue.len() {
                return Err(QueueError::InvalidIndex);
            }
            if from != to {
                let item = zi.queue.remove(from);
                zi.queue.insert(to, item);
                let cur = zi.current_index as usize;
                if zi.current_index >= 0 {
                    if cur == from {
                        zi.current_index = to as i32;
                    } else if from < cur && to >= cur {
                        zi.current_index -= 1;
                    } else if from > cur && to <= cur {
                        zi.current_index += 1;
                    }
                }
                zi.next_preloaded = false;
                zi.revision += 1;
            }
            let q_ev = Self::make_items_event(zone_id, &zi);
            (Arc::clone(&zi.ctrl), q_ev)
        };
        self.emit("zone.queue", zone_id, &q_ev);

        if from != to
            && let Some(qc) = ctrl.as_queue_controller()
        {
            let _ = qc.move_queue_item(from, to).await;
        }

        Ok(self.snapshot_zone(zone_id, &z))
    }

    pub async fn clear(&self, zone_id: &str) -> Result<ZoneSnapshot> {
        self.set_queue(zone_id, Vec::new(), -1, false).await
    }

    // --- Transport controls ---

    pub async fn play_index(&self, zone_id: &str, index: usize) -> Result<ZoneSnapshot> {
        let z = self
            .get_zone(zone_id)
            .ok_or_else(|| QueueError::ZoneNotFound(zone_id.into()))?;
        let _op = z.op_mu.lock().await;

        let ctrl = {
            let mut zi = z.inner.lock().unwrap_or_else(|e| e.into_inner());
            if index >= zi.queue.len() {
                return Err(QueueError::InvalidIndex);
            }
            zi.current_index = index as i32;
            zi.next_preloaded = false;
            zi.revision += 1;
            Arc::clone(&zi.ctrl)
        };

        self.start_current_locked(zone_id, &z, &ctrl, false).await?;
        Ok(self.snapshot_zone(zone_id, &z))
    }

    pub async fn play(&self, zone_id: &str) -> Result<ZoneSnapshot> {
        let z = self
            .get_zone(zone_id)
            .ok_or_else(|| QueueError::ZoneNotFound(zone_id.into()))?;
        let _op = z.op_mu.lock().await;

        let (ctrl, need_load) = {
            let zi = z.inner.lock().unwrap_or_else(|e| e.into_inner());
            if zi.queue.is_empty() {
                return Err(QueueError::EmptyQueue);
            }
            let st = PlaybackState::normalize(&zi.status.state);
            let need_load = st == PlaybackState::Idle
                || st == PlaybackState::Stopped
                || zi.current_uri.is_empty();
            (Arc::clone(&zi.ctrl), need_load)
        };

        if need_load {
            self.start_current_locked(zone_id, &z, &ctrl, false).await?;
        } else {
            ctrl.play()
                .await
                .map_err(|e| QueueError::Renderer(e.to_string()))?;
            let pb = {
                let mut zi = z.inner.lock().unwrap_or_else(|e| e.into_inner());
                zi.status.state = "PLAYING".into();
                zi.position_updated_at = Utc::now();
                zi.last_command_at = Instant::now();
                zi.revision += 1;
                self.make_playback_event(zone_id, &zi)
            };
            self.emit("zone.playback", zone_id, &pb);
        }
        Ok(self.snapshot_zone(zone_id, &z))
    }

    pub async fn pause(&self, zone_id: &str) -> Result<ZoneSnapshot> {
        let z = self
            .get_zone(zone_id)
            .ok_or_else(|| QueueError::ZoneNotFound(zone_id.into()))?;
        let _op = z.op_mu.lock().await;
        let ctrl = Arc::clone(&z.inner.lock().unwrap_or_else(|e| e.into_inner()).ctrl);
        ctrl.pause()
            .await
            .map_err(|e| QueueError::Renderer(e.to_string()))?;
        let pb = {
            let mut zi = z.inner.lock().unwrap_or_else(|e| e.into_inner());
            zi.status.state = "PAUSED_PLAYBACK".into();
            zi.position_updated_at = Utc::now();
            zi.last_command_at = Instant::now();
            zi.revision += 1;
            self.make_playback_event(zone_id, &zi)
        };
        self.emit("zone.playback", zone_id, &pb);
        Ok(self.snapshot_zone(zone_id, &z))
    }

    pub async fn stop(&self, zone_id: &str) -> Result<ZoneSnapshot> {
        let z = self
            .get_zone(zone_id)
            .ok_or_else(|| QueueError::ZoneNotFound(zone_id.into()))?;
        let _op = z.op_mu.lock().await;
        let ctrl = Arc::clone(&z.inner.lock().unwrap_or_else(|e| e.into_inner()).ctrl);
        ctrl.stop()
            .await
            .map_err(|e| QueueError::Renderer(e.to_string()))?;
        let pb = {
            let mut zi = z.inner.lock().unwrap_or_else(|e| e.into_inner());
            zi.status.state = "STOPPED".into();
            zi.status.position = Duration::ZERO;
            zi.position_updated_at = Utc::now();
            zi.last_command_at = Instant::now();
            zi.revision += 1;
            self.make_playback_event(zone_id, &zi)
        };
        self.emit("zone.playback", zone_id, &pb);
        Ok(self.snapshot_zone(zone_id, &z))
    }

    pub async fn next(&self, zone_id: &str) -> Result<ZoneSnapshot> {
        let z = self
            .get_zone(zone_id)
            .ok_or_else(|| QueueError::ZoneNotFound(zone_id.into()))?;
        let _op = z.op_mu.lock().await;
        let (ctrl, next_idx) = {
            let mut zi = z.inner.lock().unwrap_or_else(|e| e.into_inner());
            if zi.queue.is_empty() {
                return Err(QueueError::EmptyQueue);
            }
            let len = zi.queue.len() as i32;
            let next_idx = if zi.current_index + 1 < len {
                zi.current_index + 1
            } else if zi.repeat == RepeatMode::All {
                0
            } else {
                return Err(QueueError::InvalidIndex);
            };
            zi.current_index = next_idx;
            zi.next_preloaded = false;
            zi.revision += 1;
            (Arc::clone(&zi.ctrl), next_idx)
        };
        if next_idx >= 0 {
            self.start_current_locked(zone_id, &z, &ctrl, false).await?;
        }
        Ok(self.snapshot_zone(zone_id, &z))
    }

    pub async fn previous(&self, zone_id: &str) -> Result<ZoneSnapshot> {
        let z = self
            .get_zone(zone_id)
            .ok_or_else(|| QueueError::ZoneNotFound(zone_id.into()))?;
        let _op = z.op_mu.lock().await;

        let (ctrl, restart_only) = {
            let mut zi = z.inner.lock().unwrap_or_else(|e| e.into_inner());
            if zi.queue.is_empty() {
                return Err(QueueError::EmptyQueue);
            }
            if zi.status.position > Duration::from_secs(3) {
                (Arc::clone(&zi.ctrl), true)
            } else {
                let len = zi.queue.len() as i32;
                let prev = if zi.current_index > 0 {
                    zi.current_index - 1
                } else if zi.repeat == RepeatMode::All {
                    len - 1
                } else {
                    0
                };
                zi.current_index = prev;
                zi.next_preloaded = false;
                zi.revision += 1;
                (Arc::clone(&zi.ctrl), false)
            }
        };

        if restart_only {
            let _ = ctrl.seek(Duration::ZERO).await;
            let pb = {
                let mut zi = z.inner.lock().unwrap_or_else(|e| e.into_inner());
                zi.status.position = Duration::ZERO;
                zi.position_updated_at = Utc::now();
                zi.revision += 1;
                self.make_playback_event(zone_id, &zi)
            };
            self.emit("zone.playback", zone_id, &pb);
        } else {
            self.start_current_locked(zone_id, &z, &ctrl, false).await?;
        }
        Ok(self.snapshot_zone(zone_id, &z))
    }

    pub async fn seek(&self, zone_id: &str, position_sec: i32) -> Result<ZoneSnapshot> {
        let z = self
            .get_zone(zone_id)
            .ok_or_else(|| QueueError::ZoneNotFound(zone_id.into()))?;
        let _op = z.op_mu.lock().await;
        let pos = Duration::from_secs(position_sec.max(0) as u64);
        let ctrl = Arc::clone(&z.inner.lock().unwrap_or_else(|e| e.into_inner()).ctrl);
        ctrl.seek(pos)
            .await
            .map_err(|e| QueueError::Renderer(e.to_string()))?;
        let pb = {
            let mut zi = z.inner.lock().unwrap_or_else(|e| e.into_inner());
            zi.status.position = pos;
            zi.position_updated_at = Utc::now();
            zi.last_command_at = Instant::now();
            zi.revision += 1;
            self.make_playback_event(zone_id, &zi)
        };
        self.emit("zone.playback", zone_id, &pb);
        Ok(self.snapshot_zone(zone_id, &z))
    }

    pub async fn set_volume(&self, zone_id: &str, volume: i32) -> Result<ZoneSnapshot> {
        let z = self
            .get_zone(zone_id)
            .ok_or_else(|| QueueError::ZoneNotFound(zone_id.into()))?;
        let _op = z.op_mu.lock().await;
        let vol = volume.clamp(0, 100);
        let ctrl = Arc::clone(&z.inner.lock().unwrap_or_else(|e| e.into_inner()).ctrl);
        ctrl.set_volume(vol)
            .await
            .map_err(|e| QueueError::Renderer(e.to_string()))?;
        let vol_ev = {
            let mut zi = z.inner.lock().unwrap_or_else(|e| e.into_inner());
            zi.status.volume = vol;
            zi.status.has_vol_value = true;
            zi.revision += 1;
            VolumeEvent {
                zone_id: zone_id.into(),
                revision: zi.revision,
                volume: zi.status.volume,
                muted: zi.status.muted,
            }
        };
        self.emit("zone.volume", zone_id, &vol_ev);
        Ok(self.snapshot_zone(zone_id, &z))
    }

    pub async fn set_mute(&self, zone_id: &str, muted: bool) -> Result<ZoneSnapshot> {
        let z = self
            .get_zone(zone_id)
            .ok_or_else(|| QueueError::ZoneNotFound(zone_id.into()))?;
        let _op = z.op_mu.lock().await;
        let ctrl = Arc::clone(&z.inner.lock().unwrap_or_else(|e| e.into_inner()).ctrl);
        ctrl.set_mute(muted)
            .await
            .map_err(|e| QueueError::Renderer(e.to_string()))?;
        let vol_ev = {
            let mut zi = z.inner.lock().unwrap_or_else(|e| e.into_inner());
            zi.status.muted = muted;
            zi.status.has_mute_val = true;
            zi.revision += 1;
            VolumeEvent {
                zone_id: zone_id.into(),
                revision: zi.revision,
                volume: zi.status.volume,
                muted: zi.status.muted,
            }
        };
        self.emit("zone.volume", zone_id, &vol_ev);
        Ok(self.snapshot_zone(zone_id, &z))
    }

    pub async fn set_shuffle(&self, zone_id: &str, enabled: bool) -> Result<ZoneSnapshot> {
        let z = self
            .get_zone(zone_id)
            .ok_or_else(|| QueueError::ZoneNotFound(zone_id.into()))?;
        let _op = z.op_mu.lock().await;
        let (q_ev, pb_ev) = {
            let mut zi = z.inner.lock().unwrap_or_else(|e| e.into_inner());
            if zi.shuffle != enabled {
                zi.shuffle = enabled;
                let cur_id = current_item_pair(&zi).0;
                if enabled {
                    let mut rest: Vec<Item> = zi
                        .queue
                        .iter()
                        .filter(|it| it.item_id != cur_id)
                        .cloned()
                        .collect();
                    let mut rng = rand::rng();
                    rest.shuffle(&mut rng);
                    let mut next_q = Vec::with_capacity(zi.queue.len());
                    if let Some(cur) = zi.queue.iter().find(|it| it.item_id == cur_id).cloned() {
                        next_q.push(cur);
                        zi.current_index = 0;
                    }
                    next_q.extend(rest);
                    zi.queue = next_q;
                } else {
                    zi.queue = zi.original_queue.clone();
                    zi.current_index = zi
                        .queue
                        .iter()
                        .position(|it| it.item_id == cur_id)
                        .map(|i| i as i32)
                        .unwrap_or(if zi.queue.is_empty() { -1 } else { 0 });
                }
                zi.next_preloaded = false;
                zi.next_uri.clear();
                zi.next_index = -1;
                zi.revision += 1;
            }
            (
                Self::make_items_event(zone_id, &zi),
                self.make_playback_event(zone_id, &zi),
            )
        };
        self.emit("zone.queue", zone_id, &q_ev);
        self.emit("zone.playback", zone_id, &pb_ev);
        Ok(self.snapshot_zone(zone_id, &z))
    }

    pub async fn set_repeat(&self, zone_id: &str, mode: RepeatMode) -> Result<ZoneSnapshot> {
        let z = self
            .get_zone(zone_id)
            .ok_or_else(|| QueueError::ZoneNotFound(zone_id.into()))?;
        let _op = z.op_mu.lock().await;
        let (ctrl, shuffle, pb_ev) = {
            let mut zi = z.inner.lock().unwrap_or_else(|e| e.into_inner());
            zi.repeat = mode;
            zi.next_preloaded = false;
            zi.next_uri.clear();
            zi.next_index = -1;
            zi.revision += 1;
            (
                Arc::clone(&zi.ctrl),
                zi.shuffle,
                self.make_playback_event(zone_id, &zi),
            )
        };
        if let Some(qc) = ctrl.as_queue_controller() {
            let _ = qc.set_loop_mode(mode.as_str(), shuffle).await;
        }
        self.emit("zone.playback", zone_id, &pb_ev);
        Ok(self.snapshot_zone(zone_id, &z))
    }

    pub async fn set_audio_quality(
        &self,
        zone_id: &str,
        quality: AudioQuality,
    ) -> Result<ZoneSnapshot> {
        let z = self
            .get_zone(zone_id)
            .ok_or_else(|| QueueError::ZoneNotFound(zone_id.into()))?;
        let _op = z.op_mu.lock().await;
        let (ctrl, need_reload, pb_ev) = {
            let mut zi = z.inner.lock().unwrap_or_else(|e| e.into_inner());
            zi.audio_quality = quality;
            zi.next_preloaded = false;
            zi.next_uri.clear();
            zi.next_index = -1;
            zi.revision += 1;
            let has_native_q = zi.ctrl.as_queue_controller().is_some()
                && !zi.queue.is_empty()
                && zi.current_index >= 0;
            (
                Arc::clone(&zi.ctrl),
                has_native_q,
                self.make_playback_event(zone_id, &zi),
            )
        };
        if need_reload && ctrl.as_queue_controller().is_some() {
            let _ = self.start_current_locked(zone_id, &z, &ctrl, true).await;
        }
        self.emit("zone.playback", zone_id, &pb_ev);
        Ok(self.snapshot_zone(zone_id, &z))
    }

    async fn start_current_locked(
        &self,
        zone_id: &str,
        z: &Zone,
        ctrl: &Arc<dyn Controller>,
        use_native_queue: bool,
    ) -> Result<()> {
        let (queue_items, cur_idx, requested_q) = {
            let zi = z.inner.lock().unwrap_or_else(|e| e.into_inner());
            (zi.queue.clone(), zi.current_index, zi.audio_quality)
        };
        if cur_idx < 0 || (cur_idx as usize) >= queue_items.len() {
            return Err(QueueError::EmptyQueue);
        }
        let cur_item = &queue_items[cur_idx as usize];

        let (media_item, eff_q) = self
            .build_media_item(&cur_item.track, ctrl, requested_q)
            .await?;

        if use_native_queue && let Some(qc) = ctrl.as_queue_controller() {
            let mut all_media = Vec::with_capacity(queue_items.len());
            for (i, it) in queue_items.iter().enumerate() {
                if i == cur_idx as usize {
                    all_media.push(media_item.clone());
                } else if let Ok((mi, _)) =
                    self.build_media_item(&it.track, ctrl, requested_q).await
                {
                    all_media.push(mi);
                }
            }
            if !all_media.is_empty() && qc.replace_queue(&all_media, cur_idx as usize).await.is_ok()
            {
                let pb = {
                    let mut zi = z.inner.lock().unwrap_or_else(|e| e.into_inner());
                    zi.current_uri = media_item.url.clone();
                    zi.active_quality = Some(eff_q);
                    zi.status.state = "PLAYING".into();
                    zi.status.position = Duration::ZERO;
                    zi.status.duration = media_item.duration;
                    zi.position_updated_at = Utc::now();
                    zi.last_command_at = Instant::now();
                    zi.revision += 1;
                    if zi.last_played_item_id != cur_item.item_id {
                        zi.last_played_item_id = cur_item.item_id.clone();
                        self.catalog.record_play(&cur_item.track);
                    }
                    self.make_playback_event(zone_id, &zi)
                };
                self.emit("zone.playback", zone_id, &pb);
                return Ok(());
            }
        }

        ctrl.set_uri(&media_item)
            .await
            .map_err(|e| QueueError::Renderer(e.to_string()))?;
        ctrl.play()
            .await
            .map_err(|e| QueueError::Renderer(e.to_string()))?;

        let pb = {
            let mut zi = z.inner.lock().unwrap_or_else(|e| e.into_inner());
            zi.current_uri = media_item.url;
            zi.active_quality = Some(eff_q);
            zi.status.state = "PLAYING".into();
            zi.status.position = Duration::ZERO;
            zi.status.duration = media_item.duration;
            zi.position_updated_at = Utc::now();
            zi.last_command_at = Instant::now();
            zi.next_preloaded = false;
            zi.revision += 1;
            if zi.last_played_item_id != cur_item.item_id {
                zi.last_played_item_id = cur_item.item_id.clone();
                self.catalog.record_play(&cur_item.track);
            }
            self.make_playback_event(zone_id, &zi)
        };
        self.emit("zone.playback", zone_id, &pb);
        Ok(())
    }

    /// Handles push events from UPnP GENA or remote HTTP/SSE players.
    pub fn apply_device_event(&self, ev: DeviceEvent) {
        let Some(z) = self.get_zone(&ev.renderer_id) else {
            return;
        };
        let (vol_ev, pb_ev) = {
            let mut zi = z.inner.lock().unwrap_or_else(|e| e.into_inner());
            let mut vol_changed = false;
            let mut pb_changed = false;

            if ev.has_volume && zi.status.volume != ev.volume {
                zi.status.volume = ev.volume;
                zi.status.has_vol_value = true;
                vol_changed = true;
            }
            if ev.has_mute && zi.status.muted != ev.muted {
                zi.status.muted = ev.muted;
                zi.status.has_mute_val = true;
                vol_changed = true;
            }
            if ev.has_state && zi.status.state != ev.state {
                zi.status.state = ev.state;
                pb_changed = true;
            }
            if ev.has_position {
                zi.status.position = ev.position;
                zi.status.has_position = true;
                zi.position_updated_at = Utc::now();
                pb_changed = true;
            }
            if ev.has_duration && !ev.duration.is_zero() {
                zi.status.duration = ev.duration;
                pb_changed = true;
            }
            if ev.has_track_uri && !ev.track_uri.is_empty() {
                zi.status.track_uri = ev.track_uri;
            }
            if vol_changed || pb_changed {
                zi.revision += 1;
            }
            let ve = vol_changed.then(|| VolumeEvent {
                zone_id: ev.renderer_id.clone(),
                revision: zi.revision,
                volume: zi.status.volume,
                muted: zi.status.muted,
            });
            let pe = pb_changed.then(|| self.make_playback_event(&ev.renderer_id, &zi));
            (ve, pe)
        };

        if let Some(ve) = vol_ev {
            self.emit("zone.volume", &ev.renderer_id, &ve);
        }
        if let Some(pe) = pb_ev {
            self.emit("zone.playback", &ev.renderer_id, &pe);
        }
    }

    fn start_poll_loop(self: &Arc<Self>) {
        let mgr = Arc::clone(self);
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_secs(2));
            loop {
                tokio::select! {
                    _ = mgr.stop.cancelled() => break,
                    _ = tick.tick() => {
                        mgr.poll_all_zones().await;
                    }
                }
            }
        });
    }

    async fn poll_all_zones(&self) {
        let snapshot: Vec<(String, Arc<Zone>)> = {
            let g = self.zones.read().unwrap_or_else(|e| e.into_inner());
            g.iter().map(|(k, v)| (k.clone(), Arc::clone(v))).collect()
        };

        for (zone_id, z) in snapshot {
            let ctrl = Arc::clone(&z.inner.lock().unwrap_or_else(|e| e.into_inner()).ctrl);
            if ctrl.push_connected() {
                continue;
            }
            let Ok(st) = ctrl.status().await else {
                continue;
            };

            let mut need_advance = false;
            let mut need_preload: Option<(i32, Track, AudioQuality)> = None;
            let (vol_ev, pb_ev) = {
                let mut zi = z.inner.lock().unwrap_or_else(|e| e.into_inner());
                let prev_state = PlaybackState::normalize(&zi.status.state);
                let new_state = PlaybackState::normalize(&st.state);

                let mut vol_changed = false;
                let mut pb_changed = false;

                if st.has_vol_value && st.volume != zi.status.volume {
                    zi.status.volume = st.volume;
                    vol_changed = true;
                }
                if st.has_mute_val && st.muted != zi.status.muted {
                    zi.status.muted = st.muted;
                    vol_changed = true;
                }
                if st.state != zi.status.state {
                    zi.status.state = st.state.clone();
                    pb_changed = true;
                }
                if st.has_position {
                    let diff = zi.status.position.abs_diff(st.position);
                    zi.status.position = st.position;
                    zi.position_updated_at = Utc::now();
                    if diff >= Duration::from_secs(1) {
                        pb_changed = true;
                    }
                }
                if !st.duration.is_zero() {
                    zi.status.duration = st.duration;
                }

                // Sync native queue index if device advanced on its own (e.g. WiiM hardware Next)
                if st.has_queue_index
                    && st.queue_index >= 0
                    && (st.queue_index as usize) < zi.queue.len()
                    && st.queue_index != zi.current_index
                    && zi.last_command_at.elapsed() > Duration::from_secs(3)
                {
                    zi.current_index = st.queue_index;
                    pb_changed = true;
                }

                // Detect gapless transition via TrackURI
                if zi.next_preloaded
                    && !st.track_uri.is_empty()
                    && st.track_uri == zi.next_uri
                    && zi.next_index >= 0
                    && (zi.next_index as usize) < zi.queue.len()
                {
                    zi.current_index = zi.next_index;
                    zi.current_uri = zi.next_uri.clone();
                    zi.next_preloaded = false;
                    zi.next_uri.clear();
                    zi.next_index = -1;
                    pb_changed = true;
                    let tr = zi.queue[zi.current_index as usize].track.clone();
                    self.catalog.record_play(&tr);
                }

                // Detect track completion (PLAYING -> STOPPED) for virtual queue advance
                if prev_state == PlaybackState::Playing
                    && new_state == PlaybackState::Stopped
                    && ctrl.as_queue_controller().is_none()
                    && zi.last_command_at.elapsed() > Duration::from_secs(3)
                {
                    need_advance = true;
                }

                // Arm gapless SetNextAVTransportURI when within 25s of end of track
                if new_state == PlaybackState::Playing
                    && !zi.next_preloaded
                    && !zi.set_next_unsupported
                    && ctrl.as_queue_controller().is_none()
                    && !zi.status.duration.is_zero()
                    && zi.status.duration.saturating_sub(zi.status.position)
                        <= Duration::from_secs(25)
                {
                    let ni = compute_next_index(&zi);
                    if let Some(n) = ni {
                        need_preload =
                            Some((n, zi.queue[n as usize].track.clone(), zi.audio_quality));
                    }
                }

                if vol_changed || pb_changed {
                    zi.revision += 1;
                }
                let ve = vol_changed.then(|| VolumeEvent {
                    zone_id: zone_id.clone(),
                    revision: zi.revision,
                    volume: zi.status.volume,
                    muted: zi.status.muted,
                });
                let pe = pb_changed.then(|| self.make_playback_event(&zone_id, &zi));
                (ve, pe)
            };

            if let Some(ve) = vol_ev {
                self.emit("zone.volume", &zone_id, &ve);
            }
            if let Some(pe) = pb_ev {
                self.emit("zone.playback", &zone_id, &pe);
            }

            if let Some((ni, tr, rq)) = need_preload
                && let Ok((mi, _)) = self.build_media_item(&tr, &ctrl, rq).await
            {
                match ctrl.set_next_uri(&mi).await {
                    Ok(()) => {
                        let mut zi = z.inner.lock().unwrap_or_else(|e| e.into_inner());
                        zi.next_preloaded = true;
                        zi.next_uri = mi.url;
                        zi.next_index = ni;
                    }
                    Err(e) if e.is_action_unsupported() => {
                        z.inner
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .set_next_unsupported = true;
                    }
                    Err(_) => {}
                }
            }

            if need_advance {
                let next_idx = {
                    let zi = z.inner.lock().unwrap_or_else(|e| e.into_inner());
                    compute_next_index(&zi)
                };
                if let Some(ni) = next_idx
                    && let Err(e) = self.play_index(&zone_id, ni as usize).await
                {
                    self.emit_zone_error(&zone_id, "ADVANCE_FAILED", e.to_string(), true);
                }
            }
        }
    }
}

fn current_item_pair(zi: &ZoneInner) -> (String, Option<Track>) {
    if zi.current_index >= 0 && (zi.current_index as usize) < zi.queue.len() {
        let it = &zi.queue[zi.current_index as usize];
        (it.item_id.clone(), Some(it.track.clone()))
    } else {
        (String::new(), None)
    }
}

fn compute_next_index(zi: &ZoneInner) -> Option<i32> {
    if zi.queue.is_empty() {
        return None;
    }
    if zi.repeat == RepeatMode::One && zi.current_index >= 0 {
        return Some(zi.current_index);
    }
    let len = zi.queue.len() as i32;
    if zi.current_index + 1 < len {
        Some(zi.current_index + 1)
    } else if zi.repeat == RepeatMode::All {
        Some(0)
    } else {
        None
    }
}

fn transcoded_mime(q: AudioQuality, container: &str) -> &'static str {
    if q == AudioQuality::Wav {
        return "audio/wav";
    }
    if container == "adts" {
        return "audio/aac";
    }
    "audio/ogg; codecs=\"opus\""
}
