/**
 * The session's view state (slices 3.5g, H3, H5): pinch-zoom of the video
 * layer, the virtual touch-cursor overlay, the session bar (collapse and the
 * temporary collapse for the phone keyboard) and the fullscreen hint note. A
 * feature factory split out of `app.ts`'s `mount()`: it finds its own DOM in
 * `root`, keeps its state in the closure and talks to the rest of `mount()`
 * only through `SessionViewDeps` and the returned `SessionView`.
 */
import { unmapPointer } from "./input";
import { hostCursorPlacement } from "./viewMode";
import {
  KEYBOARD_ZOOM,
  anchorOf,
  centerOn,
  clampZoom,
  followPoint,
  identityZoom,
  panBy,
  zoomAt,
  zoomAtAnchor,
  zoomCss,
  type TouchMode,
  type ViewAnchor,
  type Zoom,
} from "./viewport";
import { collapseButtonTitle, loadSessionBarCollapsed, saveSessionBarCollapsed } from "./sessionBar";

/** How long `#fullscreen-note` (the "for exit press/hold Esc" hint) stays
 * visible after entering fullscreen (slice 3.5c). */
const FULLSCREEN_NOTE_MS = 3000;

export interface SessionViewDeps {
  /** The `mount()` root; the view looks its own elements up here. */
  root: Element;
  /** The `#video` element (frame size for the zoom maths). */
  video: HTMLVideoElement;
  /** The owner storage as it was at mount (initial bar-collapsed setting). */
  initialStorage: Storage | null;
  /** The owner storage right now (persisting the bar-collapsed setting). */
  getStorage: () => Storage | null;
  /** The current touch mode (trackpad mode makes the zoom follow the cursor). */
  getTouchMode: () => TouchMode;
  /** There is a live session (a keyboard hide after teardown restores nothing). */
  isSessionActive: () => boolean;
  /** Gives focus back to the video unless the phone keyboard owns it. */
  focusVideoUnlessTyping: () => void;
}

export interface SessionView {
  /** The `#touch-cursor` overlay (`applyCursor` sets its image). */
  touchCursorEl: HTMLImageElement;
  /** The `#host-cursor` overlay (view-only mode: the host's own cursor). */
  hostCursorEl: HTMLImageElement;
  /** Host cursor position as video-frame fractions, or `null` (hide it). */
  onHostPosition: (pos: { x: number; y: number } | null) => void;
  /** Virtual-cursor position from the touch input, or `null` (hide it). */
  onTouchCursor: (
    pos: { clientX: number; clientY: number; x: number; y: number } | null,
  ) => void;
  /** A pinch gesture at client point `(cx, cy)`. */
  onPinch: (factor: number, cx: number, cy: number) => void;
  /** A pan gesture by `(dx, dy)` client pixels. */
  onPan: (dx: number, dy: number) => void;
  /** After the host's cursor image was applied: show the overlay once it has one. */
  onHostCursor: () => void;
  /** The phone keyboard is being shown. */
  onKeyboardShow: () => void;
  /** The phone keyboard's layout changed (visual viewport resize). */
  onKeyboardLayout: () => void;
  /** The phone keyboard went away. */
  onKeyboardHide: () => void;
  /** Shows `note` in `#fullscreen-note` for a few seconds. */
  showFullscreenNote: (note: string) => void;
  /** Hides `#fullscreen-note` right away. */
  hideFullscreenNote: () => void;
  /** Session end: resets the overlay, the zoom and the auto-collapse state. */
  resetSession: () => void;
}

export function createSessionView(deps: SessionViewDeps): SessionView {
  const { root, video } = deps;

  const sessionScreen = root.querySelector<HTMLDivElement>("#session-screen")!;
  const sessionBarEl = root.querySelector<HTMLDivElement>("#session-bar")!;
  const sessionBarHandle = root.querySelector<HTMLButtonElement>("#session-bar-handle")!;
  const barCollapseBtn = root.querySelector<HTMLButtonElement>("#bar-collapse-btn")!;
  const fullscreenNoteEl = root.querySelector<HTMLSpanElement>("#fullscreen-note")!;
  const touchCursorEl = root.querySelector<HTMLImageElement>("#touch-cursor")!;
  const hostCursorEl = root.querySelector<HTMLImageElement>("#host-cursor")!;
  const sessionVideoEl = root.querySelector<HTMLDivElement>(".session-video")!;
  const zoomLayer = root.querySelector<HTMLDivElement>("#zoom-layer")!;

  let zoom: Zoom = identityZoom();
  // VM-3: last host cursor position (frame fractions); `null` -- hidden.
  let hostCursorPos: { x: number; y: number } | null = null;
  // H5-18: frame point at the container centre, to keep it when the container resizes.
  let viewAnchor: ViewAnchor | null = null;
  // Slice H5-2: last virtual-cursor frame position and the zoom to restore
  // once the phone keyboard goes away (`null` -- the keyboard didn't zoom).
  let lastTouchCursor: { x: number; y: number } | null = null;
  // H5-10: a touch is active, so the overlay must show as soon as it has an image.
  let touchCursorWanted = false;
  // Zoom scale and centre frame point before the keyboard zoomed (H5-18).
  let zoomBeforeKeyboard: { scale: number; anchor: ViewAnchor | null } | null = null;
  // H5-12: the keyboard zoom is applied once per show; later layout changes keep manual pinch/pan.
  let keyboardZoomApplied = false;
  let heightBeforeKeyboard = 0;
  // H5-4: the session bar was collapsed automatically for the keyboard in landscape.
  let barAutoCollapsed = false;
  function applyZoom(next: Zoom): void {
    zoom = clampZoom(
      next,
      sessionVideoEl.clientWidth,
      sessionVideoEl.clientHeight,
      video.videoWidth,
      video.videoHeight,
    );
    zoomLayer.style.transform = zoomCss(zoom);
    viewAnchor = anchorOf(
      zoom,
      sessionVideoEl.clientWidth,
      sessionVideoEl.clientHeight,
      video.videoWidth,
      video.videoHeight,
    );
    placeTouchCursor();
    placeHostCursor();
  }
  // H5-18: the container changed size (keyboard, bar, rotation) -- keep the
  // frame point that was at the centre there, instead of re-clamping the
  // box-relative offset (the letterbox moved the frame under it).
  function refitZoom(): void {
    const w = sessionVideoEl.clientWidth;
    const h = sessionVideoEl.clientHeight;
    if (zoom.scale === 1 || !viewAnchor) {
      applyZoom(zoom);
      return;
    }
    applyZoom(zoomAtAnchor(zoom.scale, viewAnchor, w, h, video.videoWidth, video.videoHeight));
  }
  // H5-6: places the virtual-cursor overlay from its frame position in
  // unscaled-layer coordinates (the layer at 1x is the container), so it
  // stays on the frame when the container's height changes (keyboard, bar).
  function placeTouchCursor(): void {
    if (!lastTouchCursor || touchCursorEl.hidden) return;
    const p = unmapPointer(
      lastTouchCursor.x,
      lastTouchCursor.y,
      { left: 0, top: 0, width: sessionVideoEl.clientWidth, height: sessionVideoEl.clientHeight },
      video.videoWidth,
      video.videoHeight,
    );
    if (!p) return;
    const hx = Number(touchCursorEl.dataset.hx ?? 0) || 0;
    const hy = Number(touchCursorEl.dataset.hy ?? 0) || 0;
    const s = zoom.scale;
    touchCursorEl.style.transform = `translate(${p.clientX - hx / s}px, ${p.clientY - hy / s}px) scale(${1 / s})`;
  }
  // VM-3: same placement as the touch cursor, for the host cursor overlay;
  // hidden while there is no valid position or no image yet.
  function placeHostCursor(): void {
    const p = hostCursorPlacement(
      hostCursorPos,
      { left: 0, top: 0, width: sessionVideoEl.clientWidth, height: sessionVideoEl.clientHeight },
      video.videoWidth,
      video.videoHeight,
    );
    if (!p || !hostCursorEl.getAttribute("src")) {
      hostCursorEl.hidden = true;
      return;
    }
    hostCursorEl.hidden = false;
    const hx = Number(hostCursorEl.dataset.hx ?? 0) || 0;
    const hy = Number(hostCursorEl.dataset.hy ?? 0) || 0;
    const s = zoom.scale;
    hostCursorEl.style.transform = `translate(${p.clientX - hx / s}px, ${p.clientY - hy / s}px) scale(${1 / s})`;
  }
  // The cursor image is encoded asynchronously: place it once it has loaded.
  hostCursorEl.addEventListener("load", placeHostCursor);
  window.addEventListener("resize", () => {
    refitZoom();
    placeTouchCursor();
  });
  // H5-12: the container changes height without a window resize too (session
  // bar wrapping, collapse); keep the layer clamped and the cursor overlay placed.
  if (typeof ResizeObserver !== "undefined") {
    new ResizeObserver(() => refitZoom()).observe(sessionVideoEl);
  }
  // H5-12/H5-15: collapse the bar while the keyboard is up. Measured in the
  // simulator (iPhone 17 Pro, portrait): with the keyboard open the wrapped bar
  // left the frame 80 px of height -- in portrait too, not only in landscape.
  function syncBarForKeyboard(): void {
    if (!barAutoCollapsed && !sessionScreen.classList.contains("is-collapsed")) {
      barAutoCollapsed = true;
      applyBarCollapsed(true, false);
    }
  }

  /** Applies `collapsed` to the session bar/handle (hidden state, title/
   * aria-label) and saves the choice (slice 3.5g) -- used both to apply the
   * setting loaded from storage at mount, and by `#bar-collapse-btn`'s and
   * `#session-bar-handle`'s click listeners below. The `is-collapsed` class
   * on `#session-screen` is read by the fullscreen CSS rules (slice 3.5h).
   * `persist = false` skips saving (temporary auto-collapse, H5-4). */
  function applyBarCollapsed(collapsed: boolean, persist = true): void {
    sessionScreen.classList.toggle("is-collapsed", collapsed);
    sessionBarEl.hidden = collapsed;
    sessionBarHandle.hidden = !collapsed;
    const title = collapseButtonTitle(collapsed);
    barCollapseBtn.title = title;
    barCollapseBtn.setAttribute("aria-label", title);
    sessionBarHandle.title = title;
    sessionBarHandle.setAttribute("aria-label", title);
    const storage = deps.getStorage();
    if (storage && persist) saveSessionBarCollapsed(storage, collapsed);
  }

  applyBarCollapsed(deps.initialStorage ? loadSessionBarCollapsed(deps.initialStorage) : false);

  // Slice 3.5g: collapses the session bar to its thin handle strip, which
  // becomes the only layout element left above the video -- not a pixel of
  // the remote screen is covered either way.
  barCollapseBtn.addEventListener("click", () => {
    applyBarCollapsed(true);
    deps.focusVideoUnlessTyping();
  });

  sessionBarHandle.addEventListener("click", () => {
    applyBarCollapsed(false);
    deps.focusVideoUnlessTyping();
  });

  let fullscreenNoteTimer: ReturnType<typeof setTimeout> | undefined;

  /** Shows `note` in `#fullscreen-note` for `FULLSCREEN_NOTE_MS` -- the "for
   * exit press/hold Esc" hint shown once on entering fullscreen (see the
   * `fullscreenchange` listener below). */
  function showFullscreenNote(note: string): void {
    fullscreenNoteEl.textContent = note;
    fullscreenNoteEl.hidden = false;
    if (fullscreenNoteTimer !== undefined) clearTimeout(fullscreenNoteTimer);
    fullscreenNoteTimer = setTimeout(() => {
      hideFullscreenNote();
    }, FULLSCREEN_NOTE_MS);
  }

  /** Hides `#fullscreen-note` right away, e.g. on exiting fullscreen (so a
   * still-pending hint from a brief fullscreen stay doesn't linger). */
  function hideFullscreenNote(): void {
    if (fullscreenNoteTimer !== undefined) {
      clearTimeout(fullscreenNoteTimer);
      fullscreenNoteTimer = undefined;
    }
    fullscreenNoteEl.hidden = true;
    fullscreenNoteEl.textContent = "";
  }

  return {
    touchCursorEl,
    hostCursorEl,
    onHostPosition: (pos) => {
      hostCursorPos = pos;
      placeHostCursor();
    },
    onTouchCursor: (pos) => {
      if (!pos) {
        touchCursorWanted = false;
        touchCursorEl.hidden = true;
        return;
      }
      lastTouchCursor = { x: pos.x, y: pos.y };
      touchCursorWanted = true;
      touchCursorEl.hidden = !touchCursorEl.getAttribute("src");
      placeTouchCursor();
      if (deps.getTouchMode() === "trackpad") {
        const c = sessionVideoEl.getBoundingClientRect();
        applyZoom(
          followPoint(
            zoom,
            pos.clientX - c.left,
            pos.clientY - c.top,
            sessionVideoEl.clientWidth,
            sessionVideoEl.clientHeight,
            24,
          ),
        );
      }
    },
    onPinch: (factor, cx, cy) => {
      const c = sessionVideoEl.getBoundingClientRect();
      applyZoom(
        zoomAt(
          zoom,
          factor,
          cx - c.left,
          cy - c.top,
          sessionVideoEl.clientWidth,
          sessionVideoEl.clientHeight,
        ),
      );
    },
    onPan: (dx, dy) => {
      applyZoom(panBy(zoom, dx, dy, sessionVideoEl.clientWidth, sessionVideoEl.clientHeight));
    },
    onHostCursor: () => {
      placeHostCursor();
      // H5-10: the overlay stayed hidden while it had no image; show it now.
      if (touchCursorWanted && touchCursorEl.getAttribute("src")) {
        touchCursorEl.hidden = false;
        placeTouchCursor();
      }
    },
    onKeyboardShow: () => {
      zoomBeforeKeyboard = zoom.scale < KEYBOARD_ZOOM ? { scale: zoom.scale, anchor: viewAnchor } : null;
      keyboardZoomApplied = false;
      heightBeforeKeyboard = sessionVideoEl.clientHeight;
      syncBarForKeyboard();
    },
    onKeyboardLayout: () => {
      syncBarForKeyboard();
      const w = sessionVideoEl.clientWidth;
      const h = sessionVideoEl.clientHeight;
      if (
        zoomBeforeKeyboard === null ||
        keyboardZoomApplied ||
        h >= heightBeforeKeyboard * 0.85
      ) {
        refitZoom(); // keep the centre frame point in the resized container
      } else {
        keyboardZoomApplied = true;
        const anchor = lastTouchCursor
          ? unmapPointer(
              lastTouchCursor.x,
              lastTouchCursor.y,
              { left: 0, top: 0, width: w, height: h },
              video.videoWidth,
              video.videoHeight,
            )
          : null;
        applyZoom(
          anchor
            ? centerOn(KEYBOARD_ZOOM, anchor.clientX, anchor.clientY, w, h)
            : centerOn(KEYBOARD_ZOOM, w / 2, h / 2, w, h),
        );
      }
      placeTouchCursor();
    },
    onKeyboardHide: () => {
      if (!deps.isSessionActive()) {
        zoomBeforeKeyboard = null;
        barAutoCollapsed = false;
        return;
      }
      if (barAutoCollapsed) {
        barAutoCollapsed = false;
        applyBarCollapsed(false, false);
      }
      const restore = zoomBeforeKeyboard;
      zoomBeforeKeyboard = null;
      if (restore === null) refitZoom();
      else if (restore.scale === 1 || !restore.anchor) applyZoom({ scale: restore.scale, tx: 0, ty: 0 });
      else {
        applyZoom(
          zoomAtAnchor(
            restore.scale,
            restore.anchor,
            sessionVideoEl.clientWidth,
            sessionVideoEl.clientHeight,
            video.videoWidth,
            video.videoHeight,
          ),
        );
      }
      placeTouchCursor();
    },
    showFullscreenNote,
    hideFullscreenNote,
    resetSession: () => {
      touchCursorEl.hidden = true;
      touchCursorEl.removeAttribute("src");
      touchCursorEl.style.visibility = "";
      touchCursorEl.style.transform = "";
      touchCursorEl.style.width = "";
      touchCursorEl.style.height = "";
      touchCursorWanted = false;
      hostCursorPos = null;
      hostCursorEl.hidden = true;
      hostCursorEl.removeAttribute("src");
      hostCursorEl.style.visibility = "";
      hostCursorEl.style.transform = "";
      hostCursorEl.style.width = "";
      hostCursorEl.style.height = "";
      zoomBeforeKeyboard = null;
      barAutoCollapsed = false;
      lastTouchCursor = null;
      viewAnchor = null;
      applyZoom(identityZoom());
    },
  };
}
