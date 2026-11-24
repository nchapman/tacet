import { afterEach, describe, expect, it } from "vitest";
import type { ZoneSnapshot } from "../api/types";
import { loadPlayerToken, savePlayerToken, shouldEnableBrowserPlayer } from "./useBrowserPlayer";
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

  it("waits instead of jumping to an arbitrary UPnP speaker while the local browser player is still connecting", () => {
    // Nothing saved + browser still registering -> wait instead of selecting wiim.
    expect(resolveActiveZone([wiim, xbox], "", null, true)).toBeNull();
    // Saved browser player not registered yet -> wait instead of selecting wiim.
    expect(resolveActiveZone([wiim, xbox], local.zoneId, null, true)).toBeNull();
    // Once local browser registers -> selects local immediately.
    expect(resolveActiveZone([wiim, xbox, local], "", local.zoneId, false)?.zoneId).toBe(local.zoneId);
  });

  it("still selects an explicitly saved online UPnP speaker immediately even while browser player is connecting", () => {
    expect(resolveActiveZone([xbox, wiim], wiim.zoneId, null, true)?.zoneId).toBe(wiim.zoneId);
  });
});

describe("player token persistence", () => {
  const makeStorage = () => {
    const store = new Map<string, string>();
    return {
      getItem: (k: string) => store.get(k) ?? null,
      setItem: (k: string, v: string) => {
        store.set(k, v);
      },
      removeItem: (k: string) => {
        store.delete(k);
      },
    };
  };

  afterEach(() => {
    delete (globalThis as { window?: unknown }).window;
  });

  it("stores the player token in localStorage and migrates any legacy sessionStorage token", () => {
    const localStorage = makeStorage();
    const sessionStorage = makeStorage();
    (globalThis as { window?: unknown }).window = { localStorage, sessionStorage };

    // Legacy tab had token in sessionStorage -> migrates cleanly to localStorage.
    sessionStorage.setItem("tacet.browserPlayerToken", "tok-legacy");
    expect(loadPlayerToken()).toBe("tok-legacy");
    expect(localStorage.getItem("tacet.browserPlayerToken")).toBe("tok-legacy");
    expect(sessionStorage.getItem("tacet.browserPlayerToken")).toBeNull();

    // New writes persist in localStorage so new tabs share the auth token.
    savePlayerToken("tok-fresh");
    expect(localStorage.getItem("tacet.browserPlayerToken")).toBe("tok-fresh");
    expect(loadPlayerToken()).toBe("tok-fresh");

    // Clearing removes from both storages.
    savePlayerToken("");
    expect(loadPlayerToken()).toBe("");
    expect(localStorage.getItem("tacet.browserPlayerToken")).toBeNull();
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
