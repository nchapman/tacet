import { Compass, Search, Sparkles } from "lucide-react";
import { type FormEvent, useEffect, useRef, useState } from "react";
import { tacetClient } from "@/api/client";
import { fmtCount } from "@/api/format";
import type { Album, QobuzStatus, QueueSource } from "@/api/types";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { PageBody, PageHeader } from "@/components/ui/PageHeader";
import { AlbumCardSkeleton, usePagedAlbumGrid, VirtualAlbumGrid } from "@/components/VirtualAlbumGrid";

interface QobuzViewProps {
  status: QobuzStatus | null;
  onStatusChange: (status: QobuzStatus) => void;
  onOpenAlbum: (album: Album) => void;
  onPlay: (source: QueueSource, startIndex?: number) => void;
  onOpenSearch: () => void;
}

export function QobuzView({
  status,
  onStatusChange,
  onOpenAlbum,
  onPlay,
  onOpenSearch,
}: QobuzViewProps) {
  const [authToken, setAuthToken] = useState("");
  const [appId, setAppId] = useState("");
  const [appSecret, setAppSecret] = useState("");
  const [showTokenForm, setShowTokenForm] = useState(false);
  const [loggingIn, setLoggingIn] = useState(false);
  const [loginErr, setLoginErr] = useState("");

  const scrollRef = useRef<HTMLDivElement>(null);
  const gridRef = useRef<HTMLDivElement>(null);

  useEffect(() => {
    if (status !== null) return;
    const ctrl = new AbortController();
    void tacetClient
      .qobuzStatus({ signal: ctrl.signal })
      .then(onStatusChange)
      .catch((err) => {
        if (!ctrl.signal.aborted) setLoginErr(err instanceof Error ? err.message : String(err));
      });
    return () => ctrl.abort();
  }, [status, onStatusChange]);

  const { total, initialLoading, errMsg: gridErr, win, pagesByIndex } = usePagedAlbumGrid({
    scrollRef,
    gridRef,
    queryParams: { provider: "qobuz", sort: "added" },
    enabled: Boolean(status?.authenticated),
  });

  const errMsg = loginErr || gridErr;

  const handleTokenLogin = async (e: FormEvent) => {
    e.preventDefault();
    setLoggingIn(true);
    setLoginErr("");
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
      setLoginErr(err instanceof Error ? err.message : String(err));
    } finally {
      setLoggingIn(false);
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
      <div data-scroll-stage="" className="flex h-full overflow-y-auto items-center justify-center p-6">
        <div className="w-full max-w-md rounded-2xl border border-white/[0.09] bg-white/[0.03] p-6 sm:p-8 shadow-2xl space-y-5">
          <div className="flex items-center gap-3">
            <span className="flex h-11 w-11 items-center justify-center rounded-xl bg-sky-500/15 border border-sky-400/25 text-sky-300">
              <Sparkles className="h-5 w-5" />
            </span>
            <div>
              <h1 className="text-lg font-bold tracking-tight text-white">Connect Qobuz</h1>
              <p className="text-xs text-muted-foreground">Stream up to 24-bit / 192 kHz FLAC bit-perfect</p>
            </div>
          </div>

          <p className="text-sm text-muted-foreground leading-relaxed">
            Sign in with your Qobuz account to sync your favorites into your unified library, search Qobuz alongside
            local files, and browse full artist discographies.
          </p>

          {errMsg && (
            <div role="alert" className="rounded-xl border border-red-500/30 bg-red-950/50 px-3.5 py-2.5 text-xs text-red-200">
              {errMsg}
            </div>
          )}

          <div className="space-y-3">
            <Button
              type="button"
              className="w-full rounded-full bg-white text-black hover:bg-white/90"
              onClick={() => {
                window.location.href = tacetClient.resolveUrl("/api/v1/qobuz/oauth/start");
              }}
            >
              Sign in with Qobuz
            </Button>

            <button
              type="button"
              onClick={() => setShowTokenForm((v) => !v)}
              className="w-full text-center text-xs text-muted-foreground hover:text-foreground cursor-pointer py-1"
            >
              {showTokenForm ? "Hide token sign-in" : "Or sign in with an auth token"}
            </button>
          </div>

          {showTokenForm && (
            <form onSubmit={handleTokenLogin} className="space-y-2.5 pt-2 border-t border-white/[0.06]">
              <Input
                type="password"
                placeholder="Qobuz user_auth_token"
                value={authToken}
                onChange={(e) => setAuthToken(e.target.value)}
                required
              />
              <div className="grid grid-cols-2 gap-2">
                <Input placeholder="Optional App ID" value={appId} onChange={(e) => setAppId(e.target.value)} />
                <Input
                  type="password"
                  placeholder="Optional Secret"
                  value={appSecret}
                  onChange={(e) => setAppSecret(e.target.value)}
                />
              </div>
              <Button type="submit" variant="secondary" className="w-full" disabled={loggingIn}>
                {loggingIn ? "Connecting…" : "Connect Token"}
              </Button>
            </form>
          )}
        </div>
      </div>
    );
  }

  return (
    <div className="flex h-full min-h-0 flex-col">
      <PageHeader className="py-3">
        <div className="mx-auto w-full max-w-7xl flex flex-wrap items-center justify-between gap-3">
          <div>
            <h1 className="text-xl sm:text-2xl font-bold tracking-tight text-white">Qobuz</h1>
            <p className="text-xs text-muted-foreground mt-0.5">
              {status.userName || "Connected"}
              {status.subscription ? ` · ${status.subscription}` : ""}
            </p>
          </div>
          <Button type="button" variant="secondary" size="sm" className="rounded-full" onClick={onOpenSearch}>
            <Search className="h-3.5 w-3.5" />
            Search Qobuz
          </Button>
        </div>
      </PageHeader>

      <PageBody scrollRef={scrollRef}>
        <div className="mx-auto max-w-7xl px-4 sm:px-8 pt-6 space-y-6">
          {errMsg && (
            <div role="alert" className="rounded-xl border border-red-500/30 bg-red-950/50 px-4 py-3 text-sm text-red-200">
              {errMsg}
            </div>
          )}

          {total > 0 && (
            <div className="flex items-baseline gap-2">
              <h2 className="text-base font-semibold tracking-tight text-white">Synced Qobuz Favorites</h2>
              <span className="text-xs font-mono text-muted-foreground">{fmtCount(total)}</span>
            </div>
          )}

          <div ref={gridRef}>
            {initialLoading && total === 0 ? (
              <div
                className="grid"
                style={{
                  gridTemplateColumns: `repeat(${win.columns}, minmax(0, 1fr))`,
                  columnGap: "20px",
                  rowGap: "28px",
                }}
              >
                {Array.from({ length: 12 }).map((_, i) => (
                  <AlbumCardSkeleton key={i} />
                ))}
              </div>
            ) : total > 0 ? (
              <VirtualAlbumGrid
                win={win}
                pagesByIndex={pagesByIndex}
                onOpenAlbum={onOpenAlbum}
                onPlay={onPlay}
              />
            ) : (
              <div className="flex flex-col items-center justify-center py-20 text-center">
                <Compass className="h-10 w-10 text-white/20 mb-3" />
                <p className="text-sm font-medium text-foreground/85">Explore the Qobuz catalog</p>
                <p className="mt-1 text-xs text-muted-foreground max-w-sm">
                  Use Global Search to find any artist, album, or track on Qobuz alongside your local library.
                </p>
                <Button type="button" variant="secondary" size="sm" className="mt-4 rounded-full" onClick={onOpenSearch}>
                  <Search className="h-3.5 w-3.5" />
                  Open Search
                </Button>
              </div>
            )}
          </div>
        </div>
      </PageBody>
    </div>
  );
}
