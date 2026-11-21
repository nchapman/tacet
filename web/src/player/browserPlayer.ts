import type { PlayerCommand, PlayerMediaPayload, PlayerStateReport } from "../api/types";

// Minimal 44-byte silent 8kHz mono PCM WAV used to unlock HTMLMediaElement
// autoplay inside an explicit user gesture.
const SILENT_WAV_DATA_URL =
  "data:audio/wav;base64,UklGRigAAABXQVZFZm10IBIAAAABAAEARKwAAIhYAQACABAAAABkYXRhAgAAAAEA";

export interface AudioLike {
  src: string;
  preload: string;
  volume: number;
  muted: boolean;
  currentTime: number;
  readonly duration: number;
  readonly paused: boolean;
  readonly ended: boolean;
  play(): Promise<void>;
  pause(): void;
  load(): void;
  canPlayType(mime: string): CanPlayTypeResult;
  addEventListener(type: string, listener: EventListenerOrEventListenerObject): void;
  removeEventListener(type: string, listener: EventListenerOrEventListenerObject): void;
}

export interface BrowserPlayerCallbacks {
  onReportState: (report: PlayerStateReport) => void;
  onAutoplayBlocked?: (blocked: boolean) => void;
  onMediaSessionAction?: (action: "play" | "pause" | "previoustrack" | "nexttrack" | "seekto", seekTime?: number) => void;
  resolveUrl?: (url: string) => string;
  audioFactory?: () => AudioLike;
}

const CANDIDATE_CODECS = [
  "audio/flac",
  "audio/x-flac",
  "audio/mpeg",
  "audio/mp4",
  "audio/aac",
  "audio/wav",
  "audio/ogg",
  "audio/webm",
];

export function detectBrowserCodecs(audio?: Pick<AudioLike, "canPlayType">): string[] {
  const probe = audio ?? (typeof Audio !== "undefined" ? new Audio() : null);
  if (!probe) return ["audio/flac", "audio/mpeg", "audio/mp4", "audio/wav", "audio/ogg"];
  const supported: string[] = [];
  for (const mime of CANDIDATE_CODECS) {
    if (probe.canPlayType(mime) !== "") {
      supported.push(mime);
    }
  }
  return supported.length > 0 ? supported : ["audio/flac", "audio/mpeg", "audio/mp4", "audio/wav"];
}

export function defaultBrowserPlayerName(): string {
  if (typeof navigator === "undefined") return "Browser Player";
  const ua = navigator.userAgent;
  let browser = "Browser";
  if (ua.includes("Firefox/")) browser = "Firefox";
  else if (ua.includes("Edg/")) browser = "Edge";
  else if (ua.includes("Chrome/") || ua.includes("CriOS/")) browser = "Chrome";
  else if (ua.includes("Safari/")) browser = "Safari";

  let os = "";
  if (ua.includes("iPhone")) os = "iPhone";
  else if (ua.includes("iPad")) os = "iPad";
  else if (ua.includes("Android")) os = "Android";
  else if (ua.includes("Mac OS X")) os = "Mac";
  else if (ua.includes("Windows")) os = "Windows";
  else if (ua.includes("Linux")) os = "Linux";

  return os ? `${os} ${browser}` : browser;
}

/**
 * Dual-<audio> browser playback engine that executes tacet server commands,
 * preloads the next queued track via standby audio element, integrates with
 * navigator.mediaSession, and reports live state back to the server.
 */
export class BrowserAudioEngine {
  private active: AudioLike;
  private standby: AudioLike;
  private callbacks: BrowserPlayerCallbacks;

  private currentMedia: PlayerMediaPayload | null = null;
  private nextMedia: PlayerMediaPayload | null = null;
  private currentURI = "";
  private nextURI = "";
  private volume = 100;
  private muted = false;
  private disposed = false;
  private progressTimer: ReturnType<typeof setInterval> | null = null;
  private activeCleanup: (() => void) | null = null;

  constructor(callbacks: BrowserPlayerCallbacks) {
    this.callbacks = callbacks;
    const makeAudio = callbacks.audioFactory ?? (() => new Audio());
    this.active = makeAudio();
    this.standby = makeAudio();
    this.active.preload = "auto";
    this.standby.preload = "auto";
    this.active.volume = 1;
    this.standby.volume = 1;

    this.attachActiveListeners(this.active);
    this.setupMediaSession();

    this.progressTimer = setInterval(() => {
      if (!this.disposed && !this.active.paused && !this.active.ended && this.currentURI) {
        this.emitState("PLAYING");
      }
    }, 3000);
  }

  /**
   * Unlocks both <audio> elements synchronously inside a user click/tap gesture
   * so subsequent remote Play commands succeed under browser autoplay rules.
   */
  unlock(): void {
    for (const el of [this.active, this.standby]) {
      if (el.src && el.src !== SILENT_WAV_DATA_URL) continue;
      el.src = SILENT_WAV_DATA_URL;
      try {
        const p = el.play();
        if (p && typeof p.then === "function") {
          p.then(() => {
            el.pause();
            if (el.src === SILENT_WAV_DATA_URL) {
              el.src = "";
            }
          }).catch(() => {
            // Ignore unlock failures on headless/non-gesture calls.
          });
        }
      } catch {
        // Ignore synchronous play() throws on restricted DOM shims.
      }
    }
    this.callbacks.onAutoplayBlocked?.(false);
  }

  /**
   * Resumes playback from a user click after an AUTOPLAY_BLOCKED rejection.
   */
  async resumeFromGesture(): Promise<void> {
    this.callbacks.onAutoplayBlocked?.(false);
    if (!this.currentURI) {
      this.unlock();
      return;
    }
    try {
      await this.active.play();
      this.emitState("PLAYING");
    } catch {
      this.callbacks.onAutoplayBlocked?.(true);
    }
  }

  async handleCommand(cmd: PlayerCommand): Promise<void> {
    if (this.disposed) {
      this.ack(cmd.id, false, "DISCONNECTED", "player engine disposed");
      return;
    }

    switch (cmd.type) {
      case "set_uri": {
        if (!cmd.media?.url) {
          this.ack(cmd.id, false, "INVALID_MEDIA", "missing media URL");
          return;
        }
        try {
          this.clearStandby();
          this.currentMedia = cmd.media;
          this.currentURI = cmd.media.url;
          this.active.pause();
          this.active.src = this.resolve(cmd.media.url);
          this.active.volume = this.volume / 100;
          this.active.muted = this.muted;
          this.active.load();
          this.updateMediaSessionMetadata(cmd.media);
          this.ack(cmd.id, true, undefined, undefined, "TRANSITIONING");
        } catch (err) {
          this.ack(cmd.id, false, "LOAD_ERROR", err instanceof Error ? err.message : "failed to load media");
        }
        return;
      }

      case "set_next_uri": {
        if (!cmd.media?.url) {
          this.clearStandby();
          this.ack(cmd.id, true);
          return;
        }
        try {
          this.nextMedia = cmd.media;
          this.nextURI = cmd.media.url;
          this.standby.pause();
          this.standby.src = this.resolve(cmd.media.url);
          this.standby.volume = this.volume / 100;
          this.standby.muted = this.muted;
          this.standby.load();
          this.ack(cmd.id, true);
        } catch (err) {
          this.clearStandby();
          this.ack(cmd.id, false, "LOAD_ERROR", err instanceof Error ? err.message : "failed to preload next track");
        }
        return;
      }

      case "play": {
        if (!this.currentURI) {
          this.ack(cmd.id, false, "NO_MEDIA", "no track loaded");
          return;
        }
        try {
          await this.active.play();
          this.callbacks.onAutoplayBlocked?.(false);
          this.ack(cmd.id, true, undefined, undefined, "PLAYING");
        } catch (err) {
          const isAutoplay = isNotAllowedError(err);
          if (isAutoplay) {
            this.callbacks.onAutoplayBlocked?.(true);
            this.ack(
              cmd.id,
              false,
              "PLAYBACK_BLOCKED",
              "Browser blocked autoplay — click inside the player tab to unlock audio",
              "PAUSED_PLAYBACK",
            );
          } else {
            const msg = err instanceof Error ? err.message : "Audio playback failed";
            this.ack(cmd.id, false, "PLAYBACK_ERROR", msg, "STOPPED");
          }
        }
        return;
      }

      case "pause": {
        this.active.pause();
        this.ack(cmd.id, true, undefined, undefined, "PAUSED_PLAYBACK");
        return;
      }

      case "stop": {
        this.active.pause();
        try {
          this.active.currentTime = 0;
        } catch {
          // Ignore seek reset if metadata is not yet loaded.
        }
        this.clearStandby();
        this.ack(cmd.id, true, undefined, undefined, "STOPPED");
        return;
      }

      case "seek": {
        const raw = cmd.positionSec ?? 0;
        if (!Number.isFinite(raw) || raw < 0) {
          this.ack(cmd.id, false, "INVALID_POSITION", "seek position must be a non-negative finite number");
          return;
        }
        try {
          this.active.currentTime = raw;
        } catch (err) {
          this.ack(cmd.id, false, "SEEK_ERROR", err instanceof Error ? err.message : "seek failed");
          return;
        }
        const st = this.active.paused ? "PAUSED_PLAYBACK" : "PLAYING";
        this.ack(cmd.id, true, undefined, undefined, st);
        return;
      }

      case "set_volume": {
        const v = Math.max(0, Math.min(100, Math.round(cmd.volume ?? this.volume)));
        this.volume = v;
        this.active.volume = v / 100;
        this.standby.volume = v / 100;
        this.ack(cmd.id, true);
        return;
      }

      case "set_mute": {
        this.muted = Boolean(cmd.muted);
        this.active.muted = this.muted;
        this.standby.muted = this.muted;
        this.ack(cmd.id, true);
        return;
      }

      default: {
        this.ack(cmd.id, false, "UNSUPPORTED_COMMAND", `unsupported command type: ${String((cmd as { type?: unknown }).type)}`);
      }
    }
  }

  dispose(): void {
    this.disposed = true;
    if (this.progressTimer !== null) {
      clearInterval(this.progressTimer);
      this.progressTimer = null;
    }
    this.activeCleanup?.();
    this.activeCleanup = null;
    this.active.pause();
    this.standby.pause();
    this.active.src = "";
    this.standby.src = "";
    this.clearMediaSession();
  }

  private resolve(url: string): string {
    return this.callbacks.resolveUrl ? this.callbacks.resolveUrl(url) : url;
  }

  private clearStandby(): void {
    this.standby.pause();
    this.standby.src = "";
    this.nextMedia = null;
    this.nextURI = "";
  }

  private attachActiveListeners(el: AudioLike): void {
    this.activeCleanup?.();

    const onPlaying = () => {
      if (this.disposed || el !== this.active || !this.currentURI) return;
      this.setMediaSessionPlaybackState("playing");
      this.emitState("PLAYING");
    };
    const onPause = () => {
      if (this.disposed || el !== this.active || el.ended || !this.currentURI) return;
      this.setMediaSessionPlaybackState("paused");
      this.emitState("PAUSED_PLAYBACK");
    };
    const onWaiting = () => {
      if (this.disposed || el !== this.active || !this.currentURI) return;
      this.emitState("TRANSITIONING");
    };
    const onEnded = () => {
      if (this.disposed || el !== this.active) return;
      void this.handleTrackEnded();
    };
    const onError = () => {
      if (this.disposed || el !== this.active || !this.currentURI) return;
      this.emitState("STOPPED");
    };

    el.addEventListener("playing", onPlaying);
    el.addEventListener("pause", onPause);
    el.addEventListener("waiting", onWaiting);
    el.addEventListener("ended", onEnded);
    el.addEventListener("error", onError);

    this.activeCleanup = () => {
      el.removeEventListener("playing", onPlaying);
      el.removeEventListener("pause", onPause);
      el.removeEventListener("waiting", onWaiting);
      el.removeEventListener("ended", onEnded);
      el.removeEventListener("error", onError);
    };
  }

  private async handleTrackEnded(): Promise<void> {
    if (this.nextURI && this.nextMedia) {
      // Gapless swap: promote preloaded standby element to active immediately.
      const prev = this.active;
      const promoted = this.standby;
      const promotedMedia = this.nextMedia;
      const promotedURI = this.nextURI;

      prev.pause();
      prev.src = "";

      this.active = promoted;
      this.standby = prev;
      this.currentMedia = promotedMedia;
      this.currentURI = promotedURI;
      this.nextMedia = null;
      this.nextURI = "";
      this.active.volume = this.volume / 100;
      this.active.muted = this.muted;
      this.attachActiveListeners(this.active);
      this.updateMediaSessionMetadata(promotedMedia);

      try {
        await this.active.play();
        this.setMediaSessionPlaybackState("playing");
        this.emitState("PLAYING");
      } catch (err) {
        if (isNotAllowedError(err)) {
          this.callbacks.onAutoplayBlocked?.(true);
        }
        this.emitState("STOPPED");
      }
      return;
    }

    this.setMediaSessionPlaybackState("none");
    this.emitState("STOPPED");
  }

  private ack(
    cmdId: string,
    ok: boolean,
    errorCode?: string,
    errorMessage?: string,
    state?: PlayerStateReport["state"],
  ): void {
    this.callbacks.onReportState({
      cmdId,
      ok,
      errorCode,
      errorMessage,
      state,
      positionSec: safeTime(this.active.currentTime),
      durationSec: this.effectiveDuration(),
      trackUri: this.currentURI || undefined,
      volume: this.volume,
      muted: this.muted,
    });
  }

  private emitState(state: PlayerStateReport["state"]): void {
    this.callbacks.onReportState({
      state,
      positionSec: safeTime(this.active.currentTime),
      durationSec: this.effectiveDuration(),
      trackUri: this.currentURI || undefined,
      volume: this.volume,
      muted: this.muted,
    });
  }

  private effectiveDuration(): number | undefined {
    if (Number.isFinite(this.active.duration) && this.active.duration > 0) {
      return this.active.duration;
    }
    if (this.currentMedia?.durationSec && this.currentMedia.durationSec > 0) {
      return this.currentMedia.durationSec;
    }
    return undefined;
  }

  private setupMediaSession(): void {
    if (typeof navigator === "undefined" || !("mediaSession" in navigator) || !navigator.mediaSession) return;
    const ms = navigator.mediaSession;
    const actions: Array<"play" | "pause" | "previoustrack" | "nexttrack"> = [
      "play",
      "pause",
      "previoustrack",
      "nexttrack",
    ];
    for (const action of actions) {
      try {
        ms.setActionHandler(action, () => this.callbacks.onMediaSessionAction?.(action));
      } catch {
        // Unsupported MediaSession action in this browser.
      }
    }
    try {
      ms.setActionHandler("seekto", (details) => {
        if (typeof details.seekTime === "number") {
          this.callbacks.onMediaSessionAction?.("seekto", details.seekTime);
        }
      });
    } catch {
      // Unsupported seekto handler.
    }
  }

  private updateMediaSessionMetadata(media: PlayerMediaPayload): void {
    if (typeof navigator === "undefined" || !("mediaSession" in navigator) || !navigator.mediaSession) return;
    if (typeof MediaMetadata === "undefined") return;
    const artwork = media.artworkUrl
      ? [{ src: this.resolve(media.artworkUrl) }]
      : [];
    try {
      navigator.mediaSession.metadata = new MediaMetadata({
        title: media.title || "Unknown Title",
        artist: media.artist || "",
        album: media.album || "",
        artwork,
      });
    } catch {
      // Ignore MediaMetadata failures in restricted environments.
    }
  }

  private setMediaSessionPlaybackState(state: MediaSessionPlaybackState): void {
    if (typeof navigator === "undefined" || !("mediaSession" in navigator) || !navigator.mediaSession) return;
    try {
      navigator.mediaSession.playbackState = state;
    } catch {
      // Ignore playbackState errors.
    }
  }

  private clearMediaSession(): void {
    if (typeof navigator === "undefined" || !("mediaSession" in navigator) || !navigator.mediaSession) return;
    const ms = navigator.mediaSession;
    for (const action of ["play", "pause", "previoustrack", "nexttrack", "seekto"] as const) {
      try {
        ms.setActionHandler(action, null);
      } catch {
        // Ignore cleanup errors.
      }
    }
  }
}

function safeTime(val: number): number {
  return Number.isFinite(val) && val >= 0 ? val : 0;
}

function isNotAllowedError(err: unknown): boolean {
  if (!err || typeof err !== "object") return false;
  const name = "name" in err ? String((err as { name?: unknown }).name) : "";
  return name === "NotAllowedError";
}
