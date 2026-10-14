import { describe, expect, it } from "vitest";
import {
  authRequiredStorageKey,
  clearAuthRequired,
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
