import { describe, expect, it } from "vitest";
import type { ZoneSnapshot } from "../api/types";
import { shouldEnableBrowserPlayer } from "./useBrowserPlayer";
import { resolveActiveZone } from "./useTacet";

function makeZone(zoneId: string, name: string, kind: "upnp" | "player" = "upnp"): ZoneSnapshot {
  return {
    zoneId,
    revision: 1,
    renderer: {
      id: zoneId,
      kind,
      name,
      model: kind === "player" ? "Web Browser" : "WiiM Ultra",
      host: "192.168.1.10",
      location: "",
      supportsSetNext: true,
      supportsEvents: true,
      hasVolume: true,
      supportsSeek: true,
      sinkProtocols: [],
    },
    state: "stopped",
    positionSec: 0,
    positionUpdatedAt: "2026-01-01T00:00:00Z",
    durationSec: 0,
    volume: 50,
    muted: false,
    currentIndex: -1,
    queue: [],
    gaplessArmed: false,
    eventsActive: true,
  };
}

describe("resolveActiveZone", () => {
  const wiim = makeZone("uuid:wiim", "Living Room Wiim", "upnp");
  const xbox = makeZone("uuid:xbox", "XBOX", "upnp");
  const local = makeZone("player:abc123", "Mac Chrome", "player");

  it("returns null when no zones exist", () => {
    expect(resolveActiveZone([], "", local.zoneId)).toBeNull();
  });

  it("defaults to the local browser player when nothing has been selected yet", () => {
    expect(resolveActiveZone([wiim, xbox, local], "", local.zoneId)?.zoneId).toBe(local.zoneId);
  });

  it("remembers and selects the saved UPnP zone over the local browser player", () => {
    expect(resolveActiveZone([xbox, local, wiim], wiim.zoneId, local.zoneId)?.zoneId).toBe(wiim.zoneId);
  });

  it("falls back to the local browser player when the saved zone is temporarily offline, then snaps back", () => {
    // Saved zone missing while SSDP rediscovers it -> falls back to local browser player.
    expect(resolveActiveZone([xbox, local], wiim.zoneId, local.zoneId)?.zoneId).toBe(local.zoneId);
    // Saved zone reappears -> snaps back without losing preference.
    expect(resolveActiveZone([xbox, local, wiim], wiim.zoneId, local.zoneId)?.zoneId).toBe(wiim.zoneId);
  });

  it("falls back to the first zone when browser player is disabled and nothing is saved", () => {
    expect(resolveActiveZone([wiim, xbox], "", null)?.zoneId).toBe(wiim.zoneId);
  });
});

describe("shouldEnableBrowserPlayer", () => {
  it("defaults to enabled for first-time visitors", () => {
    expect(shouldEnableBrowserPlayer(null)).toBe(true);
    expect(shouldEnableBrowserPlayer("1")).toBe(true);
  });

  it("stays off only when explicitly disabled", () => {
    expect(shouldEnableBrowserPlayer("0")).toBe(false);
  });
});
