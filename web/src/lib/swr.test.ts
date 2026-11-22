import { describe, expect, it, vi } from "vitest";
import { SwrCache } from "./swr";

describe("SwrCache", () => {
  it("evicts least-recently-used entries when exceeding capacity", () => {
    const cache = new SwrCache(2);
    cache.set("a", 1);
    cache.set("b", 2);
    // Touch 'a' so 'b' becomes least-recently-used.
    expect(cache.get("a")?.data).toBe(1);
    cache.set("c", 3);

    expect(cache.peek("a")?.data).toBe(1);
    expect(cache.peek("b")).toBeUndefined();
    expect(cache.peek("c")?.data).toBe(3);
  });

  it("treats entries as stale when version changes or staleTime elapses", () => {
    const cache = new SwrCache(10);
    cache.set("k", { ok: true }, "etag-1");

    expect(cache.isFresh("k", 60_000, "etag-1")).toBe(true);
    expect(cache.isFresh("k", 60_000, "etag-2")).toBe(false);
    expect(cache.isFresh("k", 0, "etag-1")).toBe(false);
  });

  it("deduplicates concurrent in-flight fetches for the same key and version", async () => {
    const cache = new SwrCache(10);
    let resolveFetch!: (value: string) => void;
    const fetcher = vi.fn(
      (_signal: AbortSignal) =>
        new Promise<string>((resolve) => {
          resolveFetch = resolve;
        }),
    );

    const h1 = cache.fetch("albums", fetcher, "v1");
    const h2 = cache.fetch("albums", fetcher, "v1");
    expect(fetcher).toHaveBeenCalledTimes(1);

    // Cancelling only one subscriber keeps the shared fetch alive.
    h1.cancel();
    expect(fetcher.mock.calls[0][0].aborted).toBe(false);

    resolveFetch("payload");
    await expect(Promise.all([h1.promise, h2.promise])).resolves.toEqual(["payload", "payload"]);
    expect(cache.peek("albums")?.data).toBe("payload");
  });

  it("aborts in-flight fetch once all subscribers cancel and does not cache aborted result", async () => {
    const cache = new SwrCache(10);
    let capturedSignal!: AbortSignal;
    let resolveFetch!: (value: string) => void;
    const fetcher = vi.fn((signal: AbortSignal) => {
      capturedSignal = signal;
      return new Promise<string>((resolve) => {
        resolveFetch = resolve;
      });
    });

    const h1 = cache.fetch("search:qobuz:ba", fetcher);
    const h2 = cache.fetch("search:qobuz:ba", fetcher);
    h1.cancel();
    expect(capturedSignal.aborted).toBe(false);
    h2.cancel();
    expect(capturedSignal.aborted).toBe(true);

    resolveFetch("late-result");
    await h1.promise;
    expect(cache.peek("search:qobuz:ba")).toBeUndefined();
  });

  it("does not deduplicate in-flight fetches across different versions", async () => {
    const cache = new SwrCache(10);
    let resolveV1!: (value: string) => void;
    let resolveV2!: (value: string) => void;
    const f1 = vi.fn(
      () =>
        new Promise<string>((resolve) => {
          resolveV1 = resolve;
        }),
    );
    const f2 = vi.fn(
      () =>
        new Promise<string>((resolve) => {
          resolveV2 = resolve;
        }),
    );

    const h1 = cache.fetch("albums", f1, "etag-1");
    const h2 = cache.fetch("albums", f2, "etag-2");
    expect(f1).toHaveBeenCalledTimes(1);
    expect(f2).toHaveBeenCalledTimes(1);

    resolveV1("old");
    await h1.promise;
    // Superseded by in-flight etag-2, so old payload does not write stale version.
    expect(cache.peek("albums")).toBeUndefined();

    resolveV2("fresh");
    await h2.promise;
    expect(cache.peek("albums")?.data).toBe("fresh");
    expect(cache.peek("albums")?.version).toBe("etag-2");
  });

  it("ignores slow out-of-order completion from an older version", async () => {
    const cache = new SwrCache(10);
    let resolveV1!: (value: string) => void;
    let resolveV2!: (value: string) => void;

    const h1 = cache.fetch(
      "albums",
      () =>
        new Promise<string>((resolve) => {
          resolveV1 = resolve;
        }),
      "etag-1",
    );
    const h2 = cache.fetch(
      "albums",
      () =>
        new Promise<string>((resolve) => {
          resolveV2 = resolve;
        }),
      "etag-2",
    );

    // Newer version finishes first, then older slower version resolves afterwards.
    resolveV2("fresh");
    await h2.promise;
    expect(cache.peek("albums")?.data).toBe("fresh");
    expect(cache.peek("albums")?.version).toBe("etag-2");

    resolveV1("stale");
    await h1.promise;
    expect(cache.peek("albums")?.data).toBe("fresh");
    expect(cache.peek("albums")?.version).toBe("etag-2");
  });

  it("marks prefix-matched entries stale without dropping cached data", () => {
    const cache = new SwrCache(10);
    const listener = vi.fn();
    const unsub = cache.subscribe("library:albums", listener);

    cache.set("library:albums", ["a"]);
    const beforeRef = cache.peek("library:albums");
    cache.set("qobuz:artist:1", { name: "Miles" });
    listener.mockClear();

    cache.invalidatePrefix("library:");

    expect(cache.peek("library:albums")).not.toBe(beforeRef);
    expect(cache.peek("library:albums")?.data).toEqual(["a"]);
    expect(cache.isFresh("library:albums", 60_000)).toBe(false);
    expect(cache.isFresh("qobuz:artist:1", 60_000)).toBe(true);
    expect(listener).toHaveBeenCalledTimes(1);

    unsub();
  });
});
