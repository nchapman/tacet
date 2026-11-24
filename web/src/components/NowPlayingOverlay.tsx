import {
  ChevronDown,
  Laptop,
  ListMusic,
  Loader2,
  Minimize2,
  Pause,
  Play,
  Repeat,
  Repeat1,
  Shuffle,
  SkipBack,
  SkipForward,
  Speaker,
  Volume2,
  VolumeX,
} from "lucide-react";
import { useEffect, useState } from "react";
import { fmtCount, fmtDuration } from "@/api/format";
import type { AudioQuality, RepeatMode, StreamMode, TransportAction, ZoneSnapshot } from "@/api/types";
import { DeviceList } from "@/components/DevicePicker";
import { SignalPathPopover } from "@/components/SignalPathPopover";
import { AlbumArt, AmbientBackdrop } from "@/components/ui/artwork";
import { Button } from "@/components/ui/button";
import { PlayingBars } from "@/components/ui/PlayingBars";
import { Slider } from "@/components/ui/slider";
import type { BrowserPlayerController } from "@/hooks/useBrowserPlayer";
import { useAlbumHoverPrefetch, useArtistHoverPrefetch } from "@/lib/prefetch";
import { cn } from "@/lib/utils";

interface NowPlayingOverlayProps {
  open: boolean;
  panel: "player" | "devices";
  onPanelChange: (panel: "player" | "devices") => void;
  onClose: () => void;
  zones: ZoneSnapshot[];
  activeZone: ZoneSnapshot | null;
  activeDeviceName: string;
  isBrowserActive: boolean;
  discovering: boolean;
  transcodeAvailable: boolean;
  browserPlayer: BrowserPlayerController;
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
  onSelectZone: (zoneId: string) => void;
  onSetZoneQuality: (zoneId: string, quality: AudioQuality) => void;
  onDiscover: () => void;
  onOpenConnect: () => void;
  onToggleQueue: () => void;
  onJumpItem?: (itemId: string) => void;
  onOpenAlbum?: (albumId: string) => void;
  onOpenArtist?: (artist: string) => void;
}

export function NowPlayingOverlay({
  open,
  panel,
  onPanelChange,
  onClose,
  zones,
  activeZone,
  activeDeviceName,
  isBrowserActive,
  discovering,
  transcodeAvailable,
  browserPlayer,
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
  onSelectZone,
  onSetZoneQuality,
  onDiscover,
  onOpenConnect,
  onToggleQueue,
  onJumpItem,
  onOpenAlbum,
  onOpenArtist,
}: NowPlayingOverlayProps) {
  const [showQueue, setShowQueue] = useState(false);
  const track = activeZone?.currentTrack ?? null;
  const albumPrefetch = useAlbumHoverPrefetch(track?.albumId);
  const artistPrefetch = useArtistHoverPrefetch(track?.artist);

  useEffect(() => {
    if (!open) return;
    const onKeyDown = (e: KeyboardEvent) => {
      if (e.key === "Escape") {
        e.preventDefault();
        if (panel === "devices") {
          onPanelChange("player");
        } else {
          onClose();
        }
      }
    };
    window.addEventListener("keydown", onKeyDown);
    return () => window.removeEventListener("keydown", onKeyDown);
  }, [open, panel, onPanelChange, onClose]);

  if (!open) return null;

  const playing = activeZone?.state === "playing";
  const buffering = activeZone?.state === "buffering";
  const hasZone = activeZone !== null;
  const hasPlayable =
    hasZone &&
    Boolean(track || (activeZone?.queue?.length ?? 0) > 0 || playing || buffering || activeZone?.state === "paused");
  const shuffle = Boolean(activeZone?.shuffle);
  const repeat: RepeatMode = activeZone?.repeat ?? "off";
  const nextRepeatMode: RepeatMode = repeat === "off" ? "all" : repeat === "all" ? "one" : "off";
  const repeatLabel =
    repeat === "one"
      ? "Repeat one (click to turn off)"
      : repeat === "all"
        ? "Repeat all (click for repeat one)"
        : "Enable repeat";

  const queue = activeZone?.queue ?? [];
  const currentIdx = activeZone?.currentIndex ?? -1;
  const upcoming = currentIdx >= 0 ? queue.slice(currentIdx + 1) : queue;

  return (
    <div
      role="dialog"
      aria-modal="true"
      aria-label="Now Playing"
      className="fixed inset-0 z-50 flex flex-col bg-[#09090b] overflow-hidden select-none"
    >
      {/* Rich 4-color UltraBlur backdrop */}
      <AmbientBackdrop artworkUrl={track?.artworkUrl} intensity="full" />
      <div
        aria-hidden="true"
        className="pointer-events-none fixed inset-0 bg-[radial-gradient(ellipse_at_center,rgba(0,0,0,0.12)_0%,rgba(9,9,11,0.62)_100%)]"
      />

      {/* Top bar — responsive across mobile & desktop */}
      <header className="relative z-10 flex items-center justify-between px-4 py-3 lg:px-8 lg:py-5 shrink-0">
        {/* Mobile left: Minimize */}
        <Button
          type="button"
          variant="ghost"
          size="icon"
          onClick={() => (panel === "devices" ? onPanelChange("player") : onClose())}
          aria-label="Minimize player"
          className="lg:hidden rounded-full text-muted-foreground hover:text-white"
        >
          <ChevronDown className="h-6 w-6" />
        </Button>

        {/* Playing on indicator (centered on mobile, left-aligned on desktop) */}
        <button
          type="button"
          onClick={() => onPanelChange(panel === "devices" ? "player" : "devices")}
          className="min-w-0 px-2 text-center lg:text-left lg:px-0 flex flex-col lg:flex-row lg:items-center lg:gap-2.5 cursor-pointer group"
          title="Switch output device"
        >
          <span className="hidden lg:inline-block h-2 w-2 rounded-full bg-emerald-400 shrink-0" />
          <span className="text-[10.5px] lg:text-xs font-semibold lg:font-medium uppercase lg:normal-case tracking-widest lg:tracking-normal text-muted-foreground">
            {panel === "devices" ? "Output Device" : "Playing on"}
          </span>
          <span className="truncate text-xs font-medium lg:font-semibold text-emerald-300 lg:text-foreground group-hover:underline">
            {activeDeviceName}
          </span>
        </button>

        {/* Right controls */}
        <div className="flex items-center gap-1.5 lg:gap-2">
          {/* Mobile queue button opens the mobile queue drawer */}
          <Button
            type="button"
            variant="ghost"
            size="icon"
            onClick={() => {
              onClose();
              onToggleQueue();
            }}
            aria-label="Open queue"
            className="lg:hidden rounded-full text-muted-foreground hover:text-white"
          >
            <ListMusic className="h-5 w-5" />
          </Button>

          {/* Desktop Up Next toggle */}
          <Button
            type="button"
            variant="ghost"
            size="sm"
            onClick={() => {
              if (panel === "devices") onPanelChange("player");
              setShowQueue((prev) => !prev);
            }}
            aria-pressed={showQueue}
            className={cn(
              "hidden lg:inline-flex gap-1.5 rounded-full px-3 text-xs",
              showQueue ? "bg-white/[0.12] text-white" : "text-muted-foreground hover:text-white",
            )}
          >
            <ListMusic className="h-4 w-4" />
            <span>Up Next</span>
            {upcoming.length > 0 && (
              <span className="font-mono text-[10px] text-muted-foreground">({fmtCount(upcoming.length)})</span>
            )}
          </Button>

          {/* Desktop Minimize */}
          <Button
            type="button"
            variant="ghost"
            size="icon"
            onClick={onClose}
            aria-label="Exit full screen player"
            title="Minimize (Esc)"
            className="hidden lg:inline-flex rounded-full text-muted-foreground hover:text-white hover:bg-white/[0.08]"
          >
            <Minimize2 className="h-4 w-4" />
          </Button>
        </div>
      </header>

      {/* Main stage */}
      {panel === "devices" ? (
        <div
          key="devices"
          onClick={(e) => {
            if (e.target === e.currentTarget) onPanelChange("player");
          }}
          className="relative z-10 flex flex-1 min-h-0 items-start lg:items-center justify-center overflow-y-auto px-5 py-6"
        >
          <div className="w-full max-w-md rounded-2xl border border-white/10 bg-black/45 p-2 shadow-2xl">
            <DeviceList
              zones={zones}
              activeZoneId={activeZone?.zoneId ?? ""}
              discovering={discovering}
              transcodeAvailable={transcodeAvailable}
              browserPlayer={browserPlayer}
              onSelectZone={onSelectZone}
              onSetZoneQuality={onSetZoneQuality}
              onDiscover={onDiscover}
              onOpenConnect={() => {
                onClose();
                onOpenConnect();
              }}
              onDone={() => onPanelChange("player")}
            />
          </div>
        </div>
      ) : (
        <div
          key="player"
          className="relative z-10 flex flex-1 min-h-0 items-center justify-center px-6 pb-8 lg:px-10 lg:pb-10 gap-14"
        >
          {/* Player column */}
          <div
            className={cn(
              "flex flex-col justify-between lg:justify-center items-center w-full h-full lg:h-auto max-w-[420px] transition-[max-width] duration-300 gap-5 lg:gap-0",
              showQueue ? "lg:max-w-[460px]" : "lg:max-w-[520px]",
            )}
          >
            {/* Large square cover art (scales gently when paused) */}
            <div className="flex items-center justify-center flex-1 lg:flex-none min-h-0 w-full">
              <button
                type="button"
                disabled={!track?.albumId}
                {...albumPrefetch}
                onClick={() => {
                  if (track?.albumId) {
                    onOpenAlbum?.(track.albumId);
                  }
                }}
                className={cn(
                  "aspect-square w-full max-w-[320px] lg:max-w-none lg:w-[min(44vh,400px)] rounded-2xl overflow-hidden shadow-[0_28px_75px_rgba(0,0,0,0.7)] ring-1 ring-white/15 transition-transform duration-500 ease-out cursor-pointer disabled:cursor-default",
                  playing || buffering ? "scale-100" : "scale-[0.96]",
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
            </div>

            {/* Metadata */}
            <div className="w-full min-w-0 lg:mt-7">
              <h1 className="truncate text-xl lg:text-2xl font-bold tracking-tight text-white">
                {track?.title || "Nothing Playing"}
              </h1>
              <div className="mt-0.5 lg:mt-1 flex items-center gap-1.5 text-sm text-muted-foreground truncate">
                {track ? (
                  <>
                    {track.artist && (
                      <button
                        type="button"
                        {...artistPrefetch}
                        onClick={() => {
                          onOpenArtist?.(track.artist);
                        }}
                        className="truncate hover:text-white hover:underline cursor-pointer"
                      >
                        {track.artist}
                      </button>
                    )}
                    {track.artist && track.album && <span className="text-muted-foreground/50">·</span>}
                    {track.album && (
                      <button
                        type="button"
                        disabled={!track.albumId}
                        {...albumPrefetch}
                        onClick={() => {
                          if (track.albumId) {
                            onOpenAlbum?.(track.albumId);
                          }
                        }}
                        className="truncate hover:text-white hover:underline cursor-pointer disabled:cursor-default disabled:no-underline"
                      >
                        {track.album}
                      </button>
                    )}
                  </>
                ) : (
                  <span>Select an album to begin</span>
                )}
              </div>
            </div>

            {/* Seekbar + centered SignalPathPopover (same layout on mobile & desktop) */}
            <div className="w-full space-y-2 lg:mt-5">
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
              <div className="grid grid-cols-[1fr_auto_1fr] items-center gap-2 font-mono text-[11px] tabular-nums text-muted-foreground">
                <span>{fmtDuration(shownPos)}</span>
                <div className="flex justify-center">
                  {track ? (
                    <SignalPathPopover
                      track={track}
                      zone={activeZone}
                      qualLabel={qualLabel}
                      isBrowserActive={isBrowserActive}
                      streamMode={streamMode}
                    />
                  ) : (
                    <span />
                  )}
                </div>
                <span className="text-right">{fmtDuration(duration)}</span>
              </div>
            </div>

            {/* Transport controls */}
            <div className="flex w-full items-center justify-between lg:justify-center lg:gap-5 px-1 lg:mt-4">
              <Button
                type="button"
                variant="ghost"
                size="icon"
                disabled={!hasZone}
                onClick={() => onShuffle(!shuffle)}
                aria-label={shuffle ? "Disable shuffle" : "Enable shuffle"}
                aria-pressed={shuffle}
                className={cn(
                  "relative h-11 w-11 lg:h-10 lg:w-10 rounded-full",
                  shuffle ? "text-emerald-400 hover:text-emerald-300" : "text-muted-foreground hover:text-white",
                )}
              >
                <Shuffle className="h-5 w-5 lg:h-4 lg:w-4" />
                {shuffle && <span className="absolute bottom-1 h-1 w-1 rounded-full bg-emerald-400" />}
              </Button>

              <Button
                type="button"
                variant="ghost"
                size="icon"
                disabled={!hasPlayable}
                onClick={() => onTransport("prev")}
                aria-label="Previous"
                className="h-12 w-12 lg:h-11 lg:w-11 rounded-full text-foreground/90 hover:text-white"
              >
                <SkipBack className="h-6 w-6 lg:h-5 lg:w-5 fill-current" />
              </Button>

              <button
                type="button"
                disabled={!hasPlayable}
                onClick={() => onTransport(playing || buffering ? "pause" : "play")}
                aria-label={buffering ? "Buffering — click to pause" : playing ? "Pause" : "Play"}
                className="flex h-16 w-16 lg:h-14 lg:w-14 items-center justify-center rounded-full bg-white text-black shadow-2xl hover:scale-105 active:scale-95 transition-transform disabled:opacity-40 disabled:cursor-not-allowed cursor-pointer"
              >
                {buffering ? (
                  <Loader2 className="h-7 w-7 lg:h-6 lg:w-6 animate-spin" />
                ) : playing ? (
                  <Pause className="h-7 w-7 lg:h-6 lg:w-6 fill-current" />
                ) : (
                  <Play className="h-7 w-7 lg:h-6 lg:w-6 fill-current translate-x-0.5" />
                )}
              </button>

              <Button
                type="button"
                variant="ghost"
                size="icon"
                disabled={!hasPlayable}
                onClick={() => onTransport("next")}
                aria-label="Next"
                className="h-12 w-12 lg:h-11 lg:w-11 rounded-full text-foreground/90 hover:text-white"
              >
                <SkipForward className="h-6 w-6 lg:h-5 lg:w-5 fill-current" />
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
                  "relative h-11 w-11 lg:h-10 lg:w-10 rounded-full",
                  repeat !== "off" ? "text-emerald-400 hover:text-emerald-300" : "text-muted-foreground hover:text-white",
                )}
              >
                {repeat === "one" ? (
                  <Repeat1 className="h-5 w-5 lg:h-4 lg:w-4" />
                ) : (
                  <Repeat className="h-5 w-5 lg:h-4 lg:w-4" />
                )}
                {repeat !== "off" && <span className="absolute bottom-1 h-1 w-1 rounded-full bg-emerald-400" />}
              </Button>
            </div>

            {/* Volume + mobile Devices switch */}
            <div className="flex w-full lg:w-60 items-center gap-3 lg:gap-2.5 pt-1 lg:mt-5">
              {activeZone?.renderer.hasVolume !== false && (
                <>
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
                    className="flex-1"
                  />
                </>
              )}
              <Button
                type="button"
                variant="secondary"
                size="sm"
                className="lg:hidden rounded-full text-xs shrink-0"
                onClick={() => onPanelChange("devices")}
              >
                {isBrowserActive ? <Laptop className="h-3.5 w-3.5" /> : <Speaker className="h-3.5 w-3.5" />}
                <span>Devices</span>
              </Button>
            </div>
          </div>

          {/* Optional right-side frosted Up Next panel (desktop only) */}
          {showQueue && (
            <aside className="hidden lg:flex h-[min(70vh,560px)] w-80 xl:w-96 shrink-0 flex-col rounded-2xl border border-white/[0.09] bg-black/35 overflow-hidden">
              <div className="flex items-center justify-between px-4 py-3 border-b border-white/[0.07] shrink-0">
                <span className="text-xs font-semibold uppercase tracking-wider text-muted-foreground">Up Next</span>
                <span className="font-mono text-[11px] text-muted-foreground">{fmtCount(queue.length)} in queue</span>
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
      )}
    </div>
  );
}
