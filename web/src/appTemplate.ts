// First-screen and session-screen markup for mount(). Pure string template:
// the only dynamic parts are the download block (built in app.ts) and a
// shared label constant.
import { FIX_ADDRESS_LABEL } from "./emailAuth";

export function appTemplate(params: { downloadBlock: string }): string {
  return `
    <div class="build-badge" id="build-badge"></div>
    <div class="pin-screen" id="pin-screen">
      <div class="home" id="main-card">
        <header class="home-header">
          <h1>rcdesk</h1>
          <div class="account" id="account" hidden>
            <div id="account-guest" hidden>
              <button id="login-open-btn" class="btn">Войти</button>
              <button id="signup-open-btn" class="btn btn-secondary">Создать аккаунт</button>
              <button id="passkey-login-btn" class="link-btn">Войти с passkey</button>
              <p class="hint">Войдите, чтобы список компьютеров был доступен в любом браузере</p>
            </div>
            <button id="account-open-btn" class="btn btn-secondary" hidden>Аккаунт</button>
            <p id="account-status" class="hint account-status" role="status" aria-live="polite"></p>
          </div>
        </header>
        <div class="home-grid">
          <section class="home-main">
            <div class="devices" id="devices" hidden>
              <div class="devices-head">
                <h2>Мои компьютеры</h2>
                <details class="add-menu wake-add-buttons" id="wake-add-buttons">
                  <summary class="btn btn-secondary add-menu-btn">+ Добавить</summary>
                  <div class="add-menu-pop">
                    <button id="add-manual-btn" class="menu-item">Устройство по MAC</button>
                    <button id="add-router-btn" class="menu-item">Роутер-будильник</button>
                  </div>
                </details>
              </div>
              <div class="wake-add-panel" id="wake-add-panel" hidden></div>
              <ul class="device-list" id="device-list"></ul>
              <div class="wakers" id="wakers" hidden>
                <h2>Будильники</h2>
                <ul class="device-list" id="waker-list"></ul>
              </div>
            </div>
          </section>
          <aside class="home-side">
            <div class="home-panel pin-panel">
              <h2>Подключиться по PIN</h2>
              <input
                id="pin-input"
                class="pin-input"
                type="text"
                inputmode="numeric"
                pattern="[0-9]*"
                maxlength="6"
                placeholder="000000"
                autocomplete="one-time-code"
                aria-label="PIN хоста"
              />
              <button id="connect-btn" class="btn">Подключиться</button>
              <p class="hint pin-hint">PIN показан на экране хоста</p>
              <p id="pin-status" class="hint" role="status" aria-live="polite"></p>
            </div>
            <div class="home-panel">${params.downloadBlock}
            </div>
          </aside>
        </div>
      </div>
      <div class="card account-card" id="account-card" hidden>
        <h1>Аккаунт</h1>
        <div class="devices" id="email-block" hidden>
          <h2>Почта и пароль</h2>
          <p id="email-current" class="hint email-current"></p>
          <button id="email-action-btn" class="btn btn-secondary">Добавить почту и пароль</button>
          <button id="email-change-btn" class="btn btn-secondary" hidden>Сменить почту</button>
        </div>
        <div class="devices" id="passkey-block">
          <h2>Passkey</h2>
          <ul class="device-list" id="passkey-list"></ul>
          <button id="passkey-add-btn" class="btn btn-secondary">Добавить passkey</button>
        </div>
        <div class="devices">
          <h2>Браузеры</h2>
          <ul class="device-list" id="browser-list"></ul>
        </div>
        <a id="admin-link" class="btn btn-secondary" href="/admin.html" hidden>Админка</a>
        <button id="sign-out-btn" class="btn btn-secondary account-danger-btn">Выйти</button>
        <p id="sign-out-hint" class="hint"></p>
        <p id="account-card-status" class="hint account-status" role="status" aria-live="polite"></p>
        <button id="account-back-btn" class="btn btn-secondary">Назад</button>
      </div>
      <div class="card account-card" id="auth-card" hidden>
        <h1 id="ea-title">Вход</h1>
        <form id="ea-login-form" class="ea-form" novalidate>
          <input id="ea-login-email" class="text-input" type="email" name="email"
            autocomplete="username" placeholder="Почта" aria-label="Почта" autocapitalize="off" spellcheck="false" />
          <input id="ea-login-password" class="text-input" type="password" name="password"
            autocomplete="current-password" placeholder="Пароль" aria-label="Пароль" />
          <button id="ea-login-submit" type="submit" class="btn">Войти</button>
          <button id="ea-forgot-btn" type="button" class="link-btn">Забыли пароль?</button>
          <button id="ea-signup-btn" type="button" class="link-btn">Создать аккаунт</button>
          <button id="ea-passkey-login-btn" type="button" class="link-btn">Войти с passkey</button>
        </form>
        <form id="ea-request-form" class="ea-form" novalidate>
          <input id="ea-request-email" class="text-input" type="email" name="email"
            autocomplete="username" placeholder="Почта" aria-label="Почта" autocapitalize="off" spellcheck="false" />
          <button id="ea-request-submit" type="submit" class="btn">Получить код</button>
          <button id="ea-to-login-btn" type="button" class="link-btn">У меня уже есть аккаунт</button>
          <button id="ea-passkey-save-btn" type="button" class="link-btn">или сохранить через passkey</button>
        </form>
        <form id="ea-confirm-form" class="ea-form" novalidate>
          <p id="ea-sent-text" class="hint ea-sent"></p>
          <input id="ea-code" class="text-input" type="text" name="code" inputmode="numeric"
            pattern="[0-9]*" maxlength="6" autocomplete="one-time-code" placeholder="Код из письма" aria-label="Код из письма" />
          <input id="ea-new-password" class="text-input" type="password" name="new-password"
            autocomplete="new-password" placeholder="Новый пароль (8–128 символов)" aria-label="Новый пароль" />
          <button id="ea-confirm-submit" type="submit" class="btn">Готово</button>
          <button id="ea-resend-btn" type="button" class="link-btn">Отправить код ещё раз</button>
          <button id="ea-change-email-btn" type="button" class="link-btn">${FIX_ADDRESS_LABEL}</button>
        </form>
        <p id="ea-status" class="hint account-status" role="status" aria-live="polite"></p>
        <button id="ea-back-btn" class="btn btn-secondary">Назад</button>
      </div>
    </div>
    <div class="session-screen" id="session-screen" hidden>
      <div class="session-bar" id="session-bar">
        <div class="session-bar-left">
          <label class="stats-toggle bar-secondary" title="Показывать статистику видеопотока">
            <input type="checkbox" id="stats-checkbox" />
            Статистика
          </label>
          <label class="stats-toggle bar-secondary" title="Синхронизировать буфер обмена с хостом в обе стороны">
            <input type="checkbox" id="clipboard-sync-checkbox" />
            Буфер обмена
          </label>
          <button id="clipboard-push-btn" class="btn btn-secondary bar-secondary" title="Отправить буфер телефона на хост" hidden>Буфер →</button>
          <span class="overlay" id="stats-overlay" hidden></span>
        </div>
        <div class="controls">
          <span id="file-status" class="status file-status" hidden>
            <span id="file-status-text" class="file-status-text"></span>
            <button id="file-cancel-btn" class="btn btn-secondary" title="Отменить передачу" aria-label="Отменить передачу">✕</button>
          </span>
          <span id="download-status" class="status file-status" hidden>
            <span id="download-status-text" class="file-status-text"></span>
            <button id="download-cancel-btn" class="btn btn-secondary" title="Отменить скачивание" aria-label="Отменить скачивание">✕</button>
          </span>
          <button id="file-fetch-btn" class="btn btn-secondary bar-secondary" hidden>Скачать</button>
          <button id="file-send-btn" class="btn btn-secondary bar-secondary" hidden>Отправить файлы</button>
          <input id="file-input" type="file" multiple hidden />
          <button id="folder-send-btn" class="btn btn-secondary bar-secondary" hidden>Отправить папку</button>
          <input id="folder-input" type="file" webkitdirectory multiple hidden />
          <select id="display-select" class="display-select bar-secondary" aria-label="Дисплей" hidden></select>
          <select id="quality-select" class="quality-select bar-secondary" title="Качество" aria-label="Качество"></select>
          <label id="cmd-as-ctrl-label" class="cmd-as-ctrl-toggle bar-secondary" hidden>
            <input type="checkbox" id="cmd-as-ctrl-checkbox" />
            <span id="cmd-as-ctrl-text">Cmd как Ctrl</span>
          </label>
          <span id="view-only" class="status view-only" hidden></span>
          <span id="input-blocked" class="status input-blocked" hidden></span>
          <span id="host-outdated" class="status host-outdated" hidden>
            <span id="host-outdated-text"></span>
            <button id="host-update-btn" class="btn btn-secondary bar-secondary" hidden>Обновить хост</button>
          </span>
          <span id="curtain-status" class="status curtain-status" hidden></span>
          <span id="clipboard-note" class="status clipboard-note" role="status" aria-live="polite" hidden></span>
          <span id="fullscreen-note" class="status fullscreen-note" hidden></span>
          <span id="session-status" class="status" role="status" aria-live="polite"></span>
          <button
            id="curtain-btn"
            class="btn btn-secondary curtain-btn bar-secondary"
            title="Скрыть экран Mac от тех, кто рядом с ним; при отключении Mac заблокируется"
            aria-pressed="false"
            hidden
          >
            Шторка
          </button>
          <button
            id="layout-btn"
            class="btn btn-secondary layout-btn bar-secondary"
            title="Раскладка на удалённом компьютере — нажмите или Alt+Shift"
            hidden
          >
            Яз
          </button>
          <button
            id="ctrl-alt-del-btn"
            class="btn btn-secondary bar-secondary"
            title="Отправить Ctrl+Alt+Del (экран безопасности Windows)"
            hidden
          >
            Ctrl+Alt+Del
          </button>
          <button id="keyboard-btn" class="btn btn-secondary" hidden>Клавиатура</button>
          <button id="touch-mode-btn" class="btn btn-secondary" title="Режим касаний" hidden></button>
          <button id="fullscreen-btn" class="btn btn-secondary" hidden>На весь экран</button>
          <button id="disconnect-btn" class="btn btn-secondary">Отключиться</button>
          <button
            id="bar-more-btn"
            class="btn btn-secondary bar-more-btn"
            aria-expanded="false"
            aria-controls="session-bar"
          >Ещё</button>
          <button
            id="bar-collapse-btn"
            class="btn btn-secondary bar-collapse-btn"
            title="Свернуть панель"
            aria-label="Свернуть панель"
          >▲</button>
        </div>
      </div>
      <button
        class="session-bar-handle"
        id="session-bar-handle"
        title="Показать панель"
        aria-label="Показать панель"
        hidden
      ></button>
      <div class="session-video">
        <div class="zoom-layer" id="zoom-layer">
          <video id="video" autoplay playsinline muted aria-label="Экран удалённого компьютера"></video>
          <img id="touch-cursor" class="touch-cursor" alt="" hidden />
        </div>
        <div class="connection-banner" id="connection-banner" role="alert" hidden></div>
        <div class="file-drop-hint" id="file-drop-hint" hidden>Отпустите, чтобы отправить на хост</div>
      </div>
      <div class="auth-dialog" id="auth-dialog" role="dialog" aria-modal="true" aria-labelledby="auth-dialog-title" hidden>
        <div class="auth-dialog-card">
          <h2 id="auth-dialog-title">Пароль доступа к хосту</h2>
          <p class="auth-dialog-host" id="auth-host-name"></p>
          <input
            id="auth-password"
            class="auth-password-input"
            type="password"
            autocomplete="current-password"
            aria-label="Пароль доступа к хосту"
          />
          <p id="auth-status" class="hint" role="status" aria-live="polite"></p>
          <div class="auth-dialog-actions">
            <button id="auth-submit" class="btn">Подключиться</button>
            <button id="auth-cancel" class="btn btn-secondary">Отмена</button>
          </div>
        </div>
      </div>
      <div id="soft-keys" class="soft-keys" hidden></div>
      <input id="soft-input" class="soft-input" type="text" autocomplete="off" autocapitalize="off" spellcheck="false" inputmode="text" aria-label="Ввод текста" />
    </div>
  `;
}
