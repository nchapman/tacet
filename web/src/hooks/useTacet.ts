import { useCallback, useEffect, useRef, useState } from "react";
import { tacetClient } from "../api/client";
import type { QueueTrackRequest, RendererInfo, StreamMode, TransportAction, ZoneSnapshot } from "../api/types";

export type ConnectionStatus = "connecting" | "live" | "reconnecting";

const EMPTY_SNAPSHOT: ZoneSnapshot = {
  renderer: null,
  state: "IDLE",
  positionSec: 0,
  durationSec: 0,
  volume: 50,
  muted: false,
  hasVolume: false,
  queue: [],
  currentIndex: -1,
  gaplessArmed: false,
  eventsActive: false,
  streamMode: "direct",
};

export function useTacet() {
  const [renderers, setRenderers] = useState<RendererInfo[]>([]);
  const [activeId, setActiveId] = useState<string>("");
  const [streamMode, setStreamModeState] = useState<StreamMode>("direct");
  const [snapshot, setSnapshot] = useState<ZoneSnapshot>(EMPTY_SNAPSHOT);
  const [connection, setConnection] = useState<ConnectionStatus>("connecting");
  const [error, setError] = useState<string>("");
  const [discovering, setDiscovering] = useState(false);

  const activeIdRef = useRef(activeId);
  activeIdRef.current = activeId;

  const applySnapshot = useCallback((snap: ZoneSnapshot) => {
    setSnapshot(snap);
    if (snap.renderer?.id) {
      setActiveId(snap.renderer.id);
    }
    if (snap.streamMode) {
      setStreamModeState(snap.streamMode);
    }
    setError(snap.lastError ?? "");
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

  useEffect(() => {
    let cancelled = false;
    void (async () => {
      try {
        const list = await tacetClient.listRenderers();
        if (cancelled) return;
        setRenderers(list.renderers ?? []);
        setActiveId(list.activeId ?? "");
        setStreamModeState(list.streamMode ?? "direct");
        const snap = await tacetClient.getState(list.activeId || undefined);
        if (!cancelled) applySnapshot(snap);
      } catch (err) {
        if (!cancelled) setError(err instanceof Error ? err.message : String(err));
      }
    })();

    const unsubscribe = tacetClient.subscribeEvents({
      onOpen: () => setConnection("live"),
      onError: () => setConnection("reconnecting"),
      onRenderers: (list) => {
        setRenderers(list ?? []);
        if (!activeIdRef.current && list.length > 0) {
          setActiveId(list[0].id);
        }
      },
      onState: (snap) => {
        if (!activeIdRef.current || !snap.renderer || snap.renderer.id === activeIdRef.current) {
          applySnapshot(snap);
        }
      },
    });

    return () => {
      cancelled = true;
      unsubscribe();
    };
  }, [applySnapshot]);

  const selectRenderer = useCallback(
    async (id: string) => {
      await runAction(() => tacetClient.selectRenderer(id), applySnapshot);
    },
    [applySnapshot, runAction],
  );

  const discoverRenderers = useCallback(async () => {
    setDiscovering(true);
    try {
      await runAction(
        () => tacetClient.discoverRenderers(),
        (res) => {
          setRenderers(res.renderers ?? []);
          setActiveId(res.activeId ?? "");
        },
      );
    } finally {
      setDiscovering(false);
    }
  }, [runAction]);

  const connectRenderer = useCallback(
    async (url: string): Promise<boolean> => {
      const res = await runAction(
        () => tacetClient.connectRenderer(url),
        (data) => {
          setRenderers(data.renderers ?? []);
          setActiveId(data.activeId ?? "");
        },
      );
      if (!res) return false;
      if (res.activeId) {
        const snap = await runAction(() => tacetClient.getState(res.activeId), applySnapshot);
        return Boolean(snap);
      }
      return true;
    },
    [applySnapshot, runAction],
  );

  const setStreamMode = useCallback(
    async (mode: StreamMode) => {
      await runAction(
        () => tacetClient.setStreamMode(mode),
        (res) => setStreamModeState(res.streamMode),
      );
    },
    [runAction],
  );

  const playQueue = useCallback(
    (req: QueueTrackRequest) => runAction(() => tacetClient.playQueue({ rendererId: activeId || undefined, ...req }), applySnapshot),
    [activeId, applySnapshot, runAction],
  );

  const enqueue = useCallback(
    (req: QueueTrackRequest) => runAction(() => tacetClient.enqueue({ rendererId: activeId || undefined, ...req }), applySnapshot),
    [activeId, applySnapshot, runAction],
  );

  const jumpQueue = useCallback(
    (index: number) => runAction(() => tacetClient.jumpQueue(index, activeId || undefined), applySnapshot),
    [activeId, applySnapshot, runAction],
  );

  const removeFromQueue = useCallback(
    (index: number) => runAction(() => tacetClient.removeFromQueue(index, activeId || undefined), applySnapshot),
    [activeId, applySnapshot, runAction],
  );

  const clearQueue = useCallback(
    () => runAction(() => tacetClient.clearQueue(activeId || undefined), applySnapshot),
    [activeId, applySnapshot, runAction],
  );

  const transport = useCallback(
    (action: TransportAction) => runAction(() => tacetClient.transport(action, activeId || undefined), applySnapshot),
    [activeId, applySnapshot, runAction],
  );

  const seek = useCallback(
    (sec: number) => runAction(() => tacetClient.seek(sec, activeId || undefined), applySnapshot),
    [activeId, applySnapshot, runAction],
  );

  const setVolume = useCallback(
    (vol: number) => runAction(() => tacetClient.setVolume(vol, activeId || undefined), applySnapshot),
    [activeId, applySnapshot, runAction],
  );

  const setMute = useCallback(
    (muted: boolean) => runAction(() => tacetClient.setMute(muted, activeId || undefined), applySnapshot),
    [activeId, applySnapshot, runAction],
  );

  return {
    renderers,
    activeId,
    streamMode,
    snapshot,
    connection,
    error,
    clearError: () => setError(""),
    discovering,
    selectRenderer,
    discoverRenderers,
    connectRenderer,
    setStreamMode,
    playQueue,
    enqueue,
    jumpQueue,
    removeFromQueue,
    clearQueue,
    transport,
    seek,
    setVolume,
    setMute,
  };
}
