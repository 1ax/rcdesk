import { describe, expect, it } from "vitest";
import { formatBuildBadge, isStaleSessionError, signalErrorLabel } from "./app";

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

describe("too many attempts / stale session errors", () => {
  it("labels the server's PIN rate-limit code", () => {
    expect(signalErrorLabel("too many attempts")).toBe(
      "Слишком много неверных PIN — подождите минуту и попробуйте снова",
    );
  });

  it("treats 'not in session' without a session as stale (D38)", () => {
    expect(isStaleSessionError("not in session", null)).toBe(true);
    expect(isStaleSessionError("not in session", "abc")).toBe(false);
    expect(isStaleSessionError("unknown pin", null)).toBe(false);
  });
});
