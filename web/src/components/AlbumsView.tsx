import { memo, useCallback, useRef } from "react";
import { useSearchParams } from "react-router";
import { fmtCount } from "@/api/format";
import type { Album, LibraryStatus, QueueSource } from "@/api/types";
import { PageBody, PageHeader } from "@/components/ui/PageHeader";
import { Select, SelectContent, SelectItem, SelectTrigger, SelectValue } from "@/components/ui/select";
import { AlbumCardSkeleton, usePagedAlbumGrid, VirtualAlbumGrid } from "@/components/VirtualAlbumGrid";
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
  const scrollRef = useRef<HTMLDivElement>(null);
  const gridRef = useRef<HTMLDivElement>(null);

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
      scrollRef.current?.scrollTo({ top: 0 });
      setSearchParams(next, { replace: true });
    },
    [searchParams, setSearchParams],
  );

  const { total, initialLoading, errMsg, win, pagesByIndex } = usePagedAlbumGrid({
    scrollRef,
    gridRef,
    queryParams: { source, sort, groupVersions },
  });

  const hasRemote = (libraryStatus?.remoteAlbums ?? 0) > 0;

  return (
    <div className="flex h-full min-h-0 flex-col">
      <PageHeader className="py-2.5">
        <div className="mx-auto w-full max-w-7xl flex flex-wrap items-center justify-between gap-3">
          <div className="flex items-baseline gap-2.5">
            <h1 className="text-xl sm:text-2xl font-bold tracking-tight text-white">Albums</h1>
            <span className="text-xs font-mono text-muted-foreground">{fmtCount(total)}</span>
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

            <Select value={sort} onValueChange={(v) => updateParams({ sort: v as AlbumSort })}>
              <SelectTrigger aria-label="Sort albums" className="min-w-[132px]">
                <SelectValue />
              </SelectTrigger>
              <SelectContent align="end">
                <SelectItem value="artist">Artist</SelectItem>
                <SelectItem value="title">Title</SelectItem>
                <SelectItem value="year">Year</SelectItem>
                <SelectItem value="added">Recently Added</SelectItem>
                <SelectItem value="plays">Most Played</SelectItem>
              </SelectContent>
            </Select>
          </div>
        </div>
      </PageHeader>

      <PageBody scrollRef={scrollRef}>
        <div className="mx-auto max-w-7xl px-4 sm:px-8 pt-6">
          {errMsg && (
            <div role="alert" className="mb-6 rounded-xl border border-red-500/30 bg-red-950/50 px-4 py-3 text-sm text-red-200">
              {errMsg}
            </div>
          )}

          <div ref={gridRef}>
            {initialLoading && total === 0 ? (
              <div
                className="grid"
                style={{
                  gridTemplateColumns: `repeat(${win.columns}, minmax(0, 1fr))`,
                  columnGap: "20px",
                  rowGap: "28px",
                }}
              >
                {Array.from({ length: 18 }).map((_, i) => (
                  <AlbumCardSkeleton key={i} />
                ))}
              </div>
            ) : total === 0 ? (
              <p className="py-16 text-center text-sm text-muted-foreground">No matching albums in your library.</p>
            ) : (
              <VirtualAlbumGrid
                win={win}
                pagesByIndex={pagesByIndex}
                onOpenAlbum={onOpenAlbum}
                onPlay={onPlay}
              />
            )}
          </div>
        </div>
      </PageBody>
    </div>
  );
});
