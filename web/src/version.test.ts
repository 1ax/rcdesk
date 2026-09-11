import { describe, expect, it } from "vitest";
import { formatClientVersion } from "./version";

describe("formatClientVersion", () => {
  it("formats the client version string", () => {
    expect(formatClientVersion("0.1.0")).toBe("rcdesk client 0.1.0");
  });
});
