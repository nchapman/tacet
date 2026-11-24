import { useCallback, useEffect, useRef, useState } from "react";
import { TacetAPIError, tacetClient } from "../api/client";
import type { PlayerStateReport } from "../api/types";
import { BrowserAudioEngine, defaultBrowserPlayerName, detectBrowserCodecs } from "../player/browserPlayer";

const PLAYER_ID_KEY = "tacet.browserPlayerId";
const PLAYER_NAME_KEY = "tacet.browserPlayerName";
const PLAYER_ENABLED_KEY = "tacet.browserPlayerEnabled";
const PLAYER_TOKEN_KEY = "tacet.browserPlayerToken";

function generateRandomPlayerId(): string {
  try {
    const bytes = new Uint8Array(6);
    window.crypto.getRandomValues(bytes);
    return Array.from(bytes, (b) => b.toString(16).padStart(2, "0")).join("");
  } catch {
    return "web-" + Math.random().toString(16).slice(2, 10);
  }
}

export function loadPlayerToken(): string {
  try {
    const local = window.localStorage.getItem(PLAYER_TOKEN_KEY);
    if (local) return local;
    // One-time migration from older builds that stored the token in sessionStorage.
    const session = window.sessionStorage.getItem(PLAYER_TOKEN_KEY);
    if (session) {
      window.localStorage.setItem(PLAYER_TOKEN_KEY, session);
      window.sessionStorage.removeItem(PLAYER_TOKEN_KEY);
      return session;
    }
    return "";
  } catch {
    return "";
  }
}

export function savePlayerToken(token: string): void {
  try {
    if (token) window.localStorage.setItem(PLAYER_TOKEN_KEY, token);
    else window.localStorage.removeItem(PLAYER_TOKEN_KEY);
    window.sessionStorage.removeItem(PLAYER_TOKEN_KEY);
  } catch {
    // Ignore storage failures.
  }
}

export type BrowserPlayerStatus = "off" | "connecting" | "connected" | "reconnecting";

function loadOrCreatePlayerId(): string {
  try {
    const saved = window.localStorage.getItem(PLAYER_ID_KEY);
    if (saved && saved.length > 0) return saved;
    const id = generateRandomPlayerId();
    window.localStorage.setItem(PLAYER_ID_KEY, id);
    return id;
  } catch {
    return generateRandomPlayerId();
  }
}

function rotatePlayerId(): string {
  const next = generateRandomPlayerId();
  try {
    window.localStorage.setItem(PLAYER_ID_KEY, next);
  } catch {
    // Ignore storage failures.
  }
  return next;
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
  expectedZoneId: string;
  name: string;
  needsUnlock: boolean;
  error: string | null;
  enable: (selectAfter?: boolean) => Promise<string | null>;
  disable: () => Promise<void>;
  rename: (nextName: string) => Promise<void>;
  unlockAudio: () => Promise<void>;
  reconnectNow: () => void;
}

export interface UseBrowserPlayerOptions {
  onZoneRegistered?: (zoneId: string) => void;
  onZoneRotated?: (previousZoneId: string, nextZoneId: string) => void;
}

const RECONNECT_BACKOFF_MS = [500, 1000, 2000, 4000] as const;

export function useBrowserPlayer(options?: UseBrowserPlayerOptions): BrowserPlayerController {
  const [enabled, setEnabled] = useState<boolean>(loadEnabled);
  const [status, setStatus] = useState<BrowserPlayerStatus>(() => (loadEnabled() ? "connecting" : "off"));
  const [zoneId, setZoneId] = useState<string | null>(null);
  const [name, setName] = useState<string>(loadPlayerName);
  const [needsUnlock, setNeedsUnlock] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const playerIdRef = useRef<string>(loadOrCreatePlayerId());
  const [expectedZoneId, setExpectedZoneId] = useState<string>(() => `player:${playerIdRef.current}`);
  const tokenRef = useRef<string>(loadPlayerToken());
  const zoneIdRef = useRef<string | null>(null);
  const engineRef = useRef<BrowserAudioEngine | null>(null);
  const unsubRef = useRef<(() => void) | null>(null);
  const reregisterTimerRef = useRef<ReturnType<typeof setTimeout> | null>(null);
  const reconnectAttemptRef = useRef(0);
  const connectGenRef = useRef(0);
  const statusRef = useRef<BrowserPlayerStatus>(status);
  statusRef.current = status;
  const lastRegisteredAtRef = useRef(0);
  const onZoneRegisteredRef = useRef(options?.onZoneRegistered);
  onZoneRegisteredRef.current = options?.onZoneRegistered;
  const onZoneRotatedRef = useRef(options?.onZoneRotated);
  onZoneRotatedRef.current = options?.onZoneRotated;

  const stopSession = useCallback(() => {
    connectGenRef.current++;
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
      const gen = ++connectGenRef.current;
      setStatus("connecting");
      setError(null);

      const codecs = detectBrowserCodecs();
      let engine: BrowserAudioEngine | null = null;
      const createEngine = () =>
        new BrowserAudioEngine({
          resolveUrl: (u) => tacetClient.resolveUrl(u),
          onAutoplayBlocked: setNeedsUnlock,
          onReportState: (rep: PlayerStateReport) => {
            const zid = zoneIdRef.current;
            if (!zid || gen !== connectGenRef.current) return;
            void tacetClient.reportPlayerState(zid, rep, tokenRef.current).catch(() => {});
          },
          onMediaSessionAction: (action, seekTime) => {
            const zid = zoneIdRef.current;
            if (!zid || gen !== connectGenRef.current) return;
            const z = tacetClient.zone(zid);
            if (action === "play") void z.transport("play").catch(() => {});
            else if (action === "pause") void z.transport("pause").catch(() => {});
            else if (action === "previoustrack") void z.transport("prev").catch(() => {});
            else if (action === "nexttrack") void z.transport("next").catch(() => {});
            else if (action === "seekto" && typeof seekTime === "number") void z.seek(seekTime).catch(() => {});
          },
        });

      if (unlockNow) {
        // Create & unlock synchronously inside user click handler before awaiting network.
        engine = createEngine();
        engine.unlock();
        engineRef.current = engine;
      }

      const registerOnce = (pid: string, tok: string) =>
        tacetClient.registerPlayer(
          {
            playerId: pid,
            name: playerName,
            model: "Web Browser",
            codecs,
            preferRelativeMedia: true,
            qobuzDelivery: "force",
          },
          tok || undefined,
        );

      try {
        let reg;
        const initialPid = playerIdRef.current;
        try {
          reg = await registerOnce(initialPid, tokenRef.current);
        } catch (err) {
          // If the server still holds our old playerId with a different token
          // (e.g. localStorage token cleared during the 90s grace window),
          // rotate to a fresh playerId and retry once instead of giving up.
          if (err instanceof TacetAPIError && err.status === 401) {
            const nextPid = rotatePlayerId();
            playerIdRef.current = nextPid;
            setExpectedZoneId(`player:${nextPid}`);
            tokenRef.current = "";
            savePlayerToken("");
            reg = await registerOnce(nextPid, "");
            if (gen === connectGenRef.current) {
              onZoneRotatedRef.current?.(`player:${initialPid}`, reg.zoneId);
            }
          } else {
            throw err;
          }
        }

        if (gen !== connectGenRef.current) {
          engine?.dispose();
          return null;
        }

        tokenRef.current = reg.token;
        savePlayerToken(reg.token);
        zoneIdRef.current = reg.zoneId;
        lastRegisteredAtRef.current = Date.now();
        setZoneId(reg.zoneId);
        setExpectedZoneId(reg.zoneId);

        if (!engine) {
          engine = createEngine();
          engineRef.current = engine;
        }

        const activeEngine = engine;
        unsubRef.current = tacetClient.subscribePlayerCommands(reg.zoneId, {
          onConnected: () => {
            if (gen !== connectGenRef.current) return;
            reconnectAttemptRef.current = 0;
            if (reregisterTimerRef.current !== null) {
              clearTimeout(reregisterTimerRef.current);
              reregisterTimerRef.current = null;
            }
            setStatus("connected");
            setError(null);
          },
          onCommand: (cmd) => {
            if (gen !== connectGenRef.current) return;
            void activeEngine.handleCommand(cmd);
          },
          onReplaced: () => {
            if (gen !== connectGenRef.current) return;
            // Another tab in this browser took over the player session; stop
            // locally without persisting "0" so the active tab stays enabled.
            stopSession();
            setEnabled(false);
            setStatus("off");
            setZoneId(null);
            zoneIdRef.current = null;
          },
          onError: () => {
            if (gen !== connectGenRef.current) return;
            setStatus("reconnecting");
            // Recover quickly if tacet restarted and dropped the player session (HTTP 404).
            if (reregisterTimerRef.current === null && loadEnabled()) {
              const delay =
                RECONNECT_BACKOFF_MS[Math.min(reconnectAttemptRef.current, RECONNECT_BACKOFF_MS.length - 1)];
              reconnectAttemptRef.current++;
              reregisterTimerRef.current = setTimeout(() => {
                reregisterTimerRef.current = null;
                if (gen === connectGenRef.current && loadEnabled()) {
                  void connect(loadPlayerName(), false, false);
                }
              }, delay);
            }
          },
        });

        if (selectAfter) {
          onZoneRegisteredRef.current?.(reg.zoneId);
        }
        return reg.zoneId;
      } catch (err) {
        if (gen !== connectGenRef.current) {
          engine?.dispose();
          return null;
        }
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
    savePlayerToken("");
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

  const reconnectNow = useCallback(() => {
    if (!loadEnabled() || statusRef.current === "connecting") return;
    if (Date.now() - lastRegisteredAtRef.current < 1000) return;
    reconnectAttemptRef.current = 0;
    void connect(loadPlayerName(), false, false);
  }, [connect]);

  return {
    enabled,
    status,
    zoneId,
    expectedZoneId,
    name,
    needsUnlock,
    error,
    enable,
    disable,
    rename,
    unlockAudio,
    reconnectNow,
  };
}
