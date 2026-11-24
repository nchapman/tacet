import { useQueries, useQuery, useQueryClient } from "@tanstack/react-query";
import { ArrowLeft, ChevronRight, ListPlus, Loader2, Play, SkipForward, Sparkles } from "lucide-react";
import { memo, type ReactNode, useCallback, useEffect, useMemo, useRef, useState } from "react";
import { tacetClient } from "@/api/client";
import { fmtAlbumQuality, fmtCount, fmtDuration, fmtQuality, isHiRes } from "@/api/format";
import type {
  Album,
  ArtistSummary,
  QobuzArtistDetail,
  QobuzStatus,
  QueuePosition,
  QueueSource,
} from "@/api/types";
import { AlbumArt, resolveArtworkSrc } from "@/components/ui/artwork";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { EditorialNote } from "@/components/ui/EditorialNote";
import { ExplicitBadge } from "@/components/ui/ExplicitBadge";
import { PageHeader } from "@/components/ui/PageHeader";
import {
  artistAlbumsQueryOptions,
  qobuzArtistQueryOptions,
  useAlbumHoverPrefetch,
  useArtistHoverPrefetch,
} from "@/lib/prefetch";
import { QOBUZ_ARTIST_GC_TIME_MS, QOBUZ_ARTIST_STALE_TIME_MS, queryErrorMessage, queryKeys } from "@/lib/query";
import { useScrollRestoration } from "@/lib/scroll";
import { cn } from "@/lib/utils";
import { useVirtualWindow } from "@/lib/virtualWindow";

const ARTIST_PAGE_SIZE = 100;
const ARTIST_ROW_HEIGHT = 56;
const ARTIST_LIST_PADDING_TOP = 8;
/** Hold switching the detail pane briefly so SQLite-cached Qobuz artist data paints atomically with zero flash. */
const REVEAL_GRACE_MS = 160;

function artistPageQueryOptions(page: number) {
  const offset = page * ARTIST_PAGE_SIZE;
  return {
    queryKey: queryKeys.library.artists({ limit: ARTIST_PAGE_SIZE, offset }),
    queryFn: ({ signal }: { signal: AbortSignal }) =>
      tacetClient.listLibraryArtists({ limit: ARTIST_PAGE_SIZE, offset }, { signal }),
  };
}

interface ArtistDetailSnapshot {
  artist: string;
  summary?: ArtistSummary;
  albums: Album[];
  remoteDetail: QobuzArtistDetail | null;
  remoteOnlyAlbums: Album[];
  similarArtists: QobuzArtistDetail["similarArtists"];
  heroArt?: string;
}

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
  const queryClient = useQueryClient();
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

  // Page 0 establishes total artist count (full scrollbar height) and seeds rows 0..99.
  const artistsPage0Query = useQuery(artistPageQueryOptions(0));
  const totalArtists = artistsPage0Query.data?.total ?? 0;

  const railWin = useVirtualWindow({
    scrollRef: railRef,
    totalCount: totalArtists,
    rowHeight: ARTIST_ROW_HEIGHT,
    pageSize: ARTIST_PAGE_SIZE,
    overscan: 12,
    prefetchMargin: 25,
    paddingTop: ARTIST_LIST_PADDING_TOP,
  });

  const extraArtistPages = useMemo(
    () => railWin.visiblePages.filter((p) => p > 0),
    [railWin.visiblePages],
  );

  const artistPageQueries = useQueries({
    queries: extraArtistPages.map((page) => ({
      ...artistPageQueryOptions(page),
      enabled: railWin.settled,
    })),
  });

  useEffect(() => {
    if (!railWin.settled) return;
    for (const page of railWin.prefetchPages) {
      if (page <= 0) continue;
      void queryClient.prefetchQuery(artistPageQueryOptions(page));
    }
  }, [railWin.settled, railWin.prefetchPages, queryClient]);

  const artistPagesByIndex = useMemo(() => {
    const map = new Map<number, ArtistSummary[]>();
    if (artistsPage0Query.data?.items) {
      map.set(0, artistsPage0Query.data.items);
    }
    extraArtistPages.forEach((page, i) => {
      const items = artistPageQueries[i]?.data?.items;
      if (items) map.set(page, items);
    });
    return map;
  }, [artistsPage0Query.data?.items, extraArtistPages, artistPageQueries]);

  // Default to the first artist on desktop when no artist is selected yet.
  const effectiveArtist = selectedArtist || artistsPage0Query.data?.items?.[0]?.name || "";

  const [graceExpiredForKey, setGraceExpiredForKey] = useState("");
  const [heldSnapshot, setHeldSnapshot] = useState<ArtistDetailSnapshot | null>(null);

  const handleSelectArtist = useCallback(
    (name: string) => {
      setSelectedArtist(name);
      setSelectedQobuzId(undefined);
      setMobileDetailOpen(true);
      onSelectArtist?.(name);
    },
    [onSelectArtist],
  );

  useEffect(() => {
    setShowAllPopular(false);
  }, [effectiveArtist, selectedQobuzId]);

  // Fetch library albums for the selected artist.
  const albumsQuery = useQuery({
    ...artistAlbumsQueryOptions(effectiveArtist),
    enabled: Boolean(effectiveArtist),
  });
  const albums: Album[] = useMemo(() => albumsQuery.data?.items ?? [], [albumsQuery.data]);

  // Progressively fetch remote Qobuz discography for the selected artist (cached 1h).
  const remoteQuery = useQuery<QobuzArtistDetail | null>({
    ...qobuzArtistQueryOptions(selectedQobuzId ? { id: selectedQobuzId } : { name: effectiveArtist }),
    enabled: Boolean(effectiveArtist && qobuzConnected),
  });

  const loadingArtists = artistsPage0Query.isPending;
  const loadingAlbums = albumsQuery.isLoading;
  const artistPageError = artistPageQueries.find((q) => q.error)?.error;
  const errMsg = queryErrorMessage(artistsPage0Query.error ?? artistPageError ?? albumsQuery.error);
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
  const similarArtists = useMemo(
    () =>
      (inlineSimilar.length > 0 ? inlineSimilar : (similarArtistsQuery.data?.items ?? []))
        .filter((a) => a.id !== similarArtistId)
        .slice(0, 10),
    [inlineSimilar, similarArtistsQuery.data?.items, similarArtistId],
  );

  // Deduplicate Qobuz albums already present in the library section above.
  const remoteOnlyAlbums = useMemo(() => {
    const raw = remoteDetail?.albums ?? [];
    if (raw.length === 0) return [];
    const libIds = new Set(albums.map((a) => a.id));
    const libTitles = new Set(albums.map((a) => normTitle(a.title)));
    return raw.filter((alb) => !alb.inLibrary && !libIds.has(alb.id) && !libTitles.has(normTitle(alb.title)));
  }, [remoteDetail, albums]);

  const currentSummary = useMemo(() => {
    if (!effectiveArtist) return undefined;
    const target = effectiveArtist.toLowerCase();
    for (const pageItems of artistPagesByIndex.values()) {
      const found = pageItems.find((a) => a.name.toLowerCase() === target);
      if (found) return found;
    }
    return undefined;
  }, [artistPagesByIndex, effectiveArtist]);

  const visibleArtistIndices = useMemo(() => {
    if (railWin.endIndex < railWin.startIndex) return [];
    const list: number[] = [];
    for (let i = railWin.startIndex; i <= railWin.endIndex; i++) {
      list.push(i);
    }
    return list;
  }, [railWin.startIndex, railWin.endIndex]);

  const heroArt =
    remoteDetail?.artist.artworkUrl ||
    albums[0]?.artworkUrl ||
    currentSummary?.artworkUrl ||
    remoteOnlyAlbums[0]?.artworkUrl;

  // Bounded paint-hold: when switching artists while connected to Qobuz, wait up to
  // REVEAL_GRACE_MS (160ms) for both SQLite albums and Qobuz metadata so cached artists
  // paint once atomically with zero flash, while slow uncached Qobuz lookups still reveal
  // local library albums promptly and smoothly glide in Qobuz enrichment when it arrives.
  const artistKey = effectiveArtist ? `${effectiveArtist.toLowerCase()}:${selectedQobuzId ?? ""}` : "";
  const targetReady = Boolean(effectiveArtist) && !loadingAlbums && (!qobuzConnected || !loadingRemote);

  useEffect(() => {
    if (!artistKey || targetReady) return;
    const timer = window.setTimeout(() => {
      setGraceExpiredForKey(artistKey);
    }, REVEAL_GRACE_MS);
    return () => window.clearTimeout(timer);
  }, [artistKey, targetReady]);

  const canRevealTarget = targetReady || graceExpiredForKey === artistKey;

  useEffect(() => {
    if (!effectiveArtist) return;
    if (!canRevealTarget) return;
    setHeldSnapshot({
      artist: effectiveArtist,
      summary: currentSummary,
      albums,
      remoteDetail,
      remoteOnlyAlbums,
      similarArtists,
      heroArt,
    });
  }, [canRevealTarget, effectiveArtist, currentSummary, albums, remoteDetail, remoteOnlyAlbums, similarArtists, heroArt]);

  useEffect(() => {
    if (!mobileDetailOpen && !initialArtist) {
      // Avoid flashing a previously viewed artist when drilling into a new one on mobile.
      setHeldSnapshot((prev) => (prev && prev.artist.toLowerCase() !== effectiveArtist.toLowerCase() ? null : prev));
    }
  }, [mobileDetailOpen, initialArtist, effectiveArtist]);

  const isHoldingPrevious = Boolean(!canRevealTarget && heldSnapshot && effectiveArtist);
  const isHoldingCold = Boolean(!canRevealTarget && !heldSnapshot && effectiveArtist);

  const view: ArtistDetailSnapshot | null = isHoldingPrevious
    ? heldSnapshot
    : isHoldingCold || !effectiveArtist
      ? null
      : {
          artist: effectiveArtist,
          summary: currentSummary,
          albums,
          remoteDetail,
          remoteOnlyAlbums,
          similarArtists,
          heroArt,
        };
  const viewLoadingAlbums = isHoldingPrevious ? false : loadingAlbums;
  const viewLoadingRemote = isHoldingPrevious ? false : loadingRemote;
  const viewErrMsg = isHoldingPrevious ? "" : errMsg;
  const viewRemoteErr = isHoldingPrevious ? "" : remoteErr;

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
            <span className="text-xs font-mono text-muted-foreground">{fmtCount(totalArtists)}</span>
          </div>
        </div>

        <div ref={railRef} data-scroll-stage="" className="flex-1 overflow-y-auto p-2">
          {loadingArtists && totalArtists === 0 ? (
            <div>
              {Array.from({ length: 10 }).map((_, i) => (
                <ArtistRailSkeleton key={i} />
              ))}
            </div>
          ) : totalArtists === 0 ? (
            <p className="py-12 text-center text-xs text-muted-foreground">No artists in your library yet.</p>
          ) : (
            <div className="relative" style={{ height: railWin.totalHeight }}>
              <div
                className="absolute inset-x-0 top-0"
                style={{ transform: `translate3d(0, ${railWin.offsetY}px, 0)` }}
              >
                {visibleArtistIndices.map((idx) => {
                  const page = Math.floor(idx / ARTIST_PAGE_SIZE);
                  const artist = artistPagesByIndex.get(page)?.[idx - page * ARTIST_PAGE_SIZE];
                  if (!artist) {
                    return <ArtistRailSkeleton key={`slot-${idx}`} />;
                  }
                  const active = artist.name.toLowerCase() === effectiveArtist.toLowerCase();
                  const committedMatch = view?.artist.toLowerCase() === artist.name.toLowerCase();
                  return (
                    <ArtistRailRow
                      key={artist.name}
                      artist={artist}
                      active={active}
                      artworkUrl={(committedMatch ? view?.heroArt : undefined) || artist.artworkUrl}
                      onSelect={handleSelectArtist}
                    />
                  );
                })}
              </div>
            </div>
          )}
        </div>
      </div>

      {/* Right Unified Discography Showcase (Library on top -> subtle divider -> More on Qobuz below) */}
      <div
        className={cn(
          "relative flex-1 min-w-0 min-h-0 flex-col",
          mobileDetailOpen ? "flex" : "hidden md:flex",
        )}
      >
        {effectiveArtist && (
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
        )}

        <div ref={detailRef} data-scroll-stage="" className="relative flex-1 min-h-0 overflow-y-auto">
          {view ? (
            <div
              key={view.artist}
              className={cn(
                "mx-auto max-w-4xl px-4 sm:px-8 pt-6 sm:pt-8 pb-12 transition-opacity duration-150 motion-reduce:transition-none",
                isHoldingPrevious ? "opacity-70 pointer-events-none" : "opacity-100",
              )}
            >
              <div className="flex items-end gap-4 pb-6 border-b border-white/[0.07]">
                <ArtistAvatar
                  artworkUrl={view.heroArt}
                  alt={view.artist}
                  className="h-16 w-16 sm:h-20 sm:w-20 shrink-0 rounded-full shadow-xl ring-1 ring-white/15"
                />
                <div className="min-w-0">
                  <div className="text-[11px] font-semibold uppercase tracking-[0.14em] text-muted-foreground">
                    Artist
                  </div>
                  <h2 className="truncate text-2xl sm:text-3xl font-bold tracking-tight text-white">
                    {view.artist}
                  </h2>
                  <p className="text-xs text-muted-foreground mt-1">
                    {view.summary ? (
                      <>
                        {fmtCount(view.summary.albumCount)} {view.summary.albumCount === 1 ? "album" : "albums"} ·{" "}
                        {fmtCount(view.summary.trackCount)} tracks in library
                      </>
                    ) : view.remoteDetail ? (
                      <>Qobuz Artist · {fmtCount(view.remoteDetail.albums.length)} releases</>
                    ) : (
                      <>In your library</>
                    )}
                  </p>
                </div>
              </div>

              <SmoothReveal open={Boolean(view.remoteDetail?.artist.biography)}>
                {view.remoteDetail?.artist.biography && (
                  <div className="pt-4">
                    <EditorialNote
                      text={view.remoteDetail.artist.biography}
                      title={`About ${view.artist}`}
                      targetWords={30}
                      className="max-w-3xl"
                    />
                  </div>
                )}
              </SmoothReveal>

              {viewErrMsg && (
                <div role="alert" className="mt-4 rounded-xl border border-red-500/30 bg-red-950/50 px-4 py-3 text-sm text-red-200">
                  {viewErrMsg}
                </div>
              )}

              {/* Popular Tracks from Qobuz — smoothly glides in if Qobuz resolves after grace window */}
              <SmoothReveal open={Boolean(qobuzConnected && (view.remoteDetail?.tracks?.length ?? 0) > 0)}>
                {qobuzConnected && (view.remoteDetail?.tracks?.length ?? 0) > 0 && (() => {
                  const popularTracks = view.remoteDetail!.tracks!;
                  const visibleTracks = showAllPopular ? popularTracks : popularTracks.slice(0, 5);
                  const popularIds = popularTracks.map((t) => t.id);
                  return (
                    <section aria-label="Popular tracks" className="pt-6">
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
                            {showAllPopular ? "Show less" : `Show all ${fmtCount(popularTracks.length)}`}
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
              </SmoothReveal>

              {/* Library Albums */}
              {(viewLoadingAlbums || view.albums.length > 0) && (
                <section aria-label="Library albums" className="mt-6 space-y-4">
                  {qobuzConnected && (view.remoteOnlyAlbums.length > 0 || viewLoadingRemote) && view.albums.length > 0 && (
                    <h3 className="text-xs font-semibold uppercase tracking-wider text-muted-foreground">
                      In Your Library
                    </h3>
                  )}

                  {viewLoadingAlbums && view.albums.length === 0 ? (
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
                    view.albums.map((alb) => (
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
              {qobuzConnected && (viewLoadingRemote || view.remoteOnlyAlbums.length > 0 || viewRemoteErr) && (
                <section
                  aria-label="More on Qobuz"
                  className={cn(
                    "space-y-4",
                    view.albums.length > 0 ? "mt-8 border-t border-white/[0.08] pt-6" : "mt-6",
                  )}
                >
                  <div className="flex items-center gap-2">
                    <Sparkles className="h-3.5 w-3.5 text-sky-400" />
                    <h3 className="text-xs font-semibold uppercase tracking-wider text-muted-foreground">
                      {view.albums.length > 0 ? "More on Qobuz" : "On Qobuz"}
                    </h3>
                    {viewLoadingRemote && <Loader2 className="h-3.5 w-3.5 animate-spin text-sky-400" />}
                  </div>

                  {viewRemoteErr && (
                    <div role="alert" className="rounded-xl border border-red-500/30 bg-red-950/40 px-4 py-3 text-xs text-red-200">
                      Couldn’t load Qobuz releases: {viewRemoteErr}
                    </div>
                  )}

                  {view.remoteOnlyAlbums.length > 0 && (
                    <div className="space-y-4">
                      {view.remoteOnlyAlbums.map((alb) => (
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

              {!viewLoadingAlbums && !viewLoadingRemote && view.albums.length === 0 && view.remoteOnlyAlbums.length === 0 && (
                <p className="mt-8 text-sm text-muted-foreground">No albums found for {view.artist}.</p>
              )}

              {/* Contextual Similar Artists */}
              {(view.similarArtists?.length ?? 0) > 0 && (
                <section aria-label="Similar Artists" className="mt-10 border-t border-white/[0.08] pt-6 space-y-4">
                  <h3 className="text-xs font-semibold uppercase tracking-wider text-muted-foreground">
                    Similar Artists
                  </h3>
                  <div className="grid grid-cols-2 sm:grid-cols-3 md:grid-cols-5 gap-3">
                    {view.similarArtists!.map((sim) => (
                      <SimilarArtistCard
                        key={sim.id}
                        id={sim.id}
                        name={sim.name}
                        artworkUrl={sim.artworkUrl}
                        onSelect={onSelectArtist}
                      />
                    ))}
                  </div>
                </section>
              )}
            </div>
          ) : isHoldingCold ? (
            <div className="h-full" />
          ) : (
            <div className="flex h-full items-center justify-center p-8 text-sm text-muted-foreground">
              Select an artist to explore their albums.
            </div>
          )}
        </div>
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
  const hoverPrefetch = useAlbumHoverPrefetch(alb);
  const qLabel = fmtAlbumQuality(alb);
  const hiRes = isHiRes(alb.maxBitDepth, alb.maxSampleRate);
  const isDSD =
    alb.primaryFormat?.toUpperCase() === "DSF" ||
    alb.primaryFormat?.toUpperCase().startsWith("DSD") ||
    (alb.maxBitDepth === 1 && (alb.maxSampleRate ?? 0) >= 2_000_000);
  const hiResLabel = isDSD ? "DSD" : hiRes ? "HI-RES" : "";

  return (
    <div
      {...hoverPrefetch}
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
              {fmtCount(alb.versionCount)} versions
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
            {fmtCount(alb.trackCount)} {alb.trackCount === 1 ? "track" : "tracks"}
            {alb.genre ? ` · ${alb.genre}` : ""}
            {alb.playCount ? ` · ${fmtCount(alb.playCount)} plays` : ""}
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

interface ArtistRailRowProps {
  artist: ArtistSummary;
  active: boolean;
  artworkUrl?: string;
  onSelect: (name: string) => void;
}

const ArtistRailRow = memo(function ArtistRailRow({
  artist,
  active,
  artworkUrl,
  onSelect,
}: ArtistRailRowProps) {
  const hoverPrefetch = useArtistHoverPrefetch(artist.name);

  return (
    <button
      type="button"
      {...hoverPrefetch}
      onClick={() => onSelect(artist.name)}
      className={cn(
        "flex h-14 w-full items-center gap-3 rounded-xl px-2.5 text-left transition-colors cursor-pointer",
        active ? "md:bg-white/[0.1] md:text-white text-foreground/90" : "text-foreground/85 hover:bg-white/[0.04]",
      )}
    >
      <AlbumArt
        artworkUrl={artworkUrl}
        alt={artist.name}
        className="h-10 w-10 shrink-0 rounded-full ring-1 ring-white/10"
      />
      <div className="min-w-0 flex-1">
        <div className="truncate text-[13.5px] font-medium leading-tight">{artist.name}</div>
        <div className="truncate text-xs text-muted-foreground mt-0.5">
          {fmtCount(artist.albumCount)} {artist.albumCount === 1 ? "album" : "albums"} · {fmtCount(artist.trackCount)} tracks
        </div>
      </div>
      <ChevronRight className="h-4 w-4 text-muted-foreground/50 md:hidden shrink-0" />
    </button>
  );
});

const SimilarArtistCard = memo(function SimilarArtistCard({
  id,
  name,
  artworkUrl,
  onSelect,
}: {
  id: number;
  name: string;
  artworkUrl?: string;
  onSelect?: (artistName: string, qobuzArtistId?: number) => void;
}) {
  const hoverPrefetch = useArtistHoverPrefetch(name, id);
  return (
    <button
      type="button"
      {...hoverPrefetch}
      onClick={() => onSelect?.(name, id)}
      className="group flex flex-col items-center gap-2 rounded-xl border border-white/[0.06] bg-white/[0.025] p-3 text-center transition-colors hover:border-white/[0.14] hover:bg-white/[0.05] cursor-pointer"
    >
      <AlbumArt
        artworkUrl={artworkUrl}
        alt={name}
        className="h-16 w-16 rounded-full ring-1 ring-white/10"
      />
      <div className="w-full truncate text-xs font-medium text-foreground group-hover:text-white">
        {name}
      </div>
    </button>
  );
});

const ArtistRailSkeleton = memo(function ArtistRailSkeleton() {
  return (
    <div aria-hidden="true" className="flex h-14 items-center gap-3 px-2.5 animate-pulse">
      <div className="h-10 w-10 shrink-0 rounded-full bg-white/[0.06]" />
      <div className="space-y-1.5 flex-1 min-w-0">
        <div className="h-3.5 w-2/3 rounded bg-white/[0.06]" />
        <div className="h-2.5 w-1/3 rounded bg-white/[0.04]" />
      </div>
    </div>
  );
});

/**
 * Smoothly expands late-arriving enrichment (biography, popular tracks) without a harsh snap,
 * while rendering instantaneously open on frame 1 when data is already cached.
 */
function SmoothReveal({ open, children }: { open: boolean; children: ReactNode }) {
  return (
    <div
      className={cn(
        "grid transition-[grid-template-rows,opacity] duration-300 ease-out motion-reduce:transition-none",
        open ? "grid-rows-[1fr] opacity-100" : "grid-rows-[0fr] opacity-0",
      )}
    >
      <div className="min-h-0 overflow-hidden">{children}</div>
    </div>
  );
}

/**
 * Hero artist avatar that keeps showing the current loaded cover while a higher-priority
 * portrait image downloads, then crossfades smoothly instead of flashing blank.
 */
function ArtistAvatar({ artworkUrl, alt, className }: { artworkUrl?: string; alt: string; className?: string }) {
  const nextSrc = resolveArtworkSrc(artworkUrl, "sm");
  const [baseSrc, setBaseSrc] = useState(nextSrc);
  const [incomingLoaded, setIncomingLoaded] = useState(false);

  const isUpgrading = Boolean(nextSrc && baseSrc && nextSrc !== baseSrc);

  useEffect(() => {
    if (!nextSrc) {
      setBaseSrc("");
      setIncomingLoaded(false);
      return;
    }
    if (!baseSrc) {
      setBaseSrc(nextSrc);
      setIncomingLoaded(false);
      return;
    }
    if (nextSrc !== baseSrc) {
      setIncomingLoaded(false);
    }
  }, [nextSrc, baseSrc]);

  useEffect(() => {
    if (!isUpgrading || !incomingLoaded) return;
    const timer = window.setTimeout(() => {
      setBaseSrc(nextSrc);
      setIncomingLoaded(false);
    }, 310);
    return () => window.clearTimeout(timer);
  }, [isUpgrading, incomingLoaded, nextSrc]);

  if (!baseSrc && !nextSrc) {
    return <AlbumArt artworkUrl={undefined} alt={alt} className={className} priority />;
  }

  return (
    <div className={cn("relative overflow-hidden bg-white/[0.05]", className)}>
      {baseSrc && (
        <img
          src={baseSrc}
          alt={alt}
          loading="eager"
          decoding="async"
          className="h-full w-full object-cover"
        />
      )}
      {isUpgrading && (
        <img
          key={nextSrc}
          src={nextSrc}
          alt={alt}
          loading="eager"
          decoding="async"
          onLoad={() => setIncomingLoaded(true)}
          className={cn(
            "absolute inset-0 h-full w-full object-cover transition-opacity duration-300 ease-out motion-reduce:transition-none",
            incomingLoaded ? "opacity-100" : "opacity-0",
          )}
        />
      )}
    </div>
  );
}
