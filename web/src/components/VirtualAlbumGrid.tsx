import { keepPreviousData, useQueries, useQuery, useQueryClient } from "@tanstack/react-query";
import { memo, type RefObject, useEffect, useMemo } from "react";
import { tacetClient } from "@/api/client";
import type { Album, QueueSource } from "@/api/types";
import { AlbumCard } from "@/components/AlbumCard";
import { queryErrorMessage, queryKeys } from "@/lib/query";
import { useVirtualGrid, type VirtualGridState } from "@/lib/virtualWindow";

export const ALBUM_GRID_PAGE_SIZE = 60;
const COLUMN_GAP = 20; // gap-x-5
const ROW_GAP = 28; // gap-y-7
const CARD_FOOTER_HEIGHT = 48; // mt-2.5 + single-line title + mt-0.5 + single-line artist
const OVERSCAN_ROWS = 3;

export interface PagedAlbumQueryParams {
  source?: "all" | "local" | "remote";
  provider?: string;
  sort?: "artist" | "title" | "year" | "added" | "plays";
  groupVersions?: boolean;
}

function albumPageQueryOptions(params: PagedAlbumQueryParams, page: number) {
  const offset = page * ALBUM_GRID_PAGE_SIZE;
  return {
    queryKey: queryKeys.library.albums({
      source: params.source,
      provider: params.provider,
      sort: params.sort,
      groupVersions: params.groupVersions,
      limit: ALBUM_GRID_PAGE_SIZE,
      offset,
    }),
    queryFn: ({ signal }: { signal: AbortSignal }) =>
      tacetClient.listLibraryAlbums(
        {
          source: params.source,
          provider: params.provider,
          sort: params.sort,
          groupVersions: params.groupVersions,
          limit: ALBUM_GRID_PAGE_SIZE,
          offset,
        },
        { signal },
      ),
  };
}

interface UsePagedAlbumGridOptions {
  scrollRef: RefObject<HTMLElement | null>;
  gridRef: RefObject<HTMLElement | null>;
  queryParams: PagedAlbumQueryParams;
  enabled?: boolean;
}

export interface PagedAlbumGridState {
  total: number;
  initialLoading: boolean;
  errMsg: string;
  win: VirtualGridState;
  pagesByIndex: Map<number, Album[]>;
}

/**
 * Windowed random-access album grid loader. Call in the parent view above <PageBody>
 * so its useLayoutEffect runs right after PageBody's scroll restoration before paint.
 */
export function usePagedAlbumGrid({
  scrollRef,
  gridRef,
  queryParams,
  enabled = true,
}: UsePagedAlbumGridOptions): PagedAlbumGridState {
  const queryClient = useQueryClient();
  const { source, provider, sort, groupVersions } = queryParams;

  const page0Query = useQuery({
    ...albumPageQueryOptions({ source, provider, sort, groupVersions }, 0),
    placeholderData: keepPreviousData,
    enabled,
  });

  const total = page0Query.data?.total ?? 0;
  const initialLoading = page0Query.isPending;

  const win = useVirtualGrid({
    scrollRef,
    gridRef,
    totalCount: total,
    pageSize: ALBUM_GRID_PAGE_SIZE,
    columnGap: COLUMN_GAP,
    rowGap: ROW_GAP,
    cardFooterHeight: CARD_FOOTER_HEIGHT,
    overscanRows: OVERSCAN_ROWS,
    enabled,
  });

  const extraVisiblePages = useMemo(
    () => (!enabled || page0Query.isPlaceholderData ? [] : win.visiblePages.filter((p) => p > 0)),
    [enabled, page0Query.isPlaceholderData, win.visiblePages],
  );

  const pageQueries = useQueries({
    queries: extraVisiblePages.map((page) => ({
      ...albumPageQueryOptions({ source, provider, sort, groupVersions }, page),
      enabled: enabled && win.settled,
    })),
  });

  useEffect(() => {
    if (!enabled || !win.settled || page0Query.isPlaceholderData) return;
    for (const page of win.prefetchPages) {
      if (page <= 0) continue;
      void queryClient.prefetchQuery(albumPageQueryOptions({ source, provider, sort, groupVersions }, page));
    }
  }, [enabled, win.settled, win.prefetchPages, page0Query.isPlaceholderData, queryClient, source, provider, sort, groupVersions]);

  const pagesByIndex = useMemo(() => {
    const map = new Map<number, Album[]>();
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

  return {
    total,
    initialLoading,
    errMsg,
    win,
    pagesByIndex,
  };
}

interface VirtualAlbumGridProps {
  win: VirtualGridState;
  pagesByIndex: Map<number, Album[]>;
  onOpenAlbum: (album: Album) => void;
  onPlay: (source: QueueSource, startIndex?: number) => void;
}

export const VirtualAlbumGrid = memo(function VirtualAlbumGrid({
  win,
  pagesByIndex,
  onOpenAlbum,
  onPlay,
}: VirtualAlbumGridProps) {
  const visibleIndices = useMemo(() => {
    if (win.endIndex < win.startIndex) return [];
    const list: number[] = [];
    for (let i = win.startIndex; i <= win.endIndex; i++) {
      list.push(i);
    }
    return list;
  }, [win.startIndex, win.endIndex]);

  return (
    <div className="relative" style={{ height: win.totalHeight }}>
      <div
        className="absolute inset-x-0 top-0 grid"
        style={{
          transform: `translate3d(0, ${win.offsetY}px, 0)`,
          gridTemplateColumns: `repeat(${win.columns}, minmax(0, 1fr))`,
          gridAutoRows: `${win.cardHeight}px`,
          columnGap: `${COLUMN_GAP}px`,
          rowGap: `${ROW_GAP}px`,
        }}
      >
        {visibleIndices.map((idx) => {
          const page = Math.floor(idx / ALBUM_GRID_PAGE_SIZE);
          const alb = pagesByIndex.get(page)?.[idx - page * ALBUM_GRID_PAGE_SIZE];
          if (!alb) {
            return <AlbumCardSkeleton key={`slot-${idx}`} />;
          }
          return (
            <AlbumCard
              key={alb.id}
              album={alb}
              priority
              onOpen={onOpenAlbum}
              onPlay={onPlay}
            />
          );
        })}
      </div>
    </div>
  );
});

export const AlbumCardSkeleton = memo(function AlbumCardSkeleton() {
  return (
    <div aria-hidden="true" className="flex flex-col animate-pulse">
      <div className="aspect-square w-full rounded-lg bg-white/[0.06]" />
      <div className="mt-2.5 h-3.5 w-3/4 rounded bg-white/[0.06]" />
      <div className="mt-1.5 h-3 w-1/2 rounded bg-white/[0.04]" />
    </div>
  );
});
