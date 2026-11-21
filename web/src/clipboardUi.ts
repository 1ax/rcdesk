/**
 * Clipboard sync UI inside a session (slices 2.5c, E4): the opt-in checkbox,
 * the phone "send clipboard" button, the `#clipboard-note` line and the
 * lifecycle of the `ClipboardBridge` (`clipboard.ts`) with its listeners. A
 * feature factory split out of `app.ts`'s `mount()`: it finds its own DOM in
 * `root`, keeps its state in the closure and talks to the rest of `mount()`
 * only through `ClipboardUiDeps` and the returned `ClipboardUi`.
 */
import { ClipboardBridge, isSafari } from "./clipboard";

/** How long `#clipboard-note` stays visible after `ClipboardBridge.notify`
 * reports a too-large clipboard text (slice 2.5c, decision 5). */
const CLIPBOARD_NOTE_MS = 4000;

export interface ClipboardUiDeps {
  /** The `mount()` root; the UI looks its own elements up here. */
  root: Element;
  /** The persisted clipboard-sync setting (off by default). */
  initialSyncEnabled: boolean;
  /** The phone soft keyboard is open (a return tap must not trigger "Paste"). */
  isSoftKeyboardOpen: () => boolean;
  /** The current `input` data channel, if any. */
  getInputChannel: () => RTCDataChannel | null;
  /** There is a live session (a toggle mid-session attaches/detaches the bridge). */
  isSessionActive: () => boolean;
  /** Persists the clipboard-sync setting for this browser. */
  saveSyncSetting: (enabled: boolean) => void;
  /** Runs after the checkbox was toggled (gives focus back to the video). */
  afterToggle: () => void;
}

export interface ClipboardUi {
  /** Clipboard sync is enabled (checkbox state). */
  isSyncEnabled: () => boolean;
  /** Creates the bridge and its listeners (no-op if already attached). */
  attach: () => void;
  /** Removes the bridge and its listeners. */
  detach: () => void;
  /** Shows `note` in `#clipboard-note` for a few seconds. */
  showNote: (note: string) => void;
  /** Cancels the note timer and hides `#clipboard-note` (session end). */
  resetNote: () => void;
  /** Files on the host clipboard replaced the last synced text. */
  forgetLastText: () => void;
  /** `clipboard_text` arrived from the host. */
  onHostText: (text: string) => void;
  /** Before a paste: syncs the client clipboard to the host first. */
  syncBeforePaste: () => Promise<void>;
  /** A copy shortcut was pressed: starts the deferred copy (Safari). */
  beginDeferredCopy: () => void;
}

export function createClipboardUi(deps: ClipboardUiDeps): ClipboardUi {
  const { root } = deps;

  const clipboardNoteEl = root.querySelector<HTMLSpanElement>("#clipboard-note")!;
  // Slice E4: clipboard sync is opt-in (off by default), remembered per browser.
  const clipboardSyncCheckbox = root.querySelector<HTMLInputElement>("#clipboard-sync-checkbox")!;
  let clipboardSyncEnabled = deps.initialSyncEnabled;
  clipboardSyncCheckbox.checked = clipboardSyncEnabled;
  const coarsePointer = matchMedia("(pointer: coarse)").matches;
  const clipboardPushBtn = root.querySelector<HTMLButtonElement>("#clipboard-push-btn")!;
  function renderClipboardPushBtn(): void {
    clipboardPushBtn.hidden = !(coarsePointer && clipboardSyncEnabled);
  }
  renderClipboardPushBtn();
  clipboardPushBtn.addEventListener("click", () => {
    void clipboardBridge?.syncFromClient();
  });
  // A phone cannot report clipboard changes, so after returning to the tab
  // (or at session start) the first click sends the phone clipboard to the
  // host; iOS asks "Paste" (H5-9).
  let returnSyncArmed = false;
  function onClipboardReturnClick(e: Event): void {
    if (!returnSyncArmed) return;
    // H5-11: only a tap on the remote screen itself, with the phone keyboard
    // closed -- a tap on the key strip or any button would get iOS's "Paste"
    // prompt in the way of the keyboard.
    const t = e.target as HTMLElement | null;
    if (!t || !t.closest(".session-video") || deps.isSoftKeyboardOpen()) return;
    returnSyncArmed = false;
    void clipboardBridge?.syncFromClient();
  }
  document.addEventListener("visibilitychange", () => {
    if (document.visibilityState === "visible" && coarsePointer && clipboardBridge !== null) {
      returnSyncArmed = true;
    }
  });
  // Mirrors the clipboard with the host for this session (slice 2.5c), or
  // `null` when there's no session or `navigator.clipboard` isn't available
  // (e.g. an insecure context) -- see the `joined` handler and `teardown`.
  let clipboardBridge: ClipboardBridge | null = null;
  // Whether `onClipboardChange` is currently registered on
  // `navigator.clipboard` (only where `clipboardchange` exists -- Chrome),
  // so `teardown` knows whether to remove it.
  let clipboardChangeAttached = false;
  let clipboardNoteTimer: ReturnType<typeof setTimeout> | undefined;

  /** Shows `note` in `#clipboard-note` for `CLIPBOARD_NOTE_MS`, used as
   * `ClipboardBridge`'s `notify` dependency (decision 5: a too-large
   * clipboard text, in either direction). */
  function showClipboardNote(note: string): void {
    clipboardNoteEl.textContent = note;
    clipboardNoteEl.hidden = false;
    if (clipboardNoteTimer !== undefined) clearTimeout(clipboardNoteTimer);
    clipboardNoteTimer = setTimeout(() => {
      clipboardNoteEl.hidden = true;
      clipboardNoteEl.textContent = "";
      clipboardNoteTimer = undefined;
    }, CLIPBOARD_NOTE_MS);
  }

  /** Creates the `ClipboardBridge` and its listeners (slice E4: only when
   * the user enabled clipboard sync; also called on toggle mid-session). */
  function attachClipboardBridge(): void {
    if (clipboardBridge) return;
    if (navigator.clipboard) {
      const clipboard = navigator.clipboard;
      clipboardBridge = new ClipboardBridge({
        writeText: (text) => clipboard.writeText(text),
        readText: () => clipboard.readText(),
        // The deferred `ClipboardItem` write (decision 3) is a
        // Safari-only trick -- Chrome already gets the same result from
        // `writeText` in `onHostText` (decision 1b).
        writeDeferred: isSafari(navigator.userAgent)
          ? (blob) => clipboard.write([new ClipboardItem({ "text/plain": blob })])
          : undefined,
        send: (msg) => {
          const inputChannel = deps.getInputChannel();
          if (inputChannel?.readyState === "open") {
            inputChannel.send(JSON.stringify(msg));
          }
        },
        notify: (note) => showClipboardNote(note),
        setTimeout: (handler, ms) => setTimeout(handler, ms),
        clearTimeout: (handle) => clearTimeout(handle),
      });
      // H5-16: armed only after coming back from another app (see the
      // `visibilitychange` listener), not at session start -- otherwise the very
      // first tap on the remote screen pops iOS's "Paste" prompt every session.
      returnSyncArmed = false;
      document.addEventListener("click", onClipboardReturnClick, true);
      window.addEventListener("focus", onClipboardFocusOrGesture);
      document.addEventListener("pointerdown", onClipboardFocusOrGesture, true);
      // H5-17: a touch `pointerdown` carries no user activation on iOS
      // (`writeText` -> NotAllowedError; measured in the simulator), `click` does
      // -- without it the host's clipboard never reached the phone.
      document.addEventListener("click", onClipboardFocusOrGesture, true);
      document.addEventListener("keydown", onClipboardFocusOrGesture, true);
      if ("onclipboardchange" in clipboard) {
        clipboard.addEventListener("clipboardchange", onClipboardChange);
        clipboardChangeAttached = true;
      }
    } else {
      console.warn("navigator.clipboard unavailable; clipboard sync disabled");
    }
  }

  /** Removes the bridge and its listeners; with no bridge every
   * `clipboardBridge?.…` call is a no-op (slice E4). */
  function detachClipboardBridge(): void {
    if (clipboardBridge) {
      document.removeEventListener("click", onClipboardReturnClick, true);
      returnSyncArmed = false;
      window.removeEventListener("focus", onClipboardFocusOrGesture);
      document.removeEventListener("pointerdown", onClipboardFocusOrGesture, true);
      document.removeEventListener("click", onClipboardFocusOrGesture, true);
      document.removeEventListener("keydown", onClipboardFocusOrGesture, true);
      if (clipboardChangeAttached) {
        navigator.clipboard.removeEventListener("clipboardchange", onClipboardChange);
        clipboardChangeAttached = false;
      }
    }
    clipboardBridge = null;
  }

  function onClipboardFocusOrGesture(): void {
    clipboardBridge?.onFocusOrGesture();
  }

  function onClipboardChange(): void {
    void clipboardBridge?.onLocalClipboardChange();
  }

  clipboardSyncCheckbox.addEventListener("change", () => {
    clipboardSyncEnabled = clipboardSyncCheckbox.checked;
    renderClipboardPushBtn();
    deps.saveSyncSetting(clipboardSyncEnabled);
    if (deps.isSessionActive()) {
      if (clipboardSyncEnabled) attachClipboardBridge();
      else detachClipboardBridge();
    }
    deps.afterToggle();
  });

  return {
    isSyncEnabled: () => clipboardSyncEnabled,
    attach: attachClipboardBridge,
    detach: detachClipboardBridge,
    showNote: showClipboardNote,
    resetNote: () => {
      if (clipboardNoteTimer !== undefined) {
        clearTimeout(clipboardNoteTimer);
        clipboardNoteTimer = undefined;
      }
      clipboardNoteEl.hidden = true;
      clipboardNoteEl.textContent = "";
    },
    forgetLastText: () => clipboardBridge?.forgetLastText(),
    onHostText: (text) => clipboardBridge?.onHostText(text),
    syncBeforePaste: () => clipboardBridge?.syncBeforePaste() ?? Promise.resolve(),
    beginDeferredCopy: () => clipboardBridge?.beginDeferredCopy(),
  };
}
