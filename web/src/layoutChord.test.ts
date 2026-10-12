import { describe, expect, it } from "vitest";
import { createLayoutChord } from "./layoutChord";

/** Feeds ["d:Code" | "u:Code"] steps, returns how many times the chord fired. */
function run(steps: string[], chord = createLayoutChord()): number {
  let n = 0;
  for (const s of steps) {
    const code = s.slice(2);
    if (s.startsWith("d:") ? chord.keyDown(code) : chord.keyUp(code)) n++;
  }
  return n;
}

describe("layoutChord", () => {
  it("AltL -> ShiftL -> release fires once", () => {
    expect(run(["d:AltLeft", "d:ShiftLeft", "u:ShiftLeft", "u:AltLeft"])).toBe(1);
  });
  it("ShiftL -> AltL -> release fires once", () => {
    expect(run(["d:ShiftLeft", "d:AltLeft", "u:AltLeft", "u:ShiftLeft"])).toBe(1);
  });
  it("right-hand keys fire once", () => {
    expect(run(["d:AltRight", "d:ShiftRight", "u:AltRight", "u:ShiftRight"])).toBe(1);
  });
  it("fires on release of the first key, not on press", () => {
    const c = createLayoutChord();
    expect(c.keyDown("AltLeft")).toBe(false);
    expect(c.keyDown("ShiftLeft")).toBe(false);
    expect(c.keyUp("AltLeft")).toBe(true);
    expect(c.keyUp("ShiftLeft")).toBe(false);
  });
  it("another key cancels until full release", () => {
    expect(
      run(["d:AltLeft", "d:ShiftLeft", "d:KeyA", "u:KeyA", "u:ShiftLeft", "u:AltLeft"]),
    ).toBe(0);
  });
  it("Alt+Shift+Tab-like key pressed first also cancels", () => {
    expect(run(["d:Tab", "d:AltLeft", "d:ShiftLeft", "u:ShiftLeft", "u:AltLeft", "u:Tab"])).toBe(0);
  });
  it("only Alt does not fire", () => {
    expect(run(["d:AltLeft", "u:AltLeft"])).toBe(0);
  });
  it("only Shift does not fire", () => {
    expect(run(["d:ShiftLeft", "u:ShiftLeft"])).toBe(0);
  });
  it("two chords in a row fire twice", () => {
    const one = ["d:AltLeft", "d:ShiftLeft", "u:ShiftLeft", "u:AltLeft"];
    expect(run([...one, ...one])).toBe(2);
  });
  it("key repeat of Alt does not break the chord", () => {
    expect(
      run(["d:AltLeft", "d:AltLeft", "d:ShiftLeft", "d:AltLeft", "u:ShiftLeft", "u:AltLeft"]),
    ).toBe(1);
  });
  it("cancelled press does not poison the next one", () => {
    expect(
      run([
        "d:AltLeft", "d:ShiftLeft", "d:KeyA", "u:KeyA", "u:ShiftLeft", "u:AltLeft",
        "d:AltLeft", "d:ShiftLeft", "u:ShiftLeft", "u:AltLeft",
      ]),
    ).toBe(1);
  });
  it("reset mid-chord yields no firing", () => {
    const c = createLayoutChord();
    c.keyDown("AltLeft");
    c.keyDown("ShiftLeft");
    c.reset();
    expect(c.keyUp("ShiftLeft")).toBe(false);
    expect(c.keyUp("AltLeft")).toBe(false);
  });
});
