import { useRef, type ReactNode } from "react";
import { useScrollRestoration } from "@/lib/scroll";
import { cn } from "@/lib/utils";

interface PageHeaderProps {
  children: ReactNode;
  className?: string;
}

/**
 * Fixed 64px page header separated from the scrolling content below by a subtle
 * hairline border, matching the Artists rail header without sticky blur layers.
 */
export function PageHeader({ children, className }: PageHeaderProps) {
  return (
    <div
      className={cn(
        "flex min-h-16 shrink-0 items-center border-b border-white/[0.06] px-4 sm:px-8",
        className,
      )}
    >
      <div className="flex w-full items-center justify-between">{children}</div>
    </div>
  );
}

interface PageBodyProps {
  children: ReactNode;
  className?: string;
  /** Scroll restoration namespace (defaults to "stage"). Pass false to disable. */
  scrollNamespace?: string | false;
}

/**
 * Independently scrollable page viewport positioned below a fixed PageHeader,
 * with automatic Back/Forward scroll restoration.
 */
export function PageBody({ children, className, scrollNamespace = "stage" }: PageBodyProps) {
  const ref = useRef<HTMLDivElement>(null);
  useScrollRestoration(ref, {
    namespace: scrollNamespace === false ? "stage" : scrollNamespace,
    enabled: scrollNamespace !== false,
  });

  return (
    <div
      ref={ref}
      data-scroll-stage=""
      className={cn("relative flex-1 min-h-0 overflow-y-auto pb-12", className)}
    >
      {children}
    </div>
  );
}
