import { useState } from "react";
import { tacetClient } from "../api/client";
import { fmtAlbumQuality, fmtDuration, fmtQuality, isHiRes } from "../api/format";
import type { Album, AlbumVersion, QueuePosition, QueueSource } from "../api/types";

interface AlbumViewProps {
  album: Album;
  backLabel: string;
  onBack: () => void;
  onOpenAlbum: (albumId: string) => void;
  onAlbumUpdated?: (album: Album) => void;
  onPlay: (source: QueueSource, startIndex?: number) => void;
  onAdd: (source: QueueSource, position?: QueuePosition) => void;
}

function sourceLabel(provider?: string, source?: string): string {
  const p = (provider || source || "local").toLowerCase();
  if (p === "local") return "Local";
  if (p === "qobuz") return "Qobuz";
  return p.charAt(0).toUpperCase() + p.slice(1);
}

function formatVersionQuality(v: AlbumVersion): string {
  const parts: string[] = [];
  if (v.primaryFormat) parts.push(v.primaryFormat);
  if (v.maxBitDepth && v.maxSampleRate) {
    const khz = v.maxSampleRate % 1000 === 0 ? `${v.maxSampleRate / 1000}` : (v.maxSampleRate / 1000).toFixed(1);
    parts.push(`${v.maxBitDepth}/${khz}k`);
  }
  return parts.join(" · ");
}

export function AlbumView({
  album,
  backLabel,
  onBack,
  onOpenAlbum,
  onAlbumUpdated,
  onPlay,
  onAdd,
}: AlbumViewProps) {
  const [busyLibrary, setBusyLibrary] = useState(false);
  const [busyPrimary, setBusyPrimary] = useState(false);
  const [addedTrackIds, setAddedTrackIds] = useState<Record<string, boolean>>({});
  const [actionErr, setActionErr] = useState("");

  const isRemote = album.sourceType === "remote" || album.source !== "local";
  const provider = album.provider || album.source;
  const externalId = album.externalId || album.id.replace(`${provider}:`, "");
  const artSrc = tacetClient.resolveUrl(album.artworkUrl);
  const qLabel = fmtAlbumQuality(album);
  const versions = album.versions ?? [];

  const handleToggleLibrary = async () => {
    if (!isRemote || busyLibrary) return;
    setBusyLibrary(true);
    setActionErr("");
    try {
      if (album.inLibrary) {
        await tacetClient.removeRemoteAlbum(album.id);
        onAlbumUpdated?.({
          ...album,
          inLibrary: false,
          versions: undefined,
          versionCount: 1,
        });
      } else {
        const saved = await tacetClient.addRemoteAlbum(provider, externalId);
        onAlbumUpdated?.({ ...album, ...saved, inLibrary: true });
      }
    } catch (err) {
      setActionErr(err instanceof Error ? err.message : String(err));
    } finally {
      setBusyLibrary(false);
    }
  };

  const handleMakePrimary = async () => {
    if (busyPrimary) return;
    setBusyPrimary(true);
    setActionErr("");
    try {
      const updated = await tacetClient.setPrimaryVersion(album.id);
      onAlbumUpdated?.(updated);
    } catch (err) {
      setActionErr(err instanceof Error ? err.message : String(err));
    } finally {
      setBusyPrimary(false);
    }
  };

  const handleAddTrackToLibrary = async (trackId: string, trackExtId?: string) => {
    setActionErr("");
    try {
      const ext = trackExtId || trackId.replace(`${provider}:`, "");
      await tacetClient.addRemoteTrack(provider, ext);
      setAddedTrackIds((prev) => ({ ...prev, [trackId]: true }));
    } catch (err) {
      setActionErr(err instanceof Error ? err.message : String(err));
    }
  };

  const tracks = album.tracks ?? [];

  return (
    <div className="album-detail">
      {actionErr && (
        <div className="err-banner" role="alert">
          <span>{actionErr}</span>
        </div>
      )}

      <div className="album-detail-header">
        {artSrc ? (
          <img className="album-detail-cover" src={artSrc} alt={album.title} />
        ) : (
          <div className="album-detail-cover placeholder">{album.title.slice(0, 2).toUpperCase()}</div>
        )}
        <div className="album-detail-info">
          <div className="album-detail-top">
            <button type="button" className="small back-btn" onClick={onBack}>
              ← {backLabel}
            </button>
            <span className={`source-pill source-${provider}`}>{sourceLabel(album.provider, album.source)}</span>
            {qLabel && (
              <span className={`badge ${isHiRes(album.maxBitDepth, album.maxSampleRate) ? "badge-hires" : ""}`}>
                {qLabel}
              </span>
            )}
          </div>
          <h3>{album.title}</h3>
          <p className="muted">
            {album.artist}
            {album.year ? ` · ${album.year}` : ""}
            {album.genre ? ` · ${album.genre}` : ""}
            {` · ${album.trackCount || tracks.length} tracks`}
            {album.playCount ? ` · ${album.playCount} plays` : ""}
          </p>
          <div className="row">
            <button type="button" className="primary" onClick={() => onPlay({ type: "album", albumId: album.id }, 0)}>
              ▶ Play Album
            </button>
            <button type="button" onClick={() => onAdd({ type: "album", albumId: album.id }, "next")}>
              Play Next
            </button>
            <button type="button" onClick={() => onAdd({ type: "album", albumId: album.id }, "end")}>
              + Queue
            </button>
            {isRemote && (
              <button
                type="button"
                className={album.inLibrary ? "in-library-btn" : ""}
                disabled={busyLibrary}
                onClick={() => void handleToggleLibrary()}
              >
                {busyLibrary ? "Syncing…" : album.inLibrary ? "✓ In Library" : "+ Add to Library"}
              </button>
            )}
          </div>
        </div>
      </div>

      {versions.length > 1 && (
        <div className="versions-bar" aria-label="Album versions">
          <span className="versions-label">Versions ({versions.length})</span>
          <div className="versions-list">
            {versions.map((v) => {
              const active = v.id === album.id;
              return (
                <button
                  key={v.id}
                  type="button"
                  className={`version-chip ${active ? "active" : ""}`}
                  onClick={() => {
                    if (!active) onOpenAlbum(v.id);
                  }}
                >
                  <span className={`source-dot source-${v.provider}`} />
                  <span>{sourceLabel(v.provider, v.source)}</span>
                  {formatVersionQuality(v) && <span className="muted">{formatVersionQuality(v)}</span>}
                  <span className="muted">{v.trackCount}t</span>
                  {v.isPrimary && <span className="primary-tag">Primary</span>}
                </button>
              );
            })}
            {!album.isPrimary && album.inLibrary && (
              <button
                type="button"
                className="small"
                disabled={busyPrimary}
                onClick={() => void handleMakePrimary()}
              >
                {busyPrimary ? "Saving…" : "Make Primary"}
              </button>
            )}
          </div>
        </div>
      )}

      <ul className="track-list">
        {tracks.map((tr, idx) => {
          const trackInLib = Boolean(tr.inLibrary || addedTrackIds[tr.id]);
          const trQ = fmtQuality(tr);
          return (
            <li key={tr.id} className="track-row">
              <button
                type="button"
                className="track-main"
                onClick={() => onPlay({ type: "album", albumId: album.id }, idx)}
              >
                <span className="num">{tr.trackNumber || idx + 1}</span>
                <span className="info">
                  <span className="t">{tr.title}</span>
                  <span className="s">{tr.artist}</span>
                </span>
                {trQ && <span className="badge">{trQ}</span>}
                {tr.playCount ? <span className="plays-col">{tr.playCount}▶</span> : null}
                <span className="dur">{fmtDuration(tr.durationSec)}</span>
              </button>
              <div className="track-actions">
                {isRemote && !trackInLib && (
                  <button
                    type="button"
                    className="small"
                    title="Add Track to Library"
                    onClick={() => void handleAddTrackToLibrary(tr.id, tr.externalId)}
                  >
                    + Lib
                  </button>
                )}
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
              </div>
            </li>
          );
        })}
      </ul>
    </div>
  );
}
