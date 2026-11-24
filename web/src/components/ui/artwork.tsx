import { Disc3 } from "lucide-react";
import { memo, useEffect, useLayoutEffect, useRef, useState } from "react";
import { tacetClient } from "@/api/client";
import {
  extractUltraBlurPalette,
  getCachedUltraBlurPalette,
  type UltraBlurPalette,
} from "@/lib/ultrablur";
import { cn } from "@/lib/utils";

export type ArtworkSize = "sm" | "lg";

const MAX_LOADED_ART_CACHE = 2000;
const loadedArtUrls = new Set<string>();

export function isArtworkLoaded(src: string): boolean {
  return Boolean(src) && loadedArtUrls.has(src);
}

export function markArtworkLoaded(src: string): void {
  if (!src) return;
  if (loadedArtUrls.size >= MAX_LOADED_ART_CACHE && !loadedArtUrls.has(src)) {
    const oldest = loadedArtUrls.values().next().value;
    if (oldest) loadedArtUrls.delete(oldest);
  }
  loadedArtUrls.add(src);
}

/**
 * Resolves cover art URLs and appends a fixed width bucket (?w=400 or ?w=1000)
 * for server-hosted /media/art/ endpoints while leaving external CDN URLs untouched.
 */
export function resolveArtworkSrc(artworkUrl?: string, size: ArtworkSize = "sm"): string {
  const resolved = tacetClient.resolveUrl(artworkUrl);
  if (!resolved) return "";
  if (!resolved.includes("/media/art/")) return resolved;
  const sep = resolved.includes("?") ? "&" : "?";
  if (/[?&]w=/.test(resolved)) return resolved;
  return `${resolved}${sep}w=${size === "lg" ? 1000 : 400}`;
}

interface AlbumArtProps {
  artworkUrl?: string;
  alt: string;
  size?: ArtworkSize;
  className?: string;
  iconClassName?: string;
  priority?: boolean;
}

/**
 * Crisp, zero-blink cover artwork:
 * - Already-loaded / memory-cached images paint synchronously on Frame 1 at full opacity.
 * - Large /media/art/ heroes reuse any already-loaded small thumbnail as an instant base layer
 *   while the high-res bucket loads so opening an album never flashes a blank square.
 * - Cold network images stay hidden until decoded and fade in over 150ms (no progressive tearing).
 */
export const AlbumArt = memo(function AlbumArt({
  artworkUrl,
  alt,
  size = "sm",
  className,
  iconClassName,
  priority,
}: AlbumArtProps) {
  const src = resolveArtworkSrc(artworkUrl, size);
  const smFallbackSrc =
    size === "lg" && src && artworkUrl?.includes("/media/art/")
      ? resolveArtworkSrc(artworkUrl, "sm")
      : "";
  const hasCachedFallback = Boolean(smFallbackSrc && smFallbackSrc !== src && loadedArtUrls.has(smFallbackSrc));

  const imgRef = useRef<HTMLImageElement | null>(null);
  const [loadedSrc, setLoadedSrc] = useState<string>(() => (src && loadedArtUrls.has(src) ? src : ""));
  const [failedSrc, setFailedSrc] = useState<string>("");

  const isLoaded = Boolean(src) && (loadedArtUrls.has(src) || loadedSrc === src);
  const isFailed = Boolean(src) && failedSrc === src;

  useLayoutEffect(() => {
    const img = imgRef.current;
    if (src && img && img.complete && img.naturalWidth > 0) {
      markArtworkLoaded(src);
      setLoadedSrc(src);
    }
  }, [src]);

  const eager = Boolean(priority || isLoaded);

  return (
    <div className={cn("relative overflow-hidden bg-white/[0.05] flex items-center justify-center", className)}>
      {src && !isFailed ? (
        <>
          {hasCachedFallback && !isLoaded && (
            <img
              src={smFallbackSrc}
              alt=""
              aria-hidden="true"
              loading="eager"
              decoding="sync"
              className="absolute inset-0 h-full w-full object-cover"
            />
          )}
          <img
            ref={imgRef}
            src={src}
            alt={alt}
            loading={eager ? "eager" : "lazy"}
            decoding={eager ? "sync" : "async"}
            onLoad={() => {
              markArtworkLoaded(src);
              setLoadedSrc(src);
            }}
            onError={() => setFailedSrc(src)}
            className={cn(
              "relative h-full w-full object-cover",
              hasCachedFallback
                ? isLoaded
                  ? "opacity-100"
                  : "opacity-0"
                : cn(
                    "transition-opacity duration-150 ease-out motion-reduce:transition-none",
                    isLoaded ? "opacity-100" : "opacity-0",
                  ),
            )}
          />
        </>
      ) : (
        <Disc3 className={cn("h-1/3 w-1/3 text-white/20", iconClassName)} />
      )}
    </div>
  );
});

const DEFAULT_PALETTE: UltraBlurPalette = {
  primary: "rgb(42, 98, 128)",
  secondary: "rgb(36, 112, 92)",
  accent: "rgb(52, 68, 116)",
  deep: "rgb(26, 36, 56)",
};

// Tiny fractal noise SVG data URI for subtle 8-bit dark-mode anti-banding dither.
const NOISE_DATA_URI =
  "url(\"data:image/svg+xml,%3Csvg xmlns='http://www.w3.org/2000/svg' width='160' height='160'%3E%3Cfilter id='n'%3E%3CfeTurbulence type='fractalNoise' baseFrequency='0.8' numOctaves='3' stitchTiles='stitch'/%3E%3C/filter%3E%3Crect width='100%25' height='100%25' filter='url(%23n)' opacity='1'/%3E%3C/svg%3E\")";

function meshBackground(p: UltraBlurPalette): string {
  return [
    `radial-gradient(64% 58% at 15% 12%, ${p.primary} 0%, transparent 72%)`,
    `radial-gradient(58% 52% at 86% 14%, ${p.secondary} 0%, transparent 70%)`,
    `radial-gradient(54% 58% at 20% 72%, ${p.accent} 0%, transparent 72%)`,
    `radial-gradient(60% 56% at 84% 78%, ${p.deep} 0%, transparent 76%)`,
  ].join(", ");
}

interface AmbientBackdropProps {
  artworkUrl?: string;
  className?: string;
  intensity?: "subtle" | "hero" | "full";
}

interface ResolvedLayer {
  key: string;
  palette: UltraBlurPalette | null;
  fallbackSrc: string;
}

function initialLayer(src: string): ResolvedLayer {
  if (!src) {
    return { key: "default", palette: DEFAULT_PALETTE, fallbackSrc: "" };
  }
  const cached = getCachedUltraBlurPalette(src);
  if (cached) {
    return { key: `pal:${src}`, palette: cached, fallbackSrc: "" };
  }
  if (cached === null) {
    return { key: `img:${src}`, palette: null, fallbackSrc: src };
  }
  return { key: "default", palette: DEFAULT_PALETTE, fallbackSrc: "" };
}

/**
 * Plexamp-style UltraBlur ambient backdrop.
 * Samples 4 perceptually normalized dominant colors from the active cover art
 * into a lush, multi-point radial mesh gradient that smoothly crossfades across
 * the entire app shell (with subtle noise dithering to eliminate banding and an
 * automatic blurred-image fallback if a remote CDN blocks CORS canvas reads).
 */
export const AmbientBackdrop = memo(function AmbientBackdrop({ artworkUrl, className, intensity = "hero" }: AmbientBackdropProps) {
  const src = resolveArtworkSrc(artworkUrl, "sm");
  const [active, setActive] = useState<ResolvedLayer>(() => initialLayer(src));
  const [prev, setPrev] = useState<ResolvedLayer | null>(null);
  const [entering, setEntering] = useState(false);
  const activeRef = useRef<ResolvedLayer>(active);

  useEffect(() => {
    let cancelled = false;
    let raf = 0;
    let clearTimer = 0;

    const commit = (next: ResolvedLayer) => {
      if (cancelled || activeRef.current.key === next.key) return;
      setPrev(activeRef.current);
      activeRef.current = next;
      setActive(next);
      setEntering(true);
      raf = window.requestAnimationFrame(() => setEntering(false));
      clearTimer = window.setTimeout(() => setPrev(null), 900);
    };

    if (!src) {
      commit({ key: "default", palette: DEFAULT_PALETTE, fallbackSrc: "" });
      return () => {
        cancelled = true;
        window.cancelAnimationFrame(raf);
        window.clearTimeout(clearTimer);
      };
    }

    const cached = getCachedUltraBlurPalette(src);
    if (cached) {
      commit({ key: `pal:${src}`, palette: cached, fallbackSrc: "" });
    } else if (cached === null) {
      commit({ key: `img:${src}`, palette: null, fallbackSrc: src });
    } else {
      void extractUltraBlurPalette(src).then((pal) => {
        if (pal) {
          commit({ key: `pal:${src}`, palette: pal, fallbackSrc: "" });
        } else {
          commit({ key: `img:${src}`, palette: null, fallbackSrc: src });
        }
      });
    }

    return () => {
      cancelled = true;
      window.cancelAnimationFrame(raf);
      window.clearTimeout(clearTimer);
    };
  }, [src]);

  const meshOpacity =
    intensity === "full"
      ? "opacity-95"
      : intensity === "hero"
        ? "opacity-80"
        : "opacity-55";

  const imgOpacity =
    intensity === "full"
      ? "opacity-55"
      : intensity === "hero"
        ? "opacity-45"
        : "opacity-30";

  const renderLayer = (layer: ResolvedLayer, visible: boolean) => {
    if (layer.palette) {
      return (
        <div
          key={layer.key}
          style={{ backgroundImage: meshBackground(layer.palette) }}
          className={cn(
            "absolute inset-0 transition-opacity duration-700 ease-out will-change-[opacity]",
            visible ? meshOpacity : "opacity-0",
          )}
        />
      );
    }
    return (
      <img
        key={layer.key}
        src={layer.fallbackSrc}
        alt=""
        onError={() => {
          if (activeRef.current.key === layer.key) {
            const fallback: ResolvedLayer = { key: "default", palette: DEFAULT_PALETTE, fallbackSrc: "" };
            activeRef.current = fallback;
            setActive(fallback);
          }
        }}
        className={cn(
          "absolute inset-0 h-full w-full scale-125 object-cover object-center blur-[100px] saturate-[1.7] brightness-90 transition-opacity duration-700 ease-out will-change-[opacity]",
          visible ? imgOpacity : "opacity-0",
        )}
      />
    );
  };

  return (
    <div aria-hidden="true" className={cn("pointer-events-none absolute inset-0 overflow-hidden select-none", className)}>
      {prev && renderLayer(prev, entering)}
      {renderLayer(active, !entering)}

      {/* Smooth vertical + radial obsidian scrim to keep text contrast crisp */}
      <div
        className={cn(
          "absolute inset-0",
          intensity === "full"
            ? "bg-gradient-to-b from-black/20 via-[#09090b]/42 to-[#09090b]/76"
            : "bg-gradient-to-b from-black/15 via-[#09090b]/46 to-[#09090b]/78",
        )}
      />
      <div className="absolute inset-0 bg-[radial-gradient(ellipse_at_top,transparent_0%,rgba(9,9,11,0.34)_75%,rgba(9,9,11,0.62)_100%)]" />

      {/* Subtle anti-banding film grain */}
      <div
        className="absolute inset-0 opacity-[0.03] mix-blend-overlay"
        style={{ backgroundImage: NOISE_DATA_URI, backgroundRepeat: "repeat" }}
      />
    </div>
  );
});
