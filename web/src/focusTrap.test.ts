import { describe, expect, it } from "vitest";
import { trapFocus } from "./focusTrap";

// Node test environment (no DOM): minimal stubs for what `trapFocus` touches.
interface Fake {
  hidden: boolean;
  focused: boolean;
  focus(): void;
}
function el(): Fake {
  const f: Fake = {
    hidden: false,
    focused: false,
    focus() {
      f.focused = true;
    },
  };
  return f;
}
function box(items: Fake[]): HTMLElement {
  return {
    querySelectorAll: () => items,
    contains: (x: unknown) => items.includes(x as Fake),
    focus: () => undefined,
  } as unknown as HTMLElement;
}
function tab(shiftKey: boolean): KeyboardEvent & { prevented: boolean } {
  const ev = {
    key: "Tab",
    shiftKey,
    prevented: false,
    preventDefault() {
      ev.prevented = true;
    },
  };
  return ev as unknown as KeyboardEvent & { prevented: boolean };
}
const asEl = (f: Fake): Element => f as unknown as Element;

describe("trapFocus", () => {
  it("Tab on the last element wraps to the first", () => {
    const a = el();
    const b = el();
    const ev = tab(false);
    expect(trapFocus(box([a, b]), ev, asEl(b))).toBe(true);
    expect(ev.prevented).toBe(true);
    expect(a.focused).toBe(true);
  });

  it("Shift+Tab on the first element wraps to the last", () => {
    const a = el();
    const b = el();
    expect(trapFocus(box([a, b]), tab(true), asEl(a))).toBe(true);
    expect(b.focused).toBe(true);
  });

  it("Tab in the middle is left to the browser", () => {
    const [a, m, b] = [el(), el(), el()] as [Fake, Fake, Fake];
    const ev = tab(false);
    expect(trapFocus(box([a, m, b]), ev, asEl(m))).toBe(false);
    expect(ev.prevented).toBe(false);
  });

  it("pulls focus back in when it is outside the container", () => {
    const a = el();
    const b = el();
    expect(trapFocus(box([a, b]), tab(false), null)).toBe(true);
    expect(a.focused).toBe(true);
  });

  it("skips hidden elements", () => {
    const a = el();
    const b = el();
    const c = el();
    c.hidden = true;
    expect(trapFocus(box([a, b, c]), tab(false), asEl(b))).toBe(true);
    expect(a.focused).toBe(true);
  });

  it("ignores other keys", () => {
    const ev = { key: "a", shiftKey: false, preventDefault() {} } as unknown as KeyboardEvent;
    expect(trapFocus(box([el()]), ev, null)).toBe(false);
  });
});
