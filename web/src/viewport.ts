// Pinch-zoom / pan state of the video layer on the mobile client (slice H3).
// Pure logic, no DOM: the layer sits in a container of `w x h` CSS px and is
// shown with `transform: translate(tx px, ty px) scale(scale)` and
// `transform-origin: 0 0`.

import { frameLayout } from "./input";

export interface Zoom {
  scale: number;
  tx: number;
  ty: number;
}

export const ZOOM_MIN = 1;
export const ZOOM_MAX = 8;

export function identityZoom(): Zoom {
  return { scale: 1, tx: 0, ty: 0 };
}

/** Zoom the phone keyboard brings in (slice H5-2). */
export const KEYBOARD_ZOOM = 2;

/** Keeps `scale` in `[ZOOM_MIN, ZOOM_MAX]` and the container fully covered. */
export function clampZoom(z: Zoom, w: number, h: number): Zoom {
  const scale = Math.min(ZOOM_MAX, Math.max(ZOOM_MIN, z.scale));
  const tx = Math.min(0, Math.max(w - w * scale, z.tx));
  const ty = Math.min(0, Math.max(h - h * scale, z.ty));
  return { scale, tx, ty };
}

/** Zooms by `factor` keeping the container point `(px, py)` in place. */
export function zoomAt(
  z: Zoom,
  factor: number,
  px: number,
  py: number,
  w: number,
  h: number,
): Zoom {
  const scale = Math.min(ZOOM_MAX, Math.max(ZOOM_MIN, z.scale * factor));
  const ratio = scale / z.scale;
  return clampZoom(
    { scale, tx: px - (px - z.tx) * ratio, ty: py - (py - z.ty) * ratio },
    w,
    h,
  );
}

export function panBy(z: Zoom, dx: number, dy: number, w: number, h: number): Zoom {
  return clampZoom({ scale: z.scale, tx: z.tx + dx, ty: z.ty + dy }, w, h);
}

/** Shifts the view so the container point `(px, py)` stays at least
 * `margin` px away from the edges. */
export function followPoint(
  z: Zoom,
  px: number,
  py: number,
  w: number,
  h: number,
  margin: number,
): Zoom {
  let { tx, ty } = z;
  if (px < margin) tx += margin - px;
  else if (px > w - margin) tx -= px - (w - margin);
  if (py < margin) ty += margin - py;
  else if (py > h - margin) ty -= py - (h - margin);
  return clampZoom({ scale: z.scale, tx, ty }, w, h);
}

/** Value for `style.transform`; `""` for the identity. */
export function zoomCss(z: Zoom): string {
  if (z.scale === 1 && z.tx === 0 && z.ty === 0) return "";
  return `translate(${z.tx}px, ${z.ty}px) scale(${z.scale})`;
}

export const TOUCH_MODE_STORAGE_KEY = "rcdesk.touch-mode";

export type TouchMode = "trackpad" | "direct";

export function loadTouchMode(storage: Pick<Storage, "getItem"> | null): TouchMode {
  try {
    return storage?.getItem(TOUCH_MODE_STORAGE_KEY) === "direct" ? "direct" : "trackpad";
  } catch {
    return "trackpad";
  }
}

export function saveTouchMode(storage: Pick<Storage, "setItem"> | null, mode: TouchMode): void {
  try {
    storage?.setItem(TOUCH_MODE_STORAGE_KEY, mode);
  } catch {
    // Storage may be unavailable or full; the mode just won't persist.
  }
}

export function touchModeLabel(mode: TouchMode): string {
  return mode === "direct" ? "Касание" : "Трекпад";
}

/** Zoom to `scale` with the unscaled-layer point `(lx, ly)` at the centre of a
 * `w`x`h` container (clamped so the layer still covers it). */
export function centerOn(scale: number, lx: number, ly: number, w: number, h: number): Zoom {
  const s = Math.min(ZOOM_MAX, Math.max(ZOOM_MIN, scale));
  return clampZoom({ scale: s, tx: w / 2 - lx * s, ty: h / 2 - ly * s }, w, h);
}

/** Normalized frame point shown at the centre of the container (H5-18). */
export interface ViewAnchor {
  x: number;
  y: number;
}

/** Frame point at the container centre for `z`; `null` when no frame geometry.
 * Not clipped to [0,1]: the centre may sit in a letterbox bar, and restoring
 * must be exact. */
export function anchorOf(
  z: Zoom,
  w: number,
  h: number,
  videoWidth: number,
  videoHeight: number,
): ViewAnchor | null {
  const f = frameLayout({ left: 0, top: 0, width: w, height: h }, videoWidth, videoHeight);
  if (!f) return null;
  const lx = (w / 2 - z.tx) / z.scale;
  const ly = (h / 2 - z.ty) / z.scale;
  return { x: (lx - f.offsetX) / f.displayWidth, y: (ly - f.offsetY) / f.displayHeight };
}

/** Zoom to `scale` with the frame point `anchor` at the container centre (clamped). */
export function zoomAtAnchor(
  scale: number,
  anchor: ViewAnchor,
  w: number,
  h: number,
  videoWidth: number,
  videoHeight: number,
): Zoom {
  const f = frameLayout({ left: 0, top: 0, width: w, height: h }, videoWidth, videoHeight);
  if (!f) return centerOn(scale, w / 2, h / 2, w, h);
  return centerOn(
    scale,
    f.offsetX + anchor.x * f.displayWidth,
    f.offsetY + anchor.y * f.displayHeight,
    w,
    h,
  );
}
