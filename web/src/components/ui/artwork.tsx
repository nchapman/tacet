import { Disc3 } from "lucide-react";
import { useEffect, useState } from "react";
import { tacetClient } from "@/api/client";
import { cn } from "@/lib/utils";

interface AlbumArtProps {
  artworkUrl?: string;
  alt: string;
  className?: string;
  iconClassName?: string;
  priority?: boolean;
}

/** Crisp cover artwork with a quiet dark vinyl fallback. */
export function AlbumArt({ artworkUrl, alt, className, iconClassName, priority }: AlbumArtProps) {
  const src = tacetClient.resolveUrl(artworkUrl);
  return (
    <div className={cn("relative overflow-hidden bg-zinc-900/90 flex items-center justify-center", className)}>
      {src ? (
        <img
          src={src}
          alt={alt}
          loading={priority ? "eager" : "lazy"}
          decoding="async"
          className="h-full w-full object-cover"
        />
      ) : (
        <Disc3 className={cn("h-1/3 w-1/3 text-zinc-700", iconClassName)} />
      )}
    </div>
  );
}

interface AmbientBackdropProps {
  artworkUrl?: string;
  className?: string;
  intensity?: "subtle" | "hero" | "full";
}

/**
 * Plexamp / Apple Music-style album-art-driven ambient backdrop.
 * Renders a smooth crossfading, heavily blurred full-bleed color wash that
 * tints the entire app shell (including translucent sidebar, headers, and player bar)
 * while keeping dark obsidian contrast crisp and readable everywhere.
 */
export function AmbientBackdrop({ artworkUrl, className, intensity = "hero" }: AmbientBackdropProps) {
  const nextSrc = tacetClient.resolveUrl(artworkUrl) || "";
  const [activeSrc, setActiveSrc] = useState(nextSrc);
  const [prevSrc, setPrevSrc] = useState("");
  const [entering, setEntering] = useState(false);

  useEffect(() => {
    if (nextSrc === activeSrc) return;
    setPrevSrc(activeSrc);
    setActiveSrc(nextSrc);
    setEntering(true);
    const raf = window.requestAnimationFrame(() => setEntering(false));
    const timer = window.setTimeout(() => setPrevSrc(""), 800);
    return () => {
      window.cancelAnimationFrame(raf);
      window.clearTimeout(timer);
    };
  }, [nextSrc, activeSrc]);

  const opacityClass =
    intensity === "full"
      ? "opacity-55"
      : intensity === "hero"
        ? "opacity-45"
        : "opacity-30";

  const imgClass =
    "absolute inset-0 h-full w-full scale-125 object-cover object-center blur-[100px] saturate-[1.7] brightness-90 transition-opacity duration-700 ease-out will-change-[opacity]";

  return (
    <div aria-hidden="true" className={cn("pointer-events-none absolute inset-0 overflow-hidden select-none", className)}>
      {/* Quiet atmospheric fallback when no artwork is active */}
      <div
        className={cn(
          "absolute inset-0 bg-[radial-gradient(ellipse_at_top_left,rgba(56,189,248,0.14),rgba(16,185,129,0.07)_42%,transparent_75%)] transition-opacity duration-700",
          activeSrc ? "opacity-0" : "opacity-100",
        )}
      />

      {/* Outgoing artwork layer for seamless crossfades */}
      {prevSrc && (
        <img
          key={`prev:${prevSrc}`}
          src={prevSrc}
          alt=""
          className={cn(imgClass, entering ? opacityClass : "opacity-0")}
        />
      )}

      {/* Active artwork layer */}
      {activeSrc && (
        <img
          key={`curr:${activeSrc}`}
          src={activeSrc}
          alt=""
          className={cn(imgClass, entering ? "opacity-0" : opacityClass)}
        />
      )}

      {/* Smooth vertical + radial obsidian scrim to keep text contrast crisp without hard seams */}
      <div
        className={cn(
          "absolute inset-0",
          intensity === "full"
            ? "bg-gradient-to-b from-black/25 via-[#09090b]/55 to-[#09090b]/92"
            : "bg-gradient-to-b from-black/15 via-[#09090b]/58 to-[#09090b]/94",
        )}
      />
      <div className="absolute inset-0 bg-[radial-gradient(ellipse_at_top,transparent_0%,rgba(9,9,11,0.45)_75%,rgba(9,9,11,0.75)_100%)]" />
    </div>
  );
}
