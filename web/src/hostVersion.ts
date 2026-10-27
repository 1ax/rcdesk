import type { HostOs } from "./generated/HostOs";
import type { UpdateState } from "./generated/UpdateState";

// Чистая функция плашки «хост устарел» (слайс D50a): сравнивает версию хоста
// из `ControlMessage::HostInfo.version` с версией клиента. Без доступа к DOM,
// как `inputBlocked.ts`.

/** Разбирает строго `X.Y.Z` (допускается префикс `v`); иначе `null`. */
function parseVersion(v: string): [number, number, number] | null {
  const m = /^v?(\d+)\.(\d+)\.(\d+)$/.exec(v);
  if (!m) return null;
  return [Number(m[1]), Number(m[2]), Number(m[3])];
}

/** Поля нет только у хостов 0.1.0 -- с ней и сравниваем. */
const LAST_VERSION_WITHOUT_FIELD: [number, number, number] = [0, 1, 0];

/** Текст плашки, если хост старее клиента, иначе `null`. `hostVersion` ==
 * null/undefined -- старый хост (0.1.0), не шлющий поле. */
export function hostOutdatedLabel(
  hostVersion: string | null | undefined,
  clientVersion: string,
): string | null {
  const client = parseVersion(clientVersion);
  if (!client) return null;
  if (hostVersion === null || hostVersion === undefined) {
    return isOlder(LAST_VERSION_WITHOUT_FIELD, client)
      ? `На хосте старая версия rcdesk — доступна ${clientVersion}`
      : null;
  }
  const host = parseVersion(hostVersion);
  if (!host) return null;
  return isOlder(host, client) ? `На хосте rcdesk ${hostVersion} — доступна ${clientVersion}` : null;
}

function isOlder(a: [number, number, number], b: [number, number, number]): boolean {
  for (let i = 0; i < 3; i++) {
    if (a[i] !== b[i]) return a[i] < b[i];
  }
  return false;
}

/** Мегабайты (1024*1024) с одной цифрой и русской запятой: 4,2. */
function formatMb(bytes: number): string {
  return (bytes / (1024 * 1024)).toFixed(1).replace(".", ",");
}

/** Текст плашки по `update_status` (слайс D50d; D81 -- прогресс загрузки). */
export function updateStatusLabel(
  msg: {
    state: UpdateState;
    version: string | null;
    message: string | null;
    received?: number | null;
    total?: number | null;
  },
  hostOs: HostOs | null,
): string {
  const v = msg.version ?? "";
  switch (msg.state) {
    case "checking":
      return "Проверяю обновление…";
    case "downloading":
    {
      const { received, total } = msg;
      if (received === null || received === undefined) return `Скачиваю rcdesk ${v}…`;
      if (total === null || total === undefined || total <= 0) {
        return `Скачиваю rcdesk ${v}… ${formatMb(received)} МБ`;
      }
      const percent = Math.min(100, Math.floor((received * 100) / total));
      return `Скачиваю rcdesk ${v}… ${formatMb(received)} из ${formatMb(total)} МБ (${percent}%)`;
    }
    case "installing":
      return (
        `Устанавливаю rcdesk ${v} — сеанс прервётся` +
        (hostOs === "macos" ? " (на хосте нужен пароль администратора)" : "")
      );
    case "up_to_date":
      return "На хосте уже последняя версия";
    case "failed":
      return msg.message ? `Не удалось обновить: ${msg.message}` : "Не удалось обновить хост";
  }
}
