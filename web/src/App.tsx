import { Volume2 } from "lucide-react";
import { useCallback, useEffect, useRef, useState } from "react";
import { tacetClient } from "@/api/client";
import type { Album } from "@/api/types";
import { AlbumView } from "@/components/AlbumView";
import { AlbumsView } from "@/components/AlbumsView";
import { ArtistsView } from "@/components/ArtistsView";
import { ConnectModal } from "@/components/ConnectModal";
import { HomeView } from "@/components/HomeView";
import { PlayerBar } from "@/components/PlayerBar";
import { QobuzView } from "@/components/QobuzView";
import { DesktopQueueDrawer, MobileQueueSheet } from "@/components/QueueDrawer";
import { SettingsView } from "@/components/SettingsView";
import { type AppSection, MobileBottomNav, Sidebar } from "@/components/Sidebar";
import { SongsView } from "@/components/SongsView";
import { Button } from "@/components/ui/button";
import { useBrowserPlayer } from "@/hooks/useBrowserPlayer";
import { useTacet } from "@/hooks/useTacet";

function initialSection(): AppSection {
  const params = new URLSearchParams(window.location.search);
  return params.get("tab") === "qobuz" ? "qobuz" : "home";
}

const SECTION_LABELS: Record<AppSection, string> = {
  home: "Home",
  albums: "Albums",
  artists: "Artists",
  songs: "Songs",
  qobuz: "Qobuz",
  settings: "Settings",
};

export function App() {
  const tacet = useTacet();
  const browserPlayer = useBrowserPlayer(tacet.selectZone);

  const [section, setSection] = useState<AppSection>(initialSection);
  const [selectedArtist, setSelectedArtist] = useState<string | undefined>(undefined);
  const [openAlbum, setOpenAlbum] = useState<Album | null>(null);
  const [albumLoading, setAlbumLoading] = useState(false);
  const [albumErr, setAlbumErr] = useState("");
  const [queueOpen, setQueueOpen] = useState(false);
  const [connectOpen, setConnectOpen] = useState(false);

  const stageRef = useRef<HTMLElement | null>(null);
  const albumAbortRef = useRef<AbortController | null>(null);

  const activeState = tacet.activeZone?.state;
  const activeTransport = tacet.transport;

  // Spacebar toggles play/pause when focus is not in an interactive control.
  useEffect(() => {
    const onKeyDown = (e: KeyboardEvent) => {
      if (e.code !== "Space" || e.metaKey || e.ctrlKey || e.altKey) return;
      const tag = (e.target as HTMLElement | null)?.tagName;
      if (tag === "INPUT" || tag === "TEXTAREA" || tag === "SELECT" || tag === "BUTTON" || tag === "A") return;
      e.preventDefault();
      void activeTransport(activeState === "playing" ? "pause" : "play");
    };
    window.addEventListener("keydown", onKeyDown);
    return () => window.removeEventListener("keydown", onKeyDown);
  }, [activeState, activeTransport]);

  useEffect(() => () => albumAbortRef.current?.abort(), []);

  const navigate = useCallback((next: AppSection) => {
    albumAbortRef.current?.abort();
    setOpenAlbum(null);
    setAlbumLoading(false);
    setAlbumErr("");
    setSection(next);
    stageRef.current?.scrollTo({ top: 0 });
  }, []);

  const openAlbumById = useCallback(async (albumId: string, hint?: Album) => {
    albumAbortRef.current?.abort();
    const ctrl = new AbortController();
    albumAbortRef.current = ctrl;
    setAlbumLoading(true);
    setAlbumErr("");
    if (hint) {
      setOpenAlbum(hint);
      stageRef.current?.scrollTo({ top: 0 });
    }
    try {
      const isOutOfLibraryQobuz =
        hint && hint.source === "qobuz" && hint.inLibrary === false;
      const full = isOutOfLibraryQobuz
        ? await tacetClient.qobuzAlbum(albumId, { signal: ctrl.signal })
        : await tacetClient.getLibraryAlbum(albumId, { signal: ctrl.signal }).catch(async () => {
            if (albumId.startsWith("qobuz:")) {
              return tacetClient.qobuzAlbum(albumId, { signal: ctrl.signal });
            }
            throw new Error("Album not found in library");
          });
      if (!ctrl.signal.aborted) {
        setOpenAlbum(full);
        stageRef.current?.scrollTo({ top: 0 });
      }
    } catch (err) {
      if (!ctrl.signal.aborted) {
        setAlbumErr(err instanceof Error ? err.message : String(err));
      }
    } finally {
      if (!ctrl.signal.aborted) setAlbumLoading(false);
    }
  }, []);

  const openArtist = useCallback((artistName: string) => {
    albumAbortRef.current?.abort();
    setOpenAlbum(null);
    setAlbumLoading(false);
    setAlbumErr("");
    setSelectedArtist(artistName);
    setSection("artists");
  }, []);

  const isSplitPane = !openAlbum && section === "artists";

  return (
    <div className="flex h-dvh w-full flex-col overflow-hidden bg-background text-foreground">
      {/* Autoplay unlock banner when browser blocks unmuted Web Audio */}
      {browserPlayer.needsUnlock && (
        <div
          role="alert"
          className="flex shrink-0 items-center justify-between gap-3 border-b border-amber-400/25 bg-amber-400/10 px-4 py-2 text-xs text-amber-200"
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
      <div className="flex flex-1 min-h-0 overflow-hidden">
        <Sidebar
          activeSection={section}
          libraryStatus={tacet.libraryStatus}
          onNavigate={navigate}
        />

        <main
          ref={stageRef}
          className={
            isSplitPane
              ? "flex-1 min-w-0 min-h-0 overflow-hidden"
              : "flex-1 min-w-0 min-h-0 overflow-y-auto"
          }
        >
          {albumErr && (
            <div role="alert" className="m-4 sm:mx-8 sm:mt-4 rounded-xl border border-red-500/30 bg-red-950/50 px-4 py-3 text-sm text-red-200">
              {albumErr}
            </div>
          )}

          {openAlbum ? (
            <AlbumView
              album={openAlbum}
              backLabel={SECTION_LABELS[section]}
              currentTrackId={tacet.activeZone?.currentTrack?.id}
              isPlaying={tacet.activeZone?.state === "playing"}
              onBack={() => {
                albumAbortRef.current?.abort();
                setOpenAlbum(null);
                setAlbumLoading(false);
                setAlbumErr("");
              }}
              onOpenAlbum={(id) => void openAlbumById(id)}
              onOpenArtist={openArtist}
              onAlbumUpdated={(updated) => setOpenAlbum(updated)}
              onPlay={(src, startIdx) => void tacet.playSource(src, startIdx)}
              onAdd={(src, pos) => void tacet.addSource(src, pos)}
            />
          ) : albumLoading ? (
            <div className="flex h-full items-center justify-center text-sm text-muted-foreground">
              Loading album…
            </div>
          ) : section === "home" ? (
            <HomeView
              libraryStatus={tacet.libraryStatus}
              onOpenAlbum={(alb) => void openAlbumById(alb.id, alb)}
              onNavigate={navigate}
              onPlay={(src, startIdx) => void tacet.playSource(src, startIdx)}
            />
          ) : section === "albums" ? (
            <AlbumsView
              libraryStatus={tacet.libraryStatus}
              onOpenAlbum={(alb) => void openAlbumById(alb.id, alb)}
              onPlay={(src, startIdx) => void tacet.playSource(src, startIdx)}
            />
          ) : section === "artists" ? (
            <ArtistsView
              libraryStatus={tacet.libraryStatus}
              initialArtist={selectedArtist}
              onOpenAlbum={(alb) => void openAlbumById(alb.id, alb)}
              onPlay={(src, startIdx) => void tacet.playSource(src, startIdx)}
              onAdd={(src, pos) => void tacet.addSource(src, pos)}
            />
          ) : section === "songs" ? (
            <SongsView
              libraryStatus={tacet.libraryStatus}
              currentTrackId={tacet.activeZone?.currentTrack?.id}
              onOpenAlbumId={(id) => void openAlbumById(id)}
              onOpenArtist={openArtist}
              onPlay={(src, startIdx) => void tacet.playSource(src, startIdx)}
              onAdd={(src, pos) => void tacet.addSource(src, pos)}
            />
          ) : section === "qobuz" ? (
            <QobuzView
              status={tacet.qobuzStatus}
              onStatusChange={tacet.setQobuzStatus}
              onOpenAlbum={(alb) => void openAlbumById(alb.id, alb)}
              onPlay={(src, startIdx) => void tacet.playSource(src, startIdx)}
              onAdd={(src, pos) => void tacet.addSource(src, pos)}
            />
          ) : (
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
              onGoToQobuz={() => navigate("qobuz")}
            />
          )}
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
        onOpenAlbum={(albumId) => void openAlbumById(albumId)}
        onOpenArtist={openArtist}
      />

      {/* Mobile Bottom Navigation Bar */}
      <MobileBottomNav activeSection={section} onNavigate={navigate} />

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
