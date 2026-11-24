import { Check, Layers, Play } from "lucide-react";
import { memo } from "react";
import { fmtAlbumQuality, fmtCount, isHiRes } from "@/api/format";
import type { Album, QueueSource } from "@/api/types";
import { AlbumArt } from "@/components/ui/artwork";
import { useAlbumHoverPrefetch } from "@/lib/prefetch";
import { cn } from "@/lib/utils";

interface AlbumCardProps {
  album: Album;
  subtitle?: string;
  showInLibraryBadge?: boolean;
  onOpen: (album: Album) => void;
  onPlay?: (source: QueueSource, startIndex?: number) => void;
  className?: string;
}

function shortHiRes(album: Album): string {
  if (!album.maxBitDepth || !album.maxSampleRate) return "";
  if (album.maxBitDepth === 1 && album.maxSampleRate >= 2822400) {
    return "DSD";
  }
  return "HI-RES";
}

export const AlbumCard = memo(function AlbumCard({
  album,
  subtitle,
  showInLibraryBadge = false,
  onOpen,
  onPlay,
  className,
}: AlbumCardProps) {
  const hiRes = isHiRes(album.maxBitDepth, album.maxSampleRate);
  const hiResLabel = hiRes ? shortHiRes(album) : "";
  const hoverPrefetch = useAlbumHoverPrefetch(album);

  return (
    <div
      {...hoverPrefetch}
      className={cn(
        "group relative flex flex-col rounded-xl p-2.5 -m-2.5 transition-colors hover:bg-white/[0.045]",
        className,
      )}
    >
      <div className="relative aspect-square w-full overflow-hidden rounded-lg shadow-[0_8px_24px_rgba(0,0,0,0.45)] ring-1 ring-white/[0.08]">
        <button
          type="button"
          onClick={() => onOpen(album)}
          className="block h-full w-full text-left cursor-pointer focus-visible:outline-none"
          aria-label={`Open ${album.title} by ${album.artist}`}
        >
          <AlbumArt
            artworkUrl={album.artworkUrl}
            alt={album.title}
            className="h-full w-full transition-transform duration-300 group-hover:scale-[1.03]"
          />
        </button>

        {/* Top-left library membership badge (used in Qobuz Discover) */}
        {showInLibraryBadge && album.inLibrary && (
          <span className="pointer-events-none absolute left-2 top-2 inline-flex items-center gap-1 rounded-full bg-black/75 px-2 py-0.5 text-[10px] font-medium text-emerald-300 backdrop-blur-md border border-emerald-400/25">
            <Check className="h-2.5 w-2.5" />
            Library
          </span>
        )}

        {/* Bottom-left version count badge */}
        {(album.versionCount ?? 1) > 1 && (
          <span className="pointer-events-none absolute bottom-2 left-2 inline-flex items-center gap-1 rounded-full bg-black/75 px-2 py-0.5 text-[10px] font-medium text-white/90 backdrop-blur-md border border-white/10">
            <Layers className="h-2.5 w-2.5" />
            {fmtCount(album.versionCount)}
          </span>
        )}

        {/* Bottom-right hi-res pill (fades out on hover when play button appears) */}
        {hiResLabel && (
          <span
            title={fmtAlbumQuality(album)}
            className={cn(
              "pointer-events-none absolute bottom-2 right-2 rounded-md bg-black/60 px-1.5 py-0.5 text-[9px] font-semibold tracking-[0.06em] text-[#dfc07b] backdrop-blur-md border border-white/[0.12] transition-opacity",
              onPlay && "group-hover:opacity-0",
            )}
          >
            {hiResLabel}
          </span>
        )}

        {/* Spotify-style floating hover Play button */}
        {onPlay && (
          <button
            type="button"
            aria-label={`Play ${album.title}`}
            onClick={(e) => {
              e.stopPropagation();
              onPlay({ type: "album", albumId: album.id }, 0);
            }}
            className="absolute bottom-2.5 right-2.5 flex h-10 w-10 translate-y-1.5 items-center justify-center rounded-full bg-white text-black shadow-xl opacity-0 transition-all duration-200 group-hover:translate-y-0 group-hover:opacity-100 hover:scale-105 active:scale-95 cursor-pointer"
          >
            <Play className="h-4 w-4 fill-current translate-x-[1px]" />
          </button>
        )}
      </div>

      <button
        type="button"
        onClick={() => onOpen(album)}
        className="mt-2.5 text-left cursor-pointer focus-visible:outline-none min-w-0"
      >
        <div className="truncate text-[13.5px] font-medium leading-snug text-foreground group-hover:text-white">
          {album.title}
        </div>
        <div className="truncate text-xs text-muted-foreground mt-0.5">
          {subtitle ?? album.artist}
        </div>
      </button>
    </div>
  );
});
