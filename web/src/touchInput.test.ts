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

const click = (button: "left" | "right"): TouchAction[] => [{ kind: "tap", button }];

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
    expect(r.handle(ev("move", 1, 100, 130), 30)).toEqual([
      { kind: "scroll", dx: 0, dy: 15, cx: 130, cy: 115 },
    ]);
    expect(r.handle(ev("move", 2, 160, 130), 40)).toEqual([
      { kind: "scroll", dx: 0, dy: 15, cx: 130, cy: 130 },
    ]);
    expect(r.handle(ev("up", 1), 60)).toEqual([]);
    expect(r.handle(ev("up", 2), 70)).toEqual([]);
  });

  it("trackpad jitter below slop does not move", () => {
    const r = new TouchRecognizer();
    const actions: TouchAction[] = [];
    actions.push(...r.handle(ev("down", 1, 50, 50), 0));
    actions.push(...r.handle(ev("move", 1, 52, 51), 20));
    actions.push(...r.handle(ev("move", 1, 53, 52), 40));
    actions.push(...r.handle(ev("up", 1, 53, 52), 100));
    expect(actions.some((a) => a.kind === "move")).toBe(false);
    expect(actions).toEqual(click("left"));
  });

  it("trackpad first move carries the accumulated delta", () => {
    const r = new TouchRecognizer();
    r.handle(ev("down", 1, 0, 0), 0);
    expect(r.handle(ev("move", 1, 3, 0), 10)).toEqual([]);
    expect(r.handle(ev("move", 1, 12, 0), 20)).toEqual([{ kind: "move", dx: 12, dy: 0 }]);
    expect(r.handle(ev("move", 1, 15, 0), 30)).toEqual([{ kind: "move", dx: 3, dy: 0 }]);
  });

  it("double tap in direct mode reuses the first point", () => {
    const r = new TouchRecognizer(() => "direct");
    r.handle(ev("down", 1, 100, 100), 0);
    const first = r.handle(ev("up", 1, 100, 100), 50);
    r.handle(ev("down", 1, 104, 100), 250);
    const second = r.handle(ev("up", 1, 104, 100), 300);
    expect(first).toEqual([{ kind: "click", button: "left", x: 100, y: 100 }]);
    expect(second).toEqual(first);
    r.handle(ev("down", 1, 104, 100), 800);
    expect(r.handle(ev("up", 1, 104, 100), 850)).toEqual([
      { kind: "click", button: "left", x: 104, y: 100 },
    ]);
  });

  it("scroll carries the centroid", () => {
    const r = new TouchRecognizer(() => "direct");
    r.handle(ev("down", 1, 100, 100), 0);
    r.handle(ev("down", 2, 120, 100), 10);
    // Both fingers move by (0, -50); the gesture locks in as a scroll once
    // the centroid shift passes the slop.
    expect(r.handle(ev("move", 1, 100, 90), 20)).toEqual([]);
    expect(r.handle(ev("move", 2, 120, 90), 30)).toEqual([
      { kind: "scroll", dx: 0, dy: -5, cx: 110, cy: 90 },
    ]);
    r.handle(ev("move", 1, 100, 50), 40);
    expect(r.handle(ev("move", 2, 120, 50), 50)).toEqual([
      { kind: "scroll", dx: 0, dy: -20, cx: 110, cy: 50 },
    ]);
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

describe("TouchRecognizer pinch and direct mode (H3)", () => {
  it("pinch: growing distance gives pinch with factor and centroid, no scroll", () => {
    const r = new TouchRecognizer();
    r.handle(ev("down", 1, 100, 100), 0);
    r.handle(ev("down", 2, 200, 100), 10);
    const a = r.handle(ev("move", 2, 240, 100), 30);
    expect(a).toEqual([{ kind: "pinch", factor: 140 / 100, cx: 170, cy: 100 }]);
    const b = r.handle(ev("move", 1, 80, 100), 40);
    expect(b).toHaveLength(1);
    const act = b[0];
    expect(act?.kind).toBe("pinch");
    if (act?.kind === "pinch") expect(act.factor).toBeCloseTo(160 / 140);
    expect(r.handle(ev("up", 1), 60)).toEqual([]);
    expect(r.handle(ev("up", 2), 70)).toEqual([]);
  });

  it("a gesture that began as scroll never becomes a pinch", () => {
    const r = new TouchRecognizer();
    r.handle(ev("down", 1, 100, 100), 0);
    r.handle(ev("down", 2, 160, 100), 10);
    expect(r.handle(ev("move", 1, 100, 140), 30)[0]?.kind).toBe("scroll");
    const rest = r.handle(ev("move", 2, 260, 100), 40);
    expect(rest.every((x) => x.kind === "scroll")).toBe(true);
  });

  it("direct: tap clicks left at the touch point", () => {
    const r = new TouchRecognizer(() => "direct");
    r.handle(ev("down", 1, 40, 70), 0);
    expect(r.handle(ev("up", 1, 41, 70), 100)).toEqual([
      { kind: "click", button: "left", x: 40, y: 70 },
    ]);
  });

  it("direct: one-finger move pans after the slop", () => {
    const r = new TouchRecognizer(() => "direct");
    r.handle(ev("down", 1, 0, 0), 0);
    expect(r.handle(ev("move", 1, 5, 0), 10)).toEqual([]);
    expect(r.handle(ev("move", 1, 15, 0), 20)).toEqual([{ kind: "pan", dx: 15, dy: 0 }]);
    expect(r.handle(ev("move", 1, 20, 3), 30)).toEqual([{ kind: "pan", dx: 5, dy: 3 }]);
    expect(r.handle(ev("up", 1, 20, 3), 40)).toEqual([]);
  });

  it("direct: long press right-clicks, then moving pans", () => {
    const r = new TouchRecognizer(() => "direct");
    r.handle(ev("down", 1, 30, 30), 0);
    expect(r.handle(ev("timer", 1), 500)).toEqual([
      { kind: "click", button: "right", x: 30, y: 30 },
    ]);
    expect(r.handle(ev("move", 1, 40, 30), 600)).toEqual([{ kind: "pan", dx: 10, dy: 0 }]);
    expect(r.handle(ev("up", 1, 40, 30), 700)).toEqual([]);
  });

  it("direct: two-finger tap right-clicks at the centroid", () => {
    const r = new TouchRecognizer(() => "direct");
    r.handle(ev("down", 1, 100, 100), 0);
    r.handle(ev("down", 2, 200, 120), 10);
    expect(r.handle(ev("up", 1), 100)).toEqual([]);
    expect(r.handle(ev("up", 2), 120)).toEqual([
      { kind: "click", button: "right", x: 150, y: 110 },
    ]);
  });

  it("mode change applies at the next first down", () => {
    let mode: "trackpad" | "direct" = "trackpad";
    const r = new TouchRecognizer(() => mode);
    r.handle(ev("down", 1, 10, 10), 0);
    mode = "direct";
    expect(r.handle(ev("up", 1, 10, 10), 50)).toEqual(click("left"));
    r.handle(ev("down", 1, 10, 10), 100);
    expect(r.handle(ev("up", 1, 10, 10), 150)).toEqual([
      { kind: "click", button: "left", x: 10, y: 10 },
    ]);
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
