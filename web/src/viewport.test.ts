import { describe, expect, it } from "vitest";
import {
  TOUCH_MODE_STORAGE_KEY,
  anchorOf,
  centerOn,
  clampZoom,
  followPoint,
  identityZoom,
  loadTouchMode,
  panBy,
  saveTouchMode,
  touchModeLabel,
  zoomAt,
  zoomAtAnchor,
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

  it("clampZoom keeps the shown frame, not the letterbox, in cover (D79)", () => {
    // Container 400x800, video 1600x900: frame 400x225, offsetY 287.5.
    expect(clampZoom({ scale: 2, tx: -999, ty: -999 }, 400, 800, 1600, 900)).toEqual({
      scale: 2,
      tx: -400,
      ty: -400,
    });
    expect(clampZoom({ scale: 2, tx: 50, ty: -999 }, 400, 800, 1600, 900)).toEqual({
      scale: 2,
      tx: 0,
      ty: -400,
    });
    expect(clampZoom({ scale: 4, tx: 0, ty: 0 }, 400, 800, 1600, 900).ty).toBe(-1150);
    expect(clampZoom({ scale: 4, tx: 0, ty: -9999 }, 400, 800, 1600, 900).ty).toBe(-1250);
    expect(clampZoom({ scale: 1, tx: 123, ty: -77 }, 400, 800, 1600, 900)).toEqual({
      scale: 1,
      tx: 0,
      ty: 0,
    });
  });

  it("clampZoom without video size keeps the old behaviour (D79)", () => {
    const old = { scale: 2, tx: 0, ty: -300 };
    expect(clampZoom({ scale: 2, tx: 50, ty: -999 }, 400, 300)).toEqual(old);
    expect(clampZoom({ scale: 2, tx: 50, ty: -999 }, 400, 300, 0, 0)).toEqual(old);
    expect(clampZoom({ scale: 2, tx: 50, ty: -999 }, 400, 300, 0, 900)).toEqual(old);
  });

  it("clampZoom centres a pillarboxed frame on x (D79)", () => {
    // Container 800x400, video 900x1600: frame 225x400, offsetX 287.5.
    expect(clampZoom({ scale: 2, tx: 0, ty: -999 }, 800, 400, 900, 1600)).toEqual({
      scale: 2,
      tx: -400,
      ty: -400,
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

describe("view anchor", () => {
  const VW = 1920;
  const VH = 1080;
  const W = 402;
  const dispH = (W * VH) / VW; // 226.125
  const lyOf = (h: number, y: number) => (h - dispH) / 2 + y * dispH;

  it("identity zoom anchors at the frame centre", () => {
    const a = anchorOf(identityZoom(), W, 566, VW, VH);
    expect(a?.x).toBeCloseTo(0.5);
    expect(a?.y).toBeCloseTo(0.5);
  });

  it("keeps the centre frame point when the height changes", () => {
    const z = centerOn(2, 0.4 * W, lyOf(566, 0.35), W, 566);
    const a = anchorOf(z, W, 566, VW, VH);
    expect(a?.x).toBeCloseTo(0.4);
    expect(a?.y).toBeCloseTo(0.35);
    const z2 = zoomAtAnchor(2, a!, W, 367, VW, VH);
    const a2 = anchorOf(z2, W, 367, VW, VH);
    expect(a2?.x).toBeCloseTo(0.4);
    expect(a2?.y).toBeCloseTo(0.35);
  });

  it("scale 1 at the centre anchor is identity", () => {
    const z = zoomAtAnchor(1, { x: 0.5, y: 0.5 }, W, 566, VW, VH);
    expect(z.scale).toBe(1);
    expect(z.tx).toBeCloseTo(0);
    expect(z.ty).toBeCloseTo(0);
  });

  it("without video: null anchor and centred zoom", () => {
    const z = centerOn(2, 201, 283, W, 566);
    expect(anchorOf(z, W, 566, 0, 0)).toBeNull();
    expect(zoomAtAnchor(2, { x: 0.4, y: 0.35 }, W, 566, 0, 0)).toEqual(z);
  });
});
