import { describe, expect, it } from "vitest";
import {
  fullscreenButtonLabel,
  fullscreenHintLabel,
  isFullscreenSupported,
  supportsKeyboardLock,
} from "./fullscreen";

describe("isFullscreenSupported", () => {
  it("is true when fullscreenEnabled is true", () => {
    expect(isFullscreenSupported({ fullscreenEnabled: true })).toBe(true);
  });

  it("is false when fullscreenEnabled is false", () => {
    expect(isFullscreenSupported({ fullscreenEnabled: false })).toBe(false);
  });

  it("is false when fullscreenEnabled is missing", () => {
    expect(isFullscreenSupported({})).toBe(false);
  });
});

describe("fullscreenButtonLabel", () => {
  it("offers to enter fullscreen when not in it", () => {
    expect(fullscreenButtonLabel(false)).toBe("На весь экран");
  });

  it("offers to exit fullscreen when in it", () => {
    expect(fullscreenButtonLabel(true)).toBe("Выйти из полного экрана");
  });
});

describe("supportsKeyboardLock", () => {
  it("is true when navigator.keyboard.lock is a function", () => {
    expect(
      supportsKeyboardLock({ keyboard: { lock: () => Promise.resolve(), unlock: () => {} } }),
    ).toBe(true);
  });

  it("is false when navigator.keyboard is missing (Safari, Firefox)", () => {
    expect(supportsKeyboardLock({})).toBe(false);
  });

  it("is false when navigator.keyboard exists but has no lock", () => {
    expect(supportsKeyboardLock({ keyboard: { unlock: () => {} } })).toBe(false);
  });
});

describe("fullscreenHintLabel", () => {
  it("mentions holding Esc when Keyboard Lock is active", () => {
    expect(fullscreenHintLabel(true)).toBe("Для выхода удерживайте Esc");
  });

  it("mentions pressing Esc when Keyboard Lock is unavailable", () => {
    expect(fullscreenHintLabel(false)).toBe("Для выхода нажмите Esc");
  });
});
