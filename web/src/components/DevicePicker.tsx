import { Check, Laptop, Plus, RefreshCw, SlidersHorizontal, Speaker, Volume2 } from "lucide-react";
import { useState } from "react";
import { AUDIO_QUALITY_STOPS, audioQualityIndex, audioQualityStop } from "@/api/format";
import type { AudioQuality, ZoneSnapshot } from "@/api/types";
import { Button } from "@/components/ui/button";
import { Slider } from "@/components/ui/slider";
import type { BrowserPlayerController } from "@/hooks/useBrowserPlayer";
import { cn } from "@/lib/utils";

interface DeviceListProps {
  zones: ZoneSnapshot[];
  activeZoneId: string;
  discovering: boolean;
  transcodeAvailable: boolean;
  browserPlayer: BrowserPlayerController;
  onSelectZone: (zoneId: string) => void;
  onSetZoneQuality: (zoneId: string, quality: AudioQuality) => void;
  onDiscover: () => void;
  onOpenConnect: () => void;
  onDone?: () => void;
}

interface EndpointQualityPanelProps {
  zone: ZoneSnapshot;
  transcodeAvailable: boolean;
  onChange: (quality: AudioQuality) => void;
}

function EndpointQualityPanel({ zone, transcodeAvailable, onChange }: EndpointQualityPanelProps) {
  const current = zone.audioQuality ?? "original";
  const idx = audioQualityIndex(current);
  const stop = audioQualityStop(current);
  const isOpus = current.startsWith("opus_");
  const isUPnP = zone.renderer.kind !== "player";

  return (
    <div
      className="mx-1.5 mt-2 mb-1.5 rounded-xl border border-white/[0.08] bg-white/[0.03] px-3 py-2.5"
      onClick={(e) => e.stopPropagation()}
    >
      <div className="flex items-baseline justify-between gap-2 mb-2.5">
        <span className="text-[11px] font-semibold uppercase tracking-wider text-muted-foreground">Audio Quality</span>
        <div className="text-right">
          <span className="text-xs font-medium text-foreground">{stop.label}</span>
          <span className="ml-1.5 text-[11px] text-muted-foreground">{stop.detail}</span>
        </div>
      </div>

      {/* Inset slider so Radix thumb centers (radius 7px) align dead-center over the 6 grid columns */}
      <div className="px-[calc(100%/12-7px)]">
        <Slider
          aria-label={`Audio quality for ${zone.renderer.name}`}
          min={0}
          max={AUDIO_QUALITY_STOPS.length - 1}
          step={1}
          alwaysShowThumb
          disabled={!transcodeAvailable}
          value={[idx]}
          onValueChange={([v]) => {
            const next = AUDIO_QUALITY_STOPS[v];
            if (next && next.value !== current) {
              onChange(next.value);
            }
          }}
          className="my-1"
        />
      </div>

      <div className="mt-2 grid grid-cols-6 text-center">
        {AUDIO_QUALITY_STOPS.map((s) => {
          const selected = s.value === current;
          return (
            <button
              key={s.value}
              type="button"
              disabled={!transcodeAvailable && s.value !== "original"}
              onClick={() => {
                if (s.value !== current) onChange(s.value);
              }}
              className={cn(
                "mx-0.5 rounded px-0.5 py-0.5 text-[10px] font-medium transition-colors cursor-pointer disabled:opacity-40 disabled:cursor-not-allowed",
                selected ? "text-emerald-300 bg-emerald-500/15" : "text-muted-foreground hover:text-foreground",
              )}
              title={`${s.label} (${s.detail})`}
            >
              {s.shortLabel}
            </button>
          );
        })}
      </div>

      {!transcodeAvailable ? (
        <p className="mt-2 text-[11px] leading-4 text-amber-300/80">
          Install ffmpeg with libopus on the server to enable transcoding.
        </p>
      ) : (
        <p className="mt-2 text-[11px] leading-4 text-muted-foreground">
          {isOpus && isUPnP ? "Applies on next track · Opus requires speaker support." : "Applies on next track."}
        </p>
      )}
    </div>
  );
}

/**
 * Spotify Connect-style output selector listing UPnP renderers and the local
 * browser player endpoint, with per-endpoint audio quality controls, network
 * scan, and manual URL actions.
 */
export function DeviceList({
  zones,
  activeZoneId,
  discovering,
  transcodeAvailable,
  browserPlayer,
  onSelectZone,
  onSetZoneQuality,
  onDiscover,
  onOpenConnect,
  onDone,
}: DeviceListProps) {
  const [editingZoneId, setEditingZoneId] = useState<string | null>(null);

  const browserZone = browserPlayer.zoneId ? zones.find((z) => z.zoneId === browserPlayer.zoneId) : undefined;
  const browserQualityStop = browserZone ? audioQualityStop(browserZone.audioQuality) : null;
  const browserNonOriginal = Boolean(browserZone?.audioQuality && browserZone.audioQuality !== "original");

  const toggleQualityEditor = (zoneId: string) => {
    setEditingZoneId((prev) => (prev === zoneId ? null : zoneId));
  };

  return (
    <div className="flex flex-col gap-1">
      <div className="flex items-center justify-between px-2.5 pt-1.5 pb-1">
        <span className="text-[11px] font-semibold uppercase tracking-wider text-muted-foreground">
          Connect to a device
        </span>
        <button
          type="button"
          onClick={onDiscover}
          disabled={discovering}
          className="inline-flex items-center gap-1 rounded-md px-1.5 py-0.5 text-[11px] text-muted-foreground hover:bg-white/[0.08] hover:text-foreground transition-colors cursor-pointer disabled:opacity-50"
          title="Scan network for UPnP renderers"
        >
          <RefreshCw className={cn("h-3 w-3", discovering && "animate-spin")} />
          <span>{discovering ? "Scanning" : "Scan"}</span>
        </button>
      </div>

      {/* Local browser player endpoint */}
      <div>
        <div
          className={cn(
            "flex w-full items-center gap-1.5 rounded-xl px-2.5 py-2 transition-colors",
            browserPlayer.enabled && browserPlayer.zoneId === activeZoneId
              ? "bg-emerald-500/12 text-emerald-300"
              : "hover:bg-white/[0.06] text-foreground",
          )}
        >
          <button
            type="button"
            disabled={browserPlayer.status === "connecting"}
            onClick={() => {
              if (browserPlayer.enabled && browserPlayer.zoneId) {
                onSelectZone(browserPlayer.zoneId);
                onDone?.();
              } else {
                void browserPlayer.enable(false).then((zid) => {
                  if (zid) onSelectZone(zid);
                  onDone?.();
                });
              }
            }}
            className="flex min-w-0 flex-1 items-center gap-3 text-left cursor-pointer disabled:opacity-50"
          >
            <div
              className={cn(
                "flex h-9 w-9 shrink-0 items-center justify-center rounded-lg",
                browserPlayer.enabled && browserPlayer.zoneId === activeZoneId
                  ? "bg-emerald-500/20 text-emerald-300"
                  : "bg-white/[0.06] text-muted-foreground",
              )}
            >
              <Laptop className="h-4 w-4" />
            </div>
            <div className="min-w-0 flex-1">
              <div className="flex items-center gap-1.5">
                <span className="truncate text-sm font-medium">This Browser</span>
                {browserNonOriginal && browserQualityStop && (
                  <span className="shrink-0 rounded bg-white/[0.08] px-1.5 py-0.5 text-[10px] font-mono text-muted-foreground">
                    {browserQualityStop.shortLabel}
                  </span>
                )}
              </div>
              <div className="truncate text-xs text-muted-foreground">
                {browserPlayer.status === "connecting"
                  ? "Connecting audio…"
                  : browserPlayer.enabled
                    ? "Active web audio endpoint"
                    : "Listen directly on this device"}
              </div>
            </div>
          </button>

          {browserPlayer.enabled && (
            <div className="flex shrink-0 items-center gap-1">
              {browserZone && (
                <button
                  type="button"
                  onClick={() => toggleQualityEditor(browserZone.zoneId)}
                  className={cn(
                    "flex h-7 w-7 items-center justify-center rounded-lg transition-colors cursor-pointer",
                    editingZoneId === browserZone.zoneId
                      ? "bg-white/15 text-foreground"
                      : "text-muted-foreground hover:bg-white/10 hover:text-foreground",
                  )}
                  title="Audio quality"
                  aria-label="Configure audio quality for This Browser"
                >
                  <SlidersHorizontal className="h-3.5 w-3.5" />
                </button>
              )}
              {browserPlayer.zoneId === activeZoneId && <Check className="h-4 w-4 text-emerald-400" />}
            </div>
          )}
        </div>

        {browserZone && editingZoneId === browserZone.zoneId && (
          <EndpointQualityPanel
            zone={browserZone}
            transcodeAvailable={transcodeAvailable}
            onChange={(q) => onSetZoneQuality(browserZone.zoneId, q)}
          />
        )}
      </div>

      {/* Discovered UPnP & remote player zones (excluding the local browser zone already rendered above) */}
      {zones.filter((z) => z.zoneId !== browserPlayer.zoneId).length > 0 && (
        <div className="my-1 border-t border-white/[0.06] pt-1 space-y-0.5 max-h-80 overflow-y-auto">
          {zones
            .filter((z) => z.zoneId !== browserPlayer.zoneId)
            .map((z) => {
              const active = z.zoneId === activeZoneId;
              const playing = z.state === "playing";
              const qStop = audioQualityStop(z.audioQuality);
              const nonOriginal = Boolean(z.audioQuality && z.audioQuality !== "original");
              const editing = editingZoneId === z.zoneId;

              return (
                <div key={z.zoneId}>
                  <div
                    className={cn(
                      "flex w-full items-center gap-1.5 rounded-xl px-2.5 py-2 transition-colors",
                      active ? "bg-emerald-500/12 text-emerald-300" : "hover:bg-white/[0.06] text-foreground",
                    )}
                  >
                    <button
                      type="button"
                      onClick={() => {
                        onSelectZone(z.zoneId);
                        onDone?.();
                      }}
                      className="flex min-w-0 flex-1 items-center gap-3 text-left cursor-pointer"
                    >
                      <div
                        className={cn(
                          "flex h-9 w-9 shrink-0 items-center justify-center rounded-lg",
                          active ? "bg-emerald-500/20 text-emerald-300" : "bg-white/[0.06] text-muted-foreground",
                        )}
                      >
                        {playing ? <Volume2 className="h-4 w-4 animate-pulse" /> : <Speaker className="h-4 w-4" />}
                      </div>
                      <div className="min-w-0 flex-1">
                        <div className="flex items-center gap-1.5">
                          <span className="truncate text-sm font-medium">{z.renderer.name}</span>
                          {nonOriginal && (
                            <span className="shrink-0 rounded bg-white/[0.08] px-1.5 py-0.5 text-[10px] font-mono text-muted-foreground">
                              {qStop.shortLabel}
                            </span>
                          )}
                        </div>
                        <div className="truncate text-xs text-muted-foreground">
                          {z.renderer.model || (z.renderer.kind || "upnp").toUpperCase()}
                          {z.renderer.host ? ` · ${z.renderer.host}` : ""}
                        </div>
                      </div>
                    </button>

                    <div className="flex shrink-0 items-center gap-1">
                      <button
                        type="button"
                        onClick={() => toggleQualityEditor(z.zoneId)}
                        className={cn(
                          "flex h-7 w-7 items-center justify-center rounded-lg transition-colors cursor-pointer",
                          editing
                            ? "bg-white/15 text-foreground"
                            : "text-muted-foreground hover:bg-white/10 hover:text-foreground",
                        )}
                        title="Audio quality"
                        aria-label={`Configure audio quality for ${z.renderer.name}`}
                      >
                        <SlidersHorizontal className="h-3.5 w-3.5" />
                      </button>
                      {active && <Check className="h-4 w-4 shrink-0 text-emerald-400" />}
                    </div>
                  </div>

                  {editing && (
                    <EndpointQualityPanel
                      zone={z}
                      transcodeAvailable={transcodeAvailable}
                      onChange={(q) => onSetZoneQuality(z.zoneId, q)}
                    />
                  )}
                </div>
              );
            })}
        </div>
      )}

      {zones.length === 0 && !browserPlayer.enabled && (
        <p className="px-2.5 py-3 text-xs text-muted-foreground">
          No UPnP renderers discovered yet. Tap Scan, connect by URL, or listen in this browser.
        </p>
      )}

      <div className="mt-1 border-t border-white/[0.06] pt-1.5">
        <Button
          type="button"
          variant="ghost"
          size="sm"
          className="w-full justify-start text-xs text-muted-foreground hover:text-foreground"
          onClick={() => {
            onDone?.();
            onOpenConnect();
          }}
        >
          <Plus className="h-3.5 w-3.5" />
          Connect renderer by URL…
        </Button>
      </div>
    </div>
  );
}
