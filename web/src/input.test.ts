import { describe, expect, it } from "vitest";
import { keyToMessage, mapPointer, normalizeWheel } from "./input";

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
