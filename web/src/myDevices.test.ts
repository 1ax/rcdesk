import { describe, expect, it } from "vitest";
import {
  canConnect,
  deviceLabel,
  deviceStatusLabel,
  formatLastSeen,
  loadOwnerToken,
  ruPlural,
  saveOwnerToken,
  sortDevices,
} from "./myDevices";
import type { DeviceEntry } from "./generated/DeviceEntry";

function device(overrides: Partial<DeviceEntry> = {}): DeviceEntry {
  return {
    device_id: "dev-1",
    name: "My Mac",
    alias: null,
    online: true,
    busy: false,
    last_seen_at: 1000n,
    ...overrides,
  };
}

describe("deviceLabel", () => {
  it("uses the alias when set", () => {
    expect(deviceLabel(device({ alias: "Work Mac" }))).toBe("Work Mac");
  });

  it("falls back to the name when alias is null", () => {
    expect(deviceLabel(device({ alias: null, name: "My Mac" }))).toBe("My Mac");
  });

  it("falls back to the name when alias is blank after trimming", () => {
    expect(deviceLabel(device({ alias: "   ", name: "My Mac" }))).toBe("My Mac");
  });
});

describe("canConnect", () => {
  it("is true when online and not busy", () => {
    expect(canConnect(device({ online: true, busy: false }))).toBe(true);
  });

  it("is false when offline", () => {
    expect(canConnect(device({ online: false, busy: false }))).toBe(false);
  });

  it("is false when busy", () => {
    expect(canConnect(device({ online: true, busy: true }))).toBe(false);
  });
});

describe("ruPlural", () => {
  it("picks the 'one' form for 1, 21, 101 (but not 11)", () => {
    expect(ruPlural(1, "минуту", "минуты", "минут")).toBe("минуту");
    expect(ruPlural(21, "минуту", "минуты", "минут")).toBe("минуту");
    expect(ruPlural(101, "минуту", "минуты", "минут")).toBe("минуту");
  });

  it("picks the 'few' form for 2-4, 22-24 (but not 12-14)", () => {
    expect(ruPlural(2, "минуту", "минуты", "минут")).toBe("минуты");
    expect(ruPlural(4, "минуту", "минуты", "минут")).toBe("минуты");
    expect(ruPlural(22, "минуту", "минуты", "минут")).toBe("минуты");
  });

  it("picks the 'many' form for 0, 5-20, 11-14, 25", () => {
    expect(ruPlural(0, "минуту", "минуты", "минут")).toBe("минут");
    expect(ruPlural(5, "минуту", "минуты", "минут")).toBe("минут");
    expect(ruPlural(11, "минуту", "минуты", "минут")).toBe("минут");
    expect(ruPlural(12, "минуту", "минуты", "минут")).toBe("минут");
    expect(ruPlural(14, "минуту", "минуты", "минут")).toBe("минут");
    expect(ruPlural(25, "минуту", "минуты", "минут")).toBe("минут");
  });
});

describe("formatLastSeen", () => {
  it("shows 'только что' under a minute", () => {
    expect(formatLastSeen(1000, 1030)).toBe("только что");
  });

  it("declines minutes correctly: 1 минуту, 2 минуты, 5 минут", () => {
    expect(formatLastSeen(1000, 1000 + 60)).toBe("1 минуту назад");
    expect(formatLastSeen(1000, 1000 + 2 * 60)).toBe("2 минуты назад");
    expect(formatLastSeen(1000, 1000 + 5 * 60)).toBe("5 минут назад");
  });

  it("declines hours correctly: 1 час, 2 часа, 5 часов", () => {
    expect(formatLastSeen(0, 3600)).toBe("1 час назад");
    expect(formatLastSeen(0, 2 * 3600)).toBe("2 часа назад");
    expect(formatLastSeen(0, 5 * 3600)).toBe("5 часов назад");
  });

  it("declines days correctly: 1 день, 2 дня, 5 дней", () => {
    expect(formatLastSeen(0, 86400)).toBe("1 день назад");
    expect(formatLastSeen(0, 2 * 86400)).toBe("2 дня назад");
    expect(formatLastSeen(0, 5 * 86400)).toBe("5 дней назад");
  });

  it("clamps a negative difference (clock skew) to 'только что'", () => {
    expect(formatLastSeen(2000, 1000)).toBe("только что");
  });
});

describe("deviceStatusLabel", () => {
  it("shows 'В сети' when online and not busy", () => {
    expect(deviceStatusLabel(device({ online: true, busy: false }), 2000)).toBe("В сети");
  });

  it("shows 'Занят' when online and busy", () => {
    expect(deviceStatusLabel(device({ online: true, busy: true }), 2000)).toBe("Занят");
  });

  it("shows 'Не в сети' with the last-seen time when offline", () => {
    const entry = device({ online: false, last_seen_at: 1000n });
    expect(deviceStatusLabel(entry, 1000 + 5 * 60)).toBe("Не в сети (5 минут назад)");
  });
});

describe("sortDevices", () => {
  it("orders connectable devices first, then online-but-busy, then offline, alphabetically within each group", () => {
    const offlineB = device({ device_id: "1", name: "Бета", online: false, busy: false });
    const offlineA = device({ device_id: "2", name: "Альфа", online: false, busy: false });
    const busyB = device({ device_id: "3", name: "Бета", online: true, busy: true });
    const busyA = device({ device_id: "4", name: "Альфа", online: true, busy: true });
    const freeB = device({ device_id: "5", name: "Бета", online: true, busy: false });
    const freeA = device({ device_id: "6", name: "Альфа", online: true, busy: false });

    const sorted = sortDevices([offlineB, offlineA, busyB, busyA, freeB, freeA]);

    expect(sorted.map((d) => d.device_id)).toEqual(["6", "5", "4", "3", "2", "1"]);
  });

  it("does not mutate the input array", () => {
    const devices = [
      device({ device_id: "1", name: "Бета" }),
      device({ device_id: "2", name: "Альфа" }),
    ];
    const original = [...devices];
    sortDevices(devices);
    expect(devices).toEqual(original);
  });

  it("sorts by alias when set, falling back to name", () => {
    const withAlias = device({ device_id: "1", name: "Zeta", alias: "Альфа" });
    const withoutAlias = device({ device_id: "2", name: "Бета", alias: null });
    const sorted = sortDevices([withoutAlias, withAlias]);
    expect(sorted.map((d) => d.device_id)).toEqual(["1", "2"]);
  });
});

describe("loadOwnerToken / saveOwnerToken", () => {
  it("round-trips a token through a storage-like object", () => {
    const store = new Map<string, string>();
    const storage = {
      getItem: (key: string) => store.get(key) ?? null,
      setItem: (key: string, value: string) => {
        store.set(key, value);
      },
    };
    saveOwnerToken(storage, "tok-123");
    expect(loadOwnerToken(storage)).toBe("tok-123");
  });

  it("returns null when there is no saved token", () => {
    const storage = { getItem: () => null };
    expect(loadOwnerToken(storage)).toBeNull();
  });

  it("loadOwnerToken returns null when storage throws", () => {
    const storage = {
      getItem: () => {
        throw new Error("SecurityError: private browsing");
      },
    };
    expect(loadOwnerToken(storage)).toBeNull();
  });

  it("saveOwnerToken silently does nothing when storage throws", () => {
    const storage = {
      setItem: () => {
        throw new Error("SecurityError: private browsing");
      },
    };
    expect(() => saveOwnerToken(storage, "tok-123")).not.toThrow();
  });
});
