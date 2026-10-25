// DOM-обвязка экранной клавиатуры телефона (слайс H2b). Логика -- в softKeyboard.ts.

import type { InputMessage } from "./generated/InputMessage";
import {
  KeyRepeat,
  SENTINEL,
  StickyModifiers,
  composeTyped,
  diffTyped,
  keystrokes,
} from "./softKeyboard";

export interface SoftKeyboardOptions {
  button: HTMLButtonElement;
  strip: HTMLDivElement;
  field: HTMLInputElement;
  send: (msg: InputMessage) => void;
  isTextSupported: () => boolean;
  /** Element whose height follows the visual viewport while the keyboard is up. */
  fit?: HTMLElement;
  /** Called first in `show()`, before anything changes. */
  onShow?: () => void;
  /** Called when the fitted height changed (keyboard animation, rotation). */
  onLayout?: () => void;
  /** Called after the keyboard is hidden (button, blur, reset). */
  onHide?: () => void;
}

export interface SoftKeyboardController {
  show(): void;
  hide(): void;
  /** Brings the keyboard back if it is closing (iOS blurs the field on a tap
   * on the video, slice H5-5). Must be called inside a user gesture (`click`),
   * or iOS ignores the focus. `true` if there was a close to cancel. */
  resume(): boolean;
  setHostOs(os: "macos" | "windows" | null): void;
  reset(): void;
  dispose(): void;
}

const LEAD_KEYS: Array<[string, string]> = [
  ["Escape", "Esc"],
  ["Tab", "Tab"],
];

const NAV_KEYS: Array<[string, string]> = [
  ["ArrowLeft", "←"],
  ["ArrowUp", "↑"],
  ["ArrowDown", "↓"],
  ["ArrowRight", "→"],
  ["Delete", "Del"],
];

const MOD_KEYS: Array<[string, string]> = [
  ["ControlLeft", "Ctrl"],
  ["AltLeft", "Alt"],
  ["MetaLeft", "Win"],
  ["ShiftLeft", "Shift"],
];

// Safety net if `visualViewport` never returns to full height (Android,
// desktop, no `visualViewport`): finish closing anyway after this delay.
const CLOSE_TIMEOUT_MS = 400;

export function mountSoftKeyboard(opts: SoftKeyboardOptions): SoftKeyboardController {
  const { button, strip, field, send, isTextSupported, fit, onShow, onLayout, onHide } = opts;
  let lastFit = "";
  const sticky = new StickyModifiers();
  const repeat = new KeyRepeat();
  let swallowClick = false;
  let prev = SENTINEL;
  let visible = false;
  let closing = false;
  let closeTimer: ReturnType<typeof setTimeout> | null = null;
  let hostOs: "macos" | "windows" | null = null;
  let metaBtn: HTMLButtonElement | null = null;
  const vv = window.visualViewport;

  const sendAll = (msgs: InputMessage[]): void => {
    for (const m of msgs) send(m);
  };

  const makeBtn = (label: string): HTMLButtonElement => {
    const b = document.createElement("button");
    b.type = "button";
    b.className = "btn btn-secondary";
    b.textContent = label;
    return b;
  };

  const addCodeKeys = (keys: Array<[string, string]>): void => {
    for (const [code, label] of keys) {
      const b = makeBtn(label);
      b.dataset.code = code;
      strip.appendChild(b);
    }
  };
  addCodeKeys(LEAD_KEYS);
  for (const [code, label] of MOD_KEYS) {
    const b = makeBtn(label);
    b.dataset.mod = code;
    if (code === "MetaLeft") metaBtn = b;
    strip.appendChild(b);
  }
  addCodeKeys(NAV_KEYS);
  const hideBtn = makeBtn("\u2304");
  hideBtn.setAttribute("aria-label", "Скрыть клавиатуру");
  hideBtn.title = "Скрыть клавиатуру";
  hideBtn.dataset.action = "hide";
  strip.appendChild(hideBtn);

  const renderMods = (): void => {
    for (const b of Array.from(strip.querySelectorAll<HTMLButtonElement>("[data-mod]"))) {
      const s = sticky.state(b.dataset.mod!);
      b.classList.toggle("is-once", s === "once");
      b.classList.toggle("is-locked", s === "locked");
    }
  };

  const updateBottom = (): void => {
    if (!vv) return;
    strip.style.bottom = `${window.innerHeight - vv.offsetTop - vv.height}px`;
    // iOS does not shrink the layout viewport for the keyboard.
    // Строка вычитается из высоты экрана сеанса, чтобы не закрывать картинку (H5-1).
    if (fit) {
      const height = `${Math.max(0, vv.height - strip.offsetHeight)}px`;
      const changed = height !== lastFit;
      fit.style.height = height;
      lastFit = height;
      if (changed) onLayout?.();
    }
    // iOS scrolls the page to the field; the session screen sits at the top of the document.
    if (window.scrollY !== 0) window.scrollTo(0, 0);
    if (closing && vv.height >= window.innerHeight - 1) finishClosing();
  };
  const startTracking = (): void => {
    if (!vv) return;
    // addEventListener with the same function is idempotent, no duplicates on resume.
    vv.addEventListener("resize", updateBottom);
    vv.addEventListener("scroll", updateBottom);
    updateBottom();
  };
  const stopTracking = (): void => {
    if (!vv) return;
    vv.removeEventListener("resize", updateBottom);
    vv.removeEventListener("scroll", updateBottom);
    strip.style.bottom = "";
    if (fit) fit.style.height = "";
    lastFit = "";
  };

  const resetField = (): void => {
    field.value = SENTINEL;
    prev = SENTINEL;
  };

  const show = (): void => {
    const resumed = closing;
    if (closeTimer !== null) {
      clearTimeout(closeTimer);
      closeTimer = null;
    }
    closing = false;
    if (!resumed) onShow?.();
    visible = true;
    resetField();
    field.focus();
    const end = field.value.length;
    try {
      field.setSelectionRange(end, end);
    } catch {
      // некоторые браузеры не дают выделение у скрытого поля
    }
    strip.hidden = false;
    startTracking();
  };

  // iOS animates the keyboard away for ~250 ms; the zoom is restored only once
  // the viewport is full again, not on blur (H5-4).
  const beginClosing = (): void => {
    if (closing) return;
    closing = true;
    visible = false;
    strip.hidden = true;
    repeat.stop();
    closeTimer = setTimeout(finishClosing, CLOSE_TIMEOUT_MS);
  };

  function finishClosing(): void {
    if (!closing) return;
    if (closeTimer !== null) {
      clearTimeout(closeTimer);
      closeTimer = null;
    }
    closing = false;
    stopTracking();
    window.scrollTo(0, 0);
    onHide?.();
  }

  const hide = (): void => {
    beginClosing();
    field.blur();
  };

  const sendEnter = (): void => {
    sendAll(keystrokes("Enter", false, sticky.consume()));
    renderMods();
  };

  const onButtonClick = (): void => {
    if (visible) hide();
    else show();
  };

  const onInput = (e: Event): void => {
    const next = field.value;
    const { backspaces, inserted } = diffTyped(prev, next);
    if (backspaces > 0 || inserted.length > 0) {
      const mods = sticky.consume();
      sendAll(composeTyped(inserted, backspaces, mods, isTextSupported()));
      renderMods();
    }
    if (!next.startsWith(SENTINEL)) {
      resetField();
    } else {
      prev = next;
    }
    const composing = (e as InputEvent).isComposing === true;
    if (!composing && (next.endsWith(" ") || next.length > 64)) resetField();
  };

  const onKeydown = (e: KeyboardEvent): void => {
    if (e.key === "Enter") {
      e.preventDefault();
      sendEnter();
    }
  };

  const onBeforeInput = (e: Event): void => {
    const t = (e as InputEvent).inputType;
    if (t === "insertLineBreak" || t === "insertParagraph") {
      e.preventDefault();
      sendEnter();
    }
  };

  const onBlur = (): void => {
    beginClosing();
  };

  // Удержание = автоповтор (H5-3), короткий тап по-прежнему уходит через click.
  const onStripPointerDown = (e: Event): void => {
    const b = (e.target as HTMLElement).closest("button");
    if (!b) return;
    e.preventDefault();
    if (b.dataset.code) {
      swallowClick = false;
      let mods: string[] | null = null;
      repeat.start(() => {
        if (mods === null) {
          mods = sticky.consume();
          renderMods();
        }
        sendAll(keystrokes(b.dataset.code!, false, mods));
      });
    }
  };

  const onStripPointerUp = (): void => {
    if (repeat.stop()) swallowClick = true;
  };

  const onStripClick = (e: Event): void => {
    if (swallowClick) {
      swallowClick = false;
      return;
    }
    const b = (e.target as HTMLElement).closest("button");
    if (!b) return;
    if (b.dataset.code) {
      sendAll(keystrokes(b.dataset.code, false, sticky.consume()));
      renderMods();
    } else if (b.dataset.mod) {
      sticky.tap(b.dataset.mod);
      renderMods();
    } else if (b.dataset.action === "hide") {
      hide();
    }
  };

  button.addEventListener("click", onButtonClick);
  field.addEventListener("input", onInput);
  field.addEventListener("keydown", onKeydown);
  field.addEventListener("beforeinput", onBeforeInput);
  field.addEventListener("blur", onBlur);
  strip.addEventListener("pointerdown", onStripPointerDown);
  strip.addEventListener("pointerup", onStripPointerUp);
  strip.addEventListener("pointercancel", onStripPointerUp);
  strip.addEventListener("pointerleave", onStripPointerUp);
  strip.addEventListener("click", onStripClick);

  return {
    show,
    hide,
    resume() {
      if (!closing) return false;
      show();
      return true;
    },
    setHostOs(os) {
      hostOs = os;
      if (metaBtn) metaBtn.textContent = hostOs === "macos" ? "Cmd" : "Win";
    },
    reset() {
      hide();
      sticky.clear();
      renderMods();
    },
    dispose() {
      button.removeEventListener("click", onButtonClick);
      field.removeEventListener("input", onInput);
      field.removeEventListener("keydown", onKeydown);
      field.removeEventListener("beforeinput", onBeforeInput);
      field.removeEventListener("blur", onBlur);
      strip.removeEventListener("pointerdown", onStripPointerDown);
      strip.removeEventListener("pointerup", onStripPointerUp);
      strip.removeEventListener("pointercancel", onStripPointerUp);
      strip.removeEventListener("pointerleave", onStripPointerUp);
      strip.removeEventListener("click", onStripClick);
      repeat.stop();
      if (closeTimer !== null) {
        clearTimeout(closeTimer);
        closeTimer = null;
      }
      closing = false;
      stopTracking();
    },
  };
}
