import type { AppSection } from "@/components/Sidebar";

export const SECTION_PATHS: Record<AppSection, string> = {
  home: "/",
  search: "/search",
  albums: "/albums",
  artists: "/artists",
  songs: "/songs",
  qobuz: "/qobuz",
  settings: "/settings",
};

export const SECTION_LABELS: Record<AppSection, string> = {
  home: "Home",
  search: "Search",
  albums: "Albums",
  artists: "Artists",
  songs: "Songs",
  qobuz: "Qobuz",
  settings: "Settings",
};

export function sectionFromPathname(pathname: string): AppSection {
  if (pathname === "/search" || pathname.startsWith("/search/")) return "search";
  if (pathname === "/albums" || pathname.startsWith("/albums/")) return "albums";
  if (pathname === "/artists" || pathname.startsWith("/artists/")) return "artists";
  if (pathname === "/songs" || pathname.startsWith("/songs/")) return "songs";
  if (pathname === "/qobuz" || pathname.startsWith("/qobuz/")) return "qobuz";
  if (pathname === "/settings" || pathname.startsWith("/settings/")) return "settings";
  return "home";
}

export function slugify(text: string): string {
  const slug = text
    .normalize("NFKD")
    .replace(/[\u0300-\u036f]/g, "")
    .replace(/[^a-zA-Z0-9]+/g, "-")
    .replace(/^-+|-+$/g, "");
  return slug || "Album";
}

/**
 * Converts an internal album ID (`local:e9c2...`, `qobuz:0060...`) into a
 * URL-friendly segment without `%3A` (`local-e9c2...`, `qobuz-0060...`).
 */
export function encodeAlbumKey(albumId: string): string {
  const colon = albumId.indexOf(":");
  if (colon > 0 && /^[a-z0-9_]+$/.test(albumId.slice(0, colon))) {
    const source = albumId.slice(0, colon);
    const rest = albumId.slice(colon + 1);
    return `${source}-${encodeURIComponent(rest)}`;
  }
  return encodeURIComponent(albumId);
}

/**
 * Restores an internal album ID from a URL segment (`local-e9c2...` -> `local:e9c2...`),
 * while also accepting legacy colon-separated IDs.
 */
export function decodeAlbumKey(rawParam: string): string {
  if (rawParam.includes(":")) return rawParam;
  const dash = rawParam.indexOf("-");
  if (dash > 0) {
    const source = rawParam.slice(0, dash);
    if (/^[a-z0-9_]+$/.test(source)) {
      return `${source}:${rawParam.slice(dash + 1)}`;
    }
  }
  return rawParam;
}

export function albumPath(
  albumId: string,
  meta?: { artist?: string; title?: string },
): string {
  const key = encodeAlbumKey(albumId);
  const parts = [meta?.artist, meta?.title].filter((v): v is string => Boolean(v && v.trim()));
  if (parts.length > 0) {
    return `/albums/${slugify(parts.join(" "))}/${key}`;
  }
  return `/albums/${key}`;
}

export function discoverShelfPath(slug: string, genreId?: number | null): string {
  const base = `/discover/${encodeURIComponent(slug)}`;
  return genreId ? `${base}?genre=${genreId}` : base;
}

export function artistPath(artistName: string, qobuzArtistId?: number): string {
  const base = `/artists/${encodeURIComponent(artistName)}`;
  if (typeof qobuzArtistId === "number" && Number.isFinite(qobuzArtistId) && qobuzArtistId > 0) {
    return `${base}?qobuzId=${qobuzArtistId}`;
  }
  return base;
}
