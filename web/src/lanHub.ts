/** Режим «LAN-хаб» (D47): сервер объявляет его в `Authenticated.lan_hub`.
 * Аккаунтов, passkey и почты там нет, а агент с GitHub недоступен (сеть
 * изолирована), поэтому клиент прячет их UI и не шлёт связанные запросы. */
export interface LanHubUi {
  /** Показывать блок аккаунта, passkey, почту и карточку почтового входа. */
  showAccountUi: boolean;
  /** Показывать блок скачивания агента (ссылки на GitHub). */
  showDownload: boolean;
  /** Слать `list_passkeys` и `get_account` после `authenticated`. */
  requestAccountData: boolean;
}

/** Что показывать/слать для значения `lan_hub` из `authenticated`
 * (`undefined` -- старый сервер без поля -- то же, что `false`). */
export function lanHubUi(lanHub: boolean | undefined): LanHubUi {
  const on = lanHub === true;
  return { showAccountUi: !on, showDownload: !on, requestAccountData: !on };
}
