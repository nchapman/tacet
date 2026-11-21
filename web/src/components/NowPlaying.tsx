import { useState } from "react";
import { fmtDuration, fmtQuality, isHiRes, streamModeLabel } from "../api/format";
import type { TransportAction, ZoneSnapshot } from "../api/types";

interface NowPlayingProps {
  snapshot: ZoneSnapshot;
  error: string;
  onDismissError: () => void;
  onTransport: (action: TransportAction) => void;
  onSeek: (positionSec: number) => void;
  onVolume: (volume: number) => void;
  onMute: (muted: boolean) => void;
}

export function NowPlaying({
  snapshot,
  error,
  onDismissError,
  onTransport,
  onSeek,
  onVolume,
  onMute,
}: NowPlayingProps) {
  const [seekDraft, setSeekDraft] = useState<number | null>(null);
  const [volDraft, setVolDraft] = useState<number | null>(null);

  const track = snapshot.currentTrack;
  const duration = snapshot.durationSec || track?.durationSec || 0;
  const position = seekDraft ?? snapshot.positionSec ?? 0;
  const volume = volDraft ?? snapshot.volume ?? 50;
  const quality = fmtQuality(track);
  const hiRes = isHiRes(track?.bitDepth, track?.sampleRate);
  const isPlaying = snapshot.state === "PLAYING";

  const commitSeek = (val: number) => {
    setSeekDraft(null);
    onSeek(val);
  };

  const commitVolume = (val: number) => {
    setVolDraft(null);
    onVolume(val);
  };

  return (
    <section className="card now-playing">
      <div className="card-header">
        <h2>Now Playing</h2>
        <span className={`badge ${isPlaying ? "badge-ok" : snapshot.state === "TRANSITIONING" ? "badge-hi" : ""}`}>
          {snapshot.state || "IDLE"}
        </span>
      </div>

      {error && (
        <div className="err-banner" role="alert">
          <span>{error}</span>
          <button type="button" className="icon-dismiss" onClick={onDismissError} aria-label="Dismiss error">
            ✕
          </button>
        </div>
      )}

      <div className="np-art">
        {track?.artworkUrl ? (
          <img src={track.artworkUrl} alt={`${track.album || track.title} cover art`} />
        ) : (
          <div className="np-art-empty">{track ? "No artwork" : "No track loaded"}</div>
        )}
      </div>

      <div className="np-meta">
        <div className="np-title" title={track?.title}>
          {track ? track.title : "—"}
        </div>
        <div className="np-sub" title={track ? `${track.artist} — ${track.album}` : ""}>
          {track ? `${track.artist}${track.album ? ` — ${track.album}` : ""}` : "Select a track from NAS or Qobuz"}
        </div>
      </div>

      <div className="badges">
        {track && <span className="badge">{track.source === "local" ? "NAS" : "QOBUZ"}</span>}
        {quality && <span className={`badge ${hiRes ? "badge-hi" : ""}`}>{quality}</span>}
        {track?.source === "qobuz" && <span className="badge">{streamModeLabel(snapshot.streamMode)}</span>}
        {snapshot.gaplessArmed && <span className="badge badge-ok">Gapless Armed</span>}
        {snapshot.renderer && (
          <span
            className={`badge ${snapshot.eventsActive ? "badge-ok" : ""}`}
            title={snapshot.eventsActive ? "Receiving live UPnP GENA push events" : "Polling renderer state"}
          >
            {snapshot.eventsActive ? "GENA Push" : "Polling"}
          </span>
        )}
      </div>

      <div className="seek-row">
        <span className="mono">{fmtDuration(position)}</span>
        <input
          aria-label="Seek position"
          type="range"
          min={0}
          max={Math.max(1, duration)}
          value={Math.min(position, Math.max(1, duration))}
          disabled={!track || (snapshot.renderer !== null && !snapshot.renderer.supportsSeek)}
          onPointerDown={() => setSeekDraft(snapshot.positionSec)}
          onChange={(e) => setSeekDraft(Number(e.target.value))}
          onPointerUp={(e) => commitSeek(Number(e.currentTarget.value))}
          onKeyUp={(e) => {
            if (e.key.startsWith("Arrow")) commitSeek(Number(e.currentTarget.value));
          }}
        />
        <span className="mono">{fmtDuration(duration)}</span>
      </div>

      <div className="controls">
        <button type="button" onClick={() => onTransport("prev")} title="Previous track" aria-label="Previous">
          ⏮
        </button>
        <button
          type="button"
          className="primary play-btn"
          onClick={() => onTransport(isPlaying ? "pause" : "play")}
          title={isPlaying ? "Pause (Space)" : "Play (Space)"}
          aria-label={isPlaying ? "Pause" : "Play"}
        >
          {isPlaying ? "⏸" : "▶"}
        </button>
        <button type="button" onClick={() => onTransport("stop")} title="Stop" aria-label="Stop">
          ⏹
        </button>
        <button type="button" onClick={() => onTransport("next")} title="Next track" aria-label="Next">
          ⏭
        </button>
      </div>

      <div className="vol-row">
        <button
          type="button"
          className="small"
          onClick={() => onMute(!snapshot.muted)}
          title={snapshot.muted ? "Unmute" : "Mute"}
          aria-label={snapshot.muted ? "Unmute" : "Mute"}
          disabled={snapshot.renderer !== null && !snapshot.hasVolume}
        >
          {snapshot.muted ? "🔇" : "🔊"}
        </button>
        <input
          aria-label="Volume"
          type="range"
          min={0}
          max={100}
          value={volume}
          disabled={snapshot.renderer !== null && !snapshot.hasVolume}
          onPointerDown={() => setVolDraft(snapshot.volume)}
          onChange={(e) => setVolDraft(Number(e.target.value))}
          onPointerUp={(e) => commitVolume(Number(e.currentTarget.value))}
          onKeyUp={(e) => {
            if (e.key.startsWith("Arrow")) commitVolume(Number(e.currentTarget.value));
          }}
        />
        <span className="mono vol-label">{volume}%</span>
      </div>
    </section>
  );
}
