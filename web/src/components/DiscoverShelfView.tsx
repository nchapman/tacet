import { keepPreviousData, useInfiniteQuery, useQuery } from "@tanstack/react-query";
import { ArrowLeft, Loader2 } from "lucide-react";
import { memo, useMemo } from "react";
import { Link, useParams, useSearchParams } from "react-router";
import { tacetClient } from "@/api/client";
import type { Album, QueueSource } from "@/api/types";
import { AlbumCard } from "@/components/AlbumCard";
import { Button } from "@/components/ui/button";
import { QOBUZ_SEARCH_GC_TIME_MS, QOBUZ_SEARCH_STALE_TIME_MS, queryErrorMessage, queryKeys } from "@/lib/query";
import { cn } from "@/lib/utils";

interface DiscoverShelfViewProps {
  onOpenAlbum: (album: Album) => void;
  onPlay: (source: QueueSource, startIndex?: number) => void;
}

const PAGE_SIZE = 24;

export const DiscoverShelfView = memo(function DiscoverShelfView({
  onOpenAlbum,
  onPlay,
}: DiscoverShelfViewProps) {
  const { slug = "" } = useParams();
  const [searchParams, setSearchParams] = useSearchParams();
  const rawGenre = searchParams.get("genre");
  const selectedGenreId = rawGenre && /^\d+$/.test(rawGenre) ? Number(rawGenre) : null;

  const feedQuery = useQuery({
    queryKey: queryKeys.discover.feed(null),
    queryFn: ({ signal }) => tacetClient.getDiscover(undefined, { signal }),
    staleTime: QOBUZ_SEARCH_STALE_TIME_MS,
    gcTime: QOBUZ_SEARCH_GC_TIME_MS,
  });

  const shelfQuery = useInfiniteQuery({
    queryKey: queryKeys.discover.shelf(slug, selectedGenreId),
    queryFn: ({ pageParam, signal }) =>
      tacetClient.getDiscoverShelf(
        slug,
        {
          genreIds: selectedGenreId ? [selectedGenreId] : undefined,
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
  const genres = feedQuery.data?.genres ?? [];
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

  const setGenre = (genreId: number | null) => {
    const next = new URLSearchParams(searchParams);
    if (genreId) next.set("genre", String(genreId));
    else next.delete("genre");
    setSearchParams(next, { replace: true });
  };

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
          <h1 className="text-2xl sm:text-3xl font-bold tracking-tight text-white">
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

      {genres.length > 0 && (
        <div
          role="tablist"
          aria-label="Filter shelf by genre"
          className="flex items-center gap-1.5 overflow-x-auto pb-1 scrollbar-none"
        >
          <button
            type="button"
            role="tab"
            aria-selected={selectedGenreId === null}
            onClick={() => setGenre(null)}
            className={cn(
              "shrink-0 rounded-full px-3 py-1 text-xs font-medium transition-colors cursor-pointer",
              selectedGenreId === null
                ? "bg-white text-black"
                : "bg-white/[0.06] text-muted-foreground hover:bg-white/[0.11] hover:text-foreground",
            )}
          >
            All Genres
          </button>
          {genres.map((genre) => {
            const active = selectedGenreId === genre.id;
            return (
              <button
                key={genre.id}
                type="button"
                role="tab"
                aria-selected={active}
                onClick={() => setGenre(active ? null : genre.id)}
                className={cn(
                  "shrink-0 rounded-full px-3 py-1 text-xs font-medium transition-colors cursor-pointer",
                  active
                    ? "bg-white text-black"
                    : "bg-white/[0.06] text-muted-foreground hover:bg-white/[0.11] hover:text-foreground",
                )}
              >
                {genre.name}
              </button>
            );
          })}
        </div>
      )}

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
          No albums found for this genre.
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
