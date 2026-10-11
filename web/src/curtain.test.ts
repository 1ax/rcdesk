import { describe, expect, it } from "vitest";
import {
  curtainOnClick,
  curtainOnHostInfo,
  curtainOnStatus,
  curtainView,
  initialCurtainState,
} from "./curtain";

describe("curtain state", () => {
  it("is hidden until host_info says curtain_available", () => {
    const s0 = initialCurtainState();
    expect(curtainView(s0).hidden).toBe(true);
    expect(curtainView(curtainOnHostInfo(s0, false)).hidden).toBe(true);
    expect(curtainView(curtainOnHostInfo(s0, true)).hidden).toBe(false);
  });

  it("click sends the inverted value and disables the button", () => {
    const s = curtainOnHostInfo(initialCurtainState(), true);
    const first = curtainOnClick(s);
    expect(first.send).toBe(true);
    expect(curtainView(first.state).disabled).toBe(true);
    // A second click while pending sends nothing.
    expect(curtainOnClick(first.state).send).toBeNull();
    // After the host confirms "on", the next click asks for "off".
    const on = curtainOnStatus(first.state, { on: true, reason: null });
    expect(curtainOnClick(on).send).toBe(false);
  });

  it("does not send when the curtain is unavailable", () => {
    expect(curtainOnClick(initialCurtainState()).send).toBeNull();
  });

  it("curtain_status on:true presses the button and re-enables it", () => {
    const s = curtainOnClick(curtainOnHostInfo(initialCurtainState(), true)).state;
    const v = curtainView(curtainOnStatus(s, { on: true, reason: null }));
    expect(v.pressed).toBe(true);
    expect(v.disabled).toBe(false);
    expect(v.text).toBe("Шторка вкл");
    expect(v.statusText).toBeNull();
  });

  it("curtain_status on:false with a reason shows it, a later success clears it", () => {
    const base = curtainOnClick(curtainOnHostInfo(initialCurtainState(), true)).state;
    const failed = curtainOnStatus(base, { on: false, reason: "нет доступа" });
    const v = curtainView(failed);
    expect(v.pressed).toBe(false);
    expect(v.text).toBe("Шторка");
    expect(v.statusText).toContain("нет доступа");
    const ok = curtainOnStatus(curtainOnClick(failed).state, { on: true, reason: null });
    expect(curtainView(ok).statusText).toBeNull();
  });

  it("resets to hidden/off at the end of a session", () => {
    const on = curtainOnStatus(curtainOnHostInfo(initialCurtainState(), true), {
      on: true,
      reason: null,
    });
    expect(curtainView(on).pressed).toBe(true);
    const v = curtainView(initialCurtainState());
    expect(v).toEqual({
      hidden: true,
      disabled: false,
      pressed: false,
      text: "Шторка",
      statusText: null,
    });
  });
});
