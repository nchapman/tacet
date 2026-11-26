use std::collections::HashMap;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::library::{Album, Track};

use super::client::Client;
use super::types::{
    Artist, DiscoverFeed, DiscoverGenre, DiscoverShelf, DiscoverShelfPage, QAlbum, QArtist,
    QDiscoverAlbum, QTracksList, QobuzError, Result, convert_discover_albums,
};

const DISCOVER_GENRES_TTL: Duration = Duration::from_secs(24 * 3600);
const DISCOVER_FEED_TTL: Duration = Duration::from_secs(15 * 60);
const DISCOVER_SIMILAR_TTL: Duration = Duration::from_secs(3600);

struct ShelfDef {
    slug: &'static str,
    title: &'static str,
    subtitle: &'static str,
    index_key: &'static str,
    discover_path: &'static str,
    featured_type: &'static str,
}

const DISCOVER_SHELF_DEFS: &[ShelfDef] = &[
    ShelfDef {
        slug: "new-releases",
        title: "New Releases",
        subtitle: "Fresh studio masters and this week's arrivals",
        index_key: "new_releases",
        discover_path: "/discover/newReleases",
        featured_type: "",
    },
    ShelfDef {
        slug: "most-streamed",
        title: "Most Streamed",
        subtitle: "Popular albums listeners are spinning right now",
        index_key: "most_streamed",
        discover_path: "",
        featured_type: "most-streamed",
    },
    ShelfDef {
        slug: "album-of-the-week",
        title: "Album of the Week",
        subtitle: "Weekly editorial spotlights",
        index_key: "album_of_the_week",
        discover_path: "/discover/albumOfTheWeek",
        featured_type: "",
    },
    ShelfDef {
        slug: "qobuzissims",
        title: "Qobuzissime",
        subtitle: "Standout recordings singled out by Qobuz editors",
        index_key: "qobuzissims",
        discover_path: "",
        featured_type: "qobuzissims",
    },
    ShelfDef {
        slug: "press-awards",
        title: "Press Awards",
        subtitle: "Critically acclaimed releases and award winners",
        index_key: "press_awards",
        discover_path: "",
        featured_type: "press-awards",
    },
    ShelfDef {
        slug: "ideal-discography",
        title: "Ideal Discography",
        subtitle: "Essential albums worth knowing inside and out",
        index_key: "ideal_discography",
        discover_path: "/discover/idealDiscography",
        featured_type: "",
    },
];

fn lookup_shelf_def(slug: &str) -> Option<&'static ShelfDef> {
    let norm = slug.trim().to_ascii_lowercase();
    DISCOVER_SHELF_DEFS.iter().find(|d| d.slug == norm)
}

fn normalize_genre_ids(ids: &[i64]) -> String {
    let mut cp: Vec<i64> = ids.iter().copied().filter(|&id| id > 0).collect();
    if cp.is_empty() {
        return String::new();
    }
    cp.sort_unstable();
    cp.dedup();
    cp.into_iter()
        .map(|id| id.to_string())
        .collect::<Vec<_>>()
        .join(",")
}

impl Client {
    pub async fn genres(&self) -> Result<Vec<DiscoverGenre>> {
        self.require_auth()?;
        #[derive(Deserialize)]
        struct Resp {
            #[serde(default)]
            genres: GenresList,
        }
        #[derive(Default, Deserialize)]
        struct GenresList {
            #[serde(default)]
            items: Vec<GenreItem>,
        }
        #[derive(Deserialize)]
        struct GenreItem {
            #[serde(default)]
            id: i64,
            #[serde(default)]
            name: String,
        }
        let resp: Resp = self
            .get_cached_json("/genre/list", &[], DISCOVER_GENRES_TTL)
            .await?;
        Ok(resp
            .genres
            .items
            .into_iter()
            .filter_map(|g| {
                let name = g.name.trim().to_string();
                if g.id > 0 && !name.is_empty() {
                    Some(DiscoverGenre { id: g.id, name })
                } else {
                    None
                }
            })
            .collect())
    }

    pub async fn discover(&self, genre_ids: &[i64]) -> Result<DiscoverFeed> {
        self.require_auth()?;
        let genre_param = normalize_genre_ids(genre_ids);
        let mut params = Vec::new();
        if !genre_param.is_empty() {
            params.push(("genre_ids", genre_param.as_str()));
        }

        #[derive(Deserialize)]
        struct ContainerData {
            #[serde(default)]
            has_more: bool,
            #[serde(default)]
            items: Option<serde_json::Value>,
        }
        #[derive(Deserialize)]
        struct Container {
            #[serde(default)]
            data: Option<ContainerData>,
        }
        #[derive(Deserialize)]
        struct Resp {
            #[serde(default)]
            containers: HashMap<String, Container>,
        }

        let resp: Resp = self
            .get_cached_json("/discover/index", &params, DISCOVER_FEED_TTL)
            .await?;

        let genres = self.genres().await.unwrap_or_default();
        let mut shelves = Vec::with_capacity(DISCOVER_SHELF_DEFS.len());
        for def in DISCOVER_SHELF_DEFS {
            let Some(container) = resp.containers.get(def.index_key) else {
                continue;
            };
            let Some(data) = &container.data else {
                continue;
            };
            let Some(items_val) = &data.items else {
                continue;
            };
            let Ok(raw_items) = serde_json::from_value::<Vec<QDiscoverAlbum>>(items_val.clone())
            else {
                continue;
            };
            let albums = convert_discover_albums(&raw_items);
            if albums.is_empty() {
                continue;
            }
            let has_more = data.has_more || albums.len() >= 10;
            shelves.push(DiscoverShelf {
                slug: def.slug.into(),
                title: def.title.into(),
                subtitle: def.subtitle.into(),
                items: albums,
                has_more,
            });
        }
        Ok(DiscoverFeed { genres, shelves })
    }

    pub async fn discover_shelf(
        &self,
        slug: &str,
        genre_ids: &[i64],
        limit: i32,
        offset: i32,
    ) -> Result<DiscoverShelfPage> {
        self.require_auth()?;
        let def = lookup_shelf_def(slug).ok_or(QobuzError::NotFound)?;
        let limit = if limit <= 0 { 24 } else { limit.min(100) };
        let offset = offset.max(0);
        let genre_param = normalize_genre_ids(genre_ids);
        let lim_s = limit.to_string();
        let off_s = offset.to_string();

        if !def.discover_path.is_empty() {
            let mut params = vec![("limit", lim_s.as_str()), ("offset", off_s.as_str())];
            if !genre_param.is_empty() {
                params.push(("genre_ids", genre_param.as_str()));
            }
            #[derive(Deserialize)]
            struct Resp {
                #[serde(default)]
                has_more: bool,
                #[serde(default)]
                items: Vec<QDiscoverAlbum>,
            }
            let resp: Resp = self
                .get_cached_json(def.discover_path, &params, DISCOVER_FEED_TTL)
                .await?;
            let raw_count = resp.items.len() as i32;
            let has_more = resp.has_more && raw_count > 0;
            let next_offset = if has_more { offset + raw_count } else { 0 };
            Ok(DiscoverShelfPage {
                slug: def.slug.into(),
                title: def.title.into(),
                subtitle: def.subtitle.into(),
                items: convert_discover_albums(&resp.items),
                has_more,
                total: 0,
                offset,
                next_offset,
                limit,
            })
        } else {
            let mut params = vec![
                ("limit", lim_s.as_str()),
                ("offset", off_s.as_str()),
                ("type", def.featured_type),
            ];
            if !genre_param.is_empty() {
                params.push(("genre_ids", genre_param.as_str()));
            }
            #[derive(Deserialize)]
            struct AlbumsWrap {
                #[serde(default)]
                total: i32,
                #[serde(default)]
                items: Vec<QAlbum>,
            }
            #[derive(Deserialize)]
            struct Resp {
                #[serde(default)]
                albums: Option<AlbumsWrap>,
            }
            let resp: Resp = self
                .get_cached_json("/album/getFeatured", &params, DISCOVER_FEED_TTL)
                .await?;
            let wrap = resp.albums.unwrap_or(AlbumsWrap {
                total: 0,
                items: Vec::new(),
            });
            let raw_count = wrap.items.len() as i32;
            let items: Vec<Album> = wrap
                .items
                .into_iter()
                .filter(|qa| qa.streamable != Some(false))
                .map(|qa| qa.to_library_album(false))
                .collect();
            let has_more = raw_count > 0 && offset + raw_count < wrap.total;
            let next_offset = if has_more { offset + raw_count } else { 0 };
            Ok(DiscoverShelfPage {
                slug: def.slug.into(),
                title: def.title.into(),
                subtitle: def.subtitle.into(),
                items,
                has_more,
                total: wrap.total,
                offset,
                next_offset,
                limit,
            })
        }
    }

    pub async fn similar_albums(&self, album_id: &str, limit: i32) -> Result<Vec<Album>> {
        self.require_auth()?;
        let raw_id = album_id
            .trim()
            .strip_prefix("qobuz:")
            .unwrap_or(album_id.trim());
        if raw_id.is_empty() {
            return Err(QobuzError::NotFound);
        }
        let limit = if limit <= 0 { 12 } else { limit.min(50) };
        let lim_s = limit.to_string();
        #[derive(Deserialize)]
        struct AlbumsWrap {
            #[serde(default)]
            items: Vec<QDiscoverAlbum>,
        }
        #[derive(Deserialize)]
        struct Resp {
            #[serde(default)]
            albums: Option<AlbumsWrap>,
        }
        let resp: Resp = self
            .get_cached_json(
                "/album/suggest",
                &[("album_id", raw_id), ("limit", &lim_s)],
                DISCOVER_SIMILAR_TTL,
            )
            .await?;
        let items = resp.albums.map(|w| w.items).unwrap_or_default();
        let mut out = convert_discover_albums(&items);
        out.truncate(limit as usize);
        Ok(out)
    }

    pub async fn similar_artists(&self, artist_id: i64, limit: i32) -> Result<Vec<Artist>> {
        self.require_auth()?;
        if artist_id <= 0 {
            return Err(QobuzError::NotFound);
        }
        let limit = if limit <= 0 { 12 } else { limit.min(50) };
        let aid_s = artist_id.to_string();
        let lim_s = limit.to_string();
        #[derive(Deserialize)]
        struct ArtistsWrap {
            #[serde(default)]
            items: Vec<QArtist>,
        }
        #[derive(Deserialize)]
        struct Resp {
            #[serde(default)]
            artists: Option<ArtistsWrap>,
        }
        let resp: Resp = self
            .get_cached_json(
                "/artist/getSimilarArtists",
                &[("artist_id", &aid_s), ("limit", &lim_s)],
                DISCOVER_SIMILAR_TTL,
            )
            .await?;
        let items = resp.artists.map(|w| w.items).unwrap_or_default();
        Ok(items
            .into_iter()
            .map(|qa| qa.to_artist())
            .filter(|a| a.id > 0 && !a.name.is_empty())
            .collect())
    }

    /// Calls Qobuz's `POST /dynamic/suggest` radio/autoplay endpoint.
    /// All four fields (`track_id`, `artist_id`, `genre_id`, `label_id`) are sent
    /// in `track_to_analyse`; omitting artist/genre/label causes Qobuz to ignore the seed.
    pub async fn dynamic_suggest(
        &self,
        seeds: &[DynamicSuggestSeed],
        listened_track_ids: &[i64],
        limit: i32,
    ) -> Result<Vec<Track>> {
        self.require_auth()?;
        let valid_seeds: Vec<&DynamicSuggestSeed> = seeds
            .iter()
            .filter(|s| s.track_id > 0 && s.artist_id > 0 && s.genre_id > 0 && s.label_id > 0)
            .collect();
        if valid_seeds.is_empty() {
            return Ok(Vec::new());
        }
        let limit = if limit <= 0 { 20 } else { limit.min(50) };
        #[derive(Serialize)]
        struct Req<'a> {
            limit: i32,
            listened_tracks_ids: &'a [i64],
            track_to_analyse: &'a [&'a DynamicSuggestSeed],
        }
        #[derive(Deserialize)]
        struct Resp {
            #[serde(default)]
            tracks: Option<QTracksList>,
        }
        let body = Req {
            limit,
            listened_tracks_ids: listened_track_ids,
            track_to_analyse: &valid_seeds,
        };
        let resp: Resp = self.post_json("/dynamic/suggest", &body).await?;
        let items = resp.tracks.map(|t| t.items).unwrap_or_default();
        Ok(items
            .into_iter()
            .filter(|qt| qt.id > 0)
            .map(|qt| qt.to_library_track(None))
            .collect())
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct DynamicSuggestSeed {
    pub track_id: i64,
    pub artist_id: i64,
    pub genre_id: i64,
    pub label_id: i64,
}
