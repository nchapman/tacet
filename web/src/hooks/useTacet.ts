import { useQueryClient } from "@tanstack/react-query";
import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { tacetClient } from "../api/client";
import { radioSeedKey } from "../components/RadioButton";
import type {
  AudioQuality,
  LibraryStatus,
  QobuzQuality,
  QobuzStatus,
  QueuePosition,
  QueueSource,
  RadioSeedKind,
  RepeatMode,
  Settings,
  StreamMode,
  TransportAction,
  ZoneSnapshot,
} from "../api/types";
import { queryKeys } from "../lib/query";

export type ConnectionStatus = "connecting" | "live" | "reconnecting";

const SELECTED_ZONE_KEY = "tacet.selectedZoneId";

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
  preferLocalPending = false,
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
  // While the local browser player is still connecting/registering on startup
  // or after a server restart, avoid briefly jumping to an arbitrary UPnP/Cast
  // speaker in another room.
  if (preferLocalPending) {
    return null;
  }
  return zones[0];
}

export interface UseTacetOptions {
  defaultZoneId?: string | null;
  preferLocalPending?: boolean;
  onSnapshotZones?: (zones: ZoneSnapshot[]) => void;
}

export function useTacet(options?: UseTacetOptions | string | null) {
  const queryClient = useQueryClient();
  const opts: UseTacetOptions =
    typeof options === "string" || options == null ? { defaultZoneId: options } : options;
  const { defaultZoneId, preferLocalPending = false, onSnapshotZones } = opts;
  const onSnapshotZonesRef = useRef(onSnapshotZones);
  onSnapshotZonesRef.current = onSnapshotZones;
  const [zones, setZones] = useState<ZoneSnapshot[]>([]);
  const [selectedZoneId, setSelectedZoneId] = useState<string>(readSavedZoneId);
  const [settings, setSettings] = useState<Settings>({
    streamMode: "relay",
    qobuzMaxQuality: "flac_24_192",
    libraryWatchEnabled: true,
    libraryScanIntervalMin: 60,
  });
  const [qobuzStatus, setQobuzStatus] = useState<QobuzStatus | null>(null);
  const [libraryStatus, setLibraryStatus] = useState<LibraryStatus | null>(null);
  const [connection, setConnection] = useState<ConnectionStatus>("connecting");
  const [error, setError] = useState<string>("");
  const [discovering, setDiscovering] = useState(false);
  const [radioLoadingKey, setRadioLoadingKey] = useState<string | null>(null);
  const radioInFlightRef = useRef<string | null>(null);
  const radioStartRevRef = useRef<number>(0);
  const activeZoneRevRef = useRef<number>(0);

  const activeZoneIdRef = useRef(selectedZoneId);
  const clockOffsetMsRef = useRef(0);
  const getClockOffsetMs = useCallback(() => clockOffsetMsRef.current, []);
  const clearError = useCallback(() => setError(""), []);

  const syncClockOffset = useCallback((serverTime?: string) => {
    if (!serverTime) return;
    const serverMs = Date.parse(serverTime);
    if (Number.isFinite(serverMs)) {
      clockOffsetMsRef.current = Date.now() - serverMs;
    }
  }, []);

  // Apply full snapshot only if its revision is >= our current zone revision.
  const upsertZone = useCallback((incoming: ZoneSnapshot) => {
    setZones((prev) => {
      const idx = prev.findIndex((z) => z.zoneId === incoming.zoneId);
      if (idx === -1) return [...prev, incoming];
      if (incoming.revision < prev[idx].revision) return prev;
      const next = prev.slice();
      next[idx] = incoming;
      return next;
    });
  }, []);

  // Never overwrite the persisted localStorage preference here: a temporarily
  // absent zone (e.g. local browser player still registering, or UPnP renderer
  // still being discovered after a server restart) will snap back automatically
  // once it appears in `zones`.
  const replaceZones = useCallback((list: ZoneSnapshot[], force = false) => {
    setZones((prev) => {
      const prevById = new Map(prev.map((z) => [z.zoneId, z]));
      return list.map((incoming) => {
        const existing = prevById.get(incoming.zoneId);
        if (!force && existing && incoming.revision < existing.revision) {
          return existing;
        }
        return incoming;
      });
    });
  }, []);

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
        const snapZones = snap.zones ?? [];
        replaceZones(snapZones, true);
        onSnapshotZonesRef.current?.(snapZones);
        if (snap.settings) setSettings(snap.settings);
        if (snap.qobuz) setQobuzStatus(snap.qobuz);
        if (snap.library) setLibraryStatus(snap.library);
        if (snap.sonic) queryClient.setQueryData(queryKeys.sonic.status, snap.sonic);
        if (snap.enrichment) queryClient.setQueryData(queryKeys.enrichment.status, snap.enrichment);
      },
      onZonesUpdated: (ev) => {
        replaceZones(ev.zones ?? []);
      },
      onPlayback: (ev) => {
        syncClockOffset(ev.serverTime);
        setZones((prev) => {
          const idx = prev.findIndex((z) => z.zoneId === ev.zoneId);
          if (idx === -1 || ev.revision < prev[idx].revision) return prev;
          const z = prev[idx];
          const next = prev.slice();
          next[idx] = {
            ...z,
            revision: ev.revision,
            state: ev.state,
            positionSec: ev.positionSec,
            positionUpdatedAt: ev.positionUpdatedAt,
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
        setZones((prev) => {
          const idx = prev.findIndex((z) => z.zoneId === ev.zoneId);
          if (idx === -1 || ev.revision < prev[idx].revision) return prev;
          const z = prev[idx];
          const next = prev.slice();
          next[idx] = {
            ...z,
            revision: ev.revision,
            volume: ev.volume,
            muted: ev.muted,
          };
          return next;
        });
      },
      onQueue: (ev) => {
        if (
          ev.zoneId === activeZoneIdRef.current &&
          radioInFlightRef.current &&
          ev.revision > radioStartRevRef.current
        ) {
          setRadioLoadingKey(null);
        }
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
      onSonicStatus: (st) => queryClient.setQueryData(queryKeys.sonic.status, st),
      onEnrichmentStatus: (st) => queryClient.setQueryData(queryKeys.enrichment.status, st),
    });

    return unsubscribe;
  }, [queryClient, replaceZones, syncClockOffset]);

  const activeZone = useMemo<ZoneSnapshot | null>(
    () => resolveActiveZone(zones, selectedZoneId, defaultZoneId, preferLocalPending),
    [zones, selectedZoneId, defaultZoneId, preferLocalPending],
  );
  activeZoneIdRef.current = activeZone?.zoneId ?? "";
  activeZoneRevRef.current = activeZone?.revision ?? 0;

  const selectZone = useCallback((zoneId: string) => {
    setSelectedZoneId(zoneId);
    saveZoneId(zoneId);
    setError("");
  }, []);

  const replaceSelectedZoneIfMatches = useCallback((previousZoneId: string, nextZoneId: string) => {
    setSelectedZoneId((prev) => {
      if (prev === previousZoneId) {
        saveZoneId(nextZoneId);
        return nextZoneId;
      }
      return prev;
    });
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

  const startRadio = useCallback(
    async (kind: RadioSeedKind, id: string) => {
      if (!activeZoneId) {
        setError("No UPnP renderer selected or discovered");
        return undefined;
      }
      if (radioInFlightRef.current) return undefined;
      const key = radioSeedKey(kind, id);
      radioInFlightRef.current = key;
      radioStartRevRef.current = activeZoneRevRef.current;
      setRadioLoadingKey(key);
      try {
        return await runAction(
          () => tacetClient.zone(activeZoneId).startRadio({ kind, id }),
          (res) => upsertZone(res.zone),
        );
      } finally {
        radioInFlightRef.current = null;
        setRadioLoadingKey((cur) => (cur === key ? null : cur));
      }
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
    (sec: number) => {
      if (!activeZoneId) return Promise.resolve(undefined);
      const targetSec = Math.max(0, Math.round(sec));
      return runAction(() => tacetClient.zone(activeZoneId).seek(targetSec), upsertZone);
    },
    [activeZoneId, runAction, upsertZone],
  );

  const setVolume = useCallback(
    (vol: number) => {
      if (!activeZoneId) return Promise.resolve(undefined);
      const targetVol = Math.max(0, Math.min(100, Math.round(vol)));
      return runAction(() => tacetClient.zone(activeZoneId).setVolume(targetVol), upsertZone);
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
    radioLoadingKey,
    selectZone,
    replaceSelectedZoneIfMatches,
    discoverZones,
    connectZone,
    setStreamMode,
    setQobuzMaxQuality,
    setLibraryAutoScan,
    playSource,
    addSource,
    startRadio,
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
