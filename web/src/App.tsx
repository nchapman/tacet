import { useEffect, useState } from "react";
import { ConnectModal } from "./components/ConnectModal";
import { Header } from "./components/Header";
import { LocalBrowser } from "./components/LocalBrowser";
import { NowPlaying } from "./components/NowPlaying";
import { QobuzBrowser } from "./components/QobuzBrowser";
import { QueuePanel } from "./components/QueuePanel";
import { useTacet } from "./hooks/useTacet";

type BrowserTab = "local" | "qobuz";

function initialTab(): BrowserTab {
  const params = new URLSearchParams(window.location.search);
  return params.get("tab") === "qobuz" ? "qobuz" : "local";
}

export function App() {
  const tacet = useTacet();
  const [tab, setTab] = useState<BrowserTab>(initialTab);
  const [connectOpen, setConnectOpen] = useState(false);

  const activeState = tacet.activeZone?.state;
  const activeTransport = tacet.transport;

  // Space toggles play/pause when focus is not in an interactive control.
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

  return (
    <div className="app-shell">
      <Header
        zones={tacet.zones}
        activeZoneId={tacet.activeZone?.zoneId ?? ""}
        streamMode={tacet.settings.streamMode}
        connection={tacet.connection}
        discovering={tacet.discovering}
        onSelectZone={tacet.selectZone}
        onDiscover={() => void tacet.discoverZones()}
        onOpenConnect={() => setConnectOpen(true)}
        onStreamModeChange={(mode) => void tacet.setStreamMode(mode)}
      />

      <main className="layout">
        <div className="left-col">
          <NowPlaying
            zone={tacet.activeZone}
            positionSec={tacet.interpolatedPositionSec}
            streamMode={tacet.settings.streamMode}
            error={tacet.error}
            onDismissError={tacet.clearError}
            onTransport={(action) => void tacet.transport(action)}
            onSeek={(sec) => void tacet.seek(sec)}
            onVolume={(vol) => void tacet.setVolume(vol)}
            onMute={(muted) => void tacet.setMute(muted)}
          />
          <QueuePanel
            items={tacet.activeZone?.queue ?? []}
            currentItemId={tacet.activeZone?.currentItemId}
            onJump={(itemId) => void tacet.jumpItem(itemId)}
            onRemove={(itemId) => void tacet.removeItem(itemId)}
            onClear={() => void tacet.clearQueue()}
          />
        </div>

        <section className="card browser-card">
          <div className="tabs" role="tablist" aria-label="Music sources">
            <button
              type="button"
              role="tab"
              aria-selected={tab === "local"}
              className={tab === "local" ? "active" : ""}
              onClick={() => setTab("local")}
            >
              NAS Library
            </button>
            <button
              type="button"
              role="tab"
              aria-selected={tab === "qobuz"}
              className={tab === "qobuz" ? "active" : ""}
              onClick={() => setTab("qobuz")}
            >
              Qobuz
            </button>
          </div>

          {tab === "local" ? (
            <LocalBrowser
              libraryStatus={tacet.libraryStatus}
              onPlay={(src, startIdx) => void tacet.playSource(src, startIdx)}
              onAdd={(src, pos) => void tacet.addSource(src, pos)}
            />
          ) : (
            <QobuzBrowser
              status={tacet.qobuzStatus}
              onStatusChange={tacet.setQobuzStatus}
              onPlay={(src, startIdx) => void tacet.playSource(src, startIdx)}
              onAdd={(src, pos) => void tacet.addSource(src, pos)}
            />
          )}
        </section>
      </main>

      <ConnectModal
        open={connectOpen}
        onClose={() => setConnectOpen(false)}
        onConnect={(url) => tacet.connectZone(url)}
      />
    </div>
  );
}
