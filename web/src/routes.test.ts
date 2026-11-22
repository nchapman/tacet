import { describe, expect, it } from "vitest";
import { albumPath, artistPath, decodeAlbumKey, encodeAlbumKey, sectionFromPathname, slugify } from "./routes";

describe("routes", () => {
  it("maps pathnames to active sidebar sections", () => {
    expect(sectionFromPathname("/")).toBe("home");
    expect(sectionFromPathname("/search")).toBe("search");
    expect(sectionFromPathname("/albums")).toBe("albums");
    expect(sectionFromPathname("/albums/al-green-greatest-hits/local-e9c2e2a2")).toBe("albums");
    expect(sectionFromPathname("/artists")).toBe("artists");
    expect(sectionFromPathname("/artists/Miles%20Davis")).toBe("artists");
    expect(sectionFromPathname("/songs")).toBe("songs");
    expect(sectionFromPathname("/qobuz")).toBe("qobuz");
    expect(sectionFromPathname("/settings")).toBe("settings");
    expect(sectionFromPathname("/unknown")).toBe("home");
  });

  it("slugifies accents and punctuation cleanly", () => {
    expect(slugify("Al Green")).toBe("al-green");
    expect(slugify("Kind of Blue (Legacy Edition)")).toBe("kind-of-blue-legacy-edition");
    expect(slugify("Björk")).toBe("bjork");
    expect(slugify("AC/DC")).toBe("ac-dc");
  });

  it("encodes and decodes album keys without %3A and preserves hyphens in remote IDs", () => {
    expect(encodeAlbumKey("local:9a3f01")).toBe("local-9a3f01");
    expect(decodeAlbumKey("local-9a3f01")).toBe("local:9a3f01");
    expect(encodeAlbumKey("qobuz:abc-def-123")).toBe("qobuz-abc-def-123");
    expect(decodeAlbumKey("qobuz-abc-def-123")).toBe("qobuz:abc-def-123");
    // Legacy colon-separated IDs still decode cleanly.
    expect(decodeAlbumKey("local:9a3f01")).toBe("local:9a3f01");
  });

  it("builds human-readable /albums/<artist-album>/<key> URLs", () => {
    expect(albumPath("local:e9c2e2a2438c59ce", { artist: "Al Green", title: "Greatest Hits" })).toBe(
      "/albums/al-green-greatest-hits/local-e9c2e2a2438c59ce",
    );
    expect(albumPath("qobuz:006025", { artist: "Miles Davis", title: "Kind of Blue" })).toBe(
      "/albums/miles-davis-kind-of-blue/qobuz-006025",
    );
    expect(albumPath("local:9a3f01")).toBe("/albums/local-9a3f01");
  });

  it("encodes artist names including slashes and optional qobuzId", () => {
    expect(artistPath("Miles Davis")).toBe("/artists/Miles%20Davis");
    expect(artistPath("AC/DC")).toBe("/artists/AC%2FDC");
    expect(artistPath("Al Green", 4201)).toBe("/artists/Al%20Green?qobuzId=4201");
  });
});
