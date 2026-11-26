use rand::Rng;
use rand::seq::SliceRandom;

use crate::library::RecentPlayRecency;
use crate::queue::types::Item;

/// Lookback window for freshness-aware shuffle (14 days).
pub const FRESH_SHUFFLE_WINDOW_SECS: i64 = 14 * 24 * 3600;

/// Number of independent random permutations evaluated per shuffle.
const CANDIDATE_PERMUTATIONS: usize = 16;

/// Penalty applied when two consecutive tracks share the same normalized artist.
const ADJACENT_ARTIST_PENALTY: f64 = 0.35;

/// Spotify-style "Fewer Repeats" shuffle:
/// 1. Generates `K` independent random (Fisher-Yates) permutations of `items`.
/// 2. Scores each permutation for freshness (penalizing recently played tracks that land
///    near the front) and perceived variety (mild penalty for back-to-back same artist).
/// 3. Returns the highest-scoring random permutation.
///
/// Every candidate is a genuine random permutation, so ordering remains unpredictable while
/// naturally surfacing fresher tracks first and avoiding obvious clumps.
pub fn fresh_shuffle<R: Rng>(
    mut items: Vec<Item>,
    current_artist: Option<&str>,
    recency: &RecentPlayRecency,
    now_unix: i64,
    rng: &mut R,
) -> Vec<Item> {
    if items.len() <= 1 {
        return items;
    }
    if items.len() == 2 {
        items.shuffle(rng);
        return items;
    }

    // Precompute per-item recency weight (0.0 = fresh/unplayed, 1.0 = just played)
    // and normalized artist key once so scoring K permutations is pure arithmetic.
    let meta: Vec<(f64, String)> = items
        .iter()
        .map(|it| {
            let rec = recency
                .last_played_at(&it.track)
                .map_or(0.0, |ts| recency_weight(now_unix - ts));
            (rec, norm_artist(&it.track.artist))
        })
        .collect();

    let cur_artist_norm = current_artist.map(norm_artist).unwrap_or_default();

    let n = items.len();
    let mut best_order: Vec<usize> = (0..n).collect();
    let mut best_score = f64::NEG_INFINITY;
    let mut trial: Vec<usize> = (0..n).collect();

    for _ in 0..CANDIDATE_PERMUTATIONS {
        trial.shuffle(rng);
        let score = score_permutation(&trial, &meta, &cur_artist_norm);
        if score > best_score {
            best_score = score;
            best_order.clone_from(&trial);
        }
    }

    let mut slots: Vec<Option<Item>> = items.into_iter().map(Some).collect();
    best_order
        .into_iter()
        .filter_map(|idx| slots[idx].take())
        .collect()
}

fn score_permutation(order: &[usize], meta: &[(f64, String)], current_artist: &str) -> f64 {
    let mut score = 0.0f64;
    let mut prev_artist = current_artist;

    for (pos, &idx) in order.iter().enumerate() {
        let pos_w = position_decay(pos);
        let (rec_w, artist) = &meta[idx];

        // Penalize recently played tracks appearing early in the queue
        if *rec_w > 0.0 {
            score -= rec_w * pos_w;
        }

        // Mild penalty for back-to-back same artist (weighted toward the front)
        if !artist.is_empty() && artist == prev_artist {
            score -= ADJACENT_ARTIST_PENALTY * pos_w;
        }
        prev_artist = artist.as_str();
    }

    score
}

/// Smooth recency weight in `[0.0, 1.0]`: 1.0 for tracks just played, decaying linearly
/// to 0.0 at `FRESH_SHUFFLE_WINDOW_SECS`.
fn recency_weight(age_sec: i64) -> f64 {
    if age_sec <= 0 {
        return 1.0;
    }
    if age_sec >= FRESH_SHUFFLE_WINDOW_SECS {
        return 0.0;
    }
    1.0 - (age_sec as f64 / FRESH_SHUFFLE_WINDOW_SECS as f64)
}

/// Position importance weight: front of the shuffled queue matters most.
fn position_decay(pos: usize) -> f64 {
    1.0 / (1.0 + pos as f64 * 0.35)
}

fn norm_artist(artist: &str) -> String {
    artist.trim().to_lowercase()
}

#[cfg(test)]
mod tests {
    use rand::SeedableRng;
    use rand::rngs::StdRng;

    use crate::library::Track;

    use super::*;

    fn mk_item(id: &str, artist: &str, isrc: &str) -> Item {
        Item {
            item_id: format!("qi-{id}"),
            track: Track {
                id: id.into(),
                artist: artist.into(),
                title: format!("Title {id}"),
                isrc: isrc.into(),
                ..Default::default()
            },
            source_context: None,
        }
    }

    #[test]
    fn fresh_shuffle_pushes_recently_played_tracks_later_on_average() {
        let now = 1_700_000_000i64;
        let mut recency = RecentPlayRecency::default();
        // Mark 4 tracks as played 10 minutes ago (including one via cross-provider ISRC)
        recency.track_last_played.insert("t0".into(), now - 600);
        recency.track_last_played.insert("t1".into(), now - 600);
        recency.track_last_played.insert("t2".into(), now - 600);
        recency
            .isrc_last_played
            .insert("US0000000003".into(), now - 600);

        let items: Vec<Item> = (0..12)
            .map(|i| {
                let isrc = if i == 3 { "US0000000003" } else { "" };
                mk_item(&format!("t{i}"), &format!("Artist {i}"), isrc)
            })
            .collect();

        let mut rng = StdRng::seed_from_u64(42);
        let trials = 100;
        let mut recent_pos_sum = 0usize;
        let mut fresh_pos_sum = 0usize;

        for _ in 0..trials {
            let shuffled = fresh_shuffle(items.clone(), None, &recency, now, &mut rng);
            assert_eq!(shuffled.len(), items.len());
            for (pos, it) in shuffled.iter().enumerate() {
                if matches!(it.track.id.as_str(), "t0" | "t1" | "t2" | "t3") {
                    recent_pos_sum += pos;
                } else {
                    fresh_pos_sum += pos;
                }
            }
        }

        let recent_avg = recent_pos_sum as f64 / (trials * 4) as f64;
        let fresh_avg = fresh_pos_sum as f64 / (trials * 8) as f64;
        assert!(
            recent_avg > fresh_avg + 1.0,
            "expected recent tracks ({recent_avg:.2}) to land later than fresh tracks ({fresh_avg:.2})"
        );
    }

    #[test]
    fn fresh_shuffle_reduces_back_to_back_same_artist() {
        let now = 1_700_000_000i64;
        let recency = RecentPlayRecency::default();
        let items = vec![
            mk_item("a1", "Miles Davis", ""),
            mk_item("a2", "Miles Davis", ""),
            mk_item("a3", "Miles Davis", ""),
            mk_item("b1", "Bill Evans", ""),
            mk_item("c1", "John Coltrane", ""),
            mk_item("d1", "Herbie Hancock", ""),
        ];

        let mut rng = StdRng::seed_from_u64(99);
        let mut first_is_miles = 0usize;
        for _ in 0..50 {
            let shuffled =
                fresh_shuffle(items.clone(), Some("Miles Davis"), &recency, now, &mut rng);
            assert_eq!(shuffled.len(), 6);
            if shuffled[0].track.artist == "Miles Davis" {
                first_is_miles += 1;
            }
        }
        assert!(
            first_is_miles < 10,
            "expected current artist to rarely repeat immediately at pos 0 (got {first_is_miles}/50)"
        );
    }

    #[test]
    fn fresh_shuffle_handles_trivial_sizes() {
        let recency = RecentPlayRecency::default();
        let mut rng = StdRng::seed_from_u64(1);
        assert!(fresh_shuffle(vec![], None, &recency, 0, &mut rng).is_empty());
        let one = vec![mk_item("t1", "A", "")];
        assert_eq!(fresh_shuffle(one, None, &recency, 0, &mut rng).len(), 1);
        let two = vec![mk_item("t1", "A", ""), mk_item("t2", "B", "")];
        assert_eq!(fresh_shuffle(two, None, &recency, 0, &mut rng).len(), 2);
    }
}
