import * as SliderPrimitive from "@radix-ui/react-slider";
import type { ComponentPropsWithoutRef } from "react";
import { cn } from "@/lib/utils";

export function Slider({
  className,
  alwaysShowThumb = false,
  ...props
}: ComponentPropsWithoutRef<typeof SliderPrimitive.Root> & { alwaysShowThumb?: boolean }) {
  return (
    <SliderPrimitive.Root
      className={cn(
        "group relative flex w-full touch-none select-none items-center py-1.5 cursor-pointer data-[disabled]:opacity-40 data-[disabled]:cursor-default",
        className,
      )}
      {...props}
    >
      <SliderPrimitive.Track className="relative h-1 w-full grow overflow-hidden rounded-full bg-white/[0.14] group-hover:h-1.5 transition-[height]">
        <SliderPrimitive.Range className="absolute h-full rounded-full bg-white/85 group-hover:bg-emerald-400 transition-colors" />
      </SliderPrimitive.Track>
      <SliderPrimitive.Thumb
        className={cn(
          "block h-3.5 w-3.5 rounded-full bg-white shadow-md ring-offset-background focus-visible:opacity-100 focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-ring transition-opacity",
          alwaysShowThumb ? "opacity-100" : "opacity-0 group-hover:opacity-100",
        )}
      />
    </SliderPrimitive.Root>
  );
}
