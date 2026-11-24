import { createAsyncStoragePersister } from "@tanstack/query-async-storage-persister";
import {
  persistQueryClientSave,
  removeOldestQuery,
  type AsyncStorage,
  type PersistQueryClientOptions,
} from "@tanstack/react-query-persist-client";
import {
  defaultShouldDehydrateQuery,
  QueryClient,
  type Query,
} from "@tanstack/react-query";
import { useEffect, useState } from "react";
import { TacetAPIError } from "@/api/client";

export const QUERY_PERSIST_STORAGE_KEY = "tacet.queryCache";
export const QUERY_PERSIST_BUSTER = "v1";
export const QUERY_PERSIST_MAX_AGE_MS = 24 * 60 * 60_000;

export const DEFAULT_STALE_TIME_MS = 30_000;
// Keep gcTime >= QUERY_PERSIST_MAX_AGE_MS per TanStack Query persist docs so
// hydrated cache entries are not discarded by garbage collection on restore.
export const DEFAULT_GC_TIME_MS = QUERY_PERSIST_MAX_AGE_MS;
export const QOBUZ_ARTIST_STALE_TIME_MS = 60 * 60_000;
export const QOBUZ_ARTIST_GC_TIME_MS = QUERY_PERSIST_MAX_AGE_MS;
export const QOBUZ_ALBUM_STALE_TIME_MS = 60 * 60_000;
export const QOBUZ_ALBUM_GC_TIME_MS = QUERY_PERSIST_MAX_AGE_MS;
export const QOBUZ_SEARCH_STALE_TIME_MS = 15 * 60_000;
export const QOBUZ_SEARCH_GC_TIME_MS = QUERY_PERSIST_MAX_AGE_MS;

function getBrowserLocalStorage(): AsyncStorage<string> | undefined {
  try {
    return typeof window !== "undefined" ? window.localStorage : undefined;
  } catch {
    return undefined;
  }
}

function isFirstPageList(params: unknown): boolean {
  if (params == null || typeof params !== "object") return true;
  const p = params as Record<string, unknown>;
  if (typeof p.artist === "string" && p.artist.length > 0) return false;
  return p.offset === undefined || p.offset === 0;
}

/**
 * Persists durable feeds and first-page library lists across reloads so Home,
 * Albums, Artists, Songs, Qobuz, Heavy Rotation, and Discover shelves render
 * stably via SWR without unbounded localStorage growth from deep pagination or
 * per-keystroke search queries.
 */
export function shouldPersistQuery(query: Query): boolean {
  if (!defaultShouldDehydrateQuery(query)) return false;
  const [scope, kind, params] = query.queryKey;
  if (scope === "discover") {
    return kind === "feed" || kind === "shelf";
  }
  if (scope === "library") {
    if (kind === "heavy-rotation" || kind === "genres") return true;
    if (kind === "albums" || kind === "artists" || kind === "tracks") {
      return isFirstPageList(params);
    }
  }
  return false;
}

export function createQueryPersister(storage: AsyncStorage<string> | null | undefined = getBrowserLocalStorage()) {
  return createAsyncStoragePersister({
    storage,
    key: QUERY_PERSIST_STORAGE_KEY,
    throttleTime: 1000,
    retry: removeOldestQuery,
  });
}

export const queryPersister = createQueryPersister();

export const queryPersistOptions: Omit<PersistQueryClientOptions, "queryClient"> = {
  persister: queryPersister,
  maxAge: QUERY_PERSIST_MAX_AGE_MS,
  buster: QUERY_PERSIST_BUSTER,
  dehydrateOptions: {
    shouldDehydrateQuery: shouldPersistQuery,
  },
};

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
      offset?: number;
    }) => ["library", "albums", params] as const,
    artists: (params: { limit?: number; offset?: number }) => ["library", "artists", params] as const,
    genres: ["library", "genres"] as const,
    tracks: (params: { genre?: string; sort?: string; dir?: string; limit?: number; offset?: number }) =>
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
  discover: {
    all: ["discover"] as const,
    feed: (genreIds: number[] = []) =>
      ["discover", "feed", genreIds.length > 0 ? [...genreIds].sort((a, b) => a - b).join(",") : "all"] as const,
    shelf: (slug: string, genreIds: number[] = []) =>
      ["discover", "shelf", slug, genreIds.length > 0 ? [...genreIds].sort((a, b) => a - b).join(",") : "all"] as const,
    similarAlbums: (albumId: string) => ["discover", "similar-albums", albumId] as const,
    similarArtists: (artistId: number) => ["discover", "similar-artists", artistId] as const,
  },
  albums: ["album"] as const,
  album: (albumId: string) => ["album", albumId] as const,
};

/** Marks library, album, and Qobuz queries stale after an SSE library ETag change (mounted views refetch in background; Qobuz overlays fresh inLibrary flags from Go catalogCache). */
export function invalidateLibraryQueries(client: QueryClient = queryClient): void {
  void client.invalidateQueries({ queryKey: queryKeys.library.all });
  void client.invalidateQueries({ queryKey: queryKeys.albums });
  void client.invalidateQueries({ queryKey: queryKeys.qobuz.all });
  void client.invalidateQueries({ queryKey: queryKeys.discover.all });
}

/** Clears cached Qobuz remote queries and marks library/album queries stale on Qobuz sign-out. */
export function invalidateQobuzLogout(client: QueryClient = queryClient): void {
  client.removeQueries({ queryKey: queryKeys.qobuz.all });
  client.removeQueries({ queryKey: queryKeys.discover.all });
  void client.invalidateQueries({ queryKey: queryKeys.library.all });
  void client.invalidateQueries({ queryKey: queryKeys.albums });
  if (client === queryClient) {
    void (async () => {
      try {
        await persistQueryClientSave({
          ...queryPersistOptions,
          queryClient: client,
        });
      } catch (err) {
        console.warn("Failed to update persisted query cache on Qobuz logout:", err);
        try {
          await queryPersister.removeClient();
        } catch {
          // Storage unavailable or blocked; nothing more to clear.
        }
      }
    })();
  }
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
