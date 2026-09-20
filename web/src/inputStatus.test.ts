import { describe, expect, it } from "vitest";
import { viewOnlyLabel } from "./inputStatus";

describe("viewOnlyLabel", () => {
  it("returns null when input is available", () => {
    expect(viewOnlyLabel({ available: true, reason: null })).toBeNull();
  });

  it("returns a labeled reason when input is unavailable with a reason", () => {
    expect(
      viewOnlyLabel({ available: false, reason: "нет разрешения «Универсальный доступ»" }),
    ).toBe("Только просмотр: нет разрешения «Универсальный доступ»");
  });

  it("returns a bare label when input is unavailable without a reason", () => {
    expect(viewOnlyLabel({ available: false, reason: null })).toBe("Только просмотр");
  });
});
