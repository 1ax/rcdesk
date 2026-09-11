import { describe, expect, it } from "vitest";
import { cursorCss, decodeRgba } from "./cursor";

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
