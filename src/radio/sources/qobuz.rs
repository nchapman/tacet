use std::sync::Arc;
use std::time::Duration;

use crate::library::{Track, normalize_isrc};
use crate::qobuz::{Client as QobuzClient, DynamicSuggestSeed};

use crate::radio::rank::source_base_weight;
use crate::radio::seed::ResolvedSeed;
use crate::radio::types::{Candidate, CandidateSource};

const QOBUZ_SOURCE_TIMEOUT: Duration = Duration::from_secs(3);

/// Generates candidates from Qobuz (`/dynamic/suggest`, `/artist/getSimilarArtists`,
/// and `/album/suggest`). Only runs when Qobuz is authenticated and `scope == All`.
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

    // 1. Try POST /dynamic/suggest if we can assemble a full 4-field seed
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

    // 2. For Album Radio (or if dynamic_suggest returned little), pull similar albums' tracks
    if !seed.qobuz_album_id.is_empty()
        && (seed.kind == "album" || out.len() < limit / 2)
        && let Ok(sim_albums) = qobuz.similar_albums(&seed.qobuz_album_id, 6).await
    {
        let base = source_base_weight(CandidateSource::QobuzSimilar);
        for (ai, alb) in sim_albums.into_iter().take(4).enumerate() {
            let raw_id = alb.id.strip_prefix("qobuz:").unwrap_or(&alb.id);
            if let Ok(full) = qobuz.get_album(raw_id).await {
                for (ti, tr) in full.tracks.into_iter().take(3).enumerate() {
                    let score = base - (ai as f32 * 0.04) - (ti as f32 * 0.02);
                    out.push(Candidate {
                        track: tr,
                        score,
                        source: CandidateSource::QobuzSimilar,
                    });
                }
            }
        }
    }

    // 3. For Artist Radio (or thin results), pull top tracks from similar artists
    if seed.qobuz_artist_id > 0
        && (seed.kind == "artist" || out.len() < limit / 2)
        && let Ok(sim_artists) = qobuz.similar_artists(seed.qobuz_artist_id, 8).await
    {
        let base = source_base_weight(CandidateSource::QobuzSimilar);
        for (ai, art) in sim_artists.into_iter().take(5).enumerate() {
            if let Ok(detail) = qobuz.get_artist(art.id, 4).await {
                for (ti, tr) in detail.tracks.into_iter().take(3).enumerate() {
                    let score = base - (ai as f32 * 0.03) - (ti as f32 * 0.02);
                    out.push(Candidate {
                        track: tr,
                        score,
                        source: CandidateSource::QobuzSimilar,
                    });
                }
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
