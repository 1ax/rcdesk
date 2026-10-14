import { describe, expect, it } from "vitest";
import {
  TouchRecognizer,
  VirtualCursor,
  WheelAccumulator,
  type TouchAction,
  type TouchEventKind,
} from "./touchInput";

function ev(kind: TouchEventKind, id: number, x = 0, y = 0) {
  return { kind, id, x, y };
}

const click = (button: "left" | "right"): TouchAction[] => [
  { kind: "button", button, pressed: true },
  { kind: "button", button, pressed: false },
];

describe("TouchRecognizer", () => {
  it("turns a quick tap into a left click", () => {
    const r = new TouchRecognizer();
    expect(r.handle(ev("down", 1, 100, 100), 0)).toEqual([]);
    expect(r.handle(ev("up", 1, 102, 101), 120)).toEqual(click("left"));
    expect(r.nextTimerAt()).toBeNull();
  });

  it("does not click when the finger is held too long without a timer", () => {
    const r = new TouchRecognizer();
    r.handle(ev("down", 1), 0);
    expect(r.handle(ev("up", 1), 400)).toEqual([]);
  });

  it("moves the cursor by finger deltas and gives no click on up", () => {
    const r = new TouchRecognizer();
    r.handle(ev("down", 1, 100, 100), 0);
    expect(r.handle(ev("move", 1, 120, 100), 20)).toEqual([{ kind: "move", dx: 20, dy: 0 }]);
    expect(r.handle(ev("move", 1, 125, 90), 40)).toEqual([{ kind: "move", dx: 5, dy: -10 }]);
    expect(r.handle(ev("up", 1, 125, 90), 80)).toEqual([]);
  });

  it("long press starts a drag, moves drag, up releases", () => {
    const r = new TouchRecognizer();
    r.handle(ev("down", 1, 50, 50), 1000);
    expect(r.nextTimerAt()).toBe(1500);
    expect(r.handle(ev("timer", 1), 1500)).toEqual([{ kind: "drag", pressed: true }]);
    expect(r.nextTimerAt()).toBeNull();
    expect(r.handle(ev("move", 1, 60, 70), 1600)).toEqual([{ kind: "move", dx: 10, dy: 20 }]);
    expect(r.handle(ev("up", 1, 60, 70), 1700)).toEqual([{ kind: "drag", pressed: false }]);
  });

  it("ignores an early timer and a timer after movement", () => {
    const r = new TouchRecognizer();
    r.handle(ev("down", 1, 0, 0), 0);
    expect(r.handle(ev("timer", 1), 100)).toEqual([]);
    r.handle(ev("move", 1, 30, 0), 200);
    expect(r.nextTimerAt()).toBeNull();
    expect(r.handle(ev("timer", 1), 600)).toEqual([]);
  });

  it("two-finger tap is a right click", () => {
    const r = new TouchRecognizer();
    r.handle(ev("down", 1, 100, 100), 0);
    r.handle(ev("down", 2, 160, 100), 10);
    expect(r.handle(ev("up", 1, 100, 100), 100)).toEqual([]);
    expect(r.handle(ev("up", 2, 160, 100), 120)).toEqual(click("right"));
  });

  it("two-finger move scrolls by centroid deltas and gives no right click", () => {
    const r = new TouchRecognizer();
    r.handle(ev("down", 1, 100, 100), 0);
    r.handle(ev("down", 2, 160, 100), 10);
    expect(r.handle(ev("move", 1, 100, 130), 30)).toEqual([{ kind: "scroll", dx: 0, dy: 15 }]);
    expect(r.handle(ev("move", 2, 160, 130), 40)).toEqual([{ kind: "scroll", dx: 0, dy: 15 }]);
    expect(r.handle(ev("up", 1), 60)).toEqual([]);
    expect(r.handle(ev("up", 2), 70)).toEqual([]);
  });

  it("ignores a third finger", () => {
    const r = new TouchRecognizer();
    r.handle(ev("down", 1, 0, 0), 0);
    r.handle(ev("down", 2, 50, 0), 5);
    expect(r.handle(ev("down", 3, 90, 0), 10)).toEqual([]);
    expect(r.handle(ev("move", 3, 99, 0), 20)).toEqual([]);
  });

  it("cancel during a drag releases the button and resets state", () => {
    const r = new TouchRecognizer();
    r.handle(ev("down", 1), 0);
    r.handle(ev("timer", 1), 500);
    expect(r.handle(ev("cancel", 1), 600)).toEqual([{ kind: "drag", pressed: false }]);
    // Clean state: a fresh tap works.
    r.handle(ev("down", 2, 5, 5), 700);
    expect(r.handle(ev("up", 2, 5, 5), 750)).toEqual(click("left"));
  });

  it("cancel without a drag emits nothing and resets state", () => {
    const r = new TouchRecognizer();
    r.handle(ev("down", 1), 0);
    expect(r.handle(ev("cancel", 1), 50)).toEqual([]);
    expect(r.nextTimerAt()).toBeNull();
  });
});

describe("WheelAccumulator", () => {
  it("returns whole lines and keeps the remainder", () => {
    const w = new WheelAccumulator();
    expect(w.add(0, 30)).toEqual({ dx: 0, dy: 0 });
    expect(w.add(0, 30)).toEqual({ dx: 0, dy: 1 });
    expect(w.add(0, 20)).toEqual({ dx: 0, dy: 1 });
  });

  it("works per axis and for negative deltas", () => {
    const w = new WheelAccumulator();
    expect(w.add(-90, 0)).toEqual({ dx: -2, dy: 0 });
    expect(w.add(-30, 0)).toEqual({ dx: -1, dy: 0 });
  });
});

describe("VirtualCursor", () => {
  it("starts at the center and converts pixels to fractions", () => {
    const c = new VirtualCursor();
    expect([c.x, c.y]).toEqual([0.5, 0.5]);
    c.moveBy(100, -50, 1000, 500);
    expect(c.x).toBeCloseTo(0.6);
    expect(c.y).toBeCloseTo(0.4);
  });

  it("clamps to [0,1]", () => {
    const c = new VirtualCursor();
    c.moveBy(5000, -5000, 1000, 500);
    expect([c.x, c.y]).toEqual([1, 0]);
  });
});
