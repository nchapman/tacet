use std::sync::Arc;

use crate::library::{Catalog, SOURCE_TYPE_LOCAL, TrackFilter};

use crate::radio::rank::source_base_weight;
use crate::radio::seed::ResolvedSeed;
use crate::radio::types::{Candidate, CandidateSource, RadioScope};

/// Pure-offline SQLite fallback so radio always builds a full station even without
/// internet access, Qobuz, or completed sonic analysis.
pub fn candidates_from_fallback(
    seed: &ResolvedSeed,
    catalog: &Arc<Catalog>,
    scope: RadioScope,
    limit: usize,
) -> Vec<Candidate> {
    if limit == 0 {
        return Vec::new();
    }
    let source_type = match scope {
        RadioScope::Local => SOURCE_TYPE_LOCAL.to_string(),
        RadioScope::All => String::new(),
    };
    let base = source_base_weight(CandidateSource::LocalFallback);
    let mut out: Vec<Candidate> = Vec::new();

    // 1. Seed's own tracks (for Album / Artist radio — give the station an anchor!)
    if seed.seed_track.is_none() {
        for (i, tr) in seed.seed_tracks.iter().take(4).enumerate() {
            if scope == RadioScope::Local && tr.source_type != SOURCE_TYPE_LOCAL {
                continue;
            }
            out.push(Candidate {
                track: tr.clone(),
                score: base + 0.30 - (i as f32 * 0.03),
                source: CandidateSource::LocalFallback,
            });
        }
    }

    // 2. Other tracks by the same artist in library
    if !seed.artist_name.is_empty()
        && let Ok((tracks, _)) = catalog.query_tracks(&TrackFilter {
            artist: seed.artist_name.clone(),
            source_type: source_type.clone(),
            limit: 12,
            ..Default::default()
        })
    {
        for tr in tracks {
            out.push(Candidate {
                track: tr,
                score: base + 0.12,
                source: CandidateSource::LocalFallback,
            });
        }
    }

    // 3. Same genre tracks in library
    if !seed.genre.is_empty()
        && let Ok((tracks, _)) = catalog.query_tracks(&TrackFilter {
            genre: seed.genre.clone(),
            source_type: source_type.clone(),
            limit: (limit as i32).max(30),
            ..Default::default()
        })
    {
        for tr in tracks {
            let era_bonus = if seed.year > 0 && tr.year > 0 {
                let diff = (seed.year - tr.year).abs();
                if diff <= 5 {
                    0.10
                } else if diff <= 12 {
                    0.05
                } else {
                    0.0
                }
            } else {
                0.0
            };
            out.push(Candidate {
                track: tr,
                score: base + 0.08 + era_bonus,
                source: CandidateSource::LocalFallback,
            });
        }
    }

    // 4. Broad library fallback (lowest priority) so tiny offline libraries still get a playable station
    if out.len() < limit
        && let Ok((tracks, _)) = catalog.query_tracks(&TrackFilter {
            source_type,
            sort: "plays".into(),
            sort_dir: "desc".into(),
            limit: (limit as i32).max(40),
            ..Default::default()
        })
    {
        for tr in tracks {
            out.push(Candidate {
                track: tr,
                score: base * 0.5,
                source: CandidateSource::LocalFallback,
            });
        }
    }

    out
}
