import { useQuery, useQueryClient } from "@tanstack/react-query";
import { AudioWaveform, CheckCircle2, Loader2, RefreshCw, Sparkles } from "lucide-react";
import { useEffect, useRef, useState } from "react";
import { tacetClient } from "@/api/client";
import { fmtCount } from "@/api/format";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Switch } from "@/components/ui/switch";
import { queryKeys } from "@/lib/query";
import { cn } from "@/lib/utils";

interface LibraryIntelligenceSectionProps {
  onNotify?: (title: string, detail?: string) => void;
  onError?: (message: string) => void;
}

function ProgressBar({
  value,
  max,
  active,
  indeterminate,
  tone = "violet",
}: {
  value: number;
  max: number;
  active: boolean;
  indeterminate?: boolean;
  tone?: "violet" | "sky";
}) {
  const pct = max > 0 ? Math.min(100, Math.round((value / max) * 100)) : 0;
  const fillClass = tone === "violet" ? "bg-violet-400" : "bg-sky-400";

  return (
    <div className="h-1.5 w-full overflow-hidden rounded-full bg-white/[0.08]">
      {indeterminate ? (
        <div className={cn("h-full w-1/3 rounded-full animate-pulse", fillClass)} />
      ) : (
        <div
          className={cn(
            "h-full rounded-full transition-[width] duration-300 ease-out",
            fillClass,
            active && "animate-pulse",
          )}
          style={{ width: `${pct}%` }}
        />
      )}
    </div>
  );
}

export function LibraryIntelligenceSection({ onNotify, onError }: LibraryIntelligenceSectionProps) {
  const queryClient = useQueryClient();
  const [sonicBusy, setSonicBusy] = useState(false);
  const [enrichBusy, setEnrichBusy] = useState(false);
  // Keep polling briefly after a manual trigger so we catch the background worker
  // flipping `running = true` even when the POST returns before the task wakes up.
  const [sonicKickoffUntil, setSonicKickoffUntil] = useState(0);
  const [enrichKickoffUntil, setEnrichKickoffUntil] = useState(0);

  const sonicQuery = useQuery({
    queryKey: queryKeys.sonic.status,
    queryFn: ({ signal }) => tacetClient.getSonicStatus({ signal }),
    refetchInterval: (q) => {
      const d = q.state.data;
      if (d && (d.running || d.downloading)) return 1000;
      if (Date.now() < sonicKickoffUntil) return 500;
      return false;
    },
  });

  const enrichQuery = useQuery({
    queryKey: queryKeys.enrichment.status,
    queryFn: ({ signal }) => tacetClient.getEnrichmentStatus({ signal }),
    refetchInterval: (q) => {
      if (q.state.data?.running) return 1000;
      if (Date.now() < enrichKickoffUntil) return 500;
      return false;
    },
  });

  const sonic = sonicQuery.data;
  const enrich = enrichQuery.data;

  // Detect live transition from running -> done so we refresh sonic queries & show a completion toast.
  const prevSonicRunningRef = useRef(false);
  useEffect(() => {
    const running = Boolean(sonic?.running || sonic?.downloading);
    if (prevSonicRunningRef.current && !running && sonic && !sonic.lastError) {
      void queryClient.invalidateQueries({ queryKey: queryKeys.sonic.all });
      onNotify?.(
        "Sonic Analysis Complete",
        `${fmtCount(sonic.analyzedTracks)} of ${fmtCount(sonic.totalLocalTracks)} local tracks indexed`,
      );
    }
    prevSonicRunningRef.current = running;
  }, [sonic, queryClient, onNotify]);

  const prevEnrichRunningRef = useRef(false);
  useEffect(() => {
    const running = Boolean(enrich?.running);
    if (prevEnrichRunningRef.current && !running && enrich && !enrich.lastError) {
      onNotify?.(
        "Metadata Enrichment Complete",
        `${fmtCount(enrich.matchedAlbums)} of ${fmtCount(enrich.totalAlbums)} albums matched`,
      );
    }
    prevEnrichRunningRef.current = running;
  }, [enrich, onNotify]);

  const handleToggleSonic = async (enabled: boolean) => {
    setSonicBusy(true);
    try {
      const next = await tacetClient.runSonic(enabled);
      queryClient.setQueryData(queryKeys.sonic.status, next);
      if (enabled) setSonicKickoffUntil(Date.now() + 8000);
      onNotify?.(
        enabled ? "Sonic Analysis Enabled" : "Sonic Analysis Paused",
        enabled
          ? next.modelsReady
            ? "Analyzing local tracks in the background…"
            : "Downloading Essentia ONNX models and starting analysis…"
          : "Existing sonic embeddings are kept on disk.",
      );
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
      setSonicKickoffUntil(Date.now() + 8000);
    } catch (err) {
      onError?.(err instanceof Error ? err.message : String(err));
    } finally {
      setSonicBusy(false);
    }
  };

  const handleRunEnrichment = async (retryFailed: boolean) => {
    setEnrichBusy(true);
    try {
      const next = await tacetClient.runEnrichment(retryFailed);
      queryClient.setQueryData(queryKeys.enrichment.status, next);
      setEnrichKickoffUntil(Date.now() + 8000);
      onNotify?.(
        retryFailed ? "Retrying Failed Albums" : "Metadata Enrichment Started",
        "Resolving MusicBrainz & release metadata in the background…",
      );
    } catch (err) {
      onError?.(err instanceof Error ? err.message : String(err));
    } finally {
      setEnrichBusy(false);
    }
  };

  const sonicActive = Boolean(sonic?.running || sonic?.downloading);
  const sonicDone =
    Boolean(sonic?.enabled) &&
    !sonicActive &&
    (sonic?.totalLocalTracks ?? 0) > 0 &&
    (sonic?.pendingTracks ?? 0) === 0;
  const sonicPct =
    sonic && sonic.totalLocalTracks > 0
      ? Math.min(100, Math.round((sonic.analyzedTracks / sonic.totalLocalTracks) * 100))
      : 0;

  const enrichActive = Boolean(enrich?.running);
  const enrichProcessed = enrich ? Math.max(0, enrich.totalAlbums - enrich.pendingAlbums) : 0;
  const enrichDone = Boolean(enrich) && !enrichActive && (enrich?.pendingAlbums ?? 0) === 0 && (enrich?.totalAlbums ?? 0) > 0;
  const enrichPct =
    enrich && enrich.totalAlbums > 0
      ? Math.min(100, Math.round((enrichProcessed / enrich.totalAlbums) * 100))
      : 0;

  return (
    <section className="rounded-2xl border border-white/[0.08] bg-white/[0.025] p-5 space-y-5">
      <div>
        <h2 className="text-base font-semibold text-white">Library Intelligence</h2>
        <p className="text-xs text-muted-foreground mt-0.5">
          Local audio analysis and metadata enrichment powering Sonically Similar views and Radio.
        </p>
      </div>

      {/* Local Sonic Analysis */}
      <div className="rounded-xl border border-white/[0.07] bg-white/[0.02] p-4 space-y-3">
        <div className="flex items-start justify-between gap-3">
          <div className="flex items-center gap-3 min-w-0">
            <div className="flex h-9 w-9 shrink-0 items-center justify-center rounded-xl bg-violet-400/10 text-violet-300 border border-violet-400/20">
              <AudioWaveform className="h-4.5 w-4.5" />
            </div>
            <div className="min-w-0">
              <div className="flex items-center gap-2 flex-wrap">
                <h3 className="text-sm font-semibold text-white">Sonic Analysis</h3>
                {sonic?.downloading ? (
                  <Badge variant="live">Downloading Models</Badge>
                ) : sonic?.running ? (
                  <Badge variant="live">Analyzing</Badge>
                ) : sonicDone ? (
                  <span className="inline-flex items-center gap-1 text-[11px] font-medium text-emerald-300">
                    <CheckCircle2 className="h-3.5 w-3.5" />
                    Up to date
                  </span>
                ) : null}
              </div>
              <p className="text-xs text-muted-foreground mt-0.5">
                Computes local Discogs-EffNet embeddings for Sonically Similar Tracks, Albums, and Artists.
              </p>
            </div>
          </div>

          <Switch
            aria-label="Enable Sonic Analysis"
            checked={Boolean(sonic?.enabled)}
            disabled={sonicBusy || sonicQuery.isLoading || sonic?.ffmpegAvailable === false}
            onCheckedChange={(checked) => void handleToggleSonic(checked)}
          />
        </div>

        {sonic && sonic.ffmpegAvailable === false && (
          <p className="text-xs text-amber-300">
            ffmpeg was not detected on this server — install ffmpeg to enable local Sonic Analysis.
          </p>
        )}

        {sonic && (sonic.enabled || sonic.analyzedTracks > 0) && (
          <div className="space-y-1.5">
            <ProgressBar
              value={sonic.analyzedTracks}
              max={sonic.totalLocalTracks}
              active={sonicActive}
              indeterminate={sonic.downloading}
              tone="violet"
            />
            <div className="flex items-center justify-between gap-2 text-[11px] text-muted-foreground">
              <span className="flex items-center gap-1.5">
                {sonicActive && <Loader2 className="h-3 w-3 animate-spin text-violet-300" />}
                {sonic.downloading
                  ? "Downloading Essentia ONNX weights (~25 MB)…"
                  : sonic.running
                    ? `Analyzing local tracks (${fmtCount(sonic.pendingTracks)} remaining)…`
                    : `${fmtCount(sonic.analyzedTracks)} of ${fmtCount(sonic.totalLocalTracks)} local tracks analyzed`}
              </span>
              <span className="font-mono tabular-nums">{sonicPct}%</span>
            </div>
          </div>
        )}

        {sonic?.lastError && (
          <p role="alert" className="text-xs text-red-300">
            {sonic.lastError}
          </p>
        )}

        {sonic?.enabled && !sonicActive && sonic.pendingTracks > 0 && (
          <div className="pt-0.5">
            <Button
              type="button"
              variant="secondary"
              size="sm"
              disabled={sonicBusy}
              onClick={() => void handleRunSonic()}
            >
              <RefreshCw className={cn("h-3.5 w-3.5", sonicBusy && "animate-spin")} />
              Analyze {fmtCount(sonic.pendingTracks)} Pending
            </Button>
          </div>
        )}
      </div>

      {/* Metadata Enrichment */}
      <div className="rounded-xl border border-white/[0.07] bg-white/[0.02] p-4 space-y-3">
        <div className="flex items-start justify-between gap-3">
          <div className="flex items-center gap-3 min-w-0">
            <div className="flex h-9 w-9 shrink-0 items-center justify-center rounded-xl bg-sky-400/10 text-sky-300 border border-sky-400/20">
              <Sparkles className="h-4.5 w-4.5" />
            </div>
            <div className="min-w-0">
              <div className="flex items-center gap-2 flex-wrap">
                <h3 className="text-sm font-semibold text-white">Metadata Enrichment</h3>
                {enrichActive ? (
                  <Badge variant="live">Enriching</Badge>
                ) : enrichDone ? (
                  <span className="inline-flex items-center gap-1 text-[11px] font-medium text-emerald-300">
                    <CheckCircle2 className="h-3.5 w-3.5" />
                    Complete
                  </span>
                ) : null}
              </div>
              <p className="text-xs text-muted-foreground mt-0.5">
                Resolves MusicBrainz IDs, genres, and release metadata for cross-provider Radio &amp; ListenBrainz.
              </p>
            </div>
          </div>

          <div className="flex items-center gap-1.5 shrink-0">
            {(enrich?.failedAlbums ?? 0) > 0 && !enrichActive && (
              <Button
                type="button"
                variant="secondary"
                size="sm"
                disabled={enrichBusy}
                onClick={() => void handleRunEnrichment(true)}
              >
                Retry Failed ({fmtCount(enrich!.failedAlbums)})
              </Button>
            )}
            <Button
              type="button"
              variant="secondary"
              size="sm"
              disabled={enrichBusy || enrichActive}
              onClick={() => void handleRunEnrichment(false)}
            >
              <RefreshCw className={cn("h-3.5 w-3.5", (enrichBusy || enrichActive) && "animate-spin")} />
              {enrichActive ? "Running…" : "Run"}
            </Button>
          </div>
        </div>

        {enrich && enrich.totalAlbums > 0 && (
          <div className="space-y-1.5">
            <ProgressBar
              value={enrichProcessed}
              max={enrich.totalAlbums}
              active={enrichActive}
              tone="sky"
            />
            <div className="flex items-center justify-between gap-2 text-[11px] text-muted-foreground">
              <span className="flex items-center gap-1.5">
                {enrichActive && <Loader2 className="h-3 w-3 animate-spin text-sky-300" />}
                {fmtCount(enrich.matchedAlbums)} matched
                {enrich.pendingAlbums > 0 && ` · ${fmtCount(enrich.pendingAlbums)} pending`}
                {enrich.unmatchedAlbums > 0 && ` · ${fmtCount(enrich.unmatchedAlbums)} unmatched`}
                {enrich.failedAlbums > 0 && ` · ${fmtCount(enrich.failedAlbums)} failed`}
              </span>
              <span className="font-mono tabular-nums">{enrichPct}%</span>
            </div>
          </div>
        )}

        {enrich?.lastError && (
          <p role="alert" className="text-xs text-red-300">
            {enrich.lastError}
          </p>
        )}
      </div>
    </section>
  );
}
