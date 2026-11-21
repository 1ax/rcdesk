import { describe, expect, it } from "vitest";
import {
  authRequiredStorageKey,
  clearAuthRequired,
  forgetDistrustedHost,
  loadAuthRequired,
  offerRejectedByTrust,
  saveAuthRequired,
} from "./hostTrust";

function memoryStorage(): Storage {
  const m = new Map<string, string>();
  return {
    getItem: (k: string) => m.get(k) ?? null,
    setItem: (k: string, v: string) => void m.set(k, v),
    removeItem: (k: string) => void m.delete(k),
  } as unknown as Storage;
}

const throwing = {
  getItem: () => {
    throw new Error("blocked");
  },
  setItem: () => {
    throw new Error("blocked");
  },
  removeItem: () => {
    throw new Error("blocked");
  },
};

describe("authRequiredStorageKey", () => {
  it("scopes the key to the device id", () => {
    expect(authRequiredStorageKey("dev-1")).toBe("rcdesk.authRequired.dev-1");
  });
});

describe("load/save/clear AuthRequired", () => {
  it("round-trips per device", () => {
    const s = memoryStorage();
    expect(loadAuthRequired(s, "a")).toBe(false);
    saveAuthRequired(s, "a");
    expect(loadAuthRequired(s, "a")).toBe(true);
    expect(loadAuthRequired(s, "b")).toBe(false);
    clearAuthRequired(s, "a");
    expect(loadAuthRequired(s, "a")).toBe(false);
  });

  it("treats any value other than 1 as false", () => {
    const s = memoryStorage();
    s.setItem(authRequiredStorageKey("a"), "true");
    expect(loadAuthRequired(s, "a")).toBe(false);
  });

  it("does not throw when storage throws", () => {
    expect(loadAuthRequired(throwing, "a")).toBe(false);
    expect(() => saveAuthRequired(throwing, "a")).not.toThrow();
    expect(() => clearAuthRequired(throwing, "a")).not.toThrow();
  });
});

describe("offerRejectedByTrust", () => {
  it("rejects only when remembered and no session key", () => {
    expect(offerRejectedByTrust(true, false)).toBe(true);
    expect(offerRejectedByTrust(true, true)).toBe(false);
    expect(offerRejectedByTrust(false, false)).toBe(false);
    expect(offerRejectedByTrust(false, true)).toBe(false);
  });
});

describe("forgetDistrustedHost", () => {
  it("sends forget_device then list_devices and clears the flag", () => {
    const storage = memoryStorage();
    saveAuthRequired(storage, "dev1");
    const sent: unknown[] = [];
    forgetDistrustedHost((m) => sent.push(m), storage, "dev1");
    expect(sent).toEqual([
      { type: "forget_device", device_id: "dev1" },
      { type: "list_devices" },
    ]);
    expect(loadAuthRequired(storage, "dev1")).toBe(false);
  });

  it("survives a throwing storage and a null storage", () => {
    const sent: unknown[] = [];
    const throwing = {
      removeItem() {
        throw new Error("blocked");
      },
    };
    expect(() => forgetDistrustedHost((m) => sent.push(m), throwing, "d")).not.toThrow();
    expect(() => forgetDistrustedHost((m) => sent.push(m), null, "d")).not.toThrow();
    expect(sent).toHaveLength(4);
  });
});
