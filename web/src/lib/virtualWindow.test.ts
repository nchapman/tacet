import { describe, expect, it } from "vitest";
import { computeRequiredPages, computeVirtualRange } from "./virtualWindow";

describe("computeVirtualRange", () => {
  it("handles empty datasets cleanly", () => {
    expect(computeVirtualRange(0, 800, 0, 56)).toEqual({
      startIndex: 0,
      endIndex: -1,
      offsetY: 0,
      totalHeight: 0,
    });
  });

  it("sizes totalHeight to the full list and mounts only the visible window plus overscan", () => {
    // 20,000 songs at 56px = 1,120,000px total height.
    // Viewport 560px (10 rows), scrolled to row 5,000 (280,000px), overscan 10.
    const range = computeVirtualRange(5000 * 56, 560, 20_000, 56, 10, 0);
    expect(range.totalHeight).toBe(20_000 * 56);
    expect(range.startIndex).toBe(4990);
    expect(range.endIndex).toBe(5019);
    expect(range.offsetY).toBe(4990 * 56);
  });

  it("clamps cleanly at the top and bottom of the list", () => {
    const top = computeVirtualRange(0, 560, 50, 56, 10, 12);
    expect(top.startIndex).toBe(0);
    expect(top.endIndex).toBe(19);
    expect(top.offsetY).toBe(0);

    // Overscrolling past the bottom (e.g. macOS rubber-band bounce) still keeps the
    // full bottom viewport + overscan mounted instead of collapsing to a single row.
    const bottom = computeVirtualRange(70 * 56, 560, 50, 56, 10, 0);
    expect(bottom.startIndex).toBe(30);
    expect(bottom.endIndex).toBe(49);
    expect(bottom.offsetY).toBe(30 * 56);
  });
});

describe("computeRequiredPages", () => {
  it("returns only the pages intersecting the visible window without over-fetching", () => {
    expect(computeRequiredPages(0, 25, 2500, 100, 25)).toEqual({
      visiblePages: [0],
      prefetchPages: [],
    });
  });

  it("prefetches the next page when approaching a page boundary", () => {
    expect(computeRequiredPages(60, 85, 2500, 100, 25)).toEqual({
      visiblePages: [0],
      prefetchPages: [1],
    });
  });

  it("fetches directly at an arbitrary jumped offset without touching intermediate pages", () => {
    // Jumped straight to tracks 14,510..14,540 in a 20,000-track library
    expect(computeRequiredPages(14_510, 14_540, 20_000, 100, 25)).toEqual({
      visiblePages: [145],
      prefetchPages: [144],
    });
  });

  it("spans two pages across a boundary and clamps at the final page", () => {
    expect(computeRequiredPages(640, 668, 669, 100, 25)).toEqual({
      visiblePages: [6],
      prefetchPages: [],
    });

    expect(computeRequiredPages(585, 615, 669, 100, 25)).toEqual({
      visiblePages: [5, 6],
      prefetchPages: [],
    });
  });
});
