import { cn } from "@/lib/utils";

interface SignalBadgeProps {
  provider?: string;
  format?: string;
  size?: "sm" | "md";
  showTitle?: boolean;
  className?: string;
}

function normalizeProvider(provider?: string): { label: string; isLocal: boolean; isQobuz: boolean } {
  const p = (provider || "").toLowerCase();
  if (p === "local") return { label: "Local", isLocal: true, isQobuz: false };
  if (p === "qobuz") return { label: "Qobuz", isLocal: false, isQobuz: true };
  return {
    label: p ? p.charAt(0).toUpperCase() + p.slice(1) : "",
    isLocal: false,
    isQobuz: false,
  };
}

export function SignalBadge({ provider, format, size = "md", showTitle = true, className }: SignalBadgeProps) {
  const { label: srcLabel, isLocal, isQobuz } = normalizeProvider(provider);
  const displayFormat = format?.trim() || "";

  if (!srcLabel && !displayFormat) return null;

  const title = [
    isQobuz ? "Streaming from Qobuz" : isLocal ? "Local library" : srcLabel,
    format,
  ]
    .filter(Boolean)
    .join(" · ");

  return (
    <span
      title={showTitle ? title : undefined}
      className={cn(
        "inline-flex shrink-0 items-center rounded-full border border-white/[0.08] bg-white/[0.05] whitespace-nowrap",
        size === "sm" ? "gap-1.5 px-2 py-0.5 text-[10px] leading-none" : "gap-2 px-2.5 py-1 text-[11px] leading-none",
        className,
      )}
    >
      {srcLabel && (
        <span
          className={cn(
            "font-medium",
            isLocal ? "text-emerald-300/90" : isQobuz ? "text-sky-300/90" : "text-foreground/80",
          )}
        >
          {srcLabel}
        </span>
      )}
      {srcLabel && displayFormat && <span className="h-2.5 w-px bg-white/15" aria-hidden="true" />}
      {displayFormat && <span className="font-mono text-foreground/75">{displayFormat}</span>}
    </span>
  );
}
