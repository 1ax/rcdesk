import { describe, expect, it } from "vitest";
import { charToKey } from "./charKeys";

describe("charToKey", () => {
  it("латиница и цифры", () => {
    expect(charToKey("a")).toEqual({ code: "KeyA", shift: false });
    expect(charToKey("A")).toEqual({ code: "KeyA", shift: true });
    expect(charToKey("1")).toEqual({ code: "Digit1", shift: false });
  });
  it("знаки", () => {
    expect(charToKey("!")).toEqual({ code: "Digit1", shift: true });
    expect(charToKey(" ")).toEqual({ code: "Space", shift: false });
    expect(charToKey(".")).toEqual({ code: "Period", shift: false });
    expect(charToKey("?")).toEqual({ code: "Slash", shift: true });
  });
  it("кириллица", () => {
    expect(charToKey("й")).toEqual({ code: "KeyQ", shift: false });
    expect(charToKey("Ж")).toEqual({ code: "Semicolon", shift: true });
    expect(charToKey("ё")).toEqual({ code: "Backquote", shift: false });
    expect(charToKey("с")).toEqual({ code: "KeyC", shift: false });
  });
  it("неизвестное", () => {
    expect(charToKey("€")).toBeNull();
    expect(charToKey("")).toBeNull();
  });
});
