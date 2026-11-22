import { useQuery } from "@tanstack/react-query";
import { ArrowRight, Compass, Disc3, Flame, ListMusic, Sparkles, Users } from "lucide-react";
import { tacetClient } from "@/api/client";
import type { Album, HeavyRotationAlbum, LibraryStatus, QueueSource } from "@/api/types";
import { AlbumCard } from "@/components/AlbumCard";
import type { AppSection } from "@/components/Sidebar";
import { Button } from "@/components/ui/button";
import { queryErrorMessage, queryKeys } from "@/lib/query";

interface HomeViewProps {
  libraryStatus: LibraryStatus | null;
  onOpenAlbum: (album: Album) => void;
  onNavigate: (section: AppSection) => void;
  onPlay: (source: QueueSource, startIndex?: number) => void;
}

export function HomeView({ libraryStatus, onOpenAlbum, onNavigate, onPlay }: HomeViewProps) {
  const rotationQuery = useQuery({
    queryKey: queryKeys.library.heavyRotation(30, 12),
    queryFn: ({ signal }) => tacetClient.getHeavyRotation(30, 12, { signal }),
  });
  const recentQuery = useQuery({
    queryKey: queryKeys.library.albums({ sort: "added", groupVersions: true, limit: 18 }),
    queryFn: ({ signal }) => tacetClient.listLibraryAlbums({ sort: "added", groupVersions: true, limit: 18 }, { signal }),
  });

  const rotation: HeavyRotationAlbum[] = rotationQuery.data?.items ?? [];
  const recent: Album[] = recentQuery.data?.items ?? [];
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
            <Button
              type="button"
              variant="secondary"
              size="sm"
              className="rounded-full"
              onClick={() => onNavigate("qobuz")}
            >
              <Compass className="h-3.5 w-3.5" />
              Explore Qobuz
            </Button>
          </div>
        </div>

        {errMsg && (
          <div role="alert" className="rounded-xl border border-red-500/30 bg-red-950/50 px-4 py-3 text-sm text-red-200">
            {errMsg}
          </div>
        )}

        {/* Heavy Rotation Shelf (Rdio / Plexamp style) */}
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
                  showSourceBadge
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
                  showSourceBadge
                  onOpen={onOpenAlbum}
                  onPlay={onPlay}
                />
              ))}
            </div>
          )}
        </section>
      </div>
    </div>
  );
}
