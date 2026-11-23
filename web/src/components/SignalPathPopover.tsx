import { Cloud, Cpu, Disc3, HardDrive, Radio, Speaker, Waves } from "lucide-react";
import { useEffect, useRef, useState } from "react";
import { createPortal } from "react-dom";
import { fmtQuality, rendererLossyCodec } from "@/api/format";
import type { AudioQuality, StreamMode, Track, ZoneSnapshot } from "@/api/types";
import { SignalBadge } from "@/components/ui/SignalBadge";
import { cn } from "@/lib/utils";

interface SignalPathPopoverProps {
  track: Track;
  zone: ZoneSnapshot | null;
  qualLabel: string;
  isBrowserActive: boolean;
  streamMode?: StreamMode;
}

interface Stage {
  icon: typeof Disc3;
  title: string;
  detail: string;
  accent?: "emerald" | "sky" | "amber" | "neutral";
}

function fmtSize(bytes?: number): string {
  if (!bytes || bytes <= 0) return "";
  if (bytes >= 1024 * 1024) return `${(bytes / (1024 * 1024)).toFixed(1)} MB`;
  return `${Math.round(bytes / 1024)} KB`;
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
    setAnchor({
      left: Math.max(12, Math.min(r.left, window.innerWidth - 332)),
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

  const pathStatus = isOpus
    ? { label: "Transcoded", dot: "bg-amber-400", text: "text-amber-300/90" }
    : losslessSource
      ? { label: "Lossless", dot: "bg-emerald-400", text: "text-emerald-300/90" }
      : { label: "Direct", dot: "bg-sky-400", text: "text-sky-300/90" };

  const sourceQuality = fmtQuality(track);
  const channels = track.channels ? `${track.channels}ch` : "";
  const fileSize = !isQobuz ? fmtSize(track.size) : "";
  const sourceDetail = [sourceQuality, channels, fileSize].filter(Boolean).join(" · ");

  const stages: Stage[] = [
    {
      icon: isQobuz ? Cloud : HardDrive,
      title: isQobuz ? "Qobuz Stream" : "Local File",
      detail: sourceDetail || (isQobuz ? "Qobuz catalog" : "Local storage"),
      accent: isQobuz ? "sky" : "emerald",
    },
  ];

  if (isOpus) {
    const codec = rendererLossyCodec(zone?.renderer) ?? "opus";
    stages.push({
      icon: Cpu,
      title: codec === "aac" ? "AAC Transcode" : "Opus Transcode",
      detail:
        codec === "aac"
          ? `ffmpeg aac · ${opusBitrate(activeQ)} · ADTS`
          : `ffmpeg libopus · ${opusBitrate(activeQ)} · 48 kHz`,
      accent: "amber",
    });
  } else if (isWav) {
    const wavDetail =
      track.bitDepth === 1
        ? "WAV · 24-bit / 88.2 kHz"
        : `WAV${track.bitDepth && track.sampleRate ? ` · ${track.bitDepth > 16 ? 24 : 16}-bit / ${(track.sampleRate / 1000).toFixed(track.sampleRate % 1000 === 0 ? 0 : 1)} kHz` : ""}`;
    stages.push({
      icon: Waves,
      title: track.bitDepth === 1 ? "DSD → PCM Decode" : "PCM Decode",
      detail: wavDetail,
      accent: "emerald",
    });
  } else {
    stages.push({
      icon: Waves,
      title: "Bit-Perfect Pass-Through",
      detail: "No resampling or DSP",
      accent: losslessSource ? "emerald" : "neutral",
    });
  }

  if (zone) {
    const isPlayerKind = isBrowserActive || zone.renderer.kind === "player";
    if (isPlayerKind) {
      stages.push({
        icon: Radio,
        title: "HTTP Audio Stream",
        detail: isQobuz ? "Relay" : "LAN HTTP",
        accent: "neutral",
      });
    } else {
      const transportBits: string[] = [];
      if (isQobuz) {
        transportBits.push(streamMode === "direct" && !isOpus && !isWav ? "Handoff" : "Relay");
      } else {
        transportBits.push("LAN HTTP");
      }
      if (zone.gaplessArmed) {
        transportBits.push("Gapless armed");
      } else if (zone.renderer.supportsSetNext) {
        transportBits.push("Gapless ready");
      }
      stages.push({
        icon: Radio,
        title: "UPnP / DLNA AVTransport",
        detail: transportBits.join(" · "),
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
        aria-label="Show audio signal path"
        className="inline-flex cursor-pointer rounded-full focus:outline-none focus-visible:ring-1 focus-visible:ring-white/30"
      >
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
            className="fixed z-50 w-76 sm:w-80 rounded-2xl border border-white/[0.11] bg-[#111115]/95 p-4 text-left shadow-[0_24px_60px_rgba(0,0,0,0.7)] backdrop-blur-2xl"
          >
          {/* Header */}
          <div className="flex items-center justify-between gap-2 pb-3 border-b border-white/[0.07]">
            <span className="text-xs font-semibold tracking-tight text-white">Signal Path</span>
            <span className={cn("inline-flex items-center gap-1.5 text-[11px] font-medium", pathStatus.text)}>
              <span className={cn("h-1.5 w-1.5 rounded-full", pathStatus.dot)} />
              {pathStatus.label}
            </span>
          </div>

          {/* Vertical pipeline stages */}
          <ol className="mt-3.5 space-y-3">
            {stages.map((stage, i) => {
              const Icon = stage.icon;
              const isLast = i === stages.length - 1;
              const iconColor =
                stage.accent === "emerald"
                  ? "text-emerald-300/90 border-emerald-400/25 bg-emerald-400/10"
                  : stage.accent === "sky"
                    ? "text-sky-300/90 border-sky-400/25 bg-sky-400/10"
                    : stage.accent === "amber"
                      ? "text-amber-300/90 border-amber-400/25 bg-amber-400/10"
                      : "text-foreground/75 border-white/12 bg-white/[0.05]";

              return (
                <li key={stage.title} className="relative flex items-start gap-3">
                  {/* Connecting spine */}
                  {!isLast && (
                    <span
                      aria-hidden="true"
                      className="absolute left-[13px] top-7 -bottom-3 w-px bg-white/[0.1]"
                    />
                  )}
                  <span
                    className={cn(
                      "relative z-10 flex h-7 w-7 shrink-0 items-center justify-center rounded-full border",
                      iconColor,
                    )}
                  >
                    <Icon className="h-3.5 w-3.5" />
                  </span>
                  <div className="min-w-0 pt-0.5">
                    <div className="text-xs font-medium text-foreground leading-tight">{stage.title}</div>
                    <div className="mt-0.5 font-mono text-[11px] text-muted-foreground leading-snug break-words">
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
