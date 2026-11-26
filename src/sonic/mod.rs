pub mod index;
pub mod mel;
pub mod models;
pub mod quantize;

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

use chrono::Utc;
use futures_util::stream::{self, StreamExt};
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
const STATUS_EMIT_MIN_INTERVAL: Duration = Duration::from_millis(500);

type StatusCallback = Arc<dyn Fn(SonicStatus) + Send + Sync>;

/// Default worker budget for local sonic analysis: half of available logical
/// cores, clamped to `[1, 4]` so analysis scales up on multi-core hosts without
/// starving audio playback or memory on small NAS devices.
/// Overridable via `TACET_SONIC_THREADS` (clamped to `[1, 8]`).
pub(crate) fn sonic_parallelism() -> usize {
    if let Ok(raw) = std::env::var("TACET_SONIC_THREADS")
        && let Ok(n) = raw.trim().parse::<usize>()
    {
        return n.clamp(1, 8);
    }
    let cores = std::thread::available_parallelism()
        .map(std::num::NonZeroUsize::get)
        .unwrap_or(2);
    clamp_half_cores(cores)
}

/// ONNX intra-op thread count kept conservative (`[1, 2]`) so concurrent
/// decode/FFT pipeline workers do not oversubscribe CPU cores.
pub(crate) fn sonic_ort_threads() -> usize {
    sonic_parallelism().clamp(1, 2)
}

fn clamp_half_cores(cores: usize) -> usize {
    (cores / 2).clamp(1, 4)
}

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
    last_emit: Option<Instant>,
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
    on_status: RwLock<Option<StatusCallback>>,
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
            on_status: RwLock::new(None),
        })
    }

    pub fn set_on_status<F>(&self, f: F)
    where
        F: Fn(SonicStatus) + Send + Sync + 'static,
    {
        *self.on_status.write().unwrap_or_else(|e| e.into_inner()) = Some(Arc::new(f));
    }

    fn emit_status(&self) {
        self.meta
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .last_emit = Some(Instant::now());
        let cb = self
            .on_status
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        if let Some(cb) = cb {
            cb(self.status());
        }
    }

    fn emit_status_throttled(&self) {
        let should_emit = {
            let mut meta = self.meta.lock().unwrap_or_else(|e| e.into_inner());
            let due = meta
                .last_emit
                .is_none_or(|t| t.elapsed() >= STATUS_EMIT_MIN_INTERVAL);
            if due {
                meta.last_emit = Some(Instant::now());
            }
            due
        };
        if should_emit {
            let cb = self
                .on_status
                .read()
                .unwrap_or_else(|e| e.into_inner())
                .clone();
            if let Some(cb) = cb {
                cb(self.status());
            }
        }
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
        self.emit_status();
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
        self.emit_status();
        let processed = self.run_pending_loop(ffmpeg.clone()).await;
        self.running.store(false, Ordering::SeqCst);
        self.emit_status();
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
        let concurrency = sonic_parallelism();
        let mut processed = 0usize;
        let mut stream = stream::iter(batch.iter().cloned())
            .map(|track| async move {
                if self.shutdown.is_cancelled() || !self.enabled() {
                    return (track.id, None);
                }
                let res = self.analyze_track(&track, ffmpeg, models).await;
                (track.id, Some(res))
            })
            .buffered(concurrency);

        while let Some((track_id, outcome)) = stream.next().await {
            let Some(res) = outcome else {
                return (processed, true);
            };
            match res {
                Ok(()) => {
                    processed += 1;
                    *consecutive_errors = 0;
                    self.record_success();
                }
                Err(msg) => {
                    tracing::warn!(track_id = %track_id, error = %msg, "sonic analysis failed for track");
                    self.record_error(msg);
                    failed_this_pass.insert(track_id);
                    *consecutive_errors += 1;
                    if *consecutive_errors >= 10 {
                        tracing::warn!("pausing sonic analysis pass after 10 consecutive errors");
                        return (processed, true);
                    }
                }
            }
            if self.shutdown.is_cancelled() || !self.enabled() {
                return (processed, true);
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
            self.emit_status();
            let res = SonicModels::ensure_downloaded(&self.models_dir).await;
            self.downloading.store(false, Ordering::SeqCst);
            self.emit_status();
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
        {
            let mut meta = self.meta.lock().unwrap_or_else(|e| e.into_inner());
            meta.last_run_at = Some(Utc::now().timestamp());
            meta.last_error = None;
        }
        self.emit_status_throttled();
    }

    fn record_error(&self, msg: String) {
        {
            let mut meta = self.meta.lock().unwrap_or_else(|e| e.into_inner());
            meta.last_run_at = Some(Utc::now().timestamp());
            meta.last_error = Some(msg);
        }
        self.emit_status_throttled();
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

#[cfg(test)]
mod tests {
    use super::clamp_half_cores;

    #[test]
    fn half_cores_clamps_to_safe_bounds() {
        assert_eq!(clamp_half_cores(0), 1);
        assert_eq!(clamp_half_cores(1), 1);
        assert_eq!(clamp_half_cores(2), 1);
        assert_eq!(clamp_half_cores(3), 1);
        assert_eq!(clamp_half_cores(4), 2);
        assert_eq!(clamp_half_cores(6), 3);
        assert_eq!(clamp_half_cores(8), 4);
        assert_eq!(clamp_half_cores(16), 4);
        assert_eq!(clamp_half_cores(32), 4);
    }
}
