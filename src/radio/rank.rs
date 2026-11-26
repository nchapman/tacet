use std::collections::{BTreeMap, HashMap, HashSet};

use crate::library::{RecentCooldowns, SOURCE_TYPE_LOCAL, Track, TrackFeedback, normalize_isrc};

use super::types::{Candidate, CandidateSource, RadioScope};

const ARTIST_WINDOW: usize = 4;
const MAX_TRACKS_PER_ARTIST: usize = 3;

/// Optional rule to hoist a matching opener track to index 0 when no exact `seed_track`
/// is pinned (Album Radio hoists a track from the album; Artist Radio hoists the top
/// recommended track by the artist).
#[derive(Default)]
pub enum OpenerRule<'a> {
    #[default]
    None,
    Album {
        album_id: &'a str,
        seed_tracks: &'a [Track],
    },
    Artist {
        artist: &'a str,
        qobuz_artist_id: i64,
        seed_tracks: &'a [Track],
    },
}

pub struct RankContext<'a> {
    pub seed_track: Option<&'a Track>,
    pub opener_rule: OpenerRule<'a>,
    pub exclude_track_ids: &'a HashSet<String>,
    pub scope: RadioScope,
    pub limit: usize,
    pub cooldowns: &'a RecentCooldowns,
    pub feedback: &'a HashMap<String, TrackFeedback>,
    pub now_unix: i64,
    /// Deterministic jitter amplitude (0.0 disables jitter for tests).
    pub jitter: f32,
}

pub struct RankedStation {
    pub tracks: Vec<Track>,
    pub source_counts: BTreeMap<String, usize>,
}

/// Pure, I/O-free ranking pipeline:
/// 1. Filter by scope & excluded seed IDs (seed track itself is pinned first separately)
/// 2. Deduplicate across providers (preferring local tracks and highest relevance score)
/// 3. Apply listening-history cooldowns & skip/completion feedback
/// 4. Greedily space artists and cap per-artist repetitions
/// 5. Pin seed track at index 0 (for Track Radio) and tally source counts
pub fn rank_candidates<R: rand::Rng>(
    candidates: Vec<Candidate>,
    ctx: &RankContext<'_>,
    rng: &mut R,
) -> RankedStation {
    let deduped = deduplicate_candidates(candidates, ctx.scope, ctx.exclude_track_ids);
    let mut scored: Vec<Candidate> = deduped
        .into_iter()
        .map(|mut c| {
            c.score =
                apply_history_signals(c.score, &c.track, ctx.cooldowns, ctx.feedback, ctx.now_unix);
            if ctx.jitter > 0.0 {
                let delta: f32 = rng.random_range(-ctx.jitter..=ctx.jitter);
                c.score += delta;
            }
            c
        })
        .collect();

    // Sort highest score first; tie-break deterministically by track id.
    scored.sort_by(|a, b| {
        b.score
            .total_cmp(&a.score)
            .then_with(|| a.track.id.cmp(&b.track.id))
    });

    // Pin exact seed track (Track Radio) or hoist top matching opener (Album/Artist Radio)
    let opener = if let Some(seed) = ctx.seed_track {
        Some((seed.clone(), None))
    } else {
        take_opener(&mut scored, &ctx.opener_rule, ctx.scope)
    };

    let has_opener = opener.is_some();
    let target_tail = if has_opener {
        ctx.limit.saturating_sub(1)
    } else {
        ctx.limit
    };

    let selected = space_and_select(scored, target_tail, opener.as_ref().map(|(t, _)| t));

    let mut tracks = Vec::with_capacity(selected.len() + usize::from(has_opener));
    let mut source_counts: BTreeMap<String, usize> = BTreeMap::new();

    if let Some((track, source)) = opener {
        if let Some(src) = source {
            *source_counts.entry(src.as_str().to_string()).or_insert(0) += 1;
        }
        tracks.push(track);
    }
    for cand in selected {
        *source_counts
            .entry(cand.source.as_str().to_string())
            .or_insert(0) += 1;
        tracks.push(cand.track);
    }

    RankedStation {
        tracks,
        source_counts,
    }
}

fn take_opener(
    scored: &mut Vec<Candidate>,
    rule: &OpenerRule<'_>,
    scope: RadioScope,
) -> Option<(Track, Option<CandidateSource>)> {
    let (idx, fallback_tracks) = match rule {
        OpenerRule::None => return None,
        OpenerRule::Album {
            album_id,
            seed_tracks,
        } => {
            let seed_ids: HashSet<&str> = seed_tracks.iter().map(|t| t.id.as_str()).collect();
            let pos = scored.iter().position(|c| {
                (!album_id.is_empty() && c.track.album_id == *album_id)
                    || seed_ids.contains(c.track.id.as_str())
            });
            (pos, *seed_tracks)
        }
        OpenerRule::Artist {
            artist,
            qobuz_artist_id,
            seed_tracks,
        } => {
            let norm_artist = normalize_artist_key(artist);
            let seed_ids: HashSet<&str> = seed_tracks.iter().map(|t| t.id.as_str()).collect();
            let pos = scored.iter().position(|c| {
                (!norm_artist.is_empty() && normalize_artist_key(&c.track.artist) == norm_artist)
                    || (*qobuz_artist_id > 0 && c.track.qobuz_artist_id == *qobuz_artist_id)
                    || seed_ids.contains(c.track.id.as_str())
            });
            (pos, *seed_tracks)
        }
    };

    if let Some(i) = idx {
        let cand = scored.remove(i);
        return Some((cand.track, Some(cand.source)));
    }

    fallback_tracks
        .iter()
        .find(|t| scope == RadioScope::All || t.source_type == SOURCE_TYPE_LOCAL)
        .cloned()
        .map(|t| (t, None))
}

fn deduplicate_candidates(
    candidates: Vec<Candidate>,
    scope: RadioScope,
    exclude_ids: &HashSet<String>,
) -> Vec<Candidate> {
    let mut by_key: HashMap<String, Candidate> = HashMap::new();
    let mut seen_ids: HashSet<String> = HashSet::new();
    let mut seen_isrcs: HashMap<String, String> = HashMap::new();

    for cand in candidates {
        if scope == RadioScope::Local && cand.track.source_type != SOURCE_TYPE_LOCAL {
            continue;
        }
        if cand.track.id.is_empty() || exclude_ids.contains(&cand.track.id) {
            continue;
        }
        if !seen_ids.insert(cand.track.id.clone()) {
            continue;
        }

        let isrc = normalize_isrc(&cand.track.isrc);
        let fallback_key = canonical_track_key(&cand.track);
        let key = if !isrc.is_empty() {
            if let Some(existing) = seen_isrcs.get(&isrc) {
                existing.clone()
            } else {
                let k = format!("isrc:{isrc}");
                seen_isrcs.insert(isrc, k.clone());
                k
            }
        } else {
            fallback_key
        };

        match by_key.get_mut(&key) {
            Some(existing) => {
                // Always prefer local track editions; keep the best relevance score across sources.
                let new_is_local = cand.track.source_type == SOURCE_TYPE_LOCAL;
                let old_is_local = existing.track.source_type == SOURCE_TYPE_LOCAL;
                let best_score = existing.score.max(cand.score);
                if (new_is_local && !old_is_local)
                    || (new_is_local == old_is_local && cand.score > existing.score)
                {
                    *existing = Candidate {
                        track: cand.track,
                        score: best_score,
                        source: cand.source,
                    };
                } else {
                    existing.score = best_score;
                }
            }
            None => {
                by_key.insert(key, cand);
            }
        }
    }

    by_key.into_values().collect()
}

fn apply_history_signals(
    base_score: f32,
    track: &Track,
    cooldowns: &RecentCooldowns,
    feedback: &HashMap<String, TrackFeedback>,
    now_unix: i64,
) -> f32 {
    let mut score = base_score;

    // 1. Cross-provider track cooldown (id, ISRC, qobuz_track_id, mb_recording_id)
    let isrc = normalize_isrc(&track.isrc);
    let recently_played = cooldowns.track_ids.contains(&track.id)
        || (!isrc.is_empty() && cooldowns.isrcs.contains(&isrc))
        || (track.qobuz_track_id > 0 && cooldowns.qobuz_track_ids.contains(&track.qobuz_track_id))
        || (!track.mb_recording_id.is_empty()
            && cooldowns.mb_recording_ids.contains(&track.mb_recording_id));
    if recently_played {
        score -= 0.45;
    }

    // 2. Mild artist cooldown if played within the last 2 hours
    let norm_artist = normalize_artist_key(&track.artist);
    if let Some(&last_ts) = cooldowns.artists.get(&norm_artist) {
        let age_sec = (now_unix - last_ts).max(0);
        if age_sec < 2 * 3600 {
            score -= 0.15;
        }
    }

    // 3. Per-track completion / skip feedback
    if let Some(fb) = feedback.get(&track.id) {
        let total = fb.completions + fb.skips;
        if total > 0 {
            let skip_ratio = fb.skips as f32 / total as f32;
            let complete_ratio = fb.completions as f32 / total as f32;
            if fb.skips >= 2 && skip_ratio >= 0.6 {
                score -= 0.35;
            } else if fb.completions >= 2 && complete_ratio >= 0.75 {
                score += 0.10;
            }
        }
    }

    // 4. Gentle preference for local library tracks when scores are otherwise close
    if track.source_type == SOURCE_TYPE_LOCAL {
        score += 0.03;
    }

    score
}

fn space_and_select(
    mut pool: Vec<Candidate>,
    limit: usize,
    seed_track: Option<&Track>,
) -> Vec<Candidate> {
    if limit == 0 || pool.is_empty() {
        return Vec::new();
    }

    let mut chosen: Vec<Candidate> = Vec::with_capacity(limit);
    let mut artist_counts: HashMap<String, usize> = HashMap::new();
    let mut recent_artists: Vec<String> = Vec::new();

    if let Some(seed) = seed_track {
        let key = normalize_artist_key(&seed.artist);
        if !key.is_empty() {
            *artist_counts.entry(key.clone()).or_insert(0) += 1;
            recent_artists.push(key);
        }
    }

    // Pass 1: strict artist window + per-artist cap
    while chosen.len() < limit && !pool.is_empty() {
        let next_idx = pool.iter().position(|c| {
            let ak = normalize_artist_key(&c.track.artist);
            if ak.is_empty() {
                return true;
            }
            let count = artist_counts.get(&ak).copied().unwrap_or(0);
            if count >= MAX_TRACKS_PER_ARTIST {
                return false;
            }
            let window_start = recent_artists.len().saturating_sub(ARTIST_WINDOW - 1);
            !recent_artists[window_start..].iter().any(|r| r == &ak)
        });

        let Some(idx) = next_idx else {
            break;
        };
        let cand = pool.remove(idx);
        let ak = normalize_artist_key(&cand.track.artist);
        if !ak.is_empty() {
            *artist_counts.entry(ak.clone()).or_insert(0) += 1;
            recent_artists.push(ak);
        }
        chosen.push(cand);
    }

    // Pass 2: if library/pool is small, relax constraints so we still fill the station
    for cand in pool {
        if chosen.len() >= limit {
            break;
        }
        chosen.push(cand);
    }

    chosen
}

pub(crate) fn normalize_artist_key(artist: &str) -> String {
    artist.trim().to_lowercase()
}

fn canonical_track_key(track: &Track) -> String {
    format!(
        "nt:{}|{}",
        track.artist.trim().to_lowercase(),
        track.title.trim().to_lowercase()
    )
}

pub fn source_base_weight(source: CandidateSource) -> f32 {
    match source {
        CandidateSource::Sonic => 0.92,
        CandidateSource::QobuzDynamic => 0.90,
        CandidateSource::QobuzSimilar => 0.78,
        CandidateSource::ListenBrainz => 0.80,
        CandidateSource::LocalFallback => 0.45,
    }
}

#[cfg(test)]
mod tests {
    use rand::SeedableRng;
    use rand::rngs::StdRng;

    use crate::library::{SOURCE_LOCAL, SOURCE_QOBUZ, SOURCE_TYPE_REMOTE};

    use super::*;

    fn mk_track(id: &str, artist: &str, title: &str, isrc: &str, local: bool) -> Track {
        Track {
            id: id.into(),
            artist: artist.into(),
            title: title.into(),
            isrc: isrc.into(),
            source: if local { SOURCE_LOCAL } else { SOURCE_QOBUZ }.into(),
            source_type: if local {
                SOURCE_TYPE_LOCAL
            } else {
                SOURCE_TYPE_REMOTE
            }
            .into(),
            provider: if local { SOURCE_LOCAL } else { SOURCE_QOBUZ }.into(),
            available: true,
            ..Default::default()
        }
    }

    #[test]
    fn dedup_prefers_local_isrc_and_respects_local_scope() {
        let mut rng = StdRng::seed_from_u64(42);
        let seed = mk_track("local:seed", "Miles Davis", "So What", "ISRC00000001", true);
        let candidates = vec![
            Candidate {
                track: mk_track(
                    "qobuz:1",
                    "Bill Evans",
                    "Peace Piece",
                    "US0000000002",
                    false,
                ),
                score: 0.95,
                source: CandidateSource::QobuzDynamic,
            },
            Candidate {
                track: mk_track("local:1", "Bill Evans", "Peace Piece", "US0000000002", true),
                score: 0.70,
                source: CandidateSource::Sonic,
            },
            Candidate {
                track: mk_track("qobuz:2", "John Coltrane", "Naima", "US0000000003", false),
                score: 0.85,
                source: CandidateSource::QobuzDynamic,
            },
        ];

        let exclude: HashSet<String> = [seed.id.clone()].into_iter().collect();
        let cooldowns = RecentCooldowns::default();
        let feedback = HashMap::new();

        // Scope::All keeps local:1 over qobuz:1 (ISRC match) and includes qobuz:2
        let res_all = rank_candidates(
            candidates.clone(),
            &RankContext {
                seed_track: Some(&seed),
                opener_rule: OpenerRule::None,
                exclude_track_ids: &exclude,
                scope: RadioScope::All,
                limit: 10,
                cooldowns: &cooldowns,
                feedback: &feedback,
                now_unix: 1_700_000_000,
                jitter: 0.0,
            },
            &mut rng,
        );
        let ids: Vec<&str> = res_all.tracks.iter().map(|t| t.id.as_str()).collect();
        assert_eq!(ids, vec!["local:seed", "local:1", "qobuz:2"]);

        // Scope::Local excludes qobuz:2 entirely
        let res_local = rank_candidates(
            candidates,
            &RankContext {
                seed_track: Some(&seed),
                opener_rule: OpenerRule::None,
                exclude_track_ids: &exclude,
                scope: RadioScope::Local,
                limit: 10,
                cooldowns: &cooldowns,
                feedback: &feedback,
                now_unix: 1_700_000_000,
                jitter: 0.0,
            },
            &mut rng,
        );
        let local_ids: Vec<&str> = res_local.tracks.iter().map(|t| t.id.as_str()).collect();
        assert_eq!(local_ids, vec!["local:seed", "local:1"]);
    }

    #[test]
    fn spacing_cooldowns_and_skip_feedback_reorder_candidates() {
        let mut rng = StdRng::seed_from_u64(7);
        let seed = mk_track("local:seed", "Artist A", "Seed", "", true);
        let candidates = vec![
            // Same artist as seed -> should be pushed back by ARTIST_WINDOW
            Candidate {
                track: mk_track("local:a2", "Artist A", "Track A2", "", true),
                score: 0.99,
                source: CandidateSource::Sonic,
            },
            // Frequently skipped -> penalized
            Candidate {
                track: mk_track("local:skip", "Artist B", "Skip Me", "", true),
                score: 0.95,
                source: CandidateSource::Sonic,
            },
            // Recently played via ISRC cooldown -> penalized
            Candidate {
                track: mk_track("local:cool", "Artist C", "Recent", "US1234567890", true),
                score: 0.94,
                source: CandidateSource::Sonic,
            },
            // Fresh, well-loved track -> should rise to #1 after seed
            Candidate {
                track: mk_track("local:good1", "Artist D", "Good 1", "", true),
                score: 0.85,
                source: CandidateSource::Sonic,
            },
            Candidate {
                track: mk_track("local:good2", "Artist E", "Good 2", "", true),
                score: 0.82,
                source: CandidateSource::Sonic,
            },
            Candidate {
                track: mk_track("local:good3", "Artist F", "Good 3", "", true),
                score: 0.80,
                source: CandidateSource::Sonic,
            },
        ];

        let mut cooldowns = RecentCooldowns::default();
        cooldowns.isrcs.insert("US1234567890".into());

        let mut feedback = HashMap::new();
        feedback.insert(
            "local:skip".into(),
            TrackFeedback {
                completions: 0,
                skips: 3,
                last_played_at: 1_700_000_000,
                last_skipped_at: 1_700_000_000,
            },
        );
        feedback.insert(
            "local:good1".into(),
            TrackFeedback {
                completions: 4,
                skips: 0,
                last_played_at: 1_699_000_000,
                last_skipped_at: 0,
            },
        );

        let exclude: HashSet<String> = [seed.id.clone()].into_iter().collect();
        let res = rank_candidates(
            candidates,
            &RankContext {
                seed_track: Some(&seed),
                opener_rule: OpenerRule::None,
                exclude_track_ids: &exclude,
                scope: RadioScope::All,
                limit: 6,
                cooldowns: &cooldowns,
                feedback: &feedback,
                now_unix: 1_700_000_000,
                jitter: 0.0,
            },
            &mut rng,
        );

        assert_eq!(res.tracks[0].id, "local:seed");
        assert_eq!(res.tracks[1].id, "local:good1");
        assert_eq!(res.tracks[2].id, "local:good2");
        assert_eq!(res.tracks[3].id, "local:good3");
        // Artist A only appears again once outside the 4-track window
        assert_eq!(res.tracks[4].id, "local:a2");
    }

    #[test]
    fn album_and_artist_opener_rules_hoist_matching_track_and_space_next() {
        let mut rng = StdRng::seed_from_u64(11);
        let mut alb_t1 = mk_track("local:alb1", "Miles Davis", "So What", "", true);
        alb_t1.album_id = "alb:kob".into();
        let mut alb_t2 = mk_track("local:alb2", "Miles Davis", "Blue in Green", "", true);
        alb_t2.album_id = "alb:kob".into();

        let candidates = vec![
            Candidate {
                track: mk_track("local:other1", "Bill Evans", "Peace Piece", "", true),
                score: 0.99,
                source: CandidateSource::Sonic,
            },
            Candidate {
                track: mk_track("local:other2", "John Coltrane", "Naima", "", true),
                score: 0.92,
                source: CandidateSource::Sonic,
            },
            Candidate {
                track: alb_t2.clone(),
                score: 0.80,
                source: CandidateSource::Sonic,
            },
            Candidate {
                track: alb_t1.clone(),
                score: 0.70,
                source: CandidateSource::LocalFallback,
            },
        ];

        let exclude = HashSet::new();
        let cooldowns = RecentCooldowns::default();
        let feedback = HashMap::new();

        // Album Radio hoists highest-scoring track from the album (alb_t2) to index 0
        // and spaces index 1 away from Miles Davis.
        let alb_res = rank_candidates(
            candidates.clone(),
            &RankContext {
                seed_track: None,
                opener_rule: OpenerRule::Album {
                    album_id: "alb:kob",
                    seed_tracks: std::slice::from_ref(&alb_t1),
                },
                exclude_track_ids: &exclude,
                scope: RadioScope::All,
                limit: 4,
                cooldowns: &cooldowns,
                feedback: &feedback,
                now_unix: 1_700_000_000,
                jitter: 0.0,
            },
            &mut rng,
        );
        assert_eq!(alb_res.tracks[0].id, "local:alb2");
        assert_eq!(alb_res.tracks[1].id, "local:other1");

        // Artist Radio hoists top recommended track by Miles Davis to index 0.
        let art_res = rank_candidates(
            candidates,
            &RankContext {
                seed_track: None,
                opener_rule: OpenerRule::Artist {
                    artist: "Miles Davis",
                    qobuz_artist_id: 0,
                    seed_tracks: std::slice::from_ref(&alb_t1),
                },
                exclude_track_ids: &exclude,
                scope: RadioScope::All,
                limit: 4,
                cooldowns: &cooldowns,
                feedback: &feedback,
                now_unix: 1_700_000_000,
                jitter: 0.0,
            },
            &mut rng,
        );
        assert_eq!(art_res.tracks[0].id, "local:alb2");
        assert_eq!(art_res.tracks[1].id, "local:other1");
    }
}
