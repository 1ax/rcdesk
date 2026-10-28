import { describe, expect, it } from "vitest";
import type { DeviceEntry } from "./generated/DeviceEntry";
import {
  WAKE_TIMEOUT_MS,
  applyDevices,
  applyWakeError,
  applyWakeSent,
  createWakeState,
  formatElapsed,
  startWake,
  wakeErrorLabel,
  wakeStatusFor,
  wakeView,
} from "./wake";

function entry(over: Partial<DeviceEntry> = {}): DeviceEntry {
  return { device_id: "d1", name: "PC", online: false, can_wake: true, ...over } as DeviceEntry;
}

describe("formatElapsed", () => {
  it("formats m:ss", () => {
    expect(formatElapsed(0)).toBe("0:00");
    expect(formatElapsed(45_000)).toBe("0:45");
    expect(formatElapsed(125_000)).toBe("2:05");
    expect(formatElapsed(-5)).toBe("0:00");
  });
});

describe("wakeView", () => {
  it("online: no button, usual status", () => {
    const v = wakeView(entry({ online: true }), { startedMs: 0 }, 1000);
    expect(v.showButton).toBe(false);
    expect(v.status).toBeNull();
  });

  it("waiting: disabled button with elapsed time", () => {
    const v = wakeView(entry(), { startedMs: 1000 }, 46_000);
    expect(v.showButton).toBe(true);
    expect(v.buttonDisabled).toBe(true);
    expect(v.buttonLabel).toBe("Включаю…");
    expect(v.status).toContain("0:45");
    expect(v.status).toContain("обычно 1–2 мин");
  });

  it("timed out: enabled button and hint", () => {
    const v = wakeView(entry(), { startedMs: 0 }, WAKE_TIMEOUT_MS);
    expect(v.showButton).toBe(true);
    expect(v.buttonDisabled).toBe(false);
    expect(v.buttonLabel).toBe("Включить");
    expect(v.status).toContain("Не включился за 3 мин");
  });

  it("timed out without can_wake: no button", () => {
    const v = wakeView(entry({ can_wake: false }), { startedMs: 0 }, WAKE_TIMEOUT_MS);
    expect(v.showButton).toBe(false);
  });

  it("idle offline with can_wake: button", () => {
    const v = wakeView(entry(), undefined, 0);
    expect(v).toMatchObject({ showButton: true, buttonDisabled: false, buttonLabel: "Включить" });
    expect(v.status).toBeNull();
  });

  it("idle offline without can_wake: no button", () => {
    expect(wakeView(entry({ can_wake: false }), undefined, 0).showButton).toBe(false);
  });
});

describe("wakeErrorLabel", () => {
  it("maps known codes", () => {
    expect(wakeErrorLabel("no waker online")).toContain("некому включить");
    expect(wakeErrorLabel("too many attempts")).toContain("Подождите");
    expect(wakeErrorLabel("device not linked")).toBe("Устройство не привязано");
    expect(wakeErrorLabel("device online")).toBeNull();
    expect(wakeErrorLabel("whatever")).toBeNull();
  });
});

describe("wake state flow (device list)", () => {
  it("click -> waiting status, then online clears it", () => {
    const st = createWakeState();
    const off = entry();
    expect(wakeStatusFor(st, off, 0).view.showButton).toBe(true);
    startWake(st, "d1", 0);
    expect(st.pendingId).toBe("d1");
    const r = wakeStatusFor(st, off, 5000);
    expect(r.view.buttonDisabled).toBe(true);
    expect(r.status).toContain("Включаю…");
    applyWakeSent(st);
    expect(st.pendingId).toBeNull();
    expect(st.waiting.has("d1")).toBe(true);
    applyDevices(st, [entry({ online: true })]);
    expect(st.waiting.size).toBe(0);
    expect(wakeStatusFor(st, entry({ online: true }), 6000).view.showButton).toBe(false);
  });

  it("no waker online: error shown in status, waiting dropped", () => {
    const st = createWakeState();
    startWake(st, "d1", 0);
    expect(applyWakeError(st, "no waker online")).toBe(true);
    expect(st.waiting.size).toBe(0);
    expect(st.pendingId).toBeNull();
    expect(wakeStatusFor(st, entry(), 1000).status).toContain("некому включить");
    // next click clears the error
    startWake(st, "d1", 2000);
    expect(st.errors.size).toBe(0);
  });

  it("device online error is silent but consumed", () => {
    const st = createWakeState();
    startWake(st, "d1", 0);
    expect(applyWakeError(st, "device online")).toBe(true);
    expect(wakeStatusFor(st, entry(), 1).status).toBeNull();
  });

  it("errors are not intercepted without a pending wake or for foreign messages", () => {
    const st = createWakeState();
    expect(applyWakeError(st, "device not linked")).toBe(false);
    startWake(st, "d1", 0);
    expect(applyWakeError(st, "unknown pin")).toBe(false);
    expect(st.pendingId).toBe("d1");
    applyWakeSent(st);
    expect(applyWakeError(st, "too many attempts")).toBe(false);
  });

  it("device without can_wake has no button", () => {
    expect(wakeStatusFor(createWakeState(), entry({ can_wake: false }), 0).view.showButton).toBe(
      false,
    );
  });
});
