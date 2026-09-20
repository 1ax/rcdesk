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

  it("translates the device-list error codes added in slice 3.1e", () => {
    expect(signalErrorLabel("device offline")).toContain("не в сети");
    expect(signalErrorLabel("device not linked")).toContain("не привязано");
    expect(signalErrorLabel("not authenticated")).not.toBe("Ошибка: not authenticated");
    expect(signalErrorLabel("internal error")).not.toBe("Ошибка: internal error");
    expect(signalErrorLabel("replaced by a new connection")).not.toBe(
      "Ошибка: replaced by a new connection",
    );
  });
});
