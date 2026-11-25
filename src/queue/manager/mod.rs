mod ops;
mod sync;

use std::collections::HashMap;
use std::io::Write;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use serde::Serialize;
use tokio::sync::{Mutex as AsyncMutex, broadcast};
use tokio_util::sync::CancellationToken;

use crate::library::{Catalog, SOURCE_LOCAL, SOURCE_QOBUZ, Track};
use crate::media::detect_transcoder;
use crate::qobuz::Client as QobuzClient;
use crate::upnp::{Controller, GenaClient, MediaItem, RendererInfo, TransportStatus};

use super::types::{
    AudioQuality, Event, Item, ItemsEvent, PlaybackEvent, PlaybackState, QueueError, RepeatMode,
    Result, StreamMode, ZoneErrorEvent, ZoneErrorPayload, ZoneSnapshot, resolve_track_quality,
};

pub(super) struct ZoneInner {
    pub(super) ctrl: Arc<dyn Controller>,
    pub(super) queue: Vec<Item>,
    pub(super) original_queue: Vec<Item>,
    pub(super) current_index: i32,
    pub(super) shuffle: bool,
    pub(super) repeat: RepeatMode,
    pub(super) revision: u64,
    pub(super) status: TransportStatus,
    pub(super) position_updated_at: DateTime<Utc>,
    pub(super) current_uri: String,
    pub(super) next_uri: String,
    pub(super) next_index: i32,
    pub(super) audio_quality: AudioQuality,
    pub(super) active_quality: Option<AudioQuality>,
    pub(super) next_preloaded: bool,
    pub(super) set_next_unsupported: bool,
    pub(super) last_played_item_id: String,
    pub(super) last_command_at: Instant,
}

pub(super) struct Zone {
    pub(super) op_mu: AsyncMutex<()>,
    pub(super) inner: Mutex<ZoneInner>,
}

pub struct Manager {
    pub(super) catalog: Arc<Catalog>,
    pub(super) qobuz: Option<Arc<QobuzClient>>,
    server_port: u16,
    public_url: RwLock<String>,
    stream_mode: RwLock<StreamMode>,
    pub(super) zones: RwLock<HashMap<String, Arc<Zone>>>,
    gena: RwLock<Option<Arc<GenaClient>>>,
    item_seq: AtomicU64,
    event_seq: AtomicU64,
    err_seq: AtomicU64,
    events_tx: broadcast::Sender<Event>,
    event_log: Mutex<Option<Box<dyn Write + Send>>>,
    pub(super) stop: CancellationToken,
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
            event_log: Mutex::new(None),
            stop: CancellationToken::new(),
        });
        mgr.start_poll_loop();
        mgr
    }

    /// Configures an optional JSONL writer for raw event tracing (GENA, polls, SSE).
    pub fn set_event_log(&self, writer: Option<Box<dyn Write + Send>>) {
        *self.event_log.lock().unwrap_or_else(|e| e.into_inner()) = writer;
    }

    pub(super) fn log_event<T: Serialize>(&self, src: &str, zone_id: &str, data: &T) {
        let enabled = self
            .event_log
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .is_some();
        if !enabled {
            return;
        }
        #[derive(Serialize)]
        #[serde(rename_all = "camelCase")]
        struct LogEntry<'a, T: Serialize> {
            ts: String,
            src: &'a str,
            #[serde(skip_serializing_if = "str::is_empty")]
            zone_id: &'a str,
            data: &'a T,
        }
        let Ok(mut line) = serde_json::to_vec(&LogEntry {
            ts: Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Nanos, true),
            src,
            zone_id,
            data,
        }) else {
            return;
        };
        line.push(b'\n');
        let mut guard = self.event_log.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(w) = guard.as_mut() {
            let _ = w.write_all(&line);
        }
    }

    pub fn set_gena(&self, gena: Arc<GenaClient>) {
        *self.gena.write().unwrap_or_else(|e| e.into_inner()) = Some(gena);
    }

    pub fn set_public_url(&self, url: impl Into<String>) {
        let u = url.into().trim().trim_end_matches('/').to_string();
        *self.public_url.write().unwrap_or_else(|e| e.into_inner()) = u;
    }

    pub(super) fn base_url_for(&self, ctrl: &dyn Controller) -> String {
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

    pub(super) fn emit<T: Serialize>(&self, event_type: &str, zone_id: &str, data: &T) {
        let Ok(val) = serde_json::to_value(data) else {
            return;
        };
        let id = self.event_seq.fetch_add(1, Ordering::Relaxed) + 1;
        let ev = Event {
            id,
            event_type: event_type.into(),
            zone_id: zone_id.into(),
            data: val,
        };
        self.log_event("sse", zone_id, &ev);
        let _ = self.events_tx.send(ev);
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
                let old_info = zi.ctrl.info();
                // Keep live controller instance intact when endpoint is unchanged so periodic
                // discovery sweeps do not tear down active Cast sessions or UPnP SOAP locks.
                if old_info.location != info.location || old_info.host != info.host {
                    zi.ctrl = ctrl;
                }
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

    pub(super) fn get_zone(&self, zone_id: &str) -> Option<Arc<Zone>> {
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

    pub(super) fn snapshot_zone(&self, zone_id: &str, z: &Zone) -> ZoneSnapshot {
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

    pub(super) fn make_playback_event(&self, zone_id: &str, zi: &ZoneInner) -> PlaybackEvent {
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

    pub(super) fn make_items_event(zone_id: &str, zi: &ZoneInner) -> ItemsEvent {
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

    pub(super) fn emit_zone_error(
        &self,
        zone_id: &str,
        code: &str,
        message: String,
        retryable: bool,
    ) {
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

    pub(super) fn wrap_tracks(&self, tracks: Vec<Track>) -> Vec<Item> {
        tracks
            .into_iter()
            .map(|track| Item {
                item_id: format!("qi-{}", self.item_seq.fetch_add(1, Ordering::Relaxed) + 1),
                track,
            })
            .collect()
    }

    pub(super) async fn build_media_item(
        &self,
        track: &Track,
        ctrl: &Arc<dyn Controller>,
        requested_quality: AudioQuality,
    ) -> Result<(MediaItem, AudioQuality)> {
        let info = ctrl.info();
        let tc = detect_transcoder();
        let (eff_q, container) =
            resolve_track_quality(requested_quality, track, &info, tc.available());
        let base = if ctrl.prefer_relative_media() {
            String::new()
        } else {
            self.base_url_for(ctrl.as_ref())
        };
        let artwork_url = if track.artwork_url.starts_with('/') {
            format!("{base}{}", track.artwork_url)
        } else {
            track.artwork_url.clone()
        };

        let resolved = self
            .resolve_media_url(track, ctrl, &base, eff_q, container)
            .await?;

        Ok((
            MediaItem {
                id: track.id.clone(),
                url: resolved.url,
                title: track.title.clone(),
                artist: track.artist.clone(),
                album: track.album.clone(),
                album_art_url: artwork_url,
                track_number: track.track_number,
                mime: resolved.mime,
                duration: Duration::from_secs(track.duration_sec.max(0) as u64),
                size: if resolved.transcoded { 0 } else { track.size },
                sample_rate: track.sample_rate,
                bit_depth: track.bit_depth,
                channels: if track.channels > 0 {
                    track.channels
                } else {
                    2
                },
                seekable: resolved.seekable,
                transcoded: resolved.transcoded,
            },
            eff_q,
        ))
    }

    async fn resolve_media_url(
        &self,
        track: &Track,
        ctrl: &Arc<dyn Controller>,
        base: &str,
        eff_q: AudioQuality,
        container: &str,
    ) -> Result<ResolvedMedia> {
        let mut mime = track.mime.clone();
        let mut seekable = true;
        let mut transcoded = false;

        let url = if track.source == SOURCE_LOCAL {
            let ext = if track.ext.is_empty() {
                ".flac"
            } else {
                &track.ext
            };
            let mut u = format!("{base}/media/local/{}{ext}", track.id);
            if eff_q.needs_transcode() {
                append_transcode_params(&mut u, eff_q, container);
                mime = transcoded_mime(eff_q, container).into();
                seekable = false;
                transcoded = true;
            }
            u
        } else if track.source == SOURCE_QOBUZ {
            let qid = qobuz_track_id(track);
            let delivery = ctrl.qobuz_delivery();
            let use_relay = eff_q.needs_transcode()
                || delivery == "force"
                || (delivery != "direct" && self.stream_mode() == StreamMode::Relay);
            if use_relay {
                let mut u = format!("{base}/media/qobuz/{qid}.flac");
                if eff_q.needs_transcode() {
                    append_transcode_params(&mut u, eff_q, container);
                    mime = transcoded_mime(eff_q, container).into();
                    seekable = false;
                    transcoded = true;
                }
                u
            } else {
                let qb = self
                    .qobuz
                    .as_ref()
                    .ok_or_else(|| QueueError::Renderer("qobuz not configured".into()))?;
                let si = qb
                    .stream_url(qid)
                    .await
                    .map_err(|e| QueueError::Renderer(e.to_string()))?;
                mime = si.mime;
                si.url
            }
        } else {
            return Err(QueueError::Renderer(format!(
                "unsupported track source {:?}",
                track.source
            )));
        };

        if mime.is_empty() || mime == "audio/x-flac" {
            mime = "audio/flac".into();
        }
        Ok(ResolvedMedia {
            url,
            mime,
            seekable,
            transcoded,
        })
    }
}

struct ResolvedMedia {
    url: String,
    mime: String,
    seekable: bool,
    transcoded: bool,
}

fn append_transcode_params(url: &mut String, eff_q: AudioQuality, container: &str) {
    url.push_str(&format!("?q={}", eff_q.as_str()));
    if !container.is_empty() {
        url.push_str(&format!("&c={container}"));
    }
}

fn qobuz_track_id(track: &Track) -> i64 {
    if track.qobuz_track_id > 0 {
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
    }
}

pub(super) fn current_item_pair(zi: &ZoneInner) -> (String, Option<Track>) {
    if zi.current_index >= 0 && (zi.current_index as usize) < zi.queue.len() {
        let it = &zi.queue[zi.current_index as usize];
        (it.item_id.clone(), Some(it.track.clone()))
    } else {
        (String::new(), None)
    }
}

pub(super) fn compute_next_index(zi: &ZoneInner) -> Option<i32> {
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
