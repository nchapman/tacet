use std::sync::Arc;
use std::time::{Duration, Instant};

use chrono::Utc;

use crate::library::{PlayContext, PlayEventRecord, RecentPlayRecency, SOURCE_LOCAL, Track};
use crate::upnp::Controller;

use super::listen::{FlushReason, flush_active_listen, sync_active_listen};
use super::shuffle::{FRESH_SHUFFLE_WINDOW_SECS, fresh_shuffle};
use super::{Manager, Zone, current_item_pair};
use crate::queue::types::{
    AudioQuality, Item, PlaybackState, QueueError, RepeatMode, Result, VolumeEvent, ZoneSnapshot,
    is_dsd_track,
};

impl Manager {
    pub async fn set_queue(
        &self,
        zone_id: &str,
        tracks: Vec<Track>,
        start_index: i32,
        autoplay: bool,
    ) -> Result<ZoneSnapshot> {
        self.set_queue_with_context(zone_id, tracks, start_index, autoplay, None)
            .await
    }

    pub async fn set_queue_with_context(
        &self,
        zone_id: &str,
        tracks: Vec<Track>,
        start_index: i32,
        autoplay: bool,
        source_context: Option<PlayContext>,
    ) -> Result<ZoneSnapshot> {
        let z = self
            .get_zone(zone_id)
            .ok_or_else(|| QueueError::ZoneNotFound(zone_id.into()))?;
        let _op = z.op_mu.lock().await;
        self.set_queue_with_context_locked(
            zone_id,
            &z,
            tracks,
            start_index,
            autoplay,
            source_context,
        )
        .await
    }

    async fn set_queue_with_context_locked(
        &self,
        zone_id: &str,
        z: &Arc<Zone>,
        tracks: Vec<Track>,
        start_index: i32,
        autoplay: bool,
        source_context: Option<PlayContext>,
    ) -> Result<ZoneSnapshot> {
        let items = self.wrap_tracks_with_context(tracks, source_context);
        let start = if items.is_empty() {
            -1
        } else {
            start_index.clamp(0, items.len() as i32 - 1)
        };

        let (ctrl, q_ev, flushed) = {
            let mut zi = z.inner.lock().unwrap_or_else(|e| e.into_inner());
            let flushed = flush_active_listen(&mut zi, zone_id, FlushReason::Interrupted);
            zi.original_queue = items.clone();
            zi.queue = items;
            zi.current_index = start;
            zi.next_preloaded = false;
            zi.next_uri.clear();
            zi.next_index = -1;
            zi.revision += 1;
            let q_ev = Self::make_items_event(zone_id, &zi);
            (Arc::clone(&zi.ctrl), q_ev, flushed)
        };
        if let Some(rec) = flushed {
            self.record_play_event_async(rec);
        }
        self.emit("zone.queue", zone_id, &q_ev);
        self.persist_zone_queues();

        if start >= 0 && autoplay {
            self.start_current_locked(zone_id, z, &ctrl, true).await?;
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

        Ok(self.snapshot_zone(zone_id, z))
    }

    async fn sync_native_radio_tail(
        &self,
        ctrl: &Arc<dyn Controller>,
        cur_idx: usize,
        old_len: usize,
        tail_items: &[Item],
        requested_q: AudioQuality,
    ) -> bool {
        let Some(qc) = ctrl.as_queue_controller() else {
            return true;
        };
        let mut media_items = Vec::with_capacity(tail_items.len());
        for item in tail_items {
            match self.build_media_item(&item.track, ctrl, requested_q).await {
                Ok((mi, _)) => media_items.push(mi),
                Err(_) => return false,
            }
        }
        if old_len > cur_idx + 1 && qc.remove_queue_range(cur_idx + 1, old_len).await.is_err() {
            return false;
        }
        if !media_items.is_empty() && qc.append_queue(&media_items).await.is_err() {
            return false;
        }
        true
    }

    /// Starts a radio station on `zone_id`.
    /// If `seed_track_id` matches the currently active (PLAYING/PAUSED) track in the zone,
    /// keeps that track playing seamlessly and replaces the upcoming queue with the rest of
    /// the station; otherwise replaces the queue starting at index 0 and begins playback.
    pub async fn start_radio_queue(
        &self,
        zone_id: &str,
        seed_track_id: Option<&str>,
        tracks: Vec<Track>,
        source_context: PlayContext,
    ) -> Result<ZoneSnapshot> {
        let z = self
            .get_zone(zone_id)
            .ok_or_else(|| QueueError::ZoneNotFound(zone_id.into()))?;

        let was_paused = {
            let _op = z.op_mu.lock().await;
            let match_info = seed_track_id.and_then(|sid| {
                let zi = z.inner.lock().unwrap_or_else(|e| e.into_inner());
                let cur = usize::try_from(zi.current_index).ok()?;
                let active_state = zi.status.state == "PLAYING" || zi.status.state == "PAUSED";
                let it = zi.queue.get(cur)?;
                if active_state && it.track.id == sid {
                    Some((
                        cur,
                        zi.queue.len(),
                        zi.status.state == "PAUSED",
                        Arc::clone(&zi.ctrl),
                        zi.audio_quality,
                    ))
                } else {
                    None
                }
            });

            let Some((cur_idx, old_len, paused, ctrl, requested_q)) = match_info else {
                return self
                    .set_queue_with_context_locked(
                        zone_id,
                        &z,
                        tracks,
                        0,
                        true,
                        Some(source_context),
                    )
                    .await;
            };

            let sid = seed_track_id.unwrap_or_default();
            let tail_tracks: Vec<Track> =
                tracks.iter().skip_while(|t| t.id == sid).cloned().collect();
            let tail_items = self.wrap_tracks_with_context(tail_tracks, Some(source_context));

            if !self
                .sync_native_radio_tail(&ctrl, cur_idx, old_len, &tail_items, requested_q)
                .await
            {
                return self
                    .set_queue_with_context_locked(
                        zone_id,
                        &z,
                        tracks,
                        0,
                        true,
                        Some(source_context),
                    )
                    .await;
            }

            let q_ev = {
                let mut zi = z.inner.lock().unwrap_or_else(|e| e.into_inner());
                zi.queue.truncate(cur_idx + 1);
                zi.queue.extend(tail_items);
                zi.original_queue = zi.queue.clone();
                zi.next_preloaded = false;
                zi.next_uri.clear();
                zi.next_index = -1;
                zi.revision += 1;
                Self::make_items_event(zone_id, &zi)
            };
            self.emit("zone.queue", zone_id, &q_ev);
            self.persist_zone_queues();
            paused
        };

        if was_paused {
            let _ = self.play(zone_id).await;
        }
        Ok(self.snapshot_zone(zone_id, &z))
    }

    pub async fn enqueue(&self, zone_id: &str, tracks: Vec<Track>) -> Result<ZoneSnapshot> {
        self.enqueue_with_autoplay(zone_id, tracks, true).await
    }

    pub async fn enqueue_with_autoplay(
        &self,
        zone_id: &str,
        tracks: Vec<Track>,
        autoplay: bool,
    ) -> Result<ZoneSnapshot> {
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
        self.persist_zone_queues();

        if was_empty {
            if autoplay {
                self.start_current_locked(zone_id, &z, &ctrl, true).await?;
            }
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
        self.play_next_with_autoplay(zone_id, tracks, true).await
    }

    pub async fn play_next_with_autoplay(
        &self,
        zone_id: &str,
        tracks: Vec<Track>,
        autoplay: bool,
    ) -> Result<ZoneSnapshot> {
        let z = self
            .get_zone(zone_id)
            .ok_or_else(|| QueueError::ZoneNotFound(zone_id.into()))?;
        let _op = z.op_mu.lock().await;

        let new_items = self.wrap_tracks(tracks);
        if new_items.is_empty() {
            return Ok(self.snapshot_zone(zone_id, &z));
        }

        let (ctrl, was_empty, insert_at, prev_len, added_tracks, q_ev) = {
            let mut zi = z.inner.lock().unwrap_or_else(|e| e.into_inner());
            let was_empty = zi.queue.is_empty();
            let prev_len = zi.queue.len();
            let added_tracks: Vec<Track> = new_items.iter().map(|i| i.track.clone()).collect();
            let insert_at = if was_empty {
                zi.queue = new_items.clone();
                zi.original_queue = new_items;
                zi.current_index = 0;
                0
            } else {
                let ins = ((zi.current_index + 1).max(0) as usize).min(zi.queue.len());
                zi.queue.splice(ins..ins, new_items.clone());
                zi.original_queue.extend(new_items);
                ins
            };
            zi.next_preloaded = false;
            zi.next_uri.clear();
            zi.next_index = -1;
            zi.revision += 1;
            let q_ev = Self::make_items_event(zone_id, &zi);
            (
                Arc::clone(&zi.ctrl),
                was_empty,
                insert_at,
                prev_len,
                added_tracks,
                q_ev,
            )
        };
        self.emit("zone.queue", zone_id, &q_ev);
        self.persist_zone_queues();

        if was_empty {
            if autoplay {
                self.start_current_locked(zone_id, &z, &ctrl, true).await?;
            }
        } else if ctrl.as_queue_controller().is_some() {
            self.sync_native_play_next(zone_id, &z, &added_tracks, prev_len, insert_at)
                .await;
        }

        Ok(self.snapshot_zone(zone_id, &z))
    }

    async fn sync_native_play_next(
        &self,
        zone_id: &str,
        z: &Zone,
        added_tracks: &[Track],
        prev_len: usize,
        insert_at: usize,
    ) {
        let (ctrl, req_q) = {
            let zi = z.inner.lock().unwrap_or_else(|e| e.into_inner());
            (Arc::clone(&zi.ctrl), zi.audio_quality)
        };
        let Some(qc) = ctrl.as_queue_controller() else {
            return;
        };
        let mut media_items = Vec::with_capacity(added_tracks.len());
        for tr in added_tracks {
            if let Ok((mi, _)) = self.build_media_item(tr, &ctrl, req_q).await {
                media_items.push(mi);
            }
        }
        if media_items.is_empty() {
            return;
        }
        if let Err(e) = qc.append_queue(&media_items).await {
            tracing::warn!(zone = %zone_id, error = %e, "native append_queue failed during play_next");
            return;
        }
        for i in 0..media_items.len() {
            if let Err(e) = qc.move_queue_item(prev_len + i, insert_at + i).await {
                tracing::warn!(zone = %zone_id, error = %e, "native move_queue_item failed during play_next");
                break;
            }
        }
    }

    pub async fn remove_item(&self, zone_id: &str, index: usize) -> Result<ZoneSnapshot> {
        let z = self
            .get_zone(zone_id)
            .ok_or_else(|| QueueError::ZoneNotFound(zone_id.into()))?;
        let _op = z.op_mu.lock().await;
        self.remove_item_locked(zone_id, &z, index).await
    }

    pub async fn remove_item_by_id(&self, zone_id: &str, item_id: &str) -> Result<ZoneSnapshot> {
        let z = self
            .get_zone(zone_id)
            .ok_or_else(|| QueueError::ZoneNotFound(zone_id.into()))?;
        let _op = z.op_mu.lock().await;
        let idx = {
            let zi = z.inner.lock().unwrap_or_else(|e| e.into_inner());
            zi.queue
                .iter()
                .position(|it| it.item_id == item_id)
                .ok_or(QueueError::InvalidIndex)?
        };
        self.remove_item_locked(zone_id, &z, idx).await
    }

    async fn remove_item_locked(
        &self,
        zone_id: &str,
        z: &Zone,
        index: usize,
    ) -> Result<ZoneSnapshot> {
        let (ctrl, removed_current, q_ev, flushed) = {
            let mut zi = z.inner.lock().unwrap_or_else(|e| e.into_inner());
            if index >= zi.queue.len() {
                return Err(QueueError::InvalidIndex);
            }
            let idx_i32 = index as i32;
            let removed_current = idx_i32 == zi.current_index;
            let flushed = if removed_current {
                flush_active_listen(&mut zi, zone_id, FlushReason::UserSkip)
            } else {
                None
            };
            let removed = zi.queue.remove(index);
            zi.original_queue.retain(|it| it.item_id != removed.item_id);
            if zi.queue.is_empty() {
                zi.current_index = -1;
            } else if idx_i32 < zi.current_index {
                zi.current_index -= 1;
            } else if removed_current && zi.current_index >= zi.queue.len() as i32 {
                zi.current_index = zi.queue.len() as i32 - 1;
            }
            zi.next_preloaded = false;
            zi.next_uri.clear();
            zi.next_index = -1;
            zi.revision += 1;
            let q_ev = Self::make_items_event(zone_id, &zi);
            (Arc::clone(&zi.ctrl), removed_current, q_ev, flushed)
        };
        if let Some(rec) = flushed {
            self.record_play_event_async(rec);
        }
        self.emit("zone.queue", zone_id, &q_ev);
        self.persist_zone_queues();

        if let Some(qc) = ctrl.as_queue_controller()
            && let Err(e) = qc.remove_queue_range(index, index).await
        {
            tracing::warn!(zone = %zone_id, index, error = %e, "native remove_queue_range failed");
        }
        if removed_current {
            let has_items = !z
                .inner
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .queue
                .is_empty();
            if has_items {
                let _ = self.start_current_locked(zone_id, z, &ctrl, true).await;
            } else {
                let _ = ctrl.stop().await;
            }
        }

        Ok(self.snapshot_zone(zone_id, z))
    }

    pub async fn move_item(&self, zone_id: &str, from: usize, to: usize) -> Result<ZoneSnapshot> {
        let z = self
            .get_zone(zone_id)
            .ok_or_else(|| QueueError::ZoneNotFound(zone_id.into()))?;
        let _op = z.op_mu.lock().await;
        self.move_item_locked(zone_id, &z, from, to).await
    }

    pub async fn move_item_by_id(
        &self,
        zone_id: &str,
        item_id: &str,
        to: usize,
    ) -> Result<ZoneSnapshot> {
        let z = self
            .get_zone(zone_id)
            .ok_or_else(|| QueueError::ZoneNotFound(zone_id.into()))?;
        let _op = z.op_mu.lock().await;
        let from = {
            let zi = z.inner.lock().unwrap_or_else(|e| e.into_inner());
            zi.queue
                .iter()
                .position(|it| it.item_id == item_id)
                .ok_or(QueueError::InvalidIndex)?
        };
        self.move_item_locked(zone_id, &z, from, to).await
    }

    async fn move_item_locked(
        &self,
        zone_id: &str,
        z: &Zone,
        from: usize,
        to: usize,
    ) -> Result<ZoneSnapshot> {
        let (ctrl, q_ev, pb_ev) = {
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
                zi.next_uri.clear();
                zi.next_index = -1;
                zi.revision += 1;
            }
            let q_ev = Self::make_items_event(zone_id, &zi);
            let pb_ev = self.make_playback_event(zone_id, &zi);
            (Arc::clone(&zi.ctrl), q_ev, pb_ev)
        };
        self.emit("zone.queue", zone_id, &q_ev);
        self.emit("zone.playback", zone_id, &pb_ev);
        self.persist_zone_queues();

        if from != to
            && let Some(qc) = ctrl.as_queue_controller()
            && let Err(e) = qc.move_queue_item(from, to).await
        {
            tracing::warn!(zone = %zone_id, from, to, error = %e, "native move_queue_item failed");
        }

        Ok(self.snapshot_zone(zone_id, z))
    }

    pub async fn clear(&self, zone_id: &str) -> Result<ZoneSnapshot> {
        let z = self
            .get_zone(zone_id)
            .ok_or_else(|| QueueError::ZoneNotFound(zone_id.into()))?;
        let _op = z.op_mu.lock().await;
        let (ctrl, prev_len, q_ev, pb_ev, flushed) = {
            let mut zi = z.inner.lock().unwrap_or_else(|e| e.into_inner());
            let flushed = flush_active_listen(&mut zi, zone_id, FlushReason::Interrupted);
            let prev_len = zi.queue.len();
            zi.queue.clear();
            zi.original_queue.clear();
            zi.current_index = -1;
            zi.current_uri.clear();
            zi.next_preloaded = false;
            zi.next_uri.clear();
            zi.next_index = -1;
            zi.status.state = "STOPPED".into();
            zi.status.position = Duration::ZERO;
            zi.status.duration = Duration::ZERO;
            zi.user_stopped = true;
            zi.position_updated_at = Utc::now();
            zi.last_command_at = Instant::now();
            zi.revision += 1;
            (
                Arc::clone(&zi.ctrl),
                prev_len,
                Self::make_items_event(zone_id, &zi),
                self.make_playback_event(zone_id, &zi),
                flushed,
            )
        };
        if let Some(rec) = flushed {
            self.record_play_event_async(rec);
        }
        self.emit("zone.queue", zone_id, &q_ev);
        self.emit("zone.playback", zone_id, &pb_ev);
        self.persist_zone_queues();
        if prev_len > 0
            && let Some(qc) = ctrl.as_queue_controller()
        {
            let _ = qc.remove_queue_range(0, prev_len - 1).await;
        }
        let _ = ctrl.stop().await;
        Ok(self.snapshot_zone(zone_id, &z))
    }

    pub async fn play_item_by_id(&self, zone_id: &str, item_id: &str) -> Result<ZoneSnapshot> {
        let z = self
            .get_zone(zone_id)
            .ok_or_else(|| QueueError::ZoneNotFound(zone_id.into()))?;
        let _op = z.op_mu.lock().await;
        let idx = {
            let zi = z.inner.lock().unwrap_or_else(|e| e.into_inner());
            zi.queue
                .iter()
                .position(|it| it.item_id == item_id)
                .ok_or(QueueError::InvalidIndex)?
        };
        self.play_index_locked(zone_id, &z, idx).await
    }

    pub async fn play_index(&self, zone_id: &str, index: usize) -> Result<ZoneSnapshot> {
        let z = self
            .get_zone(zone_id)
            .ok_or_else(|| QueueError::ZoneNotFound(zone_id.into()))?;
        let _op = z.op_mu.lock().await;
        self.play_index_locked(zone_id, &z, index).await
    }

    pub(super) async fn play_index_locked(
        &self,
        zone_id: &str,
        z: &Zone,
        index: usize,
    ) -> Result<ZoneSnapshot> {
        let (ctrl, flushed) = {
            let mut zi = z.inner.lock().unwrap_or_else(|e| e.into_inner());
            if index >= zi.queue.len() {
                return Err(QueueError::InvalidIndex);
            }
            let flushed = flush_active_listen(&mut zi, zone_id, FlushReason::UserSkip);
            zi.current_index = index as i32;
            zi.next_preloaded = false;
            zi.next_uri.clear();
            zi.next_index = -1;
            zi.revision += 1;
            (Arc::clone(&zi.ctrl), flushed)
        };
        if let Some(rec) = flushed {
            self.record_play_event_async(rec);
        }

        self.jump_or_start_locked(zone_id, z, &ctrl, index).await?;
        Ok(self.snapshot_zone(zone_id, z))
    }

    async fn jump_or_start_locked(
        &self,
        zone_id: &str,
        z: &Zone,
        ctrl: &Arc<dyn Controller>,
        index: usize,
    ) -> Result<()> {
        if let Some(qc) = ctrl.as_queue_controller() {
            if qc.play_index(index).await.is_ok() {
                let (flushed, pb_ev) = {
                    let mut zi = z.inner.lock().unwrap_or_else(|e| e.into_inner());
                    let tr = zi.queue.get(index).map(|it| it.track.clone());
                    let now = Instant::now();
                    zi.status.state = "PLAYING".into();
                    zi.status.position = Duration::ZERO;
                    zi.status.duration = tr
                        .as_ref()
                        .map(|t| Duration::from_secs(t.duration_sec.max(0) as u64))
                        .unwrap_or(Duration::ZERO);
                    zi.current_uri.clear();
                    zi.next_preloaded = false;
                    zi.next_uri.clear();
                    zi.next_index = -1;
                    zi.active_quality = Some(zi.audio_quality);
                    zi.loaded_at = now;
                    zi.position_updated_at = Utc::now();
                    zi.last_command_at = now;
                    zi.revision += 1;
                    let flushed = sync_active_listen(&mut zi, zone_id, PlaybackState::Playing);
                    (flushed, self.make_playback_event(zone_id, &zi))
                };
                if let Some(rec) = flushed {
                    self.record_play_event_async(rec);
                }
                self.emit("zone.playback", zone_id, &pb_ev);
                return Ok(());
            }
            // Fall back to reloading the native queue so multi-track queues stay intact.
            return self.start_current_locked(zone_id, z, ctrl, true).await;
        }
        self.start_current_locked(zone_id, z, ctrl, false).await
    }

    pub(super) fn record_play_event_async(&self, event: PlayEventRecord) {
        if tokio::runtime::Handle::try_current().is_ok() {
            let cat = Arc::clone(&self.catalog);
            tokio::task::spawn_blocking(move || {
                cat.record_play_event(&event);
            });
        } else {
            self.catalog.record_play_event(&event);
        }
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
                || (st != PlaybackState::Paused && zi.current_uri.is_empty());
            (Arc::clone(&zi.ctrl), need_load)
        };

        if need_load {
            let use_native = ctrl.as_queue_controller().is_some();
            self.start_current_locked(zone_id, &z, &ctrl, use_native)
                .await?;
        } else {
            ctrl.play()
                .await
                .map_err(|e| QueueError::Renderer(e.to_string()))?;
            let (pb, flushed) = {
                let mut zi = z.inner.lock().unwrap_or_else(|e| e.into_inner());
                zi.user_stopped = false;
                zi.status.state = "PLAYING".into();
                zi.position_updated_at = Utc::now();
                zi.last_command_at = Instant::now();
                zi.revision += 1;
                let flushed = sync_active_listen(&mut zi, zone_id, PlaybackState::Playing);
                (self.make_playback_event(zone_id, &zi), flushed)
            };
            if let Some(rec) = flushed {
                self.record_play_event_async(rec);
            }
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
            if PlaybackState::normalize(&zi.status.state) == PlaybackState::Playing
                && let Ok(elapsed) = Utc::now()
                    .signed_duration_since(zi.position_updated_at)
                    .to_std()
            {
                let interp = zi.status.position.saturating_add(elapsed);
                zi.status.position = if !zi.status.duration.is_zero() {
                    interp.min(zi.status.duration)
                } else {
                    interp
                };
            }
            zi.status.state = "PAUSED_PLAYBACK".into();
            zi.position_updated_at = Utc::now();
            zi.last_command_at = Instant::now();
            zi.revision += 1;
            let _ = sync_active_listen(&mut zi, zone_id, PlaybackState::Paused);
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
        let (pb, flushed) = {
            let mut zi = z.inner.lock().unwrap_or_else(|e| e.into_inner());
            let flushed = flush_active_listen(&mut zi, zone_id, FlushReason::Interrupted);
            zi.user_stopped = true;
            zi.status.state = "STOPPED".into();
            zi.status.position = Duration::ZERO;
            zi.position_updated_at = Utc::now();
            zi.last_command_at = Instant::now();
            zi.revision += 1;
            (self.make_playback_event(zone_id, &zi), flushed)
        };
        if let Some(rec) = flushed {
            self.record_play_event_async(rec);
        }
        self.emit("zone.playback", zone_id, &pb);
        Ok(self.snapshot_zone(zone_id, &z))
    }

    pub async fn next(&self, zone_id: &str) -> Result<ZoneSnapshot> {
        let z = self
            .get_zone(zone_id)
            .ok_or_else(|| QueueError::ZoneNotFound(zone_id.into()))?;
        let _op = z.op_mu.lock().await;
        let (ctrl, next_idx, flushed) = {
            let mut zi = z.inner.lock().unwrap_or_else(|e| e.into_inner());
            if zi.queue.is_empty() {
                return Err(QueueError::EmptyQueue);
            }
            let len = zi.queue.len() as i32;
            let next_idx = if zi.repeat == RepeatMode::One {
                zi.current_index.max(0)
            } else if zi.current_index + 1 < len {
                zi.current_index + 1
            } else if zi.repeat == RepeatMode::All {
                0
            } else {
                return Err(QueueError::EndOfQueue);
            };
            let flushed = flush_active_listen(&mut zi, zone_id, FlushReason::UserSkip);
            zi.current_index = next_idx;
            zi.next_preloaded = false;
            zi.next_uri.clear();
            zi.next_index = -1;
            zi.revision += 1;
            (Arc::clone(&zi.ctrl), next_idx, flushed)
        };
        if let Some(rec) = flushed {
            self.record_play_event_async(rec);
        }
        if next_idx >= 0 {
            self.jump_or_start_locked(zone_id, &z, &ctrl, next_idx as usize)
                .await?;
        }
        Ok(self.snapshot_zone(zone_id, &z))
    }

    pub async fn previous(&self, zone_id: &str) -> Result<ZoneSnapshot> {
        let z = self
            .get_zone(zone_id)
            .ok_or_else(|| QueueError::ZoneNotFound(zone_id.into()))?;
        let _op = z.op_mu.lock().await;

        let (ctrl, restart_only, target_idx, flushed) = {
            let mut zi = z.inner.lock().unwrap_or_else(|e| e.into_inner());
            if zi.queue.is_empty() {
                return Err(QueueError::EmptyQueue);
            }
            let can_seek = is_stream_seekable(&zi);
            if zi.status.position > Duration::from_secs(5) && can_seek {
                (Arc::clone(&zi.ctrl), true, zi.current_index, None)
            } else {
                let len = zi.queue.len() as i32;
                let prev = if zi.current_index > 0 {
                    zi.current_index - 1
                } else if zi.repeat == RepeatMode::All {
                    len - 1
                } else {
                    0
                };
                let flushed = flush_active_listen(&mut zi, zone_id, FlushReason::Interrupted);
                zi.current_index = prev;
                zi.next_preloaded = false;
                zi.next_uri.clear();
                zi.next_index = -1;
                zi.revision += 1;
                (Arc::clone(&zi.ctrl), false, prev, flushed)
            }
        };
        if let Some(rec) = flushed {
            self.record_play_event_async(rec);
        }

        if restart_only {
            if ctrl.seek(Duration::ZERO).await.is_ok() {
                let pb = {
                    let mut zi = z.inner.lock().unwrap_or_else(|e| e.into_inner());
                    zi.status.position = Duration::ZERO;
                    zi.position_updated_at = Utc::now();
                    zi.revision += 1;
                    self.make_playback_event(zone_id, &zi)
                };
                self.emit("zone.playback", zone_id, &pb);
            }
        } else if target_idx >= 0 {
            self.jump_or_start_locked(zone_id, &z, &ctrl, target_idx as usize)
                .await?;
        }
        Ok(self.snapshot_zone(zone_id, &z))
    }

    pub async fn seek(&self, zone_id: &str, position_sec: i32) -> Result<ZoneSnapshot> {
        if position_sec < 0 {
            return Err(QueueError::InvalidArgument(
                "positionSec must be non-negative".into(),
            ));
        }
        let z = self
            .get_zone(zone_id)
            .ok_or_else(|| QueueError::ZoneNotFound(zone_id.into()))?;
        let _op = z.op_mu.lock().await;
        let (ctrl, non_seekable_transcode) = {
            let zi = z.inner.lock().unwrap_or_else(|e| e.into_inner());
            let live_transcode = zi.ctrl.info().supports_seek && !is_stream_seekable(&zi);
            (Arc::clone(&zi.ctrl), live_transcode)
        };
        if !ctrl.info().supports_seek {
            return Err(QueueError::CapabilityUnsupported(
                "renderer does not support seek".into(),
            ));
        }
        if non_seekable_transcode {
            return Err(QueueError::CapabilityUnsupported(
                "seeking is not supported on live-transcoded streams".into(),
            ));
        }
        let pos = Duration::from_secs(position_sec as u64);
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
        if !(0..=100).contains(&volume) {
            return Err(QueueError::InvalidArgument(
                "volume must be between 0 and 100".into(),
            ));
        }
        let z = self
            .get_zone(zone_id)
            .ok_or_else(|| QueueError::ZoneNotFound(zone_id.into()))?;
        let _op = z.op_mu.lock().await;
        let ctrl = Arc::clone(&z.inner.lock().unwrap_or_else(|e| e.into_inner()).ctrl);
        if !ctrl.info().has_volume {
            return Err(QueueError::CapabilityUnsupported(
                "renderer does not support volume control".into(),
            ));
        }
        ctrl.set_volume(volume)
            .await
            .map_err(|e| QueueError::Renderer(e.to_string()))?;
        let vol_ev = {
            let mut zi = z.inner.lock().unwrap_or_else(|e| e.into_inner());
            zi.status.volume = volume;
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
        if !ctrl.info().has_volume {
            return Err(QueueError::CapabilityUnsupported(
                "renderer does not support mute control".into(),
            ));
        }
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
        let now_unix = Utc::now().timestamp();
        let recency = self.load_shuffle_recency(enabled, now_unix);
        let (ctrl, changed, prev_len, old_cur_idx, tail_tracks, quality, q_ev, pb_ev) = {
            let mut zi = z.inner.lock().unwrap_or_else(|e| e.into_inner());
            let prev_len = zi.queue.len();
            let old_cur_idx = zi.current_index;
            let changed = zi.shuffle != enabled;
            if changed {
                zi.shuffle = enabled;
                let cur_id = current_item_pair(&zi).0;
                if enabled {
                    zi.original_queue = zi.queue.clone();
                    let cur_item = zi.queue.iter().find(|it| it.item_id == cur_id).cloned();
                    let cur_artist = cur_item.as_ref().map(|it| it.track.artist.as_str());
                    let rest: Vec<Item> = zi
                        .queue
                        .iter()
                        .filter(|it| it.item_id != cur_id)
                        .cloned()
                        .collect();
                    let mut rng = rand::rng();
                    let shuffled = fresh_shuffle(rest, cur_artist, &recency, now_unix, &mut rng);
                    let mut next_q = Vec::with_capacity(zi.queue.len());
                    if let Some(cur) = cur_item {
                        next_q.push(cur);
                        zi.current_index = 0;
                    }
                    next_q.extend(shuffled);
                    zi.queue = next_q;
                } else {
                    let mut restored = zi.original_queue.clone();
                    let seen: std::collections::HashSet<String> =
                        restored.iter().map(|it| it.item_id.clone()).collect();
                    for it in &zi.queue {
                        if !seen.contains(&it.item_id) {
                            restored.push(it.clone());
                        }
                    }
                    zi.queue = restored;
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
            let tail_tracks: Vec<Track> = if changed && zi.current_index == 0 {
                zi.queue.iter().skip(1).map(|it| it.track.clone()).collect()
            } else {
                Vec::new()
            };
            (
                Arc::clone(&zi.ctrl),
                changed,
                prev_len,
                old_cur_idx,
                tail_tracks,
                zi.audio_quality,
                Self::make_items_event(zone_id, &zi),
                self.make_playback_event(zone_id, &zi),
            )
        };
        self.emit("zone.queue", zone_id, &q_ev);
        self.emit("zone.playback", zone_id, &pb_ev);
        self.persist_zone_queues();
        if changed
            && prev_len > 1
            && old_cur_idx >= 0
            && (old_cur_idx as usize) < prev_len
            && !tail_tracks.is_empty()
        {
            self.sync_native_shuffle_tail(
                zone_id,
                &ctrl,
                prev_len,
                old_cur_idx as usize,
                &tail_tracks,
                quality,
            )
            .await;
        }
        Ok(self.snapshot_zone(zone_id, &z))
    }

    async fn sync_native_shuffle_tail(
        &self,
        zone_id: &str,
        ctrl: &Arc<dyn Controller>,
        prev_len: usize,
        old_cur_idx: usize,
        tail_tracks: &[Track],
        quality: AudioQuality,
    ) {
        let Some(qc) = ctrl.as_queue_controller() else {
            return;
        };
        let Some(items) = self.build_all_media_items(tail_tracks, ctrl, quality).await else {
            return;
        };
        // Remove items after old_cur_idx, then items before old_cur_idx, leaving ONLY the
        // currently playing track untouched at native index 0 before appending the shuffled tail.
        let mut ok = true;
        if old_cur_idx + 1 < prev_len {
            ok = qc
                .remove_queue_range(old_cur_idx + 1, prev_len - 1)
                .await
                .is_ok();
        }
        if ok && old_cur_idx > 0 {
            ok = qc.remove_queue_range(0, old_cur_idx - 1).await.is_ok();
        }
        if !ok {
            tracing::warn!(zone = %zone_id, "native shuffle clear failed");
            return;
        }
        if let Err(e) = qc.append_queue(&items).await {
            tracing::warn!(zone = %zone_id, error = %e, "native shuffle tail append failed");
        }
    }

    async fn build_all_media_items(
        &self,
        tracks: &[Track],
        ctrl: &Arc<dyn Controller>,
        quality: AudioQuality,
    ) -> Option<Vec<crate::upnp::MediaItem>> {
        let mut items = Vec::with_capacity(tracks.len());
        for t in tracks {
            let (mi, _) = self.build_media_item(t, ctrl, quality).await.ok()?;
            items.push(mi);
        }
        Some(items)
    }

    fn load_shuffle_recency(&self, enabled: bool, now_unix: i64) -> RecentPlayRecency {
        if !enabled {
            return RecentPlayRecency::default();
        }
        self.catalog
            .recent_play_recency(now_unix - FRESH_SHUFFLE_WINDOW_SECS)
            .unwrap_or_else(|err| {
                tracing::warn!(
                    error = %err,
                    "recent_play_recency lookup failed; falling back to unweighted shuffle"
                );
                RecentPlayRecency::default()
            })
    }

    pub async fn set_repeat(&self, zone_id: &str, mode: RepeatMode) -> Result<ZoneSnapshot> {
        let z = self
            .get_zone(zone_id)
            .ok_or_else(|| QueueError::ZoneNotFound(zone_id.into()))?;
        let _op = z.op_mu.lock().await;
        let (ctrl, pb_ev) = {
            let mut zi = z.inner.lock().unwrap_or_else(|e| e.into_inner());
            zi.repeat = mode;
            zi.next_preloaded = false;
            zi.next_uri.clear();
            zi.next_index = -1;
            zi.revision += 1;
            (Arc::clone(&zi.ctrl), self.make_playback_event(zone_id, &zi))
        };
        if let Some(qc) = ctrl.as_queue_controller() {
            // Tacet owns shuffle order; never enable device-side shuffle.
            let _ = qc.set_loop_mode(mode.as_str(), false).await;
        }
        self.emit("zone.playback", zone_id, &pb_ev);
        self.persist_zone_queues();
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
            let mut native_cur_idx = 0usize;
            for (i, it) in queue_items.iter().enumerate() {
                if i == cur_idx as usize {
                    native_cur_idx = all_media.len();
                    all_media.push(media_item.clone());
                } else if let Ok((mi, _)) =
                    self.build_media_item(&it.track, ctrl, requested_q).await
                {
                    all_media.push(mi);
                }
            }
            if !all_media.is_empty() && qc.replace_queue(&all_media, native_cur_idx).await.is_ok() {
                self.finish_start_locked(zone_id, z, cur_item, &media_item, eff_q, false);
                return Ok(());
            }
        }

        ctrl.set_uri(&media_item)
            .await
            .map_err(|e| QueueError::Renderer(e.to_string()))?;
        ctrl.play()
            .await
            .map_err(|e| QueueError::Renderer(e.to_string()))?;

        self.finish_start_locked(zone_id, z, cur_item, &media_item, eff_q, true);
        Ok(())
    }

    fn finish_start_locked(
        &self,
        zone_id: &str,
        z: &Zone,
        cur_item: &Item,
        media: &crate::upnp::MediaItem,
        eff_q: AudioQuality,
        clear_preloaded: bool,
    ) {
        let (pb, flushed) = {
            let mut zi = z.inner.lock().unwrap_or_else(|e| e.into_inner());
            zi.current_uri = media.url.clone();
            zi.active_quality = Some(eff_q);
            zi.user_stopped = false;
            zi.loaded_at = Instant::now();
            zi.status.state = "PLAYING".into();
            zi.status.position = Duration::ZERO;
            zi.status.duration = media.duration;
            zi.position_updated_at = Utc::now();
            zi.last_command_at = Instant::now();
            if clear_preloaded {
                zi.next_preloaded = false;
                zi.next_uri.clear();
                zi.next_index = -1;
            }
            zi.revision += 1;
            let _ = cur_item;
            let flushed = sync_active_listen(&mut zi, zone_id, PlaybackState::Playing);
            (self.make_playback_event(zone_id, &zi), flushed)
        };
        if let Some(rec) = flushed {
            self.record_play_event_async(rec);
        }
        self.emit("zone.playback", zone_id, &pb);
    }
}

fn is_stream_seekable(zi: &super::ZoneInner) -> bool {
    let info = zi.ctrl.info();
    if !info.supports_seek {
        return false;
    }
    if info.kind == "player" {
        return true;
    }
    let cur_track = current_item_pair(zi).1;
    !zi.active_quality.is_some_and(|q| {
        q.is_opus()
            || (q == AudioQuality::Wav
                && cur_track
                    .as_ref()
                    .is_none_or(|t| t.source != SOURCE_LOCAL || is_dsd_track(t)))
    })
}
