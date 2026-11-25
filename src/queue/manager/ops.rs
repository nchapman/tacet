use std::sync::Arc;
use std::time::{Duration, Instant};

use chrono::Utc;
use rand::seq::SliceRandom;

use crate::library::Track;
use crate::upnp::Controller;

use super::{Manager, Zone, current_item_pair};
use crate::queue::types::{
    AudioQuality, Item, PlaybackState, QueueError, RepeatMode, Result, VolumeEvent, ZoneSnapshot,
};

impl Manager {
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

        if let Some(qc) = ctrl.as_queue_controller()
            && qc.remove_queue_range(index, index).await.is_err()
            && !removed_current
        {
            let is_playing = {
                let zi = z.inner.lock().unwrap_or_else(|e| e.into_inner());
                !zi.queue.is_empty()
                    && PlaybackState::normalize(&zi.status.state) == PlaybackState::Playing
            };
            if is_playing {
                let _ = self.start_current_locked(zone_id, &z, &ctrl, true).await;
            }
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

    pub async fn remove_item_by_id(&self, zone_id: &str, item_id: &str) -> Result<ZoneSnapshot> {
        let z = self
            .get_zone(zone_id)
            .ok_or_else(|| QueueError::ZoneNotFound(zone_id.into()))?;
        let idx = {
            let zi = z.inner.lock().unwrap_or_else(|e| e.into_inner());
            zi.queue
                .iter()
                .position(|it| it.item_id == item_id)
                .ok_or(QueueError::InvalidIndex)?
        };
        self.remove_item(zone_id, idx).await
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
                zi.next_uri.clear();
                zi.next_index = -1;
                zi.revision += 1;
            }
            let q_ev = Self::make_items_event(zone_id, &zi);
            (Arc::clone(&zi.ctrl), q_ev)
        };
        self.emit("zone.queue", zone_id, &q_ev);

        if from != to
            && let Some(qc) = ctrl.as_queue_controller()
            && qc.move_queue_item(from, to).await.is_err()
        {
            let is_playing = {
                let zi = z.inner.lock().unwrap_or_else(|e| e.into_inner());
                !zi.queue.is_empty()
                    && PlaybackState::normalize(&zi.status.state) == PlaybackState::Playing
            };
            if is_playing {
                let _ = self.start_current_locked(zone_id, &z, &ctrl, true).await;
            }
        }

        Ok(self.snapshot_zone(zone_id, &z))
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
        let from = {
            let zi = z.inner.lock().unwrap_or_else(|e| e.into_inner());
            zi.queue
                .iter()
                .position(|it| it.item_id == item_id)
                .ok_or(QueueError::InvalidIndex)?
        };
        self.move_item(zone_id, from, to).await
    }

    pub async fn clear(&self, zone_id: &str) -> Result<ZoneSnapshot> {
        self.set_queue(zone_id, Vec::new(), -1, false).await
    }

    pub async fn play_item_by_id(&self, zone_id: &str, item_id: &str) -> Result<ZoneSnapshot> {
        let z = self
            .get_zone(zone_id)
            .ok_or_else(|| QueueError::ZoneNotFound(zone_id.into()))?;
        let idx = {
            let zi = z.inner.lock().unwrap_or_else(|e| e.into_inner());
            zi.queue
                .iter()
                .position(|it| it.item_id == item_id)
                .ok_or(QueueError::InvalidIndex)?
        };
        self.play_index(zone_id, idx).await
    }

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
            zi.next_uri.clear();
            zi.next_index = -1;
            zi.revision += 1;
            Arc::clone(&zi.ctrl)
        };

        self.jump_or_start_locked(zone_id, &z, &ctrl, index).await?;
        Ok(self.snapshot_zone(zone_id, &z))
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
                let (tr, pb_ev) = {
                    let mut zi = z.inner.lock().unwrap_or_else(|e| e.into_inner());
                    zi.status.state = "PLAYING".into();
                    zi.status.position = Duration::ZERO;
                    zi.position_updated_at = Utc::now();
                    zi.last_command_at = Instant::now();
                    zi.revision += 1;
                    let tr = zi.queue.get(index).map(|it| it.track.clone());
                    (tr, self.make_playback_event(zone_id, &zi))
                };
                if let Some(tr) = tr {
                    self.record_play_async(tr);
                }
                self.emit("zone.playback", zone_id, &pb_ev);
                return Ok(());
            }
            // Fall back to reloading the native queue so multi-track queues stay intact.
            return self.start_current_locked(zone_id, z, ctrl, true).await;
        }
        self.start_current_locked(zone_id, z, ctrl, false).await
    }

    pub(super) fn record_play_async(&self, track: Track) {
        let cat = Arc::clone(&self.catalog);
        tokio::task::spawn_blocking(move || {
            cat.record_play(&track);
        });
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

        let (ctrl, restart_only, target_idx) = {
            let mut zi = z.inner.lock().unwrap_or_else(|e| e.into_inner());
            if zi.queue.is_empty() {
                return Err(QueueError::EmptyQueue);
            }
            if zi.status.position > Duration::from_secs(3) {
                (Arc::clone(&zi.ctrl), true, zi.current_index)
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
                zi.next_uri.clear();
                zi.next_index = -1;
                zi.revision += 1;
                (Arc::clone(&zi.ctrl), false, prev)
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
        } else if target_idx >= 0 {
            self.jump_or_start_locked(zone_id, &z, &ctrl, target_idx as usize)
                .await?;
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
        let (pb, rec_tr) = {
            let mut zi = z.inner.lock().unwrap_or_else(|e| e.into_inner());
            zi.current_uri = media.url.clone();
            zi.active_quality = Some(eff_q);
            zi.status.state = "PLAYING".into();
            zi.status.position = Duration::ZERO;
            zi.status.duration = media.duration;
            zi.position_updated_at = Utc::now();
            zi.last_command_at = Instant::now();
            if clear_preloaded {
                zi.next_preloaded = false;
            }
            zi.revision += 1;
            let rec = if zi.last_played_item_id != cur_item.item_id {
                zi.last_played_item_id = cur_item.item_id.clone();
                Some(cur_item.track.clone())
            } else {
                None
            };
            (self.make_playback_event(zone_id, &zi), rec)
        };
        if let Some(tr) = rec_tr {
            self.record_play_async(tr);
        }
        self.emit("zone.playback", zone_id, &pb);
    }
}
