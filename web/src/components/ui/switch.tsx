import * as SwitchPrimitive from "@radix-ui/react-switch";
import type { ComponentPropsWithoutRef } from "react";
import { cn } from "@/lib/utils";

export function Switch({
  className,
  ...props
}: ComponentPropsWithoutRef<typeof SwitchPrimitive.Root>) {
  return (
    <SwitchPrimitive.Root
      className={cn(
        "peer inline-flex h-5 w-9 shrink-0 cursor-pointer items-center rounded-full border border-white/10 transition-colors focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-ring disabled:cursor-not-allowed disabled:opacity-50 data-[state=checked]:bg-white data-[state=unchecked]:bg-white/15",
        className,
      )}
      {...props}
    >
      <SwitchPrimitive.Thumb className="pointer-events-none block h-3.5 w-3.5 rounded-full shadow-sm ring-0 transition-transform data-[state=checked]:translate-x-4 data-[state=checked]:bg-zinc-950 data-[state=unchecked]:translate-x-0.5 data-[state=unchecked]:bg-white/85" />
    </SwitchPrimitive.Root>
  );
}
