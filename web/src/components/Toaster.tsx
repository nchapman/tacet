import { AlertCircle, CheckCircle2, Radio, X } from "lucide-react";
import { useEffect, useState } from "react";
import type { ToastItem } from "@/hooks/useToast";
import { cn } from "@/lib/utils";

interface ToasterProps {
  toasts: ToastItem[];
  onDismiss: (id: number) => void;
  error?: string;
  onDismissError?: () => void;
}

function ToastCard({ item, onDismiss }: { item: ToastItem; onDismiss: (id: number) => void }) {
  const [entered, setEntered] = useState(false);

  useEffect(() => {
    const raf = window.requestAnimationFrame(() => setEntered(true));
    return () => window.cancelAnimationFrame(raf);
  }, []);

  const visible = entered && !item.closing;
  const isError = item.tone === "error";
  const isRadio = item.tone === "radio";

  return (
    <div
      role={isError ? "alert" : "status"}
      className={cn(
        "pointer-events-auto flex items-center gap-2.5 rounded-xl border px-3.5 py-2.5 text-xs shadow-2xl backdrop-blur-xl transition-all duration-200 ease-out",
        visible ? "translate-y-0 opacity-100 scale-100" : "translate-y-2 opacity-0 scale-[0.97]",
        isError
          ? "border-red-500/30 bg-[#1a1012]/95 text-red-200"
          : "border-white/15 bg-[#121216]/90 text-foreground",
      )}
    >
      {isError ? (
        <AlertCircle className="h-4 w-4 shrink-0 text-red-400" />
      ) : isRadio ? (
        <Radio className="h-4 w-4 shrink-0 text-white/85" />
      ) : (
        <CheckCircle2 className="h-4 w-4 shrink-0 text-white/80" />
      )}
      <div className="min-w-0 flex-1">
        <div className="truncate font-medium leading-tight">{item.title}</div>
        {item.detail && <div className="truncate text-[11px] text-muted-foreground mt-0.5">{item.detail}</div>}
      </div>
      <button
        type="button"
        onClick={() => onDismiss(item.id)}
        aria-label="Dismiss notification"
        className="rounded-full p-0.5 text-muted-foreground hover:bg-white/10 hover:text-foreground transition-colors cursor-pointer"
      >
        <X className="h-3.5 w-3.5" />
      </button>
    </div>
  );
}

/** Bottom-right toast stack positioned cleanly above the player bar. */
export function Toaster({ toasts, onDismiss, error, onDismissError }: ToasterProps) {
  if (!error && toasts.length === 0) return null;

  return (
    <div
      aria-live="polite"
      className="pointer-events-none fixed bottom-[138px] inset-x-3 lg:inset-x-auto lg:bottom-[94px] lg:right-4 z-40 flex flex-col gap-2 lg:w-[340px] lg:max-w-md"
    >
      {error && (
        <div
          role="alert"
          className="pointer-events-auto flex items-center gap-2.5 rounded-xl border border-red-500/30 bg-[#1a1012]/95 px-3.5 py-2.5 text-xs text-red-200 shadow-2xl backdrop-blur-xl transition-all duration-200 ease-out"
        >
          <AlertCircle className="h-4 w-4 shrink-0 text-red-400" />
          <span className="flex-1">{error}</span>
          {onDismissError && (
            <button
              type="button"
              onClick={onDismissError}
              aria-label="Dismiss error"
              className="rounded-full p-0.5 hover:bg-white/10 cursor-pointer"
            >
              <X className="h-3.5 w-3.5" />
            </button>
          )}
        </div>
      )}
      {toasts.map((t) => (
        <ToastCard key={t.id} item={t} onDismiss={onDismiss} />
      ))}
    </div>
  );
}
