use std::sync::Arc;
use std::time::{Duration, Instant};

use chrono::Utc;

use crate::library::Track;
use crate::upnp::{Controller, DeviceEvent};

use super::listen::{
    FlushReason, flush_active_listen, sync_active_listen, tick_active_listen_if_playing,
};
use super::{Manager, Zone, ZoneInner, compute_next_index};
use crate::queue::types::{AudioQuality, PlaybackState, RepeatMode, VolumeEvent};

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
        let mut recorded_event = None;
        let advance_rev: u64;

        let (ctrl, vol_ev, pb_ev) = {
            let mut zi = z.inner.lock().unwrap_or_else(|e| e.into_inner());
            let ctrl = Arc::clone(&zi.ctrl);
            let prev_state = PlaybackState::normalize(&zi.status.state);
            let foreign = !ev.track_uri.is_empty() && is_foreign_track_uri(&zi, &ev.track_uri);
            let (vol_changed, mut pb_changed) = apply_event_fields(&mut zi, &ev);
            let new_state = PlaybackState::normalize(&zi.status.state);

            if foreign {
                recorded_event =
                    flush_active_listen(&mut zi, &ev.renderer_id, FlushReason::Interrupted);
            }

            let uri = zi.status.track_uri.clone();
            let (transitioned, gapless_ev) =
                detect_gapless_transition(&mut zi, &ev.renderer_id, &uri);
            if transitioned {
                pb_changed = true;
                if recorded_event.is_none() {
                    recorded_event = gapless_ev;
                }
            }

            // Ignore GENA initial snapshot (seq == 0 on AVTransport) to avoid spurious advances.
            let is_gena_initial = ev.service == "AVTransport" && ev.seq == 0;
            let natural_stop = !foreign
                && !is_gena_initial
                && !zi.user_stopped
                && zi.loaded_at.elapsed() > Duration::from_secs(2)
                && prev_state == PlaybackState::Playing
                && new_state == PlaybackState::Stopped
                && zi.last_command_at.elapsed() > Duration::from_secs(3);
            if natural_stop {
                if ctrl.as_queue_controller().is_none() {
                    need_advance = true;
                } else if recorded_event.is_none() {
                    recorded_event =
                        flush_active_listen(&mut zi, &ev.renderer_id, FlushReason::NaturalEnd);
                }
            }
            if let Some(ev_rec) = sync_active_listen(&mut zi, &ev.renderer_id, new_state)
                && recorded_event.is_none()
            {
                recorded_event = Some(ev_rec);
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

        if let Some(rec) = recorded_event {
            self.record_play_event_async(rec);
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
        let (next_idx, flushed) = {
            let mut zi = z.inner.lock().unwrap_or_else(|e| e.into_inner());
            if zi.revision != expected_rev {
                return;
            }
            let flushed = flush_active_listen(&mut zi, zone_id, FlushReason::NaturalEnd);
            (compute_next_index(&zi), flushed)
        };
        if let Some(rec) = flushed {
            self.record_play_event_async(rec);
        }
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
        for (_, z) in &snapshot {
            let mut zi = z.inner.lock().unwrap_or_else(|e| e.into_inner());
            tick_active_listen_if_playing(&mut zi);
        }
        let futs = snapshot
            .into_iter()
            .map(|(zone_id, z)| async move { self.poll_one_zone(zone_id, z).await });
        futures_util::future::join_all(futs).await;
    }

    fn poll_controller_if_due(&self, zone_id: &str, z: &Zone) -> Option<Arc<dyn Controller>> {
        let mut zi = z.inner.lock().unwrap_or_else(|e| e.into_inner());
        let info = zi.ctrl.info();
        if info.kind == "player" || zi.ctrl.push_connected() {
            return None;
        }
        if info.kind == "cast" && zi.status.state == "STOPPED" {
            return None;
        }
        let has_gena = self
            .gena
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .is_some_and(|g| g.is_active(zone_id));
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
            return None;
        }
        zi.last_poll = Instant::now();
        Some(Arc::clone(&zi.ctrl))
    }

    pub(super) async fn poll_one_zone(&self, zone_id: String, z: Arc<Zone>) {
        let Some(ctrl) = self.poll_controller_if_due(&zone_id, &z) else {
            return;
        };
        let Ok(st) = ctrl.status().await else {
            return;
        };
        self.log_event("poll", &zone_id, &st);

        let mut need_advance = false;
        let need_preload;
        let mut recorded_event = None;
        let advance_rev: u64;

        let (vol_ev, pb_ev) = {
            let mut zi = z.inner.lock().unwrap_or_else(|e| e.into_inner());
            let prev_state = PlaybackState::normalize(&zi.status.state);
            let prev_pos = zi.status.position;
            let prev_dur = zi.status.duration;
            let foreign = !st.track_uri.is_empty() && is_foreign_track_uri(&zi, &st.track_uri);
            let repeat_one_via_manager = is_manager_repeat_one(&zi, &ctrl);
            let native_repeat_one_moved =
                !foreign && repeat_one_via_manager && device_advanced_index(&zi, &st);

            let (vol_changed, mut pb_changed) =
                apply_poll_status(&mut zi, &st, !native_repeat_one_moved);
            let new_state = PlaybackState::normalize(&zi.status.state);

            if foreign {
                recorded_event = flush_active_listen(&mut zi, &zone_id, FlushReason::Interrupted);
            } else if native_repeat_one_moved {
                recorded_event = flush_active_listen(&mut zi, &zone_id, FlushReason::NaturalEnd);
                need_advance = true;
            }

            let (transitioned, gapless_ev) =
                detect_gapless_transition(&mut zi, &zone_id, &st.track_uri);
            if transitioned {
                pb_changed = true;
                if recorded_event.is_none() {
                    recorded_event = gapless_ev;
                }
            }

            // Detect native RepeatMode::One loop wraparound on Cast/LinkPlay so each repeat
            // emits its own play event instead of accumulating into one giant listen.
            if !foreign
                && recorded_event.is_none()
                && ctrl.as_queue_controller().is_some()
                && zi.repeat == RepeatMode::One
                && new_state == PlaybackState::Playing
                && st.has_position
                && !prev_dur.is_zero()
                && prev_pos >= prev_dur / 2
                && st.position < Duration::from_secs(10)
                && zi.last_command_at.elapsed() > Duration::from_secs(3)
            {
                recorded_event = flush_active_listen(&mut zi, &zone_id, FlushReason::NaturalEnd);
            }

            let natural_stop = !foreign
                && !zi.user_stopped
                && zi.loaded_at.elapsed() > Duration::from_secs(2)
                && prev_state == PlaybackState::Playing
                && new_state == PlaybackState::Stopped
                && zi.last_command_at.elapsed() > Duration::from_secs(3);
            if natural_stop {
                if ctrl.as_queue_controller().is_none() || repeat_one_via_manager {
                    need_advance = true;
                } else if recorded_event.is_none() {
                    recorded_event =
                        flush_active_listen(&mut zi, &zone_id, FlushReason::NaturalEnd);
                }
            }
            if let Some(ev_rec) = sync_active_listen(&mut zi, &zone_id, new_state)
                && recorded_event.is_none()
            {
                recorded_event = Some(ev_rec);
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

        if let Some(rec) = recorded_event {
            self.record_play_event_async(rec);
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

fn is_manager_repeat_one(zi: &ZoneInner, ctrl: &Arc<dyn Controller>) -> bool {
    zi.repeat == RepeatMode::One
        && ctrl
            .as_queue_controller()
            .is_some_and(|qc| !qc.supports_repeat_one())
}

fn device_advanced_index(zi: &ZoneInner, st: &crate::upnp::TransportStatus) -> bool {
    st.has_queue_index
        && st.queue_index >= 0
        && st.queue_index != zi.current_index
        && zi.last_command_at.elapsed() > Duration::from_secs(3)
}

fn apply_poll_status(
    zi: &mut ZoneInner,
    st: &crate::upnp::TransportStatus,
    accept_queue_index: bool,
) -> (bool, bool) {
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
    if accept_queue_index
        && st.has_queue_index
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

fn detect_gapless_transition(
    zi: &mut ZoneInner,
    zone_id: &str,
    track_uri: &str,
) -> (bool, Option<crate::library::PlayEventRecord>) {
    if zi.next_preloaded
        && !track_uri.is_empty()
        && track_uri == zi.next_uri
        && zi.next_index >= 0
        && (zi.next_index as usize) < zi.queue.len()
    {
        let flushed = flush_active_listen(zi, zone_id, FlushReason::NaturalEnd);
        zi.current_index = zi.next_index;
        zi.current_uri = zi.next_uri.clone();
        zi.next_preloaded = false;
        zi.next_uri.clear();
        zi.next_index = -1;
        let tr = zi.queue[zi.current_index as usize].track.clone();
        zi.status.position = Duration::ZERO;
        zi.status.duration = Duration::from_secs(tr.duration_sec.max(0) as u64);
        zi.loaded_at = Instant::now();
        zi.position_updated_at = Utc::now();
        return (true, flushed);
    }
    (false, None)
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
