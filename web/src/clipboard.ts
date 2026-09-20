// Client-side clipboard sync (slice 2.5c): mirrors the system clipboard
// between the browser and the host over the `control` (host -> client) and
// `input` (client -> host) channels -- see `proto::control::ControlMessage`
// and `proto::input::InputMessage`'s `ClipboardText` variants for the wire
// contract and why the two directions travel on different channels.
//
// No DOM access here: every browser API this needs (`navigator.clipboard`,
// `setTimeout`, sending on a data channel, showing a note) is injected via
// `ClipboardBridgeDeps`, so this stays unit testable under vitest's `node`
// environment (see `input.ts`'s `RectLike` for the same pattern applied
// elsewhere in this codebase). `app.ts` wires the real dependencies.

import type { InputMessage } from "./generated/InputMessage";

/** Matches `host::clipboard::MAX_CLIPBOARD_BYTES` -- text larger than this,
 * in either direction, is never sent or written (see `exceedsLimit`). */
export const MAX_CLIPBOARD_BYTES = 200_000;

const encoder = new TextEncoder();

/** UTF-8 byte length of `text`, the same unit `MAX_CLIPBOARD_BYTES` counts
 * in (the host measures serialized UTF-8 bytes, not JS UTF-16 code units). */
export function utf8Length(text: string): number {
  return encoder.encode(text).length;
}

/** Whether `text` is too large to sync (see `MAX_CLIPBOARD_BYTES`). */
export function exceedsLimit(text: string): boolean {
  return utf8Length(text) > MAX_CLIPBOARD_BYTES;
}

/** The fields of a `KeyboardEvent` `isPasteShortcut`/`isCopyShortcut` need --
 * a real `KeyboardEvent` satisfies this structurally, so callers pass it
 * straight through with no cast (see `input.ts`'s `onKeyDown`). */
export interface ShortcutKeyEvent {
  code: string;
  metaKey: boolean;
  ctrlKey: boolean;
  altKey: boolean;
  repeat: boolean;
}

/** Whether `e` is the paste shortcut (Cmd+V on macOS, Ctrl+V elsewhere) that
 * gates the key queue in `input.ts` -- see `ClipboardBridge.syncBeforePaste`. */
export function isPasteShortcut(e: ShortcutKeyEvent): boolean {
  return (e.metaKey || e.ctrlKey) && e.code === "KeyV" && !e.altKey && !e.repeat;
}

/** Whether `e` is a copy/cut shortcut (Cmd/Ctrl+C or Cmd/Ctrl+X) -- in
 * Safari this arms the deferred `ClipboardItem` write (see
 * `ClipboardBridge.beginDeferredCopy`). */
export function isCopyShortcut(e: ShortcutKeyEvent): boolean {
  return (e.metaKey || e.ctrlKey) && (e.code === "KeyC" || e.code === "KeyX") && !e.altKey && !e.repeat;
}

/** Whether `userAgent` looks like Safari (not Chrome/Chromium and not one of
 * the other browsers whose UA also contains "Safari" as a compatibility
 * token, e.g. Chrome on iOS/Android). Only Safari needs the deferred
 * `ClipboardItem` write in `beginDeferredCopy` -- see that method's doc
 * comment for why. */
export function isSafari(userAgent: string): boolean {
  return /Safari/.test(userAgent) && !/Chrome|Chromium|CriOS|FxiOS|Android|Edg/.test(userAgent);
}

/** How long `syncBeforePaste` waits for `readText()` (e.g. Safari's
 * "Paste" permission prompt) before giving up and letting the queued key
 * events through without a `clipboard_text`. */
const PASTE_READ_TIMEOUT_MS = 10_000;

/** How long a deferred Safari copy (`beginDeferredCopy`) waits for the
 * host's next `clipboard_text` before the `ClipboardItem` write is left to
 * reject (see that method's doc comment). */
const DEFERRED_COPY_TIMEOUT_MS = 3_000;

type TimeoutHandle = ReturnType<typeof setTimeout>;

/** Browser/transport dependencies `ClipboardBridge` needs, injected so it
 * stays testable without a DOM (see this file's header comment).
 * `writeDeferred` is only provided in Safari (see `beginDeferredCopy`) --
 * elsewhere `beginDeferredCopy` is a no-op. */
export interface ClipboardBridgeDeps {
  writeText(text: string): Promise<void>;
  readText(): Promise<string>;
  writeDeferred?(blob: Promise<Blob>): Promise<void>;
  send(msg: InputMessage): void;
  notify(note: string): void;
  setTimeout(handler: () => void, ms: number): TimeoutHandle;
  clearTimeout(handle: TimeoutHandle): void;
}

/** A deferred Safari copy armed by `beginDeferredCopy`, resolved by the next
 * `onHostText` or rejected by its own timeout. */
interface DeferredCopy {
  resolve(blob: Blob): void;
  reject(err: Error): void;
  timeoutHandle: TimeoutHandle;
}

/** Warns (length only, never content) and shows a note about a clipboard
 * text too large to sync (decision 5, slice 2.5c). */
function warnTooLarge(deps: Pick<ClipboardBridgeDeps, "notify">, text: string): void {
  const bytes = utf8Length(text);
  console.warn(`clipboard text too large to sync (${bytes} bytes)`);
  deps.notify(`Буфер обмена слишком большой для синхронизации (${Math.round(bytes / 1024)} КБ)`);
}

/**
 * Mirrors the clipboard between this browser and the host for one session.
 * See the doc comments on individual methods, and the decisions in the
 * slice 2.5c prompt, for the exact rules each one follows.
 */
export class ClipboardBridge {
  private readonly deps: ClipboardBridgeDeps;
  /** Last text this bridge wrote into the local clipboard (from the host)
   * or sent to the host (from the local clipboard) -- suppresses the echo
   * a write can cause via `clipboardchange` (decision 4). */
  private lastText: string | null = null;
  /** A host text this bridge failed to write (no focus/permission yet),
   * retried on the next `onFocusOrGesture`. A newer host text simply
   * replaces this. */
  private pendingHostText: string | null = null;
  private deferredCopy: DeferredCopy | null = null;

  constructor(deps: ClipboardBridgeDeps) {
    this.deps = deps;
  }

  /** host -> client: the host's clipboard text changed. If a Safari
   * deferred copy is waiting (see `beginDeferredCopy`), the text feeds it
   * instead of the local clipboard directly; otherwise it's written with
   * `writeText`, falling back to `pendingHostText` on failure (decision 1). */
  onHostText(text: string): void {
    if (exceedsLimit(text)) {
      warnTooLarge(this.deps, text);
      return;
    }

    if (this.deferredCopy) {
      const { resolve, timeoutHandle } = this.deferredCopy;
      this.deps.clearTimeout(timeoutHandle);
      this.deferredCopy = null;
      resolve(new Blob([text], { type: "text/plain" }));
      this.lastText = text;
      return;
    }

    this.deps
      .writeText(text)
      .then(() => {
        this.lastText = text;
        this.pendingHostText = null;
      })
      .catch(() => {
        this.pendingHostText = text;
      });
  }

  /** `window` gained focus, or a user gesture landed on the page: retry any
   * text that failed to write earlier (decision 1c). */
  onFocusOrGesture(): void {
    const text = this.pendingHostText;
    if (text === null) return;
    this.pendingHostText = null;
    this.deps
      .writeText(text)
      .then(() => {
        this.lastText = text;
      })
      .catch(() => {
        this.pendingHostText = text;
      });
  }

  /** Safari only (decision 3): called synchronously from within the
   * Cmd/Ctrl+C or +X keydown handler (a user gesture, required for
   * `navigator.clipboard.write`). Arms a `ClipboardItem` whose blob resolves
   * once the *next* `clipboard_text` arrives from the host (the host
   * applied the forwarded key first, so its clipboard now holds the copied
   * text) -- or rejects after `DEFERRED_COPY_TIMEOUT_MS` if nothing arrives.
   * A no-op when `deps.writeDeferred` wasn't provided (i.e. not Safari). */
  beginDeferredCopy(): void {
    if (!this.deps.writeDeferred) return;

    let resolveBlob!: (blob: Blob) => void;
    let rejectBlob!: (err: Error) => void;
    const blobPromise = new Promise<Blob>((resolve, reject) => {
      resolveBlob = resolve;
      rejectBlob = reject;
    });

    const entry: DeferredCopy = {
      resolve: resolveBlob,
      reject: rejectBlob,
      timeoutHandle: this.deps.setTimeout(() => {
        if (this.deferredCopy === entry) this.deferredCopy = null;
        rejectBlob(new Error("clipboard copy timed out waiting for host"));
      }, DEFERRED_COPY_TIMEOUT_MS),
    };
    this.deferredCopy = entry;

    void this.deps.writeDeferred(blobPromise).catch(() => {
      // The write itself failing (e.g. the blob promise above rejected, or
      // Safari denied it) leaves nothing else to do -- the copy just
      // doesn't land in the system clipboard.
    });
  }

  /** Reads the local clipboard and, if it holds new, non-empty,
   * within-limit text, sends it to the host -- used both before forwarding
   * a paste shortcut (decision 2a) and on `clipboardchange` (decision 2b,
   * via `onLocalClipboardChange`). Never rejects. */
  private trySyncClipboardText(): Promise<void> {
    return this.deps.readText().then((text) => {
      if (text.length === 0) return;
      if (exceedsLimit(text)) {
        warnTooLarge(this.deps, text);
        return;
      }
      if (text === this.lastText) return;
      this.deps.send({ type: "clipboard_text", text });
      this.lastText = text;
    });
  }

  /** client -> host, before forwarding a Cmd/Ctrl+V keydown (decision 2a):
   * resolves once the local clipboard has been read and, if needed, sent to
   * the host -- or after `PASTE_READ_TIMEOUT_MS`, or on any `readText`
   * error, whichever comes first. Never rejects (the caller -- `input.ts`'s
   * key gate -- just needs to know when it's safe to flush the queued keys). */
  syncBeforePaste(): Promise<void> {
    return new Promise<void>((resolve) => {
      let settled = false;
      const finish = (): void => {
        if (settled) return;
        settled = true;
        this.deps.clearTimeout(timeoutHandle);
        resolve();
      };
      const timeoutHandle = this.deps.setTimeout(finish, PASTE_READ_TIMEOUT_MS);
      this.trySyncClipboardText()
        .catch(() => {
          // readText() rejected (e.g. no permission) -- proceed without a
          // clipboard_text, same as a timeout.
        })
        .then(finish);
    });
  }

  /** client -> host, on the browser's `clipboardchange` event (decision 2b,
   * Chrome-only -- `app.ts` only listens for it where the event exists). */
  onLocalClipboardChange(): Promise<void> {
    return this.trySyncClipboardText().catch(() => {
      // Same reasoning as `syncBeforePaste`'s catch: nothing to do.
    });
  }
}
