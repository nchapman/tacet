import { type RefObject, useLayoutEffect, useRef } from "react";
import { useLocation, useNavigationType } from "react-router";

const DEFAULT_MAX_SCROLL_ENTRIES = 120;
const RESTORE_TIMEOUT_MS = 1500;

export class ScrollStore {
  private readonly maxEntries: number;
  private readonly positions = new Map<string, number>();

  constructor(maxEntries = DEFAULT_MAX_SCROLL_ENTRIES) {
    this.maxEntries = maxEntries;
  }

  save(key: string, top: number): void {
    const clamped = Math.max(0, Math.round(top));
    this.positions.delete(key);
    this.positions.set(key, clamped);
    while (this.positions.size > this.maxEntries) {
      const oldest = this.positions.keys().next().value;
      if (oldest === undefined) break;
      this.positions.delete(oldest);
    }
  }

  get(key: string): number | undefined {
    const val = this.positions.get(key);
    if (val === undefined) return undefined;
    // Promote to MRU.
    this.positions.delete(key);
    this.positions.set(key, val);
    return val;
  }

  clear(): void {
    this.positions.clear();
  }

  size(): number {
    return this.positions.size;
  }
}

export const scrollStore = new ScrollStore();

interface ScrollRestorationOptions {
  /** Namespace to separate independent scroll containers (e.g. "stage", "artists-rail", "artists-detail"). */
  namespace?: string;
  /** Optional override for the route key (defaults to pathname + search). */
  routeKey?: string;
  /** Disable restoration when this container is hidden/inactive. */
  enabled?: boolean;
  /** Reset scrollTop to 0 on PUSH navigations (default true). Set false for persistent split-pane rails. */
  resetOnPush?: boolean;
}

/**
 * Preserves and restores scrollTop for an overflow-y scroll container across
 * browser Back/Forward and in-app navigate(-1) transitions.
 *
 * Restores synchronously inside useLayoutEffect (pre-paint) so SWR-cached lists
 * land at their exact previous pixel with zero jump, with a brief user-cancellable
 * ResizeObserver fallback when a route had a cold cache miss.
 */
export function useScrollRestoration(
  containerRef: RefObject<HTMLElement | null>,
  options?: ScrollRestorationOptions,
): void {
  const location = useLocation();
  const navigationType = useNavigationType();

  const namespace = options?.namespace ?? "stage";
  const enabled = options?.enabled ?? true;
  const resetOnPush = options?.resetOnPush ?? true;
  const routeKey = options?.routeKey ?? `${location.pathname}${location.search}`;
  const locStoreKey = `loc:${namespace}:${location.key}`;
  const routeStoreKey = `route:${namespace}:${routeKey}`;

  const restoringRef = useRef(false);

  useLayoutEffect(() => {
    const el = containerRef.current;
    if (!enabled || !el) return;

    let stopped = false;
    let observer: ResizeObserver | null = null;
    let timeoutId: number | null = null;

    const stopAsyncRestore = () => {
      if (stopped) return;
      stopped = true;
      restoringRef.current = false;
      observer?.disconnect();
      observer = null;
      if (timeoutId !== null) {
        window.clearTimeout(timeoutId);
        timeoutId = null;
      }
      el.removeEventListener("wheel", stopAsyncRestore);
      el.removeEventListener("touchstart", stopAsyncRestore);
      el.removeEventListener("pointerdown", stopAsyncRestore);
    };

    const onScroll = () => {
      if (restoringRef.current) return;
      scrollStore.save(locStoreKey, el.scrollTop);
      scrollStore.save(routeStoreKey, el.scrollTop);
    };

    if (navigationType === "POP") {
      const savedTop = scrollStore.get(locStoreKey) ?? scrollStore.get(routeStoreKey) ?? 0;
      if (savedTop > 0) {
        restoringRef.current = true;
        el.scrollTop = savedTop;

        if (Math.abs(el.scrollTop - savedTop) <= 2) {
          restoringRef.current = false;
        } else {
          // Cold cache fallback: wait briefly for async data to grow content height.
          const tryApply = () => {
            if (stopped) return;
            el.scrollTop = savedTop;
            if (Math.abs(el.scrollTop - savedTop) <= 2) {
              stopAsyncRestore();
            }
          };

          if (typeof ResizeObserver !== "undefined") {
            observer = new ResizeObserver(tryApply);
            observer.observe(el);
            if (el.firstElementChild) {
              observer.observe(el.firstElementChild);
            }
          }
          el.addEventListener("wheel", stopAsyncRestore, { passive: true });
          el.addEventListener("touchstart", stopAsyncRestore, { passive: true });
          el.addEventListener("pointerdown", stopAsyncRestore, { passive: true });
          timeoutId = window.setTimeout(stopAsyncRestore, RESTORE_TIMEOUT_MS);
        }
      } else {
        el.scrollTop = 0;
      }
    } else if (navigationType === "PUSH" && resetOnPush) {
      el.scrollTop = 0;
    }
    // On REPLACE (e.g. canonicalizing album URL or typing in Search), keep current scrollTop.

    el.addEventListener("scroll", onScroll, { passive: true });

    return () => {
      // Do not re-read el.scrollTop here: React runs layout cleanup after mutating
      // the container's children, so a shorter incoming route may already have
      // clamped el.scrollTop. The passive scroll listener already captured the true position.
      el.removeEventListener("scroll", onScroll);
      stopAsyncRestore();
    };
  }, [containerRef, enabled, locStoreKey, routeStoreKey, navigationType, resetOnPush]);
}
