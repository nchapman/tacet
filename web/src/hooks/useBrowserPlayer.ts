import { useCallback, useEffect, useRef, useState } from "react";
import { tacetClient } from "../api/client";
import type { PlayerStateReport } from "../api/types";
import { BrowserAudioEngine, defaultBrowserPlayerName, detectBrowserCodecs } from "../player/browserPlayer";

const PLAYER_ID_KEY = "tacet.browserPlayerId";
const PLAYER_NAME_KEY = "tacet.browserPlayerName";
const PLAYER_ENABLED_KEY = "tacet.browserPlayerEnabled";
const PLAYER_TOKEN_KEY = "tacet.browserPlayerToken";

function loadSessionToken(): string {
  try {
    return window.sessionStorage.getItem(PLAYER_TOKEN_KEY) ?? "";
  } catch {
    return "";
  }
}

function saveSessionToken(token: string): void {
  try {
    if (token) window.sessionStorage.setItem(PLAYER_TOKEN_KEY, token);
    else window.sessionStorage.removeItem(PLAYER_TOKEN_KEY);
  } catch {
    // Ignore storage failures.
  }
}

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

/**
 * Returns whether the local browser player should be active on startup.
 * Defaults to enabled for first-time visitors; only disabled when the user
 * explicitly turned it off ("0").
 */
export function shouldEnableBrowserPlayer(storedValue: string | null): boolean {
  return storedValue !== "0";
}

function loadEnabled(): boolean {
  try {
    return shouldEnableBrowserPlayer(window.localStorage.getItem(PLAYER_ENABLED_KEY));
  } catch {
    return true;
  }
}

export interface BrowserPlayerController {
  enabled: boolean;
  status: BrowserPlayerStatus;
  zoneId: string | null;
  name: string;
  needsUnlock: boolean;
  error: string | null;
  enable: (selectAfter?: boolean) => Promise<string | null>;
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
  const tokenRef = useRef<string>(loadSessionToken());
  const zoneIdRef = useRef<string | null>(null);
  const engineRef = useRef<BrowserAudioEngine | null>(null);
  const unsubRef = useRef<(() => void) | null>(null);
  const reregisterTimerRef = useRef<ReturnType<typeof setTimeout> | null>(null);
  const onZoneRegisteredRef = useRef(onZoneRegistered);
  onZoneRegisteredRef.current = onZoneRegistered;

  const stopSession = useCallback(() => {
    if (reregisterTimerRef.current !== null) {
      clearTimeout(reregisterTimerRef.current);
      reregisterTimerRef.current = null;
    }
    unsubRef.current?.();
    unsubRef.current = null;
    engineRef.current?.dispose();
    engineRef.current = null;
  }, []);

  const connect = useCallback(
    async (playerName: string, unlockNow: boolean, selectAfter: boolean): Promise<string | null> => {
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
            qobuzDelivery: "force",
          },
          tokenRef.current || undefined,
        );
        tokenRef.current = reg.token;
        saveSessionToken(reg.token);
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
            if (reregisterTimerRef.current !== null) {
              clearTimeout(reregisterTimerRef.current);
              reregisterTimerRef.current = null;
            }
            setStatus("connected");
            setError(null);
          },
          onCommand: (cmd) => {
            void activeEngine.handleCommand(cmd);
          },
          onReplaced: () => {
            // Another tab in this browser took over the player session; stop
            // locally without persisting "0" so the active tab stays enabled.
            stopSession();
            setEnabled(false);
            setStatus("off");
            setZoneId(null);
            zoneIdRef.current = null;
          },
          onError: () => {
            setStatus("reconnecting");
            // Recover cleanly if tacet restarted and dropped the player session (HTTP 404).
            if (reregisterTimerRef.current === null && loadEnabled()) {
              reregisterTimerRef.current = setTimeout(() => {
                reregisterTimerRef.current = null;
                if (loadEnabled()) {
                  void connect(loadPlayerName(), false, false);
                }
              }, 4000);
            }
          },
        });

        if (selectAfter) {
          onZoneRegisteredRef.current?.(reg.zoneId);
        }
        return reg.zoneId;
      } catch (err) {
        stopSession();
        setEnabled(false);
        setStatus("off");
        setError(err instanceof Error ? err.message : "Failed to register browser player");
        return null;
      }
    },
    [stopSession],
  );

  // Auto-attach on load unless the user explicitly turned off this browser player.
  useEffect(() => {
    if (!loadEnabled()) return;
    void connect(loadPlayerName(), false, false);
    return () => stopSession();
  }, [connect, stopSession]);

  // Synchronously unlock <audio> on the user's first pointer/keyboard gesture so
  // clicking Play on an auto-attached browser player never hits autoplay blocks.
  useEffect(() => {
    if (!enabled) return;
    const onGesture = () => {
      engineRef.current?.unlock();
    };
    window.addEventListener("pointerdown", onGesture, { once: true, capture: true, passive: true });
    window.addEventListener("keydown", onGesture, { once: true, capture: true, passive: true });
    return () => {
      window.removeEventListener("pointerdown", onGesture, { capture: true });
      window.removeEventListener("keydown", onGesture, { capture: true });
    };
  }, [enabled, zoneId]);

  const enable = useCallback(
    async (selectAfter = true): Promise<string | null> => {
      setEnabled(true);
      try {
        window.localStorage.setItem(PLAYER_ENABLED_KEY, "1");
      } catch {
        // Ignore storage errors.
      }
      return connect(name, true, selectAfter);
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
    tokenRef.current = "";
    saveSessionToken("");
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
      if (enabled && zoneIdRef.current) {
        await tacetClient
          .updatePlayer(zoneIdRef.current, { name: cleaned }, tokenRef.current || undefined)
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
