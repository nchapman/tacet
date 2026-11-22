import { describe, expect, it } from "vitest";
import { TacetAPIError } from "@/api/client";
import {
  createQueryClient,
  invalidateLibraryQueries,
  invalidateQobuzLogout,
  queryKeys,
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

  it("invalidates library and album queries without touching qobuz remote queries", () => {
    const client = createQueryClient();
    client.setQueryData(queryKeys.library.albums({ sort: "title" }), { items: [] });
    client.setQueryData(queryKeys.album("local:1"), { id: "local:1" });
    client.setQueryData(queryKeys.qobuz.artist({ id: 42 }), { name: "Miles" });

    invalidateLibraryQueries(client);

    expect(client.getQueryState(queryKeys.library.albums({ sort: "title" }))?.isInvalidated).toBe(true);
    expect(client.getQueryState(queryKeys.album("local:1"))?.isInvalidated).toBe(true);
    expect(client.getQueryState(queryKeys.qobuz.artist({ id: 42 }))?.isInvalidated).toBe(false);
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
});
