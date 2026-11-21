import { describe, expect, it, vi } from "vitest";
import type { PlayerStateReport } from "../api/types";
import { BrowserAudioEngine, detectBrowserCodecs, type AudioLike } from "./browserPlayer";

class FakeAudio implements AudioLike {
  src = "";
  preload = "";
  volume = 1;
  muted = false;
  currentTime = 0;
  duration = 180;
  paused = true;
  ended = false;
  playRejectWith: Error | null = null;
  playCalls = 0;
  loadCalls = 0;
  private listeners = new Map<string, Set<EventListenerOrEventListenerObject>>();

  async play(): Promise<void> {
    this.playCalls++;
    if (this.playRejectWith) {
      throw this.playRejectWith;
    }
    this.paused = false;
    this.ended = false;
  }

  pause(): void {
    this.paused = true;
  }

  load(): void {
    this.loadCalls++;
  }

  canPlayType(mime: string): CanPlayTypeResult {
    return mime === "audio/flac" || mime === "audio/mpeg" ? "probably" : "";
  }

  addEventListener(type: string, listener: EventListenerOrEventListenerObject): void {
    let set = this.listeners.get(type);
    if (!set) {
      set = new Set();
      this.listeners.set(type, set);
    }
    set.add(listener);
  }

  removeEventListener(type: string, listener: EventListenerOrEventListenerObject): void {
    this.listeners.get(type)?.delete(listener);
  }

  emit(type: string): void {
    if (type === "ended") {
      this.paused = true;
      this.ended = true;
    }
    for (const l of this.listeners.get(type) ?? []) {
      if (typeof l === "function") l({ type } as Event);
      else l.handleEvent({ type } as Event);
    }
  }
}

describe("BrowserAudioEngine", () => {
  it("detects supported browser codecs while excluding unsupported ones", () => {
    const codecs = detectBrowserCodecs(new FakeAudio());
    expect(codecs).toEqual(["audio/flac", "audio/mpeg"]);
  });

  it("handles set_uri, play, volume, mute, seek, and gapless set_next_uri swap", async () => {
    const audios: FakeAudio[] = [];
    const reports: PlayerStateReport[] = [];
    const engine = new BrowserAudioEngine({
      audioFactory: () => {
        const a = new FakeAudio();
        audios.push(a);
        return a;
      },
      onReportState: (rep) => reports.push(rep),
    });

    await engine.handleCommand({
      id: "cmd_1",
      type: "set_uri",
      media: {
        id: "local:t1",
        url: "/media/local/local:t1.flac",
        title: "Track 1",
        artist: "Artist",
        album: "Album",
        mime: "audio/flac",
        durationSec: 180,
      },
    });
    expect(audios[0].src).toBe("/media/local/local:t1.flac");
    expect(reports.at(-1)).toMatchObject({ cmdId: "cmd_1", ok: true, state: "TRANSITIONING" });

    await engine.handleCommand({ id: "cmd_2", type: "play" });
    expect(audios[0].paused).toBe(false);
    expect(reports.at(-1)).toMatchObject({ cmdId: "cmd_2", ok: true, state: "PLAYING" });

    await engine.handleCommand({ id: "cmd_3", type: "set_volume", volume: 65 });
    expect(audios[0].volume).toBeCloseTo(0.65);
    expect(audios[1].volume).toBeCloseTo(0.65);

    await engine.handleCommand({ id: "cmd_4", type: "set_mute", muted: true });
    expect(audios[0].muted).toBe(true);
    expect(audios[1].muted).toBe(true);

    await engine.handleCommand({ id: "cmd_5", type: "seek", positionSec: 42 });
    expect(audios[0].currentTime).toBe(42);

    // Preload next track into standby audio element.
    await engine.handleCommand({
      id: "cmd_6",
      type: "set_next_uri",
      media: {
        id: "local:t2",
        url: "/media/local/local:t2.flac",
        title: "Track 2",
        artist: "Artist",
        album: "Album",
        mime: "audio/flac",
        durationSec: 200,
      },
    });
    expect(audios[1].src).toBe("/media/local/local:t2.flac");

    // When Track 1 ends, standby (Track 2) promotes immediately and reports PLAYING with new URI.
    audios[0].emit("ended");
    await new Promise((r) => setTimeout(r, 0));
    expect(audios[1].paused).toBe(false);
    expect(reports.at(-1)).toMatchObject({
      state: "PLAYING",
      trackUri: "/media/local/local:t2.flac",
    });

    engine.dispose();
  });

  it("reports AUTOPLAY_BLOCKED and triggers onAutoplayBlocked when browser rejects play()", async () => {
    const audios: FakeAudio[] = [];
    const reports: PlayerStateReport[] = [];
    const onAutoplayBlocked = vi.fn();

    const engine = new BrowserAudioEngine({
      audioFactory: () => {
        const a = new FakeAudio();
        audios.push(a);
        return a;
      },
      onReportState: (rep) => reports.push(rep),
      onAutoplayBlocked,
    });

    const err = new Error("user gesture required");
    err.name = "NotAllowedError";
    audios[0].playRejectWith = err;

    await engine.handleCommand({
      id: "cmd_1",
      type: "set_uri",
      media: {
        id: "local:t1",
        url: "/media/local/local:t1.flac",
        title: "Track 1",
        artist: "Artist",
        album: "Album",
        mime: "audio/flac",
        durationSec: 180,
      },
    });
    await engine.handleCommand({ id: "cmd_2", type: "play" });

    expect(onAutoplayBlocked).toHaveBeenCalledWith(true);
    expect(reports.at(-1)).toMatchObject({
      cmdId: "cmd_2",
      ok: false,
      errorCode: "PLAYBACK_BLOCKED",
    });

    // Clicking Unlock Audio clears the error and starts playback.
    audios[0].playRejectWith = null;
    await engine.resumeFromGesture();
    expect(onAutoplayBlocked).toHaveBeenCalledWith(false);
    expect(audios[0].paused).toBe(false);

    engine.dispose();
  });
});
