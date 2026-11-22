import { keepPreviousData, useQuery } from "@tanstack/react-query";
import { memo, useCallback } from "react";
import { useSearchParams } from "react-router";
import { tacetClient } from "@/api/client";
import type { Album, LibraryStatus, QueueSource } from "@/api/types";
import { AlbumCard } from "@/components/AlbumCard";
import { queryErrorMessage, queryKeys } from "@/lib/query";
import { cn } from "@/lib/utils";

type SourceFilter = "all" | "local" | "remote";
type AlbumSort = "artist" | "title" | "year" | "added" | "plays";

const VALID_SOURCES = new Set<SourceFilter>(["all", "local", "remote"]);
const VALID_SORTS = new Set<AlbumSort>(["artist", "title", "year", "added", "plays"]);

interface AlbumsViewProps {
  libraryStatus: LibraryStatus | null;
  groupVersions: boolean;
  onOpenAlbum: (album: Album) => void;
  onPlay: (source: QueueSource, startIndex?: number) => void;
}

export const AlbumsView = memo(function AlbumsView({ libraryStatus, groupVersions, onOpenAlbum, onPlay }: AlbumsViewProps) {
  const [searchParams, setSearchParams] = useSearchParams();

  const rawSource = searchParams.get("source") as SourceFilter | null;
  const source: SourceFilter = rawSource && VALID_SOURCES.has(rawSource) ? rawSource : "all";
  const rawSort = searchParams.get("sort") as AlbumSort | null;
  const sort: AlbumSort = rawSort && VALID_SORTS.has(rawSort) ? rawSort : "artist";

  const updateParams = useCallback(
    (patch: { source?: SourceFilter; sort?: AlbumSort }) => {
      const next = new URLSearchParams(searchParams);
      if (patch.source !== undefined) {
        if (patch.source === "all") next.delete("source");
        else next.set("source", patch.source);
      }
      if (patch.sort !== undefined) {
        if (patch.sort === "artist") next.delete("sort");
        else next.set("sort", patch.sort);
      }
      setSearchParams(next, { replace: true });
    },
    [searchParams, setSearchParams],
  );

  const albumsQuery = useQuery({
    queryKey: queryKeys.library.albums({ source, sort, groupVersions, limit: 500 }),
    queryFn: ({ signal }) =>
      tacetClient.listLibraryAlbums(
        {
          source,
          sort,
          groupVersions,
          limit: 500,
        },
        { signal },
      ),
    placeholderData: keepPreviousData,
  });

  const albums = albumsQuery.data?.items ?? [];
  const total = albumsQuery.data?.total ?? 0;
  const loading = albumsQuery.isPending;
  const errMsg = queryErrorMessage(albumsQuery.error);

  const hasRemote = (libraryStatus?.remoteAlbums ?? 0) > 0;

  return (
    <div className="min-h-full pb-12">
      {/* Sticky sleek filter header */}
      <div className="sticky top-0 z-20 border-b border-white/[0.06] bg-[#09090b]/45 px-4 sm:px-8 py-4 backdrop-blur-2xl">
        <div className="mx-auto max-w-7xl flex flex-wrap items-center justify-between gap-3">
          <div className="flex items-baseline gap-2.5">
            <h1 className="text-xl sm:text-2xl font-bold tracking-tight text-white">Albums</h1>
            <span className="text-xs font-mono text-muted-foreground">{total}</span>
          </div>

          <div className="flex flex-wrap items-center gap-3">
            {hasRemote && (
              <div className="inline-flex rounded-full border border-white/[0.08] bg-white/[0.03] p-0.5">
                {(["all", "local", "remote"] as SourceFilter[]).map((f) => (
                  <button
                    key={f}
                    type="button"
                    onClick={() => updateParams({ source: f })}
                    className={cn(
                      "rounded-full px-3 py-1 text-xs font-medium transition-colors cursor-pointer",
                      source === f ? "bg-white/[0.14] text-white" : "text-muted-foreground hover:text-foreground",
                    )}
                  >
                    {f === "all" ? "All" : f === "local" ? "Local" : "Qobuz"}
                  </button>
                ))}
              </div>
            )}

            <select
              aria-label="Sort albums"
              value={sort}
              onChange={(e) => updateParams({ sort: e.target.value as AlbumSort })}
              className="h-8.5 rounded-lg border border-white/[0.09] bg-white/[0.05] px-2.5 text-xs text-foreground focus:outline-none focus:border-white/25 cursor-pointer"
            >
              <option value="artist">Artist</option>
              <option value="title">Title</option>
              <option value="year">Year</option>
              <option value="added">Recently Added</option>
              <option value="plays">Most Played</option>
            </select>
          </div>
        </div>
      </div>

      <div className="mx-auto max-w-7xl px-4 sm:px-8 pt-6">
        {errMsg && (
          <div role="alert" className="mb-6 rounded-xl border border-red-500/30 bg-red-950/50 px-4 py-3 text-sm text-red-200">
            {errMsg}
          </div>
        )}

        {loading && albums.length === 0 ? (
          <div className="grid grid-cols-2 sm:grid-cols-3 md:grid-cols-4 xl:grid-cols-6 gap-5">
            {Array.from({ length: 18 }).map((_, i) => (
              <div key={i} className="space-y-2.5 animate-pulse">
                <div className="aspect-square w-full rounded-lg bg-white/[0.06]" />
                <div className="h-3.5 w-3/4 rounded bg-white/[0.06]" />
                <div className="h-3 w-1/2 rounded bg-white/[0.04]" />
              </div>
            ))}
          </div>
        ) : albums.length === 0 ? (
          <p className="py-16 text-center text-sm text-muted-foreground">No matching albums in your library.</p>
        ) : (
          <div className="grid grid-cols-2 sm:grid-cols-3 md:grid-cols-4 xl:grid-cols-6 gap-x-5 gap-y-7">
            {albums.map((alb) => (
              <AlbumCard
                key={alb.id}
                album={alb}
                onOpen={onOpenAlbum}
                onPlay={onPlay}
              />
            ))}
          </div>
        )}
      </div>
    </div>
  );
});
