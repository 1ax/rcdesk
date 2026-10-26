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
import { isCopyShortcut, isPasteShortcut } from "./clipboard";
import { remapCode, type KeyRemapMode } from "./keyRemap";
import { createLayoutChord } from "./layoutChord";
import type { TouchMode } from "./viewport";
import {
  TouchRecognizer,
  VirtualCursor,
  WheelAccumulator,
  type TouchAction,
  type TouchEventKind,
} from "./touchInput";

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
export type RectLike = Pick<DOMRect, "left" | "top" | "width" | "height">;

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
  const layout = frameLayout(rect, videoWidth, videoHeight);
  if (!layout) return null;
  const { displayWidth, displayHeight, offsetX, offsetY } = layout;

  const frameX = clientX - rect.left - offsetX;
  const frameY = clientY - rect.top - offsetY;
  if (frameX < 0 || frameX > displayWidth || frameY < 0 || frameY > displayHeight) {
    return null;
  }

  return { x: frameX / displayWidth, y: frameY / displayHeight };
}

/** Inverse of `mapPointer`: a normalized `[0,1]` position within the frame
 * back to client-space coordinates (accounting for the same `object-fit:
 * contain` letterbox). `null` when either dimension is degenerate. Used to
 * place the touch-mode virtual cursor overlay. */
export function unmapPointer(
  x: number,
  y: number,
  rect: RectLike,
  videoWidth: number,
  videoHeight: number,
): { clientX: number; clientY: number } | null {
  const layout = frameLayout(rect, videoWidth, videoHeight);
  if (!layout) return null;
  return {
    clientX: rect.left + layout.offsetX + x * layout.displayWidth,
    clientY: rect.top + layout.offsetY + y * layout.displayHeight,
  };
}

/** Size and offset of the displayed frame inside `rect` under `object-fit:
 * contain`, or `null` when nothing is rendered yet. */
export function frameLayout(
  rect: RectLike,
  videoWidth: number,
  videoHeight: number,
): { displayWidth: number; displayHeight: number; offsetX: number; offsetY: number } | null {
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
  return {
    displayWidth,
    displayHeight,
    offsetX: (rect.width - displayWidth) / 2,
    offsetY: (rect.height - displayHeight) / 2,
  };
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

const MODIFIER_CODES: ReadonlySet<string> = new Set([
  "ShiftLeft",
  "ShiftRight",
  "ControlLeft",
  "ControlRight",
  "AltLeft",
  "AltRight",
  "MetaLeft",
  "MetaRight",
  "OSLeft",
  "OSRight",
  "CapsLock",
]);

/** True for modifier keys (by `KeyboardEvent.code`, before any remap). */
export function isModifierCode(code: string): boolean {
  return MODIFIER_CODES.has(code);
}

/** Builds the `Key` message for a keydown/keyup, or `null` for an
 * auto-repeat keydown of a modifier. The host OS does not repeat injected
 * (CGEventPost / SendInput) key presses, so auto-repeat keydowns of ordinary
 * keys are forwarded as plain `pressed: true` messages (slice I8); a
 * modifier needs no repeat -- it stays held until its keyup. `remapMode` (slices
 * 3.5f/4.6e, default `"none"`) is applied to `code` via `keyRemap.remapCode`
 * before the message is built -- the same call, with the same flag, for
 * both a keydown and the matching keyup (see `attachInput`'s `onKeyDown`/
 * `onKeyUp`) so a held key can't desync into "Meta down, Ctrl up" on the
 * host if the setting changes mid-press (`app.ts`'s toggle also sends a
 * `release_all` on change, belt and suspenders). */
export function keyToMessage(
  code: string,
  pressed: boolean,
  repeat: boolean,
  remapMode: KeyRemapMode = "none",
): InputMessage | null {
  if (repeat && isModifierCode(code)) return null;
  return { type: "key", code: remapCode(code, remapMode), pressed };
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
 * Gates `input`-channel messages behind an in-flight paste sync (slice
 * 2.5c, decision 2a): while open, every message passed to `send` is queued
 * instead of forwarded straight to `sink`; `release` closes the gate and
 * flushes the queue through `sink`, in the order the messages arrived --
 * so a Cmd/Ctrl+V keydown (queued first, see `attachInput`'s `onKeyDown`)
 * and every key event that lands while the host's clipboard is being
 * synced go out in the same order they happened, after the sync settles
 * (`ClipboardBridge.syncBeforePaste` never rejects, but `release` is
 * called either way -- see `attachInput`). */
export class KeyMessageGate {
  private queue: InputMessage[] = [];
  private open_ = false;

  get isOpen(): boolean {
    return this.open_;
  }

  open(): void {
    this.open_ = true;
  }

  /** Forwards `msg` to `sink` immediately if the gate is closed, or queues
   * it (in arrival order) if open. */
  send(msg: InputMessage, sink: (msg: InputMessage) => void): void {
    if (this.open_) this.queue.push(msg);
    else sink(msg);
  }

  /** Closes the gate and flushes the queue through `sink`, in order. Safe
   * to call when already closed (flushes an empty queue, a no-op). */
  release(sink: (msg: InputMessage) => void): void {
    this.open_ = false;
    const pending = this.queue;
    this.queue = [];
    for (const msg of pending) sink(msg);
  }
}

/** Optional clipboard hooks `attachInput` calls on the matching keyboard
 * shortcuts (slice 2.5c) -- `app.ts` wires these to a `ClipboardBridge`.
 * Omitted (e.g. no `ClipboardBridge` for this session), `attachInput`
 * behaves exactly as it did before this slice: shortcuts are forwarded as
 * plain `key` messages with no gating or copy hook. */
/** Slices H1/H3: touch-mode callbacks supplied by the app. */
export interface TouchHooks {
  mode: () => TouchMode;
  /** Virtual cursor: client position (to draw it) plus the normalized `[0,1]`
   * frame position (slice H5-2 zooms around it), or `null` (hide it). */
  onCursor: (pos: { clientX: number; clientY: number; x: number; y: number } | null) => void;
  onPinch: (factor: number, clientX: number, clientY: number) => void;
  onPan: (dx: number, dy: number) => void;
}

export interface InputHooks {
  /** Called on a Cmd/Ctrl+V keydown, before that keydown (and any key
   * event that arrives while this promise is pending) is sent to the host
   * -- gives the caller a chance to sync the local clipboard to the host
   * first (`ClipboardBridge.syncBeforePaste`). Never expected to reject,
   * but `attachInput` releases the gate either way. */
  beforePaste?: () => Promise<void>;
  /** Called synchronously from within a Cmd/Ctrl+C or +X keydown handler --
   * i.e. still inside the user gesture, which Safari's
   * `navigator.clipboard.write` requires (`ClipboardBridge.beginDeferredCopy`). */
  onCopyShortcut?: () => void;
}

/**
 * Wires pointer/wheel/keyboard listeners on `video` and starts forwarding
 * them to the host over `channels`. Returns a `detach` function that removes
 * every listener this installed.
 */
export function attachInput(
  video: HTMLVideoElement,
  channels: InputChannels,
  hooks?: InputHooks,
  // Slice 3.5f: a getter, not a plain value, since the owner can flip
  // the "Cmd как Ctrl" toggle (`app.ts`) mid-session -- every keydown/keyup
  // reads the current value at the moment it fires, rather than capturing
  // whatever was true when `attachInput` was called.
  getRemapMode?: () => KeyRemapMode,
  // Slice 4.6d: called when the Alt+Shift chord completes (see
  // `layoutChord.ts`). Key forwarding to the host is unchanged.
  onLayoutChord?: () => void,
  // Slices H1/H3: touch hooks (mode, cursor overlay, pinch, pan).
  touch?: TouchHooks,
): () => void {
  const layoutChord = createLayoutChord();
  let rafHandle: number | null = null;
  let pendingMove: { x: number; y: number } | null = null;
  const keyGate = new KeyMessageGate();

  function sendGatedInput(msg: InputMessage): void {
    keyGate.send(msg, (m) => send(channels.input, m));
  }

  function flushPointerMove(): void {
    rafHandle = null;
    if (!pendingMove) return;
    send(channels.pointer, { type: "pointer_move", x: pendingMove.x, y: pendingMove.y });
    pendingMove = null;
  }

  // H5-16: a tap goes out like a finger would -- hover, press, release -- and
  // taps are serialized, so a quick second tap never overtakes the first one's
  // release. macOS menus drop a zero-length click (measured: 3 of 5 menu taps
  // landed; with these delays 6 of 6).
  const TAP_HOVER_MS = 20;
  const TAP_HOLD_MS = 60;
  // A right tap stays instantaneous: macOS opens the context menu on the
  // right button's press, and a release 60 ms later would land on that menu
  // (measured: the menu never stayed open, 0 of 6 pastes).
  const RIGHT_TAP_HOLD_MS = 0;
  let tapChain: Promise<void> = Promise.resolve();
  const wait = (ms: number) => new Promise<void>((r) => setTimeout(r, ms));
  function sendTap(button: PointerButton, point: { x: number; y: number }): void {
    pendingMove = null;
    send(channels.pointer, { type: "pointer_move", x: point.x, y: point.y });
    tapChain = tapChain
      .then(() => wait(TAP_HOVER_MS))
      .then(() =>
        send(channels.input, { type: "pointer_button", button, pressed: true, x: point.x, y: point.y }),
      )
      .then(() => wait(button === "right" ? RIGHT_TAP_HOLD_MS : TAP_HOLD_MS))
      .then(() =>
        send(channels.input, { type: "pointer_button", button, pressed: false, x: point.x, y: point.y }),
      )
      .catch(() => undefined);
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

  // --- Touch "trackpad" mode (slice H1) ---
  const recognizer = new TouchRecognizer(() => touch?.mode() ?? "trackpad");
  const virtualCursor = new VirtualCursor();
  const wheelAcc = new WheelAccumulator();
  let touchActive = false;
  let touchTimer: ReturnType<typeof setTimeout> | null = null;

  function showTouchCursor(): void {
    if (!touch) return;
    const rect = video.getBoundingClientRect();
    const pos = unmapPointer(
      virtualCursor.x,
      virtualCursor.y,
      rect,
      video.videoWidth,
      video.videoHeight,
    );
    touch.onCursor(pos ? { ...pos, x: virtualCursor.x, y: virtualCursor.y } : null);
  }

  function runTouchAction(action: TouchAction): void {
    switch (action.kind) {
      case "pinch":
        touch?.onPinch(action.factor, action.cx, action.cy);
        return;
      case "pan":
        touch?.onPan(action.dx, action.dy);
        return;
      case "click": {
        const point = pointFromClient(action.x, action.y);
        if (!point) return;
        virtualCursor.x = point.x;
        virtualCursor.y = point.y;
        showTouchCursor();
        sendTap(action.button, point);
        return;
      }
      case "tap":
        sendTap(action.button, { x: virtualCursor.x, y: virtualCursor.y });
        return;
      default:
        break;
    }
    const rect = video.getBoundingClientRect();
    const layout = frameLayout(rect, video.videoWidth, video.videoHeight);
    if (!layout) return;
    switch (action.kind) {
      case "move":
        virtualCursor.moveBy(action.dx, action.dy, layout.displayWidth, layout.displayHeight);
        queuePointerMove({ x: virtualCursor.x, y: virtualCursor.y });
        showTouchCursor();
        break;
      case "button":
        send(channels.input, {
          type: "pointer_button",
          button: action.button,
          pressed: action.pressed,
          x: virtualCursor.x,
          y: virtualCursor.y,
        });
        break;
      case "drag":
        send(channels.input, {
          type: "pointer_button",
          button: "left",
          pressed: action.pressed,
          x: virtualCursor.x,
          y: virtualCursor.y,
        });
        break;
      case "scroll": {
        const lines = wheelAcc.add(action.dx, action.dy);
        if (lines.dx === 0 && lines.dy === 0) break;
        // macOS scrolls the window under the pointer (H5-13): in direct mode
        // the pointer goes under the fingers first.
        if (touch?.mode() === "direct") {
          const p = pointFromClient(action.cx, action.cy);
          if (p) {
            virtualCursor.x = p.x;
            virtualCursor.y = p.y;
            pendingMove = null;
            send(channels.pointer, { type: "pointer_move", x: p.x, y: p.y });
          }
        }
        // Finger up = content up = wheel down (as on a phone).
        send(channels.input, {
          type: "wheel",
          dx: 0 - lines.dx,
          dy: 0 - lines.dy,
          x: virtualCursor.x,
          y: virtualCursor.y,
        });
        break;
      }
    }
  }

  function scheduleTouchTimer(): void {
    if (touchTimer !== null) {
      clearTimeout(touchTimer);
      touchTimer = null;
    }
    const at = recognizer.nextTimerAt();
    if (at === null) return;
    touchTimer = setTimeout(
      () => {
        touchTimer = null;
        feedTouch("timer", 0, 0, 0);
      },
      Math.max(0, at - performance.now()),
    );
  }

  function feedTouch(kind: TouchEventKind, id: number, x: number, y: number): void {
    const actions = recognizer.handle({ kind, id, x, y }, performance.now());
    for (const action of actions) runTouchAction(action);
    scheduleTouchTimer();
  }

  function enterTouch(): void {
    if (touchActive) return;
    touchActive = true;
    showTouchCursor();
  }

  function leaveTouch(): void {
    if (!touchActive) return;
    touchActive = false;
    touch?.onCursor(null);
  }

  function onPointerMove(e: PointerEvent): void {
    if (e.pointerType === "touch") {
      enterTouch();
      feedTouch("move", e.pointerId, e.clientX, e.clientY);
      return;
    }
    leaveTouch();
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
    if (e.pointerType === "touch") {
      // No focus change and no compatibility mouse events for a touch: the
      // gesture recognizer owns it (and a focus move would dismiss the
      // phone keyboard, slice H2b).
      e.preventDefault();
      enterTouch();
      feedTouch("down", e.pointerId, e.clientX, e.clientY);
      return;
    }
    leaveTouch();
    sendPointerButton(e, true);
  }

  function onPointerUp(e: PointerEvent): void {
    if (e.pointerType === "touch") {
      feedTouch("up", e.pointerId, e.clientX, e.clientY);
      return;
    }
    sendPointerButton(e, false);
  }

  function onPointerCancel(e: PointerEvent): void {
    if (e.pointerType === "touch") {
      feedTouch("cancel", e.pointerId, e.clientX, e.clientY);
    }
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
    const msg = keyToMessage(e.code, true, e.repeat, getRemapMode?.());

    if (msg && isPasteShortcut(e) && hooks?.beforePaste) {
      // Open the gate *before* queueing this keydown, so it -- and every
      // key event that lands while `beforePaste` is pending -- goes out
      // strictly after whatever `beforePaste` itself sends (the host's
      // `clipboard_text`, sent directly on the same `input` channel, see
      // `ClipboardBridge.syncBeforePaste`), in arrival order.
      keyGate.open();
      sendGatedInput(msg);
      const wait = hooks.beforePaste();
      wait
        .catch(() => {
          // `syncBeforePaste` doesn't reject, but the gate must still open
          // if some other hook ever does.
        })
        .then(() => keyGate.release((m) => send(channels.input, m)));
      return;
    }

    if (isCopyShortcut(e)) hooks?.onCopyShortcut?.();

    layoutChord.keyDown(e.code); // idempotent for auto-repeat
    if (msg) sendGatedInput(msg);
  }

  function onKeyUp(e: KeyboardEvent): void {
    e.preventDefault();
    const msg = keyToMessage(e.code, false, e.repeat, getRemapMode?.());
    if (msg) sendGatedInput(msg);
    if (layoutChord.keyUp(e.code)) onLayoutChord?.();
  }

  function releaseAll(): void {
    layoutChord.reset();
    sendGatedInput({ type: "release_all" });
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
  video.addEventListener("pointercancel", onPointerCancel);
  video.addEventListener("contextmenu", onContextMenu);
  video.addEventListener("wheel", onWheel, { passive: false });
  video.addEventListener("keydown", onKeyDown);
  video.addEventListener("keyup", onKeyUp);
  window.addEventListener("blur", onWindowBlur);
  document.addEventListener("visibilitychange", onVisibilityChange);

  return () => {
    if (rafHandle !== null) cancelAnimationFrame(rafHandle);
    if (touchTimer !== null) clearTimeout(touchTimer);
    touchTimer = null;
    touch?.onCursor(null);
    video.removeEventListener("pointermove", onPointerMove);
    video.removeEventListener("pointerdown", onPointerDown);
    video.removeEventListener("pointerup", onPointerUp);
    video.removeEventListener("pointercancel", onPointerCancel);
    video.removeEventListener("contextmenu", onContextMenu);
    video.removeEventListener("wheel", onWheel);
    video.removeEventListener("keydown", onKeyDown);
    video.removeEventListener("keyup", onKeyUp);
    window.removeEventListener("blur", onWindowBlur);
    document.removeEventListener("visibilitychange", onVisibilityChange);
  };
}
