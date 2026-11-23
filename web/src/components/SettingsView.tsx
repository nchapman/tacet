import { useQuery, useQueryClient } from "@tanstack/react-query";
import { Check, CheckCircle2, Compass, HardDrive, Plus, Radio, RefreshCw, Speaker } from "lucide-react";
import { useState } from "react";
import { tacetClient } from "@/api/client";
import type { LibraryStatus, QobuzStatus, StreamMode, ZoneSnapshot } from "@/api/types";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { PageHeader } from "@/components/ui/PageHeader";
import { Switch } from "@/components/ui/switch";
import { invalidateQobuzLogout, QOBUZ_SEARCH_GC_TIME_MS, QOBUZ_SEARCH_STALE_TIME_MS, queryKeys } from "@/lib/query";
import { cn } from "@/lib/utils";

interface SettingsViewProps {
  libraryStatus: LibraryStatus | null;
  qobuzStatus: QobuzStatus | null;
  streamMode: StreamMode;
  groupVersions: boolean;
  discoverGenreIds: number[];
  zones: ZoneSnapshot[];
  discovering: boolean;
  onStreamModeChange: (mode: StreamMode) => void;
  onGroupVersionsChange: (enabled: boolean) => void;
  onDiscoverGenreIdsChange: (ids: number[]) => void;
  onQobuzStatusChange: (status: QobuzStatus) => void;
  onDiscover: () => void;
  onOpenConnect: () => void;
  onGoToQobuz: () => void;
}

export function SettingsView({
  libraryStatus,
  qobuzStatus,
  streamMode,
  groupVersions,
  discoverGenreIds,
  zones,
  discovering,
  onStreamModeChange,
  onGroupVersionsChange,
  onDiscoverGenreIdsChange,
  onQobuzStatusChange,
  onDiscover,
  onOpenConnect,
  onGoToQobuz,
}: SettingsViewProps) {
  const queryClient = useQueryClient();
  const [syncing, setSyncing] = useState(false);
  const [msg, setMsg] = useState("");
  const [errMsg, setErrMsg] = useState("");

  const discoverQuery = useQuery({
    queryKey: queryKeys.discover.feed([]),
    queryFn: ({ signal }) => tacetClient.getDiscover(undefined, { signal }),
    enabled: Boolean(qobuzStatus?.authenticated),
    staleTime: QOBUZ_SEARCH_STALE_TIME_MS,
    gcTime: QOBUZ_SEARCH_GC_TIME_MS,
  });
  const discoverGenres = discoverQuery.data?.genres ?? [];

  const toggleDiscoverGenre = (genreId: number) => {
    if (discoverGenreIds.includes(genreId)) {
      onDiscoverGenreIdsChange(discoverGenreIds.filter((id) => id !== genreId));
    } else {
      onDiscoverGenreIdsChange([...discoverGenreIds, genreId]);
    }
  };

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
      invalidateQobuzLogout(queryClient);
      setMsg("Signed out of Qobuz. Your play history is preserved.");
    } catch (err) {
      setErrMsg(err instanceof Error ? err.message : String(err));
    }
  };

  return (
    <div className="min-h-full pb-12">
      <PageHeader>
        <div className="w-full max-w-3xl">
          <h1 className="text-xl sm:text-2xl font-bold tracking-tight text-white">Settings</h1>
        </div>
      </PageHeader>

      <div className="max-w-3xl px-4 sm:px-8 pt-6 space-y-6">
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
            <p className="text-xs text-muted-foreground">
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

          <div className="border-t border-white/[0.06] pt-4 flex items-center justify-between gap-4">
            <div className="min-w-0">
              <div className="text-sm font-medium text-white">Group album versions</div>
              <p className="text-xs text-muted-foreground">
                Combine duplicate local and Qobuz editions of the same album into a single entry.
              </p>
            </div>
            <Switch
              aria-label="Group album versions"
              checked={groupVersions}
              onCheckedChange={onGroupVersionsChange}
            />
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
                    ? "border-white/30 bg-white/[0.08]"
                    : "border-white/[0.08] bg-white/[0.02] hover:bg-white/[0.05]",
                )}
              >
                <div className="flex items-center justify-between gap-2">
                  <span className="text-sm font-medium text-white">Handoff</span>
                  {streamMode === "direct" && (
                    <span className="inline-flex items-center gap-1 text-[11px] font-medium text-white/90">
                      <Check className="h-3.5 w-3.5" />
                      Active
                    </span>
                  )}
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
                    ? "border-white/30 bg-white/[0.08]"
                    : "border-white/[0.08] bg-white/[0.02] hover:bg-white/[0.05]",
                )}
              >
                <div className="flex items-center justify-between gap-2">
                  <span className="text-sm font-medium text-white">Relay</span>
                  {streamMode === "relay" && (
                    <span className="inline-flex items-center gap-1 text-[11px] font-medium text-white/90">
                      <Check className="h-3.5 w-3.5" />
                      Active
                    </span>
                  )}
                </div>
                <p className="mt-1 text-xs text-muted-foreground leading-relaxed">
                  Server terminates HTTPS and streams raw bit-perfect bytes over plain LAN HTTP.
                </p>
              </button>
            </div>
          </div>
        </section>

        {/* Discovery Preferences */}
        {qobuzStatus?.authenticated && discoverGenres.length > 0 && (
          <section className="rounded-2xl border border-white/[0.08] bg-white/[0.025] p-5 space-y-4">
            <div className="flex flex-wrap items-center justify-between gap-3">
              <div className="flex items-center gap-3">
                <div className="flex h-10 w-10 items-center justify-center rounded-xl bg-[#d4af68]/10 text-[#dfc07b] border border-[#d4af68]/20">
                  <Compass className="h-5 w-5" />
                </div>
                <div>
                  <h2 className="text-base font-semibold text-white">Discovery Genres</h2>
                  <p className="text-xs text-muted-foreground">
                    {discoverGenreIds.length === 0
                      ? "Showing editorial picks across all genres on Home"
                      : `Filtering Home discovery shelves to ${discoverGenreIds.length} favorite ${discoverGenreIds.length === 1 ? "genre" : "genres"}`}
                  </p>
                </div>
              </div>
              {discoverGenreIds.length > 0 && (
                <Button
                  type="button"
                  variant="ghost"
                  size="sm"
                  onClick={() => onDiscoverGenreIdsChange([])}
                >
                  Reset to All
                </Button>
              )}
            </div>

            <div className="flex flex-wrap gap-2" role="group" aria-label="Favorite discovery genres">
              <button
                type="button"
                aria-pressed={discoverGenreIds.length === 0}
                onClick={() => onDiscoverGenreIdsChange([])}
                className={cn(
                  "inline-flex items-center gap-1.5 rounded-full px-3 py-1.5 text-xs font-medium transition-colors cursor-pointer border",
                  discoverGenreIds.length === 0
                    ? "border-white bg-white text-black"
                    : "border-white/[0.08] bg-white/[0.04] text-muted-foreground hover:bg-white/[0.08] hover:text-foreground",
                )}
              >
                {discoverGenreIds.length === 0 && <Check className="h-3 w-3" />}
                All Genres
              </button>
              {discoverGenres.map((genre) => {
                const selected = discoverGenreIds.includes(genre.id);
                return (
                  <button
                    key={genre.id}
                    type="button"
                    aria-pressed={selected}
                    onClick={() => toggleDiscoverGenre(genre.id)}
                    className={cn(
                      "inline-flex items-center gap-1.5 rounded-full px-3 py-1.5 text-xs font-medium transition-colors cursor-pointer border",
                      selected
                        ? "border-white bg-white text-black"
                        : "border-white/[0.08] bg-white/[0.04] text-muted-foreground hover:bg-white/[0.08] hover:text-foreground",
                    )}
                  >
                    {selected && <Check className="h-3 w-3" />}
                    {genre.name}
                  </button>
                );
              })}
            </div>
          </section>
        )}

        {/* Playback Devices */}
        <section className="rounded-2xl border border-white/[0.08] bg-white/[0.025] p-5 space-y-4">
          <div className="flex flex-wrap items-center justify-between gap-3">
            <div className="flex items-center gap-3">
              <div className="flex h-10 w-10 items-center justify-center rounded-xl bg-violet-400/10 text-violet-300 border border-violet-400/20">
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

          <div className="border-t border-white/[0.06] pt-3">
            {zones.length === 0 ? (
              <p className="text-xs text-muted-foreground py-1">
                No playback devices found on the network yet. Click Scan Network or Connect URL to add one.
              </p>
            ) : (
              <ul className="divide-y divide-white/[0.05]">
                {zones.map((z) => {
                  const isPlaying = z.state === "playing";
                  const isPaused = z.state === "paused";
                  const kindLabel = z.renderer.kind === "player" ? "Remote Player" : "UPnP / DLNA";
                  const model = [z.renderer.manufacturer, z.renderer.model].filter(Boolean).join(" ");
                  const meta = [kindLabel, model, z.renderer.host].filter(Boolean).join(" · ");
                  return (
                    <li key={z.zoneId} className="flex items-center justify-between gap-3 py-2.5 first:pt-1 last:pb-0">
                      <div className="min-w-0">
                        <div className="truncate text-sm font-medium text-white">{z.renderer.name}</div>
                        <div className="truncate text-xs text-muted-foreground">{meta}</div>
                      </div>
                      <span className="shrink-0 inline-flex items-center gap-1.5 text-xs text-muted-foreground">
                        <span
                          className={cn(
                            "h-1.5 w-1.5 rounded-full",
                            isPlaying ? "bg-emerald-400" : isPaused ? "bg-white/60" : "bg-white/30",
                          )}
                        />
                        {isPlaying ? "Playing" : isPaused ? "Paused" : "Ready"}
                      </span>
                    </li>
                  );
                })}
              </ul>
            )}
          </div>
        </section>
      </div>
    </div>
  );
}
