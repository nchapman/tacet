import { describe, expect, it } from "vitest";
import { albumPath, artistPath, sectionFromPathname } from "./routes";

describe("routes", () => {
  it("maps pathnames to active sidebar sections", () => {
    expect(sectionFromPathname("/")).toBe("home");
    expect(sectionFromPathname("/search")).toBe("search");
    expect(sectionFromPathname("/albums")).toBe("albums");
    expect(sectionFromPathname("/albums/local%3Aabc")).toBe("albums");
    expect(sectionFromPathname("/artists")).toBe("artists");
    expect(sectionFromPathname("/artists/Miles%20Davis")).toBe("artists");
    expect(sectionFromPathname("/songs")).toBe("songs");
    expect(sectionFromPathname("/qobuz")).toBe("qobuz");
    expect(sectionFromPathname("/settings")).toBe("settings");
    expect(sectionFromPathname("/unknown")).toBe("home");
  });

  it("encodes album IDs safely including colons", () => {
    expect(albumPath("local:9a3f01")).toBe("/albums/local%3A9a3f01");
    expect(albumPath("qobuz:006025")).toBe("/albums/qobuz%3A006025");
  });

  it("encodes artist names including slashes and optional qobuzId", () => {
    expect(artistPath("Miles Davis")).toBe("/artists/Miles%20Davis");
    expect(artistPath("AC/DC")).toBe("/artists/AC%2FDC");
    expect(artistPath("Al Green", 4201)).toBe("/artists/Al%20Green?qobuzId=4201");
  });
});
