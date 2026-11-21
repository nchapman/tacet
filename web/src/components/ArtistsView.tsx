import { ArrowLeft, ChevronRight, ListPlus, Play, Search } from "lucide-react";
import { useEffect, useState } from "react";
import { tacetClient } from "@/api/client";
import { fmtAlbumQuality, isHiRes } from "@/api/format";
import type { Album, ArtistSummary, LibraryStatus, QueuePosition, QueueSource } from "@/api/types";
import { AlbumArt, AmbientBackdrop } from "@/components/ui/artwork";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { cn } from "@/lib/utils";

interface ArtistsViewProps {
  libraryStatus: LibraryStatus | null;
  initialArtist?: string;
  onOpenAlbum: (album: Album) => void;
  onPlay: (source: QueueSource, startIndex?: number) => void;
  onAdd: (source: QueueSource, position?: QueuePosition) => void;
}

/**
 * Album-forward Artists browser:
 * - Desktop: split master-detail with independently scrolled Artist rail & stacked album discography.
 * - Mobile: clean list -> stacked albums drilldown with back bar.
 */
export function ArtistsView({ libraryStatus, initialArtist, onOpenAlbum, onPlay, onAdd }: ArtistsViewProps) {
  const [query, setQuery] = useState("");
  const [debouncedQuery, setDebouncedQuery] = useState("");
  const [artists, setArtists] = useState<ArtistSummary[]>([]);
  const [selectedArtist, setSelectedArtist] = useState(initialArtist ?? "");
  const [mobileDetailOpen, setMobileDetailOpen] = useState(Boolean(initialArtist));
  const [albums, setAlbums] = useState<Album[]>([]);
  const [loadingArtists, setLoadingArtists] = useState(true);
  const [loadingAlbums, setLoadingAlbums] = useState(false);
  const [errMsg, setErrMsg] = useState("");

  useEffect(() => {
    if (initialArtist) {
      setSelectedArtist(initialArtist);
      setMobileDetailOpen(true);
    }
  }, [initialArtist]);

  useEffect(() => {
    const id = window.setTimeout(() => setDebouncedQuery(query.trim()), 150);
    return () => window.clearTimeout(id);
  }, [query]);

  // Fetch unified artist list.
  useEffect(() => {
    const ctrl = new AbortController();
    setLoadingArtists(true);
    setErrMsg("");
    void tacetClient
      .listLibraryArtists({ q: debouncedQuery || undefined, limit: 500 }, { signal: ctrl.signal })
      .then((res) => {
        if (ctrl.signal.aborted) return;
        const list = res.items ?? [];
        setArtists(list);
        setSelectedArtist((prev) => {
          if (prev && list.some((a) => a.name === prev)) return prev;
          return list[0]?.name ?? "";
        });
      })
      .catch((err) => {
        if (!ctrl.signal.aborted) setErrMsg(err instanceof Error ? err.message : String(err));
      })
      .finally(() => {
        if (!ctrl.signal.aborted) setLoadingArtists(false);
      });
    return () => ctrl.abort();
  }, [debouncedQuery, libraryStatus?.etag]);

  // Fetch stacked albums for the selected artist (abortable single source of truth).
  useEffect(() => {
    if (!selectedArtist) {
      setAlbums([]);
      return;
    }
    const ctrl = new AbortController();
    setLoadingAlbums(true);
    setErrMsg("");
    void tacetClient
      .listLibraryAlbums(
        { artist: selectedArtist, groupVersions: true, sort: "year", limit: 200 },
        { signal: ctrl.signal },
      )
      .then((res) => {
        if (ctrl.signal.aborted) return;
        setAlbums(res.items ?? []);
      })
      .catch((err) => {
        if (!ctrl.signal.aborted) setErrMsg(err instanceof Error ? err.message : String(err));
      })
      .finally(() => {
        if (!ctrl.signal.aborted) setLoadingAlbums(false);
      });
    return () => ctrl.abort();
  }, [selectedArtist, libraryStatus?.etag]);

  const currentSummary = artists.find((a) => a.name === selectedArtist);
  const heroArt = albums[0]?.artworkUrl || currentSummary?.artworkUrl;

  return (
    <div className="flex h-full min-h-0">
      {/* Left Artist Rail (full width on mobile until artist selected; fixed 288px on md+) */}
      <div
        className={cn(
          "flex flex-col border-r border-white/[0.06] bg-[#0b0b0e]/60 md:w-72 lg:w-80 md:shrink-0",
          mobileDetailOpen ? "hidden md:flex" : "flex w-full",
        )}
      >
        <div className="border-b border-white/[0.06] p-4 space-y-3 shrink-0">
          <div className="flex items-baseline justify-between">
            <h1 className="text-xl font-bold tracking-tight text-white">Artists</h1>
            <span className="text-xs font-mono text-muted-foreground">{artists.length}</span>
          </div>
          <div className="relative">
            <Search className="pointer-events-none absolute left-3 top-1/2 h-3.5 w-3.5 -translate-y-1/2 text-muted-foreground" />
            <Input
              type="search"
              placeholder="Filter artists…"
              value={query}
              onChange={(e) => setQuery(e.target.value)}
              className="pl-8.5 h-8.5 text-xs"
            />
          </div>
        </div>

        <div className="flex-1 overflow-y-auto p-2 space-y-0.5">
          {loadingArtists && artists.length === 0 ? (
            <div className="space-y-2 p-2">
              {Array.from({ length: 10 }).map((_, i) => (
                <div key={i} className="flex items-center gap-3 animate-pulse py-1.5">
                  <div className="h-10 w-10 rounded-full bg-white/[0.06]" />
                  <div className="space-y-1.5 flex-1">
                    <div className="h-3.5 w-2/3 rounded bg-white/[0.06]" />
                    <div className="h-2.5 w-1/3 rounded bg-white/[0.04]" />
                  </div>
                </div>
              ))}
            </div>
          ) : artists.length === 0 ? (
            <p className="py-12 text-center text-xs text-muted-foreground">No matching artists.</p>
          ) : (
            artists.map((artist) => {
              const active = artist.name === selectedArtist;
              return (
                <button
                  key={artist.name}
                  type="button"
                  onClick={() => {
                    setSelectedArtist(artist.name);
                    setMobileDetailOpen(true);
                  }}
                  className={cn(
                    "flex w-full items-center gap-3 rounded-xl px-2.5 py-2 text-left transition-colors cursor-pointer",
                    active ? "md:bg-white/[0.1] md:text-white text-foreground/90" : "text-foreground/85 hover:bg-white/[0.04]",
                  )}
                >
                  <AlbumArt
                    artworkUrl={artist.artworkUrl}
                    alt={artist.name}
                    className="h-10 w-10 shrink-0 rounded-full ring-1 ring-white/10"
                  />
                  <div className="min-w-0 flex-1">
                    <div className="truncate text-[13.5px] font-medium leading-tight">{artist.name}</div>
                    <div className="truncate text-xs text-muted-foreground mt-0.5">
                      {artist.albumCount} {artist.albumCount === 1 ? "album" : "albums"} · {artist.trackCount} tracks
                    </div>
                  </div>
                  <ChevronRight className="h-4 w-4 text-muted-foreground/50 md:hidden shrink-0" />
                </button>
              );
            })
          )}
        </div>
      </div>

      {/* Right Stacked Albums Showcase */}
      <div
        className={cn(
          "relative flex-1 min-w-0 overflow-y-auto",
          mobileDetailOpen ? "block" : "hidden md:block",
        )}
      >
        {selectedArtist ? (
          <div className="relative min-h-full pb-12">
            <div className="pointer-events-none absolute inset-x-0 top-0 h-72 overflow-hidden">
              <AmbientBackdrop artworkUrl={heroArt} intensity="subtle" />
            </div>

            {/* Mobile back bar */}
            <div className="sticky top-0 z-20 flex items-center gap-2 border-b border-white/[0.06] bg-[#09090b]/70 px-4 py-3 backdrop-blur-xl md:hidden">
              <Button
                type="button"
                variant="ghost"
                size="sm"
                className="-ml-2"
                onClick={() => setMobileDetailOpen(false)}
              >
                <ArrowLeft className="h-4 w-4" />
                Artists
              </Button>
            </div>

            <div className="relative z-10 mx-auto max-w-4xl px-4 sm:px-8 pt-6 sm:pt-8">
              <div className="flex items-end gap-4 pb-6 border-b border-white/[0.07]">
                <AlbumArt
                  artworkUrl={heroArt}
                  alt={selectedArtist}
                  className="h-16 w-16 sm:h-20 sm:w-20 shrink-0 rounded-full shadow-xl ring-1 ring-white/15"
                  priority
                />
                <div className="min-w-0">
                  <div className="text-[11px] font-semibold uppercase tracking-[0.14em] text-muted-foreground">
                    Artist
                  </div>
                  <h2 className="truncate text-2xl sm:text-3xl font-bold tracking-tight text-white">
                    {selectedArtist}
                  </h2>
                  {currentSummary && (
                    <p className="text-xs text-muted-foreground mt-1">
                      {currentSummary.albumCount} {currentSummary.albumCount === 1 ? "album" : "albums"} ·{" "}
                      {currentSummary.trackCount} tracks
                    </p>
                  )}
                </div>
              </div>

              {errMsg && (
                <div role="alert" className="mt-4 rounded-xl border border-red-500/30 bg-red-950/50 px-4 py-3 text-sm text-red-200">
                  {errMsg}
                </div>
              )}

              {/* Stacked Album Cards (Roon / Plexamp discography feel) */}
              <div className="mt-6 space-y-4">
                {loadingAlbums && albums.length === 0 ? (
                  Array.from({ length: 3 }).map((_, i) => (
                    <div
                      key={i}
                      className="flex items-center gap-5 rounded-2xl border border-white/[0.06] bg-white/[0.02] p-4 animate-pulse"
                    >
                      <div className="h-28 w-28 rounded-xl bg-white/[0.06] shrink-0" />
                      <div className="space-y-2 flex-1">
                        <div className="h-5 w-1/2 rounded bg-white/[0.06]" />
                        <div className="h-3.5 w-1/3 rounded bg-white/[0.04]" />
                      </div>
                    </div>
                  ))
                ) : (
                  albums.map((alb) => {
                    const qLabel = fmtAlbumQuality(alb);
                    const hiRes = isHiRes(alb.maxBitDepth, alb.maxSampleRate);
                    const provider = (alb.provider || alb.source || "local").toLowerCase();

                    return (
                      <div
                        key={alb.id}
                        className="group flex flex-col sm:flex-row sm:items-center gap-4 sm:gap-5 rounded-2xl border border-white/[0.07] bg-white/[0.025] p-3.5 sm:p-4 transition-all hover:border-white/[0.14] hover:bg-white/[0.045]"
                      >
                        <button
                          type="button"
                          onClick={() => onOpenAlbum(alb)}
                          className="relative h-28 w-28 sm:h-32 sm:w-32 shrink-0 overflow-hidden rounded-xl shadow-lg ring-1 ring-white/10 cursor-pointer self-start"
                        >
                          <AlbumArt
                            artworkUrl={alb.artworkUrl}
                            alt={alb.title}
                            className="h-full w-full transition-transform duration-300 group-hover:scale-[1.04]"
                          />
                        </button>

                        <div className="min-w-0 flex-1 space-y-2">
                          <div className="flex flex-wrap items-center gap-2">
                            {alb.year ? (
                              <span className="font-mono text-xs text-muted-foreground">{alb.year}</span>
                            ) : null}
                            {qLabel && (
                              <Badge variant={hiRes ? "hires" : "muted"} className="text-[10.5px]">
                                {qLabel}
                              </Badge>
                            )}
                            {provider !== "local" && (
                              <Badge variant="qobuz" className="text-[10.5px] capitalize">
                                {provider}
                              </Badge>
                            )}
                            {(alb.versionCount ?? 1) > 1 && (
                              <Badge variant="muted" className="text-[10.5px]">
                                {alb.versionCount} versions
                              </Badge>
                            )}
                          </div>

                          <button
                            type="button"
                            onClick={() => onOpenAlbum(alb)}
                            className="block text-left cursor-pointer"
                          >
                            <h3 className="text-lg sm:text-xl font-semibold tracking-tight text-white group-hover:underline">
                              {alb.title}
                            </h3>
                            <p className="text-xs text-muted-foreground mt-0.5">
                              {alb.trackCount} {alb.trackCount === 1 ? "track" : "tracks"}
                              {alb.genre ? ` · ${alb.genre}` : ""}
                              {alb.playCount ? ` · ${alb.playCount} plays` : ""}
                            </p>
                          </button>

                          <div className="flex flex-wrap items-center gap-2 pt-1">
                            <Button
                              type="button"
                              size="sm"
                              className="rounded-full bg-white text-black hover:bg-white/90 h-8 px-4"
                              onClick={() => onPlay({ type: "album", albumId: alb.id }, 0)}
                            >
                              <Play className="h-3.5 w-3.5 fill-current" />
                              Play
                            </Button>
                            <Button
                              type="button"
                              variant="secondary"
                              size="sm"
                              className="rounded-full h-8 px-3.5"
                              onClick={() => onAdd({ type: "album", albumId: alb.id }, "end")}
                            >
                              <ListPlus className="h-3.5 w-3.5" />
                              Queue
                            </Button>
                            <Button
                              type="button"
                              variant="ghost"
                              size="sm"
                              className="rounded-full h-8 px-3 text-xs"
                              onClick={() => onOpenAlbum(alb)}
                            >
                              View tracks
                              <ChevronRight className="h-3.5 w-3.5" />
                            </Button>
                          </div>
                        </div>
                      </div>
                    );
                  })
                )}
              </div>
            </div>
          </div>
        ) : (
          <div className="flex h-full items-center justify-center p-8 text-sm text-muted-foreground">
            Select an artist to explore their albums.
          </div>
        )}
      </div>
    </div>
  );
}
