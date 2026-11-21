import { useState } from "react";
import { tacetClient } from "../api/client";
import { fmtDuration, fmtQuality, isHiRes, streamModeLabel } from "../api/format";
import type { StreamMode, TransportAction, ZoneSnapshot } from "../api/types";

interface NowPlayingProps {
  zone: ZoneSnapshot | null;
  positionSec: number;
  streamMode: StreamMode;
  error: string;
  onDismissError: () => void;
  onTransport: (action: TransportAction) => void;
  onSeek: (positionSec: number) => void;
  onVolume: (volume: number) => void;
  onMute: (muted: boolean) => void;
}

export function NowPlaying({
  zone,
  positionSec,
  streamMode,
  error,
  onDismissError,
  onTransport,
  onSeek,
  onVolume,
  onMute,
}: NowPlayingProps) {
  const [seekDraft, setSeekDraft] = useState<number | null>(null);
  const [volDraft, setVolDraft] = useState<number | null>(null);

  const track = zone?.currentTrack;
  const duration = zone?.durationSec || track?.durationSec || 0;
  const position = seekDraft ?? positionSec;
  const volume = volDraft ?? zone?.volume ?? 0;
  const muted = zone?.muted ?? false;
  const hasVolume = zone?.renderer.hasVolume ?? false;
  const supportsSeek = zone?.renderer.supportsSeek ?? false;
  const quality = fmtQuality(track);
  const hiRes = isHiRes(track?.bitDepth, track?.sampleRate);
  const state = zone?.state ?? "idle";
  const isPlaying = state === "playing";
  const artSrc = tacetClient.resolveUrl(track?.artworkUrl);

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
        <span className={`badge ${isPlaying ? "badge-ok" : state === "buffering" ? "badge-hi" : ""}`}>
          {state.toUpperCase()}
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
        {artSrc ? (
          <img src={artSrc} alt={`${track?.album || track?.title || "Track"} cover art`} />
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
        {track?.source === "qobuz" && <span className="badge">{streamModeLabel(streamMode)}</span>}
        {zone?.gaplessArmed && <span className="badge badge-ok">Gapless Armed</span>}
        {zone && (
          <span
            className={`badge ${zone.eventsActive ? "badge-ok" : ""}`}
            title={
              zone.renderer.kind === "player"
                ? zone.eventsActive
                  ? "Connected remote audio player endpoint"
                  : "Remote audio player reconnecting"
                : zone.eventsActive
                  ? "Receiving live UPnP GENA push events"
                  : "Polling renderer state"
            }
          >
            {zone.renderer.kind === "player"
              ? zone.eventsActive
                ? "Player Endpoint"
                : "Reconnecting"
              : zone.eventsActive
                ? "GENA Push"
                : "Polling"}
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
          disabled={!track || !supportsSeek}
          onPointerDown={() => setSeekDraft(positionSec)}
          onChange={(e) => setSeekDraft(Number(e.target.value))}
          onPointerUp={(e) => commitSeek(Number(e.currentTarget.value))}
          onKeyUp={(e) => {
            if (e.key.startsWith("Arrow")) commitSeek(Number(e.currentTarget.value));
          }}
        />
        <span className="mono">{fmtDuration(duration)}</span>
      </div>

      <div className="controls">
        <button type="button" onClick={() => onTransport("prev")} disabled={!zone} title="Previous track" aria-label="Previous">
          ⏮
        </button>
        <button
          type="button"
          className="primary play-btn"
          onClick={() => onTransport(isPlaying ? "pause" : "play")}
          disabled={!zone}
          title={isPlaying ? "Pause (Space)" : "Play (Space)"}
          aria-label={isPlaying ? "Pause" : "Play"}
        >
          {isPlaying ? "⏸" : "▶"}
        </button>
        <button type="button" onClick={() => onTransport("stop")} disabled={!zone} title="Stop" aria-label="Stop">
          ⏹
        </button>
        <button type="button" onClick={() => onTransport("next")} disabled={!zone} title="Next track" aria-label="Next">
          ⏭
        </button>
      </div>

      <div className="vol-row">
        <button
          type="button"
          className="small"
          onClick={() => onMute(!muted)}
          title={muted ? "Unmute" : "Mute"}
          aria-label={muted ? "Unmute" : "Mute"}
          disabled={!zone || !hasVolume}
        >
          {muted ? "🔇" : "🔊"}
        </button>
        <input
          aria-label="Volume"
          type="range"
          min={0}
          max={100}
          value={volume}
          disabled={!zone || !hasVolume}
          onPointerDown={() => setVolDraft(zone?.volume ?? 0)}
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
