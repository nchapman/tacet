use std::time::Instant;

use chrono::{DateTime, Utc};

use crate::library::{PlayContext, PlayEventRecord, PlayOutcome, Track};
use crate::queue::types::PlaybackState;

use super::ZoneInner;

/// Caps a single wall-clock tick so laptop sleep or clock jumps cannot inflate listened time.
const MAX_TICK_DELTA_MS: u64 = 15_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum FlushReason {
    /// Track played to its natural end (gapless transition, queue advance, or repeat-one wrap).
    NaturalEnd,
    /// User explicitly left the track (next, prev, jump, play_tracks, stop, clear, remove).
    UserSkip,
    /// Playback was interrupted externally (foreign URI takeover, zone disconnect, shutdown).
    Interrupted,
}

#[derive(Debug, Clone)]
pub(super) struct ActiveListen {
    pub item_id: String,
    pub track: Track,
    pub source_context: PlayContext,
    pub started_at: DateTime<Utc>,
    pub listened_ms: u64,
    pub playing_since: Option<Instant>,
    pub flushed: bool,
}

impl ActiveListen {
    pub fn start(
        item_id: String,
        track: Track,
        source_context: PlayContext,
        now_wall: DateTime<Utc>,
        now_mono: Instant,
    ) -> Self {
        Self {
            item_id,
            track,
            source_context,
            started_at: now_wall,
            listened_ms: 0,
            playing_since: Some(now_mono),
            flushed: false,
        }
    }

    /// Accumulates elapsed time since `playing_since` (if currently playing).
    pub fn tick(&mut self, playing: bool, now: Instant) {
        if let Some(since) = self.playing_since {
            let delta_ms = now
                .saturating_duration_since(since)
                .as_millis()
                .min(u128::from(MAX_TICK_DELTA_MS)) as u64;
            self.listened_ms = self.listened_ms.saturating_add(delta_ms);
        }
        self.playing_since = if playing { Some(now) } else { None };
    }

    /// Finalizes this listen into a `PlayEventRecord` if it meets the minimum signal threshold.
    /// Marks `flushed = true` so the same listen can never be emitted twice.
    pub fn finalize(
        &mut self,
        zone_id: &str,
        reason: FlushReason,
        duration_hint_sec: i32,
        now_wall: DateTime<Utc>,
        now_mono: Instant,
    ) -> Option<PlayEventRecord> {
        if self.flushed {
            return None;
        }
        self.tick(false, now_mono);
        self.flushed = true;

        let duration_sec = if duration_hint_sec > 0 {
            duration_hint_sec
        } else {
            self.track.duration_sec.max(0)
        };
        let listened_sec = (self.listened_ms / 1000) as i32;
        let outcome = classify_outcome(listened_sec, duration_sec, reason)?;

        Some(PlayEventRecord {
            track: self.track.clone(),
            zone_id: zone_id.to_string(),
            source_context: self.source_context,
            started_at: self.started_at,
            played_at: now_wall,
            listened_sec,
            duration_sec,
            outcome,
        })
    }
}

/// Classifies a finalized listen using ListenBrainz/Last.fm completion thresholds:
/// - `Completed`: listened >= min(duration / 2, 240s) (min 5s, or min 2s for tracks < 10s),
///   or natural end with >= 5s listened.
/// - `Skipped`: explicit user skip after >= 2s listened.
/// - `Partial`: non-skip interruption after >= 5s listened.
/// - `None`: accidental bounce (< 2s) or unplayed load.
pub(super) fn classify_outcome(
    listened_sec: i32,
    duration_sec: i32,
    reason: FlushReason,
) -> Option<PlayOutcome> {
    let threshold = if duration_sec >= 10 {
        (duration_sec / 2).clamp(5, 240)
    } else if duration_sec > 0 {
        (duration_sec / 2).max(2)
    } else {
        30
    };

    if listened_sec >= threshold || (reason == FlushReason::NaturalEnd && listened_sec >= 5) {
        Some(PlayOutcome::Completed)
    } else if reason == FlushReason::UserSkip && listened_sec >= 2 {
        Some(PlayOutcome::Skipped)
    } else if reason != FlushReason::UserSkip && listened_sec >= 5 {
        Some(PlayOutcome::Partial)
    } else {
        None
    }
}

/// Flushes and clears `zi.active_listen`, returning a `PlayEventRecord` if worth persisting.
pub(super) fn flush_active_listen(
    zi: &mut ZoneInner,
    zone_id: &str,
    reason: FlushReason,
) -> Option<PlayEventRecord> {
    let dur_hint = zi.status.duration.as_secs() as i32;
    let mut active = zi.active_listen.take()?;
    active.finalize(zone_id, reason, dur_hint, Utc::now(), Instant::now())
}

/// Synchronizes `zi.active_listen` with the current queue item and transport state.
/// Returns a finalized `PlayEventRecord` if the active item changed unexpectedly.
pub(super) fn sync_active_listen(
    zi: &mut ZoneInner,
    zone_id: &str,
    state: PlaybackState,
) -> Option<PlayEventRecord> {
    let now_mono = Instant::now();
    let now_wall = Utc::now();
    let is_playing = state == PlaybackState::Playing;

    let (item_id, item_ctx, item_track) = {
        let cur_item = if zi.current_index >= 0 {
            zi.queue.get(zi.current_index as usize)
        } else {
            None
        };
        let Some(item) = cur_item else {
            return flush_active_listen(zi, zone_id, FlushReason::Interrupted);
        };
        (
            item.item_id.clone(),
            item.source_context,
            item.track.clone(),
        )
    };

    // If we're already tracking this exact queue item, just tick the accumulator.
    if let Some(active) = zi.active_listen.as_mut()
        && active.item_id == item_id
    {
        active.tick(is_playing, now_mono);
        return None;
    }

    // Current item changed without an explicit flush — flush the old listen first.
    let flushed = flush_active_listen(zi, zone_id, FlushReason::NaturalEnd);

    if is_playing {
        let ctx = item_ctx.unwrap_or(if zi.shuffle {
            PlayContext::Shuffle
        } else {
            PlayContext::Manual
        });
        zi.active_listen = Some(ActiveListen::start(
            item_id, item_track, ctx, now_wall, now_mono,
        ));
    }

    flushed
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn sample_track(dur: i32) -> Track {
        Track {
            id: "local:tr1".into(),
            album_id: "local:alb1".into(),
            provider: "local".into(),
            title: "Track 1".into(),
            artist: "Artist A".into(),
            duration_sec: dur,
            ..Default::default()
        }
    }

    #[test]
    fn classify_outcome_follows_completion_and_skip_rules() {
        // Standard 3-minute track (180s -> threshold 90s)
        assert_eq!(
            classify_outcome(1, 180, FlushReason::UserSkip),
            None,
            "under 2s skip is ignored"
        );
        assert_eq!(
            classify_outcome(12, 180, FlushReason::UserSkip),
            Some(PlayOutcome::Skipped)
        );
        assert_eq!(
            classify_outcome(45, 180, FlushReason::Interrupted),
            Some(PlayOutcome::Partial)
        );
        assert_eq!(
            classify_outcome(90, 180, FlushReason::UserSkip),
            Some(PlayOutcome::Completed)
        );

        // Long 12-minute track (720s -> threshold capped at 240s)
        assert_eq!(
            classify_outcome(239, 720, FlushReason::UserSkip),
            Some(PlayOutcome::Skipped)
        );
        assert_eq!(
            classify_outcome(240, 720, FlushReason::UserSkip),
            Some(PlayOutcome::Completed)
        );

        // Natural end with >= 5s counts as completed (e.g. shorter actual file)
        assert_eq!(
            classify_outcome(10, 180, FlushReason::NaturalEnd),
            Some(PlayOutcome::Completed)
        );
        assert_eq!(classify_outcome(3, 180, FlushReason::NaturalEnd), None);
    }

    #[test]
    fn active_listen_prevents_double_flush_and_caps_sleep_jumps() {
        let now_wall = Utc::now();
        let t0 = Instant::now();
        let mut active = ActiveListen::start(
            "item-1".into(),
            sample_track(200),
            PlayContext::Radio,
            now_wall,
            t0,
        );

        // Simulate a 60s clock jump (e.g. laptop sleep) — must cap at 15s
        active.tick(true, t0 + Duration::from_secs(60));
        assert_eq!(active.listened_ms, MAX_TICK_DELTA_MS);

        // Pause, wait 30s, resume — paused time must not accumulate
        active.tick(false, t0 + Duration::from_secs(62));
        assert_eq!(active.listened_ms, MAX_TICK_DELTA_MS + 2_000);
        active.tick(true, t0 + Duration::from_secs(92));
        assert_eq!(active.listened_ms, MAX_TICK_DELTA_MS + 2_000);

        // First finalize produces a Skipped record (17s < 100s threshold)
        let rec = active
            .finalize(
                "zone-1",
                FlushReason::UserSkip,
                200,
                now_wall + chrono::Duration::seconds(92),
                t0 + Duration::from_secs(92),
            )
            .expect("should emit skipped event");
        assert_eq!(rec.outcome, PlayOutcome::Skipped);
        assert_eq!(rec.listened_sec, 17);
        assert_eq!(rec.source_context, PlayContext::Radio);
        assert_eq!(rec.zone_id, "zone-1");

        // Second finalize must return None (no double-counting)
        assert!(
            active
                .finalize(
                    "zone-1",
                    FlushReason::NaturalEnd,
                    200,
                    now_wall + chrono::Duration::seconds(95),
                    t0 + Duration::from_secs(95),
                )
                .is_none()
        );
    }
}
