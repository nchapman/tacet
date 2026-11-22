import { keepPreviousData, useQuery } from "@tanstack/react-query";
import { Disc3, ListPlus, Loader2, Play, Search, SkipForward, Sparkles, UserRound, X } from "lucide-react";
import { useEffect, useMemo, useRef } from "react";
import { tacetClient } from "@/api/client";
import { fmtAlbumQuality, fmtDuration, fmtQuality, isHiRes } from "@/api/format";
import type {
  Album,
  ArtistSummary,
  LibrarySearchResult,
  QobuzArtist,
  QobuzSearchResult,
  QobuzStatus,
  QueuePosition,
  QueueSource,
  Track,
} from "@/api/types";
import { AlbumCard } from "@/components/AlbumCard";
import { AlbumArt } from "@/components/ui/artwork";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import {
  QOBUZ_SEARCH_GC_TIME_MS,
  QOBUZ_SEARCH_STALE_TIME_MS,
  queryErrorMessage,
  queryKeys,
  useDebouncedValue,
} from "@/lib/query";
import { cn } from "@/lib/utils";

interface SearchViewProps {
  query: string;
  onQueryChange: (q: string) => void;
  focusNonce: number;
  qobuzStatus: QobuzStatus | null;
  currentTrackId?: string;
  onOpenAlbum: (album: Album) => void;
  onOpenArtist: (name: string, qobuzArtistId?: number) => void;
  onPlay: (source: QueueSource, startIndex?: number) => void;
  onAdd: (source: QueueSource, position?: QueuePosition) => void;
}

function normKey(s: string): string {
  return s
    .toLowerCase()
    .replace(/[^a-z0-9]+/g, " ")
    .trim();
}

export function SearchView({
  query,
  onQueryChange,
  focusNonce,
  qobuzStatus,
  currentTrackId,
  onOpenAlbum,
  onOpenArtist,
  onPlay,
  onAdd,
}: SearchViewProps) {
  const inputRef = useRef<HTMLInputElement | null>(null);

  const trimmed = query.trim();
  const qobuzConnected = Boolean(qobuzStatus?.authenticated);

  // Auto-focus search input when opening Search or hitting Cmd+K / '/'.
  useEffect(() => {
    inputRef.current?.focus();
    inputRef.current?.select();
  }, [focusNonce]);

  const debouncedLib = useDebouncedValue(trimmed, 60);
  const debouncedQobuz = useDebouncedValue(trimmed, 280);

  // Tier 1: Fast SQLite FTS5 local+synced library search (instant on cache hit, 60ms debounce while typing).
  const libQuery = useQuery<LibrarySearchResult>({
    queryKey: queryKeys.library.search(debouncedLib),
    queryFn: ({ signal }) => tacetClient.searchLibrary(debouncedLib, 8, { signal }),
    enabled: Boolean(debouncedLib),
    placeholderData: keepPreviousData,
  });

  // Tier 2: Progressive Qobuz remote catalog search below library results (cached 15 min, 280ms debounce while typing).
  const qobuzQuery = useQuery<QobuzSearchResult>({
    queryKey: queryKeys.qobuz.search(debouncedQobuz),
    queryFn: ({ signal }) => tacetClient.qobuzSearch(debouncedQobuz, { limit: 12 }, { signal }),
    enabled: Boolean(debouncedQobuz && qobuzConnected),
    staleTime: QOBUZ_SEARCH_STALE_TIME_MS,
    gcTime: QOBUZ_SEARCH_GC_TIME_MS,
    placeholderData: keepPreviousData,
  });

  const normalizedTrimmed = trimmed.toLowerCase();
  const libRes = trimmed ? (libQuery.data ?? null) : null;
  const libLoading = Boolean(trimmed) && (debouncedLib.toLowerCase() !== normalizedTrimmed || libQuery.isFetching);
  const libErr = trimmed ? queryErrorMessage(libQuery.error) : "";

  const qobuzRes = trimmed && qobuzConnected ? (qobuzQuery.data ?? null) : null;
  const qobuzLoading =
    Boolean(trimmed && qobuzConnected) && (debouncedQobuz.toLowerCase() !== normalizedTrimmed || qobuzQuery.isFetching);
  const qobuzErr = trimmed && qobuzConnected ? queryErrorMessage(qobuzQuery.error) : "";

  // Deduplicate Qobuz items that already appeared in the Library section above.
  const dedupedQobuz = useMemo(() => {
    if (!qobuzRes) return null;
    const libArtistNames = new Set((libRes?.artists ?? []).map((a) => normKey(a.name)));
    const libAlbumIds = new Set((libRes?.albums ?? []).map((a) => a.id));
    const libAlbumKeys = new Set((libRes?.albums ?? []).map((a) => `${normKey(a.artist)}::${normKey(a.title)}`));
    const libTrackIds = new Set((libRes?.tracks ?? []).map((t) => t.id));
    const libTrackKeys = new Set((libRes?.tracks ?? []).map((t) => `${normKey(t.artist)}::${normKey(t.title)}`));

    const artists = (qobuzRes.artists ?? []).filter((a) => !libArtistNames.has(normKey(a.name)));
    const albums = (qobuzRes.albums ?? []).filter(
      (a) => !libAlbumIds.has(a.id) && !libAlbumKeys.has(`${normKey(a.artist)}::${normKey(a.title)}`),
    );
    const tracks = (qobuzRes.tracks ?? []).filter(
      (t) => !libTrackIds.has(t.id) && !libTrackKeys.has(`${normKey(t.artist)}::${normKey(t.title)}`),
    );
    return { artists, albums, tracks };
  }, [qobuzRes, libRes]);

  const visibleLibArtists = useMemo(() => {
    const list = libRes?.artists ?? [];
    if (libRes?.topResult?.type === "artist" && list.length === 1) return [];
    return list;
  }, [libRes]);

  const visibleLibAlbums = useMemo(() => {
    const list = libRes?.albums ?? [];
    if (libRes?.topResult?.type === "album" && list.length === 1) return [];
    return list;
  }, [libRes]);

  const hasLibMatches =
    Boolean(libRes?.topResult) ||
    (libRes?.artists.length ?? 0) > 0 ||
    (libRes?.albums.length ?? 0) > 0 ||
    (libRes?.tracks.length ?? 0) > 0;

  const hasQobuzMatches =
    Boolean(dedupedQobuz) &&
    (dedupedQobuz!.artists.length > 0 || dedupedQobuz!.albums.length > 0 || dedupedQobuz!.tracks.length > 0);

  return (
    <div className="relative flex min-h-full flex-col">
      {/* Spotify-style large centered search bar pinned at top */}
      <div className="sticky top-0 z-20 border-b border-white/[0.06] bg-[#09090b]/45 px-4 sm:px-8 py-4 backdrop-blur-2xl">
        <div className="mx-auto max-w-2xl">
          <div className="relative flex items-center">
            <Search className="pointer-events-none absolute left-4 h-4.5 w-4.5 text-muted-foreground" />
            <input
              ref={inputRef}
              type="text"
              value={query}
              onChange={(e) => onQueryChange(e.target.value)}
              onKeyDown={(e) => {
                if (e.key === "Escape" && query) {
                  e.preventDefault();
                  onQueryChange("");
                }
              }}
              placeholder={
                qobuzConnected
                  ? "What do you want to listen to? (Library + Qobuz)"
                  : "What do you want to listen to?"
              }
              aria-label="Global search"
              className="h-12 w-full rounded-full border border-white/[0.12] bg-white/[0.06] pl-11 pr-12 text-[15px] text-foreground placeholder:text-muted-foreground/75 transition-colors focus:border-white/25 focus:bg-white/[0.08] focus:outline-none"
            />
            {query ? (
              <button
                type="button"
                onClick={() => {
                  onQueryChange("");
                  inputRef.current?.focus();
                }}
                aria-label="Clear search"
                className="absolute right-3 flex h-7 w-7 items-center justify-center rounded-full text-muted-foreground hover:bg-white/10 hover:text-foreground cursor-pointer"
              >
                <X className="h-4 w-4" />
              </button>
            ) : (
              <kbd className="pointer-events-none absolute right-4 hidden sm:inline-flex items-center font-mono text-[11px] text-muted-foreground/35">
                ⌘K
              </kbd>
            )}
          </div>
        </div>
      </div>

      {/* Results body */}
      {!trimmed ? (
        <div className="relative z-10 flex flex-1 flex-col items-center justify-center px-4 sm:px-8 py-12 pb-16 text-center">
          <div className="mb-4 flex h-14 w-14 items-center justify-center rounded-2xl border border-white/10 bg-white/[0.04] text-muted-foreground">
            <Search className="h-6 w-6" />
          </div>
          <h2 className="text-lg font-semibold tracking-tight text-white">Find anything in your music</h2>
          <p className="mt-1.5 max-w-md text-sm text-muted-foreground">
            Search by artist, album, or song. Results from your library appear instantly on top
            {qobuzConnected ? ", followed by Qobuz catalog results below." : "."}
          </p>
        </div>
      ) : (
        <div className="relative z-10 mx-auto w-full max-w-6xl px-4 sm:px-8 py-6 pb-12 space-y-8">
            {/* TIER 1: IN YOUR LIBRARY */}
            <section aria-label="Library results" className="space-y-6">
              <div className="flex items-center gap-2">
                <h2 className="text-xs font-semibold uppercase tracking-wider text-muted-foreground">
                  In Your Library
                </h2>
                {libLoading && <Loader2 className="h-3.5 w-3.5 animate-spin text-muted-foreground" />}
              </div>

              {libErr && (
                <div role="alert" className="rounded-xl border border-red-500/30 bg-red-950/40 px-4 py-3 text-sm text-red-200">
                  {libErr}
                </div>
              )}

              {!libErr && libRes && !hasLibMatches && (
                <p className="rounded-xl border border-white/[0.06] bg-white/[0.02] px-4 py-3 text-sm text-muted-foreground">
                  No matches in your library for <span className="text-foreground">“{libRes.query || trimmed}”</span>.
                </p>
              )}

              {hasLibMatches && libRes && (
                <div className="space-y-7">
                  {/* Top Result + Top Songs split */}
                  <div className="grid grid-cols-1 gap-5 lg:grid-cols-12">
                    {libRes.topResult && (
                      <div className={cn(libRes.tracks.length > 0 ? "lg:col-span-5" : "lg:col-span-6")}>
                        <div className="mb-2.5 text-sm font-semibold text-white">Top Result</div>
                        <TopResultCard
                          result={libRes.topResult}
                          onOpenAlbum={onOpenAlbum}
                          onOpenArtist={(name) => onOpenArtist(name)}
                          onPlay={onPlay}
                          onAdd={onAdd}
                        />
                      </div>
                    )}

                    {libRes.tracks.length > 0 && (
                      <div className={cn(libRes.topResult ? "lg:col-span-7" : "lg:col-span-12")}>
                        <div className="mb-2.5 text-sm font-semibold text-white">Songs</div>
                        <div className="space-y-0.5">
                          {libRes.tracks.slice(0, 5).map((tr) => (
                            <TrackResultRow
                              key={tr.id}
                              track={tr}
                              active={tr.id === currentTrackId}
                              onPlay={() => onPlay({ type: "tracks", trackIds: [tr.id] }, 0)}
                              onPlayNext={() => onAdd({ type: "tracks", trackIds: [tr.id] }, "next")}
                              onQueue={() => onAdd({ type: "tracks", trackIds: [tr.id] }, "end")}
                              onOpenArtist={() => onOpenArtist(tr.albumArtist || tr.artist)}
                            />
                          ))}
                        </div>
                      </div>
                    )}
                  </div>

                  {/* Library Albums */}
                  {visibleLibAlbums.length > 0 && (
                    <div className="space-y-3">
                      <h3 className="text-sm font-semibold text-white">Albums</h3>
                      <div className="grid grid-cols-2 gap-x-5 gap-y-7 sm:grid-cols-3 md:grid-cols-4 lg:grid-cols-6">
                        {visibleLibAlbums.slice(0, 6).map((alb) => (
                          <AlbumCard key={alb.id} album={alb} onOpen={onOpenAlbum} onPlay={onPlay} />
                        ))}
                      </div>
                    </div>
                  )}

                  {/* Library Artists */}
                  {visibleLibArtists.length > 0 && (
                    <div className="space-y-3">
                      <h3 className="text-sm font-semibold text-white">Artists</h3>
                      <div className="grid grid-cols-2 gap-3 sm:grid-cols-3 md:grid-cols-4 lg:grid-cols-6">
                        {visibleLibArtists.slice(0, 6).map((artist) => (
                          <ArtistResultCard
                            key={artist.name}
                            name={artist.name}
                            subtitle={`${artist.albumCount} ${artist.albumCount === 1 ? "album" : "albums"}`}
                            artworkUrl={artist.artworkUrl}
                            onClick={() => onOpenArtist(artist.name)}
                          />
                        ))}
                      </div>
                    </div>
                  )}
                </div>
              )}
            </section>

            {/* TIER 2: ON QOBUZ (strictly below library results so nothing above ever shifts) */}
            {qobuzConnected && (
              <section aria-label="Qobuz results" className="border-t border-white/[0.08] pt-6 space-y-6">
                <div className="flex items-center gap-2">
                  <Sparkles className="h-3.5 w-3.5 text-sky-400" />
                  <h2 className="text-xs font-semibold uppercase tracking-wider text-muted-foreground">
                    On Qobuz
                  </h2>
                  {qobuzLoading && (
                    <span className="inline-flex items-center gap-1.5 text-xs text-muted-foreground/80">
                      <Loader2 className="h-3.5 w-3.5 animate-spin text-sky-400" />
                      Searching Qobuz…
                    </span>
                  )}
                </div>

                {qobuzErr && (
                  <div role="alert" className="rounded-xl border border-red-500/30 bg-red-950/40 px-4 py-3 text-sm text-red-200">
                    {qobuzErr}
                  </div>
                )}

                {!qobuzErr && dedupedQobuz && !hasQobuzMatches && (
                  <p className="text-sm text-muted-foreground">
                    No additional results on Qobuz for “{debouncedQobuz || trimmed}”.
                  </p>
                )}

                {hasQobuzMatches && dedupedQobuz && (
                  <div className="space-y-7">
                    {dedupedQobuz.artists.length > 0 && (
                      <div className="space-y-3">
                        <h3 className="text-sm font-semibold text-white">Artists</h3>
                        <div className="grid grid-cols-2 gap-3 sm:grid-cols-3 md:grid-cols-4 lg:grid-cols-6">
                          {dedupedQobuz.artists.slice(0, 6).map((artist: QobuzArtist) => (
                            <ArtistResultCard
                              key={artist.id}
                              name={artist.name}
                              subtitle={
                                artist.albumsCount
                                  ? `${artist.albumsCount} ${artist.albumsCount === 1 ? "release" : "releases"}`
                                  : "Qobuz Artist"
                              }
                              artworkUrl={artist.artworkUrl}
                              remote
                              onClick={() => onOpenArtist(artist.name, artist.id)}
                            />
                          ))}
                        </div>
                      </div>
                    )}

                    {dedupedQobuz.albums.length > 0 && (
                      <div className="space-y-3">
                        <h3 className="text-sm font-semibold text-white">Albums</h3>
                        <div className="grid grid-cols-2 gap-x-5 gap-y-7 sm:grid-cols-3 md:grid-cols-4 lg:grid-cols-6">
                          {dedupedQobuz.albums.slice(0, 12).map((alb) => (
                            <AlbumCard
                              key={alb.id}
                              album={alb}
                              showInLibraryBadge
                              onOpen={onOpenAlbum}
                              onPlay={onPlay}
                            />
                          ))}
                        </div>
                      </div>
                    )}

                    {dedupedQobuz.tracks.length > 0 && (
                      <div className="space-y-3">
                        <h3 className="text-sm font-semibold text-white">Songs</h3>
                        <div className="space-y-0.5">
                          {dedupedQobuz.tracks.slice(0, 8).map((tr) => (
                            <TrackResultRow
                              key={tr.id}
                              track={tr}
                              active={tr.id === currentTrackId}
                              onPlay={() => onPlay({ type: "tracks", trackIds: [tr.id] }, 0)}
                              onPlayNext={() => onAdd({ type: "tracks", trackIds: [tr.id] }, "next")}
                              onQueue={() => onAdd({ type: "tracks", trackIds: [tr.id] }, "end")}
                              onOpenArtist={() => onOpenArtist(tr.albumArtist || tr.artist)}
                            />
                          ))}
                        </div>
                      </div>
                    )}
                  </div>
                )}
              </section>
            )}
        </div>
      )}
    </div>
  );
}

function TopResultCard({
  result,
  onOpenAlbum,
  onOpenArtist,
  onPlay,
  onAdd,
}: {
  result: NonNullable<LibrarySearchResult["topResult"]>;
  onOpenAlbum: (album: Album) => void;
  onOpenArtist: (name: string) => void;
  onPlay: (source: QueueSource, startIndex?: number) => void;
  onAdd: (source: QueueSource, position?: QueuePosition) => void;
}) {
  if (result.type === "artist" && result.artist) {
    const a: ArtistSummary = result.artist;
    return (
      <div
        role="button"
        tabIndex={0}
        onClick={() => onOpenArtist(a.name)}
        onKeyDown={(e) => {
          if (e.key === "Enter" || e.key === " ") {
            e.preventDefault();
            onOpenArtist(a.name);
          }
        }}
        className="group relative flex h-[216px] flex-col justify-between rounded-2xl border border-white/[0.08] bg-white/[0.04] p-5 transition-colors hover:bg-white/[0.07] cursor-pointer"
      >
        <div className="h-20 w-20 overflow-hidden rounded-full border border-white/10 bg-zinc-900 shadow-lg">
          {a.artworkUrl ? (
            <AlbumArt artworkUrl={a.artworkUrl} alt={a.name} className="h-full w-full" />
          ) : (
            <div className="flex h-full w-full items-center justify-center text-muted-foreground">
              <UserRound className="h-8 w-8" />
            </div>
          )}
        </div>
        <div className="min-w-0">
          <div className="truncate text-2xl font-bold tracking-tight text-white">{a.name}</div>
          <div className="mt-1 flex items-center gap-2 text-xs text-muted-foreground">
            <Badge variant="muted" className="text-[10px]">
              Artist
            </Badge>
            <span>
              {a.albumCount} {a.albumCount === 1 ? "album" : "albums"} · {a.trackCount} tracks
            </span>
          </div>
        </div>
      </div>
    );
  }

  if (result.type === "album" && result.album) {
    const alb = result.album;
    const qLabel = fmtAlbumQuality(alb);
    const hiRes = isHiRes(alb.maxBitDepth, alb.maxSampleRate);
    return (
      <div
        role="button"
        tabIndex={0}
        onClick={() => onOpenAlbum(alb)}
        onKeyDown={(e) => {
          if (e.key === "Enter" || e.key === " ") {
            e.preventDefault();
            onOpenAlbum(alb);
          }
        }}
        className="group relative flex h-[216px] flex-col justify-between rounded-2xl border border-white/[0.08] bg-white/[0.04] p-5 transition-colors hover:bg-white/[0.07] cursor-pointer"
      >
        <div className="flex items-start justify-between gap-3">
          <AlbumArt artworkUrl={alb.artworkUrl} alt={alb.title} className="h-20 w-20 rounded-xl shadow-lg" />
          <div className="flex items-center gap-1.5">
            <Button
              size="icon"
              variant="secondary"
              aria-label={`Queue ${alb.title}`}
              onClick={(e) => {
                e.stopPropagation();
                onAdd({ type: "album", albumId: alb.id }, "end");
              }}
            >
              <ListPlus className="h-4 w-4" />
            </Button>
            <Button
              size="icon"
              aria-label={`Play ${alb.title}`}
              onClick={(e) => {
                e.stopPropagation();
                onPlay({ type: "album", albumId: alb.id }, 0);
              }}
              className="rounded-full bg-white text-black hover:bg-white/90"
            >
              <Play className="h-4 w-4 fill-current ml-0.5" />
            </Button>
          </div>
        </div>
        <div className="min-w-0">
          <div className="truncate text-2xl font-bold tracking-tight text-white">{alb.title}</div>
          <div className="mt-1 flex flex-wrap items-center gap-2 text-xs text-muted-foreground">
            <Badge variant="muted" className="text-[10px]">
              Album
            </Badge>
            <span className="truncate text-foreground/85">{alb.artist}</span>
            {alb.year ? <span>· {alb.year}</span> : null}
            {qLabel && (
              <Badge variant={hiRes ? "hires" : "muted"} className="text-[10px]">
                {qLabel}
              </Badge>
            )}
          </div>
        </div>
      </div>
    );
  }

  if (result.type === "track" && result.track) {
    const tr = result.track;
    const qLabel = fmtQuality(tr);
    const hiRes = isHiRes(tr.bitDepth, tr.sampleRate);
    return (
      <div className="group relative flex h-[216px] flex-col justify-between rounded-2xl border border-white/[0.08] bg-white/[0.04] p-5 transition-colors hover:bg-white/[0.07]">
        <div className="flex items-start justify-between gap-3">
          <AlbumArt artworkUrl={tr.artworkUrl} alt={tr.album || tr.title} className="h-20 w-20 rounded-xl shadow-lg" />
          <div className="flex items-center gap-1.5">
            <Button
              size="icon"
              variant="secondary"
              title="Play Next"
              aria-label={`Play ${tr.title} next`}
              onClick={() => onAdd({ type: "tracks", trackIds: [tr.id] }, "next")}
            >
              <SkipForward className="h-4 w-4" />
            </Button>
            <Button
              size="icon"
              aria-label={`Play ${tr.title}`}
              onClick={() => onPlay({ type: "tracks", trackIds: [tr.id] }, 0)}
              className="rounded-full bg-white text-black hover:bg-white/90"
            >
              <Play className="h-4 w-4 fill-current ml-0.5" />
            </Button>
          </div>
        </div>
        <div className="min-w-0">
          <div className="truncate text-2xl font-bold tracking-tight text-white">{tr.title}</div>
          <div className="mt-1 flex flex-wrap items-center gap-2 text-xs text-muted-foreground">
            <Badge variant="muted" className="text-[10px]">
              Song
            </Badge>
            <button
              type="button"
              onClick={() => onOpenArtist(tr.albumArtist || tr.artist)}
              className="truncate text-foreground/85 hover:underline cursor-pointer"
            >
              {tr.artist}
            </button>
            {tr.album && <span className="truncate">· {tr.album}</span>}
            {qLabel && (
              <Badge variant={hiRes ? "hires" : "muted"} className="text-[10px]">
                {qLabel}
              </Badge>
            )}
          </div>
        </div>
      </div>
    );
  }

  return null;
}

function ArtistResultCard({
  name,
  subtitle,
  artworkUrl,
  remote,
  onClick,
}: {
  name: string;
  subtitle: string;
  artworkUrl?: string;
  remote?: boolean;
  onClick: () => void;
}) {
  return (
    <button
      type="button"
      onClick={onClick}
      className="group flex flex-col items-center rounded-xl border border-white/[0.06] bg-white/[0.03] p-4 text-center transition-colors hover:bg-white/[0.07] cursor-pointer"
    >
      <div className="h-24 w-24 overflow-hidden rounded-full border border-white/10 bg-zinc-900 shadow-md transition-transform duration-300 group-hover:scale-[1.03]">
        {artworkUrl ? (
          <AlbumArt artworkUrl={artworkUrl} alt={name} className="h-full w-full" />
        ) : (
          <div className="flex h-full w-full items-center justify-center text-muted-foreground/70">
            <Disc3 className="h-8 w-8" />
          </div>
        )}
      </div>
      <div className="mt-3 w-full truncate text-sm font-medium text-white">{name}</div>
      <div className="mt-0.5 flex items-center justify-center gap-1.5 text-xs text-muted-foreground">
        <span className="truncate">{subtitle}</span>
        {remote && (
          <Badge variant="qobuz" className="px-1.5 py-0 text-[9px]">
            Qobuz
          </Badge>
        )}
      </div>
    </button>
  );
}

function TrackResultRow({
  track,
  active,
  onPlay,
  onPlayNext,
  onQueue,
  onOpenArtist,
}: {
  track: Track;
  active: boolean;
  onPlay: () => void;
  onPlayNext: () => void;
  onQueue: () => void;
  onOpenArtist: () => void;
}) {
  const qual = fmtQuality(track);
  const hiRes = isHiRes(track.bitDepth, track.sampleRate);
  return (
    <div
      className={cn(
        "group flex items-center gap-3 rounded-lg px-2.5 py-1.5 transition-colors",
        active ? "bg-white/[0.08]" : "hover:bg-white/[0.05]",
      )}
    >
      <button
        type="button"
        onClick={onPlay}
        aria-label={`Play ${track.title}`}
        className="relative h-10 w-10 shrink-0 overflow-hidden rounded-md ring-1 ring-white/10 cursor-pointer"
      >
        <AlbumArt artworkUrl={track.artworkUrl} alt={track.album || track.title} className="h-full w-full" />
        <span className="absolute inset-0 flex items-center justify-center bg-black/50 opacity-0 transition-opacity group-hover:opacity-100">
          <Play className="h-4 w-4 fill-white text-white ml-0.5" />
        </span>
      </button>

      <div className="min-w-0 flex-1">
        <button
          type="button"
          onClick={onPlay}
          className={cn(
            "block w-full truncate text-left text-sm font-medium cursor-pointer",
            active ? "text-emerald-400" : "text-foreground",
          )}
        >
          {track.title}
        </button>
        <div className="flex items-center gap-1.5 truncate text-xs text-muted-foreground">
          <button
            type="button"
            onClick={onOpenArtist}
            className="truncate hover:text-foreground hover:underline cursor-pointer"
          >
            {track.artist}
          </button>
          {track.album && <span className="truncate text-muted-foreground/70">· {track.album}</span>}
        </div>
      </div>

      {qual && (
        <span className="hidden sm:inline-flex items-center gap-1.5 font-mono text-[11px] text-muted-foreground/75 shrink-0">
          {hiRes && <span className="h-1.5 w-1.5 shrink-0 rounded-full bg-[#d4af68]/75" title="Hi-Res Audio" />}
          {qual}
        </span>
      )}

      <div className="flex items-center gap-0.5 opacity-0 transition-opacity group-hover:opacity-100 focus-within:opacity-100">
        <button
          type="button"
          onClick={onPlayNext}
          title="Play Next"
          aria-label={`Play ${track.title} next`}
          className="flex h-7 w-7 items-center justify-center rounded-md text-muted-foreground hover:bg-white/10 hover:text-foreground cursor-pointer"
        >
          <SkipForward className="h-3.5 w-3.5" />
        </button>
        <button
          type="button"
          onClick={onQueue}
          title="Add to Queue"
          aria-label={`Queue ${track.title}`}
          className="flex h-7 w-7 items-center justify-center rounded-md text-muted-foreground hover:bg-white/10 hover:text-foreground cursor-pointer"
        >
          <ListPlus className="h-3.5 w-3.5" />
        </button>
      </div>

      <span className="w-10 text-right font-mono text-xs tabular-nums text-muted-foreground">
        {fmtDuration(track.durationSec)}
      </span>
    </div>
  );
}
