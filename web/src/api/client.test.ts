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
    expect(streamModeLabel("direct")).toBe("Direct Handoff");
    expect(streamModeLabel("relay")).toBe("Quiet Relay");
  });
});

describe("TacetClient", () => {
  it("sends JSON payloads and query params to the expected endpoints", async () => {
    const fetchMock = vi.fn<typeof fetch>().mockResolvedValue(
      new Response(JSON.stringify({ streamMode: "relay" }), {
        status: 200,
        headers: { "Content-Type": "application/json" },
      }),
    );
    const client = new TacetClient({ baseUrl: "http://localhost:8080/", fetchImpl: fetchMock });

    const res = await client.setStreamMode("relay");
    expect(res.streamMode).toBe("relay");
    expect(fetchMock).toHaveBeenCalledTimes(1);
    const [url, init] = fetchMock.mock.calls[0];
    expect(url).toBe("http://localhost:8080/api/settings/stream-mode");
    expect(init?.method).toBe("POST");
    expect(init?.body).toBe(JSON.stringify({ mode: "relay" }));
  });

  it("clamps volume and seek values before posting", async () => {
    const fetchMock = vi.fn<typeof fetch>().mockImplementation(async () =>
      new Response(JSON.stringify({ state: "PLAYING", volume: 100 }), { status: 200 }),
    );
    const client = new TacetClient({ fetchImpl: fetchMock });

    await client.setVolume(140.4, "uuid:wiim");
    await client.seek(-12, "uuid:wiim");

    expect(JSON.parse(String(fetchMock.mock.calls[0][1]?.body))).toEqual({ rendererId: "uuid:wiim", volume: 100 });
    expect(JSON.parse(String(fetchMock.mock.calls[1][1]?.body))).toEqual({ rendererId: "uuid:wiim", positionSec: 0 });
  });

  it("surfaces structured API errors as TacetAPIError", async () => {
    const fetchMock = vi.fn<typeof fetch>().mockResolvedValue(
      new Response(JSON.stringify({ error: "renderer not found" }), {
        status: 404,
        headers: { "Content-Type": "application/json" },
      }),
    );
    const client = new TacetClient({ fetchImpl: fetchMock });

    await expect(client.selectRenderer("missing")).rejects.toMatchObject({
      name: "TacetAPIError",
      message: "renderer not found",
      status: 404,
    } satisfies Partial<TacetAPIError>);
  });

  it("encodes search query params and album path segments", async () => {
    const fetchMock = vi.fn<typeof fetch>().mockImplementation(async () =>
      new Response(JSON.stringify({ tracks: [], albums: [] }), { status: 200 }),
    );
    const client = new TacetClient({ fetchImpl: fetchMock });

    await client.qobuzSearch("miles davis & coltrane");
    await client.getLocalAlbum("a/b");

    expect(fetchMock.mock.calls[0][0]).toBe("/api/qobuz/search?q=miles+davis+%26+coltrane");
    expect(fetchMock.mock.calls[1][0]).toBe("/api/library/albums/a%2Fb");
  });

  it("rejects empty or non-JSON 2xx responses with TacetAPIError", async () => {
    const emptyFetch = vi.fn<typeof fetch>().mockResolvedValue(new Response("", { status: 200 }));
    await expect(new TacetClient({ fetchImpl: emptyFetch }).getState()).rejects.toMatchObject({
      name: "TacetAPIError",
      status: 200,
    });

    const htmlFetch = vi.fn<typeof fetch>().mockResolvedValue(new Response("<html>proxy error</html>", { status: 200 }));
    await expect(new TacetClient({ fetchImpl: htmlFetch }).getState()).rejects.toMatchObject({
      name: "TacetAPIError",
      status: 200,
    });
  });
});
