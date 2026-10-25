// Чистая логика экранной клавиатуры телефона (слайс H2b): диф поля ввода,
// «липкие» модификаторы, сборка сообщений протокола. Без DOM.

import { charToKey } from "./charKeys";
import type { InputMessage } from "./generated/InputMessage";

/** Страж в начале скрытого поля: Backspace на «пустом» поле виден как удаление. */
export const SENTINEL = " ";

export function diffTyped(prev: string, next: string): { backspaces: number; inserted: string } {
  const a = Array.from(prev);
  const b = Array.from(next);
  let i = 0;
  while (i < a.length && i < b.length && a[i] === b[i]) i++;
  return { backspaces: a.length - i, inserted: b.slice(i).join("") };
}

export type StickyState = "off" | "once" | "locked";

export class StickyModifiers {
  private states = new Map<string, StickyState>();

  tap(code: string): void {
    const s = this.state(code);
    const next: StickyState = s === "off" ? "once" : s === "once" ? "locked" : "off";
    if (next === "off") this.states.delete(code);
    else this.states.set(code, next);
  }

  state(code: string): StickyState {
    return this.states.get(code) ?? "off";
  }

  /** Активные коды; `once` после вызова гаснут, `locked` остаются. */
  consume(): string[] {
    const active = Array.from(this.states.keys());
    for (const [code, s] of Array.from(this.states)) {
      if (s === "once") this.states.delete(code);
    }
    return active;
  }

  clear(): void {
    this.states.clear();
  }
}

/** Хост понимает `text` с версии 0.1.8. */
export function hostSupportsText(version: string | null | undefined): boolean {
  if (version === null || version === undefined) return false;
  const m = /^v?(\d+)\.(\d+)\.(\d+)$/.exec(version);
  if (!m) return false;
  const v = [Number(m[1]), Number(m[2]), Number(m[3])];
  const min = [0, 1, 8];
  for (let i = 0; i < 3; i++) {
    if (v[i] !== min[i]) return v[i]! > min[i]!;
  }
  return true;
}

export function keystrokes(code: string, shift: boolean, mods: readonly string[]): InputMessage[] {
  const out: InputMessage[] = [];
  for (const m of mods) out.push({ type: "key", code: m, pressed: true });
  const addShift = shift && !mods.includes("ShiftLeft");
  if (addShift) out.push({ type: "key", code: "ShiftLeft", pressed: true });
  out.push({ type: "key", code, pressed: true });
  out.push({ type: "key", code, pressed: false });
  if (addShift) out.push({ type: "key", code: "ShiftLeft", pressed: false });
  for (const m of [...mods].reverse()) out.push({ type: "key", code: m, pressed: false });
  return out;
}

export function composeTyped(
  inserted: string,
  backspaces: number,
  mods: readonly string[],
  textSupported: boolean,
): InputMessage[] {
  const out: InputMessage[] = [];
  for (let i = 0; i < backspaces; i++) out.push(...keystrokes("Backspace", false, []));
  const viaKeys = (m: readonly string[]): void => {
    for (const ch of Array.from(inserted)) {
      const k = charToKey(ch);
      if (k) out.push(...keystrokes(k.code, k.shift, m));
    }
  };
  if (mods.length > 0) {
    viaKeys(mods);
  } else if (textSupported && inserted.length > 0) {
    out.push({ type: "text", text: inserted });
  } else {
    viaKeys([]);
  }
  return out;
}

export const REPEAT_DELAY_MS = 500;
export const REPEAT_INTERVAL_MS = 50;

/** Autorepeat for a held strip key (slice H5-3): `fire` runs once after
 * `REPEAT_DELAY_MS`, then every `REPEAT_INTERVAL_MS`, until `stop()`.
 * `stop()` reports whether anything fired -- the release's `click` must then
 * be swallowed, or the key would go out one extra time. */
export class KeyRepeat {
  private delay: ReturnType<typeof setTimeout> | null = null;
  private timer: ReturnType<typeof setInterval> | null = null;
  private fired = false;

  start(fire: () => void): void {
    this.stop();
    this.delay = setTimeout(() => {
      this.delay = null;
      this.fired = true;
      fire();
      this.timer = setInterval(fire, REPEAT_INTERVAL_MS);
    }, REPEAT_DELAY_MS);
  }

  /** Cancels the repeat; `true` if at least one repeat fired. */
  stop(): boolean {
    if (this.delay !== null) clearTimeout(this.delay);
    if (this.timer !== null) clearInterval(this.timer);
    this.delay = null;
    this.timer = null;
    const fired = this.fired;
    this.fired = false;
    return fired;
  }
}
