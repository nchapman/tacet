import { Cloud, Cpu, Disc3, HardDrive, Radio, Speaker, Waves } from "lucide-react";
import { useEffect, useRef, useState } from "react";
import { createPortal } from "react-dom";
import { fmtQuality, isHiResOutput, rendererLossyCodec } from "@/api/format";
import type { AudioQuality, StreamMode, Track, ZoneSnapshot } from "@/api/types";
import { SignalBadge } from "@/components/ui/SignalBadge";
import { cn } from "@/lib/utils";

interface SignalPathPopoverProps {
  track: Track;
  zone: ZoneSnapshot | null;
  qualLabel: string;
  isBrowserActive: boolean;
  streamMode?: StreamMode;
  variant?: "pill" | "led";
}

interface Stage {
  icon: typeof Disc3;
  title: string;
  detail: string;
  meta?: string;
  accent?: "emerald" | "sky" | "amber" | "neutral";
}

function fmtSize(bytes?: number): string {
  if (!bytes || bytes <= 0) return "";
  if (bytes >= 1024 * 1024) return `${(bytes / (1024 * 1024)).toFixed(1)} MB`;
  return `${Math.round(bytes / 1024)} KB`;
}

function fmtChannels(channels?: number): string {
  if (!channels || channels <= 0) return "";
  if (channels === 1) return "Mono";
  if (channels === 2) return "Stereo";
  if (channels === 6) return "5.1";
  return `${channels}ch`;
}

function isLosslessTrack(track: Track): boolean {
  const mime = (track.mime || "").toLowerCase();
  const ext = (track.ext || "").toLowerCase();
  return (
    mime.includes("flac") ||
    mime.includes("wav") ||
    mime.includes("aiff") ||
    mime.includes("alac") ||
    mime.includes("dsd") ||
    mime.includes("dsf") ||
    ext === ".flac" ||
    ext === ".wav" ||
    ext === ".aiff" ||
    ext === ".aif" ||
    ext === ".dsf" ||
    ext === ".dff"
  );
}

function opusBitrate(q: AudioQuality): string {
  switch (q) {
    case "opus_96":
      return "96 kbps";
    case "opus_128":
      return "128 kbps";
    case "opus_160":
      return "160 kbps";
    case "opus_256":
      return "256 kbps";
    default:
      return "";
  }
}

export function SignalPathPopover({
  track,
  zone,
  qualLabel,
  isBrowserActive,
  streamMode,
  variant = "pill",
}: SignalPathPopoverProps) {
  const [open, setOpen] = useState(false);
  const [pinned, setPinned] = useState(false);
  const [anchor, setAnchor] = useState<{ left: number; bottom: number } | null>(null);
  const closeTimer = useRef<number | null>(null);
  const rootRef = useRef<HTMLDivElement>(null);
  const popoverRef = useRef<HTMLDivElement>(null);

  const updateAnchor = () => {
    const el = rootRef.current;
    if (!el) return;
    const r = el.getBoundingClientRect();
    const popWidth = Math.min(344, window.innerWidth - 24);
    setAnchor({
      left: Math.max(12, Math.min(r.left, window.innerWidth - popWidth - 12)),
      bottom: window.innerHeight - r.top + 10,
    });
  };

  const clearTimer = () => {
    if (closeTimer.current !== null) {
      window.clearTimeout(closeTimer.current);
      closeTimer.current = null;
    }
  };

  const handleEnter = () => {
    clearTimer();
    updateAnchor();
    setOpen(true);
  };

  const handleLeave = () => {
    if (pinned) return;
    clearTimer();
    closeTimer.current = window.setTimeout(() => setOpen(false), 140);
  };

  useEffect(() => {
    if (!open) return;
    updateAnchor();
    window.addEventListener("resize", updateAnchor);
    return () => window.removeEventListener("resize", updateAnchor);
  }, [open]);

  useEffect(() => {
    if (!pinned) return;
    const onDocDown = (e: MouseEvent) => {
      const target = e.target as Node;
      if (
        rootRef.current &&
        !rootRef.current.contains(target) &&
        (!popoverRef.current || !popoverRef.current.contains(target))
      ) {
        setPinned(false);
        setOpen(false);
      }
    };
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") {
        setPinned(false);
        setOpen(false);
      }
    };
    document.addEventListener("mousedown", onDocDown);
    document.addEventListener("keydown", onKey);
    return () => {
      document.removeEventListener("mousedown", onDocDown);
      document.removeEventListener("keydown", onKey);
    };
  }, [pinned]);

  useEffect(() => () => clearTimer(), []);

  const provider = (track.provider || track.source || "local").toLowerCase();
  const isQobuz = provider === "qobuz";
  const activeQ: AudioQuality = zone?.activeQuality || "original";
  const isOpus = activeQ.startsWith("opus_");
  const isWav = activeQ === "wav";
  const losslessSource = isLosslessTrack(track);
  const hiResOut = isHiResOutput(track, activeQ);

  const pathStatus = isOpus
    ? {
        label: "Transcoded",
        dot: "bg-amber-400 shadow-[0_0_8px_rgba(251,191,36,0.65)]",
        pill: "border-amber-400/20 bg-amber-400/[0.08] text-amber-300/95",
        spine: "from-amber-400/35 via-amber-400/20 to-white/[0.08]",
      }
    : losslessSource
      ? {
          label: hiResOut ? "Lossless · Hi-Res" : "Lossless",
          dot: "bg-emerald-400 shadow-[0_0_8px_rgba(52,211,153,0.65)]",
          pill: "border-emerald-400/20 bg-emerald-400/[0.08] text-emerald-300/95",
          spine: "from-emerald-400/35 via-emerald-400/20 to-white/[0.08]",
        }
      : {
          label: "Direct",
          dot: "bg-sky-400 shadow-[0_0_8px_rgba(56,189,248,0.65)]",
          pill: "border-sky-400/20 bg-sky-400/[0.08] text-sky-300/95",
          spine: "from-sky-400/35 via-sky-400/20 to-white/[0.08]",
        };

  const sourceQuality = fmtQuality(track);
  const sourceMeta = [fmtChannels(track.channels), !isQobuz ? fmtSize(track.size) : ""]
    .filter(Boolean)
    .join(" · ");

  const stages: Stage[] = [
    {
      icon: isQobuz ? Cloud : HardDrive,
      title: isQobuz ? "Qobuz Stream" : "Local File",
      detail: sourceQuality || (isQobuz ? "Qobuz catalog" : "Local storage"),
      meta: sourceMeta || undefined,
      accent: isQobuz ? "sky" : "emerald",
    },
  ];

  if (isOpus) {
    const codec = rendererLossyCodec(zone?.renderer) ?? "opus";
    stages.push({
      icon: Cpu,
      title: codec === "aac" ? "AAC Transcode" : "Opus Transcode",
      detail: codec === "aac" ? "ffmpeg aac · ADTS" : "ffmpeg libopus · 48 kHz",
      meta: opusBitrate(activeQ),
      accent: "amber",
    });
  } else if (isWav) {
    const wavDetail =
      track.bitDepth === 1
        ? "24-bit / 88.2 kHz PCM"
        : track.bitDepth && track.sampleRate
          ? `${track.bitDepth > 16 ? 24 : 16}-bit / ${(track.sampleRate / 1000).toFixed(track.sampleRate % 1000 === 0 ? 0 : 1)} kHz PCM`
          : "Uncompressed PCM";
    stages.push({
      icon: Waves,
      title: track.bitDepth === 1 ? "DSD → PCM Decode" : "PCM Decode",
      detail: wavDetail,
      meta: "WAV",
      accent: "emerald",
    });
  } else {
    stages.push({
      icon: Waves,
      title: "Bit-Perfect Pass-Through",
      detail: "No resampling or DSP",
      meta: "Original",
      accent: losslessSource ? "emerald" : "neutral",
    });
  }

  if (zone) {
    const isPlayerKind = isBrowserActive || zone.renderer.kind === "player";
    if (isPlayerKind) {
      stages.push({
        icon: Radio,
        title: "HTTP Audio Stream",
        detail: isOpus ? "Transcoded stream" : "Direct lossless stream",
        meta: isQobuz ? "Relay" : "LAN HTTP",
        accent: "neutral",
      });
    } else {
      const isCastKind = zone.renderer.kind === "cast";
      const deliveryTag = isQobuz
        ? streamMode === "direct" && !isOpus && !isWav
          ? "Handoff"
          : "Relay"
        : "LAN HTTP";
      const transportDetail = isCastKind
        ? "Cast media session"
        : zone.gaplessArmed
          ? "Gapless armed"
          : zone.renderer.supportsSetNext
            ? "Gapless ready"
            : "Standard transport";
      stages.push({
        icon: Radio,
        title: isCastKind ? "Google Cast" : "UPnP / DLNA AVTransport",
        detail: transportDetail,
        meta: deliveryTag,
        accent: "neutral",
      });
    }

    const rendererName = isBrowserActive ? "This Browser" : zone.renderer.name;
    const rendererSub = isBrowserActive
      ? "Browser audio output"
      : [zone.renderer.manufacturer, zone.renderer.model].filter(Boolean).join(" ") || zone.renderer.host || "UPnP Renderer";

    stages.push({
      icon: Speaker,
      title: rendererName,
      detail: rendererSub,
      accent: "neutral",
    });
  }

  return (
    <div
      ref={rootRef}
      className="relative inline-flex"
      onMouseEnter={handleEnter}
      onMouseLeave={handleLeave}
    >
      <button
        type="button"
        onClick={() => {
          setPinned((prev) => {
            const next = !prev;
            setOpen(next);
            return next;
          });
        }}
        aria-expanded={open}
        aria-label={`Show audio signal path (${pathStatus.label})`}
        className={cn(
          "inline-flex cursor-pointer items-center justify-center rounded-full focus:outline-none focus-visible:ring-1 focus-visible:ring-white/30",
          variant === "led" &&
            cn(
              "h-5 w-5 shrink-0 transition-colors hover:bg-white/[0.08]",
              open && "bg-white/[0.1]",
            ),
        )}
      >
        {variant === "led" ? (
          <span className={cn("h-2 w-2 rounded-full", pathStatus.dot)} />
        ) : (
          <SignalBadge
            provider={track.provider || track.source}
            format={qualLabel}
            size="sm"
            showTitle={false}
            className={cn(
              "transition-colors hover:border-white/20 hover:bg-white/[0.09]",
              open && "border-white/20 bg-white/[0.09]",
            )}
          />
        )}
      </button>

      {open &&
        anchor &&
        createPortal(
          <div
            ref={popoverRef}
            role="dialog"
            aria-label="Signal path"
            onMouseEnter={handleEnter}
            onMouseLeave={handleLeave}
            style={{ left: anchor.left, bottom: anchor.bottom }}
            className="fixed z-50 w-[min(344px,calc(100vw-24px))] rounded-2xl border border-white/[0.11] bg-[#111115]/96 p-4 text-left shadow-[0_24px_60px_rgba(0,0,0,0.75)] backdrop-blur-2xl"
          >
            {/* Header */}
            <div className="flex items-center justify-between gap-3 pb-3.5 border-b border-white/[0.07]">
              <span className="text-xs font-semibold tracking-tight text-white">Signal Path</span>
              <span
                className={cn(
                  "inline-flex items-center gap-1.5 rounded-full border px-2 py-0.5 text-[10.5px] font-medium whitespace-nowrap",
                  pathStatus.pill,
                )}
              >
                <span className={cn("h-1.5 w-1.5 rounded-full", pathStatus.dot)} />
                {pathStatus.label}
              </span>
            </div>

            {/* Vertical pipeline stages */}
            <ol className="relative mt-4 space-y-3.5">
              {/* Continuous colored signal spine */}
              {stages.length > 1 && (
                <span
                  aria-hidden="true"
                  className={cn(
                    "pointer-events-none absolute left-[15px] top-4 bottom-4 w-px bg-gradient-to-b",
                    pathStatus.spine,
                  )}
                />
              )}
              {stages.map((stage) => {
                const Icon = stage.icon;
                const iconColor =
                  stage.accent === "emerald"
                    ? "text-emerald-300 border-emerald-400/30 bg-[#111a17]"
                    : stage.accent === "sky"
                      ? "text-sky-300 border-sky-400/30 bg-[#101820]"
                      : stage.accent === "amber"
                        ? "text-amber-300 border-amber-400/30 bg-[#1c1710]"
                        : "text-foreground/75 border-white/[0.13] bg-[#18181d]";

                return (
                  <li key={stage.title} className="relative flex items-start gap-3">
                    <span
                      className={cn(
                        "relative z-10 flex h-8 w-8 shrink-0 items-center justify-center rounded-full border",
                        iconColor,
                      )}
                    >
                      <Icon className="h-3.5 w-3.5" />
                    </span>
                    <div className="min-w-0 flex-1 pt-0.5">
                      <div className="flex items-baseline justify-between gap-3">
                        <span className="truncate text-[12.5px] font-medium text-foreground leading-tight">
                          {stage.title}
                        </span>
                        {stage.meta && (
                          <span className="shrink-0 font-mono text-[10.5px] text-muted-foreground/80 whitespace-nowrap">
                            {stage.meta}
                          </span>
                        )}
                      </div>
                      <div className="mt-0.5 font-mono text-[11px] text-muted-foreground leading-snug truncate">
                        {stage.detail}
                      </div>
                    </div>
                  </li>
                );
              })}
            </ol>
          </div>,
        document.body,
      )}
    </div>
  );
}
