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

  // Space toggles play/pause when focus is not in an interactive text field.
  useEffect(() => {
    const onKeyDown = (e: KeyboardEvent) => {
      if (e.code !== "Space" || e.metaKey || e.ctrlKey || e.altKey) return;
      const tag = (e.target as HTMLElement | null)?.tagName;
      if (tag === "INPUT" || tag === "TEXTAREA" || tag === "SELECT" || tag === "BUTTON" || tag === "A") return;
      e.preventDefault();
      void tacet.transport(tacet.snapshot.state === "PLAYING" ? "pause" : "play");
    };
    window.addEventListener("keydown", onKeyDown);
    return () => window.removeEventListener("keydown", onKeyDown);
  }, [tacet]);

  return (
    <div className="app-shell">
      <Header
        renderers={tacet.renderers}
        activeId={tacet.activeId}
        streamMode={tacet.streamMode}
        connection={tacet.connection}
        discovering={tacet.discovering}
        onSelectRenderer={(id) => void tacet.selectRenderer(id)}
        onDiscover={() => void tacet.discoverRenderers()}
        onOpenConnect={() => setConnectOpen(true)}
        onStreamModeChange={(mode) => void tacet.setStreamMode(mode)}
      />

      <main className="layout">
        <div className="left-col">
          <NowPlaying
            snapshot={tacet.snapshot}
            error={tacet.error}
            onDismissError={tacet.clearError}
            onTransport={(action) => void tacet.transport(action)}
            onSeek={(sec) => void tacet.seek(sec)}
            onVolume={(vol) => void tacet.setVolume(vol)}
            onMute={(muted) => void tacet.setMute(muted)}
          />
          <QueuePanel
            queue={tacet.snapshot.queue ?? []}
            currentIndex={tacet.snapshot.currentIndex}
            onJump={(idx) => void tacet.jumpQueue(idx)}
            onRemove={(idx) => void tacet.removeFromQueue(idx)}
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
            <LocalBrowser onPlay={(req) => void tacet.playQueue(req)} onEnqueue={(req) => void tacet.enqueue(req)} />
          ) : (
            <QobuzBrowser onPlay={(req) => void tacet.playQueue(req)} onEnqueue={(req) => void tacet.enqueue(req)} />
          )}
        </section>
      </main>

      <ConnectModal
        open={connectOpen}
        onClose={() => setConnectOpen(false)}
        onConnect={(url) => tacet.connectRenderer(url)}
      />
    </div>
  );
}
