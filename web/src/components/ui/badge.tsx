import { cva, type VariantProps } from "class-variance-authority";
import type { HTMLAttributes } from "react";
import { cn } from "@/lib/utils";

const badgeVariants = cva(
  "inline-flex items-center gap-1 rounded-full border px-2 py-0.5 text-[11px] font-medium tracking-tight transition-colors",
  {
    variants: {
      variant: {
        default: "border-white/[0.1] bg-white/[0.06] text-foreground/90",
        muted: "border-white/[0.07] bg-white/[0.04] text-muted-foreground",
        hires: "border-[#d4af68]/25 bg-[#d4af68]/[0.08] text-[#dfc07b] font-mono",
        local: "border-emerald-400/25 bg-emerald-400/10 text-emerald-300",
        qobuz: "border-sky-400/25 bg-sky-400/10 text-sky-300",
        live: "border-emerald-400/30 bg-emerald-400/10 text-emerald-300",
      },
    },
    defaultVariants: {
      variant: "default",
    },
  },
);

export interface BadgeProps extends HTMLAttributes<HTMLSpanElement>, VariantProps<typeof badgeVariants> {}

export function Badge({ className, variant, ...props }: BadgeProps) {
  return <span className={cn(badgeVariants({ variant }), className)} {...props} />;
}
