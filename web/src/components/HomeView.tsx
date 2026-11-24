import { keepPreviousData, useQuery } from "@tanstack/react-query";
import {
  ArrowRight,
  CalendarPlus,
  Compass,
  Crown,
  Disc3,
  Flame,
  Gem,
  LibraryBig,
  Sparkles,
  TrendingUp,
  Trophy,
} from "lucide-react";
import { memo } from "react";
import { Link } from "react-router";
import { tacetClient } from "@/api/client";
import type { Album, DiscoverShelf, HeavyRotationAlbum, LibraryStatus, QueueSource } from "@/api/types";
import { AlbumCard } from "@/components/AlbumCard";
import { PageBody, PageHeader } from "@/components/ui/PageHeader";
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

const HOME_ROW_LIMIT = 6;
const RECENTLY_ADDED_WINDOW_MS = 7 * 24 * 60 * 60 * 1000;

function shelfIcon(slug: string) {
  switch (slug) {
    case "new-releases":
      return <Sparkles className="h-4 w-4 text-sky-400" />;
    case "most-streamed":
      return <TrendingUp className="h-4 w-4 text-rose-400" />;
    case "album-of-the-week":
      return <Crown className="h-4 w-4 text-[#dfc07b]" />;
    case "qobuzissims":
      return <Gem className="h-4 w-4 text-[#dfc07b]" />;
    case "press-awards":
      return <Trophy className="h-4 w-4 text-amber-400" />;
    case "ideal-discography":
      return <LibraryBig className="h-4 w-4 text-violet-400" />;
    default:
      return <Compass className="h-4 w-4 text-white/80" />;
  }
}

function isWithinRecentWindow(addedAt?: string): boolean {
  if (!addedAt) return false;
  const ts = Date.parse(addedAt);
  if (Number.isNaN(ts)) return false;
  return Date.now() - ts <= RECENTLY_ADDED_WINDOW_MS;
}

export const HomeView = memo(function HomeView({
  libraryStatus,
  discoverGenreIds,
  onOpenAlbum,
  onNavigate,
  onPlay,
}: HomeViewProps) {
  const rotationQuery = useQuery({
    queryKey: queryKeys.library.heavyRotation(90, HOME_ROW_LIMIT + 1),
    queryFn: ({ signal }) => tacetClient.getHeavyRotation(90, HOME_ROW_LIMIT + 1, { signal }),
  });
  const recentQuery = useQuery({
    queryKey: queryKeys.library.albums({ sort: "added", groupVersions: true, limit: HOME_ROW_LIMIT }),
    queryFn: ({ signal }) =>
      tacetClient.listLibraryAlbums({ sort: "added", groupVersions: true, limit: HOME_ROW_LIMIT }, { signal }),
  });
  const discoverQuery = useQuery({
    queryKey: queryKeys.discover.feed(discoverGenreIds),
    queryFn: ({ signal }) =>
      tacetClient.getDiscover(discoverGenreIds.length > 0 ? discoverGenreIds : undefined, { signal }),
    staleTime: QOBUZ_SEARCH_STALE_TIME_MS,
    gcTime: QOBUZ_SEARCH_GC_TIME_MS,
    placeholderData: keepPreviousData,
  });

  const allRotation: HeavyRotationAlbum[] = rotationQuery.data?.items ?? [];
  const rotation = allRotation.slice(0, HOME_ROW_LIMIT);
  const hasMoreRotation = allRotation.length > HOME_ROW_LIMIT;

  const recent: Album[] = recentQuery.data?.items ?? [];
  const showRecentlyAdded = recent.length > 0 && isWithinRecentWindow(recent[0]?.addedAt);

  const discover = discoverQuery.data;
  const discoverShelves: DiscoverShelf[] = discover?.shelves ?? [];
  const loading = rotationQuery.isPending || recentQuery.isPending;
  const errMsg = queryErrorMessage(rotationQuery.error ?? recentQuery.error);
  const libraryEmpty = !loading && recent.length === 0 && (libraryStatus?.albums ?? 0) === 0;

  return (
    <div className="flex h-full min-h-0 flex-col">
      <PageHeader>
        <div className="mx-auto w-full max-w-7xl">
          <h1 className="text-xl sm:text-2xl font-bold tracking-tight text-white">Home</h1>
        </div>
      </PageHeader>

      <PageBody>
        <div className="mx-auto max-w-7xl px-4 sm:px-8 pt-6 space-y-10">

        {errMsg && (
          <div role="alert" className="rounded-xl border border-red-500/30 bg-red-950/50 px-4 py-3 text-sm text-red-200">
            {errMsg}
          </div>
        )}

        {/* Heavy Rotation — single frecency-ranked row */}
        {rotation.length > 0 && (
          <section aria-label="Heavy Rotation" className="space-y-4">
            <div className="flex items-center justify-between gap-4">
              <div className="flex items-center gap-2">
                <Flame className="h-4 w-4 text-amber-400" />
                <h2 className="text-lg font-semibold tracking-tight text-white">Heavy Rotation</h2>
              </div>
              {hasMoreRotation && (
                <Link
                  to="/heavy-rotation"
                  className="inline-flex shrink-0 items-center gap-1 text-xs font-medium text-muted-foreground hover:text-foreground transition-colors"
                >
                  <span>More</span>
                  <ArrowRight className="h-3.5 w-3.5" />
                </Link>
              )}
            </div>

            <div className="grid grid-cols-2 sm:grid-cols-3 md:grid-cols-4 xl:grid-cols-6 gap-x-5 gap-y-7">
              {rotation.map((alb) => (
                <AlbumCard
                  key={alb.id}
                  album={alb}
                  onOpen={onOpenAlbum}
                  onPlay={onPlay}
                />
              ))}
            </div>
          </section>
        )}

        {/* Recently Added — single row, shown only when an album was added in the last 7 days */}
        {showRecentlyAdded && (
          <section aria-label="Recently Added" className="space-y-4">
            <div className="flex items-center justify-between gap-4">
              <div className="flex items-center gap-2">
                <CalendarPlus className="h-4 w-4 text-emerald-400" />
                <h2 className="text-lg font-semibold tracking-tight text-white">Recently Added</h2>
              </div>
              <button
                type="button"
                onClick={() => onNavigate("albums")}
                className="inline-flex shrink-0 items-center gap-1 text-xs font-medium text-muted-foreground hover:text-foreground transition-colors cursor-pointer"
              >
                <span>See all</span>
                <ArrowRight className="h-3.5 w-3.5" />
              </button>
            </div>

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
          </section>
        )}

        {/* Empty library onboarding state */}
        {libraryEmpty && !discover?.connected && (
          <div className="rounded-2xl border border-white/[0.07] bg-white/[0.02] p-8 text-center">
            <Disc3 className="mx-auto h-9 w-9 text-white/20 mb-3" />
            <h2 className="text-base font-medium text-foreground">Your library is empty</h2>
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
        )}

        {/* Unified Editorial Discovery Shelves — ordered by update cadence */}
        {discover?.connected && discoverShelves.length > 0 && (
          <div className="space-y-10">
            {discoverShelves.map((shelf) => {
              const items = shelf.items.slice(0, HOME_ROW_LIMIT);
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
                    {(shelf.hasMore || shelf.items.length > HOME_ROW_LIMIT) && (
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
      </PageBody>
    </div>
  );
});
