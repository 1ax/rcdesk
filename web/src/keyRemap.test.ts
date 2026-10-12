import { describe, expect, it } from "vitest";
import {
  cmdAsCtrlApplies,
  keyRemapMode,
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

describe("keyRemapMode", () => {
  it("Mac client + windows host", () => {
    expect(keyRemapMode(true, "windows", true)).toBe("cmd-as-ctrl");
    expect(keyRemapMode(true, "windows", false)).toBe("none");
  });

  it("non-Mac client + macos host", () => {
    expect(keyRemapMode(false, "macos", true)).toBe("ctrl-as-cmd");
    expect(keyRemapMode(false, "macos", false)).toBe("none");
  });

  it("is none for same-OS family, other and unknown hosts", () => {
    expect(keyRemapMode(true, "macos", true)).toBe("none");
    expect(keyRemapMode(false, "windows", true)).toBe("none");
    expect(keyRemapMode(false, "other", true)).toBe("none");
    expect(keyRemapMode(true, null, true)).toBe("none");
    expect(keyRemapMode(false, null, true)).toBe("none");
  });
});

describe("remapCode", () => {
  it("cmd-as-ctrl maps Meta to Control, side preserved", () => {
    expect(remapCode("MetaLeft", "cmd-as-ctrl")).toBe("ControlLeft");
    expect(remapCode("MetaRight", "cmd-as-ctrl")).toBe("ControlRight");
  });

  it("cmd-as-ctrl does not touch Control or other codes", () => {
    expect(remapCode("ControlLeft", "cmd-as-ctrl")).toBe("ControlLeft");
    expect(remapCode("ControlRight", "cmd-as-ctrl")).toBe("ControlRight");
    expect(remapCode("KeyC", "cmd-as-ctrl")).toBe("KeyC");
  });

  it("ctrl-as-cmd swaps Control and Meta, side preserved", () => {
    expect(remapCode("ControlLeft", "ctrl-as-cmd")).toBe("MetaLeft");
    expect(remapCode("ControlRight", "ctrl-as-cmd")).toBe("MetaRight");
    expect(remapCode("MetaLeft", "ctrl-as-cmd")).toBe("ControlLeft");
    expect(remapCode("MetaRight", "ctrl-as-cmd")).toBe("ControlRight");
  });

  it("ctrl-as-cmd leaves other codes alone", () => {
    expect(remapCode("KeyC", "ctrl-as-cmd")).toBe("KeyC");
    expect(remapCode("ShiftLeft", "ctrl-as-cmd")).toBe("ShiftLeft");
  });

  it("none changes nothing", () => {
    for (const c of ["MetaLeft", "MetaRight", "ControlLeft", "ControlRight", "KeyC"]) {
      expect(remapCode(c, "none")).toBe(c);
    }
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
