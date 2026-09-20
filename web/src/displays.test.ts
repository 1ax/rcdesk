import { describe, expect, it } from "vitest";
import { displayOptions, parseDisplayId, shouldShowPicker } from "./displays";
import type { DisplayEntry } from "./generated/DisplayEntry";

function display(overrides: Partial<DisplayEntry> = {}): DisplayEntry {
  return {
    id: 1,
    title: "Built-in Display",
    width: 1920,
    height: 1080,
    primary: true,
    ...overrides,
  };
}

describe("displayOptions", () => {
  it("labels each option 1-based with title and size, and selects the current display", () => {
    const displays = [
      display({ id: 1, title: "Built-in Display", width: 1920, height: 1080, primary: true }),
      display({ id: 2, title: "External", width: 2560, height: 1440, primary: false }),
    ];

    const options = displayOptions(displays, 2);

    expect(options).toEqual([
      { value: "1", label: "1: Built-in Display 1920×1080 (основной)", selected: false },
      { value: "2", label: "2: External 2560×1440", selected: true },
    ]);
  });

  it("appends the primary suffix only for the primary display", () => {
    const displays = [display({ id: 5, title: "Studio", primary: true })];
    const options = displayOptions(displays, 5);
    expect(options[0].label).toBe("1: Studio 1920×1080 (основной)");
  });

  it("returns an empty array for an empty display list", () => {
    expect(displayOptions([], 0)).toEqual([]);
  });

  it("preserves the order displays arrived in", () => {
    const displays = [
      display({ id: 9, title: "Nine" }),
      display({ id: 3, title: "Three" }),
    ];
    const options = displayOptions(displays, 9);
    expect(options.map((o) => o.value)).toEqual(["9", "3"]);
  });
});

describe("shouldShowPicker", () => {
  it("is false for an empty display list", () => {
    expect(shouldShowPicker([])).toBe(false);
  });

  it("is false for a single display", () => {
    expect(shouldShowPicker([display()])).toBe(false);
  });

  it("is true for two or more displays", () => {
    expect(shouldShowPicker([display({ id: 1 }), display({ id: 2 })])).toBe(true);
  });
});

describe("parseDisplayId", () => {
  it("parses a valid non-negative integer string", () => {
    expect(parseDisplayId("2")).toBe(2);
  });

  it("returns null for a non-numeric string", () => {
    expect(parseDisplayId("x")).toBeNull();
  });

  it("returns null for a negative number", () => {
    expect(parseDisplayId("-1")).toBeNull();
  });
});
