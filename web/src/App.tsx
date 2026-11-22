import { Volume2 } from "lucide-react";
import { useCallback, useEffect, useRef, useState } from "react";
import {
  Navigate,
  Route,
  Routes,
  useLocation,
  useNavigate,
  useParams,
  useSearchParams,
} from "react-router";
import { TacetAPIError, tacetClient } from "@/api/client";
import type { Album, QueuePosition, QueueSource } from "@/api/types";
import { AlbumView } from "@/components/AlbumView";
import { AlbumsView } from "@/components/AlbumsView";
import { ArtistsView } from "@/components/ArtistsView";
import { ConnectModal } from "@/components/ConnectModal";
import { HomeView } from "@/components/HomeView";
import { PlayerBar } from "@/components/PlayerBar";
import { QobuzView } from "@/components/QobuzView";
import { DesktopQueueDrawer, MobileQueueSheet } from "@/components/QueueDrawer";
import { SearchView } from "@/components/SearchView";
import { SettingsView } from "@/components/SettingsView";
import { type AppSection, MobileBottomNav, Sidebar } from "@/components/Sidebar";
import { SongsView } from "@/components/SongsView";
import { AmbientBackdrop } from "@/components/ui/artwork";
import { Button } from "@/components/ui/button";
import { useBrowserPlayer } from "@/hooks/useBrowserPlayer";
import { useTacet } from "@/hooks/useTacet";
import { useScrollRestoration } from "@/lib/scroll";
import { useSWR } from "@/lib/swr";
import { albumPath, artistPath, decodeAlbumKey, SECTION_LABELS, SECTION_PATHS, sectionFromPathname } from "@/routes";

interface AlbumLocationState {
  album?: Album;
  backLabel?: string;
  fromApp?: boolean;
}

export function App() {
  const browserPlayer = useBrowserPlayer();
  const tacet = useTacet(browserPlayer.zoneId);

  const location = useLocation();
  const routerNavigate = useNavigate();
  const section = sectionFromPathname(location.pathname);

  const [searchFocusNonce, setSearchFocusNonce] = useState(0);
  const [queueOpen, setQueueOpen] = useState(false);
  const [connectOpen, setConnectOpen] = useState(false);
  const [albumAmbientArt, setAlbumAmbientArt] = useState<string | undefined>(undefined);

  const stageRef = useRef<HTMLElement | null>(null);

  const activeState = tacet.activeZone?.state;
  const activeHasPlayable = Boolean(
    tacet.activeZone &&
      (tacet.activeZone.currentTrack ||
        (tacet.activeZone.queue?.length ?? 0) > 0 ||
        activeState === "playing" ||
        activeState === "paused"),
  );
  const activeTransport = tacet.transport;

  const isSplitPane = location.pathname === "/artists" || location.pathname.startsWith("/artists/");

  // Preserve & restore main-stage scroll position across Back/Forward and in-app back navigation.
  useScrollRestoration(stageRef, { namespace: "stage", enabled: !isSplitPane });

  // Clear album-page UltraBlur override when leaving an album route.
  useEffect(() => {
    if (!location.pathname.startsWith("/albums/")) {
      setAlbumAmbientArt(undefined);
    }
  }, [location.pathname]);

  const navigateSection = useCallback(
    (next: AppSection) => {
      if (next === "search") {
        setSearchFocusNonce((n) => n + 1);
      }
      const target = SECTION_PATHS[next];
      if (location.pathname !== target) {
        void routerNavigate(target);
      } else {
        stageRef.current?.scrollTo({ top: 0 });
      }
    },
    [location.pathname, routerNavigate],
  );

  // Global keyboard shortcuts: Space toggles play/pause, Cmd/Ctrl+K or '/' opens Search.
  useEffect(() => {
    const onKeyDown = (e: KeyboardEvent) => {
      if ((e.metaKey || e.ctrlKey) && e.key.toLowerCase() === "k") {
        e.preventDefault();
        navigateSection("search");
        return;
      }
      const tag = (e.target as HTMLElement | null)?.tagName;
      const inInput =
        tag === "INPUT" ||
        tag === "TEXTAREA" ||
        tag === "SELECT" ||
        (e.target as HTMLElement | null)?.isContentEditable;
      if (e.key === "/" && !e.metaKey && !e.ctrlKey && !e.altKey && !inInput) {
        e.preventDefault();
        navigateSection("search");
        return;
      }
      if (e.code !== "Space" || e.metaKey || e.ctrlKey || e.altKey) return;
      if (inInput || tag === "BUTTON" || tag === "A") return;
      if (!activeHasPlayable) return;
      e.preventDefault();
      void activeTransport(activeState === "playing" ? "pause" : "play");
    };
    window.addEventListener("keydown", onKeyDown);
    return () => window.removeEventListener("keydown", onKeyDown);
  }, [activeState, activeHasPlayable, activeTransport, navigateSection]);

  const openAlbumById = useCallback(
    (albumId: string, hint?: Album) => {
      const state: AlbumLocationState = {
        album: hint,
        backLabel: SECTION_LABELS[section],
        fromApp: true,
      };
      const meta = hint ? { artist: hint.artist, title: hint.title } : undefined;
      void routerNavigate(albumPath(albumId, meta), { state });
    },
    [routerNavigate, section],
  );

  const openArtist = useCallback(
    (artistName: string, qobuzArtistId?: number) => {
      void routerNavigate(artistPath(artistName, qobuzArtistId));
    },
    [routerNavigate],
  );

  const isAlbumPage = location.pathname.startsWith("/albums/");
  const ambientArtwork = (isAlbumPage && albumAmbientArt) || tacet.activeZone?.currentTrack?.artworkUrl;
  const ambientIntensity = isAlbumPage && albumAmbientArt ? "hero" : "subtle";

  return (
    <div className="relative flex h-dvh w-full flex-col overflow-hidden bg-[#09090b] text-foreground">
      {/* Unified full-viewport ambient artwork wash behind sidebar, stage, queue & player bar */}
      <AmbientBackdrop artworkUrl={ambientArtwork} intensity={ambientIntensity} />

      {/* Autoplay unlock banner when browser blocks unmuted Web Audio */}
      {browserPlayer.needsUnlock && (
        <div
          role="alert"
          className="relative z-10 flex shrink-0 items-center justify-between gap-3 border-b border-amber-400/25 bg-amber-400/10 px-4 py-2 text-xs text-amber-200 backdrop-blur-xl"
        >
          <span className="inline-flex items-center gap-2">
            <Volume2 className="h-3.5 w-3.5 text-amber-300" />
            Browser paused audio until you interact with this tab.
          </span>
          <Button type="button" size="sm" onClick={() => void browserPlayer.unlockAudio()}>
            Unlock Audio
          </Button>
        </div>
      )}

      {/* Middle 100dvh Workspace: Sidebar | Independently Scrollable Main Stage | Queue Drawer */}
      <div className="relative z-10 flex flex-1 min-h-0 overflow-hidden">
        <Sidebar
          activeSection={section}
          libraryStatus={tacet.libraryStatus}
          onNavigate={(next) => {
            if (next === "search") setSearchFocusNonce((n) => n + 1);
            if (location.pathname === SECTION_PATHS[next]) {
              stageRef.current?.scrollTo({ top: 0 });
            }
          }}
        />

        <main
          ref={stageRef}
          className={
            isSplitPane
              ? "flex-1 min-w-0 min-h-0 overflow-hidden"
              : "flex-1 min-w-0 min-h-0 overflow-y-auto"
          }
        >
          <Routes>
            <Route
              path="/"
              element={
                <HomeRoute
                  libraryStatus={tacet.libraryStatus}
                  onOpenAlbum={(alb) => openAlbumById(alb.id, alb)}
                  onNavigate={navigateSection}
                  onPlay={(src, startIdx) => void tacet.playSource(src, startIdx)}
                />
              }
            />
            <Route
              path="/search"
              element={
                <SearchRoute
                  focusNonce={searchFocusNonce}
                  qobuzStatus={tacet.qobuzStatus}
                  currentTrackId={tacet.activeZone?.currentTrack?.id}
                  onOpenAlbum={(alb) => openAlbumById(alb.id, alb)}
                  onOpenArtist={openArtist}
                  onPlay={(src, startIdx) => void tacet.playSource(src, startIdx)}
                  onAdd={(src, pos) => void tacet.addSource(src, pos)}
                />
              }
            />
            <Route
              path="/albums"
              element={
                <AlbumsView
                  libraryStatus={tacet.libraryStatus}
                  onOpenAlbum={(alb) => openAlbumById(alb.id, alb)}
                  onPlay={(src, startIdx) => void tacet.playSource(src, startIdx)}
                />
              }
            />
            <Route
              path="/albums/:albumSlug/:albumKey"
              element={
                <AlbumRoute
                  libraryEtag={tacet.libraryStatus?.etag}
                  currentTrackId={tacet.activeZone?.currentTrack?.id}
                  isPlaying={tacet.activeZone?.state === "playing"}
                  onAmbientArt={setAlbumAmbientArt}
                  onOpenAlbum={(id) => openAlbumById(id)}
                  onOpenArtist={openArtist}
                  onPlay={(src, startIdx) => void tacet.playSource(src, startIdx)}
                  onAdd={(src, pos) => void tacet.addSource(src, pos)}
                />
              }
            />
            <Route
              path="/albums/:albumKey"
              element={
                <AlbumRoute
                  libraryEtag={tacet.libraryStatus?.etag}
                  currentTrackId={tacet.activeZone?.currentTrack?.id}
                  isPlaying={tacet.activeZone?.state === "playing"}
                  onAmbientArt={setAlbumAmbientArt}
                  onOpenAlbum={(id) => openAlbumById(id)}
                  onOpenArtist={openArtist}
                  onPlay={(src, startIdx) => void tacet.playSource(src, startIdx)}
                  onAdd={(src, pos) => void tacet.addSource(src, pos)}
                />
              }
            />
            <Route
              path="/artists"
              element={
                <ArtistsRoute
                  libraryStatus={tacet.libraryStatus}
                  qobuzStatus={tacet.qobuzStatus}
                  onSelectArtist={(name) => openArtist(name)}
                  onBackToArtists={() => void routerNavigate("/artists")}
                  onOpenAlbum={(alb) => openAlbumById(alb.id, alb)}
                  onPlay={(src, startIdx) => void tacet.playSource(src, startIdx)}
                  onAdd={(src, pos) => void tacet.addSource(src, pos)}
                />
              }
            />
            <Route
              path="/artists/:artistName"
              element={
                <ArtistsRoute
                  libraryStatus={tacet.libraryStatus}
                  qobuzStatus={tacet.qobuzStatus}
                  onSelectArtist={(name) => openArtist(name)}
                  onBackToArtists={() => void routerNavigate("/artists")}
                  onOpenAlbum={(alb) => openAlbumById(alb.id, alb)}
                  onPlay={(src, startIdx) => void tacet.playSource(src, startIdx)}
                  onAdd={(src, pos) => void tacet.addSource(src, pos)}
                />
              }
            />
            <Route
              path="/songs"
              element={
                <SongsView
                  libraryStatus={tacet.libraryStatus}
                  currentTrackId={tacet.activeZone?.currentTrack?.id}
                  onOpenAlbumId={(id) => openAlbumById(id)}
                  onOpenArtist={openArtist}
                  onPlay={(src, startIdx) => void tacet.playSource(src, startIdx)}
                  onAdd={(src, pos) => void tacet.addSource(src, pos)}
                />
              }
            />
            <Route
              path="/qobuz"
              element={
                <QobuzView
                  status={tacet.qobuzStatus}
                  libraryStatus={tacet.libraryStatus}
                  onStatusChange={tacet.setQobuzStatus}
                  onOpenAlbum={(alb) => openAlbumById(alb.id, alb)}
                  onPlay={(src, startIdx) => void tacet.playSource(src, startIdx)}
                  onOpenSearch={() => navigateSection("search")}
                />
              }
            />
            <Route
              path="/settings"
              element={
                <SettingsView
                  libraryStatus={tacet.libraryStatus}
                  qobuzStatus={tacet.qobuzStatus}
                  streamMode={tacet.settings.streamMode}
                  zones={tacet.zones}
                  discovering={tacet.discovering}
                  onStreamModeChange={(mode) => void tacet.setStreamMode(mode)}
                  onQobuzStatusChange={tacet.setQobuzStatus}
                  onDiscover={() => void tacet.discoverZones()}
                  onOpenConnect={() => setConnectOpen(true)}
                  onGoToQobuz={() => navigateSection("qobuz")}
                />
              }
            />
            <Route path="*" element={<Navigate to="/" replace />} />
          </Routes>
        </main>

        {/* Spotify-style toggleable right-side Queue Drawer (desktop) */}
        <DesktopQueueDrawer
          open={queueOpen}
          onOpenChange={setQueueOpen}
          items={tacet.activeZone?.queue ?? []}
          currentItemId={tacet.activeZone?.currentItemId}
          onJump={(itemId) => void tacet.jumpItem(itemId)}
          onRemove={(itemId) => void tacet.removeItem(itemId)}
          onClear={() => void tacet.clearQueue()}
        />
      </div>

      {/* Spotify-style Bottom Player Bar (Desktop) & Mini-Player + Expandable Full-Screen Sheet (Mobile) */}
      <div className="relative z-10 shrink-0">
        <PlayerBar
          zones={tacet.zones}
          activeZone={tacet.activeZone}
          positionSec={tacet.interpolatedPositionSec}
          error={tacet.error}
          discovering={tacet.discovering}
          queueOpen={queueOpen}
          browserPlayer={browserPlayer}
          onDismissError={tacet.clearError}
          onTransport={(action) => void tacet.transport(action)}
          onSeek={(sec) => void tacet.seek(sec)}
          onVolume={(vol) => void tacet.setVolume(vol)}
          onMute={(muted) => void tacet.setMute(muted)}
          onSelectZone={tacet.selectZone}
          onDiscover={() => void tacet.discoverZones()}
          onOpenConnect={() => setConnectOpen(true)}
          onToggleQueue={() => setQueueOpen((v) => !v)}
          onOpenAlbum={(albumId) => openAlbumById(albumId)}
          onOpenArtist={openArtist}
        />

        {/* Mobile Bottom Navigation Bar */}
        <MobileBottomNav
          activeSection={section}
          onNavigate={(next) => {
            if (next === "search") setSearchFocusNonce((n) => n + 1);
          }}
        />
      </div>

      {/* Mobile Queue Bottom Sheet */}
      <MobileQueueSheet
        open={queueOpen}
        onOpenChange={setQueueOpen}
        items={tacet.activeZone?.queue ?? []}
        currentItemId={tacet.activeZone?.currentItemId}
        onJump={(itemId) => void tacet.jumpItem(itemId)}
        onRemove={(itemId) => void tacet.removeItem(itemId)}
        onClear={() => void tacet.clearQueue()}
      />

      {/* Connect UPnP Renderer by URL Dialog */}
      <ConnectModal
        open={connectOpen}
        externalError={tacet.error}
        onClose={() => setConnectOpen(false)}
        onConnect={(url) => tacet.connectZone(url)}
      />
    </div>
  );
}

function HomeRoute({
  libraryStatus,
  onOpenAlbum,
  onNavigate,
  onPlay,
}: {
  libraryStatus: ReturnType<typeof useTacet>["libraryStatus"];
  onOpenAlbum: (album: Album) => void;
  onNavigate: (section: AppSection) => void;
  onPlay: (source: QueueSource, startIndex?: number) => void;
}) {
  const [searchParams] = useSearchParams();
  // Backward-compatible redirect for legacy /?tab=qobuz OAuth callback URLs.
  if (searchParams.get("tab") === "qobuz") {
    return <Navigate to="/qobuz" replace />;
  }
  return (
    <HomeView
      libraryStatus={libraryStatus}
      onOpenAlbum={onOpenAlbum}
      onNavigate={onNavigate}
      onPlay={onPlay}
    />
  );
}

function SearchRoute({
  focusNonce,
  qobuzStatus,
  currentTrackId,
  onOpenAlbum,
  onOpenArtist,
  onPlay,
  onAdd,
}: {
  focusNonce: number;
  qobuzStatus: ReturnType<typeof useTacet>["qobuzStatus"];
  currentTrackId?: string;
  onOpenAlbum: (album: Album) => void;
  onOpenArtist: (artistName: string, qobuzArtistId?: number) => void;
  onPlay: (source: QueueSource, startIndex?: number) => void;
  onAdd: (source: QueueSource, position?: QueuePosition) => void;
}) {
  const [searchParams, setSearchParams] = useSearchParams();
  const query = searchParams.get("q") ?? "";

  const handleQueryChange = useCallback(
    (next: string) => {
      if (next) {
        setSearchParams({ q: next }, { replace: true });
      } else {
        setSearchParams({}, { replace: true });
      }
    },
    [setSearchParams],
  );

  return (
    <SearchView
      query={query}
      onQueryChange={handleQueryChange}
      focusNonce={focusNonce}
      qobuzStatus={qobuzStatus}
      currentTrackId={currentTrackId}
      onOpenAlbum={onOpenAlbum}
      onOpenArtist={onOpenArtist}
      onPlay={onPlay}
      onAdd={onAdd}
    />
  );
}

function ArtistsRoute({
  libraryStatus,
  qobuzStatus,
  onSelectArtist,
  onBackToArtists,
  onOpenAlbum,
  onPlay,
  onAdd,
}: {
  libraryStatus: ReturnType<typeof useTacet>["libraryStatus"];
  qobuzStatus: ReturnType<typeof useTacet>["qobuzStatus"];
  onSelectArtist: (artistName: string) => void;
  onBackToArtists: () => void;
  onOpenAlbum: (album: Album) => void;
  onPlay: (source: QueueSource, startIndex?: number) => void;
  onAdd: (source: QueueSource, position?: QueuePosition) => void;
}) {
  const { artistName } = useParams<{ artistName?: string }>();
  const [searchParams] = useSearchParams();
  const rawQobuzId = searchParams.get("qobuzId");
  const parsedId = rawQobuzId ? Number(rawQobuzId) : Number.NaN;
  const qobuzArtistId = Number.isFinite(parsedId) && parsedId > 0 ? parsedId : undefined;

  return (
    <ArtistsView
      libraryStatus={libraryStatus}
      qobuzStatus={qobuzStatus}
      initialArtist={artistName}
      initialQobuzArtistId={qobuzArtistId}
      onSelectArtist={onSelectArtist}
      onBackToArtists={onBackToArtists}
      onOpenAlbum={onOpenAlbum}
      onPlay={onPlay}
      onAdd={onAdd}
    />
  );
}

function AlbumRoute({
  libraryEtag,
  currentTrackId,
  isPlaying,
  onAmbientArt,
  onOpenAlbum,
  onOpenArtist,
  onPlay,
  onAdd,
}: {
  libraryEtag?: string;
  currentTrackId?: string;
  isPlaying: boolean;
  onAmbientArt?: (artworkUrl?: string) => void;
  onOpenAlbum: (albumId: string) => void;
  onOpenArtist: (artistName: string, qobuzArtistId?: number) => void;
  onPlay: (source: QueueSource, startIndex?: number) => void;
  onAdd: (source: QueueSource, position?: QueuePosition) => void;
}) {
  const { albumKey } = useParams<{ albumKey: string }>();
  const albumId = albumKey ? decodeAlbumKey(albumKey) : undefined;
  const location = useLocation();
  const routerNavigate = useNavigate();
  const state = (location.state as AlbumLocationState | null) ?? null;
  const hint = state?.album && state.album.id === albumId ? state.album : undefined;

  const isOutOfLibraryQobuz = Boolean(hint && hint.source === "qobuz" && hint.inLibrary === false);

  const albumQuery = useSWR<Album>(
    albumId ? `album:${albumId}` : null,
    (signal) => {
      if (!albumId) return Promise.reject(new Error("Missing album id"));
      if (isOutOfLibraryQobuz) {
        return tacetClient.qobuzAlbum(albumId, { signal });
      }
      return tacetClient.getLibraryAlbum(albumId, { signal }).catch((err) => {
        if (err instanceof TacetAPIError && err.status === 404 && albumId.startsWith("qobuz:")) {
          return tacetClient.qobuzAlbum(albumId, { signal });
        }
        throw err;
      });
    },
    {
      staleTimeMs: 60_000,
      version: albumId?.startsWith("local:") ? libraryEtag : undefined,
      fallbackData: hint,
    },
  );

  const album = albumQuery.data ?? null;
  const errMsg = albumQuery.error;

  useEffect(() => {
    onAmbientArt?.(album?.artworkUrl);
  }, [album?.artworkUrl, onAmbientArt]);

  // Silently canonicalize short/legacy album URLs to /albums/<artist-album>/<key>.
  const canonicalRef = useRef({ pathname: location.pathname, state });
  canonicalRef.current = { pathname: location.pathname, state };
  useEffect(() => {
    if (!album) return;
    const canonical = albumPath(album.id, { artist: album.artist, title: album.title });
    if (canonicalRef.current.pathname !== canonical) {
      void routerNavigate(canonical, { replace: true, state: canonicalRef.current.state });
    }
  }, [album, routerNavigate]);

  const handleBack = useCallback(() => {
    if (state?.fromApp) {
      void routerNavigate(-1);
    } else {
      void routerNavigate("/albums");
    }
  }, [routerNavigate, state?.fromApp]);

  if (errMsg && !album) {
    return (
      <div className="p-6 sm:p-8 space-y-4">
        <div role="alert" className="rounded-xl border border-red-500/30 bg-red-950/50 px-4 py-3 text-sm text-red-200">
          {errMsg}
        </div>
        <Button type="button" variant="secondary" size="sm" onClick={handleBack}>
          Back to Albums
        </Button>
      </div>
    );
  }

  if (!album) {
    return (
      <div className="flex h-full items-center justify-center text-sm text-muted-foreground">
        Loading album…
      </div>
    );
  }

  return (
    <>
      {errMsg && (
        <div role="alert" className="m-4 sm:mx-8 sm:mt-4 rounded-xl border border-red-500/30 bg-red-950/50 px-4 py-3 text-sm text-red-200">
          {errMsg}
        </div>
      )}
      <AlbumView
        album={album}
        backLabel={state?.backLabel ?? "Albums"}
        loadingTracks={albumQuery.isValidating && (album.tracks?.length ?? 0) === 0}
        currentTrackId={currentTrackId}
        isPlaying={isPlaying}
        onBack={handleBack}
        onOpenAlbum={onOpenAlbum}
        onOpenArtist={onOpenArtist}
        onAlbumUpdated={albumQuery.mutate}
        onPlay={onPlay}
        onAdd={onAdd}
      />
    </>
  );
}
