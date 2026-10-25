import { describe, expect, it } from "vitest";
import {
  TOUCH_MODE_STORAGE_KEY,
  centerOn,
  clampZoom,
  followPoint,
  identityZoom,
  loadTouchMode,
  panBy,
  saveTouchMode,
  touchModeLabel,
  zoomAt,
  zoomCss,
} from "./viewport";

describe("zoom", () => {
  it("zoomAt keeps the anchor point in place", () => {
    const z = zoomAt(identityZoom(), 2, 100, 50, 400, 300);
    expect(z).toEqual({ scale: 2, tx: -100, ty: -50 });
    expect(z.tx + 100 * 2).toBe(100);
  });

  it("caps at 8x and returns to identity below 1x", () => {
    expect(zoomAt(identityZoom(), 10, 0, 0, 400, 300).scale).toBe(8);
    const z = zoomAt(zoomAt(identityZoom(), 2, 100, 50, 400, 300), 0.1, 100, 50, 400, 300);
    expect(z).toEqual(identityZoom());
  });

  it("panBy stops at the edges", () => {
    const z = { scale: 2, tx: -100, ty: -50 };
    expect(panBy(z, 500, 500, 400, 300)).toEqual({ scale: 2, tx: 0, ty: 0 });
    expect(panBy(z, -900, -900, 400, 300)).toEqual({ scale: 2, tx: -400, ty: -300 });
    expect(panBy(z, -10, 5, 400, 300)).toEqual({ scale: 2, tx: -110, ty: -45 });
  });

  it("clampZoom covers the container", () => {
    expect(clampZoom({ scale: 2, tx: 50, ty: -999 }, 400, 300)).toEqual({
      scale: 2,
      tx: 0,
      ty: -300,
    });
  });

  it("followPoint moves only when the point is within the margin", () => {
    const z = { scale: 2, tx: -200, ty: -100 };
    expect(followPoint(z, 200, 150, 400, 300, 24)).toEqual(z);
    expect(followPoint(z, 10, 150, 400, 300, 24)).toEqual({ scale: 2, tx: -186, ty: -100 });
    expect(followPoint(z, 390, 290, 400, 300, 24)).toEqual({ scale: 2, tx: -214, ty: -114 });
    expect(followPoint(identityZoom(), 0, 0, 400, 300, 24)).toEqual(identityZoom());
  });

  it("centerOn puts the anchor at the centre", () => {
    expect(centerOn(2, 100, 75, 400, 300)).toEqual({ scale: 2, tx: 0, ty: 0 });
  });

  it("centerOn clamps at the edges", () => {
    expect(centerOn(2, 10, 10, 400, 300)).toEqual({ scale: 2, tx: 0, ty: 0 });
    expect(centerOn(2, 390, 290, 400, 300)).toEqual({ scale: 2, tx: -400, ty: -300 });
  });

  it("centerOn clamps the scale", () => {
    expect(centerOn(16, 200, 150, 400, 300).scale).toBe(8);
    expect(centerOn(0.5, 100, 75, 400, 300)).toEqual({ scale: 1, tx: 0, ty: 0 });
  });

  it("zoomCss is empty for identity", () => {
    expect(zoomCss(identityZoom())).toBe("");
    expect(zoomCss({ scale: 2, tx: -100, ty: -50 })).toBe(
      "translate(-100px, -50px) scale(2)",
    );
  });
});

describe("touch mode storage", () => {
  it("defaults to trackpad and round-trips", () => {
    const m = new Map<string, string>();
    const storage = {
      getItem: (k: string) => m.get(k) ?? null,
      setItem: (k: string, v: string) => void m.set(k, v),
    };
    expect(loadTouchMode(storage)).toBe("trackpad");
    saveTouchMode(storage, "direct");
    expect(m.get(TOUCH_MODE_STORAGE_KEY)).toBe("direct");
    expect(loadTouchMode(storage)).toBe("direct");
    expect(loadTouchMode(null)).toBe("trackpad");
  });

  it("swallows storage exceptions", () => {
    const bad = {
      getItem: () => {
        throw new Error("x");
      },
      setItem: () => {
        throw new Error("x");
      },
    };
    expect(loadTouchMode(bad)).toBe("trackpad");
    expect(() => saveTouchMode(bad, "direct")).not.toThrow();
  });

  it("labels", () => {
    expect(touchModeLabel("trackpad")).toBe("Трекпад");
    expect(touchModeLabel("direct")).toBe("Касание");
  });
});
