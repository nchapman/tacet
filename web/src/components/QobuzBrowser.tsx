import { type FormEvent, useEffect, useRef, useState } from "react";
import { tacetClient } from "../api/client";
import { fmtAlbumQuality, fmtDuration, fmtQuality, isHiRes } from "../api/format";
import type { Album, QobuzSearchResult, QobuzStatus, QueuePosition, QueueSource } from "../api/types";
import { AlbumView } from "./AlbumView";

interface QobuzBrowserProps {
  status: QobuzStatus | null;
  onStatusChange: (status: QobuzStatus) => void;
  onPlay: (source: QueueSource, startIndex?: number) => void;
  onAdd: (source: QueueSource, position?: QueuePosition) => void;
}

type QobuzView =
  | { kind: "empty" }
  | { kind: "results"; query: string; data: QobuzSearchResult }
  | { kind: "album"; album: Album; previous?: { query: string; data: QobuzSearchResult } };

export function QobuzBrowser({ status, onStatusChange, onPlay, onAdd }: QobuzBrowserProps) {
  const [authToken, setAuthToken] = useState("");
  const [appId, setAppId] = useState("");
  const [appSecret, setAppSecret] = useState("");
  const [showAdvanced, setShowAdvanced] = useState(false);
  const [loggingIn, setLoggingIn] = useState(false);

  const [query, setQuery] = useState("");
  const [view, setView] = useState<QobuzView>({ kind: "empty" });
  const [searching, setSearching] = useState(false);
  const [syncingFavs, setSyncingFavs] = useState(false);
  const [addedTrackIds, setAddedTrackIds] = useState<Record<string, boolean>>({});
  const [errMsg, setErrMsg] = useState("");
  const abortRef = useRef<AbortController | null>(null);

  const startRequest = () => {
    abortRef.current?.abort();
    const ctrl = new AbortController();
    abortRef.current = ctrl;
    return ctrl.signal;
  };

  useEffect(() => {
    if (status !== null) return;
    const ctrl = new AbortController();
    void tacetClient
      .qobuzStatus({ signal: ctrl.signal })
      .then(onStatusChange)
      .catch((err) => {
        if (!ctrl.signal.aborted) setErrMsg(err instanceof Error ? err.message : String(err));
      });
    return () => ctrl.abort();
  }, [status, onStatusChange]);

  useEffect(() => () => abortRef.current?.abort(), []);

  const handleTokenLogin = async (e: FormEvent) => {
    e.preventDefault();
    setLoggingIn(true);
    setErrMsg("");
    try {
      const st = await tacetClient.qobuzLogin({
        authToken: authToken.trim() || undefined,
        appId: appId.trim() || undefined,
        appSecret: appSecret.trim() || undefined,
      });
      onStatusChange(st);
      setAuthToken("");
      setAppId("");
      setAppSecret("");
    } catch (err) {
      setErrMsg(err instanceof Error ? err.message : String(err));
    } finally {
      setLoggingIn(false);
    }
  };

  const handleLogout = async () => {
    setErrMsg("");
    try {
      const st = await tacetClient.qobuzLogout();
      onStatusChange(st);
      setView({ kind: "empty" });
    } catch (err) {
      setErrMsg(err instanceof Error ? err.message : String(err));
    }
  };

  const handleSyncFavorites = async () => {
    setSyncingFavs(true);
    setErrMsg("");
    try {
      await tacetClient.syncLibrary("qobuz", false);
    } catch (err) {
      setErrMsg(err instanceof Error ? err.message : String(err));
    } finally {
      setSyncingFavs(false);
    }
  };

  const handleAddTrack = async (trackId: string, externalId?: string) => {
    setErrMsg("");
    try {
      await tacetClient.addRemoteTrack("qobuz", externalId || trackId.replace("qobuz:", ""));
      setAddedTrackIds((prev) => ({ ...prev, [trackId]: true }));
    } catch (err) {
      setErrMsg(err instanceof Error ? err.message : String(err));
    }
  };

  const handleSearch = async (e: FormEvent) => {
    e.preventDefault();
    const trimmed = query.trim();
    if (!trimmed) return;
    const signal = startRequest();
    setSearching(true);
    setErrMsg("");
    try {
      const raw = await tacetClient.qobuzSearch(trimmed, { limit: 30 }, { signal });
      if (signal.aborted) return;
      const data: QobuzSearchResult = {
        albums: Array.isArray(raw?.albums) ? raw.albums : [],
        tracks: Array.isArray(raw?.tracks) ? raw.tracks : [],
      };
      setView({ kind: "results", query: trimmed, data });
    } catch (err) {
      if (signal.aborted) return;
      setErrMsg(err instanceof Error ? err.message : String(err));
    } finally {
      if (!signal.aborted) setSearching(false);
    }
  };

  const openAlbum = async (albumId: string) => {
    const previous =
      view.kind === "results"
        ? { query: view.query, data: view.data }
        : view.kind === "album"
          ? view.previous
          : undefined;
    const signal = startRequest();
    setSearching(true);
    setErrMsg("");
    try {
      const album = albumId.startsWith("local:")
        ? await tacetClient.getLibraryAlbum(albumId, { signal })
        : await tacetClient.qobuzAlbum(albumId, { signal });
      if (signal.aborted) return;
      setView({ kind: "album", album, previous });
    } catch (err) {
      if (signal.aborted) return;
      setErrMsg(err instanceof Error ? err.message : String(err));
    } finally {
      if (!signal.aborted) setSearching(false);
    }
  };

  return (
    <div>
      {errMsg && (
        <div className="err-banner" role="alert">
          <span>{errMsg}</span>
        </div>
      )}

      {status === null ? (
        <p className="muted">Checking Qobuz status…</p>
      ) : !status.authenticated ? (
        <div className="auth-card">
          <h3>Connect Qobuz</h3>
          <p className="muted">
            Sign in with Qobuz to browse and stream bit-perfect hi-res FLAC (up to 24-bit / 192 kHz) via Direct
            Handoff or Quiet Relay.
          </p>
          <div className="row">
            <a className="button-link primary" href={tacetClient.qobuzOAuthStartURL()}>
              Sign In with Qobuz (Browser OAuth)
            </a>
            <button type="button" onClick={() => setShowAdvanced((v) => !v)}>
              {showAdvanced ? "Hide Token Login" : "Use Auth Token"}
            </button>
          </div>

          {showAdvanced && (
            <form className="form-stack advanced-form" onSubmit={handleTokenLogin}>
              <input
                type="password"
                autoComplete="off"
                placeholder="Qobuz user_auth_token"
                value={authToken}
                onChange={(e) => setAuthToken(e.target.value)}
                required
              />
              <div className="row">
                <input
                  placeholder="Optional App ID"
                  value={appId}
                  onChange={(e) => setAppId(e.target.value)}
                />
                <input
                  type="password"
                  autoComplete="off"
                  placeholder="Optional App Secret"
                  value={appSecret}
                  onChange={(e) => setAppSecret(e.target.value)}
                />
              </div>
              <div>
                <button type="submit" className="primary" disabled={loggingIn || !authToken.trim()}>
                  {loggingIn ? "Verifying…" : "Connect with Token"}
                </button>
              </div>
            </form>
          )}
        </div>
      ) : (
        <div>
          <div className="section-bar qobuz-bar">
            <span className="badge badge-ok">
              Qobuz Connected: {status.userName || "Account"}
              {status.subscription ? ` (${status.subscription})` : ""}
            </span>
            <div className="row">
              <button
                type="button"
                className="small"
                disabled={syncingFavs}
                onClick={() => void handleSyncFavorites()}
              >
                {syncingFavs ? "Syncing…" : "Sync Favorites"}
              </button>
              <button type="button" className="small" onClick={() => void handleLogout()}>
                Sign Out
              </button>
            </div>
          </div>

          <form className="search-bar" onSubmit={handleSearch}>
            <input
              type="search"
              placeholder="Search Qobuz catalog (artists, albums, tracks)…"
              value={query}
              onChange={(e) => setQuery(e.target.value)}
            />
            <button type="submit" className="primary" disabled={searching}>
              {searching ? "Searching…" : "Search"}
            </button>
          </form>

          {searching ? (
            <p className="muted">Loading from Qobuz…</p>
          ) : view.kind === "empty" ? (
            <p className="muted">Search Qobuz to browse lossless & hi-res albums and tracks.</p>
          ) : view.kind === "album" ? (
            <AlbumView
              album={view.album}
              backLabel={view.previous ? "Results" : "Qobuz"}
              onBack={() =>
                setView(
                  view.previous
                    ? { kind: "results", query: view.previous.query, data: view.previous.data }
                    : { kind: "empty" },
                )
              }
              onOpenAlbum={(id) => void openAlbum(id)}
              onAlbumUpdated={(updated) => setView({ ...view, album: updated })}
              onPlay={onPlay}
              onAdd={onAdd}
            />
          ) : (
            <div className="search-results">
              {view.data.albums.length > 0 && (
                <div>
                  <h3 className="subsection-title">Albums</h3>
                  <div className="album-grid">
                    {view.data.albums.map((alb) => {
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
                            {alb.inLibrary && <span className="cover-source source-qobuz">✓ Library</span>}
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
                </div>
              )}

              {view.data.tracks.length > 0 && (
                <div>
                  <h3 className="subsection-title">Tracks</h3>
                  <ul className="track-list">
                    {view.data.tracks.map((t) => {
                      const inLib = Boolean(t.inLibrary || addedTrackIds[t.id]);
                      return (
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
                          <span className={`track-qual ${isHiRes(t.bitDepth, t.sampleRate) ? "hi" : ""}`}>
                            {fmtQuality(t)}
                          </span>
                          <span className="track-dur mono">{fmtDuration(t.durationSec)}</span>
                          {!inLib && (
                            <button
                              type="button"
                              className="small"
                              title="Add to Library"
                              onClick={() => void handleAddTrack(t.id, t.externalId)}
                            >
                              + Lib
                            </button>
                          )}
                          <button
                            type="button"
                            className="small"
                            onClick={() => onAdd({ type: "tracks", trackIds: [t.id] }, "end")}
                            title="Add to queue"
                          >
                            +
                          </button>
                        </li>
                      );
                    })}
                  </ul>
                </div>
              )}

              {view.data.albums.length === 0 && view.data.tracks.length === 0 && (
                <p className="muted">No Qobuz matches for “{view.query}”.</p>
              )}
            </div>
          )}
        </div>
      )}
    </div>
  );
}
