import { describe, expect, it } from "vitest";
import { vi } from "vitest";
import { applyCursor, cursorCss, decodeRgba, isValidCursorShape } from "./cursor";

describe("decodeRgba", () => {
  it("decodes a base64 string to its raw bytes", () => {
    // "QUJD" is the base64 encoding of the ASCII bytes for "ABC" (65, 66, 67).
    expect(Array.from(decodeRgba("QUJD"))).toEqual([65, 66, 67]);
  });

  it("decodes an empty string to an empty array", () => {
    expect(decodeRgba("").length).toBe(0);
  });

  it("round-trips bytes that don't decode as valid UTF-8 text", () => {
    // 0xFF 0xEC 0x20 0x55 0x00, base64-encoded ("/+wgVQA=") -- includes a
    // high byte and a null byte, both of which would be mangled by a naive
    // string-based decode.
    expect(Array.from(decodeRgba("/+wgVQA="))).toEqual([0xff, 0xec, 0x20, 0x55, 0x00]);
  });
});

describe("cursorCss", () => {
  it("builds a plain url() cursor at scale 1", () => {
    expect(cursorCss("data:image/png;base64,AAAA", 5, 5, 1, true)).toBe(
      'url("data:image/png;base64,AAAA") 5 5, auto',
    );
  });

  it("builds an image-set() cursor at scale 2 when supported", () => {
    expect(cursorCss("data:image/png;base64,AAAA", 10, 12, 2, true)).toBe(
      'image-set(url("data:image/png;base64,AAAA") 2x) 10 12, auto',
    );
  });

  it("falls back to plain url() at scale 2 when image-set() isn't supported", () => {
    expect(cursorCss("data:image/png;base64,AAAA", 10, 12, 2, false)).toBe(
      'url("data:image/png;base64,AAAA") 10 12, auto',
    );
  });
});

describe("W-11 isValidCursorShape", () => {
  const ok = { width: 32, height: 32, hotspot_x: 4, hotspot_y: 4, scale: 2 };
  it("accepts a sane shape", () => {
    expect(isValidCursorShape(ok)).toBe(true);
  });
  it.each([
    { width: 0 },
    { width: 257 },
    { height: 1.5 },
    { scale: 0 },
    { scale: 5 },
    { scale: NaN },
    { hotspot_x: -1 },
    { hotspot_x: 17 }, // > width / scale = 16
    { hotspot_y: Infinity },
  ])("rejects %j", (patch) => {
    expect(isValidCursorShape({ ...ok, ...patch })).toBe(false);
  });
});

describe("W-11 applyCursor rejects invalid shapes", () => {
  const msg = (rgba: string, width = 1, height = 1) =>
    ({
      type: "cursor_shape",
      width,
      height,
      hotspot_x: 0,
      hotspot_y: 0,
      scale: 1,
      rgba,
    }) as never;

  it("ignores a wrong rgba length and warns once per shape", () => {
    const warn = vi.spyOn(console, "warn").mockImplementation(() => {});
    const video = { style: { cursor: "keep" } } as unknown as HTMLElement;
    applyCursor(video, msg("QUJD")); // 3 bytes, needs 4
    applyCursor(video, msg("QUJD"));
    expect(video.style.cursor).toBe("keep");
    expect(warn).toHaveBeenCalledTimes(1);
    warn.mockRestore();
  });

  it("ignores out-of-range dimensions", () => {
    const warn = vi.spyOn(console, "warn").mockImplementation(() => {});
    const video = { style: { cursor: "keep" } } as unknown as HTMLElement;
    applyCursor(video, msg("AAAA", 0, 1));
    expect(video.style.cursor).toBe("keep");
    expect(warn).toHaveBeenCalledTimes(1);
    warn.mockRestore();
  });
});
