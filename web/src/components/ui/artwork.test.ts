import { describe, expect, it } from "vitest";
import { isArtworkLoaded, markArtworkLoaded, resolveArtworkSrc } from "./artwork";

describe("artwork helpers", () => {
  it("appends fixed width buckets for /media/art/ URLs while leaving CDN URLs untouched", () => {
    expect(resolveArtworkSrc("/media/art/abc123", "sm")).toBe("/media/art/abc123?w=400");
    expect(resolveArtworkSrc("/media/art/abc123", "lg")).toBe("/media/art/abc123?w=1000");
    expect(resolveArtworkSrc("/media/art/abc123?w=400", "lg")).toBe("/media/art/abc123?w=400");
    expect(resolveArtworkSrc("https://static.qobuz.com/images/cover.jpg", "lg")).toBe(
      "https://static.qobuz.com/images/cover.jpg",
    );
    expect(resolveArtworkSrc(undefined)).toBe("");
  });

  it("tracks loaded artwork URLs in the session cache", () => {
    const url = "/media/art/test-cache-entry?w=400";
    expect(isArtworkLoaded(url)).toBe(false);
    markArtworkLoaded(url);
    expect(isArtworkLoaded(url)).toBe(true);
    expect(isArtworkLoaded("")).toBe(false);
  });
});
