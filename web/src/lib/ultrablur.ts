export interface UltraBlurPalette {
  /** Primary vibrant hero color (top-left / sidebar + cover glow) */
  primary: string;
  /** Secondary contrasting color (top-right horizon wash) */
  secondary: string;
  /** Mid-tone accent color (mid-left sidebar-to-stage bridge) */
  accent: string;
  /** Deep anchor color (lower-right atmospheric depth) */
  deep: string;
}

interface RGB {
  r: number;
  g: number;
  b: number;
}

interface HSL {
  h: number; // 0..360
  s: number; // 0..1
  l: number; // 0..1
}

interface Bucket {
  rSum: number;
  gSum: number;
  bSum: number;
  weight: number;
  count: number;
}

const SAMPLE_SIZE = 48;
const MAX_CACHE_ENTRIES = 120;
const paletteCache = new Map<string, UltraBlurPalette | null>();
const inflightCache = new Map<string, Promise<UltraBlurPalette | null>>();

function clamp(v: number, min: number, max: number): number {
  return Math.min(max, Math.max(min, v));
}

export function rgbToHsl(r: number, g: number, b: number): HSL {
  const rn = r / 255;
  const gn = g / 255;
  const bn = b / 255;
  const max = Math.max(rn, gn, bn);
  const min = Math.min(rn, gn, bn);
  const l = (max + min) / 2;
  const d = max - min;

  if (d === 0) {
    return { h: 0, s: 0, l };
  }
  const s = l > 0.5 ? d / (2 - max - min) : d / (max + min);
  let h = 0;
  if (max === rn) {
    h = (gn - bn) / d + (gn < bn ? 6 : 0);
  } else if (max === gn) {
    h = (bn - rn) / d + 2;
  } else {
    h = (rn - gn) / d + 4;
  }
  return { h: h * 60, s, l };
}

export function hslToRgb(h: number, s: number, l: number): RGB {
  const hn = (((h % 360) + 360) % 360) / 360;
  if (s === 0) {
    const v = Math.round(clamp(l, 0, 1) * 255);
    return { r: v, g: v, b: v };
  }
  const q = l < 0.5 ? l * (1 + s) : l + s - l * s;
  const p = 2 * l - q;
  const hue2rgb = (t: number) => {
    let tn = t;
    if (tn < 0) tn += 1;
    if (tn > 1) tn -= 1;
    if (tn < 1 / 6) return p + (q - p) * 6 * tn;
    if (tn < 1 / 2) return q;
    if (tn < 2 / 3) return p + (q - p) * (2 / 3 - tn) * 6;
    return p;
  };
  return {
    r: Math.round(clamp(hue2rgb(hn + 1 / 3), 0, 1) * 255),
    g: Math.round(clamp(hue2rgb(hn), 0, 1) * 255),
    b: Math.round(clamp(hue2rgb(hn - 1 / 3), 0, 1) * 255),
  };
}

function rgbString({ r, g, b }: RGB): string {
  return `rgb(${r}, ${g}, ${b})`;
}

function colorDistance(a: RGB, b: RGB): number {
  // Perceptually weighted Euclidean distance in RGB space.
  const dr = a.r - b.r;
  const dg = a.g - b.g;
  const db = a.b - b.b;
  return Math.sqrt(dr * dr * 0.3 + dg * dg * 0.59 + db * db * 0.11);
}

/**
 * Normalizes an extracted color into a consistent dark-mode UltraBlur luminance
 * & vibrancy range so bright, dark, and busy album covers all feel equally balanced.
 * True monochrome covers stay cool neutral slate instead of inventing fake color.
 */
function normalizeSwatch(
  rgb: RGB,
  opts: { minL: number; maxL: number; targetBoostS: number; hueShift?: number },
): RGB {
  const hsl = rgbToHsl(rgb.r, rgb.g, rgb.b);
  const isMonochrome = hsl.s < 0.08;
  const h = isMonochrome ? 222 : (hsl.h + (opts.hueShift ?? 0) + 360) % 360;
  const s = isMonochrome
    ? 0.09
    : clamp(hsl.s * opts.targetBoostS, 0.32, 0.86);
  const l = clamp(hsl.l, opts.minL, opts.maxL);
  return hslToRgb(h, s, l);
}

/**
 * Pure Plexamp-style 4-color UltraBlur palette extractor from raw RGBA pixels.
 * Quantizes pixels into a 5-bit/channel color cube with vibrance weighting,
 * filters near-black/near-white margins unless monochrome, picks 4 maximally
 * distinct dominant colors, and normalizes them for consistent dark-mode glow.
 */
export function extractPaletteFromPixels(rgba: Uint8ClampedArray, width: number, height: number): UltraBlurPalette {
  const buckets = new Map<number, Bucket>();
  const fallbackBuckets = new Map<number, Bucket>();
  const totalPixels = Math.min(width * height, Math.floor(rgba.length / 4));

  for (let i = 0; i < totalPixels; i++) {
    const off = i * 4;
    const a = rgba[off + 3];
    if (a < 128) continue;

    const r = rgba[off];
    const g = rgba[off + 1];
    const b = rgba[off + 2];

    const hsl = rgbToHsl(r, g, b);
    const key = ((r >> 3) << 10) | ((g >> 3) << 5) | (b >> 3);

    // Always track fallback buckets in case the entire cover is pure B&W / near-black.
    const fb = fallbackBuckets.get(key);
    if (fb) {
      fb.rSum += r;
      fb.gSum += g;
      fb.bSum += b;
      fb.weight += 1;
      fb.count += 1;
    } else {
      fallbackBuckets.set(key, { rSum: r, gSum: g, bSum: b, weight: 1, count: 1 });
    }

    // Skip extreme dark/white margins when looking for colorful pigment.
    if (hsl.l < 0.07 || hsl.l > 0.93) continue;

    // Vibrance weighting: favor rich mid-tone colors over muddy grays while still
    // letting large color blocks dominate.
    const midtoneBonus = 1 - Math.abs(hsl.l - 0.48) * 1.1;
    const vibranceWeight = 0.25 + hsl.s * hsl.s * 3.4 * Math.max(0.2, midtoneBonus);

    const bucket = buckets.get(key);
    if (bucket) {
      bucket.rSum += r * vibranceWeight;
      bucket.gSum += g * vibranceWeight;
      bucket.bSum += b * vibranceWeight;
      bucket.weight += vibranceWeight;
      bucket.count += 1;
    } else {
      buckets.set(key, {
        rSum: r * vibranceWeight,
        gSum: g * vibranceWeight,
        bSum: b * vibranceWeight,
        weight: vibranceWeight,
        count: 1,
      });
    }
  }

  const activeMap = buckets.size > 0 ? buckets : fallbackBuckets;
  const candidates = Array.from(activeMap.values())
    .map((b) => ({
      rgb: {
        r: Math.round(b.rSum / b.weight),
        g: Math.round(b.gSum / b.weight),
        b: Math.round(b.bSum / b.weight),
      },
      score: b.weight,
    }))
    .sort((a, b) => b.score - a.score);

  if (candidates.length === 0) {
    return {
      primary: "rgb(45, 96, 128)",
      secondary: "rgb(36, 112, 92)",
      accent: "rgb(54, 72, 118)",
      deep: "rgb(24, 34, 52)",
    };
  }

  // Pick up to 4 well-separated dominant colors (greedy farthest-weighted selection).
  const picked: RGB[] = [candidates[0].rgb];
  const minDistances = [52, 40, 28];

  for (const minDist of minDistances) {
    let best: RGB | null = null;
    for (const cand of candidates) {
      if (picked.every((p) => colorDistance(p, cand.rgb) >= minDist)) {
        best = cand.rgb;
        break;
      }
    }
    if (best) {
      picked.push(best);
    }
    if (picked.length === 4) break;
  }

  // If the cover is monochromatic or single-hue, synthesize harmonious neighboring stops.
  const base = picked[0];
  const c1 = normalizeSwatch(base, { minL: 0.34, maxL: 0.50, targetBoostS: 1.2 });
  const c2 = normalizeSwatch(picked[1] ?? base, {
    minL: 0.30,
    maxL: 0.46,
    targetBoostS: 1.15,
    hueShift: picked[1] ? 0 : 22,
  });
  const c3 = normalizeSwatch(picked[2] ?? picked[1] ?? base, {
    minL: 0.26,
    maxL: 0.42,
    targetBoostS: 1.1,
    hueShift: picked[2] ? 0 : -18,
  });
  const c4 = normalizeSwatch(picked[3] ?? base, {
    minL: 0.16,
    maxL: 0.28,
    targetBoostS: 0.95,
    hueShift: picked[3] ? 0 : -10,
  });

  return {
    primary: rgbString(c1),
    secondary: rgbString(c2),
    accent: rgbString(c3),
    deep: rgbString(c4),
  };
}

function setCachedPalette(src: string, palette: UltraBlurPalette | null) {
  if (paletteCache.size >= MAX_CACHE_ENTRIES) {
    const oldestKey = paletteCache.keys().next().value;
    if (oldestKey !== undefined) {
      paletteCache.delete(oldestKey);
    }
  }
  paletteCache.set(src, palette);
}

export function getCachedUltraBlurPalette(src: string): UltraBlurPalette | null | undefined {
  return paletteCache.get(src);
}

/**
 * Extracts and caches a 4-color UltraBlur palette from an image URL using a tiny
 * 48x48 offscreen canvas. Returns null if the remote image cannot be read (e.g.
 * missing CORS headers), allowing the caller to fall back cleanly.
 */
export function extractUltraBlurPalette(src: string): Promise<UltraBlurPalette | null> {
  if (!src) return Promise.resolve(null);
  if (paletteCache.has(src)) {
    return Promise.resolve(paletteCache.get(src) ?? null);
  }
  const existing = inflightCache.get(src);
  if (existing) return existing;

  const task = new Promise<UltraBlurPalette | null>((resolve) => {
    if (typeof Image === "undefined" || typeof document === "undefined") {
      resolve(null);
      return;
    }

    const img = new Image();
    img.crossOrigin = "anonymous";
    img.decoding = "async";

    let settled = false;
    const finish = (palette: UltraBlurPalette | null, cacheFailure: boolean) => {
      if (settled) return;
      settled = true;
      window.clearTimeout(timer);
      img.onload = null;
      img.onerror = null;
      if (palette !== null || cacheFailure) {
        setCachedPalette(src, palette);
      }
      resolve(palette);
    };

    const timer = window.setTimeout(() => {
      img.src = "";
      finish(null, false);
    }, 10_000);

    img.onload = () => {
      try {
        const canvas = document.createElement("canvas");
        canvas.width = SAMPLE_SIZE;
        canvas.height = SAMPLE_SIZE;
        const ctx = canvas.getContext("2d", { willReadFrequently: true });
        if (!ctx) {
          finish(null, true);
          return;
        }
        ctx.drawImage(img, 0, 0, SAMPLE_SIZE, SAMPLE_SIZE);
        const { data } = ctx.getImageData(0, 0, SAMPLE_SIZE, SAMPLE_SIZE);
        const palette = extractPaletteFromPixels(data, SAMPLE_SIZE, SAMPLE_SIZE);
        finish(palette, true);
      } catch {
        // Cross-origin canvas taint or decode error -> cache null and fall back cleanly.
        finish(null, true);
      }
    };

    img.onerror = () => {
      finish(null, true);
    };

    img.src = src;
  }).finally(() => {
    inflightCache.delete(src);
  });

  inflightCache.set(src, task);
  return task;
}
