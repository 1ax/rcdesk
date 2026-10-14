import { describe, expect, it, vi } from "vitest";
import { ScreenWakeLock } from "./wakeLock";
import type { WakeLockSentinelLike } from "./wakeLock";

function makeNav() {
  const sentinels: { release: ReturnType<typeof vi.fn>; fire: () => void }[] = [];
  const request = vi.fn(async (_t: "screen") => {
    let listener: () => void = () => {};
    const release = vi.fn(async () => {});
    sentinels.push({ release, fire: () => listener() });
    const s: WakeLockSentinelLike = {
      release,
      addEventListener: (_type, l) => {
        listener = l;
      },
    };
    return s;
  });
  return { nav: { wakeLock: { request } }, request, sentinels };
}

const flush = () => new Promise((r) => setTimeout(r, 0));

describe("ScreenWakeLock", () => {
  it("enable requests a screen lock", async () => {
    const { nav, request } = makeNav();
    new ScreenWakeLock(nav, () => true).enable();
    await flush();
    expect(request).toHaveBeenCalledWith("screen");
  });

  it("does nothing without wakeLock support", () => {
    const wl = new ScreenWakeLock({}, () => true);
    expect(() => {
      wl.enable();
      wl.onVisibilityChange();
      wl.disable();
    }).not.toThrow();
  });

  it("disable releases the sentinel", async () => {
    const { nav, sentinels } = makeNav();
    const wl = new ScreenWakeLock(nav, () => true);
    wl.enable();
    await flush();
    wl.disable();
    expect(sentinels[0]!.release).toHaveBeenCalledTimes(1);
  });

  it("second enable with a live sentinel does not request again", async () => {
    const { nav, request } = makeNav();
    const wl = new ScreenWakeLock(nav, () => true);
    wl.enable();
    await flush();
    wl.enable();
    await flush();
    expect(request).toHaveBeenCalledTimes(1);
  });

  it("re-acquires on visibility change after a release event", async () => {
    const { nav, request, sentinels } = makeNav();
    const wl = new ScreenWakeLock(nav, () => true);
    wl.enable();
    await flush();
    sentinels[0]!.fire();
    wl.onVisibilityChange();
    await flush();
    expect(request).toHaveBeenCalledTimes(2);
  });

  it("does not acquire while hidden or when not wanted", async () => {
    const { nav, request } = makeNav();
    let visible = false;
    const wl = new ScreenWakeLock(nav, () => visible);
    wl.enable();
    visible = true;
    wl.disable();
    wl.onVisibilityChange();
    await flush();
    expect(request).not.toHaveBeenCalled();
  });

  it("a rejected request does not surface", async () => {
    const debug = vi.spyOn(console, "debug").mockImplementation(() => {});
    const nav = { wakeLock: { request: vi.fn(() => Promise.reject(new Error("denied"))) } };
    const wl = new ScreenWakeLock(nav, () => true);
    expect(() => wl.enable()).not.toThrow();
    await flush();
    expect(debug).toHaveBeenCalled();
    debug.mockRestore();
  });
});
