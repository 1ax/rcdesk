import { describe, expect, it } from "vitest";
import { lanHubUi } from "./lanHub";

describe("lanHubUi", () => {
  it("hides account UI and skips account requests in LAN-hub mode", () => {
    expect(lanHubUi(true)).toEqual({
      showAccountUi: false,
      showDownload: false,
      requestAccountData: false,
    });
  });

  it("keeps everything for a regular server", () => {
    expect(lanHubUi(false)).toEqual({
      showAccountUi: true,
      showDownload: true,
      requestAccountData: true,
    });
  });

  it("treats a missing field (old server) as a regular server", () => {
    expect(lanHubUi(undefined)).toEqual(lanHubUi(false));
  });
});
