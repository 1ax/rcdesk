import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import type { Mock } from "vitest";
import {
  ClipboardBridge,
  MAX_CLIPBOARD_BYTES,
  exceedsLimit,
  isCopyShortcut,
  isPasteShortcut,
  isSafari,
  utf8Length,
} from "./clipboard";
import type { ClipboardBridgeDeps } from "./clipboard";
import type { InputMessage } from "./generated/InputMessage";

describe("isSafari", () => {
  it("recognizes Safari 26 on macOS", () => {
    const ua =
      "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/26.0 Safari/605.1.15";
    expect(isSafari(ua)).toBe(true);
  });

  it("rejects Chrome 153 on macOS", () => {
    const ua =
      "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/153.0.0.0 Safari/537.36";
    expect(isSafari(ua)).toBe(false);
  });

  it("rejects Chrome on Windows", () => {
    const ua =
      "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/153.0.0.0 Safari/537.36";
    expect(isSafari(ua)).toBe(false);
  });

  it("rejects Edge", () => {
    const ua =
      "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36 Edg/120.0.0.0";
    expect(isSafari(ua)).toBe(false);
  });

  it("rejects Chrome on iOS (CriOS)", () => {
    const ua =
      "Mozilla/5.0 (iPhone; CPU iPhone OS 17_0 like Mac OS X) AppleWebKit/605.1.15 (KHTML, like Gecko) CriOS/119.0.6045.66 Mobile/15E148 Safari/604.1";
    expect(isSafari(ua)).toBe(false);
  });
});

describe("isPasteShortcut", () => {
  it("accepts Cmd+V", () => {
    expect(isPasteShortcut({ code: "KeyV", metaKey: true, ctrlKey: false, altKey: false, repeat: false })).toBe(
      true,
    );
  });

  it("accepts Ctrl+V", () => {
    expect(isPasteShortcut({ code: "KeyV", metaKey: false, ctrlKey: true, altKey: false, repeat: false })).toBe(
      true,
    );
  });

  it("rejects with Alt held", () => {
    expect(isPasteShortcut({ code: "KeyV", metaKey: true, ctrlKey: false, altKey: true, repeat: false })).toBe(
      false,
    );
  });

  it("rejects an auto-repeat", () => {
    expect(isPasteShortcut({ code: "KeyV", metaKey: true, ctrlKey: false, altKey: false, repeat: true })).toBe(
      false,
    );
  });

  it("rejects a different key", () => {
    expect(isPasteShortcut({ code: "KeyC", metaKey: true, ctrlKey: false, altKey: false, repeat: false })).toBe(
      false,
    );
  });
});

describe("isCopyShortcut", () => {
  it("accepts Cmd+C", () => {
    expect(isCopyShortcut({ code: "KeyC", metaKey: true, ctrlKey: false, altKey: false, repeat: false })).toBe(
      true,
    );
  });

  it("accepts Ctrl+X", () => {
    expect(isCopyShortcut({ code: "KeyX", metaKey: false, ctrlKey: true, altKey: false, repeat: false })).toBe(
      true,
    );
  });

  it("rejects with Alt held", () => {
    expect(isCopyShortcut({ code: "KeyC", metaKey: true, ctrlKey: false, altKey: true, repeat: false })).toBe(
      false,
    );
  });

  it("rejects an auto-repeat", () => {
    expect(isCopyShortcut({ code: "KeyC", metaKey: true, ctrlKey: false, altKey: false, repeat: true })).toBe(
      false,
    );
  });
});

describe("utf8Length / exceedsLimit", () => {
  it("counts ASCII as one byte per character", () => {
    expect(utf8Length("hello")).toBe(5);
    expect(exceedsLimit("hello")).toBe(false);
  });

  it("counts a 4-byte UTF-8 character (emoji) correctly, distinct from JS string length", () => {
    expect("😀".length).toBe(2); // UTF-16 code units
    expect(utf8Length("😀")).toBe(4);
  });

  it("does not exceed at exactly MAX_CLIPBOARD_BYTES", () => {
    const text = "a".repeat(MAX_CLIPBOARD_BYTES);
    expect(exceedsLimit(text)).toBe(false);
  });

  it("exceeds at MAX_CLIPBOARD_BYTES + 1", () => {
    const text = "a".repeat(MAX_CLIPBOARD_BYTES + 1);
    expect(exceedsLimit(text)).toBe(true);
  });
});

/** Lets any already-queued microtasks (e.g. a rejected `writeText()`'s
 * `.catch` handler inside `onHostText`) run before the test continues --
 * `await`ing a macrotask guarantees the microtask queue drained first. */
function flushMicrotasks(): Promise<void> {
  return new Promise((resolve) => setTimeout(resolve, 0));
}

/** `ClipboardBridgeDeps`, but with every field a test might assert on typed
 * as a mock -- `makeDeps`'s return type. Keeping these typed as
 * `ReturnType<typeof vi.fn>` from the start (rather than widening through a
 * generic `Partial<ClipboardBridgeDeps>` override) is what lets a test both
 * override a field with its own `vi.fn()` and assert on it afterwards. */
interface MockClipboardDeps extends ClipboardBridgeDeps {
  writeText: Mock<(text: string) => Promise<void>>;
  readText: Mock<() => Promise<string>>;
  writeDeferred?: Mock<(blob: Promise<Blob>) => Promise<void>>;
  send: Mock<(msg: InputMessage) => void>;
  notify: Mock<(note: string) => void>;
}

/** Builds a `ClipboardBridgeDeps` with controllable mocks, defaulting
 * `setTimeout`/`clearTimeout` to the (possibly faked, see individual tests)
 * globals -- matches how `app.ts` wires the real dependencies. */
function makeDeps(overrides: Partial<MockClipboardDeps> = {}): MockClipboardDeps {
  return {
    writeText: vi.fn().mockResolvedValue(undefined),
    readText: vi.fn().mockResolvedValue(""),
    send: vi.fn(),
    notify: vi.fn(),
    setTimeout: (handler: () => void, ms: number) => setTimeout(handler, ms),
    clearTimeout: (handle) => clearTimeout(handle),
    ...overrides,
  };
}

describe("ClipboardBridge.onHostText", () => {
  it("writes the text to the local clipboard on success", async () => {
    const deps = makeDeps();
    const bridge = new ClipboardBridge(deps);

    bridge.onHostText("hello from host");
    await vi.waitFor(() => expect(deps.writeText).toHaveBeenCalledWith("hello from host"));
  });

  it("falls back to pending and retries on the next focus/gesture when writeText fails", async () => {
    const writeText = vi.fn().mockRejectedValueOnce(new Error("NotAllowedError")).mockResolvedValue(undefined);
    const deps = makeDeps({ writeText });
    const bridge = new ClipboardBridge(deps);

    bridge.onHostText("host text");
    await flushMicrotasks();
    expect(writeText).toHaveBeenCalledTimes(1);

    bridge.onFocusOrGesture();
    await flushMicrotasks();
    expect(writeText).toHaveBeenCalledTimes(2);
    expect(writeText).toHaveBeenLastCalledWith("host text");
  });

  it("a new pending host text replaces an older, not-yet-flushed one", async () => {
    const writeText = vi
      .fn()
      .mockRejectedValueOnce(new Error("no focus"))
      .mockRejectedValueOnce(new Error("no focus"))
      .mockResolvedValue(undefined);
    const deps = makeDeps({ writeText });
    const bridge = new ClipboardBridge(deps);

    bridge.onHostText("first");
    await flushMicrotasks();
    expect(writeText).toHaveBeenCalledTimes(1);
    bridge.onHostText("second");
    await flushMicrotasks();
    expect(writeText).toHaveBeenCalledTimes(2);

    bridge.onFocusOrGesture();
    await flushMicrotasks();
    expect(writeText).toHaveBeenCalledTimes(3);
    expect(writeText).toHaveBeenLastCalledWith("second");
  });

  it("drops text over the limit without writing, and shows a note", () => {
    const deps = makeDeps();
    const bridge = new ClipboardBridge(deps);
    const warnSpy = vi.spyOn(console, "warn").mockImplementation(() => {});

    const big = "a".repeat(MAX_CLIPBOARD_BYTES + 1);
    bridge.onHostText(big);

    expect(deps.writeText).not.toHaveBeenCalled();
    expect(deps.notify).toHaveBeenCalledWith(expect.stringContaining("слишком большой"));
    expect(warnSpy.mock.calls[0]?.join(" ")).not.toContain(big);
    warnSpy.mockRestore();
  });
});

describe("ClipboardBridge deferred Safari copy", () => {
  beforeEach(() => {
    vi.useFakeTimers();
  });

  afterEach(() => {
    vi.useRealTimers();
  });

  it("feeds the next host text to the deferred blob instead of calling writeText", async () => {
    let capturedBlobPromise: Promise<Blob> | null = null;
    const writeDeferred = vi.fn().mockImplementation((blob: Promise<Blob>) => {
      capturedBlobPromise = blob;
      return Promise.resolve();
    });
    const deps = makeDeps({ writeDeferred });
    const bridge = new ClipboardBridge(deps);

    bridge.beginDeferredCopy();
    expect(writeDeferred).toHaveBeenCalledTimes(1);

    bridge.onHostText("copied text");
    expect(deps.writeText).not.toHaveBeenCalled();

    expect(capturedBlobPromise).not.toBeNull();
    const blob = await capturedBlobPromise!;
    const text = await blob.text();
    expect(text).toBe("copied text");
  });

  it("rejects the deferred blob after 3s when no host text arrives", async () => {
    let capturedBlobPromise: Promise<Blob> | null = null;
    const writeDeferred = vi.fn().mockImplementation((blob: Promise<Blob>) => {
      capturedBlobPromise = blob;
      return Promise.resolve();
    });
    const deps = makeDeps({ writeDeferred });
    const bridge = new ClipboardBridge(deps);

    bridge.beginDeferredCopy();
    const assertion = expect(capturedBlobPromise).rejects.toThrow();

    await vi.advanceTimersByTimeAsync(3000);
    await assertion;
  });

  it("is a no-op when writeDeferred isn't provided (non-Safari)", () => {
    const deps = makeDeps();
    const bridge = new ClipboardBridge(deps);

    expect(() => bridge.beginDeferredCopy()).not.toThrow();
    bridge.onHostText("plain host text");
    // Falls through to the normal writeText path since no deferred copy was armed.
    expect(deps.writeText).toHaveBeenCalledWith("plain host text");
  });
});

describe("ClipboardBridge.syncBeforePaste", () => {
  it("sends the local clipboard text when it differs from lastText", async () => {
    const deps = makeDeps({ readText: vi.fn().mockResolvedValue("clipboard contents") });
    const bridge = new ClipboardBridge(deps);

    await bridge.syncBeforePaste();

    expect(deps.send).toHaveBeenCalledWith({ type: "clipboard_text", text: "clipboard contents" } satisfies InputMessage);
  });

  it("does not send when the text matches lastText (echo after a host write)", async () => {
    const deps = makeDeps({ readText: vi.fn().mockResolvedValue("same text") });
    const bridge = new ClipboardBridge(deps);

    bridge.onHostText("same text");
    await vi.waitFor(() => expect(deps.writeText).toHaveBeenCalled());

    await bridge.syncBeforePaste();

    expect(deps.send).not.toHaveBeenCalled();
  });

  it("does not send an empty clipboard", async () => {
    const deps = makeDeps({ readText: vi.fn().mockResolvedValue("") });
    const bridge = new ClipboardBridge(deps);

    await bridge.syncBeforePaste();

    expect(deps.send).not.toHaveBeenCalled();
  });

  it("does not send text over the limit, and shows a note", async () => {
    const big = "a".repeat(MAX_CLIPBOARD_BYTES + 1);
    const deps = makeDeps({ readText: vi.fn().mockResolvedValue(big) });
    const bridge = new ClipboardBridge(deps);
    const warnSpy = vi.spyOn(console, "warn").mockImplementation(() => {});

    await bridge.syncBeforePaste();

    expect(deps.send).not.toHaveBeenCalled();
    expect(deps.notify).toHaveBeenCalledWith(expect.stringContaining("слишком большой"));
    warnSpy.mockRestore();
  });

  it("resolves without sending when readText rejects", async () => {
    const deps = makeDeps({ readText: vi.fn().mockRejectedValue(new Error("denied")) });
    const bridge = new ClipboardBridge(deps);

    await expect(bridge.syncBeforePaste()).resolves.toBeUndefined();
    expect(deps.send).not.toHaveBeenCalled();
  });

  it("resolves after a 10s timeout when readText never settles", async () => {
    vi.useFakeTimers();
    try {
      const readText = vi.fn().mockReturnValue(new Promise<string>(() => {}));
      const deps = makeDeps({ readText });
      const bridge = new ClipboardBridge(deps);

      const assertion = expect(bridge.syncBeforePaste()).resolves.toBeUndefined();
      await vi.advanceTimersByTimeAsync(10_000);
      await assertion;
      expect(deps.send).not.toHaveBeenCalled();
    } finally {
      vi.useRealTimers();
    }
  });
});

describe("ClipboardBridge.onLocalClipboardChange", () => {
  it("sends new clipboard text, same checks as syncBeforePaste", async () => {
    const deps = makeDeps({ readText: vi.fn().mockResolvedValue("changed") });
    const bridge = new ClipboardBridge(deps);

    await bridge.onLocalClipboardChange();

    expect(deps.send).toHaveBeenCalledWith({ type: "clipboard_text", text: "changed" } satisfies InputMessage);
  });

  it("does not send when it matches lastText", async () => {
    const deps = makeDeps({ readText: vi.fn().mockResolvedValue("unchanged") });
    const bridge = new ClipboardBridge(deps);

    deps.send.mockClear();
    // Prime lastText via a first successful send.
    await bridge.onLocalClipboardChange();
    expect(deps.send).toHaveBeenCalledTimes(1);

    await bridge.onLocalClipboardChange();
    expect(deps.send).toHaveBeenCalledTimes(1);
  });

  it("resolves without throwing when readText rejects", async () => {
    const deps = makeDeps({ readText: vi.fn().mockRejectedValue(new Error("denied")) });
    const bridge = new ClipboardBridge(deps);

    await expect(bridge.onLocalClipboardChange()).resolves.toBeUndefined();
  });
});
