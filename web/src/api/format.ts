import type { Album, StreamMode, Track } from "./types";

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

export function isHiRes(bitDepth?: number, sampleRate?: number): boolean {
  if (!sampleRate) return false;
  if (bitDepth === 1 && sampleRate >= 2_000_000) return true; // DSD
  return (bitDepth ?? 0) > 16 || sampleRate > 48000;
}

export function streamModeLabel(mode: StreamMode): string {
  return mode === "relay" ? "Quiet Relay" : "Direct Handoff";
}
