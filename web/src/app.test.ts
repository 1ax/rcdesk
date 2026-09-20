import { describe, expect, it } from "vitest";
import { formatBuildBadge, signalErrorLabel } from "./app";

describe("formatBuildBadge", () => {
  it("formats a short commit sha", () => {
    expect(formatBuildBadge("abc1234")).toBe("build abc1234");
  });

  it("formats the local dev placeholder", () => {
    expect(formatBuildBadge("dev")).toBe("build dev");
  });
});

describe("signalErrorLabel", () => {
  it("translates the signaling server's known error codes", () => {
    expect(signalErrorLabel("unknown pin")).toContain("PIN");
    expect(signalErrorLabel("unknown pin")).not.toContain("unknown");
    expect(signalErrorLabel("host busy")).toContain("другой клиент");
  });

  it("passes an unknown code through instead of swallowing it", () => {
    expect(signalErrorLabel("something new")).toBe("Ошибка: something new");
  });
});
