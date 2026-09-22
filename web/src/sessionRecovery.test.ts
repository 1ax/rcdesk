import { describe, expect, it } from "vitest";
import {
  byeOutcome,
  connectionBannerLabel,
  connectionStateOutcome,
  MAX_RECOVERY_ATTEMPTS,
  recoveryDelayMs,
  recoveryExhausted,
  shouldAttemptRecovery,
} from "./sessionRecovery";

describe("connectionStateOutcome", () => {
  it("is connected regardless of whether a grace window was running", () => {
    expect(connectionStateOutcome("connected", false)).toBe("connected");
    expect(connectionStateOutcome("connected", true)).toBe("connected");
  });

  it("starts a disconnect grace window on a fresh disconnected", () => {
    expect(connectionStateOutcome("disconnected", false)).toBe("start-disconnect-grace");
  });

  it("does not restart the window on a repeated disconnected", () => {
    expect(connectionStateOutcome("disconnected", true)).toBe("already-disconnecting");
  });

  it("treats failed and closed as the session being lost regardless of the window", () => {
    expect(connectionStateOutcome("failed", false)).toBe("session-lost");
    expect(connectionStateOutcome("failed", true)).toBe("session-lost");
    expect(connectionStateOutcome("closed", false)).toBe("session-lost");
    expect(connectionStateOutcome("closed", true)).toBe("session-lost");
  });

  it("ignores transient states", () => {
    expect(connectionStateOutcome("new", false)).toBe("ignore");
    expect(connectionStateOutcome("connecting", false)).toBe("ignore");
  });
});

describe("recoveryDelayMs", () => {
  it("follows the 1, 2, 4, 8, 16s schedule", () => {
    expect(recoveryDelayMs(1)).toBe(1000);
    expect(recoveryDelayMs(2)).toBe(2000);
    expect(recoveryDelayMs(3)).toBe(4000);
    expect(recoveryDelayMs(4)).toBe(8000);
    expect(recoveryDelayMs(5)).toBe(16000);
  });

  it("clamps below 1 to attempt 1", () => {
    expect(recoveryDelayMs(0)).toBe(1000);
    expect(recoveryDelayMs(-3)).toBe(1000);
  });

  it("clamps past the last attempt to the last delay", () => {
    expect(recoveryDelayMs(6)).toBe(16000);
    expect(recoveryDelayMs(100)).toBe(16000);
  });
});

describe("recoveryExhausted", () => {
  it("is false for every attempt up to and including the last one", () => {
    for (let attempt = 1; attempt <= MAX_RECOVERY_ATTEMPTS; attempt++) {
      expect(recoveryExhausted(attempt)).toBe(false);
    }
  });

  it("is true once past the last attempt", () => {
    expect(recoveryExhausted(MAX_RECOVERY_ATTEMPTS + 1)).toBe(true);
    expect(recoveryExhausted(100)).toBe(true);
  });
});

describe("shouldAttemptRecovery", () => {
  it("is true with a known device id", () => {
    expect(shouldAttemptRecovery("dev123")).toBe(true);
  });

  it("is false without one (unknown or a pre-3.5b server)", () => {
    expect(shouldAttemptRecovery(null)).toBe(false);
  });
});

describe("byeOutcome", () => {
  it("is session-lost when a disconnect grace window is already running", () => {
    expect(byeOutcome(true)).toBe("session-lost");
  });

  it("is teardown when no grace window is running", () => {
    expect(byeOutcome(false)).toBe("teardown");
  });
});

describe("connectionBannerLabel", () => {
  it("shows the connecting label before the first frame", () => {
    expect(connectionBannerLabel({ kind: "connecting" })).toBe("Подключение к хосту…");
  });

  it("shows the disconnecting label during the grace window", () => {
    expect(connectionBannerLabel({ kind: "disconnecting" })).toBe("Связь прерывается…");
  });

  it("shows the attempt count while reconnecting", () => {
    expect(connectionBannerLabel({ kind: "reconnecting", attempt: 1 })).toBe(
      "Переподключение… (1 из 5)",
    );
    expect(connectionBannerLabel({ kind: "reconnecting", attempt: 5 })).toBe(
      "Переподключение… (5 из 5)",
    );
  });

  it("is hidden (null) once streaming", () => {
    expect(connectionBannerLabel({ kind: "streaming" })).toBeNull();
  });
});
