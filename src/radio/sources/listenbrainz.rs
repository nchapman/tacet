use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::cache::{Cache, Ttl};
use crate::library::{Catalog, SOURCE_TYPE_LOCAL, TrackFilter};
use crate::qobuz::Client as QobuzClient;

use crate::radio::rank::source_base_weight;
use crate::radio::seed::ResolvedSeed;
use crate::radio::types::{Candidate, CandidateSource, RadioScope};

const LB_SIMILAR_ARTISTS_URL: &str = "https://labs.api.listenbrainz.org/similar-artists/json";
const LB_ALGORITHM: &str =
    "session_based_days_7500_session_300_contribution_5_threshold_10_limit_100_filter_true_skip_30";
const LB_CACHE_NS: &str = "lb_similar_artists";
const LB_CACHE_TTL: Duration = Duration::from_secs(7 * 24 * 3600);
const LB_TIMEOUT: Duration = Duration::from_secs(2);
const LB_TOTAL_TIMEOUT: Duration = Duration::from_secs(3);

#[derive(Debug, Clone, Serialize, Deserialize)]
struct SimilarArtistHit {
    #[serde(default)]
    artist_mbid: String,
    #[serde(default)]
    name: String,
    #[serde(default)]
    score: f32,
}

/// Fetches similar artists from ListenBrainz Labs (cached 7 days in SQLite) using the
/// seed's MusicBrainz artist ID, then maps those artists onto playable local tracks
/// (and optionally Qobuz top tracks when `scope == All`).
pub async fn candidates_from_listenbrainz(
    seed: &ResolvedSeed,
    catalog: &Arc<Catalog>,
    qobuz: Option<&Arc<QobuzClient>>,
    cache: Option<&Arc<Cache>>,
    http: &reqwest::Client,
    scope: RadioScope,
    limit: usize,
) -> Vec<Candidate> {
    if seed.mb_artist_id.trim().is_empty() || limit == 0 {
        return Vec::new();
    }

    match tokio::time::timeout(
        LB_TOTAL_TIMEOUT,
        fetch_listenbrainz_candidates(seed, catalog, qobuz, cache, http, scope, limit),
    )
    .await
    {
        Ok(cands) => cands,
        Err(_) => {
            tracing::debug!(seed = %seed.label, "listenbrainz radio source timed out");
            Vec::new()
        }
    }
}

async fn fetch_listenbrainz_candidates(
    seed: &ResolvedSeed,
    catalog: &Arc<Catalog>,
    qobuz: Option<&Arc<QobuzClient>>,
    cache: Option<&Arc<Cache>>,
    http: &reqwest::Client,
    scope: RadioScope,
    limit: usize,
) -> Vec<Candidate> {
    let similar = fetch_similar_artists(&seed.mb_artist_id, cache, http).await;
    if similar.is_empty() {
        return Vec::new();
    }

    let source_type = match scope {
        RadioScope::Local => SOURCE_TYPE_LOCAL.to_string(),
        RadioScope::All => String::new(),
    };
    let base = source_base_weight(CandidateSource::ListenBrainz);
    let max_score = similar
        .iter()
        .map(|h| h.score)
        .fold(1.0f32, f32::max)
        .max(1.0);

    let mut out = Vec::new();
    let mut unmatched_for_qobuz: Vec<&SimilarArtistHit> = Vec::new();

    for hit in similar.iter().take(20) {
        if hit.name.trim().is_empty() {
            continue;
        }
        let norm_sim = (hit.score / max_score).clamp(0.25, 1.0);
        if let Ok((tracks, _)) = catalog.query_tracks(&TrackFilter {
            artist: hit.name.clone(),
            source_type: source_type.clone(),
            limit: 4,
            ..Default::default()
        }) && !tracks.is_empty()
        {
            for (i, tr) in tracks.into_iter().take(3).enumerate() {
                out.push(Candidate {
                    track: tr,
                    score: base * norm_sim - (i as f32 * 0.03),
                    source: CandidateSource::ListenBrainz,
                });
            }
            continue;
        }
        unmatched_for_qobuz.push(hit);
    }

    // If scope == All and local library didn't have enough matches, resolve top 3 via Qobuz
    if scope == RadioScope::All
        && out.len() < limit / 2
        && let Some(q) = qobuz
        && q.is_connected()
    {
        for hit in unmatched_for_qobuz.into_iter().take(3) {
            let norm_sim = (hit.score / max_score).clamp(0.25, 1.0);
            if let Ok(Ok(detail)) =
                tokio::time::timeout(LB_TIMEOUT, q.get_artist_by_name(&hit.name, 3)).await
            {
                for (i, tr) in detail.tracks.into_iter().take(2).enumerate() {
                    out.push(Candidate {
                        track: tr,
                        score: base * norm_sim - (i as f32 * 0.03),
                        source: CandidateSource::ListenBrainz,
                    });
                }
            }
        }
    }

    out
}

async fn fetch_similar_artists(
    mb_artist_id: &str,
    cache: Option<&Arc<Cache>>,
    http: &reqwest::Client,
) -> Vec<SimilarArtistHit> {
    let key = mb_artist_id.trim().to_lowercase();
    let cache_key = format!("{LB_CACHE_NS}:{key}");
    if let Some(c) = cache
        && let Ok(Some(bytes)) = c.get(&cache_key)
        && let Ok(cached) = serde_json::from_slice::<Vec<SimilarArtistHit>>(&bytes)
    {
        return cached;
    }

    let Some(hits) = request_similar_artists(&key, http).await else {
        return Vec::new();
    };
    if let Some(c) = cache
        && let Ok(bytes) = serde_json::to_vec(&hits)
    {
        let _ = c.set(&cache_key, &bytes, Ttl::After(LB_CACHE_TTL));
    }
    hits
}

async fn request_similar_artists(
    artist_mbid: &str,
    http: &reqwest::Client,
) -> Option<Vec<SimilarArtistHit>> {
    #[derive(Serialize)]
    struct ReqItem<'a> {
        artist_mbids: [&'a str; 1],
        algorithm: &'a str,
    }

    let body = [ReqItem {
        artist_mbids: [artist_mbid],
        algorithm: LB_ALGORITHM,
    }];
    let req = http
        .post(LB_SIMILAR_ARTISTS_URL)
        .header(
            "User-Agent",
            format!(
                "tacet/{} (https://github.com/nate-c/tacet)",
                env!("CARGO_PKG_VERSION")
            ),
        )
        .json(&body)
        .send();

    let resp = tokio::time::timeout(LB_TIMEOUT, req).await.ok()?.ok()?;
    if !resp.status().is_success() {
        return None;
    }
    resp.json::<Vec<SimilarArtistHit>>().await.ok()
}
