import { describe, expect, it } from "vitest";
import { addDeviceErrorLabel, findNetwork, networkLabel, networkValue } from "./wakeDevices";

const net = { network: "192.168.88.0", prefix: 24, label: "Mac, PC" };

describe("addDeviceErrorLabel", () => {
  it("maps known errors", () => {
    expect(addDeviceErrorLabel("invalid name")).toBe("Укажите имя");
    expect(addDeviceErrorLabel("invalid mac")).toBe("MAC в виде aa:bb:cc:dd:ee:ff");
    expect(addDeviceErrorLabel("unknown network")).toBe("Сеть не найдена — обновите страницу");
    expect(addDeviceErrorLabel("duplicate mac")).toBe("Устройство с таким MAC уже есть");
    expect(addDeviceErrorLabel("too many devices")).toBe("Достигнут предел — 32 устройства");
  });

  it("returns null for foreign errors", () => {
    expect(addDeviceErrorLabel("no waker online")).toBeNull();
    expect(addDeviceErrorLabel("device not wakeable")).toBeNull();
  });
});

describe("network helpers", () => {
  it("labels and values", () => {
    expect(networkLabel(net)).toBe("192.168.88.0/24 — Mac, PC");
    expect(networkValue(net)).toBe("192.168.88.0/24");
  });

  it("finds by value", () => {
    expect(findNetwork([net], "192.168.88.0/24")).toBe(net);
    expect(findNetwork([net], "10.0.0.0/8")).toBeUndefined();
  });
});
