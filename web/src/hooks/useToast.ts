import { useCallback, useEffect, useRef, useState } from "react";

export type ToastTone = "info" | "radio" | "error";

export interface ToastInput {
  tone?: ToastTone;
  title: string;
  detail?: string;
  /** Auto-dismiss delay in milliseconds (default 3800; 0 disables auto-dismiss). */
  durationMs?: number;
}

export interface ToastItem {
  id: number;
  tone: ToastTone;
  title: string;
  detail?: string;
  closing: boolean;
}

const DEFAULT_DURATION_MS = 3800;
const EXIT_ANIMATION_MS = 220;
const MAX_VISIBLE_TOASTS = 3;

export function useToast() {
  const [toasts, setToasts] = useState<ToastItem[]>([]);
  const nextIdRef = useRef(1);
  const timersRef = useRef<Map<number, number>>(new Map());

  const clearTimer = useCallback((id: number) => {
    const handle = timersRef.current.get(id);
    if (handle !== undefined) {
      window.clearTimeout(handle);
      timersRef.current.delete(id);
    }
  }, []);

  const dismissToast = useCallback(
    (id: number) => {
      clearTimer(id);
      setToasts((prev) => prev.map((t) => (t.id === id ? { ...t, closing: true } : t)));
      const exitHandle = window.setTimeout(() => {
        timersRef.current.delete(id);
        setToasts((prev) => prev.filter((t) => t.id !== id));
      }, EXIT_ANIMATION_MS);
      timersRef.current.set(id, exitHandle);
    },
    [clearTimer],
  );

  const pushToast = useCallback(
    ({ tone = "info", title, detail, durationMs = DEFAULT_DURATION_MS }: ToastInput) => {
      const id = nextIdRef.current++;
      setToasts((prev) => {
        const closing = prev.filter((t) => t.closing);
        const active = prev.filter((t) => !t.closing);
        const keptActive =
          active.length >= MAX_VISIBLE_TOASTS ? active.slice(-(MAX_VISIBLE_TOASTS - 1)) : active;
        return [...closing, ...keptActive, { id, tone, title, detail, closing: false }];
      });
      if (durationMs > 0) {
        const handle = window.setTimeout(() => dismissToast(id), durationMs);
        timersRef.current.set(id, handle);
      }
      return id;
    },
    [dismissToast],
  );

  useEffect(() => {
    return () => {
      for (const handle of timersRef.current.values()) {
        window.clearTimeout(handle);
      }
      timersRef.current.clear();
    };
  }, []);

  return { toasts, pushToast, dismissToast };
}
