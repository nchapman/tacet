import { useCallback, useEffect, useRef, useState } from "react";
import { tacetClient } from "../api/client";
import { fmtAlbumQuality, fmtDuration, fmtQuality, isHiRes } from "../api/format";
import type {
  Album,
  ArtistSummary,
  GenreSummary,
  HeavyRotationAlbum,
  LibraryStatus,
  QueuePosition,
  QueueSource,
  Track,
} from "../api/types";
import { AlbumView } from "./AlbumView";

interface LibraryBrowserProps {
  libraryStatus: LibraryStatus | null;
  onPlay: (source: QueueSource, startIndex?: number) => void;
  onAdd: (source: QueueSource, position?: QueuePosition) => void;
}

type LibraryViewMode = "albums" | "songs" | "artists" | "rotation";
type SourceFilter = "all" | "local" | "remote";
type AlbumSort = "artist" | "title" | "year" | "added" | "plays";
type TrackSort = "album" | "title" | "artist" | "year" | "added" | "plays" | "duration" | "quality";

function sourceTag(provider?: string, source?: string): string {
  const p = (provider || source || "local").toLowerCase();
  if (p === "local") return "Local";
  if (p === "qobuz") return "Qobuz";
  return p.charAt(0).toUpperCase() + p.slice(1);
}

export function LibraryBrowser({ libraryStatus, onPlay, onAdd }: LibraryBrowserProps) {
  const [mode, setMode] = useState<LibraryViewMode>("albums");
  const [sourceFilter, setSourceFilter] = useState<SourceFilter>("all");
  const [query, setQuery] = useState("");
  const [debouncedQuery, setDebouncedQuery] = useState("");
  const [albumSort, setAlbumSort] = useState<AlbumSort>("artist");
  const [groupVersions, setGroupVersions] = useState(true);

  // Classic iTunes Songs table & Column Browser state
  const [showColumnBrowser, setShowColumnBrowser] = useState(false);
  const [selectedGenre, setSelectedGenre] = useState("");
  const [selectedArtist, setSelectedArtist] = useState("");
  const [selectedAlbumId, setSelectedAlbumId] = useState("");
  const [trackSort, setTrackSort] = useState<TrackSort>("artist");
  const [trackDir, setTrackDir] = useState<"asc" | "desc">("asc");

  // Data slices
  const [albums, setAlbums] = useState<Album[]>([]);
  const [tracks, setTracks] = useState<Track[]>([]);
  const [tracksTotal, setTracksTotal] = useState(0);
  const [artists, setArtists] = useState<ArtistSummary[]>([]);
  const [genres, setGenres] = useState<GenreSummary[]>([]);
  const [rotation, setRotation] = useState<HeavyRotationAlbum[]>([]);
  const [recentlyAdded, setRecentlyAdded] = useState<Album[]>([]);

  // Detail drilldowns
  const [openAlbum, setOpenAlbum] = useState<Album | null>(null);
  const [detailArtist, setDetailArtist] = useState<string>("");
  const [artistAlbums, setArtistAlbums] = useState<Album[]>([]);

  const [loading, setLoading] = useState(false);
  const [syncing, setSyncing] = useState(false);
  const [errMsg, setErrMsg] = useState("");
  const abortRef = useRef<AbortController | null>(null);

  // Debounce live search for crisp FTS5 typing without spamming requests.
  useEffect(() => {
    const id = window.setTimeout(() => setDebouncedQuery(query.trim()), 150);
    return () => window.clearTimeout(id);
  }, [query]);

  const startRequest = () => {
    abortRef.current?.abort();
    const ctrl = new AbortController();
    abortRef.current = ctrl;
    return ctrl.signal;
  };

  useEffect(() => () => abortRef.current?.abort(), []);

  // Load main view data whenever filters, view mode, or catalog ETag change.
  const loadView = useCallback(async () => {
    const signal = startRequest();
    setLoading(true);
    setErrMsg("");
    try {
      if (mode === "albums") {
        const res = await tacetClient.listLibraryAlbums(
          {
            q: debouncedQuery || undefined,
            source: sourceFilter,
            sort: albumSort,
            groupVersions,
            limit: 500,
          },
          { signal },
        );
        if (!signal.aborted) setAlbums(res.items ?? []);
      } else if (mode === "songs") {
        const [trRes, gRes, aRes, albRes] = await Promise.all([
          tacetClient.listLibraryTracks(
            {
              q: debouncedQuery || undefined,
              source: sourceFilter,
              genre: selectedGenre || undefined,
              artist: selectedArtist || undefined,
              albumId: selectedAlbumId || undefined,
              sort: trackSort,
              dir: trackDir,
              limit: 500,
            },
            { signal },
          ),
          showColumnBrowser
            ? tacetClient.listLibraryGenres({ source: sourceFilter }, { signal })
            : Promise.resolve(null),
          showColumnBrowser
            ? tacetClient.listLibraryArtists(
                { source: sourceFilter, genre: selectedGenre || undefined, limit: 500 },
                { signal },
              )
            : Promise.resolve(null),
          showColumnBrowser
            ? tacetClient.listLibraryAlbums(
                {
                  source: sourceFilter,
                  genre: selectedGenre || undefined,
                  artist: selectedArtist || undefined,
                  groupVersions: false,
                  limit: 500,
                },
                { signal },
              )
            : Promise.resolve(null),
        ]);
        if (signal.aborted) return;
        setTracks(trRes.items ?? []);
        setTracksTotal(trRes.total ?? 0);
        if (gRes) setGenres(gRes.items ?? []);
        if (aRes) setArtists(aRes.items ?? []);
        if (albRes) setAlbums(albRes.items ?? []);
      } else if (mode === "artists") {
        const res = await tacetClient.listLibraryArtists(
          {
            q: debouncedQuery || undefined,
            source: sourceFilter,
            limit: 500,
          },
          { signal },
        );
        if (signal.aborted) return;
        const list = res.items ?? [];
        setArtists(list);
        if (detailArtist) {
          const albRes = await tacetClient.listLibraryAlbums(
            { artist: detailArtist, source: sourceFilter, groupVersions, sort: "year", limit: 200 },
            { signal },
          );
          if (!signal.aborted) setArtistAlbums(albRes.items ?? []);
        }
      } else if (mode === "rotation") {
        const [rotRes, recRes] = await Promise.all([
          tacetClient.getHeavyRotation(30, 20, { signal }),
          tacetClient.listLibraryAlbums(
            { source: sourceFilter, sort: "added", groupVersions, limit: 24 },
            { signal },
          ),
        ]);
        if (signal.aborted) return;
        setRotation(rotRes.items ?? []);
        setRecentlyAdded(recRes.items ?? []);
      }
    } catch (err) {
      if (!signal.aborted) setErrMsg(err instanceof Error ? err.message : String(err));
    } finally {
      if (!signal.aborted) setLoading(false);
    }
  }, [
    mode,
    sourceFilter,
    debouncedQuery,
    albumSort,
    groupVersions,
    showColumnBrowser,
    selectedGenre,
    selectedArtist,
    selectedAlbumId,
    trackSort,
    trackDir,
    detailArtist,
  ]);

  useEffect(() => {
    void loadView();
  }, [loadView, libraryStatus?.etag]);

  const handleOpenAlbum = async (albumId: string) => {
    setErrMsg("");
    try {
      const alb = await tacetClient.getLibraryAlbum(albumId);
      setOpenAlbum(alb);
    } catch (err) {
      setErrMsg(err instanceof Error ? err.message : String(err));
    }
  };

  const handleSelectArtistDetail = (artistName: string) => {
    setArtistAlbums([]);
    setDetailArtist(artistName);
  };

  const handleSync = async () => {
    setSyncing(true);
    setErrMsg("");
    try {
      await tacetClient.syncLibrary("all", false);
    } catch (err) {
      setErrMsg(err instanceof Error ? err.message : String(err));
    } finally {
      setSyncing(false);
    }
  };

  const toggleTrackSort = (col: TrackSort) => {
    if (trackSort === col) {
      setTrackDir((d) => (d === "asc" ? "desc" : "asc"));
    } else {
      setTrackSort(col);
      setTrackDir(col === "plays" || col === "year" || col === "added" || col === "quality" ? "desc" : "asc");
    }
  };

  const isBusy = syncing || Boolean(libraryStatus?.scanning);

  if (openAlbum) {
    return (
      <AlbumView
        album={openAlbum}
        backLabel={mode === "rotation" ? "Heavy Rotation" : mode === "artists" ? detailArtist || "Artists" : "Library"}
        onBack={() => setOpenAlbum(null)}
        onOpenAlbum={(id) => void handleOpenAlbum(id)}
        onAlbumUpdated={(updated) => {
          if (!updated.inLibrary) {
            setOpenAlbum(null);
          } else {
            setOpenAlbum(updated);
          }
          void loadView();
        }}
        onPlay={onPlay}
        onAdd={onAdd}
      />
    );
  }

  return (
    <div className="library-browser">
      <div className="library-toolbar">
        <div className="subview-pills" role="tablist" aria-label="Library view">
          <button
            type="button"
            role="tab"
            aria-selected={mode === "albums"}
            className={mode === "albums" ? "active" : ""}
            onClick={() => setMode("albums")}
          >
            Albums
          </button>
          <button
            type="button"
            role="tab"
            aria-selected={mode === "songs"}
            className={mode === "songs" ? "active" : ""}
            onClick={() => setMode("songs")}
          >
            Songs
          </button>
          <button
            type="button"
            role="tab"
            aria-selected={mode === "artists"}
            className={mode === "artists" ? "active" : ""}
            onClick={() => setMode("artists")}
          >
            Artists
          </button>
          <button
            type="button"
            role="tab"
            aria-selected={mode === "rotation"}
            className={mode === "rotation" ? "active" : ""}
            onClick={() => setMode("rotation")}
          >
            Heavy Rotation
          </button>
        </div>

        <div className="source-pills" role="group" aria-label="Filter by source">
          <button
            type="button"
            className={sourceFilter === "all" ? "active" : ""}
            onClick={() => setSourceFilter("all")}
          >
            All
          </button>
          <button
            type="button"
            className={sourceFilter === "local" ? "active" : ""}
            onClick={() => setSourceFilter("local")}
          >
            Local{libraryStatus?.localAlbums ? ` (${libraryStatus.localAlbums})` : ""}
          </button>
          <button
            type="button"
            className={sourceFilter === "remote" ? "active" : ""}
            onClick={() => setSourceFilter("remote")}
          >
            Qobuz{libraryStatus?.remoteAlbums ? ` (${libraryStatus.remoteAlbums})` : ""}
          </button>
        </div>
      </div>

      <div className="search-bar library-controls">
        <input
          type="search"
          placeholder="Filter library (title, artist, album, genre)…"
          value={query}
          onChange={(e) => setQuery(e.target.value)}
        />
        {mode === "albums" && (
          <>
            <select
              aria-label="Sort albums"
              value={albumSort}
              onChange={(e) => setAlbumSort(e.target.value as AlbumSort)}
            >
              <option value="artist">Sort: Artist</option>
              <option value="title">Sort: Title</option>
              <option value="year">Sort: Year</option>
              <option value="added">Sort: Recently Added</option>
              <option value="plays">Sort: Most Played</option>
            </select>
            <label className="checkbox-pill">
              <input
                type="checkbox"
                checked={groupVersions}
                onChange={(e) => setGroupVersions(e.target.checked)}
              />
              <span>Group Versions</span>
            </label>
          </>
        )}
        {mode === "songs" && (
          <button
            type="button"
            className={showColumnBrowser ? "primary" : ""}
            onClick={() => setShowColumnBrowser((v) => !v)}
          >
            Column Browser
          </button>
        )}
        <button type="button" onClick={() => void handleSync()} disabled={isBusy}>
          {isBusy ? "Syncing…" : "Sync"}
        </button>
      </div>

      {libraryStatus && (
        <div className="library-stats muted">
          <span>
            {libraryStatus.tracks} tracks · {libraryStatus.albums} albums
          </span>
          {libraryStatus.missingTracks ? (
            <span className="warn-text">
              · {libraryStatus.missingTracks} missing track{libraryStatus.missingTracks === 1 ? "" : "s"} (soft-deleted)
            </span>
          ) : null}
        </div>
      )}

      {errMsg && (
        <div className="err-banner" role="alert">
          <span>{errMsg}</span>
        </div>
      )}

      {loading && albums.length === 0 && tracks.length === 0 && artists.length === 0 ? (
        <p className="muted">Loading library…</p>
      ) : mode === "albums" ? (
        albums.length === 0 ? (
          <p className="muted">
            {libraryStatus && !libraryStatus.configured
              ? "No local music directory or remote library configured yet."
              : "No matching albums in your library."}
          </p>
        ) : (
          <div className="album-grid">
            {albums.map((alb) => (
              <AlbumTile key={alb.id} album={alb} onOpen={(id) => void handleOpenAlbum(id)} />
            ))}
          </div>
        )
      ) : mode === "songs" ? (
        <div>
          {showColumnBrowser && (
            <div className="column-browser" aria-label="Column browser">
              <div className="cb-pane">
                <div className="cb-head">Genres ({genres.length})</div>
                <ul>
                  <li>
                    <button
                      type="button"
                      className={selectedGenre === "" ? "active" : ""}
                      onClick={() => {
                        setSelectedGenre("");
                        setSelectedArtist("");
                        setSelectedAlbumId("");
                      }}
                    >
                      All Genres
                    </button>
                  </li>
                  {genres.map((g) => (
                    <li key={g.name}>
                      <button
                        type="button"
                        className={selectedGenre === g.name ? "active" : ""}
                        onClick={() => {
                          setSelectedGenre(g.name);
                          setSelectedArtist("");
                          setSelectedAlbumId("");
                        }}
                      >
                        <span>{g.name}</span>
                        <span className="muted">{g.trackCount}</span>
                      </button>
                    </li>
                  ))}
                </ul>
              </div>
              <div className="cb-pane">
                <div className="cb-head">Artists ({artists.length})</div>
                <ul>
                  <li>
                    <button
                      type="button"
                      className={selectedArtist === "" ? "active" : ""}
                      onClick={() => {
                        setSelectedArtist("");
                        setSelectedAlbumId("");
                      }}
                    >
                      All Artists
                    </button>
                  </li>
                  {artists.map((a) => (
                    <li key={a.name}>
                      <button
                        type="button"
                        className={selectedArtist === a.name ? "active" : ""}
                        onClick={() => {
                          setSelectedArtist(a.name);
                          setSelectedAlbumId("");
                        }}
                      >
                        <span>{a.name}</span>
                        <span className="muted">{a.trackCount}</span>
                      </button>
                    </li>
                  ))}
                </ul>
              </div>
              <div className="cb-pane">
                <div className="cb-head">Albums ({albums.length})</div>
                <ul>
                  <li>
                    <button
                      type="button"
                      className={selectedAlbumId === "" ? "active" : ""}
                      onClick={() => setSelectedAlbumId("")}
                    >
                      All Albums
                    </button>
                  </li>
                  {albums.map((a) => (
                    <li key={a.id}>
                      <button
                        type="button"
                        className={selectedAlbumId === a.id ? "active" : ""}
                        onClick={() => setSelectedAlbumId(a.id)}
                      >
                        <span>{a.title}</span>
                        <span className="muted">{a.trackCount}</span>
                      </button>
                    </li>
                  ))}
                </ul>
              </div>
            </div>
          )}

          {tracks.length === 0 ? (
            <p className="muted">No matching songs in your library.</p>
          ) : (
            <div className="songs-table-wrap">
              <table className="songs-table">
                <thead>
                  <tr>
                    <th className="col-num">#</th>
                    <th>
                      <button type="button" onClick={() => toggleTrackSort("title")}>
                        Title {trackSort === "title" ? (trackDir === "asc" ? "▲" : "▼") : ""}
                      </button>
                    </th>
                    <th>
                      <button type="button" onClick={() => toggleTrackSort("artist")}>
                        Artist {trackSort === "artist" ? (trackDir === "asc" ? "▲" : "▼") : ""}
                      </button>
                    </th>
                    <th>
                      <button type="button" onClick={() => toggleTrackSort("album")}>
                        Album {trackSort === "album" ? (trackDir === "asc" ? "▲" : "▼") : ""}
                      </button>
                    </th>
                    <th>Genre</th>
                    <th>
                      <button type="button" onClick={() => toggleTrackSort("quality")}>
                        Quality {trackSort === "quality" ? (trackDir === "asc" ? "▲" : "▼") : ""}
                      </button>
                    </th>
                    <th>Source</th>
                    <th className="col-right">
                      <button type="button" onClick={() => toggleTrackSort("plays")}>
                        Plays {trackSort === "plays" ? (trackDir === "asc" ? "▲" : "▼") : ""}
                      </button>
                    </th>
                    <th className="col-right">
                      <button type="button" onClick={() => toggleTrackSort("duration")}>
                        Time {trackSort === "duration" ? (trackDir === "asc" ? "▲" : "▼") : ""}
                      </button>
                    </th>
                    <th className="col-act" />
                  </tr>
                </thead>
                <tbody>
                  {tracks.map((tr, i) => (
                    <tr
                      key={tr.id}
                      onDoubleClick={() => onPlay({ type: "tracks", trackIds: [tr.id] }, 0)}
                    >
                      <td className="col-num muted">{tr.trackNumber || i + 1}</td>
                      <td>
                        <button
                          type="button"
                          className="table-link"
                          onClick={() => onPlay({ type: "tracks", trackIds: [tr.id] }, 0)}
                        >
                          {tr.title}
                        </button>
                      </td>
                      <td className="muted">{tr.artist}</td>
                      <td>
                        {tr.albumId ? (
                          <button
                            type="button"
                            className="table-link muted"
                            onClick={() => void handleOpenAlbum(tr.albumId!)}
                          >
                            {tr.album}
                          </button>
                        ) : (
                          <span className="muted">{tr.album}</span>
                        )}
                      </td>
                      <td className="muted">{tr.genre || "—"}</td>
                      <td>
                        <span className="badge">{fmtQuality(tr) || tr.ext.replace(".", "").toUpperCase()}</span>
                      </td>
                      <td>
                        <span className={`source-pill source-${tr.provider || tr.source}`}>
                          {sourceTag(tr.provider, tr.source)}
                        </span>
                      </td>
                      <td className="col-right muted">{tr.playCount || 0}</td>
                      <td className="col-right muted">{fmtDuration(tr.durationSec)}</td>
                      <td className="col-act">
                        <button
                          type="button"
                          className="small"
                          title="Play Next"
                          onClick={() => onAdd({ type: "tracks", trackIds: [tr.id] }, "next")}
                        >
                          ⏭
                        </button>
                        <button
                          type="button"
                          className="small"
                          title="Append to Queue"
                          onClick={() => onAdd({ type: "tracks", trackIds: [tr.id] }, "end")}
                        >
                          +
                        </button>
                      </td>
                    </tr>
                  ))}
                </tbody>
              </table>
              {tracksTotal > tracks.length && (
                <p className="muted table-foot">
                  Showing {tracks.length} of {tracksTotal} tracks — narrow with search or Column Browser.
                </p>
              )}
            </div>
          )}
        </div>
      ) : mode === "artists" ? (
        <div className="artists-split">
          <div className="artists-list">
            {artists.length === 0 ? (
              <p className="muted">No artists found.</p>
            ) : (
              artists.map((a) => (
                <button
                  key={a.name}
                  type="button"
                  className={`artist-row ${detailArtist === a.name ? "active" : ""}`}
                  onClick={() => handleSelectArtistDetail(a.name)}
                >
                  <div className="artist-name">{a.name}</div>
                  <div className="artist-meta muted">
                    {a.albumCount} album{a.albumCount === 1 ? "" : "s"} · {a.trackCount} tracks
                    {a.providers.length > 0 && (
                      <span className="artist-provs">
                        {a.providers.map((p) => (
                          <span key={p} className={`source-dot source-${p}`} title={sourceTag(p, p)} />
                        ))}
                      </span>
                    )}
                  </div>
                </button>
              ))
            )}
          </div>
          <div className="artists-detail">
            {!detailArtist ? (
              <p className="muted">Select an artist to view their albums across Local and Qobuz.</p>
            ) : (
              <>
                <h3 className="subhead">{detailArtist}</h3>
                <div className="album-grid">
                  {artistAlbums.map((alb) => (
                    <AlbumTile key={alb.id} album={alb} onOpen={(id) => void handleOpenAlbum(id)} />
                  ))}
                </div>
              </>
            )}
          </div>
        </div>
      ) : (
        <div className="rotation-view">
          <h3 className="subhead">Heavy Rotation · Last 30 Days</h3>
          {rotation.length === 0 ? (
            <p className="muted">
              Albums you spin will appear here automatically once tracks play past 30 seconds.
            </p>
          ) : (
            <div className="album-grid">
              {rotation.map((alb) => (
                <AlbumTile
                  key={alb.id}
                  album={alb}
                  extraBadge={`${alb.recentPlays} play${alb.recentPlays === 1 ? "" : "s"}`}
                  onOpen={(id) => void handleOpenAlbum(id)}
                />
              ))}
            </div>
          )}

          <h3 className="subhead">Recently Added</h3>
          {recentlyAdded.length === 0 ? (
            <p className="muted">No albums added yet.</p>
          ) : (
            <div className="album-grid">
              {recentlyAdded.map((alb) => (
                <AlbumTile key={alb.id} album={alb} onOpen={(id) => void handleOpenAlbum(id)} />
              ))}
            </div>
          )}
        </div>
      )}
    </div>
  );
}

function AlbumTile({
  album,
  extraBadge,
  onOpen,
}: {
  album: Album;
  extraBadge?: string;
  onOpen: (id: string) => void;
}) {
  const qLabel = fmtAlbumQuality(album);
  const artSrc = tacetClient.resolveUrl(album.artworkUrl);
  const provider = album.provider || album.source;

  return (
    <button type="button" className="album-tile" onClick={() => onOpen(album.id)}>
      <div className="cover">
        {artSrc ? (
          <img src={artSrc} alt={album.title} loading="lazy" />
        ) : (
          <span>{album.title.slice(0, 2).toUpperCase()}</span>
        )}
        <span className={`cover-source source-${provider}`}>{sourceTag(album.provider, album.source)}</span>
        {extraBadge ? (
          <span className="cover-badge">{extraBadge}</span>
        ) : qLabel && isHiRes(album.maxBitDepth, album.maxSampleRate) ? (
          <span className="cover-badge">{qLabel}</span>
        ) : null}
        {(album.versionCount ?? 1) > 1 && (
          <span className="cover-versions">{album.versionCount}v</span>
        )}
      </div>
      <div className="meta">
        <div className="t" title={album.title}>
          {album.title}
        </div>
        <div className="a" title={album.artist}>
          {album.artist}
        </div>
      </div>
    </button>
  );
}
