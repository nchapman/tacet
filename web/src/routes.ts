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

export function albumPath(albumId: string): string {
  return `/albums/${encodeURIComponent(albumId)}`;
}

export function artistPath(artistName: string, qobuzArtistId?: number): string {
  const base = `/artists/${encodeURIComponent(artistName)}`;
  if (typeof qobuzArtistId === "number" && Number.isFinite(qobuzArtistId) && qobuzArtistId > 0) {
    return `${base}?qobuzId=${qobuzArtistId}`;
  }
  return base;
}
