import { CheckCircle2, HardDrive, Plus, Radio, RefreshCw, Speaker } from "lucide-react";
import { useState } from "react";
import { tacetClient } from "@/api/client";
import type { LibraryStatus, QobuzStatus, StreamMode, ZoneSnapshot } from "@/api/types";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { invalidateQobuzLogout } from "@/lib/query";
import { cn } from "@/lib/utils";

interface SettingsViewProps {
  libraryStatus: LibraryStatus | null;
  qobuzStatus: QobuzStatus | null;
  streamMode: StreamMode;
  zones: ZoneSnapshot[];
  discovering: boolean;
  onStreamModeChange: (mode: StreamMode) => void;
  onQobuzStatusChange: (status: QobuzStatus) => void;
  onDiscover: () => void;
  onOpenConnect: () => void;
  onGoToQobuz: () => void;
}

export function SettingsView({
  libraryStatus,
  qobuzStatus,
  streamMode,
  zones,
  discovering,
  onStreamModeChange,
  onQobuzStatusChange,
  onDiscover,
  onOpenConnect,
  onGoToQobuz,
}: SettingsViewProps) {
  const [syncing, setSyncing] = useState(false);
  const [msg, setMsg] = useState("");
  const [errMsg, setErrMsg] = useState("");

  const handleSync = async (provider: "all" | "local" | "qobuz") => {
    setSyncing(true);
    setMsg("");
    setErrMsg("");
    try {
      await tacetClient.syncLibrary(provider, false);
      setMsg(provider === "all" ? "Library sync started in the background." : `Syncing ${provider}…`);
    } catch (err) {
      setErrMsg(err instanceof Error ? err.message : String(err));
    } finally {
      setSyncing(false);
    }
  };

  const handleLogoutQobuz = async () => {
    setErrMsg("");
    setMsg("");
    try {
      const st = await tacetClient.qobuzLogout();
      onQobuzStatusChange(st);
      invalidateQobuzLogout();
      setMsg("Signed out of Qobuz. Your play history is preserved.");
    } catch (err) {
      setErrMsg(err instanceof Error ? err.message : String(err));
    }
  };

  return (
    <div className="min-h-full pb-12">
      <div className="sticky top-0 z-20 border-b border-white/[0.06] bg-[#09090b]/45 px-4 sm:px-8 py-4 backdrop-blur-2xl">
        <div className="mx-auto max-w-3xl">
          <h1 className="text-xl sm:text-2xl font-bold tracking-tight text-white">Settings</h1>
        </div>
      </div>

      <div className="mx-auto max-w-3xl px-4 sm:px-8 pt-6 space-y-6">
        {msg && (
          <div className="flex items-center gap-2 rounded-xl border border-emerald-400/25 bg-emerald-400/10 px-4 py-2.5 text-sm text-emerald-200">
            <CheckCircle2 className="h-4 w-4 shrink-0 text-emerald-400" />
            <span>{msg}</span>
          </div>
        )}
        {errMsg && (
          <div role="alert" className="rounded-xl border border-red-500/30 bg-red-950/50 px-4 py-2.5 text-sm text-red-200">
            {errMsg}
          </div>
        )}

        {/* Unified Library & Sync */}
        <section className="rounded-2xl border border-white/[0.08] bg-white/[0.025] p-5 space-y-4">
          <div className="flex items-start justify-between gap-3">
            <div className="flex items-center gap-3">
              <div className="flex h-10 w-10 items-center justify-center rounded-xl bg-emerald-400/10 text-emerald-300 border border-emerald-400/20">
                <HardDrive className="h-5 w-5" />
              </div>
              <div>
                <h2 className="text-base font-semibold text-white">Library</h2>
                <p className="text-xs text-muted-foreground">
                  {libraryStatus
                    ? `${libraryStatus.albums} albums · ${libraryStatus.tracks} tracks (${libraryStatus.localAlbums ?? 0} local, ${libraryStatus.remoteAlbums ?? 0} Qobuz)`
                    : "Loading status…"}
                </p>
              </div>
            </div>
            {libraryStatus?.scanning && <Badge variant="live">Syncing</Badge>}
          </div>

          {(libraryStatus?.missingTracks ?? 0) > 0 && (
            <p className="text-xs text-amber-300/90">
              {libraryStatus!.missingTracks} missing local track(s) are soft-deleted and will restore automatically if
              their drive reconnects within the retention window.
            </p>
          )}

          <div className="flex flex-wrap gap-2">
            <Button
              type="button"
              variant="secondary"
              size="sm"
              disabled={syncing || Boolean(libraryStatus?.scanning)}
              onClick={() => void handleSync("all")}
            >
              <RefreshCw className={cn("h-3.5 w-3.5", (syncing || libraryStatus?.scanning) && "animate-spin")} />
              Sync All Sources
            </Button>
            <Button
              type="button"
              variant="ghost"
              size="sm"
              disabled={syncing || Boolean(libraryStatus?.scanning)}
              onClick={() => void handleSync("local")}
            >
              Rescan Local Files
            </Button>
          </div>
        </section>

        {/* Qobuz Account & Streaming Delivery */}
        <section className="rounded-2xl border border-white/[0.08] bg-white/[0.025] p-5 space-y-5">
          <div className="flex flex-wrap items-center justify-between gap-3">
            <div className="flex items-center gap-3">
              <div className="flex h-10 w-10 items-center justify-center rounded-xl bg-sky-400/10 text-sky-300 border border-sky-400/20">
                <Radio className="h-5 w-5" />
              </div>
              <div>
                <h2 className="text-base font-semibold text-white">Qobuz</h2>
                <p className="text-xs text-muted-foreground">
                  {qobuzStatus?.authenticated
                    ? `Signed in as ${qobuzStatus.userName || "Account"}${qobuzStatus.subscription ? ` (${qobuzStatus.subscription})` : ""}`
                    : "Not connected"}
                </p>
              </div>
            </div>

            <div className="flex items-center gap-2">
              {qobuzStatus?.authenticated ? (
                <>
                  <Button type="button" variant="secondary" size="sm" disabled={syncing} onClick={() => void handleSync("qobuz")}>
                    Sync Favorites
                  </Button>
                  <Button type="button" variant="ghost" size="sm" onClick={() => void handleLogoutQobuz()}>
                    Sign Out
                  </Button>
                </>
              ) : (
                <Button type="button" size="sm" onClick={onGoToQobuz}>
                  Connect Qobuz
                </Button>
              )}
            </div>
          </div>

          {/* Delivery Mode Radio Cards */}
          <div className="space-y-2 border-t border-white/[0.06] pt-4">
            <div className="text-xs font-medium text-foreground/90">Stream Delivery Mode</div>
            <div className="grid sm:grid-cols-2 gap-2.5">
              <button
                type="button"
                onClick={() => onStreamModeChange("direct")}
                className={cn(
                  "rounded-xl border p-3.5 text-left transition-colors cursor-pointer",
                  streamMode === "direct"
                    ? "border-emerald-400/40 bg-emerald-400/10"
                    : "border-white/[0.08] bg-white/[0.02] hover:bg-white/[0.05]",
                )}
              >
                <div className="flex items-center justify-between">
                  <span className="text-sm font-medium text-white">Direct Handoff</span>
                  {streamMode === "direct" && <Badge variant="live">Active</Badge>}
                </div>
                <p className="mt-1 text-xs text-muted-foreground leading-relaxed">
                  Renderer streams HTTPS FLAC directly from Qobuz CDN. Zero audio traffic through the NAS.
                </p>
              </button>

              <button
                type="button"
                onClick={() => onStreamModeChange("relay")}
                className={cn(
                  "rounded-xl border p-3.5 text-left transition-colors cursor-pointer",
                  streamMode === "relay"
                    ? "border-emerald-400/40 bg-emerald-400/10"
                    : "border-white/[0.08] bg-white/[0.02] hover:bg-white/[0.05]",
                )}
              >
                <div className="flex items-center justify-between">
                  <span className="text-sm font-medium text-white">Quiet Relay</span>
                  {streamMode === "relay" && <Badge variant="live">Active</Badge>}
                </div>
                <p className="mt-1 text-xs text-muted-foreground leading-relaxed">
                  NAS fetches HTTPS and streams raw bit-perfect bytes over plain LAN HTTP for older renderers.
                </p>
              </button>
            </div>
          </div>
        </section>

        {/* Playback Devices */}
        <section className="rounded-2xl border border-white/[0.08] bg-white/[0.025] p-5 space-y-4">
          <div className="flex flex-wrap items-center justify-between gap-3">
            <div className="flex items-center gap-3">
              <div className="flex h-10 w-10 items-center justify-center rounded-xl bg-white/[0.07] text-foreground border border-white/10">
                <Speaker className="h-5 w-5" />
              </div>
              <div>
                <h2 className="text-base font-semibold text-white">Playback Devices</h2>
                <p className="text-xs text-muted-foreground">
                  {zones.length} {zones.length === 1 ? "zone" : "zones"} available on the network
                </p>
              </div>
            </div>
            <div className="flex items-center gap-2">
              <Button type="button" variant="secondary" size="sm" disabled={discovering} onClick={onDiscover}>
                <RefreshCw className={cn("h-3.5 w-3.5", discovering && "animate-spin")} />
                {discovering ? "Scanning…" : "Scan Network"}
              </Button>
              <Button type="button" variant="ghost" size="sm" onClick={onOpenConnect}>
                <Plus className="h-3.5 w-3.5" />
                Connect URL
              </Button>
            </div>
          </div>
        </section>
      </div>
    </div>
  );
}
