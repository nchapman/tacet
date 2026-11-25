use std::sync::Arc;
use std::time::{Duration, Instant};

use chrono::Utc;

use crate::library::Track;
use crate::upnp::{Controller, DeviceEvent};

use super::{Manager, Zone, ZoneInner, compute_next_index};
use crate::queue::types::{AudioQuality, PlaybackState, VolumeEvent};

struct PreloadTarget {
    next_index: i32,
    track: Track,
    quality: AudioQuality,
    expected_cur: i32,
}

impl Manager {
    /// Handles push events from UPnP GENA or remote HTTP/SSE players.
    pub fn apply_device_event(self: &Arc<Self>, ev: DeviceEvent) {
        self.log_event("gena", &ev.renderer_id, &ev);
        let Some(z) = self.get_zone(&ev.renderer_id) else {
            return;
        };

        let mut need_advance = false;
        let need_preload;
        let mut recorded_track = None;
        let advance_rev: u64;

        let (ctrl, vol_ev, pb_ev) = {
            let mut zi = z.inner.lock().unwrap_or_else(|e| e.into_inner());
            let ctrl = Arc::clone(&zi.ctrl);
            let prev_state = PlaybackState::normalize(&zi.status.state);
            let (vol_changed, mut pb_changed) = apply_event_fields(&mut zi, &ev);
            let new_state = PlaybackState::normalize(&zi.status.state);

            let uri = zi.status.track_uri.clone();
            if let Some(tr) = detect_gapless_transition(&mut zi, &uri) {
                pb_changed = true;
                recorded_track = Some(tr);
            }

            // Ignore GENA initial snapshot (seq == 0 on AVTransport) to avoid spurious advances.
            let is_gena_initial = ev.service == "AVTransport" && ev.seq == 0;
            if !is_gena_initial
                && !zi.user_stopped
                && zi.loaded_at.elapsed() > Duration::from_secs(2)
                && prev_state == PlaybackState::Playing
                && new_state == PlaybackState::Stopped
                && ctrl.as_queue_controller().is_none()
                && zi.last_command_at.elapsed() > Duration::from_secs(3)
            {
                need_advance = true;
            }
            if recorded_track.is_none() {
                recorded_track = maybe_record_play(&mut zi, new_state);
            }

            if vol_changed || pb_changed {
                zi.revision += 1;
            }
            advance_rev = zi.revision;
            need_preload = arm_gapless_preload(&mut zi, &ctrl, new_state);

            let ve = vol_changed.then(|| VolumeEvent {
                zone_id: ev.renderer_id.clone(),
                revision: zi.revision,
                volume: zi.status.volume,
                muted: zi.status.muted,
            });
            let pe = pb_changed.then(|| self.make_playback_event(&ev.renderer_id, &zi));
            (ctrl, ve, pe)
        };

        if let Some(tr) = recorded_track {
            self.record_play_async(tr);
        }
        if let Some(ve) = vol_ev {
            self.emit("zone.volume", &ev.renderer_id, &ve);
        }
        if let Some(pe) = pb_ev {
            self.emit("zone.playback", &ev.renderer_id, &pe);
        }

        if let Some(target) = need_preload {
            let mgr = Arc::clone(self);
            let z_clone = Arc::clone(&z);
            tokio::spawn(async move {
                mgr.preload_next_if_unchanged(&z_clone, &ctrl, target).await;
            });
        }
        if need_advance {
            let mgr = Arc::clone(self);
            let zid = ev.renderer_id;
            let z_clone = Arc::clone(&z);
            tokio::spawn(async move {
                mgr.advance_if_unchanged(&zid, &z_clone, advance_rev).await;
            });
        }
    }

    async fn preload_next_if_unchanged(
        &self,
        z: &Zone,
        ctrl: &Arc<dyn Controller>,
        target: PreloadTarget,
    ) {
        let Ok((mi, _)) = self
            .build_media_item(&target.track, ctrl, target.quality)
            .await
        else {
            clear_preload_if_matching(z, target.expected_cur, target.next_index, false);
            return;
        };
        let _op = z.op_mu.lock().await;
        {
            let zi = z.inner.lock().unwrap_or_else(|e| e.into_inner());
            if zi.current_index != target.expected_cur || zi.next_index != target.next_index {
                return;
            }
        }
        match ctrl.set_next_uri(&mi).await {
            Ok(()) => {
                let mut zi = z.inner.lock().unwrap_or_else(|e| e.into_inner());
                if zi.current_index == target.expected_cur && zi.next_index == target.next_index {
                    zi.next_preloaded = true;
                    zi.next_uri = mi.url;
                    zi.next_index = target.next_index;
                }
            }
            Err(e) if e.is_action_unsupported() => {
                clear_preload_if_matching(z, target.expected_cur, target.next_index, true);
            }
            Err(_) => {
                clear_preload_if_matching(z, target.expected_cur, target.next_index, false);
            }
        }
    }

    async fn advance_if_unchanged(&self, zone_id: &str, z: &Zone, expected_rev: u64) {
        let _op = z.op_mu.lock().await;
        let next_idx = {
            let zi = z.inner.lock().unwrap_or_else(|e| e.into_inner());
            if zi.revision != expected_rev {
                return;
            }
            compute_next_index(&zi)
        };
        if let Some(ni) = next_idx
            && let Err(e) = self.play_index_locked(zone_id, z, ni as usize).await
        {
            self.emit_zone_error(zone_id, "ADVANCE_FAILED", e.to_string(), true);
        }
    }

    pub(super) fn start_poll_loop(self: &Arc<Self>) {
        let mgr = Arc::clone(self);
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_secs(2));
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
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
        let futs = snapshot
            .into_iter()
            .map(|(zone_id, z)| async move { self.poll_one_zone(zone_id, z).await });
        futures_util::future::join_all(futs).await;
    }

    async fn poll_one_zone(&self, zone_id: String, z: Arc<Zone>) {
        let ctrl = {
            let mut zi = z.inner.lock().unwrap_or_else(|e| e.into_inner());
            let info = zi.ctrl.info();
            if info.kind == "player" || zi.ctrl.push_connected() {
                return;
            }
            if info.kind == "cast" && zi.status.state == "STOPPED" {
                return;
            }
            let has_gena = self
                .gena
                .read()
                .unwrap_or_else(|e| e.into_inner())
                .as_ref()
                .is_some_and(|g| g.is_active(&zone_id));
            let near_end = zi.status.state == "PLAYING"
                && zi.status.duration > Duration::ZERO
                && zi.status.duration.saturating_sub(zi.status.position) <= Duration::from_secs(15);
            let cadence = match zi.status.state.as_str() {
                "PLAYING" if near_end => Duration::from_secs(2),
                "PLAYING" => Duration::from_secs(if has_gena { 15 } else { 3 }),
                "TRANSITIONING" => Duration::from_secs(if has_gena { 10 } else { 2 }),
                _ => Duration::from_secs(if has_gena { 60 } else { 10 }),
            };
            if zi.last_poll.elapsed() < cadence {
                return;
            }
            zi.last_poll = Instant::now();
            Arc::clone(&zi.ctrl)
        };
        let Ok(st) = ctrl.status().await else {
            return;
        };
        self.log_event("poll", &zone_id, &st);

        let mut need_advance = false;
        let need_preload;
        let mut recorded_track = None;
        let advance_rev: u64;

        let (vol_ev, pb_ev) = {
            let mut zi = z.inner.lock().unwrap_or_else(|e| e.into_inner());
            let prev_state = PlaybackState::normalize(&zi.status.state);
            let new_state = PlaybackState::normalize(&st.state);
            let (vol_changed, mut pb_changed) = apply_poll_status(&mut zi, &st);

            if let Some(tr) = detect_gapless_transition(&mut zi, &st.track_uri) {
                pb_changed = true;
                recorded_track = Some(tr);
            }
            if !zi.user_stopped
                && zi.loaded_at.elapsed() > Duration::from_secs(2)
                && prev_state == PlaybackState::Playing
                && new_state == PlaybackState::Stopped
                && ctrl.as_queue_controller().is_none()
                && zi.last_command_at.elapsed() > Duration::from_secs(3)
            {
                need_advance = true;
            }
            if recorded_track.is_none() {
                recorded_track = maybe_record_play(&mut zi, new_state);
            }

            if vol_changed || pb_changed {
                zi.revision += 1;
            }
            advance_rev = zi.revision;
            need_preload = arm_gapless_preload(&mut zi, &ctrl, new_state);

            let ve = vol_changed.then(|| VolumeEvent {
                zone_id: zone_id.clone(),
                revision: zi.revision,
                volume: zi.status.volume,
                muted: zi.status.muted,
            });
            let pe = pb_changed.then(|| self.make_playback_event(&zone_id, &zi));
            (ve, pe)
        };

        if let Some(tr) = recorded_track {
            self.record_play_async(tr);
        }
        if let Some(ve) = vol_ev {
            self.emit("zone.volume", &zone_id, &ve);
        }
        if let Some(pe) = pb_ev {
            self.emit("zone.playback", &zone_id, &pe);
        }
        if let Some(target) = need_preload {
            self.preload_next_if_unchanged(&z, &ctrl, target).await;
        }
        if need_advance {
            self.advance_if_unchanged(&zone_id, &z, advance_rev).await;
        }
    }
}

fn apply_event_fields(zi: &mut ZoneInner, ev: &DeviceEvent) -> (bool, bool) {
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
        zi.status.state = ev.state.clone();
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
        zi.status.track_uri = ev.track_uri.clone();
        if is_foreign_track_uri(zi, &ev.track_uri) && zi.status.state != "STOPPED" {
            zi.status.state = "STOPPED".into();
            pb_changed = true;
        }
    }
    (vol_changed, pb_changed)
}

fn is_foreign_track_uri(zi: &ZoneInner, uri: &str) -> bool {
    if uri.is_empty() || zi.current_uri.is_empty() {
        return false;
    }
    if uri == zi.current_uri || (!zi.next_uri.is_empty() && uri == zi.next_uri) {
        return false;
    }
    !uri.contains("/media/local/") && !uri.contains("/media/qobuz/")
}

fn maybe_record_play(zi: &mut ZoneInner, state: PlaybackState) -> Option<Track> {
    if state != PlaybackState::Playing || zi.current_index < 0 {
        return None;
    }
    let item = zi.queue.get(zi.current_index as usize)?;
    if zi.last_played_item_id == item.item_id {
        return None;
    }
    let dur_secs = if !zi.status.duration.is_zero() {
        zi.status.duration.as_secs()
    } else {
        item.track.duration_sec.max(0) as u64
    };
    let threshold = if dur_secs > 0 {
        (dur_secs / 2).clamp(5, 30)
    } else {
        5
    };
    if zi.status.position.as_secs() < threshold {
        return None;
    }
    zi.last_played_item_id = item.item_id.clone();
    Some(item.track.clone())
}

fn apply_poll_status(zi: &mut ZoneInner, st: &crate::upnp::TransportStatus) -> (bool, bool) {
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
    if !st.track_uri.is_empty() && is_foreign_track_uri(zi, &st.track_uri) {
        if zi.status.state != "STOPPED" {
            zi.status.state = "STOPPED".into();
            pb_changed = true;
        }
    } else if st.state != zi.status.state {
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
    if st.has_queue_index
        && st.queue_index >= 0
        && (st.queue_index as usize) < zi.queue.len()
        && st.queue_index != zi.current_index
        && zi.last_command_at.elapsed() > Duration::from_secs(3)
    {
        zi.current_index = st.queue_index;
        zi.next_preloaded = false;
        zi.next_uri.clear();
        zi.next_index = -1;
        pb_changed = true;
    }
    (vol_changed, pb_changed)
}

fn detect_gapless_transition(zi: &mut ZoneInner, track_uri: &str) -> Option<Track> {
    if zi.next_preloaded
        && !track_uri.is_empty()
        && track_uri == zi.next_uri
        && zi.next_index >= 0
        && (zi.next_index as usize) < zi.queue.len()
    {
        zi.current_index = zi.next_index;
        zi.current_uri = zi.next_uri.clone();
        zi.next_preloaded = false;
        zi.next_uri.clear();
        zi.next_index = -1;
        return Some(zi.queue[zi.current_index as usize].track.clone());
    }
    None
}

/// Optimistically marks `next_preloaded` under lock so rapid position ticks cannot spawn
/// duplicate `set_next_uri` calls (cleared on failure in `preload_next_if_unchanged`).
fn arm_gapless_preload(
    zi: &mut ZoneInner,
    ctrl: &Arc<dyn Controller>,
    new_state: PlaybackState,
) -> Option<PreloadTarget> {
    if new_state == PlaybackState::Playing
        && !zi.next_preloaded
        && !zi.set_next_unsupported
        && ctrl.as_queue_controller().is_none()
        && !zi.status.duration.is_zero()
        && zi.status.duration.saturating_sub(zi.status.position) <= Duration::from_secs(25)
        && let Some(n) = compute_next_index(zi)
        && n != zi.current_index
    {
        zi.next_preloaded = true;
        zi.next_index = n;
        return Some(PreloadTarget {
            next_index: n,
            track: zi.queue[n as usize].track.clone(),
            quality: zi.audio_quality,
            expected_cur: zi.current_index,
        });
    }
    None
}

fn clear_preload_if_matching(z: &Zone, expected_cur: i32, next_index: i32, mark_unsupported: bool) {
    let mut zi = z.inner.lock().unwrap_or_else(|e| e.into_inner());
    if mark_unsupported {
        zi.set_next_unsupported = true;
    }
    if zi.current_index == expected_cur && zi.next_index == next_index {
        zi.next_preloaded = false;
        zi.next_uri.clear();
        zi.next_index = -1;
    }
}
