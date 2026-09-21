import { describe, expect, it } from "vitest";
import {
  loadQualityPreset,
  parseQualityPreset,
  QUALITY_PRESET_LABELS,
  qualityPresetStorageKey,
  saveQualityPreset,
} from "./qualityPreset";

describe("QUALITY_PRESET_LABELS", () => {
  it("has a Russian label for every wire value", () => {
    expect(QUALITY_PRESET_LABELS).toEqual({
      auto: "Авто",
      sharp: "Чёткость",
      smooth: "Плавность",
    });
  });
});

describe("qualityPresetStorageKey", () => {
  it("scopes the key to the device id", () => {
    expect(qualityPresetStorageKey("dev-1")).toBe("rcdesk.quality-preset.dev-1");
    expect(qualityPresetStorageKey("dev-2")).toBe("rcdesk.quality-preset.dev-2");
  });
});

describe("parseQualityPreset", () => {
  it("recognizes sharp and smooth", () => {
    expect(parseQualityPreset("sharp")).toBe("sharp");
    expect(parseQualityPreset("smooth")).toBe("smooth");
  });

  it("falls back to auto for auto itself, null, and anything unrecognized", () => {
    expect(parseQualityPreset("auto")).toBe("auto");
    expect(parseQualityPreset(null)).toBe("auto");
    expect(parseQualityPreset("")).toBe("auto");
    expect(parseQualityPreset("blazing-fast")).toBe("auto");
  });
});

describe("loadQualityPreset / saveQualityPreset", () => {
  function mapStorage(): Storage {
    const store = new Map<string, string>();
    return {
      getItem: (key: string) => store.get(key) ?? null,
      setItem: (key: string, value: string) => {
        store.set(key, value);
      },
    } as Storage;
  }

  it("round-trips a preset through a storage-like object, scoped by device id", () => {
    const storage = mapStorage();
    saveQualityPreset(storage, "dev-1", "sharp");
    expect(loadQualityPreset(storage, "dev-1")).toBe("sharp");
  });

  it("keeps different devices' presets separate", () => {
    const storage = mapStorage();
    saveQualityPreset(storage, "dev-1", "sharp");
    saveQualityPreset(storage, "dev-2", "smooth");
    expect(loadQualityPreset(storage, "dev-1")).toBe("sharp");
    expect(loadQualityPreset(storage, "dev-2")).toBe("smooth");
  });

  it("returns auto when there is nothing saved for that device", () => {
    const storage = mapStorage();
    expect(loadQualityPreset(storage, "dev-1")).toBe("auto");
  });

  it("loadQualityPreset returns auto without touching storage when deviceId is null", () => {
    const storage = {
      getItem: () => {
        throw new Error("must not be called");
      },
    };
    expect(loadQualityPreset(storage, null)).toBe("auto");
  });

  it("saveQualityPreset does nothing when deviceId is null", () => {
    const storage = {
      setItem: () => {
        throw new Error("must not be called");
      },
    };
    expect(() => saveQualityPreset(storage, null, "sharp")).not.toThrow();
  });

  it("loadQualityPreset returns auto when storage throws", () => {
    const storage = {
      getItem: () => {
        throw new Error("SecurityError: private browsing");
      },
    };
    expect(loadQualityPreset(storage, "dev-1")).toBe("auto");
  });

  it("saveQualityPreset silently does nothing when storage throws", () => {
    const storage = {
      setItem: () => {
        throw new Error("SecurityError: private browsing");
      },
    };
    expect(() => saveQualityPreset(storage, "dev-1", "smooth")).not.toThrow();
  });
});
