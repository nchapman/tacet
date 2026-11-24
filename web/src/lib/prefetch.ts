import { useEffect, useMemo } from "react";
import { TacetAPIError, tacetClient } from "@/api/client";
import type { Album, QobuzArtistDetail } from "@/api/types";
import { createHoverIntent, type HoverIntentHandlers } from "@/lib/hoverIntent";
import {
  DEFAULT_GC_TIME_MS,
  QOBUZ_ALBUM_GC_TIME_MS,
  QOBUZ_ALBUM_STALE_TIME_MS,
  QOBUZ_ARTIST_GC_TIME_MS,
  QOBUZ_ARTIST_STALE_TIME_MS,
  queryClient,
  queryKeys,
} from "@/lib/query";

let qobuzConnectedFlag = false;

/** Keeps speculative hover-intent prefetches aligned with the current Qobuz auth state. */
export function setPrefetchQobuzConnected(connected: boolean): void {
  qobuzConnectedFlag = connected;
}

function isSaveDataEnabled(): boolean {
  if (typeof navigator === "undefined") return false;
  const conn = (navigator as Navigator & { connection?: { saveData?: boolean } }).connection;
  return Boolean(conn?.saveData);
}

export interface AlbumDetailQueryConfig {
  qobuzConnected: boolean;
  hint?: Pick<Album, "id" | "source" | "inLibrary">;
}

/**
 * Shared query options for album detail loading so AlbumRoute and hoverIntent
 * prefetches always hit the exact same cache key, fetch path, and stale window.
 */
export function albumDetailQueryOptions(albumId: string, { qobuzConnected, hint }: AlbumDetailQueryConfig) {
  const isOutOfLibraryQobuz = Boolean(hint && hint.source === "qobuz" && hint.inLibrary === false);
  const isQobuzAlbum = albumId.startsWith("qobuz:");

  return {
    queryKey: queryKeys.album(albumId),
    queryFn: ({ signal }: { signal: AbortSignal }): Promise<Album> => {
      if (!albumId) return Promise.reject(new Error("Missing album id"));
      // Always fetch Qobuz albums via qobuzAlbum when connected (served from Go catalogCache
      // with live SQLite inLibrary/versions/playCount overlay) so partial 1-track SQLite albums
      // never hide the rest of the album's tracklist.
      if (isOutOfLibraryQobuz || (isQobuzAlbum && qobuzConnected)) {
        return tacetClient.qobuzAlbum(albumId, { signal }).catch((err) => {
          if (err instanceof TacetAPIError && err.status === 404 && !isOutOfLibraryQobuz) {
            return tacetClient.getLibraryAlbum(albumId, { signal });
          }
          throw err;
        });
      }
      return tacetClient.getLibraryAlbum(albumId, { signal }).catch((err) => {
        if (err instanceof TacetAPIError && err.status === 404 && isQobuzAlbum) {
          return tacetClient.qobuzAlbum(albumId, { signal });
        }
        throw err;
      });
    },
    staleTime: isQobuzAlbum ? QOBUZ_ALBUM_STALE_TIME_MS : 60_000,
    gcTime: isQobuzAlbum ? QOBUZ_ALBUM_GC_TIME_MS : DEFAULT_GC_TIME_MS,
  };
}

export function artistAlbumsQueryOptions(artistName: string) {
  return {
    queryKey: queryKeys.library.albums({
      artist: artistName.toLowerCase(),
      groupVersions: true,
      sort: "year",
      limit: 200,
    }),
    queryFn: ({ signal }: { signal: AbortSignal }) =>
      tacetClient.listLibraryAlbums(
        {
          artist: artistName,
          groupVersions: true,
          sort: "year",
          limit: 200,
        },
        { signal },
      ),
  };
}

export function qobuzArtistQueryOptions(target: { id: number } | { name: string }) {
  return {
    queryKey: queryKeys.qobuz.artist(target),
    queryFn: async ({ signal }: { signal: AbortSignal }): Promise<QobuzArtistDetail | null> => {
      try {
        return await tacetClient.qobuzArtist(
          "id" in target ? { id: target.id, limit: 50 } : { name: target.name, limit: 50 },
          { signal },
        );
      } catch (err) {
        // 404 is expected for local-only artists not on Qobuz; cache null to avoid repeat 404s.
        if (err instanceof TacetAPIError && err.status === 404) return null;
        throw err;
      }
    },
    staleTime: QOBUZ_ARTIST_STALE_TIME_MS,
    gcTime: QOBUZ_ARTIST_GC_TIME_MS,
  };
}

export function prefetchAlbum(target: Pick<Album, "id" | "source" | "inLibrary"> | string): void {
  if (isSaveDataEnabled()) return;
  const albumId = typeof target === "string" ? target : target.id;
  if (!albumId) return;
  const hint = typeof target === "string" ? undefined : target;
  void queryClient.prefetchQuery(
    albumDetailQueryOptions(albumId, { qobuzConnected: qobuzConnectedFlag, hint }),
  );
}

export function prefetchArtist(name: string, qobuzArtistId?: number): void {
  if (isSaveDataEnabled()) return;
  const trimmed = name.trim();
  if (!trimmed) return;
  void queryClient.prefetchQuery(artistAlbumsQueryOptions(trimmed));
  if (qobuzConnectedFlag) {
    void queryClient.prefetchQuery(
      qobuzArtistQueryOptions(qobuzArtistId ? { id: qobuzArtistId } : { name: trimmed }),
    );
  }
}

const NOOP_HANDLERS: HoverIntentHandlers = {
  onMouseEnter: () => {},
  onMouseMove: () => {},
  onMouseLeave: () => {},
  onFocus: () => {},
  onBlur: () => {},
};

export function useAlbumHoverPrefetch(
  target?: Pick<Album, "id" | "source" | "inLibrary"> | string | null,
): HoverIntentHandlers {
  const id = typeof target === "string" ? target : target?.id;
  const source = typeof target === "string" ? undefined : target?.source;
  const inLibrary = typeof target === "string" ? undefined : target?.inLibrary;

  const handlers = useMemo(() => {
    if (!id) return NOOP_HANDLERS;
    return createHoverIntent(() =>
      prefetchAlbum(source !== undefined ? { id, source, inLibrary } : id),
    );
  }, [id, source, inLibrary]);

  useEffect(() => () => handlers.onMouseLeave(), [handlers]);

  return handlers;
}

export function useArtistHoverPrefetch(name?: string | null, qobuzArtistId?: number): HoverIntentHandlers {
  const trimmed = name?.trim() ?? "";
  const handlers = useMemo(() => {
    if (!trimmed) return NOOP_HANDLERS;
    return createHoverIntent(() => prefetchArtist(trimmed, qobuzArtistId));
  }, [trimmed, qobuzArtistId]);

  useEffect(() => () => handlers.onMouseLeave(), [handlers]);

  return handlers;
}
