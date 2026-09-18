import { describe, expect, it } from "vitest";
import { KeyMessageGate, keyToMessage, mapPointer, normalizeWheel } from "./input";
import type { InputMessage } from "./generated/InputMessage";

describe("mapPointer", () => {
  // A 16:9 frame (1600x900) inside a 4:3 element (800x600): the frame fills
  // the element's width and is letterboxed top/bottom (height 450 instead
  // of 600, centered -> a 75px bar above and below).
  const rect = { left: 0, top: 0, width: 800, height: 600 };
  const videoWidth = 1600;
  const videoHeight = 900;

  it("maps the center of the element to the center of the frame", () => {
    expect(mapPointer(400, 300, rect, videoWidth, videoHeight)).toEqual({ x: 0.5, y: 0.5 });
  });

  it("returns null for a point inside the letterbox bar", () => {
    expect(mapPointer(400, 10, rect, videoWidth, videoHeight)).toBeNull();
  });

  it("maps the top-left corner of the frame to (0,0)", () => {
    expect(mapPointer(0, 75, rect, videoWidth, videoHeight)).toEqual({ x: 0, y: 0 });
  });

  it("returns null when the video has no intrinsic size yet", () => {
    expect(mapPointer(400, 300, rect, 0, 0)).toBeNull();
  });
});

describe("normalizeWheel", () => {
  it("converts pixel-mode deltas to lines, rounded to 0.1", () => {
    expect(normalizeWheel(0, 53, 0)).toEqual({ dx: 0, dy: 1.3 });
  });

  it("passes line-mode deltas through unchanged", () => {
    expect(normalizeWheel(0, 3, 1)).toEqual({ dx: 0, dy: 3 });
  });

  it("multiplies page-mode deltas by 3", () => {
    expect(normalizeWheel(0, 2, 2)).toEqual({ dx: 0, dy: 6 });
  });
});

describe("keyToMessage", () => {
  it("returns null for an auto-repeat keydown", () => {
    expect(keyToMessage("KeyA", true, true)).toBeNull();
  });

  it("builds a Key message for a non-repeat event", () => {
    expect(keyToMessage("KeyA", true, false)).toEqual({
      type: "key",
      code: "KeyA",
      pressed: true,
    });
  });

  it("builds a Key message for a key-up event", () => {
    expect(keyToMessage("ShiftLeft", false, false)).toEqual({
      type: "key",
      code: "ShiftLeft",
      pressed: false,
    });
  });
});

describe("KeyMessageGate", () => {
  const key = (code: string): InputMessage => ({ type: "key", code, pressed: true });

  it("forwards messages straight to the sink while closed", () => {
    const gate = new KeyMessageGate();
    const sent: InputMessage[] = [];

    gate.send(key("KeyA"), (m) => sent.push(m));

    expect(sent).toEqual([key("KeyA")]);
  });

  it("queues messages while open and flushes them in order on release", () => {
    const gate = new KeyMessageGate();
    const sent: InputMessage[] = [];

    gate.open();
    gate.send(key("KeyV"), (m) => sent.push(m));
    gate.send(key("KeyB"), (m) => sent.push(m));
    gate.send({ type: "release_all" }, (m) => sent.push(m));
    expect(sent).toEqual([]); // nothing sent yet -- still queued

    gate.release((m) => sent.push(m));

    expect(sent).toEqual([key("KeyV"), key("KeyB"), { type: "release_all" }]);
  });

  it("sends immediately again after release closes the gate", () => {
    const gate = new KeyMessageGate();
    const sent: InputMessage[] = [];

    gate.open();
    gate.send(key("KeyV"), (m) => sent.push(m));
    gate.release((m) => sent.push(m));

    gate.send(key("KeyC"), (m) => sent.push(m));

    expect(sent).toEqual([key("KeyV"), key("KeyC")]);
  });

  it("still flushes the queue on release even after a rejected wait (caller's responsibility to call release)", async () => {
    const gate = new KeyMessageGate();
    const sent: InputMessage[] = [];

    gate.open();
    gate.send(key("KeyV"), (m) => sent.push(m));

    // Simulates `attachInput`'s `wait.catch(...).then(() => gate.release(...))`
    // -- release still runs, and in the same order, whether the awaited
    // promise resolved or rejected.
    await Promise.reject(new Error("boom"))
      .catch(() => {})
      .then(() => gate.release((m) => sent.push(m)));

    expect(sent).toEqual([key("KeyV")]);
  });
});
