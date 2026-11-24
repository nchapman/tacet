export interface HoverIntentOptions {
  /** Sampling interval in ms to check whether cursor has slowed down (default 65ms). */
  intervalMs?: number;
  /** Max pointer movement (Manhattan px) over intervalMs to treat as intentional hover (default 7px). */
  sensitivityPx?: number;
}

export interface HoverIntentHandlers {
  onMouseEnter: (e: { clientX: number; clientY: number }) => void;
  onMouseMove: (e: { clientX: number; clientY: number }) => void;
  onMouseLeave: () => void;
  onFocus: () => void;
  onBlur: () => void;
}

// Only one hover-intent check can be pending across the page at a time so moving
// from card A to card B automatically cancels A with zero leaked timers.
let activeCancel: (() => void) | null = null;

export function cancelActiveHoverIntent(): void {
  if (activeCancel) {
    const fn = activeCancel;
    activeCancel = null;
    fn();
  }
}

/**
 * Velocity-aware hoverIntent (inspired by Brian Cherne's hoverIntent):
 * Ignores fast cursor sweeps across large cards/links and fires `onIntent` once
 * when the pointer slows down or settles over the target, or when focused via keyboard.
 */
export function createHoverIntent(
  onIntent: () => void,
  { intervalMs = 65, sensitivityPx = 7 }: HoverIntentOptions = {},
): HoverIntentHandlers {
  let timer: ReturnType<typeof setTimeout> | null = null;
  let fired = false;
  let prevX = 0;
  let prevY = 0;
  let curX = 0;
  let curY = 0;

  const cancel = () => {
    if (timer !== null) {
      clearTimeout(timer);
      timer = null;
    }
    if (activeCancel === cancel) {
      activeCancel = null;
    }
  };

  const checkVelocity = () => {
    timer = null;
    const dist = Math.abs(curX - prevX) + Math.abs(curY - prevY);
    if (dist <= sensitivityPx) {
      fired = true;
      if (activeCancel === cancel) activeCancel = null;
      onIntent();
      return;
    }
    // Pointer is still moving fast across this element; keep sampling until it settles or leaves.
    prevX = curX;
    prevY = curY;
    timer = setTimeout(checkVelocity, intervalMs);
  };

  return {
    onMouseEnter(e) {
      cancelActiveHoverIntent();
      fired = false;
      prevX = e.clientX;
      prevY = e.clientY;
      curX = e.clientX;
      curY = e.clientY;
      activeCancel = cancel;
      timer = setTimeout(checkVelocity, intervalMs);
    },
    onMouseMove(e) {
      curX = e.clientX;
      curY = e.clientY;
    },
    onMouseLeave() {
      cancel();
      fired = false;
    },
    onFocus() {
      if (fired) return;
      cancelActiveHoverIntent();
      activeCancel = cancel;
      timer = setTimeout(() => {
        timer = null;
        fired = true;
        if (activeCancel === cancel) activeCancel = null;
        onIntent();
      }, intervalMs);
    },
    onBlur() {
      cancel();
      fired = false;
    },
  };
}
