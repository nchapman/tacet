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

const ABBREV_RE = /(?:(?:^|[\s(])(?:[A-Za-z]|Mr|Mrs|Ms|Dr|St|Jr|Sr|Prof|Rev|Gen|Col|Sgt|No|Vol|vs|etc|Inc|Ltd|Co)\.|(?:[A-Za-z]\.){2,})$/i;

function splitSentences(text: string): string[] {
  const out: string[] = [];
  let start = 0;
  for (let i = 0; i < text.length; i++) {
    const ch = text[i];
    if (ch === "\n") {
      const seg = text.slice(start, i).trim();
      if (seg) out.push(seg);
      start = i + 1;
      continue;
    }
    if (ch !== "." && ch !== "!" && ch !== "?") continue;

    // Advance past consecutive punctuation (e.g. "..." or "?!") and closing quotes/parens.
    let end = i + 1;
    while (end < text.length && (text[end] === "." || text[end] === "!" || text[end] === "?")) end++;
    while (end < text.length && `"'”’)]`.includes(text[end])) end++;

    // Must be at end of text or followed by whitespace (never mid-token like "U.K." or "3.5").
    if (end < text.length && !/\s/.test(text[end])) {
      i = end - 1;
      continue;
    }
    const candidate = text.slice(start, end).trim();
    if (ch === "." && ABBREV_RE.test(candidate)) {
      i = end - 1;
      continue;
    }
    if (candidate) out.push(candidate);
    start = end;
    i = end - 1;
  }
  const tail = text.slice(start).trim();
  if (tail) out.push(tail);
  return out;
}

/**
 * Truncates editorial copy (album reviews, artist bios) cleanly on a sentence
 * boundary near targetWords so previews end on a complete thought instead of
 * mid-word ellipses.
 */
export function truncateAtSentence(text: string, targetWords = 35): { preview: string; truncated: boolean } {
  const trimmed = text.trim();
  if (!trimmed) return { preview: "", truncated: false };

  const allWords = trimmed.split(/\s+/);
  if (allWords.length <= targetWords) {
    return { preview: trimmed, truncated: false };
  }

  const sentences = splitSentences(trimmed);
  if (sentences.length > 1) {
    const picked: string[] = [];
    let wordCount = 0;
    for (const s of sentences) {
      const sw = s.split(/\s+/).length;
      if (picked.length > 0 && wordCount >= targetWords) break;
      if (picked.length > 0 && wordCount + sw > targetWords * 1.65) break;
      picked.push(s);
      wordCount += sw;
    }
    const preview = picked.join(" ");
    if (preview.length < trimmed.length) {
      return { preview, truncated: true };
    }
    return { preview: trimmed, truncated: false };
  }

  // Fallback for a single run-on sentence: stop at word boundary.
  return { preview: `${allWords.slice(0, targetWords).join(" ")}…`, truncated: true };
}

export function streamModeLabel(mode: StreamMode): string {
  return mode === "relay" ? "Relay" : "Handoff";
}
