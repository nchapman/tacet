import { cn } from "@/lib/utils";

interface PlayingBarsProps {
  className?: string;
}

const BARS = [
  { duration: "0.48s", delay: "-0.12s", rest: "60%" },
  { duration: "0.60s", delay: "-0.36s", rest: "100%" },
  { duration: "0.44s", delay: "-0.06s", rest: "70%" },
  { duration: "0.55s", delay: "-0.28s", rest: "88%" },
] as const;

export function PlayingBars({ className }: PlayingBarsProps) {
  return (
    <span
      aria-label="Playing"
      className={cn("inline-flex h-3.5 items-end gap-[2px]", className)}
    >
      {BARS.map((bar, i) => (
        <span
          key={i}
          className="eq-bar w-[2px] rounded-full bg-emerald-400"
          style={{
            height: bar.rest,
            animationDuration: bar.duration,
            animationDelay: bar.delay,
          }}
        />
      ))}
    </span>
  );
}
