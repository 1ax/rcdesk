// Pure logic backing the client's "На весь экран" control and Keyboard Lock
// hint (slice 3.5c). No DOM access here -- same reasoning as
// `displays.ts`/`sessionRecovery.ts`: `app.ts` is the only place that touches
// `document.fullscreenElement`/`requestFullscreen`/`navigator.keyboard`.
//
// Safari note: the unprefixed Fullscreen API (`document.fullscreenEnabled`,
// `Element.requestFullscreen`, `document.exitFullscreen`, `fullscreenchange`)
// has been supported by Safari, unprefixed, since Safari 16.4 (macOS Ventura
// 13.3, March 2023) -- well before Safari 26. No `webkit`-prefixed fallback
// is implemented here; see the slice 3.5c report for the full reasoning.

/** Minimal shape of the flag this module needs to decide whether the
 * fullscreen control should be shown at all -- a real `Document` satisfies
 * this structurally. */
export interface FullscreenCapability {
  fullscreenEnabled?: boolean;
}

/** Whether the fullscreen control is worth showing (`app.ts` hides
 * `#fullscreen-btn` entirely when this is false). */
export function isFullscreenSupported(doc: FullscreenCapability): boolean {
  return doc.fullscreenEnabled === true;
}

/** `#fullscreen-btn`'s label for the current state. */
export function fullscreenButtonLabel(isFullscreen: boolean): string {
  return isFullscreen ? "Выйти из полного экрана" : "На весь экран";
}

/** The subset of the (not-yet-standard-in-lib.dom) Keyboard Lock API
 * (https://wicg.github.io/keyboard-lock/) `app.ts` uses. Chrome only, as of
 * this slice -- Safari and Firefox don't implement it, so both members are
 * optional (a future/other browser could expose `navigator.keyboard` without
 * either method). */
export interface KeyboardLockApi {
  lock?: (codes?: string[]) => Promise<void>;
  unlock?: () => void;
}

/** `navigator`, extended with the optional `keyboard` property lib.dom
 * doesn't declare. */
export interface NavigatorWithKeyboard {
  keyboard?: KeyboardLockApi;
}

/** Whether `nav.keyboard.lock` exists -- feature detection, not a browser
 * name check (Chrome today, but this stays correct if that changes). */
export function supportsKeyboardLock(nav: NavigatorWithKeyboard): boolean {
  return typeof nav.keyboard?.lock === "function";
}

/** The ~3s hint shown on entering fullscreen (see `FULLSCREEN_NOTE_MS` in
 * `app.ts`): how to get back out depends on whether Keyboard Lock is active,
 * since a locked Escape keydown is forwarded to the host instead of exiting
 * fullscreen on a single press (Chrome still honors a long-press, ~2s). */
export function fullscreenHintLabel(keyboardLockActive: boolean): string {
  return keyboardLockActive
    ? "Для выхода удерживайте Esc"
    : "Для выхода нажмите Esc";
}
