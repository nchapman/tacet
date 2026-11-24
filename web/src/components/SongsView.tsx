import { keepPreviousData, useQueries, useQuery, useQueryClient } from "@tanstack/react-query";
import { ListPlus, Play, SkipForward } from "lucide-react";
import { memo, useCallback, useEffect, useMemo, useRef } from "react";
import { useSearchParams } from "react-router";
import { tacetClient } from "@/api/client";
import { fmtCount, fmtDuration, fmtQuality } from "@/api/format";
import type { GenreSummary, QueuePosition, QueueSource, Track } from "@/api/types";
import { AlbumArt } from "@/components/ui/artwork";
import { PageBody, PageHeader } from "@/components/ui/PageHeader";
import { Select, SelectContent, SelectItem, SelectTrigger, SelectValue } from "@/components/ui/select";
import { useAlbumHoverPrefetch, useArtistHoverPrefetch } from "@/lib/prefetch";
import { queryErrorMessage, queryKeys } from "@/lib/query";
import { cn } from "@/lib/utils";
import { useVirtualWindow } from "@/lib/virtualWindow";

type TrackSort = "artist" | "title" | "album" | "added" | "plays" | "quality";

const VALID_TRACK_SORTS = new Set<TrackSort>(["artist", "title", "album", "added", "plays", "quality"]);

const PAGE_SIZE = 100;
const ROW_HEIGHT = 56;
const LIST_PADDING_TOP = 12;
const OVERSCAN = 12;
const PREFETCH_MARGIN = 25;

const SKELETON_TITLE_WIDTHS = ["w-44", "w-56", "w-36", "w-64", "w-48", "w-52"] as const;
const SKELETON_ARTIST_WIDTHS = ["w-28", "w-36", "w-24", "w-32", "w-28", "w-40"] as const;

interface SongsViewProps {
  currentTrackId?: string;
  onOpenAlbumId: (albumId: string) => void;
  onOpenArtist: (artist: string) => void;
  onPlay: (source: QueueSource, startIndex?: number) => void;
  onAdd: (source: QueueSource, position?: QueuePosition) => void;
}

function trackPageQueryOptions(genre: string, sort: TrackSort, dir: "asc" | "desc", page: number) {
  const offset = page * PAGE_SIZE;
  return {
    queryKey: queryKeys.library.tracks({
      genre: genre || undefined,
      sort,
      dir,
      limit: PAGE_SIZE,
      offset,
    }),
    queryFn: ({ signal }: { signal: AbortSignal }) =>
      tacetClient.listLibraryTracks(
        {
          genre: genre || undefined,
          sort,
          dir,
          limit: PAGE_SIZE,
          offset,
        },
        { signal },
      ),
  };
}

/**
 * Windowed / virtualized Songs list backed by random-access paged queries.
 * The native scrollbar reflects the full library size while only visible
 * pages (plus adjacent lookahead) are fetched and mounted.
 */
export const SongsView = memo(function SongsView({
  currentTrackId,
  onOpenAlbumId,
  onOpenArtist,
  onPlay,
  onAdd,
}: SongsViewProps) {
  const [searchParams, setSearchParams] = useSearchParams();
  const queryClient = useQueryClient();
  const scrollRef = useRef<HTMLDivElement>(null);

  const rawSort = searchParams.get("sort") as TrackSort | null;
  const sort: TrackSort = rawSort && VALID_TRACK_SORTS.has(rawSort) ? rawSort : "artist";
  const genre = searchParams.get("genre") ?? "";

  const updateParams = useCallback(
    (patch: { sort?: TrackSort; genre?: string }) => {
      const next = new URLSearchParams(searchParams);
      if (patch.sort !== undefined) {
        if (patch.sort === "artist") next.delete("sort");
        else next.set("sort", patch.sort);
      }
      if (patch.genre !== undefined) {
        if (!patch.genre) next.delete("genre");
        else next.set("genre", patch.genre);
      }
      scrollRef.current?.scrollTo({ top: 0 });
      setSearchParams(next, { replace: true });
    },
    [searchParams, setSearchParams],
  );

  const dir: "asc" | "desc" = sort === "plays" || sort === "added" || sort === "quality" ? "desc" : "asc";

  const genresQuery = useQuery({
    queryKey: queryKeys.library.genres,
    queryFn: ({ signal }) => tacetClient.listLibraryGenres({}, { signal }),
  });

  // Page 0 establishes total count immediately (full scrollbar height) and seeds rows 0..99.
  const page0Query = useQuery({
    ...trackPageQueryOptions(genre, sort, dir, 0),
    placeholderData: keepPreviousData,
  });

  const genres: GenreSummary[] = genresQuery.data?.items ?? [];
  const total = page0Query.data?.total ?? 0;
  const initialLoading = page0Query.isPending;

  const win = useVirtualWindow({
    scrollRef,
    totalCount: total,
    rowHeight: ROW_HEIGHT,
    pageSize: PAGE_SIZE,
    overscan: OVERSCAN,
    prefetchMargin: PREFETCH_MARGIN,
    paddingTop: LIST_PADDING_TOP,
  });

  const extraVisiblePages = useMemo(
    () => (page0Query.isPlaceholderData ? [] : win.visiblePages.filter((p) => p > 0)),
    [page0Query.isPlaceholderData, win.visiblePages],
  );

  // Subscribe to visible pages beyond page 0; while fast-scrubbing (settled === false),
  // cached pages still render immediately without firing intermediate network requests.
  const pageQueries = useQueries({
    queries: extraVisiblePages.map((page) => ({
      ...trackPageQueryOptions(genre, sort, dir, page),
      enabled: win.settled,
    })),
  });

  // Silently prefetch adjacent pages when approaching a page boundary.
  useEffect(() => {
    if (!win.settled || page0Query.isPlaceholderData) return;
    for (const page of win.prefetchPages) {
      if (page <= 0) continue;
      void queryClient.prefetchQuery(trackPageQueryOptions(genre, sort, dir, page));
    }
  }, [win.settled, win.prefetchPages, page0Query.isPlaceholderData, queryClient, genre, sort, dir]);

  const pagesByIndex = useMemo(() => {
    const map = new Map<number, Track[]>();
    if (page0Query.data?.items) {
      map.set(0, page0Query.data.items);
    }
    extraVisiblePages.forEach((page, i) => {
      const items = pageQueries[i]?.data?.items;
      if (items) map.set(page, items);
    });
    return map;
  }, [page0Query.data?.items, extraVisiblePages, pageQueries]);

  const pageError = pageQueries.find((q) => q.error)?.error;
  const errMsg = queryErrorMessage(page0Query.error ?? pageError);

  const visibleIndices = useMemo(() => {
    if (win.endIndex < win.startIndex) return [];
    const list: number[] = [];
    for (let i = win.startIndex; i <= win.endIndex; i++) {
      list.push(i);
    }
    return list;
  }, [win.startIndex, win.endIndex]);

  return (
    <div className="flex h-full min-h-0 flex-col">
      <PageHeader className="py-2.5">
        <div className="flex w-full flex-wrap items-center justify-between gap-3">
          <div className="flex items-baseline gap-2.5">
            <h1 className="text-xl sm:text-2xl font-bold tracking-tight text-white">Songs</h1>
            <span className="text-xs font-mono text-muted-foreground">{fmtCount(total)}</span>
          </div>

          <div className="flex flex-wrap items-center gap-2.5">
            {genres.length > 0 && (
              <Select
                value={genre || "__all__"}
                onValueChange={(v) => updateParams({ genre: v === "__all__" ? "" : v })}
              >
                <SelectTrigger aria-label="Filter by genre" className="max-w-[190px]">
                  <SelectValue />
                </SelectTrigger>
                <SelectContent align="end">
                  <SelectItem value="__all__">All genres</SelectItem>
                  {genres.map((g) => (
                    <SelectItem key={g.name} value={g.name}>
                      {g.name} ({fmtCount(g.trackCount)})
                    </SelectItem>
                  ))}
                </SelectContent>
              </Select>
            )}

            <Select value={sort} onValueChange={(v) => updateParams({ sort: v as TrackSort })}>
              <SelectTrigger aria-label="Sort songs" className="min-w-[136px]">
                <SelectValue />
              </SelectTrigger>
              <SelectContent align="end">
                <SelectItem value="artist">Artist</SelectItem>
                <SelectItem value="title">Title</SelectItem>
                <SelectItem value="album">Album</SelectItem>
                <SelectItem value="added">Recently Added</SelectItem>
                <SelectItem value="plays">Most Played</SelectItem>
                <SelectItem value="quality">Highest Quality</SelectItem>
              </SelectContent>
            </Select>
          </div>
        </div>
      </PageHeader>

      <PageBody scrollRef={scrollRef}>
        <div className="px-3 sm:px-8 pt-3">
          {errMsg && (
            <div role="alert" className="my-4 rounded-xl border border-red-500/30 bg-red-950/50 px-4 py-3 text-sm text-red-200">
              {errMsg}
            </div>
          )}

          {initialLoading && total === 0 ? (
            <div>
              {Array.from({ length: 12 }).map((_, i) => (
                <SongRowSkeleton key={i} index={i} />
              ))}
            </div>
          ) : total === 0 ? (
            <p className="py-16 text-center text-sm text-muted-foreground">No matching songs.</p>
          ) : (
            <div className="relative" style={{ height: win.totalHeight }}>
              <div
                className="absolute inset-x-0 top-0"
                style={{ transform: `translate3d(0, ${win.offsetY}px, 0)` }}
              >
                {visibleIndices.map((idx) => {
                  const page = Math.floor(idx / PAGE_SIZE);
                  const tr = pagesByIndex.get(page)?.[idx - page * PAGE_SIZE];
                  if (!tr) {
                    return <SongRowSkeleton key={`slot-${idx}`} index={idx} />;
                  }
                  return (
                    <SongRow
                      key={tr.id}
                      track={tr}
                      active={tr.id === currentTrackId}
                      onOpenAlbumId={onOpenAlbumId}
                      onOpenArtist={onOpenArtist}
                      onPlay={onPlay}
                      onAdd={onAdd}
                    />
                  );
                })}
              </div>
            </div>
          )}
        </div>
      </PageBody>
    </div>
  );
});

interface SongRowProps {
  track: Track;
  active: boolean;
  onOpenAlbumId: (albumId: string) => void;
  onOpenArtist: (artist: string) => void;
  onPlay: (source: QueueSource, startIndex?: number) => void;
  onAdd: (source: QueueSource, position?: QueuePosition) => void;
}

const SongRow = memo(function SongRow({
  track: tr,
  active,
  onOpenAlbumId,
  onOpenArtist,
  onPlay,
  onAdd,
}: SongRowProps) {
  const qual = fmtQuality(tr);
  const artistPrefetch = useArtistHoverPrefetch(tr.artist);
  const albumPrefetch = useAlbumHoverPrefetch(tr.albumId);

  return (
    <div
      className={cn(
        "group grid h-14 grid-cols-[1fr_auto] md:grid-cols-[minmax(0,2fr)_minmax(0,1.2fr)_175px_60px_68px] items-center gap-3 rounded-xl border-b border-white/[0.03] px-2.5",
        active ? "bg-white/[0.08]" : "hover:bg-white/[0.06]",
      )}
    >
      {/* Cover thumb + Title & Artist */}
      <div className="flex min-w-0 items-center gap-3">
        <button
          type="button"
          onClick={() => onPlay({ type: "tracks", trackIds: [tr.id] }, 0)}
          aria-label={`Play ${tr.title}`}
          className="relative h-10 w-10 shrink-0 overflow-hidden rounded-md ring-1 ring-white/10 cursor-pointer"
        >
          <AlbumArt artworkUrl={tr.artworkUrl} alt={tr.album || tr.title} priority className="h-full w-full" />
          <span className="absolute inset-0 flex items-center justify-center bg-black/55 opacity-0 group-hover:opacity-100 transition-opacity">
            <Play className="h-3.5 w-3.5 fill-white text-white" />
          </span>
        </button>
        <div className="min-w-0 flex-1">
          <button
            type="button"
            onClick={() => onPlay({ type: "tracks", trackIds: [tr.id] }, 0)}
            className={cn(
              "block max-w-full truncate text-[13.5px] font-medium leading-snug text-left cursor-pointer",
              active ? "text-emerald-400" : "text-foreground hover:text-white",
            )}
          >
            {tr.title}
          </button>
          <button
            type="button"
            {...artistPrefetch}
            onClick={() => onOpenArtist(tr.artist)}
            className="block max-w-full truncate text-xs text-muted-foreground hover:text-foreground hover:underline text-left cursor-pointer"
          >
            {tr.artist}
          </button>
        </div>
      </div>

      {/* Album link (md+) */}
      <div className="hidden md:block min-w-0">
        {tr.albumId ? (
          <button
            type="button"
            {...albumPrefetch}
            onClick={() => onOpenAlbumId(tr.albumId!)}
            className="block max-w-full truncate text-xs text-muted-foreground hover:text-foreground hover:underline text-left cursor-pointer"
          >
            {tr.album}
          </button>
        ) : (
          <span className="block truncate text-xs text-muted-foreground">{tr.album}</span>
        )}
      </div>

      {/* Format (md+) */}
      <div className="hidden md:flex items-center">
        {qual && <span className="font-mono text-[11px] text-muted-foreground/75">{qual}</span>}
      </div>

      {/* Duration (md+) */}
      <span className="hidden md:block text-right font-mono text-xs tabular-nums text-muted-foreground">
        {fmtDuration(tr.durationSec)}
      </span>

      {/* Actions */}
      <div className="flex items-center justify-end gap-0.5">
        <button
          type="button"
          title="Play Next"
          onClick={() => onAdd({ type: "tracks", trackIds: [tr.id] }, "next")}
          className="rounded-full p-1.5 text-muted-foreground hover:bg-white/10 hover:text-foreground transition-colors cursor-pointer md:opacity-0 md:group-hover:opacity-100"
        >
          <SkipForward className="h-3.5 w-3.5" />
        </button>
        <button
          type="button"
          title="Append to Queue"
          onClick={() => onAdd({ type: "tracks", trackIds: [tr.id] }, "end")}
          className="rounded-full p-1.5 text-muted-foreground hover:bg-white/10 hover:text-foreground transition-colors cursor-pointer md:opacity-0 md:group-hover:opacity-100"
        >
          <ListPlus className="h-3.5 w-3.5" />
        </button>
      </div>
    </div>
  );
});

const SongRowSkeleton = memo(function SongRowSkeleton({ index }: { index: number }) {
  const titleW = SKELETON_TITLE_WIDTHS[index % SKELETON_TITLE_WIDTHS.length];
  const artistW = SKELETON_ARTIST_WIDTHS[index % SKELETON_ARTIST_WIDTHS.length];

  return (
    <div
      aria-hidden="true"
      className="grid h-14 grid-cols-[1fr_auto] md:grid-cols-[minmax(0,2fr)_minmax(0,1.2fr)_175px_60px_68px] items-center gap-3 border-b border-white/[0.03] px-2.5 animate-pulse"
    >
      <div className="flex min-w-0 items-center gap-3">
        <div className="h-10 w-10 shrink-0 rounded-md bg-white/[0.06]" />
        <div className="min-w-0 flex-1 space-y-1.5">
          <div className={cn("h-3.5 max-w-[75%] rounded bg-white/[0.06]", titleW)} />
          <div className={cn("h-3 max-w-[50%] rounded bg-white/[0.04]", artistW)} />
        </div>
      </div>
      <div className="hidden md:block">
        <div className="h-3 w-32 rounded bg-white/[0.04]" />
      </div>
      <div className="hidden md:block">
        <div className="h-3 w-24 rounded bg-white/[0.04]" />
      </div>
      <div className="hidden md:flex justify-end">
        <div className="h-3 w-9 rounded bg-white/[0.04]" />
      </div>
      <div className="w-[68px]" />
    </div>
  );
});
