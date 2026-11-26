use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::Deserialize;
use tract_onnx::prelude::*;

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

type RunnableModel = std::sync::Arc<TypedRunnableModel>;

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
    base: RunnableModel,
    style_labels: Vec<String>,
    heads: Vec<(&'static HeadSpec, RunnableModel)>,
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
        let base = load_onnx(&base_path)?;

        let style_labels = std::fs::read_to_string(models_dir.join(BASE_META_FILE))
            .ok()
            .and_then(|s| serde_json::from_str::<EffnetMeta>(&s).ok())
            .map(|m| m.classes)
            .unwrap_or_default();

        let mut heads = Vec::new();
        for spec in HEAD_SPECS {
            let path = models_dir.join(spec.file);
            if path.is_file() {
                match load_onnx(&path) {
                    Ok(m) => heads.push((spec, m)),
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
        let arr =
            tract_ndarray::Array3::from_shape_vec((num_patches, PATCH_FRAMES, N_MELS), patches)
                .map_err(|e| format!("build mel tensor: {e}"))?;
        let input: Tensor = arr.into();
        let outputs = self
            .base
            .run(tvec!(input.into()))
            .map_err(|e| format!("run discogs-effnet: {e}"))?;

        let emb_out = outputs
            .iter()
            .find(|o| o.shape().last() == Some(&EMBEDDING_DIM))
            .ok_or_else(|| "missing 1280-d embedding output".to_string())?;
        let emb_view = emb_out
            .to_plain_array_view::<f32>()
            .map_err(|e| format!("extract embeddings: {e}"))?;

        let mut embedding = vec![0.0f32; EMBEDDING_DIM];
        let inv_n = 1.0 / (num_patches as f32);
        for row in 0..num_patches {
            for c in 0..EMBEDDING_DIM {
                embedding[c] += emb_view[[row, c]] * inv_n;
            }
        }

        let mut tags = extract_top_styles(&outputs, &self.style_labels, num_patches);
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
        let arr =
            tract_ndarray::Array2::from_shape_vec((1, EMBEDDING_DIM), embedding.to_vec()).ok()?;
        let input: Tensor = arr.into();

        let mut scores = TrackSonicHeadsWrite {
            heads_version: HEADS_VERSION,
            ..Default::default()
        };
        let mut any = false;

        for (spec, model) in &self.heads {
            let Ok(outs) = model.run(tvec!(input.clone().into())) else {
                continue;
            };
            let Some(first) = outs.first() else {
                continue;
            };
            let Ok(view) = first.to_plain_array_view::<f32>() else {
                continue;
            };
            let Some(&val) = view.iter().nth(spec.positive_idx) else {
                continue;
            };
            if !val.is_finite() {
                continue;
            }
            let clamped = val.clamp(0.0, 1.0);
            any = true;
            match spec.key {
                "engagement" => scores.engagement = Some(clamped),
                "approachability" => scores.approachability = Some(clamped),
                "instrumental" => scores.instrumental = Some(clamped),
                "danceability" => scores.danceability = Some(clamped),
                "mood_relaxed" => {
                    scores.mood_relaxed = Some(clamped);
                    if clamped >= 0.65 {
                        tags.push(SonicTagEntry {
                            kind: "mood".into(),
                            tag: "relaxed".into(),
                            score: f64::from(clamped),
                            source: "head".into(),
                        });
                    }
                }
                "mood_acoustic" => {
                    scores.mood_acoustic = Some(clamped);
                    if clamped >= 0.65 {
                        tags.push(SonicTagEntry {
                            kind: "mood".into(),
                            tag: "acoustic".into(),
                            score: f64::from(clamped),
                            source: "head".into(),
                        });
                    }
                }
                "mood_aggressive" => {
                    scores.mood_aggressive = Some(clamped);
                    if clamped >= 0.65 {
                        tags.push(SonicTagEntry {
                            kind: "mood".into(),
                            tag: "aggressive".into(),
                            score: f64::from(clamped),
                            source: "head".into(),
                        });
                    }
                }
                "mood_electronic" => {
                    scores.mood_electronic = Some(clamped);
                    if clamped >= 0.65 {
                        tags.push(SonicTagEntry {
                            kind: "mood".into(),
                            tag: "electronic".into(),
                            score: f64::from(clamped),
                            source: "head".into(),
                        });
                    }
                }
                _ => {}
            }
        }

        any.then_some(scores)
    }
}

fn load_onnx(path: &Path) -> Result<RunnableModel, String> {
    tract_onnx::onnx()
        .model_for_path(path)
        .map_err(|e| format!("parse {}: {e}", path.display()))?
        .into_optimized()
        .map_err(|e| format!("optimize {}: {e}", path.display()))?
        .into_runnable()
        .map_err(|e| format!("compile {}: {e}", path.display()))
}

fn extract_top_styles(
    outputs: &TVec<TValue>,
    labels: &[String],
    num_patches: usize,
) -> Vec<SonicTagEntry> {
    if labels.is_empty() {
        return Vec::new();
    }
    let Some(act_out) = outputs
        .iter()
        .find(|o| o.shape().last() == Some(&labels.len()))
    else {
        return Vec::new();
    };
    let Ok(view) = act_out.to_plain_array_view::<f32>() else {
        return Vec::new();
    };
    let inv_n = 1.0 / (num_patches as f32);
    let mut scored: Vec<(usize, f32)> = (0..labels.len())
        .map(|c| {
            let mut sum = 0.0f32;
            for r in 0..num_patches {
                sum += view[[r, c]];
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
