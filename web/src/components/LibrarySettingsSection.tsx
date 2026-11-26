import { useQuery, useQueryClient } from "@tanstack/react-query";
import { AlertTriangle, HardDrive, Loader2, RefreshCw } from "lucide-react";
import { useEffect, useRef, useState } from "react";
import { tacetClient } from "@/api/client";
import { fmtCount } from "@/api/format";
import type { LibraryStatus, QobuzStatus } from "@/api/types";
import { Button } from "@/components/ui/button";
import { Switch } from "@/components/ui/switch";
import { queryKeys } from "@/lib/query";
import { cn } from "@/lib/utils";

const SCAN_INTERVAL_OPTIONS = [
  { minutes: 0, label: "Off" },
  { minutes: 15, label: "15m" },
  { minutes: 30, label: "30m" },
  { minutes: 60, label: "1h" },
  { minutes: 360, label: "6h" },
  { minutes: 1440, label: "24h" },
] as const;

interface LibrarySettingsSectionProps {
  libraryStatus: LibraryStatus | null;
  qobuzStatus: QobuzStatus | null;
  libraryWatchEnabled: boolean;
  libraryScanIntervalMin: number;
  groupVersions: boolean;
  onLibraryWatchChange: (enabled: boolean) => void;
  onLibraryScanIntervalChange: (minutes: number) => void;
  onGroupVersionsChange: (enabled: boolean) => void;
  onNotify?: (title: string, detail?: string) => void;
  onError?: (message: string) => void;
}

interface ActivityStage {
  id: string;
  label: string;
  pct?: number;
}

function StageRow({ stage }: { stage: ActivityStage }) {
  const determinate = stage.pct !== undefined;
  return (
    <div className="space-y-1.5">
      <div className="flex items-center justify-between gap-2 text-xs text-white/85">
        <span className="flex items-center gap-2 min-w-0 truncate">
          <Loader2 className="h-3.5 w-3.5 shrink-0 animate-spin text-emerald-400" />
          <span className="truncate">{stage.label}</span>
        </span>
        {determinate && (
          <span className="font-mono text-[11px] tabular-nums text-muted-foreground shrink-0">
            {stage.pct}%
          </span>
        )}
      </div>
      <div
        role="progressbar"
        aria-label={stage.label}
        aria-valuenow={determinate ? stage.pct : undefined}
        aria-valuemin={determinate ? 0 : undefined}
        aria-valuemax={determinate ? 100 : undefined}
        className="h-1 w-full overflow-hidden rounded-full bg-white/[0.08]"
      >
        {determinate ? (
          <div
            className="h-full rounded-full bg-emerald-400 transition-[width] duration-300 ease-out"
            style={{ width: `${stage.pct}%` }}
          />
        ) : (
          <div className="h-full w-1/3 rounded-full bg-emerald-400 animate-pulse" />
        )}
      </div>
    </div>
  );
}

function formatLibrarySubtitle(status: LibraryStatus | null): string {
  if (!status) return "Loading status…";
  const base = `${fmtCount(status.albums)} albums · ${fmtCount(status.tracks)} tracks`;
  const local = status.localAlbums ?? 0;
  const remote = status.remoteAlbums ?? 0;
  if (local > 0 && remote > 0) {
    return `${base} · ${fmtCount(local)} local, ${fmtCount(remote)} Qobuz`;
  }
  return base;
}

export function LibrarySettingsSection({
  libraryStatus,
  qobuzStatus,
  libraryWatchEnabled,
  libraryScanIntervalMin,
  groupVersions,
  onLibraryWatchChange,
  onLibraryScanIntervalChange,
  onGroupVersionsChange,
  onNotify,
  onError,
}: LibrarySettingsSectionProps) {
  const queryClient = useQueryClient();
  const [scanStarting, setScanStarting] = useState(false);
  const [sonicBusy, setSonicBusy] = useState(false);
  const [enrichBusy, setEnrichBusy] = useState(false);

  // Live updates arrive via SSE (`sonic.status` / `enrichment.status`) in useTacet;
  // these queries only hydrate on mount if not already populated by the SSE snapshot.
  const sonicQuery = useQuery({
    queryKey: queryKeys.sonic.status,
    queryFn: ({ signal }) => tacetClient.getSonicStatus({ signal }),
    staleTime: Number.POSITIVE_INFINITY,
  });

  const enrichQuery = useQuery({
    queryKey: queryKeys.enrichment.status,
    queryFn: ({ signal }) => tacetClient.getEnrichmentStatus({ signal }),
    staleTime: Number.POSITIVE_INFINITY,
  });

  const sonic = sonicQuery.data;
  const enrich = enrichQuery.data;

  // Invalidate sonic similarity queries once analysis finishes so views light up immediately.
  const prevSonicRunningRef = useRef(false);
  useEffect(() => {
    const running = Boolean(sonic?.running || sonic?.downloading);
    if (prevSonicRunningRef.current && !running && sonic && !sonic.lastError) {
      void queryClient.invalidateQueries({ queryKey: queryKeys.sonic.all });
    }
    prevSonicRunningRef.current = running;
  }, [sonic, queryClient]);

  const localConfigured = Boolean(libraryStatus?.configured);
  const qobuzConnected = Boolean(qobuzStatus?.authenticated);
  const scanning = Boolean(libraryStatus?.scanning);

  const handleScan = async () => {
    setScanStarting(true);
    try {
      const provider = localConfigured ? (qobuzConnected ? "all" : "local") : "qobuz";
      await tacetClient.syncLibrary(provider, false);
    } catch (err) {
      onError?.(err instanceof Error ? err.message : String(err));
    } finally {
      setScanStarting(false);
    }
  };

  const handleToggleSonic = async (enabled: boolean) => {
    setSonicBusy(true);
    try {
      const next = await tacetClient.runSonic(enabled);
      queryClient.setQueryData(queryKeys.sonic.status, next);
      if (enabled) {
        onNotify?.(
          "Sonic Analysis Enabled",
          next.modelsReady
            ? "Analyzing local audio in the background…"
            : "Downloading audio model (~25 MB) and starting analysis…",
        );
      }
    } catch (err) {
      onError?.(err instanceof Error ? err.message : String(err));
    } finally {
      setSonicBusy(false);
    }
  };

  const handleRunSonic = async () => {
    setSonicBusy(true);
    try {
      const next = await tacetClient.runSonic();
      queryClient.setQueryData(queryKeys.sonic.status, next);
    } catch (err) {
      onError?.(err instanceof Error ? err.message : String(err));
    } finally {
      setSonicBusy(false);
    }
  };

  const handleRetryEnrichment = async () => {
    setEnrichBusy(true);
    try {
      const next = await tacetClient.runEnrichment(true);
      queryClient.setQueryData(queryKeys.enrichment.status, next);
    } catch (err) {
      onError?.(err instanceof Error ? err.message : String(err));
    } finally {
      setEnrichBusy(false);
    }
  };

  // Build active stages for the unified transient Activity Strip.
  const activeStages: ActivityStage[] = [];
  if (scanning) {
    activeStages.push({ id: "scan", label: "Scanning library…" });
  }
  if (enrich?.running) {
    const done = Math.max(0, enrich.totalAlbums - enrich.pendingAlbums);
    const pct = enrich.totalAlbums > 0 ? Math.min(100, Math.round((done / enrich.totalAlbums) * 100)) : 0;
    activeStages.push({
      id: "enrich",
      label: `Enriching metadata · ${fmtCount(done)} of ${fmtCount(enrich.totalAlbums)} albums`,
      pct,
    });
  }
  if (sonic?.downloading) {
    activeStages.push({ id: "sonic-download", label: "Downloading audio model (~25 MB)…" });
  } else if (sonic?.running) {
    const pct =
      sonic.totalLocalTracks > 0
        ? Math.min(100, Math.round((sonic.analyzedTracks / sonic.totalLocalTracks) * 100))
        : 0;
    activeStages.push({
      id: "sonic-analyze",
      label: `Analyzing audio · ${fmtCount(sonic.analyzedTracks)} of ${fmtCount(sonic.totalLocalTracks)} tracks`,
      pct,
    });
  }

  const sonicActive = Boolean(sonic?.running || sonic?.downloading);
  const showFailedEnrich = !enrich?.running && (enrich?.failedAlbums ?? 0) > 0;
  const showPendingSonic = Boolean(sonic?.enabled) && !sonicActive && (sonic?.pendingTracks ?? 0) > 0;
  const missingTracks = libraryStatus?.missingTracks ?? 0;

  return (
    <section className="rounded-2xl border border-white/[0.08] bg-white/[0.025] p-5 space-y-4">
      {/* Header + Primary Action */}
      <div className="flex flex-wrap items-center justify-between gap-3">
        <div className="flex items-center gap-3 min-w-0">
          <div className="flex h-10 w-10 shrink-0 items-center justify-center rounded-xl bg-emerald-400/10 text-emerald-300 border border-emerald-400/20">
            <HardDrive className="h-5 w-5" />
          </div>
          <div className="min-w-0">
            <h2 className="text-base font-semibold text-white">Library</h2>
            <p className="text-xs text-muted-foreground">{formatLibrarySubtitle(libraryStatus)}</p>
          </div>
        </div>

        <Button
          type="button"
          variant="secondary"
          size="sm"
          disabled={scanStarting || scanning}
          onClick={() => void handleScan()}
        >
          <RefreshCw className={cn("h-3.5 w-3.5", (scanStarting || scanning) && "animate-spin")} />
          {scanning ? "Scanning…" : localConfigured ? "Scan Library" : "Sync Library"}
        </Button>
      </div>

      {/* Unified Activity Strip (only shown while any pipeline stage is running) */}
      {activeStages.length > 0 && (
        <div
          role="status"
          aria-live="polite"
          className="rounded-xl border border-white/[0.07] bg-white/[0.03] px-3.5 py-3 space-y-2.5"
        >
          {activeStages.map((stage) => (
            <StageRow key={stage.id} stage={stage} />
          ))}
        </div>
      )}

      {/* Actionable Attention Notices (silence means healthy) */}
      {sonic?.lastError && (
        <div
          role="alert"
          className="flex items-center justify-between gap-3 rounded-xl border border-red-500/25 bg-red-950/35 px-3.5 py-2 text-xs text-red-200"
        >
          <span className="truncate">Sonic analysis error: {sonic.lastError}</span>
          {sonic.enabled && !sonicActive && (
            <Button type="button" variant="ghost" size="sm" disabled={sonicBusy} onClick={() => void handleRunSonic()}>
              Retry
            </Button>
          )}
        </div>
      )}

      {enrich?.lastError && (
        <div
          role="alert"
          className="flex items-center justify-between gap-3 rounded-xl border border-red-500/25 bg-red-950/35 px-3.5 py-2 text-xs text-red-200"
        >
          <span className="truncate">Metadata enrichment error: {enrich.lastError}</span>
          {!enrich.running && (
            <Button
              type="button"
              variant="ghost"
              size="sm"
              disabled={enrichBusy}
              onClick={() => void handleRetryEnrichment()}
            >
              Retry
            </Button>
          )}
        </div>
      )}

      {missingTracks > 0 && (
        <p className="flex items-center gap-2 text-xs text-amber-200/80">
          <AlertTriangle className="h-3.5 w-3.5 shrink-0 text-amber-300/80" />
          <span>
            {fmtCount(missingTracks)} local {missingTracks === 1 ? "track" : "tracks"} missing — they&apos;ll restore
            automatically if their drive reconnects.
          </span>
        </p>
      )}

      {showFailedEnrich && !enrich?.lastError && (
        <div className="flex items-center justify-between gap-3 rounded-xl border border-white/[0.06] bg-white/[0.02] px-3.5 py-2 text-xs text-muted-foreground">
          <span className="flex items-center gap-2">
            <AlertTriangle className="h-3.5 w-3.5 shrink-0 text-amber-300/80" />
            {fmtCount(enrich!.failedAlbums)} {enrich!.failedAlbums === 1 ? "album" : "albums"} couldn&apos;t be
            enriched
          </span>
          <Button
            type="button"
            variant="ghost"
            size="sm"
            disabled={enrichBusy}
            onClick={() => void handleRetryEnrichment()}
          >
            Retry
          </Button>
        </div>
      )}

      {showPendingSonic && !sonic?.lastError && (
        <div className="flex items-center justify-between gap-3 rounded-xl border border-white/[0.06] bg-white/[0.02] px-3.5 py-2 text-xs text-muted-foreground">
          <span>
            {fmtCount(sonic!.pendingTracks)} local {sonic!.pendingTracks === 1 ? "track" : "tracks"} awaiting sonic
            analysis
          </span>
          <Button type="button" variant="ghost" size="sm" disabled={sonicBusy} onClick={() => void handleRunSonic()}>
            Analyze
          </Button>
        </div>
      )}

      {/* Flat Settings Rows: Ingestion -> Organization -> Intelligence */}
      {localConfigured && (
        <>
          <div className="border-t border-white/[0.06] pt-4 flex items-center justify-between gap-4">
            <div className="min-w-0">
              <div className="text-sm font-medium text-white">Watch for file changes</div>
              <p className="text-xs text-muted-foreground">
                Automatically update your library when local audio files are added, edited, or removed.
              </p>
            </div>
            <Switch
              aria-label="Watch for file changes"
              checked={libraryWatchEnabled}
              onCheckedChange={onLibraryWatchChange}
            />
          </div>

          <div className="border-t border-white/[0.06] pt-4 flex flex-wrap items-center justify-between gap-3">
            <div className="min-w-0">
              <div className="text-sm font-medium text-white">Scheduled scan</div>
              <p className="text-xs text-muted-foreground">
                Run a periodic background check. Helpful for network drives and NAS mounts.
              </p>
            </div>
            <div className="flex items-center gap-1 rounded-xl border border-white/[0.08] bg-white/[0.03] p-1">
              {SCAN_INTERVAL_OPTIONS.map((opt) => {
                const active = libraryScanIntervalMin === opt.minutes;
                return (
                  <button
                    key={opt.minutes}
                    type="button"
                    aria-pressed={active}
                    onClick={() => {
                      if (!active) onLibraryScanIntervalChange(opt.minutes);
                    }}
                    className={cn(
                      "rounded-lg px-2.5 py-1 text-xs font-medium transition-colors cursor-pointer",
                      active
                        ? "bg-white text-black"
                        : "text-muted-foreground hover:text-foreground hover:bg-white/[0.06]",
                    )}
                  >
                    {opt.label}
                  </button>
                );
              })}
            </div>
          </div>
        </>
      )}

      <div className="border-t border-white/[0.06] pt-4 flex items-center justify-between gap-4">
        <div className="min-w-0">
          <div className="text-sm font-medium text-white">Group album versions</div>
          <p className="text-xs text-muted-foreground">
            Combine multiple editions of the same album into a single entry.
          </p>
        </div>
        <Switch
          aria-label="Group album versions"
          checked={groupVersions}
          onCheckedChange={onGroupVersionsChange}
        />
      </div>

      {localConfigured && (
        <div className="border-t border-white/[0.06] pt-4 flex items-center justify-between gap-4">
          <div className="min-w-0">
            <div className="text-sm font-medium text-white">Sonic analysis</div>
            <p className="text-xs text-muted-foreground">
              Analyze local audio to power Sonically Similar tracks, albums, and artists. Downloads a ~25 MB model on
              first use.
            </p>
            {sonic?.ffmpegAvailable === false && (
              <p className="text-xs text-amber-300/90 mt-1">Requires ffmpeg on the server.</p>
            )}
          </div>
          <Switch
            aria-label="Sonic analysis"
            checked={Boolean(sonic?.enabled)}
            disabled={sonicBusy || sonicQuery.isLoading || sonic?.ffmpegAvailable === false}
            onCheckedChange={(checked) => void handleToggleSonic(checked)}
          />
        </div>
      )}
    </section>
  );
}
