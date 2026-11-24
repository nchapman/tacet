import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { cancelActiveHoverIntent, createHoverIntent } from "./hoverIntent";

describe("createHoverIntent", () => {
  beforeEach(() => {
    vi.useFakeTimers();
    cancelActiveHoverIntent();
  });

  afterEach(() => {
    cancelActiveHoverIntent();
    vi.useRealTimers();
  });

  it("ignores fast cursor sweeps across a wide card until the pointer settles", () => {
    const onIntent = vi.fn();
    const h = createHoverIntent(onIntent, { intervalMs: 65, sensitivityPx: 7 });

    h.onMouseEnter({ clientX: 0, clientY: 0 });
    h.onMouseMove({ clientX: 40, clientY: 12 });
    vi.advanceTimersByTime(65);
    expect(onIntent).not.toHaveBeenCalled();

    // Still sweeping across the card
    h.onMouseMove({ clientX: 110, clientY: 30 });
    vi.advanceTimersByTime(65);
    expect(onIntent).not.toHaveBeenCalled();

    // Pointer slows down & settles
    h.onMouseMove({ clientX: 112, clientY: 31 });
    vi.advanceTimersByTime(65);
    expect(onIntent).toHaveBeenCalledTimes(1);

    // Staying over the card does not re-fire
    vi.advanceTimersByTime(200);
    expect(onIntent).toHaveBeenCalledTimes(1);
  });

  it("cancels cleanly when leaving before the pointer settles", () => {
    const onIntent = vi.fn();
    const h = createHoverIntent(onIntent, { intervalMs: 65, sensitivityPx: 7 });

    h.onMouseEnter({ clientX: 10, clientY: 10 });
    vi.advanceTimersByTime(40);
    h.onMouseLeave();
    vi.advanceTimersByTime(100);
    expect(onIntent).not.toHaveBeenCalled();
  });

  it("cancels the previous target when moving directly to another target", () => {
    const first = vi.fn();
    const second = vi.fn();
    const h1 = createHoverIntent(first, { intervalMs: 65 });
    const h2 = createHoverIntent(second, { intervalMs: 65 });

    h1.onMouseEnter({ clientX: 10, clientY: 10 });
    vi.advanceTimersByTime(30);
    h2.onMouseEnter({ clientX: 20, clientY: 20 });
    vi.advanceTimersByTime(65);

    expect(first).not.toHaveBeenCalled();
    expect(second).toHaveBeenCalledTimes(1);
  });

  it("supports keyboard focus and cancels on quick tab-through", () => {
    const first = vi.fn();
    const second = vi.fn();
    const h1 = createHoverIntent(first, { intervalMs: 65 });
    const h2 = createHoverIntent(second, { intervalMs: 65 });

    h1.onFocus();
    vi.advanceTimersByTime(30);
    h1.onBlur();
    h2.onFocus();
    vi.advanceTimersByTime(65);

    expect(first).not.toHaveBeenCalled();
    expect(second).toHaveBeenCalledTimes(1);
  });
});
