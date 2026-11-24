import {
  persistQueryClientRestore,
  persistQueryClientSave,
  type AsyncStorage,
} from "@tanstack/react-query-persist-client";
import { describe, expect, it } from "vitest";
import { TacetAPIError } from "@/api/client";
import {
  DEFAULT_GC_TIME_MS,
  QOBUZ_ALBUM_GC_TIME_MS,
  QOBUZ_ARTIST_GC_TIME_MS,
  QOBUZ_SEARCH_GC_TIME_MS,
  QUERY_PERSIST_BUSTER,
  QUERY_PERSIST_MAX_AGE_MS,
  createQueryClient,
  createQueryPersister,
  invalidateLibraryQueries,
  invalidateQobuzLogout,
  queryKeys,
  shouldPersistQuery,
  shouldRetryQuery,
} from "./query";

describe("query config and helpers", () => {
  it("respects TacetAPIError.retryable and retries transient errors once", () => {
    expect(
      shouldRetryQuery(0, new TacetAPIError(404, { code: "not_found", message: "not found", retryable: false })),
    ).toBe(false);
    expect(
      shouldRetryQuery(0, new TacetAPIError(429, { code: "rate_limited", message: "slow down", retryable: true })),
    ).toBe(true);
    expect(
      shouldRetryQuery(1, new TacetAPIError(429, { code: "rate_limited", message: "slow down", retryable: true })),
    ).toBe(false);
    expect(
      shouldRetryQuery(0, new TacetAPIError(500, { code: "internal", message: "server error", retryable: true })),
    ).toBe(true);
    expect(shouldRetryQuery(0, new Error("network down"))).toBe(true);
    expect(shouldRetryQuery(1, new Error("network down"))).toBe(false);
  });

  it("invalidates library, album, and qobuz queries on library ETag change", () => {
    const client = createQueryClient();
    client.setQueryData(queryKeys.library.albums({ sort: "title" }), { items: [] });
    client.setQueryData(queryKeys.album("local:1"), { id: "local:1" });
    client.setQueryData(queryKeys.qobuz.artist({ id: 42 }), { name: "Miles" });

    invalidateLibraryQueries(client);

    expect(client.getQueryState(queryKeys.library.albums({ sort: "title" }))?.isInvalidated).toBe(true);
    expect(client.getQueryState(queryKeys.album("local:1"))?.isInvalidated).toBe(true);
    expect(client.getQueryState(queryKeys.qobuz.artist({ id: 42 }))?.isInvalidated).toBe(true);
  });

  it("removes cached qobuz queries and invalidates library/albums on Qobuz logout", () => {
    const client = createQueryClient();
    client.setQueryData(queryKeys.qobuz.search("miles"), { albums: [] });
    client.setQueryData(queryKeys.library.albums({ sort: "added" }), { items: [] });
    client.setQueryData(queryKeys.album("qobuz:123"), { id: "qobuz:123" });

    invalidateQobuzLogout(client);

    expect(client.getQueryData(queryKeys.qobuz.search("miles"))).toBeUndefined();
    expect(client.getQueryState(queryKeys.library.albums({ sort: "added" }))?.isInvalidated).toBe(true);
    expect(client.getQueryState(queryKeys.album("qobuz:123"))?.isInvalidated).toBe(true);
  });

  it("keeps gcTime >= persist maxAge across all query presets", () => {
    expect(DEFAULT_GC_TIME_MS).toBeGreaterThanOrEqual(QUERY_PERSIST_MAX_AGE_MS);
    expect(QOBUZ_ARTIST_GC_TIME_MS).toBeGreaterThanOrEqual(QUERY_PERSIST_MAX_AGE_MS);
    expect(QOBUZ_ALBUM_GC_TIME_MS).toBeGreaterThanOrEqual(QUERY_PERSIST_MAX_AGE_MS);
    expect(QOBUZ_SEARCH_GC_TIME_MS).toBeGreaterThanOrEqual(QUERY_PERSIST_MAX_AGE_MS);
  });

  it("persists and restores cached feeds across QueryClient instances while preserving dataUpdatedAt", async () => {
    const map = new Map<string, string>();
    const storage: AsyncStorage<string> = {
      getItem: (key) => map.get(key) ?? null,
      setItem: (key, value) => {
        map.set(key, value);
      },
      removeItem: (key) => {
        map.delete(key);
      },
    };
    const persister = createQueryPersister(storage);
    const persistOpts = {
      persister,
      maxAge: QUERY_PERSIST_MAX_AGE_MS,
      buster: QUERY_PERSIST_BUSTER,
      dehydrateOptions: {
        shouldDehydrateQuery: shouldPersistQuery,
      },
    };

    const updatedAt = Date.now() - 60_000;
    const writer = createQueryClient();
    writer.setQueryData(queryKeys.discover.feed([]), { connected: true, genres: [], shelves: [] }, { updatedAt });
    writer.setQueryData(queryKeys.library.heavyRotation(90, 7), { items: [{ id: "local:1" }] }, { updatedAt });
    writer.setQueryData(queryKeys.library.albums({ sort: "title", limit: 60, offset: 0 }), { items: [] }, { updatedAt });
    writer.setQueryData(queryKeys.library.albums({ sort: "title", limit: 60, offset: 60 }), { items: [] }, { updatedAt });
    writer.setQueryData(queryKeys.library.search("miles"), { albums: [], artists: [], tracks: [] }, { updatedAt });
    writer.setQueryData(queryKeys.qobuz.search("miles"), { albums: [], artists: [], tracks: [] }, { updatedAt });

    await persistQueryClientSave({ ...persistOpts, queryClient: writer });

    // Restore into a brand-new QueryClient (simulating a page reload).
    const reader = createQueryClient();
    await persistQueryClientRestore({ ...persistOpts, queryClient: reader });

    expect(reader.getQueryData(queryKeys.discover.feed([]))).toEqual({ connected: true, genres: [], shelves: [] });
    expect(reader.getQueryData(queryKeys.library.heavyRotation(90, 7))).toEqual({ items: [{ id: "local:1" }] });
    expect(reader.getQueryData(queryKeys.library.albums({ sort: "title", limit: 60, offset: 0 }))).toEqual({
      items: [],
    });
    expect(reader.getQueryState(queryKeys.discover.feed([]))?.dataUpdatedAt).toBe(updatedAt);
    // Deep pagination pages and ephemeral search queries are excluded from localStorage persistence.
    expect(reader.getQueryData(queryKeys.library.albums({ sort: "title", limit: 60, offset: 60 }))).toBeUndefined();
    expect(reader.getQueryData(queryKeys.library.search("miles"))).toBeUndefined();
    expect(reader.getQueryData(queryKeys.qobuz.search("miles"))).toBeUndefined();

    // Mismatched buster discards stale schema cleanly.
    const bumped = createQueryClient();
    await persistQueryClientRestore({ ...persistOpts, buster: "v2", queryClient: bumped });
    expect(bumped.getQueryData(queryKeys.discover.feed([]))).toBeUndefined();
  });
});
