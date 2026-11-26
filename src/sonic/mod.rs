pub mod index;
pub mod mel;
pub mod models;
pub mod quantize;

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use chrono::Utc;
use serde::Serialize;
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;

use crate::library::{Catalog, PREF_SONIC_ENABLED, SonicStats, Track, TrackSonicWrite};
use crate::media::detect_transcoder;

pub use index::SonicIndex;
pub use models::{HEADS_VERSION, MODEL_VERSION, SonicModels};
pub use quantize::{EMBEDDING_DIM, Int8UnitVec, QuantizedEmbedding, quantize_uint8};

const BATCH_LIMIT: usize = 32;
const PERIODIC_WAKE_SECS: u64 = 15 * 60;

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SonicStatus {
    pub enabled: bool,
    pub models_ready: bool,
    pub downloading: bool,
    pub running: bool,
    pub ffmpeg_available: bool,
    pub indexed_tracks: usize,
    #[serde(flatten)]
    pub stats: SonicStats,
    pub last_run_at: Option<i64>,
    pub last_error: Option<String>,
}

#[derive(Default)]
struct WorkerMeta {
    last_run_at: Option<i64>,
    last_error: Option<String>,
}

pub struct SonicWorker {
    catalog: Arc<Catalog>,
    index: Arc<SonicIndex>,
    models_dir: PathBuf,
    ffmpeg_path: Option<PathBuf>,
    models: RwLock<Option<Arc<SonicModels>>>,
    enabled: AtomicBool,
    downloading: AtomicBool,
    running: AtomicBool,
    notify: Arc<Notify>,
    shutdown: CancellationToken,
    meta: Mutex<WorkerMeta>,
}

impl SonicWorker {
    pub fn new(
        catalog: Arc<Catalog>,
        models_dir: PathBuf,
        shutdown: CancellationToken,
    ) -> Arc<Self> {
        let enabled = resolve_sonic_enabled(&catalog);
        let ffmpeg_path = detect_transcoder().ffmpeg_path.clone();
        let index = Arc::new(SonicIndex::new());

        if let Ok(rows) = catalog.load_all_track_sonic(MODEL_VERSION) {
            let loaded = index.load_from_blobs(rows);
            if loaded > 0 {
                tracing::info!(tracks = loaded, "loaded sonic embeddings into RAM index");
            }
        }

        Arc::new(Self {
            catalog,
            index,
            models_dir,
            ffmpeg_path,
            models: RwLock::new(None),
            enabled: AtomicBool::new(enabled),
            downloading: AtomicBool::new(false),
            running: AtomicBool::new(false),
            notify: Arc::new(Notify::new()),
            shutdown,
            meta: Mutex::new(WorkerMeta::default()),
        })
    }

    pub fn index(&self) -> &Arc<SonicIndex> {
        &self.index
    }

    pub fn enabled(&self) -> bool {
        self.enabled.load(Ordering::Relaxed)
    }

    pub fn set_enabled(&self, enabled: bool) {
        self.enabled.store(enabled, Ordering::SeqCst);
        let _ = self
            .catalog
            .set_pref(PREF_SONIC_ENABLED, if enabled { "true" } else { "false" });
        if enabled {
            self.trigger();
        }
    }

    pub fn trigger(&self) {
        if self.enabled() {
            self.notify.notify_one();
        }
    }

    pub fn status(&self) -> SonicStatus {
        let stats = self.catalog.sonic_stats(MODEL_VERSION).unwrap_or_default();
        let meta = self.meta.lock().unwrap_or_else(|e| e.into_inner());
        SonicStatus {
            enabled: self.enabled(),
            models_ready: SonicModels::models_present(&self.models_dir),
            downloading: self.downloading.load(Ordering::Relaxed),
            running: self.running.load(Ordering::Relaxed),
            ffmpeg_available: self.ffmpeg_path.is_some(),
            indexed_tracks: self.index.len(),
            stats,
            last_run_at: meta.last_run_at,
            last_error: meta.last_error.clone(),
        }
    }

    pub fn start(self: &Arc<Self>) {
        let worker = Arc::clone(self);
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_secs(PERIODIC_WAKE_SECS));
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
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
                if worker.enabled() {
                    let _ = worker.run_once().await;
                }
            }
        });
    }

    pub async fn run_once(&self) -> usize {
        if !self.enabled() {
            return 0;
        }
        let Some(ref ffmpeg) = self.ffmpeg_path else {
            self.record_error(
                "ffmpeg not found on PATH; cannot decode audio for sonic analysis".into(),
            );
            return 0;
        };
        if self
            .running
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_err()
        {
            return 0;
        }
        let processed = self.run_pending_loop(ffmpeg.clone()).await;
        self.running.store(false, Ordering::SeqCst);
        processed
    }

    async fn run_pending_loop(&self, ffmpeg: PathBuf) -> usize {
        let models = match self.ensure_models_loaded().await {
            Ok(m) => m,
            Err(e) => {
                self.record_error(e);
                return 0;
            }
        };

        let mut total = 0usize;
        let mut consecutive_errors = 0usize;
        let mut failed_this_pass = HashSet::new();

        while !self.shutdown.is_cancelled() && self.enabled() {
            let batch = match self
                .catalog
                .list_tracks_needing_sonic(MODEL_VERSION, BATCH_LIMIT + failed_this_pass.len())
            {
                Ok(t) => t,
                Err(e) => {
                    self.record_error(format!("query pending sonic tracks: {e}"));
                    break;
                }
            };
            let runnable: Vec<Track> = batch
                .into_iter()
                .filter(|t| !failed_this_pass.contains(&t.id))
                .take(BATCH_LIMIT)
                .collect();
            if runnable.is_empty() {
                break;
            }

            let (n, paused) = self
                .process_batch(
                    &runnable,
                    &ffmpeg,
                    &models,
                    &mut consecutive_errors,
                    &mut failed_this_pass,
                )
                .await;
            total += n;
            if paused {
                break;
            }
        }

        if total > 0 {
            tracing::info!(tracks = total, "local sonic analysis pass complete");
        }
        total
    }

    async fn process_batch(
        &self,
        batch: &[Track],
        ffmpeg: &Path,
        models: &Arc<SonicModels>,
        consecutive_errors: &mut usize,
        failed_this_pass: &mut HashSet<String>,
    ) -> (usize, bool) {
        let mut processed = 0usize;
        for track in batch {
            if self.shutdown.is_cancelled() || !self.enabled() {
                return (processed, true);
            }
            match self.analyze_track(track, ffmpeg, models).await {
                Ok(()) => {
                    processed += 1;
                    *consecutive_errors = 0;
                    self.record_success();
                }
                Err(msg) => {
                    tracing::warn!(track_id = %track.id, error = %msg, "sonic analysis failed for track");
                    self.record_error(msg);
                    failed_this_pass.insert(track.id.clone());
                    *consecutive_errors += 1;
                    if *consecutive_errors >= 10 {
                        tracing::warn!("pausing sonic analysis pass after 10 consecutive errors");
                        return (processed, true);
                    }
                }
            }
        }
        (processed, false)
    }

    async fn analyze_track(
        &self,
        track: &Track,
        ffmpeg: &Path,
        models: &Arc<SonicModels>,
    ) -> Result<(), String> {
        let ffmpeg_path = ffmpeg.to_path_buf();
        let file_path = PathBuf::from(&track.file_path);
        let models_arc = Arc::clone(models);

        let inferred = tokio::task::spawn_blocking(move || {
            let samples = mel::decode_audio_16k_mono(&ffmpeg_path, &file_path)?;
            let (patches, num_patches) = mel::compute_mel_patches(&samples);
            if num_patches == 0 {
                return Err("decoded audio was empty".into());
            }
            models_arc.infer_patches(patches, num_patches)
        })
        .await
        .map_err(|e| format!("sonic worker task panicked: {e}"))??;

        let quant = quantize_uint8(&inferred.embedding);
        let unit = Int8UnitVec::from_raw_f32(&inferred.embedding)
            .ok_or_else(|| "embedding had zero norm".to_string())?;

        let write = TrackSonicWrite {
            track_id: track.id.clone(),
            model_version: MODEL_VERSION.to_string(),
            file_mtime_ns: track.file_mtime_ns,
            emb_uint8: quant.bytes,
            emb_min: quant.min,
            emb_scale: quant.scale,
            heads: inferred.heads,
            tags: inferred.tags,
        };
        self.catalog
            .save_track_sonic(&write)
            .map_err(|e| format!("save track_sonic: {e}"))?;
        self.index.upsert_track(track.id.clone(), unit);
        Ok(())
    }

    async fn ensure_models_loaded(&self) -> Result<Arc<SonicModels>, String> {
        if let Some(m) = self
            .models
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
        {
            return Ok(m);
        }
        if !SonicModels::models_present(&self.models_dir) {
            self.downloading.store(true, Ordering::SeqCst);
            let res = SonicModels::ensure_downloaded(&self.models_dir).await;
            self.downloading.store(false, Ordering::SeqCst);
            res?;
        }
        let dir = self.models_dir.clone();
        let loaded = tokio::task::spawn_blocking(move || SonicModels::load_from_dir(&dir))
            .await
            .map_err(|e| format!("load models task failed: {e}"))??;
        let arc = Arc::new(loaded);
        *self.models.write().unwrap_or_else(|e| e.into_inner()) = Some(Arc::clone(&arc));
        Ok(arc)
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

fn resolve_sonic_enabled(catalog: &Catalog) -> bool {
    if let Ok(v) = std::env::var("TACET_SONIC_ENABLED") {
        let t = v.trim();
        return t == "1" || t.eq_ignore_ascii_case("true");
    }
    catalog
        .get_pref(PREF_SONIC_ENABLED)
        .ok()
        .flatten()
        .is_some_and(|v| {
            let t = v.trim();
            t == "1" || t.eq_ignore_ascii_case("true")
        })
}
