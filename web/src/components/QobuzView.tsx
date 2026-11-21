import { Compass, ListPlus, Play, Plus, Search, Sparkles } from "lucide-react";
import { type FormEvent, useEffect, useRef, useState } from "react";
import { tacetClient } from "@/api/client";
import { fmtDuration, fmtQuality, isHiRes } from "@/api/format";
import type { Album, QobuzSearchResult, QobuzStatus, QueuePosition, QueueSource } from "@/api/types";
import { AlbumCard } from "@/components/AlbumCard";
import { AlbumArt } from "@/components/ui/artwork";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { cn } from "@/lib/utils";

interface QobuzViewProps {
  status: QobuzStatus | null;
  onStatusChange: (status: QobuzStatus) => void;
  onOpenAlbum: (album: Album) => void;
  onPlay: (source: QueueSource, startIndex?: number) => void;
  onAdd: (source: QueueSource, position?: QueuePosition) => void;
}

export function QobuzView({ status, onStatusChange, onOpenAlbum, onPlay, onAdd }: QobuzViewProps) {
  const [authToken, setAuthToken] = useState("");
  const [appId, setAppId] = useState("");
  const [appSecret, setAppSecret] = useState("");
  const [showTokenForm, setShowTokenForm] = useState(false);
  const [loggingIn, setLoggingIn] = useState(false);

  const [query, setQuery] = useState("");
  const [submittedQuery, setSubmittedQuery] = useState("");
  const [results, setResults] = useState<QobuzSearchResult | null>(null);
  const [searching, setSearching] = useState(false);
  const [addedTrackIds, setAddedTrackIds] = useState<Record<string, boolean>>({});
  const [errMsg, setErrMsg] = useState("");
  const abortRef = useRef<AbortController | null>(null);

  useEffect(() => {
    if (status !== null) return;
    const ctrl = new AbortController();
    void tacetClient
      .qobuzStatus({ signal: ctrl.signal })
      .then(onStatusChange)
      .catch((err) => {
        if (!ctrl.signal.aborted) setErrMsg(err instanceof Error ? err.message : String(err));
      });
    return () => ctrl.abort();
  }, [status, onStatusChange]);

  useEffect(() => () => abortRef.current?.abort(), []);

  const handleTokenLogin = async (e: FormEvent) => {
    e.preventDefault();
    setLoggingIn(true);
    setErrMsg("");
    try {
      const st = await tacetClient.qobuzLogin({
        authToken: authToken.trim() || undefined,
        appId: appId.trim() || undefined,
        appSecret: appSecret.trim() || undefined,
      });
      onStatusChange(st);
      setAuthToken("");
      setAppId("");
      setAppSecret("");
    } catch (err) {
      setErrMsg(err instanceof Error ? err.message : String(err));
    } finally {
      setLoggingIn(false);
    }
  };

  const handleSearch = async (e: FormEvent) => {
    e.preventDefault();
    const trimmed = query.trim();
    if (!trimmed) return;
    abortRef.current?.abort();
    const ctrl = new AbortController();
    abortRef.current = ctrl;
    setSearching(true);
    setErrMsg("");
    setSubmittedQuery(trimmed);
    try {
      const raw = await tacetClient.qobuzSearch(trimmed, { limit: 30 }, { signal: ctrl.signal });
      if (ctrl.signal.aborted) return;
      setResults({
        albums: Array.isArray(raw?.albums) ? raw.albums : [],
        tracks: Array.isArray(raw?.tracks) ? raw.tracks : [],
      });
    } catch (err) {
      if (!ctrl.signal.aborted) setErrMsg(err instanceof Error ? err.message : String(err));
    } finally {
      if (!ctrl.signal.aborted) setSearching(false);
    }
  };

  const handleAddTrack = async (trackId: string, externalId?: string) => {
    setErrMsg("");
    try {
      await tacetClient.addRemoteTrack("qobuz", externalId || trackId.replace("qobuz:", ""));
      setAddedTrackIds((prev) => ({ ...prev, [trackId]: true }));
    } catch (err) {
      setErrMsg(err instanceof Error ? err.message : String(err));
    }
  };

  if (status === null) {
    return (
      <div className="flex h-full items-center justify-center p-8 text-sm text-muted-foreground">
        Checking Qobuz connection…
      </div>
    );
  }

  if (!status.authenticated) {
    return (
      <div className="flex min-h-full items-center justify-center p-6">
        <div className="w-full max-w-md rounded-2xl border border-white/[0.09] bg-white/[0.03] p-6 sm:p-8 shadow-2xl space-y-5">
          <div className="flex h-12 w-12 items-center justify-center rounded-2xl bg-sky-500/15 text-sky-300 border border-sky-400/25">
            <Sparkles className="h-6 w-6" />
          </div>
          <div>
            <h1 className="text-xl font-bold tracking-tight text-white">Explore Qobuz</h1>
            <p className="mt-1.5 text-sm text-muted-foreground leading-relaxed">
              Sign in to search the Qobuz catalog, stream bit-perfect hi-res FLAC up to 24-bit / 192 kHz, and sync your
              Qobuz Favorites directly into your unified library.
            </p>
          </div>

          {errMsg && (
            <div role="alert" className="rounded-xl border border-red-500/30 bg-red-950/50 px-3.5 py-2.5 text-xs text-red-200">
              {errMsg}
            </div>
          )}

          <div className="flex flex-col gap-2.5">
            <Button asChild size="lg" className="w-full bg-white text-black hover:bg-white/90">
              <a href={tacetClient.qobuzOAuthStartURL()}>Sign In with Qobuz</a>
            </Button>
            <Button
              type="button"
              variant="ghost"
              size="sm"
              className="w-full text-xs text-muted-foreground"
              onClick={() => setShowTokenForm((v) => !v)}
            >
              {showTokenForm ? "Hide token login" : "Use auth token instead"}
            </Button>
          </div>

          {showTokenForm && (
            <form onSubmit={handleTokenLogin} className="space-y-3 border-t border-white/[0.07] pt-4">
              <Input
                type="password"
                autoComplete="off"
                placeholder="Qobuz user_auth_token"
                value={authToken}
                onChange={(e) => setAuthToken(e.target.value)}
                required
              />
              <div className="grid grid-cols-2 gap-2">
                <Input placeholder="Optional App ID" value={appId} onChange={(e) => setAppId(e.target.value)} />
                <Input
                  type="password"
                  autoComplete="off"
                  placeholder="Optional Secret"
                  value={appSecret}
                  onChange={(e) => setAppSecret(e.target.value)}
                />
              </div>
              <Button type="submit" variant="secondary" className="w-full" disabled={loggingIn || !authToken.trim()}>
                {loggingIn ? "Verifying…" : "Connect with Token"}
              </Button>
            </form>
          )}
        </div>
      </div>
    );
  }

  return (
    <div className="min-h-full pb-12">
      <div className="sticky top-0 z-20 border-b border-white/[0.06] bg-[#09090b]/85 px-4 sm:px-8 py-4 backdrop-blur-xl">
        <div className="mx-auto max-w-7xl space-y-3">
          <div className="flex items-center justify-between gap-2">
            <h1 className="text-xl sm:text-2xl font-bold tracking-tight text-white">Qobuz</h1>
            <span className="text-xs text-muted-foreground truncate">
              {status.userName || "Connected"}
              {status.subscription ? ` · ${status.subscription}` : ""}
            </span>
          </div>

          <form onSubmit={handleSearch} className="flex items-center gap-2 max-w-xl">
            <div className="relative flex-1">
              <Search className="pointer-events-none absolute left-3 top-1/2 h-4 w-4 -translate-y-1/2 text-muted-foreground" />
              <Input
                type="search"
                placeholder="Search Qobuz artists, albums, tracks…"
                value={query}
                onChange={(e) => setQuery(e.target.value)}
                className="pl-9"
              />
            </div>
            <Button type="submit" disabled={searching || !query.trim()}>
              {searching ? "Searching…" : "Search"}
            </Button>
          </form>
        </div>
      </div>

      <div className="mx-auto max-w-7xl px-4 sm:px-8 pt-6 space-y-8">
        {errMsg && (
          <div role="alert" className="rounded-xl border border-red-500/30 bg-red-950/50 px-4 py-3 text-sm text-red-200">
            {errMsg}
          </div>
        )}

        {!results && !searching && (
          <div className="flex flex-col items-center justify-center py-20 text-center">
            <Compass className="h-10 w-10 text-zinc-700 mb-3" />
            <p className="text-sm font-medium text-foreground/85">Discover hi-res music on Qobuz</p>
            <p className="mt-1 text-xs text-muted-foreground max-w-sm">
              Any album you open plays instantly and can be added to your unified library with one click.
            </p>
          </div>
        )}

        {results && results.albums.length > 0 && (
          <section className="space-y-4">
            <h2 className="text-base font-semibold tracking-tight text-white">Albums</h2>
            <div className="grid grid-cols-2 sm:grid-cols-3 md:grid-cols-4 xl:grid-cols-6 gap-x-5 gap-y-7">
              {results.albums.map((alb) => (
                <AlbumCard
                  key={alb.id}
                  album={alb}
                  showInLibraryBadge
                  onOpen={onOpenAlbum}
                  onPlay={onPlay}
                />
              ))}
            </div>
          </section>
        )}

        {results && results.tracks.length > 0 && (
          <section className="space-y-3">
            <h2 className="text-base font-semibold tracking-tight text-white">Tracks</h2>
            <div className="divide-y divide-white/[0.03]">
              {results.tracks.map((tr) => {
                const inLib = Boolean(tr.inLibrary || addedTrackIds[tr.id]);
                const qual = fmtQuality(tr);
                const hiRes = isHiRes(tr.bitDepth, tr.sampleRate);
                return (
                  <div
                    key={tr.id}
                    className="group grid grid-cols-[1fr_auto] sm:grid-cols-[minmax(0,2fr)_auto_60px_auto] items-center gap-3 rounded-xl px-2.5 py-2 hover:bg-white/[0.05] transition-colors"
                  >
                    <div className="flex min-w-0 items-center gap-3">
                      <button
                        type="button"
                        onClick={() => onPlay({ type: "tracks", trackIds: [tr.id] }, 0)}
                        className="relative h-10 w-10 shrink-0 overflow-hidden rounded-md ring-1 ring-white/10 cursor-pointer"
                      >
                        <AlbumArt artworkUrl={tr.artworkUrl} alt={tr.album || tr.title} className="h-full w-full" />
                        <span className="absolute inset-0 flex items-center justify-center bg-black/55 opacity-0 group-hover:opacity-100 transition-opacity">
                          <Play className="h-3.5 w-3.5 fill-white text-white" />
                        </span>
                      </button>
                      <div className="min-w-0 flex-1">
                        <button
                          type="button"
                          onClick={() => onPlay({ type: "tracks", trackIds: [tr.id] }, 0)}
                          className="block max-w-full truncate text-[13.5px] font-medium text-foreground hover:text-white text-left cursor-pointer"
                        >
                          {tr.title}
                        </button>
                        <div className="truncate text-xs text-muted-foreground">
                          {tr.artist}
                          {tr.album ? ` · ${tr.album}` : ""}
                        </div>
                      </div>
                    </div>

                    <span className={cn("hidden sm:block font-mono text-[11px]", hiRes ? "text-amber-300/90" : "text-muted-foreground")}>
                      {qual}
                    </span>

                    <span className="hidden sm:block text-right font-mono text-xs tabular-nums text-muted-foreground">
                      {fmtDuration(tr.durationSec)}
                    </span>

                    <div className="flex items-center gap-1">
                      {!inLib && (
                        <Button
                          type="button"
                          variant="ghost"
                          size="icon-sm"
                          title="Add track to Library"
                          onClick={() => void handleAddTrack(tr.id, tr.externalId)}
                        >
                          <Plus className="h-3.5 w-3.5" />
                        </Button>
                      )}
                      <Button
                        type="button"
                        variant="ghost"
                        size="icon-sm"
                        title="Add to Queue"
                        onClick={() => onAdd({ type: "tracks", trackIds: [tr.id] }, "end")}
                      >
                        <ListPlus className="h-3.5 w-3.5" />
                      </Button>
                    </div>
                  </div>
                );
              })}
            </div>
          </section>
        )}

        {results && results.albums.length === 0 && results.tracks.length === 0 && (
          <p className="py-16 text-center text-sm text-muted-foreground">
            No Qobuz matches for “{submittedQuery}”.
          </p>
        )}
      </div>
    </div>
  );
}
