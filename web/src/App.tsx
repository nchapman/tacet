import { useQuery, useQueryClient } from "@tanstack/react-query";
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
import type { Album, QueueItem, QueuePosition, QueueSource } from "@/api/types";
import { AlbumView } from "@/components/AlbumView";
import { AlbumsView } from "@/components/AlbumsView";
import { ArtistsView } from "@/components/ArtistsView";
import { ConnectModal } from "@/components/ConnectModal";
import { DiscoverShelfView } from "@/components/DiscoverShelfView";
import { HeavyRotationView } from "@/components/HeavyRotationView";
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
import {
  DEFAULT_GC_TIME_MS,
  invalidateLibraryQueries,
  invalidateQobuzLogout,
  QOBUZ_ALBUM_GC_TIME_MS,
  QOBUZ_ALBUM_STALE_TIME_MS,
  queryErrorMessage,
  queryKeys,
} from "@/lib/query";
import { useScrollRestoration } from "@/lib/scroll";
import { albumPath, artistPath, decodeAlbumKey, SECTION_LABELS, SECTION_PATHS, sectionFromPathname } from "@/routes";

const EMPTY_QUEUE: QueueItem[] = [];

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

  const queryClient = useQueryClient();
  const [groupVersions, setGroupVersions] = useState<boolean>(() => {
    try {
      return window.localStorage.getItem("tacet.groupVersions") !== "0";
    } catch {
      return true;
    }
  });
  const handleGroupVersionsChange = useCallback((enabled: boolean) => {
    setGroupVersions(enabled);
    try {
      window.localStorage.setItem("tacet.groupVersions", enabled ? "1" : "0");
    } catch {
      // ignore storage errors
    }
  }, []);

  const [discoverGenreIds, setDiscoverGenreIds] = useState<number[]>(() => {
    try {
      const raw = window.localStorage.getItem("tacet.discoverGenres");
      if (!raw) return [];
      const parsed: unknown = JSON.parse(raw);
      if (!Array.isArray(parsed)) return [];
      return parsed.filter((v): v is number => typeof v === "number" && Number.isInteger(v) && v > 0);
    } catch {
      return [];
    }
  });
  const handleDiscoverGenreIdsChange = useCallback((ids: number[]) => {
    const clean = Array.from(new Set(ids.filter((id) => Number.isInteger(id) && id > 0))).sort((a, b) => a - b);
    setDiscoverGenreIds(clean);
    try {
      if (clean.length === 0) window.localStorage.removeItem("tacet.discoverGenres");
      else window.localStorage.setItem("tacet.discoverGenres", JSON.stringify(clean));
    } catch {
      // ignore storage errors
    }
  }, []);

  const prevEtagRef = useRef<string | undefined>(undefined);
  const libraryEtag = tacet.libraryStatus?.etag;

  // Invalidate library & album queries whenever the server library ETag advances over SSE.
  useEffect(() => {
    if (!libraryEtag) return;
    if (prevEtagRef.current !== undefined && prevEtagRef.current !== libraryEtag) {
      invalidateLibraryQueries(queryClient);
    }
    prevEtagRef.current = libraryEtag;
  }, [libraryEtag, queryClient]);

  // Clear Qobuz queries across all LAN clients when Qobuz signs out or switches accounts over SSE.
  const prevQobuzRef = useRef<{ authenticated: boolean; userName: string } | null>(null);
  const qobuzAuth = tacet.qobuzStatus?.authenticated;
  const qobuzUser = tacet.qobuzStatus?.userName ?? "";
  useEffect(() => {
    if (qobuzAuth === undefined) return;
    const prev = prevQobuzRef.current;
    if (
      prev &&
      ((prev.authenticated && !qobuzAuth) ||
        (prev.authenticated && qobuzAuth && prev.userName && qobuzUser && prev.userName !== qobuzUser))
    ) {
      invalidateQobuzLogout(queryClient);
    }
    prevQobuzRef.current = { authenticated: qobuzAuth, userName: qobuzUser };
  }, [qobuzAuth, qobuzUser, queryClient]);

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

  const handleOpenAlbum = useCallback((alb: Album) => openAlbumById(alb.id, alb), [openAlbumById]);
  const handleOpenAlbumId = useCallback((id: string) => openAlbumById(id), [openAlbumById]);
  const handlePlay = useCallback(
    (src: QueueSource, startIdx?: number) => {
      void tacet.playSource(src, startIdx);
    },
    [tacet.playSource],
  );
  const handleAdd = useCallback(
    (src: QueueSource, pos?: QueuePosition) => {
      void tacet.addSource(src, pos);
    },
    [tacet.addSource],
  );
  const handleJumpItem = useCallback(
    (itemId: string) => {
      void tacet.jumpItem(itemId);
    },
    [tacet.jumpItem],
  );
  const handleRemoveItem = useCallback(
    (itemId: string) => {
      void tacet.removeItem(itemId);
    },
    [tacet.removeItem],
  );
  const handleClearQueue = useCallback(() => {
    void tacet.clearQueue();
  }, [tacet.clearQueue]);
  const handleBackToArtists = useCallback(() => {
    void routerNavigate("/artists");
  }, [routerNavigate]);
  const handleOpenSearch = useCallback(() => navigateSection("search"), [navigateSection]);
  const handleOpenConnect = useCallback(() => setConnectOpen(true), []);
  const handleToggleQueue = useCallback(() => setQueueOpen((v) => !v), []);
  const handleSidebarNavigate = useCallback(
    (next: AppSection) => {
      if (next === "search") setSearchFocusNonce((n) => n + 1);
      if (location.pathname === SECTION_PATHS[next]) {
        stageRef.current?.scrollTo({ top: 0 });
      }
    },
    [location.pathname],
  );
  const handleTransport = useCallback(
    (action: Parameters<typeof tacet.transport>[0]) => {
      void tacet.transport(action);
    },
    [tacet.transport],
  );
  const handleSeek = useCallback(
    (sec: number) => {
      void tacet.seek(sec);
    },
    [tacet.seek],
  );
  const handleVolume = useCallback(
    (vol: number) => {
      void tacet.setVolume(vol);
    },
    [tacet.setVolume],
  );
  const handleMute = useCallback(
    (muted: boolean) => {
      void tacet.setMute(muted);
    },
    [tacet.setMute],
  );
  const handleShuffle = useCallback(
    (enabled: boolean) => {
      void tacet.setShuffle(enabled);
    },
    [tacet.setShuffle],
  );
  const handleRepeat = useCallback(
    (mode: Parameters<typeof tacet.setRepeat>[0]) => {
      void tacet.setRepeat(mode);
    },
    [tacet.setRepeat],
  );
  const handleSetZoneQuality = useCallback(
    (zoneId: string, quality: Parameters<typeof tacet.setZoneQuality>[1]) => {
      void tacet.setZoneQuality(zoneId, quality);
    },
    [tacet.setZoneQuality],
  );
  const handleDiscover = useCallback(() => {
    void tacet.discoverZones();
  }, [tacet.discoverZones]);
  const handleStreamModeChange = useCallback(
    (mode: Parameters<typeof tacet.setStreamMode>[0]) => {
      void tacet.setStreamMode(mode);
    },
    [tacet.setStreamMode],
  );
  const handleGoToQobuz = useCallback(() => navigateSection("qobuz"), [navigateSection]);

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
          onNavigate={handleSidebarNavigate}
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
                  discoverGenreIds={discoverGenreIds}
                  onOpenAlbum={handleOpenAlbum}
                  onNavigate={navigateSection}
                  onPlay={handlePlay}
                />
              }
            />
            <Route
              path="/heavy-rotation"
              element={
                <HeavyRotationView
                  onOpenAlbum={handleOpenAlbum}
                  onPlay={handlePlay}
                />
              }
            />
            <Route
              path="/discover/:slug"
              element={
                <DiscoverShelfView
                  discoverGenreIds={discoverGenreIds}
                  onOpenAlbum={handleOpenAlbum}
                  onPlay={handlePlay}
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
                  onOpenAlbum={handleOpenAlbum}
                  onOpenArtist={openArtist}
                  onPlay={handlePlay}
                  onAdd={handleAdd}
                />
              }
            />
            <Route
              path="/albums"
              element={
                <AlbumsView
                  libraryStatus={tacet.libraryStatus}
                  groupVersions={groupVersions}
                  onOpenAlbum={handleOpenAlbum}
                  onPlay={handlePlay}
                />
              }
            />
            <Route
              path="/albums/:albumSlug/:albumKey"
              element={
                <AlbumRoute
                  qobuzConnected={Boolean(tacet.qobuzStatus?.authenticated)}
                  currentTrackId={tacet.activeZone?.currentTrack?.id}
                  isPlaying={tacet.activeZone?.state === "playing"}
                  onAmbientArt={setAlbumAmbientArt}
                  onOpenAlbum={handleOpenAlbumId}
                  onOpenArtist={openArtist}
                  onPlay={handlePlay}
                  onAdd={handleAdd}
                />
              }
            />
            <Route
              path="/albums/:albumKey"
              element={
                <AlbumRoute
                  qobuzConnected={Boolean(tacet.qobuzStatus?.authenticated)}
                  currentTrackId={tacet.activeZone?.currentTrack?.id}
                  isPlaying={tacet.activeZone?.state === "playing"}
                  onAmbientArt={setAlbumAmbientArt}
                  onOpenAlbum={handleOpenAlbumId}
                  onOpenArtist={openArtist}
                  onPlay={handlePlay}
                  onAdd={handleAdd}
                />
              }
            />
            <Route
              path="/artists"
              element={
                <ArtistsRoute
                  qobuzStatus={tacet.qobuzStatus}
                  onSelectArtist={openArtist}
                  onBackToArtists={handleBackToArtists}
                  onOpenAlbum={handleOpenAlbum}
                  onPlay={handlePlay}
                  onAdd={handleAdd}
                />
              }
            />
            <Route
              path="/artists/:artistName"
              element={
                <ArtistsRoute
                  qobuzStatus={tacet.qobuzStatus}
                  onSelectArtist={openArtist}
                  onBackToArtists={handleBackToArtists}
                  onOpenAlbum={handleOpenAlbum}
                  onPlay={handlePlay}
                  onAdd={handleAdd}
                />
              }
            />
            <Route
              path="/songs"
              element={
                <SongsView
                  currentTrackId={tacet.activeZone?.currentTrack?.id}
                  onOpenAlbumId={handleOpenAlbumId}
                  onOpenArtist={openArtist}
                  onPlay={handlePlay}
                  onAdd={handleAdd}
                />
              }
            />
            <Route
              path="/qobuz"
              element={
                <QobuzView
                  status={tacet.qobuzStatus}
                  onStatusChange={tacet.setQobuzStatus}
                  onOpenAlbum={handleOpenAlbum}
                  onPlay={handlePlay}
                  onOpenSearch={handleOpenSearch}
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
                  groupVersions={groupVersions}
                  discoverGenreIds={discoverGenreIds}
                  zones={tacet.zones}
                  discovering={tacet.discovering}
                  onStreamModeChange={handleStreamModeChange}
                  onGroupVersionsChange={handleGroupVersionsChange}
                  onDiscoverGenreIdsChange={handleDiscoverGenreIdsChange}
                  onQobuzStatusChange={tacet.setQobuzStatus}
                  onDiscover={handleDiscover}
                  onOpenConnect={handleOpenConnect}
                  onGoToQobuz={handleGoToQobuz}
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
          items={tacet.activeZone?.queue ?? EMPTY_QUEUE}
          currentItemId={tacet.activeZone?.currentItemId}
          onJump={handleJumpItem}
          onRemove={handleRemoveItem}
          onClear={handleClearQueue}
        />
      </div>

      {/* Spotify-style Bottom Player Bar (Desktop) & Mini-Player + Expandable Full-Screen Sheet (Mobile) */}
      <div className="relative z-10 shrink-0">
        <PlayerBar
          zones={tacet.zones}
          activeZone={tacet.activeZone}
          getClockOffsetMs={tacet.getClockOffsetMs}
          error={tacet.error}
          discovering={tacet.discovering}
          transcodeAvailable={Boolean(tacet.settings.transcodeAvailable)}
          queueOpen={queueOpen}
          browserPlayer={browserPlayer}
          onDismissError={tacet.clearError}
          onTransport={handleTransport}
          onSeek={handleSeek}
          onVolume={handleVolume}
          onMute={handleMute}
          onShuffle={handleShuffle}
          onRepeat={handleRepeat}
          onSelectZone={tacet.selectZone}
          onSetZoneQuality={handleSetZoneQuality}
          onDiscover={handleDiscover}
          onOpenConnect={handleOpenConnect}
          onToggleQueue={handleToggleQueue}
          onOpenAlbum={handleOpenAlbumId}
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
  discoverGenreIds,
  onOpenAlbum,
  onNavigate,
  onPlay,
}: {
  libraryStatus: ReturnType<typeof useTacet>["libraryStatus"];
  discoverGenreIds: number[];
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
      discoverGenreIds={discoverGenreIds}
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
  qobuzStatus,
  onSelectArtist,
  onBackToArtists,
  onOpenAlbum,
  onPlay,
  onAdd,
}: {
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
  qobuzConnected,
  currentTrackId,
  isPlaying,
  onAmbientArt,
  onOpenAlbum,
  onOpenArtist,
  onPlay,
  onAdd,
}: {
  qobuzConnected: boolean;
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
  const queryClient = useQueryClient();
  const state = (location.state as AlbumLocationState | null) ?? null;
  const hint = state?.album && state.album.id === albumId ? state.album : undefined;

  const isOutOfLibraryQobuz = Boolean(hint && hint.source === "qobuz" && hint.inLibrary === false);

  const isQobuzAlbum = Boolean(albumId?.startsWith("qobuz:"));

  const albumQuery = useQuery<Album>({
    queryKey: queryKeys.album(albumId ?? ""),
    queryFn: ({ signal }) => {
      if (!albumId) return Promise.reject(new Error("Missing album id"));
      // Always fetch Qobuz albums via qobuzAlbum when connected (served from Go catalogCache
      // with live SQLite inLibrary/versions/playCount overlay) so partial 1-track SQLite albums
      // never hide the rest of the album's tracklist.
      if (isOutOfLibraryQobuz || (isQobuzAlbum && qobuzConnected)) {
        return tacetClient.qobuzAlbum(albumId, { signal }).catch((err) => {
          if (err instanceof TacetAPIError && err.status === 404 && !isOutOfLibraryQobuz) {
            return tacetClient.getLibraryAlbum(albumId, { signal });
          }
          throw err;
        });
      }
      return tacetClient.getLibraryAlbum(albumId, { signal }).catch((err) => {
        if (err instanceof TacetAPIError && err.status === 404 && isQobuzAlbum) {
          return tacetClient.qobuzAlbum(albumId, { signal });
        }
        throw err;
      });
    },
    enabled: Boolean(albumId),
    staleTime: isQobuzAlbum ? QOBUZ_ALBUM_STALE_TIME_MS : 60_000,
    gcTime: isQobuzAlbum ? QOBUZ_ALBUM_GC_TIME_MS : DEFAULT_GC_TIME_MS,
    placeholderData: hint,
  });

  const album = albumQuery.data ?? null;
  const errMsg = queryErrorMessage(albumQuery.error);

  const handleAlbumUpdated = useCallback(
    (updated: Album) => {
      if (albumId) {
        queryClient.setQueryData(queryKeys.album(albumId), updated);
      }
      void queryClient.invalidateQueries({ queryKey: queryKeys.library.all });
    },
    [albumId, queryClient],
  );

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
        key={album.id}
        album={album}
        backLabel={state?.backLabel ?? "Albums"}
        qobuzConnected={qobuzConnected}
        loadingTracks={albumQuery.isFetching && (album.tracks?.length ?? 0) === 0}
        currentTrackId={currentTrackId}
        isPlaying={isPlaying}
        onBack={handleBack}
        onOpenAlbum={onOpenAlbum}
        onOpenArtist={onOpenArtist}
        onAlbumUpdated={handleAlbumUpdated}
        onPlay={onPlay}
        onAdd={onAdd}
      />
    </>
  );
}
