import { describe, expect, it } from "vitest";
import { hostOutdatedLabel, updateStatusLabel } from "./hostVersion";

describe("hostOutdatedLabel", () => {
  it("warns when the host is older", () => {
    expect(hostOutdatedLabel("0.1.0", "0.2.0")).toBe(
      "На хосте rcdesk 0.1.0 — доступна 0.2.0",
    );
  });
  it("is null when equal or newer", () => {
    expect(hostOutdatedLabel("0.2.0", "0.2.0")).toBeNull();
    expect(hostOutdatedLabel("0.3.0", "0.2.0")).toBeNull();
  });
  it("compares numerically, not lexically", () => {
    expect(hostOutdatedLabel("0.1.10", "0.1.9")).toBeNull();
    expect(hostOutdatedLabel("0.1.9", "0.1.10")).not.toBeNull();
  });
  it("treats a missing field as an old host", () => {
    const text = "На хосте старая версия rcdesk — доступна 0.2.0";
    expect(hostOutdatedLabel(null, "0.2.0")).toBe(text);
    expect(hostOutdatedLabel(undefined, "0.2.0")).toBe(text);
  });
  it("does not warn about a field-less host when the client is 0.1.0 too", () => {
    expect(hostOutdatedLabel(null, "0.1.0")).toBeNull();
    expect(hostOutdatedLabel(undefined, "0.1.0")).toBeNull();
  });
  it("is null on garbage", () => {
    expect(hostOutdatedLabel("abc", "0.2.0")).toBeNull();
    expect(hostOutdatedLabel("0.1", "0.2.0")).toBeNull();
    expect(hostOutdatedLabel("0.1.0", "garbage")).toBeNull();
    expect(hostOutdatedLabel(null, "garbage")).toBeNull();
  });
  it("accepts a v prefix", () => {
    expect(hostOutdatedLabel("v0.1.0", "v0.2.0")).toBe(
      "На хосте rcdesk v0.1.0 — доступна v0.2.0",
    );
    expect(hostOutdatedLabel("v0.2.0", "0.2.0")).toBeNull();
  });
});

describe("updateStatusLabel", () => {
  const base = { version: null, message: null };
  it("checking", () => {
    expect(updateStatusLabel({ ...base, state: "checking" }, null)).toBe("Проверяю обновление…");
  });
  it("downloading", () => {
    expect(updateStatusLabel({ ...base, state: "downloading", version: "0.2.0" }, "windows")).toBe(
      "Скачиваю rcdesk 0.2.0…",
    );
  });
  it("downloading with progress and total", () => {
    expect(
      updateStatusLabel(
        { ...base, state: "downloading", version: "0.3.3", received: 4_404_019, total: 13_958_644 },
        null,
      ),
    ).toBe("Скачиваю rcdesk 0.3.3… 4,2 из 13,3 МБ (31%)");
  });
  it("downloading with progress, percent capped at 100", () => {
    expect(
      updateStatusLabel(
        { ...base, state: "downloading", version: "0.3.3", received: 2_000_000, total: 1_000_000 },
        null,
      ),
    ).toBe("Скачиваю rcdesk 0.3.3… 1,9 из 1,0 МБ (100%)");
  });
  it("downloading with progress without total", () => {
    expect(
      updateStatusLabel(
        { ...base, state: "downloading", version: "0.3.3", received: 4_404_019, total: null },
        null,
      ),
    ).toBe("Скачиваю rcdesk 0.3.3… 4,2 МБ");
  });
  it("installing on windows", () => {
    expect(updateStatusLabel({ ...base, state: "installing", version: "0.2.0" }, "windows")).toBe(
      "Устанавливаю rcdesk 0.2.0 — сеанс прервётся",
    );
  });
  it("installing on macos mentions the admin password", () => {
    expect(updateStatusLabel({ ...base, state: "installing", version: "0.2.0" }, "macos")).toBe(
      "Устанавливаю rcdesk 0.2.0 — сеанс прервётся (на хосте нужен пароль администратора)",
    );
  });
  it("up_to_date", () => {
    expect(updateStatusLabel({ ...base, state: "up_to_date", version: "0.2.0" }, null)).toBe(
      "На хосте уже последняя версия",
    );
  });
  it("failed with and without message", () => {
    expect(updateStatusLabel({ ...base, state: "failed", message: "нет сети" }, null)).toBe(
      "Не удалось обновить: нет сети",
    );
    expect(updateStatusLabel({ ...base, state: "failed" }, null)).toBe("Не удалось обновить хост");
  });
});
