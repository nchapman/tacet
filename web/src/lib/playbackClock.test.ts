import { describe, expect, it } from "vitest";
import { syncPlaybackClock, tickPlaybackClock } from "./playbackClock";

describe("playbackClock", () => {
  it("ticks smoothly forward every second while playing", () => {
    let clk = syncPlaybackClock(null, {
      zoneId: "z1",
      trackKey: "item-1",
      playing: true,
      serverPosSec: 0,
      positionUpdatedAtMs: 1000,
      durationSec: 200,
      nowMs: 1000,
    });
    expect(Math.floor(clk.displayPos)).toBe(0);

    clk = tickPlaybackClock(clk, 2000);
    expect(Math.floor(clk.displayPos)).toBe(1);

    clk = tickPlaybackClock(clk, 3000);
    expect(Math.floor(clk.displayPos)).toBe(2);
  });

  it("never jumps backward on small negative jitter and slews smoothly to converge", () => {
    let clk = syncPlaybackClock(null, {
      zoneId: "z1",
      trackKey: "item-1",
      playing: true,
      serverPosSec: 0,
      positionUpdatedAtMs: 0,
      durationSec: 200,
      nowMs: 0,
    });

    // Local clock ticks forward to 6.0s.
    clk = tickPlaybackClock(clk, 6000);
    expect(Math.floor(clk.displayPos)).toBe(6);

    // Server reports speaker is at 4s (-2s jitter): must NOT jump back to 4!
    clk = syncPlaybackClock(clk, {
      zoneId: "z1",
      trackKey: "item-1",
      playing: true,
      serverPosSec: 4,
      positionUpdatedAtMs: 6000,
      durationSec: 200,
      nowMs: 6000,
    });
    expect(Math.floor(clk.displayPos)).toBe(6);

    // Over the next 15 seconds it ticks monotonically forward and converges with server time.
    let prev = clk.displayPos;
    for (let t = 7000; t <= 26000; t += 1000) {
      clk = tickPlaybackClock(clk, t);
      expect(clk.displayPos).toBeGreaterThanOrEqual(prev);
      prev = clk.displayPos;
    }
    // Server is now at 4 + 20 = 24s; displayPos should have converged within < 0.5s.
    expect(Math.abs(clk.displayPos - 24)).toBeLessThan(0.5);
  });

  it("snaps immediately on large seeks (> 4s) or track changes", () => {
    let clk = syncPlaybackClock(null, {
      zoneId: "z1",
      trackKey: "item-1",
      playing: true,
      serverPosSec: 10,
      positionUpdatedAtMs: 10_000,
      durationSec: 200,
      nowMs: 10_000,
    });

    // User scrubs to 120s -> immediate snap.
    clk = syncPlaybackClock(clk, {
      zoneId: "z1",
      trackKey: "item-1",
      playing: true,
      serverPosSec: 120,
      positionUpdatedAtMs: 11_000,
      durationSec: 200,
      nowMs: 11_000,
    });
    expect(Math.floor(clk.displayPos)).toBe(120);

    // Track changes -> immediate snap to 0.
    clk = syncPlaybackClock(clk, {
      zoneId: "z1",
      trackKey: "item-2",
      playing: true,
      serverPosSec: 0,
      positionUpdatedAtMs: 12_000,
      durationSec: 180,
      nowMs: 12_000,
    });
    expect(Math.floor(clk.displayPos)).toBe(0);
  });
});
