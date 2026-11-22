import { keepPreviousData, useQuery } from "@tanstack/react-query";
import { ArrowRight, Award, Compass, Disc3, Flame, ListMusic, Sparkles, Star, TrendingUp, Users } from "lucide-react";
import { memo } from "react";
import { Link } from "react-router";
import { tacetClient } from "@/api/client";
import type { Album, DiscoverShelf, HeavyRotationAlbum, LibraryStatus, QueueSource } from "@/api/types";
import { AlbumCard } from "@/components/AlbumCard";
import type { AppSection } from "@/components/Sidebar";
import { Button } from "@/components/ui/button";
import { QOBUZ_SEARCH_GC_TIME_MS, QOBUZ_SEARCH_STALE_TIME_MS, queryErrorMessage, queryKeys } from "@/lib/query";
import { discoverShelfPath } from "@/routes";

interface HomeViewProps {
  libraryStatus: LibraryStatus | null;
  discoverGenreIds: number[];
  onOpenAlbum: (album: Album) => void;
  onNavigate: (section: AppSection) => void;
  onPlay: (source: QueueSource, startIndex?: number) => void;
}

const SHELF_LIMITS: Record<string, number> = {
  "new-releases": 12,
};

function shelfIcon(slug: string) {
  switch (slug) {
    case "qobuzissims":
      return <Star className="h-4 w-4 text-[#dfc07b]" />;
    case "press-awards":
      return <Award className="h-4 w-4 text-[#dfc07b]" />;
    case "most-streamed":
      return <TrendingUp className="h-4 w-4 text-sky-400" />;
    case "album-of-the-week":
      return <Sparkles className="h-4 w-4 text-amber-300" />;
    default:
      return <Compass className="h-4 w-4 text-zinc-300" />;
  }
}

export const HomeView = memo(function HomeView({
  libraryStatus,
  discoverGenreIds,
  onOpenAlbum,
  onNavigate,
  onPlay,
}: HomeViewProps) {
  const rotationQuery = useQuery({
    queryKey: queryKeys.library.heavyRotation(30, 12),
    queryFn: ({ signal }) => tacetClient.getHeavyRotation(30, 12, { signal }),
  });
  const recentQuery = useQuery({
    queryKey: queryKeys.library.albums({ sort: "added", groupVersions: true, limit: 12 }),
    queryFn: ({ signal }) => tacetClient.listLibraryAlbums({ sort: "added", groupVersions: true, limit: 12 }, { signal }),
  });
  const discoverQuery = useQuery({
    queryKey: queryKeys.discover.feed(discoverGenreIds),
    queryFn: ({ signal }) =>
      tacetClient.getDiscover(discoverGenreIds.length > 0 ? discoverGenreIds : undefined, { signal }),
    staleTime: QOBUZ_SEARCH_STALE_TIME_MS,
    gcTime: QOBUZ_SEARCH_GC_TIME_MS,
    placeholderData: keepPreviousData,
  });

  const rotation: HeavyRotationAlbum[] = rotationQuery.data?.items ?? [];
  const recent: Album[] = recentQuery.data?.items ?? [];
  const discover = discoverQuery.data;
  const discoverShelves: DiscoverShelf[] = discover?.shelves ?? [];
  const loading = rotationQuery.isPending || recentQuery.isPending;
  const errMsg = queryErrorMessage(rotationQuery.error ?? recentQuery.error);

  return (
    <div className="relative min-h-full pb-12">
      <div className="relative z-10 mx-auto max-w-7xl px-4 sm:px-8 pt-6 sm:pt-8 space-y-10">
        {/* Top welcome & quick jump bar */}
        <div className="flex flex-col sm:flex-row sm:items-end justify-between gap-4">
          <div>
            <h1 className="text-2xl sm:text-3xl font-bold tracking-tight text-white">Home</h1>
            {libraryStatus && libraryStatus.albums > 0 && (
              <p className="mt-1 text-sm text-muted-foreground">
                {libraryStatus.albums} albums · {libraryStatus.tracks} tracks ready for bit-perfect playback
              </p>
            )}
          </div>

          <div className="flex flex-wrap items-center gap-2">
            <Button
              type="button"
              variant="secondary"
              size="sm"
              className="rounded-full"
              onClick={() => onNavigate("albums")}
            >
              <Disc3 className="h-3.5 w-3.5" />
              All Albums
            </Button>
            <Button
              type="button"
              variant="secondary"
              size="sm"
              className="rounded-full"
              onClick={() => onNavigate("artists")}
            >
              <Users className="h-3.5 w-3.5" />
              Artists
            </Button>
            <Button
              type="button"
              variant="secondary"
              size="sm"
              className="rounded-full"
              onClick={() => onNavigate("songs")}
            >
              <ListMusic className="h-3.5 w-3.5" />
              Songs
            </Button>
          </div>
        </div>

        {errMsg && (
          <div role="alert" className="rounded-xl border border-red-500/30 bg-red-950/50 px-4 py-3 text-sm text-red-200">
            {errMsg}
          </div>
        )}

        {/* Heavy Rotation Shelf */}
        {rotation.length > 0 && (
          <section aria-label="Heavy Rotation" className="space-y-4">
            <div className="flex items-center justify-between">
              <div className="flex items-center gap-2">
                <Flame className="h-4 w-4 text-amber-400" />
                <h2 className="text-lg font-semibold tracking-tight text-white">Heavy Rotation</h2>
                <span className="text-xs text-muted-foreground">Last 30 days</span>
              </div>
            </div>

            <div className="grid grid-cols-2 sm:grid-cols-3 md:grid-cols-4 xl:grid-cols-6 gap-x-5 gap-y-7">
              {rotation.map((alb) => (
                <AlbumCard
                  key={alb.id}
                  album={alb}
                  subtitle={`${alb.artist} · ${alb.recentPlays} ${alb.recentPlays === 1 ? "play" : "plays"}`}
                  onOpen={onOpenAlbum}
                  onPlay={onPlay}
                />
              ))}
            </div>
          </section>
        )}

        {/* Recently Added Shelf */}
        <section aria-label="Recently Added" className="space-y-4">
          <div className="flex items-center justify-between">
            <div className="flex items-center gap-2">
              <Sparkles className="h-4 w-4 text-emerald-400" />
              <h2 className="text-lg font-semibold tracking-tight text-white">Recently Added</h2>
            </div>
            <button
              type="button"
              onClick={() => onNavigate("albums")}
              className="inline-flex items-center gap-1 text-xs font-medium text-muted-foreground hover:text-foreground transition-colors cursor-pointer"
            >
              <span>See all</span>
              <ArrowRight className="h-3.5 w-3.5" />
            </button>
          </div>

          {loading && recent.length === 0 ? (
            <div className="grid grid-cols-2 sm:grid-cols-3 md:grid-cols-4 xl:grid-cols-6 gap-5">
              {Array.from({ length: 12 }).map((_, i) => (
                <div key={i} className="space-y-2.5 animate-pulse">
                  <div className="aspect-square w-full rounded-lg bg-white/[0.06]" />
                  <div className="h-3.5 w-3/4 rounded bg-white/[0.06]" />
                  <div className="h-3 w-1/2 rounded bg-white/[0.04]" />
                </div>
              ))}
            </div>
          ) : recent.length === 0 ? (
            <div className="rounded-2xl border border-white/[0.07] bg-white/[0.02] p-8 text-center">
              <Disc3 className="mx-auto h-9 w-9 text-zinc-700 mb-3" />
              <h3 className="text-base font-medium text-foreground">Your library is empty</h3>
              <p className="mt-1 text-sm text-muted-foreground max-w-md mx-auto">
                Point <code className="rounded bg-white/10 px-1.5 py-0.5 font-mono text-xs">--music-dir</code> at your NAS library or connect your Qobuz account in Settings.
              </p>
              <Button
                type="button"
                variant="secondary"
                className="mt-4 rounded-full"
                onClick={() => onNavigate("settings")}
              >
                Open Settings
              </Button>
            </div>
          ) : (
            <div className="grid grid-cols-2 sm:grid-cols-3 md:grid-cols-4 xl:grid-cols-6 gap-x-5 gap-y-7">
              {recent.map((alb) => (
                <AlbumCard
                  key={alb.id}
                  album={alb}
                  onOpen={onOpenAlbum}
                  onPlay={onPlay}
                />
              ))}
            </div>
          )}
        </section>

        {/* Unified Editorial Discovery Shelves */}
        {discover?.connected && discoverShelves.length > 0 && (
          <div className="space-y-10 pt-2 border-t border-white/[0.07]">
            {discoverShelves.map((shelf) => {
              const maxItems = SHELF_LIMITS[shelf.slug] ?? 6;
              const items = shelf.items.slice(0, maxItems);
              if (items.length === 0) return null;
              return (
                <section key={shelf.slug} aria-label={shelf.title} className="space-y-4">
                  <div className="flex items-end justify-between gap-4">
                    <div className="min-w-0">
                      <div className="flex items-center gap-2">
                        {shelfIcon(shelf.slug)}
                        <h2 className="text-lg font-semibold tracking-tight text-white">{shelf.title}</h2>
                      </div>
                      {shelf.subtitle && (
                        <p className="mt-0.5 text-xs text-muted-foreground truncate">{shelf.subtitle}</p>
                      )}
                    </div>
                    {(shelf.hasMore || shelf.items.length > maxItems) && (
                      <Link
                        to={discoverShelfPath(shelf.slug)}
                        className="inline-flex shrink-0 items-center gap-1 text-xs font-medium text-muted-foreground hover:text-foreground transition-colors"
                      >
                        <span>See all</span>
                        <ArrowRight className="h-3.5 w-3.5" />
                      </Link>
                    )}
                  </div>

                  <div className="grid grid-cols-2 sm:grid-cols-3 md:grid-cols-4 xl:grid-cols-6 gap-x-5 gap-y-7">
                    {items.map((alb) => (
                      <AlbumCard
                        key={alb.id}
                        album={alb}
                        showInLibraryBadge
                        onOpen={onOpenAlbum}
                        onPlay={onPlay}
                      />
                    ))}
                  </div>
                </section>
              );
            })}
          </div>
        )}
      </div>
    </div>
  );
});
