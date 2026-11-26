use std::collections::HashSet;
use std::sync::Arc;

use crate::library::Catalog;
use crate::sonic::SonicIndex;

use crate::radio::rank::source_base_weight;
use crate::radio::seed::ResolvedSeed;
use crate::radio::types::{Candidate, CandidateSource};

/// Generates local library candidates using `SonicIndex` cosine similarity.
/// Uses direct track vector lookup for Track Radio, or the centroid of all analyzed
/// tracks on the seed album/artist for Album/Artist Radio.
pub fn candidates_from_sonic(
    seed: &ResolvedSeed,
    catalog: &Arc<Catalog>,
    sonic_index: &Arc<SonicIndex>,
    limit: usize,
) -> Vec<Candidate> {
    if sonic_index.is_empty() || limit == 0 {
        return Vec::new();
    }

    let neighbors: Vec<(String, f32)> = if let Some(track) = &seed.seed_track {
        sonic_index.similar_tracks(&track.id, limit)
    } else {
        let seed_ids: Vec<String> = seed.seed_tracks.iter().map(|t| t.id.clone()).collect();
        let Some(centroid) = sonic_index.centroid_for_tracks(&seed_ids) else {
            return Vec::new();
        };
        let exclude: HashSet<String> = seed_ids.into_iter().collect();
        sonic_index.similar_to_vector(&centroid, &exclude, limit)
    };

    let base = source_base_weight(CandidateSource::Sonic);
    neighbors
        .into_iter()
        .filter(|(_, sim)| *sim > 0.25)
        .filter_map(|(track_id, sim)| {
            let track = catalog.track(&track_id)?;
            if !track.available {
                return None;
            }
            Some(Candidate {
                track,
                score: base * sim.clamp(0.0, 1.0),
                source: CandidateSource::Sonic,
            })
        })
        .collect()
}
