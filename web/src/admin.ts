// Admin cabinet page (slice A4): `/admin.html`. Talks to the same `/ws` as the
// main page, authenticates with the saved owner token and renders the
// `AdminSnapshot` the server sends (non-admins get `error: forbidden`).
import "./style.css";
import "./admin.css";
import { SignalingClient } from "./signaling";
import { loadOwnerToken } from "./myDevices";
import {
  budgetShare,
  chartBars,
  endReasonLabel,
  formatBytes,
  formatDateTime,
  formatDuration,
  formatHoursMinutes,
  formatRelative,
  parseUserAgent,
  sessionBytes,
  sessionDurationLabel,
} from "./adminView";
import type { AdminSnapshot } from "./generated/AdminSnapshot";
import type { AdminDay } from "./generated/AdminDay";

const REFRESH_MS = 30_000;
const SVG_NS = "http://www.w3.org/2000/svg";

// Same logic as the private helpers in app.ts (not exported there; importing
// app.ts would pull the whole client into this page).
function ownerStorage(): Storage | null {
  try {
    return window.localStorage;
  } catch {
    return null;
  }
}

function signalUrl(): string {
  const scheme = location.protocol === "https:" ? "wss" : "ws";
  return `${scheme}://${location.host}/ws`;
}

function el<K extends keyof HTMLElementTagNameMap>(
  tag: K,
  className?: string,
  text?: string,
): HTMLElementTagNameMap[K] {
  const node = document.createElement(tag);
  if (className) node.className = className;
  if (text !== undefined) node.textContent = text;
  return node;
}

function svg<K extends keyof SVGElementTagNameMap>(
  tag: K,
  attrs: Record<string, string | number>,
  className?: string,
): SVGElementTagNameMap[K] {
  const node = document.createElementNS(SVG_NS, tag);
  for (const [k, v] of Object.entries(attrs)) node.setAttribute(k, String(v));
  if (className) node.setAttribute("class", className);
  return node;
}

const root = document.querySelector<HTMLDivElement>("#admin")!;
const signaling = new SignalingClient();

let snapshot: AdminSnapshot | null = null;
let authed = false;
let connecting = false;
let denied = false;
let statusText = "Подключаюсь…";
let lastRequestAt = 0;
let refreshBtn: HTMLButtonElement | null = null;
let statusEl: HTMLElement | null = null;

function showMessage(title: string, text: string, linkText: string, href: string): void {
  root.replaceChildren();
  const box = el("div", "adm-msg");
  box.append(el("h1", undefined, title), el("p", undefined, text));
  const a = el("a", "btn", linkText);
  a.href = href;
  box.append(a);
  root.append(box);
}

function setStatus(text: string): void {
  statusText = text;
  if (statusEl) statusEl.textContent = text;
}

function request(): void {
  lastRequestAt = Date.now();
  signaling.send({ type: "admin_get" });
}

function connect(): void {
  const storage = ownerStorage();
  const token = storage ? loadOwnerToken(storage) : null;
  if (token === null) {
    showMessage("Нужен вход", "Войдите в rcdesk на главной странице.", "На главную", "/");
    return;
  }
  connecting = true;
  authed = false;
  setStatus("Подключаюсь…");
  signaling.connect(signalUrl());
  signaling.send({ type: "client_auth", token });
}

function refresh(): void {
  if (denied) return;
  if (authed) {
    setStatus("Обновляю…");
    request();
  } else if (!connecting) {
    connect();
  }
}

signaling.on("authenticated", () => {
  connecting = false;
  authed = true;
  request();
});

signaling.on("admin_data", (msg) => {
  snapshot = msg.data;
  render();
});

signaling.on("error", (msg) => {
  if (msg.message === "forbidden") {
    denied = true;
    showMessage("Нет доступа", "У этого аккаунта нет прав администратора.", "На главную", "/");
  } else if (msg.message === "not authenticated") {
    denied = true;
    showMessage("Нужен вход", "Войдите в rcdesk на главной странице.", "На главную", "/");
  } else {
    setStatus(`Ошибка сервера: ${msg.message}`);
  }
});

signaling.addEventListener("close", () => {
  connecting = false;
  authed = false;
  if (denied) return;
  setStatus("Связь потеряна. Нажмите «Обновить».");
});

document.addEventListener("visibilitychange", () => {
  if (document.visibilityState === "visible" && Date.now() - lastRequestAt >= REFRESH_MS) refresh();
});

setInterval(() => {
  if (document.visibilityState === "visible" && authed) refresh();
}, REFRESH_MS);

// ---- rendering ----

function tile(label: string, value: string, sub?: string): HTMLElement {
  const t = el("div", "adm-tile");
  t.append(el("div", "adm-tile-label", label), el("div", "adm-tile-value", value));
  if (sub) t.append(el("div", "adm-tile-sub", sub));
  return t;
}

function renderTiles(s: AdminSnapshot): HTMLElement {
  const sum = s.summary;
  const wrap = el("div", "adm-tiles");
  wrap.append(
    tile("Пользователи", String(sum.users_total), `с почтой: ${sum.users_with_email}`),
    tile("Новые", `${sum.new_users_7d} / ${sum.new_users_30d}`, "за 7 / 30 дней"),
    tile("Устройства", String(sum.devices_total), `в сети: ${sum.hosts_online}`),
    tile("Активные сеансы", String(sum.sessions_active)),
    tile(
      "Трафик за 30 дней",
      formatBytes(sum.bytes_30d),
      `через релей: ${formatBytes(sum.relay_bytes_30d)}`,
    ),
  );
  const share = budgetShare(sum.relay_bytes_month, sum.relay_budget_bytes);
  const relay = tile(
    "Релей в этом месяце",
    formatBytes(sum.relay_bytes_month),
    `из ${formatBytes(sum.relay_budget_bytes)} · ${share.percent}%`,
  );
  const bar = el("div", "adm-bar");
  const fill = el("div", share.danger ? "adm-bar-fill is-danger" : "adm-bar-fill");
  fill.style.width = `${(share.fraction * 100).toFixed(1)}%`;
  bar.append(fill);
  relay.append(bar);
  wrap.append(relay);
  return wrap;
}

function section(title: string): HTMLElement {
  const s = el("section", "adm-section");
  s.append(el("h2", undefined, title));
  return s;
}

function dayTitle(d: AdminDay): string {
  const [y, m, dd] = d.day.split("-");
  return (
    `${dd}.${m}.${y}\nСеансов: ${d.sessions}\nВремя: ${formatDuration(d.seconds)}\n` +
    `Трафик: ${formatBytes(d.bytes)} (релей ${formatBytes(d.relay_bytes)})`
  );
}

function renderChart(days: AdminDay[]): HTMLElement {
  const sec = section("Последние 30 дней");
  if (days.length === 0 || days.every((d) => d.sessions === 0 && d.bytes === 0)) {
    sec.append(el("p", "adm-empty", "Пока нет данных о сеансах."));
    return sec;
  }
  const legend = el("div", "adm-legend");
  for (const [cls, label] of [
    ["is-direct", "Трафик напрямую"],
    ["is-relay", "Через релей"],
    ["is-hours", "Часы сеансов"],
  ] as const) {
    const item = el("span");
    item.append(el("span", `adm-swatch ${cls}`), document.createTextNode(label));
    legend.append(item);
  }
  sec.append(legend);

  const W = 720;
  const trafficH = 110;
  const hoursH = 26;
  const gap = 14;
  const left = 44;
  const bottom = 14;
  const H = trafficH + gap + hoursH + bottom;
  const n = days.length;
  const slot = (W - left) / n;
  const barW = Math.max(2, slot * 0.7);
  const bars = chartBars(days, trafficH, hoursH);
  const chart = svg("svg", { viewBox: `0 0 ${W} ${H}`, role: "img" }, "adm-chart");
  chart.setAttribute("aria-label", "Трафик и часы сеансов по дням");
  chart.append(svg("line", { x1: left, y1: trafficH, x2: W, y2: trafficH }, "axis-line"));
  const maxBytes = Math.max(...days.map((d) => d.bytes));
  const maxSecs = Math.max(...days.map((d) => d.seconds));
  const top = svg("text", { x: left - 4, y: 9, "text-anchor": "end" }, "axis-label");
  top.textContent = formatBytes(maxBytes);
  const hTop = svg("text", { x: left - 4, y: trafficH + gap + 9, "text-anchor": "end" }, "axis-label");
  hTop.textContent = formatHoursMinutes(maxSecs);
  chart.append(top, hTop);

  days.forEach((d, i) => {
    const b = bars[i]!;
    const x = left + i * slot + (slot - barW) / 2;
    const g = svg("g", {});
    const title = svg("title", {});
    title.textContent = dayTitle(d);
    g.append(title);
    g.append(svg("rect", { x: left + i * slot, y: 0, width: slot, height: H - bottom }, "hit"));
    g.append(
      svg("rect", { x, y: trafficH - b.directH, width: barW, height: b.directH }, "bar-direct"),
      svg(
        "rect",
        { x, y: trafficH - b.directH - b.relayH, width: barW, height: b.relayH },
        "bar-relay",
      ),
      svg(
        "rect",
        { x, y: trafficH + gap + hoursH - b.hoursH, width: barW, height: b.hoursH },
        "bar-hours",
      ),
    );
    chart.append(g);
  });
  const first = days[0]!.day.slice(5).split("-").reverse().join(".");
  const last = days[n - 1]!.day.slice(5).split("-").reverse().join(".");
  const l = svg("text", { x: left, y: H - 2 }, "axis-label");
  l.textContent = first;
  const r = svg("text", { x: W, y: H - 2, "text-anchor": "end" }, "axis-label");
  r.textContent = last;
  chart.append(l, r);
  sec.append(chart);
  return sec;
}

function table(headers: string[], rows: HTMLElement[][], emptyText: string): HTMLElement {
  if (rows.length === 0) return el("p", "adm-empty", emptyText);
  const wrap = el("div", "adm-scroll");
  const t = el("table", "adm-table");
  const head = el("tr");
  for (const h of headers) head.append(el("th", undefined, h));
  const thead = el("thead");
  thead.append(head);
  t.append(thead);
  const body = el("tbody");
  for (const cells of rows) {
    const tr = el("tr");
    for (const c of cells) tr.append(c);
    body.append(tr);
  }
  t.append(body);
  wrap.append(t);
  return wrap;
}

function td(text: string, className?: string): HTMLTableCellElement {
  return el("td", className, text);
}

function noteCell(ownerId: string, note: string | null): HTMLTableCellElement {
  const cell = el("td", note ? "adm-note" : "adm-note adm-muted", note ?? "заметка…");
  cell.title = "Нажмите, чтобы изменить";
  cell.addEventListener("click", () => {
    if (cell.querySelector("input")) return;
    const input = el("input", "adm-note-input");
    input.type = "text";
    input.maxLength = 200;
    input.value = note ?? "";
    let done = false;
    const finish = (save: boolean): void => {
      if (done) return;
      done = true;
      const value = input.value.trim();
      if (save && value !== (note ?? "")) {
        signaling.send({ type: "admin_set_note", owner_id: ownerId, note: value === "" ? null : value });
        cell.textContent = value === "" ? "заметка…" : value;
        cell.className = value === "" ? "adm-note adm-muted" : "adm-note";
      } else {
        cell.textContent = note ?? "заметка…";
      }
    };
    input.addEventListener("keydown", (e) => {
      if (e.key === "Enter") finish(true);
      else if (e.key === "Escape") finish(false);
    });
    input.addEventListener("blur", () => finish(true));
    cell.replaceChildren(input);
    input.focus();
  });
  return cell;
}

function renderUsers(s: AdminSnapshot, now: number): HTMLElement {
  const sec = section(`Пользователи (${s.users.length})`);
  const rows = s.users.map((u) => {
    const email = u.email === null ? td("без почты", "adm-muted") : td(u.email);
    return [
      email,
      noteCell(u.owner_id, u.note),
      td(formatDateTime(u.created_at)),
      td(formatRelative(now, u.last_active_at)),
      td(`${u.devices} / ${u.browsers} / ${u.passkeys}`),
      td(String(u.sessions_total)),
      td(formatHoursMinutes(u.seconds_total)),
      td(`${formatBytes(u.bytes_30d)} / ${formatBytes(u.bytes_total)}`),
    ];
  });
  sec.append(
    table(
      [
        "Почта",
        "Заметка",
        "Зарегистрирован",
        "Активность",
        "Устр. / браузеров / passkey",
        "Сеансов",
        "Время",
        "Трафик 30 дн / всего",
      ],
      rows,
      "Пользователей нет.",
    ),
  );
  return sec;
}

function renderDevices(s: AdminSnapshot, now: number): HTMLElement {
  const sec = section(`Устройства (${s.devices.length})`);
  const rows = s.devices.map((d) => {
    const status = d.online
      ? d.busy
        ? td("● занят")
        : td("● в сети", "adm-ok")
      : td("не в сети", "adm-muted");
    return [
      td(d.name),
      td(d.owners.join(", ") || "—"),
      status,
      td(d.host_version ?? "—"),
      td(d.online ? "сейчас" : formatRelative(now, d.last_seen_at)),
    ];
  });
  sec.append(
    table(["Имя", "Владельцы", "Статус", "Версия хоста", "Последний раз в сети"], rows, "Устройств нет."),
  );
  return sec;
}

function renderSessions(s: AdminSnapshot, now: number): HTMLElement {
  const sec = section(`Сеансы (последние ${s.sessions.length})`);
  const rows = s.sessions.map((x) => {
    const bytes = sessionBytes(x);
    return [
      td(formatDateTime(x.started_at)),
      td(sessionDurationLabel(x, now)),
      x.email !== null ? td(x.email) : td("по PIN, без входа", "adm-muted"),
      td(x.device_name ?? "—"),
      td(x.via === "pin" ? "PIN" : "из списка"),
      td(parseUserAgent(x.client_ua)),
      bytes === null ? td("—", "adm-muted") : td(formatBytes(bytes)),
      bytes === null ? td("—", "adm-muted") : td(x.relay ? "релей" : "напрямую"),
      td(x.active && x.ended_at === null ? "—" : endReasonLabel(x.end_reason)),
    ];
  });
  sec.append(
    table(
      ["Начало", "Длительность", "Кто", "Устройство", "Способ", "Браузер", "Объём", "Путь", "Завершил"],
      rows,
      "Сеансов пока нет.",
    ),
  );
  return sec;
}

function render(): void {
  if (!snapshot) return;
  const s = snapshot;
  const now = s.generated_at;
  root.replaceChildren();
  const page = el("div", "adm");
  const head = el("div", "adm-head");
  refreshBtn = el("button", "btn btn-secondary", "Обновить");
  refreshBtn.addEventListener("click", refresh);
  const home = el("a", "btn btn-secondary", "На главную");
  home.href = "/";
  statusEl = el("span", "adm-status", "");
  head.append(el("h1", undefined, "rcdesk · админка"), statusEl, refreshBtn, home);
  page.append(
    head,
    renderTiles(s),
    renderChart(s.daily),
    renderUsers(s, now),
    renderDevices(s, now),
    renderSessions(s, now),
    el("p", "adm-foot", `обновлено ${formatDateTime(s.generated_at)}`),
  );
  root.append(page);
  setStatus("");
}

root.append(el("p", "adm-status", statusText));
connect();
