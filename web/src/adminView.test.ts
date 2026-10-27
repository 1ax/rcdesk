import { describe, expect, it } from "vitest";
import {
  budgetShare,
  chartBars,
  endReasonLabel,
  formatBytes,
  formatDuration,
  formatHoursMinutes,
  formatRelative,
  parseUserAgent,
  sessionBytes,
  sessionDurationLabel,
  sessionDurationSecs,
} from "./adminView";
import type { AdminSession } from "./generated/AdminSession";

function session(overrides: Partial<AdminSession> = {}): AdminSession {
  return {
    session_id: "s1",
    device_id: "d1",
    device_name: "Mac",
    owner_id: null,
    email: null,
    via: "pin",
    client_ua: "",
    started_at: 1000,
    last_seen_at: 1500,
    ended_at: null,
    end_reason: null,
    active: false,
    bytes_sent: 0,
    bytes_received: 0,
    relay: false,
    ...overrides,
  };
}

describe("formatBytes", () => {
  it("uses decimal units", () => {
    expect(formatBytes(0)).toBe("0 Б");
    expect(formatBytes(999)).toBe("999 Б");
    expect(formatBytes(1000)).toBe("1,00 КБ");
    expect(formatBytes(1_500_000)).toBe("1,50 МБ");
    expect(formatBytes(12_340_000_000)).toBe("12,3 ГБ");
    expect(formatBytes(5_000_000_000_000)).toBe("5,00 ТБ");
    expect(formatBytes(250_000_000)).toBe("250 МБ");
  });
});

describe("formatDuration / formatHoursMinutes", () => {
  it("formats seconds, minutes, hours", () => {
    expect(formatDuration(45)).toBe("45 с");
    expect(formatDuration(720)).toBe("12 мин");
    expect(formatDuration(7500)).toBe("2 ч 05 мин");
    expect(formatDuration(-5)).toBe("0 с");
  });
  it("formats h:mm", () => {
    expect(formatHoursMinutes(0)).toBe("0:00");
    expect(formatHoursMinutes(3 * 3600 + 7 * 60)).toBe("3:07");
  });
});

describe("formatRelative", () => {
  const now = new Date(2026, 5, 15, 14, 0, 0).getTime() / 1000;
  it("handles minutes, hours, yesterday, older", () => {
    expect(formatRelative(now, now - 10)).toBe("только что");
    expect(formatRelative(now, now - 5 * 60)).toBe("5 мин назад");
    expect(formatRelative(now, new Date(2026, 5, 15, 10, 0, 0).getTime() / 1000)).toBe("4 ч назад");
    expect(formatRelative(now, new Date(2026, 5, 14, 23, 0, 0).getTime() / 1000)).toBe("вчера");
    expect(formatRelative(now, new Date(2026, 5, 10, 12, 0, 0).getTime() / 1000)).toBe("10.06.2026");
  });
});

describe("parseUserAgent", () => {
  it("recognises common browsers and systems", () => {
    expect(
      parseUserAgent(
        "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/17.0 Safari/605.1.15",
      ),
    ).toBe("Safari · macOS");
    expect(
      parseUserAgent(
        "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0 Safari/537.36",
      ),
    ).toBe("Chrome · Windows");
    expect(
      parseUserAgent(
        "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 Chrome/120.0 Safari/537.36 Edg/120.0",
      ),
    ).toBe("Edge · Windows");
    expect(parseUserAgent("Mozilla/5.0 (X11; Linux x86_64; rv:121.0) Gecko/20100101 Firefox/121.0")).toBe(
      "Firefox · Linux",
    );
    expect(
      parseUserAgent(
        "Mozilla/5.0 (iPhone; CPU iPhone OS 17_0 like Mac OS X) AppleWebKit/605.1.15 Version/17.0 Mobile/15E148 Safari/604.1",
      ),
    ).toBe("Safari · iOS");
    expect(
      parseUserAgent("Mozilla/5.0 (Linux; Android 14; Pixel 8) AppleWebKit/537.36 Chrome/120.0 Mobile Safari/537.36"),
    ).toBe("Chrome · Android");
  });
  it("falls back to a dash", () => {
    expect(parseUserAgent("")).toBe("—");
    expect(parseUserAgent("curl/8.0")).toBe("—");
  });
});

describe("session helpers", () => {
  it("computes duration", () => {
    expect(sessionDurationSecs(session({ ended_at: 1300 }), 9999)).toBe(300);
    expect(sessionDurationSecs(session({ active: true }), 1720)).toBe(720);
    expect(sessionDurationSecs(session({ active: false }), 9999)).toBe(500);
    expect(sessionDurationSecs(session({ ended_at: 900 }), 9999)).toBe(0);
  });
  it("labels running sessions", () => {
    expect(sessionDurationLabel(session({ active: true }), 1720)).toBe("идёт · 12 мин");
    expect(sessionDurationLabel(session({ ended_at: 1720 }), 9999)).toBe("12 мин");
  });
  it("hides zero bytes", () => {
    expect(sessionBytes(session())).toBeNull();
    expect(sessionBytes(session({ bytes_sent: 5, bytes_received: 7 }))).toBe(12);
  });
  it("labels end reasons", () => {
    expect(endReasonLabel("host_bye")).toBe("хост");
    expect(endReasonLabel("host")).toBe("хост");
    expect(endReasonLabel("client_bye")).toBe("клиент");
    expect(endReasonLabel(null)).toBe("—");
    expect(endReasonLabel("weird")).toBe("weird");
  });
});

describe("budgetShare", () => {
  it("computes share and danger threshold", () => {
    expect(budgetShare(0, 100)).toEqual({ fraction: 0, percent: 0, danger: false });
    expect(budgetShare(79, 100).danger).toBe(false);
    expect(budgetShare(80, 100).danger).toBe(true);
    expect(budgetShare(150, 100)).toEqual({ fraction: 1, percent: 150, danger: true });
    expect(budgetShare(5, 0)).toEqual({ fraction: 0, percent: 0, danger: false });
  });
});

describe("chartBars", () => {
  it("scales against the busiest day", () => {
    const bars = chartBars(
      [
        { day: "2026-06-01", sessions: 1, seconds: 100, bytes: 1000, relay_bytes: 250 },
        { day: "2026-06-02", sessions: 1, seconds: 50, bytes: 500, relay_bytes: 0 },
        { day: "2026-06-03", sessions: 0, seconds: 0, bytes: 0, relay_bytes: 0 },
      ],
      100,
      20,
    );
    expect(bars[0]).toEqual({ directH: 75, relayH: 25, hoursH: 20 });
    expect(bars[1]).toEqual({ directH: 50, relayH: 0, hoursH: 10 });
    expect(bars[2]).toEqual({ directH: 0, relayH: 0, hoursH: 0 });
  });
  it("keeps tiny values visible and handles empty input", () => {
    const bars = chartBars(
      [
        { day: "a", sessions: 1, seconds: 1, bytes: 1_000_000, relay_bytes: 0 },
        { day: "b", sessions: 1, seconds: 1, bytes: 1, relay_bytes: 0 },
      ],
      100,
      20,
    );
    expect(bars[1]?.directH).toBe(1);
    expect(chartBars([], 100, 20)).toEqual([]);
  });
});
