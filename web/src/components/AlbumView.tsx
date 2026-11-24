import { useQuery, useQueryClient } from "@tanstack/react-query";
import { ArrowLeft, Award, Check, Clock3, FileText, Layers, ListPlus, Pause, Play, Plus, SkipForward, Sparkles, Users } from "lucide-react";
import { useState } from "react";
import { tacetClient } from "@/api/client";
import { fmtAlbumQuality, fmtDuration, fmtQuality } from "@/api/format";
import type { Album, AlbumVersion, QueuePosition, QueueSource } from "@/api/types";
import { AlbumCard } from "@/components/AlbumCard";
import { AlbumArt } from "@/components/ui/artwork";
import { Button } from "@/components/ui/button";
import { Dialog, DialogContent, DialogDescription, DialogTitle } from "@/components/ui/dialog";
import { EditorialNote } from "@/components/ui/EditorialNote";
import { ExplicitBadge } from "@/components/ui/ExplicitBadge";
import { PageBody, PageHeader } from "@/components/ui/PageHeader";
import { PlayingBars } from "@/components/ui/PlayingBars";
import { SignalBadge } from "@/components/ui/SignalBadge";
import { QOBUZ_ALBUM_GC_TIME_MS, QOBUZ_ALBUM_STALE_TIME_MS, queryKeys } from "@/lib/query";
import { cn } from "@/lib/utils";

interface AlbumViewProps {
  album: Album;
  backLabel: string;
  qobuzConnected?: boolean;
  loadingTracks?: boolean;
  loadError?: string;
  currentTrackId?: string;
  isPlaying?: boolean;
  onBack: () => void;
  onOpenAlbum: (albumId: string) => void;
  onOpenArtist?: (artist: string) => void;
  onAlbumUpdated?: (album: Album) => void;
  onPlay: (source: QueueSource, startIndex?: number) => void;
  onTogglePlayPause?: () => void;
  onAdd: (source: QueueSource, position?: QueuePosition) => void;
}

function sourceLabel(provider?: string, source?: string): string {
  const p = (provider || source || "local").toLowerCase();
  if (p === "local") return "Local";
  if (p === "qobuz") return "Qobuz";
  return p.charAt(0).toUpperCase() + p.slice(1);
}

function formatVersionSpec(v: AlbumVersion): string {
  const parts: string[] = [];
  if (v.primaryFormat) parts.push(v.primaryFormat);
  if (v.maxBitDepth && v.maxSampleRate) {
    const khz = v.maxSampleRate % 1000 === 0 ? `${v.maxSampleRate / 1000}` : (v.maxSampleRate / 1000).toFixed(1);
    parts.push(`${v.maxBitDepth}/${khz}k`);
  }
  return parts.join(" · ");
}

function totalAlbumDuration(album: Album): string {
  const secs = album.durationSec || (album.tracks ?? []).reduce((acc, t) => acc + (t.durationSec || 0), 0);
  if (!secs) return "";
  const mins = Math.round(secs / 60);
  if (mins < 60) return `${mins} min`;
  const h = Math.floor(mins / 60);
  const m = mins % 60;
  return `${h} hr ${m} min`;
}

function fmtReleaseDate(iso?: string): string {
  if (!iso || !/^\d{4}-\d{2}-\d{2}$/.test(iso)) return "";
  const [y, m, d] = iso.split("-").map(Number);
  const dt = new Date(Date.UTC(y, m - 1, d));
  if (Number.isNaN(dt.getTime())) return "";
  return dt.toLocaleDateString("en-US", { year: "numeric", month: "long", day: "numeric", timeZone: "UTC" });
}

export function AlbumView({
  album,
  backLabel,
  qobuzConnected = false,
  loadingTracks = false,
  loadError,
  currentTrackId,
  isPlaying,
  onBack,
  onOpenAlbum,
  onOpenArtist,
  onAlbumUpdated,
  onPlay,
  onTogglePlayPause,
  onAdd,
}: AlbumViewProps) {
  const queryClient = useQueryClient();
  const canSuggest = qobuzConnected && Boolean(album.id);
  const similarQuery = useQuery({
    queryKey: queryKeys.discover.similarAlbums(album.id),
    queryFn: ({ signal }) => tacetClient.getSimilarAlbums(album.id, 12, { signal }),
    enabled: canSuggest,
    staleTime: QOBUZ_ALBUM_STALE_TIME_MS,
    gcTime: QOBUZ_ALBUM_GC_TIME_MS,
  });
  const similarAlbums = (similarQuery.data?.items ?? [])
    .filter((a) => a.id !== album.id)
    .slice(0, 6);
  const [busyLibrary, setBusyLibrary] = useState(false);
  const [busyPrimary, setBusyPrimary] = useState(false);
  const [creditsOpen, setCreditsOpen] = useState(false);
  const [addedTrackIds, setAddedTrackIds] = useState<Record<string, boolean>>({});
  const [actionErr, setActionErr] = useState("");

  const isRemote = album.sourceType === "remote" || album.source !== "local";
  const provider = (album.provider || album.source || "local").toLowerCase();
  const externalId = album.externalId || album.id.replace(`${provider}:`, "");
  const qLabel = fmtAlbumQuality(album);
  const fullFormat =
    qLabel && qLabel.startsWith("DSD")
      ? qLabel
      : [album.primaryFormat, qLabel].filter(Boolean).join(" ");
  const versions = album.versions ?? [];
  const tracks = album.tracks ?? [];
  const totalTime = totalAlbumDuration(album);
  const isAlbumActive = Boolean(currentTrackId && tracks.some((t) => t.id === currentTrackId));
  const isAlbumPlaying = isAlbumActive && isPlaying;

  const handleToggleLibrary = async () => {
    if (!isRemote || busyLibrary) return;
    setBusyLibrary(true);
    setActionErr("");
    try {
      if (album.inLibrary) {
        await tacetClient.removeRemoteAlbum(album.id);
        onAlbumUpdated?.({
          ...album,
          inLibrary: false,
          versions: undefined,
          versionCount: 1,
          tracks: album.tracks?.map((t) => ({ ...t, inLibrary: false })),
        });
      } else {
        const saved = await tacetClient.addRemoteAlbum(provider, externalId);
        onAlbumUpdated?.({ ...album, ...saved, inLibrary: true });
      }
    } catch (err) {
      setActionErr(err instanceof Error ? err.message : String(err));
    } finally {
      setBusyLibrary(false);
    }
  };

  const handleMakePrimary = async () => {
    if (busyPrimary) return;
    setBusyPrimary(true);
    setActionErr("");
    try {
      const updated = await tacetClient.setPrimaryVersion(album.id);
      onAlbumUpdated?.(updated);
    } catch (err) {
      setActionErr(err instanceof Error ? err.message : String(err));
    } finally {
      setBusyPrimary(false);
    }
  };

  const handleAddTrack = async (trackId: string, trackExtId?: string) => {
    setActionErr("");
    try {
      await tacetClient.addRemoteTrack(provider, trackExtId || trackId.replace(`${provider}:`, ""));
      setAddedTrackIds((prev) => ({ ...prev, [trackId]: true }));
      void queryClient.invalidateQueries({ queryKey: queryKeys.library.all });
    } catch (err) {
      setActionErr(err instanceof Error ? err.message : String(err));
    }
  };

  return (
    <div className="flex h-full min-h-0 flex-col">
      <PageHeader className="justify-between py-3">
        <Button type="button" variant="ghost" size="sm" onClick={onBack} className="-ml-2 text-foreground/85">
          <ArrowLeft className="h-4 w-4" />
          <span>{backLabel}</span>
        </Button>
        <SignalBadge provider={album.provider || album.source} format={fullFormat} size="md" />
      </PageHeader>

      <PageBody>
        <div className="px-4 sm:px-8 pt-6 sm:pt-8">
          {loadError && (
            <div role="alert" className="mb-6 rounded-xl border border-red-500/30 bg-red-950/50 px-4 py-3 text-sm text-red-200">
              {loadError}
            </div>
          )}
          {actionErr && (
            <div role="alert" className="mb-6 rounded-xl border border-red-500/30 bg-red-950/50 px-4 py-2.5 text-sm text-red-200">
              {actionErr}
            </div>
          )}

        {/* Hero header: Cover + Typography + Actions */}
        <div className="flex flex-col sm:flex-row items-center sm:items-start gap-6 sm:gap-8">
          <div className="h-48 w-48 sm:h-52 sm:w-52 lg:h-60 lg:w-60 shrink-0 overflow-hidden rounded-2xl shadow-[0_24px_60px_rgba(0,0,0,0.65)] ring-1 ring-white/15">
            <AlbumArt artworkUrl={album.artworkUrl} alt={album.title} size="lg" className="h-full w-full" priority />
          </div>

          <div className="flex flex-col items-center sm:items-start text-center sm:text-left min-w-0 flex-1">
            <span className="text-[11px] font-semibold uppercase tracking-[0.14em] text-foreground/65">
              {album.genre || "Album"}
            </span>
            <h1 className="mt-1.5 text-2xl sm:text-4xl lg:text-[42px] font-bold tracking-tight text-white leading-[1.1] break-words">
              {album.title}
            </h1>

            <div className="mt-3 flex flex-wrap items-center justify-center sm:justify-start gap-x-2 gap-y-1 text-sm text-foreground/80">
              {onOpenArtist ? (
                <button
                  type="button"
                  onClick={() => onOpenArtist(album.artist)}
                  className="font-semibold text-white hover:underline cursor-pointer"
                >
                  {album.artist}
                </button>
              ) : (
                <span className="font-semibold text-white">{album.artist}</span>
              )}
              {album.year ? <span className="text-muted-foreground">· {album.year}</span> : null}
              <span className="text-muted-foreground">· {album.trackCount || tracks.length} tracks</span>
              {totalTime && <span className="text-muted-foreground">· {totalTime}</span>}
              {album.label && <span className="text-muted-foreground">· {album.label}</span>}
              {album.playCount ? <span className="text-muted-foreground">· {album.playCount} plays</span> : null}
            </div>

            {/* Primary Action Bar */}
            <div className="mt-5 flex flex-wrap items-center justify-center sm:justify-start gap-2.5">
              <Button
                type="button"
                size="lg"
                onClick={() => {
                  if (isAlbumActive && onTogglePlayPause) {
                    onTogglePlayPause();
                  } else {
                    onPlay({ type: "album", albumId: album.id }, 0);
                  }
                }}
                className="min-w-[114px] bg-white text-black hover:bg-white/90 shadow-lg"
              >
                {isAlbumPlaying ? (
                  <>
                    <Pause className="h-4 w-4 fill-current" />
                    Pause
                  </>
                ) : (
                  <>
                    <Play className="h-4 w-4 fill-current" />
                    Play
                  </>
                )}
              </Button>
              <Button
                type="button"
                variant="secondary"
                className="rounded-full h-10 px-4"
                onClick={() => onAdd({ type: "album", albumId: album.id }, "next")}
              >
                <SkipForward className="h-4 w-4" />
                Play Next
              </Button>
              <Button
                type="button"
                variant="secondary"
                className="rounded-full h-10 px-4"
                onClick={() => onAdd({ type: "album", albumId: album.id }, "end")}
              >
                <ListPlus className="h-4 w-4" />
                Queue
              </Button>
              {isRemote && (
                <Button
                  type="button"
                  variant="secondary"
                  disabled={busyLibrary}
                  onClick={() => void handleToggleLibrary()}
                  className={cn(
                    "rounded-full h-10 px-4",
                    album.inLibrary && "border-emerald-400/35 bg-emerald-400/12 text-emerald-300 hover:bg-emerald-400/20",
                  )}
                >
                  {album.inLibrary ? <Check className="h-4 w-4" /> : <Plus className="h-4 w-4" />}
                  {busyLibrary ? "Syncing…" : album.inLibrary ? "In Library" : "Add to Library"}
                </Button>
              )}
            </div>
            {album.awards && album.awards.length > 0 && (
              <div className="mt-3.5 flex flex-wrap items-center justify-center sm:justify-start gap-2">
                {album.awards.slice(0, 2).map((aw) => (
                  <span
                    key={aw}
                    className="inline-flex items-center gap-1 rounded-full border border-sky-400/25 bg-sky-400/10 px-2.5 py-0.5 text-[11px] font-medium text-sky-300"
                  >
                    <Award className="h-3 w-3 shrink-0" />
                    {aw}
                  </span>
                ))}
              </div>
            )}

            {album.description && (
              <EditorialNote
                text={album.description}
                title={album.title}
                subtitle={[album.artist, album.year, album.label].filter(Boolean).join(" · ")}
                targetWords={35}
                className={cn("max-w-2xl", album.awards?.length ? "mt-3.5" : "mt-5")}
              />
            )}
          </div>
        </div>

        {/* Alternate Editions / Versions Bar (Roon style) */}
        {versions.length > 1 && (
          <div className="mt-7 flex flex-wrap items-center gap-2 rounded-2xl border border-white/[0.08] bg-white/[0.03] px-4 py-3">
            <span className="inline-flex items-center gap-1.5 text-xs font-medium text-muted-foreground mr-1">
              <Layers className="h-3.5 w-3.5" />
              Versions ({versions.length})
            </span>
            {versions.map((v) => {
              const active = v.id === album.id;
              const vProv = (v.provider || v.source || "local").toLowerCase();
              return (
                <button
                  key={v.id}
                  type="button"
                  onClick={() => !active && onOpenAlbum(v.id)}
                  className={cn(
                    "inline-flex items-center gap-2 rounded-full border px-3 py-1.5 text-xs transition-colors cursor-pointer",
                    active
                      ? "border-white/25 bg-white/[0.12] text-white font-medium"
                      : "border-white/[0.08] bg-white/[0.03] text-muted-foreground hover:text-foreground hover:bg-white/[0.07]",
                  )}
                >
                  <span
                    className={cn(
                      "h-1.5 w-1.5 rounded-full",
                      vProv === "local" ? "bg-emerald-400" : "bg-sky-400",
                    )}
                  />
                  <span>{sourceLabel(v.provider, v.source)}</span>
                  {formatVersionSpec(v) && <span className="font-mono text-[11px] opacity-80">{formatVersionSpec(v)}</span>}
                  <span className="opacity-60">{v.trackCount}t</span>
                  {v.isPrimary && (
                    <span className="rounded-full bg-emerald-400/15 px-1.5 py-0.2 text-[10px] text-emerald-300">
                      Primary
                    </span>
                  )}
                </button>
              );
            })}
            {!album.isPrimary && album.inLibrary && (
              <Button
                type="button"
                variant="ghost"
                size="sm"
                disabled={busyPrimary}
                onClick={() => void handleMakePrimary()}
                className="ml-auto text-xs"
              >
                <Sparkles className="h-3.5 w-3.5" />
                {busyPrimary ? "Saving…" : "Make Primary"}
              </Button>
            )}
          </div>
        )}

        {/* Spotify-proportioned Tracklist */}
        <div className="mt-8">
          <div className="hidden sm:grid grid-cols-[40px_1fr_auto_72px_72px] items-center gap-4 border-b border-white/[0.07] px-3 pb-2 text-[11px] font-medium uppercase tracking-wider text-muted-foreground/75">
            <span className="text-right">#</span>
            <span>Title</span>
            <span>Format</span>
            <span className="text-right">
              <Clock3 className="inline h-3.5 w-3.5" />
            </span>
            <span />
          </div>

          <div className="mt-1 divide-y divide-white/[0.03]">
            {loadingTracks && tracks.length === 0 && (
              <div className="space-y-2 py-2">
                {Array.from({ length: Math.min(album.trackCount || 8, 10) }).map((_, i) => (
                  <div
                    key={i}
                    className="flex items-center gap-4 rounded-xl px-3 py-3 animate-pulse"
                  >
                    <div className="h-3.5 w-6 rounded bg-white/[0.06]" />
                    <div className="h-3.5 flex-1 max-w-xs rounded bg-white/[0.06]" />
                    <div className="ml-auto h-3 w-12 rounded bg-white/[0.04]" />
                  </div>
                ))}
              </div>
            )}
            {!loadingTracks && tracks.length === 0 && album.available === false && (
              <div className="rounded-xl border border-white/[0.08] bg-white/[0.03] px-4 py-5 text-sm text-muted-foreground">
                This album is no longer available on {sourceLabel(album.provider, album.source)}.
                {album.inLibrary && " You can remove it from your library above."}
              </div>
            )}
            {tracks.map((tr, idx) => {
              const active = tr.id === currentTrackId;
              const trackInLib = Boolean(tr.inLibrary || addedTrackIds[tr.id]);
              const trQual = fmtQuality(tr);

              return (
                <div
                  key={tr.id}
                  className={cn(
                    "group grid grid-cols-[32px_1fr_auto] sm:grid-cols-[40px_1fr_auto_72px_72px] items-center gap-3 sm:gap-4 rounded-xl px-2.5 sm:px-3 py-2.5 transition-colors",
                    active ? "bg-white/[0.08]" : "hover:bg-white/[0.05]",
                  )}
                >
                  {/* Track number / animated playing bars / hover Play or Pause */}
                  <button
                    type="button"
                    onClick={() => {
                      if (active && onTogglePlayPause) {
                        onTogglePlayPause();
                      } else {
                        onPlay({ type: "album", albumId: album.id }, idx);
                      }
                    }}
                    aria-label={active && isPlaying ? `Pause ${tr.title}` : `Play ${tr.title}`}
                    className="flex h-8 w-8 items-center justify-end pr-1 text-right cursor-pointer"
                  >
                    <span
                      className={cn(
                        "flex items-center justify-end font-mono text-xs tabular-nums group-hover:hidden",
                        active ? "text-emerald-400 font-semibold" : "text-muted-foreground",
                      )}
                    >
                      {active && isPlaying ? <PlayingBars /> : tr.trackNumber || idx + 1}
                    </span>
                    {active && isPlaying ? (
                      <Pause className="hidden h-3.5 w-3.5 fill-white text-white group-hover:block" />
                    ) : (
                      <Play className="hidden h-3.5 w-3.5 fill-white text-white group-hover:block" />
                    )}
                  </button>

                  {/* Title & Artist */}
                  <button
                    type="button"
                    onClick={() => {
                      if (active && onTogglePlayPause) {
                        onTogglePlayPause();
                      } else {
                        onPlay({ type: "album", albumId: album.id }, idx);
                      }
                    }}
                    className="min-w-0 text-left cursor-pointer"
                  >
                    <div className="flex items-center gap-1.5 min-w-0">
                      <span
                        className={cn(
                          "truncate text-[14px] font-medium leading-snug",
                          active ? "text-emerald-400" : "text-foreground",
                        )}
                      >
                        {tr.title}
                      </span>
                      {tr.explicit && <ExplicitBadge />}
                    </div>
                    {tr.artist && tr.artist !== album.artist && (
                      <div className="truncate text-xs text-muted-foreground mt-0.5">{tr.artist}</div>
                    )}
                  </button>

                  {/* Format badge (desktop) */}
                  <div className="hidden sm:flex items-center">
                    {trQual && <span className="font-mono text-[11px] text-muted-foreground/75">{trQual}</span>}
                  </div>

                  {/* Duration */}
                  <span className="hidden sm:block text-right font-mono text-xs tabular-nums text-muted-foreground">
                    {fmtDuration(tr.durationSec)}
                  </span>

                  {/* Row actions */}
                  <div className="flex items-center justify-end gap-0.5">
                    {isRemote && !trackInLib && (
                      <button
                        type="button"
                        title="Add Track to Library"
                        onClick={() => void handleAddTrack(tr.id, tr.externalId)}
                        className="rounded-full p-1.5 text-muted-foreground hover:bg-white/10 hover:text-foreground transition-colors cursor-pointer sm:opacity-0 sm:group-hover:opacity-100"
                      >
                        <Plus className="h-3.5 w-3.5" />
                      </button>
                    )}
                    <button
                      type="button"
                      title="Play Next"
                      onClick={() => onAdd({ type: "tracks", trackIds: [tr.id] }, "next")}
                      className="rounded-full p-1.5 text-muted-foreground hover:bg-white/10 hover:text-foreground transition-colors cursor-pointer sm:opacity-0 sm:group-hover:opacity-100"
                    >
                      <SkipForward className="h-3.5 w-3.5" />
                    </button>
                    <button
                      type="button"
                      title="Append to Queue"
                      onClick={() => onAdd({ type: "tracks", trackIds: [tr.id] }, "end")}
                      className="rounded-full p-1.5 text-muted-foreground hover:bg-white/10 hover:text-foreground transition-colors cursor-pointer sm:opacity-0 sm:group-hover:opacity-100"
                    >
                      <ListPlus className="h-3.5 w-3.5" />
                    </button>
                  </div>
                </div>
              );
            })}
          </div>

          {(album.releaseDate || album.label || album.copyright || album.bookletUrl || (album.credits?.length ?? 0) > 0) && (
            <div className="mt-5 px-3 space-y-1.5 text-[11px] text-muted-foreground/65">
              {(fmtReleaseDate(album.releaseDate) || album.label) && (
                <p>
                  {[fmtReleaseDate(album.releaseDate), album.label].filter(Boolean).join(" · ")}
                </p>
              )}
              {album.copyright && <p className="text-muted-foreground/55">{album.copyright}</p>}
              {(album.bookletUrl || (album.credits?.length ?? 0) > 0) && (
                <div className="flex flex-wrap items-center gap-4 pt-1.5">
                  {album.bookletUrl && (
                    <a
                      href={album.bookletUrl}
                      target="_blank"
                      rel="noopener noreferrer"
                      className="inline-flex items-center gap-1.5 text-xs font-medium text-muted-foreground hover:text-white transition-colors"
                    >
                      <FileText className="h-3.5 w-3.5" />
                      Digital Booklet
                    </a>
                  )}
                  {album.credits && album.credits.length > 0 && (
                    <button
                      type="button"
                      onClick={() => setCreditsOpen(true)}
                      className="inline-flex items-center gap-1.5 text-xs font-medium text-muted-foreground hover:text-white focus:outline-none focus-visible:underline transition-colors cursor-pointer"
                    >
                      <Users className="h-3.5 w-3.5" />
                      Credits
                    </button>
                  )}
                </div>
              )}
            </div>
          )}

          {album.credits && album.credits.length > 0 && (
            <Dialog open={creditsOpen} onOpenChange={setCreditsOpen}>
              <DialogContent className="max-w-lg p-6">
                <div className="pr-8">
                  <DialogTitle className="text-lg font-bold tracking-tight text-white">Credits</DialogTitle>
                  <DialogDescription className="mt-0.5 text-xs">
                    {album.title} · {album.artist}
                  </DialogDescription>
                </div>
                <div className="mt-4 max-h-[60vh] overflow-y-auto pr-2 divide-y divide-white/[0.06]">
                  {album.credits.map((c) => (
                    <div key={c.name} className="flex items-baseline justify-between gap-4 py-2.5 text-sm">
                      <span className="font-medium text-foreground">{c.name}</span>
                      <span className="text-right text-xs text-muted-foreground">{c.roles.join(", ")}</span>
                    </div>
                  ))}
                </div>
              </DialogContent>
            </Dialog>
          )}
        </div>

        {/* Contextual "More Like This" recommendations */}
        {similarAlbums.length > 0 && (
          <section aria-label="More Like This" className="mt-12 border-t border-white/[0.08] pt-8 space-y-4">
            <h2 className="text-base font-semibold tracking-tight text-white">More Like This</h2>
            <div className="grid grid-cols-2 sm:grid-cols-3 md:grid-cols-6 gap-x-4 gap-y-6">
              {similarAlbums.map((sim) => (
                <AlbumCard
                  key={sim.id}
                  album={sim}
                  showInLibraryBadge
                  onOpen={(a) => onOpenAlbum(a.id)}
                  onPlay={onPlay}
                />
              ))}
            </div>
          </section>
        )}
        </div>
      </PageBody>
    </div>
  );
}
