import { describe, expect, it } from "vitest";
import { extractPaletteFromPixels, rgbToHsl } from "./ultrablur";

function parseRgb(str: string): [number, number, number] {
  const m = /^rgb\((\d+),\s*(\d+),\s*(\d+)\)$/.exec(str);
  if (!m) throw new Error(`unexpected rgb string: ${str}`);
  return [Number(m[1]), Number(m[2]), Number(m[3])];
}

function makeCanvas(width: number, height: number, paint: (x: number, y: number) => [number, number, number]): Uint8ClampedArray {
  const data = new Uint8ClampedArray(width * height * 4);
  for (let y = 0; y < height; y++) {
    for (let x = 0; x < width; x++) {
      const [r, g, b] = paint(x, y);
      const off = (y * width + x) * 4;
      data[off] = r;
      data[off + 1] = g;
      data[off + 2] = b;
      data[off + 3] = 255;
    }
  }
  return data;
}

describe("extractPaletteFromPixels (UltraBlur)", () => {
  it("extracts distinct colors from a two-tone red + blue album cover", () => {
    const pixels = makeCanvas(16, 16, (x) => (x < 8 ? [210, 45, 35] : [30, 95, 220]));
    const pal = extractPaletteFromPixels(pixels, 16, 16);

    const p1 = rgbToHsl(...parseRgb(pal.primary));
    const p2 = rgbToHsl(...parseRgb(pal.secondary));

    // One should be red-ish (< 30 or > 330) and the other blue-ish (190..260).
    const hues = [p1.h, p2.h];
    expect(hues.some((h) => h < 30 || h > 330)).toBe(true);
    expect(hues.some((h) => h > 190 && h < 260)).toBe(true);
  });

  it("extracts the vibrant accent from a mostly-white cover and clamps dark-mode lightness", () => {
    // 85% pure white background, 15% rich emerald stripe
    const pixels = makeCanvas(20, 20, (_x, y) => (y >= 17 ? [16, 185, 110] : [250, 250, 250]));
    const pal = extractPaletteFromPixels(pixels, 20, 20);
    const [r, g, b] = parseRgb(pal.primary);
    const hsl = rgbToHsl(r, g, b);

    expect(g).toBeGreaterThan(r);
    expect(hsl.l).toBeGreaterThanOrEqual(0.32);
    expect(hsl.l).toBeLessThanOrEqual(0.52);
  });

  it("keeps monochrome B&W covers classy low-saturation slate instead of inventing neon", () => {
    const pixels = makeCanvas(16, 16, (x, y) => ((x + y) % 2 === 0 ? [35, 35, 35] : [215, 215, 215]));
    const pal = extractPaletteFromPixels(pixels, 16, 16);
    const hsl = rgbToHsl(...parseRgb(pal.primary));

    expect(hsl.s).toBeLessThan(0.15);
    expect(hsl.l).toBeGreaterThanOrEqual(0.30);
    expect(hsl.l).toBeLessThanOrEqual(0.52);
  });
});
