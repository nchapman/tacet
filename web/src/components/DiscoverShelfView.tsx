import { keepPreviousData, useInfiniteQuery } from "@tanstack/react-query";
import { ArrowLeft, Loader2 } from "lucide-react";
import { memo, useMemo } from "react";
import { Link, useParams } from "react-router";
import { tacetClient } from "@/api/client";
import type { Album, QueueSource } from "@/api/types";
import { AlbumCard } from "@/components/AlbumCard";
import { Button } from "@/components/ui/button";
import { QOBUZ_SEARCH_GC_TIME_MS, QOBUZ_SEARCH_STALE_TIME_MS, queryErrorMessage, queryKeys } from "@/lib/query";
import { cn } from "@/lib/utils";

interface DiscoverShelfViewProps {
  discoverGenreIds: number[];
  onOpenAlbum: (album: Album) => void;
  onPlay: (source: QueueSource, startIndex?: number) => void;
}

const PAGE_SIZE = 24;

export const DiscoverShelfView = memo(function DiscoverShelfView({
  discoverGenreIds,
  onOpenAlbum,
  onPlay,
}: DiscoverShelfViewProps) {
  const { slug = "" } = useParams();

  const shelfQuery = useInfiniteQuery({
    queryKey: queryKeys.discover.shelf(slug, discoverGenreIds),
    queryFn: ({ pageParam, signal }) =>
      tacetClient.getDiscoverShelf(
        slug,
        {
          genreIds: discoverGenreIds.length > 0 ? discoverGenreIds : undefined,
          limit: PAGE_SIZE,
          offset: pageParam,
        },
        { signal },
      ),
    initialPageParam: 0,
    getNextPageParam: (lastPage) =>
      lastPage.hasMore && lastPage.nextOffset && lastPage.nextOffset > lastPage.offset
        ? lastPage.nextOffset
        : undefined,
    staleTime: QOBUZ_SEARCH_STALE_TIME_MS,
    gcTime: QOBUZ_SEARCH_GC_TIME_MS,
    placeholderData: keepPreviousData,
    enabled: slug.length > 0,
  });

  const firstPage = shelfQuery.data?.pages[0];
  const albums = useMemo(() => {
    const pages = shelfQuery.data?.pages ?? [];
    const seen = new Set<string>();
    const out: Album[] = [];
    for (const page of pages) {
      for (const alb of page.items) {
        if (seen.has(alb.id)) continue;
        seen.add(alb.id);
        out.push(alb);
      }
    }
    return out;
  }, [shelfQuery.data?.pages]);

  return (
    <div className="mx-auto max-w-7xl px-4 sm:px-8 py-6 sm:py-8 space-y-6">
      <div className="space-y-2">
        <Link
          to="/"
          className="inline-flex items-center gap-1.5 text-xs font-medium text-muted-foreground hover:text-foreground transition-colors"
        >
          <ArrowLeft className="h-3.5 w-3.5" />
          <span>Back to Home</span>
        </Link>

        <div className="flex items-center gap-3">
          <h1 className="text-xl sm:text-2xl font-bold tracking-tight text-white">
            {firstPage?.title ?? slug.replace(/-/g, " ")}
          </h1>
          {shelfQuery.isFetching && !shelfQuery.isFetchingNextPage && (
            <Loader2 className="h-4 w-4 animate-spin text-muted-foreground" />
          )}
        </div>
        {firstPage?.subtitle && (
          <p className="text-sm text-muted-foreground">{firstPage.subtitle}</p>
        )}
      </div>

      {shelfQuery.error && (
        <div role="alert" className="rounded-xl border border-red-500/30 bg-red-950/50 px-4 py-3 text-sm text-red-200">
          {queryErrorMessage(shelfQuery.error)}
        </div>
      )}

      {shelfQuery.isPending && albums.length === 0 ? (
        <div className="grid grid-cols-2 sm:grid-cols-3 md:grid-cols-4 xl:grid-cols-6 gap-x-5 gap-y-7">
          {Array.from({ length: 18 }).map((_, i) => (
            <div key={i} className="space-y-2.5 animate-pulse">
              <div className="aspect-square w-full rounded-lg bg-white/[0.06]" />
              <div className="h-3.5 w-3/4 rounded bg-white/[0.06]" />
              <div className="h-3 w-1/2 rounded bg-white/[0.04]" />
            </div>
          ))}
        </div>
      ) : albums.length === 0 ? (
        <p className="rounded-xl border border-white/[0.06] bg-white/[0.02] px-4 py-6 text-center text-sm text-muted-foreground">
          No albums found for your selected genres.
        </p>
      ) : (
        <div
          className={cn(
            "grid grid-cols-2 sm:grid-cols-3 md:grid-cols-4 xl:grid-cols-6 gap-x-5 gap-y-7 transition-opacity duration-200",
            shelfQuery.isPlaceholderData && "opacity-60",
          )}
        >
          {albums.map((alb) => (
            <AlbumCard
              key={alb.id}
              album={alb}
              showInLibraryBadge
              onOpen={onOpenAlbum}
              onPlay={onPlay}
            />
          ))}
        </div>
      )}

      {shelfQuery.hasNextPage && (
        <div className="flex justify-center pt-4">
          <Button
            type="button"
            variant="secondary"
            className="rounded-full px-6"
            disabled={shelfQuery.isFetchingNextPage}
            onClick={() => void shelfQuery.fetchNextPage()}
          >
            {shelfQuery.isFetchingNextPage ? (
              <>
                <Loader2 className="h-4 w-4 animate-spin" />
                Loading…
              </>
            ) : (
              "Load more"
            )}
          </Button>
        </div>
      )}
    </div>
  );
});
