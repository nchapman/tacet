import { QueryClient } from "@tanstack/react-query";
import { useEffect, useState } from "react";
import { TacetAPIError } from "@/api/client";

export const DEFAULT_STALE_TIME_MS = 30_000;
export const DEFAULT_GC_TIME_MS = 10 * 60_000;
export const QOBUZ_ARTIST_STALE_TIME_MS = 60 * 60_000;
export const QOBUZ_ARTIST_GC_TIME_MS = 4 * 60 * 60_000;
export const QOBUZ_ALBUM_STALE_TIME_MS = 60 * 60_000;
export const QOBUZ_ALBUM_GC_TIME_MS = 4 * 60 * 60_000;
export const QOBUZ_SEARCH_STALE_TIME_MS = 15 * 60_000;
export const QOBUZ_SEARCH_GC_TIME_MS = 60 * 60_000;

export function shouldRetryQuery(failureCount: number, error: unknown): boolean {
  if (error instanceof TacetAPIError) {
    return error.retryable && failureCount < 1;
  }
  return failureCount < 1;
}

export function createQueryClient(): QueryClient {
  return new QueryClient({
    defaultOptions: {
      queries: {
        staleTime: DEFAULT_STALE_TIME_MS,
        gcTime: DEFAULT_GC_TIME_MS,
        retry: shouldRetryQuery,
        refetchOnWindowFocus: false,
      },
    },
  });
}

export const queryClient = createQueryClient();

export const queryKeys = {
  library: {
    all: ["library"] as const,
    heavyRotation: (days: number, limit: number) => ["library", "heavy-rotation", { days, limit }] as const,
    albums: (params: {
      source?: string;
      provider?: string;
      artist?: string;
      sort?: string;
      groupVersions?: boolean;
      limit?: number;
    }) => ["library", "albums", params] as const,
    artists: (limit: number) => ["library", "artists", { limit }] as const,
    genres: ["library", "genres"] as const,
    tracks: (params: { genre?: string; sort?: string; dir?: string; limit?: number }) =>
      ["library", "tracks", params] as const,
    search: (q: string) => ["library", "search", q.toLowerCase()] as const,
  },
  qobuz: {
    all: ["qobuz"] as const,
    artist: (target: { id: number } | { name: string }) =>
      "id" in target
        ? (["qobuz", "artist", "id", target.id] as const)
        : (["qobuz", "artist", "name", target.name.toLowerCase()] as const),
    search: (q: string) => ["qobuz", "search", q.toLowerCase()] as const,
  },
  albums: ["album"] as const,
  album: (albumId: string) => ["album", albumId] as const,
};

/** Marks library, album, and Qobuz queries stale after an SSE library ETag change (mounted views refetch in background; Qobuz overlays fresh inLibrary flags from Go catalogCache). */
export function invalidateLibraryQueries(client: QueryClient = queryClient): void {
  void client.invalidateQueries({ queryKey: queryKeys.library.all });
  void client.invalidateQueries({ queryKey: queryKeys.albums });
  void client.invalidateQueries({ queryKey: queryKeys.qobuz.all });
}

/** Clears cached Qobuz remote queries and marks library/album queries stale on Qobuz sign-out. */
export function invalidateQobuzLogout(client: QueryClient = queryClient): void {
  client.removeQueries({ queryKey: queryKeys.qobuz.all });
  void client.invalidateQueries({ queryKey: queryKeys.library.all });
  void client.invalidateQueries({ queryKey: queryKeys.albums });
}

export function queryErrorMessage(error: unknown): string {
  if (!error) return "";
  return error instanceof Error ? error.message : String(error);
}

/** Debounces rapid input changes while flushing empty strings immediately. */
export function useDebouncedValue<T>(value: T, delayMs: number): T {
  const [debounced, setDebounced] = useState(value);

  useEffect(() => {
    if (value === "" || delayMs <= 0) {
      setDebounced(value);
      return;
    }
    const timer = window.setTimeout(() => setDebounced(value), delayMs);
    return () => window.clearTimeout(timer);
  }, [value, delayMs]);

  return value === "" ? value : debounced;
}
