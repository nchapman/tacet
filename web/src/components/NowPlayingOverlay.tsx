import {
  ChevronDown,
  ListMusic,
  Pause,
  Play,
  Repeat,
  Repeat1,
  Shuffle,
  SkipBack,
  SkipForward,
  Volume2,
  VolumeX,
} from "lucide-react";
import { useEffect, useState } from "react";
import { fmtDuration } from "@/api/format";
import type { RepeatMode, StreamMode, TransportAction, ZoneSnapshot } from "@/api/types";
import { SignalPathPopover } from "@/components/SignalPathPopover";
import { AlbumArt, AmbientBackdrop } from "@/components/ui/artwork";
import { Button } from "@/components/ui/button";
import { PlayingBars } from "@/components/ui/PlayingBars";
import { Slider } from "@/components/ui/slider";
import { cn } from "@/lib/utils";

interface NowPlayingOverlayProps {
  open: boolean;
  onClose: () => void;
  activeZone: ZoneSnapshot | null;
  activeDeviceName: string;
  isBrowserActive: boolean;
  streamMode?: StreamMode;
  qualLabel: string;
  shownPos: number;
  duration: number;
  shownVol: number;
  onScrub: (pos: number | null) => void;
  onSeek: (sec: number) => void;
  onVolDraft: (vol: number | null) => void;
  onVolume: (vol: number) => void;
  onMute: (muted: boolean) => void;
  onTransport: (action: TransportAction) => void;
  onShuffle: (enabled: boolean) => void;
  onRepeat: (mode: RepeatMode) => void;
  onJumpItem?: (itemId: string) => void;
  onOpenAlbum?: (albumId: string) => void;
  onOpenArtist?: (artist: string) => void;
}

export function NowPlayingOverlay({
  open,
  onClose,
  activeZone,
  activeDeviceName,
  isBrowserActive,
  streamMode,
  qualLabel,
  shownPos,
  duration,
  shownVol,
  onScrub,
  onSeek,
  onVolDraft,
  onVolume,
  onMute,
  onTransport,
  onShuffle,
  onRepeat,
  onJumpItem,
  onOpenAlbum,
  onOpenArtist,
}: NowPlayingOverlayProps) {
  const [showQueue, setShowQueue] = useState(false);

  useEffect(() => {
    if (!open) return;
    const onKeyDown = (e: KeyboardEvent) => {
      if (e.key === "Escape") {
        e.preventDefault();
        onClose();
      }
    };
    window.addEventListener("keydown", onKeyDown);
    return () => window.removeEventListener("keydown", onKeyDown);
  }, [open, onClose]);

  if (!open) return null;

  const track = activeZone?.currentTrack ?? null;
  const playing = activeZone?.state === "playing";
  const hasZone = activeZone !== null;
  const hasPlayable =
    hasZone &&
    Boolean(track || (activeZone?.queue?.length ?? 0) > 0 || playing || activeZone?.state === "paused");
  const shuffle = Boolean(activeZone?.shuffle);
  const repeat: RepeatMode = activeZone?.repeat ?? "off";
  const nextRepeatMode: RepeatMode = repeat === "off" ? "all" : repeat === "all" ? "one" : "off";
  const repeatLabel =
    repeat === "one" ? "Repeat one (click to turn off)" : repeat === "all" ? "Repeat all (click for repeat one)" : "Enable repeat";

  const queue = activeZone?.queue ?? [];
  const currentIdx = activeZone?.currentIndex ?? -1;
  const upcoming = currentIdx >= 0 ? queue.slice(currentIdx + 1) : queue;

  return (
    <div
      role="dialog"
      aria-modal="true"
      aria-label="Now Playing"
      className="fixed inset-0 z-50 hidden lg:flex flex-col bg-[#09090b] overflow-hidden select-none"
    >
      {/* Rich 4-color UltraBlur backdrop */}
      <AmbientBackdrop artworkUrl={track?.artworkUrl} />
      <div
        aria-hidden="true"
        className="pointer-events-none fixed inset-0 bg-[radial-gradient(ellipse_at_center,rgba(0,0,0,0.12)_0%,rgba(9,9,11,0.62)_100%)]"
      />

      {/* Top bar */}
      <header className="relative z-10 flex items-center justify-between px-8 py-5 shrink-0">
        <div className="flex items-center gap-2.5 min-w-0">
          <span className="h-2 w-2 rounded-full bg-emerald-400 shrink-0" />
          <span className="text-xs font-medium text-muted-foreground">Playing on</span>
          <span className="truncate text-xs font-semibold text-foreground">{activeDeviceName}</span>
        </div>

        <div className="flex items-center gap-2">
          <Button
            type="button"
            variant="ghost"
            size="sm"
            onClick={() => setShowQueue((prev) => !prev)}
            aria-pressed={showQueue}
            className={cn(
              "gap-1.5 rounded-full px-3 text-xs",
              showQueue ? "bg-white/[0.12] text-white" : "text-muted-foreground hover:text-white",
            )}
          >
            <ListMusic className="h-4 w-4" />
            <span>Up Next</span>
            {upcoming.length > 0 && (
              <span className="font-mono text-[10px] text-muted-foreground">({upcoming.length})</span>
            )}
          </Button>
          <Button
            type="button"
            variant="ghost"
            size="icon"
            onClick={onClose}
            aria-label="Exit full screen player"
            title="Minimize (Esc)"
            className="rounded-full text-muted-foreground hover:text-white hover:bg-white/[0.08]"
          >
            <ChevronDown className="h-5 w-5" />
          </Button>
        </div>
      </header>

      {/* Main stage */}
      <div className="relative z-10 flex flex-1 min-h-0 items-center justify-center px-10 pb-10 gap-14">
        {/* Left / Center: Artwork + Metadata + Controls */}
        <div
          className={cn(
            "flex flex-col items-center justify-center w-full transition-all duration-300",
            showQueue ? "max-w-[460px]" : "max-w-[520px]",
          )}
        >
          {/* Large square cover art (scales gently when paused) */}
          <button
            type="button"
            disabled={!track?.albumId}
            onClick={() => {
              if (track?.albumId) {
                onClose();
                onOpenAlbum?.(track.albumId);
              }
            }}
            className={cn(
              "aspect-square w-[min(44vh,400px)] rounded-2xl overflow-hidden shadow-[0_32px_80px_rgba(0,0,0,0.7)] ring-1 ring-white/15 transition-transform duration-500 ease-out cursor-pointer disabled:cursor-default",
              playing ? "scale-100" : "scale-[0.96]",
            )}
          >
            <AlbumArt
              artworkUrl={track?.artworkUrl}
              alt={track?.album || track?.title || "Album cover"}
              size="lg"
              className="h-full w-full"
              priority
            />
          </button>

          {/* Title, Artist/Album, Signal Path */}
          <div className="mt-7 w-full flex items-start justify-between gap-4">
            <div className="min-w-0">
              <h1 className="truncate text-2xl font-bold tracking-tight text-white">
                {track?.title || "Not Playing"}
              </h1>
              <div className="mt-1 flex items-center gap-1.5 text-sm text-muted-foreground truncate">
                {track?.artist && (
                  <button
                    type="button"
                    onClick={() => {
                      onClose();
                      onOpenArtist?.(track.artist);
                    }}
                    className="truncate hover:text-white hover:underline cursor-pointer"
                  >
                    {track.artist}
                  </button>
                )}
                {track?.artist && track?.album && <span className="text-muted-foreground/50">·</span>}
                {track?.album && (
                  <button
                    type="button"
                    disabled={!track.albumId}
                    onClick={() => {
                      if (track.albumId) {
                        onClose();
                        onOpenAlbum?.(track.albumId);
                      }
                    }}
                    className="truncate hover:text-white hover:underline cursor-pointer disabled:cursor-default disabled:no-underline"
                  >
                    {track.album}
                  </button>
                )}
              </div>
            </div>

            {track && (
              <div className="shrink-0 pt-1">
                <SignalPathPopover
                  track={track}
                  zone={activeZone}
                  qualLabel={qualLabel}
                  isBrowserActive={isBrowserActive}
                  streamMode={streamMode}
                />
              </div>
            )}
          </div>

          {/* Seekbar */}
          <div className="mt-5 w-full space-y-1.5">
            <Slider
              aria-label="Seek position"
              min={0}
              max={duration || 1}
              step={1}
              value={[Math.min(shownPos, duration || 1)]}
              disabled={!hasZone || duration === 0}
              onValueChange={([v]) => onScrub(v)}
              onValueCommit={([v]) => {
                onScrub(null);
                onSeek(v);
              }}
            />
            <div className="flex items-center justify-between font-mono text-[11px] tabular-nums text-muted-foreground">
              <span>{fmtDuration(shownPos)}</span>
              <span>{fmtDuration(duration)}</span>
            </div>
          </div>

          {/* Transport controls */}
          <div className="mt-4 flex w-full items-center justify-center gap-5">
            <Button
              type="button"
              variant="ghost"
              size="icon"
              disabled={!hasZone}
              onClick={() => onShuffle(!shuffle)}
              aria-label={shuffle ? "Disable shuffle" : "Enable shuffle"}
              aria-pressed={shuffle}
              className={cn(
                "relative h-10 w-10 rounded-full",
                shuffle ? "text-emerald-400 hover:text-emerald-300" : "text-muted-foreground hover:text-white",
              )}
            >
              <Shuffle className="h-4 w-4" />
              {shuffle && <span className="absolute bottom-1 h-1 w-1 rounded-full bg-emerald-400" />}
            </Button>

            <Button
              type="button"
              variant="ghost"
              size="icon"
              disabled={!hasPlayable}
              onClick={() => onTransport("prev")}
              aria-label="Previous"
              className="h-11 w-11 rounded-full text-foreground/90 hover:text-white"
            >
              <SkipBack className="h-5 w-5 fill-current" />
            </Button>

            <button
              type="button"
              disabled={!hasPlayable}
              onClick={() => onTransport(playing ? "pause" : "play")}
              aria-label={playing ? "Pause" : "Play"}
              className="flex h-14 w-14 items-center justify-center rounded-full bg-white text-black shadow-2xl hover:scale-105 active:scale-95 transition-transform disabled:opacity-40 disabled:cursor-not-allowed cursor-pointer"
            >
              {playing ? (
                <Pause className="h-6 w-6 fill-current" />
              ) : (
                <Play className="h-6 w-6 fill-current translate-x-0.5" />
              )}
            </button>

            <Button
              type="button"
              variant="ghost"
              size="icon"
              disabled={!hasPlayable}
              onClick={() => onTransport("next")}
              aria-label="Next"
              className="h-11 w-11 rounded-full text-foreground/90 hover:text-white"
            >
              <SkipForward className="h-5 w-5 fill-current" />
            </Button>

            <Button
              type="button"
              variant="ghost"
              size="icon"
              disabled={!hasZone}
              onClick={() => onRepeat(nextRepeatMode)}
              aria-label={repeatLabel}
              aria-pressed={repeat !== "off"}
              className={cn(
                "relative h-10 w-10 rounded-full",
                repeat !== "off" ? "text-emerald-400 hover:text-emerald-300" : "text-muted-foreground hover:text-white",
              )}
            >
              {repeat === "one" ? <Repeat1 className="h-4 w-4" /> : <Repeat className="h-4 w-4" />}
              {repeat !== "off" && <span className="absolute bottom-1 h-1 w-1 rounded-full bg-emerald-400" />}
            </Button>
          </div>

          {/* Volume */}
          {activeZone?.renderer.hasVolume !== false && (
            <div className="mt-5 flex w-60 items-center gap-2.5">
              <Button
                type="button"
                variant="ghost"
                size="icon-sm"
                disabled={!hasZone}
                onClick={() => onMute(!activeZone?.muted)}
                aria-label={activeZone?.muted ? "Unmute" : "Mute"}
                className="text-muted-foreground hover:text-white"
              >
                {activeZone?.muted || shownVol === 0 ? (
                  <VolumeX className="h-4 w-4" />
                ) : (
                  <Volume2 className="h-4 w-4" />
                )}
              </Button>
              <Slider
                aria-label="Volume"
                min={0}
                max={100}
                step={1}
                value={[activeZone?.muted ? 0 : shownVol]}
                disabled={!hasZone}
                onValueChange={([v]) => onVolDraft(v)}
                onValueCommit={([v]) => {
                  onVolume(v);
                  onVolDraft(null);
                }}
              />
            </div>
          )}
        </div>

        {/* Optional right-side frosted Up Next panel */}
        {showQueue && (
          <aside className="flex h-[min(70vh,560px)] w-80 xl:w-96 shrink-0 flex-col rounded-2xl border border-white/[0.09] bg-black/35 backdrop-blur-2xl overflow-hidden">
            <div className="flex items-center justify-between px-4 py-3 border-b border-white/[0.07] shrink-0">
              <span className="text-xs font-semibold uppercase tracking-wider text-muted-foreground">Up Next</span>
              <span className="font-mono text-[11px] text-muted-foreground">{queue.length} in queue</span>
            </div>
            <div className="flex-1 overflow-y-auto p-2 space-y-0.5">
              {queue.length === 0 ? (
                <div className="flex h-full items-center justify-center text-xs text-muted-foreground">
                  Queue is empty
                </div>
              ) : (
                queue.map((item) => {
                  const isCurrent = item.itemId === activeZone?.currentItemId;
                  return (
                    <button
                      key={item.itemId}
                      type="button"
                      onClick={() => onJumpItem?.(item.itemId)}
                      className={cn(
                        "flex w-full items-center gap-3 rounded-xl px-2.5 py-2 text-left transition-colors cursor-pointer",
                        isCurrent ? "bg-white/[0.1]" : "hover:bg-white/[0.06]",
                      )}
                    >
                      <AlbumArt
                        artworkUrl={item.track.artworkUrl}
                        alt={item.track.album || item.track.title}
                        className="h-9 w-9 shrink-0 rounded-lg ring-1 ring-white/10"
                      />
                      <div className="min-w-0 flex-1">
                        <div
                          className={cn(
                            "truncate text-xs font-medium",
                            isCurrent ? "text-emerald-400" : "text-foreground",
                          )}
                        >
                          {item.track.title}
                        </div>
                        <div className="truncate text-[11px] text-muted-foreground">{item.track.artist}</div>
                      </div>
                      {isCurrent && playing ? (
                        <PlayingBars className="shrink-0" />
                      ) : (
                        <span className="font-mono text-[11px] text-muted-foreground/70 shrink-0">
                          {fmtDuration(item.track.durationSec)}
                        </span>
                      )}
                    </button>
                  );
                })
              )}
            </div>
          </aside>
        )}
      </div>
    </div>
  );
}
