use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::library::Track;

pub const DEFAULT_RADIO_LIMIT: usize = 25;
pub const MAX_RADIO_LIMIT: usize = 50;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RadioScope {
    /// Blend local library and connected streaming services (preferring local editions).
    #[default]
    All,
    /// Strictly restrict candidates to local library tracks.
    Local,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RadioSeed {
    Track {
        id: String,
    },
    Album {
        id: String,
    },
    /// Artist identifier: numeric Qobuz artist ID, `qobuz:<id>`, or artist name.
    Artist {
        id: String,
    },
}

#[derive(Debug, Clone)]
pub struct RadioRequest {
    pub seed: RadioSeed,
    pub limit: usize,
    pub scope: RadioScope,
}

impl RadioRequest {
    pub fn clamped_limit(&self) -> usize {
        if self.limit == 0 {
            DEFAULT_RADIO_LIMIT
        } else {
            self.limit.min(MAX_RADIO_LIMIT)
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CandidateSource {
    Sonic,
    QobuzDynamic,
    QobuzSimilar,
    ListenBrainz,
    LocalFallback,
}

impl CandidateSource {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Sonic => "sonic",
            Self::QobuzDynamic => "qobuz_dynamic",
            Self::QobuzSimilar => "qobuz_similar",
            Self::ListenBrainz => "listenbrainz",
            Self::LocalFallback => "local_fallback",
        }
    }
}

#[derive(Debug, Clone)]
pub struct Candidate {
    pub track: Track,
    /// Base relevance score before history/cooldown/diversity adjustments (typically 0.0..=1.2).
    pub score: f32,
    pub source: CandidateSource,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RadioStation {
    pub seed_kind: String,
    pub seed_label: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub seed_track_id: Option<String>,
    pub tracks: Vec<Track>,
    pub source_counts: BTreeMap<String, usize>,
}

#[derive(Debug, Error)]
pub enum RadioError {
    #[error("seed not found: {0}")]
    SeedNotFound(String),
    #[error("no playable tracks available for radio station")]
    EmptyStation,
    #[error("invalid radio request: {0}")]
    InvalidRequest(String),
}
