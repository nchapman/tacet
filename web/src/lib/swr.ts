import { useCallback, useEffect, useRef, useState, useSyncExternalStore } from "react";

interface CacheEntry<T = unknown> {
  data: T;
  updatedAt: number;
  version?: string;
}

interface InflightEntry<T = unknown> {
  promise: Promise<T>;
  controller: AbortController;
  refCount: number;
}

export interface FetchHandle<T> {
  promise: Promise<T>;
  /** Releases this caller's interest; aborts the underlying request once no callers remain. */
  cancel: () => void;
}

export interface SwrOptions<T> {
  /** Milliseconds before a cached entry is revalidated in the background. Default: 30s. */
  staleTimeMs?: number;
  /** External cache version (e.g. libraryStatus.etag). When it changes, cached data renders immediately and revalidates in the background. */
  version?: string;
  /** Immediate placeholder data (e.g. route navigation state) used only when the cache has no entry for this key yet. */
  fallbackData?: T;
  /** Delay before firing a network request on cache miss / stale revalidation (useful for search inputs). Cached fresh hits still return synchronously. */
  debounceMs?: number;
  /** When the key changes, keep displaying the previous key's data until the new key resolves (prevents skeleton flashes when changing sort/filter). */
  keepPreviousData?: boolean;
}

export interface SwrResult<T> {
  data: T | undefined;
  error: string;
  /** True only when there is no cached/fallback/previous data to display yet and a fetch is active. */
  isLoading: boolean;
  /** True whenever a network fetch or background revalidation is in flight. */
  isValidating: boolean;
  /** Optimistically update the cached entry for the active key. */
  mutate: (next: T) => void;
}

const DEFAULT_MAX_ENTRIES = 200;
const DEFAULT_STALE_TIME_MS = 30_000;

function makeInflightKey(key: string, version?: string): string {
  return version !== undefined ? `${key}\0${version}` : key;
}

export class SwrCache {
  private readonly maxEntries: number;
  private readonly entries = new Map<string, CacheEntry>();
  private readonly inflight = new Map<string, InflightEntry>();
  private readonly listeners = new Map<string, Set<() => void>>();
  private readonly latestSeq = new Map<string, number>();
  private seqCounter = 0;

  constructor(maxEntries = DEFAULT_MAX_ENTRIES) {
    this.maxEntries = maxEntries;
  }

  get<T>(key: string): CacheEntry<T> | undefined {
    const entry = this.entries.get(key);
    if (!entry) return undefined;
    // Promote to most-recently-used.
    this.entries.delete(key);
    this.entries.set(key, entry);
    return entry as CacheEntry<T>;
  }

  /** Non-mutating read used inside useSyncExternalStore getSnapshot. */
  peek<T>(key: string): CacheEntry<T> | undefined {
    return this.entries.get(key) as CacheEntry<T> | undefined;
  }

  set<T>(key: string, data: T, version?: string): void {
    this.latestSeq.set(key, ++this.seqCounter);
    this.entries.delete(key);
    this.entries.set(key, { data, updatedAt: Date.now(), version });
    while (this.entries.size > this.maxEntries) {
      const oldestKey = this.entries.keys().next().value;
      if (oldestKey === undefined) break;
      this.entries.delete(oldestKey);
      this.latestSeq.delete(oldestKey);
    }
    this.notify(key);
  }

  /** Marks matching entries stale without deleting them so UI stays populated while revalidating. */
  invalidatePrefix(prefix: string): void {
    const touched: string[] = [];
    for (const [k, entry] of this.entries) {
      if (k.startsWith(prefix)) {
        // Replace the entry object so useSyncExternalStore observes a new snapshot reference.
        this.entries.set(k, { ...entry, updatedAt: 0 });
        touched.push(k);
      }
    }
    for (const k of touched) {
      this.notify(k);
    }
  }

  isFresh(key: string, staleTimeMs: number, version?: string): boolean {
    const entry = this.entries.get(key);
    if (!entry) return false;
    if (version !== undefined && entry.version !== version) return false;
    return Date.now() - entry.updatedAt < staleTimeMs;
  }

  /**
   * Starts or joins an in-flight fetch for (key, version) with reference-counted AbortSignal cancellation.
   * Aborts the underlying fetch only once every active caller cancels.
   */
  fetch<T>(key: string, fetcher: (signal: AbortSignal) => Promise<T>, version?: string): FetchHandle<T> {
    const ik = makeInflightKey(key, version);
    let rec = this.inflight.get(ik) as InflightEntry<T> | undefined;
    if (rec && !rec.controller.signal.aborted) {
      rec.refCount++;
    } else {
      const controller = new AbortController();
      const seq = ++this.seqCounter;
      this.latestSeq.set(key, seq);
      const entryRec: InflightEntry<T> = {
        controller,
        refCount: 1,
        promise: Promise.resolve() as Promise<T>,
      };
      entryRec.promise = fetcher(controller.signal)
        .then((data) => {
          if (!controller.signal.aborted && this.latestSeq.get(key) === seq) {
            this.writeEntry(key, data, version);
          }
          return data;
        })
        .finally(() => {
          if (this.inflight.get(ik) === entryRec) {
            this.inflight.delete(ik);
          }
        });
      this.inflight.set(ik, entryRec);
      rec = entryRec;
    }

    const activeRec = rec;
    let released = false;
    return {
      promise: activeRec.promise,
      cancel: () => {
        if (released) return;
        released = true;
        activeRec.refCount = Math.max(0, activeRec.refCount - 1);
        if (activeRec.refCount === 0) {
          if (this.inflight.get(ik) === activeRec) {
            this.inflight.delete(ik);
          }
          activeRec.controller.abort();
        }
      },
    };
  }

  subscribe(key: string, listener: () => void): () => void {
    let set = this.listeners.get(key);
    if (!set) {
      set = new Set();
      this.listeners.set(key, set);
    }
    set.add(listener);
    return () => {
      const cur = this.listeners.get(key);
      if (!cur) return;
      cur.delete(listener);
      if (cur.size === 0) {
        this.listeners.delete(key);
      }
    };
  }

  size(): number {
    return this.entries.size;
  }

  clear(): void {
    const keys = Array.from(this.listeners.keys());
    for (const rec of this.inflight.values()) {
      rec.controller.abort();
    }
    this.entries.clear();
    this.inflight.clear();
    this.latestSeq.clear();
    for (const k of keys) {
      this.notify(k);
    }
  }

  private writeEntry<T>(key: string, data: T, version?: string): void {
    this.entries.delete(key);
    this.entries.set(key, { data, updatedAt: Date.now(), version });
    while (this.entries.size > this.maxEntries) {
      const oldestKey = this.entries.keys().next().value;
      if (oldestKey === undefined) break;
      this.entries.delete(oldestKey);
      this.latestSeq.delete(oldestKey);
    }
    this.notify(key);
  }

  private notify(key: string): void {
    const set = this.listeners.get(key);
    if (!set) return;
    for (const fn of set) {
      fn();
    }
  }
}

export const swrCache = new SwrCache(DEFAULT_MAX_ENTRIES);

export function mutateSwr<T>(key: string, data: T, version?: string): void {
  swrCache.set(key, data, version);
}

export function invalidateSwrPrefix(prefix: string): void {
  swrCache.invalidatePrefix(prefix);
}

export function clearSwrCache(): void {
  swrCache.clear();
}

/**
 * Stale-while-revalidate hook backed by a global bounded LRU cache.
 * Reads cached entries synchronously during render so Back navigation and
 * revisited views paint immediately with zero skeleton flash.
 */
export function useSWR<T>(
  key: string | null,
  fetcher: (signal: AbortSignal) => Promise<T>,
  options?: SwrOptions<T>,
): SwrResult<T> {
  const staleTimeMs = options?.staleTimeMs ?? DEFAULT_STALE_TIME_MS;
  const version = options?.version;
  const debounceMs = options?.debounceMs ?? 0;
  const keepPreviousData = options?.keepPreviousData ?? false;
  const fallbackData = options?.fallbackData;

  const fetcherRef = useRef(fetcher);
  fetcherRef.current = fetcher;

  const subscribe = useCallback(
    (onStoreChange: () => void) => {
      if (!key) return () => {};
      return swrCache.subscribe(key, onStoreChange);
    },
    [key],
  );

  const getSnapshot = useCallback((): CacheEntry<T> | undefined => {
    if (!key) return undefined;
    return swrCache.peek<T>(key);
  }, [key]);

  const entry = useSyncExternalStore(subscribe, getSnapshot, getSnapshot);

  // Track last non-undefined data when keepPreviousData is enabled so filter/sort
  // changes don't flash empty skeletons.
  const prevDataRef = useRef<T | undefined>(undefined);
  if (entry !== undefined) {
    prevDataRef.current = entry.data;
  } else if (key === null) {
    prevDataRef.current = undefined;
  }

  const [validatingKey, setValidatingKey] = useState<string | null>(null);
  const [errorState, setErrorState] = useState<{ key: string; message: string } | null>(null);

  useEffect(() => {
    if (!key) {
      setValidatingKey(null);
      return;
    }

    // Touch LRU order when a cached entry is viewed.
    const existing = swrCache.get<T>(key);
    if (existing && swrCache.isFresh(key, staleTimeMs, version)) {
      setValidatingKey((cur) => (cur === key ? null : cur));
      setErrorState((cur) => (cur?.key === key ? null : cur));
      return;
    }

    let cancelled = false;
    let activeHandle: FetchHandle<T> | null = null;
    setValidatingKey(key);
    setErrorState((cur) => (cur?.key === key ? null : cur));

    const run = () => {
      activeHandle = swrCache.fetch(key, (signal) => fetcherRef.current(signal), version);
      void activeHandle.promise
        .then(() => {
          if (!cancelled) {
            setErrorState((cur) => (cur?.key === key ? null : cur));
          }
        })
        .catch((err) => {
          if (!cancelled) {
            setErrorState({
              key,
              message: err instanceof Error ? err.message : String(err),
            });
          }
        })
        .finally(() => {
          if (!cancelled) {
            setValidatingKey((cur) => (cur === key ? null : cur));
          }
        });
    };

    if (debounceMs > 0) {
      const timer = window.setTimeout(run, debounceMs);
      return () => {
        cancelled = true;
        window.clearTimeout(timer);
        activeHandle?.cancel();
      };
    }

    run();
    return () => {
      cancelled = true;
      activeHandle?.cancel();
    };
  }, [key, staleTimeMs, version, debounceMs, entry?.updatedAt]);

  const mutate = useCallback(
    (next: T) => {
      if (key) {
        swrCache.set(key, next, version);
      }
    },
    [key, version],
  );

  const data =
    entry !== undefined
      ? entry.data
      : fallbackData !== undefined
        ? fallbackData
        : keepPreviousData
          ? prevDataRef.current
          : undefined;

  const isValidating = Boolean(key && validatingKey === key);
  const error = errorState && errorState.key === key ? errorState.message : "";
  const isLoading = Boolean(key && data === undefined && !error);

  return { data, error, isLoading, isValidating, mutate };
}
