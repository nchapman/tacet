import { describe, expect, it } from "vitest";
import { AUDIO_QUALITY_STOPS, audioQualityIndex, fmtOutputQuality, isHiResOutput } from "./format";

describe("audio quality helpers", () => {
  it("orders 6 stops with Original at index 4 and WAV on the far right", () => {
    expect(AUDIO_QUALITY_STOPS.map((s) => s.value)).toEqual([
      "opus_96",
      "opus_128",
      "opus_160",
      "opus_256",
      "original",
      "wav",
    ]);
    expect(audioQualityIndex(undefined)).toBe(4);
    expect(audioQualityIndex("original")).toBe(4);
    expect(audioQualityIndex("wav")).toBe(5);
  });

  it("formats Opus output pills without showing the original source format", () => {
    const hiResFlac = { ext: ".flac", mime: "audio/flac", bitDepth: 24, sampleRate: 192000 };
    expect(fmtOutputQuality(hiResFlac, "opus_96")).toBe("OPUS 96k");
    expect(fmtOutputQuality(hiResFlac, "opus_128")).toBe("OPUS 128k");
    expect(fmtOutputQuality(hiResFlac, "opus_160")).toBe("OPUS 160k");
    expect(fmtOutputQuality(hiResFlac, "opus_256")).toBe("OPUS 256k");
    expect(isHiResOutput(hiResFlac, "opus_128")).toBe(false);
  });

  it("formats WAV and Original output pills preserving hi-res badge", () => {
    const hiResFlac = { ext: ".flac", mime: "audio/flac", bitDepth: 24, sampleRate: 96000 };
    expect(fmtOutputQuality(hiResFlac, "wav")).toBe("WAV 24-bit / 96 kHz");
    expect(isHiResOutput(hiResFlac, "wav")).toBe(true);
    expect(fmtOutputQuality(hiResFlac, "original")).toBe("FLAC 24-bit / 96 kHz");
    expect(isHiResOutput(hiResFlac, "original")).toBe(true);
  });
});
