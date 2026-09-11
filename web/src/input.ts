// Captures mouse/keyboard input on the `<video>` element and forwards it to
// the host over the `input`/`pointer` data channels (see ARCHITECTURE.md §5
// and §7, and `proto::input::InputMessage` in `proto/src/input.rs`, the
// single source of truth for the wire shape).
//
// `pointer` is the lossy, unordered channel (a stale mouse position is
// worthless once a newer one exists -- see `host/src/transport/mod.rs`'s
// `data_channel_specs`), so pointer moves are the only message coalesced
// here (at most one `pointer_move` per animation frame); everything else
// goes out immediately on the reliable `input` channel.

import type { InputMessage } from "./generated/InputMessage";
import type { PointerButton } from "./generated/PointerButton";

/** The two data channels `attachInput` needs, opened by the host and handed
 * to the client via `PeerSession`'s `onDataChannel` callback (see
 * `app.ts`). */
export interface InputChannels {
  input: RTCDataChannel;
  pointer: RTCDataChannel;
}

/** Minimal shape of a `DOMRect` this module depends on, so tests can pass a
 * plain object instead of constructing a real one (this file's tests run
 * under vitest's `node` environment, with no DOM -- see `session.test.ts`
 * for the same pattern applied to `RTCPeerConnection`). */
type RectLike = Pick<DOMRect, "left" | "top" | "width" | "height">;

/**
 * Maps a client-space point (e.g. `PointerEvent.clientX/Y`) to a normalized
 * `[0,1]` position within the video *frame*, accounting for `object-fit:
 * contain` letterboxing: the frame is centered in `rect` and scaled to the
 * largest size that fits without cropping, so if the frame's aspect ratio
 * doesn't match `rect`'s there are blank bars on two sides that aren't part
 * of the frame at all.
 *
 * Returns `null` when the point falls in a letterbox bar (outside the
 * frame) or when either dimension is degenerate (nothing rendered yet).
 */
export function mapPointer(
  clientX: number,
  clientY: number,
  rect: RectLike,
  videoWidth: number,
  videoHeight: number,
): { x: number; y: number } | null {
  if (rect.width <= 0 || rect.height <= 0 || videoWidth <= 0 || videoHeight <= 0) {
    return null;
  }

  const containerAspect = rect.width / rect.height;
  const videoAspect = videoWidth / videoHeight;

  let displayWidth: number;
  let displayHeight: number;
  if (videoAspect > containerAspect) {
    // Frame is relatively wider than the element: fills the width,
    // letterboxed top/bottom.
    displayWidth = rect.width;
    displayHeight = rect.width / videoAspect;
  } else {
    // Fills the height, letterboxed left/right.
    displayHeight = rect.height;
    displayWidth = rect.height * videoAspect;
  }
  const offsetX = (rect.width - displayWidth) / 2;
  const offsetY = (rect.height - displayHeight) / 2;

  const frameX = clientX - rect.left - offsetX;
  const frameY = clientY - rect.top - offsetY;
  if (frameX < 0 || frameX > displayWidth || frameY < 0 || frameY > displayHeight) {
    return null;
  }

  return { x: frameX / displayWidth, y: frameY / displayHeight };
}

/** Normalizes a `WheelEvent`'s delta to "lines" (see
 * `proto::input::InputMessage::Wheel`), collapsing the three
 * `WheelEvent.deltaMode` units browsers report into one. */
export function normalizeWheel(
  deltaX: number,
  deltaY: number,
  deltaMode: number,
): { dx: number; dy: number } {
  // `WheelEvent.DOM_DELTA_{PIXEL,LINE,PAGE}` are `0`, `1`, `2` respectively
  // -- used as literals (not the `WheelEvent` static properties) so this
  // function has no dependency on a browser-only global and stays testable
  // under vitest's `node` environment (see `session.test.ts`'s doc comment
  // for the same reasoning applied to `RTCPeerConnection`).
  const toLines = (delta: number): number => {
    switch (deltaMode) {
      case 1: // DOM_DELTA_LINE
        return delta;
      case 2: // DOM_DELTA_PAGE
        return delta * 3;
      default:
        // DOM_DELTA_PIXEL: ~40px per line is the long-standing convention
        // (matches what most browsers use internally for line-mode delta).
        return Math.round((delta / 40) * 10) / 10;
    }
  };
  return { dx: toLines(deltaX), dy: toLines(deltaY) };
}

/** Builds the `Key` message for a keydown/keyup, or `null` for an
 * auto-repeat keydown -- the OS/host repeats a held key on its own once, so
 * forwarding every repeat event would double it up. */
export function keyToMessage(code: string, pressed: boolean, repeat: boolean): InputMessage | null {
  if (repeat) return null;
  return { type: "key", code, pressed };
}

const POINTER_BUTTON_NAMES: readonly PointerButton[] = ["left", "middle", "right", "back", "forward"];

/** Maps `PointerEvent.button` (0-4) to the proto `PointerButton` name, or
 * `null` for an index this UA doesn't use. */
function pointerButtonFromIndex(button: number): PointerButton | null {
  if (button < 0 || button >= POINTER_BUTTON_NAMES.length) return null;
  return POINTER_BUTTON_NAMES[button] ?? null;
}

function send(channel: RTCDataChannel, msg: InputMessage): void {
  // A channel not yet (or no longer) open would throw on `send`; dropping
  // the message is correct here -- there's no queue to retry into, and a
  // dropped pointer move/keystroke while reconnecting is not worth stalling
  // over.
  if (channel.readyState !== "open") return;
  channel.send(JSON.stringify(msg));
}

/**
 * Wires pointer/wheel/keyboard listeners on `video` and starts forwarding
 * them to the host over `channels`. Returns a `detach` function that removes
 * every listener this installed.
 */
export function attachInput(video: HTMLVideoElement, channels: InputChannels): () => void {
  let rafHandle: number | null = null;
  let pendingMove: { x: number; y: number } | null = null;

  function flushPointerMove(): void {
    rafHandle = null;
    if (!pendingMove) return;
    send(channels.pointer, { type: "pointer_move", x: pendingMove.x, y: pendingMove.y });
    pendingMove = null;
  }

  function queuePointerMove(point: { x: number; y: number }): void {
    pendingMove = point;
    if (rafHandle === null) {
      rafHandle = requestAnimationFrame(flushPointerMove);
    }
  }

  function pointFromClient(clientX: number, clientY: number): { x: number; y: number } | null {
    const rect = video.getBoundingClientRect();
    return mapPointer(clientX, clientY, rect, video.videoWidth, video.videoHeight);
  }

  function onPointerMove(e: PointerEvent): void {
    const events =
      typeof e.getCoalescedEvents === "function" ? e.getCoalescedEvents() : [e];
    const latest = events.length > 0 ? events[events.length - 1] : e;
    if (!latest) return;
    const point = pointFromClient(latest.clientX, latest.clientY);
    if (point) queuePointerMove(point);
  }

  function sendPointerButton(e: PointerEvent, pressed: boolean): void {
    const point = pointFromClient(e.clientX, e.clientY);
    if (!point) return;
    const button = pointerButtonFromIndex(e.button);
    if (!button) return;
    send(channels.input, { type: "pointer_button", button, pressed, x: point.x, y: point.y });
  }

  function onPointerDown(e: PointerEvent): void {
    sendPointerButton(e, true);
  }

  function onPointerUp(e: PointerEvent): void {
    sendPointerButton(e, false);
  }

  function onContextMenu(e: MouseEvent): void {
    e.preventDefault();
  }

  function onWheel(e: WheelEvent): void {
    e.preventDefault();
    const point = pointFromClient(e.clientX, e.clientY);
    if (!point) return;
    const { dx, dy } = normalizeWheel(e.deltaX, e.deltaY, e.deltaMode);
    send(channels.input, { type: "wheel", dx, dy, x: point.x, y: point.y });
  }

  function onKeyDown(e: KeyboardEvent): void {
    e.preventDefault();
    const msg = keyToMessage(e.code, true, e.repeat);
    if (msg) send(channels.input, msg);
  }

  function onKeyUp(e: KeyboardEvent): void {
    e.preventDefault();
    const msg = keyToMessage(e.code, false, e.repeat);
    if (msg) send(channels.input, msg);
  }

  function releaseAll(): void {
    send(channels.input, { type: "release_all" });
  }

  function onWindowBlur(): void {
    releaseAll();
  }

  function onVisibilityChange(): void {
    if (document.hidden) releaseAll();
  }

  video.addEventListener("pointermove", onPointerMove);
  video.addEventListener("pointerdown", onPointerDown);
  video.addEventListener("pointerup", onPointerUp);
  video.addEventListener("contextmenu", onContextMenu);
  video.addEventListener("wheel", onWheel, { passive: false });
  video.addEventListener("keydown", onKeyDown);
  video.addEventListener("keyup", onKeyUp);
  window.addEventListener("blur", onWindowBlur);
  document.addEventListener("visibilitychange", onVisibilityChange);

  return () => {
    if (rafHandle !== null) cancelAnimationFrame(rafHandle);
    video.removeEventListener("pointermove", onPointerMove);
    video.removeEventListener("pointerdown", onPointerDown);
    video.removeEventListener("pointerup", onPointerUp);
    video.removeEventListener("contextmenu", onContextMenu);
    video.removeEventListener("wheel", onWheel);
    video.removeEventListener("keydown", onKeyDown);
    video.removeEventListener("keyup", onKeyUp);
    window.removeEventListener("blur", onWindowBlur);
    document.removeEventListener("visibilitychange", onVisibilityChange);
  };
}
