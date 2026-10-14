// Touch "trackpad" gesture recognizer for the mobile client (slice H1).
//
// A finger on the screen drives a *virtual* cursor relative to its own
// movement, like a laptop trackpad, instead of acting as a pressed left
// button at the touch point (which is what a raw `pointerType === "touch"`
// PointerEvent looks like to `input.ts`). This module is pure logic with no
// DOM and no clock of its own -- events carry CSS-pixel coordinates and the
// caller passes `now` (ms) -- so it runs under vitest's `node` environment,
// like `layoutChord.ts`.
//
// Gestures:
//   - one-finger tap (<= 300 ms, shift < 10 px)  -> left click
//   - one-finger move                            -> cursor move (1:1)
//   - one-finger long press (>= 500 ms, < 10 px) -> left button held (drag);
//     moving drags, lifting releases
//   - two-finger tap (<= 300 ms, centroid shift < 10 px) -> right click
//   - two-finger move                            -> scroll
// A double tap is not recognized here: two taps are two clicks and the
// host OS assembles the double click by timing. A third finger is ignored. Slice H3 adds pinch
// and a "direct" mode (taps click at the touch point, one finger pans).

import type { TouchMode } from "./viewport";

export type TouchEventKind = "down" | "move" | "up" | "cancel" | "timer";

export interface TouchEvent {
  kind: TouchEventKind;
  id: number;
  x: number;
  y: number;
}

export type TouchAction =
  | { kind: "move"; dx: number; dy: number }
  | { kind: "button"; button: "left" | "right"; pressed: boolean }
  | { kind: "scroll"; dx: number; dy: number }
  | { kind: "drag"; pressed: boolean }
  // Slice H3: pinch (centroid in client px), view pan and direct-mode click.
  | { kind: "pinch"; factor: number; cx: number; cy: number }
  | { kind: "pan"; dx: number; dy: number }
  | { kind: "click"; button: "left" | "right"; x: number; y: number };

export const TAP_MAX_MS = 300;
export const LONG_PRESS_MS = 500;
export const SLOP_PX = 10;

type Phase =
  | "idle"
  | "pending" // one finger down, not yet moved past the slop
  | "moving" // one finger leading the cursor
  | "drag" // long press: left button held
  | "two" // two fingers down
  | "tail"; // two-finger gesture over, waiting for the rest to lift

interface Finger {
  x: number;
  y: number;
}

export class TouchRecognizer {
  private phase: Phase = "idle";
  private fingers = new Map<number, Finger>();
  private downAt = 0;
  private startX = 0;
  private startY = 0;
  // Two-finger state.
  private twoAt = 0;
  private twoStartCx = 0;
  private twoStartCy = 0;
  private lastCx = 0;
  private lastCy = 0;
  private twoKind: "none" | "scroll" | "pinch" = "none";
  private tapLike = false;
  private dPrev = 0;
  private mode: TouchMode = "trackpad";

  private readonly getMode: () => TouchMode;

  constructor(getMode: () => TouchMode = () => "trackpad") {
    this.getMode = getMode;
  }

  /** When the caller must feed a `timer` event (ms, same clock as `now`),
   * or `null` if no timer is needed. */
  nextTimerAt(): number | null {
    return this.phase === "pending" ? this.downAt + LONG_PRESS_MS : null;
  }

  handle(ev: TouchEvent, now: number): TouchAction[] {
    switch (ev.kind) {
      case "down":
        return this.onDown(ev, now);
      case "move":
        return this.onMove(ev);
      case "up":
        return this.onUp(ev, now);
      case "cancel":
        return this.onCancel(ev);
      case "timer":
        return this.onTimer(now);
    }
  }

  private reset(): void {
    this.phase = "idle";
    this.fingers.clear();
    this.twoKind = "none";
    this.tapLike = false;
  }

  private distance(): number {
    const [a, b] = [...this.fingers.values()];
    return a && b ? Math.hypot(a.x - b.x, a.y - b.y) : 0;
  }

  private centroid(): { cx: number; cy: number } {
    let sx = 0;
    let sy = 0;
    for (const f of this.fingers.values()) {
      sx += f.x;
      sy += f.y;
    }
    const n = this.fingers.size || 1;
    return { cx: sx / n, cy: sy / n };
  }

  private onDown(ev: TouchEvent, now: number): TouchAction[] {
    if (this.fingers.has(ev.id)) return [];
    if (this.phase === "idle") {
      this.fingers.set(ev.id, { x: ev.x, y: ev.y });
      this.phase = "pending";
      this.mode = this.getMode();
      this.downAt = now;
      this.startX = ev.x;
      this.startY = ev.y;
      return [];
    }
    if ((this.phase === "pending" || this.phase === "moving") && this.fingers.size === 1) {
      this.fingers.set(ev.id, { x: ev.x, y: ev.y });
      this.phase = "two";
      this.twoAt = now;
      this.twoKind = "none";
      this.dPrev = this.distance();
      const { cx, cy } = this.centroid();
      this.twoStartCx = this.lastCx = cx;
      this.twoStartCy = this.lastCy = cy;
    }
    // Third finger, or a second one during a drag/tail: ignored.
    return [];
  }

  private onMove(ev: TouchEvent): TouchAction[] {
    const f = this.fingers.get(ev.id);
    if (!f) return [];
    const dx = ev.x - f.x;
    const dy = ev.y - f.y;
    f.x = ev.x;
    f.y = ev.y;
    switch (this.phase) {
      case "pending":
        if (this.mode === "direct") {
          if (Math.hypot(ev.x - this.startX, ev.y - this.startY) < SLOP_PX) return [];
          this.phase = "moving";
          const pdx = ev.x - this.startX;
          const pdy = ev.y - this.startY;
          return [{ kind: "pan", dx: pdx, dy: pdy }];
        }
        if (Math.hypot(ev.x - this.startX, ev.y - this.startY) >= SLOP_PX) {
          this.phase = "moving";
        }
        return dx === 0 && dy === 0 ? [] : [{ kind: "move", dx, dy }];
      case "moving":
      case "drag":
        if (this.mode === "direct") {
          return dx === 0 && dy === 0 ? [] : [{ kind: "pan", dx, dy }];
        }
        return dx === 0 && dy === 0 ? [] : [{ kind: "move", dx, dy }];
      case "two": {
        const { cx, cy } = this.centroid();
        const sdx = cx - this.lastCx;
        const sdy = cy - this.lastCy;
        this.lastCx = cx;
        this.lastCy = cy;
        const d = this.distance();
        if (this.twoKind === "none") {
          const shift = Math.hypot(cx - this.twoStartCx, cy - this.twoStartCy);
          const spread = Math.abs(d - this.dPrev);
          if (shift < SLOP_PX && spread < SLOP_PX) return [];
          this.twoKind = spread > shift ? "pinch" : "scroll";
        }
        if (this.twoKind === "scroll") {
          return sdx === 0 && sdy === 0 ? [] : [{ kind: "scroll", dx: sdx, dy: sdy }];
        }
        const prev = this.dPrev;
        this.dPrev = d;
        if (prev <= 0 || d <= 0 || d === prev) return [];
        return [{ kind: "pinch", factor: d / prev, cx, cy }];
      }
      default:
        return [];
    }
  }

  private onUp(ev: TouchEvent, now: number): TouchAction[] {
    if (!this.fingers.has(ev.id)) return [];
    const phase = this.phase;
    this.fingers.delete(ev.id);
    switch (phase) {
      case "pending": {
        const tap = now - this.downAt <= TAP_MAX_MS;
        this.reset();
        if (!tap) return [];
        if (this.mode === "direct") {
          return [{ kind: "click", button: "left", x: this.startX, y: this.startY }];
        }
        return [
          { kind: "button", button: "left", pressed: true },
          { kind: "button", button: "left", pressed: false },
        ];
      }
      case "moving":
        this.reset();
        return [];
      case "drag":
        this.reset();
        return [{ kind: "drag", pressed: false }];
      case "two": {
        // Remember whether this still looks like a two-finger tap; the
        // right click fires when the *second* finger lifts.
        const tapLike = this.twoKind === "none" && now - this.twoAt <= TAP_MAX_MS;
        this.phase = "tail";
        this.tapLike = tapLike;
        return [];
      }
      case "tail": {
        const tapLike = this.tapLike && now - this.twoAt <= TAP_MAX_MS;
        this.reset();
        if (tapLike && this.mode === "direct") {
          return [{ kind: "click", button: "right", x: this.twoStartCx, y: this.twoStartCy }];
        }
        return tapLike
          ? [
              { kind: "button", button: "right", pressed: true },
              { kind: "button", button: "right", pressed: false },
            ]
          : [];
      }
      default:
        return [];
    }
  }

  private onCancel(ev: TouchEvent): TouchAction[] {
    if (!this.fingers.has(ev.id)) return [];
    const wasDrag = this.phase === "drag";
    this.reset();
    return wasDrag ? [{ kind: "drag", pressed: false }] : [];
  }

  private onTimer(now: number): TouchAction[] {
    if (this.phase === "pending" && now >= this.downAt + LONG_PRESS_MS) {
      if (this.mode === "direct") {
        // Long press: right click at the touch point, then the finger pans.
        this.phase = "moving";
        return [{ kind: "click", button: "right", x: this.startX, y: this.startY }];
      }
      this.phase = "drag";
      return [{ kind: "drag", pressed: true }];
    }
    return [];
  }
}

/** Pixels per wheel "line" -- the same convention as `normalizeWheel` in
 * `input.ts` for `DOM_DELTA_PIXEL`. */
export const PX_PER_LINE = 40;

/** Accumulates scroll pixels per axis and hands out whole lines (the host
 * rounds `wheel` deltas, so fractions below 0.5 would be lost); the
 * remainder is kept for the next `add`. */
export class WheelAccumulator {
  private remX = 0;
  private remY = 0;

  add(dxPx: number, dyPx: number): { dx: number; dy: number } {
    this.remX += dxPx;
    this.remY += dyPx;
    const dx = Math.trunc(this.remX / PX_PER_LINE);
    const dy = Math.trunc(this.remY / PX_PER_LINE);
    this.remX -= dx * PX_PER_LINE;
    this.remY -= dy * PX_PER_LINE;
    return { dx, dy };
  }
}

/** The virtual cursor of trackpad mode: a normalized `[0,1]` position within
 * the video frame, starting at the center. */
export class VirtualCursor {
  x = 0.5;
  y = 0.5;

  moveBy(dxPx: number, dyPx: number, frameWidthPx: number, frameHeightPx: number): void {
    if (frameWidthPx > 0) this.x = Math.min(1, Math.max(0, this.x + dxPx / frameWidthPx));
    if (frameHeightPx > 0) this.y = Math.min(1, Math.max(0, this.y + dyPx / frameHeightPx));
  }
}
