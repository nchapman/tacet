import {
  AlertCircle,
  ChevronDown,
  Laptop,
  ListMusic,
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
  X,
} from "lucide-react";
import { useState } from "react";
import { fmtDuration, fmtOutputQuality, isHiResOutput } from "@/api/format";
import type { AudioQuality, RepeatMode, TransportAction, ZoneSnapshot } from "@/api/types";
import { DeviceList } from "@/components/DevicePicker";
import { AlbumArt, AmbientBackdrop } from "@/components/ui/artwork";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Dialog, DialogContent, DialogTitle } from "@/components/ui/dialog";
import { Popover, PopoverContent, PopoverTrigger } from "@/components/ui/popover";
import { Slider } from "@/components/ui/slider";
import type { BrowserPlayerController } from "@/hooks/useBrowserPlayer";
import { cn } from "@/lib/utils";

interface PlayerBarProps {
  zones: ZoneSnapshot[];
  activeZone: ZoneSnapshot | null;
  positionSec: number;
  error: string;
  discovering: boolean;
  transcodeAvailable: boolean;
  queueOpen: boolean;
  browserPlayer: BrowserPlayerController;
  onDismissError: () => void;
  onTransport: (action: TransportAction) => void;
  onSeek: (sec: number) => void;
  onVolume: (vol: number) => void;
  onMute: (muted: boolean) => void;
  onShuffle: (enabled: boolean) => void;
  onRepeat: (mode: RepeatMode) => void;
  onSelectZone: (zoneId: string) => void;
  onSetZoneQuality: (zoneId: string, quality: AudioQuality) => void;
  onDiscover: () => void;
  onOpenConnect: () => void;
  onToggleQueue: () => void;
  onOpenAlbum?: (albumId: string) => void;
  onOpenArtist?: (artist: string) => void;
}

export function PlayerBar({
  zones,
  activeZone,
  positionSec,
  error,
  discovering,
  transcodeAvailable,
  queueOpen,
  browserPlayer,
  onDismissError,
  onTransport,
  onSeek,
  onVolume,
  onMute,
  onShuffle,
  onRepeat,
  onSelectZone,
  onSetZoneQuality,
  onDiscover,
  onOpenConnect,
  onToggleQueue,
  onOpenAlbum,
  onOpenArtist,
}: PlayerBarProps) {
  const [scrubPos, setScrubPos] = useState<number | null>(null);
  const [volDraft, setVolDraft] = useState<number | null>(null);
  const [devicePopoverOpen, setDevicePopoverOpen] = useState(false);
  const [mobileExpanded, setMobileExpanded] = useState(false);
  const [mobilePanel, setMobilePanel] = useState<"player" | "devices">("player");

  const track = activeZone?.currentTrack;
  const shuffle = Boolean(activeZone?.shuffle);
  const repeat: RepeatMode = activeZone?.repeat ?? "off";
  const nextRepeatMode: RepeatMode = repeat === "off" ? "all" : repeat === "all" ? "one" : "off";
  const repeatLabel =
    repeat === "one"
      ? "Repeat One (click to turn off)"
      : repeat === "all"
        ? "Repeat All (click for Repeat One)"
        : "Repeat Off (click for Repeat All)";
  const duration = Math.max(0, activeZone?.durationSec || track?.durationSec || 0);
  const shownPos = scrubPos ?? Math.min(positionSec, duration || positionSec);
  const shownVol = volDraft ?? activeZone?.volume ?? 0;
  const playing = activeZone?.state === "playing";
  const hasZone = activeZone !== null;
  const hasPlayable =
    hasZone &&
    Boolean(track || (activeZone?.queue?.length ?? 0) > 0 || playing || activeZone?.state === "paused");
  const isBrowserActive = Boolean(browserPlayer.enabled && activeZone?.zoneId === browserPlayer.zoneId);
  const activeDeviceName = activeZone
    ? isBrowserActive
      ? "This Browser"
      : activeZone.renderer.name
    : "No Device";
  const qualLabel = track ? fmtOutputQuality(track, activeZone?.activeQuality) : "";
  const hiRes = track ? isHiResOutput(track, activeZone?.activeQuality) : false;
  const progressPct = duration > 0 ? Math.min(100, (shownPos / duration) * 100) : 0;

  return (
    <>
      {/* Playback error toast above player bar */}
      {error && (
        <div
          role="alert"
          className="fixed bottom-[138px] inset-x-3 lg:inset-x-auto lg:bottom-[94px] lg:right-4 z-40 flex lg:max-w-md items-center gap-2.5 rounded-xl border border-red-500/30 bg-[#1a1012]/95 px-3.5 py-2.5 text-xs text-red-200 shadow-2xl backdrop-blur-xl"
        >
          <AlertCircle className="h-4 w-4 shrink-0 text-red-400" />
          <span className="flex-1">{error}</span>
          <button
            type="button"
            onClick={onDismissError}
            aria-label="Dismiss error"
            className="rounded-full p-0.5 hover:bg-white/10 cursor-pointer"
          >
            <X className="h-3.5 w-3.5" />
          </button>
        </div>
      )}

      {/* ==================== DESKTOP PLAYER BAR (>= lg) ==================== */}
      <footer
        aria-label="Player controls"
        className="hidden lg:grid grid-cols-[minmax(220px,1fr)_minmax(340px,540px)_minmax(240px,1fr)] items-center gap-4 h-[84px] shrink-0 border-t border-white/[0.08] bg-black/35 px-4 backdrop-blur-2xl relative overflow-hidden select-none"
      >
        {/* Left: Track artwork, metadata & format badge */}
        <div className="relative z-10 flex min-w-0 items-center gap-3">
          <button
            type="button"
            disabled={!track?.albumId}
            onClick={() => track?.albumId && onOpenAlbum?.(track.albumId)}
            className="group relative h-13 w-13 shrink-0 rounded-lg overflow-hidden shadow-lg ring-1 ring-white/10 cursor-pointer disabled:cursor-default"
            title={track?.album ? `Open ${track.album}` : undefined}
          >
            <AlbumArt artworkUrl={track?.artworkUrl} alt={track?.album || track?.title || "No track"} className="h-full w-full" />
          </button>
          <div className="min-w-0 flex-1">
            {track ? (
              <>
                <div className="flex items-center gap-2 min-w-0">
                  <button
                    type="button"
                    onClick={() => track.albumId && onOpenAlbum?.(track.albumId)}
                    className="truncate text-[13.5px] font-medium text-foreground hover:underline cursor-pointer text-left"
                  >
                    {track.title}
                  </button>
                </div>
                <div className="flex items-center gap-1.5 mt-0.5 min-w-0">
                  <button
                    type="button"
                    onClick={() => track.artist && onOpenArtist?.(track.artist)}
                    className="truncate text-xs text-muted-foreground hover:text-foreground hover:underline cursor-pointer text-left"
                  >
                    {track.artist}
                  </button>
                  {qualLabel && (
                    <Badge
                      variant={hiRes ? "hires" : "muted"}
                      className="shrink-0 text-[10px] px-1.5 py-0 leading-4"
                    >
                      {qualLabel}
                    </Badge>
                  )}
                </div>
              </>
            ) : (
              <div>
                <div className="text-[13px] font-medium text-muted-foreground">Not Playing</div>
                <div className="text-xs text-muted-foreground/60">Choose an album to listen</div>
              </div>
            )}
          </div>
        </div>

        {/* Center: Transport + Scrub Bar */}
        <div className="relative z-10 flex flex-col items-center gap-1.5">
          <div className="flex items-center gap-2">
            <Button
              type="button"
              variant="ghost"
              size="icon-sm"
              disabled={!hasZone}
              onClick={() => onShuffle(!shuffle)}
              aria-label={shuffle ? "Disable shuffle" : "Enable shuffle"}
              aria-pressed={shuffle}
              title={shuffle ? "Shuffle On" : "Shuffle Off"}
              className={cn(
                "relative",
                shuffle ? "text-emerald-400 hover:text-emerald-300" : "text-muted-foreground hover:text-foreground",
              )}
            >
              <Shuffle className="h-3.5 w-3.5" />
              {shuffle && <span className="absolute bottom-0.5 h-1 w-1 rounded-full bg-emerald-400" />}
            </Button>

            <Button
              type="button"
              variant="ghost"
              size="icon-sm"
              disabled={!hasPlayable}
              onClick={() => onTransport("prev")}
              aria-label="Previous"
            >
              <SkipBack className="h-4 w-4 fill-current" />
            </Button>

            <button
              type="button"
              disabled={!hasPlayable}
              onClick={() => onTransport(playing ? "pause" : "play")}
              aria-label={playing ? "Pause" : "Play"}
              className="flex h-9 w-9 items-center justify-center rounded-full bg-white text-black shadow-md transition-transform hover:scale-105 active:scale-95 disabled:opacity-40 disabled:cursor-not-allowed disabled:hover:scale-100 cursor-pointer"
            >
              {playing ? (
                <Pause className="h-4 w-4 fill-current" />
              ) : (
                <Play className="h-4 w-4 fill-current translate-x-[1px]" />
              )}
            </button>

            <Button
              type="button"
              variant="ghost"
              size="icon-sm"
              disabled={!hasPlayable}
              onClick={() => onTransport("next")}
              aria-label="Next"
            >
              <SkipForward className="h-4 w-4 fill-current" />
            </Button>

            <Button
              type="button"
              variant="ghost"
              size="icon-sm"
              disabled={!hasZone}
              onClick={() => onRepeat(nextRepeatMode)}
              aria-label={repeatLabel}
              aria-pressed={repeat !== "off"}
              title={repeatLabel}
              className={cn(
                "relative",
                repeat !== "off"
                  ? "text-emerald-400 hover:text-emerald-300"
                  : "text-muted-foreground hover:text-foreground",
              )}
            >
              {repeat === "one" ? <Repeat1 className="h-3.5 w-3.5" /> : <Repeat className="h-3.5 w-3.5" />}
              {repeat !== "off" && <span className="absolute bottom-0.5 h-1 w-1 rounded-full bg-emerald-400" />}
            </Button>
          </div>

          <div className="flex w-full items-center gap-2.5">
            <span className="w-10 text-right text-[11px] font-mono tabular-nums text-muted-foreground">
              {fmtDuration(shownPos)}
            </span>
            <Slider
              aria-label="Seek position"
              min={0}
              max={duration || 1}
              step={1}
              value={[Math.min(shownPos, duration || 1)]}
              disabled={!hasZone || duration === 0}
              onValueChange={([v]) => setScrubPos(v)}
              onValueCommit={([v]) => {
                onSeek(v);
                setScrubPos(null);
              }}
              className="flex-1"
            />
            <span className="w-10 text-[11px] font-mono tabular-nums text-muted-foreground">
              {fmtDuration(duration)}
            </span>
          </div>
        </div>

        {/* Right: Integrated Output Device Popover, Queue Toggle, Volume */}
        <div className="relative z-10 flex items-center justify-end gap-1.5">
          <Popover open={devicePopoverOpen} onOpenChange={setDevicePopoverOpen}>
            <PopoverTrigger asChild>
              <button
                type="button"
                aria-label="Select playback device"
                className={cn(
                  "inline-flex h-8 max-w-[175px] items-center gap-2 rounded-full border px-2.5 text-xs font-medium transition-colors cursor-pointer",
                  hasZone
                    ? "border-emerald-400/25 bg-emerald-400/10 text-emerald-300 hover:bg-emerald-400/15"
                    : "border-white/10 bg-white/[0.05] text-muted-foreground hover:text-foreground",
                )}
              >
                {isBrowserActive ? (
                  <Laptop className="h-3.5 w-3.5 shrink-0" />
                ) : (
                  <Speaker className="h-3.5 w-3.5 shrink-0" />
                )}
                <span className="truncate">{activeDeviceName}</span>
              </button>
            </PopoverTrigger>
            <PopoverContent align="end" side="top" className="max-h-[calc(100dvh-110px)] overflow-y-auto">
              <DeviceList
                zones={zones}
                activeZoneId={activeZone?.zoneId ?? ""}
                discovering={discovering}
                transcodeAvailable={transcodeAvailable}
                browserPlayer={browserPlayer}
                onSelectZone={onSelectZone}
                onSetZoneQuality={onSetZoneQuality}
                onDiscover={onDiscover}
                onOpenConnect={onOpenConnect}
                onDone={() => setDevicePopoverOpen(false)}
              />
            </PopoverContent>
          </Popover>

          <Button
            type="button"
            variant="ghost"
            size="icon-sm"
            onClick={onToggleQueue}
            aria-label="Toggle queue"
            aria-pressed={queueOpen}
            className={cn("relative", queueOpen && "text-emerald-400 bg-emerald-400/10")}
          >
            <ListMusic className="h-4 w-4" />
            {(activeZone?.queue?.length ?? 0) > 0 && (
              <span className="absolute -top-1 -right-1.5 flex h-4 min-w-4 items-center justify-center rounded-full border border-white/15 bg-zinc-800 px-1 text-[9px] font-mono text-foreground shadow">
                {activeZone!.queue.length}
              </span>
            )}
          </Button>

          <div className="flex items-center gap-1.5 pl-1">
            <Button
              type="button"
              variant="ghost"
              size="icon-sm"
              disabled={!hasZone}
              onClick={() => onMute(!activeZone?.muted)}
              aria-label={activeZone?.muted ? "Unmute" : "Mute"}
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
              onValueChange={([v]) => setVolDraft(v)}
              onValueCommit={([v]) => {
                onVolume(v);
                setVolDraft(null);
              }}
              className="w-24"
            />
          </div>
        </div>
      </footer>

      {/* ==================== MOBILE MINI-PLAYER (< lg) ==================== */}
      <div className="lg:hidden shrink-0 px-2 pb-1.5 bg-transparent">
        <div className="relative overflow-hidden rounded-2xl border border-white/[0.1] bg-[#141419]/75 backdrop-blur-2xl shadow-xl">
          {/* Thin top progress hairline */}
          <div className="h-[2px] w-full bg-white/[0.08]">
            <div className="h-full bg-emerald-400 transition-[width] duration-300" style={{ width: `${progressPct}%` }} />
          </div>

          <div className="flex items-center gap-3 px-2.5 py-2">
            <button
              type="button"
              onClick={() => {
                setMobilePanel("player");
                setMobileExpanded(true);
              }}
              className="flex min-w-0 flex-1 items-center gap-3 text-left cursor-pointer"
            >
              <AlbumArt
                artworkUrl={track?.artworkUrl}
                alt={track?.title || "Now Playing"}
                className="h-11 w-11 shrink-0 rounded-lg ring-1 ring-white/10"
              />
              <div className="min-w-0 flex-1">
                <div className="truncate text-[13.5px] font-medium text-foreground">
                  {track ? track.title : "Not Playing"}
                </div>
                <div className="flex items-center gap-1.5 text-xs text-muted-foreground truncate">
                  <span className="truncate">{track ? track.artist : activeDeviceName}</span>
                  {track && (
                    <span className="inline-flex items-center gap-1 text-[11px] text-emerald-400 shrink-0">
                      · {activeDeviceName}
                    </span>
                  )}
                </div>
              </div>
            </button>

            <Button
              type="button"
              variant="ghost"
              size="icon-sm"
              onClick={() => {
                setMobilePanel("devices");
                setMobileExpanded(true);
              }}
              aria-label="Output device"
              className={cn(hasZone && "text-emerald-400")}
            >
              {isBrowserActive ? <Laptop className="h-4 w-4" /> : <Speaker className="h-4 w-4" />}
            </Button>

            <Button
              type="button"
              variant="ghost"
              size="icon-sm"
              disabled={!hasPlayable}
              onClick={() => onTransport(playing ? "pause" : "play")}
              aria-label={playing ? "Pause" : "Play"}
              className="text-foreground"
            >
              {playing ? <Pause className="h-5 w-5 fill-current" /> : <Play className="h-5 w-5 fill-current" />}
            </Button>
          </div>
        </div>
      </div>

      {/* ==================== MOBILE FULL-SCREEN NOW PLAYING SHEET ==================== */}
      <Dialog open={mobileExpanded} onOpenChange={setMobileExpanded}>
        <DialogContent
          hideClose
          className="lg:hidden inset-0 left-0 top-0 translate-x-0 translate-y-0 w-full max-w-none h-dvh rounded-none border-0 p-0 bg-[#09090b] overflow-hidden flex flex-col"
        >
          <DialogTitle className="sr-only">Now Playing</DialogTitle>
          <AmbientBackdrop artworkUrl={track?.artworkUrl} intensity="full" />

          <div className="relative z-10 flex h-full flex-col px-6 pt-[max(1rem,env(safe-area-inset-top))] pb-[max(1.5rem,env(safe-area-inset-bottom))]">
            {/* Top bar */}
            <div className="flex items-center justify-between">
              <Button
                type="button"
                variant="ghost"
                size="icon"
                onClick={() => setMobileExpanded(false)}
                aria-label="Minimize player"
              >
                <ChevronDown className="h-6 w-6" />
              </Button>
              <div className="text-center min-w-0 px-2">
                <div className="text-[10.5px] font-semibold uppercase tracking-widest text-muted-foreground">
                  {mobilePanel === "devices" ? "Output Device" : "Playing on"}
                </div>
                <div className="truncate text-xs font-medium text-emerald-300">{activeDeviceName}</div>
              </div>
              <Button
                type="button"
                variant="ghost"
                size="icon"
                onClick={() => {
                  setMobileExpanded(false);
                  onToggleQueue();
                }}
                aria-label="Open queue"
              >
                <ListMusic className="h-5 w-5" />
              </Button>
            </div>

            {mobilePanel === "devices" ? (
              <div className="flex-1 overflow-y-auto py-6">
                <div className="rounded-2xl border border-white/10 bg-black/40 p-2 backdrop-blur-xl">
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
                      setMobileExpanded(false);
                      onOpenConnect();
                    }}
                    onDone={() => setMobilePanel("player")}
                  />
                </div>
              </div>
            ) : (
              <div className="flex flex-1 flex-col justify-between py-6 gap-6">
                {/* Large album cover */}
                <div className="flex items-center justify-center flex-1 min-h-0">
                  <button
                    type="button"
                    disabled={!track?.albumId}
                    onClick={() => {
                      if (track?.albumId) {
                        setMobileExpanded(false);
                        onOpenAlbum?.(track.albumId);
                      }
                    }}
                    className="aspect-square w-full max-w-[320px] rounded-2xl overflow-hidden shadow-[0_24px_60px_rgba(0,0,0,0.65)] ring-1 ring-white/15 cursor-pointer disabled:cursor-default"
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
                <div className="min-w-0 space-y-0.5">
                  <h2 className="truncate text-xl font-semibold tracking-tight text-foreground">
                    {track?.title || "Nothing Playing"}
                  </h2>
                  <p className="truncate text-sm text-muted-foreground">
                    {track ? `${track.artist}${track.album ? ` · ${track.album}` : ""}` : "Select an album to begin"}
                  </p>
                </div>

                {/* Seek slider + centered quality badge */}
                <div className="space-y-1.5">
                  <Slider
                    aria-label="Seek position"
                    min={0}
                    max={duration || 1}
                    step={1}
                    value={[Math.min(shownPos, duration || 1)]}
                    disabled={!hasZone || duration === 0}
                    onValueChange={([v]) => setScrubPos(v)}
                    onValueCommit={([v]) => {
                      setScrubPos(null);
                      onSeek(v);
                    }}
                  />
                  <div className="grid grid-cols-[1fr_auto_1fr] items-center gap-2 text-[11px] font-mono tabular-nums text-muted-foreground">
                    <span>{fmtDuration(shownPos)}</span>
                    {qualLabel ? (
                      <Badge variant={hiRes ? "hires" : "muted"} className="px-2 py-0 text-[10px] leading-4 whitespace-nowrap">
                        {qualLabel}
                      </Badge>
                    ) : (
                      <span />
                    )}
                    <span className="text-right">{fmtDuration(duration)}</span>
                  </div>
                </div>

                {/* Transport controls */}
                <div className="flex items-center justify-between px-1">
                  <Button
                    type="button"
                    variant="ghost"
                    size="icon"
                    disabled={!hasZone}
                    onClick={() => onShuffle(!shuffle)}
                    aria-label={shuffle ? "Disable shuffle" : "Enable shuffle"}
                    aria-pressed={shuffle}
                    className={cn(
                      "relative h-11 w-11",
                      shuffle ? "text-emerald-400 hover:text-emerald-300" : "text-muted-foreground hover:text-foreground",
                    )}
                  >
                    <Shuffle className="h-5 w-5" />
                    {shuffle && <span className="absolute bottom-1 h-1 w-1 rounded-full bg-emerald-400" />}
                  </Button>
                  <Button
                    type="button"
                    variant="ghost"
                    size="icon"
                    disabled={!hasPlayable}
                    onClick={() => onTransport("prev")}
                    aria-label="Previous"
                    className="h-12 w-12"
                  >
                    <SkipBack className="h-6 w-6 fill-current" />
                  </Button>
                  <button
                    type="button"
                    disabled={!hasPlayable}
                    onClick={() => onTransport(playing ? "pause" : "play")}
                    aria-label={playing ? "Pause" : "Play"}
                    className="flex h-16 w-16 items-center justify-center rounded-full bg-white text-black shadow-xl active:scale-95 transition-transform disabled:opacity-40 disabled:cursor-not-allowed cursor-pointer"
                  >
                    {playing ? (
                      <Pause className="h-7 w-7 fill-current" />
                    ) : (
                      <Play className="h-7 w-7 fill-current translate-x-0.5" />
                    )}
                  </button>
                  <Button
                    type="button"
                    variant="ghost"
                    size="icon"
                    disabled={!hasPlayable}
                    onClick={() => onTransport("next")}
                    aria-label="Next"
                    className="h-12 w-12"
                  >
                    <SkipForward className="h-6 w-6 fill-current" />
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
                      "relative h-11 w-11",
                      repeat !== "off"
                        ? "text-emerald-400 hover:text-emerald-300"
                        : "text-muted-foreground hover:text-foreground",
                    )}
                  >
                    {repeat === "one" ? <Repeat1 className="h-5 w-5" /> : <Repeat className="h-5 w-5" />}
                    {repeat !== "off" && <span className="absolute bottom-1 h-1 w-1 rounded-full bg-emerald-400" />}
                  </Button>
                </div>

                {/* Volume + Output Device switch */}
                <div className="flex items-center gap-3 pt-1">
                  <Button
                    type="button"
                    variant="ghost"
                    size="icon-sm"
                    disabled={!hasZone}
                    onClick={() => onMute(!activeZone?.muted)}
                    aria-label={activeZone?.muted ? "Unmute" : "Mute"}
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
                    onValueChange={([v]) => setVolDraft(v)}
                    onValueCommit={([v]) => {
                      setVolDraft(null);
                      onVolume(v);
                    }}
                    className="flex-1"
                  />
                  <Button
                    type="button"
                    variant="secondary"
                    size="sm"
                    className="rounded-full text-xs shrink-0"
                    onClick={() => setMobilePanel("devices")}
                  >
                    {isBrowserActive ? <Laptop className="h-3.5 w-3.5" /> : <Speaker className="h-3.5 w-3.5" />}
                    <span>Devices</span>
                  </Button>
                </div>
              </div>
            )}
          </div>
        </DialogContent>
      </Dialog>
    </>
  );
}
