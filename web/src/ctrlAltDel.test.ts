import { describe, expect, it } from "vitest";
import { ctrlAltDelMessage, ctrlAltDelVisible } from "./ctrlAltDel";

describe("ctrlAltDelVisible", () => {
  it("is hidden without the flag (old host)", () => {
    expect(ctrlAltDelVisible(undefined)).toBe(false);
  });

  it("is hidden when the host says it is unavailable", () => {
    expect(ctrlAltDelVisible(false)).toBe(false);
  });

  it("is shown only when the host says it is available", () => {
    expect(ctrlAltDelVisible(true)).toBe(true);
  });

  it("is hidden again after the session resets the flag", () => {
    let available: boolean | undefined = true;
    expect(ctrlAltDelVisible(available)).toBe(true);
    available = undefined;
    expect(ctrlAltDelVisible(available)).toBe(false);
  });
});

describe("ctrlAltDelMessage", () => {
  it("serializes to exactly the wire message", () => {
    expect(JSON.stringify(ctrlAltDelMessage())).toBe('{"type":"send_ctrl_alt_del"}');
  });
});
