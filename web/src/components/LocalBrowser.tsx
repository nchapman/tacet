import { type FormEvent, useCallback, useEffect, useState } from "react";
import { tacetClient } from "../api/client";
import { fmtAlbumQuality, fmtDuration, fmtQuality, isHiRes } from "../api/format";
import type { Album, QueueTrackRequest, Track } from "../api/types";

interface LocalBrowserProps {
  onPlay: (req: QueueTrackRequest) => void;
  onEnqueue: (req: QueueTrackRequest) => void;
}

type LocalView =
  | { kind: "albums"; albums: Album[]; roots: string[] }
  | { kind: "album"; album: Album }
  | { kind: "search"; query: string; tracks: Track[] };

export function LocalBrowser({ onPlay, onEnqueue }: LocalBrowserProps) {
  const [query, setQuery] = useState("");
  const [view, setView] = useState<LocalView>({ kind: "albums", albums: [], roots: [] });
  const [loading, setLoading] = useState(true);
  const [rescanning, setRescanning] = useState(false);
  const [errMsg, setErrMsg] = useState("");

  const loadAlbums = useCallback(async () => {
    setLoading(true);
    setErrMsg("");
    try {
      const res = await tacetClient.listLocalAlbums();
      setView({ kind: "albums", albums: res.albums ?? [], roots: res.roots ?? [] });
    } catch (err) {
      setErrMsg(err instanceof Error ? err.message : String(err));
    } finally {
      setLoading(false);
    }
  }, []);

  useEffect(() => {
    void loadAlbums();
  }, [loadAlbums]);

  const handleSearch = async (e: FormEvent) => {
    e.preventDefault();
    const trimmed = query.trim();
    if (!trimmed) {
      void loadAlbums();
      return;
    }
    setLoading(true);
    setErrMsg("");
    try {
      const res = await tacetClient.searchLocalTracks(trimmed);
      setView({ kind: "search", query: trimmed, tracks: res.tracks ?? [] });
    } catch (err) {
      setErrMsg(err instanceof Error ? err.message : String(err));
    } finally {
      setLoading(false);
    }
  };

  const openAlbum = async (id: string) => {
    setLoading(true);
    setErrMsg("");
    try {
      const alb = await tacetClient.getLocalAlbum(id);
      setView({ kind: "album", album: alb });
    } catch (err) {
      setErrMsg(err instanceof Error ? err.message : String(err));
    } finally {
      setLoading(false);
    }
  };

  const handleRescan = async () => {
    setRescanning(true);
    setErrMsg("");
    try {
      await tacetClient.rescanLibrary();
      await loadAlbums();
    } catch (err) {
      setErrMsg(err instanceof Error ? err.message : String(err));
    } finally {
      setRescanning(false);
    }
  };

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
        <button type="button" onClick={() => void handleRescan()} disabled={rescanning}>
          {rescanning ? "Scanning…" : "Rescan"}
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
            {view.roots.length === 0
              ? "No local music directory configured. Start tacet with --music-dir /path/to/music."
              : `No supported audio files found in ${view.roots.join(", ")}.`}
          </p>
        ) : (
          <div className="album-grid">
            {view.albums.map((alb) => {
              const qLabel = fmtAlbumQuality(alb);
              return (
                <button key={alb.id} type="button" className="album-tile" onClick={() => void openAlbum(alb.id)}>
                  <div className="cover">
                    {alb.artworkUrl ? (
                      <img src={alb.artworkUrl} alt={alb.title} loading="lazy" />
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
              <img className="album-detail-cover" src={view.album.artworkUrl} alt={view.album.title} />
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
                  onClick={() => onPlay({ localAlbumId: view.album.id, startIndex: 0 })}
                >
                  ▶ Play Album
                </button>
                <button type="button" onClick={() => onEnqueue({ localAlbumId: view.album.id })}>
                  + Queue Album
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
                  onClick={() => onPlay({ localAlbumId: view.album.id, startIndex: idx })}
                >
                  <div className="track-title">{t.title}</div>
                  <div className="track-meta">{t.artist}</div>
                </button>
                <span className={`track-qual ${isHiRes(t.bitDepth, t.sampleRate) ? "hi" : ""}`}>{fmtQuality(t)}</span>
                <span className="track-dur mono">{fmtDuration(t.durationSec)}</span>
                <button
                  type="button"
                  className="small"
                  onClick={() => onEnqueue({ localTrackId: t.id })}
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
              {view.tracks.length} result{view.tracks.length === 1 ? "" : "s"} for “{view.query}”
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
                  <button type="button" className="track-info-btn" onClick={() => onPlay({ localTrackId: t.id })}>
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
                    onClick={() => onEnqueue({ localTrackId: t.id })}
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
