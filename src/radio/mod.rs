pub mod rank;
pub mod seed;
pub mod sources;
pub mod types;

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;

use crate::cache::Cache;
use crate::library::{Catalog, RecentCooldowns};
use crate::qobuz::Client as QobuzClient;
use crate::sonic::SonicIndex;

use rank::{OpenerRule, RankContext, rank_candidates};
use seed::resolve_seed;
use sources::fallback::candidates_from_fallback;
use sources::listenbrainz::candidates_from_listenbrainz;
use sources::qobuz::candidates_from_qobuz;
use sources::sonic::candidates_from_sonic;

pub use types::{
    Candidate, CandidateSource, DEFAULT_RADIO_LIMIT, MAX_RADIO_LIMIT, RadioError, RadioRequest,
    RadioScope, RadioSeed, RadioStation,
};

const COOLDOWN_WINDOW_SECS: i64 = 6 * 3600;
const FEEDBACK_WINDOW_SECS: i64 = 90 * 24 * 3600;
const DEFAULT_JITTER: f32 = 0.04;

pub struct RadioEngine {
    catalog: Arc<Catalog>,
    qobuz: Option<Arc<QobuzClient>>,
    sonic_index: Arc<SonicIndex>,
    cache: Option<Arc<Cache>>,
    http: reqwest::Client,
    /// Disabled in offline unit tests that shouldn't hit external ListenBrainz.
    enable_listenbrainz: bool,
}

impl RadioEngine {
    pub fn new(
        catalog: Arc<Catalog>,
        qobuz: Option<Arc<QobuzClient>>,
        sonic_index: Arc<SonicIndex>,
        cache: Option<Arc<Cache>>,
    ) -> Self {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(3))
            .build()
            .unwrap_or_default();
        Self {
            catalog,
            qobuz,
            sonic_index,
            cache,
            http,
            enable_listenbrainz: true,
        }
    }

    #[cfg(test)]
    pub(crate) fn with_listenbrainz(mut self, enabled: bool) -> Self {
        self.enable_listenbrainz = enabled;
        self
    }

    pub async fn build_station(&self, req: RadioRequest) -> Result<RadioStation, RadioError> {
        let limit = req.clamped_limit();
        let resolved =
            resolve_seed(&req.seed, req.scope, &self.catalog, self.qobuz.as_ref()).await?;

        let now = Utc::now().timestamp();
        let cooldowns: RecentCooldowns = self
            .catalog
            .recent_cooldowns(now - COOLDOWN_WINDOW_SECS)
            .unwrap_or_default();
        let feedback = self
            .catalog
            .track_feedback_since(now - FEEDBACK_WINDOW_SECS)
            .unwrap_or_else(|_| HashMap::new());

        let listened_qobuz_ids: Vec<i64> = cooldowns.qobuz_track_ids.iter().copied().collect();
        let fetch_limit = (limit * 2).max(30);

        // Local sources (fast, synchronous/in-memory)
        let sonic_cands =
            candidates_from_sonic(&resolved, &self.catalog, &self.sonic_index, fetch_limit);
        let fallback_cands =
            candidates_from_fallback(&resolved, &self.catalog, req.scope, fetch_limit);

        // Remote sources (concurrent with per-source timeouts)
        let qobuz_fut = async {
            if req.scope == RadioScope::All
                && let Some(q) = &self.qobuz
                && q.is_connected()
            {
                candidates_from_qobuz(&resolved, q, &listened_qobuz_ids, fetch_limit).await
            } else {
                Vec::new()
            }
        };
        let lb_fut = async {
            if self.enable_listenbrainz {
                candidates_from_listenbrainz(
                    &resolved,
                    &self.catalog,
                    self.qobuz.as_ref(),
                    self.cache.as_ref(),
                    &self.http,
                    req.scope,
                    fetch_limit,
                )
                .await
            } else {
                Vec::new()
            }
        };

        let (qobuz_cands, lb_cands) = tokio::join!(qobuz_fut, lb_fut);

        let mut all_candidates = Vec::with_capacity(
            sonic_cands.len() + qobuz_cands.len() + lb_cands.len() + fallback_cands.len(),
        );
        all_candidates.extend(sonic_cands);
        all_candidates.extend(qobuz_cands);
        all_candidates.extend(lb_cands);
        all_candidates.extend(fallback_cands);

        // Upgrade any remote candidates to local editions via ISRC-first matching before ranking
        let upgraded_tracks = self
            .catalog
            .prefer_local_tracks(all_candidates.iter().map(|c| c.track.clone()).collect());
        for (cand, upgraded) in all_candidates.iter_mut().zip(upgraded_tracks) {
            cand.track = upgraded;
        }

        let opener_rule = match &req.seed {
            RadioSeed::Track { .. } => OpenerRule::None,
            RadioSeed::Album { id } => OpenerRule::Album {
                album_id: id.as_str(),
                seed_tracks: &resolved.seed_tracks,
            },
            RadioSeed::Artist { .. } => OpenerRule::Artist {
                artist: &resolved.artist_name,
                qobuz_artist_id: resolved.qobuz_artist_id,
                seed_tracks: &resolved.seed_tracks,
            },
        };

        let mut rng = rand::rng();
        let ranked = rank_candidates(
            all_candidates,
            &RankContext {
                seed_track: resolved.seed_track.as_ref(),
                opener_rule,
                exclude_track_ids: &resolved.exclude_track_ids,
                scope: req.scope,
                limit,
                cooldowns: &cooldowns,
                feedback: &feedback,
                now_unix: now,
                jitter: DEFAULT_JITTER,
            },
            &mut rng,
        );

        if ranked.tracks.is_empty() {
            return Err(RadioError::EmptyStation);
        }

        Ok(RadioStation {
            seed_kind: resolved.kind.to_string(),
            seed_label: resolved.label,
            seed_track_id: resolved.seed_track.map(|t| t.id),
            tracks: ranked.tracks,
            source_counts: ranked.source_counts,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::library::{Album, SOURCE_LOCAL, SOURCE_TYPE_LOCAL, Track};
    use crate::sonic::quantize::{EMBEDDING_DIM, Int8UnitVec};

    fn basis(hot: usize) -> Int8UnitVec {
        let mut raw = vec![0.0f32; EMBEDDING_DIM];
        raw[hot] = 1.0;
        Int8UnitVec::from_raw_f32(&raw).unwrap()
    }

    fn blend(a: usize, b: usize, t: f32) -> Int8UnitVec {
        let mut raw = vec![0.0f32; EMBEDDING_DIM];
        raw[a] = 1.0 - t;
        raw[b] = t;
        Int8UnitVec::from_raw_f32(&raw).unwrap()
    }

    fn seed_local_library() -> (Arc<Catalog>, Arc<SonicIndex>) {
        let cat = Catalog::new_in_memory(vec![]);
        let items = [
            (
                "local:a1",
                "local:t1",
                "Miles Davis",
                "Kind of Blue",
                "So What",
                "Jazz",
                1959,
            ),
            (
                "local:a2",
                "local:t2",
                "Bill Evans",
                "Sunday",
                "Gloria's Step",
                "Jazz",
                1961,
            ),
            (
                "local:a3",
                "local:t3",
                "John Coltrane",
                "Blue Train",
                "Blue Train",
                "Jazz",
                1957,
            ),
            (
                "local:a4",
                "local:t4",
                "Daft Punk",
                "Discovery",
                "One More Time",
                "Electronic",
                2001,
            ),
        ];
        for (aid, tid, artist, album_title, title, genre, year) in items {
            let alb = Album {
                id: aid.into(),
                source: SOURCE_LOCAL.into(),
                source_type: SOURCE_TYPE_LOCAL.into(),
                provider: SOURCE_LOCAL.into(),
                external_id: aid.into(),
                title: album_title.into(),
                artist: artist.into(),
                genre: genre.into(),
                year,
                available: true,
                ..Default::default()
            };
            let tr = Track {
                id: tid.into(),
                album_id: aid.into(),
                source: SOURCE_LOCAL.into(),
                source_type: SOURCE_TYPE_LOCAL.into(),
                provider: SOURCE_LOCAL.into(),
                external_id: tid.into(),
                title: title.into(),
                artist: artist.into(),
                album_artist: artist.into(),
                album: album_title.into(),
                genre: genre.into(),
                year,
                duration_sec: 240,
                available: true,
                ..Default::default()
            };
            cat.store()
                .upsert_album_with_tracks(&alb, std::slice::from_ref(&tr))
                .unwrap();
        }

        let sonic = Arc::new(SonicIndex::new());
        sonic.upsert_track("local:t1".into(), basis(0));
        sonic.upsert_track("local:t2".into(), blend(0, 1, 0.25)); // closest to t1
        sonic.upsert_track("local:t3".into(), blend(0, 1, 0.65)); // moderate to t1
        sonic.upsert_track("local:t4".into(), basis(2)); // orthogonal to t1
        (cat, sonic)
    }

    #[tokio::test]
    async fn local_track_album_and_artist_radio_build_full_stations() {
        let (cat, sonic) = seed_local_library();
        let engine = RadioEngine::new(cat, None, sonic, None).with_listenbrainz(false);

        // 1. Track Radio: seed track pinned at index 0, closest sonic neighbor at index 1
        let track_station = engine
            .build_station(RadioRequest {
                seed: RadioSeed::Track {
                    id: "local:t1".into(),
                },
                limit: 4,
                scope: RadioScope::All,
            })
            .await
            .unwrap();
        assert_eq!(track_station.seed_kind, "track");
        assert_eq!(track_station.seed_track_id.as_deref(), Some("local:t1"));
        assert_eq!(track_station.tracks[0].id, "local:t1");
        assert_eq!(track_station.tracks[1].id, "local:t2");
        assert!(
            track_station
                .source_counts
                .get("sonic")
                .copied()
                .unwrap_or(0)
                >= 2
        );

        // 2. Album Radio: centroid finds similar tracks + includes anchor
        let album_station = engine
            .build_station(RadioRequest {
                seed: RadioSeed::Album {
                    id: "local:a1".into(),
                },
                limit: 4,
                scope: RadioScope::Local,
            })
            .await
            .unwrap();
        assert_eq!(album_station.seed_kind, "album");
        assert!(album_station.seed_track_id.is_none());
        assert_eq!(album_station.tracks.len(), 4);

        // 3. Artist Radio: resolves by local artist name
        let artist_station = engine
            .build_station(RadioRequest {
                seed: RadioSeed::Artist {
                    id: "Miles Davis".into(),
                },
                limit: 4,
                scope: RadioScope::Local,
            })
            .await
            .unwrap();
        assert_eq!(artist_station.seed_kind, "artist");
        assert_eq!(artist_station.seed_label, "Miles Davis");
        assert_eq!(artist_station.tracks.len(), 4);

        // 4. Unknown seed returns SeedNotFound
        let err = engine
            .build_station(RadioRequest {
                seed: RadioSeed::Track {
                    id: "local:missing".into(),
                },
                limit: 10,
                scope: RadioScope::Local,
            })
            .await
            .unwrap_err();
        assert!(matches!(err, RadioError::SeedNotFound(_)));
    }
}
