import { type RefObject, useLayoutEffect, useMemo, useRef, useState } from "react";

export interface VirtualRange {
  startIndex: number;
  /** Inclusive end index, or -1 when totalCount === 0. */
  endIndex: number;
  offsetY: number;
  totalHeight: number;
}

export interface PageWindow {
  /** 0-based page numbers intersecting [startIndex, endIndex]. */
  visiblePages: number[];
  /** Adjacent 0-based page numbers to warm ahead/behind near boundaries. */
  prefetchPages: number[];
}

/**
 * Computes the slice of fixed-height rows to mount for a scroll container while
 * sizing the container to the full totalCount * rowHeight so the native scrollbar
 * represents the entire dataset.
 */
export function computeVirtualRange(
  scrollTop: number,
  viewportHeight: number,
  totalCount: number,
  rowHeight: number,
  overscan = 12,
  paddingTop = 0,
): VirtualRange {
  if (totalCount <= 0 || rowHeight <= 0) {
    return { startIndex: 0, endIndex: -1, offsetY: 0, totalHeight: 0 };
  }

  const totalHeight = totalCount * rowHeight;
  const effectiveViewport = Math.max(viewportHeight, rowHeight);
  const maxScrollTop = Math.max(0, totalHeight - effectiveViewport);
  const effectiveTop = Math.min(maxScrollTop, Math.max(0, scrollTop - paddingTop));

  const visibleStart = Math.floor(effectiveTop / rowHeight);
  const visibleEnd = Math.ceil((effectiveTop + effectiveViewport) / rowHeight) - 1;

  const startIndex = Math.max(0, Math.min(totalCount - 1, visibleStart - overscan));
  const endIndex = Math.max(startIndex, Math.min(totalCount - 1, visibleEnd + overscan));

  return {
    startIndex,
    endIndex,
    offsetY: startIndex * rowHeight,
    totalHeight,
  };
}

/**
 * Computes the 0-based random-access pages needed to cover [startIndex, endIndex]
 * plus optional lookahead/lookbehind prefetch pages when close to a page boundary.
 */
export function computeRequiredPages(
  startIndex: number,
  endIndex: number,
  totalCount: number,
  pageSize: number,
  prefetchMargin = 25,
): PageWindow {
  if (totalCount <= 0 || pageSize <= 0 || endIndex < startIndex) {
    return { visiblePages: [], prefetchPages: [] };
  }

  const maxPage = Math.floor((totalCount - 1) / pageSize);
  const firstPage = Math.max(0, Math.min(maxPage, Math.floor(startIndex / pageSize)));
  const lastPage = Math.max(firstPage, Math.min(maxPage, Math.floor(endIndex / pageSize)));

  const visiblePages: number[] = [];
  for (let p = firstPage; p <= lastPage; p++) {
    visiblePages.push(p);
  }

  const prefetchPages: number[] = [];
  if (firstPage > 0 && startIndex - firstPage * pageSize < prefetchMargin) {
    prefetchPages.push(firstPage - 1);
  }
  if (lastPage < maxPage && (lastPage + 1) * pageSize - 1 - endIndex < prefetchMargin) {
    prefetchPages.push(lastPage + 1);
  }

  return { visiblePages, prefetchPages };
}

interface UseVirtualWindowOptions {
  scrollRef: RefObject<HTMLElement | null>;
  totalCount: number;
  rowHeight: number;
  pageSize: number;
  overscan?: number;
  prefetchMargin?: number;
  paddingTop?: number;
  /** Debounce uncached page fetches during fast scrollbar scrubbing (ms). */
  settleMs?: number;
}

export interface VirtualWindowState extends VirtualRange, PageWindow {
  /** False briefly while the user is rapidly scrubbing the scrollbar across large jumps. */
  settled: boolean;
}

export function useVirtualWindow({
  scrollRef,
  totalCount,
  rowHeight,
  pageSize,
  overscan = 12,
  prefetchMargin = 25,
  paddingTop = 0,
  settleMs = 80,
}: UseVirtualWindowOptions): VirtualWindowState {
  const [metrics, setMetrics] = useState({ scrollTop: 0, viewportHeight: 800 });
  const [settled, setSettled] = useState(true);
  const lastScrollTopRef = useRef(0);

  useLayoutEffect(() => {
    const el = scrollRef.current;
    if (!el) return;

    let rafId: number | null = null;
    let settleTimer: number | null = null;

    const readMetrics = () => {
      const nextTop = el.scrollTop;
      const nextHeight = el.clientHeight > 0 ? el.clientHeight : 800;
      lastScrollTopRef.current = nextTop;
      setMetrics((prev) =>
        prev.scrollTop === nextTop && prev.viewportHeight === nextHeight
          ? prev
          : { scrollTop: nextTop, viewportHeight: nextHeight },
      );
    };

    // Sync immediately before paint (picks up scrollTop restored by PageBody).
    readMetrics();

    const onScroll = () => {
      const nextTop = el.scrollTop;
      const delta = Math.abs(nextTop - lastScrollTopRef.current);
      const jumpThreshold = Math.max((el.clientHeight || 800) * 2, rowHeight * 12);

      if (delta > jumpThreshold) {
        setSettled(false);
        if (settleTimer !== null) window.clearTimeout(settleTimer);
        settleTimer = window.setTimeout(() => {
          settleTimer = null;
          setSettled(true);
        }, settleMs);
      }

      if (rafId === null) {
        rafId = window.requestAnimationFrame(() => {
          rafId = null;
          readMetrics();
        });
      }
    };

    el.addEventListener("scroll", onScroll, { passive: true });

    let observer: ResizeObserver | null = null;
    if (typeof ResizeObserver !== "undefined") {
      observer = new ResizeObserver(readMetrics);
      observer.observe(el);
    } else {
      window.addEventListener("resize", readMetrics, { passive: true });
    }

    return () => {
      el.removeEventListener("scroll", onScroll);
      if (observer) {
        observer.disconnect();
      } else {
        window.removeEventListener("resize", readMetrics);
      }
      if (rafId !== null) window.cancelAnimationFrame(rafId);
      if (settleTimer !== null) window.clearTimeout(settleTimer);
    };
  }, [scrollRef, rowHeight, settleMs]);

  const range = useMemo(
    () =>
      computeVirtualRange(
        metrics.scrollTop,
        metrics.viewportHeight,
        totalCount,
        rowHeight,
        overscan,
        paddingTop,
      ),
    [metrics.scrollTop, metrics.viewportHeight, totalCount, rowHeight, overscan, paddingTop],
  );

  const pages = useMemo(
    () => computeRequiredPages(range.startIndex, range.endIndex, totalCount, pageSize, prefetchMargin),
    [range.startIndex, range.endIndex, totalCount, pageSize, prefetchMargin],
  );

  return {
    ...range,
    ...pages,
    settled,
  };
}
