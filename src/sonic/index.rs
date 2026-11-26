use std::collections::{HashMap, HashSet};
use std::sync::RwLock;

use crate::library::StoredSonicEmbedding;

use super::quantize::Int8UnitVec;

struct IndexInner {
    entries: Vec<(String, Int8UnitVec)>,
    positions: HashMap<String, usize>,
}

/// RAM-resident `int8` symmetric cosine index over analyzed local tracks.
/// Brute-force scan across 10,000 tracks takes ~1ms in release builds.
pub struct SonicIndex {
    inner: RwLock<IndexInner>,
}

impl Default for SonicIndex {
    fn default() -> Self {
        Self::new()
    }
}

impl SonicIndex {
    pub fn new() -> Self {
        Self {
            inner: RwLock::new(IndexInner {
                entries: Vec::new(),
                positions: HashMap::new(),
            }),
        }
    }

    pub fn load_from_blobs(&self, rows: Vec<StoredSonicEmbedding>) -> usize {
        let mut entries = Vec::with_capacity(rows.len());
        let mut positions = HashMap::with_capacity(rows.len());
        for row in rows {
            if let Some(vec) =
                Int8UnitVec::from_uint8_blob(&row.emb_uint8, row.emb_min, row.emb_scale)
            {
                let idx = entries.len();
                positions.insert(row.track_id.clone(), idx);
                entries.push((row.track_id, vec));
            }
        }
        let count = entries.len();
        let mut guard = self.inner.write().unwrap_or_else(|e| e.into_inner());
        guard.entries = entries;
        guard.positions = positions;
        count
    }

    pub fn upsert_track(&self, track_id: String, vec: Int8UnitVec) {
        let mut guard = self.inner.write().unwrap_or_else(|e| e.into_inner());
        if let Some(&idx) = guard.positions.get(&track_id) {
            guard.entries[idx].1 = vec;
        } else {
            let idx = guard.entries.len();
            guard.positions.insert(track_id.clone(), idx);
            guard.entries.push((track_id, vec));
        }
    }

    pub fn len(&self) -> usize {
        self.inner
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .entries
            .len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn has_track(&self, track_id: &str) -> bool {
        self.inner
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .positions
            .contains_key(track_id)
    }

    pub fn similar_tracks(&self, seed_track_id: &str, limit: usize) -> Vec<(String, f32)> {
        if limit == 0 {
            return Vec::new();
        }
        let guard = self.inner.read().unwrap_or_else(|e| e.into_inner());
        let Some(&seed_idx) = guard.positions.get(seed_track_id) else {
            return Vec::new();
        };
        let seed = &guard.entries[seed_idx].1;
        let mut scored: Vec<(String, f32)> = guard
            .entries
            .iter()
            .enumerate()
            .filter(|(i, _)| *i != seed_idx)
            .map(|(_, (id, vec))| (id.clone(), seed.cosine_similarity(vec)))
            .collect();
        scored.sort_by(|a, b| b.1.total_cmp(&a.1));
        scored.truncate(limit);
        scored
    }

    pub fn similar_to_vector(
        &self,
        query: &Int8UnitVec,
        exclude: &HashSet<String>,
        limit: usize,
    ) -> Vec<(String, f32)> {
        if limit == 0 {
            return Vec::new();
        }
        let guard = self.inner.read().unwrap_or_else(|e| e.into_inner());
        let mut scored: Vec<(String, f32)> = guard
            .entries
            .iter()
            .filter(|(id, _)| !exclude.contains(id))
            .map(|(id, vec)| (id.clone(), query.cosine_similarity(vec)))
            .collect();
        scored.sort_by(|a, b| b.1.total_cmp(&a.1));
        scored.truncate(limit);
        scored
    }

    /// Computes a smooth multi-step path ("Sonic Adventure") from `start_id` to `end_id`
    /// by linearly interpolating target vectors in cosine space and picking nearest unvisited
    /// library tracks at each step.
    pub fn sonic_adventure(
        &self,
        start_id: &str,
        end_id: &str,
        intermediate_steps: usize,
    ) -> Vec<String> {
        let guard = self.inner.read().unwrap_or_else(|e| e.into_inner());
        let (Some(&start_idx), Some(&end_idx)) =
            (guard.positions.get(start_id), guard.positions.get(end_id))
        else {
            return Vec::new();
        };
        if start_idx == end_idx {
            return vec![start_id.to_string()];
        }

        let a = &guard.entries[start_idx].1;
        let b = &guard.entries[end_idx].1;
        let mut used = HashSet::new();
        used.insert(start_id.to_string());
        used.insert(end_id.to_string());

        let mut path = Vec::with_capacity(intermediate_steps + 2);
        path.push(start_id.to_string());

        for step in 1..=intermediate_steps {
            let t = step as f32 / (intermediate_steps + 1) as f32;
            let mut target = [0.0f32; super::quantize::EMBEDDING_DIM];
            for (i, dst) in target.iter_mut().enumerate() {
                let va = f32::from(a.data[i]) * a.scale;
                let vb = f32::from(b.data[i]) * b.scale;
                *dst = va * (1.0 - t) + vb * t;
            }
            let Some(q) = Int8UnitVec::from_raw_f32(&target) else {
                continue;
            };
            let best = guard
                .entries
                .iter()
                .filter(|(id, _)| !used.contains(id))
                .max_by(|(_, x), (_, y)| q.cosine_similarity(x).total_cmp(&q.cosine_similarity(y)));
            if let Some((best_id, _)) = best {
                used.insert(best_id.clone());
                path.push(best_id.clone());
            }
        }

        path.push(end_id.to_string());
        path
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sonic::quantize::EMBEDDING_DIM;

    fn basis_vec(hot: usize) -> Int8UnitVec {
        let mut raw = vec![0.0f32; EMBEDDING_DIM];
        raw[hot] = 1.0;
        Int8UnitVec::from_raw_f32(&raw).unwrap()
    }

    fn blend_vec(a: usize, b: usize, t: f32) -> Int8UnitVec {
        let mut raw = vec![0.0f32; EMBEDDING_DIM];
        raw[a] = 1.0 - t;
        raw[b] = t;
        Int8UnitVec::from_raw_f32(&raw).unwrap()
    }

    #[test]
    fn similar_tracks_and_sonic_adventure_rank_correctly() {
        let idx = SonicIndex::new();
        idx.upsert_track("t_start".into(), basis_vec(0));
        idx.upsert_track("t_mid".into(), blend_vec(0, 1, 0.5));
        idx.upsert_track("t_end".into(), basis_vec(1));
        idx.upsert_track("t_far".into(), basis_vec(2));

        let sim = idx.similar_tracks("t_start", 2);
        assert_eq!(sim.len(), 2);
        assert_eq!(sim[0].0, "t_mid");
        assert!(sim[0].1 > 0.65);

        let path = idx.sonic_adventure("t_start", "t_end", 1);
        assert_eq!(path, vec!["t_start", "t_mid", "t_end"]);
    }

    #[test]
    fn sonic_index_handles_empty_unknown_and_trivial_inputs() {
        let idx = SonicIndex::new();
        assert!(idx.similar_tracks("missing", 5).is_empty());
        assert!(idx.sonic_adventure("a", "b", 2).is_empty());

        idx.upsert_track("t1".into(), basis_vec(0));
        assert!(idx.similar_tracks("t1", 5).is_empty());
        assert!(idx.similar_tracks("t1", 0).is_empty());
        assert_eq!(idx.sonic_adventure("t1", "t1", 3), vec!["t1"]);
        assert!(idx.sonic_adventure("t1", "missing", 1).is_empty());

        idx.upsert_track("t2".into(), basis_vec(1));
        assert_eq!(idx.sonic_adventure("t1", "t2", 0), vec!["t1", "t2"]);
    }
}
