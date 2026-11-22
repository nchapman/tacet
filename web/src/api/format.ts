import type { Album, AudioQuality, StreamMode, Track } from "./types";

export function fmtDuration(seconds?: number): string {
  const total = Math.max(0, Math.floor(seconds ?? 0));
  const hrs = Math.floor(total / 3600);
  const mins = Math.floor((total % 3600) / 60);
  const secs = total % 60;
  const pad = (n: number) => (n < 10 ? `0${n}` : `${n}`);
  return hrs > 0 ? `${hrs}:${pad(mins)}:${pad(secs)}` : `${mins}:${pad(secs)}`;
}

export function fmtSampleRate(hz?: number): string {
  if (!hz || hz <= 0) return "";
  if (hz >= 1_000_000) {
    const mhz = hz / 1_000_000;
    return `${mhz.toFixed(2)} MHz`;
  }
  const khz = hz / 1000;
  return `${hz % 1000 === 0 ? khz.toFixed(0) : khz.toFixed(1)} kHz`;
}

export function fmtQuality(track?: Pick<Track, "ext" | "mime" | "bitDepth" | "sampleRate"> | null): string {
  if (!track) return "";
  const ext = (track.ext ?? "").replace(/^\./, "").toUpperCase();
  const codec = ext || (track.mime ?? "").replace(/^audio\//, "").toUpperCase();
  if (track.bitDepth && track.sampleRate) {
    if (track.bitDepth === 1 && track.sampleRate >= 2_000_000) {
      const mult = Math.round(track.sampleRate / 44100);
      return `DSD${mult} (${fmtSampleRate(track.sampleRate)})`;
    }
    return `${codec} ${track.bitDepth}-bit / ${fmtSampleRate(track.sampleRate)}`;
  }
  if (track.sampleRate) {
    return `${codec} ${fmtSampleRate(track.sampleRate)}`;
  }
  return codec;
}

export function fmtAlbumQuality(album?: Pick<Album, "maxBitDepth" | "maxSampleRate"> | null): string {
  if (!album || !album.maxBitDepth || !album.maxSampleRate) return "";
  if (album.maxBitDepth === 1 && album.maxSampleRate >= 2_000_000) {
    return `DSD${Math.round(album.maxSampleRate / 44100)}`;
  }
  return `${album.maxBitDepth}-bit / ${fmtSampleRate(album.maxSampleRate)}`;
}

export interface AudioQualityStop {
  value: AudioQuality;
  label: string;
  detail: string;
  shortLabel: string;
}

export const AUDIO_QUALITY_STOPS: readonly AudioQualityStop[] = [
  { value: "opus_96", label: "Efficient", detail: "Opus · 96 kbps", shortLabel: "96k" },
  { value: "opus_128", label: "Standard", detail: "Opus · 128 kbps", shortLabel: "128k" },
  { value: "opus_160", label: "High", detail: "Opus · 160 kbps", shortLabel: "160k" },
  { value: "opus_256", label: "Very High", detail: "Opus · 256 kbps", shortLabel: "256k" },
  { value: "original", label: "Original", detail: "Bit-perfect source", shortLabel: "Orig" },
  { value: "wav", label: "WAV", detail: "Uncompressed PCM", shortLabel: "WAV" },
] as const;

export function audioQualityIndex(quality?: AudioQuality): number {
  const idx = AUDIO_QUALITY_STOPS.findIndex((s) => s.value === (quality || "original"));
  return idx >= 0 ? idx : 4;
}

export function audioQualityStop(quality?: AudioQuality): AudioQualityStop {
  return AUDIO_QUALITY_STOPS[audioQualityIndex(quality)];
}

/**
 * Formats the active output quality pill. When Opus transcoding is active,
 * shows only the Opus preset (e.g. "OPUS 128k") rather than the source format.
 */
export function fmtOutputQuality(
  track?: Pick<Track, "ext" | "mime" | "bitDepth" | "sampleRate"> | null,
  activeQuality?: AudioQuality,
): string {
  switch (activeQuality) {
    case "opus_96":
      return "OPUS 96k";
    case "opus_128":
      return "OPUS 128k";
    case "opus_160":
      return "OPUS 160k";
    case "opus_256":
      return "OPUS 256k";
    case "wav": {
      if (!track) return "WAV";
      if (track.bitDepth === 1) {
        return "WAV 24-bit / 88.2 kHz";
      }
      if (track.bitDepth && track.sampleRate) {
        const bd = track.bitDepth > 16 ? 24 : 16;
        return `WAV ${bd}-bit / ${fmtSampleRate(track.sampleRate)}`;
      }
      return "WAV";
    }
    default:
      return fmtQuality(track);
  }
}

export function isHiResOutput(
  track?: Pick<Track, "bitDepth" | "sampleRate"> | null,
  activeQuality?: AudioQuality,
): boolean {
  if (
    activeQuality === "opus_96" ||
    activeQuality === "opus_128" ||
    activeQuality === "opus_160" ||
    activeQuality === "opus_256"
  ) {
    return false;
  }
  return Boolean(track && isHiRes(track.bitDepth, track.sampleRate));
}

export function isHiRes(bitDepth?: number, sampleRate?: number): boolean {
  if (!sampleRate) return false;
  if (bitDepth === 1 && sampleRate >= 2_000_000) return true; // DSD
  return (bitDepth ?? 0) > 16 || sampleRate > 48000;
}

export function streamModeLabel(mode: StreamMode): string {
  return mode === "relay" ? "Quiet Relay" : "Direct Handoff";
}
