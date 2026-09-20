import { describe, expect, it } from "vitest";
import { formatBuildBadge } from "./app";

describe("formatBuildBadge", () => {
  it("formats a short commit sha", () => {
    expect(formatBuildBadge("abc1234")).toBe("build abc1234");
  });

  it("formats the local dev placeholder", () => {
    expect(formatBuildBadge("dev")).toBe("build dev");
  });
});
