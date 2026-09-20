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
          "Активное окно запущено с правами администратора: Windows блокирует ввод (UIPI)",
      }),
    ).toBe(
      "Окно администратора: Windows не пропускает ввод (Активное окно запущено с правами администратора: Windows блокирует ввод (UIPI))",
    );
  });

  it("returns a bare label when blocked without a reason", () => {
    expect(inputBlockedLabel({ blocked: true, reason: null })).toBe(
      "Окно администратора: Windows не пропускает ввод",
    );
  });
});
