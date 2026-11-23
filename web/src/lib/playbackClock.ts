/**
 * Smooth monotonic playback clock for the UI.
 *
 * The client ticks forward steadily whenever a zone is playing. When the server
 * sends an updated position for the same track:
 * - Small jitter (|delta| <= JITTER_THRESHOLD_SEC) gently slews the tick rate
 *   (0.85x–1.15x) so the displayed second converges smoothly without ever
 *   jumping backward.
 * - Large jumps (seeks, scrubs, track changes, zone switches) snap immediately.
 */

export const JITTER_THRESHOLD_SEC = 4;
const MIN_SLEW_RATE = 0.85;
const MAX_SLEW_RATE = 1.15;

export interface PlaybackClockState {
  zoneId: string;
  trackKey: string;
  displayPos: number;
  targetPos: number;
  targetUpdatedMs: number;
  lastTickMs: number;
  playing: boolean;
  durationSec: number;
}

export interface PlaybackClockInput {
  zoneId: string;
  trackKey: string;
  playing: boolean;
  serverPosSec: number;
  positionUpdatedAtMs: number;
  durationSec: number;
  nowMs: number;
}

function clampPos(pos: number, durationSec: number): number {
  const nonNeg = Math.max(0, pos);
  return durationSec > 0 ? Math.min(durationSec, nonNeg) : nonNeg;
}

export function estimateServerPos(
  serverPosSec: number,
  positionUpdatedAtMs: number,
  playing: boolean,
  durationSec: number,
  nowMs: number,
): number {
  if (!playing || !Number.isFinite(positionUpdatedAtMs)) {
    return clampPos(serverPosSec, durationSec);
  }
  const elapsedSec = Math.max(0, (nowMs - positionUpdatedAtMs) / 1000);
  return clampPos(serverPosSec + elapsedSec, durationSec);
}

/**
 * Reconciles a new server snapshot/event into the local clock state.
 */
export function syncPlaybackClock(
  prev: PlaybackClockState | null,
  input: PlaybackClockInput,
): PlaybackClockState {
  const { zoneId, trackKey, playing, serverPosSec, positionUpdatedAtMs, durationSec, nowMs } = input;
  const estServer = estimateServerPos(serverPosSec, positionUpdatedAtMs, playing, durationSec, nowMs);

  const sameTrack = prev !== null && prev.zoneId === zoneId && prev.trackKey === trackKey;
  if (!sameTrack) {
    return {
      zoneId,
      trackKey,
      displayPos: estServer,
      targetPos: serverPosSec,
      targetUpdatedMs: positionUpdatedAtMs,
      lastTickMs: nowMs,
      playing,
      durationSec,
    };
  }

  // Advance previous displayPos up to nowMs before comparing so we don't lose
  // sub-interval progress on an SSE update.
  const current = prev.playing ? tickPlaybackClock(prev, nowMs).displayPos : prev.displayPos;
  const delta = estServer - current;

  // Snap immediately on seeks/large jumps, or when paused/stopped with a real jump.
  // For small jitter on the same playing track, keep displayPos monotonic and let
  // tickPlaybackClock smoothly slew into estServer.
  const shouldSnap = Math.abs(delta) > JITTER_THRESHOLD_SEC || (!playing && Math.abs(delta) > 1);

  return {
    zoneId,
    trackKey,
    displayPos: shouldSnap ? estServer : current,
    targetPos: serverPosSec,
    targetUpdatedMs: positionUpdatedAtMs,
    lastTickMs: nowMs,
    playing,
    durationSec,
  };
}

/**
 * Advances the local clock by elapsed wall time, slewing gently toward the
 * latest server target if within jitter bounds so it never jumps backward.
 */
export function tickPlaybackClock(state: PlaybackClockState, nowMs: number): PlaybackClockState {
  if (!state.playing) {
    return { ...state, lastTickMs: nowMs };
  }
  const dtSec = Math.max(0, (nowMs - state.lastTickMs) / 1000);
  if (dtSec === 0) return state;

  const estServerAtPrev = estimateServerPos(
    state.targetPos,
    state.targetUpdatedMs,
    true,
    state.durationSec,
    state.lastTickMs,
  );
  const delta = estServerAtPrev - state.displayPos;

  // Gently adjust tick rate (±15% max) so small drift converges over ~10-15s
  // while always moving smoothly forward.
  let rate = 1;
  if (Math.abs(delta) > 0.15) {
    rate = Math.max(MIN_SLEW_RATE, Math.min(MAX_SLEW_RATE, 1 + delta * 0.15));
  }

  const nextPos = clampPos(state.displayPos + dtSec * rate, state.durationSec);
  return {
    ...state,
    displayPos: nextPos,
    lastTickMs: nowMs,
  };
}
