use std::collections::HashSet;
use std::sync::Arc;

use crate::library::{Catalog, Track, TrackFilter};
use crate::qobuz::Client as QobuzClient;

use super::types::{RadioError, RadioScope, RadioSeed};

#[derive(Debug, Clone)]
pub struct ResolvedSeed {
    pub kind: &'static str,
    pub label: String,
    /// Pinned at index 0 for Track Radio.
    pub seed_track: Option<Track>,
    /// Representative tracks for the seed (1 for track, album tracks for album, artist tracks for artist).
    pub seed_tracks: Vec<Track>,
    /// Track IDs excluded from the candidate tail (e.g. the seed track itself).
    pub exclude_track_ids: HashSet<String>,
    pub artist_name: String,
    pub qobuz_album_id: String,
    pub qobuz_artist_id: i64,
    pub mb_artist_id: String,
    pub genre: String,
    pub year: i32,
}

pub async fn resolve_seed(
    seed: &RadioSeed,
    scope: RadioScope,
    catalog: &Arc<Catalog>,
    qobuz: Option<&Arc<QobuzClient>>,
) -> Result<ResolvedSeed, RadioError> {
    let qobuz_live = match (scope, qobuz) {
        (RadioScope::All, Some(q)) if q.is_connected() => Some(q),
        _ => None,
    };

    match seed {
        RadioSeed::Track { id } => resolve_track_seed(id, catalog, qobuz_live).await,
        RadioSeed::Album { id } => resolve_album_seed(id, catalog, qobuz_live).await,
        RadioSeed::Artist { id } => resolve_artist_seed(id, catalog, qobuz_live).await,
    }
}

async fn resolve_track_seed(
    id: &str,
    catalog: &Arc<Catalog>,
    qobuz: Option<&Arc<QobuzClient>>,
) -> Result<ResolvedSeed, RadioError> {
    let clean = id.trim();
    if clean.is_empty() {
        return Err(RadioError::InvalidRequest("track id is required".into()));
    }

    let mut track = catalog.track(clean);
    if track.is_none()
        && let Some(q) = qobuz
        && let Ok(qid) = parse_qobuz_numeric_id(clean)
        && let Ok(t) = q.get_track(qid).await
    {
        track = Some(t);
    }

    let track = track.ok_or_else(|| RadioError::SeedNotFound(clean.into()))?;
    let album = if !track.album_id.is_empty() {
        catalog.album(&track.album_id)
    } else {
        None
    };

    let mb_artist_id = first_non_empty(&[
        track.mb_artist_id.as_str(),
        album.as_ref().map_or("", |a| a.mb_artist_id.as_str()),
    ]);
    let qobuz_artist_id = if track.qobuz_artist_id > 0 {
        track.qobuz_artist_id
    } else {
        album.as_ref().map_or(0, |a| a.qobuz_artist_id)
    };
    let genre = first_non_empty(&[
        track.genre.as_str(),
        album.as_ref().map_or("", |a| a.genre.as_str()),
    ]);
    let year = if track.year > 0 {
        track.year
    } else {
        album.as_ref().map_or(0, |a| a.year)
    };
    let qobuz_album_id = track
        .album_id
        .strip_prefix("qobuz:")
        .unwrap_or("")
        .to_string();

    let mut exclude = HashSet::new();
    exclude.insert(track.id.clone());

    let label = track.title.clone();

    Ok(ResolvedSeed {
        kind: "track",
        label,
        artist_name: track.artist.clone(),
        seed_tracks: vec![track.clone()],
        seed_track: Some(track),
        exclude_track_ids: exclude,
        qobuz_album_id,
        qobuz_artist_id,
        mb_artist_id,
        genre,
        year,
    })
}

async fn resolve_album_seed(
    id: &str,
    catalog: &Arc<Catalog>,
    qobuz: Option<&Arc<QobuzClient>>,
) -> Result<ResolvedSeed, RadioError> {
    let clean = id.trim();
    if clean.is_empty() {
        return Err(RadioError::InvalidRequest("album id is required".into()));
    }

    let mut album = catalog.album(clean);
    if album.is_none()
        && let Some(q) = qobuz
    {
        let raw = clean.strip_prefix("qobuz:").unwrap_or(clean);
        if let Ok(a) = q.get_album(raw).await {
            album = Some(a);
        }
    }

    let album = album.ok_or_else(|| RadioError::SeedNotFound(clean.into()))?;
    let qobuz_album_id = album.id.strip_prefix("qobuz:").unwrap_or("").to_string();

    let mut seed_tracks = album.tracks.clone();
    if seed_tracks.is_empty()
        && let Ok((ts, _)) = catalog.query_tracks(&TrackFilter {
            album_id: album.id.clone(),
            limit: 100,
            ..Default::default()
        })
    {
        seed_tracks = ts;
    }

    let qobuz_artist_id = if album.qobuz_artist_id > 0 {
        album.qobuz_artist_id
    } else {
        seed_tracks
            .iter()
            .find(|t| t.qobuz_artist_id > 0)
            .map_or(0, |t| t.qobuz_artist_id)
    };
    let mb_artist_id = if !album.mb_artist_id.is_empty() {
        album.mb_artist_id.clone()
    } else {
        seed_tracks
            .iter()
            .find(|t| !t.mb_artist_id.is_empty())
            .map_or_else(String::new, |t| t.mb_artist_id.clone())
    };
    let genre = if !album.genre.is_empty() {
        album.genre.clone()
    } else {
        seed_tracks
            .iter()
            .find(|t| !t.genre.is_empty())
            .map_or_else(String::new, |t| t.genre.clone())
    };

    let label = album.title.clone();

    Ok(ResolvedSeed {
        kind: "album",
        label,
        seed_track: None,
        seed_tracks,
        exclude_track_ids: HashSet::new(),
        artist_name: album.artist,
        qobuz_album_id,
        qobuz_artist_id,
        mb_artist_id,
        genre,
        year: album.year,
    })
}

async fn resolve_artist_seed(
    id: &str,
    catalog: &Arc<Catalog>,
    qobuz: Option<&Arc<QobuzClient>>,
) -> Result<ResolvedSeed, RadioError> {
    let clean = id.trim();
    if clean.is_empty() {
        return Err(RadioError::InvalidRequest("artist id is required".into()));
    }

    let numeric_qid = parse_qobuz_numeric_id(clean).ok();
    let mut artist_name = if numeric_qid.is_none() {
        clean.to_string()
    } else {
        String::new()
    };
    let mut qobuz_artist_id = numeric_qid.unwrap_or(0);
    let mut seed_tracks: Vec<Track> = Vec::new();

    // If numeric Qobuz artist ID, fetch artist detail from Qobuz to get name + top tracks.
    if let (Some(qid), Some(q)) = (numeric_qid, qobuz)
        && let Ok(detail) = q.get_artist(qid, 12).await
    {
        if !detail.artist.name.is_empty() {
            artist_name = detail.artist.name;
        }
        seed_tracks.extend(detail.tracks);
    }

    // Look up local library tracks by artist name (exact or prefix-safe via TrackFilter.artist).
    if !artist_name.is_empty()
        && let Ok((local_tracks, _)) = catalog.query_tracks(&TrackFilter {
            artist: artist_name.clone(),
            limit: 60,
            ..Default::default()
        })
    {
        seed_tracks.extend(local_tracks);
    }

    // If we only had a name and Qobuz is connected, look up Qobuz artist ID + top tracks.
    if qobuz_artist_id == 0
        && !artist_name.is_empty()
        && let Some(q) = qobuz
        && let Ok(detail) = q.get_artist_by_name(&artist_name, 10).await
    {
        qobuz_artist_id = detail.artist.id;
        if seed_tracks.len() < 10 {
            seed_tracks.extend(detail.tracks);
        }
    }

    if artist_name.is_empty() && seed_tracks.is_empty() {
        return Err(RadioError::SeedNotFound(clean.into()));
    }
    if artist_name.is_empty() {
        artist_name = seed_tracks
            .first()
            .map(|t| t.artist.clone())
            .unwrap_or_else(|| clean.to_string());
    }

    let mb_artist_id = seed_tracks
        .iter()
        .find(|t| !t.mb_artist_id.is_empty())
        .map_or_else(String::new, |t| t.mb_artist_id.clone());
    if qobuz_artist_id == 0 {
        qobuz_artist_id = seed_tracks
            .iter()
            .find(|t| t.qobuz_artist_id > 0)
            .map_or(0, |t| t.qobuz_artist_id);
    }
    let genre = seed_tracks
        .iter()
        .find(|t| !t.genre.is_empty())
        .map_or_else(String::new, |t| t.genre.clone());
    let year = seed_tracks
        .iter()
        .find(|t| t.year > 0)
        .map_or(0, |t| t.year);

    Ok(ResolvedSeed {
        kind: "artist",
        label: artist_name.clone(),
        seed_track: None,
        seed_tracks,
        exclude_track_ids: HashSet::new(),
        artist_name,
        qobuz_album_id: String::new(),
        qobuz_artist_id,
        mb_artist_id,
        genre,
        year,
    })
}

fn parse_qobuz_numeric_id(id: &str) -> Result<i64, ()> {
    let raw = id.strip_prefix("qobuz:").unwrap_or(id);
    raw.parse::<i64>().ok().filter(|&v| v > 0).ok_or(())
}

fn first_non_empty(vals: &[&str]) -> String {
    vals.iter()
        .map(|s| s.trim())
        .find(|s| !s.is_empty())
        .unwrap_or("")
        .to_string()
}
