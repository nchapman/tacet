use rusqlite::{Connection, OptionalExtension, params, types::Value};

use super::{LibraryStore, row_to_track, unix_to_dt};
use crate::library::types::{
    Album, AlbumFilter, AlbumVersion, ArtistSummary, FacetFilter, GenreSummary, LibraryError,
    Result, SOURCE_LOCAL, TopSearchResult, Track, TrackFilter, UnifiedSearchResult,
};

impl LibraryStore {
    pub fn query_albums(&self, f: &AlbumFilter) -> Result<(Vec<Album>, i32)> {
        let inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let conn = inner.conn.as_ref().ok_or(LibraryError::NotFound)?;
        query_albums_conn(conn, f)
    }

    pub fn get_album(&self, id: &str) -> Result<Option<Album>> {
        let inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let conn = inner.conn.as_ref().ok_or(LibraryError::NotFound)?;

        let row: Option<Album> = conn
            .query_row(
                "SELECT a.id, a.source_type, a.provider, a.external_id, a.title, a.artist, a.genre, a.year,
                        a.track_count, a.duration_sec, a.artwork_url, a.primary_format,
                        a.max_bit_depth, a.max_sample_rate, a.version_group_id, a.is_primary,
                        a.is_partial, a.available, a.added_at,
                        COALESCE((SELECT SUM(t.play_count) FROM tracks t WHERE t.album_id = a.id), 0)
                 FROM albums a WHERE a.id = ?1",
                params![id],
                |r| row_to_album(r, false),
            )
            .optional()?;

        let Some(mut album) = row else {
            return Ok(None);
        };

        {
            let mut stmt = conn.prepare(
                "SELECT id, album_id, source_type, provider, external_id, title, artist, album_artist,
                        album, genre, disc_number, track_number, year, duration_sec, sample_rate,
                        bit_depth, channels, mime, ext, size_bytes, artwork_url, file_path,
                        file_mtime_ns, folder_art, has_embed_art, qobuz_track_id, play_count,
                        last_played_at, loved, available, added_at
                 FROM tracks WHERE album_id = ?1 AND available = 1
                 ORDER BY disc_number ASC, track_number ASC, sort_title ASC, id ASC",
            )?;
            let rows = stmt.query_map(params![album.id], row_to_track)?;
            for r in rows {
                album.tracks.push(r?);
            }
        }

        if !album.version_group_id.is_empty() {
            let mut stmt = conn.prepare(
                "SELECT id, source_type, provider, title, year, track_count,
                        primary_format, max_sample_rate, max_bit_depth, is_primary, available
                 FROM albums WHERE version_group_id = ?1 AND available = 1
                 ORDER BY is_primary DESC, max_bit_depth DESC, max_sample_rate DESC, id ASC",
            )?;
            let rows = stmt.query_map(params![album.version_group_id], |r| {
                let provider: String = r.get(2)?;
                Ok(AlbumVersion {
                    id: r.get(0)?,
                    source: provider.clone(),
                    source_type: r.get(1)?,
                    provider,
                    title: r.get(3)?,
                    year: r.get(4)?,
                    track_count: r.get(5)?,
                    primary_format: r.get(6)?,
                    max_sample_rate: r.get(7)?,
                    max_bit_depth: r.get(8)?,
                    is_primary: r.get::<_, i32>(9)? != 0,
                    available: r.get::<_, i32>(10)? != 0,
                })
            })?;
            for r in rows {
                album.versions.push(r?);
            }
            album.version_count = album.versions.len() as i32;
        } else {
            album.version_count = 1;
        }

        Ok(Some(album))
    }

    pub fn has_album(&self, id: &str) -> bool {
        let inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let Some(conn) = inner.conn.as_ref() else {
            return false;
        };
        conn.query_row(
            "SELECT 1 FROM albums WHERE id = ?1 AND available = 1",
            params![id],
            |_| Ok(()),
        )
        .optional()
        .ok()
        .flatten()
        .is_some()
    }

    pub fn query_tracks(&self, f: &TrackFilter) -> Result<(Vec<Track>, i32)> {
        let inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let conn = inner.conn.as_ref().ok_or(LibraryError::NotFound)?;
        query_tracks_conn(conn, f)
    }

    pub fn get_track(&self, id: &str) -> Result<Option<Track>> {
        let inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let conn = inner.conn.as_ref().ok_or(LibraryError::NotFound)?;
        let row = conn
            .query_row(
                "SELECT id, album_id, source_type, provider, external_id, title, artist, album_artist,
                        album, genre, disc_number, track_number, year, duration_sec, sample_rate,
                        bit_depth, channels, mime, ext, size_bytes, artwork_url, file_path,
                        file_mtime_ns, folder_art, has_embed_art, qobuz_track_id, play_count,
                        last_played_at, loved, available, added_at
                 FROM tracks WHERE id = ?1",
                params![id],
                row_to_track,
            )
            .optional()?;
        Ok(row)
    }

    pub fn get_track_by_artwork_url(&self, artwork_url: &str) -> Result<Option<Track>> {
        let inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let conn = inner.conn.as_ref().ok_or(LibraryError::NotFound)?;
        let row = conn
            .query_row(
                "SELECT id, album_id, source_type, provider, external_id, title, artist, album_artist,
                        album, genre, disc_number, track_number, year, duration_sec, sample_rate,
                        bit_depth, channels, mime, ext, size_bytes, artwork_url, file_path,
                        file_mtime_ns, folder_art, has_embed_art, qobuz_track_id, play_count,
                        last_played_at, loved, available, added_at
                 FROM tracks WHERE artwork_url = ?1 AND available = 1 LIMIT 1",
                params![artwork_url],
                row_to_track,
            )
            .optional()?;
        Ok(row)
    }

    pub fn list_local_art_hashes(&self) -> Result<Vec<String>> {
        let inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let conn = inner.conn.as_ref().ok_or(LibraryError::NotFound)?;
        let mut stmt = conn.prepare(
            "SELECT DISTINCT artwork_url FROM tracks
             WHERE provider = ?1 AND available = 1 AND artwork_url LIKE '/media/art/%'",
        )?;
        let rows = stmt.query_map(params![SOURCE_LOCAL], |r| r.get::<_, String>(0))?;
        let mut out = Vec::new();
        for r in rows {
            let url = r?;
            if let Some(hash) = url.strip_prefix("/media/art/")
                && hash.len() == 16
                && hash.chars().all(|c| c.is_ascii_hexdigit())
            {
                out.push(hash.to_string());
            }
        }
        Ok(out)
    }

    pub fn list_artists(&self, f: &FacetFilter) -> Result<(Vec<ArtistSummary>, i32)> {
        let inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let conn = inner.conn.as_ref().ok_or(LibraryError::NotFound)?;
        list_artists_conn(conn, f)
    }

    pub fn list_genres(&self, f: &FacetFilter) -> Result<(Vec<GenreSummary>, i32)> {
        let inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let conn = inner.conn.as_ref().ok_or(LibraryError::NotFound)?;
        list_genres_conn(conn, f)
    }

    pub fn unified_search(&self, raw_query: &str, mut limit: i32) -> Result<UnifiedSearchResult> {
        let q = raw_query.trim().to_string();
        if limit <= 0 {
            limit = 8;
        }
        limit = limit.min(50);

        let mut res = UnifiedSearchResult {
            query: q.clone(),
            top_result: None,
            artists: Vec::new(),
            albums: Vec::new(),
            tracks: Vec::new(),
        };
        if q.is_empty() {
            return Ok(res);
        }

        let inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let conn = inner.conn.as_ref().ok_or(LibraryError::NotFound)?;

        let candidate_limit = (limit * 4).max(32);
        let (artists, _) = list_artists_conn(
            conn,
            &FacetFilter {
                query: q.clone(),
                limit: candidate_limit,
                ..Default::default()
            },
        )?;
        let (albums, _) = query_albums_conn(
            conn,
            &AlbumFilter {
                query: q.clone(),
                group_versions: true,
                limit: candidate_limit,
                ..Default::default()
            },
        )?;
        let (tracks, _) = query_tracks_conn(
            conn,
            &TrackFilter {
                query: q.clone(),
                limit: candidate_limit,
                ..Default::default()
            },
        )?;

        let norm_q = normalize_search_text(&q);
        let q_tokens: Vec<&str> = norm_q.split_whitespace().collect();
        let scored_artists = rank_artists(artists, &norm_q, &q_tokens);
        let scored_albums = rank_albums(albums, &norm_q, &q_tokens);
        let scored_tracks = rank_tracks(tracks, &norm_q, &q_tokens);

        res.top_result = pick_top_result(
            scored_artists.first(),
            scored_albums.first(),
            scored_tracks.first(),
        );
        let lim = limit as usize;
        res.artists = scored_artists.into_iter().take(lim).map(|x| x.0).collect();
        res.albums = scored_albums.into_iter().take(lim).map(|x| x.0).collect();
        res.tracks = scored_tracks.into_iter().take(lim).map(|x| x.0).collect();
        Ok(res)
    }
}

fn rank_artists(
    artists: Vec<ArtistSummary>,
    norm_q: &str,
    q_tokens: &[&str],
) -> Vec<(ArtistSummary, i32)> {
    let mut scored: Vec<(ArtistSummary, i32)> = artists
        .into_iter()
        .map(|a| {
            let mut sc = score_search_text(&a.name, norm_q, q_tokens);
            if sc > 0 {
                if normalize_search_text(&a.name) == norm_q {
                    sc += 40;
                }
                sc += (a.album_count * 5).min(30);
            }
            (a, sc)
        })
        .collect();
    scored.sort_by(|a, b| {
        b.1.cmp(&a.1)
            .then_with(|| b.0.album_count.cmp(&a.0.album_count))
            .then_with(|| a.0.name.to_lowercase().cmp(&b.0.name.to_lowercase()))
    });
    scored
}

fn rank_albums(albums: Vec<Album>, norm_q: &str, q_tokens: &[&str]) -> Vec<(Album, i32)> {
    let mut scored: Vec<(Album, i32)> = albums
        .into_iter()
        .map(|a| {
            let title_sc = score_search_text(&a.title, norm_q, q_tokens);
            let combo_sc =
                score_search_text(&format!("{} {}", a.artist, a.title), norm_q, q_tokens);
            let artist_sc = score_search_text(&a.artist, norm_q, q_tokens) / 3;
            let mut sc = title_sc.max(combo_sc).max(artist_sc);
            if title_sc >= 500 {
                sc += (a.play_count * 3).min(25);
            }
            (a, sc)
        })
        .collect();
    scored.sort_by(|a, b| {
        b.1.cmp(&a.1)
            .then_with(|| b.0.play_count.cmp(&a.0.play_count))
            .then_with(|| a.0.title.to_lowercase().cmp(&b.0.title.to_lowercase()))
    });
    scored
}

fn rank_tracks(tracks: Vec<Track>, norm_q: &str, q_tokens: &[&str]) -> Vec<(Track, i32)> {
    let mut scored: Vec<(Track, i32)> = tracks
        .into_iter()
        .map(|t| {
            let title_sc = score_search_text(&t.title, norm_q, q_tokens);
            let combo_sc =
                score_search_text(&format!("{} {}", t.artist, t.title), norm_q, q_tokens);
            let meta_sc = score_search_text(&t.artist, norm_q, q_tokens)
                .max(score_search_text(&t.album, norm_q, q_tokens))
                / 4;
            let mut sc = title_sc.max(combo_sc).max(meta_sc);
            if title_sc >= 500 {
                sc += (t.play_count * 3).min(25);
            }
            (t, sc)
        })
        .collect();
    scored.sort_by(|a, b| {
        b.1.cmp(&a.1)
            .then_with(|| b.0.play_count.cmp(&a.0.play_count))
            .then_with(|| a.0.title.to_lowercase().cmp(&b.0.title.to_lowercase()))
    });
    scored
}

fn pick_top_result(
    artist: Option<&(ArtistSummary, i32)>,
    album: Option<&(Album, i32)>,
    track: Option<&(Track, i32)>,
) -> Option<TopSearchResult> {
    let mut best: Option<TopSearchResult> = None;
    if let Some((a, sc)) = artist {
        best = Some(TopSearchResult {
            kind: "artist".into(),
            score: *sc,
            artist: Some(a.clone()),
            album: None,
            track: None,
        });
    }
    if let Some((a, sc)) = album
        && best.as_ref().is_none_or(|b| *sc > b.score)
    {
        best = Some(TopSearchResult {
            kind: "album".into(),
            score: *sc,
            artist: None,
            album: Some(a.clone()),
            track: None,
        });
    }
    if let Some((t, sc)) = track
        && best.as_ref().is_none_or(|b| *sc > b.score)
    {
        best = Some(TopSearchResult {
            kind: "track".into(),
            score: *sc,
            artist: None,
            album: None,
            track: Some(t.clone()),
        });
    }
    best
}

fn row_to_album(r: &rusqlite::Row<'_>, with_version_count: bool) -> rusqlite::Result<Album> {
    let provider: String = r.get(2)?;
    let is_partial = r.get::<_, i32>(16)? != 0;
    let available = r.get::<_, i32>(17)? != 0;
    Ok(Album {
        id: r.get(0)?,
        source: provider.clone(),
        source_type: r.get(1)?,
        provider,
        external_id: r.get(3)?,
        title: r.get(4)?,
        artist: r.get(5)?,
        genre: r.get(6)?,
        year: r.get(7)?,
        track_count: r.get(8)?,
        duration_sec: r.get(9)?,
        artwork_url: r.get(10)?,
        primary_format: r.get(11)?,
        max_bit_depth: r.get(12)?,
        max_sample_rate: r.get(13)?,
        version_group_id: r.get(14)?,
        is_primary: r.get::<_, i32>(15)? != 0,
        is_partial,
        available,
        added_at: unix_to_dt(r.get(18)?),
        play_count: r.get(19)?,
        version_count: if with_version_count { r.get(20)? } else { 0 },
        in_library: available && !is_partial,
        ..Default::default()
    })
}

fn build_album_filter_where(f: &AlbumFilter) -> (String, Vec<Value>) {
    let mut clauses: Vec<String> = Vec::new();
    let mut bind_vals: Vec<Value> = Vec::new();

    if !f.include_unavailable {
        clauses.push("a.available = 1".into());
    }
    let st = f.source_type.trim().to_ascii_lowercase();
    if !st.is_empty() && st != "all" {
        bind_vals.push(Value::Text(st));
        clauses.push(format!("a.source_type = ?{}", bind_vals.len()));
    }
    let prov = f.provider.trim().to_ascii_lowercase();
    if !prov.is_empty() && prov != "all" {
        bind_vals.push(Value::Text(prov));
        clauses.push(format!("a.provider = ?{}", bind_vals.len()));
    }
    let artist = f.artist.trim();
    if !artist.is_empty() {
        bind_vals.push(Value::Text(artist.to_lowercase()));
        clauses.push(format!("LOWER(a.artist) = ?{}", bind_vals.len()));
    }
    let genre = f.genre.trim();
    if !genre.is_empty() {
        bind_vals.push(Value::Text(genre.to_lowercase()));
        clauses.push(format!("LOWER(a.genre) = ?{}", bind_vals.len()));
    }
    if f.group_versions {
        clauses.push("a.is_primary = 1".into());
    }
    let fts = build_fts_query(&f.query);
    if !fts.is_empty() {
        bind_vals.push(Value::Text(fts.clone()));
        let alb_idx = bind_vals.len();
        bind_vals.push(Value::Text(fts));
        let trk_idx = bind_vals.len();
        clauses.push(format!(
            "(a.id IN (SELECT album_id FROM albums_fts WHERE albums_fts MATCH ?{alb_idx}) \
             OR a.id IN (SELECT t.album_id FROM tracks t WHERE t.available = 1 AND t.id IN (SELECT track_id FROM tracks_fts WHERE tracks_fts MATCH ?{trk_idx})))"
        ));
    }
    let where_sql = if clauses.is_empty() {
        String::new()
    } else {
        format!("WHERE {}", clauses.join(" AND "))
    };
    (where_sql, bind_vals)
}

fn query_albums_conn(conn: &Connection, f: &AlbumFilter) -> Result<(Vec<Album>, i32)> {
    let (where_sql, bind_vals) = build_album_filter_where(f);
    let count_sql = format!("SELECT COUNT(*) FROM albums a {where_sql}");
    let total: i32 = conn.query_row(
        &count_sql,
        rusqlite::params_from_iter(bind_vals.iter()),
        |r| r.get(0),
    )?;

    let order_by = match f.sort.trim().to_ascii_lowercase().as_str() {
        "title" => "a.sort_title ASC, a.sort_artist ASC, a.id ASC",
        "year" => "a.year DESC, a.sort_artist ASC, a.sort_title ASC, a.id ASC",
        "added" => "a.added_at DESC, a.sort_title ASC, a.id ASC",
        "plays" => "play_count DESC, a.sort_artist ASC, a.sort_title ASC, a.id ASC",
        _ => "a.sort_artist ASC, a.sort_title ASC, a.id ASC",
    };

    let limit = if f.limit <= 0 {
        200
    } else {
        f.limit.min(100_000)
    };
    let offset = f.offset.max(0);

    let mut query_vals = bind_vals;
    query_vals.push(Value::Integer(i64::from(limit)));
    let lim_idx = query_vals.len();
    query_vals.push(Value::Integer(i64::from(offset)));
    let off_idx = query_vals.len();

    let sql = format!(
        "SELECT a.id, a.source_type, a.provider, a.external_id, a.title, a.artist, a.genre, a.year,
                a.track_count, a.duration_sec, a.artwork_url, a.primary_format,
                a.max_bit_depth, a.max_sample_rate, a.version_group_id, a.is_primary,
                a.is_partial, a.available, a.added_at,
                COALESCE((SELECT SUM(t.play_count) FROM tracks t WHERE t.album_id = a.id), 0) AS play_count,
                CASE WHEN a.version_group_id != '' THEN
                    (SELECT COUNT(*) FROM albums b WHERE b.version_group_id = a.version_group_id AND b.available = 1)
                ELSE 1 END AS version_count
         FROM albums a
         {where_sql}
         ORDER BY {order_by}
         LIMIT ?{lim_idx} OFFSET ?{off_idx}"
    );

    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(rusqlite::params_from_iter(query_vals.iter()), |r| {
        row_to_album(r, true)
    })?;

    let mut out = Vec::new();
    for r in rows {
        out.push(r?);
    }
    Ok((out, total))
}

fn build_track_filter_where(f: &TrackFilter) -> (String, Vec<Value>) {
    let mut clauses: Vec<String> = Vec::new();
    let mut bind_vals: Vec<Value> = Vec::new();

    if !f.include_unavailable {
        clauses.push("t.available = 1".into());
    }
    let st = f.source_type.trim().to_ascii_lowercase();
    if !st.is_empty() && st != "all" {
        bind_vals.push(Value::Text(st));
        clauses.push(format!("t.source_type = ?{}", bind_vals.len()));
    }
    let prov = f.provider.trim().to_ascii_lowercase();
    if !prov.is_empty() && prov != "all" {
        bind_vals.push(Value::Text(prov));
        clauses.push(format!("t.provider = ?{}", bind_vals.len()));
    }
    if !f.album_id.trim().is_empty() {
        bind_vals.push(Value::Text(f.album_id.trim().to_string()));
        clauses.push(format!("t.album_id = ?{}", bind_vals.len()));
    }
    if !f.artist.trim().is_empty() {
        let low = f.artist.trim().to_lowercase();
        bind_vals.push(Value::Text(low.clone()));
        let p1 = bind_vals.len();
        bind_vals.push(Value::Text(low));
        let p2 = bind_vals.len();
        clauses.push(format!(
            "(LOWER(t.artist) = ?{p1} OR LOWER(t.album_artist) = ?{p2})"
        ));
    }
    if !f.album.trim().is_empty() {
        bind_vals.push(Value::Text(f.album.trim().to_lowercase()));
        clauses.push(format!("LOWER(t.album) = ?{}", bind_vals.len()));
    }
    if !f.genre.trim().is_empty() {
        bind_vals.push(Value::Text(f.genre.trim().to_lowercase()));
        clauses.push(format!("LOWER(t.genre) = ?{}", bind_vals.len()));
    }
    let fts = build_fts_query(&f.query);
    if !fts.is_empty() {
        bind_vals.push(Value::Text(fts));
        clauses.push(format!(
            "t.id IN (SELECT track_id FROM tracks_fts WHERE tracks_fts MATCH ?{})",
            bind_vals.len()
        ));
    }
    let where_sql = if clauses.is_empty() {
        String::new()
    } else {
        format!("WHERE {}", clauses.join(" AND "))
    };
    (where_sql, bind_vals)
}

fn track_order_by(f: &TrackFilter) -> String {
    let dir = if f.sort_dir.eq_ignore_ascii_case("desc") {
        "DESC"
    } else {
        "ASC"
    };
    match f.sort.trim().to_ascii_lowercase().as_str() {
        "title" => format!("t.sort_title {dir}, t.sort_artist ASC, t.id ASC"),
        "album" => format!(
            "t.sort_album {dir}, t.disc_number ASC, t.track_number ASC, t.sort_title ASC, t.id ASC"
        ),
        "artist" => format!(
            "t.sort_artist {dir}, t.sort_album ASC, t.disc_number ASC, t.track_number ASC, t.id ASC"
        ),
        "year" => format!(
            "t.year {dir}, t.sort_album ASC, t.disc_number ASC, t.track_number ASC, t.id ASC"
        ),
        "added" => format!(
            "t.added_at {dir}, t.sort_album ASC, t.disc_number ASC, t.track_number ASC, t.id ASC"
        ),
        "plays" => format!("t.play_count {dir}, t.last_played_at DESC, t.sort_title ASC, t.id ASC"),
        "duration" => format!("t.duration_sec {dir}, t.sort_title ASC, t.id ASC"),
        "quality" => format!("t.bit_depth {dir}, t.sample_rate {dir}, t.sort_title ASC, t.id ASC"),
        _ => format!(
            "t.sort_artist {dir}, t.sort_album ASC, t.disc_number ASC, t.track_number ASC, t.sort_title ASC, t.id ASC"
        ),
    }
}

fn query_tracks_conn(conn: &Connection, f: &TrackFilter) -> Result<(Vec<Track>, i32)> {
    let (where_sql, bind_vals) = build_track_filter_where(f);
    let count_sql = format!("SELECT COUNT(*) FROM tracks t {where_sql}");
    let total: i32 = conn.query_row(
        &count_sql,
        rusqlite::params_from_iter(bind_vals.iter()),
        |r| r.get(0),
    )?;

    let order_by = track_order_by(f);
    let limit = if f.limit <= 0 {
        500
    } else {
        f.limit.min(100_000)
    };
    let offset = f.offset.max(0);

    let mut query_vals = bind_vals;
    query_vals.push(Value::Integer(i64::from(limit)));
    let lim_idx = query_vals.len();
    query_vals.push(Value::Integer(i64::from(offset)));
    let off_idx = query_vals.len();

    let sql = format!(
        "SELECT t.id, t.album_id, t.source_type, t.provider, t.external_id, t.title, t.artist,
                t.album_artist, t.album, t.genre, t.disc_number, t.track_number, t.year,
                t.duration_sec, t.sample_rate, t.bit_depth, t.channels, t.mime, t.ext,
                t.size_bytes, t.artwork_url, t.file_path, t.file_mtime_ns, t.folder_art,
                t.has_embed_art, t.qobuz_track_id, t.play_count, t.last_played_at,
                t.loved, t.available, t.added_at
         FROM tracks t
         {where_sql}
         ORDER BY {order_by}
         LIMIT ?{lim_idx} OFFSET ?{off_idx}"
    );

    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(rusqlite::params_from_iter(query_vals.iter()), row_to_track)?;
    let mut out = Vec::new();
    for r in rows {
        out.push(r?);
    }
    Ok((out, total))
}

fn list_artists_conn(conn: &Connection, f: &FacetFilter) -> Result<(Vec<ArtistSummary>, i32)> {
    let mut clauses: Vec<String> = vec!["a.artist != ''".into()];
    let mut bind_vals: Vec<Value> = Vec::new();

    if !f.include_unavailable {
        clauses.push("a.available = 1".into());
    }
    let st = f.source_type.trim().to_ascii_lowercase();
    if !st.is_empty() && st != "all" {
        bind_vals.push(Value::Text(st));
        clauses.push(format!("a.source_type = ?{}", bind_vals.len()));
    }
    let prov = f.provider.trim().to_ascii_lowercase();
    if !prov.is_empty() && prov != "all" {
        bind_vals.push(Value::Text(prov));
        clauses.push(format!("a.provider = ?{}", bind_vals.len()));
    }
    if !f.genre.trim().is_empty() {
        bind_vals.push(Value::Text(f.genre.trim().to_lowercase()));
        clauses.push(format!("LOWER(a.genre) = ?{}", bind_vals.len()));
    }
    if !f.query.trim().is_empty() {
        let like = format!("%{}%", f.query.trim().to_lowercase());
        bind_vals.push(Value::Text(like));
        clauses.push(format!("LOWER(a.artist) LIKE ?{}", bind_vals.len()));
    }

    let where_sql = format!("WHERE {}", clauses.join(" AND "));
    let count_sql = format!("SELECT COUNT(DISTINCT LOWER(a.artist)) FROM albums a {where_sql}");
    let total: i32 = conn.query_row(
        &count_sql,
        rusqlite::params_from_iter(bind_vals.iter()),
        |r| r.get(0),
    )?;

    let limit = if f.limit <= 0 { 200 } else { f.limit.min(5000) };
    let offset = f.offset.max(0);

    let mut query_vals = bind_vals;
    query_vals.push(Value::Integer(i64::from(limit)));
    let lim_idx = query_vals.len();
    query_vals.push(Value::Integer(i64::from(offset)));
    let off_idx = query_vals.len();

    let sql = format!(
        "SELECT MIN(a.artist) AS name,
                COUNT(DISTINCT CASE WHEN a.version_group_id != '' THEN a.version_group_id ELSE a.id END) AS album_count,
                COALESCE(SUM(CASE WHEN a.is_primary = 1 THEN a.track_count ELSE 0 END), 0) AS track_count,
                MAX(CASE WHEN a.artwork_url != '' THEN a.artwork_url ELSE '' END) AS artwork_url,
                GROUP_CONCAT(DISTINCT a.provider) AS providers,
                MIN(a.sort_artist) AS sort_key
         FROM albums a
         {where_sql}
         GROUP BY LOWER(a.artist)
         ORDER BY sort_key ASC, LOWER(name) ASC
         LIMIT ?{lim_idx} OFFSET ?{off_idx}"
    );

    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(rusqlite::params_from_iter(query_vals.iter()), |r| {
        let raw_provs: Option<String> = r.get(4)?;
        let mut providers: Vec<String> = raw_provs
            .unwrap_or_default()
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(String::from)
            .collect();
        providers.sort();
        providers.dedup();
        Ok(ArtistSummary {
            name: r.get(0)?,
            album_count: r.get(1)?,
            track_count: r.get(2)?,
            artwork_url: r.get::<_, Option<String>>(3)?.unwrap_or_default(),
            providers,
        })
    })?;

    let mut out = Vec::new();
    for r in rows {
        out.push(r?);
    }
    Ok((out, total))
}

fn list_genres_conn(conn: &Connection, f: &FacetFilter) -> Result<(Vec<GenreSummary>, i32)> {
    let mut clauses: Vec<String> = vec!["a.genre != ''".into()];
    let mut bind_vals: Vec<Value> = Vec::new();

    if !f.include_unavailable {
        clauses.push("a.available = 1".into());
    }
    let st = f.source_type.trim().to_ascii_lowercase();
    if !st.is_empty() && st != "all" {
        bind_vals.push(Value::Text(st));
        clauses.push(format!("a.source_type = ?{}", bind_vals.len()));
    }
    let prov = f.provider.trim().to_ascii_lowercase();
    if !prov.is_empty() && prov != "all" {
        bind_vals.push(Value::Text(prov));
        clauses.push(format!("a.provider = ?{}", bind_vals.len()));
    }
    if !f.query.trim().is_empty() {
        let like = format!("%{}%", f.query.trim().to_lowercase());
        bind_vals.push(Value::Text(like));
        clauses.push(format!("LOWER(a.genre) LIKE ?{}", bind_vals.len()));
    }

    let where_sql = format!("WHERE {}", clauses.join(" AND "));
    let count_sql = format!("SELECT COUNT(DISTINCT LOWER(a.genre)) FROM albums a {where_sql}");
    let total: i32 = conn.query_row(
        &count_sql,
        rusqlite::params_from_iter(bind_vals.iter()),
        |r| r.get(0),
    )?;

    let limit = if f.limit <= 0 { 200 } else { f.limit.min(5000) };
    let offset = f.offset.max(0);

    let mut query_vals = bind_vals;
    query_vals.push(Value::Integer(i64::from(limit)));
    let lim_idx = query_vals.len();
    query_vals.push(Value::Integer(i64::from(offset)));
    let off_idx = query_vals.len();

    let sql = format!(
        "SELECT MIN(a.genre) AS name,
                COUNT(DISTINCT CASE WHEN a.version_group_id != '' THEN a.version_group_id ELSE a.id END) AS album_count,
                COALESCE(SUM(CASE WHEN a.is_primary = 1 THEN a.track_count ELSE 0 END), 0) AS track_count
         FROM albums a
         {where_sql}
         GROUP BY LOWER(a.genre)
         ORDER BY album_count DESC, LOWER(name) ASC
         LIMIT ?{lim_idx} OFFSET ?{off_idx}"
    );

    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(rusqlite::params_from_iter(query_vals.iter()), |r| {
        Ok(GenreSummary {
            name: r.get(0)?,
            album_count: r.get(1)?,
            track_count: r.get(2)?,
        })
    })?;

    let mut out = Vec::new();
    for r in rows {
        out.push(r?);
    }
    Ok((out, total))
}

pub fn build_fts_query(raw: &str) -> String {
    let mut tokens = Vec::new();
    let mut cur = String::new();
    for ch in raw.trim().chars() {
        if ch.is_alphanumeric() {
            for lc in ch.to_lowercase() {
                cur.push(lc);
            }
        } else if !cur.is_empty() {
            tokens.push(format!("\"{}\"*", cur.replace('"', "")));
            cur.clear();
        }
    }
    if !cur.is_empty() {
        tokens.push(format!("\"{}\"*", cur.replace('"', "")));
    }
    tokens.join(" ")
}

fn normalize_search_text(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut last_space = true;
    for ch in s.chars() {
        if ch.is_alphanumeric() {
            for lc in ch.to_lowercase() {
                out.push(lc);
            }
            last_space = false;
        } else if !last_space {
            out.push(' ');
            last_space = true;
        }
    }
    out.trim().to_string()
}

fn score_search_text(candidate: &str, norm_q: &str, q_tokens: &[&str]) -> i32 {
    if norm_q.is_empty() {
        return 0;
    }
    let cand = normalize_search_text(candidate);
    if cand.is_empty() {
        return 0;
    }
    if cand == norm_q {
        return 1000;
    }
    let trimmed = cand.strip_prefix("the ").unwrap_or(&cand);
    if trimmed != cand && trimmed == norm_q {
        return 960;
    }
    if cand.starts_with(norm_q) {
        return 820 - ((cand.len().saturating_sub(norm_q.len())) as i32).min(120);
    }
    if trimmed != cand && trimmed.starts_with(norm_q) {
        return 790 - ((trimmed.len().saturating_sub(norm_q.len())) as i32).min(120);
    }
    let words: Vec<&str> = cand.split_whitespace().collect();
    if !q_tokens.is_empty() {
        let all_prefix = q_tokens
            .iter()
            .all(|qt| words.iter().any(|w| w.starts_with(qt)));
        if all_prefix {
            return 620 - ((cand.len().saturating_sub(norm_q.len())) as i32).min(120);
        }
    }
    if cand.contains(norm_q) {
        return 420 - ((cand.len().saturating_sub(norm_q.len())) as i32).min(120);
    }
    0
}
