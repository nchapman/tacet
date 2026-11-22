import { useQuery } from "@tanstack/react-query";
import { ArrowLeft, Flame } from "lucide-react";
import { memo } from "react";
import { Link } from "react-router";
import { tacetClient } from "@/api/client";
import type { Album, HeavyRotationAlbum, QueueSource } from "@/api/types";
import { AlbumCard } from "@/components/AlbumCard";
import { queryErrorMessage, queryKeys } from "@/lib/query";

interface HeavyRotationViewProps {
  onOpenAlbum: (album: Album) => void;
  onPlay: (source: QueueSource, startIndex?: number) => void;
}

export const HeavyRotationView = memo(function HeavyRotationView({
  onOpenAlbum,
  onPlay,
}: HeavyRotationViewProps) {
  const rotationQuery = useQuery({
    queryKey: queryKeys.library.heavyRotation(90, 48),
    queryFn: ({ signal }) => tacetClient.getHeavyRotation(90, 48, { signal }),
  });

  const albums: HeavyRotationAlbum[] = rotationQuery.data?.items ?? [];
  const errMsg = queryErrorMessage(rotationQuery.error);

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

        <div className="flex items-center gap-2.5">
          <Flame className="h-5 w-5 text-amber-400" />
          <h1 className="text-xl sm:text-2xl font-bold tracking-tight text-white">Heavy Rotation</h1>
        </div>
        <p className="text-sm text-muted-foreground">
          Albums you've been spinning most recently and frequently
        </p>
      </div>

      {errMsg && (
        <div role="alert" className="rounded-xl border border-red-500/30 bg-red-950/50 px-4 py-3 text-sm text-red-200">
          {errMsg}
        </div>
      )}

      {rotationQuery.isPending && albums.length === 0 ? (
        <div className="grid grid-cols-2 sm:grid-cols-3 md:grid-cols-4 xl:grid-cols-6 gap-x-5 gap-y-7">
          {Array.from({ length: 12 }).map((_, i) => (
            <div key={i} className="space-y-2.5 animate-pulse">
              <div className="aspect-square w-full rounded-lg bg-white/[0.06]" />
              <div className="h-3.5 w-3/4 rounded bg-white/[0.06]" />
              <div className="h-3 w-1/2 rounded bg-white/[0.04]" />
            </div>
          ))}
        </div>
      ) : albums.length === 0 ? (
        <p className="rounded-xl border border-white/[0.06] bg-white/[0.02] px-4 py-6 text-center text-sm text-muted-foreground">
          Play a few albums and your heavy rotation will appear here.
        </p>
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
  );
});
