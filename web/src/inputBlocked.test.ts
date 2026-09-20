import { describe, expect, it } from "vitest";
import { inputBlockedLabel } from "./inputBlocked";

describe("inputBlockedLabel", () => {
  it("returns null when not blocked", () => {
    expect(inputBlockedLabel({ blocked: false, reason: null })).toBeNull();
  });

  it("returns null when not blocked even if a stale reason is present", () => {
    expect(inputBlockedLabel({ blocked: false, reason: "stale" })).toBeNull();
  });

  it("returns a labeled reason when blocked with a reason", () => {
    expect(
      inputBlockedLabel({
        blocked: true,
        reason:
          "Foreground window runs with administrator rights; input is blocked by Windows (UIPI)",
      }),
    ).toBe(
      "Admin window: input is blocked by Windows (Foreground window runs with administrator rights; input is blocked by Windows (UIPI))",
    );
  });

  it("returns a bare label when blocked without a reason", () => {
    expect(inputBlockedLabel({ blocked: true, reason: null })).toBe(
      "Admin window: input is blocked by Windows",
    );
  });
});
