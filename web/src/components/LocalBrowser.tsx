import { type FormEvent, useCallback, useEffect, useRef, useState } from "react";
import { tacetClient } from "../api/client";
import { fmtAlbumQuality, fmtDuration, fmtQuality, isHiRes } from "../api/format";
import type { Album, LibraryStatus, QueuePosition, QueueSource, Track } from "../api/types";

interface LocalBrowserProps {
  libraryStatus: LibraryStatus | null;
  onPlay: (source: QueueSource, startIndex?: number) => void;
  onAdd: (source: QueueSource, position?: QueuePosition) => void;
}

type LocalView =
  | { kind: "albums"; albums: Album[]; total: number }
  | { kind: "album"; album: Album }
  | { kind: "search"; query: string; tracks: Track[]; total: number };

export function LocalBrowser({ libraryStatus, onPlay, onAdd }: LocalBrowserProps) {
  const [query, setQuery] = useState("");
  const [view, setView] = useState<LocalView>({ kind: "albums", albums: [], total: 0 });
  const [loading, setLoading] = useState(true);
  const [rescanning, setRescanning] = useState(false);
  const [errMsg, setErrMsg] = useState("");
  const abortRef = useRef<AbortController | null>(null);

  const startRequest = () => {
    abortRef.current?.abort();
    const ctrl = new AbortController();
    abortRef.current = ctrl;
    return ctrl.signal;
  };

  const loadAlbums = useCallback(async () => {
    const signal = startRequest();
    setLoading(true);
    setErrMsg("");
    try {
      const res = await tacetClient.listLocalAlbums({ limit: 500 }, { signal });
      if (signal.aborted) return;
      setView({ kind: "albums", albums: res.items ?? [], total: res.total ?? 0 });
    } catch (err) {
      if (signal.aborted) return;
      setErrMsg(err instanceof Error ? err.message : String(err));
    } finally {
      if (!signal.aborted) setLoading(false);
    }
  }, []);

  // Load initially and refresh automatically whenever SSE reports a new catalog ETag.
  useEffect(() => {
    void loadAlbums();
    return () => abortRef.current?.abort();
  }, [loadAlbums, libraryStatus?.etag]);

  const handleSearch = async (e: FormEvent) => {
    e.preventDefault();
    const trimmed = query.trim();
    if (!trimmed) {
      void loadAlbums();
      return;
    }
    const signal = startRequest();
    setLoading(true);
    setErrMsg("");
    try {
      const res = await tacetClient.listLocalTracks({ q: trimmed, limit: 100 }, { signal });
      if (signal.aborted) return;
      setView({ kind: "search", query: trimmed, tracks: res.items ?? [], total: res.total ?? 0 });
    } catch (err) {
      if (signal.aborted) return;
      setErrMsg(err instanceof Error ? err.message : String(err));
    } finally {
      if (!signal.aborted) setLoading(false);
    }
  };

  const openAlbum = async (id: string) => {
    const signal = startRequest();
    setLoading(true);
    setErrMsg("");
    try {
      const alb = await tacetClient.getLocalAlbum(id, { signal });
      if (signal.aborted) return;
      setView({ kind: "album", album: alb });
    } catch (err) {
      if (signal.aborted) return;
      setErrMsg(err instanceof Error ? err.message : String(err));
    } finally {
      if (!signal.aborted) setLoading(false);
    }
  };

  const handleRescan = async () => {
    setRescanning(true);
    setErrMsg("");
    try {
      await tacetClient.rescanLibrary(true);
      await loadAlbums();
    } catch (err) {
      setErrMsg(err instanceof Error ? err.message : String(err));
    } finally {
      setRescanning(false);
    }
  };

  const isScanning = rescanning || Boolean(libraryStatus?.scanning);

  return (
    <div>
      <form className="search-bar" onSubmit={handleSearch}>
        <input
          type="search"
          placeholder="Search local NAS library…"
          value={query}
          onChange={(e) => setQuery(e.target.value)}
        />
        <button type="submit">Search</button>
        <button type="button" onClick={() => void loadAlbums()}>
          Albums
        </button>
        <button type="button" onClick={() => void handleRescan()} disabled={isScanning}>
          {isScanning ? "Scanning…" : "Rescan"}
        </button>
      </form>

      {errMsg && (
        <div className="err-banner" role="alert">
          <span>{errMsg}</span>
        </div>
      )}

      {loading ? (
        <p className="muted">Loading library…</p>
      ) : view.kind === "albums" ? (
        view.albums.length === 0 ? (
          <p className="muted">
            {libraryStatus && !libraryStatus.configured
              ? "No local music directory configured. Start tacet with --music-dir /path/to/music."
              : "No supported audio files indexed yet."}
          </p>
        ) : (
          <div className="album-grid">
            {view.albums.map((alb) => {
              const qLabel = fmtAlbumQuality(alb);
              const artSrc = tacetClient.resolveUrl(alb.artworkUrl);
              return (
                <button key={alb.id} type="button" className="album-tile" onClick={() => void openAlbum(alb.id)}>
                  <div className="cover">
                    {artSrc ? (
                      <img src={artSrc} alt={alb.title} loading="lazy" />
                    ) : (
                      <span>{alb.title.slice(0, 2).toUpperCase()}</span>
                    )}
                    {qLabel && isHiRes(alb.maxBitDepth, alb.maxSampleRate) && (
                      <span className="cover-badge">{qLabel}</span>
                    )}
                  </div>
                  <div className="meta">
                    <div className="t" title={alb.title}>
                      {alb.title}
                    </div>
                    <div className="a" title={alb.artist}>
                      {alb.artist}
                    </div>
                  </div>
                </button>
              );
            })}
          </div>
        )
      ) : view.kind === "album" ? (
        <div className="album-detail">
          <div className="album-detail-header">
            {view.album.artworkUrl && (
              <img
                className="album-detail-cover"
                src={tacetClient.resolveUrl(view.album.artworkUrl)}
                alt={view.album.title}
              />
            )}
            <div className="album-detail-info">
              <button type="button" className="small back-btn" onClick={() => void loadAlbums()}>
                ← Albums
              </button>
              <h3>{view.album.title}</h3>
              <p className="muted">
                {view.album.artist}
                {view.album.year ? ` · ${view.album.year}` : ""} · {view.album.trackCount} tracks
                {fmtAlbumQuality(view.album) ? ` · ${fmtAlbumQuality(view.album)}` : ""}
              </p>
              <div className="row">
                <button
                  type="button"
                  className="primary"
                  onClick={() => onPlay({ type: "album", albumId: view.album.id }, 0)}
                >
                  ▶ Play Album
                </button>
                <button type="button" onClick={() => onAdd({ type: "album", albumId: view.album.id }, "next")}>
                  Play Next
                </button>
                <button type="button" onClick={() => onAdd({ type: "album", albumId: view.album.id }, "end")}>
                  + Queue
                </button>
              </div>
            </div>
          </div>

          <ul className="track-list">
            {(view.album.tracks ?? []).map((t, idx) => (
              <li key={t.id} className="track-row">
                <span className="track-num mono">{t.trackNumber || idx + 1}</span>
                <button
                  type="button"
                  className="track-info-btn"
                  onClick={() => onPlay({ type: "album", albumId: view.album.id }, idx)}
                >
                  <div className="track-title">{t.title}</div>
                  <div className="track-meta">{t.artist}</div>
                </button>
                <span className={`track-qual ${isHiRes(t.bitDepth, t.sampleRate) ? "hi" : ""}`}>{fmtQuality(t)}</span>
                <span className="track-dur mono">{fmtDuration(t.durationSec)}</span>
                <button
                  type="button"
                  className="small"
                  onClick={() => onAdd({ type: "tracks", trackIds: [t.id] }, "end")}
                  title="Add to queue"
                >
                  +
                </button>
              </li>
            ))}
          </ul>
        </div>
      ) : (
        <div>
          <div className="section-bar">
            <span className="muted">
              {view.total} result{view.total === 1 ? "" : "s"} for “{view.query}”
            </span>
            <button type="button" className="small" onClick={() => void loadAlbums()}>
              Back to Albums
            </button>
          </div>
          {view.tracks.length === 0 ? (
            <p className="muted">No matching local tracks.</p>
          ) : (
            <ul className="track-list">
              {view.tracks.map((t) => (
                <li key={t.id} className="track-row">
                  <button
                    type="button"
                    className="track-info-btn"
                    onClick={() => onPlay({ type: "tracks", trackIds: [t.id] }, 0)}
                  >
                    <div className="track-title">{t.title}</div>
                    <div className="track-meta">
                      {t.artist}
                      {t.album ? ` — ${t.album}` : ""}
                    </div>
                  </button>
                  <span className={`track-qual ${isHiRes(t.bitDepth, t.sampleRate) ? "hi" : ""}`}>{fmtQuality(t)}</span>
                  <span className="track-dur mono">{fmtDuration(t.durationSec)}</span>
                  <button
                    type="button"
                    className="small"
                    onClick={() => onAdd({ type: "tracks", trackIds: [t.id] }, "end")}
                    title="Add to queue"
                  >
                    +
                  </button>
                </li>
              ))}
            </ul>
          )}
        </div>
      )}
    </div>
  );
}
