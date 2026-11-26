import { Loader2, Radio } from "lucide-react";
import type { RadioSeedKind } from "@/api/types";
import { Button } from "@/components/ui/button";
import { cn } from "@/lib/utils";

export function radioSeedKey(kind: RadioSeedKind, id: string): string {
  return `${kind}:${id}`;
}

interface RadioButtonProps {
  kind: RadioSeedKind;
  seedId: string;
  onStartRadio?: (kind: RadioSeedKind, id: string) => void;
  radioLoadingKey?: string | null;
  size?: "sm" | "default";
  stopPropagation?: boolean;
  className?: string;
}

/** Pill "Radio" action button used in Album/Artist headers and cards. */
export function RadioButton({
  kind,
  seedId,
  onStartRadio,
  radioLoadingKey,
  size = "default",
  stopPropagation = false,
  className,
}: RadioButtonProps) {
  if (!onStartRadio || !seedId) return null;
  const spinning = radioLoadingKey === radioSeedKey(kind, seedId);
  const iconClass = size === "sm" ? "h-3.5 w-3.5" : "h-4 w-4";

  return (
    <Button
      type="button"
      variant="secondary"
      size={size}
      disabled={Boolean(radioLoadingKey)}
      className={cn("rounded-full", className)}
      onClick={(e) => {
        if (stopPropagation) e.stopPropagation();
        onStartRadio(kind, seedId);
      }}
    >
      {spinning ? <Loader2 className={cn(iconClass, "animate-spin")} /> : <Radio className={iconClass} />}
      Radio
    </Button>
  );
}

interface RadioIconButtonProps {
  kind: RadioSeedKind;
  seedId: string;
  label: string;
  title?: string;
  onStartRadio?: (kind: RadioSeedKind, id: string) => void;
  radioLoadingKey?: string | null;
  /** Keep hover-revealed row icons visible while this button's own spinner is active. */
  hoverReveal?: "none" | "sm" | "md";
  /** Use shadcn ghost icon-sm Button styling (PlayerBar) instead of bare row icon button. */
  variant?: "row" | "toolbar";
  stopPropagation?: boolean;
}

/** Compact icon-only Radio trigger for track rows and the player bar. */
export function RadioIconButton({
  kind,
  seedId,
  label,
  title = "Start Radio",
  onStartRadio,
  radioLoadingKey,
  hoverReveal = "none",
  variant = "row",
  stopPropagation = false,
}: RadioIconButtonProps) {
  if (!onStartRadio || !seedId) return null;
  const spinning = radioLoadingKey === radioSeedKey(kind, seedId);

  if (variant === "toolbar") {
    return (
      <Button
        type="button"
        variant="ghost"
        size="icon-sm"
        disabled={Boolean(radioLoadingKey)}
        onClick={(e) => {
          if (stopPropagation) e.stopPropagation();
          onStartRadio(kind, seedId);
        }}
        title={title}
        aria-label={label}
      >
        {spinning ? <Loader2 className="h-4 w-4 animate-spin" /> : <Radio className="h-4 w-4" />}
      </Button>
    );
  }

  const revealClass = spinning
    ? "opacity-100 text-foreground"
    : hoverReveal === "sm"
      ? "sm:opacity-0 sm:group-hover:opacity-100"
      : hoverReveal === "md"
        ? "md:opacity-0 md:group-hover:opacity-100"
        : "";

  return (
    <button
      type="button"
      disabled={Boolean(radioLoadingKey)}
      title={title}
      aria-label={label}
      onClick={(e) => {
        if (stopPropagation) e.stopPropagation();
        onStartRadio(kind, seedId);
      }}
      className={cn(
        "rounded-full p-1.5 text-muted-foreground hover:bg-white/10 hover:text-foreground transition-colors cursor-pointer disabled:cursor-default",
        revealClass,
      )}
    >
      {spinning ? <Loader2 className="h-3.5 w-3.5 animate-spin" /> : <Radio className="h-3.5 w-3.5" />}
    </button>
  );
}
