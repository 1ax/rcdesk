import { describe, expect, it } from "vitest";
import {
  collapseButtonTitle,
  loadSessionBarCollapsed,
  loadStatsVisible,
  saveSessionBarCollapsed,
  saveStatsVisible,
  SESSION_BAR_COLLAPSED_STORAGE_KEY,
  STATS_VISIBLE_STORAGE_KEY,
} from "./sessionBar";

/** A minimal in-memory `Storage` stand-in, same pattern as
 * `keyRemap.test.ts`/`qualityPreset.test.ts`/`myDevices.test.ts`. */
function memoryStorage(initial: Record<string, string> = {}): Storage {
  const data = new Map(Object.entries(initial));
  return {
    getItem: (key: string) => data.get(key) ?? null,
    setItem: (key: string, value: string) => {
      data.set(key, value);
    },
    removeItem: (key: string) => {
      data.delete(key);
    },
    clear: () => data.clear(),
    key: () => null,
    get length() {
      return data.size;
    },
  };
}

function throwingStorage(): Storage {
  return {
    getItem: () => {
      throw new Error("blocked");
    },
    setItem: () => {
      throw new Error("blocked");
    },
    removeItem: () => {
      throw new Error("blocked");
    },
    clear: () => {
      throw new Error("blocked");
    },
    key: () => null,
    length: 0,
  };
}

describe("loadSessionBarCollapsed / loadStatsVisible", () => {
  it("default to false when nothing is stored", () => {
    expect(loadSessionBarCollapsed(memoryStorage())).toBe(false);
    expect(loadStatsVisible(memoryStorage())).toBe(false);
  });

  it("read back a saved '1' as true", () => {
    expect(
      loadSessionBarCollapsed(memoryStorage({ [SESSION_BAR_COLLAPSED_STORAGE_KEY]: "1" })),
    ).toBe(true);
    expect(loadStatsVisible(memoryStorage({ [STATS_VISIBLE_STORAGE_KEY]: "1" }))).toBe(true);
  });

  it("read back a saved '0' as false", () => {
    expect(
      loadSessionBarCollapsed(memoryStorage({ [SESSION_BAR_COLLAPSED_STORAGE_KEY]: "0" })),
    ).toBe(false);
    expect(loadStatsVisible(memoryStorage({ [STATS_VISIBLE_STORAGE_KEY]: "0" }))).toBe(false);
  });

  it("default to false when storage throws", () => {
    expect(loadSessionBarCollapsed(throwingStorage())).toBe(false);
    expect(loadStatsVisible(throwingStorage())).toBe(false);
  });
});

describe("saveSessionBarCollapsed", () => {
  it("persists the choice under the expected key", () => {
    const storage = memoryStorage();
    saveSessionBarCollapsed(storage, true);
    expect(storage.getItem(SESSION_BAR_COLLAPSED_STORAGE_KEY)).toBe("1");
    saveSessionBarCollapsed(storage, false);
    expect(storage.getItem(SESSION_BAR_COLLAPSED_STORAGE_KEY)).toBe("0");
  });

  it("does not throw when storage throws", () => {
    expect(() => saveSessionBarCollapsed(throwingStorage(), true)).not.toThrow();
  });
});

describe("saveStatsVisible", () => {
  it("persists the choice under the expected key", () => {
    const storage = memoryStorage();
    saveStatsVisible(storage, true);
    expect(storage.getItem(STATS_VISIBLE_STORAGE_KEY)).toBe("1");
    saveStatsVisible(storage, false);
    expect(storage.getItem(STATS_VISIBLE_STORAGE_KEY)).toBe("0");
  });

  it("does not throw when storage throws", () => {
    expect(() => saveStatsVisible(throwingStorage(), true)).not.toThrow();
  });
});

describe("collapseButtonTitle", () => {
  it("reads 'Показать панель' when collapsed", () => {
    expect(collapseButtonTitle(true)).toBe("Показать панель");
  });

  it("reads 'Свернуть панель' when expanded", () => {
    expect(collapseButtonTitle(false)).toBe("Свернуть панель");
  });
});
