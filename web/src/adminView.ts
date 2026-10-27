// Pure presentation helpers for the admin cabinet page (slice A4). No DOM,
// no clocks: every function takes `now` explicitly so it stays testable.
import type { AdminDay } from "./generated/AdminDay";
import type { AdminSession } from "./generated/AdminSession";

const UNITS = ["Б", "КБ", "МБ", "ГБ", "ТБ"];

/** Decimal (1000-based) byte size: the VPS quota is decimal too. */
export function formatBytes(bytes: number): string {
  if (!Number.isFinite(bytes) || bytes <= 0) return "0 Б";
  let value = bytes;
  let unit = 0;
  while (value >= 1000 && unit < UNITS.length - 1) {
    value /= 1000;
    unit += 1;
  }
  if (unit === 0) return `${Math.round(value)} Б`;
  const text = value >= 100 ? value.toFixed(0) : value >= 10 ? value.toFixed(1) : value.toFixed(2);
  return `${text.replace(".", ",")} ${UNITS[unit]}`;
}

/** Short human duration: "45 с", "12 мин", "2 ч 05 мин". */
export function formatDuration(secs: number): string {
  const s = Math.max(0, Math.floor(secs));
  if (s < 60) return `${s} с`;
  if (s < 3600) return `${Math.floor(s / 60)} мин`;
  const h = Math.floor(s / 3600);
  const m = Math.floor((s % 3600) / 60);
  return `${h} ч ${String(m).padStart(2, "0")} мин`;
}

/** Total time as `h:mm`. */
export function formatHoursMinutes(secs: number): string {
  const s = Math.max(0, Math.floor(secs));
  const h = Math.floor(s / 3600);
  const m = Math.floor((s % 3600) / 60);
  return `${h}:${String(m).padStart(2, "0")}`;
}

function startOfLocalDay(secs: number): number {
  const d = new Date(secs * 1000);
  d.setHours(0, 0, 0, 0);
  return d.getTime() / 1000;
}

/** Local date `dd.mm.yyyy`. */
export function formatDate(ts: number): string {
  const d = new Date(ts * 1000);
  const dd = String(d.getDate()).padStart(2, "0");
  const mm = String(d.getMonth() + 1).padStart(2, "0");
  return `${dd}.${mm}.${d.getFullYear()}`;
}

/** Local `dd.mm.yyyy HH:MM`. */
export function formatDateTime(ts: number): string {
  const d = new Date(ts * 1000);
  const hh = String(d.getHours()).padStart(2, "0");
  const mi = String(d.getMinutes()).padStart(2, "0");
  return `${formatDate(ts)} ${hh}:${mi}`;
}

/** "только что" / "5 мин назад" / "3 ч назад" / "вчера" / date. Unix seconds. */
export function formatRelative(now: number, ts: number): string {
  const diff = now - ts;
  if (diff < 60) return "только что";
  if (diff < 3600) {
    const m = Math.floor(diff / 60);
    return `${m} мин назад`;
  }
  const today = startOfLocalDay(now);
  if (ts >= today) {
    return `${Math.floor(diff / 3600)} ч назад`;
  }
  if (ts >= startOfLocalDay(today - 1)) return "вчера";
  return formatDate(ts);
}

/** "Chrome · macOS", or "—" when nothing is recognised. */
export function parseUserAgent(ua: string): string {
  let browser = "";
  if (/Edg(e|A|iOS)?\//.test(ua)) browser = "Edge";
  else if (/Firefox\/|FxiOS\//.test(ua)) browser = "Firefox";
  else if (/Chrome\/|CriOS\//.test(ua)) browser = "Chrome";
  else if (/Safari\//.test(ua)) browser = "Safari";
  let os = "";
  if (/iPhone|iPad|iPod/.test(ua)) os = "iOS";
  else if (/Android/.test(ua)) os = "Android";
  else if (/Windows/.test(ua)) os = "Windows";
  else if (/Mac OS X|Macintosh/.test(ua)) os = "macOS";
  else if (/Linux|X11/.test(ua)) os = "Linux";
  const parts = [browser, os].filter((p) => p !== "");
  return parts.length > 0 ? parts.join(" · ") : "—";
}

/** Session length in seconds: to `ended_at`; an active one runs to `now`;
 * a dead one without `ended_at` (server restart) stops at `last_seen_at`. */
export function sessionDurationSecs(session: AdminSession, now: number): number {
  const end = session.ended_at ?? (session.active ? now : session.last_seen_at);
  return Math.max(0, end - session.started_at);
}

/** Duration cell text; running sessions read "идёт · 12 мин". */
export function sessionDurationLabel(session: AdminSession, now: number): string {
  const text = formatDuration(sessionDurationSecs(session, now));
  return session.active && session.ended_at === null ? `идёт · ${text}` : text;
}

export interface BudgetShare {
  /** 0..1, clamped, for the bar width. */
  fraction: number;
  /** Whole percent, not clamped. */
  percent: number;
  /** At or over 80 %. */
  danger: boolean;
}

export function budgetShare(used: number, budget: number): BudgetShare {
  if (!(budget > 0)) return { fraction: 0, percent: 0, danger: false };
  const raw = Math.max(0, used) / budget;
  return { fraction: Math.min(1, raw), percent: Math.round(raw * 100), danger: raw >= 0.8 };
}

/** Bytes shown for a session, or `null` when none were reported (old hosts). */
export function sessionBytes(session: AdminSession): number | null {
  const total = session.bytes_sent + session.bytes_received;
  return total > 0 ? total : null;
}

export function endReasonLabel(reason: string | null): string {
  switch (reason) {
    case "host_bye":
    case "host":
      return "хост";
    case "client_bye":
      return "клиент";
    case null:
      return "—";
    default:
      return reason;
  }
}

export interface Bar {
  /** Pixel heights within the chart area. */
  directH: number;
  relayH: number;
  hoursH: number;
}

/** Scales daily bars: traffic against the busiest day's bytes, hours against
 * the busiest day's seconds. Non-zero values never collapse below 1 px. */
export function chartBars(days: AdminDay[], trafficMaxH: number, hoursMaxH: number): Bar[] {
  const maxBytes = Math.max(0, ...days.map((d) => d.bytes));
  const maxSecs = Math.max(0, ...days.map((d) => d.seconds));
  const scale = (v: number, max: number, h: number): number =>
    v > 0 && max > 0 ? Math.max(1, (v / max) * h) : 0;
  return days.map((d) => {
    const relay = Math.min(d.relay_bytes, d.bytes);
    return {
      relayH: scale(relay, maxBytes, trafficMaxH),
      directH: scale(d.bytes - relay, maxBytes, trafficMaxH),
      hoursH: scale(d.seconds, maxSecs, hoursMaxH),
    };
  });
}
