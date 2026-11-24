// View-only mode (slice VM-3): the connected person watches the screen and
// the host does nothing on their behalf (the host enforces it, see
// `ControlMessage::ViewMode`). Pure helpers plus a tiny touch handler for the
// local zoom/pan gestures that must keep working while input is detached.
// `app.ts` owns the DOM state; the host is the source of truth, so the UI
// changes on the host's `ViewMode` answer, not on the click.

import type { RectLike } from "./input";
import { unmapPointer } from "./input";
import { TouchRecognizer, type TouchEventKind } from "./touchInput";

/** What `#view-mode-btn` shows for the current mode. */
export interface ViewModeButtonView {
  hidden: boolean;
  text: string;
  title: string;
}

/** `available` is `host_info.view_mode_available` (an old host omits it, so
 * anything but a literal `true` hides the button). */
export function viewModeButtonView(
  on: boolean,
  available: boolean | undefined,
): ViewModeButtonView {
  return on
    ? {
        hidden: available !== true,
        text: "Управлять",
        title: "Включить управление: ввод, буфер обмена и файлы снова заработают на хосте",
      }
    : {
        hidden: available !== true,
        text: "Только смотреть",
        title: "Только смотреть: на хосте ничего не будет нажиматься, копироваться и передаваться",
      };
}

/** Text of the `#view-mode-badge` banner, or `null` when not in view mode. */
export function viewModeBadgeText(on: boolean): string | null {
  return on ? "Режим просмотра" : null;
}

/** Whether keyboard/pointer input may be attached: neither the D26
 * "host cannot inject" state nor the view-only mode forbids it. */
export function inputAttachAllowed(state: { inputBlocked: boolean; viewMode: boolean }): boolean {
  return !state.inputBlocked && !state.viewMode;
}

/** Inverse of the pointer normalisation: the host cursor's frame position
 * `(x, y)` (fractions of the video frame) to coordinates inside `rect` (the
 * video element's box, `object-fit: contain` letterbox included). `null`
 * when the position is not finite or outside `[0, 1]` (the host cursor is on
 * another display) or nothing is rendered yet. */
export function hostCursorPlacement(
  pos: { x: number; y: number } | null,
  rect: RectLike,
  videoWidth: number,
  videoHeight: number,
): { clientX: number; clientY: number } | null {
  if (!pos) return null;
  const { x, y } = pos;
  if (!Number.isFinite(x) || !Number.isFinite(y)) return null;
  if (x < 0 || x > 1 || y < 0 || y > 1) return null;
  return unmapPointer(x, y, rect, videoWidth, videoHeight);
}

export interface ViewTouchHooks {
  onPinch: (factor: number, clientX: number, clientY: number) => void;
  onPan: (dx: number, dy: number) => void;
}

/** Local-only touch gestures for the view-only mode: one finger pans and two
 * fingers pinch the picture (the recognizer's "direct" mode); nothing is sent
 * anywhere. Returns a detach function. */
export function attachViewTouch(video: HTMLElement, hooks: ViewTouchHooks): () => void {
  const recognizer = new TouchRecognizer(() => "direct");
  let timer: ReturnType<typeof setTimeout> | null = null;

  function feed(kind: TouchEventKind, id: number, x: number, y: number): void {
    for (const action of recognizer.handle({ kind, id, x, y }, performance.now())) {
      if (action.kind === "pinch") hooks.onPinch(action.factor, action.cx, action.cy);
      else if (action.kind === "pan") hooks.onPan(action.dx, action.dy);
    }
    if (timer !== null) {
      clearTimeout(timer);
      timer = null;
    }
    const at = recognizer.nextTimerAt();
    if (at !== null) {
      timer = setTimeout(
        () => {
          timer = null;
          feed("timer", 0, 0, 0);
        },
        Math.max(0, at - performance.now()),
      );
    }
  }

  const onDown = (e: PointerEvent): void => {
    if (e.pointerType !== "touch") return;
    e.preventDefault();
    feed("down", e.pointerId, e.clientX, e.clientY);
  };
  const onMove = (e: PointerEvent): void => {
    if (e.pointerType === "touch") feed("move", e.pointerId, e.clientX, e.clientY);
  };
  const onUp = (e: PointerEvent): void => {
    if (e.pointerType === "touch") feed("up", e.pointerId, e.clientX, e.clientY);
  };
  const onCancel = (e: PointerEvent): void => {
    if (e.pointerType === "touch") feed("cancel", e.pointerId, e.clientX, e.clientY);
  };
  video.addEventListener("pointerdown", onDown);
  video.addEventListener("pointermove", onMove);
  video.addEventListener("pointerup", onUp);
  video.addEventListener("pointercancel", onCancel);
  return () => {
    if (timer !== null) clearTimeout(timer);
    timer = null;
    video.removeEventListener("pointerdown", onDown);
    video.removeEventListener("pointermove", onMove);
    video.removeEventListener("pointerup", onUp);
    video.removeEventListener("pointercancel", onCancel);
  };
}
