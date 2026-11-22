import { useEffect, useState } from "react";
import { tacetClient } from "@/api/client";
import type { Album, LibraryStatus, QueueSource } from "@/api/types";
import { AlbumCard } from "@/components/AlbumCard";
import { Switch } from "@/components/ui/switch";
import { cn } from "@/lib/utils";

type SourceFilter = "all" | "local" | "remote";
type AlbumSort = "artist" | "title" | "year" | "added" | "plays";

interface AlbumsViewProps {
  libraryStatus: LibraryStatus | null;
  onOpenAlbum: (album: Album) => void;
  onPlay: (source: QueueSource, startIndex?: number) => void;
}

export function AlbumsView({ libraryStatus, onOpenAlbum, onPlay }: AlbumsViewProps) {
  const [source, setSource] = useState<SourceFilter>("all");
  const [sort, setSort] = useState<AlbumSort>("artist");
  const [groupVersions, setGroupVersions] = useState(true);
  const [albums, setAlbums] = useState<Album[]>([]);
  const [total, setTotal] = useState(0);
  const [loading, setLoading] = useState(true);
  const [errMsg, setErrMsg] = useState("");

  useEffect(() => {
    const ctrl = new AbortController();
    setLoading(true);
    setErrMsg("");
    void tacetClient
      .listLibraryAlbums(
        {
          source,
          sort,
          groupVersions,
          limit: 500,
        },
        { signal: ctrl.signal },
      )
      .then((res) => {
        if (ctrl.signal.aborted) return;
        setAlbums(res.items ?? []);
        setTotal(res.total ?? 0);
      })
      .catch((err) => {
        if (!ctrl.signal.aborted) setErrMsg(err instanceof Error ? err.message : String(err));
      })
      .finally(() => {
        if (!ctrl.signal.aborted) setLoading(false);
      });
    return () => ctrl.abort();
  }, [source, sort, groupVersions, libraryStatus?.etag]);

  const hasRemote = (libraryStatus?.remoteAlbums ?? 0) > 0;

  return (
    <div className="min-h-full pb-12">
      {/* Sticky sleek filter header */}
      <div className="sticky top-0 z-20 border-b border-white/[0.06] bg-[#09090b]/85 px-4 sm:px-8 py-4 backdrop-blur-xl">
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
                    onClick={() => setSource(f)}
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
              onChange={(e) => setSort(e.target.value as AlbumSort)}
              className="h-8.5 rounded-lg border border-white/[0.09] bg-white/[0.05] px-2.5 text-xs text-foreground focus:outline-none focus:border-white/25 cursor-pointer"
            >
              <option value="artist">Artist</option>
              <option value="title">Title</option>
              <option value="year">Year</option>
              <option value="added">Recently Added</option>
              <option value="plays">Most Played</option>
            </select>

            <label className="inline-flex items-center gap-2 text-xs text-muted-foreground cursor-pointer select-none">
              <Switch checked={groupVersions} onCheckedChange={setGroupVersions} />
              <span>Group versions</span>
            </label>
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
                showSourceBadge={hasRemote}
                onOpen={onOpenAlbum}
                onPlay={onPlay}
              />
            ))}
          </div>
        )}
      </div>
    </div>
  );
}
