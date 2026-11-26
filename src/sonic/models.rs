use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use ort::environment::Environment;
use ort::session::Session;
use ort::session::builder::GraphOptimizationLevel;
use ort::value::Tensor;
use serde::Deserialize;

use crate::library::{SonicTagEntry, TrackSonicHeadsWrite};

use super::mel::{N_MELS, PATCH_FRAMES};
use super::quantize::EMBEDDING_DIM;

pub const MODEL_VERSION: &str = "discogs-effnet-bsdynamic-1";
pub const HEADS_VERSION: i32 = 1;

const BASE_MODEL_FILE: &str = "discogs-effnet-bsdynamic-1.onnx";
const BASE_META_FILE: &str = "discogs-effnet-bsdynamic-1.json";
const BASE_MODEL_URL: &str = "https://essentia.upf.edu/models/feature-extractors/discogs-effnet/discogs-effnet-bsdynamic-1.onnx";
const BASE_META_URL: &str = "https://essentia.upf.edu/models/feature-extractors/discogs-effnet/discogs-effnet-bsdynamic-1.json";

#[derive(Debug, Clone, Copy)]
struct HeadSpec {
    key: &'static str,
    file: &'static str,
    url: &'static str,
    /// Index of the positive/target class in the head's output vector (0 for regression heads).
    positive_idx: usize,
}

const HEAD_SPECS: &[HeadSpec] = &[
    HeadSpec {
        key: "engagement",
        file: "engagement_regression-discogs-effnet-1.onnx",
        url: "https://essentia.upf.edu/models/classification-heads/engagement/engagement_regression-discogs-effnet-1.onnx",
        positive_idx: 0,
    },
    HeadSpec {
        key: "approachability",
        file: "approachability_regression-discogs-effnet-1.onnx",
        url: "https://essentia.upf.edu/models/classification-heads/approachability/approachability_regression-discogs-effnet-1.onnx",
        positive_idx: 0,
    },
    HeadSpec {
        key: "instrumental",
        file: "voice_instrumental-discogs-effnet-1.onnx",
        url: "https://essentia.upf.edu/models/classification-heads/voice_instrumental/voice_instrumental-discogs-effnet-1.onnx",
        positive_idx: 0, // [instrumental, voice]
    },
    HeadSpec {
        key: "danceability",
        file: "danceability-discogs-effnet-1.onnx",
        url: "https://essentia.upf.edu/models/classification-heads/danceability/danceability-discogs-effnet-1.onnx",
        positive_idx: 0, // [danceable, not_danceable]
    },
    HeadSpec {
        key: "mood_relaxed",
        file: "mood_relaxed-discogs-effnet-1.onnx",
        url: "https://essentia.upf.edu/models/classification-heads/mood_relaxed/mood_relaxed-discogs-effnet-1.onnx",
        positive_idx: 1, // [non_relaxed, relaxed]
    },
    HeadSpec {
        key: "mood_acoustic",
        file: "mood_acoustic-discogs-effnet-1.onnx",
        url: "https://essentia.upf.edu/models/classification-heads/mood_acoustic/mood_acoustic-discogs-effnet-1.onnx",
        positive_idx: 0, // [acoustic, non_acoustic]
    },
    HeadSpec {
        key: "mood_aggressive",
        file: "mood_aggressive-discogs-effnet-1.onnx",
        url: "https://essentia.upf.edu/models/classification-heads/mood_aggressive/mood_aggressive-discogs-effnet-1.onnx",
        positive_idx: 0, // [aggressive, not_aggressive]
    },
    HeadSpec {
        key: "mood_electronic",
        file: "mood_electronic-discogs-effnet-1.onnx",
        url: "https://essentia.upf.edu/models/classification-heads/mood_electronic/mood_electronic-discogs-effnet-1.onnx",
        positive_idx: 0, // [electronic, non_electronic]
    },
];

#[derive(Deserialize)]
struct EffnetMeta {
    #[serde(default)]
    classes: Vec<String>,
}

pub struct SonicInferenceResult {
    pub embedding: Vec<f32>,
    pub heads: Option<TrackSonicHeadsWrite>,
    pub tags: Vec<SonicTagEntry>,
}

pub struct SonicModels {
    base: Mutex<Session>,
    style_labels: Vec<String>,
    heads: Vec<(&'static HeadSpec, Mutex<Session>)>,
}

impl SonicModels {
    pub fn models_present(models_dir: &Path) -> bool {
        models_dir.join(BASE_MODEL_FILE).is_file()
    }

    /// Downloads the base model, style label metadata, and scalar classification heads
    /// on demand into `models_dir` using atomic temp-file renames.
    pub async fn ensure_downloaded(models_dir: &Path) -> Result<(), String> {
        tokio::fs::create_dir_all(models_dir)
            .await
            .map_err(|e| format!("create models dir: {e}"))?;

        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(180))
            .build()
            .map_err(|e| format!("build download client: {e}"))?;

        download_if_missing(&client, BASE_MODEL_URL, &models_dir.join(BASE_MODEL_FILE)).await?;
        let _ = download_if_missing(&client, BASE_META_URL, &models_dir.join(BASE_META_FILE)).await;

        for spec in HEAD_SPECS {
            let dest = models_dir.join(spec.file);
            if let Err(e) = download_if_missing(&client, spec.url, &dest).await {
                tracing::warn!(head = spec.key, error = %e, "optional sonic head download failed");
            }
        }
        Ok(())
    }

    pub fn load_from_dir(models_dir: &Path) -> Result<Self, String> {
        let base_path = models_dir.join(BASE_MODEL_FILE);
        let base = Mutex::new(load_session(&base_path)?);

        let style_labels = std::fs::read_to_string(models_dir.join(BASE_META_FILE))
            .ok()
            .and_then(|s| serde_json::from_str::<EffnetMeta>(&s).ok())
            .map(|m| m.classes)
            .unwrap_or_default();

        let mut heads = Vec::new();
        for spec in HEAD_SPECS {
            let path = models_dir.join(spec.file);
            if path.is_file() {
                match load_session(&path) {
                    Ok(s) => heads.push((spec, Mutex::new(s))),
                    Err(e) => {
                        tracing::warn!(head = spec.key, error = %e, "skipping unreadable sonic head");
                    }
                }
            }
        }

        Ok(Self {
            base,
            style_labels,
            heads,
        })
    }

    pub fn infer_patches(
        &self,
        patches: Vec<f32>,
        num_patches: usize,
    ) -> Result<SonicInferenceResult, String> {
        if num_patches == 0 || patches.len() != num_patches * PATCH_FRAMES * N_MELS {
            return Err("invalid mel patch buffer".into());
        }
        let input = Tensor::from_array(([num_patches, PATCH_FRAMES, N_MELS], patches))
            .map_err(|e| format!("build mel tensor: {e}"))?;

        let (embedding, mut tags) = {
            let mut session = self.base.lock().unwrap_or_else(|e| e.into_inner());
            let outputs = session
                .run(ort::inputs![input])
                .map_err(|e| format!("run discogs-effnet: {e}"))?;

            let mut pooled_emb: Option<Vec<f32>> = None;
            let mut style_tags = Vec::new();
            let inv_n = 1.0 / (num_patches as f32);

            for (_, val) in outputs.iter() {
                let Ok((shape, data)) = val.try_extract_tensor::<f32>() else {
                    continue;
                };
                let last_dim = shape.last().copied().unwrap_or(0) as usize;
                if last_dim == EMBEDDING_DIM && data.len() == num_patches * EMBEDDING_DIM {
                    let mut emb = vec![0.0f32; EMBEDDING_DIM];
                    for row in 0..num_patches {
                        let base = row * EMBEDDING_DIM;
                        for c in 0..EMBEDDING_DIM {
                            emb[c] += data[base + c] * inv_n;
                        }
                    }
                    pooled_emb = Some(emb);
                } else if !self.style_labels.is_empty()
                    && last_dim == self.style_labels.len()
                    && data.len() == num_patches * last_dim
                {
                    style_tags = extract_top_styles(data, &self.style_labels, num_patches);
                }
            }

            let embedding =
                pooled_emb.ok_or_else(|| "missing 1280-d embedding output".to_string())?;
            (embedding, style_tags)
        };

        let heads = self.run_heads(&embedding, &mut tags);

        Ok(SonicInferenceResult {
            embedding,
            heads,
            tags,
        })
    }

    fn run_heads(
        &self,
        embedding: &[f32],
        tags: &mut Vec<SonicTagEntry>,
    ) -> Option<TrackSonicHeadsWrite> {
        if self.heads.is_empty() {
            return None;
        }
        let mut scores = TrackSonicHeadsWrite {
            heads_version: HEADS_VERSION,
            ..Default::default()
        };
        let mut any = false;

        for (spec, mutex) in &self.heads {
            let Ok(input) = Tensor::from_array(([1usize, EMBEDDING_DIM], embedding.to_vec()))
            else {
                continue;
            };
            let mut session = mutex.lock().unwrap_or_else(|e| e.into_inner());
            let Ok(outputs) = session.run(ort::inputs![input]) else {
                continue;
            };
            let Some((_, first)) = outputs.iter().next() else {
                continue;
            };
            let Ok((_, data)) = first.try_extract_tensor::<f32>() else {
                continue;
            };
            let Some(&val) = data.get(spec.positive_idx) else {
                continue;
            };
            if !val.is_finite() {
                continue;
            }
            let clamped = val.clamp(0.0, 1.0);
            any = true;
            record_head_score(spec.key, clamped, &mut scores, tags);
        }

        any.then_some(scores)
    }
}

fn record_head_score(
    key: &str,
    val: f32,
    scores: &mut TrackSonicHeadsWrite,
    tags: &mut Vec<SonicTagEntry>,
) {
    match key {
        "engagement" => scores.engagement = Some(val),
        "approachability" => scores.approachability = Some(val),
        "instrumental" => scores.instrumental = Some(val),
        "danceability" => scores.danceability = Some(val),
        "mood_relaxed" => {
            scores.mood_relaxed = Some(val);
            maybe_push_mood(tags, "relaxed", val);
        }
        "mood_acoustic" => {
            scores.mood_acoustic = Some(val);
            maybe_push_mood(tags, "acoustic", val);
        }
        "mood_aggressive" => {
            scores.mood_aggressive = Some(val);
            maybe_push_mood(tags, "aggressive", val);
        }
        "mood_electronic" => {
            scores.mood_electronic = Some(val);
            maybe_push_mood(tags, "electronic", val);
        }
        _ => {}
    }
}

fn maybe_push_mood(tags: &mut Vec<SonicTagEntry>, name: &str, val: f32) {
    if val >= 0.65 {
        tags.push(SonicTagEntry {
            kind: "mood".into(),
            tag: name.into(),
            score: f64::from(val),
            source: "head".into(),
        });
    }
}

fn ort_env() -> Result<&'static Environment, String> {
    static ENV: OnceLock<Result<Environment, String>> = OnceLock::new();
    match ENV.get_or_init(|| {
        ort::init()
            .with_name("tacet-sonic")
            .build()
            .map_err(|e| format!("init ort environment: {e}"))
    }) {
        Ok(env) => Ok(env),
        Err(e) => Err(e.clone()),
    }
}

fn load_session(path: &Path) -> Result<Session, String> {
    let env = ort_env()?;
    let threads = super::sonic_ort_threads();
    let mut builder = Session::builder(env)
        .map_err(|e| format!("create ort builder: {e}"))?
        .with_optimization_level(GraphOptimizationLevel::Level3)
        .map_err(|e| format!("set ort opt level: {e}"))?
        .with_intra_threads(threads)
        .map_err(|e| format!("set ort threads: {e}"))?;
    builder
        .commit_from_file(path)
        .map_err(|e| format!("load {}: {e}", path.display()))
}

fn extract_top_styles(data: &[f32], labels: &[String], num_patches: usize) -> Vec<SonicTagEntry> {
    let n_classes = labels.len();
    let inv_n = 1.0 / (num_patches as f32);
    let mut scored: Vec<(usize, f32)> = (0..n_classes)
        .map(|c| {
            let mut sum = 0.0f32;
            for r in 0..num_patches {
                sum += data[r * n_classes + c];
            }
            (c, sum * inv_n)
        })
        .filter(|(_, s)| s.is_finite() && *s >= 0.08)
        .collect();
    scored.sort_by(|a, b| b.1.total_cmp(&a.1));
    scored.truncate(5);

    scored
        .into_iter()
        .map(|(idx, score)| SonicTagEntry {
            kind: "style".into(),
            tag: labels[idx].clone(),
            score: f64::from(score),
            source: "effnet".into(),
        })
        .collect()
}

async fn download_if_missing(
    client: &reqwest::Client,
    url: &str,
    dest: &Path,
) -> Result<(), String> {
    if dest.is_file() {
        return Ok(());
    }
    tracing::info!(url, dest = %dest.display(), "downloading sonic analysis model");
    let resp = client
        .get(url)
        .send()
        .await
        .map_err(|e| format!("GET {url}: {e}"))?;
    if !resp.status().is_success() {
        return Err(format!("GET {url}: HTTP {}", resp.status()));
    }
    let bytes = resp.bytes().await.map_err(|e| format!("read {url}: {e}"))?;

    let tmp: PathBuf = dest.with_extension("tmp");
    tokio::fs::write(&tmp, &bytes)
        .await
        .map_err(|e| format!("write {}: {e}", tmp.display()))?;
    tokio::fs::rename(&tmp, dest)
        .await
        .map_err(|e| format!("rename {}: {e}", dest.display()))?;
    Ok(())
}
