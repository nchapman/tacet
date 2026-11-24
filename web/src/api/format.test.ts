import { describe, expect, it } from "vitest";
import {
  AUDIO_QUALITY_STOPS,
  audioQualityIndex,
  fmtCount,
  fmtOutputQuality,
  isHiResOutput,
  truncateAtSentence,
} from "./format";

describe("fmtCount", () => {
  const expected = (n: number) => new Intl.NumberFormat().format(n);

  it("formats counts with grouping delimiters and handles empty/edge values", () => {
    expect(fmtCount(0)).toBe("0");
    expect(fmtCount(42)).toBe("42");
    expect(fmtCount(1234)).toBe(expected(1234));
    expect(fmtCount(1234567)).toBe(expected(1234567));
    expect(fmtCount(undefined)).toBe("0");
    expect(fmtCount(null)).toBe("0");
    expect(fmtCount(-5)).toBe("0");
    expect(fmtCount(Number.NaN)).toBe("0");
    expect(fmtCount(1000.9)).toBe(expected(1000));
  });
});

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

describe("truncateAtSentence", () => {
  it("stops cleanly at a sentence boundary near the word target", () => {
    const review =
      "Fiona Apple demonstrates considerable talent on her debut album, Tidal, but it is unformed, unfocused talent. " +
      "Her voice is surprisingly rich and supple for a teenager, and her jazzy, sophisticated piano playing also belies her age. " +
      "Given the right material, such talents could have flourished, but she has concentrated on her own compositions.";
    const res = truncateAtSentence(review, 35);
    expect(res.truncated).toBe(true);
    expect(res.preview).toBe(
      "Fiona Apple demonstrates considerable talent on her debut album, Tidal, but it is unformed, unfocused talent. " +
        "Her voice is surprisingly rich and supple for a teenager, and her jazzy, sophisticated piano playing also belies her age.",
    );
  });

  it("does not split inside abbreviations like U.K. or U.S.", () => {
    const bio =
      "In late 2003, things picked up rapidly and a bit weirdly for folkie Alexi Murdoch. " +
      "Moving to balmy Los Angeles from Scotland in the late '90s, Murdoch let the sleepy, weepy U.K. folk tradition guide his debut EP. " +
      "He later released Time Without Consequence in 2006 to wide critical acclaim.";
    const res = truncateAtSentence(bio, 30);
    expect(res.truncated).toBe(true);
    expect(res.preview).toContain("U.K. folk tradition guide his debut EP.");
    expect(res.preview).not.toMatch(/U\.$/);
  });

  it("does not truncate short copy", () => {
    const res = truncateAtSentence("Short editorial note.", 35);
    expect(res).toEqual({ preview: "Short editorial note.", truncated: false });
  });
});
