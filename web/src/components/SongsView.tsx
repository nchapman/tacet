import { keepPreviousData, useQuery } from "@tanstack/react-query";
import { ListPlus, Play, SkipForward } from "lucide-react";
import { useCallback } from "react";
import { useSearchParams } from "react-router";
import { tacetClient } from "@/api/client";
import { fmtDuration, fmtQuality, isHiRes } from "@/api/format";
import type { GenreSummary, QueuePosition, QueueSource, Track } from "@/api/types";
import { AlbumArt } from "@/components/ui/artwork";
import { queryErrorMessage, queryKeys } from "@/lib/query";
import { cn } from "@/lib/utils";

type TrackSort = "artist" | "title" | "album" | "added" | "plays" | "quality";

const VALID_TRACK_SORTS = new Set<TrackSort>(["artist", "title", "album", "added", "plays", "quality"]);

interface SongsViewProps {
  currentTrackId?: string;
  onOpenAlbumId: (albumId: string) => void;
  onOpenArtist: (artist: string) => void;
  onPlay: (source: QueueSource, startIndex?: number) => void;
  onAdd: (source: QueueSource, position?: QueuePosition) => void;
}

/**
 * Sleek Spotify-proportioned Songs list (artwork thumbs + stacked title/artist)
 * rather than a dense spreadsheet table.
 */
export function SongsView({
  currentTrackId,
  onOpenAlbumId,
  onOpenArtist,
  onPlay,
  onAdd,
}: SongsViewProps) {
  const [searchParams, setSearchParams] = useSearchParams();
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
      setSearchParams(next, { replace: true });
    },
    [searchParams, setSearchParams],
  );

  const dir = sort === "plays" || sort === "added" || sort === "quality" ? "desc" : "asc";

  const genresQuery = useQuery({
    queryKey: queryKeys.library.genres,
    queryFn: ({ signal }) => tacetClient.listLibraryGenres({}, { signal }),
  });
  const tracksQuery = useQuery({
    queryKey: queryKeys.library.tracks({ genre: genre || undefined, sort, dir, limit: 300 }),
    queryFn: ({ signal }) =>
      tacetClient.listLibraryTracks(
        {
          genre: genre || undefined,
          sort,
          dir,
          limit: 300,
        },
        { signal },
      ),
    placeholderData: keepPreviousData,
  });

  const genres: GenreSummary[] = genresQuery.data?.items ?? [];
  const tracks: Track[] = tracksQuery.data?.items ?? [];
  const total = tracksQuery.data?.total ?? 0;
  const loading = tracksQuery.isPending;
  const errMsg = queryErrorMessage(tracksQuery.error);

  return (
    <div className="min-h-full pb-12">
      <div className="sticky top-0 z-20 border-b border-white/[0.06] bg-[#09090b]/45 px-4 sm:px-8 py-4 backdrop-blur-2xl">
        <div className="mx-auto max-w-6xl flex flex-wrap items-center justify-between gap-3">
          <div className="flex items-baseline gap-2.5">
            <h1 className="text-xl sm:text-2xl font-bold tracking-tight text-white">Songs</h1>
            <span className="text-xs font-mono text-muted-foreground">{total}</span>
          </div>

          <div className="flex flex-wrap items-center gap-2.5">
            {genres.length > 0 && (
              <select
                aria-label="Filter by genre"
                value={genre}
                onChange={(e) => updateParams({ genre: e.target.value })}
                className="h-8.5 max-w-[180px] rounded-lg border border-white/[0.09] bg-white/[0.05] px-2.5 text-xs text-foreground focus:outline-none focus:border-white/25 cursor-pointer"
              >
                <option value="">All genres</option>
                {genres.map((g) => (
                  <option key={g.name} value={g.name}>
                    {g.name} ({g.trackCount})
                  </option>
                ))}
              </select>
            )}

            <select
              aria-label="Sort songs"
              value={sort}
              onChange={(e) => updateParams({ sort: e.target.value as TrackSort })}
              className="h-8.5 rounded-lg border border-white/[0.09] bg-white/[0.05] px-2.5 text-xs text-foreground focus:outline-none focus:border-white/25 cursor-pointer"
            >
              <option value="artist">Artist</option>
              <option value="title">Title</option>
              <option value="album">Album</option>
              <option value="added">Recently Added</option>
              <option value="plays">Most Played</option>
              <option value="quality">Highest Quality</option>
            </select>
          </div>
        </div>
      </div>

      <div className="mx-auto max-w-6xl px-3 sm:px-8 pt-3">
        {errMsg && (
          <div role="alert" className="my-4 rounded-xl border border-red-500/30 bg-red-950/50 px-4 py-3 text-sm text-red-200">
            {errMsg}
          </div>
        )}

        {loading && tracks.length === 0 ? (
          <div className="space-y-2 py-4">
            {Array.from({ length: 12 }).map((_, i) => (
              <div key={i} className="flex items-center gap-3 px-2 py-2 animate-pulse">
                <div className="h-10 w-10 rounded-md bg-white/[0.06]" />
                <div className="space-y-1.5 flex-1">
                  <div className="h-3.5 w-1/3 rounded bg-white/[0.06]" />
                  <div className="h-3 w-1/5 rounded bg-white/[0.04]" />
                </div>
              </div>
            ))}
          </div>
        ) : tracks.length === 0 ? (
          <p className="py-16 text-center text-sm text-muted-foreground">No matching songs.</p>
        ) : (
          <div className="divide-y divide-white/[0.03]">
            {tracks.map((tr) => {
              const active = tr.id === currentTrackId;
              const qual = fmtQuality(tr);
              const hiRes = isHiRes(tr.bitDepth, tr.sampleRate);

              return (
                <div
                  key={tr.id}
                  className={cn(
                    "group grid grid-cols-[1fr_auto] md:grid-cols-[minmax(0,2fr)_minmax(0,1.2fr)_auto_60px_68px] items-center gap-3 rounded-xl px-2.5 py-2 transition-colors",
                    active ? "bg-white/[0.08]" : "hover:bg-white/[0.05]",
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
                      <AlbumArt artworkUrl={tr.artworkUrl} alt={tr.album || tr.title} className="h-full w-full" />
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
                  <div className="hidden md:flex items-center gap-1.5">
                    {qual && (
                      <>
                        <span className="font-mono text-[11px] text-muted-foreground/75">{qual}</span>
                        {hiRes && <span className="h-1.5 w-1.5 shrink-0 rounded-full bg-[#d4af68]/75" title="Hi-Res Audio" />}
                      </>
                    )}
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
            })}
          </div>
        )}

        {total > tracks.length && (
          <p className="mt-6 text-center text-xs text-muted-foreground">
            Showing {tracks.length} of {total} tracks — refine with search or genre filter.
          </p>
        )}
      </div>
    </div>
  );
}
