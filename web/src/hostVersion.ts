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
