import { describe, expect, it } from "vitest";
import { mikrotikScript, openwrtScript } from "./routerScripts";

describe.each([
  ["mikrotik", mikrotikScript],
  ["openwrt", openwrtScript],
])("%s script", (_label, gen) => {
  it("substitutes origin, token and name", () => {
    const s = gen("https://rcdesk.app", "tok123", "Дом");
    expect(s).toContain('"https://rcdesk.app/wake/poll"');
    expect(s).toContain("Authorization: Bearer tok123");
    expect(s).toContain("роутер-будильник «Дом»");
  });

  it("leaves no placeholders", () => {
    const s = gen("https://rcdesk.app", "tok123", "Дом");
    expect(s).not.toContain("{{");
    expect(s).not.toContain("ORIGIN");
    expect(s).not.toContain("TOKEN");
    expect(s).not.toContain("NAME");
  });

  it("strips trailing slashes from origin", () => {
    const s = gen("https://rcdesk.app/", "t", "n");
    expect(s).toContain("https://rcdesk.app/wake/poll");
    expect(s).not.toContain("app//wake");
  });

  it("neutralises dangerous characters in the name", () => {
    const s = gen("https://x", "t", 'a"b\\c$d{e}f;g\nh');
    expect(s).toContain("«a b c d e f g h»");
  });

  it("does not re-substitute placeholders inside values", () => {
    const s = gen("https://x", "TOKEN-NAME", "ORIGIN");
    expect(s).toContain("Bearer TOKEN-NAME");
    expect(s).toContain("«ORIGIN»");
  });
});
