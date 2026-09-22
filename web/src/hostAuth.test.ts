import { describe, expect, it } from "vitest";
import {
  authFailedPhase,
  canSubmit,
  lockedLabel,
  phaseLabel,
  wrongPasswordPhase,
} from "./hostAuth";
import type { AuthPhase } from "./hostAuth";

describe("authFailedPhase", () => {
  it("returns a retryable prompt when retry_after_secs is null", () => {
    expect(authFailedPhase(null, 1000)).toEqual({
      kind: "prompt",
      message: "Хост отверг попытку, попробуйте ещё раз",
    });
  });

  it("returns a locked phase until now + n seconds when retry_after_secs is set", () => {
    expect(authFailedPhase(30, 1_000_000)).toEqual({
      kind: "locked",
      untilMs: 1_000_000 + 30_000,
      message: "Слишком много попыток",
    });
  });

  it("locks even for retry_after_secs of 0", () => {
    expect(authFailedPhase(0, 1_000_000)).toEqual({
      kind: "locked",
      untilMs: 1_000_000,
      message: "Слишком много попыток",
    });
  });
});

describe("lockedLabel", () => {
  it("rounds up the remaining time", () => {
    expect(lockedLabel(10_500, 9_000)).toBe("Повторить через 2 с");
  });

  it("shows exact whole seconds without rounding up further", () => {
    expect(lockedLabel(11_000, 9_000)).toBe("Повторить через 2 с");
  });

  it("never goes below 1 second even once the deadline has passed", () => {
    expect(lockedLabel(9_000, 9_500)).toBe("Повторить через 1 с");
    expect(lockedLabel(9_000, 20_000)).toBe("Повторить через 1 с");
  });
});

describe("wrongPasswordPhase", () => {
  it("returns a prompt with the wrong-password message", () => {
    expect(wrongPasswordPhase()).toEqual({ kind: "prompt", message: "Неверный пароль" });
  });
});

describe("phaseLabel", () => {
  it("labels starting", () => {
    expect(phaseLabel({ kind: "starting" })).toBe("Подготовка…");
  });

  it("labels verifying", () => {
    expect(phaseLabel({ kind: "verifying" })).toBe("Проверка пароля…");
  });

  it("labels waiting-host", () => {
    expect(phaseLabel({ kind: "waiting-host" })).toBe("Ожидание хоста…");
  });

  it("labels prompt with its message", () => {
    expect(phaseLabel({ kind: "prompt", message: "Неверный пароль" })).toBe("Неверный пароль");
  });

  it("labels a bare prompt as an empty string", () => {
    expect(phaseLabel({ kind: "prompt", message: null })).toBe("");
  });

  it("labels locked via the countdown, using the given nowMs", () => {
    const phase: AuthPhase = { kind: "locked", untilMs: 10_000, message: "Слишком много попыток" };
    expect(phaseLabel(phase, 9_000)).toBe("Повторить через 1 с");
    expect(phaseLabel(phase, 5_000)).toBe("Повторить через 5 с");
  });
});

describe("canSubmit", () => {
  it("is true only for a prompt phase with a non-empty password", () => {
    expect(canSubmit({ kind: "prompt", message: null }, "secret")).toBe(true);
  });

  it("is false for an empty password", () => {
    expect(canSubmit({ kind: "prompt", message: null }, "")).toBe(false);
  });

  it("is false for every non-prompt phase, even with a password typed", () => {
    const phases: AuthPhase[] = [
      { kind: "starting" },
      { kind: "verifying" },
      { kind: "waiting-host" },
      { kind: "locked", untilMs: 10_000, message: "Слишком много попыток" },
    ];
    for (const phase of phases) {
      expect(canSubmit(phase, "secret")).toBe(false);
    }
  });
});
