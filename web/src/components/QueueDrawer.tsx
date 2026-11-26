import { ListMusic, Play, Trash2, X } from "lucide-react";
import { memo, useEffect, useState } from "react";
import { fmtCount, fmtDuration } from "@/api/format";
import type { QueueItem, QueuePosition, QueueSource } from "@/api/types";
import { QueueRelatedTab } from "@/components/QueueRelatedTab";
import { AlbumArt } from "@/components/ui/artwork";
import { Button } from "@/components/ui/button";
import { Dialog, DialogContent, DialogTitle } from "@/components/ui/dialog";
import { useArtistHoverPrefetch } from "@/lib/prefetch";
import { cn } from "@/lib/utils";

interface QueueContentProps {
  items: QueueItem[];
  currentItemId?: string;
  onJump: (itemId: string) => void;
  onRemove: (itemId: string) => void;
  onClear: () => void;
  onAdd?: (source: QueueSource, position?: QueuePosition) => void;
  onOpenArtist?: (artist: string) => void;
  onClose?: () => void;
}

type QueueTab = "up-next" | "related";

export function QueueContent({
  items,
  currentItemId,
  onJump,
  onRemove,
  onClear,
  onAdd,
  onOpenArtist,
  onClose,
}: QueueContentProps) {
  const [tab, setTab] = useState<QueueTab>("up-next");
  const currentIdx = items.findIndex((it) => it.itemId === currentItemId);
  const current = currentIdx >= 0 ? items[currentIdx] : undefined;
  const upNext = currentIdx >= 0 ? items.slice(currentIdx + 1) : items;
  const history = currentIdx > 0 ? items.slice(0, currentIdx) : [];

  return (
    <div className="flex h-full flex-col">
      <div className="border-b border-white/[0.06] shrink-0">
        <div className="flex items-center justify-between px-4 pt-3 pb-2">
          <div className="flex items-center gap-2">
            <ListMusic className="h-4 w-4 text-muted-foreground" />
            <h2 className="text-sm font-semibold tracking-tight">Queue</h2>
            {items.length > 0 && (
              <span className="rounded-full bg-white/[0.07] px-2 py-0.5 text-[11px] font-mono text-muted-foreground">
                {fmtCount(items.length)}
              </span>
            )}
          </div>
          <div className="flex items-center gap-1">
            {tab === "up-next" && items.length > 0 && (
              <Button
                type="button"
                variant="ghost"
                size="sm"
                className="h-7 px-2 text-xs text-muted-foreground hover:text-foreground"
                onClick={onClear}
              >
                Clear
              </Button>
            )}
            {onClose && (
              <Button type="button" variant="ghost" size="icon-sm" onClick={onClose} aria-label="Close queue">
                <X className="h-4 w-4" />
              </Button>
            )}
          </div>
        </div>

        <div role="tablist" aria-label="Queue sections" className="grid grid-cols-2 px-2">
          <button
            type="button"
            role="tab"
            aria-selected={tab === "up-next"}
            onClick={() => setTab("up-next")}
            className={cn(
              "border-b-2 py-2 text-[11px] font-semibold uppercase tracking-wider transition-colors cursor-pointer",
              tab === "up-next"
                ? "border-white text-white"
                : "border-transparent text-muted-foreground hover:text-foreground",
            )}
          >
            Up Next
          </button>
          <button
            type="button"
            role="tab"
            aria-selected={tab === "related"}
            onClick={() => setTab("related")}
            className={cn(
              "border-b-2 py-2 text-[11px] font-semibold uppercase tracking-wider transition-colors cursor-pointer",
              tab === "related"
                ? "border-white text-white"
                : "border-transparent text-muted-foreground hover:text-foreground",
            )}
          >
            Related
          </button>
        </div>
      </div>

      <div className="flex-1 overflow-y-auto px-3 py-3 space-y-5">
        {tab === "related" ? (
          <QueueRelatedTab currentTrack={current?.track} onAdd={onAdd} onOpenArtist={onOpenArtist} />
        ) : items.length === 0 ? (
          <div className="flex flex-col items-center justify-center py-16 text-center">
            <ListMusic className="h-8 w-8 text-white/20 mb-3" />
            <p className="text-sm font-medium text-foreground/80">Queue is empty</p>
            <p className="mt-1 text-xs text-muted-foreground max-w-[200px]">
              Play an album or add tracks to build your listening session.
            </p>
          </div>
        ) : (
          <>
            {current && (
              <div>
                <div className="px-2 pb-2 text-[11px] font-semibold uppercase tracking-wider text-muted-foreground">
                  Now Playing
                </div>
                <QueueRow item={current} active onJump={onJump} onRemove={onRemove} onOpenArtist={onOpenArtist} />
              </div>
            )}

            {upNext.length > 0 && (
              <div>
                <div className="px-2 pb-2 text-[11px] font-semibold uppercase tracking-wider text-muted-foreground">
                  Next Up
                </div>
                <div className="space-y-0.5">
                  {upNext.map((item) => (
                    <QueueRow key={item.itemId} item={item} onJump={onJump} onRemove={onRemove} onOpenArtist={onOpenArtist} />
                  ))}
                </div>
              </div>
            )}

            {history.length > 0 && (
              <div>
                <div className="px-2 pb-2 text-[11px] font-semibold uppercase tracking-wider text-muted-foreground/70">
                  Played Earlier
                </div>
                <div className="space-y-0.5 opacity-65 hover:opacity-100 transition-opacity">
                  {history.map((item) => (
                    <QueueRow key={item.itemId} item={item} onJump={onJump} onRemove={onRemove} onOpenArtist={onOpenArtist} />
                  ))}
                </div>
              </div>
            )}
          </>
        )}
      </div>
    </div>
  );
}

function QueueRow({
  item,
  active,
  onJump,
  onRemove,
  onOpenArtist,
}: {
  item: QueueItem;
  active?: boolean;
  onJump: (id: string) => void;
  onRemove: (id: string) => void;
  onOpenArtist?: (artist: string) => void;
}) {
  const artistPrefetch = useArtistHoverPrefetch(item.track.artist);

  return (
    <div
      className={cn(
        "group flex items-center gap-2.5 rounded-xl px-2 py-1.5",
        active ? "bg-white/[0.08]" : "hover:bg-white/[0.06]",
      )}
    >
      <button
        type="button"
        onClick={() => onJump(item.itemId)}
        aria-label={`Play ${item.track.title}`}
        className="relative h-10 w-10 shrink-0 rounded-md overflow-hidden cursor-pointer"
      >
        <AlbumArt artworkUrl={item.track.artworkUrl} alt={item.track.album || item.track.title} className="h-full w-full" />
        <div className="absolute inset-0 flex items-center justify-center bg-black/50 opacity-0 group-hover:opacity-100 transition-opacity">
          <Play className="h-3.5 w-3.5 fill-white text-white" />
        </div>
      </button>
      <div className="min-w-0 flex-1">
        <button
          type="button"
          onClick={() => onJump(item.itemId)}
          className={cn(
            "block max-w-full truncate text-left text-[13px] font-medium leading-tight cursor-pointer",
            active ? "text-emerald-400" : "text-foreground hover:text-white",
          )}
        >
          {item.track.title}
        </button>
        {item.track.artist &&
          (onOpenArtist ? (
            <button
              type="button"
              {...artistPrefetch}
              onClick={() => onOpenArtist(item.track.artist)}
              className="mt-0.5 block max-w-full truncate text-left text-xs text-muted-foreground hover:text-foreground hover:underline cursor-pointer"
            >
              {item.track.artist}
            </button>
          ) : (
            <div className="truncate text-xs text-muted-foreground mt-0.5">{item.track.artist}</div>
          ))}
      </div>
      <span className="text-[11px] font-mono tabular-nums text-muted-foreground shrink-0 group-hover:hidden">
        {fmtDuration(item.track.durationSec)}
      </span>
      <button
        type="button"
        onClick={() => onRemove(item.itemId)}
        aria-label={`Remove ${item.track.title}`}
        className="hidden group-hover:inline-flex h-7 w-7 items-center justify-center rounded-full text-muted-foreground hover:bg-white/10 hover:text-foreground transition-colors cursor-pointer shrink-0"
      >
        <Trash2 className="h-3.5 w-3.5" />
      </button>
    </div>
  );
}

interface QueueDrawerProps extends QueueContentProps {
  open: boolean;
  onOpenChange: (open: boolean) => void;
}

export const DesktopQueueDrawer = memo(function DesktopQueueDrawer({ open, onOpenChange, ...props }: QueueDrawerProps) {
  if (!open) return null;
  return (
    <aside
      aria-label="Playback Queue"
      className="hidden lg:flex w-80 xl:w-[340px] shrink-0 flex-col border-l border-white/[0.07] bg-black/30"
    >
      <QueueContent {...props} onClose={() => onOpenChange(false)} />
    </aside>
  );
});

function useIsMobileViewport(): boolean {
  const [mobile, setMobile] = useState(() =>
    typeof window !== "undefined" ? !window.matchMedia("(min-width: 1024px)").matches : false,
  );
  useEffect(() => {
    const mq = window.matchMedia("(min-width: 1024px)");
    const onChange = () => setMobile(!mq.matches);
    onChange();
    mq.addEventListener("change", onChange);
    return () => mq.removeEventListener("change", onChange);
  }, []);
  return mobile;
}

export function MobileQueueSheet({ open, onOpenChange, ...props }: QueueDrawerProps) {
  const isMobile = useIsMobileViewport();
  if (!isMobile) return null;
  return (
    <Dialog open={open} onOpenChange={onOpenChange}>
      <DialogContent
        hideClose
        className="top-auto bottom-0 left-0 right-0 translate-x-0 translate-y-0 w-full max-w-none h-[82dvh] rounded-t-3xl rounded-b-none p-0 bg-[#101014] border-t border-white/[0.1] border-x-0 border-b-0"
      >
        <DialogTitle className="sr-only">Playback Queue</DialogTitle>
        <QueueContent {...props} onClose={() => onOpenChange(false)} />
      </DialogContent>
    </Dialog>
  );
}
