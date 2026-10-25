import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import {
  KeyRepeat,
  SENTINEL,
  StickyModifiers,
  composeTyped,
  diffTyped,
  hostSupportsText,
  isPasteCombo,
  graphemes,
  keystrokes,
  plainPunctuation,
} from "./softKeyboard";

describe("H5-14", () => {
  it("diffTyped deletes one grapheme per phone Backspace", () => {
    expect(diffTyped(" a❤️", " a")).toEqual({ backspaces: 1, inserted: "" });
    expect(diffTyped(" 👍🏽x", " ")).toEqual({ backspaces: 2, inserted: "" });
    expect(diffTyped(" e\u0301", " ")).toEqual({ backspaces: 1, inserted: "" });
  });
  it("graphemes falls back sanely", () => {
    expect(graphemes("ab")).toEqual(["a", "b"]);
  });
  it("plainPunctuation", () => {
    expect(plainPunctuation("‘a’ “b” — c…")).toBe("'a' \"b\" -- c...");
    expect(plainPunctuation("plain 'x' \"y\"")).toBe("plain 'x' \"y\"");
  });
  it("composeTyped sends plain punctuation as text", () => {
    expect(composeTyped("’", 0, [], true)).toEqual([{ type: "text", text: "'" }]);
  });
});

const k = (code: string, pressed: boolean) => ({ type: "key", code, pressed });

describe("diffTyped", () => {
  it("дописали символ", () => {
    expect(diffTyped(" a", " ab")).toEqual({ backspaces: 0, inserted: "b" });
  });
  it("удалили один", () => {
    expect(diffTyped(" ab", " a")).toEqual({ backspaces: 1, inserted: "" });
  });
  it("автозамена слова", () => {
    expect(diffTyped(" helo", " hello ")).toEqual({ backspaces: 1, inserted: "lo " });
  });
  it("удалён страж", () => {
    expect(diffTyped(SENTINEL, "")).toEqual({ backspaces: 1, inserted: "" });
  });
  it("эмодзи удаляется как один", () => {
    expect(diffTyped(" a😀", " a")).toEqual({ backspaces: 1, inserted: "" });
  });
});

describe("StickyModifiers", () => {
  it("цикл off -> once -> locked -> off", () => {
    const s = new StickyModifiers();
    expect(s.state("ControlLeft")).toBe("off");
    s.tap("ControlLeft");
    expect(s.state("ControlLeft")).toBe("once");
    s.tap("ControlLeft");
    expect(s.state("ControlLeft")).toBe("locked");
    s.tap("ControlLeft");
    expect(s.state("ControlLeft")).toBe("off");
  });
  it("consume гасит once, оставляет locked", () => {
    const s = new StickyModifiers();
    s.tap("ControlLeft");
    s.tap("AltLeft");
    s.tap("AltLeft");
    expect(s.consume().sort()).toEqual(["AltLeft", "ControlLeft"]);
    expect(s.state("ControlLeft")).toBe("off");
    expect(s.state("AltLeft")).toBe("locked");
    s.clear();
    expect(s.consume()).toEqual([]);
  });
});

describe("hostSupportsText", () => {
  it("версии", () => {
    expect(hostSupportsText("0.1.7")).toBe(false);
    expect(hostSupportsText("0.1.8")).toBe(true);
    expect(hostSupportsText("0.2.0")).toBe(true);
    expect(hostSupportsText("1.0.0")).toBe(true);
    expect(hostSupportsText(null)).toBe(false);
    expect(hostSupportsText(undefined)).toBe(false);
    expect(hostSupportsText("abc")).toBe(false);
  });
});

describe("keystrokes", () => {
  it("с модификатором", () => {
    expect(keystrokes("KeyC", false, ["ControlLeft", "AltLeft"])).toEqual([
      k("ControlLeft", true),
      k("AltLeft", true),
      k("KeyC", true),
      k("KeyC", false),
      k("AltLeft", false),
      k("ControlLeft", false),
    ]);
  });
  it("shift не дублируется, если уже в mods", () => {
    expect(keystrokes("KeyA", true, ["ShiftLeft"])).toEqual([
      k("ShiftLeft", true),
      k("KeyA", true),
      k("KeyA", false),
      k("ShiftLeft", false),
    ]);
  });
});

describe("composeTyped", () => {
  it("новый хост без модификаторов -> text", () => {
    expect(composeTyped("привет", 0, [], true)).toEqual([{ type: "text", text: "привет" }]);
  });
  it("Ctrl + c", () => {
    const want = [k("ControlLeft", true), k("KeyC", true), k("KeyC", false), k("ControlLeft", false)];
    expect(composeTyped("c", 0, ["ControlLeft"], true)).toEqual(want);
    expect(composeTyped("с", 0, ["ControlLeft"], true)).toEqual(want);
  });
  it("старый хост", () => {
    expect(composeTyped("ab", 0, [], false)).toEqual([
      k("KeyA", true),
      k("KeyA", false),
      k("KeyB", true),
      k("KeyB", false),
    ]);
    expect(composeTyped("A", 0, [], false)).toEqual([
      k("ShiftLeft", true),
      k("KeyA", true),
      k("KeyA", false),
      k("ShiftLeft", false),
    ]);
  });
  it("backspaces первыми", () => {
    const r = composeTyped("x", 2, [], true);
    expect(r).toEqual([
      k("Backspace", true),
      k("Backspace", false),
      k("Backspace", true),
      k("Backspace", false),
      { type: "text", text: "x" },
    ]);
  });
});

describe("KeyRepeat", () => {
  beforeEach(() => {
    vi.useFakeTimers();
  });
  afterEach(() => {
    vi.useRealTimers();
  });

  it("does not fire before the delay", () => {
    const fn = vi.fn();
    const r = new KeyRepeat();
    r.start(fn);
    vi.advanceTimersByTime(499);
    expect(fn).not.toHaveBeenCalled();
    expect(r.stop()).toBe(false);
  });

  it("fires after the delay and then every interval", () => {
    const fn = vi.fn();
    const r = new KeyRepeat();
    r.start(fn);
    vi.advanceTimersByTime(500);
    expect(fn).toHaveBeenCalledTimes(1);
    vi.advanceTimersByTime(100);
    expect(fn).toHaveBeenCalledTimes(3);
    expect(r.stop()).toBe(true);
    vi.advanceTimersByTime(200);
    expect(fn).toHaveBeenCalledTimes(3);
  });

  it("start restarts the delay", () => {
    const fn = vi.fn();
    const r = new KeyRepeat();
    r.start(fn);
    vi.advanceTimersByTime(400);
    r.start(fn);
    vi.advanceTimersByTime(400);
    expect(fn).not.toHaveBeenCalled();
    vi.advanceTimersByTime(100);
    expect(fn).toHaveBeenCalledTimes(1);
  });
});

describe("isPasteCombo", () => {
  it("true for Cmd/Ctrl + v in both layouts", () => {
    expect(isPasteCombo("v", ["MetaLeft"])).toBe(true);
    expect(isPasteCombo("V", ["ControlLeft"])).toBe(true);
    expect(isPasteCombo("м", ["MetaLeft"])).toBe(true);
  });

  it("false without a modifier or for other keys", () => {
    expect(isPasteCombo("v", [])).toBe(false);
    expect(isPasteCombo("c", ["MetaLeft"])).toBe(false);
    expect(isPasteCombo("v", ["ShiftLeft"])).toBe(false);
  });

  it("false for multi-character input", () => {
    expect(isPasteCombo("vv", ["MetaLeft"])).toBe(false);
  });
});
