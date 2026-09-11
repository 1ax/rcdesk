// Applies the host's system cursor *shape* to the local `<video>` element as
// a CSS `cursor: url(...)`, driven by `control` channel messages (see
// ARCHITECTURE.md §7 and §5, and `proto::control::ControlMessage` in
// `proto/src/control.rs`, the single source of truth for the wire shape).
//
// The browser's own OS cursor keeps *moving* with zero added latency --
// nothing here touches pointer position, only the cursor's *image*, which
// changes far less often (a click-and-hold, hovering a text field, ...).

import type { ControlMessage } from "./generated/ControlMessage";

/** Decodes a base64 string (as produced by the host, see
 * `ControlMessage.CursorShape`'s doc comment) into raw bytes. A pure
 * function so it's unit-testable under vitest's `node` environment (no
 * `document`/canvas needed) -- see `session.test.ts` for the same reasoning
 * applied elsewhere in this codebase. */
export function decodeRgba(base64: string): Uint8ClampedArray<ArrayBuffer> {
  const binary = atob(base64);
  const bytes = new Uint8ClampedArray(binary.length);
  for (let i = 0; i < binary.length; i++) {
    bytes[i] = binary.charCodeAt(i);
  }
  return bytes;
}

/** Builds the value for the CSS `cursor` property that shows `dataUrl` with
 * its hotspot at logical point `(hx, hy)` -- the same coordinate space
 * `ControlMessage.CursorShape.hotspot_x/y` uses, and (per the CSS Basic UI
 * spec) the space browsers interpret a cursor's hotspot offset in whether or
 * not `image-set()` is used, so no scale-dependent adjustment is needed
 * here. When `scale > 1` and the browser supports `image-set()`
 * (`supportsImageSet`), the image is shown at native (device-pixel)
 * resolution via `image-set(... ${scale}x)`; otherwise `dataUrl` is expected
 * to already be downscaled to its logical size (see `applyCursor`) and a
 * plain `url(...)` is used. Always falls back to `auto` if the image
 * ultimately can't be shown (unsupported format, cursor too large, ...). */
export function cursorCss(
  dataUrl: string,
  hx: number,
  hy: number,
  scale: number,
  supportsImageSet: boolean,
): string {
  const image =
    scale > 1 && supportsImageSet
      ? `image-set(url("${dataUrl}") ${scale}x)`
      : `url("${dataUrl}")`;
  return `${image} ${hx} ${hy}, auto`;
}

/** Whether this browser supports `cursor: image-set(...)` -- computed once
 * and cached, `CSS.supports` calls are cheap but there's no reason to repeat
 * one per cursor change. `undefined` until first checked (see
 * `supportsImageSet`), so a test environment without a global `CSS` object
 * doesn't crash at module load time. */
let supportsImageSetCache: boolean | undefined;

function supportsImageSet(): boolean {
  if (supportsImageSetCache === undefined) {
    supportsImageSetCache =
      typeof CSS !== "undefined" &&
      typeof CSS.supports === "function" &&
      CSS.supports("cursor", 'image-set(url("data:,") 1x)');
  }
  return supportsImageSetCache;
}

/** Renders `rgba` (as decoded by `decodeRgba`) at `width`x`height` onto a
 * canvas and returns a `data:image/png` URL for it. Prefers
 * `OffscreenCanvas` (works without inserting anything into the DOM);
 * `document.createElement("canvas")` is the fallback for browsers without
 * it. `targetWidth`/`targetHeight`, if given, downscale the drawn image
 * (used for the no-`image-set()` fallback, see `applyCursor`). */
async function renderToDataUrl(
  rgba: Uint8ClampedArray<ArrayBuffer>,
  width: number,
  height: number,
  targetWidth: number,
  targetHeight: number,
): Promise<string> {
  const source =
    typeof OffscreenCanvas !== "undefined"
      ? new OffscreenCanvas(width, height)
      : document.createElement("canvas");
  if (!(source instanceof OffscreenCanvas)) {
    source.width = width;
    source.height = height;
  }
  const sourceCtx = source.getContext("2d") as
    | OffscreenCanvasRenderingContext2D
    | CanvasRenderingContext2D
    | null;
  if (!sourceCtx) throw new Error("2d canvas context unavailable");
  sourceCtx.putImageData(new ImageData(rgba, width, height), 0, 0);

  let output: OffscreenCanvas | HTMLCanvasElement = source;
  if (targetWidth !== width || targetHeight !== height) {
    output =
      typeof OffscreenCanvas !== "undefined"
        ? new OffscreenCanvas(targetWidth, targetHeight)
        : document.createElement("canvas");
    if (!(output instanceof OffscreenCanvas)) {
      output.width = targetWidth;
      output.height = targetHeight;
    }
    const outCtx = output.getContext("2d") as
      | OffscreenCanvasRenderingContext2D
      | CanvasRenderingContext2D
      | null;
    if (!outCtx) throw new Error("2d canvas context unavailable");
    outCtx.drawImage(source as CanvasImageSource, 0, 0, targetWidth, targetHeight);
  }

  if (output instanceof OffscreenCanvas) {
    const blob = await output.convertToBlob({ type: "image/png" });
    return await new Promise<string>((resolve, reject) => {
      const reader = new FileReader();
      reader.onload = () => resolve(reader.result as string);
      reader.onerror = () => reject(reader.error ?? new Error("FileReader failed"));
      reader.readAsDataURL(blob);
    });
  }
  return output.toDataURL("image/png");
}

/** Cache of already-encoded cursor CSS values, keyed by the message's raw
 * `rgba` base64 string (itself a stable identity for one cursor image --
 * two different shapes producing the exact same bytes is indistinguishable
 * from, and as cheap to handle as, a cache hit). Bounded to avoid unbounded
 * growth over a long session with many distinct cursor shapes (text-editing
 * apps, custom app cursors, ...). */
const MAX_CACHE_ENTRIES = 32;
const cssCache = new Map<string, string>();

function cacheCss(key: string, css: string): void {
  if (cssCache.size >= MAX_CACHE_ENTRIES) {
    const oldest = cssCache.keys().next().value;
    if (oldest !== undefined) cssCache.delete(oldest);
  }
  cssCache.set(key, css);
}

/** Applies one `control`-channel `ControlMessage` to `video`'s CSS cursor.
 * `ping`/`pong` are not cursor messages and are ignored here (see `app.ts`,
 * which handles those for the `app N ms` overlay stat). Encoding a new
 * shape to a data URL is asynchronous (`OffscreenCanvas.convertToBlob`), so
 * this returns immediately and updates `video.style.cursor` once ready --
 * fine for a cursor shape, which humans can't perceive arriving a frame or
 * two late. */
export function applyCursor(video: HTMLElement, msg: ControlMessage): void {
  if (msg.type === "cursor_hidden") {
    video.style.cursor = "none";
    return;
  }
  if (msg.type !== "cursor_shape") return;

  const cached = cssCache.get(msg.rgba);
  if (cached) {
    video.style.cursor = cached;
    return;
  }

  const { width, height, hotspot_x, hotspot_y, scale, rgba } = msg;
  const useImageSet = scale > 1 && supportsImageSet();
  const targetWidth = useImageSet ? width : Math.max(1, Math.round(width / scale));
  const targetHeight = useImageSet ? height : Math.max(1, Math.round(height / scale));
  const cssScale = useImageSet ? scale : 1;

  renderToDataUrl(decodeRgba(rgba), width, height, targetWidth, targetHeight)
    .then((dataUrl) => {
      const css = cursorCss(dataUrl, hotspot_x, hotspot_y, cssScale, useImageSet);
      cacheCss(rgba, css);
      video.style.cursor = css;
    })
    .catch((err: unknown) => {
      console.error("failed to render cursor shape", err);
    });
}
