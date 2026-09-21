import { describe, expect, it } from "vitest";
import { reconnectBackoffMs } from "./reconnectBackoff";

describe("reconnectBackoffMs", () => {
  it("doubles from 1s on successive attempts", () => {
    expect(reconnectBackoffMs(1)).toBe(1000);
    expect(reconnectBackoffMs(2)).toBe(2000);
    expect(reconnectBackoffMs(3)).toBe(4000);
    expect(reconnectBackoffMs(4)).toBe(8000);
    expect(reconnectBackoffMs(5)).toBe(16000);
  });

  it("caps at 30s and stays there", () => {
    expect(reconnectBackoffMs(6)).toBe(30000);
    expect(reconnectBackoffMs(7)).toBe(30000);
    expect(reconnectBackoffMs(100)).toBe(30000);
  });

  it("treats an attempt below 1 as attempt 1", () => {
    expect(reconnectBackoffMs(0)).toBe(1000);
    expect(reconnectBackoffMs(-5)).toBe(1000);
  });
});
