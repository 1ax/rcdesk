import { describe, expect, it, vi } from "vitest";
import {
  attachViewTouch,
  hostCursorPlacement,
  inputAttachAllowed,
  viewModeBadgeText,
  viewModeButtonView,
} from "./viewMode";

describe("viewModeButtonView", () => {
  it("is hidden unless the host reports availability", () => {
    expect(viewModeButtonView(false, undefined).hidden).toBe(true);
    expect(viewModeButtonView(true, false).hidden).toBe(true);
    expect(viewModeButtonView(false, true).hidden).toBe(false);
  });
  it("labels by the current mode", () => {
    expect(viewModeButtonView(true, true).text).toBe("Управлять");
    expect(viewModeButtonView(false, true).text).toBe("Только смотреть");
    expect(viewModeButtonView(true, true).title).not.toBe(viewModeButtonView(false, true).title);
  });
});

describe("viewModeBadgeText", () => {
  it("shows the badge only in view mode", () => {
    expect(viewModeBadgeText(true)).toBe("Режим просмотра");
    expect(viewModeBadgeText(false)).toBeNull();
  });
});

describe("inputAttachAllowed", () => {
  it("needs neither D26 block nor view mode", () => {
    expect(inputAttachAllowed({ inputBlocked: false, viewMode: false })).toBe(true);
    expect(inputAttachAllowed({ inputBlocked: true, viewMode: false })).toBe(false);
    expect(inputAttachAllowed({ inputBlocked: false, viewMode: true })).toBe(false);
    expect(inputAttachAllowed({ inputBlocked: true, viewMode: true })).toBe(false);
  });
});

describe("hostCursorPlacement", () => {
  const rect = { left: 0, top: 0, width: 1000, height: 500 };
  it("maps the frame centre with no letterbox", () => {
    expect(hostCursorPlacement({ x: 0.5, y: 0.5 }, rect, 2000, 1000)).toEqual({
      clientX: 500,
      clientY: 250,
    });
  });
  it("accounts for the pillarbox letterbox and the rect origin", () => {
    // 1000x500 box, 1:1 frame -> 500x500 centred, offsetX 250.
    const r = { left: 10, top: 20, width: 1000, height: 500 };
    expect(hostCursorPlacement({ x: 1, y: 0 }, r, 100, 100)).toEqual({
      clientX: 10 + 250 + 500,
      clientY: 20,
    });
  });
  it("accounts for the letterbox bars on top and bottom", () => {
    // 1000x1000 box, 2:1 frame -> 1000x500 with offsetY 250.
    expect(
      hostCursorPlacement({ x: 0, y: 1 }, { left: 0, top: 0, width: 1000, height: 1000 }, 200, 100),
    ).toEqual({ clientX: 0, clientY: 750 });
  });
  it("hides when out of [0,1], non-finite, null, or nothing rendered", () => {
    expect(hostCursorPlacement({ x: -0.01, y: 0.5 }, rect, 100, 100)).toBeNull();
    expect(hostCursorPlacement({ x: 0.5, y: 1.01 }, rect, 100, 100)).toBeNull();
    expect(hostCursorPlacement({ x: Number.NaN, y: 0.5 }, rect, 100, 100)).toBeNull();
    expect(hostCursorPlacement(null, rect, 100, 100)).toBeNull();
    expect(hostCursorPlacement({ x: 0.5, y: 0.5 }, rect, 0, 0)).toBeNull();
  });
});

describe("attachViewTouch", () => {
  function fakeVideo() {
    const handlers = new Map<string, (e: unknown) => void>();
    return {
      el: {
        addEventListener: (t: string, h: (e: unknown) => void) => handlers.set(t, h),
        removeEventListener: (t: string) => handlers.delete(t),
      } as unknown as HTMLElement,
      fire: (t: string, id: number, x: number, y: number, pointerType = "touch") =>
        handlers.get(t)?.({ pointerType, pointerId: id, clientX: x, clientY: y, preventDefault() {} }),
      count: () => handlers.size,
    };
  }
  it("pans with one finger and sends nothing else", () => {
    const v = fakeVideo();
    const onPan = vi.fn();
    const onPinch = vi.fn();
    const detach = attachViewTouch(v.el, { onPan, onPinch });
    v.fire("pointerdown", 1, 100, 100);
    v.fire("pointermove", 1, 140, 100);
    expect(onPan).toHaveBeenCalledWith(40, 0);
    expect(onPinch).not.toHaveBeenCalled();
    v.fire("pointerup", 1, 140, 100);
    detach();
    expect(v.count()).toBe(0);
  });
  it("pinches with two fingers", () => {
    const v = fakeVideo();
    const onPinch = vi.fn();
    const detach = attachViewTouch(v.el, { onPan: vi.fn(), onPinch });
    v.fire("pointerdown", 1, 100, 100);
    v.fire("pointerdown", 2, 200, 100);
    v.fire("pointermove", 2, 300, 100);
    expect(onPinch).toHaveBeenCalled();
    expect(onPinch.mock.calls[0]?.[0]).toBeGreaterThan(1);
    detach();
  });
  it("ignores mouse pointers", () => {
    const v = fakeVideo();
    const onPan = vi.fn();
    attachViewTouch(v.el, { onPan, onPinch: vi.fn() });
    v.fire("pointerdown", 1, 0, 0, "mouse");
    v.fire("pointermove", 1, 50, 0, "mouse");
    expect(onPan).not.toHaveBeenCalled();
  });
});
