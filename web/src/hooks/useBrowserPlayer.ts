import { useCallback, useEffect, useRef, useState } from "react";
import { tacetClient } from "../api/client";
import type { PlayerStateReport } from "../api/types";
import { BrowserAudioEngine, defaultBrowserPlayerName, detectBrowserCodecs } from "../player/browserPlayer";

const PLAYER_ID_KEY = "tacet.browserPlayerId";
const PLAYER_NAME_KEY = "tacet.browserPlayerName";
const PLAYER_ENABLED_KEY = "tacet.browserPlayerEnabled";

export type BrowserPlayerStatus = "off" | "connecting" | "connected" | "reconnecting";

function loadOrCreatePlayerId(): string {
  try {
    const saved = window.localStorage.getItem(PLAYER_ID_KEY);
    if (saved && saved.length > 0) return saved;
    const bytes = new Uint8Array(6);
    window.crypto.getRandomValues(bytes);
    const id = Array.from(bytes, (b) => b.toString(16).padStart(2, "0")).join("");
    window.localStorage.setItem(PLAYER_ID_KEY, id);
    return id;
  } catch {
    return "web-" + Math.random().toString(16).slice(2, 10);
  }
}

function loadPlayerName(): string {
  try {
    const saved = window.localStorage.getItem(PLAYER_NAME_KEY);
    if (saved && saved.trim().length > 0) return saved.trim();
  } catch {
    // Ignore storage failures.
  }
  return defaultBrowserPlayerName();
}

function loadEnabled(): boolean {
  try {
    return window.localStorage.getItem(PLAYER_ENABLED_KEY) === "1";
  } catch {
    return false;
  }
}

export interface BrowserPlayerController {
  enabled: boolean;
  status: BrowserPlayerStatus;
  zoneId: string | null;
  name: string;
  needsUnlock: boolean;
  error: string | null;
  enable: (selectAfter?: boolean) => Promise<void>;
  disable: () => Promise<void>;
  rename: (nextName: string) => Promise<void>;
  unlockAudio: () => Promise<void>;
}

export function useBrowserPlayer(onZoneRegistered?: (zoneId: string) => void): BrowserPlayerController {
  const [enabled, setEnabled] = useState<boolean>(loadEnabled);
  const [status, setStatus] = useState<BrowserPlayerStatus>(() => (loadEnabled() ? "connecting" : "off"));
  const [zoneId, setZoneId] = useState<string | null>(null);
  const [name, setName] = useState<string>(loadPlayerName);
  const [needsUnlock, setNeedsUnlock] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const playerIdRef = useRef<string>(loadOrCreatePlayerId());
  const tokenRef = useRef<string>("");
  const zoneIdRef = useRef<string | null>(null);
  const engineRef = useRef<BrowserAudioEngine | null>(null);
  const unsubRef = useRef<(() => void) | null>(null);
  const onZoneRegisteredRef = useRef(onZoneRegistered);
  onZoneRegisteredRef.current = onZoneRegistered;

  const stopSession = useCallback(() => {
    unsubRef.current?.();
    unsubRef.current = null;
    engineRef.current?.dispose();
    engineRef.current = null;
  }, []);

  const connect = useCallback(
    async (playerName: string, unlockNow: boolean, selectAfter: boolean) => {
      stopSession();
      setStatus("connecting");
      setError(null);

      const codecs = detectBrowserCodecs();
      let engine: BrowserAudioEngine | null = null;
      if (unlockNow) {
        // Create & unlock synchronously inside user click handler before awaiting network.
        engine = new BrowserAudioEngine({
          resolveUrl: (u) => tacetClient.resolveUrl(u),
          onAutoplayBlocked: setNeedsUnlock,
          onReportState: (rep) => {
            const zid = zoneIdRef.current;
            if (!zid) return;
            void tacetClient.reportPlayerState(zid, rep, tokenRef.current).catch(() => {});
          },
          onMediaSessionAction: (action, seekTime) => {
            const zid = zoneIdRef.current;
            if (!zid) return;
            const z = tacetClient.zone(zid);
            if (action === "play") void z.transport("play").catch(() => {});
            else if (action === "pause") void z.transport("pause").catch(() => {});
            else if (action === "previoustrack") void z.transport("prev").catch(() => {});
            else if (action === "nexttrack") void z.transport("next").catch(() => {});
            else if (action === "seekto" && typeof seekTime === "number") void z.seek(seekTime).catch(() => {});
          },
        });
        engine.unlock();
        engineRef.current = engine;
      }

      try {
        const reg = await tacetClient.registerPlayer(
          {
            playerId: playerIdRef.current,
            name: playerName,
            model: "Web Browser",
            codecs,
            preferRelativeMedia: true,
          },
          tokenRef.current || undefined,
        );
        tokenRef.current = reg.token;
        zoneIdRef.current = reg.zoneId;
        setZoneId(reg.zoneId);

        if (!engine) {
          engine = new BrowserAudioEngine({
            resolveUrl: (u) => tacetClient.resolveUrl(u),
            onAutoplayBlocked: setNeedsUnlock,
            onReportState: (rep: PlayerStateReport) => {
              const zid = zoneIdRef.current;
              if (!zid) return;
              void tacetClient.reportPlayerState(zid, rep, tokenRef.current).catch(() => {});
            },
            onMediaSessionAction: (action, seekTime) => {
              const zid = zoneIdRef.current;
              if (!zid) return;
              const z = tacetClient.zone(zid);
              if (action === "play") void z.transport("play").catch(() => {});
              else if (action === "pause") void z.transport("pause").catch(() => {});
              else if (action === "previoustrack") void z.transport("prev").catch(() => {});
              else if (action === "nexttrack") void z.transport("next").catch(() => {});
              else if (action === "seekto" && typeof seekTime === "number") void z.seek(seekTime).catch(() => {});
            },
          });
          engineRef.current = engine;
        }

        const activeEngine = engine;
        unsubRef.current = tacetClient.subscribePlayerCommands(reg.zoneId, {
          onConnected: () => {
            setStatus("connected");
            setError(null);
          },
          onCommand: (cmd) => {
            void activeEngine.handleCommand(cmd);
          },
          onReplaced: () => {
            stopSession();
            setEnabled(false);
            setStatus("off");
            try {
              window.localStorage.setItem(PLAYER_ENABLED_KEY, "0");
            } catch {
              // Ignore storage errors.
            }
          },
          onError: () => {
            setStatus("reconnecting");
          },
        });

        if (selectAfter) {
          onZoneRegisteredRef.current?.(reg.zoneId);
        }
      } catch (err) {
        stopSession();
        setEnabled(false);
        setStatus("off");
        setError(err instanceof Error ? err.message : "Failed to register browser player");
        try {
          window.localStorage.setItem(PLAYER_ENABLED_KEY, "0");
        } catch {
          // Ignore storage errors.
        }
      }
    },
    [stopSession],
  );

  // Auto-reattach on page reload if the user previously enabled this browser player.
  useEffect(() => {
    if (!loadEnabled()) return;
    void connect(loadPlayerName(), false, false);
    return () => stopSession();
  }, [connect, stopSession]);

  const enable = useCallback(
    async (selectAfter = true) => {
      setEnabled(true);
      try {
        window.localStorage.setItem(PLAYER_ENABLED_KEY, "1");
      } catch {
        // Ignore storage errors.
      }
      await connect(name, true, selectAfter);
    },
    [connect, name],
  );

  const disable = useCallback(async () => {
    const zid = zoneIdRef.current;
    const tok = tokenRef.current;
    stopSession();
    setEnabled(false);
    setStatus("off");
    setNeedsUnlock(false);
    setZoneId(null);
    zoneIdRef.current = null;
    try {
      window.localStorage.setItem(PLAYER_ENABLED_KEY, "0");
    } catch {
      // Ignore storage errors.
    }
    if (zid) {
      await tacetClient.unregisterPlayer(zid, tok || undefined).catch(() => {});
    }
  }, [stopSession]);

  const rename = useCallback(
    async (nextName: string) => {
      const cleaned = nextName.trim();
      if (!cleaned) return;
      setName(cleaned);
      try {
        window.localStorage.setItem(PLAYER_NAME_KEY, cleaned);
      } catch {
        // Ignore storage errors.
      }
      if (enabled) {
        await tacetClient
          .registerPlayer(
            {
              playerId: playerIdRef.current,
              name: cleaned,
              model: "Web Browser",
              codecs: detectBrowserCodecs(),
              preferRelativeMedia: true,
            },
            tokenRef.current || undefined,
          )
          .catch(() => {});
      }
    },
    [enabled],
  );

  const unlockAudio = useCallback(async () => {
    await engineRef.current?.resumeFromGesture();
  }, []);

  return {
    enabled,
    status,
    zoneId,
    name,
    needsUnlock,
    error,
    enable,
    disable,
    rename,
    unlockAudio,
  };
}
