import { describe, expect, it, vi } from "vitest";
import { TacetAPIError, TacetClient } from "./client";
import { fmtAlbumQuality, fmtDuration, fmtQuality, isHiRes, streamModeLabel } from "./format";

describe("format helpers", () => {
  it("formats durations cleanly across seconds, minutes, and hours", () => {
    expect(fmtDuration(0)).toBe("0:00");
    expect(fmtDuration(65)).toBe("1:05");
    expect(fmtDuration(3661)).toBe("1:01:01");
    expect(fmtDuration(-5)).toBe("0:00");
  });

  it("formats PCM and DSD track quality badges", () => {
    expect(fmtQuality({ ext: ".flac", mime: "audio/flac", bitDepth: 24, sampleRate: 192000 })).toBe("FLAC 24-bit / 192 kHz");
    expect(fmtQuality({ ext: ".flac", mime: "audio/flac", bitDepth: 16, sampleRate: 44100 })).toBe("FLAC 16-bit / 44.1 kHz");
    expect(fmtQuality({ ext: ".dsf", mime: "audio/x-dsf", bitDepth: 1, sampleRate: 2822400 })).toBe("DSD64 (2.82 MHz)");
    expect(fmtAlbumQuality({ maxBitDepth: 24, maxSampleRate: 96000 })).toBe("24-bit / 96 kHz");
  });

  it("detects hi-res PCM and DSD tracks", () => {
    expect(isHiRes(16, 44100)).toBe(false);
    expect(isHiRes(16, 48000)).toBe(false);
    expect(isHiRes(24, 48000)).toBe(true);
    expect(isHiRes(24, 192000)).toBe(true);
    expect(isHiRes(1, 2822400)).toBe(true);
    expect(streamModeLabel("direct")).toBe("Handoff");
    expect(streamModeLabel("relay")).toBe("Relay");
  });
});

describe("TacetClient v1", () => {
  it("resolves server-relative media paths while preserving absolute URLs", () => {
    const client = new TacetClient({ baseUrl: "http://nas.local:8080/" });
    expect(client.resolveUrl("/media/art/local:abc")).toBe("http://nas.local:8080/media/art/local:abc");
    expect(client.resolveUrl("https://static.qobuz.com/cover.jpg")).toBe("https://static.qobuz.com/cover.jpg");
    expect(client.resolveUrl("")).toBe("");
  });

  it("uses zone facade routes and discriminated QueueSource payloads", async () => {
    const fetchMock = vi.fn<typeof fetch>().mockImplementation(async () =>
      new Response(JSON.stringify({ zoneId: "uuid:wiim", revision: 2 }), {
        status: 200,
        headers: { "Content-Type": "application/json" },
      }),
    );
    const client = new TacetClient({ baseUrl: "http://localhost:8080/", fetchImpl: fetchMock });
    const zone = client.zone("uuid:wiim");

    await zone.play({ type: "album", albumId: "local:abc" }, 2);
    await zone.addItems({ type: "tracks", trackIds: ["qobuz:123"] }, { position: "next", autoplay: false });
    await zone.removeItem("qi_9");
    await zone.setVolume(140.4);
    await zone.seek(-12);

    expect(fetchMock.mock.calls[0][0]).toBe("http://localhost:8080/api/v1/zones/uuid%3Awiim/queue");
    expect(fetchMock.mock.calls[0][1]?.method).toBe("PUT");
    expect(JSON.parse(String(fetchMock.mock.calls[0][1]?.body))).toEqual({
      source: { type: "album", albumId: "local:abc" },
      startIndex: 2,
    });

    expect(fetchMock.mock.calls[1][0]).toBe("http://localhost:8080/api/v1/zones/uuid%3Awiim/queue/items");
    expect(JSON.parse(String(fetchMock.mock.calls[1][1]?.body))).toEqual({
      source: { type: "tracks", trackIds: ["qobuz:123"] },
      position: "next",
      autoplay: false,
    });

    expect(fetchMock.mock.calls[2][0]).toBe("http://localhost:8080/api/v1/zones/uuid%3Awiim/queue/items/qi_9");
    expect(fetchMock.mock.calls[2][1]?.method).toBe("DELETE");

    expect(fetchMock.mock.calls[3][0]).toBe("http://localhost:8080/api/v1/zones/uuid%3Awiim/volume");
    expect(fetchMock.mock.calls[3][1]?.method).toBe("PUT");
    expect(JSON.parse(String(fetchMock.mock.calls[3][1]?.body))).toEqual({ volume: 100 });

    expect(fetchMock.mock.calls[4][0]).toBe("http://localhost:8080/api/v1/zones/uuid%3Awiim/transport/seek");
    expect(JSON.parse(String(fetchMock.mock.calls[4][1]?.body))).toEqual({ positionSec: 0 });
  });

  it("surfaces structured API error envelopes as TacetAPIError", async () => {
    const fetchMock = vi.fn<typeof fetch>().mockResolvedValue(
      new Response(
        JSON.stringify({
          error: {
            code: "END_OF_QUEUE",
            message: "already at end of queue",
            retryable: false,
          },
        }),
        { status: 409, headers: { "Content-Type": "application/json" } },
      ),
    );
    const client = new TacetClient({ fetchImpl: fetchMock });

    await expect(client.zone("uuid:wiim").transport("next")).rejects.toMatchObject({
      name: "TacetAPIError",
      code: "END_OF_QUEUE",
      message: "already at end of queue",
      retryable: false,
      status: 409,
    } satisfies Partial<TacetAPIError>);
  });

  it("encodes search query params and album path segments and forwards AbortSignal", async () => {
    const fetchMock = vi.fn<typeof fetch>().mockImplementation(async () =>
      new Response(JSON.stringify({ tracks: [], albums: [] }), { status: 200 }),
    );
    const client = new TacetClient({ fetchImpl: fetchMock });
    const ac = new AbortController();

    await client.qobuzSearch("miles davis & coltrane", { limit: 20, offset: 40 }, { signal: ac.signal });
    await client.getLocalAlbum("local:a/b");

    expect(fetchMock.mock.calls[0][0]).toBe("/api/v1/qobuz/search?q=miles+davis+%26+coltrane&limit=20&offset=40");
    expect(fetchMock.mock.calls[0][1]?.signal).toBe(ac.signal);
    expect(fetchMock.mock.calls[1][0]).toBe("/api/v1/library/albums/local%3Aa%2Fb");
  });

  it("queries unified library search and qobuz artist endpoints", async () => {
    const fetchMock = vi
      .fn<typeof fetch>()
      .mockResolvedValueOnce(
        new Response(JSON.stringify({ query: "al green", artists: [], albums: [], tracks: [] }), { status: 200 }),
      )
      .mockResolvedValueOnce(
        new Response(JSON.stringify({ artist: { id: 42, name: "Al Green" }, albums: [] }), { status: 200 }),
      );
    const client = new TacetClient({ fetchImpl: fetchMock });

    await client.searchLibrary("al green", 8);
    await client.qobuzArtist({ name: "Al Green", limit: 50 });

    expect(fetchMock.mock.calls[0][0]).toBe("/api/v1/library/search?q=al+green&limit=8");
    expect(fetchMock.mock.calls[1][0]).toBe("/api/v1/qobuz/artist?name=Al+Green&limit=50");
  });

  it("rejects empty or non-JSON 2xx responses with TacetAPIError", async () => {
    const emptyFetch = vi.fn<typeof fetch>().mockResolvedValue(new Response("", { status: 200 }));
    await expect(new TacetClient({ fetchImpl: emptyFetch }).listZones()).rejects.toMatchObject({
      name: "TacetAPIError",
      code: "INVALID_RESPONSE",
      status: 200,
    });

    const htmlFetch = vi.fn<typeof fetch>().mockResolvedValue(new Response("<html>proxy error</html>", { status: 200 }));
    await expect(new TacetClient({ fetchImpl: htmlFetch }).listZones()).rejects.toMatchObject({
      name: "TacetAPIError",
      code: "INVALID_RESPONSE",
      status: 200,
    });
  });

  it("subscribes to split SSE events using injectable EventSource", () => {
    const listeners = new Map<string, (ev: MessageEvent<string>) => void>();
    const closeSpy = vi.fn();
    let capturedURL = "";

    class FakeEventSource {
      onopen: (() => void) | null = null;
      onerror: (() => void) | null = null;
      constructor(url: string) {
        capturedURL = url;
      }
      addEventListener(name: string, fn: (ev: MessageEvent<string>) => void) {
        listeners.set(name, fn);
      }
      close() {
        closeSpy();
      }
    }

    const client = new TacetClient({
      baseUrl: "http://localhost:8080",
      eventSourceImpl: FakeEventSource as unknown as typeof EventSource,
    });

    const onVolume = vi.fn();
    const unsub = client.subscribeEvents({ onVolume }, ["uuid:living", "uuid:office"]);
    expect(capturedURL).toBe("http://localhost:8080/api/v1/events?zone=uuid%3Aliving&zone=uuid%3Aoffice");

    listeners.get("zone.volume")?.({
      data: JSON.stringify({ zoneId: "uuid:living", revision: 5, volume: 42, muted: false }),
    } as MessageEvent<string>);

    expect(onVolume).toHaveBeenCalledWith({ zoneId: "uuid:living", revision: 5, volume: 42, muted: false });
    unsub();
    expect(closeSpy).toHaveBeenCalledTimes(1);
  });

  it("encodes unified library filters, version controls, and remote sync calls", async () => {
    const fetchMock = vi.fn<typeof fetch>().mockImplementation(async () =>
      new Response(JSON.stringify({ items: [], total: 0 }), {
        status: 200,
        headers: { "Content-Type": "application/json" },
      }),
    );
    const client = new TacetClient({ baseUrl: "http://localhost:8080/", fetchImpl: fetchMock });

    await client.listLibraryAlbums({ q: "miles", source: "remote", sort: "plays", groupVersions: true, limit: 50 });
    await client.listLibraryTracks({ genre: "Jazz", artist: "Miles Davis", sort: "quality", dir: "desc" });
    await client.getHeavyRotation(30, 12);
    await client.addRemoteAlbum("qobuz", "alb-99");
    await client.setPrimaryVersion("local:abc");
    await client.syncLibrary("qobuz", true);

    expect(fetchMock.mock.calls[0][0]).toBe(
      "http://localhost:8080/api/v1/library/albums?q=miles&source=remote&sort=plays&groupVersions=true&limit=50",
    );
    expect(fetchMock.mock.calls[1][0]).toBe(
      "http://localhost:8080/api/v1/library/tracks?artist=Miles+Davis&genre=Jazz&sort=quality&dir=desc",
    );
    expect(fetchMock.mock.calls[2][0]).toBe(
      "http://localhost:8080/api/v1/library/heavy-rotation?days=30&limit=12",
    );
    expect(fetchMock.mock.calls[3][0]).toBe("http://localhost:8080/api/v1/library/remote/albums");
    expect(JSON.parse(String(fetchMock.mock.calls[3][1]?.body))).toEqual({ provider: "qobuz", externalId: "alb-99" });
    expect(fetchMock.mock.calls[4][0]).toBe("http://localhost:8080/api/v1/library/albums/local%3Aabc/primary");
    expect(fetchMock.mock.calls[5][0]).toBe("http://localhost:8080/api/v1/library/sync?provider=qobuz&wait=true");
  });
});
