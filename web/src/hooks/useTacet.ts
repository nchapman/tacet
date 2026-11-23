import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { tacetClient } from "../api/client";
import type {
  AudioQuality,
  LibraryStatus,
  QobuzQuality,
  QobuzStatus,
  QueuePosition,
  QueueSource,
  RepeatMode,
  Settings,
  StreamMode,
  TransportAction,
  ZoneSnapshot,
} from "../api/types";

export type ConnectionStatus = "connecting" | "live" | "reconnecting";

const SELECTED_ZONE_KEY = "tacet.selectedZoneId";
const SEEK_GRACE_MS = 2000;
const VOLUME_GRACE_MS = 1500;

export interface PendingSeek {
  zoneId: string;
  itemId?: string;
  targetSec: number;
  setAtMs: number;
  expiresAtMs: number;
}

export interface PendingVolume {
  zoneId: string;
  targetVol: number;
  expiresAtMs: number;
}

export function reconcilePendingSeek(
  pending: PendingSeek | null,
  zoneId: string,
  itemId: string | undefined,
  incomingPos: number,
  isPlaying: boolean,
  nowMs: number,
): { nextPending: PendingSeek | null; useOptimistic: boolean } {
  if (!pending || pending.zoneId !== zoneId) {
    return { nextPending: pending, useOptimistic: false };
  }
  if (nowMs >= pending.expiresAtMs) {
    return { nextPending: null, useOptimistic: false };
  }
  if (pending.itemId && itemId && pending.itemId !== itemId) {
    return { nextPending: null, useOptimistic: false };
  }
  const elapsedSec = isPlaying ? Math.max(0, (nowMs - pending.setAtMs) / 1000) : 0;
  const expected = pending.targetSec + elapsedSec;
  if (Math.abs(incomingPos - expected) <= 3) {
    return { nextPending: null, useOptimistic: false };
  }
  return { nextPending: pending, useOptimistic: true };
}

export function reconcilePendingVolume(
  pending: PendingVolume | null,
  zoneId: string,
  incomingVol: number,
  nowMs: number,
): { nextPending: PendingVolume | null; useOptimistic: boolean } {
  if (!pending || pending.zoneId !== zoneId) {
    return { nextPending: pending, useOptimistic: false };
  }
  if (nowMs >= pending.expiresAtMs) {
    return { nextPending: null, useOptimistic: false };
  }
  if (Math.abs(incomingVol - pending.targetVol) <= 1) {
    return { nextPending: null, useOptimistic: false };
  }
  return { nextPending: pending, useOptimistic: true };
}

function readSavedZoneId(): string {
  try {
    return window.localStorage.getItem(SELECTED_ZONE_KEY) ?? "";
  } catch {
    return "";
  }
}

function saveZoneId(id: string) {
  try {
    if (id) {
      window.localStorage.setItem(SELECTED_ZONE_KEY, id);
    } else {
      window.localStorage.removeItem(SELECTED_ZONE_KEY);
    }
  } catch {
    // Ignore storage unavailability (private browsing, etc.)
  }
}

export function resolveActiveZone(
  zones: ZoneSnapshot[],
  savedZoneId: string,
  defaultZoneId?: string | null,
): ZoneSnapshot | null {
  if (!zones.length) return null;
  if (savedZoneId) {
    const exact = zones.find((z) => z.zoneId === savedZoneId);
    if (exact) return exact;
  }
  if (defaultZoneId) {
    const fallbackLocal = zones.find((z) => z.zoneId === defaultZoneId);
    if (fallbackLocal) return fallbackLocal;
  }
  return zones[0];
}

export function useTacet(defaultZoneId?: string | null) {
  const [zones, setZones] = useState<ZoneSnapshot[]>([]);
  const [selectedZoneId, setSelectedZoneId] = useState<string>(readSavedZoneId);
  const [settings, setSettings] = useState<Settings>({
    streamMode: "direct",
    qobuzMaxQuality: "flac_24_192",
    libraryWatchEnabled: true,
    libraryScanIntervalMin: 60,
  });
  const [qobuzStatus, setQobuzStatus] = useState<QobuzStatus | null>(null);
  const [libraryStatus, setLibraryStatus] = useState<LibraryStatus | null>(null);
  const [connection, setConnection] = useState<ConnectionStatus>("connecting");
  const [error, setError] = useState<string>("");
  const [discovering, setDiscovering] = useState(false);

  const activeZoneIdRef = useRef(selectedZoneId);
  const clockOffsetMsRef = useRef(0);
  const getClockOffsetMs = useCallback(() => clockOffsetMsRef.current, []);
  const clearError = useCallback(() => setError(""), []);
  const pendingSeekRef = useRef<PendingSeek | null>(null);
  const pendingVolumeRef = useRef<PendingVolume | null>(null);

  const applyPendingLocks = useCallback((existing: ZoneSnapshot | undefined, incoming: ZoneSnapshot): ZoneSnapshot => {
    if (!existing) return incoming;
    const now = Date.now();
    let result = incoming;

    const seekCheck = reconcilePendingSeek(
      pendingSeekRef.current,
      incoming.zoneId,
      incoming.currentItemId,
      incoming.positionSec,
      incoming.state === "playing",
      now,
    );
    pendingSeekRef.current = seekCheck.nextPending;
    if (seekCheck.useOptimistic) {
      result = {
        ...result,
        positionSec: existing.positionSec,
        positionUpdatedAt: existing.positionUpdatedAt,
      };
    }

    const volCheck = reconcilePendingVolume(pendingVolumeRef.current, incoming.zoneId, incoming.volume, now);
    pendingVolumeRef.current = volCheck.nextPending;
    if (volCheck.useOptimistic) {
      result = { ...result, volume: existing.volume };
    }

    return result;
  }, []);

  const syncClockOffset = useCallback((serverTime?: string) => {
    if (!serverTime) return;
    const serverMs = Date.parse(serverTime);
    if (Number.isFinite(serverMs)) {
      clockOffsetMsRef.current = Date.now() - serverMs;
    }
  }, []);

  // Apply full snapshot only if its revision is >= our current zone revision.
  const upsertZone = useCallback(
    (incoming: ZoneSnapshot) => {
      setZones((prev) => {
        const idx = prev.findIndex((z) => z.zoneId === incoming.zoneId);
        if (idx === -1) return [...prev, incoming];
        if (incoming.revision < prev[idx].revision) return prev;
        const next = prev.slice();
        next[idx] = applyPendingLocks(prev[idx], incoming);
        return next;
      });
    },
    [applyPendingLocks],
  );

  // Never overwrite the persisted localStorage preference here: a temporarily
  // absent zone (e.g. local browser player still registering, or UPnP renderer
  // still being discovered after a server restart) will snap back automatically
  // once it appears in `zones`.
  const replaceZones = useCallback(
    (list: ZoneSnapshot[], force = false) => {
      setZones((prev) => {
        const prevById = new Map(prev.map((z) => [z.zoneId, z]));
        return list.map((incoming) => {
          const existing = prevById.get(incoming.zoneId);
          if (!force && existing && incoming.revision < existing.revision) {
            return existing;
          }
          return applyPendingLocks(existing, incoming);
        });
      });
    },
    [applyPendingLocks],
  );

  const runAction = useCallback(
    async <T,>(fn: () => Promise<T>, onSuccess?: (result: T) => void): Promise<T | undefined> => {
      try {
        setError("");
        const result = await fn();
        onSuccess?.(result);
        return result;
      } catch (err) {
        setError(err instanceof Error ? err.message : String(err));
        return undefined;
      }
    },
    [],
  );

  // SSE subscription is the single live source of truth (initial `snapshot` event
  // delivers zones, settings, qobuz, and library status immediately on connect).
  useEffect(() => {
    const resyncAll = () => {
      void tacetClient
        .listZones()
        .then((res) => replaceZones(res.zones ?? [], true))
        .catch(() => {});
    };

    const unsubscribe = tacetClient.subscribeEvents({
      onOpen: () => setConnection("live"),
      onError: () => setConnection("reconnecting"),
      onResync: resyncAll,
      onSnapshot: (snap) => {
        syncClockOffset(snap.serverTime);
        replaceZones(snap.zones ?? [], true);
        if (snap.settings) setSettings(snap.settings);
        if (snap.qobuz) setQobuzStatus(snap.qobuz);
        if (snap.library) setLibraryStatus(snap.library);
      },
      onZonesUpdated: (ev) => {
        replaceZones(ev.zones ?? []);
      },
      onPlayback: (ev) => {
        syncClockOffset(ev.serverTime);
        const now = Date.now();
        setZones((prev) => {
          const idx = prev.findIndex((z) => z.zoneId === ev.zoneId);
          if (idx === -1 || ev.revision < prev[idx].revision) return prev;
          const z = prev[idx];
          const seekCheck = reconcilePendingSeek(
            pendingSeekRef.current,
            ev.zoneId,
            ev.currentItemId,
            ev.positionSec,
            ev.state === "playing",
            now,
          );
          pendingSeekRef.current = seekCheck.nextPending;
          const next = prev.slice();
          next[idx] = {
            ...z,
            revision: ev.revision,
            state: ev.state,
            positionSec: seekCheck.useOptimistic ? z.positionSec : ev.positionSec,
            positionUpdatedAt: seekCheck.useOptimistic ? z.positionUpdatedAt : ev.positionUpdatedAt,
            durationSec: ev.durationSec,
            currentIndex: ev.currentIndex,
            currentItemId: ev.currentItemId,
            currentTrack: ev.currentTrack,
            audioQuality: ev.audioQuality ?? z.audioQuality,
            activeQuality: ev.activeQuality,
            shuffle: ev.shuffle ?? z.shuffle,
            repeat: ev.repeat ?? z.repeat,
            gaplessArmed: ev.gaplessArmed,
            eventsActive: ev.eventsActive,
          };
          return next;
        });
      },
      onVolume: (ev) => {
        const now = Date.now();
        setZones((prev) => {
          const idx = prev.findIndex((z) => z.zoneId === ev.zoneId);
          if (idx === -1 || ev.revision < prev[idx].revision) return prev;
          const z = prev[idx];
          const volCheck = reconcilePendingVolume(pendingVolumeRef.current, ev.zoneId, ev.volume, now);
          pendingVolumeRef.current = volCheck.nextPending;
          const next = prev.slice();
          next[idx] = {
            ...z,
            revision: ev.revision,
            volume: volCheck.useOptimistic ? z.volume : ev.volume,
            muted: ev.muted,
          };
          return next;
        });
      },
      onQueue: (ev) => {
        setZones((prev) => {
          const idx = prev.findIndex((z) => z.zoneId === ev.zoneId);
          if (idx === -1 || ev.revision < prev[idx].revision) return prev;
          const z = prev[idx];
          const next = prev.slice();
          next[idx] = {
            ...z,
            revision: ev.revision,
            currentIndex: ev.currentIndex,
            currentItemId: ev.currentItemId,
            currentTrack: ev.currentTrack,
            queue: ev.items ?? [],
          };
          return next;
        });
      },
      onZoneError: (ev) => {
        if (ev.zoneId === activeZoneIdRef.current) {
          setError(ev.error.message);
        }
      },
      onSettingsUpdated: (s) => setSettings(s),
      onQobuzStatus: (st) => setQobuzStatus(st),
      onLibraryUpdated: (st) => setLibraryStatus(st),
    });

    return unsubscribe;
  }, [replaceZones, syncClockOffset]);

  const activeZone = useMemo<ZoneSnapshot | null>(
    () => resolveActiveZone(zones, selectedZoneId, defaultZoneId),
    [zones, selectedZoneId, defaultZoneId],
  );
  activeZoneIdRef.current = activeZone?.zoneId ?? "";

  const selectZone = useCallback((zoneId: string) => {
    setSelectedZoneId(zoneId);
    saveZoneId(zoneId);
    setError("");
  }, []);

  const discoverZones = useCallback(async () => {
    setDiscovering(true);
    try {
      await runAction(() => tacetClient.discoverZones(), (res) => replaceZones(res.zones ?? []));
    } finally {
      setDiscovering(false);
    }
  }, [replaceZones, runAction]);

  const connectZone = useCallback(
    async (url: string): Promise<boolean> => {
      const res = await runAction(
        () => tacetClient.connectZone(url),
        (data) => {
          replaceZones(data.zones ?? []);
          if (data.zone?.zoneId) {
            setSelectedZoneId(data.zone.zoneId);
            saveZoneId(data.zone.zoneId);
          }
        },
      );
      return Boolean(res);
    },
    [replaceZones, runAction],
  );

  const setStreamMode = useCallback(
    async (mode: StreamMode) => {
      await runAction(() => tacetClient.patchSettings({ streamMode: mode }), setSettings);
    },
    [runAction],
  );

  const setQobuzMaxQuality = useCallback(
    async (qobuzMaxQuality: QobuzQuality) => {
      await runAction(() => tacetClient.patchSettings({ qobuzMaxQuality }), setSettings);
    },
    [runAction],
  );

  const setLibraryAutoScan = useCallback(
    async (patch: { libraryWatchEnabled?: boolean; libraryScanIntervalMin?: number }) => {
      await runAction(() => tacetClient.patchSettings(patch), setSettings);
    },
    [runAction],
  );

  const activeZoneId = activeZone?.zoneId ?? "";

  const playSource = useCallback(
    (source: QueueSource, startIndex = 0) => {
      if (!activeZoneId) {
        setError("No UPnP renderer selected or discovered");
        return Promise.resolve(undefined);
      }
      return runAction(() => tacetClient.zone(activeZoneId).play(source, startIndex), upsertZone);
    },
    [activeZoneId, runAction, upsertZone],
  );

  const addSource = useCallback(
    (source: QueueSource, position: QueuePosition = "end", autoplay = true) => {
      if (!activeZoneId) {
        setError("No UPnP renderer selected or discovered");
        return Promise.resolve(undefined);
      }
      return runAction(() => tacetClient.zone(activeZoneId).addItems(source, { position, autoplay }), upsertZone);
    },
    [activeZoneId, runAction, upsertZone],
  );

  const jumpItem = useCallback(
    (itemId: string) => {
      if (!activeZoneId) return Promise.resolve(undefined);
      return runAction(() => tacetClient.zone(activeZoneId).jump(itemId), upsertZone);
    },
    [activeZoneId, runAction, upsertZone],
  );

  const removeItem = useCallback(
    (itemId: string) => {
      if (!activeZoneId) return Promise.resolve(undefined);
      return runAction(() => tacetClient.zone(activeZoneId).removeItem(itemId), upsertZone);
    },
    [activeZoneId, runAction, upsertZone],
  );

  const clearQueue = useCallback(() => {
    if (!activeZoneId) return Promise.resolve(undefined);
    return runAction(() => tacetClient.zone(activeZoneId).clearQueue(), upsertZone);
  }, [activeZoneId, runAction, upsertZone]);

  const transport = useCallback(
    (action: TransportAction) => {
      if (!activeZoneId) return Promise.resolve(undefined);
      return runAction(() => tacetClient.zone(activeZoneId).transport(action), upsertZone);
    },
    [activeZoneId, runAction, upsertZone],
  );

  const seek = useCallback(
    async (sec: number) => {
      if (!activeZoneId) return undefined;
      const now = Date.now();
      const targetSec = Math.max(0, Math.round(sec));
      const optimisticUpdatedAt = new Date(now - clockOffsetMsRef.current).toISOString();
      const pending: PendingSeek = {
        zoneId: activeZoneId,
        itemId: activeZone?.currentItemId,
        targetSec,
        setAtMs: now,
        expiresAtMs: now + SEEK_GRACE_MS,
      };
      pendingSeekRef.current = pending;
      setZones((prev) =>
        prev.map((z) =>
          z.zoneId === activeZoneId ? { ...z, positionSec: targetSec, positionUpdatedAt: optimisticUpdatedAt } : z,
        ),
      );
      const res = await runAction(() => tacetClient.zone(activeZoneId).seek(targetSec), upsertZone);
      if (!res && pendingSeekRef.current === pending) {
        pendingSeekRef.current = null;
      }
      return res;
    },
    [activeZoneId, activeZone?.currentItemId, runAction, upsertZone],
  );

  const setVolume = useCallback(
    async (vol: number) => {
      if (!activeZoneId) return undefined;
      const now = Date.now();
      const targetVol = Math.max(0, Math.min(100, Math.round(vol)));
      const pending: PendingVolume = {
        zoneId: activeZoneId,
        targetVol,
        expiresAtMs: now + VOLUME_GRACE_MS,
      };
      pendingVolumeRef.current = pending;
      setZones((prev) => prev.map((z) => (z.zoneId === activeZoneId ? { ...z, volume: targetVol } : z)));
      const res = await runAction(() => tacetClient.zone(activeZoneId).setVolume(targetVol), upsertZone);
      if (!res && pendingVolumeRef.current === pending) {
        pendingVolumeRef.current = null;
      }
      return res;
    },
    [activeZoneId, runAction, upsertZone],
  );

  const setMute = useCallback(
    (muted: boolean) => {
      if (!activeZoneId) return Promise.resolve(undefined);
      return runAction(() => tacetClient.zone(activeZoneId).setMute(muted), upsertZone);
    },
    [activeZoneId, runAction, upsertZone],
  );

  const setZoneQuality = useCallback(
    (zoneId: string, quality: AudioQuality) => {
      return runAction(() => tacetClient.zone(zoneId).setQuality(quality), upsertZone);
    },
    [runAction, upsertZone],
  );

  const setShuffle = useCallback(
    (enabled: boolean) => {
      if (!activeZoneId) return Promise.resolve(undefined);
      return runAction(() => tacetClient.zone(activeZoneId).setShuffle(enabled), upsertZone);
    },
    [activeZoneId, runAction, upsertZone],
  );

  const setRepeat = useCallback(
    (mode: RepeatMode) => {
      if (!activeZoneId) return Promise.resolve(undefined);
      return runAction(() => tacetClient.zone(activeZoneId).setRepeat(mode), upsertZone);
    },
    [activeZoneId, runAction, upsertZone],
  );

  return {
    zones,
    activeZone,
    getClockOffsetMs,
    settings,
    qobuzStatus,
    setQobuzStatus,
    libraryStatus,
    connection,
    error,
    clearError,
    discovering,
    selectZone,
    discoverZones,
    connectZone,
    setStreamMode,
    setQobuzMaxQuality,
    setLibraryAutoScan,
    playSource,
    addSource,
    jumpItem,
    removeItem,
    clearQueue,
    transport,
    seek,
    setVolume,
    setMute,
    setZoneQuality,
    setShuffle,
    setRepeat,
  };
}
