use std::sync::Arc;
use std::time::Duration;

use crate::library::{Track, normalize_isrc};
use crate::qobuz::{Client as QobuzClient, DynamicSuggestSeed};

use crate::radio::rank::source_base_weight;
use crate::radio::seed::ResolvedSeed;
use crate::radio::types::{Candidate, CandidateSource};

const QOBUZ_SOURCE_TIMEOUT: Duration = Duration::from_secs(4);

/// Generates candidates from Qobuz, preferring the fast single-request `/radio/*` endpoints
/// (`GET /radio/artist`, `GET /radio/album`, `GET /radio/track`) and blending with
/// `POST /dynamic/suggest` + concurrent similar-artist/album fan-out when needed.
pub async fn candidates_from_qobuz(
    seed: &ResolvedSeed,
    qobuz: &Arc<QobuzClient>,
    listened_qobuz_ids: &[i64],
    limit: usize,
) -> Vec<Candidate> {
    if !qobuz.is_connected() || limit == 0 {
        return Vec::new();
    }

    match tokio::time::timeout(
        QOBUZ_SOURCE_TIMEOUT,
        fetch_qobuz_candidates(seed, qobuz, listened_qobuz_ids, limit),
    )
    .await
    {
        Ok(cands) => cands,
        Err(_) => {
            tracing::debug!(seed = %seed.label, "qobuz radio source timed out");
            Vec::new()
        }
    }
}

async fn fetch_qobuz_candidates(
    seed: &ResolvedSeed,
    qobuz: &Arc<QobuzClient>,
    listened_qobuz_ids: &[i64],
    limit: usize,
) -> Vec<Candidate> {
    let mut out = Vec::new();

    // 1. Primary fast path: Qobuz's dedicated single-request /radio/{artist,album,track}
    let radio_tracks = fetch_dedicated_radio(seed, qobuz).await;
    if !radio_tracks.is_empty() {
        let base = source_base_weight(CandidateSource::QobuzDynamic);
        let n = radio_tracks.len().max(1) as f32;
        for (i, track) in radio_tracks.into_iter().enumerate() {
            let decay = 1.0 - (i as f32 / (n * 3.0));
            out.push(Candidate {
                track,
                score: base * decay,
                source: CandidateSource::QobuzDynamic,
            });
        }
    }

    // 2. For Track/Album Radio (or if /radio/* was empty), blend POST /dynamic/suggest
    if seed.kind != "artist" || out.len() < limit / 2 {
        let suggest_seeds = resolve_dynamic_seeds(seed, qobuz).await;
        if !suggest_seeds.is_empty()
            && let Ok(tracks) = qobuz
                .dynamic_suggest(&suggest_seeds, listened_qobuz_ids, (limit as i32).min(30))
                .await
        {
            let base = source_base_weight(CandidateSource::QobuzDynamic);
            let n = tracks.len().max(1) as f32;
            for (i, track) in tracks.into_iter().enumerate() {
                let decay = 1.0 - (i as f32 / (n * 2.5));
                out.push(Candidate {
                    track,
                    score: base * decay,
                    source: CandidateSource::QobuzDynamic,
                });
            }
        }
    }

    // 3. Fallback fan-out (concurrent) only when /radio/* + /dynamic/suggest came up thin
    if out.len() < limit / 2 {
        if !seed.qobuz_album_id.is_empty() && seed.kind == "album" {
            out.extend(fetch_similar_album_tracks(&seed.qobuz_album_id, qobuz).await);
        } else if seed.qobuz_artist_id > 0 {
            out.extend(fetch_similar_artist_tracks(seed.qobuz_artist_id, qobuz).await);
        }
    }

    out
}

async fn fetch_dedicated_radio(seed: &ResolvedSeed, qobuz: &Arc<QobuzClient>) -> Vec<Track> {
    match seed.kind {
        "artist" => {
            let aid = if seed.qobuz_artist_id > 0 {
                seed.qobuz_artist_id
            } else if !seed.artist_name.is_empty() {
                qobuz
                    .get_artist_by_name(&seed.artist_name, 1)
                    .await
                    .map(|d| d.artist.id)
                    .unwrap_or(0)
            } else {
                0
            };
            if aid > 0 {
                qobuz.radio_artist(aid).await.unwrap_or_default()
            } else {
                Vec::new()
            }
        }
        "album" => {
            if !seed.qobuz_album_id.is_empty() {
                qobuz
                    .radio_album(&seed.qobuz_album_id)
                    .await
                    .unwrap_or_default()
            } else {
                Vec::new()
            }
        }
        "track" => {
            let tid = seed.seed_track.as_ref().map_or(0, |t| t.qobuz_track_id);
            if tid > 0 {
                qobuz.radio_track(tid).await.unwrap_or_default()
            } else {
                Vec::new()
            }
        }
        _ => Vec::new(),
    }
}

async fn fetch_similar_album_tracks(album_id: &str, qobuz: &Arc<QobuzClient>) -> Vec<Candidate> {
    let Ok(sim_albums) = qobuz.similar_albums(album_id, 6).await else {
        return Vec::new();
    };
    let mut set = tokio::task::JoinSet::new();
    for (ai, alb) in sim_albums.into_iter().take(4).enumerate() {
        let q = Arc::clone(qobuz);
        set.spawn(async move {
            let raw_id = alb.id.strip_prefix("qobuz:").unwrap_or(&alb.id);
            let full = q.get_album(raw_id).await.ok()?;
            Some((ai, full.tracks))
        });
    }
    let base = source_base_weight(CandidateSource::QobuzSimilar);
    let mut out = Vec::new();
    while let Some(res) = set.join_next().await {
        if let Ok(Some((ai, tracks))) = res {
            for (ti, tr) in tracks.into_iter().take(3).enumerate() {
                out.push(Candidate {
                    track: tr,
                    score: base - (ai as f32 * 0.04) - (ti as f32 * 0.02),
                    source: CandidateSource::QobuzSimilar,
                });
            }
        }
    }
    out
}

async fn fetch_similar_artist_tracks(artist_id: i64, qobuz: &Arc<QobuzClient>) -> Vec<Candidate> {
    let Ok(sim_artists) = qobuz.similar_artists(artist_id, 8).await else {
        return Vec::new();
    };
    let mut set = tokio::task::JoinSet::new();
    for (ai, art) in sim_artists.into_iter().take(5).enumerate() {
        let q = Arc::clone(qobuz);
        set.spawn(async move {
            let detail = q.get_artist(art.id, 4).await.ok()?;
            Some((ai, detail.tracks))
        });
    }
    let base = source_base_weight(CandidateSource::QobuzSimilar);
    let mut out = Vec::new();
    while let Some(res) = set.join_next().await {
        if let Ok(Some((ai, tracks))) = res {
            for (ti, tr) in tracks.into_iter().take(3).enumerate() {
                out.push(Candidate {
                    track: tr,
                    score: base - (ai as f32 * 0.03) - (ti as f32 * 0.02),
                    source: CandidateSource::QobuzSimilar,
                });
            }
        }
    }
    out
}

async fn resolve_dynamic_seeds(
    seed: &ResolvedSeed,
    qobuz: &Arc<QobuzClient>,
) -> Vec<DynamicSuggestSeed> {
    let mut seeds = Vec::new();
    for tr in seed.seed_tracks.iter().take(3) {
        if let Some(ds) = track_to_suggest_seed(tr) {
            seeds.push(ds);
        }
    }
    if !seeds.is_empty() {
        return seeds;
    }

    // If the seed is a local track with an ISRC (or artist+title), resolve its Qobuz edition once.
    if let Some(tr) = seed.seed_tracks.first() {
        let isrc = normalize_isrc(&tr.isrc);
        let query = if !isrc.is_empty() {
            isrc
        } else if !tr.artist.is_empty() && !tr.title.is_empty() {
            format!("{} {}", tr.artist, tr.title)
        } else {
            return seeds;
        };
        if let Ok(res) = qobuz.search(&query, 5, 0).await
            && let Some(matched) = res
                .tracks
                .into_iter()
                .find_map(|qt| track_to_suggest_seed(&qt))
        {
            seeds.push(matched);
        }
    }
    seeds
}

fn track_to_suggest_seed(tr: &Track) -> Option<DynamicSuggestSeed> {
    if tr.qobuz_track_id > 0
        && tr.qobuz_artist_id > 0
        && tr.qobuz_genre_id > 0
        && tr.qobuz_label_id > 0
    {
        Some(DynamicSuggestSeed {
            track_id: tr.qobuz_track_id,
            artist_id: tr.qobuz_artist_id,
            genre_id: tr.qobuz_genre_id,
            label_id: tr.qobuz_label_id,
        })
    } else {
        None
    }
}
