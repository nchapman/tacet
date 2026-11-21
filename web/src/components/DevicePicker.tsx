import { Check, Laptop, Plus, RefreshCw, Speaker, Volume2 } from "lucide-react";
import type { ZoneSnapshot } from "@/api/types";
import { Button } from "@/components/ui/button";
import type { BrowserPlayerController } from "@/hooks/useBrowserPlayer";
import { cn } from "@/lib/utils";

interface DeviceListProps {
  zones: ZoneSnapshot[];
  activeZoneId: string;
  discovering: boolean;
  browserPlayer: BrowserPlayerController;
  onSelectZone: (zoneId: string) => void;
  onDiscover: () => void;
  onOpenConnect: () => void;
  onDone?: () => void;
}

/**
 * Spotify Connect-style output selector listing UPnP renderers and the local
 * browser player endpoint, plus network scan and manual URL actions.
 */
export function DeviceList({
  zones,
  activeZoneId,
  discovering,
  browserPlayer,
  onSelectZone,
  onDiscover,
  onOpenConnect,
  onDone,
}: DeviceListProps) {
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
      <button
        type="button"
        disabled={browserPlayer.status === "connecting"}
        onClick={() => {
          if (browserPlayer.enabled && browserPlayer.zoneId) {
            onSelectZone(browserPlayer.zoneId);
            onDone?.();
          } else {
            void browserPlayer.enable(true).then(() => onDone?.());
          }
        }}
        className={cn(
          "flex w-full items-center gap-3 rounded-xl px-2.5 py-2.5 text-left transition-colors cursor-pointer",
          browserPlayer.enabled && browserPlayer.zoneId === activeZoneId
            ? "bg-emerald-500/12 text-emerald-300"
            : "hover:bg-white/[0.06] text-foreground",
        )}
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
          <div className="truncate text-sm font-medium">This Browser</div>
          <div className="truncate text-xs text-muted-foreground">
            {browserPlayer.status === "connecting"
              ? "Connecting audio…"
              : browserPlayer.enabled
                ? "Active web audio endpoint"
                : "Listen directly on this device"}
          </div>
        </div>
        {browserPlayer.enabled && (
          <div className="flex items-center gap-1">
            {browserPlayer.zoneId === activeZoneId && <Check className="h-4 w-4 text-emerald-400" />}
            <span
              role="button"
              tabIndex={0}
              onClick={(e) => {
                e.stopPropagation();
                void browserPlayer.disable();
              }}
              onKeyDown={(e) => {
                if (e.key === "Enter" || e.key === " ") {
                  e.preventDefault();
                  e.stopPropagation();
                  void browserPlayer.disable();
                }
              }}
              className="rounded-md px-1.5 py-0.5 text-[11px] text-muted-foreground hover:bg-white/10 hover:text-foreground"
            >
              Off
            </span>
          </div>
        )}
      </button>

      {/* Discovered UPnP & remote player zones (excluding the local browser zone already rendered above) */}
      {zones.filter((z) => z.zoneId !== browserPlayer.zoneId).length > 0 && (
        <div className="my-1 border-t border-white/[0.06] pt-1 space-y-0.5 max-h-80 overflow-y-auto">
          {zones
            .filter((z) => z.zoneId !== browserPlayer.zoneId)
            .map((z) => {
              const active = z.zoneId === activeZoneId;
              const playing = z.state === "playing";
              return (
                <button
                  key={z.zoneId}
                  type="button"
                  onClick={() => {
                    onSelectZone(z.zoneId);
                    onDone?.();
                  }}
                  className={cn(
                    "flex w-full items-center gap-3 rounded-xl px-2.5 py-2.5 text-left transition-colors cursor-pointer",
                    active ? "bg-emerald-500/12 text-emerald-300" : "hover:bg-white/[0.06] text-foreground",
                  )}
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
                    <div className="truncate text-sm font-medium">{z.renderer.name}</div>
                    <div className="truncate text-xs text-muted-foreground">
                      {z.renderer.model || (z.renderer.kind || "upnp").toUpperCase()}
                      {z.renderer.host ? ` · ${z.renderer.host}` : ""}
                    </div>
                  </div>
                  {active && <Check className="h-4 w-4 shrink-0 text-emerald-400" />}
                </button>
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
