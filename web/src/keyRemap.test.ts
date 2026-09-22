import { describe, expect, it } from "vitest";
import {
  cmdAsCtrlApplies,
  CMD_AS_CTRL_STORAGE_KEY,
  isMacClient,
  loadCmdAsCtrlSetting,
  remapCode,
  saveCmdAsCtrlSetting,
} from "./keyRemap";

describe("isMacClient", () => {
  it("uses userAgentData.platform when present", () => {
    expect(isMacClient({ userAgentData: { platform: "macOS" } })).toBe(true);
    expect(isMacClient({ userAgentData: { platform: "Windows" } })).toBe(false);
  });

  it("falls back to navigator.platform when userAgentData is absent", () => {
    expect(isMacClient({ platform: "MacIntel" })).toBe(true);
    expect(isMacClient({ platform: "Win32" })).toBe(false);
  });

  it("falls back to userAgent when neither platform field is available", () => {
    expect(
      isMacClient({
        userAgent: "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/605.1.15",
      }),
    ).toBe(true);
    expect(
      isMacClient({
        userAgent: "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36",
      }),
    ).toBe(false);
  });

  it("prefers userAgentData.platform over a stale/empty navigator.platform", () => {
    expect(isMacClient({ userAgentData: { platform: "" }, platform: "MacIntel" })).toBe(true);
  });

  it("returns false when nothing is available", () => {
    expect(isMacClient({})).toBe(false);
  });
});

describe("cmdAsCtrlApplies", () => {
  it("applies only for a Mac client talking to a Windows host", () => {
    expect(cmdAsCtrlApplies(true, "windows")).toBe(true);
  });

  it("does not apply for a non-Mac client", () => {
    expect(cmdAsCtrlApplies(false, "windows")).toBe(false);
  });

  it("does not apply for a Mac host", () => {
    expect(cmdAsCtrlApplies(true, "macos")).toBe(false);
  });

  it("does not apply for an unknown ('other') host", () => {
    expect(cmdAsCtrlApplies(true, "other")).toBe(false);
  });

  it("does not apply before the host's OS is known", () => {
    expect(cmdAsCtrlApplies(true, null)).toBe(false);
  });
});

describe("remapCode", () => {
  it("remaps MetaLeft/MetaRight to the matching Ctrl code when enabled", () => {
    expect(remapCode("MetaLeft", true)).toBe("ControlLeft");
    expect(remapCode("MetaRight", true)).toBe("ControlRight");
  });

  it("leaves MetaLeft/MetaRight alone when disabled", () => {
    expect(remapCode("MetaLeft", false)).toBe("MetaLeft");
    expect(remapCode("MetaRight", false)).toBe("MetaRight");
  });

  it("leaves every other code unchanged regardless of the flag", () => {
    expect(remapCode("KeyC", true)).toBe("KeyC");
    expect(remapCode("ControlLeft", true)).toBe("ControlLeft");
    expect(remapCode("ShiftLeft", false)).toBe("ShiftLeft");
  });
});

/** A minimal in-memory `Storage` stand-in, same pattern as
 * `qualityPreset.test.ts`/`myDevices.test.ts`. */
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

describe("loadCmdAsCtrlSetting", () => {
  it("defaults to true (on) when nothing is stored", () => {
    expect(loadCmdAsCtrlSetting(memoryStorage())).toBe(true);
  });

  it("reads back a saved 'off' choice", () => {
    expect(loadCmdAsCtrlSetting(memoryStorage({ [CMD_AS_CTRL_STORAGE_KEY]: "0" }))).toBe(false);
  });

  it("reads back a saved 'on' choice", () => {
    expect(loadCmdAsCtrlSetting(memoryStorage({ [CMD_AS_CTRL_STORAGE_KEY]: "1" }))).toBe(true);
  });

  it("defaults to true when storage throws", () => {
    expect(loadCmdAsCtrlSetting(throwingStorage())).toBe(true);
  });
});

describe("saveCmdAsCtrlSetting", () => {
  it("persists the choice under the expected key", () => {
    const storage = memoryStorage();
    saveCmdAsCtrlSetting(storage, false);
    expect(storage.getItem(CMD_AS_CTRL_STORAGE_KEY)).toBe("0");
    saveCmdAsCtrlSetting(storage, true);
    expect(storage.getItem(CMD_AS_CTRL_STORAGE_KEY)).toBe("1");
  });

  it("does not throw when storage throws", () => {
    expect(() => saveCmdAsCtrlSetting(throwingStorage(), true)).not.toThrow();
  });
});
