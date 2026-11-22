import { useEffect, useRef, useState, type ReactNode } from "react";
import { cn } from "@/lib/utils";

function findScrollParent(el: HTMLElement | null): HTMLElement | null {
  let node = el?.parentElement ?? null;
  while (node) {
    const oy = window.getComputedStyle(node).overflowY;
    if (oy === "auto" || oy === "scroll") return node;
    node = node.parentElement;
  }
  return null;
}

interface PageHeaderProps {
  children: ReactNode;
  className?: string;
}

/**
 * Shared 64px sticky page header that stays transparent at the top of the page
 * so the UltraBlur gradient flows uninterrupted, then crossfades in a dark
 * frosted-glass backdrop layer once content scrolls underneath.
 */
export function PageHeader({ children, className }: PageHeaderProps) {
  const ref = useRef<HTMLDivElement>(null);
  const [scrolled, setScrolled] = useState(false);

  useEffect(() => {
    const scroller = findScrollParent(ref.current);
    if (!scroller) return;
    const onScroll = () => {
      const next = scroller.scrollTop > 8;
      setScrolled((prev) => (prev === next ? prev : next));
    };
    onScroll();
    scroller.addEventListener("scroll", onScroll, { passive: true });
    return () => scroller.removeEventListener("scroll", onScroll);
  }, []);

  return (
    <div
      ref={ref}
      className={cn(
        "sticky top-0 z-20 flex min-h-16 items-center border-b px-4 sm:px-8 transition-colors duration-200",
        scrolled ? "border-white/[0.08]" : "border-white/[0.06]",
        className,
      )}
    >
      <div
        aria-hidden="true"
        className={cn(
          "pointer-events-none absolute inset-0 bg-[#09090b]/75 backdrop-blur-2xl backdrop-saturate-150 transition-opacity duration-200 ease-out",
          scrolled ? "opacity-100" : "opacity-0",
        )}
      />
      <div className="relative z-10 flex w-full items-center justify-between">{children}</div>
    </div>
  );
}
