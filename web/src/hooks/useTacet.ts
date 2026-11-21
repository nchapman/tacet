import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { tacetClient } from "../api/client";
import type {
  LibraryStatus,
  QobuzQuality,
  QobuzStatus,
  QueuePosition,
  QueueSource,
  Settings,
  StreamMode,
  TransportAction,
  ZoneSnapshot,
} from "../api/types";

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

export function useTacet() {
  const [zones, setZones] = useState<ZoneSnapshot[]>([]);
  const [selectedZoneId, setSelectedZoneId] = useState<string>(readSavedZoneId);
  const [settings, setSettings] = useState<Settings>({ streamMode: "direct", qobuzMaxQuality: "flac_24_192" });
  const [qobuzStatus, setQobuzStatus] = useState<QobuzStatus | null>(null);
  const [libraryStatus, setLibraryStatus] = useState<LibraryStatus | null>(null);
  const [connection, setConnection] = useState<ConnectionStatus>("connecting");
  const [error, setError] = useState<string>("");
  const [discovering, setDiscovering] = useState(false);
  const [nowMs, setNowMs] = useState<number>(() => Date.now());

  const selectedZoneIdRef = useRef(selectedZoneId);
  selectedZoneIdRef.current = selectedZoneId;

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

  const replaceZones = useCallback((list: ZoneSnapshot[]) => {
    setZones(list);
    setSelectedZoneId((cur) => {
      if (cur && list.some((z) => z.zoneId === cur)) return cur;
      const fallback = list[0]?.zoneId ?? "";
      saveZoneId(fallback);
      return fallback;
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
        .then((res) => replaceZones(res.zones ?? []))
        .catch(() => {});
    };

    const unsubscribe = tacetClient.subscribeEvents({
      onOpen: () => setConnection("live"),
      onError: () => setConnection("reconnecting"),
      onResync: resyncAll,
      onSnapshot: (snap) => {
        replaceZones(snap.zones ?? []);
        if (snap.settings) setSettings(snap.settings);
        if (snap.qobuz) setQobuzStatus(snap.qobuz);
        if (snap.library) setLibraryStatus(snap.library);
      },
      onZonesUpdated: (ev) => {
        replaceZones(ev.zones ?? []);
      },
      onPlayback: (ev) => {
        setZones((prev) =>
          prev.map((z) => {
            if (z.zoneId !== ev.zoneId || ev.revision < z.revision) return z;
            return {
              ...z,
              revision: ev.revision,
              state: ev.state,
              positionSec: ev.positionSec,
              positionUpdatedAt: ev.positionUpdatedAt,
              durationSec: ev.durationSec,
              currentIndex: ev.currentIndex,
              currentItemId: ev.currentItemId,
              currentTrack: ev.currentTrack,
              gaplessArmed: ev.gaplessArmed,
              eventsActive: ev.eventsActive,
            };
          }),
        );
      },
      onVolume: (ev) => {
        setZones((prev) =>
          prev.map((z) => {
            if (z.zoneId !== ev.zoneId || ev.revision < z.revision) return z;
            return { ...z, revision: ev.revision, volume: ev.volume, muted: ev.muted };
          }),
        );
      },
      onQueue: (ev) => {
        setZones((prev) =>
          prev.map((z) => {
            if (z.zoneId !== ev.zoneId || ev.revision < z.revision) return z;
            return {
              ...z,
              revision: ev.revision,
              currentIndex: ev.currentIndex,
              currentItemId: ev.currentItemId,
              currentTrack: ev.currentTrack,
              queue: ev.items ?? [],
            };
          }),
        );
      },
      onZoneError: (ev) => {
        if (ev.zoneId === selectedZoneIdRef.current) {
          setError(ev.error.message);
        }
      },
      onSettingsUpdated: (s) => setSettings(s),
      onQobuzStatus: (st) => setQobuzStatus(st),
      onLibraryUpdated: (st) => setLibraryStatus(st),
    });

    return unsubscribe;
  }, [replaceZones]);

  const activeZone = useMemo<ZoneSnapshot | null>(() => {
    if (!zones.length) return null;
    return zones.find((z) => z.zoneId === selectedZoneId) ?? zones[0];
  }, [zones, selectedZoneId]);

  // Smooth client-side 1s progress interpolation while the active zone is playing,
  // avoiding unnecessary full-queue SSE frames every second.
  useEffect(() => {
    if (activeZone?.state !== "playing") return;
    setNowMs(Date.now());
    const id = window.setInterval(() => setNowMs(Date.now()), 500);
    return () => window.clearInterval(id);
  }, [activeZone?.state, activeZone?.positionUpdatedAt]);

  const interpolatedPositionSec = useMemo(() => {
    if (!activeZone) return 0;
    const base = activeZone.positionSec ?? 0;
    if (activeZone.state !== "playing") return base;
    const updatedMs = Date.parse(activeZone.positionUpdatedAt);
    if (!Number.isFinite(updatedMs)) return base;
    const elapsed = Math.max(0, Math.floor((nowMs - updatedMs) / 1000));
    const maxDur = activeZone.durationSec || activeZone.currentTrack?.durationSec || 0;
    return maxDur > 0 ? Math.min(maxDur, base + elapsed) : base + elapsed;
  }, [activeZone, nowMs]);

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
    (sec: number) => {
      if (!activeZoneId) return Promise.resolve(undefined);
      return runAction(() => tacetClient.zone(activeZoneId).seek(sec), upsertZone);
    },
    [activeZoneId, runAction, upsertZone],
  );

  const setVolume = useCallback(
    (vol: number) => {
      if (!activeZoneId) return Promise.resolve(undefined);
      return runAction(() => tacetClient.zone(activeZoneId).setVolume(vol), upsertZone);
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

  return {
    zones,
    activeZone,
    interpolatedPositionSec,
    settings,
    qobuzStatus,
    setQobuzStatus,
    libraryStatus,
    connection,
    error,
    clearError: () => setError(""),
    discovering,
    selectZone,
    discoverZones,
    connectZone,
    setStreamMode,
    setQobuzMaxQuality,
    playSource,
    addSource,
    jumpItem,
    removeItem,
    clearQueue,
    transport,
    seek,
    setVolume,
    setMute,
  };
}
