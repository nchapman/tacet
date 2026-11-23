import { useQuery } from "@tanstack/react-query";
import { ArrowLeft, ChevronRight, ListPlus, Loader2, Play, SkipForward, Sparkles } from "lucide-react";
import { useEffect, useMemo, useRef, useState } from "react";
import { TacetAPIError, tacetClient } from "@/api/client";
import { fmtAlbumQuality, fmtDuration, fmtQuality, isHiRes } from "@/api/format";
import type {
  Album,
  ArtistSummary,
  QobuzArtistDetail,
  QobuzStatus,
  QueuePosition,
  QueueSource,
} from "@/api/types";
import { AlbumArt } from "@/components/ui/artwork";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { EditorialNote } from "@/components/ui/EditorialNote";
import { ExplicitBadge } from "@/components/ui/ExplicitBadge";
import { PageHeader } from "@/components/ui/PageHeader";
import { QOBUZ_ARTIST_GC_TIME_MS, QOBUZ_ARTIST_STALE_TIME_MS, queryErrorMessage, queryKeys } from "@/lib/query";
import { useScrollRestoration } from "@/lib/scroll";
import { cn } from "@/lib/utils";

interface ArtistsViewProps {
  qobuzStatus: QobuzStatus | null;
  initialArtist?: string;
  initialQobuzArtistId?: number;
  onSelectArtist?: (artistName: string, qobuzArtistId?: number) => void;
  onBackToArtists?: () => void;
  onOpenAlbum: (album: Album) => void;
  onPlay: (source: QueueSource, startIndex?: number) => void;
  onAdd: (source: QueueSource, position?: QueuePosition) => void;
}

function normTitle(s: string): string {
  return s
    .toLowerCase()
    .replace(/\([^)]*\)/g, " ")
    .replace(/\[[^\]]*\]/g, " ")
    .replace(/[^a-z0-9]+/g, " ")
    .trim();
}

/**
 * Unified Smart Artist Page:
 * - Desktop: split master-detail with independently scrolled Library Artist rail & unified discography.
 * - Mobile: clean list -> unified discography drilldown with back bar.
 * - Works for both library artists AND remote-only Qobuz artists opened from Search:
 *   shows Library albums on top, then a clean divider and "More on Qobuz" remote albums below.
 */
export function ArtistsView({
  qobuzStatus,
  initialArtist,
  initialQobuzArtistId,
  onSelectArtist,
  onBackToArtists,
  onOpenAlbum,
  onPlay,
  onAdd,
}: ArtistsViewProps) {
  const [selectedArtist, setSelectedArtist] = useState(initialArtist ?? "");
  const [selectedQobuzId, setSelectedQobuzId] = useState<number | undefined>(initialQobuzArtistId);
  const [showAllPopular, setShowAllPopular] = useState(false);
  const [mobileDetailOpen, setMobileDetailOpen] = useState(Boolean(initialArtist));

  const railRef = useRef<HTMLDivElement | null>(null);
  const detailRef = useRef<HTMLDivElement | null>(null);

  useScrollRestoration(railRef, {
    namespace: "artists-rail",
    routeKey: "/artists",
    resetOnPush: false,
  });
  useScrollRestoration(detailRef, {
    namespace: "artists-detail",
  });

  const qobuzConnected = Boolean(qobuzStatus?.authenticated);

  useEffect(() => {
    if (initialArtist) {
      setSelectedArtist(initialArtist);
      setSelectedQobuzId(initialQobuzArtistId);
      setMobileDetailOpen(true);
    } else {
      setSelectedQobuzId(undefined);
      setMobileDetailOpen(false);
    }
  }, [initialArtist, initialQobuzArtistId]);

  // Fetch unified library artist list.
  const artistsQuery = useQuery({
    queryKey: queryKeys.library.artists(500),
    queryFn: ({ signal }) => tacetClient.listLibraryArtists({ limit: 500 }, { signal }),
  });
  const artists: ArtistSummary[] = useMemo(() => artistsQuery.data?.items ?? [], [artistsQuery.data]);

  // Default to the first artist on desktop when no artist is selected yet.
  const effectiveArtist = selectedArtist || artists[0]?.name || "";

  useEffect(() => {
    setShowAllPopular(false);
  }, [effectiveArtist, selectedQobuzId]);

  // Fetch library albums for the selected artist.
  const albumsQuery = useQuery({
    queryKey: queryKeys.library.albums({
      artist: effectiveArtist.toLowerCase(),
      groupVersions: true,
      sort: "year",
      limit: 200,
    }),
    queryFn: ({ signal }) =>
      tacetClient.listLibraryAlbums(
        {
          artist: effectiveArtist,
          groupVersions: true,
          sort: "year",
          limit: 200,
        },
        { signal },
      ),
    enabled: Boolean(effectiveArtist),
  });
  const albums: Album[] = useMemo(() => albumsQuery.data?.items ?? [], [albumsQuery.data]);

  // Progressively fetch remote Qobuz discography for the selected artist (cached 1h).
  const remoteQuery = useQuery<QobuzArtistDetail | null>({
    queryKey: queryKeys.qobuz.artist(selectedQobuzId ? { id: selectedQobuzId } : { name: effectiveArtist }),
    queryFn: async ({ signal }) => {
      try {
        return await tacetClient.qobuzArtist(
          selectedQobuzId ? { id: selectedQobuzId, limit: 50 } : { name: effectiveArtist, limit: 50 },
          { signal },
        );
      } catch (err) {
        // 404 is expected for local-only artists not on Qobuz; cache null to avoid repeat 404s.
        if (err instanceof TacetAPIError && err.status === 404) return null;
        throw err;
      }
    },
    enabled: Boolean(effectiveArtist && qobuzConnected),
    staleTime: QOBUZ_ARTIST_STALE_TIME_MS,
    gcTime: QOBUZ_ARTIST_GC_TIME_MS,
  });

  const loadingArtists = artistsQuery.isPending;
  const loadingAlbums = albumsQuery.isLoading;
  const errMsg = queryErrorMessage(artistsQuery.error ?? albumsQuery.error);
  const remoteDetail = remoteQuery.data ?? null;
  const loadingRemote = remoteQuery.isLoading;
  const remoteErr = queryErrorMessage(remoteQuery.error);

  const similarArtistId = remoteDetail?.artist?.id ?? initialQobuzArtistId;
  const inlineSimilar = remoteDetail?.similarArtists ?? [];
  const similarArtistsQuery = useQuery({
    queryKey: queryKeys.discover.similarArtists(similarArtistId ?? 0),
    queryFn: ({ signal }) => tacetClient.getSimilarArtists(similarArtistId!, 12, { signal }),
    enabled:
      qobuzConnected &&
      Boolean(similarArtistId && similarArtistId > 0) &&
      inlineSimilar.length === 0 &&
      !loadingRemote,
    staleTime: QOBUZ_ARTIST_STALE_TIME_MS,
    gcTime: QOBUZ_ARTIST_GC_TIME_MS,
  });
  const similarArtists = (inlineSimilar.length > 0 ? inlineSimilar : (similarArtistsQuery.data?.items ?? []))
    .filter((a) => a.id !== similarArtistId)
    .slice(0, 10);

  // Deduplicate Qobuz albums already present in the library section above.
  const remoteOnlyAlbums = useMemo(() => {
    const raw = remoteDetail?.albums ?? [];
    if (raw.length === 0) return [];
    const libIds = new Set(albums.map((a) => a.id));
    const libTitles = new Set(albums.map((a) => normTitle(a.title)));
    return raw.filter((alb) => !alb.inLibrary && !libIds.has(alb.id) && !libTitles.has(normTitle(alb.title)));
  }, [remoteDetail, albums]);

  const currentSummary = artists.find((a) => a.name.toLowerCase() === effectiveArtist.toLowerCase());
  const heroArt =
    remoteDetail?.artist.artworkUrl ||
    albums[0]?.artworkUrl ||
    currentSummary?.artworkUrl ||
    remoteOnlyAlbums[0]?.artworkUrl;

  return (
    <div className="flex h-full min-h-0">
      {/* Left Artist Rail (full width on mobile until artist selected; fixed 288px on md+) */}
      <div
        className={cn(
          "flex flex-col border-r border-white/[0.06] bg-black/20 md:w-72 lg:w-80 md:shrink-0",
          mobileDetailOpen ? "hidden md:flex" : "flex w-full",
        )}
      >
        <div className="flex h-16 items-center border-b border-white/[0.06] px-4 shrink-0">
          <div className="flex w-full items-baseline justify-between">
            <h1 className="text-xl sm:text-2xl font-bold tracking-tight text-white">Artists</h1>
            <span className="text-xs font-mono text-muted-foreground">{artists.length}</span>
          </div>
        </div>

        <div ref={railRef} className="flex-1 overflow-y-auto p-2 space-y-0.5">
          {loadingArtists && artists.length === 0 ? (
            <div className="space-y-2 p-2">
              {Array.from({ length: 10 }).map((_, i) => (
                <div key={i} className="flex items-center gap-3 animate-pulse py-1.5">
                  <div className="h-10 w-10 rounded-full bg-white/[0.06]" />
                  <div className="space-y-1.5 flex-1">
                    <div className="h-3.5 w-2/3 rounded bg-white/[0.06]" />
                    <div className="h-2.5 w-1/3 rounded bg-white/[0.04]" />
                  </div>
                </div>
              ))}
            </div>
          ) : artists.length === 0 ? (
            <p className="py-12 text-center text-xs text-muted-foreground">No artists in your library yet.</p>
          ) : (
            artists.map((artist) => {
              const active = artist.name.toLowerCase() === effectiveArtist.toLowerCase();
              return (
                <button
                  key={artist.name}
                  type="button"
                  onClick={() => {
                    setSelectedArtist(artist.name);
                    setSelectedQobuzId(undefined);
                    setMobileDetailOpen(true);
                    onSelectArtist?.(artist.name);
                  }}
                  className={cn(
                    "flex w-full items-center gap-3 rounded-xl px-2.5 py-2 text-left transition-colors cursor-pointer",
                    active ? "md:bg-white/[0.1] md:text-white text-foreground/90" : "text-foreground/85 hover:bg-white/[0.04]",
                  )}
                >
                  <AlbumArt
                    artworkUrl={(active ? heroArt : undefined) || artist.artworkUrl}
                    alt={artist.name}
                    className="h-10 w-10 shrink-0 rounded-full ring-1 ring-white/10"
                  />
                  <div className="min-w-0 flex-1">
                    <div className="truncate text-[13.5px] font-medium leading-tight">{artist.name}</div>
                    <div className="truncate text-xs text-muted-foreground mt-0.5">
                      {artist.albumCount} {artist.albumCount === 1 ? "album" : "albums"} · {artist.trackCount} tracks
                    </div>
                  </div>
                  <ChevronRight className="h-4 w-4 text-muted-foreground/50 md:hidden shrink-0" />
                </button>
              );
            })
          )}
        </div>
      </div>

      {/* Right Unified Discography Showcase (Library on top -> subtle divider -> More on Qobuz below) */}
      <div
        ref={detailRef}
        className={cn(
          "relative flex-1 min-w-0 overflow-y-auto",
          mobileDetailOpen ? "block" : "hidden md:block",
        )}
      >
        {effectiveArtist ? (
          <div className="relative min-h-full pb-12">
            {/* Mobile back bar */}
            <PageHeader className="min-h-0 gap-2 px-4 py-3 md:hidden">
              <Button
                type="button"
                variant="ghost"
                size="sm"
                className="-ml-2"
                onClick={() => {
                  setMobileDetailOpen(false);
                  onBackToArtists?.();
                }}
              >
                <ArrowLeft className="h-4 w-4" />
                Artists
              </Button>
            </PageHeader>

            <div className="relative z-10 mx-auto max-w-4xl px-4 sm:px-8 pt-6 sm:pt-8">
              <div className="flex items-end gap-4 pb-6 border-b border-white/[0.07]">
                <AlbumArt
                  artworkUrl={heroArt}
                  alt={effectiveArtist}
                  className="h-16 w-16 sm:h-20 sm:w-20 shrink-0 rounded-full shadow-xl ring-1 ring-white/15"
                  priority
                />
                <div className="min-w-0">
                  <div className="text-[11px] font-semibold uppercase tracking-[0.14em] text-muted-foreground">
                    Artist
                  </div>
                  <h2 className="truncate text-2xl sm:text-3xl font-bold tracking-tight text-white">
                    {effectiveArtist}
                  </h2>
                  <p className="text-xs text-muted-foreground mt-1">
                    {currentSummary ? (
                      <>
                        {currentSummary.albumCount} {currentSummary.albumCount === 1 ? "album" : "albums"} ·{" "}
                        {currentSummary.trackCount} tracks in library
                      </>
                    ) : remoteDetail ? (
                      <>Qobuz Artist · {remoteDetail.albums.length} releases</>
                    ) : (
                      <>In your library</>
                    )}
                  </p>
                </div>
              </div>

              {remoteDetail?.artist.biography && (
                <EditorialNote
                  text={remoteDetail.artist.biography}
                  title={`About ${effectiveArtist}`}
                  targetWords={30}
                  className="mt-4 max-w-3xl"
                />
              )}

              {errMsg && (
                <div role="alert" className="mt-4 rounded-xl border border-red-500/30 bg-red-950/50 px-4 py-3 text-sm text-red-200">
                  {errMsg}
                </div>
              )}

              {/* Popular Tracks from Qobuz */}
              {qobuzConnected && (remoteDetail?.tracks?.length ?? 0) > 0 && (() => {
                const popularTracks = remoteDetail!.tracks!;
                const visibleTracks = showAllPopular ? popularTracks : popularTracks.slice(0, 5);
                const popularIds = popularTracks.map((t) => t.id);
                return (
                  <section aria-label="Popular tracks" className="mt-6">
                    <div className="flex items-center justify-between mb-3">
                      <h3 className="text-xs font-semibold uppercase tracking-wider text-muted-foreground">
                        Popular
                      </h3>
                      {popularTracks.length > 5 && (
                        <button
                          type="button"
                          onClick={() => setShowAllPopular((v) => !v)}
                          className="text-xs font-medium text-muted-foreground hover:text-white focus:outline-none focus-visible:underline transition-colors cursor-pointer"
                        >
                          {showAllPopular ? "Show less" : `Show all ${popularTracks.length}`}
                        </button>
                      )}
                    </div>
                    <div className="space-y-0.5">
                      {visibleTracks.map((tr, idx) => {
                        const qual = fmtQuality(tr);
                        return (
                          <div
                            key={tr.id}
                            onClick={() => onPlay({ type: "tracks", trackIds: popularIds }, idx)}
                            className="group grid grid-cols-[24px_40px_1fr_auto] sm:grid-cols-[24px_40px_1fr_172px_48px_68px] items-center gap-3 rounded-xl px-2.5 py-2 transition-colors hover:bg-white/[0.06] cursor-pointer"
                          >
                            <div className="flex items-center justify-center">
                              <span className="font-mono text-xs text-muted-foreground group-hover:hidden">
                                {idx + 1}
                              </span>
                              <Play className="hidden h-3.5 w-3.5 fill-white text-white group-hover:block" />
                            </div>
                            <AlbumArt
                              artworkUrl={tr.artworkUrl}
                              alt={tr.album || tr.title}
                              className="h-10 w-10 rounded-lg ring-1 ring-white/10"
                            />
                            <div className="min-w-0">
                              <div className="flex items-center gap-1.5 min-w-0">
                                <span className="truncate text-sm font-medium text-foreground">{tr.title}</span>
                                {tr.explicit && <ExplicitBadge />}
                              </div>
                              {tr.album && (
                                <div className="truncate text-xs text-muted-foreground">{tr.album}</div>
                              )}
                            </div>
                            <div className="hidden sm:flex items-center justify-start whitespace-nowrap">
                              {qual && <span className="font-mono text-[11px] text-muted-foreground/75">{qual}</span>}
                            </div>
                            <div className="hidden sm:block text-right font-mono text-xs text-muted-foreground">
                              {fmtDuration(tr.durationSec)}
                            </div>
                            <div className="flex items-center justify-end gap-0.5">
                              <button
                                type="button"
                                title="Play Next"
                                onClick={(e) => {
                                  e.stopPropagation();
                                  onAdd({ type: "tracks", trackIds: [tr.id] }, "next");
                                }}
                                className="rounded-full p-1.5 text-muted-foreground hover:bg-white/10 hover:text-foreground transition-colors cursor-pointer sm:opacity-0 sm:group-hover:opacity-100"
                              >
                                <SkipForward className="h-3.5 w-3.5" />
                              </button>
                              <button
                                type="button"
                                title="Append to Queue"
                                onClick={(e) => {
                                  e.stopPropagation();
                                  onAdd({ type: "tracks", trackIds: [tr.id] }, "end");
                                }}
                                className="rounded-full p-1.5 text-muted-foreground hover:bg-white/10 hover:text-foreground transition-colors cursor-pointer sm:opacity-0 sm:group-hover:opacity-100"
                              >
                                <ListPlus className="h-3.5 w-3.5" />
                              </button>
                            </div>
                          </div>
                        );
                      })}
                    </div>
                  </section>
                );
              })()}

              {/* Library Albums (always anchored on top so remote fetch never shifts them) */}
              {(loadingAlbums || albums.length > 0) && (
                <section aria-label="Library albums" className="mt-6 space-y-4">
                  {qobuzConnected && (remoteOnlyAlbums.length > 0 || loadingRemote) && albums.length > 0 && (
                    <h3 className="text-xs font-semibold uppercase tracking-wider text-muted-foreground">
                      In Your Library
                    </h3>
                  )}

                  {loadingAlbums && albums.length === 0 ? (
                    Array.from({ length: 3 }).map((_, i) => (
                      <div
                        key={i}
                        className="flex items-center gap-5 rounded-2xl border border-white/[0.06] bg-white/[0.02] p-4 animate-pulse"
                      >
                        <div className="h-28 w-28 rounded-xl bg-white/[0.06] shrink-0" />
                        <div className="space-y-2 flex-1">
                          <div className="h-5 w-1/2 rounded bg-white/[0.06]" />
                          <div className="h-3.5 w-1/3 rounded bg-white/[0.04]" />
                        </div>
                      </div>
                    ))
                  ) : (
                    albums.map((alb) => (
                      <StackedAlbumCard
                        key={alb.id}
                        album={alb}
                        onOpenAlbum={onOpenAlbum}
                        onPlay={onPlay}
                        onAdd={onAdd}
                      />
                    ))
                  )}
                </section>
              )}

              {/* Subtle divider + More on Qobuz remote albums below */}
              {qobuzConnected && (loadingRemote || remoteOnlyAlbums.length > 0 || remoteErr) && (
                <section
                  aria-label="More on Qobuz"
                  className={cn(
                    "space-y-4",
                    albums.length > 0 ? "mt-8 border-t border-white/[0.08] pt-6" : "mt-6",
                  )}
                >
                  <div className="flex items-center gap-2">
                    <Sparkles className="h-3.5 w-3.5 text-sky-400" />
                    <h3 className="text-xs font-semibold uppercase tracking-wider text-muted-foreground">
                      {albums.length > 0 ? "More on Qobuz" : "On Qobuz"}
                    </h3>
                    {loadingRemote && <Loader2 className="h-3.5 w-3.5 animate-spin text-sky-400" />}
                  </div>

                  {remoteErr && (
                    <div role="alert" className="rounded-xl border border-red-500/30 bg-red-950/40 px-4 py-3 text-xs text-red-200">
                      Couldn’t load Qobuz releases: {remoteErr}
                    </div>
                  )}

                  {remoteOnlyAlbums.length > 0 && (
                    <div className="space-y-4">
                      {remoteOnlyAlbums.map((alb) => (
                        <StackedAlbumCard
                          key={alb.id}
                          album={alb}
                          onOpenAlbum={onOpenAlbum}
                          onPlay={onPlay}
                          onAdd={onAdd}
                        />
                      ))}
                    </div>
                  )}
                </section>
              )}

              {!loadingAlbums && !loadingRemote && albums.length === 0 && remoteOnlyAlbums.length === 0 && (
                <p className="mt-8 text-sm text-muted-foreground">No albums found for {effectiveArtist}.</p>
              )}

              {/* Contextual Similar Artists */}
              {similarArtists.length > 0 && (
                <section aria-label="Similar Artists" className="mt-10 border-t border-white/[0.08] pt-6 space-y-4">
                  <h3 className="text-xs font-semibold uppercase tracking-wider text-muted-foreground">
                    Similar Artists
                  </h3>
                  <div className="grid grid-cols-2 sm:grid-cols-3 md:grid-cols-5 gap-3">
                    {similarArtists.map((sim) => (
                      <button
                        key={sim.id}
                        type="button"
                        onClick={() => onSelectArtist?.(sim.name, sim.id)}
                        className="group flex flex-col items-center gap-2 rounded-xl border border-white/[0.06] bg-white/[0.025] p-3 text-center transition-colors hover:border-white/[0.14] hover:bg-white/[0.05] cursor-pointer"
                      >
                        <AlbumArt
                          artworkUrl={sim.artworkUrl}
                          alt={sim.name}
                          className="h-16 w-16 rounded-full ring-1 ring-white/10"
                        />
                        <div className="w-full truncate text-xs font-medium text-foreground group-hover:text-white">
                          {sim.name}
                        </div>
                      </button>
                    ))}
                  </div>
                </section>
              )}
            </div>
          </div>
        ) : (
          <div className="flex h-full items-center justify-center p-8 text-sm text-muted-foreground">
            Select an artist to explore their albums.
          </div>
        )}
      </div>
    </div>
  );
}

function StackedAlbumCard({
  album: alb,
  onOpenAlbum,
  onPlay,
  onAdd,
}: {
  album: Album;
  onOpenAlbum: (album: Album) => void;
  onPlay: (source: QueueSource, startIndex?: number) => void;
  onAdd: (source: QueueSource, position?: QueuePosition) => void;
}) {
  const qLabel = fmtAlbumQuality(alb);
  const hiRes = isHiRes(alb.maxBitDepth, alb.maxSampleRate);
  const isDSD =
    alb.primaryFormat?.toUpperCase() === "DSF" ||
    alb.primaryFormat?.toUpperCase().startsWith("DSD") ||
    (alb.maxBitDepth === 1 && (alb.maxSampleRate ?? 0) >= 2_000_000);
  const hiResLabel = isDSD ? "DSD" : hiRes ? "HI-RES" : "";

  return (
    <div
      onClick={() => onOpenAlbum(alb)}
      className="group flex flex-col sm:flex-row sm:items-center gap-4 sm:gap-5 rounded-2xl border border-white/[0.07] bg-white/[0.025] p-3.5 sm:p-4 transition-all hover:border-white/[0.14] hover:bg-white/[0.045] cursor-pointer"
    >
      <div className="relative h-28 w-28 sm:h-32 sm:w-32 shrink-0 overflow-hidden rounded-xl shadow-lg ring-1 ring-white/10 self-start">
        <AlbumArt
          artworkUrl={alb.artworkUrl}
          alt={alb.title}
          className="h-full w-full transition-transform duration-300 group-hover:scale-[1.04]"
        />
        {hiResLabel && (
          <span
            title={qLabel}
            className="pointer-events-none absolute bottom-2 right-2 rounded-md bg-black/60 px-1.5 py-0.5 text-[9px] font-semibold tracking-[0.06em] text-[#dfc07b] backdrop-blur-md border border-white/[0.12]"
          >
            {hiResLabel}
          </span>
        )}
      </div>

      <div className="min-w-0 flex-1 space-y-2">
        <div className="flex flex-wrap items-center gap-2 text-xs font-mono text-muted-foreground">
          {alb.year ? <span>{alb.year}</span> : null}
          {qLabel && (
            <span className="inline-flex items-center gap-1.5">
              {alb.year ? <span className="text-white/20">·</span> : null}
              <span>{qLabel}</span>
            </span>
          )}
          {(alb.versionCount ?? 1) > 1 && (
            <Badge variant="muted" className="text-[10.5px]">
              {alb.versionCount} versions
            </Badge>
          )}
        </div>

        <button
          type="button"
          onClick={(e) => {
            e.stopPropagation();
            onOpenAlbum(alb);
          }}
          className="block text-left cursor-pointer"
        >
          <h3 className="text-lg sm:text-xl font-semibold tracking-tight text-white group-hover:underline">
            {alb.title}
          </h3>
          <p className="text-xs text-muted-foreground mt-0.5">
            {alb.trackCount} {alb.trackCount === 1 ? "track" : "tracks"}
            {alb.genre ? ` · ${alb.genre}` : ""}
            {alb.playCount ? ` · ${alb.playCount} plays` : ""}
          </p>
        </button>

        <div className="flex flex-wrap items-center gap-2 pt-1">
          <Button
            type="button"
            size="sm"
            className="rounded-full bg-white text-black hover:bg-white/90 h-8 px-4"
            onClick={(e) => {
              e.stopPropagation();
              onPlay({ type: "album", albumId: alb.id }, 0);
            }}
          >
            <Play className="h-3.5 w-3.5 fill-current" />
            Play
          </Button>
          <Button
            type="button"
            variant="secondary"
            size="sm"
            className="rounded-full h-8 px-3.5"
            onClick={(e) => {
              e.stopPropagation();
              onAdd({ type: "album", albumId: alb.id }, "end");
            }}
          >
            <ListPlus className="h-3.5 w-3.5" />
            Queue
          </Button>
        </div>
      </div>
    </div>
  );
}
