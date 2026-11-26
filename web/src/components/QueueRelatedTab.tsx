import { useQuery } from "@tanstack/react-query";
import { Check, Loader2, Plus, Radio, SkipForward } from "lucide-react";
import { useCallback, useMemo, useState } from "react";
import { tacetClient } from "@/api/client";
import type { QueuePosition, QueueSource, Track } from "@/api/types";
import { AlbumArt } from "@/components/ui/artwork";
import { useArtistHoverPrefetch } from "@/lib/prefetch";
import { queryKeys } from "@/lib/query";

const SONIC_LIMIT = 5;
const RELATED_LIMIT = 20;
const RELATED_STALE_MS = 5 * 60 * 1000;

interface QueueRelatedTabProps {
  currentTrack?: Track;
  onAdd?: (source: QueueSource, position?: QueuePosition) => void;
  onOpenArtist?: (artist: string) => void;
}

export function QueueRelatedTab({ currentTrack, onAdd, onOpenArtist }: QueueRelatedTabProps) {
  const trackId = currentTrack?.id ?? "";
  const [recentlyAdded, setRecentlyAdded] = useState<Set<string>>(() => new Set());

  const sonicQuery = useQuery({
    queryKey: queryKeys.sonic.similarTracks(trackId, SONIC_LIMIT),
    queryFn: ({ signal }) => tacetClient.getSonicSimilarTracks(trackId, SONIC_LIMIT, { signal }),
    enabled: Boolean(trackId),
    staleTime: RELATED_STALE_MS,
  });

  const relatedQuery = useQuery({
    queryKey: queryKeys.radio.preview("track", trackId, 25),
    queryFn: ({ signal }) =>
      tacetClient.previewRadio({ kind: "track", id: trackId, limit: 25 }, { signal }),
    enabled: Boolean(trackId),
    staleTime: RELATED_STALE_MS,
  });

  const sonicItems = sonicQuery.data?.items ?? [];

  const relatedTracks = useMemo(() => {
    const exclude = new Set<string>([trackId, ...sonicItems.map((it) => it.track.id)]);
    return (relatedQuery.data?.tracks ?? [])
      .filter((tr) => tr.id && !exclude.has(tr.id))
      .slice(0, RELATED_LIMIT);
  }, [trackId, sonicItems, relatedQuery.data]);

  const handleQueue = useCallback(
    (id: string, position: QueuePosition) => {
      if (!onAdd) return;
      onAdd({ type: "tracks", trackIds: [id] }, position);
      setRecentlyAdded((prev) => new Set(prev).add(id));
      window.setTimeout(() => {
        setRecentlyAdded((prev) => {
          const next = new Set(prev);
          next.delete(id);
          return next;
        });
      }, 1400);
    },
    [onAdd],
  );

  if (!trackId) {
    return (
      <div className="flex flex-col items-center justify-center py-16 text-center">
        <Radio className="h-8 w-8 text-white/20 mb-3" />
        <p className="text-sm font-medium text-foreground/80">Nothing playing</p>
        <p className="mt-1 text-xs text-muted-foreground max-w-[210px]">
          Play a track to see sonically similar and related recommendations.
        </p>
      </div>
    );
  }

  const initialLoading = sonicQuery.isLoading && relatedQuery.isLoading;
  const nothingFound = !initialLoading && sonicItems.length === 0 && relatedTracks.length === 0;

  if (initialLoading) {
    return (
      <div className="flex items-center justify-center gap-2 py-16 text-xs text-muted-foreground">
        <Loader2 className="h-4 w-4 animate-spin" />
        Finding related tracks…
      </div>
    );
  }

  if (nothingFound) {
    return (
      <div className="flex flex-col items-center justify-center py-16 text-center">
        <Radio className="h-8 w-8 text-white/20 mb-3" />
        <p className="text-sm font-medium text-foreground/80">No related tracks found</p>
        <p className="mt-1 text-xs text-muted-foreground max-w-[210px]">
          Try another track or enable Sonic Analysis for deeper local matches.
        </p>
      </div>
    );
  }

  return (
    <div className="space-y-5">
      {sonicItems.length > 0 && (
        <section aria-label="Sonically similar tracks">
          <div className="px-2 pb-2 text-[11px] font-semibold uppercase tracking-wider text-muted-foreground">
            Sonically Similar Tracks
          </div>
          <div className="space-y-0.5">
            {sonicItems.map(({ track, score }) => (
              <RelatedTrackRow
                key={track.id}
                track={track}
                matchPct={Math.max(1, Math.min(99, Math.round(score * 100)))}
                added={recentlyAdded.has(track.id)}
                onQueue={onAdd ? handleQueue : undefined}
                onOpenArtist={onOpenArtist}
              />
            ))}
          </div>
        </section>
      )}

      {(relatedTracks.length > 0 || relatedQuery.isLoading) && (
        <section aria-label="Related tracks">
          <div className="px-2 pb-2 text-[11px] font-semibold uppercase tracking-wider text-muted-foreground">
            Related Tracks
          </div>
          {relatedQuery.isLoading && relatedTracks.length === 0 ? (
            <div className="flex items-center gap-2 px-2 py-4 text-xs text-muted-foreground">
              <Loader2 className="h-3.5 w-3.5 animate-spin" />
              Loading related tracks…
            </div>
          ) : (
            <div className="space-y-0.5">
              {relatedTracks.map((track) => (
                <RelatedTrackRow
                  key={track.id}
                  track={track}
                  added={recentlyAdded.has(track.id)}
                  onQueue={onAdd ? handleQueue : undefined}
                  onOpenArtist={onOpenArtist}
                />
              ))}
            </div>
          )}
        </section>
      )}
    </div>
  );
}

function RelatedTrackRow({
  track,
  matchPct,
  added,
  onQueue,
  onOpenArtist,
}: {
  track: Track;
  matchPct?: number;
  added: boolean;
  onQueue?: (trackId: string, position: QueuePosition) => void;
  onOpenArtist?: (artist: string) => void;
}) {
  const artistPrefetch = useArtistHoverPrefetch(track.artist);

  return (
    <div className="group flex items-center gap-2 rounded-lg px-1.5 py-1.5 hover:bg-white/[0.06] transition-colors">
      {onQueue && (
        <button
          type="button"
          title={added ? "Added to Queue" : "Add to Queue"}
          aria-label={`Add ${track.title} to queue`}
          onClick={() => onQueue(track.id, "end")}
          className="flex h-7 w-7 shrink-0 items-center justify-center rounded-full text-muted-foreground hover:bg-white/10 hover:text-foreground transition-colors cursor-pointer"
        >
          {added ? <Check className="h-4 w-4 text-emerald-400" /> : <Plus className="h-4 w-4" />}
        </button>
      )}

      <AlbumArt
        artworkUrl={track.artworkUrl}
        alt={track.title}
        className="h-9 w-9 shrink-0 rounded-md"
        iconClassName="h-4 w-4"
      />

      <div className="min-w-0 flex-1">
        <div className="truncate text-[13px] font-medium leading-tight text-foreground">{track.title}</div>
        <div className="truncate text-[11px] text-muted-foreground mt-0.5">
          {onOpenArtist && track.artist ? (
            <button
              type="button"
              onClick={() => onOpenArtist(track.artist)}
              {...artistPrefetch}
              className="hover:underline hover:text-foreground cursor-pointer"
            >
              {track.artist}
            </button>
          ) : (
            track.artist
          )}
          {matchPct !== undefined && (
            <span className="font-mono text-muted-foreground/80"> — {matchPct}%</span>
          )}
        </div>
      </div>

      {onQueue && (
        <button
          type="button"
          title="Play Next"
          aria-label={`Play ${track.title} next`}
          onClick={() => onQueue(track.id, "next")}
          className="rounded-full p-1 text-muted-foreground opacity-0 group-hover:opacity-100 hover:bg-white/10 hover:text-foreground transition-opacity cursor-pointer"
        >
          <SkipForward className="h-3.5 w-3.5" />
        </button>
      )}
    </div>
  );
}
