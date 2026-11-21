import { Disc3 } from "lucide-react";
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
 * Plexamp-style album-art-driven blurred gradient backdrop.
 * Uses GPU-accelerated scaled & blurred artwork blended cleanly into obsidian.
 */
export function AmbientBackdrop({ artworkUrl, className, intensity = "hero" }: AmbientBackdropProps) {
  const src = tacetClient.resolveUrl(artworkUrl);
  const opacityClass =
    intensity === "full"
      ? "opacity-45"
      : intensity === "hero"
        ? "opacity-35"
        : "opacity-20";

  return (
    <div aria-hidden="true" className={cn("pointer-events-none absolute inset-0 overflow-hidden select-none", className)}>
      {src ? (
        <img
          src={src}
          alt=""
          className={cn(
            "h-full w-full scale-125 object-cover blur-[72px] saturate-150 transition-opacity duration-700",
            opacityClass,
          )}
        />
      ) : (
        <div className="h-full w-full bg-[radial-gradient(ellipse_at_top_left,rgba(56,189,248,0.14),rgba(16,185,129,0.06)_40%,transparent_75%)]" />
      )}
      <div className="absolute inset-0 bg-gradient-to-b from-black/20 via-[#09090b]/70 to-[#09090b]" />
    </div>
  );
}
