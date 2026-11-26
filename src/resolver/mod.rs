pub mod client;
pub mod types;

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use chrono::Utc;
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;

use crate::library::{Album, AlbumEnrichmentUpdate, Catalog, Track, TrackEnrichmentPatch};

pub use client::{
    DEFAULT_RESOLVER_API_KEY, DEFAULT_RESOLVER_URL, ResolverClient, ResolverConfig, ResolverError,
};
pub use types::{
    EnrichmentStatus, ResolveAlbumRequest, ResolveAlbumResponse, ResolveAlbumTrackInput,
    ResolveTrackInput, ResolveTracksRequest, ResolveTracksResponse, ResolvedAlbumInfo,
    ResolvedAlbumTrack, ResolvedLooseTrack,
};

const MAX_ALBUM_TRACKS: usize = 250;
const MAX_LOOSE_TRACKS_BATCH: usize = 50;
const BATCH_QUERY_LIMIT: usize = 50;
const LOOSE_TRACK_MIN_CONFIDENCE: f64 = 0.85;
const LOOSE_TRACK_MAX_DURATION_DELTA_SEC: i32 = 15;
const ERROR_RETRY_SECS: i64 = 3_600;
const UNMATCHED_RETRY_SECS: i64 = 30 * 86_400;
const MAX_RATE_LIMIT_RETRIES: usize = 3;
const PERIODIC_WAKE_SECS: u64 = 15 * 60;

#[derive(Default)]
struct WorkerMeta {
    last_run_at: Option<i64>,
    last_error: Option<String>,
}

pub struct EnrichmentWorker {
    catalog: Arc<Catalog>,
    client: Arc<ResolverClient>,
    cfg: ResolverConfig,
    notify: Arc<Notify>,
    shutdown: CancellationToken,
    running: AtomicBool,
    meta: Mutex<WorkerMeta>,
}

impl EnrichmentWorker {
    pub fn new(
        catalog: Arc<Catalog>,
        cfg: ResolverConfig,
        shutdown: CancellationToken,
    ) -> Arc<Self> {
        let client = Arc::new(ResolverClient::from_config(&cfg));
        Arc::new(Self {
            catalog,
            client,
            cfg,
            notify: Arc::new(Notify::new()),
            shutdown,
            running: AtomicBool::new(false),
            meta: Mutex::new(WorkerMeta::default()),
        })
    }

    pub fn enabled(&self) -> bool {
        self.cfg.enabled
    }

    pub fn trigger(&self) {
        if self.cfg.enabled {
            self.notify.notify_one();
        }
    }

    pub fn status(&self) -> EnrichmentStatus {
        let stats = self.catalog.enrichment_stats().unwrap_or_default();
        let meta = self.meta.lock().unwrap_or_else(|e| e.into_inner());
        EnrichmentStatus {
            enabled: self.cfg.enabled,
            running: self.running.load(Ordering::Relaxed),
            stats,
            last_run_at: meta.last_run_at,
            last_error: meta.last_error.clone(),
        }
    }

    pub fn start(self: &Arc<Self>) {
        if !self.cfg.enabled {
            tracing::info!("tacet-resolver enrichment disabled");
            return;
        }
        let worker = Arc::clone(self);
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_secs(PERIODIC_WAKE_SECS));
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            // Skip immediate first tick so startup scan can trigger us cleanly.
            interval.tick().await;
            loop {
                tokio::select! {
                    () = worker.shutdown.cancelled() => break,
                    () = worker.notify.notified() => {}
                    _ = interval.tick() => {}
                }
                if worker.shutdown.is_cancelled() {
                    break;
                }
                let _ = worker.run_once().await;
            }
        });
    }

    /// Runs a single pass over all currently pending/due albums.
    /// Returns the number of albums processed in this pass.
    pub async fn run_once(&self) -> usize {
        if !self.cfg.enabled {
            return 0;
        }
        if self
            .running
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_err()
        {
            return 0;
        }
        let processed = self.run_pending_loop().await;
        self.running.store(false, Ordering::SeqCst);
        processed
    }

    async fn run_pending_loop(&self) -> usize {
        let mut total_processed = 0usize;
        let mut consecutive_errors = 0usize;

        while !self.shutdown.is_cancelled() {
            let batch = match self
                .catalog
                .list_albums_needing_enrichment(Utc::now().timestamp(), BATCH_QUERY_LIMIT)
            {
                Ok(albums) if !albums.is_empty() => albums,
                Ok(_) => break,
                Err(e) => {
                    self.record_error(format!("query pending albums failed: {e}"));
                    break;
                }
            };

            let (n, paused) = self.process_batch(&batch, &mut consecutive_errors).await;
            total_processed += n;
            if paused {
                break;
            }
        }

        if total_processed > 0 {
            tracing::info!(
                albums = total_processed,
                "tacet-resolver enrichment pass complete"
            );
        }
        total_processed
    }

    async fn process_batch(
        &self,
        batch: &[Album],
        consecutive_errors: &mut usize,
    ) -> (usize, bool) {
        let mut processed = 0usize;
        for album in batch {
            if self.shutdown.is_cancelled() {
                return (processed, true);
            }
            let retryable_err = self.process_single_album(album).await;
            processed += 1;
            if retryable_err {
                *consecutive_errors += 1;
                if *consecutive_errors >= 5 {
                    tracing::warn!("pausing enrichment pass after 5 consecutive resolver errors");
                    return (processed, true);
                }
            } else {
                *consecutive_errors = 0;
            }
            if !self.sleep_interruptible(self.cfg.min_request_gap).await {
                return (processed, true);
            }
        }
        (processed, false)
    }

    async fn process_single_album(&self, album: &Album) -> bool {
        match self.enrich_album(album).await {
            Ok(()) => {
                self.record_success();
                false
            }
            Err(err) => {
                let msg = err.to_string();
                self.record_error(msg.clone());
                let retry_after = Utc::now().timestamp() + ERROR_RETRY_SECS;
                let _ = self.catalog.apply_album_enrichment(
                    &album.id,
                    &AlbumEnrichmentUpdate {
                        status: "error".into(),
                        retry_after,
                        ..Default::default()
                    },
                );
                err.is_retryable()
            }
        }
    }

    async fn enrich_album(&self, album: &Album) -> Result<(), ResolverError> {
        let tracks = self
            .catalog
            .album(&album.id)
            .map(|a| a.tracks)
            .unwrap_or_default();
        if tracks.is_empty() {
            return self
                .catalog
                .apply_album_enrichment(
                    &album.id,
                    &AlbumEnrichmentUpdate {
                        status: "unmatched".into(),
                        retry_after: Utc::now().timestamp() + UNMATCHED_RETRY_SECS,
                        ..Default::default()
                    },
                )
                .map_err(|e| ResolverError::Transport(format!("save empty album state: {e}")));
        }

        let album_slice = &tracks[..tracks.len().min(MAX_ALBUM_TRACKS)];
        let req = build_album_request(album, album_slice);
        let resp = self.call_resolve_album_with_retry(&req).await?;

        let mut patches = build_album_track_patches(album_slice, &resp);

        // Collect stragglers (unmatched indices from the album call + any overflow tracks > 250)
        // and attempt loose-track resolution with strict confidence + duration guards.
        let straggler_indices = collect_straggler_indices(tracks.len(), &resp);
        if !straggler_indices.is_empty() {
            let extra = self.resolve_stragglers(&tracks, &straggler_indices).await?;
            merge_track_patches(&mut patches, extra);
        }

        let update = build_enrichment_update(&resp, tracks.len(), patches);
        self.catalog
            .apply_album_enrichment(&album.id, &update)
            .map_err(|e| ResolverError::Transport(format!("save enrichment: {e}")))
    }

    async fn call_resolve_album_with_retry(
        &self,
        req: &ResolveAlbumRequest,
    ) -> Result<ResolveAlbumResponse, ResolverError> {
        for attempt in 0..=MAX_RATE_LIMIT_RETRIES {
            match self.client.resolve_album(req).await {
                Ok(resp) => return Ok(resp),
                Err(ResolverError::RateLimited { retry_after })
                    if attempt < MAX_RATE_LIMIT_RETRIES =>
                {
                    tracing::debug!(?retry_after, attempt, "resolver 429 on /v1/resolve/album");
                    if !self.sleep_interruptible(retry_after).await {
                        return Err(ResolverError::Transport("shutdown during backoff".into()));
                    }
                }
                Err(e) => return Err(e),
            }
        }
        Err(ResolverError::Transport("exhausted retries".into()))
    }

    async fn call_resolve_tracks_with_retry(
        &self,
        req: &ResolveTracksRequest,
    ) -> Result<ResolveTracksResponse, ResolverError> {
        for attempt in 0..=MAX_RATE_LIMIT_RETRIES {
            match self.client.resolve_tracks(req).await {
                Ok(resp) => return Ok(resp),
                Err(ResolverError::RateLimited { retry_after })
                    if attempt < MAX_RATE_LIMIT_RETRIES =>
                {
                    tracing::debug!(?retry_after, attempt, "resolver 429 on /v1/resolve/tracks");
                    if !self.sleep_interruptible(retry_after).await {
                        return Err(ResolverError::Transport("shutdown during backoff".into()));
                    }
                }
                Err(e) => return Err(e),
            }
        }
        Err(ResolverError::Transport("exhausted retries".into()))
    }

    async fn resolve_stragglers(
        &self,
        tracks: &[Track],
        indices: &[usize],
    ) -> Result<Vec<TrackEnrichmentPatch>, ResolverError> {
        let mut out = Vec::new();
        for chunk in indices.chunks(MAX_LOOSE_TRACKS_BATCH) {
            if self.shutdown.is_cancelled() {
                break;
            }
            let inputs: Vec<ResolveTrackInput> = chunk
                .iter()
                .filter_map(|&idx| tracks.get(idx).map(|t| build_loose_track_input(idx, t)))
                .collect();
            if inputs.is_empty() {
                continue;
            }
            let req = ResolveTracksRequest { tracks: inputs };
            let resp = self.call_resolve_tracks_with_retry(&req).await?;
            for rt in resp.tracks {
                let Some(orig) = tracks.get(rt.index) else {
                    continue;
                };
                if accept_loose_track_match(orig, &rt) {
                    out.push(TrackEnrichmentPatch {
                        track_id: orig.id.clone(),
                        isrc: rt.isrc,
                        mb_recording_id: rt.recording_mbid,
                        mb_artist_id: rt.artist_mbid,
                        genres: rt.genres,
                    });
                }
            }
        }
        Ok(out)
    }

    async fn sleep_interruptible(&self, dur: Duration) -> bool {
        if dur.is_zero() {
            return !self.shutdown.is_cancelled();
        }
        tokio::select! {
            () = self.shutdown.cancelled() => false,
            () = tokio::time::sleep(dur) => true,
        }
    }

    fn record_success(&self) {
        let mut meta = self.meta.lock().unwrap_or_else(|e| e.into_inner());
        meta.last_run_at = Some(Utc::now().timestamp());
        meta.last_error = None;
    }

    fn record_error(&self, msg: String) {
        let mut meta = self.meta.lock().unwrap_or_else(|e| e.into_inner());
        meta.last_run_at = Some(Utc::now().timestamp());
        meta.last_error = Some(msg);
    }
}

fn build_album_request(album: &Album, tracks: &[Track]) -> ResolveAlbumRequest {
    let year = (album.year > 0).then_some(album.year);
    let track_inputs = tracks
        .iter()
        .enumerate()
        .map(|(index, t)| ResolveAlbumTrackInput {
            index,
            disc: (t.disc_number > 0).then_some(t.disc_number),
            track: (t.track_number > 0).then_some(t.track_number),
            title: t.title.clone(),
            duration_sec: (t.duration_sec > 0).then_some(t.duration_sec),
            isrc: t.isrc.clone(),
            fingerprint: String::new(),
        })
        .collect();
    ResolveAlbumRequest {
        artist: album.artist.clone(),
        album: album.title.clone(),
        year,
        upc: album.upc.clone(),
        tracks: track_inputs,
    }
}

fn build_loose_track_input(index: usize, t: &Track) -> ResolveTrackInput {
    ResolveTrackInput {
        index,
        isrc: t.isrc.clone(),
        artist: if t.artist.is_empty() {
            t.album_artist.clone()
        } else {
            t.artist.clone()
        },
        title: t.title.clone(),
        duration_sec: (t.duration_sec > 0).then_some(t.duration_sec),
        fingerprint: String::new(),
    }
}

fn build_album_track_patches(
    album_slice: &[Track],
    resp: &ResolveAlbumResponse,
) -> HashMap<String, TrackEnrichmentPatch> {
    let album_artist_mbid = resp
        .album
        .as_ref()
        .map(|a| a.artist_mbid.as_str())
        .unwrap_or("");
    let mut map = HashMap::new();
    for rt in &resp.tracks {
        if !rt.matched {
            continue;
        }
        let Some(orig) = album_slice.get(rt.index) else {
            continue;
        };
        let artist_mbid = if rt.artist_mbid.is_empty() {
            album_artist_mbid.to_string()
        } else {
            rt.artist_mbid.clone()
        };
        map.insert(
            orig.id.clone(),
            TrackEnrichmentPatch {
                track_id: orig.id.clone(),
                isrc: rt.isrc.clone(),
                mb_recording_id: rt.recording_mbid.clone(),
                mb_artist_id: artist_mbid,
                genres: rt.genres.clone(),
            },
        );
    }
    map
}

fn collect_straggler_indices(total_tracks: usize, resp: &ResolveAlbumResponse) -> Vec<usize> {
    let mut indices: Vec<usize> = resp
        .unmatched_indices
        .iter()
        .copied()
        .filter(|&i| i < total_tracks)
        .collect();
    if total_tracks > MAX_ALBUM_TRACKS {
        indices.extend(MAX_ALBUM_TRACKS..total_tracks);
    }
    indices.sort_unstable();
    indices.dedup();
    indices
}

fn accept_loose_track_match(orig: &Track, rt: &ResolvedLooseTrack) -> bool {
    if !rt.matched || rt.confidence < LOOSE_TRACK_MIN_CONFIDENCE {
        return false;
    }
    if orig.duration_sec > 0
        && let Some(resolved_dur) = rt.duration_sec
        && resolved_dur > 0
        && (orig.duration_sec - resolved_dur).abs() > LOOSE_TRACK_MAX_DURATION_DELTA_SEC
    {
        return false;
    }
    true
}

fn merge_track_patches(
    base: &mut HashMap<String, TrackEnrichmentPatch>,
    extra: Vec<TrackEnrichmentPatch>,
) {
    for p in extra {
        base.entry(p.track_id.clone()).or_insert(p);
    }
}

fn build_enrichment_update(
    resp: &ResolveAlbumResponse,
    total_tracks: usize,
    patches: HashMap<String, TrackEnrichmentPatch>,
) -> AlbumEnrichmentUpdate {
    let matched_count = patches.len();
    let status = if resp.matched && matched_count >= total_tracks {
        "matched"
    } else if resp.matched || matched_count > 0 {
        "partial"
    } else {
        "unmatched"
    };
    let retry_after = if status == "unmatched" {
        Utc::now().timestamp() + UNMATCHED_RETRY_SECS
    } else {
        0
    };
    let (upc, mb_release_id, mb_release_group_id, mb_artist_id, album_genres) =
        if let Some(alb) = &resp.album {
            (
                alb.upc.clone(),
                alb.release_mbid.clone(),
                alb.release_group_mbid.clone(),
                alb.artist_mbid.clone(),
                alb.genres.clone(),
            )
        } else {
            (
                String::new(),
                String::new(),
                String::new(),
                String::new(),
                Vec::new(),
            )
        };
    AlbumEnrichmentUpdate {
        status: status.into(),
        confidence: resp.confidence,
        matched_by: resp.matched_by.join(","),
        retry_after,
        upc,
        mb_release_id,
        mb_release_group_id,
        mb_artist_id,
        album_genres,
        track_patches: patches.into_values().collect(),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use axum::extract::State;
    use axum::http::{HeaderMap, StatusCode};
    use axum::response::{IntoResponse, Response};
    use axum::routing::post;
    use axum::{Json, Router};

    use super::*;
    use crate::library::{SOURCE_LOCAL, SOURCE_TYPE_LOCAL};

    #[derive(Clone, Default)]
    struct MockResolverState {
        album_calls: Arc<AtomicUsize>,
        track_calls: Arc<AtomicUsize>,
        fail_first_with_429: bool,
    }

    async fn mock_resolve_album(
        State(st): State<MockResolverState>,
        headers: HeaderMap,
        Json(req): Json<serde_json::Value>,
    ) -> Response {
        if headers.get("x-api-key").and_then(|v| v.to_str().ok()) != Some("test-key") {
            return (StatusCode::UNAUTHORIZED, "bad key").into_response();
        }
        let n = st.album_calls.fetch_add(1, Ordering::SeqCst);
        if st.fail_first_with_429 && n == 0 {
            return (
                StatusCode::TOO_MANY_REQUESTS,
                [(reqwest::header::RETRY_AFTER, "1")],
                "slow down",
            )
                .into_response();
        }
        let title = req["album"].as_str().unwrap_or("");
        if title == "19" {
            Json(serde_json::json!({
                "matched": true,
                "confidence": 1.0,
                "matchedBy": ["metadata"],
                "album": {
                    "releaseMbid": "rel-19",
                    "releaseGroupMbid": "rg-19",
                    "artistMbid": "art-adele",
                    "title": "19",
                    "artist": "Adele",
                    "year": 2008,
                    "upc": "634904031329",
                    "genres": ["Pop", "Blue-Eyed Soul"]
                },
                "tracks": [
                    {
                        "index": 0,
                        "matched": true,
                        "confidence": 1.0,
                        "matchedBy": ["metadata"],
                        "recordingMbid": "rec-daydreamer",
                        "isrc": "GB-BKS-07-00581",
                        "title": "Daydreamer"
                    }
                ],
                "unmatchedIndices": [1]
            }))
            .into_response()
        } else {
            Json(serde_json::json!({
                "matched": false,
                "confidence": 0.0,
                "matchedBy": [],
                "tracks": [],
                "unmatchedIndices": [0]
            }))
            .into_response()
        }
    }

    async fn mock_resolve_tracks(
        State(st): State<MockResolverState>,
        Json(_req): Json<serde_json::Value>,
    ) -> Response {
        st.track_calls.fetch_add(1, Ordering::SeqCst);
        Json(serde_json::json!({
            "tracks": [
                {
                    "index": 1,
                    "matched": true,
                    "confidence": 0.95,
                    "matchedBy": ["metadata"],
                    "recordingMbid": "rec-chasing",
                    "artistMbid": "art-adele",
                    "isrc": "ATW050700190",
                    "durationSec": 223,
                    "genres": ["Soul"]
                }
            ]
        }))
        .into_response()
    }

    async fn spawn_mock_resolver(state: MockResolverState) -> String {
        let app = Router::new()
            .route("/v1/resolve/album", post(mock_resolve_album))
            .route("/v1/resolve/tracks", post(mock_resolve_tracks))
            .with_state(state);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        format!("http://{addr}")
    }

    #[tokio::test]
    async fn enrichment_worker_resolves_album_stragglers_and_handles_429() {
        let mock_state = MockResolverState {
            fail_first_with_429: true,
            ..Default::default()
        };
        let base_url = spawn_mock_resolver(mock_state.clone()).await;

        let catalog = Catalog::new_in_memory(vec![]);
        let alb = Album {
            id: "local:a19".into(),
            source: SOURCE_LOCAL.into(),
            source_type: SOURCE_TYPE_LOCAL.into(),
            provider: SOURCE_LOCAL.into(),
            title: "19".into(),
            artist: "Adele".into(),
            year: 2008,
            available: true,
            ..Default::default()
        };
        let tracks = vec![
            Track {
                id: "local:t1".into(),
                album_id: "local:a19".into(),
                source: SOURCE_LOCAL.into(),
                source_type: SOURCE_TYPE_LOCAL.into(),
                provider: SOURCE_LOCAL.into(),
                title: "Daydreamer".into(),
                artist: "Adele".into(),
                album: "19".into(),
                track_number: 1,
                duration_sec: 220,
                available: true,
                ..Default::default()
            },
            Track {
                id: "local:t2".into(),
                album_id: "local:a19".into(),
                source: SOURCE_LOCAL.into(),
                source_type: SOURCE_TYPE_LOCAL.into(),
                provider: SOURCE_LOCAL.into(),
                title: "Chasing Pavements".into(),
                artist: "Adele".into(),
                album: "19".into(),
                track_number: 2,
                duration_sec: 220,
                available: true,
                ..Default::default()
            },
        ];
        catalog
            .store()
            .upsert_album_with_tracks(&alb, &tracks)
            .unwrap();

        let cfg = ResolverConfig {
            base_url,
            api_key: "test-key".into(),
            enabled: true,
            request_timeout: Duration::from_secs(5),
            min_request_gap: Duration::ZERO,
        };
        let worker = EnrichmentWorker::new(Arc::clone(&catalog), cfg, CancellationToken::new());
        let processed = worker.run_once().await;
        assert_eq!(processed, 1);
        // 1 initial 429 + 1 retry success
        assert_eq!(mock_state.album_calls.load(Ordering::SeqCst), 2);
        assert_eq!(mock_state.track_calls.load(Ordering::SeqCst), 1);

        let updated_alb = catalog.album("local:a19").unwrap();
        assert_eq!(updated_alb.upc, "634904031329");
        assert_eq!(updated_alb.mb_release_id, "rel-19");
        assert_eq!(updated_alb.mb_release_group_id, "rg-19");
        assert_eq!(updated_alb.mb_artist_id, "art-adele");

        let t1 = catalog.track("local:t1").unwrap();
        assert_eq!(t1.isrc, "GBBKS0700581");
        assert_eq!(t1.mb_recording_id, "rec-daydreamer");
        assert_eq!(t1.mb_artist_id, "art-adele");

        let t2 = catalog.track("local:t2").unwrap();
        assert_eq!(t2.isrc, "ATW050700190");
        assert_eq!(t2.mb_recording_id, "rec-chasing");

        let tags = catalog.list_entity_tags("album", "local:a19").unwrap();
        assert_eq!(tags.len(), 2);
        assert_eq!(tags[0].tag, "blue-eyed soul");
        assert_eq!(tags[1].tag, "pop");

        let status = worker.status();
        assert!(status.enabled);
        assert!(!status.running);
        assert_eq!(status.stats.matched_albums, 1);
        assert_eq!(status.stats.pending_albums, 0);

        // Second run_once finds no pending albums.
        assert_eq!(worker.run_once().await, 0);
    }

    #[test]
    fn loose_track_match_rejects_low_confidence_or_duration_mismatch() {
        let orig = Track {
            duration_sec: 159,
            ..Default::default()
        };
        let live_version = ResolvedLooseTrack {
            matched: true,
            confidence: 0.90,
            duration_sec: Some(678),
            ..Default::default()
        };
        assert!(!accept_loose_track_match(&orig, &live_version));

        let low_conf = ResolvedLooseTrack {
            matched: true,
            confidence: 0.80,
            duration_sec: Some(160),
            ..Default::default()
        };
        assert!(!accept_loose_track_match(&orig, &low_conf));

        let good = ResolvedLooseTrack {
            matched: true,
            confidence: 0.90,
            duration_sec: Some(165),
            ..Default::default()
        };
        assert!(accept_loose_track_match(&orig, &good));
    }
}
